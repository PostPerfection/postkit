use crate::pipeline::CANCELLED;
use crate::preview::{MAX_FRAME_BYTES, PictureReader, PreviewError};
use asdcplib::EssenceType;
use asdcplib::jp2k::PictureDescriptor;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureMxfInfo {
    pub frames: u64,
    pub edit_rate_num: u32,
    pub edit_rate_den: u32,
    pub width: u32,
    pub height: u32,
}

enum PictureMxfWrapping {
    AsDcp,
    As02,
    Stereoscopic,
}

fn picture_mxf_wrapping(mxf: &Path) -> Option<PictureMxfWrapping> {
    match asdcplib::essence_type(&mxf.to_string_lossy()).ok()? {
        EssenceType::Jpeg2000 => Some(PictureMxfWrapping::AsDcp),
        EssenceType::As02Jpeg2000 => Some(PictureMxfWrapping::As02),
        EssenceType::Jpeg2000Stereo => Some(PictureMxfWrapping::Stereoscopic),
        _ => None,
    }
}

pub(crate) fn is_picture_mxf(path: &Path) -> bool {
    picture_mxf_wrapping(path).is_some()
}

fn open_importable(mxf: &Path) -> Result<(PictureReader, PictureDescriptor), String> {
    let as02 = match picture_mxf_wrapping(mxf) {
        Some(PictureMxfWrapping::AsDcp) => false,
        Some(PictureMxfWrapping::As02) => true,
        Some(PictureMxfWrapping::Stereoscopic) => {
            return Err(format!(
                "{} is a stereoscopic picture MXF, which cannot be imported",
                mxf.display()
            ));
        }
        None => {
            return Err(format!("{} is not a JPEG 2000 picture MXF", mxf.display()));
        }
    };
    let mxf_error = |error: PreviewError| format!("{}: {error}", mxf.display());
    let mut reader = PictureReader::open(mxf, as02).map_err(mxf_error)?;
    if reader.writer_info().map_err(mxf_error)?.encrypted_essence {
        return Err(format!(
            "{} is encrypted, a KDM-protected picture MXF cannot be imported",
            mxf.display()
        ));
    }
    let descriptor = reader.picture_descriptor().map_err(mxf_error)?;
    Ok((reader, descriptor))
}

fn picture_mxf_info(mxf: &Path, descriptor: &PictureDescriptor) -> Result<PictureMxfInfo, String> {
    let rate = descriptor.edit_rate;
    let unsigned_rate = |value: i32| {
        u32::try_from(value).map_err(|_| {
            format!(
                "{} declares the edit rate {}/{}",
                mxf.display(),
                rate.numerator,
                rate.denominator
            )
        })
    };
    Ok(PictureMxfInfo {
        frames: u64::from(descriptor.container_duration),
        edit_rate_num: unsigned_rate(rate.numerator)?,
        edit_rate_den: unsigned_rate(rate.denominator)?,
        width: descriptor.stored_width,
        height: descriptor.stored_height,
    })
}

pub fn probe_picture_mxf(mxf: &Path) -> Result<PictureMxfInfo, String> {
    let (mut reader, descriptor) = open_importable(mxf)?;
    reader.close();
    picture_mxf_info(mxf, &descriptor)
}

