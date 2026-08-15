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
    /// When set, the essence is AES-128 encrypted at wrap time. Every essence
    /// type honours it, so no wrap silently ships cleartext under a key the CPL
    /// claims. Never serialized: it carries secret key material.
    #[serde(skip)]
    pub encryption: Option<MxfEncryption>,
    /// SMPTE 377-4 MCA label config for PCM, asdcp-wrap style, e.g.
    /// `"51(L,R,C,LFE,Ls,Rs),HI,VIN"`. AS-DCP (DCP) only. Build one from a
    /// soundfield with [`crate::mca::soundfield_to_mca_config`].
    #[serde(default)]
    pub mca_config: Option<String>,
    /// TimedText only: explicit asset ids for the ancillary resources (the
    /// `input_files` after the first). Entry `i` is the id for `input_files[i+1]`.
    /// A DCST that references a font/image by `urn:uuid:<id>` must embed that
    /// resource under the same id, so the caller controls it here. Missing or
    /// short: those resources get a random id (back-compatible default).
    #[serde(default)]
    pub resource_ids: Vec<[u8; 16]>,
    /// HDR/WCG picture metadata (ST 2067-21) for a J2K wrap: transfer/colour ULs
    /// and ST 2086 mastering display, written onto the RGBA essence descriptor
    /// via open_write_hdr. Never serialized (asdcplib type is not serde).
    #[serde(skip)]
    pub hdr: Option<asdcplib::jp2k::HdrMetadata>,
    /// The asset id written into the MXF as its AssetUUID and returned as
    /// [`MxfTrackFile::uuid`]. A caller that names the output file or writes the
    /// CPL/PKL/ASSETMAP entry before wrapping must pass its id here, otherwise
    /// the MXF ends up carrying a different id than the package claims. None
    /// mints a fresh id.
    #[serde(default)]
    pub asset_uuid: Option<[u8; 16]>,
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
        hdr: Option<&asdcplib::jp2k::HdrMetadata>,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match (self, hdr) {
            (Self::AsDcp(w), None) => w.open_write(filename, info, desc, header_size),
            (Self::AsDcp(w), Some(h)) => w.open_write_hdr(filename, info, desc, h, header_size),
            (Self::As02(w), None) => w.open_write(filename, info, desc, header_size),
            (Self::As02(w), Some(h)) => w.open_write_hdr(filename, info, desc, h, header_size),
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

    /// Open the writer, declaring the ancillary resources the wrap will then
    /// write. A reader can only enumerate resources declared here, so the list
    /// has to be complete and in the order the writes follow.
    ///
    /// AS-02 has no equivalent entry point in asdcplib, which is why
    /// `wrap_timed_text` refuses ancillary resources on that standard.
    fn open_write(
        &mut self,
        filename: &str,
        info: &asdcplib::WriterInfo,
        desc: &asdcplib::timed_text::TimedTextDescriptor,
        resources: &[asdcplib::timed_text::AncillaryResourceInfo],
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => {
                w.open_write_with_resources(filename, info, desc, resources, header_size)
            }
            Self::As02(w) => w.open_write(filename, info, desc, header_size),
        }
    }

    fn write_timed_text_resource(
        &mut self,
        xml: &str,
        enc: &mut EssenceCrypto,
    ) -> asdcplib::Result<()> {
        let (e, h) = enc.contexts();
        match self {
            Self::AsDcp(w) => w.write_timed_text_resource(xml, e, h),
            Self::As02(w) => w.write_timed_text_resource(xml, e, h),
        }
    }

    fn write_ancillary_resource(
        &mut self,
        data: &[u8],
        uuid: &[u8; 16],
        mime_type: &str,
        enc: &mut EssenceCrypto,
    ) -> asdcplib::Result<()> {
        let (e, h) = enc.contexts();
        match self {
            Self::AsDcp(w) => w.write_ancillary_resource(data, uuid, mime_type, e, h),
            Self::As02(w) => w.write_ancillary_resource(data, uuid, mime_type, e, h),
        }
    }

    fn finalize(&mut self) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => w.finalize(),
            Self::As02(w) => w.finalize(),
        }
    }
}

