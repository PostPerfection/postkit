use std::path::Path;

use asdcplib::crypto::{AesDecContext, HmacContext};

use crate::content_keys::ContentKeys;

// Kumu's RESULT_SMALLBUF
const SMALL_BUFFER_RESULT: i32 = -7;

// ST 429-18:2023 7.2.4 Table 1, the largest frame in bytes for each edit rate in frames a second
const LARGEST_FRAME_BYTES: [(i32, usize); 9] = [
    (24, 781_250),
    (25, 750_000),
    (30, 625_000),
    (48, 390_625),
    (50, 375_000),
    (60, 312_500),
    (96, 195_313),
    (100, 187_500),
    (120, 156_250),
];

fn largest_frame_bytes(edit_rate: asdcplib::Rational) -> Result<usize, String> {
    LARGEST_FRAME_BYTES
        .iter()
        .find(|(frames_per_second, _)| {
            edit_rate.denominator == 1 && edit_rate.numerator == *frames_per_second
        })
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| {
            format!(
                "ST 429-18 Table 1 sets no frame size for the edit rate {}/{}",
                edit_rate.numerator, edit_rate.denominator
            )
        })
}

fn check_imf_track_is_clear(info: &asdcplib::WriterInfo) -> Result<(), String> {
    if info.encrypted_essence {
        return Err(
            "the IMF IAB track is encrypted and asdcplib reads AS-02 IAB in the clear only".into(),
        );
    }
    Ok(())
}

enum Track {
    // ST 429-18, the DCP wrap
    Dcp {
        reader: asdcplib::atmos::MxfReader,
        frame_buffer: Vec<u8>,
        keys: Option<(AesDecContext, HmacContext)>,
    },
    // ST 2067-201, the IMF wrap, which asdcplib writes in the clear only
    Imf {
        reader: asdcplib::as02::iab::MxfReader,
    },
}

pub struct AtmosReader {
    track: Track,
    frame_count: u32,
    edit_rate: asdcplib::Rational,
}

