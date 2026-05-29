//! mpv IPC player for preview functionality.
//!
//! Provides a cross-platform `MpvPlayer` that communicates with mpv via
//! Unix domain sockets (macOS/Linux) or named pipes (Windows).
//!
//! # Embedding
//!
//! Call `set_parent_wid()` with a native window handle before starting mpv
//! to embed the video inside that window:
//! - Windows: pass an HWND
//! - macOS: pass the raw pointer to an NSView
//! - Linux X11: pass an XID
//!
//! Without a parent wid, mpv runs as a floating always-on-top window.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;

/// Cross-platform mpv player with IPC control.
pub struct MpvPlayer {
    process: Mutex<Option<Child>>,
    #[cfg(unix)]
    ipc_path: String,
    #[cfg(windows)]
    ipc_path: String,
    parent_wid: Mutex<Option<u64>>,
    title: String,
}

impl MpvPlayer {
    /// Create a new MpvPlayer. `app_name` is used for the IPC socket/pipe name
    /// and the window title (e.g. "DCPWizard", "IMFWizard").
    pub fn new(app_name: &str) -> Self {
        let ipc_path = Self::make_ipc_path(app_name);
        let title = format!("{} Preview", app_name);
        Self {
            process: Mutex::new(None),
            ipc_path,
            parent_wid: Mutex::new(None),
            title,
        }
    }

    #[cfg(unix)]
    fn make_ipc_path(app_name: &str) -> String {
        format!(
            "/tmp/{}-mpv-{}.sock",
            app_name.to_lowercase(),
            std::process::id()
        )
    }

    #[cfg(windows)]
    fn make_ipc_path(app_name: &str) -> String {
        format!(
            r"\\.\pipe\{}-mpv-{}",
            app_name.to_lowercase(),
            std::process::id()
        )
    }

    /// Set a parent window ID for embedding. mpv will render inside this window.
    pub fn set_parent_wid(&self, wid: u64) {
        *self.parent_wid.lock().unwrap() = Some(wid);
    }

    /// Check if the mpv process is alive and responsive.
    pub fn is_alive(&self) -> bool {
        let mut proc = self.process.lock().unwrap();
        let process_ok = proc
            .as_mut()
            .is_some_and(|p| p.try_wait().ok().flatten().is_none());
        if !process_ok {
            return false;
        }
        self.can_connect()
    }

    #[cfg(unix)]
    fn can_connect(&self) -> bool {
        std::os::unix::net::UnixStream::connect(&self.ipc_path).is_ok()
    }

    #[cfg(windows)]
    fn can_connect(&self) -> bool {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(PIPE_ACCESS_DUPLEX)
            .open(&self.ipc_path)
            .is_ok()
    }

    /// Start the mpv process. Kills any existing instance first.
    pub fn start_mpv(&self) -> Result<(), String> {
        let mut proc = self.process.lock().unwrap();
        if let Some(mut old) = proc.take() {
            let _ = old.kill();
            let _ = old.wait();
        }

        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc_path);

        let mut args = vec![
            "--idle=yes".to_string(),
            "--no-terminal".to_string(),
            "--keep-open=yes".to_string(),
            "--osc=yes".to_string(),
            format!("--input-ipc-server={}", self.ipc_path),
            format!("--title={}", self.title),
        ];

        if let Some(wid) = *self.parent_wid.lock().unwrap() {
            args.push(format!("--wid={}", wid));
        } else {
            args.push("--force-window=yes".to_string());
            args.push("--ontop=yes".to_string());
            args.push("--geometry=640x360+0+0".to_string());
        }

        let mut cmd = Command::new("mpv");
        cmd.args(&args);

        // On Linux Wayland, clear WAYLAND_DISPLAY to force XWayland where --ontop works
        #[cfg(target_os = "linux")]
        {
            cmd.env_remove("WAYLAND_DISPLAY");
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to start mpv: {e}"))?;
        *proc = Some(child);

        // Wait for IPC to become connectable
        for _ in 0..50 {
            if self.ipc_ready() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("mpv IPC did not become available".to_string())
    }

    #[cfg(unix)]
    fn ipc_ready(&self) -> bool {
        std::path::Path::new(&self.ipc_path).exists()
    }

    #[cfg(windows)]
    fn ipc_ready(&self) -> bool {
        self.can_connect()
    }

    /// Ensure mpv is running, starting it if necessary.
    pub fn ensure_running(&self) -> Result<(), String> {
        if !self.is_alive() {
            self.start_mpv()?;
        }
        Ok(())
    }

    /// Send a JSON IPC command to mpv. Returns an error if mpv isn't running.
    pub fn send_command(&self, cmd: &str) -> Result<String, String> {
        if !self.is_alive() {
            return Err("mpv not running".to_string());
        }
        self.try_send(cmd)
    }

    #[cfg(unix)]
    fn try_send(&self, cmd: &str) -> Result<String, String> {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&self.ipc_path)
            .map_err(|e| format!("Failed to connect to mpv: {e}"))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        stream
            .write_all(cmd.as_bytes())
            .map_err(|e| format!("Failed to send: {e}"))?;
        stream
            .write_all(b"\n")
            .map_err(|e| format!("Failed to send newline: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).ok();
        Ok(response)
    }

    #[cfg(windows)]
    fn try_send(&self, cmd: &str) -> Result<String, String> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;

        let mut pipe = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(PIPE_ACCESS_DUPLEX)
            .open(&self.ipc_path)
            .map_err(|e| format!("Failed to connect to mpv pipe: {e}"))?;

        pipe.write_all(cmd.as_bytes())
            .map_err(|e| format!("Failed to send: {e}"))?;
        pipe.write_all(b"\n")
            .map_err(|e| format!("Failed to send newline: {e}"))?;

        let mut reader = BufReader::new(pipe);
        let mut response = String::new();
        reader.read_line(&mut response).ok();
        Ok(response)
    }

    /// Kill the mpv process and clean up.
    pub fn kill(&self) {
        if let Some(mut child) = self.process.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc_path);
    }

