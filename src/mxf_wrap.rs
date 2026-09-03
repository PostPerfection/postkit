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
/// so the key cannot leak through logs or an on-disk options blob, and zeroed on
/// drop like the keys recovered from a KDM. A clone owns its own copy and clears
/// it the same way, so no copy outlives the value it came from.
#[derive(Clone)]
pub struct MxfEncryption {
    pub content_key: [u8; 16],
    pub key_id: [u8; 16],
}

impl MxfEncryption {
    /// Clear the content key. `key_id` is not secret: it is written into the MXF
    /// header and named by the CPL.
    fn clear_content_key(&mut self) {
        use zeroize::Zeroize;
        self.content_key.zeroize();
    }
}

impl Drop for MxfEncryption {
    fn drop(&mut self) {
        self.clear_content_key();
    }
}

impl std::fmt::Debug for MxfEncryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MxfEncryption")
            .field("content_key", &"<redacted>")
            .field("key_id", &uuid::Uuid::from_bytes(self.key_id))
            .finish()
    }
}

/// The MCA labels a PCM wrap carries: the asdcp-wrap style label string, e.g.
/// `"51(L,R,C,LFE,Ls,Rs),HI,VIN"` (build one from a soundfield with
/// [`crate::mca::soundfield_to_mca_config`]), and the RFC 5646 spoken language
/// written on every label. asdcplib writes en-US when the language is None.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McaConfig {
    pub labels: String,
    pub spoken_language: Option<String>,
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
    /// SMPTE 377-4 MCA labels for PCM. AS-DCP (DCP) only.
    #[serde(default)]
    pub mca_config: Option<McaConfig>,
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
    /// TimedText only: the essence duration to write, in frames at the wrap edit
    /// rate. A reel's subtitle asset has to span the whole reel even where the
    /// cues do not, so the caller that knows the reel length sets it here. The
    /// wrap is refused when a cue ends past it. None derives the duration from
    /// the last cue, which also means a document with no cue timing is refused.
    #[serde(default)]
    pub timed_text_duration_frames: Option<u32>,
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
        mca_config: Option<&McaConfig>,
        header_size: u32,
    ) -> asdcplib::Result<()> {
        match self {
            Self::AsDcp(w) => match mca_config {
                Some(m) => w.open_write_mca(
                    filename,
                    info,
                    desc,
                    &m.labels,
                    m.spoken_language.as_deref(),
                    header_size,
                ),
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

/// Bytes read at a time when hashing a finished MXF. A feature's picture track
/// file is tens of gigabytes, so it is never held in memory at once.
const HASH_READ_BYTES: usize = 1 << 20;

fn compute_hash_and_size(path: &std::path::Path) -> (String, u64) {
    use sha1::Digest;
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), 0);
    };
    let mut hasher = sha1::Sha1::new();
    let mut buffer = vec![0u8; HASH_READ_BYTES];
    let mut size = 0u64;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
                size += read as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return (String::new(), 0),
        }
    }
    let hash = hasher.finalize();
    (
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        size,
    )
}

/// Bytes asdcplib reserves for the header partition, so it can rewrite the
/// header in place at finalize.
const MXF_HEADER_SIZE: u32 = 16384;

/// Refuse a codestream the wrap cannot carry, naming `source` (a file path or a
/// frame number) in the error.
///
/// A DCI cinema codestream carries X'Y'Z' samples and an IMF one carries RGB, so
/// each standard takes only its own family: neither reader is told which space
/// the samples are in, it follows from the wrap.
fn check_j2k_codestream(
    data: &[u8],
    standard: MxfStandard,
    source: &str,
) -> Result<crate::j2k::J2kHeader, String> {
    let Some(header) = crate::j2k::parse_j2k_header(data) else {
        return Err(format!("invalid JPEG 2000 codestream: {source}"));
    };
    let profile = crate::j2k::J2kProfile::from(header.profile);
    if standard == MxfStandard::As02 && profile.is_dci_cinema() {
        return Err(format!(
            "RSIZ {:#06x} is a DCI cinema profile, so its samples are X'Y'Z': \
             a DCP codestream cannot go in an IMF track file: {source}",
            header.profile
        ));
    }
    if standard == MxfStandard::AsDcp && profile == crate::j2k::J2kProfile::Imf {
        return Err(format!(
            "RSIZ {:#06x} is an IMF profile, so its samples are RGB: \
             an IMF codestream cannot go in a DCP track file: {source}",
            header.profile
        ));
    }
    if standard == MxfStandard::AsDcp {
        crate::j2k::validate_dci_header(&header)
            .map_err(|error| format!("invalid DCI JPEG 2000 codestream: {error}: {source}"))?;
    } else if header.width == 0 || header.height == 0 {
        return Err(format!("JPEG 2000 codestream has no image area: {source}"));
    }
    Ok(header)
}

/// The colour ULs a Rec.709 SDR IMF App 2E picture signals, for the
/// [`MxfWrapOptions::hdr`] of a wrap that has no mastering display to declare.
pub fn rec709_sdr_picture_colour() -> asdcplib::jp2k::HdrMetadata {
    asdcplib::jp2k::HdrMetadata {
        color_primaries: Some(asdcplib::jp2k::COLOR_PRIMARIES_BT709),
        transfer_characteristic: Some(asdcplib::jp2k::TRANSFER_CHARACTERISTIC_BT709),
        ..Default::default()
    }
}

/// Refuse an AS-02 picture wrap that would leave the reader guessing the colour
/// the samples are in: ST 2067-21 App 2E puts ColorPrimaries and
/// TransferCharacteristic on the RGBA essence descriptor, and
/// [`MxfWrapOptions::hdr`] is the only thing that writes them.
fn check_as02_picture_colour(
    standard: MxfStandard,
    hdr: Option<&asdcplib::jp2k::HdrMetadata>,
) -> Result<(), String> {
    if standard != MxfStandard::As02 {
        return Ok(());
    }
    const REQUIREMENT: &str = "an IMF App 2E picture must signal ColorPrimaries and \
                               TransferCharacteristic on its essence descriptor";
    match hdr {
        None => Err(format!(
            "{REQUIREMENT}, and this wrap carries no colour metadata"
        )),
        Some(hdr) if hdr.color_primaries.is_none() && hdr.transfer_characteristic.is_none() => Err(
            format!("{REQUIREMENT}, and this wrap's colour metadata sets neither"),
        ),
        Some(_) => Ok(()),
    }
}

