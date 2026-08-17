//! Structural accessibility probe for a DCP or IMP.
//!
//! Every verdict here rests on a named element or attribute in the composition
//! playlist, or on an ST 377-4 MCA label in the sound MXF header. Nothing is
//! inferred from free text, so an annotation that happens to mention captions or
//! a director never counts as a track.

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use serde::{Deserialize, Serialize};

use crate::mca::McaTagSymbol;

/// ISDCF Doc 13 declares a sign-language video program through this
/// ExtensionMetadata scope in the ST 429-16 composition metadata.
const SIGN_LANGUAGE_EXTENSION_SCOPE: &str = "http://isdcf.com/2017/10/SignLanguageVideo";

/// ST 2067-2 core constraints, which declare the IMF sequence elements. The 2020
/// revision moved the namespace from /schemas/ to /ns/ and kept every sequence
/// name the 2016 revision had.
const IMF_CORE_CONSTRAINTS_NAMESPACES: [&str; 2] = [
    "http://www.smpte-ra.org/schemas/2067-2/2016",
    "http://www.smpte-ra.org/ns/2067-2/2020",
];

/// A ST 2067-3 SequenceList accepts any element from a foreign namespace, so a
/// sequence name on its own proves nothing. Only these two settle a track, and
/// both are caption tracks a viewer selects.
const CAPTION_SEQUENCE_NAMES: [&str; 2] = ["HearingImpairedCaptionsSequence", "CDPSequence"];

/// Appended to the AudioDescription evidence, because a
/// VisuallyImpairedTextSequence is not the VI-N narration channel and settles
/// nothing about it. The text track is reported on its own as
/// VisuallyImpairedText.
const VISUALLY_IMPAIRED_TEXT_NOTE: &str = ", and a VisuallyImpairedTextSequence was read, which carries text rather than a narration channel";

/// ISDCF Doc 13 §5.2 requires this MCA tag symbol on the sound channel carrying
/// a sign-language video stream, where §5.1 only recommends the extension
/// metadata. Doc 13 defines no MainSoundConfiguration token for it.
const SIGN_LANGUAGE_TAG_SYMBOL: &str = "SLVS";

/// ST 429-16 MainSoundConfiguration channel token for the hearing-impaired mix.
const HEARING_IMPAIRED_CHANNEL_TOKEN: &str = "HI";

/// ST 429-16 MainSoundConfiguration channel token for the visually-impaired
/// narration mix.
const VISUALLY_IMPAIRED_CHANNEL_TOKEN: &str = "VIN";

/// MainSoundConfiguration writes silent fill channels as this placeholder.
const SILENT_CHANNEL_TOKEN: &str = "-";

const ALL_TRACKS: [AccessibilityTrack; 7] = [
    AccessibilityTrack::AudioDescription,
    AccessibilityTrack::HearingImpaired,
    AccessibilityTrack::SignLanguage,
    AccessibilityTrack::OpenCaptions,
    AccessibilityTrack::ClosedCaptions,
    AccessibilityTrack::Commentary,
    AccessibilityTrack::VisuallyImpairedText,
];

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

/// Accessibility track type. Non-exhaustive, because a new standard or a new
/// schema revision can name a track this list does not have yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AccessibilityTrack {
    /// Visually-impaired narration carried as a sound channel, declared as the
    /// VI-N channel of the ST 429-16 MainSoundConfiguration.
    AudioDescription,
    /// The dialogue-boosted mix for the hearing impaired, declared as the HI
    /// channel of the ST 429-16 MainSoundConfiguration.
    HearingImpaired,
    /// Sign language video, declared by the ISDCF Doc 13 extension metadata.
    SignLanguage,
    /// Captions burned into the picture. Nothing in a package declares these,
    /// so they are always undeterminable.
    OpenCaptions,
    /// A caption track the viewer selects: an ST 429-12 MainClosedCaption asset
    /// in a DCP reel, or an ST 2067-2 HearingImpairedCaptionsSequence or
    /// CDPSequence in an IMF sequence list.
    ClosedCaptions,
    /// Director or audio commentary. Only an ST 2067-2 CommentarySequence
    /// declares one, and commentary can also ride an ordinary audio track, so
    /// this is never reported absent.
    Commentary,
    /// Timed text describing the picture for visually impaired viewers,
    /// declared as an ST 2067-2 VisuallyImpairedTextSequence.
    ///
    /// Deliberately not AudioDescription, and the two must not be merged. This
    /// is text a renderer speaks aloud at playback, while AudioDescription is a
    /// narration channel already carried as audio in the sound essence. They
    /// are declared by different structures, and a package can carry either one
    /// without the other, so reading one as the other would report a track the
    /// package does not have.
    VisuallyImpairedText,
}

/// Severity of an accessibility finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// What the probe could establish about one track type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackStatus {
    /// The package declares the track.
    Present,
    /// The package declares the structure that would carry the track, and the
    /// track is not in it.
    Absent,
    /// Nothing in the package settles it either way.
    Undeterminable,
}

/// One track type, what the probe concluded, and what that rests on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackDetection {
    pub track: AccessibilityTrack,
    pub status: TrackStatus,
    /// What was found, or why nothing could be established.
    pub evidence: String,
}

/// Single compliance finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessibilityFinding {
    pub severity: Severity,
    pub track_type: AccessibilityTrack,
    /// Standard prefix and track code, e.g. "CVAA-CC-1", "EAA-AD-1"
    pub rule_id: String,
    pub description: String,
    pub recommendation: String,
}

/// Result of a structural accessibility probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessibilityResult {
    /// True only when every track the standard requires was positively found in
    /// the package. An undeterminable required track keeps this false.
    pub compliant: bool,
    pub standard: AccessibilityStandard,
    pub findings: Vec<AccessibilityFinding>,
    pub errors: u32,
    pub warnings: u32,
    /// Tracks positively found, whether or not the standard requires them.
    pub tracks_present: Vec<AccessibilityTrack>,
    /// Required tracks that were not positively found, so both the absent and
    /// the undeterminable ones.
    pub tracks_missing: Vec<AccessibilityTrack>,
    /// Every track the probe could not settle, whether or not it is required.
    #[serde(default)]
    pub tracks_undeterminable: Vec<AccessibilityTrack>,
    /// One entry per track type, with the evidence behind each verdict.
    #[serde(default)]
    pub tracks: Vec<TrackDetection>,
}