impl AtmosReader {
    pub fn open(path: &Path, keys: Option<&ContentKeys>) -> Result<Self, String> {
        let path_text = path.to_string_lossy();
        let named = |error: String| format!("{}: {error}", path.display());
        let essence_type =
            asdcplib::essence_type(&path_text).map_err(|error| named(error.to_string()))?;
        match essence_type {
            asdcplib::EssenceType::DcDataDolbyAtmos => {
                let mut reader = asdcplib::atmos::MxfReader::new();
                reader
                    .open_read(&path_text)
                    .map_err(|error| named(error.to_string()))?;
                let descriptor = reader
                    .atmos_descriptor()
                    .map_err(|error| named(error.to_string()))?;
                let info = reader
                    .writer_info()
                    .map_err(|error| named(error.to_string()))?;
                let keys = if info.encrypted_essence {
                    let keys = keys.ok_or_else(|| {
                        named("the Atmos track is encrypted and no content key was given".into())
                    })?;
                    Some(
                        keys.decrypt_and_hmac_contexts(&info, "Atmos")
                            .map_err(named)?,
                    )
                } else {
                    None
                };
                let largest_frame = largest_frame_bytes(descriptor.edit_rate).map_err(named)?;
                Ok(AtmosReader {
                    track: Track::Dcp {
                        reader,
                        frame_buffer: vec![0; largest_frame],
                        keys,
                    },
                    frame_count: descriptor.container_duration,
                    edit_rate: descriptor.edit_rate,
                })
            }
            asdcplib::EssenceType::As02Iab => {
                let mut reader = asdcplib::as02::iab::MxfReader::new();
                reader
                    .open_read(&path_text)
                    .map_err(|error| named(error.to_string()))?;
                let info = reader
                    .writer_info()
                    .map_err(|error| named(error.to_string()))?;
                check_imf_track_is_clear(&info).map_err(named)?;
                let frame_count = reader
                    .frame_count()
                    .map_err(|error| named(error.to_string()))?;
                let descriptor = reader
                    .iab_essence_descriptor()
                    .map_err(|error| named(error.to_string()))?;
                Ok(AtmosReader {
                    track: Track::Imf { reader },
                    frame_count,
                    edit_rate: descriptor.sample_rate,
                })
            }
            other => Err(named(format!("{other:?} is not an Atmos track"))),
        }
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    pub fn edit_rate(&self) -> asdcplib::Rational {
        self.edit_rate
    }

    // the IAB bytes of one frame, decrypted and checked against its HMAC when encrypted
    pub fn read_frame(&mut self, frame_number: u32) -> Result<&[u8], String> {
        if frame_number >= self.frame_count {
            return Err(format!(
                "Atmos frame {frame_number} is past the last of {} frames",
                self.frame_count
            ));
        }
        match &mut self.track {
            Track::Imf { reader } => reader
                .read_frame(frame_number)
                .map_err(|error| format!("Atmos frame {frame_number}: {error}")),
            Track::Dcp {
                reader,
                frame_buffer,
                keys,
            } => {
                let (decrypt, hmac) = match keys {
                    Some((decrypt, hmac)) => (Some(decrypt), Some(hmac)),
                    None => (None, None),
                };
                let result = reader.read_frame(frame_number, frame_buffer, decrypt, hmac);
                match result {
                    Ok(size) => Ok(&frame_buffer[..size]),
                    Err(asdcplib::Error::AsdcpError(SMALL_BUFFER_RESULT)) => Err(format!(
                        "Atmos frame {frame_number} is over the {} bytes ST 429-18 Table 1 allows at its edit rate",
                        frame_buffer.len()
                    )),
                    Err(error) => Err(format!("Atmos frame {frame_number}: {error}")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iab::tests::{fixture_directory, read_zipped_stream};
    use crate::mxf_wrap::{
        EssenceType, McaConfig, MxfEncryption, MxfStandard, MxfWrapOptions, SoundfieldGroup,
        mxf_wrap,
    };
    use std::path::PathBuf;

    const CONTENT_KEY: [u8; 16] = [0x5a; 16];
    const KEY_ID: [u8; 16] = [0x17; 16];
    const FRAME_COUNT: usize = 4;

    fn stream_1_frames() -> Vec<Vec<u8>> {
        let bytes = read_zipped_stream(&fixture_directory().join("IABValidationStream_1.iab.zip"));
        crate::iab::parse::split_frames(&bytes).unwrap()[..FRAME_COUNT]
            .iter()
            .map(|frame| frame.to_vec())
            .collect()
    }

    fn wrap(
        directory: &Path,
        frames: &[Vec<u8>],
        standard: MxfStandard,
        encryption: Option<MxfEncryption>,
    ) -> PathBuf {
        let input_files: Vec<PathBuf> = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let path = directory.join(format!("frame_{index:05}.iab"));
                std::fs::write(&path, frame).unwrap();
                path
            })
            .collect();
        let output = directory.join("atmos.mxf");
        let mca_config = (standard == MxfStandard::As02).then(|| McaConfig {
            labels: String::new(),
            spoken_language: Some("en-US".to_string()),
            soundfield_group: Some(SoundfieldGroup {
                title: "IAB".to_string(),
                title_version: "1".to_string(),
                audio_content_kind: "PRM".to_string(),
                audio_element_kind: "FCMP".to_string(),
            }),
        });
        let result = mxf_wrap(&MxfWrapOptions {
            input_files,
            output: output.clone(),
            essence_type: EssenceType::Atmos,
            standard,
            fps_num: 24,
            fps_den: 1,
            partition_size: 1,
            encryption,
            mca_config,
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
        });
        assert!(result.success, "{}", result.error);
        output
    }

    fn keys_with(directory: &Path, content_key: [u8; 16]) -> ContentKeys {
        let path = directory.join("keys.json");
        let json = serde_json::json!({ "keys": [{
            "key_id": uuid::Uuid::from_bytes(KEY_ID).to_string(),
            "content_key_hex": hex::encode(content_key),
        }]});
        std::fs::write(&path, json.to_string()).unwrap();
        ContentKeys::from_keys_json(&path).unwrap()
    }

    fn encryption() -> Option<MxfEncryption> {
        Some(MxfEncryption {
            content_key: CONTENT_KEY,
            key_id: KEY_ID,
        })
    }

    fn assert_reads_back(reader: &mut AtmosReader, frames: &[Vec<u8>]) {
        assert_eq!(reader.frame_count(), FRAME_COUNT as u32);
        assert_eq!(reader.edit_rate(), asdcplib::Rational::new(24, 1));
        for (index, frame) in frames.iter().enumerate().rev() {
            assert_eq!(
                reader.read_frame(index as u32).unwrap(),
                frame.as_slice(),
                "frame {index}"
            );
        }
        assert!(reader.read_frame(FRAME_COUNT as u32).is_err());
    }

    #[test]
    fn a_clear_dcp_atmos_track_reads_back_its_frames() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = wrap(directory.path(), &frames, MxfStandard::AsDcp, None);
        assert_reads_back(&mut AtmosReader::open(&path, None).unwrap(), &frames);
    }

    #[test]
    fn an_imf_iab_track_reads_back_its_frames() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = wrap(directory.path(), &frames, MxfStandard::As02, None);
        assert_reads_back(&mut AtmosReader::open(&path, None).unwrap(), &frames);
    }

    #[test]
    fn an_encrypted_dcp_atmos_track_decrypts_with_its_key() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = wrap(directory.path(), &frames, MxfStandard::AsDcp, encryption());
        let keys = keys_with(directory.path(), CONTENT_KEY);
        assert_reads_back(&mut AtmosReader::open(&path, Some(&keys)).unwrap(), &frames);
        assert!(AtmosReader::open(&path, None).is_err());
    }

    #[test]
    fn a_wrong_key_fails_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = wrap(directory.path(), &frames, MxfStandard::AsDcp, encryption());
        let keys = keys_with(directory.path(), [0xa5; 16]);
        let mut reader = AtmosReader::open(&path, Some(&keys)).unwrap();
        assert!(reader.read_frame(0).is_err());
    }

