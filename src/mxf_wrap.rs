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

/// AES-128 essence encryption material for a single MXF.
///
/// The 16-byte content key encrypts the essence at wrap time; the MXF header
/// only records `key_id`. Kept out of any serialized form and redacted in Debug
/// so the key cannot leak through logs or an on-disk options blob.
#[derive(Clone)]
pub struct MxfEncryption {
    pub content_key: [u8; 16],
    pub key_id: [u8; 16],
}

impl std::fmt::Debug for MxfEncryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MxfEncryption")
            .field("content_key", &"<redacted>")
            .field("key_id", &uuid::Uuid::from_bytes(self.key_id))
            .finish()
    }
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
    /// When set, the essence is AES-128 encrypted at wrap time (J2K/PCM only).
    /// Never serialized: it carries secret key material.
    #[serde(skip)]
    pub encryption: Option<MxfEncryption>,
    /// SMPTE 377-4 MCA label config for PCM, asdcp-wrap style, e.g.
    /// `"51(L,R,C,LFE,Ls,Rs),HI,VIN"`. AS-DCP (DCP) only. Build one from a
    /// soundfield with [`crate::mca::soundfield_to_mca_config`].
    #[serde(default)]
    pub mca_config: Option<String>,
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

    fn write_frame(&mut self, data: &[u8], enc: &mut EssenceCrypto) -> asdcplib::Result<()> {
        let (e, h) = enc.contexts();
        match self {
            Self::AsDcp(w) => w.write_frame(data, e, h),
            Self::As02(w) => w.write_frame(data, e, h),
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

    /// Open the writer, attaching MCA labels when `mca_config` is set (AS-DCP only;
    /// the AS-02 path here never carries MCA labels).
    fn open_write(
        &mut self,
        filename: &str,
        info: &asdcplib::WriterInfo,
        desc: &asdcplib::pcm::AudioDescriptor,
        mca_config: Option<&str>,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => match mca_config {
                Some(m) => w.open_write_mca(filename, info, desc, m, header_size),
                None => w.open_write(filename, info, desc, header_size),
            },
            Self::As02(w) => w.open_write(filename, info, desc, header_size),
        }
    }

    fn write_frame(&mut self, data: &[u8], enc: &mut EssenceCrypto) -> asdcplib::Result<()> {
        let (e, h) = enc.contexts();
        match self {
            Self::AsDcp(w) => w.write_frame(data, e, h),
            Self::As02(w) => w.write_frame(data, e, h),
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

/// AES/HMAC contexts for one wrap, or empty for cleartext essence.
struct EssenceCrypto {
    enc: Option<asdcplib::crypto::AesEncContext>,
    hmac: Option<asdcplib::crypto::HmacContext>,
}

impl EssenceCrypto {
    fn none() -> Self {
        Self {
            enc: None,
            hmac: None,
        }
    }

    #[allow(clippy::type_complexity)]
    fn contexts(
        &mut self,
    ) -> (
        Option<&mut asdcplib::crypto::AesEncContext>,
        Option<&mut asdcplib::crypto::HmacContext>,
    ) {
        (self.enc.as_mut(), self.hmac.as_mut())
    }
}

/// Flag `info` as encrypted and build the AES/HMAC contexts for the essence.
///
/// Sets the SMPTE key-id and HMAC-required flags so the MXF header carries a
/// cryptographic context; asdcplib generates a fresh per-frame IV internally.
fn setup_encryption(
    info: &mut asdcplib::WriterInfo,
    enc: &Option<MxfEncryption>,
) -> Result<EssenceCrypto, String> {
    let Some(e) = enc else {
        return Ok(EssenceCrypto::none());
    };
    info.encrypted_essence = true;
    info.uses_hmac = true;
    info.cryptographic_key_id = e.key_id;
    let mut ec = asdcplib::crypto::AesEncContext::new();
    ec.init_key(&e.content_key)
        .map_err(|err| format!("AES key init failed: {err}"))?;
    let mut hc = asdcplib::crypto::HmacContext::new();
    hc.init_key(&e.content_key, info.label_set)
        .map_err(|err| format!("HMAC key init failed: {err}"))?;
    Ok(EssenceCrypto {
        enc: Some(ec),
        hmac: Some(hc),
    })
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

    let mut info = make_writer_info();
    let mut crypto = match setup_encryption(&mut info, &opts.encryption) {
        Ok(c) => c,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };
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
        if let Err(e) = writer.write_frame(frame, &mut crypto) {
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

/// The audio parameters and PCM payload location parsed from a WAV file.
#[derive(Debug)]
struct WavFormat {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    /// Byte offset and length of the `data` chunk payload.
    data_offset: usize,
    data_len: usize,
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

fn le_u16(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn le_u32(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

/// Parse a RIFF/WAVE header: read the `fmt ` chunk and locate the `data` chunk.
///
/// Only linear PCM is accepted (tag 1, or WAVE_FORMAT_EXTENSIBLE whose subformat
/// is PCM). Anything malformed or non-PCM is an error rather than a wrong MXF.
fn parse_wav(data: &[u8]) -> Result<WavFormat, String> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (tag, channels, rate, bits)
    let mut data_chunk: Option<(usize, usize)> = None;

    // Chunks start after the 12-byte RIFF/WAVE header; each is an 8-byte header
    // (4-byte id + 4-byte LE size) followed by size bytes, padded to even.
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = le_u32(data, pos + 4) as usize;
        let body = pos + 8;
        if body + size > data.len() {
            return Err(format!(
                "chunk '{}' claims {size} bytes past end of file",
                String::from_utf8_lossy(id)
            ));
        }

        if id == b"fmt " {
            if size < 16 {
                return Err("fmt chunk is too short".into());
            }
            let mut tag = le_u16(data, body);
            let channels = le_u16(data, body + 2);
            let sample_rate = le_u32(data, body + 4);
            let bits = le_u16(data, body + 14);
            // WAVE_FORMAT_EXTENSIBLE stores the real tag in the SubFormat GUID.
            if tag == WAVE_FORMAT_EXTENSIBLE {
                if size < 40 {
                    return Err("extensible fmt chunk is too short for a SubFormat".into());
                }
                tag = le_u16(data, body + 24);
            }
            fmt = Some((tag, channels, sample_rate, bits));
        } else if id == b"data" {
            data_chunk = Some((body, size));
        }

        pos = body + size + (size & 1);
    }

    let (tag, channels, sample_rate, bits) = fmt.ok_or("no fmt chunk")?;
    if tag != WAVE_FORMAT_PCM {
        return Err(format!("audio format {tag:#06x} is not linear PCM"));
    }
    if channels == 0 || sample_rate == 0 || bits == 0 || bits % 8 != 0 {
        return Err(format!(
            "unusable PCM parameters: {channels} channels, {sample_rate} Hz, {bits} bits"
        ));
    }
    let (data_offset, data_len) = data_chunk.ok_or("no data chunk")?;

    Ok(WavFormat {
        channels,
        sample_rate,
        bits_per_sample: bits,
        data_offset,
        data_len,
    })
}

/// Map a channel count to a SMPTE channel configuration where one applies; other
/// counts get no configuration label (the caller can add MCA labels).
fn channel_format_for(channels: u32) -> asdcplib::pcm::ChannelFormat {
    match channels {
        6 => asdcplib::pcm::ChannelFormat::Cfg1, // 5.1
        _ => asdcplib::pcm::ChannelFormat::None,
    }
}

fn wrap_pcm(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    if opts.mca_config.is_some() && opts.standard == MxfStandard::As02 {
        return MxfTrackFile {
            error: "MCA labels are only supported on the AS-DCP (DCP) PCM path".to_string(),
            ..Default::default()
        };
    }

    let wav_data = match std::fs::read(&opts.input_files[0]) {
        Ok(d) => d,
        Err(e) => {
            return MxfTrackFile {
                error: format!("failed to read WAV: {e}"),
                ..Default::default()
            };
        }
    };

    // Parse the real RIFF/WAVE header instead of assuming 5.1/24-bit/48k.
    let wav = match parse_wav(&wav_data) {
        Ok(w) => w,
        Err(e) => {
            return MxfTrackFile {
                error: format!("invalid WAV {}: {e}", opts.input_files[0].display()),
                ..Default::default()
            };
        }
    };

    let mut info = make_writer_info();
    let channels = wav.channels as u32;
    let bits = wav.bits_per_sample as u32;
    let sample_rate = wav.sample_rate;
    let block_align = (bits / 8) * channels;
    let samples_per_frame =
        (sample_rate as f64 / (opts.fps_num as f64 / opts.fps_den as f64)).ceil() as u32;
    let frame_size = samples_per_frame * block_align;

    let pcm_data = &wav_data[wav.data_offset..wav.data_offset + wav.data_len];
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
        channel_format: channel_format_for(channels),
    };

    let mut crypto = match setup_encryption(&mut info, &opts.encryption) {
        Ok(c) => c,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };

    let mut writer = PcmWriter::new(opts.standard);
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, opts.mca_config.as_deref(), 16384)
    {
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
        if let Err(e) = writer.write_frame(&pcm_data[start..end], &mut crypto) {
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

/// Options for stereoscopic 3D (ST 429-10) JP2K MXF wrapping.
///
/// Left and right eye codestreams are wrapped into one stereoscopic picture MXF.
/// Stereo is AS-DCP only, so there is no `standard` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StereoscopicWrapOptions {
    /// Left-eye J2K codestream files, one per frame.
    pub left_files: Vec<PathBuf>,
    /// Right-eye J2K codestream files, one per frame. Must match `left_files` length.
    pub right_files: Vec<PathBuf>,
    /// Output MXF file path.
    pub output: PathBuf,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator.
    pub fps_den: u32,
    /// When set, the essence is AES-128 encrypted at wrap time.
    /// Never serialized: it carries secret key material.
    #[serde(skip)]
    pub encryption: Option<MxfEncryption>,
}

/// Wrap left/right eye J2K frame sequences into one stereoscopic picture MXF.
pub fn wrap_stereoscopic(opts: &StereoscopicWrapOptions) -> MxfTrackFile {
    use asdcplib::jp2k::StereoscopicPhase;

    if opts.left_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }
    if opts.left_files.len() != opts.right_files.len() {
        return MxfTrackFile {
            error: format!(
                "left/right frame counts differ: {} left, {} right",
                opts.left_files.len(),
                opts.right_files.len()
            ),
            ..Default::default()
        };
    }

    // Read all left and right frames.
    let read_all = |files: &[PathBuf]| -> Result<Vec<Vec<u8>>, String> {
        files
            .iter()
            .map(|f| std::fs::read(f).map_err(|e| format!("failed to read {}: {e}", f.display())))
            .collect()
    };
    let left = match read_all(&opts.left_files) {
        Ok(f) => f,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };
    let right = match read_all(&opts.right_files) {
        Ok(f) => f,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };

    let Some(header) = crate::j2k::parse_j2k_header(&left[0]) else {
        return MxfTrackFile {
            error: format!(
                "invalid JPEG 2000 codestream: {}",
                opts.left_files[0].display()
            ),
            ..Default::default()
        };
    };
    if header.width == 0 || header.height == 0 {
        return MxfTrackFile {
            error: format!(
                "JPEG 2000 codestream has no image area: {}",
                opts.left_files[0].display()
            ),
            ..Default::default()
        };
    }

    let mut info = make_writer_info();
    let mut crypto = match setup_encryption(&mut info, &opts.encryption) {
        Ok(c) => c,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };
    // container_duration counts stereo frame pairs, not individual eye writes.
    let desc = asdcplib::jp2k::PictureDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        sample_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        stored_width: header.width,
        stored_height: header.height,
        aspect_ratio: asdcplib::Rational::new(header.width as i32, header.height as i32),
        container_duration: left.len() as u32,
        component_count: header.num_components,
    };

    let mut writer = asdcplib::jp2k::StereoMxfWriter::new();
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, 16384) {
        return MxfTrackFile {
            error: format!("stereoscopic open_write failed: {e}"),
            ..Default::default()
        };
    }

    for (l, r) in left.iter().zip(right.iter()) {
        let (e, h) = crypto.contexts();
        if let Err(err) = writer.write_frame(l, StereoscopicPhase::Left, e, h) {
            return MxfTrackFile {
                error: format!("stereoscopic write_frame (left) failed: {err}"),
                ..Default::default()
            };
        }
        let (e, h) = crypto.contexts();
        if let Err(err) = writer.write_frame(r, StereoscopicPhase::Right, e, h) {
            return MxfTrackFile {
                error: format!("stereoscopic write_frame (right) failed: {err}"),
                ..Default::default()
            };
        }
    }

    if let Err(e) = writer.finalize() {
        return MxfTrackFile {
            error: format!("stereoscopic finalize failed: {e}"),
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
        duration: left.len() as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PCM WAV (fmt + data chunks) with the given parameters.
    fn make_wav(channels: u16, sample_rate: u32, bits: u16, sample_frames: u32) -> Vec<u8> {
        let block_align = (bits / 8) as u32 * channels as u32;
        let data_len = block_align * sample_frames;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&(sample_rate * block_align).to_le_bytes()); // byte rate
        w.extend_from_slice(&(block_align as u16).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        w.resize(w.len() + data_len as usize, 0);
        w
    }

    #[test]
    fn parse_wav_reads_non_default_params() {
        let wav = make_wav(2, 44100, 16, 100);
        let f = parse_wav(&wav).expect("parse");
        assert_eq!(f.channels, 2);
        assert_eq!(f.sample_rate, 44100);
        assert_eq!(f.bits_per_sample, 16);
        assert_eq!(f.data_len, 2 * 2 * 100);
        assert_eq!(&wav[f.data_offset..f.data_offset + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn parse_wav_rejects_malformed_and_non_pcm() {
        assert!(parse_wav(b"not a wav at all").is_err());

        // Float (tag 3) is not linear PCM.
        let mut wav = make_wav(2, 48000, 32, 10);
        wav[20..22].copy_from_slice(&3u16.to_le_bytes());
        let err = parse_wav(&wav).expect_err("float must be rejected");
        assert!(err.contains("not linear PCM"), "got: {err}");
    }

    #[test]
    fn wrap_pcm_descriptor_reflects_the_input_wav() {
        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("in.wav");
        // 2ch / 96000 / 16-bit, one second: all non-default vs the old 5.1/24/48k
        // (44100 is exercised by parse_wav; asdcplib only wraps 48k/96k for DCP).
        std::fs::write(&wav_path, make_wav(2, 96000, 16, 96000)).unwrap();
        let out = dir.path().join("out.mxf");

        let opts = MxfWrapOptions {
            input_files: vec![wav_path],
            output: out.clone(),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
        };
        let result = wrap_pcm(&opts);
        assert!(result.success, "wrap failed: {}", result.error);

        let mut reader = asdcplib::pcm::MxfReader::new();
        reader
            .open_read(&out.to_string_lossy())
            .expect("open the wrapped MXF");
        let desc = reader.audio_descriptor().expect("read audio descriptor");
        assert_eq!(desc.channel_count, 2, "channel count");
        assert_eq!(desc.audio_sampling_rate.numerator, 96000, "sample rate");
        assert_eq!(desc.quantization_bits, 16, "bit depth");
        assert_eq!(desc.block_align, 4, "block align = 2ch * 16-bit");
    }

    #[test]
    fn wrap_pcm_errors_on_a_non_wav_input() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("bogus.wav");
        std::fs::write(&bogus, b"this is not a wav file").unwrap();
        let opts = MxfWrapOptions {
            input_files: vec![bogus],
            output: dir.path().join("out.mxf"),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
        };
        let result = wrap_pcm(&opts);
        assert!(!result.success, "must not wrap a non-WAV file");
        assert!(
            result.error.contains("invalid WAV"),
            "got: {}",
            result.error
        );
    }

    /// Minimal JPEG 2000 codestream `parse_j2k_header` accepts and asdcplib will
    /// wrap: SOC, a well-formed SIZ (2048x1080, 3 components), then SOD/EOC.
    fn synthetic_j2k() -> Vec<u8> {
        let mut d = vec![0xFF, 0x4F]; // SOC
        d.extend_from_slice(&[0xFF, 0x51]); // SIZ marker
        let mut siz = Vec::new();
        siz.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
        siz.extend_from_slice(&2048u32.to_be_bytes()); // Xsiz
        siz.extend_from_slice(&1080u32.to_be_bytes()); // Ysiz
        siz.extend_from_slice(&0u32.to_be_bytes()); // XOsiz
        siz.extend_from_slice(&0u32.to_be_bytes()); // YOsiz
        siz.extend_from_slice(&2048u32.to_be_bytes()); // XTsiz
        siz.extend_from_slice(&1080u32.to_be_bytes()); // YTsiz
        siz.extend_from_slice(&0u32.to_be_bytes()); // XTOsiz
        siz.extend_from_slice(&0u32.to_be_bytes()); // YTOsiz
        siz.extend_from_slice(&3u16.to_be_bytes()); // Csiz = 3 components
        for _ in 0..3 {
            siz.push(11); // Ssiz: 12-bit unsigned
            siz.push(1); // XRsiz
            siz.push(1); // YRsiz
        }
        let lsiz = (siz.len() + 2) as u16;
        d.extend_from_slice(&lsiz.to_be_bytes());
        d.extend_from_slice(&siz);
        d.extend_from_slice(&[0xFF, 0x93]); // SOD
        // distinctive payload so we can tell whether the essence was encrypted
        for _ in 0..4 {
            d.extend_from_slice(PLAINTEXT_TAG);
        }
        d.extend_from_slice(&[0xFF, 0xD9]); // EOC
        d
    }

    /// A byte run that won't occur in MXF structure, only in our essence.
    const PLAINTEXT_TAG: &[u8] = b"DCPWIZARD_PLAINTEXT_ESSENCE_TAG!";

    fn j2k_opts(
        input: std::path::PathBuf,
        output: std::path::PathBuf,
        encryption: Option<MxfEncryption>,
    ) -> MxfWrapOptions {
        MxfWrapOptions {
            input_files: vec![input],
            output,
            essence_type: EssenceType::J2k,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption,
            mca_config: None,
        }
    }

    #[test]
    fn encrypted_wrap_succeeds_and_differs_from_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("0001.j2c");
        std::fs::write(&frame, synthetic_j2k()).unwrap();

        let plain_out = dir.path().join("plain.mxf");
        let plain = mxf_wrap(&j2k_opts(frame.clone(), plain_out.clone(), None));
        assert!(plain.success, "plaintext wrap failed: {}", plain.error);

        let enc_out = dir.path().join("enc.mxf");
        let enc = mxf_wrap(&j2k_opts(
            frame,
            enc_out.clone(),
            Some(MxfEncryption {
                content_key: [0x11; 16],
                key_id: [0x22; 16],
            }),
        ));
        assert!(enc.success, "encrypted wrap failed: {}", enc.error);

        let p = std::fs::read(&plain_out).unwrap();
        let e = std::fs::read(&enc_out).unwrap();
        // plaintext essence is stored verbatim; encrypted essence must not be
        assert!(
            contains(&p, PLAINTEXT_TAG),
            "plaintext MXF should store the essence verbatim"
        );
        assert!(
            !contains(&e, PLAINTEXT_TAG),
            "essence tag survived into the encrypted MXF: essence was not encrypted"
        );
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn wrap_stereoscopic_roundtrips_both_eyes() {
        use asdcplib::jp2k::{StereoMxfReader, StereoscopicPhase};

        let dir = tempfile::tempdir().unwrap();
        // distinct left/right payloads so a swap cannot pass
        let mut left_frame = synthetic_j2k();
        left_frame.extend_from_slice(b"LEFT_EYE");
        let mut right_frame = synthetic_j2k();
        right_frame.extend_from_slice(b"RIGHT_EYE");
        let l = dir.path().join("l0001.j2c");
        let r = dir.path().join("r0001.j2c");
        std::fs::write(&l, &left_frame).unwrap();
        std::fs::write(&r, &right_frame).unwrap();

        let out = dir.path().join("stereo.mxf");
        let result = wrap_stereoscopic(&StereoscopicWrapOptions {
            left_files: vec![l],
            right_files: vec![r],
            output: out.clone(),
            fps_num: 24,
            fps_den: 1,
            encryption: None,
        });
        assert!(result.success, "stereo wrap failed: {}", result.error);
        assert_eq!(result.duration, 1, "one stereo frame pair");

        assert_eq!(
            asdcplib::essence_type(&out.to_string_lossy()).unwrap(),
            asdcplib::EssenceType::Jpeg2000Stereo
        );

        let mut reader = StereoMxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        let desc = reader.picture_descriptor().unwrap();
        assert_eq!(desc.container_duration, 1, "frame-pair count");

        let mut buf = vec![0u8; 8192];
        let n = reader
            .read_frame(0, StereoscopicPhase::Left, &mut buf, None, None)
            .unwrap();
        assert_eq!(&buf[..n], left_frame.as_slice(), "left eye");
        let n = reader
            .read_frame(0, StereoscopicPhase::Right, &mut buf, None, None)
            .unwrap();
        assert_eq!(&buf[..n], right_frame.as_slice(), "right eye");
    }

    #[test]
    fn wrap_stereoscopic_rejects_mismatched_eye_counts() {
        let dir = tempfile::tempdir().unwrap();
        let l = dir.path().join("l.j2c");
        std::fs::write(&l, synthetic_j2k()).unwrap();
        let result = wrap_stereoscopic(&StereoscopicWrapOptions {
            left_files: vec![l],
            right_files: vec![],
            output: dir.path().join("out.mxf"),
            fps_num: 24,
            fps_den: 1,
            encryption: None,
        });
        assert!(!result.success);
        assert!(
            result.error.contains("counts differ"),
            "got: {}",
            result.error
        );
    }

    #[test]
    fn wrap_pcm_attaches_mca_labels() {
        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("51.wav");
        // 5.1, 24-bit, 48k, one second
        std::fs::write(&wav_path, make_wav(6, 48000, 24, 48000)).unwrap();
        let out = dir.path().join("out.mxf");

        let mca = crate::mca::soundfield_to_mca_config(&crate::mca::soundfield_51());
        assert_eq!(mca.as_deref(), Some("51(L,R,C,LFE,Ls,Rs)"));

        let opts = MxfWrapOptions {
            input_files: vec![wav_path],
            output: out.clone(),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: mca,
        };
        let result = wrap_pcm(&opts);
        assert!(result.success, "wrap failed: {}", result.error);

        let mut reader = asdcplib::pcm::MxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        let labels = reader.mca_labels().expect("read mca labels");
        assert_eq!(labels.channel_labels, 6, "one label per 5.1 channel");
        assert_eq!(labels.soundfield_groups, 1, "one 5.1 soundfield group");
        assert!(
            labels.has_mca_channel_assignment,
            "MCA ChannelAssignment UL must be set"
        );
    }

    #[test]
    fn wrap_pcm_mca_rejected_on_as02() {
        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("51.wav");
        std::fs::write(&wav_path, make_wav(6, 48000, 24, 48000)).unwrap();
        let opts = MxfWrapOptions {
            input_files: vec![wav_path],
            output: dir.path().join("out.mxf"),
            essence_type: EssenceType::Pcm,
            standard: MxfStandard::As02,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: Some("51(L,R,C,LFE,Ls,Rs)".to_string()),
        };
        let result = wrap_pcm(&opts);
        assert!(!result.success, "MCA on AS-02 must be rejected");
        assert!(result.error.contains("AS-DCP"), "got: {}", result.error);
    }

    /// Wrap a structurally valid but synthetic DCData/Atmos payload and confirm
    /// the container is a Dolby Atmos aux-data MXF the reader accepts. This does
    /// NOT validate real Atmos essence: the frames are filler, so only the MXF
    /// structure and descriptor are exercised. Real-essence verification needs
    /// real Atmos material.
    #[test]
    fn wrap_atmos_produces_a_readable_dcdata_container() {
        let dir = tempfile::tempdir().unwrap();
        let frames: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("atmos{i}.dat"));
                let payload: Vec<u8> = (0..2048).map(|b| (b as u8).wrapping_add(i * 5)).collect();
                std::fs::write(&p, payload).unwrap();
                p
            })
            .collect();
        let out = dir.path().join("atmos.mxf");

        let opts = MxfWrapOptions {
            input_files: frames,
            output: out.clone(),
            essence_type: EssenceType::Atmos,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: None,
        };
        let result = mxf_wrap(&opts);
        assert!(result.success, "atmos wrap failed: {}", result.error);
        assert_eq!(result.duration, 3);

        assert_eq!(
            asdcplib::essence_type(&out.to_string_lossy()).unwrap(),
            asdcplib::EssenceType::DcDataDolbyAtmos
        );

        let mut reader = asdcplib::atmos::MxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        let desc = reader.atmos_descriptor().unwrap();
        assert_eq!(desc.container_duration, 3, "frame count");
        assert_eq!(desc.edit_rate, asdcplib::Rational::new(24, 1));
    }
}
