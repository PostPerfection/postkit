use serde::{Deserialize, Serialize};

/// JPEG 2000 marker codes.
const SOC: u16 = 0xFF4F; // Start of codestream
const SIZ: u16 = 0xFF51; // Image and tile size
const COD: u16 = 0xFF52; // Coding style default
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
}

/// DCI compliance profile identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum J2kProfile {
    /// No profile / unrestricted
    None,
    /// Profile 0 (DCI 2K)
    CinemaS2k,
    /// Profile 1 (DCI 4K)
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
            3 => J2kProfile::CinemaS2k,
            4 => J2kProfile::CinemaS4k,
            5 => J2kProfile::Broadcast,
            v => J2kProfile::Unknown(v),
        }
    }
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
            }
            COD if seg.len() >= 5 => {
                hdr.progression_order = seg[1];
                hdr.num_layers = u16::from_be_bytes([seg[2], seg[3]]);
                hdr.num_decomp_levels = seg[5];
            }
            _ => {}
        }

        pos += seg_len - 2;
    }

    Some(hdr)
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
}
