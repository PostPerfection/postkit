use crate::mxf_unwrap::tests::{FRAME_COUNT, write_frames};
use crate::mxf_wrap::{
    EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap, rec709_sdr_picture_colour,
};
use crate::packaging::{
    App2eEdition, AssetMap, AssetMapAsset, ImfCpl, ImfEssenceDescriptor, ImfResource, ImfTrackKind,
    PackingList, PklAsset, ns,
};
use asdcplib::crypto::{AesEncContext, HmacContext};
use asdcplib::jp2k::{CodestreamHeader, PictureDescriptor};
use asdcplib::{LabelSet, Rational, WriterInfo};
use std::path::{Path, PathBuf};

pub(crate) const CPL_ID: &str = "1a1a0000-0000-4000-8000-000000000001";
pub(crate) const TITLE: &str = "Review Master";
pub(crate) const EDIT_RATE: (u32, u32) = (24000, 1001);
pub(crate) const FRAMES: u64 = FRAME_COUNT as u64;
pub(crate) const PICTURE_KEY_ID: [u8; 16] = [0x5e; 16];
pub(crate) const PICTURE_KEY: [u8; 16] = [0x6f; 16];
// 48000 Hz at 24000/1001
pub(crate) const SAMPLE_FRAMES_PER_EDIT_UNIT: usize = 2_002;
pub(crate) const SOUND_CHANNELS: u16 = 2;

const PICTURE_ID: &str = "1a1a0000-0000-4000-8000-000000000002";
const SOUND_ID: &str = "1a1a0000-0000-4000-8000-000000000003";
const PICTURE_DESCRIPTOR_ID: &str = "1a1a0000-0000-4000-8000-000000000004";
const PACKING_LIST_ID: &str = "1a1a0000-0000-4000-8000-000000000005";
const ASSETMAP_ID: &str = "1a1a0000-0000-4000-8000-000000000006";
const SAMPLE_RATE: u32 = 48_000;
const SOUND_BITS: u16 = 24;
const MXF_HEADER_SIZE: u32 = 16_384;
const PICTURE_SIZE: u32 = 64;
const ENCRYPTION_IV: [u8; 16] = [0x9c; 16];
const CRYPTOGRAPHIC_CONTEXT_ID: [u8; 16] = [0xc7; 16];
const XML_TYPE: &str = "text/xml";
const MXF_TYPE: &str = "application/mxf";

// what the picture descriptor in the CPL says, the colour App 2E Rec.709 signals
const PICTURE_DESCRIPTOR: &str = "<r0:RGBADescriptor xmlns:r0=\"http://www.smpte-ra.org/reg/395/2014/13/1/aaf\" \
     xmlns:r1=\"http://www.smpte-ra.org/reg/335/2012\">\
     <r1:TransferCharacteristic>urn:smpte:ul:060e2b34.04010101.04010101.01020000</r1:TransferCharacteristic>\
     <r1:ColorPrimaries>urn:smpte:ul:060e2b34.04010106.04010101.03030000</r1:ColorPrimaries>\
     </r0:RGBADescriptor>";

// a sample value nothing else in the clip repeats close by
pub(crate) fn sample(index: usize) -> i32 {
    const SAMPLE_STEP: usize = 37;
    ((index * SAMPLE_STEP) % usize::from(u16::MAX)) as i32
}

