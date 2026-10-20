use std::path::{Path, PathBuf};

use crate::atmos_fixture::{AtmosReelAsset, FRAMES_PER_SECOND, with_atmos_reels};
use crate::iab::tests::wrap_atmos_track;
use crate::mxf_unwrap::tests::wrap as wrap_picture;
use crate::mxf_wrap::{EssenceType, MxfEncryption, MxfStandard, MxfWrapOptions, mxf_wrap};
use crate::packaging::{AssetMap, AssetMapAsset, DcpCpl, DcpCplReel, ns};

pub(super) const SAMPLE_FRAMES_PER_FRAME: usize = 2_000;
const SAMPLE_RATE: u32 = 48_000;
const MAIN_SOUND_CHANNELS: usize = 2;
const MAIN_SOUND_SAMPLE_CYCLE: usize = 30_000;
const MAIN_SOUND_REEL_STEP: usize = 3_001;
const PICTURE_SIZE: u32 = 64;
const CPL_ID: &str = "a7000000-0000-4000-8000-00000000c000";

pub(super) struct AtmosDcpReel {
    pub(super) picture_frames: usize,
    pub(super) atmos_frames: Option<Vec<Vec<u8>>>,
}

pub(super) struct AtmosDcp {
    pub(super) package: PathBuf,
    // by reel, None for a reel without Atmos
    pub(super) atmos_tracks: Vec<Option<PathBuf>>,
    pub(super) main_sound_tracks: Vec<PathBuf>,
}

fn fixture_id(kind: u32, reel: usize) -> String {
    format!("a7000000-0000-4000-8000-{kind:04x}{reel:08x}")
}

// stereo 16-bit, interleaved, numbered so that no two neighbouring frames hold the same samples
pub(super) fn main_sound_samples(reel: usize, frames: usize) -> Vec<i16> {
    (0..frames * SAMPLE_FRAMES_PER_FRAME * MAIN_SOUND_CHANNELS)
        .map(|index| ((index + reel * MAIN_SOUND_REEL_STEP) % MAIN_SOUND_SAMPLE_CYCLE + 1) as i16)
        .collect()
}

// a 24 fps DCP whose reels each hold a picture, the main sound main_sound_samples gives and the Atmos frames given
pub(super) fn write_atmos_dcp(
    directory: &Path,
    reels: &[AtmosDcpReel],
    atmos_encryption: Option<&MxfEncryption>,
) -> AtmosDcp {
    let package = directory.join("package");
    std::fs::create_dir_all(&package).unwrap();
    let codestream = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinema2k_64x64.j2c"),
    )
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut assets = vec![AssetMapAsset {
        id: CPL_ID.into(),
        path: "CPL.xml".into(),
        ..Default::default()
    }];
    let mut cpl_reels = Vec::new();
    let mut atmos_assets = Vec::new();
    let mut atmos_tracks = Vec::new();
    let mut main_sound_tracks = Vec::new();
    for (index, reel) in reels.iter().enumerate() {
        let picture_files: Vec<PathBuf> = (0..reel.picture_frames)
            .map(|frame| {
                let path = scratch.path().join(format!("reel{index}_{frame:05}.j2c"));
                std::fs::write(&path, &codestream).unwrap();
                path
            })
            .collect();
        let picture_name = format!("picture_{index}.mxf");
        wrap_picture(picture_files, package.join(&picture_name), None);
        let sound_name = format!("sound_{index}.mxf");
        write_main_sound(
            scratch.path(),
            &package.join(&sound_name),
            &main_sound_samples(index, reel.picture_frames),
        );
        main_sound_tracks.push(package.join(&sound_name));
        let (picture_id, sound_id, atmos_id) = (
            fixture_id(1, index),
            fixture_id(2, index),
            fixture_id(3, index),
        );
        assets.push(AssetMapAsset {
            id: picture_id.clone(),
            path: picture_name,
            ..Default::default()
        });
        assets.push(AssetMapAsset {
            id: sound_id.clone(),
            path: sound_name,
            ..Default::default()
        });
        cpl_reels.push(DcpCplReel {
            reel_id: fixture_id(4, index),
            picture_id,
            picture_edit_rate_num: FRAMES_PER_SECOND,
            picture_edit_rate_den: 1,
            picture_duration: reel.picture_frames as u64,
            picture_width: PICTURE_SIZE,
            picture_height: PICTURE_SIZE,
            sound_id: Some(sound_id),
            sound_edit_rate_num: FRAMES_PER_SECOND,
            sound_edit_rate_den: 1,
            sound_duration: reel.picture_frames as u64,
            ..Default::default()
        });
        let Some(frames) = &reel.atmos_frames else {
            atmos_assets.push(None);
            atmos_tracks.push(None);
            continue;
        };
        let atmos_name = format!("atmos_{index}.mxf");
        let atmos_path = package.join(&atmos_name);
        wrap_atmos_track(
            frames,
            &atmos_path,
            MxfStandard::AsDcp,
            atmos_encryption.cloned(),
        );
        assets.push(AssetMapAsset {
            id: atmos_id.clone(),
            path: atmos_name,
            ..Default::default()
        });
        atmos_assets.push(Some(AtmosReelAsset {
            id: atmos_id,
            intrinsic_duration: frames.len() as u64,
            entry_point: 0,
            duration: frames.len() as u64,
            key_id: atmos_encryption
                .map(|encryption| uuid::Uuid::from_bytes(encryption.key_id).to_string()),
        }));
        atmos_tracks.push(Some(atmos_path));
    }
    let cpl = DcpCpl {
        uuid: CPL_ID.into(),
        namespace: ns::CPL_SMPTE.into(),
        title: "Atmos".into(),
        reels: cpl_reels,
        ..Default::default()
    };
    std::fs::write(
        package.join("CPL.xml"),
        with_atmos_reels(&cpl.to_xml(), &atmos_assets),
    )
    .unwrap();
    std::fs::write(
        package.join("ASSETMAP.xml"),
        AssetMap {
            uuid: fixture_id(5, 0),
            namespace: ns::AM_SMPTE.into(),
            assets,
            ..Default::default()
        }
        .to_xml(),
    )
    .unwrap();
    AtmosDcp {
        package,
        atmos_tracks,
        main_sound_tracks,
    }
}

fn write_main_sound(scratch: &Path, output: &Path, samples: &[i16]) {
    let wav = scratch.join("main_sound.wav");
    let spec = hound::WavSpec {
        channels: MAIN_SOUND_CHANNELS as u16,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
    for sample in samples {
        writer.write_sample(*sample).unwrap();
    }
    writer.finalize().unwrap();
    let track = mxf_wrap(&MxfWrapOptions {
        input_files: vec![wav],
        output: output.to_path_buf(),
        essence_type: EssenceType::Pcm,
        standard: MxfStandard::AsDcp,
        fps_num: FRAMES_PER_SECOND,
        fps_den: 1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: Vec::new(),
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(track.success, "sound wrap failed: {}", track.error);
}
