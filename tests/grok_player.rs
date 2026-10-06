#![cfg(feature = "grok-ffi")]

use asdcplib::crypto::{AesEncContext, HmacContext};
use asdcplib::jp2k::{CodestreamHeader, MxfWriter, PictureDescriptor};
use asdcplib::{LabelSet, Rational, WriterInfo};
use postkit::colour::XyzToSrgb;
use postkit::composition_timeline;
use postkit::grok_player::{
    DecodeScale, FrameRange, GrokPlayer, OverlayRectangle, PictureMasks, PictureScaling,
    PresentationSettings, SourceOptions, SubtitlePresentation, SubtitleSlot,
};
use postkit::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FRAMES_PER_SECOND: u32 = 24;
const CINEMA_2K_PROFILE: u16 = 0x0003;
const SOFTWARE_BYTES_PER_PIXEL: usize = 4;
const PATIENCE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(2);
const DISTINCT_FRAME_COLOURS: usize = 10;
// polls that must find the position unmoved before playback counts as stopped
const STILLNESS_POLLS: usize = 50;

// ─── fixtures ──────────────────────────────────────────────────────────────

// only the first DISTINCT_FRAME_COLOURS frames differ, 12 bits has no room for more
fn frame_code(index: usize) -> i32 {
    const FIRST_CODE: i32 = 300;
    const CODE_STEP: i32 = 350;
    FIRST_CODE + CODE_STEP * (index % DISTINCT_FRAME_COLOURS) as i32
}

fn frame_colour(index: usize) -> [u8; 3] {
    let code = frame_code(index) as u16;
    XyzToSrgb::new().pixel(code, code, code)
}

fn flat_codestreams(width: u32, height: u32, count: usize, profile: u16) -> Vec<Vec<u8>> {
    let codes: Vec<[i32; 3]> = (0..count).map(|index| [frame_code(index); 3]).collect();
    xyz_codestreams(width, height, &codes, profile)
}

// one flat frame per X'Y'Z' code triple
fn xyz_codestreams(
    width: u32,
    height: u32,
    frame_codes: &[[i32; 3]],
    profile: u16,
) -> Vec<Vec<u8>> {
    let count = frame_codes.len();
    let params = postkit::grok_encoder::CompressParams {
        irreversible: false,
        compression_ratio: 1.0,
        mct: false,
        apply_xyz_transform: false,
        profile,
        num_resolutions: 3,
        ..postkit::grok_encoder::CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let directory = tempfile::tempdir().unwrap();
    let samples = (width * height) as usize;
    let mut next = 0usize;
    let result = postkit::grok_encoder::encode_pipeline(
        directory.path(),
        &params,
        count as u64,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &std::sync::Arc::new(postkit::grok_encoder::PhaseClocks::default()),
        |_| {
            if next >= count {
                return None;
            }
            let [x, y, z] = frame_codes[next];
            let frame = postkit::grok_encoder::RawFrame::Planar {
                components: [vec![x; samples], vec![y; samples], vec![z; samples]],
                width,
                height,
                precision: 12,
                index: next as u64,
            };
            next += 1;
            Some(frame)
        },
        |_| {},
    );
    assert!(result.success, "fixture encode failed: {}", result.error);
    (0..count)
        .map(|index| {
            let path = directory.path().join(format!("frame_{:08}.j2c", index));
            std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

fn descriptor(first_frame: &[u8], frames: u32, width: u32, height: u32) -> PictureDescriptor {
    PictureDescriptor {
        edit_rate: Rational::new(FRAMES_PER_SECOND as i32, 1),
        sample_rate: Rational::new(FRAMES_PER_SECOND as i32, 1),
        stored_width: width,
        stored_height: height,
        aspect_ratio: Rational::new(width as i32, height as i32),
        container_duration: frames,
        codestream: CodestreamHeader::parse(first_frame).expect("fixture is a codestream"),
    }
}

fn write_mxf(path: &Path, frames: &[Vec<u8>], key: Option<[u8; 16]>, width: u32, height: u32) {
    let info = WriterInfo {
        asset_uuid: [8; 16],
        context_id: [0xc7; 16],
        cryptographic_key_id: [0xd4; 16],
        encrypted_essence: key.is_some(),
        uses_hmac: key.is_some(),
        label_set: LabelSet::Smpte,
        ..Default::default()
    };
    let mut writer = MxfWriter::new();
    writer
        .open_write(
            &path.to_string_lossy(),
            &info,
            &descriptor(&frames[0], frames.len() as u32, width, height),
            16_384,
        )
        .unwrap();
    let mut crypto = key.map(|key| {
        let mut encryptor = AesEncContext::new();
        encryptor.init_key(&key).unwrap();
        encryptor.set_ivec(&[0x9c; 16]).unwrap();
        let mut hmac = HmacContext::new();
        hmac.init_key(&key, LabelSet::Smpte).unwrap();
        (encryptor, hmac)
    });
    for frame in frames {
        match crypto.as_mut() {
            Some((encryptor, hmac)) => writer
                .write_frame(frame, Some(encryptor), Some(hmac))
                .unwrap(),
            None => writer.write_frame(frame, None, None).unwrap(),
        }
    }
    writer.finalize().unwrap();
}

fn flat_mxf(directory: &Path, name: &str, width: u32, height: u32, count: usize) -> PathBuf {
    let frames = flat_codestreams(width, height, count, CINEMA_2K_PROFILE);
    let path = directory.join(name);
    write_mxf(&path, &frames, None, width, height);
    path
}

// ─── polling helpers ───────────────────────────────────────────────────────

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(POLL);
    }
    panic!("{what} did not happen within {PATIENCE:?}");
}

// so the next wait_for_frame answers for the command that follows
fn forget_frames(player: &GrokPlayer) {
    while player.wants_redraw() {}
}

fn wait_for_frame(player: &GrokPlayer) {
    wait_until("a frame was composed", || player.wants_redraw());
}

fn software_frame(player: &GrokPlayer, width: usize, height: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; width * height * SOFTWARE_BYTES_PER_PIXEL];
    player
        .render_software(width, height, &mut buffer)
        .expect("software render");
    buffer
}

fn pixel(buffer: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let at = (y * width + x) * SOFTWARE_BYTES_PER_PIXEL;
    [buffer[at], buffer[at + 1], buffer[at + 2]]
}

fn shown_colour(player: &GrokPlayer, width: usize, height: usize) -> [u8; 3] {
    pixel(&software_frame(player, width, height), width, 0, 0)
}

// a frame skipped only because the scheduler thread woke late is not counted
fn dropped_frames_not_decoded(player: &GrokPlayer) -> u64 {
    const KEY: &str = r#""dropped_frames_not_decoded": "#;
    let metadata = player.metadata_json();
    let at = metadata
        .find(KEY)
        .expect("dropped_frames_not_decoded in the metadata")
        + KEY.len();
    let rest = &metadata[at..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("a dropped frame count")
}

// the smallest the cache gets while it waits, sampled every poll
fn lowest_cached_until(player: &GrokPlayer, what: &str, mut ready: impl FnMut() -> bool) -> usize {
    let deadline = Instant::now() + PATIENCE;
    let mut lowest = player.cached_frame_count();
    while !ready() {
        lowest = lowest.min(player.cached_frame_count());
        assert!(Instant::now() < deadline, "{what} did not happen");
        std::thread::sleep(POLL);
    }
    lowest
}

fn loaded_player(source: &Path) -> GrokPlayer {
    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player.load(source, None).expect("load");
    // load clears whatever was on screen before it, so wait for a real frame
    wait_until("the first frame was composed", || {
        player.frame_size().is_some()
    });
    forget_frames(&player);
    player
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[test]
fn a_loaded_source_shows_frame_zero_exactly_as_the_still_path_renders_it() {
    const SIZE: u32 = 128;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 3);

    let player = loaded_player(&mxf);
    assert_eq!(player.position(), Some(0.0));
    assert_eq!(
        player.duration(),
        Some(3.0 / f64::from(FRAMES_PER_SECOND)),
        "three frames at {FRAMES_PER_SECOND} fps"
    );
    assert_eq!(player.source_size(), Some((SIZE, SIZE)));
    assert!(player.paused(), "a freshly loaded source waits paused");

    // the same frame through the still path, which is what the step preview draws
    let still = directory.path().join("frame0.ppm");
    postkit::preview::render_dcp_frame(
        &postkit::preview::DcpPreviewOptions {
            source: mxf.clone(),
            ..Default::default()
        },
        0,
        &still,
    )
    .expect("still render");
    let expected = read_ppm(&still, SIZE, SIZE);

    let played = software_frame(&player, SIZE as usize, SIZE as usize);
    let played_rgb: Vec<u8> = played
        .as_chunks::<SOFTWARE_BYTES_PER_PIXEL>()
        .0
        .iter()
        .flat_map(|pixel| pixel[..3].to_vec())
        .collect();
    assert_eq!(
        played_rgb, expected,
        "the played frame is not the pixels the still path writes"
    );
}

#[test]
fn playing_to_the_end_stops_on_the_last_frame_at_eof() {
    const SIZE: u32 = 64;
    const FRAMES: usize = 6;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, FRAMES);

    let player = loaded_player(&mxf);
    player.set_paused(false);
    wait_until("playback reached the end", || player.eof_reached());

    assert!(player.paused(), "the end of a source pauses it");
    let last = (FRAMES - 1) as f64 / f64::from(FRAMES_PER_SECOND);
    assert_eq!(player.position(), Some(last));
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        frame_colour(FRAMES - 1),
        "the last frame stays on screen"
    );
    assert!(
        player.metadata_json().contains(r#""eof": true"#),
        "{}",
        player.metadata_json()
    );

    player.set_paused(false);
    wait_until("play at the end started over", || {
        !player.eof_reached() && player.position().is_some_and(|seconds| seconds < last)
    });
    assert!(!player.paused(), "play at the end of a source restarts it");
}

#[test]
fn stepping_and_seeking_land_on_the_exact_frame() {
    const SIZE: u32 = 64;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 12);
    let (width, height) = (SIZE as usize, SIZE as usize);

    let player = loaded_player(&mxf);

    player.frame_step();
    wait_for_frame(&player);
    assert_eq!(shown_colour(&player, width, height), frame_colour(1));
    assert_eq!(player.position(), Some(1.0 / f64::from(FRAMES_PER_SECOND)));

    forget_frames(&player);
    player.frame_back_step();
    wait_for_frame(&player);
    assert_eq!(shown_colour(&player, width, height), frame_colour(0));

    forget_frames(&player);
    player.seek_absolute(3.0 / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert_eq!(shown_colour(&player, width, height), frame_colour(3));
    assert_eq!(player.position(), Some(3.0 / f64::from(FRAMES_PER_SECOND)));

    forget_frames(&player);
    player.seek(-2.0 / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert_eq!(shown_colour(&player, width, height), frame_colour(1));

    // a step during playback stops the clock where it lands
    player.seek_absolute(0.0);
    player.set_paused(false);
    wait_until("playback started", || !player.paused());
    player.frame_step();
    wait_until("the step paused playback", || player.paused());
    let landed = player.position();
    for _ in 0..STILLNESS_POLLS {
        assert_eq!(player.position(), landed, "playback ran on past the step");
        std::thread::sleep(POLL);
    }
}

#[test]
fn the_decode_scale_sets_the_size_of_the_frame_on_screen() {
    const SIZE: u32 = 64;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 12);

    let player = loaded_player(&mxf);
    assert_eq!(player.frame_size(), Some((SIZE, SIZE)));

    player.set_decode_scale(DecodeScale::Half);
    wait_until("a half-scale frame arrived while paused", || {
        player.frame_size() == Some((SIZE / 2, SIZE / 2))
    });
    assert_eq!(
        player.source_size(),
        Some((SIZE, SIZE)),
        "the source size does not follow the decode scale"
    );

    player.set_paused(false);
    wait_until("playback moved past the first frame", || {
        player.position().is_some_and(|position| position > 0.0)
    });
    assert_eq!(
        player.frame_size(),
        Some((SIZE / 2, SIZE / 2)),
        "playing frames stay at the decode scale"
    );

    player.set_paused(true);
    player.set_decode_scale(DecodeScale::Full);
    wait_until("the full-scale frame came back", || {
        player.frame_size() == Some((SIZE, SIZE))
    });
}

#[test]
fn a_decode_scale_change_during_playback_keeps_the_frames_already_decoded() {
    const SIZE: u32 = 320;
    const FRAMES: usize = 72;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, FRAMES);

    let player = loaded_player(&mxf);
    let window = (player.lookahead_frames() + 1).min(FRAMES);
    wait_until("the decode window filled", || {
        player.cached_frame_count() >= window
    });

    player.set_paused(false);
    wait_until("playback moved off the first frame", || {
        player.position().is_some_and(|position| position > 0.0)
    });
    let before = player.position().expect("a position while playing");

    player.set_decode_scale(DecodeScale::Quarter);
    let lowest = lowest_cached_until(&player, "a quarter-scale frame reached the screen", || {
        player.frame_size() == Some((SIZE / 4, SIZE / 4))
    });
    assert!(!player.eof_reached(), "the fixture ended before the change");
    assert!(
        lowest >= window / 2,
        "the scale change emptied the cache, down to {lowest} frames of {window}"
    );
    assert_eq!(
        dropped_frames_not_decoded(&player),
        0,
        "the scale change dropped frames that had not decoded in time"
    );

    // six frame periods, so a picture held back by the refill shows up as a stop
    const KEEPS_MOVING: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + KEEPS_MOVING;
    while player.position() == Some(before) {
        assert!(
            Instant::now() < deadline,
            "the picture stopped at {before:?} over the scale change"
        );
        std::thread::sleep(POLL);
    }
    assert_eq!(
        dropped_frames_not_decoded(&player),
        0,
        "frames that had not decoded in time were dropped after the scale change"
    );

    // the same change with no clock to carry it
    player.set_paused(true);
    player.set_decode_scale(DecodeScale::Full);
    wait_until("the full-scale frame came back while paused", || {
        player.frame_size() == Some((SIZE, SIZE))
    });
}

