use super::*;
use crate::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};

const CHANNELS: usize = 16;
const ASSET_UUID: [u8; 16] = [
    0xe0, 0x04, 0x04, 0x6e, 0x09, 0x23, 0x4f, 0x90, 0xa4, 0xae, 0x43, 0x55, 0xe7, 0xe8, 0x35, 0x06,
];

fn sample_at(frame: &[u8], sample: usize, channel: usize) -> i32 {
    let offset = (sample * CHANNELS + channel) * BYTES_PER_SAMPLE;
    i32::from_le_bytes([0, frame[offset], frame[offset + 1], frame[offset + 2]]) >> 8
}

fn patterned_frame(frames_per_second: u32) -> Vec<u8> {
    let samples = (ATMOS_SYNC_SAMPLE_RATE / frames_per_second) as usize;
    (0..samples * CHANNELS)
        .flat_map(|index| ((index as i32 * 37) % 5_000_000).to_le_bytes()[..3].to_vec())
        .collect()
}

fn signed_frame(signal: &mut AtmosSyncSignal, frames_per_second: u32) -> Vec<u8> {
    let mut frame = patterned_frame(frames_per_second);
    signal.write_frame(&mut frame);
    frame
}

#[test]
fn the_crc_matches_the_ccitt_xmodem_check_value() {
    let bits: Vec<bool> = b"123456789"
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1 == 1))
        .collect();
    assert_eq!(crc16(&bits), 0x31c3);
}

#[test]
fn the_waveform_opens_with_the_sync_word_most_significant_bit_first() {
    let mut signal = AtmosSyncSignal::new(ASSET_UUID, 24, 48_000, 16).unwrap();
    let frame = signed_frame(&mut signal, 24);
    // a 1 swings through zero halfway through its four samples, a 0 keeps one sign
    let opening: String = (0..16)
        .map(|bit| {
            let first = sample_at(&frame, bit * 4, ATMOS_SYNC_CHANNEL_INDEX);
            let third = sample_at(&frame, bit * 4 + 2, ATMOS_SYNC_CHANNEL_INDEX);
            if (first > 0) != (third > 0) { '1' } else { '0' }
        })
        .collect();
    assert_eq!(opening, "0100110101010110");
}

#[test]
fn each_frame_at_24_fps_carries_four_packets_naming_the_asset_and_the_frame() {
    let mut signal = AtmosSyncSignal::new(ASSET_UUID, 24, 48_000, 16).unwrap();
    for frame_index in 0..3 {
        let original = patterned_frame(24);
        let frame = signed_frame(&mut signal, 24);
        let packets = decode_atmos_sync_frame(&frame, CHANNELS, 24).unwrap();
        assert_eq!(packets.len(), 4);
        for (place, packet) in packets.iter().enumerate() {
            assert_eq!(packet.counter, place as u8);
            assert_eq!(
                packet.sound_asset_uuid_quarter,
                ASSET_UUID[place * 4..place * 4 + 4]
            );
            assert_eq!(packet.frame_index, frame_index);
        }
        for sample in 0..2000 {
            for channel in (0..CHANNELS).filter(|&channel| channel != ATMOS_SYNC_CHANNEL_INDEX) {
                assert_eq!(
                    sample_at(&frame, sample, channel),
                    sample_at(&original, sample, channel)
                );
            }
        }
    }
}

#[test]
fn the_packet_counter_runs_on_across_frames_at_48_fps() {
    let mut signal = AtmosSyncSignal::new(ASSET_UUID, 48, 48_000, 16).unwrap();
    let counters: Vec<u8> = (0..2)
        .flat_map(|_| {
            decode_atmos_sync_frame(&signed_frame(&mut signal, 48), CHANNELS, 48).unwrap()
        })
        .map(|packet| packet.counter)
        .collect();
    assert_eq!(counters, [0, 1, 2, 3]);
}

#[test]
fn the_waveform_crosses_zero_at_every_bit_boundary() {
    let mut signal = AtmosSyncSignal::new(ASSET_UUID, 25, 48_000, 16).unwrap();
    let frames = [signed_frame(&mut signal, 25), signed_frame(&mut signal, 25)];
    let samples: Vec<i32> = frames
        .iter()
        .flat_map(|frame| {
            (0..1920).map(|sample| sample_at(frame, sample, ATMOS_SYNC_CHANNEL_INDEX))
        })
        .collect();
    for boundary in (SAMPLES_PER_BIT..samples.len()).step_by(SAMPLES_PER_BIT) {
        assert!(
            (samples[boundary - 1] > 0) != (samples[boundary] > 0),
            "no zero crossing at sample {boundary}"
        );
    }
}