    #[test]
    fn a_frame_over_the_st_429_18_limit_fails_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let mut frames = stream_1_frames();
        frames[2] = vec![0; 781_250];
        frames[3] = vec![0; 781_251];
        let path = wrap(directory.path(), &frames, MxfStandard::AsDcp, None);
        let mut reader = AtmosReader::open(&path, None).unwrap();
        assert_eq!(reader.read_frame(2).unwrap(), frames[2].as_slice());
        let error = match reader.read_frame(3) {
            Ok(frame) => panic!("read {} bytes", frame.len()),
            Err(error) => error,
        };
        assert!(error.contains("781250"), "{error}");
        assert_eq!(reader.read_frame(0).unwrap(), frames[0].as_slice());
    }

    #[test]
    fn an_encrypted_imf_iab_track_is_refused() {
        let encrypted = asdcplib::WriterInfo {
            encrypted_essence: true,
            ..Default::default()
        };
        assert!(check_imf_track_is_clear(&encrypted).is_err());
        assert!(check_imf_track_is_clear(&asdcplib::WriterInfo::default()).is_ok());
    }

    #[test]
    fn a_tampered_frame_fails_the_hmac() {
        // the encrypted triplet key of SMPTE ST 429-6 as asdcplib writes it
        const ENCRYPTED_TRIPLET_KEY: [u8; 16] = [
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x04, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x7e,
            0x01, 0x00,
        ];
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = wrap(directory.path(), &frames, MxfStandard::AsDcp, encryption());
        let mut bytes = std::fs::read(&path).unwrap();
        let first_frame = bytes
            .windows(16)
            .position(|window| window == ENCRYPTED_TRIPLET_KEY)
            .unwrap();
        // well inside the ciphertext, which runs for nearly the whole frame
        let tampered = first_frame + frames[0].len() / 2;
        bytes[tampered] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();

        let keys = keys_with(directory.path(), CONTENT_KEY);
        let mut reader = AtmosReader::open(&path, Some(&keys)).unwrap();
        assert!(reader.read_frame(0).is_err());
        assert_eq!(reader.read_frame(1).unwrap(), frames[1].as_slice());
    }
}
