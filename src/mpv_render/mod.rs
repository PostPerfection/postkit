//! In-process video playback through libmpv's render API.
//!
//! The player owns an mpv instance whose video output is a render context the
//! caller drives: an OpenGL framebuffer for on-screen playback, or a plain RGBA
//! buffer for tests and screenshots. Native Wayland cannot reparent another
//! process's window, so an embedded preview has to render in-process.
//!
//! Threading follows libmpv's rule: every method here may be called from any
//! thread except [`MpvRenderPlayer::render_opengl`], which must run on the
//! thread holding the GL context the render context was created with.

mod ffi;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub use ffi::mpv_get_proc_address_fn;

use crate::mpv::pick_picture_mxf;

/// Pixel format libmpv writes in software mode, and the one `render_software`
/// promises its callers.
const SOFTWARE_PIXEL_FORMAT: &str = "rgb0";
const BYTES_PER_PIXEL: usize = 4;

/// Options applied before `mpv_initialize`. `vo=libmpv` is what makes mpv route
/// video to a render context instead of opening its own window.
const COMMON_OPTIONS: &[(&str, &str)] = &[
    ("vo", "libmpv"),
    ("terminal", "no"),
    ("idle", "yes"),
    ("keep-open", "yes"),
    ("osc", "no"),
    ("input-default-bindings", "no"),
    ("input-vo-keyboard", "no"),
];

/// Hardware decoding is negotiated by mpv; `auto-safe` falls back to software
/// decode rather than failing when VAAPI is unusable. Direct rendering is off
/// because it freezes playback for good against advanced control: the decoder's
/// buffers come from the render thread, which is waiting on the core.
const OPENGL_OPTIONS: &[(&str, &str)] = &[("hwdec", "auto-safe"), ("vd-lavc-dr", "no")];

/// The software renderer has no GPU to hand frames to, and a test machine may
/// have no sound device at all.
const SOFTWARE_OPTIONS: &[(&str, &str)] = &[("hwdec", "no"), ("audio", "no")];

/// The windowing system's display handle. Without it mpv cannot open a VA
/// display, and hardware decoding silently falls back to the CPU.
pub enum NativeDisplay {
    /// `struct wl_display *`
    Wayland(*mut c_void),
    /// `Display *`
    X11(*mut c_void),
}

pub struct MpvRenderPlayer {
    handle: *mut ffi::mpv_handle,
    render: Mutex<*mut ffi::mpv_render_context>,
    update_callback: Mutex<Option<*mut UpdateCallback>>,
    initialized: AtomicBool,
}

type UpdateCallback = Box<dyn Fn() + Send + 'static>;

// mpv_handle is documented as thread-safe, and the one call with a thread
// requirement (render_opengl) states it in its own docs.
unsafe impl Send for MpvRenderPlayer {}
unsafe impl Sync for MpvRenderPlayer {}

