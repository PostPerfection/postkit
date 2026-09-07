mod compositor;
mod decode_pool;
mod presenter;
mod scheduler;
mod timeline;

use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use presenter::PictureRectangle;

// the signature libmpv's mpv_opengl_init_params takes, declared here so this player needs no libmpv
pub type GetProcAddressFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeScale {
    Full,
    Half,
    Quarter,
}

impl DecodeScale {
    fn reduce(self) -> u8 {
        match self {
            DecodeScale::Full => 0,
            DecodeScale::Half => 1,
            DecodeScale::Quarter => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSlot {
    Subtitle,
    Caption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// stated in full-resolution source pixels whatever the decode scale is
pub struct OverlayRectangle {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub colour: [u8; 3],
    pub alpha: u8,
}

const RGBA_BYTES_PER_PIXEL: usize = 4;
const OPAQUE_ALPHA: u8 = 255;

// a 4K gl upload costs 12.3 ms as rgb8 against 2.3 ms as rgba8 on the amd display gpu
struct Rgba8Frame {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Rgba8Frame {
    fn from_rgb8(picture: &crate::preview::Rgb8Frame) -> Self {
        const RGB_BYTES_PER_PIXEL: usize = 3;
        let pixels = picture.width as usize * picture.height as usize;
        let mut data = Vec::with_capacity(pixels * RGBA_BYTES_PER_PIXEL);
        for colour in picture.data.as_chunks::<RGB_BYTES_PER_PIXEL>().0 {
            data.extend_from_slice(colour);
            data.push(OPAQUE_ALPHA);
        }
        Rgba8Frame {
            width: picture.width,
            height: picture.height,
            data,
        }
    }
}

enum ComposedPixels {
    // nothing drawn, so the decoded frame is shared
    Picture(Arc<Rgba8Frame>),
    Drawn(Vec<u8>),
}

struct ComposedFrame {
    width: u32,
    height: u32,
    pixels: ComposedPixels,
}

impl ComposedFrame {
    fn data(&self) -> &[u8] {
        match &self.pixels {
            ComposedPixels::Picture(picture) => &picture.data,
            ComposedPixels::Drawn(data) => data,
        }
    }
}

const RENDER_TIMING_VARIABLE: &str = "POSTKIT_RENDER_TIMING";
const RENDER_TIMING_WINDOW_FRAMES: usize = 96;
const MILLISECONDS_PER_SECOND: f64 = 1000.0;

thread_local! {
    static RENDER_TIMINGS: RefCell<Vec<(Duration, Duration)>> = const { RefCell::new(Vec::new()) };
}

fn render_timing_wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| std::env::var_os(RENDER_TIMING_VARIABLE).is_some())
}

fn record_render_timing(upload: Duration, call: Duration) {
    if !render_timing_wanted() {
        return;
    }
    let window = RENDER_TIMINGS.with_borrow_mut(|timings| {
        timings.push((upload, call));
        if timings.len() < RENDER_TIMING_WINDOW_FRAMES {
            return None;
        }
        Some(std::mem::take(timings))
    });
    let Some(window) = window else {
        return;
    };
    let (uploads, calls): (Vec<Duration>, Vec<Duration>) = window.into_iter().unzip();
    eprintln!(
        "grok player render: {RENDER_TIMING_WINDOW_FRAMES} frames, upload {}, whole call {}",
        milliseconds_spread(uploads),
        milliseconds_spread(calls)
    );
}

fn milliseconds_spread(mut costs: Vec<Duration>) -> String {
    costs.sort_unstable();
    let milliseconds = |cost: Duration| cost.as_secs_f64() * MILLISECONDS_PER_SECOND;
    let total: Duration = costs.iter().sum();
    format!(
        "mean {:.1} median {:.1} max {:.1} ms",
        milliseconds(total) / costs.len() as f64,
        milliseconds(costs[costs.len() / 2]),
        milliseconds(costs[costs.len() - 1])
    )
}

#[derive(Default, Clone)]
struct Status {
    position: Option<f64>,
    duration: Option<f64>,
    paused: bool,
    filename: Option<String>,
    dropped_frames: u64,
    delayed_frames: u64,
    decoder_fps: Option<f64>,
    container_fps: Option<f64>,
    eof: bool,
}

type UpdateCallback = Box<dyn Fn() + Send + 'static>;

enum Command {
    Load(PathBuf, Sender<Result<(), String>>),
    Stop,
    SetPaused(bool),
    TogglePause,
    Seek(f64),
    SeekAbsolute(f64),
    Step(i64),
    SetDecodeScale(DecodeScale),
    SetSubtitleFile(SubtitleSlot, Option<PathBuf>, Sender<Result<(), String>>),
    SetSubtitleVisibility(SubtitleSlot, bool),
    SetOverlay(Vec<OverlayRectangle>),
    DecodeFinished,
    Shutdown,
}

struct Shared {
    frame: Mutex<Option<Arc<ComposedFrame>>>,
    serial: AtomicU64,
    acknowledged_serial: AtomicU64,
    status: Mutex<Status>,
    source_size: Mutex<Option<(u32, u32)>>,
    callback: Mutex<Option<UpdateCallback>>,
    cached_frames: AtomicUsize,
    lookahead_frames: AtomicUsize,
}

impl Shared {
    fn new() -> Self {
        Shared {
            frame: Mutex::new(None),
            serial: AtomicU64::new(0),
            acknowledged_serial: AtomicU64::new(0),
            status: Mutex::new(Status {
                paused: true,
                ..Status::default()
            }),
            source_size: Mutex::new(None),
            callback: Mutex::new(None),
            cached_frames: AtomicUsize::new(0),
            lookahead_frames: AtomicUsize::new(0),
        }
    }

    fn publish(&self, frame: ComposedFrame) {
        *self.frame.lock().unwrap() = Some(Arc::new(frame));
        self.serial.fetch_add(1, Ordering::AcqRel);
        self.fire_update();
    }

    fn clear_frame(&self) {
        *self.frame.lock().unwrap() = None;
        self.serial.fetch_add(1, Ordering::AcqRel);
    }

    fn current_frame(&self) -> Option<Arc<ComposedFrame>> {
        self.frame.lock().unwrap().clone()
    }

    fn fire_update(&self) {
        if let Some(callback) = self.callback.lock().unwrap().as_ref() {
            callback();
        }
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = status;
    }

    fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn set_source_size(&self, size: Option<(u32, u32)>) {
        *self.source_size.lock().unwrap() = size;
    }

    fn set_cached_frames(&self, count: usize) {
        self.cached_frames.store(count, Ordering::Release);
    }

    fn set_lookahead(&self, count: usize) {
        self.lookahead_frames.store(count, Ordering::Release);
    }
}

// callable from any thread except init_opengl and render_opengl, which need the gl context current
pub struct GrokPlayer {
    commands: Sender<Command>,
    shared: Arc<Shared>,
    scheduler: Mutex<Option<JoinHandle<()>>>,
    presenter: Mutex<Option<presenter::GlPresenter>>,
    software: AtomicBool,
}

impl Default for GrokPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokPlayer {
    pub fn new() -> Self {
        let (commands, receiver) = channel();
        let shared = Arc::new(Shared::new());
        let scheduler = {
            let shared = shared.clone();
            let finished = commands.clone();
            std::thread::spawn(move || scheduler::run(shared, receiver, finished))
        };
        GrokPlayer {
            commands,
            shared,
            scheduler: Mutex::new(Some(scheduler)),
            presenter: Mutex::new(None),
            software: AtomicBool::new(false),
        }
    }

    // stereoscopic essence is refused, the mono asdcplib reader cannot read it
    pub fn accepts(source: &Path) -> bool {
        timeline::accepts(source)
    }

    // ─── render backends ───────────────────────────────────────────────────

    pub fn init_opengl(
        &self,
        get_proc_address: GetProcAddressFn,
        get_proc_address_ctx: *mut c_void,
    ) -> Result<(), String> {
        let mut presenter = self.presenter.lock().unwrap();
        if presenter.is_some() {
            return Err("the GL presenter is already built".to_string());
        }
        *presenter = Some(presenter::GlPresenter::build(
            get_proc_address,
            get_proc_address_ctx,
        )?);
        Ok(())
    }

    pub fn init_software(&self) -> Result<(), String> {
        if self.software.swap(true, Ordering::AcqRel) {
            return Err("the software renderer is already set up".to_string());
        }
        Ok(())
    }

    // flip_y is true for a top-left origin surface
    pub fn render_opengl(
        &self,
        framebuffer: i32,
        width: i32,
        height: i32,
        flip_y: bool,
    ) -> Result<(), String> {
        let mut presenter = self.presenter.lock().unwrap();
        let presenter = presenter
            .as_mut()
            .ok_or_else(|| "no GL presenter".to_string())?;
        let frame = self.shared.current_frame();
        let serial = self.shared.serial.load(Ordering::Acquire);
        let started = Instant::now();
        let upload =
            presenter.draw(framebuffer, width, height, flip_y, frame.as_deref(), serial)?;
        if let Some(upload) = upload {
            record_render_timing(upload, started.elapsed());
        }
        Ok(())
    }

    pub fn render_software(
        &self,
        width: usize,
        height: usize,
        target: &mut [u8],
    ) -> Result<(), String> {
        if !self.software.load(Ordering::Acquire) {
            return Err("no software render context".to_string());
        }
        let needed = width * height * presenter::SOFTWARE_BYTES_PER_PIXEL;
        if target.len() < needed {
            return Err(format!(
                "target buffer holds {} bytes, needs {needed}",
                target.len()
            ));
        }
        let Some(frame) = self.shared.current_frame() else {
            target[..needed].fill(0);
            return Ok(());
        };
        presenter::draw_software(&frame, width, height, target)
    }

    pub fn picture_rectangle(
        &self,
        surface_width: u32,
        surface_height: u32,
    ) -> Option<PictureRectangle> {
        let frame = self.shared.current_frame()?;
        presenter::picture_rectangle(surface_width, surface_height, frame.width, frame.height)
    }

    // true once for each newly composed frame
    pub fn wants_redraw(&self) -> bool {
        let serial = self.shared.serial.load(Ordering::Acquire);
        self.shared
            .acknowledged_serial
            .swap(serial, Ordering::AcqRel)
            != serial
    }

    // fires on the scheduler thread, so it must not touch ui state directly
    pub fn set_update_callback<F: Fn() + Send + 'static>(&self, callback: F) {
        *self.shared.callback.lock().unwrap() = Some(Box::new(callback));
    }

    // ─── transport ─────────────────────────────────────────────────────────

    pub fn load(&self, source: &Path) -> Result<(), String> {
        let (reply, answer) = channel();
        self.send(Command::Load(source.to_path_buf(), reply))?;
        answer
            .recv()
            .map_err(|_| "the decode thread is gone".to_string())?
    }

    pub fn stop(&self) {
        let _ = self.send(Command::Stop);
    }

    pub fn play_pause(&self) {
        let _ = self.send(Command::TogglePause);
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.send(Command::SetPaused(paused));
    }

    pub fn paused(&self) -> bool {
        self.shared.status().paused
    }

    pub fn seek(&self, seconds: f64) {
        let _ = self.send(Command::Seek(seconds));
    }

    pub fn seek_absolute(&self, seconds: f64) {
        let _ = self.send(Command::SeekAbsolute(seconds));
    }

    // pauses, the way mpv's frame-step does
    pub fn frame_step(&self) {
        let _ = self.send(Command::Step(1));
    }

    pub fn frame_back_step(&self) {
        let _ = self.send(Command::Step(-1));
    }

    pub fn position(&self) -> Option<f64> {
        self.shared.status().position
    }

    pub fn duration(&self) -> Option<f64> {
        self.shared.status().duration
    }

    pub fn eof_reached(&self) -> bool {
        self.shared.status().eof
    }

    pub fn source_size(&self) -> Option<(u32, u32)> {
        *self.shared.source_size.lock().unwrap()
    }

    pub fn frame_size(&self) -> Option<(u32, u32)> {
        let frame = self.shared.current_frame()?;
        Some((frame.width, frame.height))
    }

    pub fn set_decode_scale(&self, scale: DecodeScale) {
        let _ = self.send(Command::SetDecodeScale(scale));
    }

    // ─── subtitles and overlays ────────────────────────────────────────────

    pub fn set_subtitle_file(&self, slot: SubtitleSlot, file: Option<&Path>) -> Result<(), String> {
        let (reply, answer) = channel();
        self.send(Command::SetSubtitleFile(
            slot,
            file.map(Path::to_path_buf),
            reply,
        ))?;
        answer
            .recv()
            .map_err(|_| "the decode thread is gone".to_string())?
    }

    pub fn set_subtitle_visibility(&self, slot: SubtitleSlot, visible: bool) {
        let _ = self.send(Command::SetSubtitleVisibility(slot, visible));
    }

    pub fn set_overlay(&self, rectangles: Vec<OverlayRectangle>) {
        let _ = self.send(Command::SetOverlay(rectangles));
    }

    // ─── what the transport bar polls ──────────────────────────────────────

    pub fn metadata_json(&self) -> String {
        let status = self.shared.status();
        format!(
            r#"{{"position": {}, "duration": {}, "paused": {}, "filename": {}, "dropped_frames": {}, "delayed_frames": {}, "cache_seconds": null, "decoder_fps": {}, "container_fps": {}, "eof": {}}}"#,
            json_number(status.position),
            json_number(status.duration),
            status.paused,
            json_string(status.filename.as_deref()),
            status.dropped_frames,
            status.delayed_frames,
            json_number(status.decoder_fps),
            json_number(status.container_fps),
            status.eof,
        )
    }

    pub fn cached_frame_count(&self) -> usize {
        self.shared.cached_frames.load(Ordering::Acquire)
    }

    pub fn lookahead_frames(&self) -> usize {
        self.shared.lookahead_frames.load(Ordering::Acquire)
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the decode thread is gone".to_string())
    }
}

impl GrokPlayer {
    /// Stops the scheduler and with it the decode pool, which ends a running device batch.
    /// An app calls this before it exits: a batch left open holds the plugin's host callbacks,
    /// and the CUDA teardown at process exit waits on them for a long time.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(scheduler) = self.scheduler.lock().unwrap().take() {
            let _ = scheduler.join();
        }
    }
}