#[test]
fn a_multi_reel_package_plays_every_reel_with_the_trims_the_cpl_states() {
    const SIZE: u32 = 64;
    const REELS: [(&str, &str, usize, u64); 3] = [
        ("head.mxf", "11111111-1111-1111-1111-111111111111", 3, 3),
        ("feature.mxf", "22222222-2222-2222-2222-222222222222", 4, 2),
        ("tail.mxf", "33333333-3333-3333-3333-333333333333", 5, 5),
    ];
    const TITLE: &str = "Three Reel Test";

    let directory = tempfile::tempdir().unwrap();
    let mut assets = vec![AssetMapAsset {
        id: "cc10cc10-0000-0000-0000-000000000000".into(),
        path: "CPL_test.xml".into(),
        ..Default::default()
    }];
    let mut reels = Vec::new();
    // one colour per reel, so a seek proves which reel is on screen
    let mut reel_colours = Vec::new();
    for (index, (name, picture_id, frames, played)) in REELS.iter().enumerate() {
        let codestreams = flat_codestreams(SIZE, SIZE, *frames, CINEMA_2K_PROFILE);
        // every frame of a reel is the same colour, distinct from the other reels
        let flat = vec![codestreams[index].clone(); *frames];
        write_mxf(&directory.path().join(name), &flat, None, SIZE, SIZE);
        reel_colours.push(frame_colour(index));
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
            picture_duration: *played,
            // a middle reel the composition enters late
            picture_entry_point: (*frames as u64) - *played,
            picture_width: SIZE,
            picture_height: SIZE,
            ..Default::default()
        });
    }
    std::fs::write(
        directory.path().join("ASSETMAP.xml"),
        AssetMap {
            uuid: "bbbbbbbb-0000-0000-0000-000000000000".into(),
            namespace: ns::AM_SMPTE.into(),
            assets,
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("CPL_test.xml"),
        DcpCpl {
            uuid: "cc10cc10-0000-0000-0000-000000000000".into(),
            namespace: ns::CPL_SMPTE.into(),
            title: TITLE.into(),
            reels,
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();

    // the exposed segments and the EDL mpv gets have to name the same spans
    let (segments, title) = composition_timeline::read_composition(directory.path());
    assert_eq!(title.as_deref(), Some(TITLE));
    assert_eq!(segments.len(), REELS.len());
    let edl = composition_timeline::mpv_source(directory.path())
        .expect("the package resolves to a source")
        .uri;
    for (segment, (name, _, frames, played)) in segments.iter().zip(REELS) {
        assert_eq!(segment.path, directory.path().join(name));
        let path = segment.path.to_string_lossy();
        let entry = frames as u64 - played;
        if entry == 0 {
            assert_eq!(segment.trim, None, "{name} plays whole");
            assert!(
                edl.contains(&format!("%{}%{path}", path.len())),
                "the EDL does not carry {name}: {edl}"
            );
            continue;
        }
        let trim = segment.trim.as_ref().expect("the middle reel is trimmed");
        let start = entry as f64 / f64::from(FRAMES_PER_SECOND);
        let length = played as f64 / f64::from(FRAMES_PER_SECOND);
        assert_eq!(trim.start_seconds, start);
        assert_eq!(trim.length_seconds, Some(length));
        assert!(
            edl.contains(&format!("%{}%{path},{start},{length}", path.len())),
            "the EDL does not carry {name}'s span: {edl}"
        );
    }

    let played_total: u64 = REELS.iter().map(|(_, _, _, played)| played).sum();
    let player = loaded_player(directory.path());
    assert_eq!(
        player.duration(),
        Some(played_total as f64 / f64::from(FRAMES_PER_SECOND)),
        "the duration is the sum of the trims, not of the files"
    );
    assert!(
        player.metadata_json().contains(TITLE),
        "the composition title names the source: {}",
        player.metadata_json()
    );
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        reel_colours[0]
    );

    // the first frame of the second reel is the fourth frame of the composition
    forget_frames(&player);
    player.seek_absolute(REELS[0].3 as f64 / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        reel_colours[1],
        "a seek past the first reel does not show the second reel"
    );

    forget_frames(&player);
    player.seek_absolute((REELS[0].3 + REELS[1].3) as f64 / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        reel_colours[2]
    );
}

fn write_package(directory: &Path, cpl_id: &str, assets: &[(&str, &str)], reels: Vec<DcpCplReel>) {
    let cpl_name = format!("CPL_{cpl_id}.xml");
    let assets = std::iter::once((cpl_id, cpl_name.as_str()))
        .chain(assets.iter().copied())
        .map(|(id, path)| AssetMapAsset {
            id: id.into(),
            path: path.into(),
            ..Default::default()
        })
        .collect();
    std::fs::write(
        directory.join("ASSETMAP.xml"),
        AssetMap {
            uuid: "bbbbbbbb-0000-0000-0000-000000000000".into(),
            namespace: ns::AM_SMPTE.into(),
            assets,
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();
    std::fs::write(
        directory.join(&cpl_name),
        DcpCpl {
            uuid: cpl_id.into(),
            namespace: ns::CPL_SMPTE.into(),
            title: cpl_name.clone(),
            reels,
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();
}

#[test]
fn a_version_file_plays_its_original_version_reel_in_order() {
    const SIZE: u32 = 64;
    const FRAMES_PER_REEL: usize = 3;
    const ORIGINAL_VERSION_CPL: &str = "0e000000-0000-0000-0000-000000000000";
    const VERSION_FILE_CPL: &str = "0f000000-0000-0000-0000-000000000000";
    // reel 2 is only in the original version
    const REELS: [(&str, &str); 3] = [
        ("11111111-1111-1111-1111-111111111111", "reel1.mxf"),
        ("22222222-2222-2222-2222-222222222222", "reel2.mxf"),
        ("33333333-3333-3333-3333-333333333333", "reel3.mxf"),
    ];
    let library = tempfile::tempdir().unwrap();
    let original_version = library.path().join("ov");
    let version_file = library.path().join("vf");
    std::fs::create_dir_all(&original_version).unwrap();
    std::fs::create_dir_all(&version_file).unwrap();

    let codestreams =
        flat_codestreams(SIZE, SIZE, FRAMES_PER_REEL * REELS.len(), CINEMA_2K_PROFILE);
    let reels: Vec<DcpCplReel> = REELS
        .iter()
        .enumerate()
        .map(|(index, (picture_id, name))| {
            let package = if index == 1 {
                &original_version
            } else {
                &version_file
            };
            let frames = &codestreams[index * FRAMES_PER_REEL..(index + 1) * FRAMES_PER_REEL];
            write_mxf(&package.join(name), frames, None, SIZE, SIZE);
            DcpCplReel {
                reel_id: format!("aaaaaaaa-0000-0000-0000-00000000000{index}"),
                picture_id: (*picture_id).into(),
                picture_edit_rate_num: FRAMES_PER_SECOND,
                picture_edit_rate_den: 1,
                picture_duration: FRAMES_PER_REEL as u64,
                picture_width: SIZE,
                picture_height: SIZE,
                ..Default::default()
            }
        })
        .collect();
    write_package(
        &original_version,
        ORIGINAL_VERSION_CPL,
        &[REELS[1]],
        vec![reels[1].clone()],
    );
    write_package(
        &version_file,
        VERSION_FILE_CPL,
        &[REELS[0], REELS[2]],
        reels,
    );

    let searched = vec![version_file.clone(), original_version.clone()];
    assert_eq!(
        composition_timeline::find_original_version_packages(&version_file, &searched),
        Ok(vec![original_version.clone()])
    );
    assert!(!GrokPlayer::accepts(&version_file));
    assert!(GrokPlayer::accepts_with_packages(
        &version_file,
        std::slice::from_ref(&original_version)
    ));

    let player = GrokPlayer::new();
    player.init_software().unwrap();
    let error = player
        .load(&version_file, None)
        .expect_err("reel 2 is in no package searched");
    assert!(error.contains(REELS[1].0), "{error}");
    assert!(
        error.contains(&version_file.display().to_string()),
        "{error}"
    );

    player
        .load_with_packages(&version_file, None, &[original_version])
        .expect("load");
    wait_until("the first frame was composed", || {
        player.frame_size().is_some()
    });
    let total_frames = FRAMES_PER_REEL * REELS.len();
    assert_eq!(
        player.duration(),
        Some(total_frames as f64 / f64::from(FRAMES_PER_SECOND))
    );
    let (width, height) = (SIZE as usize, SIZE as usize);
    assert_eq!(shown_colour(&player, width, height), frame_colour(0));
    for frame in 1..total_frames {
        forget_frames(&player);
        player.frame_step();
        wait_for_frame(&player);
        assert_eq!(
            shown_colour(&player, width, height),
            frame_colour(frame),
            "frame {frame}"
        );
    }
}

#[test]
fn a_square_picture_is_letterboxed_into_a_wide_surface() {
    const SIZE: u32 = 64;
    const SURFACE_WIDTH: usize = 320;
    const SURFACE_HEIGHT: usize = 180;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 2);

    let player = loaded_player(&mxf);
    let rectangle = player
        .picture_rectangle(SURFACE_WIDTH as u32, SURFACE_HEIGHT as u32)
        .expect("a composed frame lands somewhere");
    assert_eq!(
        (rectangle.x, rectangle.y, rectangle.width, rectangle.height),
        (70, 0, 180, 180)
    );

    let buffer = software_frame(&player, SURFACE_WIDTH, SURFACE_HEIGHT);
    let colour = frame_colour(0);
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 0, 90), [0, 0, 0], "left bar");
    assert_eq!(
        pixel(&buffer, SURFACE_WIDTH, 69, 90),
        [0, 0, 0],
        "the bar ends at the rectangle"
    );
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 70, 90), colour, "picture");
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 249, 90), colour, "picture");
    assert_eq!(
        pixel(&buffer, SURFACE_WIDTH, 250, 90),
        [0, 0, 0],
        "right bar"
    );
    assert_eq!(
        pixel(&buffer, SURFACE_WIDTH, 319, 179),
        [0, 0, 0],
        "right bar"
    );
}