/// The picture descriptor a J2K wrap opens with, built from the first frame's
/// codestream header.
///
/// `container_duration` is only what the caller knows at open time: asdcplib
/// overwrites it with the frames actually written when it rewrites the header
/// partition at finalize, so a wrap that does not know its length yet can pass 0.
fn j2k_picture_descriptor(
    header: &crate::j2k::J2kHeader,
    first_frame: &[u8],
    fps_num: u32,
    fps_den: u32,
    container_duration: u32,
) -> Result<asdcplib::jp2k::PictureDescriptor, String> {
    let codestream = asdcplib::jp2k::CodestreamHeader::parse(first_frame)
        .map_err(|error| format!("cannot read the JPEG 2000 codestream header: {error}"))?;
    Ok(asdcplib::jp2k::PictureDescriptor {
        edit_rate: asdcplib::Rational::new(fps_num as i32, fps_den as i32),
        sample_rate: asdcplib::Rational::new(fps_num as i32, fps_den as i32),
        stored_width: header.width,
        stored_height: header.height,
        aspect_ratio: asdcplib::Rational::new(header.width as i32, header.height as i32),
        container_duration,
        codestream,
    })
}

fn wrap_j2k(opts: &MxfWrapOptions) -> MxfTrackFile {
    if opts.input_files.is_empty() {
        return MxfTrackFile {
            error: "no input files".to_string(),
            ..Default::default()
        };
    }

    if let Err(error) = check_as02_picture_colour(opts.standard, opts.hdr.as_ref()) {
        return MxfTrackFile {
            error,
            ..Default::default()
        };
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

    if let Err(error) = write_j2k_frames(opts, &info, &mut crypto) {
        // the writer closed the file on its way out, so the part-written MXF can go
        let _ = std::fs::remove_file(&opts.output);
        return MxfTrackFile {
            error,
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
        duration: opts.input_files.len() as u64,
        path: opts.output.clone(),
        success: true,
        error: String::new(),
    }
}

/// Write every input codestream into the MXF at `opts.output`, holding one in
/// memory at a time: a feature's picture essence does not fit in memory.
fn write_j2k_frames(
    opts: &MxfWrapOptions,
    info: &asdcplib::WriterInfo,
    crypto: &mut EssenceCrypto,
) -> Result<(), String> {
    let output_str = opts.output.to_string_lossy().to_string();
    let mut writer: Option<J2kWriter> = None;
    for path in &opts.input_files {
        let frame =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let header = check_j2k_codestream(&frame, opts.standard, &path.display().to_string())?;
        if writer.is_none() {
            let desc = j2k_picture_descriptor(
                &header,
                &frame,
                opts.fps_num,
                opts.fps_den,
                opts.input_files.len() as u32,
            )?;
            let mut opened = J2kWriter::new(opts.standard);
            opened
                .open_write(&output_str, info, &desc, opts.hdr.as_ref(), MXF_HEADER_SIZE)
                .map_err(|e| format!("JP2K open_write failed: {e}"))?;
            writer = Some(opened);
        }
        writer
            .as_mut()
            .expect("opened just above")
            .write_frame(&frame, crypto)
            .map_err(|e| format!("JP2K write_frame failed: {e}"))?;
    }
    writer
        .expect("input_files is not empty")
        .finalize()
        .map(|_| ())
        .map_err(|e| format!("JP2K finalize failed: {e}"))
}

/// What a J2K picture wrap needs before its first frame arrives.
///
/// The [`MxfWrapOptions`] fields a frame-by-frame wrap cannot use are absent: it
/// has no input files, its essence type is J2K by construction, and it learns its
/// duration from the frames it is given.
#[derive(Debug)]
pub struct IncrementalWrapOptions {
    pub output: PathBuf,
    pub standard: MxfStandard,
    pub fps_num: u32,
    pub fps_den: u32,
    /// When set, the essence is AES-128 encrypted as it is written, exactly as
    /// [`MxfWrapOptions::encryption`] does it: the same key and key id produce
    /// the same essence either way.
    pub encryption: Option<MxfEncryption>,
    pub hdr: Option<asdcplib::jp2k::HdrMetadata>,
    pub asset_uuid: Option<[u8; 16]>,
}

/// A J2K picture MXF written one frame at a time, as an encoder finishes them,
/// instead of from a directory of codestreams.
///
/// The descriptor comes from the first frame's codestream header, so asdcplib
/// opens on the first [`Self::write_frame`] rather than in [`Self::new`].
/// Dropping this without [`Self::finish`] deletes the part-written file: asdcplib
/// writes the footer and the real duration at finalize, so an MXF that never got
/// there cannot be read.
pub struct IncrementalJ2kWrap {
    output: PathBuf,
    standard: MxfStandard,
    fps_num: u32,
    fps_den: u32,
    hdr: Option<asdcplib::jp2k::HdrMetadata>,
    info: asdcplib::WriterInfo,
    crypto: EssenceCrypto,
    writer: Option<J2kWriter>,
    frames_written: u64,
    finished: bool,
}

impl IncrementalJ2kWrap {
    pub fn new(options: IncrementalWrapOptions) -> Result<Self, String> {
        let mut info = make_writer_info(options.asset_uuid);
        check_as02_picture_colour(options.standard, options.hdr.as_ref())?;
        let crypto = setup_encryption(&mut info, &options.encryption)?;
        Ok(Self {
            output: options.output,
            standard: options.standard,
            fps_num: options.fps_num,
            fps_den: options.fps_den,
            hdr: options.hdr,
            info,
            crypto,
            writer: None,
            frames_written: 0,
            finished: false,
        })
    }

    /// Append one codestream. Frames land in the essence in the order they are
    /// given here, so the caller owes them in presentation order.
    pub fn write_frame(&mut self, data: &[u8]) -> Result<(), String> {
        let header = check_j2k_codestream(
            data,
            self.standard,
            &format!("frame {}", self.frames_written),
        )?;
        if self.writer.is_none() {
            let desc = j2k_picture_descriptor(&header, data, self.fps_num, self.fps_den, 0)?;
            let mut writer = J2kWriter::new(self.standard);
            writer
                .open_write(
                    &self.output.to_string_lossy(),
                    &self.info,
                    &desc,
                    self.hdr.as_ref(),
                    MXF_HEADER_SIZE,
                )
                .map_err(|e| format!("JP2K open_write failed: {e}"))?;
            self.writer = Some(writer);
        }
        let writer = self.writer.as_mut().expect("opened just above");
        writer
            .write_frame(data, &mut self.crypto)
            .map_err(|e| format!("JP2K write_frame failed: {e}"))?;
        self.frames_written += 1;
        Ok(())
    }

    /// Write the footer and the duration, then hash the finished MXF.
    pub fn finish(mut self) -> Result<MxfTrackFile, String> {
        let Some(writer) = self.writer.as_mut() else {
            return Err("no frames reached the wrap".to_string());
        };
        writer
            .finalize()
            .map_err(|e| format!("JP2K finalize failed: {e}"))?;
        // close the file before hashing it
        self.writer = None;
        self.finished = true;
        let (hash, size) = compute_hash_and_size(&self.output);
        Ok(MxfTrackFile {
            uuid: uuid::Uuid::from_bytes(self.info.asset_uuid)
                .hyphenated()
                .to_string(),
            hash,
            size,
            duration: self.frames_written,
            path: self.output.clone(),
            success: true,
            error: String::new(),
        })
    }
}

impl Drop for IncrementalJ2kWrap {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // asdcplib closes the file when the writer goes, which has to happen
        // before the file can be removed on windows
        self.writer = None;
        let _ = std::fs::remove_file(&self.output);
    }
}

/// Frames arriving out of order, released as the contiguous run they form.
///
/// The in-process encoder hands frames to its writer in completion order off a
/// FIFO work queue, so arrival order runs ahead of index order by roughly the
/// queue depth plus the thread count.
/// `capacity` caps how far ahead: past that the wrap fails rather than holding an
/// unbounded number of frames, which is also what a frame that is never coming
/// looks like.
struct FrameReorderBuffer {
    next_index: u64,
    pending: std::collections::BTreeMap<u64, Vec<u8>>,
    capacity: usize,
}

impl FrameReorderBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            next_index: 0,
            pending: std::collections::BTreeMap::new(),
            capacity,
        }
    }

    /// Take one frame in.
    fn accept(&mut self, index: u64, data: Vec<u8>) -> Result<(), String> {
        if index < self.next_index {
            return Err(format!(
                "frame {index} arrived after the wrap moved past it to frame {}",
                self.next_index
            ));
        }
        if self.pending.contains_key(&index) {
            return Err(format!("frame {index} arrived twice"));
        }
        if self.pending.len() >= self.capacity {
            return Err(format!(
                "{} frames are held waiting for frame {}, which is not coming",
                self.pending.len(),
                self.next_index
            ));
        }
        self.pending.insert(index, data);
        Ok(())
    }

    /// The next frame in order, once it has arrived.
    fn take_next(&mut self) -> Option<Vec<u8>> {
        let data = self.pending.remove(&self.next_index)?;
        self.next_index += 1;
        Some(data)
    }

    /// The frame the run is waiting on, while frames behind it are held.
    fn stalled_on(&self) -> Option<u64> {
        (!self.pending.is_empty()).then_some(self.next_index)
    }
}

