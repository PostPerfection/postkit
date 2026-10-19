use std::path::Path;

use asdcplib::crypto::{AesDecContext, HmacContext};

use crate::content_keys::ContentKeys;

// Kumu's RESULT_SMALLBUF, which comes back without the size the frame needs
const SMALL_BUFFER_RESULT: i32 = -7;
const FIRST_FRAME_BUFFER_BYTES: usize = 1 << 20;

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
        let essence_type = asdcplib::essence_type(&path_text).map_err(|e| named(e.to_string()))?;
        match essence_type {
            asdcplib::EssenceType::DcDataDolbyAtmos => {
                let mut reader = asdcplib::atmos::MxfReader::new();
                reader
                    .open_read(&path_text)
                    .map_err(|e| named(e.to_string()))?;
                let descriptor = reader
                    .atmos_descriptor()
                    .map_err(|e| named(e.to_string()))?;
                let info = reader.writer_info().map_err(|e| named(e.to_string()))?;
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
                Ok(AtmosReader {
                    track: Track::Dcp {
                        reader,
                        frame_buffer: vec![0; FIRST_FRAME_BUFFER_BYTES],
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
                    .map_err(|e| named(e.to_string()))?;
                let frame_count = reader.frame_count().map_err(|e| named(e.to_string()))?;
                let descriptor = reader
                    .iab_essence_descriptor()
                    .map_err(|e| named(e.to_string()))?;
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
                .map_err(|e| format!("Atmos frame {frame_number}: {e}")),
            Track::Dcp {
                reader,
                frame_buffer,
                keys,
            } => {
                let (mut decrypt, mut hmac) = match keys {
                    Some((decrypt, hmac)) => (Some(decrypt), Some(hmac)),
                    None => (None, None),
                };
                loop {
                    let result = reader.read_frame(
                        frame_number,
                        frame_buffer,
                        decrypt.as_deref_mut(),
                        hmac.as_deref_mut(),
                    );
                    match result {
                        Ok(size) => return Ok(&frame_buffer[..size]),
                        Err(asdcplib::Error::AsdcpError(SMALL_BUFFER_RESULT))
                            if frame_buffer.len() < u32::MAX as usize =>
                        {
                            let grown = frame_buffer.len().saturating_mul(2).min(u32::MAX as usize);
                            *frame_buffer = vec![0; grown];
                        }
                        Err(error) => return Err(format!("Atmos frame {frame_number}: {error}")),
                    }
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