pub fn unwrap_picture_mxf(
    mxf: &Path,
    out_dir: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<PictureMxfInfo, String> {
    let (mut reader, descriptor) = open_importable(mxf)?;
    let info = picture_mxf_info(mxf, &descriptor)?;
    std::fs::create_dir_all(out_dir)
        .map_err(|error| format!("cannot create {}: {error}", out_dir.display()))?;
    let mut codestream = vec![0u8; MAX_FRAME_BYTES];
    for frame in 0..descriptor.container_duration {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let size = reader
            .read_frame(frame, &mut codestream, None, None)
            .map_err(|error| format!("cannot read frame {frame} of {}: {error}", mxf.display()))?;
        let path = out_dir.join(format!("frame_{frame:08}.j2c"));
        std::fs::write(&path, &codestream[..size])
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        on_progress(u64::from(frame) + 1, info.frames);
    }
    reader.close();
    Ok(info)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mxf_wrap::{
        EssenceType as WrapEssenceType, MxfEncryption, MxfStandard, MxfWrapOptions,
        StereoscopicWrapOptions, mxf_wrap, wrap_stereoscopic,
    };
    use std::path::PathBuf;

    pub(crate) const FRAME_COUNT: usize = 4;
    const EDIT_RATE_NUM: u32 = 24;
    const EDIT_RATE_DEN: u32 = 1;
    const FIXTURE_SIZE: u32 = 64;

    fn cinema_codestream() -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinema2k_64x64.j2c"),
        )
        .unwrap()
    }

    pub(crate) fn write_frames(dir: &Path, name: &str) -> (Vec<PathBuf>, Vec<Vec<u8>>) {
        let frames: Vec<Vec<u8>> = (0..FRAME_COUNT)
            .map(|index| {
                let mut frame = cinema_codestream();
                frame.extend_from_slice(format!("{name}{index:04}").as_bytes());
                frame
            })
            .collect();
        let paths = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let path = dir.join(format!("{name}_{index}.j2c"));
                std::fs::write(&path, frame).unwrap();
                path
            })
            .collect();
        (paths, frames)
    }

    pub(crate) fn wrap(
        input_files: Vec<PathBuf>,
        output: PathBuf,
        encryption: Option<MxfEncryption>,
    ) -> PathBuf {
        let track = mxf_wrap(&MxfWrapOptions {
            input_files,
            output: output.clone(),
            essence_type: WrapEssenceType::J2k,
            standard: MxfStandard::AsDcp,
            fps_num: EDIT_RATE_NUM,
            fps_den: EDIT_RATE_DEN,
            partition_size: 0,
            encryption,
            mca_config: None,
            resource_ids: Vec::new(),
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        });
        assert!(track.success, "wrap failed: {}", track.error);
        output
    }

    pub(crate) fn wrapped_picture_mxf(dir: &Path) -> (PathBuf, Vec<Vec<u8>>) {
        let (paths, frames) = write_frames(dir, "plain");
        (wrap(paths, dir.join("picture.mxf"), None), frames)
    }

    fn unwrap_into(mxf: &Path, out_dir: &Path) -> (Result<PictureMxfInfo, String>, Vec<u64>) {
        let mut reported = Vec::new();
        let result =
            unwrap_picture_mxf(mxf, out_dir, &AtomicBool::new(false), &mut |done, total| {
                assert_eq!(total, FRAME_COUNT as u64);
                reported.push(done);
            });
        (result, reported)
    }

    #[test]
    fn a_picture_mxf_unwraps_to_the_codestreams_it_was_wrapped_from() {
        let dir = tempfile::tempdir().unwrap();
        let (mxf, frames) = wrapped_picture_mxf(dir.path());
        let expected = PictureMxfInfo {
            frames: FRAME_COUNT as u64,
            edit_rate_num: EDIT_RATE_NUM,
            edit_rate_den: EDIT_RATE_DEN,
            width: FIXTURE_SIZE,
            height: FIXTURE_SIZE,
        };
        assert_eq!(probe_picture_mxf(&mxf).unwrap(), expected);

        let out_dir = dir.path().join("j2k");
        let (result, reported) = unwrap_into(&mxf, &out_dir);
        assert_eq!(result.unwrap(), expected);
        assert_eq!(reported, (1..=FRAME_COUNT as u64).collect::<Vec<_>>());
        for (index, frame) in frames.iter().enumerate() {
            let written = std::fs::read(out_dir.join(format!("frame_{index:08}.j2c"))).unwrap();
            assert!(written == *frame, "frame {index} differs from its source");
        }
        assert!(!out_dir.join(format!("frame_{FRAME_COUNT:08}.j2c")).exists());
    }

    #[test]
    fn an_encrypted_picture_mxf_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, _) = write_frames(dir.path(), "secret");
        let encryption = MxfEncryption {
            content_key: [0x11; 16],
            key_id: [0x22; 16],
        };
        let mxf = wrap(paths, dir.path().join("encrypted.mxf"), Some(encryption));
        let expected = format!(
            "{} is encrypted, a KDM-protected picture MXF cannot be imported",
            mxf.display()
        );

        assert_eq!(probe_picture_mxf(&mxf).unwrap_err(), expected);
        let (result, reported) = unwrap_into(&mxf, &dir.path().join("j2k"));
        assert_eq!(result.unwrap_err(), expected);
        assert!(reported.is_empty());
    }

    #[test]
    fn a_stereoscopic_picture_mxf_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (left_files, _) = write_frames(dir.path(), "left");
        let (right_files, _) = write_frames(dir.path(), "right");
        let mxf = dir.path().join("stereoscopic.mxf");
        let track = wrap_stereoscopic(&StereoscopicWrapOptions {
            left_files,
            right_files,
            output: mxf.clone(),
            fps_num: EDIT_RATE_NUM,
            fps_den: EDIT_RATE_DEN,
            encryption: None,
            asset_uuid: None,
        });
        assert!(track.success, "stereoscopic wrap failed: {}", track.error);
        let expected = format!(
            "{} is a stereoscopic picture MXF, which cannot be imported",
            mxf.display()
        );

        assert_eq!(probe_picture_mxf(&mxf).unwrap_err(), expected);
        let (result, _) = unwrap_into(&mxf, &dir.path().join("j2k"));
        assert_eq!(result.unwrap_err(), expected);
    }

    #[test]
    fn a_cancelled_unwrap_stops_before_writing_every_frame() {
        let dir = tempfile::tempdir().unwrap();
        let (mxf, _) = wrapped_picture_mxf(dir.path());
        let out_dir = dir.path().join("j2k");

        let result = unwrap_picture_mxf(&mxf, &out_dir, &AtomicBool::new(true), &mut |_, _| {});

        assert_eq!(result.unwrap_err(), CANCELLED);
        assert!(
            crate::grok_encoder::contiguous_encoded_frames(&out_dir) < FRAME_COUNT as u64,
            "a cancelled unwrap wrote every frame"
        );
    }
}
