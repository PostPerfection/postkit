use serde::{Deserialize, Serialize};

/// Accessibility standard to check against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessibilityStandard {
    /// US: 21st Century Communications and Video Accessibility Act
    Cvaa,
    /// EU: European Accessibility Act (2025)
    Eaa,
    /// Canada: Accessibility for Ontarians with Disabilities Act
    Aoda,
    /// UK: Ofcom broadcasting accessibility code
    Ofcom,
}

/// Accessibility track type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessibilityTrack {
    /// AD — visually impaired narration
    AudioDescription,
    /// HI — SDH/CC subtitles for deaf/hard of hearing
    HearingImpaired,
    /// SL — sign language video overlay
    SignLanguage,
    /// OC — burned-in captions
    OpenCaptions,
    /// CC — CEA-608/708 caption stream
    ClosedCaptions,
    /// Director/audio commentary
    Commentary,
}

/// Severity of an accessibility finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// Single compliance finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessibilityFinding {
    pub severity: Severity,
    pub track_type: AccessibilityTrack,
    /// e.g. "CVAA-3.1", "EAA-4.2"
    pub rule_id: String,
    pub description: String,
    pub recommendation: String,
}

/// Result of an accessibility heuristic scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessibilityResult {
    /// Heuristic pass: every required track keyword was found. Not a certified
    /// compliance verdict, see `check_accessibility`.
    pub compliant: bool,
    pub standard: AccessibilityStandard,
    pub findings: Vec<AccessibilityFinding>,
    pub errors: u32,
    pub warnings: u32,
    pub tracks_present: Vec<AccessibilityTrack>,
    pub tracks_missing: Vec<AccessibilityTrack>,
}

/// Heuristic accessibility check of a DCP or IMP.
///
/// This is a keyword scan, not a certified compliance test: it concatenates the
/// package's CPL XML files and looks for accessibility track markers (AD, HI, SL,
/// CC) by case-insensitive substring, then reports which standard-required tracks
/// appear to be missing. It does not parse the track structure or MCA labels, so
/// a `compliant: true` is evidence the keywords are present, not a legal sign-off.
pub fn check_accessibility(
    package_dir: &std::path::Path,
    standard: AccessibilityStandard,
) -> AccessibilityResult {
    let mut result = AccessibilityResult {
        compliant: true,
        standard,
        findings: Vec::new(),
        errors: 0,
        warnings: 0,
        tracks_present: Vec::new(),
        tracks_missing: Vec::new(),
    };

    // Scan CPL files for track references
    let cpl_content = find_and_read_cpls(package_dir);

    // Detect which tracks are present by searching for MCA labels and track types
    let tracks_found = detect_accessibility_tracks(&cpl_content);
    result.tracks_present = tracks_found;

    // Determine required tracks per standard
    let required = required_tracks(standard);

    for req in &required {
        if !result.tracks_present.contains(req) {
            result.tracks_missing.push(*req);
            let (rule_id, desc, rec) = requirement_details(standard, *req);
            result.findings.push(AccessibilityFinding {
                severity: Severity::Error,
                track_type: *req,
                rule_id,
                description: desc,
                recommendation: rec,
            });
            result.errors += 1;
            result.compliant = false;
        }
    }

    // Check recommended (warning-level) tracks
    let recommended = recommended_tracks(standard);
    for rec_track in &recommended {
        if !result.tracks_present.contains(rec_track) && !result.tracks_missing.contains(rec_track)
        {
            result.findings.push(AccessibilityFinding {
                severity: Severity::Warning,
                track_type: *rec_track,
                rule_id: format!("{}-REC", standard_prefix(standard)),
                description: format!("{rec_track:?} track recommended but not found"),
                recommendation: format!(
                    "Consider adding {rec_track:?} track for broader accessibility"
                ),
            });
            result.warnings += 1;
        }
    }

    result
}

fn find_and_read_cpls(dir: &std::path::Path) -> String {
    let mut content = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.starts_with("CPL") || name.starts_with("cpl"))
                && name.ends_with(".xml")
                && let Ok(c) = std::fs::read_to_string(&path)
            {
                content.push_str(&c);
            }
        }
    }
    content
}

fn detect_accessibility_tracks(cpl_content: &str) -> Vec<AccessibilityTrack> {
    let mut found = Vec::new();
    let lower = cpl_content.to_lowercase();

    // Audio Description — look for MCA label "Visually Impaired" or "AudioDescription"
    if lower.contains("audiodescription")
        || lower.contains("visually impaired")
        || lower.contains("visually-impaired")
        || lower.contains("vi-narration")
    {
        found.push(AccessibilityTrack::AudioDescription);
    }

    // Hearing Impaired — look for HI/SDH markers
    if lower.contains("hearingimpaired")
        || lower.contains("hearing impaired")
        || lower.contains("sdh")
        || lower.contains("hearing-impaired")
    {
        found.push(AccessibilityTrack::HearingImpaired);
    }

    // Sign Language
    if lower.contains("signlanguage")
        || lower.contains("sign language")
        || lower.contains("sign-language")
    {
        found.push(AccessibilityTrack::SignLanguage);
    }

    // Closed Captions
    if lower.contains("closedcaption")
        || lower.contains("closed caption")
        || lower.contains("cea-608")
        || lower.contains("cea-708")
        || lower.contains("cc1")
    {
        found.push(AccessibilityTrack::ClosedCaptions);
    }

    // Open Captions
    if lower.contains("opencaption")
        || lower.contains("open caption")
        || lower.contains("burned-in")
    {
        found.push(AccessibilityTrack::OpenCaptions);
    }

    // Commentary
    if lower.contains("commentary") || lower.contains("director") {
        found.push(AccessibilityTrack::Commentary);
    }

    found
}

