use std::path::Path;

use asdcplib::crypto::{AesDecContext, HmacContext};

use crate::content_keys::decrypt_and_hmac_contexts_from_key;

// Kumu's RESULT_SMALLBUF
const SMALL_BUFFER_RESULT: i32 = -7;

// ST 429-18:2023 7.2.4 Table 1, the largest frame in bytes for each edit rate in frames a second
pub(super) const LARGEST_FRAME_BYTES: [(i32, usize); 9] = [
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

// ST 2067-201 sets no IAB frame size, 16 MiB is over twenty times the 128 uncoded 24-bit channels ST 2098-2 Annex C lets a frame render
pub const LARGEST_IMF_FRAME_BYTES: u32 = 16 * 1024 * 1024;

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

impl Track {
    fn open(path: &Path) -> Result<Track, String> {
        let path_text = path.to_string_lossy();
        let essence_type = asdcplib::essence_type(&path_text).map_err(|error| error.to_string())?;
        match essence_type {
            asdcplib::EssenceType::DcDataDolbyAtmos => {
                let mut reader = asdcplib::atmos::MxfReader::new();
                reader
                    .open_read(&path_text)
                    .map_err(|error| error.to_string())?;
                Ok(Track::Dcp {
                    reader,
                    frame_buffer: Vec::new(),
                    keys: None,
                })
            }
            asdcplib::EssenceType::As02Iab => {
                let mut reader = asdcplib::as02::iab::MxfReader::new(LARGEST_IMF_FRAME_BYTES);
                reader
                    .open_read(&path_text)
                    .map_err(|error| error.to_string())?;
                Ok(Track::Imf { reader })
            }
            other => Err(format!("{other:?} is not an Atmos track")),
        }
    }

    fn writer_info(&mut self) -> asdcplib::Result<asdcplib::WriterInfo> {
        match self {
            Track::Dcp { reader, .. } => reader.writer_info(),
            Track::Imf { reader } => reader.writer_info(),
        }
    }
}

pub fn atmos_writer_info(path: &Path) -> Result<asdcplib::WriterInfo, String> {
    let named = |error: String| format!("{}: {error}", path.display());
    Track::open(path)
        .map_err(named)?
        .writer_info()
        .map_err(|error| named(error.to_string()))
}

pub struct AtmosReader {
    track: Track,
    frame_count: u32,
    edit_rate: asdcplib::Rational,
}

impl AtmosReader {
    pub fn open(path: &Path, content_key: Option<&[u8; 16]>) -> Result<Self, String> {
        let named = |error: String| format!("{}: {error}", path.display());
        let mut track = Track::open(path).map_err(named)?;
        let info = track
            .writer_info()
            .map_err(|error| named(error.to_string()))?;
        let (frame_count, edit_rate) = match &mut track {
            Track::Dcp {
                reader,
                frame_buffer,
                keys,
            } => {
                if info.encrypted_essence {
                    let key = content_key.ok_or_else(|| {
                        named("the Atmos track is encrypted and no content key was given".into())
                    })?;
                    *keys = Some(
                        decrypt_and_hmac_contexts_from_key(key, info.label_set).map_err(named)?,
                    );
                }
                let descriptor = reader
                    .atmos_descriptor()
                    .map_err(|error| named(error.to_string()))?;
                *frame_buffer = vec![0; largest_frame_bytes(descriptor.edit_rate).map_err(named)?];
                (descriptor.container_duration, descriptor.edit_rate)
            }
            Track::Imf { reader } => {
                check_imf_track_is_clear(&info).map_err(named)?;
                let frame_count = reader
                    .frame_count()
                    .map_err(|error| named(error.to_string()))?;
                let descriptor = reader
                    .iab_essence_descriptor()
                    .map_err(|error| named(error.to_string()))?;
                (frame_count, descriptor.sample_rate)
            }
        };
        Ok(AtmosReader {
            track,
            frame_count,
            edit_rate,
        })
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
            return Err(format!("the track holds only {} frames", self.frame_count));
        }
        match &mut self.track {
            Track::Imf { reader } => reader
                .read_frame(frame_number)
                .map_err(|error| error.to_string()),
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
                        "the frame is over the {} bytes ST 429-18 Table 1 allows at its edit rate",
                        frame_buffer.len()
                    )),
                    Err(error) => Err(error.to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iab::tests::{validation_stream_frames, wrap_atmos_track};
    use crate::mxf_wrap::{MxfEncryption, MxfStandard};

    const CONTENT_KEY: [u8; 16] = [0x5a; 16];
    const KEY_ID: [u8; 16] = [0x17; 16];
    const FRAME_COUNT: usize = 4;

    fn stream_1_frames() -> Vec<Vec<u8>> {
        validation_stream_frames(1)[..FRAME_COUNT].to_vec()
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
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::AsDcp, None);
        assert_reads_back(&mut AtmosReader::open(&path, None).unwrap(), &frames);
    }

    #[test]
    fn an_imf_iab_track_reads_back_its_frames() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::As02, None);
        assert_reads_back(&mut AtmosReader::open(&path, None).unwrap(), &frames);
    }

    #[test]
    fn an_encrypted_dcp_atmos_track_decrypts_with_its_key() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::AsDcp, encryption());
        assert_reads_back(
            &mut AtmosReader::open(&path, Some(&CONTENT_KEY)).unwrap(),
            &frames,
        );
        assert!(AtmosReader::open(&path, None).is_err());
        assert_eq!(
            atmos_writer_info(&path).unwrap().cryptographic_key_id,
            KEY_ID
        );
    }

    #[test]
    fn a_wrong_key_fails_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let frames = stream_1_frames();
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::AsDcp, encryption());
        let mut reader = AtmosReader::open(&path, Some(&[0xa5; 16])).unwrap();
        assert!(reader.read_frame(0).is_err());
    }

    #[test]
    fn a_frame_over_the_st_429_18_limit_fails_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let mut frames = stream_1_frames();
        frames[2] = vec![0; 781_250];
        frames[3] = vec![0; 781_251];
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::AsDcp, None);
        let mut reader = AtmosReader::open(&path, None).unwrap();
        assert_eq!(reader.read_frame(2).unwrap(), frames[2].as_slice());
        let error = match reader.read_frame(3) {
            Ok(frame) => panic!("read {} bytes", frame.len()),
            Err(error) => error,
        };
        assert!(error.contains("781250"), "{error}");
        assert_eq!(reader.read_frame(0).unwrap(), frames[0].as_slice());
    }

    fn ia_bitstream_frame_of(frame_bytes: usize) -> Vec<u8> {
        const PREAMBLE_TAG: u8 = 0x01;
        const IA_FRAME_TAG: u8 = 0x02;
        const ELEMENT_HEADER_BYTES: usize = 5;
        let ia_frame_length = frame_bytes - 2 * ELEMENT_HEADER_BYTES;
        let mut frame = vec![PREAMBLE_TAG, 0, 0, 0, 0, IA_FRAME_TAG];
        frame.extend((ia_frame_length as u32).to_be_bytes());
        frame.resize(frame_bytes, 0);
        frame
    }

    #[test]
    fn an_imf_iab_frame_over_the_limit_fails_the_read() {
        let directory = tempfile::tempdir().unwrap();
        let largest = LARGEST_IMF_FRAME_BYTES as usize;
        let mut frames = stream_1_frames();
        frames[2] = ia_bitstream_frame_of(largest);
        frames[3] = ia_bitstream_frame_of(largest + 1);
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::As02, None);
        let mut reader = AtmosReader::open(&path, None).unwrap();
        assert_eq!(reader.read_frame(2).unwrap(), frames[2].as_slice());
        let error = match reader.read_frame(3) {
            Ok(frame) => panic!("read {} bytes", frame.len()),
            Err(error) => error,
        };
        assert!(
            error.contains(&LARGEST_IMF_FRAME_BYTES.to_string()),
            "{error}"
        );
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
        let path = directory.path().join("atmos.mxf");
        wrap_atmos_track(&frames, &path, MxfStandard::AsDcp, encryption());
        let mut bytes = std::fs::read(&path).unwrap();
        let first_frame = bytes
            .windows(16)
            .position(|window| window == ENCRYPTED_TRIPLET_KEY)
            .unwrap();
        // well inside the ciphertext, which runs for nearly the whole frame
        let tampered = first_frame + frames[0].len() / 2;
        bytes[tampered] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();

        let mut reader = AtmosReader::open(&path, Some(&CONTENT_KEY)).unwrap();
        assert!(reader.read_frame(0).is_err());
        assert_eq!(reader.read_frame(1).unwrap(), frames[1].as_slice());
    }
}
