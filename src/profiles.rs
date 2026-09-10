use serde::{Deserialize, Serialize};

/// Delivery platform target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    TheatricalDci2k,
    TheatricalDci4k,
    Netflix,
    Disney,
    Hbo,
    ArchivalPreservation,
    Broadcast,
}

/// Encoding profile for a platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodingProfile {
    pub platform: Platform,
    pub name: String,
    pub description: String,
    /// Resolution width
    pub width: u32,
    /// Resolution height
    pub height: u32,
    /// Frame rate as string (e.g. "24", "23.976", "25")
    pub frame_rate: String,
    /// Target bitrate in Mbps, 0 where the specification states no ceiling
    pub bitrate_mbps: f64,
    /// Color space
    pub colour_space: String,
    /// Bit depth
    pub bit_depth: u32,
    pub light_levels_required: bool,
    /// JPEG 2000 progression order
    pub progression: String,
    /// Audio sample rate in Hz
    pub audio_sample_rate: u32,
    /// Audio bit depth
    pub audio_bit_depth: u32,
    /// Audio channels (e.g. "5.1", "7.1.4", "stereo")
    pub audio_channels: String,
    /// Subtitle format (e.g. "IMSC1", "PNG", "SRT")
    pub subtitle_format: String,
    // where these numbers come from, so a profile cannot be added without saying
    pub specification: String,
}

impl EncodingProfile {
    pub fn bitrate_ceiling_mbps(&self) -> Option<f64> {
        (self.bitrate_mbps > 0.0).then_some(self.bitrate_mbps)
    }
}

/// Get all available encoding profiles.
pub fn all_profiles() -> Vec<EncodingProfile> {
    vec![
        theatrical_2k(),
        theatrical_4k(),
        netflix(),
        disney(),
        hbo(),
        archival(),
        broadcast(),
    ]
}

/// Get encoding profile for a specific platform.
pub fn profile_for(platform: Platform) -> EncodingProfile {
    match platform {
        Platform::TheatricalDci2k => theatrical_2k(),
        Platform::TheatricalDci4k => theatrical_4k(),
        Platform::Netflix => netflix(),
        Platform::Disney => disney(),
        Platform::Hbo => hbo(),
        Platform::ArchivalPreservation => archival(),
        Platform::Broadcast => broadcast(),
    }
}

fn theatrical_2k() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::TheatricalDci2k,
        name: "DCI 2K Theatrical".to_string(),
        description: "DCI-compliant 2K digital cinema package".to_string(),
        width: 2048,
        height: 1080,
        frame_rate: "24".to_string(),
        bitrate_mbps: 250.0,
        colour_space: "XYZ".to_string(),
        bit_depth: 12,
        light_levels_required: false,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "DCI Digital Cinema System Specification 4.3.3: 1,302,083 bytes a frame aggregate, so 250 Mbit/s at 24fps for 2K and 4K alike".to_string(),
        subtitle_format: "PNG".to_string(),
    }
}

fn theatrical_4k() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::TheatricalDci4k,
        name: "DCI 4K Theatrical".to_string(),
        description: "DCI-compliant 4K digital cinema package".to_string(),
        width: 4096,
        height: 2160,
        frame_rate: "24".to_string(),
        bitrate_mbps: 250.0,
        colour_space: "XYZ".to_string(),
        bit_depth: 12,
        light_levels_required: false,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "7.1".to_string(),
        specification: "DCI Digital Cinema System Specification 4.3.3: 1,302,083 bytes a frame aggregate, so 250 Mbit/s at 24fps for 2K and 4K alike".to_string(),
        subtitle_format: "PNG".to_string(),
    }
}

// the Mainlevel 6 Sublevel 3 ceiling an App 2E UHD picture up to 30 fps is allowed
const IMF_MAIN_LEVEL_6_SUB_LEVEL_3_MBPS: f64 = 800.0;

