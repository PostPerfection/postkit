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
    /// No profile / unrestricted
    None,
    /// Profile 0 (DCI 2K)
    Dci2k,
    /// Profile 1 (DCI 4K)
    Dci4k,
    /// Cinema 2K
    CinemaS2k,
    /// Cinema 4K
    CinemaS4k,
    /// Profile 3 (broadcast)
    Broadcast,
    /// Unknown profile
    Unknown(u16),
}

impl From<u16> for J2kProfile {
    fn from(rsiz: u16) -> Self {
        match rsiz {
            0 => J2kProfile::None,
            1 => J2kProfile::Dci2k,
            2 => J2kProfile::Dci4k,
            3 => J2kProfile::CinemaS2k,
            4 => J2kProfile::CinemaS4k,
            5 => J2kProfile::Broadcast,
            v => J2kProfile::Unknown(v),
        }
    }
}

/// Check the header fields DCP picture wrapping requires.
pub fn validate_dci_header(header: &J2kHeader) -> Result<(), String> {
    let max_dimensions = match header.profile {
        1 | 3 => (2048, 1080),
        2 | 4 => (4096, 2160),
        profile => {
            return Err(format!(
                "RSIZ {profile:#06x} is not a DCI JPEG 2000 profile"
            ));
        }
    };

    if header.width == 0 || header.height == 0 {
        return Err("JPEG 2000 codestream has no image area".to_string());
    }
    if header.width > max_dimensions.0 || header.height > max_dimensions.1 {
        return Err(format!(
            "JPEG 2000 {} profile exceeds its {}x{} limit: {}x{}",
            match header.profile {
                1 | 3 => "2K",
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

/// Compute DCI max bitrate for a given resolution.
pub fn dci_max_bitrate_mbps(width: u32) -> f64 {
    if width > 2048 {
        500.0 // 4K
    } else {
        250.0 // 2K
    }
}

/// Analyse bitrate of a sequence of J2K files.
pub fn analyse_bitrate(j2k_files: &[std::path::PathBuf], fps: f64, width: u32) -> BitrateAnalysis {
    let max_allowed = dci_max_bitrate_mbps(width);
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
        if mbps > max_allowed {
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
        dci_max_mbps: max_allowed,
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
    // DCI caps a frame near 1.3 MB (2K) / 2.6 MB (4K); 16 MiB is safe headroom.
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

/// Frame-level bitrate statistics for a picture MXF, read via asdcplib.
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

/// Analyse per-frame bitrate of a JP2K picture MXF via the asdcplib reader.
/// Reads each frame's stored size (unencrypted; ciphertext frame sizes match
/// plaintext) and derives the DCI-relevant peak/avg bitrate.
pub fn analyse_mxf_bitrate(mxf_path: &Path) -> MxfBitrateStats {
    let mut stats = MxfBitrateStats::default();

    let Some(path_str) = mxf_path.to_str() else {
        stats.error = "non-UTF-8 MXF path".into();
        return stats;
    };

    let mut reader = asdcplib::jp2k::MxfReader::new();
    if let Err(e) = reader.open_read(path_str) {
        stats.error = format!("Failed to open MXF: {e}");
        return stats;
    }

    let desc = match reader.picture_descriptor() {
        Ok(d) => d,
        Err(e) => {
            stats.error = format!("Failed to read picture descriptor: {e}");
            return stats;
        }
    };

    stats.frame_count = desc.container_duration;
    stats.width = desc.stored_width;
    stats.height = desc.stored_height;
    stats.frame_rate = desc.edit_rate.numerator as f64 / desc.edit_rate.denominator.max(1) as f64;

    if stats.frame_count == 0 || stats.frame_rate <= 0.0 {
        stats.error = "Invalid frame count or rate".into();
        return stats;
    }

    stats.min_frame_bytes = u64::MAX;
    let mut buf = vec![0u8; 16 * 1024 * 1024];

    for i in 0..stats.frame_count {
        let frame_size = match reader.read_frame(i, &mut buf, None, None) {
            Ok(sz) => sz as u64,
            Err(_) => break,
        };
        stats.total_bytes += frame_size;
        if frame_size > stats.max_frame_bytes {
            stats.max_frame_bytes = frame_size;
            stats.max_frame_index = i;
        }
        if frame_size < stats.min_frame_bytes {
            stats.min_frame_bytes = frame_size;
        }
    }

    if stats.min_frame_bytes == u64::MAX {
        stats.min_frame_bytes = 0;
    }

    let frame_duration_sec = 1.0 / stats.frame_rate;
    stats.avg_bitrate_mbps = (stats.total_bytes as f64 * 8.0)
        / (stats.frame_count as f64 * frame_duration_sec * 1_000_000.0);
    stats.max_bitrate_mbps =
        (stats.max_frame_bytes as f64 * 8.0) / (frame_duration_sec * 1_000_000.0);
    stats.min_bitrate_mbps =
        (stats.min_frame_bytes as f64 * 8.0) / (frame_duration_sec * 1_000_000.0);

    stats.valid = true;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dci_limits() {
        assert_eq!(dci_max_bitrate_mbps(2048), 250.0);
        assert_eq!(dci_max_bitrate_mbps(4096), 500.0);
    }

    #[test]
    fn profile_from_rsiz() {
        assert_eq!(J2kProfile::from(0), J2kProfile::None);
        assert_eq!(J2kProfile::from(1), J2kProfile::Dci2k);
        assert_eq!(J2kProfile::from(2), J2kProfile::Dci4k);
        assert_eq!(J2kProfile::from(3), J2kProfile::CinemaS2k);
        assert_eq!(J2kProfile::from(4), J2kProfile::CinemaS4k);
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
        assert_eq!(J2kProfile::from(hdr.profile), J2kProfile::CinemaS4k);
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
