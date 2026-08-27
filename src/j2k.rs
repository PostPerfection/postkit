use serde::{Deserialize, Serialize};
use std::path::Path;

/// JPEG 2000 marker codes.
const SOC: u16 = 0xFF4F; // Start of codestream
const SIZ: u16 = 0xFF51; // Image and tile size
const COD: u16 = 0xFF52; // Coding style default
const QCD: u16 = 0xFF5C; // Quantization default
const TLM: u16 = 0xFF55; // Tile-part lengths
const POC: u16 = 0xFF5F; // Progression order change
const SOT: u16 = 0xFF90; // Start of tile-part
const SOD: u16 = 0xFF93; // Start of data
const EOC: u16 = 0xFFD9; // End of codestream

/// JPEG 2000 codestream header info.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct J2kHeader {
    /// Image width
    pub width: u32,
    /// Image height
    pub height: u32,
    /// Number of components
    pub num_components: u16,
    /// Bit depth per component (from Ssiz)
    pub bit_depth: u8,
    /// Signed components
    pub is_signed: bool,
    /// Profile (RSIZ value)
    pub profile: u16,
    /// Tile width
    pub tile_width: u32,
    /// Tile height
    pub tile_height: u32,
    /// Number of decomposition levels (from COD)
    pub num_decomp_levels: u8,
    /// Progression order
    pub progression_order: u8,
    /// Number of quality layers
    pub num_layers: u16,
    /// Bit depth per component (from each Ssiz), same order as the components
    pub bit_depths: Vec<u8>,
    /// Code-block width exponent (actual width = 2^(exp+2))
    pub codeblock_width_exp: u8,
    /// Code-block height exponent (actual height = 2^(exp+2))
    pub codeblock_height_exp: u8,
    /// Code-block width in samples
    pub codeblock_width: u32,
    /// Code-block height in samples
    pub codeblock_height: u32,
    /// Wavelet transform: true = 9-7 irreversible, false = 5-3 reversible
    pub irreversible_transform: bool,
    /// Multi-component transform present
    pub mct: bool,
    /// Guard bits declared in the QCD marker (top 3 bits of Sqcd)
    pub guard_bits: u8,
    /// TLM (tile-part lengths) marker present
    pub tlm_present: bool,
    /// POC (progression order change) marker present
    pub poc_present: bool,
    /// Number of tile-parts (SOT markers)
    pub tile_part_count: u32,
}

/// DCI compliance profile identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum J2kProfile {
    /// Rsiz 0, unrestricted
    None,
    /// Rsiz 3, 2K digital cinema
    Cinema2k,
    /// Rsiz 4, 4K digital cinema
    Cinema4k,
    /// Rsiz 5, scalable 2K digital cinema
    CinemaScalable2k,
    /// Rsiz 6, scalable 4K digital cinema
    CinemaScalable4k,
    /// Rsiz 7, cinema long-term storage
    CinemaLongTermStorage,
    /// Rsiz 0x0100 to 0x03ff, broadcast contribution. The low bits carry a main
    /// level this does not keep.
    Broadcast,
    /// Rsiz 0x0400 to 0x09ff, IMF. The low bits carry main and sub levels this
    /// does not keep.
    Imf,
    /// Rsiz this does not name
    Unknown(u16),
}

/// Rsiz carries the profile in its low 12 bits; the Part 2 extension bit is above.
const RSIZ_PROFILE_MASK: u16 = 0x0fff;

impl From<u16> for J2kProfile {
    /// Classify an Rsiz value per ISO/IEC 15444-1 Amd 1. These are the values
    /// grok's own `GRK_PROFILE_*` constants use, and what a DCP carries: a 2K
    /// picture is Rsiz 3, not 1.
    fn from(rsiz: u16) -> Self {
        match rsiz & RSIZ_PROFILE_MASK {
            0x0000 => J2kProfile::None,
            0x0003 => J2kProfile::Cinema2k,
            0x0004 => J2kProfile::Cinema4k,
            0x0005 => J2kProfile::CinemaScalable2k,
            0x0006 => J2kProfile::CinemaScalable4k,
            0x0007 => J2kProfile::CinemaLongTermStorage,
            0x0100..=0x03ff => J2kProfile::Broadcast,
            0x0400..=0x09ff => J2kProfile::Imf,
            other => J2kProfile::Unknown(other),
        }
    }
}

impl J2kProfile {
    /// Whether this is a digital cinema profile, so the samples are X'Y'Z'
    /// rather than the RGB or YCbCr an IMF or broadcast codestream carries.
    pub fn is_dci_cinema(self) -> bool {
        matches!(
            self,
            J2kProfile::Cinema2k
                | J2kProfile::Cinema4k
                | J2kProfile::CinemaScalable2k
                | J2kProfile::CinemaScalable4k
                | J2kProfile::CinemaLongTermStorage
        )
    }
}