fn netflix() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Netflix,
        name: "Netflix IMF".to_string(),
        description: "Netflix UHD SDR IMF delivery".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        bitrate_mbps: IMF_MAIN_LEVEL_6_SUB_LEVEL_3_MBPS,
        colour_space: "BT.709 RGB full range".to_string(),
        bit_depth: 10,
        light_levels_required: false,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "Netflix IMF Delivery Specifications, read 2026-09-10 at \
                        https://studiopartner.netflix.net/studio/branded-imf-delivery-specifications. \
                        SDR is 10-bit BT.709 RGB 4:4:4 full range, and 800 Mbit/s is the 4k IMF \
                        Single Tile Lossy Profile Mainlevel 6 Sublevel 3 ceiling for UHD up to 30 \
                        fps. A Dolby Vision delivery is 12-bit P3-D65 ST 2084 instead, checked by \
                        compliance -s dolby rather than by this row"
            .to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn disney() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Disney,
        name: "Disney+ IMF".to_string(),
        description: "Disney UHD SDR IMF distribution package".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        bitrate_mbps: 0.0,
        colour_space: "BT.709 YCbCr".to_string(),
        bit_depth: 10,
        light_levels_required: false,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "Disney IMF Distribution Packages v1.13.2, read 2026-09-10 at \
                        https://mediatechspecs.disney.com/mastering/video/imf-distribution-packages. \
                        App 2E ST 2067-21:2020, SDR is 10-bit BT.709 / BT.1886 YCbCr and HDR is \
                        12-bit BT.2020 ST 2084. The page states no bitrate ceiling, so this row \
                        names none"
            .to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn hbo() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Hbo,
        name: "HBO Max HDR IMF".to_string(),
        description: "HBO Max UHD HDR IMF delivery".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        bitrate_mbps: IMF_MAIN_LEVEL_6_SUB_LEVEL_3_MBPS,
        colour_space: "BT.2020 PQ".to_string(),
        bit_depth: 12,
        light_levels_required: true,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "IAB".to_string(),
        specification: "Warner Bros. Discovery High Dynamic Range (HDR) ingest specification v1.9 \
                        of 2026-02-06, read 2026-09-10 at \
                        https://partnerhub.warnermediagroup.com/ingest-specifications/hdr-content. \
                        UHD HDR IMP only, 12-bit full range RGB BT.2020 ST 2084, Dolby Atmos as \
                        IAB with 5.1 and 2.0 not accepted, and MaxCLL and MaxFALL must be present \
                        in MMC metadata or the CPL. The page names JPEG 2000 IMF single-tile lossy \
                        Main-Level 6 Sub-Level 3 and no Mbit/s figure, so 800 Mbit/s here is that \
                        sub level's own ceiling"
            .to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn archival() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::ArchivalPreservation,
        name: "Archival / Preservation".to_string(),
        description: "Lossless archival preservation profile".to_string(),
        width: 4096,
        height: 2160,
        frame_rate: "24".to_string(),
        bitrate_mbps: 0.0, // lossless
        colour_space: "XYZ".to_string(),
        bit_depth: 16,
        light_levels_required: false,
        progression: "LRCP".to_string(),
        audio_sample_rate: 96000,
        audio_bit_depth: 24,
        audio_channels: "7.1".to_string(),
        specification: "house profile, no external specification".to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn broadcast() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Broadcast,
        name: "Broadcast".to_string(),
        description: "Standard broadcast delivery profile".to_string(),
        width: 1920,
        height: 1080,
        frame_rate: "25".to_string(),
        bitrate_mbps: 200.0,
        colour_space: "Rec.709".to_string(),
        bit_depth: 10,
        light_levels_required: false,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "stereo".to_string(),
        specification: "house profile, no external specification".to_string(),
        subtitle_format: "SRT".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_profiles_count() {
        assert_eq!(all_profiles().len(), 7);
    }

    #[test]
    fn profile_lookup() {
        let p = profile_for(Platform::Netflix);
        assert_eq!(p.width, 3840);
        assert_eq!(p.colour_space, "BT.709 RGB full range");
    }

    const PUBLISHED_STREAMING_PLATFORMS: [Platform; 3] =
        [Platform::Netflix, Platform::Disney, Platform::Hbo];

    #[test]
    fn every_streaming_profile_cites_a_page_someone_can_open() {
        for platform in PUBLISHED_STREAMING_PLATFORMS {
            let profile = profile_for(platform);
            assert!(
                profile.specification.contains("https://"),
                "{} cites no URL: {}",
                profile.name,
                profile.specification
            );
        }
    }

    #[test]
    fn no_profile_carries_unverified_numbers() {
        for profile in all_profiles() {
            assert!(
                !profile.specification.contains("unverified"),
                "{} still calls its numbers unverified",
                profile.name
            );
        }
    }

    #[test]
    fn a_profile_stating_no_bitrate_ceiling_names_none() {
        assert_eq!(profile_for(Platform::Disney).bitrate_ceiling_mbps(), None);
        assert_eq!(
            profile_for(Platform::ArchivalPreservation).bitrate_ceiling_mbps(),
            None
        );
        assert_eq!(
            profile_for(Platform::Netflix).bitrate_ceiling_mbps(),
            Some(800.0)
        );
    }

    /// A profile with no citation is a number nobody can check, which is how the
    /// unverified ones got in.
    #[test]
    fn every_profile_says_where_its_numbers_came_from() {
        for profile in all_profiles() {
            assert!(
                !profile.specification.trim().is_empty(),
                "{} cites no specification",
                profile.name
            );
        }
    }

    #[test]
    fn dci_caps_2k_and_4k_at_the_same_bitrate() {
        assert_eq!(
            profile_for(Platform::TheatricalDci4k).bitrate_mbps,
            profile_for(Platform::TheatricalDci2k).bitrate_mbps
        );
    }

    #[test]
    fn theatrical_2k_dci_compliant() {
        let p = profile_for(Platform::TheatricalDci2k);
        assert_eq!(p.width, 2048);
        assert_eq!(p.height, 1080);
        assert_eq!(p.bit_depth, 12);
        assert_eq!(p.bitrate_mbps, 250.0);
        assert_eq!(p.colour_space, "XYZ");
    }
}
