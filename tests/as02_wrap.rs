//! AS-02 (IMF) MXF wrapping roundtrip through postkit's public wrap API.

use postkit::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};
use std::path::PathBuf;

fn temp_path(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "postkit-as02-{tag}-{}-{unique}",
        std::process::id()
    ))
}

/// JPEG 2000 codestream with a real SOC/SIZ so postkit's header parse accepts it.
/// asdcplib stores the frame opaquely, so payload bytes survive a write/read cycle.
fn synthetic_j2c(width: u32, height: u32, seed: u8, payload_len: usize) -> Vec<u8> {
    let comps: u16 = 3;
    let mut siz = Vec::new();
    siz.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
    siz.extend_from_slice(&width.to_be_bytes()); // Xsiz
    siz.extend_from_slice(&height.to_be_bytes()); // Ysiz
    siz.extend_from_slice(&0u32.to_be_bytes()); // XOsiz
    siz.extend_from_slice(&0u32.to_be_bytes()); // YOsiz
    siz.extend_from_slice(&width.to_be_bytes()); // XTsiz
    siz.extend_from_slice(&height.to_be_bytes()); // YTsiz
    siz.extend_from_slice(&0u32.to_be_bytes()); // XTOsiz
    siz.extend_from_slice(&0u32.to_be_bytes()); // YTOsiz
    siz.extend_from_slice(&comps.to_be_bytes()); // Csiz
    for _ in 0..comps {
        siz.extend_from_slice(&[0x0b, 0x01, 0x01]); // Ssiz, XRsiz, YRsiz
    }

    let mut data = vec![0xff, 0x4f]; // SOC
    data.extend_from_slice(&[0xff, 0x51]); // SIZ marker
    data.extend_from_slice(&((siz.len() + 2) as u16).to_be_bytes()); // Lsiz includes itself
    data.extend_from_slice(&siz);
    data.extend_from_slice(&[0xff, 0x93]); // SOD
    data.extend((0..payload_len).map(|i| seed.wrapping_add(i as u8)));
    data.extend_from_slice(&[0xff, 0xd9]); // EOC
    data
}

/// Minimal 44-byte WAV header + raw PCM body of the requested length.
fn synthetic_wav(pcm: &[u8]) -> Vec<u8> {
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + pcm.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&6u16.to_le_bytes()); // channels
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&864_000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&18u16.to_le_bytes()); // block align
    wav.extend_from_slice(&24u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

#[test]
fn as02_j2k_roundtrip() {
    let dir = temp_path("j2k-in");
    std::fs::create_dir_all(&dir).unwrap();
    let frames: Vec<Vec<u8>> = (0..3)
        .map(|i| synthetic_j2c(2048, 1080, i as u8 * 40 + 1, 4096 + i * 32))
        .collect();
    let mut input_files = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        let p = dir.join(format!("frame{i}.j2c"));
        std::fs::write(&p, frame).unwrap();
        input_files.push(p);
    }

    let output = temp_path("j2k.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files,
        output: output.clone(),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);

    let out_str = output.to_string_lossy().to_string();
    assert_eq!(
        asdcplib::essence_type(&out_str).unwrap(),
        asdcplib::EssenceType::As02Jpeg2000
    );

    let mut reader = asdcplib::as02::jp2k::MxfReader::new();
    reader.open_read(&out_str).unwrap();
    let desc = reader.picture_descriptor().unwrap();
    assert_eq!(desc.stored_width, 2048);
    assert_eq!(desc.stored_height, 1080);
    assert_eq!(desc.container_duration, frames.len() as u32);
    let mut buf = vec![0u8; 16 * 1024];
    let size = reader.read_frame(0, &mut buf, None, None).unwrap();
    assert_eq!(&buf[..size], frames[0].as_slice());
    reader.close().unwrap();

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_file(&output).ok();
}

#[test]
fn as02_pcm_roundtrip() {
    // 2 frames at 48k/24fps, 6ch * 24-bit = 36000 bytes/frame
    let frame_size = 36_000usize;
    let pcm: Vec<u8> = (0..frame_size * 2).map(|i| (i % 251) as u8).collect();
    let wav = synthetic_wav(&pcm);
    let input = temp_path("audio.wav");
    std::fs::write(&input, &wav).unwrap();

    let output = temp_path("pcm.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![input.clone()],
        output: output.clone(),
        essence_type: EssenceType::Pcm,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);
    assert_eq!(result.duration, 2);

    let out_str = output.to_string_lossy().to_string();
    assert_eq!(
        asdcplib::essence_type(&out_str).unwrap(),
        asdcplib::EssenceType::As02Pcm24b48k
    );

    let mut reader = asdcplib::as02::pcm::MxfReader::new();
    reader
        .open_read(&out_str, asdcplib::Rational::new(24, 1))
        .unwrap();
    let desc = reader.audio_descriptor().unwrap();
    assert_eq!(desc.channel_count, 6);
    assert_eq!(desc.quantization_bits, 24);
    let mut buf = vec![0u8; frame_size];
    let size = reader.read_frame(0, &mut buf, None, None).unwrap();
    assert_eq!(size, frame_size);
    assert_eq!(&buf[..size], &pcm[..frame_size]);
    reader.close().unwrap();

    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&output).ok();
}

#[test]
fn as02_atmos_errors() {
    let input = temp_path("atmos.iab");
    std::fs::write(&input, b"dummy").unwrap();
    let output = temp_path("atmos.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![input.clone()],
        output,
        essence_type: EssenceType::Atmos,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
    });
    assert!(!result.success);
    assert!(
        result.error.contains("AS-02"),
        "error was: {}",
        result.error
    );
    std::fs::remove_file(&input).ok();
}