/// Frames the handoff to the wrap thread may hold. It blocks past this, so a disk
/// slower than the encoder cannot let the queue grow without limit.
const WRAP_QUEUE_FRAMES: usize = 8;

/// Frames the reorder buffer may hold while waiting for the next one in order.
///
/// How far arrival order really runs ahead is the encoder's queue depth plus its
/// thread count, and both follow the core count. This only has to be past that,
/// so that reaching it means a frame is never coming rather than that the machine
/// is a wide one.
fn wrap_reorder_capacity() -> usize {
    const PER_CORE: usize = 4;
    const MINIMUM: usize = 64;
    std::thread::available_parallelism()
        .map(|cores| cores.get() * PER_CORE)
        .unwrap_or(MINIMUM)
        .max(MINIMUM)
}

/// One frame for a wrap running alongside the encoder, or the request to finish.
enum WrapMessage {
    Frame { index: u64, data: Vec<u8> },
    Finalize { expected_frames: u64 },
}

/// The sending end of a wrap running alongside the encoder.
#[derive(Clone)]
pub struct J2kFrameSender(std::sync::mpsc::SyncSender<WrapMessage>);

impl J2kFrameSender {
    /// Hand one encoded frame to the wrap, in any order. Blocks while the wrap is
    /// behind, and errors once the wrap has stopped, which is how a failed wrap
    /// reaches the encoder.
    pub fn send(&self, index: u64, data: Vec<u8>) -> Result<(), String> {
        self.0
            .send(WrapMessage::Frame { index, data })
            .map_err(|_| format!("the MXF wrap stopped before frame {index}"))
    }
}

/// A J2K picture MXF written on its own thread while the encode still runs.
///
/// [`Self::sender`] hands out the end an encoder pushes finished codestreams
/// into, in any order; a [`FrameReorderBuffer`] here releases them to an
/// [`IncrementalJ2kWrap`] in index order, so no codestream is ever read back off
/// disk. [`Self::finish`] is the only path that leaves an MXF behind: dropping
/// this, or [`Self::abandon`], deletes the part-written file.
pub struct OverlappedJ2kWrap {
    sender: Option<std::sync::mpsc::SyncSender<WrapMessage>>,
    thread: Option<std::thread::JoinHandle<Result<Option<MxfTrackFile>, String>>>,
}

impl OverlappedJ2kWrap {
    pub fn start(options: IncrementalWrapOptions) -> Result<Self, String> {
        let mut wrap = IncrementalJ2kWrap::new(options)?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(WRAP_QUEUE_FRAMES);
        let thread = std::thread::spawn(move || {
            let mut buffer = FrameReorderBuffer::new(wrap_reorder_capacity());
            let mut expected_frames = None;
            for message in receiver {
                match message {
                    WrapMessage::Frame { index, data } => {
                        buffer.accept(index, data)?;
                        while let Some(frame) = buffer.take_next() {
                            wrap.write_frame(&frame)?;
                        }
                    }
                    WrapMessage::Finalize { expected_frames: n } => {
                        expected_frames = Some(n);
                        break;
                    }
                }
            }
            // nobody asked for an MXF: the wrap drops unfinished and takes the
            // part-written file with it
            let Some(expected_frames) = expected_frames else {
                return Ok(None);
            };
            if let Some(index) = buffer.stalled_on() {
                return Err(format!("frame {index} never reached the wrap"));
            }
            if wrap.frames_written != expected_frames {
                return Err(format!(
                    "the wrap got {} of the {expected_frames} encoded frames",
                    wrap.frames_written
                ));
            }
            wrap.finish().map(Some)
        });
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    pub fn sender(&self) -> J2kFrameSender {
        J2kFrameSender(
            self.sender
                .clone()
                .expect("the sender is only dropped by finish or abandon"),
        )
    }

    /// Stop taking frames, write the footer and hash the MXF. `expected_frames`
    /// is what the encoder says it produced: a wrap holding any other number
    /// wrote the wrong essence, so it fails and deletes the file instead.
    pub fn finish(mut self, expected_frames: u64) -> Result<MxfTrackFile, String> {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(WrapMessage::Finalize { expected_frames });
        }
        match self.join()? {
            Some(track) => Ok(track),
            None => Err("the MXF wrap stopped before it was asked to finish".to_string()),
        }
    }

