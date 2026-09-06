use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::Command;
use super::timeline::DisplayRender;
use crate::colour::XyzToSrgb;
use crate::preview::{self, Display, FrameRender, Rgb8Frame};

// one grok thread per decode, the pool is the parallelism
const SINGLE_DECODE_THREAD: u32 = 1;
const FALLBACK_WORKER_COUNT: usize = 4;
const LOOKAHEAD_FRAMES_PER_WORKER: usize = 2;
const RGB_COMPONENT_COUNT: usize = 3;
const DISPLAY_PRECISION_BITS: u8 = 8;

pub(super) struct DecodeJob {
    pub generation: u64,
    pub frame_index: u64,
    pub codestream: Vec<u8>,
    pub reduce: u8,
    pub render: DisplayRender,
    pub mxf: PathBuf,
}

pub(super) enum CachedFrame {
    Decoded(Arc<Rgb8Frame>),
    Failed(String),
}

pub(super) struct FrameCache {
    generation: u64,
    frames: BTreeMap<u64, CachedFrame>,
}

impl FrameCache {
    fn new() -> Self {
        FrameCache {
            generation: 0,
            frames: BTreeMap::new(),
        }
    }

    pub fn restart(&mut self, generation: u64) {
        self.generation = generation;
        self.frames.clear();
    }

    // a result from before a seek is dropped
    pub fn store(&mut self, generation: u64, frame_index: u64, frame: CachedFrame) -> bool {
        if generation != self.generation {
            return false;
        }
        self.frames.insert(frame_index, frame);
        true
    }

    pub fn holds(&self, frame_index: u64) -> bool {
        self.frames.contains_key(&frame_index)
    }

    pub fn decoded(&self, frame_index: u64) -> Option<Arc<Rgb8Frame>> {
        match self.frames.get(&frame_index) {
            Some(CachedFrame::Decoded(frame)) => Some(frame.clone()),
            _ => None,
        }
    }

    pub fn failure(&self, frame_index: u64) -> Option<&str> {
        match self.frames.get(&frame_index) {
            Some(CachedFrame::Failed(reason)) => Some(reason),
            _ => None,
        }
    }

    // what the clock shows when the frame it asked for has not arrived
    pub fn newest_decoded_in(&self, first: u64, last: u64) -> Option<u64> {
        self.frames
            .range(first..=last)
            .rev()
            .find(|(_, frame)| matches!(frame, CachedFrame::Decoded(_)))
            .map(|(index, _)| *index)
    }

    pub fn forget_before(&mut self, first: u64) {
        self.frames.retain(|index, _| *index >= first);
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }
}

struct JobQueue {
    jobs: Mutex<Option<VecDeque<DecodeJob>>>,
    ready: Condvar,
}

impl JobQueue {
    fn push(&self, job: DecodeJob) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(queue) = jobs.as_mut() {
            queue.push_back(job);
            self.ready.notify_one();
        }
    }

    // a seek must not leave workers finishing frames the cache will refuse
    fn discard_queued(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(queue) = jobs.as_mut() {
            queue.clear();
        }
    }

    fn pop(&self) -> Option<DecodeJob> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            let queue = jobs.as_mut()?;
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
            jobs = self.ready.wait(jobs).unwrap();
        }
    }

    fn close(&self) {
        *self.jobs.lock().unwrap() = None;
        self.ready.notify_all();
    }
}

pub(super) struct DecodePool {
    queue: Arc<JobQueue>,
    workers: Vec<JoinHandle<()>>,
    cache: Arc<Mutex<FrameCache>>,
    pub worker_count: usize,
}

impl DecodePool {
    pub fn start(finished: Sender<Command>) -> Self {
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(FALLBACK_WORKER_COUNT);
        let queue = Arc::new(JobQueue {
            jobs: Mutex::new(Some(VecDeque::new())),
            ready: Condvar::new(),
        });
        let cache = Arc::new(Mutex::new(FrameCache::new()));
        let workers = (0..worker_count)
            .map(|_| {
                let queue = queue.clone();
                let cache = cache.clone();
                let finished = finished.clone();
                std::thread::spawn(move || run_worker(&queue, &cache, &finished))
            })
            .collect();
        DecodePool {
            queue,
            workers,
            cache,
            worker_count,
        }
    }

    pub fn lookahead_frames(&self) -> usize {
        self.worker_count * LOOKAHEAD_FRAMES_PER_WORKER
    }

    pub fn submit(&self, job: DecodeJob) {
        self.queue.push(job);
    }

    pub fn restart(&self, generation: u64) {
        self.queue.discard_queued();
        self.cache.lock().unwrap().restart(generation);
    }

    pub fn record_failure(&self, generation: u64, frame_index: u64, reason: String) {
        self.cache
            .lock()
            .unwrap()
            .store(generation, frame_index, CachedFrame::Failed(reason));
    }

    pub fn holds(&self, frame_index: u64) -> bool {
        self.cache.lock().unwrap().holds(frame_index)
    }

    pub fn decoded(&self, frame_index: u64) -> Option<Arc<Rgb8Frame>> {
        self.cache.lock().unwrap().decoded(frame_index)
    }

