use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Essence type for MXF wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EssenceType {
    /// JPEG 2000 picture essence
    J2k,
    /// PCM audio essence
    Pcm,
    /// Timed text (subtitle) essence
    TimedText,
    /// Dolby Atmos (IAB) essence
    Atmos,
}

/// MXF standard variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MxfStandard {
    /// SMPTE ST 429 (DCP)
    AsDcp,
    /// SMPTE ST 2067 (IMF)
    As02,
}

/// Options for MXF wrapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MxfWrapOptions {
    /// Input essence files (J2K codestreams or WAV files)
    pub input_files: Vec<PathBuf>,
    /// Output MXF file path
    pub output: PathBuf,
    /// Essence type
    pub essence_type: EssenceType,
    /// MXF standard
    pub standard: MxfStandard,
    /// Frame rate numerator
    pub fps_num: u32,
    /// Frame rate denominator
    pub fps_den: u32,
    /// Edit rate (frames per partition) for AS-02
    pub partition_size: u32,
}

/// Result of MXF wrapping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MxfTrackFile {
    /// Generated UUID for this track file
    pub uuid: String,
    /// SHA-1 hash of the output MXF
    pub hash: String,
    /// Output file size in bytes
    pub size: u64,
    /// Duration in frames
    pub duration: u64,
    /// Output path
    pub path: PathBuf,
    pub success: bool,
    pub error: String,
}

/// Wrap essence into MXF using asdcplib FFI.
pub fn mxf_wrap(opts: &MxfWrapOptions) -> MxfTrackFile {
    match opts.essence_type {
        EssenceType::J2k => wrap_j2k(opts),
        EssenceType::Pcm => wrap_pcm(opts),
        EssenceType::TimedText => wrap_timed_text(opts),
        EssenceType::Atmos => wrap_atmos(opts),
    }
}

/// Dispatches JP2K frame writes to the AS-DCP or AS-02 (frame-wrapped) writer.
enum J2kWriter {
    AsDcp(asdcplib::jp2k::MxfWriter),
    As02(asdcplib::as02::jp2k::MxfWriter),
}

impl J2kWriter {
    fn new(standard: MxfStandard) -> Self {
        match standard {
            MxfStandard::AsDcp => Self::AsDcp(asdcplib::jp2k::MxfWriter::new()),
            MxfStandard::As02 => Self::As02(asdcplib::as02::jp2k::MxfWriter::new()),
        }
    }

    fn open_write(
        &mut self,
        filename: &str,
        info: &asdcplib::WriterInfo,
        desc: &asdcplib::jp2k::PictureDescriptor,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.open_write(filename, info, desc, header_size),
            Self::As02(w) => w.open_write(filename, info, desc, header_size),
        }
    }

    fn write_frame(&mut self, data: &[u8]) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.write_frame(data, None, None),
            Self::As02(w) => w.write_frame(data, None, None),
        }
    }

    fn finalize(&mut self) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.finalize(),
            Self::As02(w) => w.finalize(),
        }
    }
}

/// Dispatches PCM writes to the AS-DCP or AS-02 (clip-wrapped) writer.
enum PcmWriter {
    AsDcp(asdcplib::pcm::MxfWriter),
    As02(asdcplib::as02::pcm::MxfWriter),
}

impl PcmWriter {
    fn new(standard: MxfStandard) -> Self {
        match standard {
            MxfStandard::AsDcp => Self::AsDcp(asdcplib::pcm::MxfWriter::new()),
            MxfStandard::As02 => Self::As02(asdcplib::as02::pcm::MxfWriter::new()),
        }
    }

    fn open_write(
        &mut self,
        filename: &str,
        info: &asdcplib::WriterInfo,
        desc: &asdcplib::pcm::AudioDescriptor,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.open_write(filename, info, desc, header_size),
            Self::As02(w) => w.open_write(filename, info, desc, header_size),
        }
    }

    fn write_frame(&mut self, data: &[u8]) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.write_frame(data, None, None),
            Self::As02(w) => w.write_frame(data, None, None),
        }
    }

    fn finalize(&mut self) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.finalize(),
            Self::As02(w) => w.finalize(),
        }
    }
}

