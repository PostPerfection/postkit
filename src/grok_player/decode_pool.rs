use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::Command;
use super::timeline::DisplayRender;
use crate::colour::XyzToSrgb;
use crate::grok_decoder::DecodedFrame;
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

enum Popped<T> {
    Job(T),
    Timeout,
    Closed,
}

struct JobQueue<T> {
    jobs: Mutex<Option<VecDeque<T>>>,
    ready: Condvar,
}

impl<T> JobQueue<T> {
    fn new() -> Arc<Self> {
        Arc::new(JobQueue {
            jobs: Mutex::new(Some(VecDeque::new())),
            ready: Condvar::new(),
        })
    }

    fn push(&self, job: T) {
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

    fn pop(&self) -> Option<T> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            let queue = jobs.as_mut()?;
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
            jobs = self.ready.wait(jobs).unwrap();
        }
    }

    fn pop_timeout(&self, timeout: Duration) -> Popped<T> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            let Some(queue) = jobs.as_mut() else {
                return Popped::Closed;
            };
            if let Some(job) = queue.pop_front() {
                return Popped::Job(job);
            }
            let (guard, waited) = self.ready.wait_timeout(jobs, timeout).unwrap();
            jobs = guard;
            if waited.timed_out() {
                return Popped::Timeout;
            }
        }
    }

    fn push_front(&self, job: T) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(queue) = jobs.as_mut() {
            queue.push_front(job);
            self.ready.notify_one();
        }
    }

    fn drain(&self) -> Vec<T> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.as_mut()
            .map(|queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    // blocks while the queue is empty and `open` holds, None once either ends
    fn pop_while_open(&self, open: &std::sync::atomic::AtomicBool) -> Option<T> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            if !open.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            let queue = jobs.as_mut()?;
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
            jobs = self.ready.wait(jobs).unwrap();
        }
    }

    fn wake_all(&self) {
        let _jobs = self.jobs.lock().unwrap();
        self.ready.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.jobs.lock().unwrap().is_none()
    }

    fn is_empty(&self) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|queue| queue.is_empty())
    }

    fn close(&self) {
        *self.jobs.lock().unwrap() = None;
        self.ready.notify_all();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Device,
    Cpu,
}

// the device decodes full resolution only, and only while the plugin is switched on
fn route(reduce: u8, device_switched_on: bool) -> Route {
    if reduce == 0 && device_switched_on {
        Route::Device
    } else {
        Route::Cpu
    }
}

pub(super) struct DecodePool {
    cpu_queue: Arc<JobQueue<DecodeJob>>,
    device_queue: Option<Arc<JobQueue<DecodeJob>>>,
    workers: Vec<JoinHandle<()>>,
    device_thread: Option<JoinHandle<()>>,
    cache: Arc<Mutex<FrameCache>>,
    pub worker_count: usize,
}

impl DecodePool {
    pub fn start(finished: Sender<Command>) -> Self {
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(FALLBACK_WORKER_COUNT);
        let cpu_queue = JobQueue::new();
        let cache = Arc::new(Mutex::new(FrameCache::new()));
        let workers = (0..worker_count)
            .map(|_| {
                let queue = cpu_queue.clone();
                let cache = cache.clone();
                let finished = finished.clone();
                std::thread::spawn(move || run_worker(&queue, &cache, &finished))
            })
            .collect();
        let (device_queue, device_thread) = device::start(&cpu_queue, &cache, &finished);
        DecodePool {
            cpu_queue,
            device_queue,
            workers,
            device_thread,
            cache,
            worker_count,
        }
    }

    pub fn lookahead_frames(&self) -> usize {
        self.worker_count * LOOKAHEAD_FRAMES_PER_WORKER
    }

    pub fn submit(&self, job: DecodeJob) {
        let device_switched_on = crate::grok_encoder::gpu_active();
        match (&self.device_queue, route(job.reduce, device_switched_on)) {
            (Some(device_queue), Route::Device) => device_queue.push(job),
            _ => self.cpu_queue.push(job),
        }
    }