#[test]
fn a_presentation_change_while_paused_redraws_the_still_frame() {
    const SIZE: u32 = 64;
    const SURFACE_WIDTH: usize = 320;
    const SURFACE_HEIGHT: usize = 180;
    const BRIGHTNESS: f32 = 0.5;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 2);

    let player = loaded_player(&mxf);
    assert!(
        !player.wants_redraw(),
        "a paused player has nothing to redraw"
    );
    player.set_presentation(PresentationSettings {
        brightness: BRIGHTNESS,
        masks: PictureMasks {
            left: 0.5,
            ..PictureMasks::default()
        },
        scaling: PictureScaling::Fill,
    });
    wait_until("the presentation change asked for a redraw", || {
        player.wants_redraw()
    });

    let rectangle = player
        .picture_rectangle(SURFACE_WIDTH as u32, SURFACE_HEIGHT as u32)
        .expect("a composed frame lands somewhere");
    assert_eq!(
        (rectangle.x, rectangle.y, rectangle.width, rectangle.height),
        (0, 0, 320, 180),
        "fill covers the surface"
    );
    let buffer = software_frame(&player, SURFACE_WIDTH, SURFACE_HEIGHT);
    let dimmed = frame_colour(0).map(|sample| (f32::from(sample) * BRIGHTNESS).round() as u8);
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 0, 90), [0, 0, 0], "masked");
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 159, 90), [0, 0, 0], "masked");
    assert_eq!(pixel(&buffer, SURFACE_WIDTH, 160, 90), dimmed, "dimmed");
    assert_eq!(
        pixel(&buffer, SURFACE_WIDTH, 319, 0),
        dimmed,
        "dimmed corner"
    );
}

#[test]
fn a_subtitle_draws_in_its_own_band_only_while_its_cue_runs() {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    const CUE_END_MS: u64 = 500;
    let directory = tempfile::tempdir().unwrap();
    // every frame the same colour, so a difference is the subtitle and nothing else
    let mxf = directory.path().join("picture.mxf");
    let frames = vec![flat_codestreams(WIDTH, HEIGHT, 1, CINEMA_2K_PROFILE).remove(0); 24];
    write_mxf(&mxf, &frames, None, WIDTH, HEIGHT);
    let srt = directory.path().join("cues.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:00,500\nHELLO THERE\n\n").unwrap();

    let player = loaded_player(&mxf);
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let plain = software_frame(&player, width, height);

    forget_frames(&player);
    player
        .set_subtitle_file(SubtitleSlot::Subtitle, Some(&srt))
        .expect("srt loads");
    wait_for_frame(&player);
    let burnt = software_frame(&player, width, height);
    let changed = changed_rows(&plain, &burnt, width, height);
    assert!(
        !changed.is_empty(),
        "the subtitle drew nothing on the frame"
    );
    assert!(
        changed.iter().all(|row| *row > height / 2),
        "a subtitle drew outside the bottom band, at rows {changed:?}"
    );

    // a frame past the cue's end is the plain frame again
    forget_frames(&player);
    let past_the_cue = (CUE_END_MS * u64::from(FRAMES_PER_SECOND) / 1000 + 4) as f64
        / f64::from(FRAMES_PER_SECOND);
    player.seek_absolute(past_the_cue);
    wait_for_frame(&player);
    assert!(
        changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height
        )
        .is_empty(),
        "the subtitle drew past the end of its cue"
    );

    forget_frames(&player);
    player.seek_absolute(0.0);
    wait_for_frame(&player);
    player
        .set_subtitle_file(SubtitleSlot::Caption, Some(&srt))
        .expect("caption srt loads");
    wait_until("the caption drew", || {
        changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height,
        )
        .iter()
        .any(|row| *row < height / 2)
    });
    player.set_subtitle_visibility(SubtitleSlot::Subtitle, false);
    wait_until("only the caption is left", || {
        let changed = changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height,
        );
        !changed.is_empty() && changed.iter().all(|row| *row < height / 2)
    });

    player.set_subtitle_visibility(SubtitleSlot::Caption, false);
    wait_until("hiding both tracks restores the plain frame", || {
        changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height,
        )
        .is_empty()
    });
}

// one cue in the second reel, from its sixth frame to its twelfth
const SECOND_REEL_SUBTITLE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<SubtitleReel xmlns=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\">
  <Id>urn:uuid:5b000000-0000-4000-8000-000000000001</Id>
  <ContentTitleText>reel 2</ContentTitleText>
  <Language>de</Language>
  <EditRate>24 1</EditRate>
  <TimeCodeRate>24</TimeCodeRate>
  <SubtitleList>
    <Font Color=\"FFFFFFFF\">
      <Subtitle SpotNumber=\"1\" TimeIn=\"00:00:00:06\" TimeOut=\"00:00:00:12\">
        <Text Valign=\"bottom\" Vposition=\"10\" Halign=\"center\">REEL TWO</Text>
      </Subtitle>
    </Font>
  </SubtitleList>
</SubtitleReel>";

// the CPL with a MainSubtitle in the reel at `reel`
fn with_reel_subtitle(cpl: &str, reel: usize, subtitle_id: &str, frames: u64) -> String {
    let element = format!(
        "<MainSubtitle><Id>urn:uuid:{subtitle_id}</Id><EditRate>24 1</EditRate>\
         <IntrinsicDuration>{frames}</IntrinsicDuration><EntryPoint>0</EntryPoint>\
         <Duration>{frames}</Duration><Language>de</Language></MainSubtitle>"
    );
    let at = cpl
        .match_indices("</AssetList>")
        .nth(reel)
        .expect("the reel exists")
        .0;
    format!("{}{element}{}", &cpl[..at], &cpl[at..])
}

#[test]
fn a_cpl_subtitle_track_shows_its_cue_at_the_right_frame_of_the_second_reel() {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    const REEL_FRAMES: u64 = 12;
    const CPL_ID: &str = "5c000000-0000-4000-8000-000000000001";
    const PICTURE_IDS: [&str; 2] = [
        "5c000000-0000-4000-8000-000000000002",
        "5c000000-0000-4000-8000-000000000003",
    ];
    const SUBTITLE_ID: &str = "5c000000-0000-4000-8000-000000000004";
    let directory = tempfile::tempdir().unwrap();
    let frames =
        vec![flat_codestreams(WIDTH, HEIGHT, 1, CINEMA_2K_PROFILE).remove(0); REEL_FRAMES as usize];
    write_mxf(
        &directory.path().join("reel1.mxf"),
        &frames,
        None,
        WIDTH,
        HEIGHT,
    );
    write_mxf(
        &directory.path().join("reel2.mxf"),
        &frames,
        None,
        WIDTH,
        HEIGHT,
    );
    timed_text_track(
        directory.path(),
        "subtitle",
        SECOND_REEL_SUBTITLE,
        REEL_FRAMES,
    );
    let reels = PICTURE_IDS
        .iter()
        .enumerate()
        .map(|(index, picture_id)| DcpCplReel {
            reel_id: format!("5c000000-0000-4000-8000-00000000001{index}"),
            picture_id: (*picture_id).into(),
            picture_edit_rate_num: FRAMES_PER_SECOND,
            picture_edit_rate_den: 1,
            picture_duration: REEL_FRAMES,
            picture_width: WIDTH,
            picture_height: HEIGHT,
            ..Default::default()
        })
        .collect();
    write_package(
        directory.path(),
        CPL_ID,
        &[
            (PICTURE_IDS[0], "reel1.mxf"),
            (PICTURE_IDS[1], "reel2.mxf"),
            (SUBTITLE_ID, "subtitle.mxf"),
        ],
        reels,
    );
    let cpl_path = directory.path().join(format!("CPL_{CPL_ID}.xml"));
    let cpl = std::fs::read_to_string(&cpl_path).unwrap();
    std::fs::write(
        &cpl_path,
        with_reel_subtitle(&cpl, 1, SUBTITLE_ID, REEL_FRAMES),
    )
    .unwrap();

    let player = loaded_player(directory.path());
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let plain = software_frame(&player, width, height);
    let frame_rows = |frame: u64| {
        forget_frames(&player);
        player.seek_absolute(frame as f64 / f64::from(FRAMES_PER_SECOND));
        wait_for_frame(&player);
        changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height,
        )
    };

    assert!(
        frame_rows(REEL_FRAMES + 5).is_empty(),
        "the cue drew before its first frame"
    );
    let cue_rows = frame_rows(REEL_FRAMES + 6);
    assert!(
        !cue_rows.is_empty(),
        "the cue drew nothing on its first frame"
    );
    assert!(
        cue_rows.iter().all(|row| *row > height / 2),
        "the cue left the bottom band: {cue_rows:?}"
    );
    let metadata: serde_json::Value = serde_json::from_str(&player.metadata_json()).unwrap();
    assert_eq!(metadata["subtitle_track"]["language"], "de");
    assert_eq!(metadata["caption_track"], serde_json::Value::Null);
}