fn required_tracks(standard: AccessibilityStandard) -> Vec<AccessibilityTrack> {
    match standard {
        AccessibilityStandard::Cvaa => vec![
            AccessibilityTrack::ClosedCaptions,
            AccessibilityTrack::AudioDescription,
        ],
        AccessibilityStandard::Eaa => vec![
            AccessibilityTrack::AudioDescription,
            AccessibilityTrack::HearingImpaired,
        ],
        AccessibilityStandard::Aoda => vec![
            AccessibilityTrack::ClosedCaptions,
            AccessibilityTrack::AudioDescription,
        ],
        AccessibilityStandard::Ofcom => vec![
            AccessibilityTrack::AudioDescription,
            AccessibilityTrack::HearingImpaired,
            AccessibilityTrack::SignLanguage,
        ],
    }
}

fn recommended_tracks(standard: AccessibilityStandard) -> Vec<AccessibilityTrack> {
    match standard {
        AccessibilityStandard::Cvaa => vec![AccessibilityTrack::HearingImpaired],
        AccessibilityStandard::Eaa => vec![AccessibilityTrack::SignLanguage],
        AccessibilityStandard::Aoda => vec![AccessibilityTrack::HearingImpaired],
        AccessibilityStandard::Ofcom => vec![],
    }
}

fn standard_prefix(standard: AccessibilityStandard) -> &'static str {
    match standard {
        AccessibilityStandard::Cvaa => "CVAA",
        AccessibilityStandard::Eaa => "EAA",
        AccessibilityStandard::Aoda => "AODA",
        AccessibilityStandard::Ofcom => "OFCOM",
    }
}

fn requirement_details(
    standard: AccessibilityStandard,
    track: AccessibilityTrack,
) -> (String, String, String) {
    let prefix = standard_prefix(standard);
    match track {
        AccessibilityTrack::ClosedCaptions => (
            format!("{prefix}-CC-1"),
            "Closed captions track required".into(),
            "Add CEA-608/708 or SMPTE-TT closed caption track".into(),
        ),
        AccessibilityTrack::AudioDescription => (
            format!("{prefix}-AD-1"),
            "Audio description track required".into(),
            "Add an audio description (VI narration) track".into(),
        ),
        AccessibilityTrack::HearingImpaired => (
            format!("{prefix}-HI-1"),
            "Hearing-impaired subtitle track required".into(),
            "Add SDH/HI subtitle track".into(),
        ),
        AccessibilityTrack::SignLanguage => (
            format!("{prefix}-SL-1"),
            "Sign language track required".into(),
            "Add sign language video overlay track".into(),
        ),
        _ => (
            format!("{prefix}-GEN-1"),
            format!("{track:?} track required"),
            format!("Add {track:?} track to package"),
        ),
    }
}

/// Check accessibility compliance against multiple standards.
pub fn check_accessibility_multi(
    package_dir: &std::path::Path,
    standards: &[AccessibilityStandard],
) -> Vec<AccessibilityResult> {
    standards
        .iter()
        .map(|&s| check_accessibility(package_dir, s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_package_fails_cvaa() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert!(!result.compliant);
        assert_eq!(result.errors, 2); // CC + AD required
        assert_eq!(result.tracks_missing.len(), 2);
    }

    #[test]
    fn test_detect_tracks_from_cpl() {
        let content = r#"<MainSoundSequence>
            <MCATagSymbol>AudioDescription</MCATagSymbol>
            <MCATagName>Visually Impaired</MCATagName>
        </MainSoundSequence>
        <MainSubtitle>ClosedCaption CEA-608</MainSubtitle>"#;
        let tracks = detect_accessibility_tracks(content);
        assert!(tracks.contains(&AccessibilityTrack::AudioDescription));
        assert!(tracks.contains(&AccessibilityTrack::ClosedCaptions));
    }

    #[test]
    fn test_required_tracks_vary_by_standard() {
        let cvaa = required_tracks(AccessibilityStandard::Cvaa);
        assert!(cvaa.contains(&AccessibilityTrack::ClosedCaptions));

        let ofcom = required_tracks(AccessibilityStandard::Ofcom);
        assert!(ofcom.contains(&AccessibilityTrack::SignLanguage));
    }
}