/// Dispatches timed text writes to the AS-DCP or AS-02 writer.
enum TimedTextWriter {
    AsDcp(asdcplib::timed_text::MxfWriter),
    As02(asdcplib::as02::timed_text::MxfWriter),
}

impl TimedTextWriter {
    fn new(standard: MxfStandard) -> Self {
        match standard {
            MxfStandard::AsDcp => Self::AsDcp(asdcplib::timed_text::MxfWriter::new()),
            MxfStandard::As02 => Self::As02(asdcplib::as02::timed_text::MxfWriter::new()),
        }
    }

    fn open_write(
        &mut self,
        filename: &str,
        info: &asdcplib::WriterInfo,
        desc: &asdcplib::timed_text::TimedTextDescriptor,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.open_write(filename, info, desc, header_size),
            Self::As02(w) => w.open_write(filename, info, desc, header_size),
        }
    }

    fn write_timed_text_resource(&mut self, xml: &str) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.write_timed_text_resource(xml, None, None),
            Self::As02(w) => w.write_timed_text_resource(xml, None, None),
        }
    }

    fn write_ancillary_resource(
        &mut self,
        data: &[u8],
        uuid: &[u8; 16],
        mime_type: &str,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.write_ancillary_resource(data, uuid, mime_type, None, None),
            Self::As02(w) => w.write_ancillary_resource(data, uuid, mime_type, None, None),
        }
    }

    fn finalize(&mut self) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.finalize(),
            Self::As02(w) => w.finalize(),
        }
    }
}

fn make_writer_info() -> asdcplib::WriterInfo {
    let asset_uuid = uuid::Uuid::new_v4();
    let context_id = uuid::Uuid::new_v4();
    asdcplib::WriterInfo {
        asset_uuid: *asset_uuid.as_bytes(),
        context_id: *context_id.as_bytes(),
        label_set: asdcplib::LabelSet::Smpte,
        ..Default::default()
    }
}

fn compute_hash_and_size(path: &std::path::Path) -> (String, u64) {
    use sha1::Digest;
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return (String::new(), 0),
    };
    let hash = sha1::Sha1::digest(&data);
    (
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        data.len() as u64,
    )
}

fn wrap_j2k(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    // Read all J2K frames
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for f in &opts.input_files {
        match std::fs::read(f) {
            Ok(data) => frames.push(data),
            Err(e) => {
                return MxfTrackFile {
                    error: format!("failed to read {}: {e}", f.display()),
                    ..Default::default()
                };
            }
        }
    }

    let Some(header) = crate::j2k::parse_j2k_header(&frames[0]) else {
        return MxfTrackFile {
            error: format!(
                "invalid JPEG 2000 codestream: {}",
                opts.input_files[0].display()
            ),
            ..Default::default()
        };
    };
    if header.width == 0 || header.height == 0 {
        return MxfTrackFile {
            error: format!(
                "JPEG 2000 codestream has no image area: {}",
                opts.input_files[0].display()
            ),
            ..Default::default()
        };
    }

    let info = make_writer_info();
    let desc = asdcplib::jp2k::PictureDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        sample_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        stored_width: header.width,
        stored_height: header.height,
        aspect_ratio: asdcplib::Rational::new(header.width as i32, header.height as i32),
        container_duration: frames.len() as u32,
        component_count: header.num_components,
    };

    let mut writer = J2kWriter::new(opts.standard);
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, 16384) {
        return MxfTrackFile {
            error: format!("JP2K open_write failed: {e}"),
            ..Default::default()
        };
    }

    for frame in &frames {
        if let Err(e) = writer.write_frame(frame) {
            return MxfTrackFile {
                error: format!("JP2K write_frame failed: {e}"),
                ..Default::default()
            };
        }
    }

    if let Err(e) = writer.finalize() {
        return MxfTrackFile {
            error: format!("JP2K finalize failed: {e}"),
            ..Default::default()
        };
    }

    let (hash, size) = compute_hash_and_size(&opts.output);
    let uuid_str = uuid::Uuid::from_bytes(info.asset_uuid)
        .hyphenated()
        .to_string();

    MxfTrackFile {
        uuid: uuid_str,
        hash,
        size,
        duration: frames.len() as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}