    /// Give up on the MXF and delete it, handing back the wrap's own error where
    /// it had one. This is the cancel and encode-failure path: a wrap that failed
    /// first explains more than the encoder's "the wrap stopped".
    pub fn abandon(&mut self) -> Option<String> {
        self.sender = None;
        self.join().err()
    }

    fn join(&mut self) -> Result<Option<MxfTrackFile>, String> {
        let Some(handle) = self.thread.take() else {
            return Ok(None);
        };
        handle
            .join()
            .unwrap_or_else(|_| Err("the MXF wrap thread panicked".to_string()))
    }
}

impl Drop for OverlappedJ2kWrap {
    fn drop(&mut self) {
        self.sender = None;
        let _ = self.join();
    }
}

/// The audio parameters and PCM payload location parsed from a WAV file.
#[derive(Debug)]
struct WavFormat {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    /// Byte offset and length of the `data` chunk payload.
    data_offset: u64,
    data_len: u64,
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// A `fmt ` chunk is 16 to 40 bytes, so a larger one is a corrupt header.
const MAXIMUM_FMT_CHUNK_BYTES: u32 = 4096;

const RIFF_HEADER_BYTES: usize = 12;
const CHUNK_HEADER_BYTES: usize = 8;

fn le_u16(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn le_u32(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

/// Parse a RIFF/WAVE header: read the `fmt ` chunk and locate the `data` chunk,
/// seeking past every chunk body so a feature-length file is never read here.
///
/// Only linear PCM is accepted (tag 1, or WAVE_FORMAT_EXTENSIBLE whose subformat
/// is PCM). Anything malformed or non-PCM is an error rather than a wrong MXF.
fn parse_wav<R: std::io::Read + std::io::Seek>(reader: &mut R) -> Result<WavFormat, String> {
    use std::io::SeekFrom;

    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("cannot size the WAV: {e}"))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("cannot seek the WAV: {e}"))?;

    let mut riff = [0u8; RIFF_HEADER_BYTES];
    if reader.read_exact(&mut riff).is_err() || &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (tag, channels, rate, bits)
    let mut data_chunk: Option<(u64, u64)> = None;

    // Chunks start after the 12-byte RIFF/WAVE header; each is an 8-byte header
    // (4-byte id + 4-byte LE size) followed by size bytes, padded to even.
    let mut pos = RIFF_HEADER_BYTES as u64;
    let mut chunk_header = [0u8; CHUNK_HEADER_BYTES];
    while pos + CHUNK_HEADER_BYTES as u64 <= file_len {
        reader
            .seek(SeekFrom::Start(pos))
            .map_err(|e| format!("cannot seek the WAV: {e}"))?;
        reader
            .read_exact(&mut chunk_header)
            .map_err(|e| format!("cannot read a WAV chunk header: {e}"))?;
        let id = &chunk_header[0..4];
        let size = le_u32(&chunk_header, 4);
        let body = pos + CHUNK_HEADER_BYTES as u64;
        if body + size as u64 > file_len {
            return Err(format!(
                "chunk '{}' claims {size} bytes past end of file",
                String::from_utf8_lossy(id)
            ));
        }

        if id == b"fmt " {
            if size < 16 {
                return Err("fmt chunk is too short".into());
            }
            if size > MAXIMUM_FMT_CHUNK_BYTES {
                return Err(format!("fmt chunk claims {size} bytes"));
            }
            let mut chunk = vec![0u8; size as usize];
            reader
                .read_exact(&mut chunk)
                .map_err(|e| format!("cannot read the fmt chunk: {e}"))?;
            let mut tag = le_u16(&chunk, 0);
            let channels = le_u16(&chunk, 2);
            let sample_rate = le_u32(&chunk, 4);
            let bits = le_u16(&chunk, 14);
            // WAVE_FORMAT_EXTENSIBLE stores the real tag in the SubFormat GUID.
            if tag == WAVE_FORMAT_EXTENSIBLE {
                if size < 40 {
                    return Err("extensible fmt chunk is too short for a SubFormat".into());
                }
                tag = le_u16(&chunk, 24);
            }
            fmt = Some((tag, channels, sample_rate, bits));
        } else if id == b"data" {
            data_chunk = Some((body, size as u64));
        }

        pos = body + size as u64 + (size & 1) as u64;
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
    use std::io::{Read, Seek};

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

    let mut wav_file = match std::fs::File::open(&opts.input_files[0]) {
        Ok(file) => std::io::BufReader::new(file),
        Err(e) => {
            return MxfTrackFile {
                error: format!("failed to read WAV: {e}"),
                ..Default::default()
            };
        }
    };

    // Parse the real RIFF/WAVE header instead of assuming 5.1/24-bit/48k.
    let wav = match parse_wav(&mut wav_file) {
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

    let num_frames = wav.data_len.checked_div(frame_size as u64).unwrap_or(0) as u32;

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
    if let Err(e) = writer.open_write(&output_str, &info, &desc, opts.mca_config.as_ref(), 16384) {
        return MxfTrackFile {
            error: format!("PCM open_write failed: {e}"),
            ..Default::default()
        };
    }

    if let Err(e) = wav_file.seek(std::io::SeekFrom::Start(wav.data_offset)) {
        return MxfTrackFile {
            error: format!("failed to read WAV: {e}"),
            ..Default::default()
        };
    }
    let mut frame = vec![0u8; frame_size as usize];
    for _ in 0..num_frames {
        if let Err(e) = wav_file.read_exact(&mut frame) {
            return MxfTrackFile {
                error: format!("failed to read WAV: {e}"),
                ..Default::default()
            };
        }
        if let Err(e) = writer.write_frame(&frame, &mut crypto) {
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

/// The id a timed-text document declares: the DCST `<Id>`, or the Interop
/// `SubtitleID` written either as an element or as an attribute on the
/// `DCSubtitle` root. ST 429-5 makes this the MXF ResourceID, which is what
/// tells a player the document apart from the track file carrying it.
fn timed_text_document_id(xml: &str) -> Option<[u8; 16]> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut in_id_element = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let local = qname.local_name();
                let name = String::from_utf8_lossy(local.as_ref()).into_owned();
                if name == "DCSubtitle"
                    && let Some(id) = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == b"SubtitleID")
                        .and_then(|a| String::from_utf8(a.value.into_owned()).ok())
                        .and_then(|value| parse_document_uuid(&value))
                {
                    return Some(id);
                }
                in_id_element = name == "Id" || name == "SubtitleID";
            }
            Ok(Event::Text(e)) if in_id_element => {
                return parse_document_uuid(&e.unescape().ok()?);
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// A document id written either as a `urn:uuid:` or as the bare uuid Interop
/// uses.
fn parse_document_uuid(text: &str) -> Option<[u8; 16]> {
    let trimmed = text.trim();
    let hex = trimmed.strip_prefix("urn:uuid:").unwrap_or(trimmed);
    uuid::Uuid::parse_str(hex).ok().map(|id| *id.as_bytes())
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

    // ST 429-5 makes the DCST document id the ResourceID; IMSC carries no such
    // id, so an AS-02 wrap without one keeps the track file id there as before
    let document_id = match (timed_text_document_id(&xml_data), opts.standard) {
        (Some(id), _) => Some(id),
        (None, MxfStandard::As02) => None,
        (None, MxfStandard::AsDcp) => {
            return MxfTrackFile {
                error: format!(
                    "timed text {} declares no document id: ST 429-5 makes the DCST <Id> (Interop SubtitleID) the MXF ResourceID",
                    opts.input_files[0].display()
                ),
                ..Default::default()
            };
        }
    };

    let fps = opts.fps_num as f64 / opts.fps_den.max(1) as f64;
    let last_cue_frames = crate::subtitle_retime::subtitle_end_time_seconds(&xml_data, fps)
        .map(|end_secs| (end_secs * fps).ceil() as u32);
    let duration_frames = match opts.timed_text_duration_frames {
        Some(declared) => {
            if let Some(cue_end) = last_cue_frames
                && cue_end > declared
            {
                return MxfTrackFile {
                    error: format!(
                        "timed text {} has a cue ending at frame {cue_end}, past the declared duration of {declared} frames",
                        opts.input_files[0].display()
                    ),
                    ..Default::default()
                };
            }
            declared
        }
        None => {
            let Some(frames) = last_cue_frames else {
                return MxfTrackFile {
                    error: format!(
                        "cannot determine subtitle duration: no parsable end/TimeOut timing in {}",
                        opts.input_files[0].display()
                    ),
                    ..Default::default()
                };
            };
            frames
        }
    };

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
    // asdcplib writes the descriptor's AssetID as the ResourceID of the
    // timed-text resource, which ST 429-5 requires to be the document's own id
    let desc = asdcplib::timed_text::TimedTextDescriptor {
        edit_rate: asdcplib::Rational::new(opts.fps_num as i32, opts.fps_den as i32),
        container_duration: duration_frames,
        asset_id: document_id.unwrap_or(info.asset_uuid),
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
        container_duration: opts.input_files.len() as u32,
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

    // one frame is held at a time, as in the picture wrap
    for path in &opts.input_files {
        let frame = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                return MxfTrackFile {
                    error: format!("failed to read {}: {e}", path.display()),
                    ..Default::default()
                };
            }
        };
        let (enc, hmac) = crypto.contexts();
        if let Err(e) = writer.write_frame(&frame, enc, hmac) {
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
        duration: opts.input_files.len() as u64,
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

    let read_frame = |path: &std::path::Path| -> Result<(Vec<u8>, crate::j2k::J2kHeader), String> {
        let data =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let Some(header) = crate::j2k::parse_j2k_header(&data) else {
            return Err(format!("invalid JPEG 2000 codestream: {}", path.display()));
        };
        crate::j2k::validate_dci_header(&header).map_err(|error| {
            format!(
                "invalid DCI JPEG 2000 codestream: {error}: {}",
                path.display()
            )
        })?;
        Ok((data, header))
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

    // one pair of codestreams is held at a time, as in the monoscopic wrap
    let mut writer: Option<asdcplib::jp2k::StereoMxfWriter> = None;
    let output_str = opts.output.to_string_lossy().to_string();
    for (left_path, right_path) in opts.left_files.iter().zip(opts.right_files.iter()) {
        let (left, header) = match read_frame(left_path) {
            Ok(frame) => frame,
            Err(error) => {
                return MxfTrackFile {
                    error,
                    ..Default::default()
                };
            }
        };
        let (right, _) = match read_frame(right_path) {
            Ok(frame) => frame,
            Err(error) => {
                return MxfTrackFile {
                    error,
                    ..Default::default()
                };
            }
        };
        if writer.is_none() {
            // container_duration counts stereo frame pairs, not individual eye writes.
            let desc = match j2k_picture_descriptor(
                &header,
                &left,
                opts.fps_num,
                opts.fps_den,
                opts.left_files.len() as u32,
            ) {
                Ok(desc) => desc,
                Err(error) => {
                    return MxfTrackFile {
                        error,
                        ..Default::default()
                    };
                }
            };
            let mut opened = asdcplib::jp2k::StereoMxfWriter::new();
            if let Err(e) = opened.open_write(&output_str, &info, &desc, 16384) {
                return MxfTrackFile {
                    error: format!("stereoscopic open_write failed: {e}"),
                    ..Default::default()
                };
            }
            writer = Some(opened);
        }
        let writer = writer.as_mut().expect("opened just above");
        let (e, h) = crypto.contexts();
        if let Err(err) = writer.write_frame(&left, StereoscopicPhase::Left, e, h) {
            return MxfTrackFile {
                error: format!("stereoscopic write_frame (left) failed: {err}"),
                ..Default::default()
            };
        }
        let (e, h) = crypto.contexts();
        if let Err(err) = writer.write_frame(&right, StereoscopicPhase::Right, e, h) {
            return MxfTrackFile {
                error: format!("stereoscopic write_frame (right) failed: {err}"),
                ..Default::default()
            };
        }
    }

    let mut writer = writer.expect("left_files is not empty");
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
        duration: opts.left_files.len() as u64,
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
        let f = parse_wav(&mut std::io::Cursor::new(&wav)).expect("parse");
        assert_eq!(f.channels, 2);
        assert_eq!(f.sample_rate, 44100);
        assert_eq!(f.bits_per_sample, 16);
        assert_eq!(f.data_len, 2 * 2 * 100);
        let data_offset = f.data_offset as usize;
        assert_eq!(&wav[data_offset..data_offset + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn parse_wav_rejects_malformed_and_non_pcm() {
        assert!(parse_wav(&mut std::io::Cursor::new(b"not a wav at all")).is_err());

        // Float (tag 3) is not linear PCM.
        let mut wav = make_wav(2, 48000, 32, 10);
        wav[20..22].copy_from_slice(&3u16.to_le_bytes());
        let err = parse_wav(&mut std::io::Cursor::new(&wav)).expect_err("float must be rejected");
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
            timed_text_duration_frames: None,
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
            timed_text_duration_frames: None,
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

    /// IMF 2K, main level 4 sub level 2: an Rsiz an AS-02 picture wrap accepts.
    const IMF_2K_RSIZ: u16 = 0x0424;

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
            timed_text_duration_frames: None,
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
    fn the_reorder_buffer_holds_a_frame_until_the_ones_before_it_arrive() {
        let mut buffer = FrameReorderBuffer::new(8);

        buffer.accept(2, vec![2]).unwrap();
        buffer.accept(1, vec![1]).unwrap();
        assert!(
            buffer.take_next().is_none(),
            "frame 0 has not arrived, so nothing can be written yet"
        );
        assert_eq!(buffer.stalled_on(), Some(0));

        buffer.accept(0, vec![0]).unwrap();
        let released: Vec<Vec<u8>> = std::iter::from_fn(|| buffer.take_next()).collect();
        assert_eq!(
            released,
            vec![vec![0], vec![1], vec![2]],
            "the whole contiguous run should come out in index order"
        );
        assert_eq!(buffer.stalled_on(), None, "nothing is held any more");
    }

    /// A frame that never arrives is how an encode that failed mid-way looks from
    /// the wrap's side, and the essence would be short if the wrap wrote what it
    /// had. It has to be caught by the frames piling up behind the gap.
    #[test]
    fn the_reorder_buffer_refuses_more_frames_than_it_can_hold_behind_a_gap() {
        let mut buffer = FrameReorderBuffer::new(3);
        for index in 1..=3 {
            buffer.accept(index, vec![index as u8]).unwrap();
        }
        let error = buffer
            .accept(4, vec![4])
            .expect_err("frame 0 is never coming");
        assert!(error.contains("waiting for frame 0"), "{error}");
        assert_eq!(buffer.stalled_on(), Some(0));
    }

    #[test]
    fn the_reorder_buffer_refuses_a_frame_it_cannot_place() {
        let mut buffer = FrameReorderBuffer::new(8);
        buffer.accept(0, vec![0]).unwrap();
        let error = buffer
            .accept(0, vec![0])
            .expect_err("frame 0 arrived twice");
        assert!(error.contains("twice"), "{error}");

        buffer.take_next().expect("frame 0");
        let error = buffer
            .accept(0, vec![0])
            .expect_err("frame 0 is already in the essence");
        assert!(error.contains("moved past it"), "{error}");
    }

    /// One codestream per frame, each carrying its index, so a wrap that reorders
    /// or drops a frame cannot pass.
    fn indexed_frames(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|index| {
                let mut frame = synthetic_j2k();
                frame.extend_from_slice(format!("FRAME{index:04}").as_bytes());
                frame
            })
            .collect()
    }

    /// Read every frame of a J2K MXF back through asdcplib, decrypting under
    /// `key` when the essence is encrypted.
    fn read_j2k_essence(path: &std::path::Path, key: Option<[u8; 16]>) -> Vec<Vec<u8>> {
        let mut reader = asdcplib::jp2k::MxfReader::new();
        reader.open_read(&path.to_string_lossy()).unwrap();
        let frames = reader.picture_descriptor().unwrap().container_duration;
        let mut contexts = key.map(|k| read_contexts(&k));
        (0..frames)
            .map(|index| {
                let mut buf = vec![0u8; 1 << 20];
                let (dec, hmac) = match contexts.as_mut() {
                    Some((dec, hmac)) => (Some(dec), Some(hmac)),
                    None => (None, None),
                };
                let read = reader.read_frame(index, &mut buf, dec, hmac).unwrap();
                buf.truncate(read);
                buf
            })
            .collect()
    }

    fn overlapped_opts(output: std::path::PathBuf) -> IncrementalWrapOptions {
        IncrementalWrapOptions {
            output,
            standard: MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            encryption: None,
            hdr: None,
            asset_uuid: None,
        }
    }

    /// The overlapped wrap has to produce the essence the batch wrap does, even
    /// when the encoder finishes the frames out of order, or the two paths make
    /// different DCPs.
    #[test]
    fn an_overlapped_wrap_writes_the_same_essence_as_a_batch_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let frames = indexed_frames(5);

        let batch_inputs: Vec<std::path::PathBuf> = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let path = dir.path().join(format!("frame_{index:08}.j2c"));
                std::fs::write(&path, frame).unwrap();
                path
            })
            .collect();
        let batch_out = dir.path().join("batch.mxf");
        let batch = mxf_wrap(&wrap_opts(
            EssenceType::J2k,
            batch_inputs,
            batch_out.clone(),
            None,
        ));
        assert!(batch.success, "batch wrap failed: {}", batch.error);

        let overlapped_out = dir.path().join("overlapped.mxf");
        let wrap = OverlappedJ2kWrap::start(overlapped_opts(overlapped_out.clone())).unwrap();
        let sender = wrap.sender();
        // the encoder hands frames over as they finish, not in order
        for index in [3usize, 1, 0, 4, 2] {
            sender.send(index as u64, frames[index].clone()).unwrap();
        }
        drop(sender);
        let track = wrap.finish(frames.len() as u64).expect("overlapped wrap");

        assert_eq!(track.duration, frames.len() as u64);
        assert_eq!(
            read_j2k_essence(&overlapped_out, None),
            frames,
            "the overlapped wrap should carry every frame in index order"
        );
        assert_eq!(
            read_j2k_essence(&overlapped_out, None),
            read_j2k_essence(&batch_out, None),
            "the two wraps should differ only in their ids and timestamps"
        );
    }

    /// Encryption is per frame, so it has to survive being fed one frame at a
    /// time: the essence must come back under the caller's key and must not be
    /// findable in the file.
    #[test]
    fn an_overlapped_encrypted_wrap_reads_back_under_the_caller_key() {
        let dir = tempfile::tempdir().unwrap();
        let frames = indexed_frames(3);
        let output = dir.path().join("enc.mxf");
        let key = [0x11u8; 16];

        let wrap = OverlappedJ2kWrap::start(IncrementalWrapOptions {
            encryption: Some(MxfEncryption {
                content_key: key,
                key_id: [0x22; 16],
            }),
            ..overlapped_opts(output.clone())
        })
        .unwrap();
        let sender = wrap.sender();
        for index in [2usize, 0, 1] {
            sender.send(index as u64, frames[index].clone()).unwrap();
        }
        drop(sender);
        wrap.finish(frames.len() as u64).expect("encrypted wrap");

        assert_eq!(
            read_j2k_essence(&output, Some(key)),
            frames,
            "every frame should decrypt to the codestream that went in"
        );
        assert!(
            !contains(&std::fs::read(&output).unwrap(), PLAINTEXT_TAG),
            "essence tag survived into the encrypted MXF: essence was not encrypted"
        );
    }

    #[test]
    fn an_abandoned_overlapped_wrap_leaves_no_mxf() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("cancelled.mxf");
        let frames = indexed_frames(2);

        let mut wrap = OverlappedJ2kWrap::start(overlapped_opts(output.clone())).unwrap();
        let sender = wrap.sender();
        sender.send(0, frames[0].clone()).unwrap();
        // the file has to exist before the abandon, or its deletion proves nothing
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !output.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(output.exists(), "the wrap never opened the MXF");
        drop(sender);
        assert_eq!(wrap.abandon(), None, "abandoning is not itself an error");
        assert!(
            !output.exists(),
            "an MXF with no footer cannot be read, so it must not be left behind"
        );
    }

    /// The wrap counts the frames it wrote; the encoder counts the frames it
    /// encoded. An MXF is only worth keeping when the two agree.
    #[test]
    fn an_overlapped_wrap_refuses_to_finish_short_of_the_encoded_frames() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("short.mxf");
        let frames = indexed_frames(2);

        let wrap = OverlappedJ2kWrap::start(overlapped_opts(output.clone())).unwrap();
        let sender = wrap.sender();
        sender.send(0, frames[0].clone()).unwrap();
        drop(sender);
        let error = wrap.finish(2).expect_err("only one frame reached the wrap");
        assert!(error.contains("1 of the 2 encoded frames"), "{error}");
        assert!(!output.exists(), "the short MXF must be deleted");
    }

    #[test]
    fn an_overlapped_wrap_refuses_a_codestream_the_standard_cannot_carry() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("bad.mxf");

        let wrap = OverlappedJ2kWrap::start(overlapped_opts(output.clone())).unwrap();
        let sender = wrap.sender();
        sender.send(0, synthetic_j2k_with_profile(0)).unwrap();
        drop(sender);
        let error = wrap.finish(1).expect_err("profile 0 is not DCI");
        assert!(error.contains("not a DCI JPEG 2000 profile"), "{error}");
        assert!(!output.exists());
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
            mca_config: Some(McaConfig {
                labels: mca.unwrap(),
                spoken_language: Some("fr-CA".to_string()),
            }),
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
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
        let subdescriptors = reader
            .mca_label_subdescriptors()
            .expect("read mca subdescriptors");
        assert_eq!(
            subdescriptors.len(),
            7,
            "six channels plus the soundfield group"
        );
        for label in &subdescriptors {
            assert_eq!(label.spoken_language.as_deref(), Some("fr-CA"));
        }
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
            mca_config: Some(McaConfig {
                labels: "51(L,R,C,LFE,Ls,Rs)".to_string(),
                spoken_language: None,
            }),
            resource_ids: vec![],
            hdr: None,
            asset_uuid: None,
            timed_text_duration_frames: None,
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
            timed_text_duration_frames: None,
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

    /// The `<Id>` the `DCST` fixture declares, which ST 429-5 makes the MXF
    /// ResourceID.
    const DCST_DOCUMENT_ID: &str = "11111111-1111-1111-1111-111111111111";

    /// ST 429-5 wants three distinct ids on a timed-text asset: the track file's
    /// own, the document's, and the ResourceID naming the document inside the
    /// MXF, which repeats the document's. libdcp and dcpdoctor both reject a wrap
    /// that reuses the file id as the ResourceID.
    #[test]
    fn timed_text_resource_id_is_the_document_id_not_the_file_id() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sub.mxf");
        let mut opts = timed_text_with_resources(dir.path(), out.clone(), None);
        let asset_uuid = *uuid::Uuid::parse_str("33333333-3333-3333-3333-333333333333")
            .unwrap()
            .as_bytes();
        opts.asset_uuid = Some(asset_uuid);

        let result = mxf_wrap(&opts);
        assert!(result.success, "timed text wrap failed: {}", result.error);

        let mut reader = asdcplib::timed_text::MxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        let descriptor = reader.descriptor().unwrap();
        let info = reader.writer_info().unwrap();

        let document_id = *uuid::Uuid::parse_str(DCST_DOCUMENT_ID).unwrap().as_bytes();
        assert_eq!(
            descriptor.asset_id, document_id,
            "the ResourceID repeats the document's Id"
        );
        assert_eq!(
            info.asset_uuid, asset_uuid,
            "the track file keeps the caller's asset id"
        );
        assert_ne!(
            descriptor.asset_id, info.asset_uuid,
            "the ResourceID and the asset id are different things"
        );
    }

    /// Without a document id there is nothing to write as the ResourceID, and a
    /// wrap that invented one would name a document that does not exist.
    #[test]
    fn timed_text_without_a_document_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("sub.xml");
        std::fs::write(&xml, DCST.replace(DCST_DOCUMENT_ID, "not-a-uuid")).unwrap();
        let out = dir.path().join("sub.mxf");
        let result = mxf_wrap(&wrap_opts(
            EssenceType::TimedText,
            vec![xml],
            out.clone(),
            None,
        ));
        assert!(!result.success, "a document with no id must not wrap");
        assert!(
            result.error.contains("declares no document id"),
            "the error names the missing id, got: {}",
            result.error
        );
        assert!(!out.exists(), "no MXF is left behind");
    }

    /// A DCST with no cues at all, which is what a reel of a subtitled
    /// composition carries when none of the cues fall inside it.
    const EMPTY_DCST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<dcst:SubtitleReel xmlns:dcst=\"http://www.smpte-ra.org/schemas/428-7/2010/DCST\">\n\
  <dcst:Id>urn:uuid:11111111-1111-1111-1111-111111111111</dcst:Id>\n\
  <dcst:ContentTitleText>t</dcst:ContentTitleText>\n\
  <dcst:IssueDate>2020-01-01T00:00:00+00:00</dcst:IssueDate>\n\
  <dcst:EditRate>24 1</dcst:EditRate>\n\
  <dcst:TimeCodeRate>24</dcst:TimeCodeRate>\n\
  <dcst:SubtitleList/>\n\
</dcst:SubtitleReel>\n";

    /// A reel length in frames, longer than the `DCST` fixture's last cue.
    const DECLARED_DURATION_FRAMES: u32 = 240;

    /// A reel with no cues still needs a subtitle asset spanning it, so the
    /// caller's duration is what gets written and the missing cue timing is no
    /// longer a reason to refuse.
    #[test]
    fn timed_text_with_no_cues_wraps_at_the_declared_duration() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("sub.xml");
        std::fs::write(&xml, EMPTY_DCST).unwrap();
        let out = dir.path().join("sub.mxf");
        let mut opts = wrap_opts(EssenceType::TimedText, vec![xml], out.clone(), None);
        opts.timed_text_duration_frames = Some(DECLARED_DURATION_FRAMES);

        let result = mxf_wrap(&opts);
        assert!(
            result.success,
            "empty subtitle wrap failed: {}",
            result.error
        );
        assert_eq!(result.duration, DECLARED_DURATION_FRAMES as u64);

        let mut reader = asdcplib::timed_text::MxfReader::new();
        reader.open_read(&out.to_string_lossy()).unwrap();
        assert_eq!(
            reader.descriptor().unwrap().container_duration,
            DECLARED_DURATION_FRAMES,
            "the essence spans the reel the caller declared"
        );

        let mut buf = vec![0u8; RESOURCE_READ_BUFFER_LEN];
        let n = reader
            .read_timed_text_resource(&mut buf, None, None)
            .unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("<dcst:SubtitleList/>"),
            "the empty document round-trips"
        );
    }

