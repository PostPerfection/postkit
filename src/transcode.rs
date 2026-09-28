use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CANCELLED_ERROR: &str = "cancelled before the transcode finished";

/// Transcode options (ffmpeg-based).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscodeOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub codec: String,
    pub resolution: Option<(u32, u32)>,
    pub fps: Option<f64>,
    pub bitrate: Option<String>,
    pub audio_codec: Option<String>,
    pub extra_args: Vec<String>,
    #[serde(skip)]
    pub cancel: Arc<AtomicBool>,
}

/// Transcode result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscodeResult {
    pub success: bool,
    pub error: String,
    pub output: PathBuf,
}

/// Transcode media via ffmpeg.
pub fn transcode(opts: &TranscodeOptions) -> TranscodeResult {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").arg("-i").arg(&opts.input);

    if !opts.codec.is_empty() {
        cmd.arg("-c:v").arg(&opts.codec);
    }
    if let Some((w, h)) = opts.resolution {
        cmd.arg("-s").arg(format!("{w}x{h}"));
    }
    if let Some(fps) = opts.fps {
        cmd.arg("-r").arg(fps.to_string());
    }
    if let Some(ref br) = opts.bitrate {
        cmd.arg("-b:v").arg(br);
    }
    if let Some(ref ac) = opts.audio_codec {
        cmd.arg("-c:a").arg(ac);
    }
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&opts.output);

    let (success, error) = match run_ffmpeg_until_cancelled(cmd, &opts.output, &opts.cancel) {
        Ok(()) => (true, String::new()),
        Err(error) => (false, error),
    };
    TranscodeResult {
        success,
        error,
        output: opts.output.clone(),
    }
}

enum Ending {
    Exited(ExitStatus),
    Cancelled,
}

pub fn run_ffmpeg_until_cancelled(
    mut cmd: Command,
    output: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;
    // a full stderr pipe would stall ffmpeg before it exits
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let ending = wait_or_kill(&mut child, cancel);
    let stderr = stderr_reader
        .join()
        .expect("the stderr reader does not panic");
    match ending? {
        Ending::Exited(status) if status.success() => Ok(()),
        Ending::Exited(_) => Err(stderr),
        Ending::Cancelled => match std::fs::remove_file(output) {
            Ok(()) => Err(CANCELLED_ERROR.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CANCELLED_ERROR.to_string()),
            Err(e) => Err(format!(
                "{CANCELLED_ERROR}, and the partial {} could not be removed: {e}",
                output.display()
            )),
        },
    }
}

fn wait_or_kill(child: &mut Child, cancel: &AtomicBool) -> Result<Ending, String> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("Failed to wait for ffmpeg: {e}"))?
        {
            return Ok(Ending::Exited(status));
        }
        if cancel.load(Ordering::Relaxed) {
            child
                .kill()
                .map_err(|e| format!("Failed to stop ffmpeg: {e}"))?;
            child
                .wait()
                .map_err(|e| format!("Failed to wait for ffmpeg: {e}"))?;
            return Ok(Ending::Cancelled);
        }
        std::thread::sleep(CANCEL_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_DURATION_SECONDS: &str = "600";

    #[test]
    fn a_cancelled_transcode_removes_its_partial_output() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("partial.mkv");
        std::fs::write(&output, b"partial").unwrap();
        let mut command = Command::new("ffmpeg");
        command
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=24"])
            .args(["-t", SOURCE_DURATION_SECONDS])
            .arg(&output);
        let cancel = AtomicBool::new(true);

        let result = run_ffmpeg_until_cancelled(command, &output, &cancel);

        assert_eq!(result, Err(CANCELLED_ERROR.to_string()));
        assert!(!output.exists(), "the cancelled transcode left its output");
    }
}