impl AccessibilityResult {
    /// Status of one track type. Undeterminable when the probe did not record
    /// the track at all.
    pub fn track_status(&self, track: AccessibilityTrack) -> TrackStatus {
        status_of(&self.tracks, track)
    }
}

/// Structural accessibility probe of a DCP or IMP.
///
/// Parses every CPL in the package directory and reads accessibility tracks off
/// the composition structure. From a DCP CPL: an ST 429-12 MainClosedCaption
/// asset under a reel's AssetList, the HI and VI-N channel labels of the
/// ST 429-16 MainSoundConfiguration, and the ISDCF Doc 13 sign-language
/// ExtensionMetadata scope. From an IMF CPL: the ST 2067-2
/// HearingImpairedCaptionsSequence, CDPSequence, CommentarySequence and
/// VisuallyImpairedTextSequence elements under a ST 2067-3 SequenceList, in
/// either the 2016 or the 2020 namespace.
///
/// When no composition declares a MainSoundConfiguration, the sound MXF headers
/// are opened and the sound tracks are settled from their ST 377-4 MCA label
/// subdescriptors instead. Only the header is read, and a file that cannot be
/// resolved, opened or labelled leaves its tracks undeterminable rather than
/// absent. Picture essence is never opened, so anything recorded only in the
/// picture stays undeterminable, as does a track whose carrier the composition
/// does not enumerate.
///
/// `compliant` is true only when every track the standard requires was
/// positively found. It is not a certified compliance verdict, and it is never
/// true while a required track is undeterminable.
pub fn check_accessibility(
    package_dir: &std::path::Path,
    standard: AccessibilityStandard,
) -> AccessibilityResult {
    let evidence = read_package_evidence(package_dir);
    let detections: Vec<TrackDetection> = ALL_TRACKS
        .into_iter()
        .map(|track| detect_track(track, &evidence))
        .collect();

    let mut findings = Vec::new();
    let mut tracks_missing = Vec::new();
    let mut errors = 0;
    let mut warnings = 0;

    for track in required_tracks(standard) {
        let status = status_of(&detections, track);
        if status == TrackStatus::Present {
            continue;
        }
        tracks_missing.push(track);
        errors += 1;
        findings.push(requirement_finding(
            standard,
            track,
            status,
            Severity::Error,
            &detections,
        ));
    }

    for track in recommended_tracks(standard) {
        let status = status_of(&detections, track);
        if status == TrackStatus::Present {
            continue;
        }
        warnings += 1;
        findings.push(requirement_finding(
            standard,
            track,
            status,
            Severity::Warning,
            &detections,
        ));
    }

    AccessibilityResult {
        compliant: tracks_missing.is_empty(),
        standard,
        findings,
        errors,
        warnings,
        tracks_present: tracks_with(&detections, TrackStatus::Present),
        tracks_missing,
        tracks_undeterminable: tracks_with(&detections, TrackStatus::Undeterminable),
        tracks: detections,
    }
}

fn status_of(detections: &[TrackDetection], track: AccessibilityTrack) -> TrackStatus {
    detections
        .iter()
        .find(|d| d.track == track)
        .map(|d| d.status)
        .unwrap_or(TrackStatus::Undeterminable)
}

