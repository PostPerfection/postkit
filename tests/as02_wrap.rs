//! AS-02 (IMF) MXF wrapping roundtrip through postkit's public wrap API.

use postkit::mxf_wrap::{EssenceType, MxfEncryption, MxfStandard, MxfWrapOptions, mxf_wrap};
use std::path::PathBuf;

/// A one-subtitle DCST, the smallest input the timed-text wrap accepts.
const DCST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<dcst:SubtitleReel xmlns:dcst=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\">\n\
  <dcst:Id>urn:uuid:11111111-1111-1111-1111-111111111111</dcst:Id>\n\
  <dcst:ContentTitleText>t</dcst:ContentTitleText>\n\
  <dcst:IssueDate>2020-01-01T00:00:00+00:00</dcst:IssueDate>\n\
  <dcst:EditRate>24 1</dcst:EditRate>\n\
  <dcst:TimeCodeRate>24</dcst:TimeCodeRate>\n\
  <dcst:SubtitleList>\n\
    <dcst:Font ID=\"f1\">\n\
      <dcst:Subtitle SpotNumber=\"1\" TimeIn=\"00:00:01:00\" TimeOut=\"00:00:04:00\">\n\
        <dcst:Text>hi</dcst:Text>\n\
      </dcst:Subtitle>\n\
    </dcst:Font>\n\
  </dcst:SubtitleList>\n\
</dcst:SubtitleReel>\n";
/// A string only the cleartext subtitle XML can contain.
const MARKER: &[u8] = b"SubtitleReel";

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

/// The Netflix IMF 4K codestream, with `filler` bytes appended after its EOC so
/// a multi-frame wrap has payloads of distinct content and length: a frame
/// mix-up would otherwise read back as a pass.
fn imf_frame(filler: usize) -> Vec<u8> {
    let mut frame = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/imf4k_black_3840x2160.j2c"),
    )
    .expect("IMF fixture codestream");
    frame.extend(std::iter::repeat_n(filler as u8, filler));
    frame
}

/// The raster the IMF fixture carries.
const IMF_FIXTURE_SIZE: (u32, u32) = (3840, 2160);

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
    let frames: Vec<Vec<u8>> = (0..3).map(|i| imf_frame(i * 32 + 1)).collect();
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
        resource_ids: vec![],
        hdr: Some(postkit::mxf_wrap::rec709_sdr_picture_colour()),
        asset_uuid: None,
        timed_text_duration_frames: None,
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
    assert_eq!(desc.stored_width, IMF_FIXTURE_SIZE.0);
    assert_eq!(desc.stored_height, IMF_FIXTURE_SIZE.1);
    assert_eq!(desc.container_duration, frames.len() as u32);
    let mut buf = vec![0u8; 64 * 1024];
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
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
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

/// IMF subtitles reach the same encryption path as the DCP ones, so an
/// encrypted AS-02 wrap must not leave the XML readable in the file.
#[test]
fn as02_timed_text_encrypts_the_subtitle_xml() {
    const CONTENT_KEY: [u8; 16] = [0x51; 16];
    const KEY_ID: [u8; 16] = [0x52; 16];

    let input = temp_path("sub.xml");
    std::fs::write(&input, DCST).unwrap();
    let output = temp_path("sub.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![input.clone()],
        output: output.clone(),
        essence_type: EssenceType::TimedText,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: Some(MxfEncryption {
            content_key: CONTENT_KEY,
            key_id: KEY_ID,
        }),
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);

    let bytes = std::fs::read(&output).unwrap();
    assert!(
        !bytes.windows(MARKER.len()).any(|w| w == MARKER),
        "subtitle XML survived into the encrypted AS-02 MXF"
    );

    let mut reader = asdcplib::as02::timed_text::MxfReader::new();
    reader.open_read(&output.to_string_lossy()).unwrap();
    let info = reader.writer_info().unwrap();
    assert!(info.encrypted_essence);
    assert!(info.uses_hmac);
    assert_eq!(info.cryptographic_key_id, KEY_ID);

    let mut dec = asdcplib::crypto::AesDecContext::new();
    dec.init_key(&CONTENT_KEY).unwrap();
    let mut hmac = asdcplib::crypto::HmacContext::new();
    hmac.init_key(&CONTENT_KEY, asdcplib::LabelSet::Smpte)
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let size = reader
        .read_timed_text_resource(&mut buf, Some(&mut dec), Some(&mut hmac))
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&buf[..size]), DCST);

    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&output).ok();
}

/// IMSC has no document id element, so an AS-02 wrap of a TTML without one keeps
/// the track file id as the ResourceID instead of refusing the way ST 429-5 asks
/// for a DCST.
#[test]
fn as02_timed_text_without_a_document_id_wraps() {
    const TTML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml" xml:lang="en">
  <body><div><p begin="00:00:01.000" end="00:00:02.000">Hello</p></div></body>
</tt>"#;
    let input = temp_path("imsc.ttml");
    std::fs::write(&input, TTML).unwrap();
    let output = temp_path("imsc.mxf");
    let asset_uuid = [0x53; 16];
    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![input.clone()],
        output: output.clone(),
        essence_type: EssenceType::TimedText,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: Some(asset_uuid),
        timed_text_duration_frames: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);

    let mut reader = asdcplib::as02::timed_text::MxfReader::new();
    reader.open_read(&output.to_string_lossy()).unwrap();
    let descriptor = reader.descriptor().unwrap();
    assert_eq!(descriptor.asset_id, asset_uuid);

    std::fs::remove_file(&input).ok();
    std::fs::remove_file(&output).ok();
}

/// asdcplib has no AS-02 entry point that declares ancillary resources in the
/// header, and an undeclared resource is one no reader can find. Refusing beats
/// embedding a font the player will never see.
#[test]
fn as02_timed_text_refuses_ancillary_resources() {
    let xml = temp_path("sub-with-font.xml");
    std::fs::write(&xml, DCST).unwrap();
    let font = temp_path("f.ttf");
    std::fs::write(&font, vec![0xa1u8; 4096]).unwrap();
    let output = temp_path("sub-with-font.mxf");

    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![xml.clone(), font.clone()],
        output: output.clone(),
        essence_type: EssenceType::TimedText,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(!result.success);
    assert!(
        result.error.contains("cannot embed fonts or images"),
        "error was: {}",
        result.error
    );
    assert!(
        !output.exists(),
        "a refused wrap must not leave a file behind"
    );

    std::fs::remove_file(&xml).ok();
    std::fs::remove_file(&font).ok();
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
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(!result.success);
    assert!(
        result.error.contains("AS-02"),
        "error was: {}",
        result.error
    );
    std::fs::remove_file(&input).ok();
}