/// The IMF profile a raster picks. Each one caps the picture size, and its Rsiz
/// base carries the main and sub level in the low byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImfProfile {
    /// Rsiz 0x0400 plus levels, up to 2048x1556
    Imf2k,
    /// Rsiz 0x0500 plus levels, up to 4096x3112
    Imf4k,
    /// Rsiz 0x0600 plus levels, up to 8192x6224
    Imf8k,
}

const IMF_2K_MAX_RASTER: (u32, u32) = (2048, 1556);
const IMF_4K_MAX_RASTER: (u32, u32) = (4096, 3112);
const IMF_8K_MAX_RASTER: (u32, u32) = (8192, 6224);

const IMF_2K_RSIZ_BASE: u16 = 0x0400;
const IMF_4K_RSIZ_BASE: u16 = 0x0500;
const IMF_8K_RSIZ_BASE: u16 = 0x0600;

impl ImfProfile {
    /// The smallest IMF profile that holds a `width` x `height` picture.
    pub fn for_raster(width: u32, height: u32) -> Result<Self, String> {
        for (profile, (max_width, max_height)) in [
            (ImfProfile::Imf2k, IMF_2K_MAX_RASTER),
            (ImfProfile::Imf4k, IMF_4K_MAX_RASTER),
            (ImfProfile::Imf8k, IMF_8K_MAX_RASTER),
        ] {
            if width <= max_width && height <= max_height {
                return Ok(profile);
            }
        }
        Err(format!(
            "{width}x{height} is past the {}x{} the IMF 8K profile allows",
            IMF_8K_MAX_RASTER.0, IMF_8K_MAX_RASTER.1
        ))
    }

    fn rsiz_base(self) -> u16 {
        match self {
            ImfProfile::Imf2k => IMF_2K_RSIZ_BASE,
            ImfProfile::Imf4k => IMF_4K_RSIZ_BASE,
            ImfProfile::Imf8k => IMF_8K_RSIZ_BASE,
        }
    }
}

/// The two levels an IMF Rsiz carries: the main level bounds the sample rate a
/// decoder must keep up with, the sub level bounds the bit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImfLevels {
    pub main_level: u8,
    pub sub_level: u8,
}

/// Main level 1 to 11, in megasamples per second.
const IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND: [u32; 11] =
    [65, 130, 195, 260, 520, 1200, 2400, 4800, 9600, 19200, 38400];

/// Sub level 1 to 9, in megabits per second.
const IMF_SUB_LEVEL_MEGABITS_PER_SECOND: [u32; 9] =
    [200, 400, 800, 1600, 3200, 6400, 12800, 25600, 51200];

/// The sample rate counts all three components of an IMF picture.
const IMF_COMPONENTS: f64 = 3.0;

/// Table A.53 pairs the levels: sub level 2 and up needs a main level two above
/// it, so a high bit rate can raise the main level past what the sample rate
/// alone asks for.
const IMF_MAIN_LEVEL_ABOVE_SUB_LEVEL: u8 = 2;

/// The lowest levels that cover a picture of this size and rate at this bit
/// rate. Refuses anything past level 11 or sub level 9 rather than declaring a
/// level the essence exceeds.
pub fn imf_levels(
    width: u32,
    height: u32,
    frame_rate: f64,
    max_bits_per_second: u64,
) -> Result<ImfLevels, String> {
    if frame_rate <= 0.0 {
        return Err(format!(
            "IMF levels need a positive frame rate, got {frame_rate}"
        ));
    }
    let megasamples_per_second =
        width as f64 * height as f64 * IMF_COMPONENTS * frame_rate / 1_000_000.0;
    let sample_rate_level = IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND
        .iter()
        .position(|&limit| megasamples_per_second <= limit as f64)
        .map(|index| index as u8 + 1)
        .ok_or_else(|| {
            format!(
                "{width}x{height} at {frame_rate} fps is {megasamples_per_second:.1} \
                 megasamples per second, past the {} of IMF main level {}",
                IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND.last().unwrap(),
                IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND.len()
            )
        })?;

    let megabits_per_second = max_bits_per_second as f64 / 1_000_000.0;
    let sub_level = IMF_SUB_LEVEL_MEGABITS_PER_SECOND
        .iter()
        .position(|&limit| megabits_per_second <= limit as f64)
        .map(|index| index as u8 + 1)
        .ok_or_else(|| {
            format!(
                "{megabits_per_second:.1} megabits per second is past the {} of IMF sub level {}",
                IMF_SUB_LEVEL_MEGABITS_PER_SECOND.last().unwrap(),
                IMF_SUB_LEVEL_MEGABITS_PER_SECOND.len()
            )
        })?;

    let main_level_the_sub_level_needs = if sub_level > 1 {
        sub_level + IMF_MAIN_LEVEL_ABOVE_SUB_LEVEL
    } else {
        1
    };
    let main_level = sample_rate_level.max(main_level_the_sub_level_needs);
    if main_level as usize > IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND.len() {
        return Err(format!(
            "sub level {sub_level} needs main level {main_level}, past the {} IMF allows",
            IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND.len()
        ));
    }

    Ok(ImfLevels {
        main_level,
        sub_level,
    })
}