fn tracks_with(detections: &[TrackDetection], status: TrackStatus) -> Vec<AccessibilityTrack> {
    detections
        .iter()
        .filter(|d| d.status == status)
        .map(|d| d.track)
        .collect()
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

/// Everything the probe managed to read out of the package's compositions.
#[derive(Debug, Default)]
struct PackageEvidence {
    /// Reel AssetList elements read across all compositions. Zero means no
    /// composition described its reels, so nothing about reel assets is known.
    reel_asset_lists: usize,
    /// ST 2067-3 SequenceList elements read, the IMF counterpart of a reel
    /// AssetList.
    sequence_lists: usize,
    closed_caption_assets: usize,
    caption_sequences: usize,
    commentary_sequences: usize,
    visually_impaired_text_sequences: usize,
    sign_language_extensions: usize,
    /// Channel tokens of every MainSoundConfiguration read, silent fill removed.
    /// None means no composition declared a sound configuration.
    sound_channels: Option<Vec<String>>,
    /// MCA tag symbols read from the sound MXFs the compositions reference.
    /// Empty means none could be read, which settles nothing either way.
    mca_tag_symbols: Vec<String>,
}

fn read_package_evidence(package_dir: &std::path::Path) -> PackageEvidence {
    let mut evidence = PackageEvidence::default();
    let mut main_sound_ids: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(package_dir) else {
        return evidence;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_cpl_name = name.to_ascii_lowercase().starts_with("cpl") && name.ends_with(".xml");
        if !is_cpl_name {
            continue;
        }
        let Ok(xml) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(composition) = read_composition(&xml) {
            evidence.reel_asset_lists += composition.reel_asset_lists;
            evidence.sequence_lists += composition.sequence_lists;
            evidence.closed_caption_assets += composition.closed_caption_assets;
            evidence.caption_sequences += composition.caption_sequences;
            evidence.commentary_sequences += composition.commentary_sequences;
            evidence.visually_impaired_text_sequences +=
                composition.visually_impaired_text_sequences;
            evidence.sign_language_extensions += composition.sign_language_extensions;
            if !composition.sound_channels.is_empty() {
                evidence
                    .sound_channels
                    .get_or_insert_with(Vec::new)
                    .extend(composition.sound_channels);
            }
            main_sound_ids.extend(composition.main_sound_ids);
        }
    }

    // read the labels even when a MainSoundConfiguration settled the sound
    // channels, because SLVS has no configuration token and only the MCA labels
    // can rule a sign-language channel in or out
    evidence.mca_tag_symbols = read_mca_tag_symbols(package_dir, &main_sound_ids);

    evidence
}

/// MCA tag symbols carried by the sound MXFs the compositions reference. Empty
/// when no sound file could be resolved, opened, or read, and empty too when the
/// files carry no MCA labels at all, because an unlabelled MXF settles nothing.
fn read_mca_tag_symbols(package_dir: &std::path::Path, main_sound_ids: &[String]) -> Vec<String> {
    let mut symbols = Vec::new();
    for id in main_sound_ids {
        let Some(path) = crate::assetmap::resolve(package_dir, id) else {
            continue;
        };
        let mut reader = asdcplib::pcm::MxfReader::new();
        if reader.open_read(&path.to_string_lossy()).is_err() {
            continue;
        }
        let Ok(labels) = reader.mca_label_subdescriptors() else {
            continue;
        };
        symbols.extend(labels.into_iter().map(|label| label.tag_symbol));
    }
    symbols
}

#[derive(Debug, Default)]
struct CompositionEvidence {
    reel_asset_lists: usize,
    sequence_lists: usize,
    closed_caption_assets: usize,
    caption_sequences: usize,
    commentary_sequences: usize,
    visually_impaired_text_sequences: usize,
    sign_language_extensions: usize,
    sound_channels: Vec<String>,
    main_sound_ids: Vec<String>,
}

/// Read one CPL. Returns None when the document is not a composition playlist or
/// does not parse, because a document the probe could not read is evidence of
/// nothing.
fn read_composition(xml: &str) -> Option<CompositionEvidence> {
    let mut reader = NsReader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut evidence = CompositionEvidence::default();

    loop {
        let (namespace, event) = reader.read_resolved_event().ok()?;
        match event {
            Event::Start(element) => {
                let name = local_name(element.name().as_ref());
                if stack.is_empty() && name != "CompositionPlaylist" {
                    return None;
                }
                record_element(&name, namespace, &element, &stack, &mut evidence);
                stack.push(name);
            }
            Event::Empty(element) => {
                let name = local_name(element.name().as_ref());
                if stack.is_empty() {
                    return None;
                }
                record_element(&name, namespace, &element, &stack, &mut evidence);
            }
            Event::Text(text) => {
                if stack.last().is_some_and(|n| n == "MainSoundConfiguration") {
                    let value = text.unescape().ok()?;
                    evidence.sound_channels.extend(channel_tokens(&value));
                }
                if is_main_sound_id(&stack) {
                    let value = text.unescape().ok()?;
                    if let Some(id) = bare_uuid(&value) {
                        evidence.main_sound_ids.push(id);
                    }
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // quick-xml reaches Eof happily on a truncated document, so an element left
    // open is the only sign that the rest of the composition is missing.
    if !stack.is_empty() {
        return None;
    }
    Some(evidence)
}

fn record_element(
    name: &str,
    namespace: ResolveResult<'_>,
    element: &BytesStart<'_>,
    stack: &[String],
    evidence: &mut CompositionEvidence,
) {
    match name {
        "AssetList" if is_child_of(stack, "Reel") => {
            evidence.reel_asset_lists += 1;
        }
        // ST 429-12 names the element ClosedCaption, older writers used MainClosedCaption
        "ClosedCaption" | "MainClosedCaption" if is_in_reel_asset_list(stack) => {
            evidence.closed_caption_assets += 1;
        }
        "SequenceList" if is_child_of(stack, "Segment") => {
            evidence.sequence_lists += 1;
        }
        "ExtensionMetadata" if has_sign_language_scope(element) => {
            evidence.sign_language_extensions += 1;
        }
        _ => {}
    }

    if !is_child_of(stack, "SequenceList") || !is_imf_core_constraints(namespace) {
        return;
    }
    match name {
        _ if CAPTION_SEQUENCE_NAMES.contains(&name) => evidence.caption_sequences += 1,
        "CommentarySequence" => evidence.commentary_sequences += 1,
        "VisuallyImpairedTextSequence" => evidence.visually_impaired_text_sequences += 1,
        _ => {}
    }
}

/// True at the text of a `<MainSound>` asset's own `<Id>`.
fn is_main_sound_id(stack: &[String]) -> bool {
    stack.last().is_some_and(|n| n == "Id")
        && stack.len() >= 2
        && stack[stack.len() - 2] == "MainSound"
}

fn bare_uuid(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let bare = trimmed.strip_prefix("urn:uuid:").unwrap_or(trimmed);
    let is_uuid = bare.len() == 36 && bare.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    is_uuid.then(|| bare.to_ascii_lowercase())
}

fn is_child_of(stack: &[String], parent: &str) -> bool {
    stack.last().is_some_and(|last| last == parent)
}

fn is_in_reel_asset_list(stack: &[String]) -> bool {
    is_child_of(stack, "AssetList") && stack.iter().any(|ancestor| ancestor == "Reel")
}

/// A DCP CPL is matched on local names alone because Interop and SMPTE reels use
/// different namespaces for the same elements. An IMF sequence gets no such
/// slack: ST 2067-3 lets a SequenceList hold any foreign-namespace element, so
/// the sequence only counts when it is bound to ST 2067-2.
fn is_imf_core_constraints(namespace: ResolveResult<'_>) -> bool {
    let ResolveResult::Bound(bound) = namespace else {
        return false;
    };
    IMF_CORE_CONSTRAINTS_NAMESPACES
        .iter()
        .any(|known| bound.as_ref() == known.as_bytes())
}

fn has_sign_language_scope(element: &BytesStart<'_>) -> bool {
    element.attributes().flatten().any(|attribute| {
        local_name(attribute.key.as_ref()) == "scope"
            && attribute
                .unescape_value()
                .is_ok_and(|value| value == SIGN_LANGUAGE_EXTENSION_SCOPE)
    })
}

/// Split an ST 429-16 MainSoundConfiguration such as
/// `51/L,R,C,LFE,Ls,Rs,HI,VIN,-,-` into its declared channel tokens.
fn channel_tokens(configuration: &str) -> Vec<String> {
    let channels = configuration
        .rsplit('/')
        .next()
        .unwrap_or(configuration)
        .trim();
    channels
        .split(',')
        .map(|token| token.trim().to_ascii_uppercase())
        .filter(|token| !token.is_empty() && token != SILENT_CHANNEL_TOKEN)
        .collect()
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn detect_track(track: AccessibilityTrack, evidence: &PackageEvidence) -> TrackDetection {
    let (status, description) = match track {
        AccessibilityTrack::ClosedCaptions => closed_caption_status(evidence),
        AccessibilityTrack::AudioDescription => {
            let (status, mut description) = sound_channel_status(
                evidence,
                VISUALLY_IMPAIRED_CHANNEL_TOKEN,
                McaTagSymbol::Vi.tag_name(),
                McaTagSymbol::Vi.symbol_string(),
            );
            if status == TrackStatus::Undeterminable
                && evidence.visually_impaired_text_sequences > 0
            {
                description.push_str(VISUALLY_IMPAIRED_TEXT_NOTE);
            }
            (status, description)
        }
        AccessibilityTrack::HearingImpaired => sound_channel_status(
            evidence,
            HEARING_IMPAIRED_CHANNEL_TOKEN,
            McaTagSymbol::Hi.tag_name(),
            McaTagSymbol::Hi.symbol_string(),
        ),
        AccessibilityTrack::SignLanguage => sign_language_status(evidence),
        AccessibilityTrack::OpenCaptions => (
            TrackStatus::Undeterminable,
            "captions burned into the picture are not declared anywhere in a package".to_string(),
        ),
        AccessibilityTrack::Commentary => commentary_status(evidence),
        AccessibilityTrack::VisuallyImpairedText => visually_impaired_text_status(evidence),
    };
    TrackDetection {
        track,
        status,
        evidence: description,
    }
}

fn closed_caption_status(evidence: &PackageEvidence) -> (TrackStatus, String) {
    let mut found = Vec::new();
    if evidence.closed_caption_assets > 0 {
        found.push(format!(
            "{} ST 429-12 MainClosedCaption asset(s) under a reel AssetList",
            evidence.closed_caption_assets
        ));
    }
    if evidence.caption_sequences > 0 {
        found.push(format!(
            "{} ST 2067-2 {} element(s) under a SequenceList",
            evidence.caption_sequences,
            CAPTION_SEQUENCE_NAMES.join(" or ")
        ));
    }
    if !found.is_empty() {
        return (TrackStatus::Present, found.join(", "));
    }

    let mut searched = Vec::new();
    if evidence.reel_asset_lists > 0 {
        searched.push(format!("{} reel AssetList", evidence.reel_asset_lists));
    }
    if evidence.sequence_lists > 0 {
        searched.push(format!("{} SequenceList", evidence.sequence_lists));
    }
    if !searched.is_empty() {
        return (
            TrackStatus::Absent,
            format!("no caption element in the {} read", searched.join(" and ")),
        );
    }
    (
        TrackStatus::Undeterminable,
        "no composition with a reel AssetList or a SequenceList could be read".to_string(),
    )
}

fn commentary_status(evidence: &PackageEvidence) -> (TrackStatus, String) {
    if evidence.commentary_sequences > 0 {
        return (
            TrackStatus::Present,
            format!(
                "{} ST 2067-2 CommentarySequence element(s) under a SequenceList",
                evidence.commentary_sequences
            ),
        );
    }
    (
        TrackStatus::Undeterminable,
        "only an ST 2067-2 CommentarySequence declares commentary, and commentary can also ride an ordinary audio track, so its absence settles nothing".to_string(),
    )
}

/// Only an IMF SequenceList can declare this track, so a DCP composition leaves
/// it undeterminable however completely it describes its reels.
fn visually_impaired_text_status(evidence: &PackageEvidence) -> (TrackStatus, String) {
    if evidence.visually_impaired_text_sequences > 0 {
        return (
            TrackStatus::Present,
            format!(
                "{} ST 2067-2 VisuallyImpairedTextSequence element(s) under a SequenceList",
                evidence.visually_impaired_text_sequences
            ),
        );
    }
    if evidence.sequence_lists > 0 {
        return (
            TrackStatus::Absent,
            format!(
                "no VisuallyImpairedTextSequence in the {} SequenceList read",
                evidence.sequence_lists
            ),
        );
    }
    (
        TrackStatus::Undeterminable,
        "only an ST 2067-2 VisuallyImpairedTextSequence declares this track, and no SequenceList could be read".to_string(),
    )
}

fn sound_channel_status(
    evidence: &PackageEvidence,
    token: &str,
    channel_name: &str,
    tag_symbol: &str,
) -> (TrackStatus, String) {
    let Some(channels) = evidence.sound_channels.as_ref() else {
        return mca_label_status(
            evidence,
            tag_symbol,
            &format!("no ST 429-16 MainSoundConfiguration could be read, and {channel_name}"),
        );
    };
    if channels.iter().any(|c| c == token) {
        (
            TrackStatus::Present,
            format!("MainSoundConfiguration declares a {channel_name} ({token}) channel"),
        )
    } else {
        (
            TrackStatus::Absent,
            format!("MainSoundConfiguration declares no {channel_name} ({token}) channel"),
        )
    }
}

/// Settle a track from the sound MXF's MCA tag symbols. `subject` names what the
/// labels were searched for, and reads as the start of the evidence sentence.
fn mca_label_status(
    evidence: &PackageEvidence,
    tag_symbol: &str,
    subject: &str,
) -> (TrackStatus, String) {
    if evidence.mca_tag_symbols.is_empty() {
        return (
            TrackStatus::Undeterminable,
            format!("{subject} could not be read from the sound MXF's MCA labels either"),
        );
    }
    if evidence.mca_tag_symbols.iter().any(|s| s == tag_symbol) {
        (
            TrackStatus::Present,
            format!("the sound MXF carries an ST 377-4 {tag_symbol} MCA label"),
        )
    } else {
        (
            TrackStatus::Absent,
            format!(
                "{subject} is not among the sound MXF's MCA labels ({})",
                evidence.mca_tag_symbols.join(", ")
            ),
        )
    }
}

fn sign_language_status(evidence: &PackageEvidence) -> (TrackStatus, String) {
    if evidence.sign_language_extensions > 0 {
        return (
            TrackStatus::Present,
            "ISDCF Doc 13 SignLanguageVideo extension metadata".to_string(),
        );
    }
    mca_label_status(
        evidence,
        SIGN_LANGUAGE_TAG_SYMBOL,
        "the ISDCF Doc 13 extension metadata is optional and absent, and a sign-language video channel",
    )
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

struct Requirement {
    rule_id: String,
    label: &'static str,
    recommendation: &'static str,
}

fn requirement(standard: AccessibilityStandard, track: AccessibilityTrack) -> Requirement {
    let prefix = standard_prefix(standard);
    let (code, label, recommendation) = match track {
        AccessibilityTrack::ClosedCaptions => (
            "CC-1",
            "Closed caption asset",
            "Add an ST 429-12 MainClosedCaption asset to every reel",
        ),
        AccessibilityTrack::AudioDescription => (
            "AD-1",
            "Audio description channel",
            "Add a VI-N narration channel and declare it in MainSoundConfiguration",
        ),
        AccessibilityTrack::HearingImpaired => (
            "HI-1",
            "Hearing impaired channel",
            "Add an HI mix channel and declare it in MainSoundConfiguration",
        ),
        AccessibilityTrack::SignLanguage => (
            "SL-1",
            "Sign language video",
            "Add an ISDCF Doc 13 sign-language program and its ExtensionMetadata scope",
        ),
        AccessibilityTrack::OpenCaptions => (
            "OC-1",
            "Open captions",
            "Confirm the picture carries burned-in captions",
        ),
        AccessibilityTrack::Commentary => (
            "COM-1",
            "Commentary track",
            "Confirm the commentary track against the source deliverable",
        ),
        AccessibilityTrack::VisuallyImpairedText => (
            "VIT-1",
            "Visually impaired text track",
            "Add an ST 2067-2 VisuallyImpairedTextSequence to the composition",
        ),
    };
    Requirement {
        rule_id: format!("{prefix}-{code}"),
        label,
        recommendation,
    }
}

fn requirement_finding(
    standard: AccessibilityStandard,
    track: AccessibilityTrack,
    status: TrackStatus,
    severity: Severity,
    detections: &[TrackDetection],
) -> AccessibilityFinding {
    let requirement = requirement(standard, track);
    let prefix = standard_prefix(standard);
    let expectation = match severity {
        Severity::Error => "required",
        _ => "recommended",
    };
    let evidence = detections
        .iter()
        .find(|d| d.track == track)
        .map(|d| d.evidence.as_str())
        .unwrap_or("nothing was read");

    let (description, recommendation) = match status {
        TrackStatus::Undeterminable => (
            format!(
                "{} {expectation} by {prefix}, and this check cannot establish whether the package has one: {evidence}",
                requirement.label
            ),
            format!(
                "Confirm {} against the source deliverable, the package carries no evidence either way",
                requirement.label.to_lowercase()
            ),
        ),
        _ => (
            format!(
                "{} {expectation} by {prefix} and not declared by the package: {evidence}",
                requirement.label
            ),
            requirement.recommendation.to_string(),
        ),
    };

    AccessibilityFinding {
        severity,
        track_type: track,
        rule_id: requirement.rule_id,
        description,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SIGN_LANGUAGE_METADATA: &str = r#"
        <meta:ExtensionMetadataList xmlns:meta="http://www.smpte-ra.org/schemas/429-16/2014/CPL-Metadata">
          <meta:ExtensionMetadata scope="http://isdcf.com/2017/10/SignLanguageVideo">
            <meta:Name>Sign Language Video</meta:Name>
          </meta:ExtensionMetadata>
        </meta:ExtensionMetadataList>"#;

    /// A one-reel SMPTE DCP CPL. `reel_extras` goes inside the reel AssetList,
    /// `metadata_extras` inside the ST 429-16 composition metadata.
    fn cpl(annotation: &str, sound_configuration: Option<&str>, reel_extras: &str) -> String {
        let metadata = match sound_configuration {
            Some(config) => format!(
                "<meta:CompositionMetadataAsset xmlns:meta=\"http://www.smpte-ra.org/schemas/429-16/2014/CPL-Metadata\">\
                 <meta:MainSoundConfiguration>{config}</meta:MainSoundConfiguration>\
                 </meta:CompositionMetadataAsset>"
            ),
            None => String::new(),
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/429-7/2006/CPL">
  <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000001</Id>
  <AnnotationText>{annotation}</AnnotationText>
  <ContentTitleText>Test</ContentTitleText>
  <ReelList>
    <Reel>
      <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000002</Id>
      <AssetList>
        <MainPicture>
          <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000003</Id>
        </MainPicture>
        <MainSound>
          <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000004</Id>
        </MainSound>
        {reel_extras}
        {metadata}
      </AssetList>
    </Reel>
  </ReelList>
</CompositionPlaylist>"#
        )
    }

    const CLOSED_CAPTION_ASSET: &str = r#"<MainClosedCaption>
          <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000005</Id>
          <Language>en</Language>
        </MainClosedCaption>"#;

    const IMF_2016_NAMESPACE: &str = "http://www.smpte-ra.org/schemas/2067-2/2016";
    const IMF_2020_NAMESPACE: &str = "http://www.smpte-ra.org/ns/2067-2/2020";
    const FOREIGN_NAMESPACE: &str = "http://example.invalid/private-sequences";

    /// A one-segment IMF CPL. `sequences` goes inside the SequenceList,
    /// `segment_extras` inside the Segment but outside the SequenceList.
    fn imf_cpl(
        core_constraints_namespace: &str,
        annotation: &str,
        sequences: &str,
        segment_extras: &str,
    ) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/2067-3/2016"
                     xmlns:cc="{core_constraints_namespace}"
                     xmlns:vendor="{FOREIGN_NAMESPACE}">
  <Id>urn:uuid:2e0f0b1a-0000-4000-8000-000000000001</Id>
  <Annotation>{annotation}</Annotation>
  <ContentTitle>Test</ContentTitle>
  <SegmentList>
    <Segment>
      <Id>urn:uuid:2e0f0b1a-0000-4000-8000-000000000002</Id>
      <SequenceList>
        <cc:MainImageSequence>
          <Id>urn:uuid:2e0f0b1a-0000-4000-8000-000000000003</Id>
          <TrackId>urn:uuid:2e0f0b1a-0000-4000-8000-000000000004</TrackId>
        </cc:MainImageSequence>
        <cc:MainAudioSequence>
          <Id>urn:uuid:2e0f0b1a-0000-4000-8000-000000000005</Id>
          <TrackId>urn:uuid:2e0f0b1a-0000-4000-8000-000000000006</TrackId>
        </cc:MainAudioSequence>
        {sequences}
      </SequenceList>
      {segment_extras}
    </Segment>
  </SegmentList>
</CompositionPlaylist>"#
        )
    }

    fn sequence(prefix: &str, name: &str) -> String {
        format!(
            "<{prefix}:{name}>\
             <Id>urn:uuid:2e0f0b1a-0000-4000-8000-00000000000a</Id>\
             <TrackId>urn:uuid:2e0f0b1a-0000-4000-8000-00000000000b</TrackId>\
             </{prefix}:{name}>"
        )
    }

    fn package(cpl_xml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CPL_test.xml"), cpl_xml).unwrap();
        dir
    }

    fn status(dir: &Path, track: AccessibilityTrack) -> TrackStatus {
        check_accessibility(dir, AccessibilityStandard::Cvaa).track_status(track)
    }

    /// The MainSound asset id the `cpl` helper writes.
    const MAIN_SOUND_ID: &str = "1e0f0b1a-0000-4000-8000-000000000004";

    /// Wrap a real PCM MXF carrying `mca_config` into the package and register it
    /// in the ASSETMAP. The file is deliberately not named after its asset id,
    /// which is what forces the lookup to go through the ASSETMAP.
    fn add_sound_mxf(package: &Path, channels: u16, mca_config: &str) {
        let wav = package.parent().unwrap().join("source.wav");
        let spec = hound::WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let frames = 48000;
        crate::wav_io::write_interleaved(&wav, spec, &vec![0.0; channels as usize * frames])
            .unwrap();

        let mut asset_uuid = [0u8; 16];
        for (byte, pair) in asset_uuid
            .iter_mut()
            .zip(MAIN_SOUND_ID.replace('-', "").as_bytes().chunks(2))
        {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }

        let result = crate::mxf_wrap::mxf_wrap(&crate::mxf_wrap::MxfWrapOptions {
            input_files: vec![wav],
            output: package.join("audio_track.mxf"),
            essence_type: crate::mxf_wrap::EssenceType::Pcm,
            standard: crate::mxf_wrap::MxfStandard::AsDcp,
            fps_num: 24,
            fps_den: 1,
            partition_size: 0,
            encryption: None,
            mca_config: Some(crate::mxf_wrap::McaConfig {
                labels: mca_config.to_string(),
                spoken_language: None,
            }),
            resource_ids: vec![],
            hdr: None,
            asset_uuid: Some(asset_uuid),
            timed_text_duration_frames: None,
        });
        assert!(result.success, "sound wrap failed: {}", result.error);

        std::fs::write(
            package.join("ASSETMAP.xml"),
            format!(
                r#"<AssetMap><AssetList>
                  <Asset><Id>urn:uuid:{MAIN_SOUND_ID}</Id>
                    <ChunkList><Chunk><Path>audio_track.mxf</Path></Chunk></ChunkList></Asset>
                </AssetList></AssetMap>"#
            ),
        )
        .unwrap();
    }

    /// A package whose CPL declares no MainSoundConfiguration, so the sound
    /// tracks can only be settled from the MXF's MCA labels.
    fn package_without_sound_configuration() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("dcp");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(package.join("CPL_test.xml"), cpl("Feature", None, "")).unwrap();
        dir
    }

    #[test]
    fn mca_labels_settle_the_sound_tracks_when_the_cpl_declares_no_configuration() {
        let dir = package_without_sound_configuration();
        let package = dir.path().join("dcp");
        add_sound_mxf(&package, 9, "51(L,R,C,LFE,Ls,Rs),HI,VIN,SLVS");

        for track in [
            AccessibilityTrack::AudioDescription,
            AccessibilityTrack::HearingImpaired,
            AccessibilityTrack::SignLanguage,
        ] {
            assert_eq!(
                status(&package, track),
                TrackStatus::Present,
                "{track:?} should be read off the MCA labels"
            );
        }
    }

    #[test]
    fn mca_labels_without_the_accessibility_channels_report_absent() {
        let dir = package_without_sound_configuration();
        let package = dir.path().join("dcp");
        add_sound_mxf(&package, 6, "51(L,R,C,LFE,Ls,Rs)");

        for track in [
            AccessibilityTrack::AudioDescription,
            AccessibilityTrack::HearingImpaired,
            AccessibilityTrack::SignLanguage,
        ] {
            assert_eq!(
                status(&package, track),
                TrackStatus::Absent,
                "{track:?} should be absent against a labelled 5.1 MXF"
            );
        }
    }

    #[test]
    fn a_sound_mxf_that_cannot_be_resolved_leaves_the_tracks_undeterminable() {
        let dir = package_without_sound_configuration();
        let package = dir.path().join("dcp");

        for track in [
            AccessibilityTrack::AudioDescription,
            AccessibilityTrack::HearingImpaired,
            AccessibilityTrack::SignLanguage,
        ] {
            assert_eq!(status(&package, track), TrackStatus::Undeterminable);
        }
    }

    #[test]
    fn the_sound_configuration_wins_over_the_mca_labels() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("dcp");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(
            package.join("CPL_test.xml"),
            cpl("Feature", Some("51(L,R,C,LFE,Ls,Rs)"), ""),
        )
        .unwrap();
        add_sound_mxf(&package, 9, "51(L,R,C,LFE,Ls,Rs),HI,VIN,SLVS");

        assert_eq!(
            status(&package, AccessibilityTrack::HearingImpaired),
            TrackStatus::Absent,
            "a MainSoundConfiguration without HI settles the track on its own"
        );
    }

    #[test]
    fn closed_captions_detected_from_the_asset_element() {
        let dir = package(&cpl("Feature", None, CLOSED_CAPTION_ASSET));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::ClosedCaptions),
            TrackStatus::Present
        );
    }

    /// The element ST 429-12 declares, prefixed the way libdcp writes it.
    #[test]
    fn closed_captions_detected_from_the_st_429_12_element() {
        const ST_429_12_ASSET: &str = r#"<tt:ClosedCaption xmlns:tt="http://www.smpte-ra.org/schemas/429-12/2008/TT">
          <Id>urn:uuid:1e0f0b1a-0000-4000-8000-000000000005</Id>
          <tt:Language>en</tt:Language>
        </tt:ClosedCaption>"#;
        let dir = package(&cpl("Feature", None, ST_429_12_ASSET));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::ClosedCaptions),
            TrackStatus::Present
        );
    }

    #[test]
    fn caption_wording_in_an_annotation_is_not_a_closed_caption_track() {
        let dir = package(&cpl(
            "Closed Caption reference master, director commentary, SDH, burned-in",
            None,
            "",
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
        assert!(
            !result
                .tracks_present
                .contains(&AccessibilityTrack::ClosedCaptions)
        );
        assert!(!result.compliant);
    }

    #[test]
    fn commentary_wording_never_reports_a_commentary_track() {
        let dir = package(&cpl("Director commentary version", None, ""));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::Commentary),
            TrackStatus::Undeterminable
        );
    }

    #[test]
    fn undeterminable_required_track_is_not_a_pass() {
        // The CPL declares its reel assets but no sound configuration, so the
        // VI-N channel cannot be settled either way.
        let dir = package(&cpl("Feature", None, CLOSED_CAPTION_ASSET));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);

        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Undeterminable
        );
        assert!(
            result
                .tracks_undeterminable
                .contains(&AccessibilityTrack::AudioDescription)
        );
        assert!(!result.compliant);
        assert!(
            result
                .tracks_missing
                .contains(&AccessibilityTrack::AudioDescription)
        );
        assert!(
            !result
                .tracks_present
                .contains(&AccessibilityTrack::AudioDescription)
        );
        let finding = result
            .findings
            .iter()
            .find(|f| f.track_type == AccessibilityTrack::AudioDescription)
            .expect("audio description finding");
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.description.contains("cannot establish"));
    }

    #[test]
    fn sound_configuration_settles_the_accessibility_channels() {
        let dir = package(&cpl("Feature", Some("51/L,R,C,LFE,Ls,Rs,HI,VIN,-,-"), ""));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Eaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Present
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::HearingImpaired),
            TrackStatus::Present
        );
        assert!(result.compliant);
        assert_eq!(result.errors, 0);
        assert!(result.tracks_missing.is_empty());
    }

    #[test]
    fn sound_configuration_without_the_channels_reports_absent() {
        let dir = package(&cpl("Feature", Some("51/L,R,C,LFE,Ls,Rs"), ""));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Eaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Absent
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::HearingImpaired),
            TrackStatus::Absent
        );
        assert!(!result.compliant);
        assert_eq!(result.errors, 2);
    }

    #[test]
    fn cvaa_passes_only_with_both_captions_and_narration() {
        let dir = package(&cpl(
            "Feature",
            Some("51/L,R,C,LFE,Ls,Rs,VIN,-"),
            CLOSED_CAPTION_ASSET,
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert!(result.compliant);
        assert_eq!(result.errors, 0);
        // HI is only recommended under CVAA, so its absence is a warning.
        assert_eq!(result.warnings, 1);
        assert_eq!(
            result.findings[0].track_type,
            AccessibilityTrack::HearingImpaired
        );
        assert_eq!(result.findings[0].severity, Severity::Warning);
    }

    #[test]
    fn sign_language_comes_from_the_isdcf_extension_scope() {
        let with_extension = package(&cpl(
            "Feature",
            Some("51/L,R,C,LFE,Ls,Rs,HI,VIN"),
            SIGN_LANGUAGE_METADATA,
        ));
        let result = check_accessibility(with_extension.path(), AccessibilityStandard::Ofcom);
        assert_eq!(
            result.track_status(AccessibilityTrack::SignLanguage),
            TrackStatus::Present
        );
        assert!(result.compliant);

        let without = package(&cpl("Feature", Some("51/L,R,C,LFE,Ls,Rs,HI,VIN"), ""));
        let result = check_accessibility(without.path(), AccessibilityStandard::Ofcom);
        assert_eq!(
            result.track_status(AccessibilityTrack::SignLanguage),
            TrackStatus::Undeterminable
        );
        assert!(!result.compliant);
        assert_eq!(result.errors, 1);
    }

    #[test]
    fn open_captions_are_always_undeterminable() {
        let dir = package(&cpl(
            "Feature",
            Some("51/L,R,C,LFE,Ls,Rs,HI,VIN"),
            CLOSED_CAPTION_ASSET,
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::OpenCaptions),
            TrackStatus::Undeterminable
        );
        assert!(
            result
                .tracks_undeterminable
                .contains(&AccessibilityTrack::OpenCaptions)
        );
    }

    #[test]
    fn empty_package_fails_and_settles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert!(!result.compliant);
        assert_eq!(result.errors, 2);
        assert_eq!(result.tracks_missing.len(), 2);
        assert!(result.tracks_present.is_empty());
        assert_eq!(result.tracks_undeterminable.len(), ALL_TRACKS.len());
    }

    #[test]
    fn unreadable_composition_settles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CPL_broken.xml"),
            "<CompositionPlaylist><ReelList><Reel><AssetList><MainClosedCaption>",
        )
        .unwrap();
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Undeterminable
        );
        assert!(!result.compliant);
    }

    #[test]
    fn non_cpl_xml_in_the_directory_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("PKL_test.xml"),
            "<PackingList><AssetList><MainClosedCaption/></AssetList></PackingList>",
        )
        .unwrap();
        std::fs::write(dir.path().join("CPL_test.xml"), cpl("Feature", None, "")).unwrap();
        assert_eq!(
            status(dir.path(), AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
    }

    #[test]
    fn imf_caption_sequences_are_detected_in_both_schema_revisions() {
        for (namespace, element) in [
            (IMF_2016_NAMESPACE, "HearingImpairedCaptionsSequence"),
            (IMF_2020_NAMESPACE, "HearingImpairedCaptionsSequence"),
            (IMF_2016_NAMESPACE, "CDPSequence"),
            (IMF_2020_NAMESPACE, "CDPSequence"),
        ] {
            let dir = package(&imf_cpl(namespace, "Feature", &sequence("cc", element), ""));
            let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
            assert_eq!(
                result.track_status(AccessibilityTrack::ClosedCaptions),
                TrackStatus::Present,
                "{element} in {namespace}"
            );
            let detection = result
                .tracks
                .iter()
                .find(|d| d.track == AccessibilityTrack::ClosedCaptions)
                .expect("closed caption detection");
            assert!(
                detection.evidence.contains("SequenceList"),
                "evidence should name where it looked: {}",
                detection.evidence
            );
        }
    }

    #[test]
    fn imf_commentary_sequence_is_detected_from_its_element() {
        let dir = package(&imf_cpl(
            IMF_2016_NAMESPACE,
            "Feature",
            &sequence("cc", "CommentarySequence"),
            "",
        ));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::Commentary),
            TrackStatus::Present
        );
    }

    #[test]
    fn imf_forced_narrative_only_exists_in_the_2020_revision_and_settles_nothing() {
        let dir = package(&imf_cpl(
            IMF_2020_NAMESPACE,
            "Feature",
            &sequence("cc", "ForcedNarrativeSequence"),
            "",
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::Commentary),
            TrackStatus::Undeterminable
        );
        assert!(result.tracks_present.is_empty());
    }

    #[test]
    fn imf_annotation_wording_is_not_a_track() {
        let dir = package(&imf_cpl(
            IMF_2016_NAMESPACE,
            "Director commentary master with closed captions and CDPSequence notes",
            "",
            "",
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::Commentary),
            TrackStatus::Undeterminable
        );
        assert!(result.tracks_present.is_empty());
        assert!(!result.compliant);
    }

    #[test]
    fn imf_sequence_in_a_foreign_namespace_is_not_a_track() {
        // ST 2067-3 lets a SequenceList carry any foreign-namespace element, so
        // the name alone must not count.
        let sequences = format!(
            "{}{}",
            sequence("vendor", "CommentarySequence"),
            sequence("vendor", "HearingImpairedCaptionsSequence")
        );
        let dir = package(&imf_cpl(IMF_2016_NAMESPACE, "Feature", &sequences, ""));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::Commentary),
            TrackStatus::Undeterminable
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
    }

    #[test]
    fn imf_sequence_outside_a_sequence_list_is_not_a_track() {
        let outside = format!(
            "{}{}",
            sequence("cc", "CommentarySequence"),
            sequence("cc", "HearingImpairedCaptionsSequence")
        );
        let dir = package(&imf_cpl(IMF_2016_NAMESPACE, "Feature", "", &outside));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::Commentary),
            TrackStatus::Undeterminable
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
    }

    #[test]
    fn imf_visually_impaired_text_does_not_settle_audio_description() {
        let dir = package(&imf_cpl(
            IMF_2016_NAMESPACE,
            "Feature",
            &sequence("cc", "VisuallyImpairedTextSequence"),
            "",
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Undeterminable
        );
        assert!(!result.compliant);
        assert!(
            !result
                .tracks_present
                .contains(&AccessibilityTrack::AudioDescription)
        );
        let detection = result
            .tracks
            .iter()
            .find(|d| d.track == AccessibilityTrack::AudioDescription)
            .expect("audio description detection");
        assert!(
            detection.evidence.contains("VisuallyImpairedTextSequence"),
            "evidence should say why the text track settles nothing: {}",
            detection.evidence
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::VisuallyImpairedText),
            TrackStatus::Present,
            "the sequence settles its own track"
        );
    }

    #[test]
    fn visually_impaired_text_is_read_from_its_sequence_in_both_schema_revisions() {
        for namespace in [IMF_2016_NAMESPACE, IMF_2020_NAMESPACE] {
            let dir = package(&imf_cpl(
                namespace,
                "Feature",
                &sequence("cc", "VisuallyImpairedTextSequence"),
                "",
            ));
            assert_eq!(
                status(dir.path(), AccessibilityTrack::VisuallyImpairedText),
                TrackStatus::Present,
                "{namespace}"
            );
        }
    }

    #[test]
    fn a_sequence_list_without_the_text_sequence_reports_it_absent() {
        let dir = package(&imf_cpl(IMF_2016_NAMESPACE, "Feature", "", ""));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::VisuallyImpairedText),
            TrackStatus::Absent
        );
    }

    #[test]
    fn visually_impaired_text_in_a_foreign_namespace_is_not_a_track() {
        let dir = package(&imf_cpl(
            IMF_2016_NAMESPACE,
            "Feature",
            &sequence("vendor", "VisuallyImpairedTextSequence"),
            "",
        ));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::VisuallyImpairedText),
            TrackStatus::Absent
        );
    }

    #[test]
    fn a_dcp_composition_leaves_visually_impaired_text_undeterminable() {
        // no DCP structure declares the ST 2067-2 sequence, so a fully described
        // reel is not evidence that the track is missing
        let dir = package(&cpl(
            "Feature",
            Some("51/L,R,C,LFE,Ls,Rs,HI,VIN"),
            CLOSED_CAPTION_ASSET,
        ));
        assert_eq!(
            status(dir.path(), AccessibilityTrack::VisuallyImpairedText),
            TrackStatus::Undeterminable
        );
    }

    #[test]
    fn imf_composition_settles_no_dcp_only_track() {
        let dir = package(&imf_cpl(
            IMF_2016_NAMESPACE,
            "Feature",
            &sequence("cc", "HearingImpairedCaptionsSequence"),
            "",
        ));
        let result = check_accessibility(dir.path(), AccessibilityStandard::Ofcom);
        // MainSoundConfiguration and the ISDCF scope are DCP-only, so the sound
        // channels and sign language stay unsettled.
        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Undeterminable
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::HearingImpaired),
            TrackStatus::Undeterminable
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::SignLanguage),
            TrackStatus::Undeterminable
        );
        assert!(!result.compliant);
        assert_eq!(result.errors, 3);
    }

    #[test]
    fn a_dcp_and_an_imf_composition_side_by_side_do_not_interfere() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CPL_dcp.xml"),
            cpl("Feature", Some("51/L,R,C,LFE,Ls,Rs,HI,VIN"), ""),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("CPL_imf.xml"),
            imf_cpl(
                IMF_2020_NAMESPACE,
                "Feature",
                &sequence("cc", "CommentarySequence"),
                "",
            ),
        )
        .unwrap();

        let result = check_accessibility(dir.path(), AccessibilityStandard::Cvaa);
        // The DCP CPL settles the sound channels, the IMF CPL the commentary,
        // and neither invents a caption track for the other.
        assert_eq!(
            result.track_status(AccessibilityTrack::AudioDescription),
            TrackStatus::Present
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::HearingImpaired),
            TrackStatus::Present
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::Commentary),
            TrackStatus::Present
        );
        assert_eq!(
            result.track_status(AccessibilityTrack::ClosedCaptions),
            TrackStatus::Absent
        );
        assert!(!result.compliant);
    }

    #[test]
    fn required_tracks_vary_by_standard() {
        assert_eq!(
            required_tracks(AccessibilityStandard::Cvaa),
            vec![
                AccessibilityTrack::ClosedCaptions,
                AccessibilityTrack::AudioDescription
            ]
        );
        assert_eq!(
            required_tracks(AccessibilityStandard::Ofcom),
            vec![
                AccessibilityTrack::AudioDescription,
                AccessibilityTrack::HearingImpaired,
                AccessibilityTrack::SignLanguage
            ]
        );
    }

    #[test]
    fn every_standard_reports_a_finding_per_unmet_requirement() {
        let dir = tempfile::tempdir().unwrap();
        for standard in [
            AccessibilityStandard::Cvaa,
            AccessibilityStandard::Eaa,
            AccessibilityStandard::Aoda,
            AccessibilityStandard::Ofcom,
        ] {
            let result = check_accessibility(dir.path(), standard);
            assert_eq!(result.errors as usize, required_tracks(standard).len());
            for track in required_tracks(standard) {
                assert!(
                    result.findings.iter().any(|f| f.track_type == track
                        && f.rule_id.starts_with(standard_prefix(standard))),
                    "{standard:?} has no finding for {track:?}"
                );
            }
        }
    }

    #[test]
    fn channel_tokens_drop_the_group_label_and_silent_fill() {
        assert_eq!(
            channel_tokens("51/L,R,C,LFE,Ls,Rs,HI,VIN,-,-"),
            vec!["L", "R", "C", "LFE", "LS", "RS", "HI", "VIN"]
        );
        assert!(channel_tokens("-,-,-").is_empty());
    }
}
