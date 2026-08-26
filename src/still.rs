//! A single image held for a duration, as a directory of J2K frames.
//!
//! The image is encoded once and the codestream linked for every frame of the
//! hold: a frame-wrapped picture MXF may repeat one codestream, so a two-minute
//! title card costs one encode instead of two thousand. A burnt-in subtitle
//! breaks the repeat only where the cues change, so the hold still costs a
//! handful of encodes.

use std::path::Path;
use std::sync::Arc;

use crate::encode::FrameRate;

/// Where the held codestreams are written, under the job's output directory.
pub const HELD_PICTURE_DIR: &str = "j2k_still";

/// Image extensions accepted as a still. A file with one of these is a still
/// input and needs a hold duration; anything else is a video or a codestream
/// directory. ffmpeg decodes the still, so this is wider than the formats
/// grok's own image loader reads.
pub const STILL_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "tif", "tiff", "bmp", "dpx", "exr"];

/// Is this a single image file the encoder can read, rather than a video or a
/// directory of frames?
pub fn is_still_image(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| STILL_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
}

/// The filter chain the still decodes through: the picture plan's filters plus
/// the hold's rate, so 24000/1001 reaches ffmpeg as itself rather than as 24.
fn decode_filters(filters: &[String], fps: FrameRate) -> String {
    let mut chain = filters.to_vec();
    chain.push(format!("fps={}", fps.ffmpeg_filter_value()));
    chain.join(",")
}

/// Decode `image` to one rgb48be frame at `width`x`height`, sized by the caller
/// from the picture plan so a mismatch is refused before this runs. The plan's
/// filters run here, which is where a still meets the crop, turn and raster fit
/// a video meets inside the encode pipeline.
fn decode_rgb48(
    image: &Path,
    width: u32,
    height: u32,
    filters: &[String],
    fps: FrameRate,
) -> Result<Vec<u8>, String> {
    let mut command = std::process::Command::new("ffmpeg");
    command.arg("-y").arg("-i").arg(image);
    command.arg("-vf").arg(decode_filters(filters, fps));
    let output = command
        .args(["-frames:v", "1", "-pix_fmt", "rgb48be", "-f", "rawvideo"])
        .arg("pipe:1")
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run ffmpeg to read {}: {e}", image.display()))?;
    if !output.status.success() {
        return Err(format!("ffmpeg could not decode {}", image.display()));
    }
    let want = (width as usize) * (height as usize) * 6;
    if output.stdout.len() != want {
        return Err(format!(
            "{} decoded to {} bytes, not the {want} a {width}x{height} frame needs",
            image.display(),
            output.stdout.len()
        ));
    }
    Ok(output.stdout)
}

/// A still image and how long to hold it.
pub struct StillHold<'a> {
    pub image: &'a Path,
    pub frames: u64,
    pub fps: FrameRate,
    /// Size of the encoded frame, which is the picture plan's output raster.
    pub width: u32,
    pub height: u32,
    /// ffmpeg filters the picture plan resolved to, applied while the image
    /// decodes.
    pub filters: &'a [String],
    /// The compressor's own Rec.709 to X'Y'Z' pass. Off when the image is
    /// already X'Y'Z', or when `colour_transform` converts it instead.
    pub apply_xyz_transform: bool,
    /// Per-frame matrix for a source space the compressor's own transform does
    /// not model.
    pub colour_transform: Option<Arc<crate::colour::DcdmTransform>>,
    /// Subtitles burnt into the held frames.
    pub burn: Option<Arc<crate::subtitle_raster::SubtitleBurn>>,
    pub out_dir: &'a Path,
}

