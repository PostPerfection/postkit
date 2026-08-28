//! The ASS overlay the render player installs, drawn by a real libmpv.
//!
//! A black clip is rendered through the software renderer, so the only pixels
//! that can be light are the overlay's own.
#![cfg(all(target_os = "linux", feature = "libmpv"))]

use std::path::Path;
use std::time::{Duration, Instant};

use postkit::mpv_render::{MpvRenderPlayer, OsdAssOverlay};

/// The surface the frames are rendered onto, deliberately squarer than the
/// clip's 16:9, so the picture sits between bars and a drawing that ignored them
/// would be caught.
const TARGET_WIDTH: usize = 400;
const TARGET_HEIGHT: usize = 400;
const BYTES_PER_PIXEL: usize = 4;

const CLIP_WIDTH: u32 = 640;
const CLIP_HEIGHT: u32 = 360;
const CLIP_SECONDS: u32 = 2;

/// The overlay's own canvas, which mpv stretches over the whole surface.
const PLAY_RES: u32 = 100;
/// Where the light pixels of a top-half drawing must fall on that surface.
const HALF_OF_THE_SURFACE: usize = TARGET_HEIGHT / 2;

const OVERLAY_ID: i64 = 7;
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How light a pixel counts as drawn on, out of 255.
const DRAWN_ON: u8 = 200;

fn write_black_clip(path: &Path) {
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "color=c=black:size={CLIP_WIDTH}x{CLIP_HEIGHT}:rate=24:duration={CLIP_SECONDS}"
            ),
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

struct Surface {
    player: MpvRenderPlayer,
    pixels: Vec<u8>,
}

impl Surface {
    fn playing(clip: &Path) -> Self {
        let player = MpvRenderPlayer::new().expect("create mpv");
        player.init_software().expect("software render context");
        player
            .load_file(clip.to_str().unwrap())
            .expect("load the clip");
        Surface {
            player,
            pixels: vec![0u8; TARGET_WIDTH * TARGET_HEIGHT * BYTES_PER_PIXEL],
        }
    }

    /// Render frames until mpv has drawn what was asked for. A property or
    /// overlay change reaches the surface a frame or two later, so this takes
    /// several rather than one.
    fn render_frames(&mut self, count: usize) {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut drawn = 0;
        while Instant::now() < deadline && drawn < count {
            if self.player.wants_redraw() {
                self.player
                    .render_software(TARGET_WIDTH, TARGET_HEIGHT, &mut self.pixels)
                    .expect("render a frame");
                drawn += 1;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        assert_eq!(drawn, count, "libmpv offered fewer frames than {count}");
    }

    fn drawn_on(&self, x: usize, y: usize) -> bool {
        let at = (y * TARGET_WIDTH + x) * BYTES_PER_PIXEL;
        self.pixels[at..at + 3]
            .iter()
            .all(|value| *value > DRAWN_ON)
    }

    fn rows_drawn_on(&self) -> Vec<usize> {
        (0..TARGET_HEIGHT)
            .filter(|row| (0..TARGET_WIDTH).any(|column| self.drawn_on(column, *row)))
            .collect()
    }
}

/// A filled rectangle in the overlay's own coordinates, white and solid.
fn filled_rectangle(width: u32, height: u32) -> String {
    format!(
        "{{\\an7\\pos(0,0)\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&\\p1}}\
         m 0 0 l {width} 0 l {width} {height} l 0 {height}{{\\p0}}"
    )
}

fn overlay(events: &str) -> OsdAssOverlay<'_> {
    OsdAssOverlay {
        events,
        play_res_x: PLAY_RES,
        play_res_y: PLAY_RES,
    }
}

#[test]
fn an_ass_overlay_is_drawn_and_taken_away_again() {
    let directory = tempfile::tempdir().unwrap();
    let clip = directory.path().join("black.mp4");
    write_black_clip(&clip);
    let mut surface = Surface::playing(&clip);

    surface.render_frames(3);
    assert!(
        surface.rows_drawn_on().is_empty(),
        "the black clip rendered light pixels before any overlay went on"
    );

    // half the canvas, which lands on half the surface because mpv stretches the
    // canvas over all of it
    let top_half = filled_rectangle(PLAY_RES, PLAY_RES / 2);
    surface
        .player
        .set_osd_overlay(OVERLAY_ID, Some(overlay(&top_half)))
        .expect("install the overlay");
    surface.render_frames(3);
    let rows = surface.rows_drawn_on();
    assert_eq!(
        (rows.first().copied(), rows.last().copied()),
        (Some(0), Some(HALF_OF_THE_SURFACE - 1)),
        "the top half of the canvas did not land on the top half of the surface"
    );

    // the same id again, with a whole canvas this time
    surface
        .player
        .set_osd_overlay(
            OVERLAY_ID,
            Some(overlay(&filled_rectangle(PLAY_RES, PLAY_RES))),
        )
        .expect("replace the overlay");
    surface.render_frames(3);
    assert_eq!(
        surface.rows_drawn_on().len(),
        TARGET_HEIGHT,
        "replacing the overlay left rows undrawn"
    );

    surface
        .player
        .set_osd_overlay(OVERLAY_ID, None)
        .expect("remove the overlay");
    surface.render_frames(3);
    assert!(
        surface.rows_drawn_on().is_empty(),
        "the overlay was still drawn after it was removed"
    );
}

/// Every line of the events is its own dialogue event, which is what lets one
/// overlay hold drawings of different colours.
#[test]
fn each_line_of_the_events_is_drawn() {
    let directory = tempfile::tempdir().unwrap();
    let clip = directory.path().join("black.mp4");
    write_black_clip(&clip);
    let mut surface = Surface::playing(&clip);

    let two_lines = format!(
        "{}\n{}",
        filled_rectangle(PLAY_RES, PLAY_RES / 4),
        // the second line's own origin, so the two cannot be the same rectangle
        filled_rectangle(PLAY_RES, PLAY_RES).replace("\\pos(0,0)", "\\pos(0,75)")
    );
    surface
        .player
        .set_osd_overlay(OVERLAY_ID, Some(overlay(&two_lines)))
        .expect("install both lines");
    surface.render_frames(3);

    let rows = surface.rows_drawn_on();
    assert!(
        rows.contains(&0) && rows.contains(&(TARGET_HEIGHT - 1)),
        "one of the two events was not drawn, rows drawn on: {rows:?}"
    );
    assert!(
        !rows.contains(&(TARGET_HEIGHT / 2)),
        "the gap between the two events was drawn on, rows drawn on: {rows:?}"
    );
}