    pub fn restart(&self, generation: u64) {
        self.cpu_queue.discard_queued();
        if let Some(device_queue) = &self.device_queue {
            device_queue.discard_queued();
        }
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
        // the device thread ends its batch first, its last frames land on the cpu queue
        if let Some(device_queue) = &self.device_queue {
            device_queue.close();
        }
        if let Some(device_thread) = self.device_thread.take() {
            let _ = device_thread.join();
        }
        self.cpu_queue.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run_worker(queue: &JobQueue<DecodeJob>, cache: &Mutex<FrameCache>, finished: &Sender<Command>) {
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
    let decoded =
        crate::grok_decoder::decode_with_threads(codestream, reduce, SINGLE_DECODE_THREAD)?;
    render_decoded(&decoded, render, mxf, display)
}

fn render_decoded(
    decoded: &DecodedFrame,
    render: DisplayRender,
    mxf: &std::path::Path,
    display: &Display,
) -> Result<Rgb8Frame, String> {
    let render = match render {
        DisplayRender::DcpXyz => FrameRender::Dcp(display),
        DisplayRender::Imf(ref colour) => FrameRender::Imf(colour),
        DisplayRender::PlainRgb => return plain_rgb8(decoded, mxf),
    };
    preview::display_frame_from_decoded(decoded, render, mxf).map_err(|e| e.to_string())
}

fn plain_rgb8(decoded: &DecodedFrame, mxf: &std::path::Path) -> Result<Rgb8Frame, String> {
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

// the plugin's in-memory decode batch: its workers pull frames from the device queue,
// its threads hand decoded planes back, one backend thread begins and ends the batch
#[cfg(feature = "grok-ffi")]
mod device {
    use super::*;
    use crate::device_lease::{DEVICE_LEASE, PlaybackLease};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // how often the backend looks for an encode waiting on the device or a stalled tail
    const POLL: Duration = Duration::from_millis(100);
    // polls with frames inside the pipeline, nothing queued and no frame back before the
    // batch is drained: the pipeline holds a cluster of finished frames behind an empty pull
    const IDLE_POLLS_BEFORE_DRAIN: u32 = 5;

    pub(super) fn start(
        cpu_queue: &Arc<JobQueue<DecodeJob>>,
        cache: &Arc<Mutex<FrameCache>>,
        finished: &Sender<Command>,
    ) -> (Option<Arc<JobQueue<DecodeJob>>>, Option<JoinHandle<()>>) {
        let queue = JobQueue::new();
        let backend = Backend {
            state: Box::new(CallbackState {
                queue: queue.clone(),
                cpu_queue: cpu_queue.clone(),
                cache: cache.clone(),
                finished: finished.clone(),
                display: Display::Srgb(XyzToSrgb::new()),
                pull_open: AtomicBool::new(false),
                srgb8_on_device: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                returned: AtomicUsize::new(0),
            }),
            batch: None,
            declined_generation: None,
            idle_polls: 0,
            returned_at_last_poll: 0,
        };
        let thread = std::thread::spawn(move || backend.run());
        (Some(queue), Some(thread))
    }

    // what the plugin's pull and frame callbacks need, one per pool
    struct CallbackState {
        queue: Arc<JobQueue<DecodeJob>>,
        cpu_queue: Arc<JobQueue<DecodeJob>>,
        cache: Arc<Mutex<FrameCache>>,
        finished: Sender<Command>,
        display: Display,
        // false makes every pull return false, which ends the plugin's workers
        pull_open: AtomicBool,
        // the device hands back 8 bit sRGB frames instead of X'Y'Z' planes
        srgb8_on_device: AtomicBool,
        in_flight: AtomicUsize,
        returned: AtomicUsize,
    }

    // travels through the plugin as the frame's user pointer and owns the bytes it reads
    struct FrameContext {
        generation: u64,
        frame_index: u64,
        render: DisplayRender,
        mxf: PathBuf,
        codestream: Vec<u8>,
    }

    struct RunningBatch {
        _lease: PlaybackLease<'static>,
    }

    struct Backend {
        state: Box<CallbackState>,
        batch: Option<RunningBatch>,
        // the plugin declined this generation's shape, its frames stay on the cpu
        declined_generation: Option<u64>,
        idle_polls: u32,
        returned_at_last_poll: usize,
    }

    enum Begin {
        Running,
        Declined,
        DeviceBusy,
    }

    impl Backend {
        fn run(mut self) {
            loop {
                // while a batch runs the queue belongs to the plugin's pulls
                if self.batch.is_some() {
                    std::thread::sleep(POLL);
                    if self.state.queue.is_closed() {
                        break;
                    }
                    self.poll();
                    continue;
                }
                match self.state.queue.pop_timeout(POLL) {
                    Popped::Closed => break,
                    Popped::Timeout => {}
                    Popped::Job(job) => self.handle(job),
                }
            }
            self.end_batch();
        }

        // give the device to a waiting encode, or drain a stalled tail
        fn poll(&mut self) {
            if DEVICE_LEASE.encode_wants_the_device() {
                self.end_batch();
                return;
            }
            let in_flight = self.state.in_flight.load(Ordering::Acquire);
            let returned = self.state.returned.load(Ordering::Acquire);
            let stalled = in_flight > 0
                && self.state.queue.is_empty()
                && returned == self.returned_at_last_poll;
            self.returned_at_last_poll = returned;
            self.idle_polls = if stalled { self.idle_polls + 1 } else { 0 };
            if self.idle_polls >= IDLE_POLLS_BEFORE_DRAIN {
                self.end_batch();
            }
        }

        // the first job of a batch is its shape; the plugin's workers pull it back out
        fn handle(&mut self, job: DecodeJob) {
            self.idle_polls = 0;
            if DEVICE_LEASE.encode_wants_the_device() {
                self.end_batch();
                self.to_cpu(job);
                return;
            }
            if self.declined_generation == Some(job.generation) {
                self.to_cpu(job);
                return;
            }
            if self.batch.is_none() {
                match self.begin_batch(&job) {
                    Begin::Running => {}
                    Begin::Declined => {
                        self.declined_generation = Some(job.generation);
                        self.to_cpu(job);
                        return;
                    }
                    Begin::DeviceBusy => {
                        self.to_cpu(job);
                        return;
                    }
                }
            }
            self.state.queue.push_front(job);
        }

        fn to_cpu(&self, job: DecodeJob) {
            self.state.cpu_queue.push(job);
        }

        fn begin_batch(&mut self, shape: &DecodeJob) -> Begin {
            let Some(lease) = DEVICE_LEASE.try_acquire_for_playback() else {
                return Begin::DeviceBusy;
            };
            self.state.pull_open.store(true, Ordering::Release);
            // only a DCP frame is X'Y'Z', the other renders keep their planes
            let mut srgb8_on_device = false;
            let info = grokj2k_sys::grk_plugin_batch_decompress_memory_info {
                codestream: shape.codestream.as_ptr(),
                codestream_length: shape.codestream.len(),
                pull: Some(pull_frame),
                callback: Some(frame_callback),
                user: self.state.as_ref() as *const CallbackState as *mut c_void,
                srgb8_output: matches!(shape.render, DisplayRender::DcpXyz),
                srgb8_on_device: &mut srgb8_on_device,
            };
            match unsafe { grokj2k_sys::grk_plugin_batch_decompress_memory_begin(info) } {
                0 => {
                    self.state
                        .srgb8_on_device
                        .store(srgb8_on_device, Ordering::Release);
                    eprintln!(
                        "grok player decode backend: device, colour on the {}",
                        if srgb8_on_device { "device" } else { "cpu" }
                    );
                    self.batch = Some(RunningBatch { _lease: lease });
                    self.returned_at_last_poll = self.state.returned.load(Ordering::Acquire);
                    self.idle_polls = 0;
                    Begin::Running
                }
                1 => {
                    self.state.pull_open.store(false, Ordering::Release);
                    Begin::Declined
                }
                code => {
                    self.state.pull_open.store(false, Ordering::Release);
                    tracing::warn!(
                        "grok's accelerator plugin failed to start a decode batch: \
                         grk_plugin_batch_decompress_memory_begin returned {code}"
                    );
                    Begin::Declined
                }
            }
        }

        // every pulled frame has reached the frame callback when this returns; jobs
        // still queued wait for the next batch or go to the cpu when an encode holds it
        fn end_batch(&mut self) {
            if self.batch.take().is_none() {
                return;
            }
            self.state.pull_open.store(false, Ordering::Release);
            self.state.queue.wake_all();
            if !unsafe { grokj2k_sys::grk_plugin_batch_decompress_memory_end() } {
                tracing::warn!("grok's accelerator plugin failed to drain the decode batch");
            }
            eprintln!("grok player decode backend: cpu");
            if DEVICE_LEASE.encode_wants_the_device() {
                for job in self.state.queue.drain() {
                    self.to_cpu(job);
                }
            }
        }
    }

    impl FrameContext {
        fn into_job(self) -> DecodeJob {
            DecodeJob {
                generation: self.generation,
                frame_index: self.frame_index,
                codestream: self.codestream,
                reduce: 0,
                render: self.render,
                mxf: self.mxf,
            }
        }
    }

    // runs on the plugin's worker threads, several at once, blocking while the queue is empty
    unsafe extern "C" fn pull_frame(
        user: *mut c_void,
        codestream: *mut *const u8,
        length: *mut usize,
        frame_user: *mut *mut c_void,
    ) -> bool {
        let state = unsafe { &*(user as *const CallbackState) };
        let job = loop {
            let Some(job) = state.queue.pop_while_open(&state.pull_open) else {
                return false;
            };
            // an srgb8 batch transforms every frame, so a frame in another colour goes to the cpu
            let transformed_on_device = state.srgb8_on_device.load(Ordering::Acquire);
            if transformed_on_device && !matches!(job.render, DisplayRender::DcpXyz) {
                state.cpu_queue.push(job);
                continue;
            }
            break job;
        };
        let context = Box::new(FrameContext {
            generation: job.generation,
            frame_index: job.frame_index,
            render: job.render,
            mxf: job.mxf,
            codestream: job.codestream,
        });
        state.in_flight.fetch_add(1, Ordering::AcqRel);
        unsafe {
            *codestream = context.codestream.as_ptr();
            *length = context.codestream.len();
            *frame_user = Box::into_raw(context) as *mut c_void;
        }
        true
    }

    // runs on the plugin's threads, concurrently with itself
    unsafe extern "C" fn frame_callback(
        user: *mut c_void,
        frame: *mut c_void,
        image: *const grokj2k_sys::grk_image,
    ) {
        let state = unsafe { &*(user as *const CallbackState) };
        let context = unsafe { Box::from_raw(frame as *mut FrameContext) };
        if image.is_null() {
            // the device refused this one frame, the cpu pool takes it
            state.cpu_queue.push(context.into_job());
        } else {
            // colour runs here, on the plugin's thread: a queued copy of the planes is
            // 84 MB a 4K frame and the pipeline runs ahead of the colour workers
            let image = unsafe { &*image };
            let rendered = if state.srgb8_on_device.load(Ordering::Acquire) {
                read_device_rgb8(image)
            } else {
                read_device_image(image).and_then(|decoded| {
                    render_decoded(&decoded, context.render, &context.mxf, &state.display)
                })
            };
            let cached = match rendered {
                Ok(frame) => {
                    crate::grok_encoder::count_batch_accelerated_frame();
                    CachedFrame::Decoded(Arc::new(frame))
                }
                Err(reason) => CachedFrame::Failed(reason),
            };
            state
                .cache
                .lock()
                .unwrap()
                .store(context.generation, context.frame_index, cached);
            let _ = state.finished.send(Command::DecodeFinished);
        }
        state.in_flight.fetch_sub(1, Ordering::AcqRel);
        state.returned.fetch_add(1, Ordering::AcqRel);
    }

    // the device's interleaved 8 bit sRGB frame: comps[0] carries the whole buffer
    fn read_device_rgb8(image: &grokj2k_sys::grk_image) -> Result<Rgb8Frame, String> {
        if image.comps.is_null() || image.numcomps < RGB_COMPONENT_COUNT as u16 {
            return Err("the device returned an sRGB frame without three components".to_string());
        }
        let first = unsafe { &*image.comps };
        if first.data.is_null() || first.prec != DISPLAY_PRECISION_BITS {
            return Err(format!(
                "the device returned an sRGB frame at {} bits without a buffer",
                first.prec
            ));
        }
        let (width, height) = (first.w as usize, first.h as usize);
        let row_bytes = width * RGB_COMPONENT_COUNT;
        let stride = if first.stride == 0 {
            row_bytes
        } else {
            first.stride as usize
        };
        let mut data = Vec::with_capacity(row_bytes * height);
        for row in 0..height {
            let start = unsafe { (first.data as *const u8).add(row * stride) };
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(start, row_bytes) });
        }
        Ok(Rgb8Frame {
            width: first.w,
            height: first.h,
            data,
        })
    }

    // the planes are the plugin's until the callback returns, so they are copied here
    fn read_device_image(image: &grokj2k_sys::grk_image) -> Result<DecodedFrame, String> {
        let count = image.numcomps as usize;
        if image.comps.is_null() || count == 0 {
            return Err("the device returned a frame with no components".to_string());
        }
        let comps = unsafe { std::slice::from_raw_parts(image.comps, count) };
        let (width, height, precision) = (comps[0].w, comps[0].h, comps[0].prec);
        let mut components = Vec::with_capacity(count);
        for (index, comp) in comps.iter().enumerate() {
            if comp.data.is_null() || comp.w != width || comp.h != height {
                return Err(format!(
                    "the device returned component {index} as {}x{}, not the frame's {width}x{height}",
                    comp.w, comp.h
                ));
            }
            let stride = if comp.stride == 0 {
                width as usize
            } else {
                comp.stride as usize
            };
            let mut plane = Vec::with_capacity(width as usize * height as usize);
            for row in 0..height as usize {
                let start = unsafe { (comp.data as *const i32).add(row * stride) };
                plane.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(start, width as usize)
                });
            }
            components.push(plane);
        }
        Ok(DecodedFrame {
            width,
            height,
            precision,
            components,
            chroma_subsampled: false,
        })
    }
}