fn make_writer_info(asset_uuid: Option<[u8; 16]>) -> asdcplib::WriterInfo {
    let context_id = uuid::Uuid::new_v4();
    asdcplib::WriterInfo {
        asset_uuid: asset_uuid.unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes()),
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
    // dci profile rules apply to dcp only; as-02 (imf) takes any codestream with an image area
    if opts.standard == MxfStandard::AsDcp {
        if let Err(error) = crate::j2k::validate_dci_header(&header) {
            return MxfTrackFile {
                error: format!(
                    "invalid DCI JPEG 2000 codestream: {error}: {}",
                    opts.input_files[0].display()
                ),
                ..Default::default()
            };
        }
    } else if header.width == 0 || header.height == 0 {
        return MxfTrackFile {
            error: format!(
                "JPEG 2000 codestream has no image area: {}",
                opts.input_files[0].display()
            ),
            ..Default::default()
        };
    }
    for (path, frame) in opts.input_files.iter().zip(frames.iter()).skip(1) {
        let Some(header) = crate::j2k::parse_j2k_header(frame) else {
            return MxfTrackFile {
                error: format!("invalid JPEG 2000 codestream: {}", path.display()),
                ..Default::default()
            };
        };
        if opts.standard == MxfStandard::AsDcp
            && let Err(error) = crate::j2k::validate_dci_header(&header)
        {
            return MxfTrackFile {
                error: format!(
                    "invalid DCI JPEG 2000 codestream: {error}: {}",
                    path.display()
                ),
                ..Default::default()
            };
        }
    }

    let mut info = make_writer_info(opts.asset_uuid);
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
    if let Err(e) = writer.open_write(&output_str, &info, &desc, opts.hdr.as_ref(), 16384) {
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

    let mut info = make_writer_info(opts.asset_uuid);
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

/// A timed-text ancillary resource (font, image) and the identity it is embedded
/// under. All of it has to be known before the writer opens, because the header
/// declares the resource list.
struct AncillaryResource {
    data: Vec<u8>,
    uuid: [u8; 16],
    declared_type: asdcplib::timed_text::MimeType,
    mime_type: &'static str,
}

/// File extension to the MIME identity a timed-text resource is embedded under:
/// the type declared in the header and the string written beside the bytes.
/// Those two have to agree, so they are chosen together.
const ANCILLARY_MIME_TYPES: [(&str, asdcplib::timed_text::MimeType, &str); 3] = [
    (
        "ttf",
        asdcplib::timed_text::MimeType::OpenType,
        "application/x-font-opentype",
    ),
    (
        "otf",
        asdcplib::timed_text::MimeType::OpenType,
        "application/x-font-opentype",
    ),
    ("png", asdcplib::timed_text::MimeType::Png, "image/png"),
];

/// What a resource with an unrecognised extension is embedded as.
const DEFAULT_ANCILLARY_MIME: (asdcplib::timed_text::MimeType, &str) = (
    asdcplib::timed_text::MimeType::Binary,
    "application/octet-stream",
);

/// Read the ancillary resources of a timed-text wrap: every input file after the
/// subtitle XML.
fn read_ancillary_resources(opts: &MxfWrapOptions) -> Result<Vec<AncillaryResource>, String> {
    let mut resources = Vec::new();
    for (index, path) in opts.input_files.iter().skip(1).enumerate() {
        let data = std::fs::read(path)
            .map_err(|e| format!("failed to read resource {}: {e}", path.display()))?;
        // caller-supplied id so a DCST urn:uuid ref matches the embedded resource
        let uuid = opts
            .resource_ids
            .get(index)
            .copied()
            .unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes());
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let (declared_type, mime_type) = ANCILLARY_MIME_TYPES
            .iter()
            .find(|(candidate, _, _)| *candidate == extension)
            .map_or(DEFAULT_ANCILLARY_MIME, |(_, declared, mime)| {
                (*declared, *mime)
            });
        resources.push(AncillaryResource {
            data,
            uuid,
            declared_type,
            mime_type,
        });
    }
    Ok(resources)
}

fn wrap_timed_text(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    // an AS-02 resource could be written but never declared, and a reader can
    // only find a declared one, so refuse rather than embed something unusable
    if opts.input_files.len() > 1 && opts.standard == MxfStandard::As02 {
        return MxfTrackFile {
            error: "AS-02 (IMF) timed text cannot embed fonts or images: asdcplib has no AS-02 entry point that declares them in the header, so no reader could find them".to_string(),
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

    let resources = match read_ancillary_resources(opts) {
        Ok(r) => r,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };

    let mut info = make_writer_info(opts.asset_uuid);
    let mut crypto = match setup_encryption(&mut info, &opts.encryption) {
        Ok(c) => c,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };
    let desc = asdcplib::timed_text::TimedTextDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        container_duration: duration_frames,
        asset_id: info.asset_uuid,
    };

    let declared: Vec<_> = resources
        .iter()
        .map(|r| asdcplib::timed_text::AncillaryResourceInfo {
            uuid: r.uuid,
            mime_type: r.declared_type,
        })
        .collect();

    let mut writer = TimedTextWriter::new(opts.standard);
    let output_str = opts.output.to_string_lossy().to_string();
    if let Err(e) = writer.open_write(&output_str, &info, &desc, &declared, 16384) {
        return MxfTrackFile {
            error: format!("TimedText open_write failed: {e}"),
            ..Default::default()
        };
    }

    if let Err(e) = writer.write_timed_text_resource(&xml_data, &mut crypto) {
        return MxfTrackFile {
            error: format!("TimedText write_resource failed: {e}"),
            ..Default::default()
        };
    }

    // in declaration order: the header lists them in the order they are written
    for resource in &resources {
        if let Err(e) = writer.write_ancillary_resource(
            &resource.data,
            &resource.uuid,
            resource.mime_type,
            &mut crypto,
        ) {
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

    let mut info = make_writer_info(opts.asset_uuid);
    let mut crypto = match setup_encryption(&mut info, &opts.encryption) {
        Ok(c) => c,
        Err(error) => {
            return MxfTrackFile {
                error,
                ..Default::default()
            };
        }
    };
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
        let (enc, hmac) = crypto.contexts();
        if let Err(e) = writer.write_frame(frame, enc, hmac) {
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
    /// The asset id written into the MXF as its AssetUUID and returned as
    /// [`MxfTrackFile::uuid`], as in [`MxfWrapOptions::asset_uuid`].
    #[serde(default)]
    pub asset_uuid: Option<[u8; 16]>,
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
    if let Err(error) = crate::j2k::validate_dci_header(&header) {
        return MxfTrackFile {
            error: format!(
                "invalid DCI JPEG 2000 codestream: {error}: {}",
                opts.left_files[0].display()
            ),
            ..Default::default()
        };
    }
    for (path, frame) in opts
        .left_files
        .iter()
        .zip(left.iter())
        .skip(1)
        .chain(opts.right_files.iter().zip(right.iter()))
    {
        let Some(header) = crate::j2k::parse_j2k_header(frame) else {
            return MxfTrackFile {
                error: format!("invalid JPEG 2000 codestream: {}", path.display()),
                ..Default::default()
            };
        };
        if let Err(error) = crate::j2k::validate_dci_header(&header) {
            return MxfTrackFile {
                error: format!(
                    "invalid DCI JPEG 2000 codestream: {error}: {}",
                    path.display()
                ),
                ..Default::default()
            };
        }
    }

    let mut info = make_writer_info(opts.asset_uuid);
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
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
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
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
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
        synthetic_j2k_with_profile(3)
    }

    fn synthetic_j2k_with_profile(profile: u16) -> Vec<u8> {
        let mut d = vec![0xFF, 0x4F]; // SOC
        d.extend_from_slice(&[0xFF, 0x51]); // SIZ marker
        let mut siz = Vec::new();
        siz.extend_from_slice(&profile.to_be_bytes()); // Rsiz
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

    fn wrap_opts(
        essence_type: EssenceType,
        input_files: Vec<std::path::PathBuf>,
        output: std::path::PathBuf,
        encryption: Option<MxfEncryption>,
    ) -> MxfWrapOptions {
        MxfWrapOptions {
            input_files,
            output,
            essence_type,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption,
            mca_config: None,
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
        }
    }

    fn j2k_opts(
        input: std::path::PathBuf,
        output: std::path::PathBuf,
        encryption: Option<MxfEncryption>,
    ) -> MxfWrapOptions {
        wrap_opts(EssenceType::J2k, vec![input], output, encryption)
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

    #[test]
    fn wrap_j2k_rejects_non_dci_profile() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("0001.j2c");
        std::fs::write(&frame, synthetic_j2k_with_profile(0)).unwrap();

        let result = mxf_wrap(&j2k_opts(frame, dir.path().join("out.mxf"), None));
        assert!(!result.success);
        assert!(
            result.error.contains("not a DCI JPEG 2000 profile"),
            "got: {}",
            result.error
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
            asset_uuid: None,
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
            asset_uuid: None,
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
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
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
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
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
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
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

    const DCST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<dcst:SubtitleReel xmlns:dcst=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\">\n\
  <dcst:Id>urn:uuid:11111111-1111-1111-1111-111111111111</dcst:Id>\n\
  <dcst:ContentTitleText>t</dcst:ContentTitleText>\n\
  <dcst:IssueDate>2020-01-01T00:00:00+00:00</dcst:IssueDate>\n\
  <dcst:EditRate>24 1</dcst:EditRate>\n\
  <dcst:TimeCodeRate>24</dcst:TimeCodeRate>\n\
  <dcst:LoadFont ID=\"f1\">urn:uuid:22222222-2222-2222-2222-222222222222</dcst:LoadFont>\n\
  <dcst:SubtitleList>\n\
    <dcst:Font ID=\"f1\">\n\
      <dcst:Subtitle SpotNumber=\"1\" TimeIn=\"00:00:01:00\" TimeOut=\"00:00:04:00\">\n\
        <dcst:Text>hi</dcst:Text>\n\
      </dcst:Subtitle>\n\
    </dcst:Font>\n\
  </dcst:SubtitleList>\n\
</dcst:SubtitleReel>\n";

    /// The font id `DCST` references from its LoadFont element, which the wrap
    /// has to embed the resource under.
    const DCST_FONT_ID: &str = "22222222-2222-2222-2222-222222222222";

    /// A string only the cleartext subtitle XML can contain.
    const DCST_MARKER: &[u8] = b"SubtitleReel";

    /// Read buffer for a whole timed-text or Atmos resource in these tests.
    const RESOURCE_READ_BUFFER_LEN: usize = 64 * 1024;

    /// The id of the image resource in the two-resource tests. Unlike the font it
    /// is not referenced from `DCST`, since the MXF side is what is under test.
    const DCST_IMAGE_ID: &str = "33333333-3333-3333-3333-333333333333";

    /// Stand-in font and image resources. Distinct contents and lengths, so a
    /// wrap that mixed the two up could not pass. The font carries the plaintext
    /// tag, which is also what an encrypted wrap has to hide.
    fn font_bytes() -> Vec<u8> {
        tagged_payload()
    }

    fn image_bytes() -> Vec<u8> {
        vec![0xb2; 2048]
    }

    /// A subtitle wrap with a font and an image, as dcpwizard produces after font
    /// subsetting.
    fn timed_text_with_resources(
        dir: &std::path::Path,
        output: std::path::PathBuf,
        encryption: Option<MxfEncryption>,
    ) -> MxfWrapOptions {
        let xml = dir.join("sub.xml");
        std::fs::write(&xml, DCST).unwrap();
        let font = dir.join("f.ttf");
        std::fs::write(&font, font_bytes()).unwrap();
        let image = dir.join("i.png");
        std::fs::write(&image, image_bytes()).unwrap();

        let mut opts = wrap_opts(
            EssenceType::TimedText,
            vec![xml, font, image],
            output,
            encryption,
        );
        opts.resource_ids = vec![
            *uuid::Uuid::parse_str(DCST_FONT_ID).unwrap().as_bytes(),
            *uuid::Uuid::parse_str(DCST_IMAGE_ID).unwrap().as_bytes(),
        ];
        opts
    }

    /// A font a player cannot find is a font that is not there: the subtitle MXF
    /// has to declare its resources in the header, under the ids the DCST refers
    /// to, so a reader can enumerate and read them back.
    #[test]
    fn timed_text_embeds_fonts_and_images_a_reader_can_find() {
        use asdcplib::timed_text::MimeType;

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sub.mxf");
        let opts = timed_text_with_resources(dir.path(), out.clone(), None);
        let font_id = opts.resource_ids[0];
        let image_id = opts.resource_ids[1];

        let result = mxf_wrap(&opts);
        assert!(result.success, "timed text wrap failed: {}", result.error);
        assert_eq!(result.duration, 96, "4.0 s at 24 fps");

        // the primary timed-text resource round-trips out of the MXF
        let mut reader = asdcplib::timed_text::MxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        let mut buf = vec![0u8; RESOURCE_READ_BUFFER_LEN];
        let n = reader
            .read_timed_text_resource(&mut buf, None, None)
            .unwrap();
        let back = String::from_utf8_lossy(&buf[..n]);
        assert!(back.contains("SubtitleReel"), "XML round-trips");

        assert_eq!(
            reader.ancillary_resource_count().unwrap(),
            2,
            "both resources are declared in the header"
        );
        let declared = [
            (font_id, MimeType::OpenType, font_bytes()),
            (image_id, MimeType::Png, image_bytes()),
        ];
        for (index, (id, mime_type, data)) in declared.into_iter().enumerate() {
            let info = reader.ancillary_resource_info(index).unwrap();
            assert_eq!(info.uuid, id, "resource {index} is declared under its id");
            assert_eq!(info.mime_type, mime_type, "resource {index} MIME type");

            let n = reader
                .read_ancillary_resource(&id, &mut buf, None, None)
                .unwrap();
            assert_eq!(&buf[..n], &data[..], "resource {index} round-trips");
        }
    }

    /// The AES/HMAC contexts a reader needs to recover essence encrypted under
    /// `key`, built the same way the wrap builds the writing side.
    fn read_contexts(
        key: &[u8; 16],
    ) -> (
        asdcplib::crypto::AesDecContext,
        asdcplib::crypto::HmacContext,
    ) {
        let mut dec = asdcplib::crypto::AesDecContext::new();
        dec.init_key(key).unwrap();
        let mut hmac = asdcplib::crypto::HmacContext::new();
        hmac.init_key(key, asdcplib::LabelSet::Smpte).unwrap();
        (dec, hmac)
    }

    /// A distinctive essence payload, so an unencrypted wrap is findable in the
    /// file bytes and an encrypted one provably is not.
    fn tagged_payload() -> Vec<u8> {
        const TAG_REPEATS: usize = 128;
        PLAINTEXT_TAG.repeat(TAG_REPEATS)
    }

    /// Encryption has to reach the subtitle XML and its ancillary resources, and
    /// the MXF has to name the KeyId the CPL will claim for the track. Reading
    /// back under the same key proves the essence was encrypted with it rather
    /// than merely flagged, and the HMAC proves the integrity side is on.
    #[test]
    fn timed_text_encrypts_xml_and_ancillary_resources_under_the_caller_key() {
        const CONTENT_KEY: [u8; 16] = [0x31; 16];
        const KEY_ID: [u8; 16] = [0x32; 16];

        let dir = tempfile::tempdir().unwrap();

        let plain_out = dir.path().join("plain.mxf");
        let plain = mxf_wrap(&timed_text_with_resources(
            dir.path(),
            plain_out.clone(),
            None,
        ));
        assert!(plain.success, "cleartext wrap failed: {}", plain.error);
        let plain_bytes = std::fs::read(&plain_out).unwrap();
        assert!(
            contains(&plain_bytes, DCST_MARKER) && contains(&plain_bytes, PLAINTEXT_TAG),
            "the cleartext wrap should store XML and font verbatim"
        );
        let mut plain_reader = asdcplib::timed_text::MxfReader::new();
        plain_reader
            .open_read(&plain_out.to_string_lossy())
            .unwrap();
        assert!(
            !plain_reader.writer_info().unwrap().encrypted_essence,
            "an unencrypted wrap must not claim encrypted essence"
        );

        let enc_out = dir.path().join("enc.mxf");
        let opts = timed_text_with_resources(
            dir.path(),
            enc_out.clone(),
            Some(MxfEncryption {
                content_key: CONTENT_KEY,
                key_id: KEY_ID,
            }),
        );
        let font_id = opts.resource_ids[0];
        let image_id = opts.resource_ids[1];
        let enc = mxf_wrap(&opts);
        assert!(enc.success, "encrypted wrap failed: {}", enc.error);

        let enc_bytes = std::fs::read(&enc_out).unwrap();
        assert!(
            !contains(&enc_bytes, DCST_MARKER),
            "subtitle XML survived into the encrypted MXF: it was not encrypted"
        );
        assert!(
            !contains(&enc_bytes, PLAINTEXT_TAG),
            "font resource survived into the encrypted MXF: it was not encrypted"
        );

        let mut reader = asdcplib::timed_text::MxfReader::new();
        reader.open_read(&enc_out.to_string_lossy()).unwrap();
        let info = reader.writer_info().unwrap();
        assert!(info.encrypted_essence, "essence is flagged encrypted");
        assert!(info.uses_hmac, "integrity protection is on");
        assert_eq!(
            info.cryptographic_key_id, KEY_ID,
            "the MXF names the KeyId the CPL will claim"
        );

        // reading back under the same key: only the real key recovers this, so it
        // is the key the essence was encrypted with
        let (mut dec, mut hmac) = read_contexts(&CONTENT_KEY);
        let mut buf = vec![0u8; RESOURCE_READ_BUFFER_LEN];
        let n = reader
            .read_timed_text_resource(&mut buf, Some(&mut dec), Some(&mut hmac))
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&buf[..n]),
            DCST,
            "decrypted subtitle XML"
        );

        assert_eq!(
            reader.ancillary_resource_count().unwrap(),
            2,
            "encryption does not cost the resource declarations"
        );
        for (id, data) in [(font_id, font_bytes()), (image_id, image_bytes())] {
            let n = reader
                .read_ancillary_resource(&id, &mut buf, Some(&mut dec), Some(&mut hmac))
                .unwrap();
            assert_eq!(&buf[..n], &data[..], "decrypted ancillary resource");
        }
    }

    /// Same contract for Atmos: the key reaches the essence, the KeyId in the MXF
    /// is the one the CPL will claim, and the HMAC is written.
    #[test]
    fn atmos_encrypts_frames_under_the_caller_key() {
        const CONTENT_KEY: [u8; 16] = [0x41; 16];
        const KEY_ID: [u8; 16] = [0x42; 16];

        let dir = tempfile::tempdir().unwrap();
        let frame_path = dir.path().join("a.dat");
        let frame_data = tagged_payload();
        std::fs::write(&frame_path, &frame_data).unwrap();

        let plain_out = dir.path().join("plain.mxf");
        let plain = mxf_wrap(&wrap_opts(
            EssenceType::Atmos,
            vec![frame_path.clone()],
            plain_out.clone(),
            None,
        ));
        assert!(plain.success, "cleartext wrap failed: {}", plain.error);
        assert!(
            contains(&std::fs::read(&plain_out).unwrap(), PLAINTEXT_TAG),
            "the cleartext wrap should store the frame verbatim"
        );
        let mut plain_reader = asdcplib::atmos::MxfReader::new();
        plain_reader
            .open_read(&plain_out.to_string_lossy())
            .unwrap();
        assert!(
            !plain_reader.writer_info().unwrap().encrypted_essence,
            "an unencrypted wrap must not claim encrypted essence"
        );

        let enc_out = dir.path().join("enc.mxf");
        let enc = mxf_wrap(&wrap_opts(
            EssenceType::Atmos,
            vec![frame_path],
            enc_out.clone(),
            Some(MxfEncryption {
                content_key: CONTENT_KEY,
                key_id: KEY_ID,
            }),
        ));
        assert!(enc.success, "encrypted wrap failed: {}", enc.error);
        assert!(
            !contains(&std::fs::read(&enc_out).unwrap(), PLAINTEXT_TAG),
            "frame survived into the encrypted MXF: it was not encrypted"
        );

        let mut reader = asdcplib::atmos::MxfReader::new();
        reader.open_read(&enc_out.to_string_lossy()).unwrap();
        let info = reader.writer_info().unwrap();
        assert!(info.encrypted_essence, "essence is flagged encrypted");
        assert!(info.uses_hmac, "integrity protection is on");
        assert_eq!(
            info.cryptographic_key_id, KEY_ID,
            "the MXF names the KeyId the CPL will claim"
        );

        let (mut dec, mut hmac) = read_contexts(&CONTENT_KEY);
        let mut buf = vec![0u8; RESOURCE_READ_BUFFER_LEN];
        let n = reader
            .read_frame(0, &mut buf, Some(&mut dec), Some(&mut hmac))
            .unwrap();
        assert_eq!(&buf[..n], &frame_data[..], "decrypted Atmos frame");
    }

    /// Read the AssetUUID an MXF actually carries, using an independent
    /// asdcp-info binary rather than this crate's own reader. Returns None when
    /// the tool reports no AssetUUID (it refuses AS-02 files).
    fn asdcp_info_asset_uuid(tool: &str, path: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new(tool)
            .arg("-i")
            .arg(path)
            .output()
            .expect("run asdcp-info");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.trim().strip_prefix("AssetUUID:"))
            .map(|v| v.trim().to_lowercase())
    }

    /// The one id an asset is known by: the AssetUUID inside the MXF, the uuid in
    /// the file name, and the id postkit reports for the CPL/PKL/ASSETMAP must all
    /// be the same. The MXF side is read back with an external asdcp-info binary,
    /// so postkit cannot agree with itself and pass. Gated on POSTKIT_ASDCP_INFO.
    #[test]
    fn every_wrap_path_writes_the_caller_supplied_asset_uuid() {
        let Ok(tool) = std::env::var("POSTKIT_ASDCP_INFO") else {
            eprintln!("skipping: set POSTKIT_ASDCP_INFO to an asdcp-info binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();

        let frame = dir.path().join("0001.j2c");
        std::fs::write(&frame, synthetic_j2k()).unwrap();
        let wav = dir.path().join("in.wav");
        std::fs::write(&wav, make_wav(2, 48000, 24, 48000)).unwrap();
        let dcst = dir.path().join("sub.xml");
        std::fs::write(&dcst, DCST).unwrap();
        let atmos_frame = dir.path().join("a.dat");
        std::fs::write(&atmos_frame, vec![7u8; 2048]).unwrap();
        let right = dir.path().join("r.j2c");
        std::fs::write(&right, synthetic_j2k()).unwrap();

        let cases: [(&str, &str, EssenceType, &std::path::Path); 4] = [
            (
                "picture",
                "f80300de-a6d9-4be8-820f-4df99ae5143c",
                EssenceType::J2k,
                &frame,
            ),
            (
                "sound",
                "ba1ea856-b639-4868-9f1e-3b4c76279c07",
                EssenceType::Pcm,
                &wav,
            ),
            (
                "subtitle",
                "3c2b1a09-8877-4655-a433-2211ffeeddcc",
                EssenceType::TimedText,
                &dcst,
            ),
            (
                "atmos",
                "0d1c2b3a-4958-4677-8695-a4b3c2d1e0f9",
                EssenceType::Atmos,
                &atmos_frame,
            ),
        ];

        for (kind, id, essence_type, input) in cases {
            let asset_uuid = *uuid::Uuid::parse_str(id).unwrap().as_bytes();
            let output = dir.path().join(format!("{kind}_{id}.mxf"));
            let mut opts = j2k_opts(input.to_path_buf(), output.clone(), None);
            opts.essence_type = essence_type;
            opts.asset_uuid = Some(asset_uuid);
            let result = mxf_wrap(&opts);
            assert!(result.success, "{kind} wrap failed: {}", result.error);

            let in_mxf = asdcp_info_asset_uuid(&tool, &output)
                .unwrap_or_else(|| panic!("{kind}: asdcp-info reported no AssetUUID"));
            assert_eq!(in_mxf, id, "{kind}: MXF AssetUUID vs the file name");
            assert_eq!(result.uuid, in_mxf, "{kind}: reported id vs MXF AssetUUID");
        }

        let stereo_id = "5a4b3c2d-1e0f-4988-a776-655443322110";
        let stereo_out = dir.path().join(format!("picture_{stereo_id}.mxf"));
        let stereo = wrap_stereoscopic(&StereoscopicWrapOptions {
            left_files: vec![frame.clone()],
            right_files: vec![right],
            output: stereo_out.clone(),
            fps_num: 24,
            fps_den: 1,
            encryption: None,
            asset_uuid: Some(*uuid::Uuid::parse_str(stereo_id).unwrap().as_bytes()),
        });
        assert!(stereo.success, "stereo wrap failed: {}", stereo.error);
        let in_mxf = asdcp_info_asset_uuid(&tool, &stereo_out)
            .expect("stereo: asdcp-info reported no AssetUUID");
        assert_eq!(in_mxf, stereo_id, "stereo: MXF AssetUUID vs the file name");
        assert_eq!(stereo.uuid, in_mxf, "stereo: reported id vs MXF AssetUUID");
    }

    /// Without a caller-supplied id the wrap mints one, and the id it reports must
    /// still be the id the MXF carries. Read back with the external asdcp-info, so
    /// this asserts nothing the test itself chose. Gated on POSTKIT_ASDCP_INFO.
    #[test]
    fn a_minted_asset_uuid_is_reported_as_the_mxf_carries_it() {
        let Ok(tool) = std::env::var("POSTKIT_ASDCP_INFO") else {
            eprintln!("skipping: set POSTKIT_ASDCP_INFO to an asdcp-info binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("0001.j2c");
        std::fs::write(&frame, synthetic_j2k()).unwrap();
        let output = dir.path().join("minted.mxf");

        let result = mxf_wrap(&j2k_opts(frame, output.clone(), None));
        assert!(result.success, "wrap failed: {}", result.error);
        assert_eq!(
            asdcp_info_asset_uuid(&tool, &output).expect("asdcp-info reported no AssetUUID"),
            result.uuid,
        );
    }

    /// asdcp-info refuses AS-02 files, so the AS-02 wraps are checked against the
    /// file bytes: the caller's id must be present as the raw 16-byte UMID material
    /// asdcplib writes, and it must be what postkit reports.
    #[test]
    fn as02_wraps_write_the_caller_supplied_asset_uuid_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("0001.j2c");
        std::fs::write(&frame, synthetic_j2k()).unwrap();
        let wav = dir.path().join("in.wav");
        std::fs::write(&wav, make_wav(2, 48000, 24, 48000)).unwrap();
        let dcst = dir.path().join("sub.xml");
        std::fs::write(&dcst, DCST).unwrap();

        let cases: [(&str, EssenceType, &std::path::Path); 3] = [
            (
                "6f5e4d3c-2b1a-4099-8877-665544332211",
                EssenceType::J2k,
                &frame,
            ),
            (
                "1a2b3c4d-5e6f-4788-9900-aabbccddeeff",
                EssenceType::Pcm,
                &wav,
            ),
            (
                "9e8d7c6b-5a49-4382-b170-fedcba987654",
                EssenceType::TimedText,
                &dcst,
            ),
        ];

        for (id, essence_type, input) in cases {
            let asset_uuid = *uuid::Uuid::parse_str(id).unwrap().as_bytes();
            let output = dir.path().join(format!("as02_{id}.mxf"));
            let mut opts = j2k_opts(input.to_path_buf(), output.clone(), None);
            opts.essence_type = essence_type;
            opts.standard = MxfStandard::As02;
            opts.asset_uuid = Some(asset_uuid);
            let result = mxf_wrap(&opts);
            assert!(result.success, "{id} wrap failed: {}", result.error);
            assert_eq!(result.uuid, id, "{id}: reported id");

            let bytes = std::fs::read(&output).unwrap();
            assert!(
                contains(&bytes, &asset_uuid),
                "{id}: the AS-02 MXF does not carry the caller's asset id"
            );
        }
    }
}