fn timed_text_track(directory: &Path, name: &str, xml: &str, frames: u64) -> PathBuf {
    let source = directory.join(format!("{name}.xml"));
    std::fs::write(&source, xml).unwrap();
    let output = directory.join(format!("{name}.mxf"));
    let track = postkit::mxf_wrap::mxf_wrap(&postkit::mxf_wrap::MxfWrapOptions {
        input_files: vec![source],
        output: output.clone(),
        essence_type: postkit::mxf_wrap::EssenceType::TimedText,
        standard: postkit::mxf_wrap::MxfStandard::AsDcp,
        fps_num: FRAMES_PER_SECOND,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: Vec::new(),
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: Some(frames as u32),
    });
    assert!(track.success, "timed text wrap failed: {}", track.error);
    output
}

#[test]
fn closed_captions_in_two_languages_switch_on_request_and_keep_the_choice() {
    const SIZE: u32 = 64;
    const FRAMES: u64 = 12;
    const CPL_ID: &str = "5f000000-0000-4000-8000-000000000001";
    const PICTURE_ID: &str = "5f000000-0000-4000-8000-000000000002";
    const CAPTION_IDS: [&str; 2] = [
        "5f000000-0000-4000-8000-000000000003",
        "5f000000-0000-4000-8000-000000000004",
    ];
    let directory = tempfile::tempdir().unwrap();
    let frames =
        vec![flat_codestreams(SIZE, SIZE, 1, CINEMA_2K_PROFILE).remove(0); FRAMES as usize];
    write_mxf(
        &directory.path().join("picture.mxf"),
        &frames,
        None,
        SIZE,
        SIZE,
    );
    for (index, language) in ["fr", "en"].iter().enumerate() {
        let xml = SECOND_REEL_SUBTITLE
            .replace(
                "<Language>de</Language>",
                &format!("<Language>{language}</Language>"),
            )
            .replace("5b000000-0000-4000-8000-000000000001", CAPTION_IDS[index]);
        timed_text_track(
            directory.path(),
            &format!("caption_{language}"),
            &xml,
            FRAMES,
        );
    }
    write_package(
        directory.path(),
        CPL_ID,
        &[
            (PICTURE_ID, "picture.mxf"),
            (CAPTION_IDS[0], "caption_fr.mxf"),
            (CAPTION_IDS[1], "caption_en.mxf"),
        ],
        vec![DcpCplReel {
            reel_id: "5f000000-0000-4000-8000-000000000010".into(),
            picture_id: PICTURE_ID.into(),
            picture_edit_rate_num: FRAMES_PER_SECOND,
            picture_edit_rate_den: 1,
            picture_duration: FRAMES,
            picture_width: SIZE,
            picture_height: SIZE,
            ..Default::default()
        }],
    );
    let captions: String = CAPTION_IDS
        .iter()
        .zip(["fr", "en"])
        .map(|(id, language)| {
            format!(
                "<tt:ClosedCaption xmlns:tt=\"http://www.smpte-ra.org/schemas/429-12/2008/TT\">\
                 <Id>urn:uuid:{id}</Id><EditRate>24 1</EditRate><IntrinsicDuration>{FRAMES}</IntrinsicDuration>\
                 <EntryPoint>0</EntryPoint><Duration>{FRAMES}</Duration><Language>{language}</Language>\
                 </tt:ClosedCaption>"
            )
        })
        .collect();
    let cpl_path = directory.path().join(format!("CPL_{CPL_ID}.xml"));
    let cpl = std::fs::read_to_string(&cpl_path).unwrap();
    std::fs::write(
        &cpl_path,
        cpl.replacen("</AssetList>", &format!("{captions}</AssetList>"), 1),
    )
    .unwrap();
    let caption_track = |player: &GrokPlayer| {
        let metadata: serde_json::Value = serde_json::from_str(&player.metadata_json()).unwrap();
        metadata["caption_track"].clone()
    };

    let player = loaded_player(directory.path());
    wait_until("the caption track is reported", || {
        caption_track(&player)["language"] == "fr"
    });
    assert_eq!(
        caption_track(&player)["languages"],
        serde_json::json!(["fr", "en"])
    );
    player
        .set_subtitle_language(SubtitleSlot::Caption, "en")
        .expect("an en track exists");
    wait_until("the en track shows", || {
        caption_track(&player)["language"] == "en"
    });
    assert!(
        player
            .set_subtitle_language(SubtitleSlot::Caption, "de")
            .is_err()
    );

    player.load(directory.path(), None).expect("reload");
    wait_until("the next load keeps en", || {
        caption_track(&player)["language"] == "en"
    });
}

#[test]
fn a_subtitle_offset_while_paused_moves_the_drawn_cue() {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    // a tenth of 180 rows
    const OFFSET_PERCENT: f32 = 10.0;
    const OFFSET_ROWS: usize = 18;
    let directory = tempfile::tempdir().unwrap();
    let mxf = directory.path().join("picture.mxf");
    let frames = vec![flat_codestreams(WIDTH, HEIGHT, 1, CINEMA_2K_PROFILE).remove(0); 24];
    write_mxf(&mxf, &frames, None, WIDTH, HEIGHT);
    let srt = directory.path().join("cues.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nHELLO THERE\n\n").unwrap();

    let player = loaded_player(&mxf);
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let plain = software_frame(&player, width, height);
    player
        .set_subtitle_file(SubtitleSlot::Subtitle, Some(&srt))
        .expect("srt loads");
    wait_for_frame(&player);
    let unmoved = changed_rows(
        &plain,
        &software_frame(&player, width, height),
        width,
        height,
    );
    assert!(!unmoved.is_empty(), "the subtitle drew nothing");

    forget_frames(&player);
    player.set_subtitle_presentation(SubtitlePresentation {
        vertical_offset_percent: OFFSET_PERCENT,
        ..SubtitlePresentation::default()
    });
    wait_for_frame(&player);
    let moved = changed_rows(
        &plain,
        &software_frame(&player, width, height),
        width,
        height,
    );
    let expected: Vec<usize> = unmoved.iter().map(|row| row - OFFSET_ROWS).collect();
    assert_eq!(
        moved, expected,
        "the cue did not move up {OFFSET_ROWS} rows"
    );

    forget_frames(&player);
    player.set_subtitle_presentation(SubtitlePresentation {
        vertical_offset_percent: 1000.0,
        ..SubtitlePresentation::default()
    });
    wait_for_frame(&player);
    let at_the_top = changed_rows(
        &plain,
        &software_frame(&player, width, height),
        width,
        height,
    );
    assert_eq!(
        at_the_top.len(),
        unmoved.len(),
        "the cue lost rows at the frame edge"
    );
    assert!(
        at_the_top[0] < height / 10,
        "the cue stopped short of the top, at rows {at_the_top:?}"
    );
}

// two sources whose frames carry the colours 0 to first_frames - 1 and first_frames to 9
fn two_compositions(directory: &Path, size: u32, first_frames: usize) -> (PathBuf, PathBuf) {
    let mut frames = flat_codestreams(size, size, DISTINCT_FRAME_COLOURS, CINEMA_2K_PROFILE);
    let next_frames = frames.split_off(first_frames);
    let first = directory.join("first.mxf");
    let next = directory.join("next.mxf");
    write_mxf(&first, &frames, None, size, size);
    write_mxf(&next, &next_frames, None, size, size);
    (first, next)
}

fn metadata_field(player: &GrokPlayer, field: &str) -> serde_json::Value {
    let metadata: serde_json::Value =
        serde_json::from_str(&player.metadata_json()).expect("metadata is JSON");
    metadata[field].clone()
}

fn decode_capacity(player: &GrokPlayer) -> Option<f64> {
    metadata_field(player, "decode_capacity_fps").as_f64()
}

// the first reading after a change, once enough decodes are timed
fn measured_capacity(player: &GrokPlayer) -> f64 {
    wait_until("the decode capacity was measured", || {
        decode_capacity(player).is_some()
    });
    decode_capacity(player).unwrap()
}

// the paused frame comes back after the change, by then the measurement has restarted
fn capacity_after(player: &GrokPlayer, change: impl Fn(&GrokPlayer)) -> f64 {
    forget_frames(player);
    change(player);
    wait_for_frame(player);
    measured_capacity(player)
}

// other tests decode at the same time, so one reading against one other is noisy
const CAPACITY_ROUNDS: usize = 3;

fn median_capacity_ratio(
    player: &GrokPlayer,
    base: impl Fn(&GrokPlayer),
    changed: impl Fn(&GrokPlayer),
) -> (f64, Vec<(f64, f64)>) {
    // the player starts in the base state, so each round changes away from it first
    let mut readings: Vec<(f64, f64)> = (0..CAPACITY_ROUNDS)
        .map(|_| {
            let changed_reading = capacity_after(player, &changed);
            (capacity_after(player, &base), changed_reading)
        })
        .collect();
    readings.sort_by(|a, b| (a.1 / a.0).total_cmp(&(b.1 / b.0)));
    let (base_reading, changed_reading) = readings[CAPACITY_ROUNDS / 2];
    (changed_reading / base_reading, readings)
}