    pub fn failure(&self, frame_index: u64) -> Option<String> {
        self.cache
            .lock()
            .unwrap()
            .failure(frame_index)
            .map(str::to_string)
    }

    pub fn newest_decoded_in(&self, first: u64, last: u64) -> Option<u64> {
        self.cache.lock().unwrap().newest_decoded_in(first, last)
    }

    pub fn forget_before(&self, first: u64) {
        self.cache.lock().unwrap().forget_before(first);
    }

    pub fn cached_frame_count(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}

impl Drop for DecodePool {
    fn drop(&mut self) {
        self.queue.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run_worker(queue: &JobQueue, cache: &Mutex<FrameCache>, finished: &Sender<Command>) {
    // the table build costs a few hundred microseconds
    let display = Display::Srgb(XyzToSrgb::new());
    while let Some(job) = queue.pop() {
        if cache.lock().unwrap().generation != job.generation {
            continue;
        }
        let decoded = match decode_job(job.codestream, job.reduce, job.render, &job.mxf, &display) {
            Ok(frame) => CachedFrame::Decoded(Arc::new(frame)),
            Err(reason) => CachedFrame::Failed(reason),
        };
        cache
            .lock()
            .unwrap()
            .store(job.generation, job.frame_index, decoded);
        if finished.send(Command::DecodeFinished).is_err() {
            return;
        }
    }
}

fn decode_job(
    codestream: Vec<u8>,
    reduce: u8,
    render: DisplayRender,
    mxf: &std::path::Path,
    display: &Display,
) -> Result<Rgb8Frame, String> {
    let render = match render {
        DisplayRender::DcpXyz => FrameRender::Dcp(display),
        DisplayRender::Imf(ref colour) => FrameRender::Imf(colour),
        DisplayRender::PlainRgb => {
            let decoded =
                crate::grok_decoder::decode_with_threads(codestream, reduce, SINGLE_DECODE_THREAD)?;
            return plain_rgb8(&decoded, mxf);
        }
    };
    preview::display_frame_from_codestream(codestream, reduce, SINGLE_DECODE_THREAD, render, mxf)
        .map_err(|e| e.to_string())
}

fn plain_rgb8(
    decoded: &crate::grok_decoder::DecodedFrame,
    mxf: &std::path::Path,
) -> Result<Rgb8Frame, String> {
    if decoded.components.len() != RGB_COMPONENT_COUNT {
        return Err(format!(
            "{} decodes to {} components, and a codestream with no colour signalling is shown \
             only as {RGB_COMPONENT_COUNT}-component RGB",
            mxf.display(),
            decoded.components.len()
        ));
    }
    let samples = decoded.width as usize * decoded.height as usize;
    let mut data = Vec::with_capacity(samples * RGB_COMPONENT_COUNT);
    for sample in 0..samples {
        for component in &decoded.components {
            data.push(to_eight_bits(component[sample], decoded.precision));
        }
    }
    Ok(Rgb8Frame {
        width: decoded.width,
        height: decoded.height,
        data,
    })
}

fn to_eight_bits(code: i32, precision: u8) -> u8 {
    let full_scale = u8::MAX as i32;
    let code = code.max(0);
    if precision >= DISPLAY_PRECISION_BITS {
        return (code >> (precision - DISPLAY_PRECISION_BITS)).min(full_scale) as u8;
    }
    // a shift would leave the brightest sample short of white
    let maximum = ((1i32 << precision) - 1).max(1);
    (code.min(maximum) * full_scale / maximum) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32) -> CachedFrame {
        CachedFrame::Decoded(Arc::new(Rgb8Frame {
            width,
            height: 1,
            data: vec![0; width as usize * 3],
        }))
    }

    #[test]
    fn a_result_from_before_a_seek_is_discarded() {
        let mut cache = FrameCache::new();
        cache.restart(1);
        assert!(cache.store(1, 10, frame(2)), "the live generation stores");
        cache.restart(2);
        assert!(
            !cache.store(1, 11, frame(2)),
            "the stale generation does not"
        );
        assert!(!cache.holds(11));
        assert!(!cache.holds(10), "a restart empties the cache");
    }

    #[test]
    fn the_newest_decoded_frame_up_to_the_target_is_the_one_to_show() {
        let mut cache = FrameCache::new();
        for index in [3u64, 5, 8] {
            cache.store(0, index, frame(2));
        }
        cache.store(0, 7, CachedFrame::Failed("no".into()));
        assert_eq!(cache.newest_decoded_in(3, 8), Some(8));
        // 7 failed, so the answer falls back past it
        assert_eq!(cache.newest_decoded_in(3, 7), Some(5));
        assert_eq!(cache.newest_decoded_in(9, 12), None);
    }

    #[test]
    fn presenting_keeps_only_the_frame_before_the_current_one() {
        let mut cache = FrameCache::new();
        for index in 0u64..6 {
            cache.store(0, index, frame(2));
        }
        cache.forget_before(3);
        assert!(!cache.holds(2));
        assert!(cache.holds(3));
        assert_eq!(cache.len(), 3);
    }

    #[cfg(feature = "grok-ffi")]
    fn cinema_2k_codestream() -> Vec<u8> {
        const WIDTH: u32 = 2048;
        const HEIGHT: u32 = 1080;
        const TWELVE_BIT_MAX: i32 = 4095;
        const CINEMA_2K_PROFILE: u16 = 0x0003;
        const DCI_COMPRESSION_RATIO: f64 = 12.0;
        const DCI_RESOLUTIONS: u8 = 6;

        let samples = (WIDTH * HEIGHT) as usize;
        let plane = |seed: u32| -> Vec<i32> {
            (0..samples)
                .map(|index| {
                    let x = (index as u32 % WIDTH).wrapping_mul(seed.wrapping_add(7));
                    let y = (index as u32 / WIDTH).wrapping_mul(seed.wrapping_add(13));
                    ((x ^ y).wrapping_mul(2_654_435_761) >> 20) as i32 & TWELVE_BIT_MAX
                })
                .collect()
        };
        let params = crate::grok_encoder::CompressParams {
            irreversible: true,
            compression_ratio: DCI_COMPRESSION_RATIO,
            mct: false,
            apply_xyz_transform: false,
            profile: CINEMA_2K_PROFILE,
            num_resolutions: DCI_RESOLUTIONS,
            ..crate::grok_encoder::CompressParams::default()
        };
        crate::grok_encoder::initialize(0);
        let directory = tempfile::tempdir().unwrap();
        let mut frame = Some(crate::grok_encoder::RawFrame::Planar {
            components: [plane(1), plane(2), plane(3)],
            width: WIDTH,
            height: HEIGHT,
            precision: 12,
            index: 0,
        });
        let result = crate::grok_encoder::encode_pipeline(
            directory.path(),
            &params,
            1,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &Arc::new(crate::grok_encoder::PhaseClocks::default()),
            || frame.take(),
            |_| {},
        );
        assert!(result.success, "fixture encode failed: {}", result.error);
        std::fs::read(directory.path().join("frame_00000000.j2c")).expect("fixture codestream")
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    #[ignore = "a measurement, not a check"]
    fn one_frame_of_2k_cinema_decodes_and_composes_in() {
        const RUNS: u32 = 20;
        let codestream = cinema_2k_codestream();
        let display = Display::Srgb(XyzToSrgb::new());
        let path = std::path::Path::new("bench.j2c");
        println!("codestream is {} bytes", codestream.len());
        for reduce in 0..=2u8 {
            let start = std::time::Instant::now();
            let mut size = (0, 0);
            for _ in 0..RUNS {
                let frame = decode_job(
                    codestream.clone(),
                    reduce,
                    DisplayRender::DcpXyz,
                    path,
                    &display,
                )
                .expect("decode");
                // composing an overlay-free frame is the copy the scheduler makes
                let composed = frame.data.clone();
                std::hint::black_box(composed);
                size = (frame.width, frame.height);
            }
            println!(
                "reduce {reduce}: {:.1} ms a frame at {}x{}",
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(RUNS),
                size.0,
                size.1
            );
        }

        // grok's own pool, what the still path takes and what this trades away
        let start = std::time::Instant::now();
        for _ in 0..RUNS {
            let frame = preview::display_frame_from_codestream(
                codestream.clone(),
                0,
                preview::GROK_SHARED_THREAD_POOL,
                FrameRender::Dcp(&display),
                path,
            )
            .expect("decode");
            std::hint::black_box(frame.data.clone());
        }
        println!(
            "reduce 0 on grok's shared pool: {:.1} ms a frame",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(RUNS)
        );
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    #[ignore = "a measurement, not a check"]
    fn the_pool_sustains_frames_a_second_at_full_resolution() {
        const FRAMES: u64 = 96;
        let codestream = cinema_2k_codestream();
        let (finished, _drain) = std::sync::mpsc::channel();
        let pool = DecodePool::start(finished);
        println!("{} workers", pool.worker_count);

        let start = std::time::Instant::now();
        for frame_index in 0..FRAMES {
            pool.submit(DecodeJob {
                generation: 0,
                frame_index,
                codestream: codestream.clone(),
                reduce: 0,
                render: DisplayRender::DcpXyz,
                mxf: std::path::PathBuf::from("bench.j2c"),
            });
        }
        while (pool.cached_frame_count() as u64) < FRAMES {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "{FRAMES} frames in {elapsed:.2} s, {:.1} frames a second on {} workers",
            FRAMES as f64 / elapsed,
            pool.worker_count
        );
    }

    #[test]
    fn samples_scale_to_eight_bits_from_their_own_precision() {
        assert_eq!(to_eight_bits(4095, 12), 255);
        assert_eq!(to_eight_bits(2048, 12), 128);
        assert_eq!(to_eight_bits(0, 12), 0);
        assert_eq!(to_eight_bits(255, 8), 255);
        assert_eq!(to_eight_bits(3, 2), 255);
        assert_eq!(to_eight_bits(-7, 12), 0);
    }
}