fn wrap_pcm(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    // Read WAV data — for simplicity, read the entire file as raw PCM
    // A real implementation would parse WAV headers
    let wav_data = match std::fs::read(&opts.input_files[0]) {
        Ok(d) => d,
        Err(e) => {
            return MxfTrackFile {
                error: format!("failed to read WAV: {e}"),
                ..Default::default()
            };
        }
    };

    let info = make_writer_info();
    let channels = 6u32; // default 5.1
    let bits = 24u32;
    let sample_rate = 48000u32;
    let block_align = (bits / 8) * channels;
    let samples_per_frame =
        (sample_rate as f64 / (opts.fps_num as f64 / opts.fps_den as f64)).ceil() as u32;
    let frame_size = samples_per_frame * block_align;

    // Skip WAV header (44 bytes for standard WAV)
    let pcm_start = if wav_data.len() > 44 { 44 } else { 0 };
    let pcm_data = &wav_data[pcm_start..];
    let num_frames = (pcm_data.len() as u32).checked_div(frame_size).unwrap_or(0);

    let desc = asdcplib::pcm::AudioDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        audio_sampling_rate: asdcplib::Rational::new(sample_rate as i32, 1),
        locked: true,
        channel_count: channels,
        quantization_bits: bits,
        block_align,
        avg_bps: sample_rate * block_align,
        linked_track_id: 0,
        container_duration: num_frames,
        channel_format: asdcplib::pcm::ChannelFormat::Cfg1,
    };

    let mut writer = PcmWriter::new(opts.standard);
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, 16384) {
        return MxfTrackFile {
            error: format!("PCM open_write failed: {e}"),
            ..Default::default()
        };
    }

    for i in 0..num_frames {
        let start = (i * frame_size) as usize;
        let end = start + frame_size as usize;
        if end > pcm_data.len() {
            break;
        }
        if let Err(e) = writer.write_frame(&pcm_data[start..end]) {
            return MxfTrackFile {
                error: format!("PCM write_frame failed: {e}"),
                ..Default::default()
            };
        }
    }

    if let Err(e) = writer.finalize() {
        return MxfTrackFile {
            error: format!("PCM finalize failed: {e}"),
            ..Default::default()
        };
    }

    let (hash, size) = compute_hash_and_size(&opts.output);
    let uuid_str = uuid::Uuid::from_bytes(info.asset_uuid)
        .hyphenated()
        .to_string();

    MxfTrackFile {
        uuid: uuid_str,
        hash,
        size,
        duration: num_frames as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}