/// Encode the still and fill `out_dir` with one codestream per frame of the
/// hold. `out_dir` is recreated empty: frames from an earlier longer run would
/// otherwise ship as extra frames, since the MXF wrapper takes the whole
/// directory listing.
///
/// Without a burn that is one encode hard-linked for the whole hold. With one,
/// the picture only changes where the cue set does, so the hold is cut into
/// runs of frames sharing the same cues and each run costs one encode.
pub fn build_still_frames(hold: &StillHold) -> Result<(), String> {
    use crate::grok_encoder::{self, CompressParams, RawFrame, SourcePreparation};
    use std::sync::atomic::AtomicBool;

    let StillHold {
        image,
        frames,
        fps,
        width,
        height,
        filters,
        apply_xyz_transform,
        colour_transform,
        burn,
        out_dir,
    } = hold;
    let (frames, fps, width, height) = (*frames, *fps, *width, *height);

    if frames == 0 {
        return Err("a still needs a hold of at least one frame".into());
    }
    let data = decode_rgb48(image, width, height, filters, fps)?;
    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)
            .map_err(|e| format!("cannot clear {}: {e}", out_dir.display()))?;
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;

    let params = CompressParams {
        edit_rate: fps,
        apply_xyz_transform: *apply_xyz_transform,
        source_preparation: SourcePreparation {
            subtitle_burn: burn.clone(),
            colour_transform: colour_transform.clone(),
        },
        ..CompressParams::default()
    };
    let encoded = distinct_frames(burn.as_deref(), frames);
    let cancel = Arc::new(AtomicBool::new(false));
    grok_encoder::initialize(0);
    let mut next = encoded.iter().copied();
    let result = grok_encoder::encode_pipeline(
        out_dir,
        &params,
        encoded.len() as u64,
        &cancel,
        &Arc::new(grok_encoder::PhaseClocks::default()),
        || {
            let index = next.next()?;
            Some(RawFrame::Packed {
                data: data.clone(),
                width,
                height,
                precision: 16,
                index,
            })
        },
        |_p| {},
    );
    if !result.success {
        return Err(format!("still frame encode failed: {}", result.error));
    }

    // Every frame between one encoded index and the next repeats it.
    for (at, &start) in encoded.iter().enumerate() {
        let end = encoded.get(at + 1).copied().unwrap_or(frames);
        let source = out_dir.join(format!("frame_{start:08}.j2c"));
        for index in start + 1..end {
            let target = out_dir.join(format!("frame_{index:08}.j2c"));
            let _ = std::fs::remove_file(&target);
            if std::fs::hard_link(&source, &target).is_err() {
                std::fs::copy(&source, &target)
                    .map_err(|e| format!("cannot place {}: {e}", target.display()))?;
            }
        }
    }
    Ok(())
}

/// Frame indices that have to be encoded: the first frame, plus every frame
/// where the burnt-in cues change. Without a burn every frame is identical, so
/// that is frame 0 alone.
fn distinct_frames(burn: Option<&crate::subtitle_raster::SubtitleBurn>, frames: u64) -> Vec<u64> {
    let Some(burn) = burn else {
        return vec![0];
    };
    let mut encoded = vec![0u64];
    let mut current = burn.active_cues(0);
    for index in 1..frames {
        let active = burn.active_cues(index);
        if active != current {
            encoded.push(index);
            current = active;
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_files_are_stills_and_videos_are_not() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["card.png", "card.TIF", "card.dpx", "frame.exr", "card.JPG"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            assert!(is_still_image(&path), "{name} must read as a still");
        }
        for name in ["movie.mov", "movie.mp4", "frame.j2c"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            assert!(!is_still_image(&path), "{name} must not read as a still");
        }
        assert!(!is_still_image(dir.path()), "a directory is not a still");
    }

    #[test]
    fn a_hold_with_no_burn_encodes_one_frame_and_a_burn_encodes_one_per_cue_change() {
        use crate::subtitle_formats::{StyledCue, StyledRun};
        use crate::subtitle_raster::{BurnStyle, SubtitleBurn};

        assert_eq!(distinct_frames(None, 100), vec![0]);

        // one cue over frames 24..48 at 24 fps: the picture changes when it
        // arrives and again when it leaves
        let cue = StyledCue::text(1000, 2000, vec![StyledRun::plain("hello")]);
        let Ok(burn) = SubtitleBurn::new(vec![cue], None, BurnStyle::default(), 24.0) else {
            eprintln!("skipping: no font available to build a burn");
            return;
        };
        assert_eq!(distinct_frames(Some(&burn), 100), vec![0, 24, 48]);
    }

    #[test]
    fn a_hold_decodes_at_the_rate_it_was_given_rather_than_a_whole_one() {
        assert_eq!(
            decode_filters(&[], FrameRate::new(24000, 1001)),
            "fps=24000/1001"
        );
        assert_eq!(
            decode_filters(&["crop=1920:804:0:138".into()], FrameRate::whole(25)),
            "crop=1920:804:0:138,fps=25"
        );
    }

    #[test]
    fn a_zero_length_hold_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("card.png");
        std::fs::write(&image, "x").unwrap();
        let err = build_still_frames(&StillHold {
            image: &image,
            frames: 0,
            fps: FrameRate::whole(24),
            width: 2048,
            height: 1080,
            filters: &[],
            apply_xyz_transform: true,
            colour_transform: None,
            burn: None,
            out_dir: &dir.path().join("held"),
        })
        .unwrap_err();
        assert!(err.contains("at least one frame"), "got: {err}");
    }
}