/// The Rsiz an IMF codestream declares: the profile with its two levels in the
/// low byte, sub level above main level.
pub fn imf_rsiz(profile: ImfProfile, levels: ImfLevels) -> u16 {
    profile.rsiz_base() | ((levels.sub_level as u16) << 4) | levels.main_level as u16
}

/// Check the header fields DCP picture wrapping requires.
pub fn validate_dci_header(header: &J2kHeader) -> Result<(), String> {
    // a DCP carries the plain 2K or 4K cinema profile; the scalable and
    // long-term-storage ones are cinema too but SMPTE 429-4 does not wrap them
    let profile = J2kProfile::from(header.profile);
    let max_dimensions = match profile {
        J2kProfile::Cinema2k => (2048, 1080),
        J2kProfile::Cinema4k => (4096, 2160),
        _ => {
            return Err(format!(
                "RSIZ {:#06x} is not a DCI JPEG 2000 profile",
                header.profile
            ));
        }
    };

    if header.width == 0 || header.height == 0 {
        return Err("JPEG 2000 codestream has no image area".to_string());
    }
    if header.width > max_dimensions.0 || header.height > max_dimensions.1 {
        return Err(format!(
            "JPEG 2000 {} profile exceeds its {}x{} limit: {}x{}",
            match profile {
                J2kProfile::Cinema2k => "2K",
                _ => "4K",
            },
            max_dimensions.0,
            max_dimensions.1,
            header.width,
            header.height
        ));
    }
    if header.num_components != 3 {
        return Err(format!(
            "DCI JPEG 2000 requires 3 components, got {}",
            header.num_components
        ));
    }
    if header.bit_depth != 12 || header.is_signed {
        return Err(format!(
            "DCI JPEG 2000 requires unsigned 12-bit components, got {}-bit {}",
            header.bit_depth,
            if header.is_signed {
                "signed"
            } else {
                "unsigned"
            }
        ));
    }

    Ok(())
}

/// Per-frame bitrate measurement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameBitrate {
    pub frame_index: u64,
    /// Size of this frame's J2K codestream in bytes.
    pub size_bytes: u64,
    /// Bitrate in Mbps.
    pub bitrate_mbps: f64,
}

/// Bitrate analysis result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitrateAnalysis {
    pub frame_count: u64,
    pub avg_bitrate_mbps: f64,
    pub max_bitrate_mbps: f64,
    pub min_bitrate_mbps: f64,
    /// Max allowed per DCI spec
    pub dci_max_mbps: f64,
    /// Whether all frames are within DCI limit
    pub dci_compliant: bool,
    /// Frames exceeding the limit
    pub over_limit_frames: Vec<FrameBitrate>,
}