impl MpvRenderPlayer {
    /// Create and initialize an mpv instance with no render context yet. Call
    /// [`Self::init_opengl`] or [`Self::init_software`] before playing anything.
    pub fn new() -> Result<Self, String> {
        // mpv_create refuses to run unless LC_NUMERIC is "C", and a GUI toolkit
        // will have set the process locale from the environment before this.
        unsafe { libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr()) };
        let handle = unsafe { ffi::mpv_create() };
        if handle.is_null() {
            return Err("mpv_create failed".to_string());
        }
        let player = Self {
            handle,
            render: Mutex::new(ptr::null_mut()),
            update_callback: Mutex::new(None),
            initialized: AtomicBool::new(false),
        };
        for (name, value) in COMMON_OPTIONS {
            player.set_option(name, value)?;
        }
        Ok(player)
    }

    /// True once a render backend has been bound. A caller that can be reached
    /// twice, such as a GTK realize handler, should check this first.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// mpv aborts the process on a second `mpv_initialize`, so claim the right
    /// to call it rather than trusting callers not to.
    fn initialize(&self) -> Result<(), String> {
        if self
            .initialized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("player is already initialized".to_string());
        }
        check(
            unsafe { ffi::mpv_initialize(self.handle) },
            "mpv_initialize",
        )
    }

    fn set_option(&self, name: &str, value: &str) -> Result<(), String> {
        let name_c = cstring(name)?;
        let value_c = cstring(value)?;
        check(
            unsafe { ffi::mpv_set_option_string(self.handle, name_c.as_ptr(), value_c.as_ptr()) },
            &format!("set option {name}={value}"),
        )
    }

    fn create_render_context(&self, params: &mut [ffi::mpv_render_param]) -> Result<(), String> {
        let mut render = self.render.lock().unwrap();
        if !render.is_null() {
            return Err("render context already created".to_string());
        }
        let mut created: *mut ffi::mpv_render_context = ptr::null_mut();
        check(
            unsafe {
                ffi::mpv_render_context_create(&mut created, self.handle, params.as_mut_ptr())
            },
            "mpv_render_context_create",
        )?;
        *render = created;
        Ok(())
    }

    /// Bind the player to the OpenGL context that is current on the calling
    /// thread. `get_proc_address` resolves GL entry points for that context.
    ///
    /// Advanced control is on, which is what buys direct rendering: the decoder
    /// writes straight into a texture instead of a frame being copied. It comes
    /// with a hard rule, see [`Self::wants_redraw`].
    pub fn init_opengl(
        &self,
        get_proc_address: ffi::mpv_get_proc_address_fn,
        get_proc_address_ctx: *mut c_void,
        native_display: Option<NativeDisplay>,
    ) -> Result<(), String> {
        for (name, value) in OPENGL_OPTIONS {
            self.set_option(name, value)?;
        }
        self.initialize()?;

        let api_type = cstring("opengl")?;
        let mut init_params = ffi::mpv_opengl_init_params {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx,
        };
        let mut advanced_control: c_int = 1;
        let mut params = vec![
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init_params as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced_control as *mut _ as *mut c_void,
            },
        ];
        if let Some(display) = native_display {
            let (param_type, data) = match display {
                NativeDisplay::Wayland(pointer) => (ffi::MPV_RENDER_PARAM_WL_DISPLAY, pointer),
                NativeDisplay::X11(pointer) => (ffi::MPV_RENDER_PARAM_X11_DISPLAY, pointer),
            };
            params.push(ffi::mpv_render_param { param_type, data });
        }
        params.push(ffi::mpv_render_param {
            param_type: ffi::MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        });
        self.create_render_context(&mut params)
    }

    /// Bind the player to libmpv's software renderer, which draws into a buffer
    /// the caller supplies to [`Self::render_software`]. No display needed.
    pub fn init_software(&self) -> Result<(), String> {
        for (name, value) in SOFTWARE_OPTIONS {
            self.set_option(name, value)?;
        }
        self.initialize()?;

        let api_type = cstring("sw")?;
        let mut params = [
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        self.create_render_context(&mut params)
    }

    /// Run `callback` whenever mpv wants the surface redrawn. It fires on an
    /// mpv-internal thread, so it must not touch UI state directly.
    pub fn set_update_callback<F: Fn() + Send + 'static>(&self, callback: F) {
        let boxed: *mut UpdateCallback = Box::into_raw(Box::new(Box::new(callback)));
        let render = self.render.lock().unwrap();
        if render.is_null() {
            unsafe { drop(Box::from_raw(boxed)) };
            return;
        }
        let mut slot = self.update_callback.lock().unwrap();
        unsafe {
            ffi::mpv_render_context_set_update_callback(
                *render,
                Some(dispatch_update),
                boxed as *mut c_void,
            );
        }
        if let Some(previous) = slot.replace(boxed) {
            unsafe { drop(Box::from_raw(previous)) };
        }
    }

    /// True when mpv has a new frame waiting.
    ///
    /// With advanced control on this must be called once after every update
    /// callback, on the render thread with the GL context current, or the mpv
    /// core blocks waiting for it. Never call it from inside the callback
    /// itself: libmpv forbids re-entering the render API from there.
    pub fn wants_redraw(&self) -> bool {
        let render = self.render.lock().unwrap();
        if render.is_null() {
            return false;
        }
        unsafe { ffi::mpv_render_context_update(*render) & ffi::MPV_RENDER_UPDATE_FRAME != 0 }
    }

    /// Draw the current frame into an OpenGL framebuffer. Must run on the thread
    /// whose current GL context was passed to [`Self::init_opengl`].
    ///
    /// `flip_y` is true for GTK-style top-left origin surfaces.
    pub fn render_opengl(
        &self,
        framebuffer: i32,
        width: i32,
        height: i32,
        flip_y: bool,
    ) -> Result<(), String> {
        let render = self.render.lock().unwrap();
        if render.is_null() {
            return Err("no render context".to_string());
        }
        let mut fbo = ffi::mpv_opengl_fbo {
            fbo: framebuffer,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip: c_int = if flip_y { 1 } else { 0 };
        let mut params = [
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        check(
            unsafe { ffi::mpv_render_context_render(*render, params.as_mut_ptr()) },
            "mpv_render_context_render",
        )
    }

    /// Tell mpv the rendered frame reached the screen, so it can pace playback.
    pub fn report_swap(&self) {
        let render = self.render.lock().unwrap();
        if !render.is_null() {
            unsafe { ffi::mpv_render_context_report_swap(*render) };
        }
    }

    /// Draw the current frame into `target` as `width * height` RGBA pixels.
    pub fn render_software(
        &self,
        width: usize,
        height: usize,
        target: &mut [u8],
    ) -> Result<(), String> {
        let stride = width * BYTES_PER_PIXEL;
        if target.len() < stride * height {
            return Err(format!(
                "target buffer holds {} bytes, needs {}",
                target.len(),
                stride * height
            ));
        }
        let render = self.render.lock().unwrap();
        if render.is_null() {
            return Err("no render context".to_string());
        }

        let mut size: [c_int; 2] = [width as c_int, height as c_int];
        let format = cstring(SOFTWARE_PIXEL_FORMAT)?;
        let mut stride_value: usize = stride;
        let mut params = [
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_SW_FORMAT,
                data: format.as_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut stride_value as *mut _ as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_SW_POINTER,
                data: target.as_mut_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                param_type: ffi::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        check(
            unsafe { ffi::mpv_render_context_render(*render, params.as_mut_ptr()) },
            "mpv_render_context_render",
        )
    }

    // ─── Commands and properties ───────────────────────────────────────────

    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let owned: Vec<CString> = args.iter().map(|a| cstring(a)).collect::<Result<_, _>>()?;
        let mut pointers: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
        pointers.push(ptr::null());
        check(
            unsafe { ffi::mpv_command(self.handle, pointers.as_mut_ptr()) },
            &format!("command {:?}", args),
        )
    }

    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        let name_c = cstring(name)?;
        let value_c = cstring(value)?;
        check(
            unsafe { ffi::mpv_set_property_string(self.handle, name_c.as_ptr(), value_c.as_ptr()) },
            &format!("set property {name}={value}"),
        )
    }

    pub fn get_property_f64(&self, name: &str) -> Result<f64, String> {
        let name_c = cstring(name)?;
        let mut value: f64 = 0.0;
        check(
            unsafe {
                ffi::mpv_get_property(
                    self.handle,
                    name_c.as_ptr(),
                    ffi::MPV_FORMAT_DOUBLE,
                    &mut value as *mut _ as *mut c_void,
                )
            },
            &format!("get property {name}"),
        )?;
        Ok(value)
    }

    pub fn get_property_bool(&self, name: &str) -> Result<bool, String> {
        let name_c = cstring(name)?;
        let mut value: c_int = 0;
        check(
            unsafe {
                ffi::mpv_get_property(
                    self.handle,
                    name_c.as_ptr(),
                    ffi::MPV_FORMAT_FLAG,
                    &mut value as *mut _ as *mut c_void,
                )
            },
            &format!("get property {name}"),
        )?;
        Ok(value != 0)
    }

    pub fn get_property_string(&self, name: &str) -> Result<String, String> {
        let name_c = cstring(name)?;
        let mut value: *mut c_char = ptr::null_mut();
        check(
            unsafe {
                ffi::mpv_get_property(
                    self.handle,
                    name_c.as_ptr(),
                    ffi::MPV_FORMAT_STRING,
                    &mut value as *mut _ as *mut c_void,
                )
            },
            &format!("get property {name}"),
        )?;
        if value.is_null() {
            return Err(format!("property {name} is unset"));
        }
        let owned = unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::mpv_free(value as *mut c_void) };
        Ok(owned)
    }

    // ─── High-level playback ───────────────────────────────────────────────

    pub fn load_file(&self, path: &str) -> Result<(), String> {
        let file = PathBuf::from(path);
        if !file.exists() {
            return Err(format!("File not found: {path}"));
        }
        self.force_media_title("")?;
        self.command(&["loadfile", &file.display().to_string()])
    }

    /// Load a DCP or IMP directory's whole composition: every reel's picture,
    /// in reel order, as one timeline. Falls back to a single picture MXF when
    /// the package names no composition this can resolve.
    pub fn load_package_dir(&self, dir_path: &str) -> Result<(), String> {
        let directory = Path::new(dir_path);
        if !directory.is_dir() {
            return Err(format!("Not a directory: {dir_path}"));
        }
        let (source, title) = match crate::composition_timeline::mpv_source(directory) {
            Some(composition) => (composition.uri, composition.title.unwrap_or_default()),
            None => (
                pick_picture_mxf(directory)?.display().to_string(),
                String::new(),
            ),
        };
        self.force_media_title(&title)?;
        self.command(&["loadfile", &source])
    }

    /// Name what plays in the transport bar. An empty title clears the one an
    /// earlier load forced, which leaves mpv back on the filename.
    fn force_media_title(&self, title: &str) -> Result<(), String> {
        self.set_property("force-media-title", title)
    }

    pub fn play_pause(&self) -> Result<(), String> {
        self.command(&["cycle", "pause"])
    }

    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        self.command(&["seek", &seconds.to_string(), "relative"])
    }

    pub fn seek_absolute(&self, seconds: f64) -> Result<(), String> {
        self.command(&["seek", &seconds.to_string(), "absolute"])
    }

    pub fn stop(&self) -> Result<(), String> {
        self.command(&["stop"])
    }

    pub fn get_position(&self) -> Result<f64, String> {
        self.get_property_f64("time-pos")
    }

    pub fn get_duration(&self) -> Result<f64, String> {
        self.get_property_f64("duration")
    }

    /// Position, duration, pause state and filename as one JSON object, the
    /// shape the GUI transport bar polls.
    pub fn get_metadata(&self) -> Result<String, String> {
        let position = json_number(self.get_position());
        let duration = json_number(self.get_duration());
        let paused = match self.get_property_bool("pause") {
            Ok(value) => value.to_string(),
            Err(_) => "null".to_string(),
        };
        // media-title is the forced composition title when there is one and the
        // filename when there is not
        let filename = match self.get_property_string("media-title") {
            Ok(name) => format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\"")),
            Err(_) => "null".to_string(),
        };
        Ok(format!(
            r#"{{"position": {position}, "duration": {duration}, "paused": {paused}, "filename": {filename}}}"#
        ))
    }
}

