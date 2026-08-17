//! Preview of a multi-reel package plays the whole composition, not one reel.
//!
//! The package is written by postkit's own ASSETMAP and CPL writers around
//! three short clips, then loaded through the libmpv render player: the same
//! `load_package_dir` the wizards call. Needs ffmpeg for the clips, and skips
//! when it is absent.

#![cfg(all(target_os = "linux", feature = "libmpv"))]

use postkit::mpv_render::MpvRenderPlayer;
use postkit::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};
use std::path::Path;
use std::time::{Duration, Instant};

/// Reel name, picture asset uuid, and clip length in seconds. Awkward
/// characters are in the filename on purpose: `,` and `;` separate EDL fields.
const REELS: [(&str, &str, u32); 3] = [
    ("head,ident.mxf", "11111111-1111-1111-1111-111111111111", 2),
    (
        "feature reel.mxf",
        "22222222-2222-2222-2222-222222222222",
        3,
    ),
    ("tail;rating.mxf", "33333333-3333-3333-3333-333333333333", 5),
];
const FRAMES_PER_SECOND: u32 = 24;
const DURATION_TOLERANCE_SECONDS: f64 = 0.2;
const DURATION_TIMEOUT: Duration = Duration::from_secs(20);

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write_clip(path: &Path, seconds: u32) {
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=320x180:rate={FRAMES_PER_SECOND}:duration={seconds}"),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A three-reel DCP: real ASSETMAP and CPL, clips standing in for the picture
/// track files.
fn write_package(dir: &Path) {
    let mut assets = vec![AssetMapAsset {
        id: "cc10cc10-0000-0000-0000-000000000000".into(),
        path: "CPL_test.xml".into(),
        ..Default::default()
    }];
    let mut reels = Vec::new();
    for (index, (name, picture_id, seconds)) in REELS.iter().enumerate() {
        write_clip(&dir.join(name), *seconds);
        assets.push(AssetMapAsset {
            id: (*picture_id).into(),
            path: (*name).into(),
            ..Default::default()
        });
        reels.push(DcpCplReel {
            reel_id: format!("aaaaaaaa-0000-0000-0000-00000000000{index}"),
            picture_id: (*picture_id).into(),
            picture_edit_rate_num: FRAMES_PER_SECOND,
            picture_edit_rate_den: 1,
            picture_duration: u64::from(seconds * FRAMES_PER_SECOND),
            picture_width: 1998,
            picture_height: 1080,
            ..Default::default()
        });
    }

    let assetmap = AssetMap {
        uuid: "bbbbbbbb-0000-0000-0000-000000000000".into(),
        namespace: ns::AM_SMPTE.into(),
        assets,
        ..Default::default()
    };
    std::fs::write(dir.join("ASSETMAP.xml"), assetmap.to_xml()).unwrap();

    let cpl = DcpCpl {
        uuid: "cc10cc10-0000-0000-0000-000000000000".into(),
        namespace: ns::CPL_SMPTE.into(),
        title: "Three Reel Test".into(),
        reels,
        ..Default::default()
    };
    std::fs::write(dir.join("CPL_test.xml"), cpl.to_xml()).unwrap();
}

/// mpv reports the duration only once the first segment is demuxed.
fn duration_when_ready(player: &MpvRenderPlayer) -> f64 {
    let deadline = Instant::now() + DURATION_TIMEOUT;
    loop {
        if let Ok(duration) = player.get_duration()
            && duration > 0.0
        {
            return duration;
        }
        assert!(Instant::now() < deadline, "mpv reported no duration");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_three_reel_package_plays_as_one_timeline() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path());

    let player = MpvRenderPlayer::new().unwrap();
    player.init_software().unwrap();
    player
        .load_package_dir(&dir.path().to_string_lossy())
        .unwrap();

    let total: u32 = REELS.iter().map(|(_, _, seconds)| seconds).sum();
    let duration = duration_when_ready(&player);
    assert!(
        (duration - f64::from(total)).abs() < DURATION_TOLERANCE_SECONDS,
        "played {duration}s of a {total}s composition"
    );

    // seeking has to cross a reel boundary, not stop at the first reel's end
    player.seek_absolute(7.0).unwrap();
    let deadline = Instant::now() + DURATION_TIMEOUT;
    loop {
        if let Ok(position) = player.get_position()
            && position > 6.0
        {
            break;
        }
        assert!(Instant::now() < deadline, "seek past the first reel failed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