#[test]
fn the_decode_capacity_reads_once_playback_starts_and_rises_at_half_scale() {
    const SIZE: u32 = 512;
    const FRAMES: usize = 72;
    let directory = tempfile::tempdir().unwrap();
    let frame = flat_codestreams(SIZE, SIZE, 1, CINEMA_2K_PROFILE).remove(0);
    let mxf = directory.path().join("picture.mxf");
    write_mxf(&mxf, &vec![frame; FRAMES], None, SIZE, SIZE);

    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player.load(&mxf, None).expect("load");
    player.set_paused(false);
    let full_scale = measured_capacity(&player);
    assert!(full_scale > 0.0, "{full_scale}");

    player.set_paused(true);
    let (ratio, readings) = median_capacity_ratio(
        &player,
        |player| player.set_decode_scale(DecodeScale::Full),
        |player| player.set_decode_scale(DecodeScale::Half),
    );
    assert!(
        ratio > 1.3,
        "half scale against full scale, fps: {readings:?}"
    );
}

#[test]
fn a_queued_source_plays_on_from_the_last_frame_with_no_gap() {
    const SIZE: u32 = 64;
    const FIRST_FRAMES: usize = 5;
    let directory = tempfile::tempdir().unwrap();
    let (first, next) = two_compositions(directory.path(), SIZE, FIRST_FRAMES);
    let player = loaded_player(&first);
    player
        .queue_next(&next, None)
        .expect("queue the next source");
    assert_eq!(
        metadata_field(&player, "source"),
        first.display().to_string()
    );
    assert_eq!(
        metadata_field(&player, "queued_source"),
        next.display().to_string()
    );

    player.set_paused(false);
    let mut shown = vec![shown_colour(&player, SIZE as usize, SIZE as usize)];
    let deadline = Instant::now() + PATIENCE;
    while !player.eof_reached() {
        assert!(Instant::now() < deadline, "playback did not reach the end");
        let colour = shown_colour(&player, SIZE as usize, SIZE as usize);
        if shown.last() != Some(&colour) {
            shown.push(colour);
        }
        std::thread::sleep(POLL);
    }
    // eof is flagged on the tick that presents the last frame
    let last_shown = shown_colour(&player, SIZE as usize, SIZE as usize);
    if shown.last() != Some(&last_shown) {
        shown.push(last_shown);
    }
    let every_frame: Vec<[u8; 3]> = (0..DISTINCT_FRAME_COLOURS).map(frame_colour).collect();
    // a frame the scheduler woke too late to show may be missing, never out of order
    let mut remaining = every_frame.iter();
    assert!(
        shown
            .iter()
            .all(|colour| remaining.any(|frame| frame == colour)),
        "the two sources did not play their frames once each, in order: {shown:?}"
    );
    assert_eq!(shown.first(), every_frame.first());
    assert_eq!(shown.last(), every_frame.last());
    assert_eq!(
        dropped_frames_not_decoded(&player),
        0,
        "frames that had not decoded in time were dropped"
    );
    assert_eq!(
        metadata_field(&player, "source"),
        next.display().to_string()
    );
    assert_eq!(
        metadata_field(&player, "queued_source"),
        serde_json::Value::Null
    );
    let last = (DISTINCT_FRAME_COLOURS - FIRST_FRAMES - 1) as f64 / f64::from(FRAMES_PER_SECOND);
    assert_eq!(player.position(), Some(last));
}

fn ranged(in_frame: u64, out_frame: Option<u64>) -> SourceOptions {
    SourceOptions {
        range: Some(FrameRange {
            in_frame,
            out_frame,
        }),
        ..SourceOptions::default()
    }
}

// each colour once as it changes, from play until the end of the last source
fn colours_played_to_the_end(player: &GrokPlayer, size: usize) -> Vec<[u8; 3]> {
    let mut shown = vec![shown_colour(player, size, size)];
    player.set_paused(false);
    let deadline = Instant::now() + PATIENCE;
    while !player.eof_reached() {
        assert!(Instant::now() < deadline, "playback did not reach the end");
        let colour = shown_colour(player, size, size);
        if shown.last() != Some(&colour) {
            shown.push(colour);
        }
        std::thread::sleep(POLL);
    }
    let last = shown_colour(player, size, size);
    if shown.last() != Some(&last) {
        shown.push(last);
    }
    shown
}

// a frame the scheduler woke too late to show may be missing from shown, never out of order
fn assert_played_in_order(shown: &[[u8; 3]], frames: &[usize]) {
    let expected: Vec<[u8; 3]> = frames.iter().map(|frame| frame_colour(*frame)).collect();
    let mut remaining = expected.iter();
    assert!(
        shown
            .iter()
            .all(|colour| remaining.any(|frame| frame == colour)),
        "frames {frames:?} did not play once each, in order: {shown:?}"
    );
    assert_eq!(shown.first(), expected.first(), "the first frame shown");
    assert_eq!(shown.last(), expected.last(), "the last frame shown");
}

#[test]
fn a_frame_range_plays_from_its_in_frame_to_the_frame_before_its_out_frame() {
    const SIZE: u32 = 64;
    const IN_FRAME: u64 = 3;
    const OUT_FRAME: u64 = 7;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(
        directory.path(),
        "picture.mxf",
        SIZE,
        SIZE,
        DISTINCT_FRAME_COLOURS,
    );
    let (width, height) = (SIZE as usize, SIZE as usize);
    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player
        .load_with_options(&mxf, ranged(IN_FRAME, Some(OUT_FRAME)))
        .expect("load the range");
    wait_until("the first frame was composed", || {
        player.frame_size().is_some()
    });

    let fps = f64::from(FRAMES_PER_SECOND);
    assert_eq!(player.duration(), Some((OUT_FRAME - IN_FRAME) as f64 / fps));
    assert_eq!(player.position(), Some(0.0));
    assert_eq!(shown_colour(&player, width, height), frame_colour(3));
    forget_frames(&player);
    player.frame_back_step();
    player.frame_step();
    wait_for_frame(&player);
    assert_eq!(
        shown_colour(&player, width, height),
        frame_colour(4),
        "a step back at the in frame left the range"
    );
    forget_frames(&player);
    player.seek_absolute(0.0);
    wait_for_frame(&player);

    let shown = colours_played_to_the_end(&player, width);
    assert_played_in_order(&shown, &[3, 4, 5, 6]);
    assert_eq!(
        player.position(),
        Some((OUT_FRAME - IN_FRAME - 1) as f64 / fps)
    );
    assert_eq!(dropped_frames_not_decoded(&player), 0);

    forget_frames(&player);
    player.frame_step();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        shown_colour(&player, width, height),
        frame_colour(6),
        "a step at the last frame of the range went past the out frame"
    );
}

#[test]
fn a_queued_range_takes_over_at_the_out_frame_of_the_range_playing() {
    const SIZE: u32 = 64;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(
        directory.path(),
        "picture.mxf",
        SIZE,
        SIZE,
        DISTINCT_FRAME_COLOURS,
    );
    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player
        .load_with_options(&mxf, ranged(2, Some(5)))
        .expect("load the first range");
    player
        .queue_next_with_options(&mxf, ranged(6, Some(9)))
        .expect("queue the second range");
    wait_until("the first frame was composed", || {
        player.frame_size().is_some()
    });

    let shown = colours_played_to_the_end(&player, SIZE as usize);

    assert_played_in_order(&shown, &[2, 3, 4, 6, 7, 8]);
    assert_eq!(dropped_frames_not_decoded(&player), 0);
    assert_eq!(
        player.position(),
        Some(2.0 / f64::from(FRAMES_PER_SECOND)),
        "the position counts from the queued range's in frame"
    );
}

#[test]
fn a_subtitle_keeps_its_composition_time_inside_a_range() {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    // the cue runs over composition frames 12 to 17
    const IN_FRAME: u64 = 12;
    const FRAMES_PAST_THE_CUE: f64 = 7.0;
    let directory = tempfile::tempdir().unwrap();
    let mxf = directory.path().join("picture.mxf");
    let frames = vec![flat_codestreams(WIDTH, HEIGHT, 1, CINEMA_2K_PROFILE).remove(0); 24];
    write_mxf(&mxf, &frames, None, WIDTH, HEIGHT);
    let srt = directory.path().join("cues.srt");
    std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:00,750\nHELLO THERE\n\n").unwrap();
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player
        .load_with_options(&mxf, ranged(IN_FRAME, None))
        .expect("load the range");
    wait_until("the first frame was composed", || {
        player.frame_size().is_some()
    });
    let plain = software_frame(&player, width, height);

    forget_frames(&player);
    player
        .set_subtitle_file(SubtitleSlot::Subtitle, Some(&srt))
        .expect("srt loads");
    wait_for_frame(&player);
    assert!(
        !changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height
        )
        .is_empty(),
        "the cue did not draw on the in frame, composition frame {IN_FRAME}"
    );

    forget_frames(&player);
    player.seek_absolute(FRAMES_PAST_THE_CUE / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert!(
        changed_rows(
            &plain,
            &software_frame(&player, width, height),
            width,
            height
        )
        .is_empty(),
        "the cue drew past its end"
    );
}

#[test]
fn a_range_outside_the_composition_fails_naming_its_frames_and_the_length() {
    const SIZE: u32 = 64;
    const FRAMES: usize = 6;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, FRAMES);
    let player = GrokPlayer::new();
    for (range, frames) in [
        (ranged(2, Some(9)), "frames 2 to 9"),
        (ranged(4, Some(4)), "frames 4 to 4"),
        (ranged(6, None), "frames 6 to 6"),
    ] {
        let error = player
            .load_with_options(&mxf, range)
            .expect_err("the range is refused");
        assert!(error.contains(frames), "{error}");
        assert!(error.contains("which is 6 frames long"), "{error}");
    }
    player.load(&mxf, None).expect("load the whole composition");
    let error = player
        .queue_next_with_options(&mxf, ranged(1, Some(7)))
        .expect_err("the queued range is refused");
    assert!(error.contains("frames 1 to 7"), "{error}");
    assert_eq!(
        metadata_field(&player, "queued_source"),
        serde_json::Value::Null
    );
}

#[test]
fn a_source_queued_at_the_end_plays_when_play_is_pressed() {
    const SIZE: u32 = 64;
    const FIRST_FRAMES: usize = 2;
    let directory = tempfile::tempdir().unwrap();
    let (first, next) = two_compositions(directory.path(), SIZE, FIRST_FRAMES);
    let player = loaded_player(&first);
    player.set_paused(false);
    wait_until("the first source reached its end", || player.eof_reached());

    player
        .queue_next(&next, None)
        .expect("queue the next source");
    assert!(
        !player.eof_reached(),
        "the end is not the end with a source queued"
    );
    player.set_paused(false);
    wait_until("the queued source played to its end", || {
        player.eof_reached()
    });
    assert_eq!(
        metadata_field(&player, "source"),
        next.display().to_string()
    );
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        frame_colour(DISTINCT_FRAME_COLOURS - 1)
    );

    player.queue_next(&first, None).expect("queue again");
    player.clear_queued();
    wait_until("clearing the queue ends it", || {
        metadata_field(&player, "queued_source").is_null()
    });
    assert!(player.eof_reached());
}