// an App 2E IMP with AS-02 picture and sound at 24000/1001, returning its CPL path
pub(crate) fn write_imp(directory: &Path, encrypted_picture: bool) -> PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (_, frames) = write_frames(scratch.path(), "imp");
    let picture_file = "VIDEO.mxf";
    write_as02_picture(&directory.join(picture_file), &frames, encrypted_picture);
    let sound_file = "AUDIO.mxf";
    write_as02_sound(scratch.path(), &directory.join(sound_file));

    let cpl_file = format!("CPL_{CPL_ID}.xml");
    let cpl = ImfCpl {
        uuid: CPL_ID.into(),
        title: TITLE.into(),
        fps_num: EDIT_RATE.0,
        fps_den: EDIT_RATE.1,
        resources: vec![
            ImfResource {
                track_file_uuid: PICTURE_ID.into(),
                duration: FRAMES,
                kind: ImfTrackKind::Image,
                source_encoding: Some(PICTURE_DESCRIPTOR_ID.into()),
            },
            ImfResource {
                track_file_uuid: SOUND_ID.into(),
                duration: FRAMES,
                kind: ImfTrackKind::Audio,
                source_encoding: None,
            },
        ],
        essence_descriptors: vec![ImfEssenceDescriptor {
            id: PICTURE_DESCRIPTOR_ID.into(),
            body: PICTURE_DESCRIPTOR.into(),
        }],
        app2e_edition: App2eEdition::Edition2020,
        ..Default::default()
    };
    std::fs::write(directory.join(&cpl_file), cpl.to_xml()).unwrap();

    let packing_list_file = format!("PKL_{PACKING_LIST_ID}.xml");
    let listed = [
        (CPL_ID, cpl_file.as_str(), XML_TYPE),
        (PICTURE_ID, picture_file, MXF_TYPE),
        (SOUND_ID, sound_file, MXF_TYPE),
    ];
    let packing_list = PackingList {
        uuid: PACKING_LIST_ID.into(),
        namespace: ns::PKL_IMF.into(),
        assets: listed
            .iter()
            .map(|(id, _, asset_type)| PklAsset {
                id: (*id).into(),
                asset_type: (*asset_type).into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    std::fs::write(directory.join(&packing_list_file), packing_list.to_xml()).unwrap();
    let mut assets = vec![AssetMapAsset {
        id: PACKING_LIST_ID.into(),
        path: packing_list_file,
        packing_list: true,
    }];
    assets.extend(listed.iter().map(|(id, path, _)| AssetMapAsset {
        id: (*id).into(),
        path: (*path).into(),
        packing_list: false,
    }));
    let assetmap = AssetMap {
        uuid: ASSETMAP_ID.into(),
        namespace: ns::AM_SMPTE.into(),
        assets,
        ..Default::default()
    };
    std::fs::write(directory.join("ASSETMAP.xml"), assetmap.to_xml()).unwrap();
    directory.join(cpl_file)
}

// asdcplib takes any codestream into AS-02, the player picks its render from the codestream's profile
fn write_as02_picture(path: &Path, frames: &[Vec<u8>], encrypted: bool) {
    let info = WriterInfo {
        cryptographic_key_id: PICTURE_KEY_ID,
        context_id: CRYPTOGRAPHIC_CONTEXT_ID,
        encrypted_essence: encrypted,
        uses_hmac: encrypted,
        label_set: LabelSet::Smpte,
        ..Default::default()
    };
    let edit_rate = Rational::new(EDIT_RATE.0 as i32, EDIT_RATE.1 as i32);
    let descriptor = PictureDescriptor {
        edit_rate,
        sample_rate: edit_rate,
        stored_width: PICTURE_SIZE,
        stored_height: PICTURE_SIZE,
        aspect_ratio: Rational::new(1, 1),
        container_duration: frames.len() as u32,
        codestream: CodestreamHeader::parse(&frames[0]).unwrap(),
    };
    let mut writer = asdcplib::as02::jp2k::MxfWriter::new();
    writer
        .open_write_hdr(
            &path.to_string_lossy(),
            &info,
            &descriptor,
            &rec709_sdr_picture_colour(),
            MXF_HEADER_SIZE,
        )
        .unwrap();
    let mut crypto = encrypted.then(|| {
        let mut encryptor = AesEncContext::new();
        encryptor.init_key(&PICTURE_KEY).unwrap();
        encryptor.set_ivec(&ENCRYPTION_IV).unwrap();
        let mut hmac = HmacContext::new();
        hmac.init_key(&PICTURE_KEY, LabelSet::Smpte).unwrap();
        (encryptor, hmac)
    });
    for frame in frames {
        match crypto.as_mut() {
            Some((encryptor, hmac)) => writer
                .write_frame(frame, Some(encryptor), Some(hmac))
                .unwrap(),
            None => writer.write_frame(frame, None, None).unwrap(),
        }
    }
    writer.finalize().unwrap();
}

fn write_as02_sound(scratch: &Path, output: &Path) {
    let wav = scratch.join("sound.wav");
    let spec = hound::WavSpec {
        channels: SOUND_CHANNELS,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: SOUND_BITS,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
    let samples = FRAMES as usize * SAMPLE_FRAMES_PER_EDIT_UNIT * usize::from(SOUND_CHANNELS);
    for index in 0..samples {
        writer.write_sample(sample(index)).unwrap();
    }
    writer.finalize().unwrap();
    let track = mxf_wrap(&MxfWrapOptions {
        input_files: vec![wav],
        output: output.to_path_buf(),
        essence_type: EssenceType::Pcm,
        standard: MxfStandard::As02,
        fps_num: EDIT_RATE.0,
        fps_den: EDIT_RATE.1,
        partition_size: 0,
        encryption: None,
        mca_config: None,
        resource_ids: Vec::new(),
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(track.success, "AS-02 sound wrap failed: {}", track.error);
}