    // ─── High-level commands ───────────────────────────────────────────────

    /// Load a file into mpv. Starts mpv if not running.
    pub fn load_file(&self, path: &str) -> Result<(), String> {
        let p = PathBuf::from(path);
        if !p.exists() {
            return Err(format!("File not found: {path}"));
        }
        self.ensure_running()?;
        let cmd = format!(
            r#"{{"command": ["loadfile", "{}"]}}"#,
            p.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        );
        self.send_command(&cmd)?;
        Ok(())
    }

    /// Load the first video MXF from a DCP/IMP directory.
    pub fn load_package_dir(&self, dir_path: &str) -> Result<(), String> {
        let dir = PathBuf::from(dir_path);
        if !dir.is_dir() {
            return Err(format!("Not a directory: {dir_path}"));
        }

        let mut mxf_files = find_mxf_files(&dir);
        if mxf_files.is_empty() {
            return Err("No MXF files found in directory".to_string());
        }

        let video_mxf = mxf_files
            .iter()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("pic"))
            })
            .cloned()
            .unwrap_or_else(|| {
                mxf_files.sort_by(|a, b| {
                    let size_a = a.metadata().map(|m| m.len()).unwrap_or(0);
                    let size_b = b.metadata().map(|m| m.len()).unwrap_or(0);
                    size_b.cmp(&size_a)
                });
                mxf_files[0].clone()
            });

        self.ensure_running()?;
        let cmd = format!(
            r#"{{"command": ["loadfile", "{}"]}}"#,
            video_mxf
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        );
        self.send_command(&cmd)?;
        Ok(())
    }

    /// Toggle play/pause.
    pub fn play_pause(&self) -> Result<(), String> {
        self.send_command(r#"{"command": ["cycle", "pause"]}"#)?;
        Ok(())
    }

    /// Seek relative by seconds.
    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        self.send_command(&format!(
            r#"{{"command": ["seek", "{seconds}", "relative"]}}"#
        ))?;
        Ok(())
    }

    /// Seek to absolute position in seconds.
    pub fn seek_absolute(&self, seconds: f64) -> Result<(), String> {
        self.send_command(&format!(
            r#"{{"command": ["seek", "{seconds}", "absolute"]}}"#
        ))?;
        Ok(())
    }

    /// Stop playback.
    pub fn stop(&self) -> Result<(), String> {
        self.send_command(r#"{"command": ["stop"]}"#)?;
        Ok(())
    }

    /// Get current playback position in seconds.
    pub fn get_position(&self) -> Result<f64, String> {
        let resp = self.send_command(r#"{"command": ["get_property", "time-pos"]}"#)?;
        parse_property_f64(&resp)
    }

    /// Get total duration in seconds.
    pub fn get_duration(&self) -> Result<f64, String> {
        let resp = self.send_command(r#"{"command": ["get_property", "duration"]}"#)?;
        parse_property_f64(&resp)
    }

    /// Get combined metadata (position, duration, pause state, filename) as JSON.
    pub fn get_metadata(&self) -> Result<String, String> {
        let pos = self
            .send_command(r#"{"command": ["get_property", "time-pos"]}"#)
            .unwrap_or_default();
        let dur = self
            .send_command(r#"{"command": ["get_property", "duration"]}"#)
            .unwrap_or_default();
        let paused = self
            .send_command(r#"{"command": ["get_property", "pause"]}"#)
            .unwrap_or_default();
        let fname = self
            .send_command(r#"{"command": ["get_property", "filename"]}"#)
            .unwrap_or_default();

        Ok(format!(
            r#"{{"position": {}, "duration": {}, "paused": {}, "filename": {}}}"#,
            extract_data_field(&pos),
            extract_data_field(&dur),
            extract_data_field(&paused),
            extract_data_field_str(&fname),
        ))
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.kill();
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn parse_property_f64(resp: &str) -> Result<f64, String> {
    if let Some(start) = resp.find("\"data\":") {
        let after = &resp[start + 7..];
        let end = after.find([',', '}']).unwrap_or(after.len());
        let val_str = after[..end].trim();
        val_str
            .parse::<f64>()
            .map_err(|e| format!("Parse error: {e} from '{val_str}'"))
    } else {
        Err(format!("No data in response: {resp}"))
    }
}

fn extract_data_field(resp: &str) -> String {
    if let Some(start) = resp.find("\"data\":") {
        let after = &resp[start + 7..];
        let end = after.find([',', '}']).unwrap_or(after.len());
        after[..end].trim().to_string()
    } else {
        "null".to_string()
    }
}

fn extract_data_field_str(resp: &str) -> String {
    if let Some(start) = resp.find("\"data\":") {
        let after = &resp[start + 7..];
        let end = after.find([',', '}']).unwrap_or(after.len());
        let val = after[..end].trim();
        if val.starts_with('"') {
            val.to_string()
        } else {
            format!("\"{}\"", val)
        }
    } else {
        "null".to_string()
    }
}

/// Recursively find MXF files in a directory.
pub fn find_mxf_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "__MACOSX" || n.to_string_lossy().starts_with('.'))
                {
                    continue;
                }
                results.extend(find_mxf_files(&path));
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mxf"))
            {
                results.push(path);
            }
        }
    }
    results
}