fn changed_rows(before: &[u8], after: &[u8], width: usize, height: usize) -> Vec<usize> {
    (0..height)
        .filter(|row| {
            let start = row * width * SOFTWARE_BYTES_PER_PIXEL;
            let end = start + width * SOFTWARE_BYTES_PER_PIXEL;
            before[start..end] != after[start..end]
        })
        .collect()
}

#[test]
fn an_overlay_lands_on_the_source_pixels_it_names_at_every_decode_scale() {
    const SIZE: u32 = 64;
    const OVERLAY: OverlayRectangle = OverlayRectangle {
        x: 16,
        y: 16,
        width: 16,
        height: 16,
        colour: [255, 0, 0],
        alpha: 128,
    };
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, 2);

    let player = loaded_player(&mxf);
    let under = frame_colour(0);
    let blended: Vec<u8> = OVERLAY
        .colour
        .iter()
        .zip(under)
        .map(|(&over, under)| ((u32::from(over) * 128 + u32::from(under) * 127 + 127) / 255) as u8)
        .collect();

    forget_frames(&player);
    player.set_overlay(vec![OVERLAY]);
    wait_for_frame(&player);
    let full = software_frame(&player, SIZE as usize, SIZE as usize);
    assert_eq!(
        pixel(&full, SIZE as usize, 20, 20),
        blended[..],
        "the overlay did not land inside its rectangle"
    );
    assert_eq!(
        pixel(&full, SIZE as usize, 8, 8),
        under,
        "the overlay bled outside its rectangle"
    );
    assert_eq!(
        pixel(&full, SIZE as usize, 32, 32),
        under,
        "past the corner"
    );

    // at reduce 1 the frame halves and so does the rectangle
    player.set_decode_scale(DecodeScale::Half);
    wait_until("the half-scale frame arrived", || {
        player.frame_size() == Some((SIZE / 2, SIZE / 2))
    });
    let half_size = (SIZE / 2) as usize;
    let half = software_frame(&player, half_size, half_size);
    assert_eq!(pixel(&half, half_size, 10, 10), blended[..]);
    assert_eq!(pixel(&half, half_size, 4, 4), under);
    assert_eq!(pixel(&half, half_size, 16, 16), under);
}

#[test]
fn a_directory_of_codestreams_plays_one_frame_per_file() {
    const SIZE: u32 = 32;
    const FILES: usize = 5;
    let directory = tempfile::tempdir().unwrap();
    for (index, codestream) in flat_codestreams(SIZE, SIZE, FILES, CINEMA_2K_PROFILE)
        .into_iter()
        .enumerate()
    {
        std::fs::write(directory.path().join(format!("{index:04}.j2c")), codestream).unwrap();
    }

    let player = loaded_player(directory.path());
    assert_eq!(
        player.duration(),
        Some(FILES as f64 / f64::from(FRAMES_PER_SECOND)),
        "a codestream directory plays one frame a file at {FRAMES_PER_SECOND} fps"
    );
    assert_eq!(player.source_size(), Some((SIZE, SIZE)));
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        frame_colour(0)
    );

    forget_frames(&player);
    player.seek_absolute(4.0 / f64::from(FRAMES_PER_SECOND));
    wait_for_frame(&player);
    assert_eq!(
        shown_colour(&player, SIZE as usize, SIZE as usize),
        frame_colour(4),
        "the files play in name order"
    );
}

#[test]
fn encrypted_essence_will_not_load_without_a_key() {
    const SIZE: u32 = 64;
    let directory = tempfile::tempdir().unwrap();
    let frames = flat_codestreams(SIZE, SIZE, 2, CINEMA_2K_PROFILE);
    let mxf = directory.path().join("encrypted.mxf");
    write_mxf(&mxf, &frames, Some([0x2b; 16]), SIZE, SIZE);

    let player = GrokPlayer::new();
    player.init_software().unwrap();
    let error = player
        .load(&mxf, None)
        .expect_err("encrypted essence has no key here");
    assert!(
        error.contains("key"),
        "the refusal has to name the missing key: {error}"
    );
    assert_eq!(player.duration(), None, "nothing loaded");
}

#[test]
fn the_cache_fills_the_lookahead_window_after_a_load() {
    const SIZE: u32 = 32;
    const FRAMES: usize = 96;
    let directory = tempfile::tempdir().unwrap();
    let mxf = flat_mxf(directory.path(), "picture.mxf", SIZE, SIZE, FRAMES);

    let player = loaded_player(&mxf);
    let window = player.lookahead_frames();
    assert!(window > 0, "the pool states no lookahead window");
    let expected = (window + 1).min(FRAMES);
    wait_until("the lookahead window filled", || {
        player.cached_frame_count() >= expected
    });
    assert!(
        player.cached_frame_count() <= expected,
        "the cache ran past its window: {} of {expected}",
        player.cached_frame_count()
    );
}