fn wrap_timed_text(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    let xml_data = match std::fs::read_to_string(&opts.input_files[0]) {
        Ok(d) => d,
        Err(e) => {
            return MxfTrackFile {
                error: format!("failed to read XML: {e}"),
                ..Default::default()
            };
        }
    };

    let fps = opts.fps_num as f64 / opts.fps_den.max(1) as f64;
    let Some(end_secs) = crate::subtitle_retime::subtitle_end_time_seconds(&xml_data, fps) else {
        return MxfTrackFile {
            error: format!(
                "cannot determine subtitle duration: no parsable end/TimeOut timing in {}",
                opts.input_files[0].display()
            ),
            ..Default::default()
        };
    };
    let duration_frames = (end_secs * fps).ceil() as u32;

    let info = make_writer_info();
    let desc = asdcplib::timed_text::TimedTextDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        container_duration: duration_frames,
        asset_id: info.asset_uuid,
    };

    let mut writer = TimedTextWriter::new(opts.standard);
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, 16384) {
        return MxfTrackFile {
            error: format!("TimedText open_write failed: {e}"),
            ..Default::default()
        };
    }

    if let Err(e) = writer.write_timed_text_resource(&xml_data) {
        return MxfTrackFile {
            error: format!("TimedText write_resource failed: {e}"),
            ..Default::default()
        };
    }

    // Write ancillary resources (fonts, images) — remaining input files
    for f in opts.input_files.iter().skip(1) {
        let resource_data = match std::fs::read(f) {
            Ok(d) => d,
            Err(e) => {
                return MxfTrackFile {
                    error: format!("failed to read resource {}: {e}", f.display()),
                    ..Default::default()
                };
            }
        };
        let resource_uuid = *uuid::Uuid::new_v4().as_bytes();
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let mime = match ext.as_str() {
            "ttf" | "otf" => "application/x-font-opentype",
            "png" => "image/png",
            _ => "application/octet-stream",
        };
        if let Err(e) = writer.write_ancillary_resource(&resource_data, &resource_uuid, mime) {
            return MxfTrackFile {
                error: format!("TimedText write_ancillary failed: {e}"),
                ..Default::default()
            };
        }
    }

    if let Err(e) = writer.finalize() {
        return MxfTrackFile {
            error: format!("TimedText finalize failed: {e}"),
            ..Default::default()
        };
    }

    let (hash, size) = compute_hash_and_size(&opts.output);
    let uuid_str = uuid::Uuid::from_bytes(info.asset_uuid)
        .hyphenated()
        .to_string();

    MxfTrackFile {
        uuid: uuid_str,
        hash,
        size,
        duration: desc.container_duration as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}

fn wrap_atmos(opts: &MxfWrapOptions) -> MxfTrackFile {
    // asdcplib exposes AS-02 IAB as detection-only, no writer exists.
    if opts.standard == MxfStandard::As02 {
        return MxfTrackFile {
            error: "AS-02 (IMF) Atmos/IAB wrapping is not supported; asdcplib provides AS-02 writers only for J2K, PCM, and TimedText".to_string(),
            ..Default::default()
        };
    }
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    // Read all Atmos frames
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for f in &opts.input_files {
        match std::fs::read(f) {
            Ok(data) => frames.push(data),
            Err(e) => {
                return MxfTrackFile {
                    error: format!("failed to read {}: {e}", f.display()),
                    ..Default::default()
                };
            }
        }
    }

    let info = make_writer_info();
    let desc = asdcplib::atmos::AtmosDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        container_duration: frames.len() as u32,
        asset_id: info.asset_uuid,
        data_essence_coding: [0; 16],
        first_frame: 0,
        max_channel_count: 128,
        max_object_count: 118,
        atmos_id: *uuid::Uuid::new_v4().as_bytes(),
        atmos_version: 1,
    };

    let mut writer = asdcplib::atmos::MxfWriter::new();
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, 16384) {
        return MxfTrackFile {
            error: format!("Atmos open_write failed: {e}"),
            ..Default::default()
        };
    }

    for frame in &frames {
        if let Err(e) = writer.write_frame(frame, None, None) {
            return MxfTrackFile {
                error: format!("Atmos write_frame failed: {e}"),
                ..Default::default()
            };
        }
    }

    if let Err(e) = writer.finalize() {
        return MxfTrackFile {
            error: format!("Atmos finalize failed: {e}"),
            ..Default::default()
        };
    }

    let (hash, size) = compute_hash_and_size(&opts.output);
    let uuid_str = uuid::Uuid::from_bytes(info.asset_uuid)
        .hyphenated()
        .to_string();

    MxfTrackFile {
        uuid: uuid_str,
        hash,
        size,
        duration: frames.len() as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}
