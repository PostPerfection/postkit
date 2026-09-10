use postkit::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};
use postkit::restore::{RestoreSelection, TrackKind, restore_package};
use std::path::{Path, PathBuf};

const FRAMES_PER_SECOND: u32 = 24;
const PICTURE_FRAMES: usize = 2;
const SOUND_CHANNELS: u16 = 2;
const SOUND_SAMPLE_RATE: u32 = 48_000;
const SOUND_BITS: u16 = 24;
const SOUND_EDIT_UNITS: usize = 2;
const SAMPLES_PER_EDIT_UNIT: usize = (SOUND_SAMPLE_RATE / FRAMES_PER_SECOND) as usize;
const SOUND_BYTES_PER_SAMPLE: usize = (SOUND_BITS / 8) as usize;
const BLOCK_ALIGN: usize = SOUND_CHANNELS as usize * SOUND_BYTES_PER_SAMPLE;

const PICTURE_STEM: &str = "picture";
const SOUND_STEM: &str = "sound";

const PICTURE_FIXTURE: &str = "imf4k_black_3840x2160.j2c";

// distinct fillers so a frame under the wrong number fails
const FRAME_FILLERS: [usize; PICTURE_FRAMES] = [17, 4096];

fn picture_frame(filler: usize) -> Vec<u8> {
    let mut frame = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(PICTURE_FIXTURE),
    )
    .expect("the IMF fixture codestream");
    frame.extend(std::iter::repeat_n(filler as u8, filler));
    frame
}

fn pcm_body() -> Vec<u8> {
    let bytes = SOUND_EDIT_UNITS * SAMPLES_PER_EDIT_UNIT * BLOCK_ALIGN;
    // a period that does not divide the block align
    (0..bytes).map(|index| (index % 251) as u8).collect()
}

fn wrap(options: MxfWrapOptions) -> String {
    let result = mxf_wrap(&options);
    assert!(result.success, "wrap failed: {}", result.error);
    result.uuid
}

fn wrap_picture(package: &Path, frames: &[Vec<u8>]) -> String {
    let staging = package.join("j2c");
    std::fs::create_dir_all(&staging).unwrap();
    let input_files = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let path = staging.join(format!("frame_{index:08}.j2c"));
            std::fs::write(&path, frame).unwrap();
            path
        })
        .collect();
    let uuid = wrap(MxfWrapOptions {
        input_files,
        output: package.join(format!("{PICTURE_STEM}.mxf")),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::As02,
        fps_num: FRAMES_PER_SECOND,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: Some(postkit::mxf_wrap::rec709_sdr_picture_colour()),
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    std::fs::remove_dir_all(&staging).unwrap();
    uuid
}

fn wrap_sound(package: &Path, pcm: &[u8]) -> String {
    let master = package.join("master.wav");
    let spec = hound::WavSpec {
        channels: SOUND_CHANNELS,
        sample_rate: SOUND_SAMPLE_RATE,
        bits_per_sample: SOUND_BITS,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&master, spec).unwrap();
    for sample in pcm.as_chunks::<SOUND_BYTES_PER_SAMPLE>().0 {
        writer
            .write_sample(i32::from_le_bytes([0, sample[0], sample[1], sample[2]]) >> 8)
            .unwrap();
    }
    writer.finalize().unwrap();
    let uuid = wrap(MxfWrapOptions {
        input_files: vec![master.clone()],
        output: package.join(format!("{SOUND_STEM}.mxf")),
        essence_type: EssenceType::Pcm,
        standard: MxfStandard::As02,
        fps_num: FRAMES_PER_SECOND,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: None,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    std::fs::remove_file(&master).unwrap();
    uuid
}

fn write_package_documents(package: &Path, picture_id: &str, sound_id: &str) {
    std::fs::write(
        package.join("ASSETMAP.xml"),
        format!(
            r#"<AssetMap><AssetList>
  <Asset><Id>urn:uuid:{picture_id}</Id><ChunkList><Chunk><Path>{PICTURE_STEM}.mxf</Path></Chunk></ChunkList></Asset>
  <Asset><Id>urn:uuid:{sound_id}</Id><ChunkList><Chunk><Path>{SOUND_STEM}.mxf</Path></Chunk></ChunkList></Asset>
  <Asset><Id>urn:uuid:0c910000-0000-0000-0000-000000000000</Id><ChunkList><Chunk><Path>CPL.xml</Path></Chunk></ChunkList></Asset>
</AssetList></AssetMap>"#
        ),
    )
    .unwrap();
    std::fs::write(
        package.join("CPL.xml"),
        format!(
            r#"<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/2067-3/2016">
  <SegmentList><Segment><SequenceList>
    <MainImageSequence><ResourceList>
      <Resource><TrackFileId>urn:uuid:{picture_id}</TrackFileId></Resource>
    </ResourceList></MainImageSequence>
    <MainAudioSequence><ResourceList>
      <Resource><TrackFileId>urn:uuid:{sound_id}</TrackFileId></Resource>
    </ResourceList></MainAudioSequence>
  </SequenceList></Segment></SegmentList>
</CompositionPlaylist>"#
        ),
    )
    .unwrap();
}

struct Package {
    _directory: tempfile::TempDir,
    path: PathBuf,
    frames: Vec<Vec<u8>>,
    pcm: Vec<u8>,
}

fn build_package() -> Package {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("imp");
    std::fs::create_dir_all(&path).unwrap();
    let frames: Vec<Vec<u8>> = FRAME_FILLERS.iter().map(|&f| picture_frame(f)).collect();
    let pcm = pcm_body();
    let picture_id = wrap_picture(&path, &frames);
    let sound_id = wrap_sound(&path, &pcm);
    write_package_documents(&path, &picture_id, &sound_id);
    Package {
        _directory: directory,
        path,
        frames,
        pcm,
    }
}

fn files_with_extension(directory: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|found| found.eq_ignore_ascii_case(extension))
        })
        .collect();
    found.sort();
    found
}