#[test]
fn accepts_says_which_sources_this_player_takes() {
    const SIZE: u32 = 32;
    let directory = tempfile::tempdir().unwrap();

    let package = directory.path().join("package");
    std::fs::create_dir_all(&package).unwrap();
    let mxf = flat_mxf(&package, "picture.mxf", SIZE, SIZE, 2);
    std::fs::write(
        package.join("ASSETMAP.xml"),
        AssetMap {
            uuid: "bbbbbbbb-0000-0000-0000-000000000000".into(),
            namespace: ns::AM_SMPTE.into(),
            assets: vec![
                AssetMapAsset {
                    id: "cc10cc10-0000-0000-0000-000000000000".into(),
                    path: "CPL_test.xml".into(),
                    ..Default::default()
                },
                AssetMapAsset {
                    id: "11111111-1111-1111-1111-111111111111".into(),
                    path: "picture.mxf".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();
    let cpl = package.join("CPL_test.xml");
    std::fs::write(
        &cpl,
        DcpCpl {
            uuid: "cc10cc10-0000-0000-0000-000000000000".into(),
            namespace: ns::CPL_SMPTE.into(),
            title: "One Reel".into(),
            reels: vec![DcpCplReel {
                reel_id: "aaaaaaaa-0000-0000-0000-000000000000".into(),
                picture_id: "11111111-1111-1111-1111-111111111111".into(),
                picture_edit_rate_num: FRAMES_PER_SECOND,
                picture_edit_rate_den: 1,
                picture_duration: 2,
                picture_width: SIZE,
                picture_height: SIZE,
                ..Default::default()
            }],
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();

    let codestreams = directory.path().join("sequence");
    std::fs::create_dir_all(&codestreams).unwrap();
    std::fs::write(
        codestreams.join("0000.j2c"),
        &flat_codestreams(SIZE, SIZE, 1, CINEMA_2K_PROFILE)[0],
    )
    .unwrap();

    let not_media = directory.path().join("clip.mp4");
    std::fs::write(&not_media, b"not a picture track file").unwrap();
    let empty = directory.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    assert!(GrokPlayer::accepts(&mxf), "a JPEG 2000 MXF");
    assert!(GrokPlayer::accepts(&package), "a package directory");
    assert!(GrokPlayer::accepts(&cpl), "a CPL");
    assert!(GrokPlayer::accepts(&codestreams), "a codestream directory");
    assert!(!GrokPlayer::accepts(&not_media), "an mp4");
    assert!(
        !GrokPlayer::accepts(&empty),
        "a directory with neither an ASSETMAP nor codestreams"
    );
}

fn read_ppm(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("ppm written");
    assert!(bytes.starts_with(b"P6"), "not a binary ppm");
    let mut fields = 0;
    let mut index = 2;
    while fields < 3 && index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let field: u32 = std::str::from_utf8(&bytes[start..index])
            .unwrap()
            .parse()
            .unwrap();
        match fields {
            0 => assert_eq!(field, width, "ppm width"),
            1 => assert_eq!(field, height, "ppm height"),
            _ => assert_eq!(field, 255, "8 bits per channel"),
        }
        fields += 1;
    }
    let pixels = bytes[index + 1..].to_vec();
    assert_eq!(pixels.len(), (width * height * 3) as usize);
    pixels
}

#[cfg(feature = "icc")]
mod display_profile {
    use super::*;
    use lcms2::{CIExyY, CIExyYTRIPLE, Profile, ToneCurve};

    const SIZE: u32 = 64;
    const DCI_PEAK_LUMINANCE: f64 = 52.37;
    const DCI_REFERENCE_WHITE: f64 = 48.0;
    const DCDM_GAMMA: f64 = 2.6;
    const MAXIMUM_CODE: f64 = 4095.0;
    // CIE XYZ of full sRGB red, the first column of the sRGB to XYZ matrix
    const SRGB_RED_XYZ: [f64; 3] = [0.4124564, 0.2126729, 0.0193339];
    // full sRGB red as a Display P3 monitor encodes it
    const SRGB_RED_ON_DISPLAY_P3: [u8; 3] = [234, 51, 35];
    const GREY_CODE: i32 = 2000;
    const SRGB_PARAMETRIC_TYPE: i16 = 4;
    const SRGB_CURVE: [f64; 5] = [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045];
    const ONE_CODE_VALUE: u8 = 1;
    // the 12-bit X'Y'Z' codes round the colour on the way in
    const DISPLAY_P3_TOLERANCE: u8 = 2;

    // SMPTE 428-1: X'Y'Z' codes of a CIE XYZ colour relative to the reference white
    fn dcdm_codes(xyz: [f64; 3]) -> [i32; 3] {
        xyz.map(|value| {
            let peak_relative = value * DCI_REFERENCE_WHITE / DCI_PEAK_LUMINANCE;
            (peak_relative.powf(1.0 / DCDM_GAMMA) * MAXIMUM_CODE).round() as i32
        })
    }

    fn display_p3_profile() -> Profile {
        let chromaticity = |x: f64, y: f64| CIExyY { x, y, Y: 1.0 };
        let curve = ToneCurve::new_parametric(SRGB_PARAMETRIC_TYPE, &SRGB_CURVE).unwrap();
        Profile::new_rgb(
            &chromaticity(0.3127, 0.3290),
            &CIExyYTRIPLE {
                Red: chromaticity(0.680, 0.320),
                Green: chromaticity(0.265, 0.690),
                Blue: chromaticity(0.150, 0.060),
            },
            &[&curve, &curve, &curve],
        )
        .unwrap()
    }

    fn written(profile: &Profile, directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, profile.icc().unwrap()).unwrap();
        path
    }

    fn clip(directory: &Path, frame_codes: &[[i32; 3]]) -> PathBuf {
        let frames = xyz_codestreams(SIZE, SIZE, frame_codes, CINEMA_2K_PROFILE);
        let path = directory.join("picture.mxf");
        write_mxf(&path, &frames, None, SIZE, SIZE);
        path
    }

    fn shown(player: &GrokPlayer) -> [u8; 3] {
        shown_colour(player, SIZE as usize, SIZE as usize)
    }

    fn assert_near(shown: [u8; 3], expected: [u8; 3], tolerance: u8, what: &str) {
        let off = (0..3)
            .map(|channel| shown[channel].abs_diff(expected[channel]))
            .max()
            .unwrap();
        assert!(
            off <= tolerance,
            "{what}: shown {shown:?}, expected {expected:?}"
        );
    }

    fn change_profile(player: &GrokPlayer, profile: Option<&Path>) {
        forget_frames(player);
        player
            .set_display_profile(profile)
            .expect("set the profile");
        wait_for_frame(player);
    }

    #[test]
    fn a_display_profile_recolours_the_paused_frame_without_a_reload() {
        let directory = tempfile::tempdir().unwrap();
        let red = dcdm_codes(SRGB_RED_XYZ);
        let grey = [GREY_CODE; 3];
        let source = clip(directory.path(), &[grey, red]);
        let srgb = written(&Profile::new_srgb(), directory.path(), "srgb.icc");
        let display_p3 = written(&display_p3_profile(), directory.path(), "p3.icc");
        let built_in = XyzToSrgb::new();
        let built_in_of = |[x, y, z]: [i32; 3]| built_in.pixel(x as u16, y as u16, z as u16);

        let player = loaded_player(&source);
        assert_eq!(shown(&player), built_in_of(grey));

        change_profile(&player, Some(&srgb));
        assert_near(
            shown(&player),
            built_in_of(grey),
            ONE_CODE_VALUE,
            "grey through sRGB",
        );
        forget_frames(&player);
        player.frame_step();
        wait_for_frame(&player);
        assert_near(
            shown(&player),
            built_in_of(red),
            ONE_CODE_VALUE,
            "red through sRGB",
        );

        change_profile(&player, Some(&display_p3));
        assert_near(
            shown(&player),
            SRGB_RED_ON_DISPLAY_P3,
            DISPLAY_P3_TOLERANCE,
            "red through Display P3",
        );

        let garbage = directory.path().join("garbage.icc");
        std::fs::write(&garbage, b"not an icc profile").unwrap();
        let error = player
            .set_display_profile(Some(&garbage))
            .expect_err("an unreadable profile is refused");
        assert!(error.contains(&garbage.display().to_string()), "{error}");
        forget_frames(&player);
        player.frame_back_step();
        wait_for_frame(&player);
        forget_frames(&player);
        player.frame_step();
        wait_for_frame(&player);
        assert_near(
            shown(&player),
            SRGB_RED_ON_DISPLAY_P3,
            DISPLAY_P3_TOLERANCE,
            "the refused profile left Display P3 in place",
        );

        change_profile(&player, None);
        assert_eq!(shown(&player), built_in_of(red));
    }

    #[test]
    fn a_display_profile_change_during_playback_drops_no_frames() {
        const FRAMES: usize = 72;
        let directory = tempfile::tempdir().unwrap();
        let red = dcdm_codes(SRGB_RED_XYZ);
        let source = clip(directory.path(), &[red; FRAMES]);
        let display_p3 = written(&display_p3_profile(), directory.path(), "p3.icc");

        let player = loaded_player(&source);
        let window = (player.lookahead_frames() + 1).min(FRAMES);
        wait_until("the decode window filled", || {
            player.cached_frame_count() >= window
        });
        player.set_paused(false);
        wait_until("playback moved off the first frame", || {
            player.position().is_some_and(|position| position > 0.0)
        });
        player
            .set_display_profile(Some(&display_p3))
            .expect("set the profile");
        wait_until("a Display P3 frame reached the screen", || {
            let colour = shown(&player);
            (0..3).all(|channel| {
                colour[channel].abs_diff(SRGB_RED_ON_DISPLAY_P3[channel]) <= DISPLAY_P3_TOLERANCE
            })
        });
        assert!(!player.eof_reached(), "the fixture ended before the change");
        assert_eq!(
            dropped_frames_not_decoded(&player),
            0,
            "the profile change dropped frames that had not decoded in time"
        );
    }
}

mod stereoscopic {
    use super::*;
    use asdcplib::jp2k::{StereoMxfWriter, StereoscopicPhase};
    use postkit::grok_player::{PictureMasks, StereoOutput};

    const SIZE: u32 = 64;
    // the right eye of frame n shows colour n + RIGHT_EYE_COLOUR_OFFSET
    const RIGHT_EYE_COLOUR_OFFSET: usize = 5;

    fn right_eye_colour(frame: usize) -> [u8; 3] {
        frame_colour(frame + RIGHT_EYE_COLOUR_OFFSET)
    }

    fn write_stereo_mxf(path: &Path, left: &[Vec<u8>], right: &[Vec<u8>], width: u32, height: u32) {
        let info = WriterInfo {
            asset_uuid: [9; 16],
            label_set: LabelSet::Smpte,
            ..Default::default()
        };
        let mut writer = StereoMxfWriter::new();
        writer
            .open_write(
                &path.to_string_lossy(),
                &info,
                &descriptor(&left[0], left.len() as u32, width, height),
                16_384,
            )
            .unwrap();
        for (left, right) in left.iter().zip(right) {
            writer
                .write_frame(left, StereoscopicPhase::Left, None, None)
                .unwrap();
            writer
                .write_frame(right, StereoscopicPhase::Right, None, None)
                .unwrap();
        }
        writer.finalize().unwrap();
    }

    // frame n's left eye is colour n and its right eye colour n + 5, both flat
    fn stereo_mxf(directory: &Path, name: &str, width: u32, height: u32, frames: usize) -> PathBuf {
        let left_codes: Vec<[i32; 3]> = (0..frames).map(|frame| [frame_code(frame); 3]).collect();
        let right_codes: Vec<[i32; 3]> = (0..frames)
            .map(|frame| [frame_code(frame + RIGHT_EYE_COLOUR_OFFSET); 3])
            .collect();
        let left = xyz_codestreams(width, height, &left_codes, CINEMA_2K_PROFILE);
        let right = xyz_codestreams(width, height, &right_codes, CINEMA_2K_PROFILE);
        let path = directory.join(name);
        write_stereo_mxf(&path, &left, &right, width, height);
        path
    }

    fn at(player: &GrokPlayer, x: usize, y: usize) -> [u8; 3] {
        pixel(
            &software_frame(player, SIZE as usize, SIZE as usize),
            SIZE as usize,
            x,
            y,
        )
    }

    fn change_output(player: &GrokPlayer, output: StereoOutput) {
        forget_frames(player);
        player.set_stereo_output(output);
        wait_for_frame(player);
    }

    const QUARTER: usize = SIZE as usize / 4;
    const THREE_QUARTERS: usize = 3 * SIZE as usize / 4;

    #[test]
    fn each_stereo_output_puts_each_eye_in_its_place() {
        let directory = tempfile::tempdir().unwrap();
        let mxf = stereo_mxf(directory.path(), "stereo.mxf", SIZE, SIZE, 2);
        assert!(GrokPlayer::accepts(&mxf), "a stereoscopic MXF");

        let player = loaded_player(&mxf);
        assert_eq!(metadata_field(&player, "stereoscopic"), true);
        assert_eq!(player.duration(), Some(2.0 / f64::from(FRAMES_PER_SECOND)));
        assert_eq!(
            [
                at(&player, QUARTER, QUARTER),
                at(&player, THREE_QUARTERS, THREE_QUARTERS)
            ],
            [frame_colour(0); 2],
            "the left eye fills the frame by default"
        );

        change_output(&player, StereoOutput::RightEye);
        assert_eq!(
            [
                at(&player, QUARTER, QUARTER),
                at(&player, THREE_QUARTERS, THREE_QUARTERS)
            ],
            [right_eye_colour(0); 2]
        );

        change_output(&player, StereoOutput::SideBySide);
        assert_eq!(software_size(&player), (SIZE, SIZE));
        assert_eq!(at(&player, QUARTER, QUARTER), frame_colour(0), "left half");
        assert_eq!(
            at(&player, THREE_QUARTERS, QUARTER),
            right_eye_colour(0),
            "right half"
        );

        change_output(&player, StereoOutput::TopAndBottom);
        assert_eq!(
            at(&player, THREE_QUARTERS, QUARTER),
            frame_colour(0),
            "top half"
        );
        assert_eq!(
            at(&player, QUARTER, THREE_QUARTERS),
            right_eye_colour(0),
            "bottom half"
        );

        forget_frames(&player);
        player.frame_step();
        wait_for_frame(&player);
        assert_eq!(at(&player, QUARTER, QUARTER), frame_colour(1));
        assert_eq!(at(&player, QUARTER, THREE_QUARTERS), right_eye_colour(1));
    }

    fn software_size(player: &GrokPlayer) -> (u32, u32) {
        player.frame_size().expect("a frame is on screen")
    }

    // the reel form a 3D DCP's CPL takes, with the msp-cpl prefix
    fn stereoscopic_cpl(cpl_id: &str, picture_id: &str, frames: usize) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <CompositionPlaylist xmlns=\"http://www.smpte-ra.org/schemas/429-7/2006/CPL\">\
             <Id>urn:uuid:{cpl_id}</Id><ContentTitleText>Stereo Test</ContentTitleText>\
             <ReelList><Reel><Id>urn:uuid:aaaaaaaa-0000-0000-0000-000000000001</Id><AssetList>\
             <msp-cpl:MainStereoscopicPicture xmlns:msp-cpl=\"http://www.smpte-ra.org/schemas/429-10/2008/Main-Stereo-Picture-CPL\">\
             <Id>urn:uuid:{picture_id}</Id><EditRate>24 1</EditRate>\
             <IntrinsicDuration>{frames}</IntrinsicDuration><EntryPoint>0</EntryPoint>\
             <Duration>{frames}</Duration><FrameRate>48 1</FrameRate>\
             </msp-cpl:MainStereoscopicPicture></AssetList></Reel></ReelList>\
             </CompositionPlaylist>"
        )
    }

    const STEREOSCOPIC_PACKAGE_FRAMES: usize = 2;

    // a package whose CPL names its picture the way a 3D DCP does, the CPL's path
    fn stereoscopic_package(directory: &Path) -> PathBuf {
        const CPL_ID: &str = "cc10cc10-0000-0000-0000-000000000003";
        const PICTURE_ID: &str = "eee336f9-c2ce-48b0-81e5-a40d78956b9b";
        let picture = format!("picture_{PICTURE_ID}.mxf");
        stereo_mxf(directory, &picture, SIZE, SIZE, STEREOSCOPIC_PACKAGE_FRAMES);
        let cpl = format!("CPL_{CPL_ID}.xml");
        std::fs::write(
            directory.join(&cpl),
            stereoscopic_cpl(CPL_ID, PICTURE_ID, STEREOSCOPIC_PACKAGE_FRAMES),
        )
        .unwrap();
        let assets = [(CPL_ID, cpl.as_str()), (PICTURE_ID, picture.as_str())]
            .into_iter()
            .map(|(id, path)| AssetMapAsset {
                id: id.into(),
                path: path.into(),
                ..Default::default()
            })
            .collect();
        std::fs::write(
            directory.join("ASSETMAP.xml"),
            AssetMap {
                uuid: "bbbbbbbb-0000-0000-0000-000000000003".into(),
                namespace: ns::AM_SMPTE.into(),
                assets,
                ..Default::default()
            }
            .to_xml(),
        )
        .unwrap();
        directory.join(cpl)
    }

    #[test]
    fn a_3d_package_resolves_its_stereoscopic_picture_and_plays_both_eyes() {
        let directory = tempfile::tempdir().unwrap();
        let cpl = stereoscopic_package(directory.path());

        assert!(GrokPlayer::accepts(directory.path()), "a 3D package");
        assert!(GrokPlayer::accepts(&cpl), "a 3D CPL");
        let player = loaded_player(directory.path());
        assert_eq!(metadata_field(&player, "stereoscopic"), true);
        assert_eq!(
            player.duration(),
            Some(STEREOSCOPIC_PACKAGE_FRAMES as f64 / f64::from(FRAMES_PER_SECOND))
        );
        change_output(&player, StereoOutput::SideBySide);
        assert_eq!(at(&player, QUARTER, QUARTER), frame_colour(0), "left half");
        assert_eq!(
            at(&player, THREE_QUARTERS, QUARTER),
            right_eye_colour(0),
            "right half"
        );
    }

    #[test]
    fn the_still_path_shows_the_left_eye_of_a_3d_package() {
        let directory = tempfile::tempdir().unwrap();
        let cpl = stereoscopic_package(directory.path());
        for source in [directory.path().to_path_buf(), cpl] {
            let still = directory.path().join("frame0.ppm");
            postkit::preview::render_dcp_frame(
                &postkit::preview::DcpPreviewOptions {
                    source: source.clone(),
                    ..Default::default()
                },
                0,
                &still,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", source.display()));
            let pixels = read_ppm(&still, SIZE, SIZE);
            let centre = ((SIZE / 2 * SIZE + SIZE / 2) * 3) as usize;
            assert_eq!(
                pixels[centre..centre + 3],
                frame_colour(0),
                "{}",
                source.display()
            );
        }
    }

    #[test]
    fn decoding_both_eyes_halves_the_decode_capacity() {
        const EYE_SIZE: u32 = 512;
        const FRAMES: usize = 40;
        let directory = tempfile::tempdir().unwrap();
        let mxf = stereo_mxf(directory.path(), "stereo.mxf", EYE_SIZE, EYE_SIZE, FRAMES);
        let player = GrokPlayer::new();
        player.init_software().unwrap();
        player.load(&mxf, None).expect("load");
        wait_until("the first frame was composed", || {
            player.frame_size().is_some()
        });
        let (ratio, readings) = median_capacity_ratio(
            &player,
            |player| player.set_stereo_output(StereoOutput::LeftEye),
            |player| player.set_stereo_output(StereoOutput::SideBySide),
        );
        assert!(
            (0.3..0.75).contains(&ratio),
            "left eye against side by side, fps: {readings:?}"
        );
    }

    #[test]
    fn a_stereo_output_change_during_playback_drops_no_frames() {
        const FRAMES: usize = 72;
        let directory = tempfile::tempdir().unwrap();
        let left = xyz_codestreams(SIZE, SIZE, &[[frame_code(0); 3]], CINEMA_2K_PROFILE).remove(0);
        let right = xyz_codestreams(SIZE, SIZE, &[[frame_code(5); 3]], CINEMA_2K_PROFILE).remove(0);
        let mxf = directory.path().join("stereo.mxf");
        write_stereo_mxf(&mxf, &vec![left; FRAMES], &vec![right; FRAMES], SIZE, SIZE);

        let player = loaded_player(&mxf);
        let window = (player.lookahead_frames() + 1).min(FRAMES);
        wait_until("the decode window filled", || {
            player.cached_frame_count() >= window
        });
        player.set_paused(false);
        wait_until("playback moved off the first frame", || {
            player.position().is_some_and(|position| position > 0.0)
        });
        player.set_stereo_output(StereoOutput::SideBySide);
        wait_until("both eyes reached the screen", || {
            at(&player, THREE_QUARTERS, QUARTER) == frame_colour(5)
        });
        assert_eq!(at(&player, QUARTER, QUARTER), frame_colour(0));
        assert!(!player.eof_reached(), "the fixture ended before the change");
        assert_eq!(
            dropped_frames_not_decoded(&player),
            0,
            "the output change dropped frames that had not decoded in time"
        );
    }

    #[test]
    fn a_stereo_source_queued_after_a_mono_one_takes_over_at_its_end() {
        const MONO_FRAMES: usize = 3;
        let directory = tempfile::tempdir().unwrap();
        let mono = flat_mxf(directory.path(), "mono.mxf", SIZE, SIZE, MONO_FRAMES);
        let stereo = stereo_mxf(directory.path(), "stereo.mxf", SIZE, SIZE, 2);
        let player = loaded_player(&mono);
        player.set_stereo_output(StereoOutput::SideBySide);
        player
            .queue_next(&stereo, None)
            .expect("queue the stereoscopic source");

        let mut left_half = vec![at(&player, QUARTER, QUARTER)];
        player.set_paused(false);
        let deadline = Instant::now() + PATIENCE;
        while !player.eof_reached() {
            assert!(Instant::now() < deadline, "playback did not reach the end");
            let colour = at(&player, QUARTER, QUARTER);
            if left_half.last() != Some(&colour) {
                left_half.push(colour);
            }
            std::thread::sleep(POLL);
        }
        // eof is flagged on the tick that presents the last frame
        let last = at(&player, QUARTER, QUARTER);
        if left_half.last() != Some(&last) {
            left_half.push(last);
        }

        // the stereo source's left eyes are colours 0 and 1 again
        let expected = [
            frame_colour(0),
            frame_colour(1),
            frame_colour(2),
            frame_colour(0),
            frame_colour(1),
        ];
        let mut remaining = expected.iter();
        assert!(
            left_half
                .iter()
                .all(|colour| remaining.any(|frame| frame == colour)),
            "the left half did not play in order: {left_half:?}"
        );
        assert_eq!(left_half.last(), expected.last());
        assert_eq!(at(&player, THREE_QUARTERS, QUARTER), right_eye_colour(1));
        assert_eq!(metadata_field(&player, "stereoscopic"), true);
        assert_eq!(dropped_frames_not_decoded(&player), 0);
    }

    #[test]
    fn masks_cover_each_eyes_edges_side_by_side() {
        const MASKED_FRACTION: f32 = 0.25;
        let directory = tempfile::tempdir().unwrap();
        let mxf = stereo_mxf(directory.path(), "stereo.mxf", SIZE, SIZE, 1);
        let player = loaded_player(&mxf);
        change_output(&player, StereoOutput::SideBySide);
        player.set_presentation(PresentationSettings {
            masks: PictureMasks {
                left: MASKED_FRACTION,
                ..PictureMasks::default()
            },
            ..PresentationSettings::default()
        });
        let half = SIZE as usize / 2;
        // a quarter of each eye's 32 columns is masked
        let inside_the_mask = 4;
        let past_the_mask = 12;
        assert_eq!(at(&player, inside_the_mask, QUARTER), [0, 0, 0]);
        assert_eq!(at(&player, past_the_mask, QUARTER), frame_colour(0));
        assert_eq!(at(&player, half + inside_the_mask, QUARTER), [0, 0, 0]);
        assert_eq!(
            at(&player, half + past_the_mask, QUARTER),
            right_eye_colour(0)
        );
    }

    #[test]
    fn a_subtitle_draws_on_both_eyes_side_by_side() {
        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 180;
        let directory = tempfile::tempdir().unwrap();
        let left = xyz_codestreams(WIDTH, HEIGHT, &[[frame_code(0); 3]], CINEMA_2K_PROFILE);
        let mxf = directory.path().join("stereo.mxf");
        write_stereo_mxf(&mxf, &left, &left, WIDTH, HEIGHT);
        let srt = directory.path().join("cues.srt");
        std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nHELLO THERE\n\n").unwrap();
        let (width, height) = (WIDTH as usize, HEIGHT as usize);

        let player = loaded_player(&mxf);
        change_output(&player, StereoOutput::SideBySide);
        let plain = software_frame(&player, width, height);
        forget_frames(&player);
        player
            .set_subtitle_file(SubtitleSlot::Subtitle, Some(&srt))
            .expect("srt loads");
        wait_for_frame(&player);
        let burnt = software_frame(&player, width, height);

        let changed_columns: Vec<usize> = (0..width)
            .filter(|column| {
                (0..height).any(|row| {
                    let at = (row * width + column) * SOFTWARE_BYTES_PER_PIXEL;
                    plain[at..at + 3] != burnt[at..at + 3]
                })
            })
            .collect();
        let half = width / 2;
        let in_left = changed_columns
            .iter()
            .filter(|column| **column < half)
            .count();
        let in_right = changed_columns.len() - in_left;
        assert!(
            in_left > 0 && in_right > 0,
            "the cue is not on both eyes: {in_left} and {in_right} columns"
        );
        assert!(
            in_left.abs_diff(in_right) <= 2,
            "the eyes carry different cues: {in_left} and {in_right} columns"
        );
        assert!(
            !changed_rows(&plain, &burnt, width, height)
                .iter()
                .any(|row| *row < height / 2),
            "the cue drew outside the bottom band"
        );
    }
}