#[test]
fn a_damaged_or_missing_sync_signal_does_not_decode() {
    let mut signal = AtmosSyncSignal::new(ASSET_UUID, 24, 48_000, 16).unwrap();
    let mut frame = signed_frame(&mut signal, 24);
    // turns the first frame index bit of packet 0 from a 0 into a 1
    let first_frame_index_bit = 16 + 8 + 32;
    for position in 2..4 {
        let sample = first_frame_index_bit * 4 + position;
        let offset = (sample * CHANNELS + ATMOS_SYNC_CHANNEL_INDEX) * BYTES_PER_SAMPLE;
        let flipped = -sample_at(&frame, sample, ATMOS_SYNC_CHANNEL_INDEX);
        frame[offset..offset + 3].copy_from_slice(&flipped.to_le_bytes()[..3]);
    }
    let error = decode_atmos_sync_frame(&frame, CHANNELS, 24).unwrap_err();
    assert!(error.contains("CRC"), "{error}");

    let silent = vec![0u8; 2000 * CHANNELS * BYTES_PER_SAMPLE];
    let error = decode_atmos_sync_frame(&silent, CHANNELS, 24).unwrap_err();
    assert!(error.contains("no sync signal"), "{error}");
}

#[test]
fn sound_the_sync_signal_cannot_carry_is_refused() {
    assert!(check_atmos_sync_sound(24, 48_000, 14).is_ok());
    assert!(check_atmos_sync_sound(24, 96_000, 16).is_err());
    assert!(check_atmos_sync_sound(23, 48_000, 16).is_err());
    assert!(check_atmos_sync_sound(24, 48_000, 8).is_err());
}

fn write_wav(path: &std::path::Path, channels: u16, frames: usize) -> Vec<i32> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: 48_000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let samples: Vec<i32> = (0..frames * 2000 * usize::from(channels))
        .map(|index| (index as i32 * 53) % 4_000_000 - 2_000_000)
        .collect();
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for &sample in &samples {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    samples
}

fn sync_wrap_options(input: std::path::PathBuf, output: std::path::PathBuf) -> MxfWrapOptions {
    MxfWrapOptions {
        input_files: vec![input],
        output,
        essence_type: EssenceType::PcmWithAtmosSync,
        standard: MxfStandard::AsDcp,
        fps_num: 24,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: Some(ASSET_UUID),
        timed_text_duration_frames: None,
    }
}

#[test]
fn the_wrapped_sound_mxf_carries_the_sync_signal_and_every_other_channel_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sound.wav");
    let source = write_wav(&wav, 16, 3);
    let mxf = dir.path().join("sound.mxf");
    let result = mxf_wrap(&sync_wrap_options(wav, mxf.clone()));
    assert!(result.success, "{}", result.error);

    let mut reader = asdcplib::pcm::MxfReader::new();
    reader.open_read(&mxf.to_string_lossy()).unwrap();
    let mut frame = vec![0u8; 2000 * CHANNELS * BYTES_PER_SAMPLE];
    for frame_index in 0..3 {
        let read = reader
            .read_frame(frame_index, &mut frame, None, None)
            .unwrap();
        assert_eq!(read, frame.len());
        let packets = decode_atmos_sync_frame(&frame, CHANNELS, 24).unwrap();
        assert!(
            packets
                .iter()
                .all(|packet| packet.frame_index == frame_index)
        );
        let uuid: Vec<u8> = packets
            .iter()
            .flat_map(|packet| packet.sound_asset_uuid_quarter)
            .collect();
        assert_eq!(uuid, ASSET_UUID);
        for sample in 0..2000 {
            for channel in (0..CHANNELS).filter(|&channel| channel != ATMOS_SYNC_CHANNEL_INDEX) {
                let at = (frame_index as usize * 2000 + sample) * CHANNELS + channel;
                assert_eq!(sample_at(&frame, sample, channel), source[at]);
            }
        }
    }
}

#[test]
fn a_sound_track_too_narrow_for_channel_fourteen_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sound.wav");
    write_wav(&wav, 6, 1);
    let mxf = dir.path().join("sound.mxf");
    let result = mxf_wrap(&sync_wrap_options(wav, mxf.clone()));
    assert!(!result.success);
    assert!(result.error.contains("channel 14"), "{}", result.error);
    assert!(!mxf.exists());
}