impl Drop for GrokPlayer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn json_number(value: Option<f64>) -> String {
    match value {
        Some(number) if number.is_finite() => number.to_string(),
        _ => "null".to_string(),
    }
}

fn json_string(value: Option<&str>) -> String {
    match value {
        Some(text) => format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_of_an_empty_player_reads_as_unknown() {
        let player = GrokPlayer::new();
        let metadata = player.metadata_json();
        assert!(metadata.contains(r#""position": null"#), "{metadata}");
        assert!(metadata.contains(r#""duration": null"#), "{metadata}");
        assert!(metadata.contains(r#""paused": true"#), "{metadata}");
        assert!(metadata.contains(r#""filename": null"#), "{metadata}");
        assert!(metadata.contains(r#""cache_seconds": null"#), "{metadata}");
        assert!(metadata.contains(r#""eof": false"#), "{metadata}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&metadata)
                .expect("metadata is JSON")
                .as_object()
                .map(|fields| fields.len()),
            Some(10)
        );
    }

    #[test]
    fn a_quoted_title_survives_the_json_escape() {
        assert_eq!(json_string(Some(r#"a "b" \ c"#)), r#""a \"b\" \\ c""#);
        assert_eq!(json_string(None), "null");
    }

    #[test]
    fn the_decode_scale_is_the_grok_reduce_level() {
        assert_eq!(DecodeScale::Full.reduce(), 0);
        assert_eq!(DecodeScale::Half.reduce(), 1);
        assert_eq!(DecodeScale::Quarter.reduce(), 2);
    }
}
