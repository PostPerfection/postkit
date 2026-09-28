use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::encode::{FrameRate, InputType, detect_input_type, find_source_frames};

const OUTPUT_FRAME_STEM: &str = "frame_%06d";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSequence {
    pub directory: PathBuf,
    pub pattern: PathBuf,
    pub start_number: u64,
    pub extension: String,
    pub frame_rate: FrameRate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfmpegInput {
    File(PathBuf),
    ImageSequence(ImageSequence),
}

impl FfmpegInput {
    // a frame directory plays at `sequence_rate`, since stills carry no rate of their own
    pub fn resolve(path: &Path, sequence_rate: FrameRate) -> Result<Self, String> {
        if !path.is_dir() {
            return Ok(Self::File(path.to_path_buf()));
        }
        if detect_input_type(path) != InputType::ImageSequence {
            return Err(format!("{} holds no image frames", path.display()));
        }
        ImageSequence::from_directory(path, sequence_rate).map(Self::ImageSequence)
    }

    pub fn arguments(&self) -> Vec<OsString> {
        match self {
            Self::File(path) => vec!["-i".into(), path.into()],
            Self::ImageSequence(sequence) => vec![
                "-f".into(),
                "image2".into(),
                "-framerate".into(),
                sequence.frame_rate.ffmpeg_filter_value().into(),
                "-start_number".into(),
                sequence.start_number.to_string().into(),
                "-i".into(),
                (&sequence.pattern).into(),
            ],
        }
    }

    pub fn image_extension(&self) -> Option<&str> {
        match self {
            Self::File(_) => None,
            Self::ImageSequence(sequence) => Some(&sequence.extension),
        }
    }
}

struct NumberedFrame<'name> {
    prefix: &'name str,
    digits: &'name str,
    extension: &'name str,
}

fn numbered_frame(frame: &Path) -> Result<NumberedFrame<'_>, String> {
    let unnumbered = || format!("{} carries no frame number", frame.display());
    let name = frame
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} is not a UTF-8 file name", frame.display()))?;
    let (stem, extension) = name.rsplit_once('.').ok_or_else(unnumbered)?;
    let prefix = stem.trim_end_matches(|character: char| character.is_ascii_digit());
    let digits = &stem[prefix.len()..];
    if digits.is_empty() {
        return Err(unnumbered());
    }
    Ok(NumberedFrame {
        prefix,
        digits,
        extension,
    })
}

impl ImageSequence {
    pub fn from_directory(directory: &Path, frame_rate: FrameRate) -> Result<Self, String> {
        let frames = find_source_frames(directory)
            .map_err(|e| format!("cannot list {}: {e}", directory.display()))?;
        let first_frame = frames
            .first()
            .ok_or_else(|| format!("no images in {}", directory.display()))?;
        let first = numbered_frame(first_frame)?;
        let start_number: u64 = first
            .digits
            .parse()
            .map_err(|e| format!("{}: {e}", first_frame.display()))?;
        for (offset, frame) in (0u64..).zip(&frames) {
            let numbered = numbered_frame(frame)?;
            let expected = start_number + offset;
            let in_step = numbered.prefix == first.prefix
                && numbered.extension == first.extension
                && numbered.digits.len() == first.digits.len()
                && numbered.digits.parse::<u64>() == Ok(expected);
            if !in_step {
                return Err(format!(
                    "{} is not frame {expected} of the sequence {} starts, ffmpeg reads one \
                     name pattern numbered without gaps",
                    frame.display(),
                    first_frame.display()
                ));
            }
        }
        let pattern = format!(
            "{}%0{}d.{}",
            first.prefix.replace('%', "%%"),
            first.digits.len(),
            first.extension
        );
        Ok(Self {
            directory: directory.to_path_buf(),
            pattern: directory.join(pattern),
            start_number,
            extension: first.extension.to_string(),
            frame_rate,
        })
    }
}

fn names_a_directory(output: &Path) -> bool {
    let ends_with_separator = output
        .to_string_lossy()
        .chars()
        .last()
        .is_some_and(std::path::is_separator);
    output.is_dir() || ends_with_separator
}

pub fn frame_output(output: &Path, extension: Option<&str>) -> Result<PathBuf, String> {
    if !names_a_directory(output) {
        return Ok(output.to_path_buf());
    }
    let extension = extension.ok_or_else(|| {
        format!(
            "{} is a directory, and the input is not an image sequence to take the frame \
             format from: name an output file or an image pattern such as frame_%06d.tif",
            output.display()
        )
    })?;
    std::fs::create_dir_all(output)
        .map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    Ok(output.join(format!("{OUTPUT_FRAME_STEM}.{extension}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_FRAMES_PER_SECOND: u32 = 24;

    fn touch(directory: &Path, names: &[&str]) {
        for name in names {
            std::fs::write(directory.join(name), b"").unwrap();
        }
    }

    #[test]
    fn a_frame_directory_becomes_its_pattern_from_the_first_number() {
        let directory = tempfile::tempdir().unwrap();
        touch(
            directory.path(),
            &["reel1_086400.dpx", "reel1_086401.dpx", "reel1_086402.dpx"],
        );

        let sequence = ImageSequence::from_directory(
            directory.path(),
            FrameRate::whole(TEST_FRAMES_PER_SECOND),
        )
        .unwrap();

        assert_eq!(sequence.pattern, directory.path().join("reel1_%06d.dpx"));
        assert_eq!(sequence.start_number, 86400);
        assert_eq!(sequence.extension, "dpx");
    }

    #[test]
    fn a_gap_in_the_numbers_is_refused_naming_the_frame() {
        let directory = tempfile::tempdir().unwrap();
        touch(
            directory.path(),
            &["f_0001.png", "f_0002.png", "f_0004.png"],
        );

        let refused = ImageSequence::from_directory(
            directory.path(),
            FrameRate::whole(TEST_FRAMES_PER_SECOND),
        )
        .unwrap_err();

        assert!(refused.contains("f_0004.png"), "{refused}");
        assert!(refused.contains("frame 3"), "{refused}");
    }
}