/// Parse a J2K codestream header from raw bytes.
pub fn parse_j2k_header(data: &[u8]) -> Option<J2kHeader> {
    if data.len() < 4 {
        return None;
    }

    let marker = u16::from_be_bytes([data[0], data[1]]);
    if marker != SOC {
        return None;
    }

    let mut hdr = J2kHeader::default();
    let mut pos = 2;

    while pos + 2 < data.len() {
        let marker = u16::from_be_bytes([data[pos], data[pos + 1]]);
        pos += 2;

        if marker == SOD || marker == EOC || marker == SOT {
            break;
        }

        if pos + 2 > data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        // a segment length below 2 is malformed and would underflow below
        if seg_len < 2 {
            break;
        }
        if pos + seg_len - 2 > data.len() {
            break;
        }

        let seg = &data[pos..pos + seg_len - 2];

        match marker {
            SIZ if seg.len() >= 36 => {
                hdr.profile = u16::from_be_bytes([seg[0], seg[1]]);
                let xsiz = u32::from_be_bytes([seg[2], seg[3], seg[4], seg[5]]);
                let ysiz = u32::from_be_bytes([seg[6], seg[7], seg[8], seg[9]]);
                let xosiz = u32::from_be_bytes([seg[10], seg[11], seg[12], seg[13]]);
                let yosiz = u32::from_be_bytes([seg[14], seg[15], seg[16], seg[17]]);
                // image area excludes the codestream origin offset
                hdr.width = xsiz.saturating_sub(xosiz);
                hdr.height = ysiz.saturating_sub(yosiz);
                // tile size at offset 18..26
                hdr.tile_width = u32::from_be_bytes([seg[18], seg[19], seg[20], seg[21]]);
                hdr.tile_height = u32::from_be_bytes([seg[22], seg[23], seg[24], seg[25]]);
                hdr.num_components = u16::from_be_bytes([seg[34], seg[35]]);
                if seg.len() > 36 {
                    let ssiz = seg[36];
                    hdr.is_signed = (ssiz & 0x80) != 0;
                    hdr.bit_depth = (ssiz & 0x7F) + 1;
                }
                // per-component Ssiz bytes start at 36, 3 bytes each (Ssiz, XRsiz, YRsiz)
                let mut cp = 36;
                for _ in 0..hdr.num_components {
                    if cp >= seg.len() {
                        break;
                    }
                    hdr.bit_depths.push((seg[cp] & 0x7F) + 1);
                    cp += 3;
                }
            }
            COD if seg.len() >= 5 => {
                hdr.progression_order = seg[1];
                hdr.num_layers = u16::from_be_bytes([seg[2], seg[3]]);
                if seg.len() > 4 {
                    hdr.mct = seg[4] != 0;
                }
                hdr.num_decomp_levels = seg[5];
                if seg.len() > 7 {
                    hdr.codeblock_width_exp = seg[6];
                    hdr.codeblock_height_exp = seg[7];
                    hdr.codeblock_width = 1u32 << (seg[6] + 2);
                    hdr.codeblock_height = 1u32 << (seg[7] + 2);
                }
                if seg.len() > 9 {
                    // 0 = 9-7 irreversible, 1 = 5-3 reversible
                    hdr.irreversible_transform = seg[9] == 0;
                }
            }
            QCD if !seg.is_empty() => {
                hdr.guard_bits = seg[0] >> 5;
            }
            TLM => hdr.tlm_present = true,
            POC => hdr.poc_present = true,
            _ => {}
        }

        pos += seg_len - 2;
    }

    hdr.tile_part_count = count_tile_parts(data);

    Some(hdr)
}