// everything after the WAV's `data` chunk header, which is the PCM body itself
fn wav_body(wav: &Path) -> Vec<u8> {
    const DATA_CHUNK_HEADER_BYTES: usize = 8;
    let bytes = std::fs::read(wav).unwrap();
    let data = bytes
        .windows(4)
        .position(|window| window == b"data")
        .expect("a data chunk");
    bytes[data + DATA_CHUNK_HEADER_BYTES..].to_vec()
}

#[test]
fn every_picture_frame_comes_back_as_the_codestream_that_went_in() {
    let package = build_package();
    let output = package.path.parent().unwrap().join("restored-picture");

    let report =
        restore_package(&package.path, &output, RestoreSelection::All, None).expect("the restore");

    let picture = report
        .restored
        .iter()
        .find(|track| track.kind == TrackKind::Picture)
        .expect("a picture track in the report");
    assert_eq!(picture.written, PICTURE_FRAMES as u64);
    assert_eq!(picture.output, output.join(PICTURE_STEM));

    let codestreams = files_with_extension(&picture.output, "j2c");
    assert_eq!(
        codestreams,
        vec![
            picture.output.join("000000.j2c"),
            picture.output.join("000001.j2c"),
        ]
    );
    for (path, expected) in codestreams.iter().zip(&package.frames) {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            expected,
            "{} is not the codestream that was wrapped",
            path.display()
        );
    }
}

#[test]
fn the_sound_track_comes_back_as_a_wav_carrying_the_samples_that_went_in() {
    let package = build_package();
    let output = package.path.parent().unwrap().join("restored-sound");

    let report =
        restore_package(&package.path, &output, RestoreSelection::All, None).expect("the restore");

    let sound = report
        .restored
        .iter()
        .find(|track| track.kind == TrackKind::Sound)
        .expect("a sound track in the report");
    assert_eq!(
        sound.written,
        (SOUND_EDIT_UNITS * SAMPLES_PER_EDIT_UNIT) as u64
    );

    let wav = sound.output.join(format!("{SOUND_STEM}.wav"));
    let reader = hound::WavReader::open(&wav).expect("the restored wav");
    assert_eq!(reader.spec().channels, SOUND_CHANNELS);
    assert_eq!(reader.spec().sample_rate, SOUND_SAMPLE_RATE);
    assert_eq!(reader.spec().bits_per_sample, SOUND_BITS);
    assert_eq!(
        reader.duration() as usize,
        SOUND_EDIT_UNITS * SAMPLES_PER_EDIT_UNIT
    );
    assert_eq!(wav_body(&wav), package.pcm);
}

#[test]
fn picture_only_writes_no_wav_and_sound_only_writes_no_codestream() {
    let package = build_package();
    let root = package.path.parent().unwrap();

    let picture_output = root.join("picture-only");
    let report = restore_package(
        &package.path,
        &picture_output,
        RestoreSelection::PictureOnly,
        None,
    )
    .expect("the picture-only restore");
    assert_eq!(report.restored.len(), 1);
    assert_eq!(report.restored[0].kind, TrackKind::Picture);
    assert!(!picture_output.join(SOUND_STEM).exists());

    let sound_output = root.join("sound-only");
    let report = restore_package(
        &package.path,
        &sound_output,
        RestoreSelection::SoundOnly,
        None,
    )
    .expect("the sound-only restore");
    assert_eq!(report.restored.len(), 1);
    assert_eq!(report.restored[0].kind, TrackKind::Sound);
    assert!(!sound_output.join(PICTURE_STEM).exists());
}

#[test]
fn a_track_file_no_composition_names_is_left_alone() {
    let package = build_package();
    let stray = package.path.join("stray.mxf");
    std::fs::copy(package.path.join(format!("{SOUND_STEM}.mxf")), &stray).unwrap();
    let output = package.path.parent().unwrap().join("restored-named-only");

    let report =
        restore_package(&package.path, &output, RestoreSelection::All, None).expect("the restore");

    assert_eq!(report.restored.len(), 2);
    assert!(report.skipped.is_empty());
    assert!(!output.join("stray").exists());
}