impl Drop for MpvRenderPlayer {
    fn drop(&mut self) {
        // The render context has to go before the mpv instance, and the update
        // callback has to be cleared before the box behind it is freed.
        let mut render = self.render.lock().unwrap();
        if !render.is_null() {
            unsafe {
                ffi::mpv_render_context_set_update_callback(*render, None, ptr::null_mut());
                ffi::mpv_render_context_free(*render);
            }
            *render = ptr::null_mut();
        }
        if let Some(callback) = self.update_callback.lock().unwrap().take() {
            unsafe { drop(Box::from_raw(callback)) };
        }
        unsafe { ffi::mpv_terminate_destroy(self.handle) };
    }
}

unsafe extern "C" fn dispatch_update(callback_ctx: *mut c_void) {
    if callback_ctx.is_null() {
        return;
    }
    let callback = unsafe { &*(callback_ctx as *const UpdateCallback) };
    callback();
}

fn cstring(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("value contains a NUL byte: {value}"))
}

fn check(status: c_int, what: &str) -> Result<(), String> {
    if status >= 0 {
        return Ok(());
    }
    let message = unsafe { CStr::from_ptr(ffi::mpv_error_string(status)) }.to_string_lossy();
    Err(format!("{what}: {message}"))
}

fn json_number(value: Result<f64, String>) -> String {
    match value {
        Ok(number) if number.is_finite() => number.to_string(),
        _ => "null".to_string(),
    }
}
