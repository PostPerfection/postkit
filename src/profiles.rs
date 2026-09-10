use serde::{Deserialize, Serialize};

/// Delivery platform target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    TheatricalDci2k,
    TheatricalDci4k,
    Netflix,
    AmazonPrime,
    Disney,
    Apple,
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
    /// Color space
    pub colour_space: String,
    /// Bit depth
    pub bit_depth: u32,
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

/// Get all available encoding profiles.
pub fn all_profiles() -> Vec<EncodingProfile> {
    vec![
        theatrical_2k(),
        theatrical_4k(),
        netflix(),
        amazon(),
        disney(),
        apple(),
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
        Platform::AmazonPrime => amazon(),
        Platform::Disney => disney(),
        Platform::Apple => apple(),
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
        colour_space: "XYZ".to_string(),
        bit_depth: 12,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "DCI Digital Cinema System Specification: X'Y'Z' at 12 bits, 2048x1080 and 4096x2160".to_string(),
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
        colour_space: "XYZ".to_string(),
        bit_depth: 12,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "7.1".to_string(),
        specification: "DCI Digital Cinema System Specification: X'Y'Z' at 12 bits, 2048x1080 and 4096x2160".to_string(),
        subtitle_format: "PNG".to_string(),
    }
}

fn netflix() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Netflix,
        name: "Netflix IMF".to_string(),
        description: "Netflix IMF delivery specification".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        colour_space: "Rec.2020".to_string(),
        bit_depth: 16,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "no public delivery specification, these numbers are unverified".to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn amazon() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::AmazonPrime,
        name: "Amazon Prime IMF".to_string(),
        description: "Amazon Prime Video IMF delivery specification".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        colour_space: "Rec.2020".to_string(),
        bit_depth: 16,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "no public delivery specification, these numbers are unverified".to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn disney() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Disney,
        name: "Disney+ IMF".to_string(),
        description: "Disney+ IMF delivery specification".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        colour_space: "Rec.2020".to_string(),
        bit_depth: 16,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "7.1.4".to_string(),
        specification: "no public delivery specification, these numbers are unverified".to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn apple() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Apple,
        name: "Apple TV+ IMF".to_string(),
        description: "Apple TV+ IMF delivery specification".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        colour_space: "P3-D65".to_string(),
        bit_depth: 16,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "7.1.4".to_string(),
        specification: "no public delivery specification, these numbers are unverified".to_string(),
        subtitle_format: "IMSC1".to_string(),
    }
}

fn hbo() -> EncodingProfile {
    EncodingProfile {
        platform: Platform::Hbo,
        name: "HBO Max IMF".to_string(),
        description: "HBO Max IMF delivery specification".to_string(),
        width: 3840,
        height: 2160,
        frame_rate: "23.976".to_string(),
        colour_space: "Rec.2020".to_string(),
        bit_depth: 16,
        progression: "CPRL".to_string(),
        audio_sample_rate: 48000,
        audio_bit_depth: 24,
        audio_channels: "5.1".to_string(),
        specification: "no public delivery specification, these numbers are unverified".to_string(),
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
        colour_space: "XYZ".to_string(),
        bit_depth: 16,
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
        colour_space: "Rec.709".to_string(),
        bit_depth: 10,
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
        assert_eq!(all_profiles().len(), 9);
    }

    #[test]
    fn profile_lookup() {
        let p = profile_for(Platform::Netflix);
        assert_eq!(p.width, 3840);
        assert_eq!(p.colour_space, "Rec.2020");
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
    fn theatrical_2k_dci_compliant() {
        let p = profile_for(Platform::TheatricalDci2k);
        assert_eq!(p.width, 2048);
        assert_eq!(p.height, 1080);
        assert_eq!(p.bit_depth, 12);
        assert_eq!(p.colour_space, "XYZ");
    }
}