/// Count tile-parts by walking SOT segments from the first one, following each
/// Psot length. A Psot of 0 (allowed for a final tile-part) stops the walk.
fn count_tile_parts(data: &[u8]) -> u32 {
    // find the first SOT among the main-header marker segments
    let mut pos = 2; // skip SOC
    let lmh = loop {
        if pos + 4 > data.len() {
            return 0;
        }
        let marker = u16::from_be_bytes([data[pos], data[pos + 1]]);
        if marker == SOT {
            break pos;
        }
        if marker == SOD || marker == EOC {
            return 0;
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if seg_len < 2 {
            return 0;
        }
        pos += 2 + seg_len;
    };

    let mut count = 0u32;
    let mut pos = lmh;
    while pos + 12 <= data.len() && u16::from_be_bytes([data[pos], data[pos + 1]]) == SOT {
        // SOT: FF90, Lsot(2), Isot(2), Psot(4), TPsot(1), TNsot(1)
        let psot = u32::from_be_bytes([data[pos + 6], data[pos + 7], data[pos + 8], data[pos + 9]])
            as usize;
        count += 1;
        if psot == 0 {
            break;
        }
        pos += psot;
    }
    count
}

/// DCI maximum bit rate for J2K picture essence, in Mb/s.
///
/// No 4K branch on purpose: DCSS 4.3.3 caps a 4K frame at the same 1,302,083
/// bytes as 24 fps 2K, and the 500 widely used for 4K has no source.
pub const DCI_MAX_BITRATE_MBPS: f64 = 250.0;

/// The DCI per-frame codestream byte cap at `fps`: 250 Mb/s spread over one
/// second of frames, 1,302,083 bytes at 24 fps.
pub fn dci_codestream_byte_cap(fps: u32) -> u64 {
    31_250_000 / fps.max(1) as u64
}

/// Analyse bitrate of a sequence of J2K files.
pub fn analyse_bitrate(j2k_files: &[std::path::PathBuf], fps: f64) -> BitrateAnalysis {
    let mut frames = Vec::with_capacity(j2k_files.len());
    let mut total_bits = 0u64;
    let mut max_bps = 0.0f64;
    let mut min_bps = f64::MAX;
    let mut over_limit = Vec::new();

    for (i, path) in j2k_files.iter().enumerate() {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let bits = size * 8;
        let mbps = (bits as f64) / 1_000_000.0 * fps;
        total_bits += bits;

        let fb = FrameBitrate {
            frame_index: i as u64,
            size_bytes: size,
            bitrate_mbps: mbps,
        };

        if mbps > max_bps {
            max_bps = mbps;
        }
        if mbps < min_bps {
            min_bps = mbps;
        }
        if mbps > DCI_MAX_BITRATE_MBPS {
            over_limit.push(fb.clone());
        }
        frames.push(fb);
    }

    let count = j2k_files.len() as u64;
    let avg = if count > 0 {
        (total_bits as f64 / count as f64) / 1_000_000.0 * fps
    } else {
        0.0
    };

    if min_bps == f64::MAX {
        min_bps = 0.0;
    }

    BitrateAnalysis {
        frame_count: count,
        avg_bitrate_mbps: avg,
        max_bitrate_mbps: max_bps,
        min_bitrate_mbps: min_bps,
        dci_max_mbps: DCI_MAX_BITRATE_MBPS,
        dci_compliant: over_limit.is_empty(),
        over_limit_frames: over_limit,
    }
}

/// Read frame `frame` of a JP2K picture MXF and return its raw J2K codestream.
/// Reads unencrypted essence only; an encrypted picture track yields ciphertext.
pub fn read_mxf_j2k_frame(path: &Path, frame: u32) -> Result<Vec<u8>, String> {
    let s = path.to_str().ok_or("non-UTF-8 MXF path")?;
    let mut reader = asdcplib::jp2k::MxfReader::new();
    reader.open_read(s).map_err(|e| format!("open MXF: {e}"))?;
    // DCI caps a frame at 1,302,083 bytes, 2K and 4K alike; 16 MiB is headroom.
    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = reader
        .read_frame(frame, &mut buf, None, None)
        .map_err(|e| format!("read frame {frame}: {e}"))?;
    buf.truncate(n);
    Ok(buf)
}

/// Parse the J2K codestream header of frame `frame` in a JP2K picture MXF.
pub fn parse_j2k_from_mxf(path: &Path, frame: u32) -> Result<J2kHeader, String> {
    let data = read_mxf_j2k_frame(path, frame)?;
    parse_j2k_header(&data)
        .ok_or_else(|| "frame is not a J2K codestream (no SOC marker)".to_string())
}

/// Frame-level bitrate statistics for a picture MXF. Shared with dcpdoctor,
/// which measures the frames itself through its own key-aware MXF reader.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MxfBitrateStats {
    pub valid: bool,
    pub error: String,
    pub frame_count: u32,
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    pub total_bytes: u64,
    pub min_frame_bytes: u64,
    pub max_frame_bytes: u64,
    pub max_frame_index: u32,
    pub avg_bitrate_mbps: f64,
    pub min_bitrate_mbps: f64,
    pub max_bitrate_mbps: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dci_limit_matches_the_dcss_per_frame_byte_caps() {
        // DCSS 4.3.3 states the cap as bytes per frame, not a rate: 1,302,083 at
        // 24 fps for 2K and 4K alike, and 651,041 at 48 fps.
        for (bytes_per_frame, fps) in [(1_302_083.0, 24.0), (651_041.0, 48.0)] {
            let mbps = bytes_per_frame * fps * 8.0 / 1_000_000.0;
            assert!((mbps - DCI_MAX_BITRATE_MBPS).abs() < 0.001);
        }
        assert_eq!(dci_codestream_byte_cap(24), 1_302_083);
        assert_eq!(dci_codestream_byte_cap(48), 651_041);
    }

    #[test]
    fn profile_from_rsiz() {
        // the values grok writes and a DCP carries: 2K is 3, not 1
        assert_eq!(J2kProfile::from(0x0000), J2kProfile::None);
        assert_eq!(J2kProfile::from(0x0003), J2kProfile::Cinema2k);
        assert_eq!(J2kProfile::from(0x0004), J2kProfile::Cinema4k);
        assert_eq!(J2kProfile::from(0x0005), J2kProfile::CinemaScalable2k);
        assert_eq!(J2kProfile::from(0x0006), J2kProfile::CinemaScalable4k);
        assert_eq!(J2kProfile::from(0x0007), J2kProfile::CinemaLongTermStorage);
        // broadcast and IMF carry level bits below the profile
        assert_eq!(J2kProfile::from(0x0100), J2kProfile::Broadcast);
        assert_eq!(J2kProfile::from(0x0302), J2kProfile::Broadcast);
        assert_eq!(J2kProfile::from(0x0400), J2kProfile::Imf);
        assert_eq!(J2kProfile::from(0x0900), J2kProfile::Imf);
        // 1 and 2 are not cinema profiles, whatever the DCI spec calls its own
        assert_eq!(J2kProfile::from(0x0001), J2kProfile::Unknown(1));
        assert_eq!(J2kProfile::from(0x0002), J2kProfile::Unknown(2));
    }

    #[test]
    fn netflix_app_2e_picture_composes_its_rsiz() {
        // a real App 2E picture: 3840x2160 at 24 fps under 800 Mb/s carries
        // Rsiz 0x0536, IMF 4K at main level 6 and sub level 3
        let levels = imf_levels(3840, 2160, 24.0, 800_000_000).unwrap();
        assert_eq!(
            levels,
            ImfLevels {
                main_level: 6,
                sub_level: 3
            }
        );
        let profile = ImfProfile::for_raster(3840, 2160).unwrap();
        assert_eq!(profile, ImfProfile::Imf4k);
        assert_eq!(imf_rsiz(profile, levels), 0x0536);
        assert_eq!(J2kProfile::from(0x0536), J2kProfile::Imf);
    }

    #[test]
    fn a_2k_picture_at_the_dci_rate_composes_its_rsiz() {
        // 2048x1080 at 24 fps is 159.3 megasamples per second, main level 3 by
        // rate alone, and 250 Mb/s is sub level 2, which needs main level 4
        let levels = imf_levels(2048, 1080, 24.0, 250_000_000).unwrap();
        assert_eq!(
            levels,
            ImfLevels {
                main_level: 4,
                sub_level: 2
            }
        );
        let profile = ImfProfile::for_raster(2048, 1080).unwrap();
        assert_eq!(profile, ImfProfile::Imf2k);
        assert_eq!(imf_rsiz(profile, levels), 0x0424);
    }

    #[test]
    fn a_low_bit_rate_leaves_the_main_level_where_the_sample_rate_puts_it() {
        // sub level 1 pairs with any main level, so 190 Mb/s keeps main level 3
        let levels = imf_levels(2048, 1080, 24.0, 190_000_000).unwrap();
        assert_eq!(
            levels,
            ImfLevels {
                main_level: 3,
                sub_level: 1
            }
        );
    }

    #[test]
    fn imf_levels_refuse_a_rate_no_level_covers() {
        // 8192x6224 at 300 fps is 45,885 megasamples per second, past level 11
        assert!(imf_levels(8192, 6224, 300.0, 200_000_000).is_err());
        // 60 Gb/s is past sub level 9
        assert!(imf_levels(1920, 1080, 24.0, 60_000_000_000).is_err());
        assert!(imf_levels(1920, 1080, 0.0, 200_000_000).is_err());
    }

    #[test]
    fn imf_profiles_cover_their_rasters() {
        assert_eq!(ImfProfile::for_raster(2048, 1556), Ok(ImfProfile::Imf2k));
        // 2048x1080 is 2K, but a 2160-high picture needs 4K
        assert_eq!(ImfProfile::for_raster(2048, 2160), Ok(ImfProfile::Imf4k));
        assert_eq!(ImfProfile::for_raster(4096, 3112), Ok(ImfProfile::Imf4k));
        assert_eq!(ImfProfile::for_raster(4097, 2160), Ok(ImfProfile::Imf8k));
        assert!(ImfProfile::for_raster(8193, 4320).is_err());
    }

    #[cfg(feature = "grok-ffi")]
    #[test]
    fn the_imf_level_tables_match_grok() {
        use grokj2k_sys::*;
        assert_eq!(
            IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND,
            [
                GRK_IMF_MAINLEVEL_1_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_2_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_3_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_4_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_5_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_6_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_7_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_8_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_9_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_10_MSAMPLESSEC,
                GRK_IMF_MAINLEVEL_11_MSAMPLESSEC,
            ]
        );
        assert_eq!(
            IMF_SUB_LEVEL_MEGABITS_PER_SECOND,
            [
                GRK_IMF_SUBLEVEL_1_MBITSSEC,
                GRK_IMF_SUBLEVEL_2_MBITSSEC,
                GRK_IMF_SUBLEVEL_3_MBITSSEC,
                GRK_IMF_SUBLEVEL_4_MBITSSEC,
                GRK_IMF_SUBLEVEL_5_MBITSSEC,
                GRK_IMF_SUBLEVEL_6_MBITSSEC,
                GRK_IMF_SUBLEVEL_7_MBITSSEC,
                GRK_IMF_SUBLEVEL_8_MBITSSEC,
                GRK_IMF_SUBLEVEL_9_MBITSSEC,
            ]
        );
        assert_eq!(
            IMF_MAIN_LEVEL_MEGASAMPLES_PER_SECOND.len() as u32,
            GRK_LEVEL_MAX
        );
        assert_eq!(
            IMF_SUB_LEVEL_MEGABITS_PER_SECOND.len() as u32,
            GRK_IMF_SUBLEVEL_MAX
        );
        assert_eq!(IMF_2K_RSIZ_BASE as u32, GRK_PROFILE_IMF_2K);
        assert_eq!(IMF_4K_RSIZ_BASE as u32, GRK_PROFILE_IMF_4K);
        assert_eq!(IMF_8K_RSIZ_BASE as u32, GRK_PROFILE_IMF_8K);
    }

    #[test]
    fn the_cinema_profiles_are_dci() {
        for rsiz in [0x0003u16, 0x0004, 0x0005, 0x0006, 0x0007] {
            assert!(J2kProfile::from(rsiz).is_dci_cinema(), "rsiz {rsiz:#06x}");
        }
    }

    #[test]
    fn imf_and_broadcast_profiles_are_not_dci() {
        for rsiz in [0x0400u16, 0x0500, 0x0100, 0x0000, 0x0001] {
            assert!(!J2kProfile::from(rsiz).is_dci_cinema(), "rsiz {rsiz:#06x}");
        }
    }

    #[test]
    fn a_part2_extension_bit_does_not_hide_the_cinema_profile() {
        assert_eq!(J2kProfile::from(0x8000 | 0x0003), J2kProfile::Cinema2k);
        assert!(J2kProfile::from(0x8000 | 0x0003).is_dci_cinema());
    }

    #[test]
    fn dci_wrapping_takes_only_the_plain_cinema_profiles() {
        // scalable and long-term storage are cinema, but SMPTE 429-4 wraps 2K/4K
        for rsiz in [0x0005u16, 0x0006, 0x0007, 0x0001, 0x0002] {
            let header =
                parse_j2k_header(&synth_codestream(rsiz, 2048, 1080, 0, 0, 3, 12)).unwrap();
            assert!(
                validate_dci_header(&header).is_err(),
                "rsiz {rsiz:#06x} should not wrap into a DCP"
            );
        }
        for rsiz in [0x0003u16, 0x0004] {
            let header =
                parse_j2k_header(&synth_codestream(rsiz, 2048, 1080, 0, 0, 3, 12)).unwrap();
            assert!(validate_dci_header(&header).is_ok(), "rsiz {rsiz:#06x}");
        }
    }

    /// Build a minimal SOC + SIZ + SOD codestream for testing.
    fn synth_codestream(
        rsiz: u16,
        xsiz: u32,
        ysiz: u32,
        xosiz: u32,
        yosiz: u32,
        components: u16,
        bit_depth: u8,
    ) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&SOC.to_be_bytes());
        d.extend_from_slice(&SIZ.to_be_bytes());
        let lsiz = 38 + 3 * components;
        d.extend_from_slice(&lsiz.to_be_bytes());
        d.extend_from_slice(&rsiz.to_be_bytes());
        d.extend_from_slice(&xsiz.to_be_bytes());
        d.extend_from_slice(&ysiz.to_be_bytes());
        d.extend_from_slice(&xosiz.to_be_bytes());
        d.extend_from_slice(&yosiz.to_be_bytes());
        // one tile covering the whole image
        d.extend_from_slice(&xsiz.to_be_bytes());
        d.extend_from_slice(&ysiz.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&components.to_be_bytes());
        for _ in 0..components {
            d.push(bit_depth - 1); // Ssiz, unsigned
            d.push(1); // XRsiz
            d.push(1); // YRsiz
        }
        d.extend_from_slice(&SOD.to_be_bytes());
        d
    }

    #[test]
    fn siz_parses_4k_dimensions() {
        let hdr = parse_j2k_header(&synth_codestream(4, 4096, 2160, 0, 0, 3, 12)).unwrap();
        assert_eq!(hdr.width, 4096);
        assert_eq!(hdr.height, 2160);
        assert_eq!(hdr.num_components, 3);
        assert_eq!(hdr.bit_depth, 12);
        assert!(!hdr.is_signed);
        assert_eq!(J2kProfile::from(hdr.profile), J2kProfile::Cinema4k);
    }

    #[test]
    fn siz_parses_2k_scope_dimensions() {
        let hdr = parse_j2k_header(&synth_codestream(3, 2048, 858, 0, 0, 3, 12)).unwrap();
        assert_eq!(hdr.width, 2048);
        assert_eq!(hdr.height, 858);
    }

    #[test]
    fn dci_validation_rejects_non_dci_profile() {
        let header = parse_j2k_header(&synth_codestream(0, 2048, 1080, 0, 0, 3, 12)).unwrap();
        assert_eq!(
            validate_dci_header(&header),
            Err("RSIZ 0x0000 is not a DCI JPEG 2000 profile".to_string())
        );
    }

    #[test]
    fn dci_validation_rejects_oversized_2k_profile() {
        let header = parse_j2k_header(&synth_codestream(3, 2049, 1080, 0, 0, 3, 12)).unwrap();
        assert!(
            validate_dci_header(&header)
                .unwrap_err()
                .contains("2K profile exceeds")
        );
    }

    #[test]
    fn siz_subtracts_image_origin_offset() {
        // width/height are Xsiz-XOsiz and Ysiz-YOsiz, not Xsiz/Ysiz
        let hdr = parse_j2k_header(&synth_codestream(0, 4196, 2260, 100, 100, 3, 12)).unwrap();
        assert_eq!(hdr.width, 4096);
        assert_eq!(hdr.height, 2160);
    }

    #[test]
    fn rejects_non_codestream() {
        assert!(parse_j2k_header(b"not a j2k file at all").is_none());
        assert!(parse_j2k_header(&[]).is_none());
    }

    #[test]
    fn malformed_segment_length_does_not_panic() {
        // Lsiz of 0 would underflow the segment slice arithmetic
        let mut d = Vec::new();
        d.extend_from_slice(&SOC.to_be_bytes());
        d.extend_from_slice(&SIZ.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&[0u8; 8]);
        let hdr = parse_j2k_header(&d).unwrap();
        assert_eq!(hdr.width, 0);
    }

    // full codestream: SOC, SIZ, COD, QCD, TLM, then tile-parts, EOC
    fn full_codestream(rsiz: u16, w: u32, h: u32, guard: u8, tile_parts: u8) -> Vec<u8> {
        let mut d = vec![0xFF, 0x4F]; // SOC
        // SIZ
        let csiz: u16 = 3;
        let mut siz = Vec::new();
        siz.extend_from_slice(&rsiz.to_be_bytes());
        siz.extend_from_slice(&w.to_be_bytes());
        siz.extend_from_slice(&h.to_be_bytes());
        siz.extend_from_slice(&0u32.to_be_bytes());
        siz.extend_from_slice(&0u32.to_be_bytes());
        siz.extend_from_slice(&w.to_be_bytes()); // one tile
        siz.extend_from_slice(&h.to_be_bytes());
        siz.extend_from_slice(&0u32.to_be_bytes());
        siz.extend_from_slice(&0u32.to_be_bytes());
        siz.extend_from_slice(&csiz.to_be_bytes());
        for _ in 0..csiz {
            siz.extend_from_slice(&[11, 1, 1]); // 12-bit
        }
        d.extend_from_slice(&SIZ.to_be_bytes());
        d.extend_from_slice(&((2 + siz.len()) as u16).to_be_bytes());
        d.extend_from_slice(&siz);
        // COD: Scod, prog(LRCP), layers=1, MCT=1, decomp=5, cbw exp=3, cbh exp=3, style, transform=0(9-7)
        let cod = [0u8, 0, 0, 1, 1, 5, 3, 3, 0, 0];
        d.extend_from_slice(&COD.to_be_bytes());
        d.extend_from_slice(&((2 + cod.len()) as u16).to_be_bytes());
        d.extend_from_slice(&cod);
        // QCD: Sqcd guard bits in top 3 bits + 1 SPqcd byte
        d.extend_from_slice(&QCD.to_be_bytes());
        d.extend_from_slice(&(2u16 + 2).to_be_bytes());
        d.extend_from_slice(&[guard << 5, 0]);
        // TLM (empty payload)
        d.extend_from_slice(&TLM.to_be_bytes());
        d.extend_from_slice(&(2u16 + 2).to_be_bytes());
        d.extend_from_slice(&[0, 0]);
        // tile-parts
        for i in 0..tile_parts {
            let psot = (12 + 2 + 8) as u32; // SOT(12) + SOD(2) + 8 data bytes
            d.extend_from_slice(&SOT.to_be_bytes());
            d.extend_from_slice(&10u16.to_be_bytes()); // Lsot
            d.extend_from_slice(&0u16.to_be_bytes()); // Isot
            d.extend_from_slice(&psot.to_be_bytes());
            d.push(i);
            d.push(tile_parts);
            d.extend_from_slice(&SOD.to_be_bytes());
            d.extend_from_slice(&[0u8; 8]);
        }
        d.extend_from_slice(&EOC.to_be_bytes());
        d
    }

    #[test]
    fn parses_extended_fields() {
        let hdr = parse_j2k_header(&full_codestream(3, 2048, 1080, 1, 3)).unwrap();
        assert_eq!(hdr.bit_depths, vec![12, 12, 12]);
        assert_eq!(hdr.codeblock_width_exp, 3);
        assert_eq!(hdr.codeblock_height_exp, 3);
        assert_eq!(hdr.codeblock_width, 32);
        assert_eq!(hdr.codeblock_height, 32);
        assert!(hdr.irreversible_transform);
        assert!(hdr.mct);
        assert_eq!(hdr.num_decomp_levels, 5);
        assert_eq!(hdr.guard_bits, 1);
        assert!(hdr.tlm_present);
        assert!(!hdr.poc_present);
        assert_eq!(hdr.tile_part_count, 3);
    }

    #[test]
    fn counts_4k_tile_parts() {
        let hdr = parse_j2k_header(&full_codestream(4, 4096, 2160, 2, 6)).unwrap();
        assert_eq!(hdr.tile_part_count, 6);
        assert_eq!(hdr.guard_bits, 2);
    }
}