#[cfg(not(feature = "grok-ffi"))]
mod device {
    use super::*;

    pub(super) fn start(
        _cpu_queue: &Arc<JobQueue<DecodeJob>>,
        _cache: &Arc<Mutex<FrameCache>>,
        _finished: &Sender<Command>,
    ) -> (Option<Arc<JobQueue<DecodeJob>>>, Option<JoinHandle<()>>) {
        (None, None)
    }
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
        cinema_2k_codestream_of_frame(0)
    }

    #[cfg(feature = "grok-ffi")]
    fn cinema_2k_codestream_of_frame(frame_index: u32) -> Vec<u8> {
        const WIDTH: u32 = 2048;
        const HEIGHT: u32 = 1080;
        const TWELVE_BIT_MAX: i32 = 4095;
        const CINEMA_2K_PROFILE: u16 = 0x0003;
        const DCI_COMPRESSION_RATIO: f64 = 12.0;
        const DCI_RESOLUTIONS: u8 = 6;

        let samples = (WIDTH * HEIGHT) as usize;
        let plane = |seed: u32| -> Vec<i32> {
            let seed = seed.wrapping_add(frame_index.wrapping_mul(31));
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

    #[test]
    fn only_a_full_resolution_frame_goes_to_a_switched_on_device() {
        assert_eq!(route(0, true), Route::Device);
        assert_eq!(
            route(1, true),
            Route::Cpu,
            "a reduced decode stays on the cpu"
        );
        assert_eq!(route(0, false), Route::Cpu, "the plugin is off");
    }

    // every frame of a run through a pool, in frame order
    #[cfg(feature = "grok-gpu")]
    fn decode_run_through_a_pool(codestreams: &[Vec<u8>]) -> Vec<Arc<Rgb8Frame>> {
        let (finished, done) = std::sync::mpsc::channel();
        let pool = DecodePool::start(finished);
        pool.restart(1);
        for (index, codestream) in codestreams.iter().enumerate() {
            pool.submit(DecodeJob {
                generation: 1,
                frame_index: index as u64,
                codestream: codestream.clone(),
                reduce: 0,
                render: DisplayRender::DcpXyz,
                mxf: PathBuf::from("run.j2c"),
            });
        }
        for _ in 0..codestreams.len() {
            done.recv_timeout(Duration::from_secs(60))
                .expect("a frame within a minute");
        }
        (0..codestreams.len() as u64)
            .map(|index| {
                pool.decoded(index)
                    .unwrap_or_else(|| panic!("frame {index}: {:?}", pool.failure(index)))
            })
            .collect()
    }

    // the device's inverse wavelet rounds differently from the cpu's on noisy content:
    // 99.6 percent of samples identical here, and near black the sRGB curve turns a
    // code of X'Y'Z' into several of 255
    #[cfg(feature = "grok-gpu")]
    #[test]
    fn the_device_pool_shows_the_frames_the_cpu_pool_shows() {
        const FRAMES: u32 = 12;
        const EIGHT_BIT_TOLERANCE: i32 = 8;
        const SAMPLES_OVER_ONE_CODE_PER_MILLION: usize = 1000;
        let codestreams: Vec<Vec<u8>> = (0..FRAMES).map(cinema_2k_codestream_of_frame).collect();
        crate::grok_encoder::use_cpu();
        let on_cpu = decode_run_through_a_pool(&codestreams);

        if let Err(reason) = crate::grok_encoder::use_gpu_from_environment() {
            panic!("{reason}");
        }
        let before = crate::grok_encoder::accelerated_frames();
        let on_device = decode_run_through_a_pool(&codestreams);
        let device_frames = crate::grok_encoder::accelerated_frames() - before;
        crate::grok_encoder::use_cpu();
        assert_eq!(
            device_frames,
            u64::from(FRAMES),
            "the device decoded {device_frames} of {FRAMES} frames"
        );

        for (index, (device, cpu)) in on_device.iter().zip(&on_cpu).enumerate() {
            assert_eq!(
                (device.width, device.height),
                (cpu.width, cpu.height),
                "frame {index}"
            );
            let mut histogram = [0usize; 256];
            let mut first_large = None;
            for (position, (a, b)) in device.data.iter().zip(&cpu.data).enumerate() {
                let difference = (i32::from(*a) - i32::from(*b)).unsigned_abs() as usize;
                histogram[difference] += 1;
                if difference > EIGHT_BIT_TOLERANCE as usize && first_large.is_none() {
                    first_large = Some((position, *a, *b));
                }
            }
            let worst = histogram.iter().rposition(|count| *count > 0).unwrap_or(0);
            let over_one: usize = histogram[2..].iter().sum();
            println!(
                "frame {index}: differences 0:{} 1:{} 2:{} 3:{} 4+:{}, first over tolerance {:?}",
                histogram[0],
                histogram[1],
                histogram[2],
                histogram[3],
                histogram[4..].iter().sum::<usize>(),
                first_large
            );
            assert!(
                worst <= EIGHT_BIT_TOLERANCE as usize,
                "frame {index}: device and cpu renders differ by {worst} of 255"
            );
            assert!(
                over_one * 1_000_000 <= device.data.len() * SAMPLES_OVER_ONE_CODE_PER_MILLION,
                "frame {index}: {over_one} of {} samples differ by more than one code",
                device.data.len()
            );
        }
    }

    // POSTKIT_BENCH_SOURCE names a DCP directory, a picture MXF or a codestream directory
    #[cfg(feature = "grok-ffi")]
    #[test]
    #[ignore = "a measurement, not a check"]
    fn one_frame_of_the_bench_source_costs_on_the_host_and_the_device() {
        const RUNS: u32 = 10;
        let source = std::env::var("POSTKIT_BENCH_SOURCE").expect("POSTKIT_BENCH_SOURCE");
        let mut timeline =
            super::super::timeline::Timeline::open(std::path::Path::new(&source)).expect("open");
        let (codestream, render, mxf) = timeline.codestream(0).expect("frame 0");
        crate::grok_encoder::initialize(0);
        println!(
            "{}x{}, {} frames, frame 0 is {} bytes",
            timeline.width,
            timeline.height,
            timeline.frame_count,
            codestream.len()
        );
        let display = Display::Srgb(XyzToSrgb::new());
        let milliseconds = |label: &str, step: &mut dyn FnMut()| {
            let start = std::time::Instant::now();
            for _ in 0..RUNS {
                step();
            }
            println!(
                "{label}: {:.1} ms",
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(RUNS)
            );
        };

        milliseconds("decode, one grok thread", &mut || {
            let decoded = crate::grok_decoder::decode_with_threads(
                codestream.clone(),
                0,
                SINGLE_DECODE_THREAD,
            )
            .expect("decode");
            std::hint::black_box(decoded);
        });
        milliseconds("decode, grok's pool", &mut || {
            let decoded = crate::grok_decoder::decode_with_threads(
                codestream.clone(),
                0,
                preview::GROK_SHARED_THREAD_POOL,
            )
            .expect("decode");
            std::hint::black_box(decoded);
        });
        let decoded =
            crate::grok_decoder::decode_with_threads(codestream.clone(), 0, SINGLE_DECODE_THREAD)
                .expect("decode");
        milliseconds("pack to xyz12le", &mut || {
            std::hint::black_box(decoded.to_xyz12le().expect("pack"));
        });
        let raw = decoded.to_xyz12le().expect("pack");
        let mut rgb8 = Vec::new();
        milliseconds("display transform, xyz12le to rgb8", &mut || {
            display.apply(&raw, &mut rgb8);
            std::hint::black_box(&rgb8);
        });
        milliseconds("decode_job, decode and colour on one thread", &mut || {
            let frame = decode_job(codestream.clone(), 0, render, &mxf, &display).expect("decode");
            std::hint::black_box(frame);
        });

        #[cfg(feature = "grok-gpu")]
        match crate::grok_encoder::use_gpu_from_environment() {
            Ok(()) => {
                let before = crate::grok_encoder::accelerated_frames();
                milliseconds("decode on the device, one grok thread", &mut || {
                    let decoded = crate::grok_decoder::decode_with_threads(
                        codestream.clone(),
                        0,
                        SINGLE_DECODE_THREAD,
                    )
                    .expect("decode");
                    std::hint::black_box(decoded);
                });
                let device_frames = crate::grok_encoder::accelerated_frames() - before;
                println!("the device took {device_frames} of {RUNS} decodes");
                milliseconds("decode_job on the device, decode and colour", &mut || {
                    let frame =
                        decode_job(codestream.clone(), 0, render, &mxf, &display).expect("decode");
                    std::hint::black_box(frame);
                });
                crate::grok_encoder::use_cpu();
            }
            Err(reason) => println!("no device numbers: {reason}"),
        }
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
        let frames: u64 = std::env::var("POSTKIT_BENCH_FRAMES")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(96);
        let codestream = match std::env::var("POSTKIT_BENCH_SOURCE") {
            Ok(source) => {
                let mut timeline =
                    super::super::timeline::Timeline::open(std::path::Path::new(&source))
                        .expect("open");
                timeline.codestream(0).expect("frame 0").0
            }
            Err(_) => cinema_2k_codestream(),
        };
        let sustain = |label: &str| {
            let (finished, _drain) = std::sync::mpsc::channel();
            let pool = DecodePool::start(finished);
            let start = std::time::Instant::now();
            for frame_index in 0..frames {
                pool.submit(DecodeJob {
                    generation: 0,
                    frame_index,
                    codestream: codestream.clone(),
                    reduce: 0,
                    render: DisplayRender::DcpXyz,
                    mxf: std::path::PathBuf::from("bench.j2c"),
                });
            }
            let mut first_frame_after = None;
            while (pool.cached_frame_count() as u64) < frames {
                if first_frame_after.is_none() && pool.cached_frame_count() > 0 {
                    first_frame_after = Some(start.elapsed().as_secs_f64());
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let elapsed = start.elapsed().as_secs_f64();
            let first = first_frame_after.unwrap_or(elapsed);
            println!(
                "{label}: {frames} frames in {elapsed:.2} s, {:.1} frames a second on {} workers, first frame after {first:.2} s, {:.1} frames a second after it",
                frames as f64 / elapsed,
                pool.worker_count,
                (frames - 1) as f64 / (elapsed - first).max(1e-9)
            );
        };
        crate::grok_encoder::use_cpu();
        sustain("cpu pool");

        #[cfg(feature = "grok-gpu")]
        match crate::grok_encoder::use_gpu_from_environment() {
            Ok(()) => {
                let before = crate::grok_encoder::accelerated_frames();
                sustain("device batch");
                println!(
                    "the device took {} of {frames} decodes",
                    crate::grok_encoder::accelerated_frames() - before
                );
                crate::grok_encoder::use_cpu();
            }
            Err(reason) => println!("no device numbers: {reason}"),
        }
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
