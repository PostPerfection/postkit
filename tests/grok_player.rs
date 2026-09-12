#![cfg(feature = "grok-ffi")]

use asdcplib::crypto::{AesEncContext, HmacContext};
use asdcplib::jp2k::{CodestreamHeader, MxfWriter, PictureDescriptor};
use asdcplib::{LabelSet, Rational, WriterInfo};
use postkit::colour::XyzToSrgb;
use postkit::composition_timeline;
use postkit::grok_player::{DecodeScale, GrokPlayer, OverlayRectangle, SubtitleSlot};
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
        || {
            if next >= count {
                return None;
            }
            let code = frame_code(next);
            let frame = postkit::grok_encoder::RawFrame::Planar {
                components: [
                    vec![code; samples],
                    vec![code; samples],
                    vec![code; samples],
                ],
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

fn dropped_frames(player: &GrokPlayer) -> u64 {
    const KEY: &str = r#""dropped_frames": "#;
    let metadata = player.metadata_json();
    let at = metadata.find(KEY).expect("dropped_frames in the metadata") + KEY.len();
    let rest = &metadata[at..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("a dropped frame count")
}

// the smallest the cache gets while it waits, sampled as fast as the poll can read it
fn lowest_cached_until(player: &GrokPlayer, what: &str, mut ready: impl FnMut() -> bool) -> usize {
    let deadline = Instant::now() + PATIENCE;
    let mut lowest = player.cached_frame_count();
    while !ready() {
        lowest = lowest.min(player.cached_frame_count());
        assert!(Instant::now() < deadline, "{what} did not happen");
    }
    lowest
}

fn loaded_player(source: &Path) -> GrokPlayer {
    let player = GrokPlayer::new();
    player.init_software().unwrap();
    player.load(source).expect("load");
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
        dropped_frames(&player),
        0,
        "the scale change dropped frames"
    );

    // six frame periods, so a picture held back by the refill shows up as a stop
    const KEEPS_MOVING: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + KEEPS_MOVING;
    while player.position() == Some(before) {
        assert!(
            Instant::now() < deadline,
            "the picture stopped at {before:?} over the scale change"
        );
    }
    assert_eq!(
        dropped_frames(&player),
        0,
        "frames were dropped after the scale change"
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
        .load(&mxf)
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
