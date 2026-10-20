#![cfg(feature = "grok-gpu")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use postkit::composited_preview::render_grok_preview_stretches;
use postkit::device_lease::DEVICE_LEASE;
use postkit::encode::{FrameRate, SourceColour};
use postkit::grok_encoder::{accelerated_frames, gpu_active, initialize, use_gpu_from_environment};
use postkit::picture_processing::{Fit, PictureProcessing, Placement};
use postkit::pipeline::EncodeRunOptions;
use postkit::timeline::{Clip, ClipSound, SoundTrack, TIMELINE_FORMAT_VERSION, Timeline};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const RATE: u32 = 24;
const CARD_FRAMES: u64 = 4;
const RENDER_LIMIT: Duration = Duration::from_secs(120);

// noise keeps each codestream over the cinema minimum, padding up to it decodes on the device
fn noise_card(directory: &Path) -> PathBuf {
    let card = directory.join("noise_card.png");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=gray:s={WIDTH}x{HEIGHT}"))
        .args(["-vf", "noise=alls=100"])
        .args(["-frames:v", "1", "-pix_fmt", "rgb24"])
        .arg(&card)
        .output()
        .expect("ffmpeg");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    card
}

fn card_over_a_gap(card: &Path) -> Timeline {
    Timeline {
        version: TIMELINE_FORMAT_VERSION,
        fps: FrameRate::whole(RATE),
        clips: vec![Clip {
            picture: card.to_path_buf(),
            start: 0,
            layer: 1,
            opacity: 1.0,
            in_frame: 0,
            out_frame: CARD_FRAMES,
            sound: ClipSound::None,
            subtitle: None,
            source_colour: SourceColour::KeepRgb,
            picture_processing: PictureProcessing::default(),
        }],
        sound: SoundTrack::PerClip,
        subtitle: None,
        reel_starts: Vec::new(),
    }
}

fn fitted_to_the_card() -> EncodeRunOptions {
    EncodeRunOptions {
        fps: FrameRate::whole(RATE),
        source_colour: SourceColour::KeepRgb,
        picture: PictureProcessing {
            fit: Some(Fit {
                box_width: WIDTH,
                box_height: HEIGHT,
                raster_width: WIDTH,
                raster_height: HEIGHT,
                placement: Placement::default(),
            }),
            ..PictureProcessing::default()
        },
        ..Default::default()
    }
}

#[test]
fn a_grok_preview_renders_on_the_cpu_while_an_encode_holds_the_device() {
    initialize(0);
    if let Err(reason) = use_gpu_from_environment() {
        panic!("{reason}");
    }
    assert!(gpu_active());

    let directory = tempfile::tempdir().unwrap();
    let timeline = card_over_a_gap(&noise_card(directory.path()));
    let cache = directory.path().join("cache");
    let device_frames_before = accelerated_frames();

    let encode_lease = DEVICE_LEASE.acquire_for_encode();
    let (rendered_sender, rendered_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rendered = render_grok_preview_stretches(
            &timeline,
            &fitted_to_the_card(),
            &cache,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        rendered_sender.send(rendered).unwrap();
    });
    let rendered = rendered_receiver
        .recv_timeout(RENDER_LIMIT)
        .expect("the render waits for the device the encode holds")
        .expect("the grok preview renders");
    drop(encode_lease);

    assert_eq!(rendered.len(), 1);
    assert_eq!(rendered[0].frames, 0..CARD_FRAMES);
    let codestreams = std::fs::read_dir(&rendered[0].path).unwrap().count();
    assert_eq!(codestreams as u64, CARD_FRAMES);
    assert_eq!(
        accelerated_frames(),
        device_frames_before,
        "the render put frames on the device"
    );
}
