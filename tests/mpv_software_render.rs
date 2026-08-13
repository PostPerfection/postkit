#![cfg(all(target_os = "linux", feature = "libmpv"))]

use postkit::mpv_render::MpvRenderPlayer;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WIDTH: usize = 320;
const HEIGHT: usize = 180;
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A real clip if the caller points at one, otherwise a synthetic colour clip so
/// the test needs nothing but ffmpeg.
fn test_clip(directory: &std::path::Path) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("POSTKIT_MPV_TEST_MEDIA") {
        let path = PathBuf::from(path);
        assert!(
            path.exists(),
            "POSTKIT_MPV_TEST_MEDIA does not exist: {path:?}"
        );
        return Some(path);
    }
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available and POSTKIT_MPV_TEST_MEDIA unset");
        return None;
    }
    let clip = directory.join("clip.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=24:duration=2",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&clip)
        .output()
        .expect("ffmpeg");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    Some(clip)
}

#[test]
fn software_render_produces_a_non_black_frame() {
    let directory = tempfile::tempdir().unwrap();
    let Some(clip) = test_clip(directory.path()) else {
        return;
    };

    let player = MpvRenderPlayer::new().expect("create mpv");
    player.init_software().expect("software render context");
    player.load_file(clip.to_str().unwrap()).expect("load clip");

    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut rendered_something = false;
    while Instant::now() < deadline {
        if !player.wants_redraw() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        player
            .render_software(WIDTH, HEIGHT, &mut pixels)
            .expect("render frame");
        if pixels.chunks_exact(4).any(|pixel| pixel[..3] != [0, 0, 0]) {
            rendered_something = true;
            break;
        }
    }

    assert!(
        rendered_something,
        "libmpv rendered no non-black pixel from {} within {:?}",
        clip.display(),
        FRAME_TIMEOUT
    );
}