    /// An asset shorter than its own cues would drop them at playback, so the
    /// caller's duration and the document have to agree.
    #[test]
    fn timed_text_cue_past_the_declared_duration_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("sub.xml");
        std::fs::write(&xml, DCST).unwrap();
        let out = dir.path().join("sub.mxf");
        let mut opts = wrap_opts(EssenceType::TimedText, vec![xml], out.clone(), None);
        // DCST's only cue ends at 00:00:04:00, frame 96 at 24 fps
        opts.timed_text_duration_frames = Some(48);

        let result = mxf_wrap(&opts);
        assert!(!result.success, "a cue past the reel must not wrap");
        assert!(
            result.error.contains("96") && result.error.contains("48"),
            "the error names both frame counts, got: {}",
            result.error
        );
        assert!(!out.exists(), "no MXF is left behind");
    }

    /// Interop spells the document id `SubtitleID`, both as an element and as a
    /// root attribute, and a wrap of either has to find it.
    #[test]
    fn interop_subtitle_id_is_read_as_the_document_id() {
        let expected = *uuid::Uuid::parse_str(DCST_DOCUMENT_ID).unwrap().as_bytes();
        let element = format!(
            "<DCSubtitle Version=\"1.0\"><SubtitleID>{DCST_DOCUMENT_ID}</SubtitleID></DCSubtitle>"
        );
        assert_eq!(timed_text_document_id(&element), Some(expected));

        let attribute = format!("<DCSubtitle SubtitleID=\"{DCST_DOCUMENT_ID}\"/>");
        assert_eq!(timed_text_document_id(&attribute), Some(expected));

        assert_eq!(
            timed_text_document_id("<DCSubtitle Version=\"1.0\"/>"),
            None
        );
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

    /// Key material must not outlive the wrap in freed memory. Reading a dropped
    /// value is not something a test can do soundly, so this pins both halves:
    /// the type clears itself on drop, and what it clears is the content key.
    #[test]
    fn the_content_key_is_cleared_on_drop() {
        assert!(
            std::mem::needs_drop::<MxfEncryption>(),
            "MxfEncryption must run a Drop that clears its key"
        );

        let mut encryption = MxfEncryption {
            content_key: [0x7f; 16],
            key_id: [0x11; 16],
        };
        encryption.clear_content_key();
        assert_eq!(encryption.content_key, [0u8; 16], "content key is zeroed");
        assert_eq!(
            encryption.key_id, [0x11; 16],
            "the KeyId is not secret and has to survive"
        );
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

    /// Where cargo puts build output, which CARGO_TARGET_DIR moves.
    fn cargo_target_dir() -> std::path::PathBuf {
        match std::env::var_os("CARGO_TARGET_DIR") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        }
    }

    /// asdcplib's own asdcp-info. POSTKIT_ASDCP_INFO names one, otherwise the
    /// copy asdcplib-sys built into its cargo output directory.
    fn asdcp_info_tool() -> std::path::PathBuf {
        if let Some(path) = std::env::var_os("POSTKIT_ASDCP_INFO") {
            return std::path::PathBuf::from(path);
        }
        let name = format!("asdcp-info{}", std::env::consts::EXE_SUFFIX);
        let mut searched = Vec::new();
        for profile in ["debug", "release"] {
            let build = cargo_target_dir().join(profile).join("build");
            searched.push(build.join("asdcplib-sys-*").join("out/bin").join(&name));
            let Ok(entries) = std::fs::read_dir(&build) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("asdcplib-sys-")
                {
                    continue;
                }
                let tool = entry.path().join("out").join("bin").join(&name);
                if tool.is_file() {
                    return tool;
                }
            }
        }
        panic!(
            "no asdcp-info found. set POSTKIT_ASDCP_INFO, or build one where these look: {}",
            searched
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    /// Read the AssetUUID an MXF actually carries, using an independent
    /// asdcp-info binary rather than this crate's own reader. Returns None when
    /// the tool reports no AssetUUID (it refuses AS-02 files).
    fn asdcp_info_asset_uuid(tool: &std::path::Path, path: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new(tool)
            .arg("-i")
            .arg(path)
            .output()
            .unwrap_or_else(|error| panic!("could not run {}: {error}", tool.display()));
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.trim().strip_prefix("AssetUUID:"))
            .map(|v| v.trim().to_lowercase())
    }

    /// The one id an asset is known by: the AssetUUID inside the MXF, the uuid in
    /// the file name, and the id postkit reports for the CPL/PKL/ASSETMAP must all
    /// be the same. The MXF side is read back with an external asdcp-info binary,
    /// so postkit cannot agree with itself and pass.
    #[test]
    fn every_wrap_path_writes_the_caller_supplied_asset_uuid() {
        let tool = asdcp_info_tool();
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
    /// this asserts nothing the test itself chose.
    #[test]
    fn a_minted_asset_uuid_is_reported_as_the_mxf_carries_it() {
        let tool = asdcp_info_tool();
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
        std::fs::write(&frame, synthetic_j2k_with_profile(IMF_2K_RSIZ)).unwrap();
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
            opts.hdr = Some(rec709_sdr_picture_colour());
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
