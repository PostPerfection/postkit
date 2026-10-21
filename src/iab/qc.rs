use std::collections::{BTreeSet, HashSet};

use super::reader::LARGEST_FRAME_BYTES;
use super::render::{ALWAYS_USE, is_activated};
use super::{
    BedDefinition, BedRemap, BitDepth, Decorrelation, Frame, FrameRate, Gain, IabError,
    ObjectDefinition, ObjectPan, ObjectSnap, ObjectSpread, SampleRate, ZoneGain, parse_frame,
};
use ConstraintSet::{CinemaSt429_18, CinemaSt2098_2, ImfSt2067_201, ImfSt2098_2, Rdd57Profile1};
use Severity::{Error, Warning};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Dcp,
    Imp,
}

impl PackageKind {
    fn constraint_sets(self) -> &'static [ConstraintSet] {
        match self {
            PackageKind::Dcp => &[
                ConstraintSet::CinemaSt2098_2,
                ConstraintSet::CinemaSt429_18,
                ConstraintSet::Rdd57Profile1,
            ],
            PackageKind::Imp => &[ConstraintSet::ImfSt2098_2, ConstraintSet::ImfSt2067_201],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintSet {
    // ST 2098-2 as a DCP carries it
    CinemaSt2098_2,
    CinemaSt429_18,
    Rdd57Profile1,
    // ST 2098-2 as an IMP carries it
    ImfSt2098_2,
    ImfSt2067_201,
}

impl ConstraintSet {
    // this set's state is no better than the state of any of these
    fn builds_on(self) -> &'static [ConstraintSet] {
        match self {
            ConstraintSet::CinemaSt2098_2 | ConstraintSet::ImfSt2098_2 => &[],
            ConstraintSet::CinemaSt429_18 => &[ConstraintSet::CinemaSt2098_2],
            ConstraintSet::Rdd57Profile1 => {
                &[ConstraintSet::CinemaSt2098_2, ConstraintSet::CinemaSt429_18]
            }
            ConstraintSet::ImfSt2067_201 => &[ConstraintSet::ImfSt2098_2],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConformanceState {
    Valid,
    ValidWithWarning,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub message: &'static str,
    pub severity: Severity,
    pub clause: &'static str,
    pub occurrence_count: u64,
    pub frame_count: u32,
    pub first_frame_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetResult {
    pub set: ConstraintSet,
    pub state: ConformanceState,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFormat {
    pub sample_rate: SampleRate,
    pub bit_depth: BitDepth,
    pub frame_rate: FrameRate,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamSummary {
    // the frames that parsed
    pub frame_count: u32,
    pub first_frame_format: Option<FrameFormat>,
    pub largest_max_rendered: u32,
    pub largest_bed_count: usize,
    pub largest_bed_channel_count: usize,
    pub largest_object_count: usize,
    pub largest_frame_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    pub frame_index: u32,
    pub error: IabError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub sets: Vec<SetResult>,
    pub summary: StreamSummary,
    pub parse_failure: Option<ParseFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("IAB checking stopped at frame {frame_index}, which does not parse")]
pub struct CheckingStopped {
    pub frame_index: u32,
}

struct FindingTally {
    check: &'static Check,
    occurrence_count: u64,
    frame_count: u32,
    first_frame_index: Option<u32>,
}

pub struct StreamChecker {
    package: PackageKind,
    tallies: Vec<FindingTally>,
    previous: Option<Frame>,
    summary: StreamSummary,
    parse_failure: Option<ParseFailure>,
}

impl StreamChecker {
    pub fn new(package: PackageKind) -> Self {
        let sets = package.constraint_sets();
        let tallies = CHECKS
            .iter()
            .filter(|check| sets.contains(&check.set))
            .map(|check| FindingTally {
                check,
                occurrence_count: 0,
                frame_count: 0,
                first_frame_index: None,
            })
            .collect();
        StreamChecker {
            package,
            tallies,
            previous: None,
            summary: StreamSummary::default(),
            parse_failure: None,
        }
    }

    // the bytes of one IABitstream frame, preamble included
    pub fn push_frame(&mut self, bytes: &[u8]) -> Result<(), CheckingStopped> {
        if let Some(failure) = &self.parse_failure {
            return Err(CheckingStopped {
                frame_index: failure.frame_index,
            });
        }
        let frame_index = self.summary.frame_count;
        let frame = match parse_frame(bytes) {
            Ok(frame) => frame,
            Err(error) => {
                self.parse_failure = Some(ParseFailure { frame_index, error });
                return Err(CheckingStopped { frame_index });
            }
        };
        let under_check = FrameUnderCheck::new(&frame, self.previous.as_ref(), bytes.len());
        for tally in &mut self.tallies {
            let occurrences = (tally.check.occurrences)(&under_check);
            if occurrences > 0 {
                tally.occurrence_count += occurrences as u64;
                tally.frame_count += 1;
                tally.first_frame_index.get_or_insert(frame_index);
            }
        }
        self.add_to_summary(&frame, bytes.len());
        self.previous = Some(frame);
        Ok(())
    }

    fn add_to_summary(&mut self, frame: &Frame, byte_count: usize) {
        let summary = &mut self.summary;
        summary.frame_count += 1;
        summary.first_frame_format.get_or_insert(FrameFormat {
            sample_rate: frame.sample_rate,
            bit_depth: frame.bit_depth,
            frame_rate: frame.frame_rate,
        });
        let bed_channel_count = frame.beds.iter().map(|bed| bed.channels.len()).sum();
        summary.largest_max_rendered = summary.largest_max_rendered.max(frame.max_rendered);
        summary.largest_bed_count = summary.largest_bed_count.max(frame.beds.len());
        summary.largest_bed_channel_count =
            summary.largest_bed_channel_count.max(bed_channel_count);
        summary.largest_object_count = summary.largest_object_count.max(frame.objects.len());
        summary.largest_frame_bytes = summary.largest_frame_bytes.max(byte_count);
    }

    pub fn finish(self) -> StreamReport {
        let own_results: Vec<SetResult> = self
            .package
            .constraint_sets()
            .iter()
            .map(|&set| {
                let findings: Vec<Finding> = self
                    .tallies
                    .iter()
                    .filter(|tally| tally.check.set == set)
                    .filter_map(|tally| {
                        Some(Finding {
                            message: tally.check.message,
                            severity: tally.check.severity,
                            clause: tally.check.clause,
                            occurrence_count: tally.occurrence_count,
                            frame_count: tally.frame_count,
                            first_frame_index: tally.first_frame_index?,
                        })
                    })
                    .collect();
                let state = if self.parse_failure.is_some() {
                    ConformanceState::Invalid
                } else {
                    state_of_findings(&findings)
                };
                SetResult {
                    set,
                    state,
                    findings,
                }
            })
            .collect();
        let own_state = |set: ConstraintSet| {
            own_results
                .iter()
                .find(|result| result.set == set)
                .map(|result| result.state)
                .expect("a set builds only on sets of its own package")
        };
        let sets = own_results
            .iter()
            .map(|result| {
                let state = result
                    .set
                    .builds_on()
                    .iter()
                    .map(|&set| own_state(set))
                    .fold(result.state, ConformanceState::max);
                SetResult {
                    state,
                    ..result.clone()
                }
            })
            .collect();
        StreamReport {
            sets,
            summary: self.summary,
            parse_failure: self.parse_failure,
        }
    }
}

fn state_of_findings(findings: &[Finding]) -> ConformanceState {
    let has = |severity| findings.iter().any(|finding| finding.severity == severity);
    if has(Severity::Error) {
        ConformanceState::Invalid
    } else if has(Severity::Warning) {
        ConformanceState::ValidWithWarning
    } else {
        ConformanceState::Valid
    }
}

// checks every frame until the first that does not parse
pub fn check_stream<F: AsRef<[u8]>>(frames: &[F], package: PackageKind) -> StreamReport {
    let mut checker = StreamChecker::new(package);
    for frame in frames {
        if checker.push_frame(frame.as_ref()).is_err() {
            break;
        }
    }
    checker.finish()
}

struct Nested<'a, T> {
    element: &'a T,
    // 0 for a child of the IAFrame
    depth: usize,
}

fn nested<'a, T>(top_level: &'a [T], children: fn(&T) -> &[T]) -> Vec<Nested<'a, T>> {
    let mut pending: Vec<Nested<'a, T>> = top_level
        .iter()
        .rev()
        .map(|element| Nested { element, depth: 0 })
        .collect();
    let mut all = Vec::new();
    while let Some(current) = pending.pop() {
        pending.extend(
            children(current.element)
                .iter()
                .rev()
                .map(|element| Nested {
                    element,
                    depth: current.depth + 1,
                }),
        );
        all.push(current);
    }
    all
}

struct FrameUnderCheck<'a> {
    frame: &'a Frame,
    previous: Option<&'a Frame>,
    byte_count: usize,
    beds: Vec<Nested<'a, BedDefinition>>,
    objects: Vec<Nested<'a, ObjectDefinition>>,
}

impl<'a> FrameUnderCheck<'a> {
    fn new(frame: &'a Frame, previous: Option<&'a Frame>, byte_count: usize) -> Self {
        FrameUnderCheck {
            frame,
            previous,
            byte_count,
            beds: nested(&frame.beds, |bed| &bed.child_beds),
            objects: nested(&frame.objects, |object| &object.child_objects),
        }
    }

    fn all_beds(&self) -> impl Iterator<Item = &'a BedDefinition> + '_ {
        self.beds.iter().map(|nested| nested.element)
    }

    fn all_objects(&self) -> impl Iterator<Item = &'a ObjectDefinition> + '_ {
        self.objects.iter().map(|nested| nested.element)
    }

    fn channel_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.all_beds()
            .flat_map(|bed| bed.channels.iter().map(|channel| channel.channel_id))
    }

    fn remaps(&self) -> impl Iterator<Item = (&'a BedDefinition, &'a BedRemap)> + '_ {
        self.all_beds()
            .flat_map(|bed| bed.remaps.iter().map(move |remap| (bed, remap)))
    }

    fn coded_remap_destination_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.remaps().flat_map(|(_, remap)| {
            remap
                .sub_blocks
                .iter()
                .zip(&remap.remap_info_exists)
                .filter(|(_, info_exists)| **info_exists)
                .flat_map(|(destinations, _)| {
                    destinations
                        .iter()
                        .map(|destination| destination.channel_id)
                })
        })
    }

    fn coded_pans(&self) -> impl Iterator<Item = &'a ObjectPan> + '_ {
        self.all_objects().flat_map(|object| {
            object
                .pan_sub_blocks
                .iter()
                .zip(&object.pan_info_exists)
                .filter(|(_, info_exists)| **info_exists)
                .map(|(pan, _)| pan)
        })
    }

    fn bed_use_cases(&self) -> impl Iterator<Item = u8> + '_ {
        self.all_beds().filter_map(|bed| bed.use_case)
    }

    fn object_use_cases(&self) -> impl Iterator<Item = u8> + '_ {
        self.all_objects().filter_map(|object| object.use_case)
    }

    fn remap_use_cases(&self) -> impl Iterator<Item = u8> + '_ {
        self.remaps().map(|(_, remap)| remap.use_case)
    }

    fn referenced_audio_data_ids(&self) -> BTreeSet<u32> {
        let bed_ids = self
            .all_beds()
            .flat_map(|bed| bed.channels.iter().map(|channel| channel.audio_data_id));
        let object_ids = self.all_objects().map(|object| object.audio_data_id);
        bed_ids.chain(object_ids).collect()
    }
}

struct Check {
    set: ConstraintSet,
    message: &'static str,
    severity: Severity,
    // where the rule is written, the item number for RDD 57
    clause: &'static str,
    occurrences: fn(&FrameUnderCheck) -> usize,
}

#[rustfmt::skip]
const CHECKS: &[Check] = &[
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.3.6", message: "A referenced audio essence element (DLC or PCM) is missing", occurrences: missing_audio_data },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.3.5", message: "BedDefinition contains undefined or reserved channel ID", occurrences: cinema_reserved_channel_ids },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.7.1", message: "AudioDataDLC element not referenced by any BedDefinition or ObjectDefinition", occurrences: unreferenced_dlc },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.5.2", message: "ObjectDefinition contains undefined or reserved use case", occurrences: cinema_reserved_object_use_cases },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.7.1", message: "AudioDataPCM element not referenced by any BedDefinition or ObjectDefinition", occurrences: unreferenced_pcm },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.3.3", message: "BedDefinition contains undefined or reserved use case", occurrences: cinema_reserved_bed_use_cases },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.1.1", message: "IABFrame contains an undefined sub-element", occurrences: reserved_sub_elements },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 C.1", message: "IAFrame MaxRendered limit exceeded", occurrences: max_rendered_over_limit },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 429-18 7.2.1", message: "Sample rate not persistent", occurrences: sample_rate_changes },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 429-18 7.2.1", message: "Bit depth not persistent", occurrences: bit_depth_changes },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 429-18 7.2.1", message: "Frame rate not persistent", occurrences: frame_rate_changes },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.4.4", message: "BedRemap contains undefined or reserved channel ID", occurrences: cinema_reserved_remap_channel_ids },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.3.1", message: "Duplicate BedDefinition meta ID in frame", occurrences: duplicate_bed_meta_ids },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.3.5", message: "BedDefinition contains duplicate channel ID", occurrences: duplicate_channel_ids },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.3.2", message: "BedDefinition has multiple active sub-elements", occurrences: bed_children_sharing_a_use_case },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 C.1 3a", message: "BedDefinition hierarchy level exceeded", occurrences: beds_too_deep },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.4.2", message: "BedRemap source channel count not equal to parent bed", occurrences: remap_source_count_mismatches },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.3.1", message: "Duplicate ObjectDefinition meta ID in frame", occurrences: duplicate_object_meta_ids },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 C.1 3b", message: "ObjectDefinition hierarchy level exceeded", occurrences: objects_too_deep },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.7.1", message: "Duplicate AudioDataDLC duplicate ID", occurrences: duplicate_dlc_ids },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.2.2", message: "AudioDataDLC sample rate in conflict with frame sample rate", occurrences: dlc_sample_rate_conflicts },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 2098-2 10.7.1", message: "Duplicate AudioDataDPCM duplicate ID", occurrences: duplicate_pcm_ids },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.4.1", message: "BedRemap contains undefined or reserved use case", occurrences: cinema_reserved_remap_use_cases },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 Figure 2", message: "IABFrame contains multiple AuthoringToolInfo sub-elements", occurrences: multiple_authoring_tool_infos },
    Check { set: CinemaSt2098_2, severity: Error, clause: "ST 429-18 Table 1", message: "Unsupported frame rate", occurrences: non_integer_frame_rate },
    Check { set: CinemaSt2098_2, severity: Warning, clause: "ST 2098-2 10.2.5", message: "IAFrame MaxRendered not matching number of bed channels and objects", occurrences: max_rendered_mismatch },

    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.3.6", message: "A referenced audio essence element (DLC or PCM) is missing", occurrences: missing_audio_data },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.3.5", message: "BedDefinition contains undefined or reserved channel ID", occurrences: imf_reserved_channel_ids },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.7.1", message: "AudioDataDLC element not referenced by any BedDefinition or ObjectDefinition", occurrences: unreferenced_dlc },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.5.2", message: "ObjectDefinition contains undefined or reserved use case", occurrences: imf_reserved_object_use_cases },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.7.1", message: "AudioDataPCM element not referenced by any BedDefinition or ObjectDefinition", occurrences: unreferenced_pcm },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.3.3", message: "BedDefinition contains undefined or reserved use case", occurrences: imf_reserved_bed_use_cases },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.1.1", message: "IABFrame contains an undefined sub-element", occurrences: reserved_sub_elements },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 C.1", message: "IAFrame MaxRendered limit exceeded", occurrences: max_rendered_over_limit },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2067-201 5.6", message: "Sample rate not persistent", occurrences: sample_rate_changes },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2067-201 5.6", message: "Bit depth not persistent", occurrences: bit_depth_changes },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2067-201 5.6", message: "Frame rate not persistent", occurrences: frame_rate_changes },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.4.4", message: "BedRemap contains undefined or reserved channel ID", occurrences: imf_reserved_remap_channel_ids },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.3.1", message: "Duplicate BedDefinition meta ID in frame", occurrences: duplicate_bed_meta_ids },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.3.5", message: "BedDefinition contains duplicate channel ID", occurrences: duplicate_channel_ids },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.3.2", message: "BedDefinition has multiple active sub-elements", occurrences: bed_children_sharing_a_use_case },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 C.1 3a", message: "BedDefinition hierarchy level exceeded", occurrences: beds_too_deep },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.4.2", message: "BedRemap source channel count not equal to parent bed", occurrences: remap_source_count_mismatches },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.3.1", message: "Duplicate ObjectDefinition meta ID in frame", occurrences: duplicate_object_meta_ids },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 C.1 3b", message: "ObjectDefinition hierarchy level exceeded", occurrences: objects_too_deep },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.7.1", message: "Duplicate AudioDataDLC duplicate ID", occurrences: duplicate_dlc_ids },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.2.2", message: "AudioDataDLC sample rate in conflict with frame sample rate", occurrences: dlc_sample_rate_conflicts },
    Check { set: ImfSt2098_2, severity: Error, clause: "ST 2098-2 10.7.1", message: "Duplicate AudioDataDPCM duplicate ID", occurrences: duplicate_pcm_ids },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.4.1", message: "BedRemap contains undefined or reserved use case", occurrences: imf_reserved_remap_use_cases },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 Figure 2", message: "IABFrame contains multiple AuthoringToolInfo sub-elements", occurrences: multiple_authoring_tool_infos },
    Check { set: ImfSt2098_2, severity: Warning, clause: "ST 2098-2 10.2.5", message: "IAFrame MaxRendered not matching number of bed channels and objects", occurrences: max_rendered_mismatch },

    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.2", message: "AudioDataPCM is not allowed", occurrences: pcm_elements },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.4", message: "IABFrame size limit exceeded", occurrences: frame_over_size_limit },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.3", message: "Unsupported bit depth", occurrences: sixteen_bit_frame },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.6", message: "ObjectDefinition contains invalid or reserved use case", occurrences: track_file_forbidden_object_use_cases },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.6", message: "BedDefinition contains invalid or reserved use case", occurrences: track_file_forbidden_bed_use_cases },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.7", message: "BedDefinition contains invalid or reserved channel ID", occurrences: track_file_forbidden_channel_ids },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.5", message: "UserData is not allowed", occurrences: user_data_elements },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 2098-2 Table 14", message: "Undefined element found in IABFrame", occurrences: reserved_sub_elements },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.6", message: "BedRemap contains invalid or reserved use case", occurrences: track_file_forbidden_remap_use_cases },
    Check { set: CinemaSt429_18, severity: Error, clause: "ST 429-18 7.2.7", message: "BedRemap contains invalid or reserved destination channel ID", occurrences: track_file_forbidden_remap_channel_ids },

    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.1", message: "AudioDataDLC is not allowed", occurrences: dlc_elements },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition cannot have sub-element", occurrences: bed_sub_elements },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.3", message: "BedDefinition contains invalid or reserved use case", occurrences: conditional_bed_use_cases },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.2", message: "BedRemap is not an allowed sub-element", occurrences: remap_elements },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.3", message: "ObjectDefinition contains invalid or reserved use case", occurrences: conditional_object_use_cases },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "ObjectDefinition contains invalid sub-element type", occurrences: object_sub_elements_other_than_zones_19 },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.1", message: "Unsupported bit depth", occurrences: sixteen_bit_frame },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition channel count changes over program frames", occurrences: bed_channel_count_changes },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition meta ID changes over program frames", occurrences: bed_meta_id_changes },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition channel IDs change over program frames", occurrences: bed_channel_id_changes },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition conditional flag or use case changes over program frames", occurrences: bed_use_case_changes },
    Check { set: ImfSt2067_201, severity: Error, clause: "ST 2067-201 5.6.2.1", message: "BedDefinition count changes over program frames", occurrences: bed_count_changes },

    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B1", message: "BedDefinition cannot have sub-element", occurrences: bed_sub_elements },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O6", message: "ObjectDefinition contains invalid spread mode", occurrences: spread_modes_other_than_one_dimension },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 8.1.6", message: "Multiple BedDefinitions are not allowed", occurrences: multiple_beds },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B4", message: "BedDefinition contains invalid use case", occurrences: conditional_bed_use_cases },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B5", message: "BedRemap is not allowed", occurrences: remap_elements },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O9", message: "ObjectDefinition contains invalid use case", occurrences: conditional_object_use_cases },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B2", message: "BedDefinition contains invalid channel ID", occurrences: profile_1_channel_ids },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O1", message: "ObjectDefinition cannot have sub-element", occurrences: object_sub_elements },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 8.1.1", message: "Sample rate not valid", occurrences: ninety_six_kilohertz_frame },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B3", message: "BedDefinition contains invalid gain prefix", occurrences: channel_gains_other_than_unity },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O5", message: "ObjectDefinition contains invalid gain prefix", occurrences: object_gains_other_than_unity },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 IM2", message: "AuthoringToolInfo is not allowed", occurrences: authoring_tool_infos },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 B6", message: "BedDefinition channel DecorInfoExist must = 0", occurrences: channel_decorrelation_info },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O2", message: "ObjectDefinition SnapTolExist must = 0", occurrences: snap_tolerances },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O4", message: "ObjectDefinition zone gain is not a DbyCinema preset", occurrences: zone_gains_outside_table_4 },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O4", message: "ObjectDefinition contains invalid zone gain prefix", occurrences: coded_zone_gains },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O8", message: "ObjectDefinition contains invalid decorrelation prefix", occurrences: coded_object_decorrelation },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 O3", message: "Object zone19 is not allowed", occurrences: zone_19_definitions },
    Check { set: Rdd57Profile1, severity: Error, clause: "RDD 57 8.1.4", message: "ObjectDefinition count limit exceeded", occurrences: more_than_118_objects },
];

// ST 2098-2 Table 19 without the ITU-R BS.2051 channels that ST 429-18 7.2.7 keeps out of a DCP
fn is_cinema_channel_id(channel_id: u32) -> bool {
    channel_id <= 0x17
}

// ST 2098-2 Table 19 with the ITU-R BS.2051 channels
fn is_imf_channel_id(channel_id: u32) -> bool {
    is_cinema_channel_id(channel_id) || (0x80..=0x89).contains(&channel_id)
}

// ST 2098-2 Table 1 without the ITU-R BS.2051 layouts that ST 429-18 7.2.6 keeps out of a DCP
fn is_cinema_use_case(use_case: u8) -> bool {
    (0x01..=0x06).contains(&use_case) || use_case == ALWAYS_USE
}

fn is_imf_use_case(use_case: u8) -> bool {
    is_cinema_use_case(use_case) || (0x30..=0x32).contains(&use_case)
}

// ST 429-18 7.2.6
fn is_forbidden_in_track_file(use_case: u8) -> bool {
    (0x30..=0xFE).contains(&use_case)
}

// ST 429-18 7.2.7
const LARGEST_TRACK_FILE_CHANNEL_ID: u32 = 0x7F;

// RDD 57 B2, the 9.1OH channels
const PROFILE_1_CHANNEL_IDS: [u32; 10] = [0x0, 0x2, 0x4, 0x5, 0x7, 0x8, 0x9, 0xB, 0xC, 0xD];

// RDD 57 Table 4, sets 1a to 6 by column, true where ZoneGainPrefix is 0x1
const PROFILE_1_ZONE_GAIN_SETS: [[bool; 9]; 11] = [
    [true, true, true, true, true, false, false, true, true],
    [true, true, true, true, true, false, false, false, false],
    [true, true, true, false, false, true, true, true, true],
    [true, true, true, false, false, true, true, false, false],
    [false, true, false, false, false, true, true, true, true],
    [false, true, false, false, false, true, true, false, false],
    [true, true, true, false, false, false, false, true, true],
    [true, true, true, false, false, false, false, false, false],
    [false, false, false, true, true, true, true, true, true],
    [false, false, false, true, true, true, true, false, false],
    [true, true, true, true, true, true, true, false, false],
];

// RDD 57 8.1.4
const PROFILE_1_LARGEST_OBJECT_COUNT: usize = 118;

fn missing_audio_data(frame: &FrameUnderCheck) -> usize {
    let carried: HashSet<u32> = carried_audio_data_ids(frame.frame).collect();
    frame
        .referenced_audio_data_ids()
        .into_iter()
        // ST 2098-2 10.3.6: AudioDataID 0 carries no audio
        .filter(|id| *id != 0 && !carried.contains(id))
        .count()
}

fn carried_audio_data_ids(frame: &Frame) -> impl Iterator<Item = u32> + '_ {
    let dlc_ids = frame.dlc_audio.iter().map(|element| element.audio_data_id);
    let pcm_ids = frame.pcm_audio.iter().map(|element| element.audio_data_id);
    dlc_ids.chain(pcm_ids)
}

fn unreferenced_dlc(frame: &FrameUnderCheck) -> usize {
    let referenced = frame.referenced_audio_data_ids();
    let ids = frame
        .frame
        .dlc_audio
        .iter()
        .map(|element| element.audio_data_id);
    ids.filter(|id| !referenced.contains(id))
        .collect::<BTreeSet<u32>>()
        .len()
}

fn unreferenced_pcm(frame: &FrameUnderCheck) -> usize {
    let referenced = frame.referenced_audio_data_ids();
    let ids = frame
        .frame
        .pcm_audio
        .iter()
        .map(|element| element.audio_data_id);
    ids.filter(|id| !referenced.contains(id))
        .collect::<BTreeSet<u32>>()
        .len()
}

fn cinema_reserved_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .channel_ids()
        .filter(|id| !is_cinema_channel_id(*id))
        .count()
}

fn imf_reserved_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .channel_ids()
        .filter(|id| !is_imf_channel_id(*id))
        .count()
}

fn cinema_reserved_remap_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_remap_destination_ids()
        .filter(|id| !is_cinema_channel_id(*id))
        .count()
}

fn imf_reserved_remap_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_remap_destination_ids()
        .filter(|id| !is_imf_channel_id(*id))
        .count()
}

fn cinema_reserved_bed_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .bed_use_cases()
        .filter(|code| !is_cinema_use_case(*code))
        .count()
}

fn imf_reserved_bed_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .bed_use_cases()
        .filter(|code| !is_imf_use_case(*code))
        .count()
}

fn cinema_reserved_object_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .object_use_cases()
        .filter(|code| !is_cinema_use_case(*code))
        .count()
}

fn imf_reserved_object_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .object_use_cases()
        .filter(|code| !is_imf_use_case(*code))
        .count()
}

fn cinema_reserved_remap_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .remap_use_cases()
        .filter(|code| !is_cinema_use_case(*code))
        .count()
}

fn imf_reserved_remap_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .remap_use_cases()
        .filter(|code| !is_imf_use_case(*code))
        .count()
}

fn reserved_sub_elements(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.reserved_sub_element_count > 0)
}

fn max_rendered_over_limit(frame: &FrameUnderCheck) -> usize {
    let limit = match frame.frame.sample_rate {
        SampleRate::Hz48000 => 128,
        SampleRate::Hz96000 => 64,
    };
    usize::from(frame.frame.max_rendered > limit)
}

fn changed<T: PartialEq>(frame: &FrameUnderCheck, value: fn(&Frame) -> T) -> usize {
    frame.previous.map_or(0, |previous| {
        usize::from(value(previous) != value(frame.frame))
    })
}

fn sample_rate_changes(frame: &FrameUnderCheck) -> usize {
    changed(frame, |frame| frame.sample_rate)
}

fn bit_depth_changes(frame: &FrameUnderCheck) -> usize {
    changed(frame, |frame| frame.bit_depth)
}

fn frame_rate_changes(frame: &FrameUnderCheck) -> usize {
    changed(frame, |frame| frame.frame_rate)
}

fn repeats<T: Eq + std::hash::Hash + Copy>(values: impl Iterator<Item = T>) -> usize {
    let mut seen = HashSet::new();
    values.filter(|value| !seen.insert(*value)).count()
}

fn duplicate_bed_meta_ids(frame: &FrameUnderCheck) -> usize {
    repeats(frame.all_beds().map(|bed| bed.meta_id))
}

fn duplicate_object_meta_ids(frame: &FrameUnderCheck) -> usize {
    repeats(frame.all_objects().map(|object| object.meta_id))
}

fn duplicate_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .all_beds()
        .map(|bed| repeats(bed.channels.iter().map(|channel| channel.channel_id)))
        .sum()
}

fn duplicate_dlc_ids(frame: &FrameUnderCheck) -> usize {
    repeats(
        frame
            .frame
            .dlc_audio
            .iter()
            .map(|element| element.audio_data_id),
    )
}

// Frame keeps DLC and PCM elements apart, so a PCM element is the duplicate of a DLC one
fn duplicate_pcm_ids(frame: &FrameUnderCheck) -> usize {
    let all = repeats(carried_audio_data_ids(frame.frame));
    all - duplicate_dlc_ids(frame)
}

// children without a UseCase are left out, C.1 constrains UseCases only
fn bed_children_sharing_a_use_case(frame: &FrameUnderCheck) -> usize {
    frame
        .all_beds()
        .map(|bed| {
            let child_use_cases = bed.child_beds.iter().filter_map(|child| child.use_case);
            let remap_use_cases = bed.remaps.iter().map(|remap| remap.use_case);
            repeats(child_use_cases.chain(remap_use_cases))
        })
        .sum()
}

fn beds_too_deep(frame: &FrameUnderCheck) -> usize {
    frame.beds.iter().filter(|bed| bed.depth >= 2).count()
}

fn objects_too_deep(frame: &FrameUnderCheck) -> usize {
    frame
        .objects
        .iter()
        .filter(|object| object.depth >= 2)
        .count()
}

fn remap_source_count_mismatches(frame: &FrameUnderCheck) -> usize {
    frame
        .remaps()
        .filter(|(bed, remap)| remap.source_channel_count as usize != bed.channels.len())
        .count()
}

fn dlc_sample_rate_conflicts(frame: &FrameUnderCheck) -> usize {
    // ST 2098-2 Tables 15 and 29 share their codes
    let frame_code = match frame.frame.sample_rate {
        SampleRate::Hz48000 => 0,
        SampleRate::Hz96000 => 1,
    };
    frame
        .frame
        .dlc_audio
        .iter()
        // an AudioDataDLC with no bytes has no DLCSampleRate
        .filter_map(|element| element.coded.first())
        .filter(|first_byte| *first_byte >> 6 != frame_code)
        .count()
}

fn multiple_authoring_tool_infos(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.authoring_tool_uris.len() > 1)
}

fn non_integer_frame_rate(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.frame_rate == FrameRate::Fps24000Over1001)
}

fn max_rendered_mismatch(frame: &FrameUnderCheck) -> usize {
    usize::from(largest_rendered_count(frame) != frame.frame.max_rendered as usize)
}

// ST 2098-2 10.2.5: the most bed channels and objects that render in any target environment
fn largest_rendered_count(frame: &FrameUnderCheck) -> usize {
    let mut targets = BTreeSet::from([ALWAYS_USE]);
    targets.extend(frame.bed_use_cases());
    targets.extend(frame.object_use_cases());
    targets.extend(frame.remap_use_cases());
    targets
        .into_iter()
        .map(|target| {
            let bed_channels: usize = frame
                .frame
                .beds
                .iter()
                .filter(|bed| is_activated(bed.use_case, target))
                .map(|bed| rendered_bed_channel_count(bed, target))
                .sum();
            let objects = frame
                .frame
                .objects
                .iter()
                .filter(|object| is_activated(object.use_case, target))
                .count();
            bed_channels + objects
        })
        .fold(0, usize::max)
}

// ST 2098-2 10.3.2 renders the deepest activated bed, and 10.2.5 counts a remap at its wider side
fn rendered_bed_channel_count(bed: &BedDefinition, target: u8) -> usize {
    let activated_child_count = bed
        .child_beds
        .iter()
        .filter(|child| is_activated(child.use_case, target))
        .map(|child| rendered_bed_channel_count(child, target))
        .max();
    if let Some(count) = activated_child_count {
        return count;
    }
    let activated_remap_width = bed
        .remaps
        .iter()
        .filter(|remap| is_activated(Some(remap.use_case), target))
        .map(|remap| {
            remap
                .source_channel_count
                .max(remap.destination_channel_count) as usize
        })
        .max();
    match activated_remap_width {
        Some(width) => width,
        None => bed.channels.len(),
    }
}

fn pcm_elements(frame: &FrameUnderCheck) -> usize {
    frame.frame.pcm_audio.len()
}

fn dlc_elements(frame: &FrameUnderCheck) -> usize {
    frame.frame.dlc_audio.len()
}

fn frame_over_size_limit(frame: &FrameUnderCheck) -> usize {
    let frames_per_second = match frame.frame.frame_rate {
        FrameRate::Fps24 => 24,
        FrameRate::Fps25 => 25,
        FrameRate::Fps30 => 30,
        FrameRate::Fps48 => 48,
        FrameRate::Fps50 => 50,
        FrameRate::Fps60 => 60,
        FrameRate::Fps96 => 96,
        FrameRate::Fps100 => 100,
        FrameRate::Fps120 => 120,
        // ST 429-18 Table 1 sets no size at this rate
        FrameRate::Fps24000Over1001 => return 0,
    };
    LARGEST_FRAME_BYTES
        .iter()
        .filter(|(rate, largest)| *rate == frames_per_second && frame.byte_count > *largest)
        .count()
}

fn sixteen_bit_frame(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.bit_depth == BitDepth::Bits16)
}

fn track_file_forbidden_bed_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .bed_use_cases()
        .filter(|code| is_forbidden_in_track_file(*code))
        .count()
}

fn track_file_forbidden_object_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .object_use_cases()
        .filter(|code| is_forbidden_in_track_file(*code))
        .count()
}

fn track_file_forbidden_remap_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .remap_use_cases()
        .filter(|code| is_forbidden_in_track_file(*code))
        .count()
}

fn track_file_forbidden_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .channel_ids()
        .filter(|id| *id > LARGEST_TRACK_FILE_CHANNEL_ID)
        .count()
}

fn track_file_forbidden_remap_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_remap_destination_ids()
        .filter(|id| *id > LARGEST_TRACK_FILE_CHANNEL_ID)
        .count()
}

fn user_data_elements(frame: &FrameUnderCheck) -> usize {
    frame.frame.user_data.len()
}

fn bed_sub_elements(frame: &FrameUnderCheck) -> usize {
    frame
        .all_beds()
        .map(|bed| bed.child_beds.len() + bed.remaps.len() + bed.ignored_sub_element_count)
        .sum()
}

fn object_sub_elements(frame: &FrameUnderCheck) -> usize {
    frame
        .all_objects()
        .map(|object| {
            object.child_objects.len()
                + object.zone_definitions_19.len()
                + object.ignored_sub_element_count
        })
        .sum()
}

// ST 2067-201:2021 forbids an ObjectZoneDefinition19 child too, DTS's validator lets it through
fn object_sub_elements_other_than_zones_19(frame: &FrameUnderCheck) -> usize {
    frame
        .all_objects()
        .map(|object| object.child_objects.len() + object.ignored_sub_element_count)
        .sum()
}

fn remap_elements(frame: &FrameUnderCheck) -> usize {
    frame.remaps().count()
}

fn conditional_bed_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .bed_use_cases()
        .filter(|code| *code != ALWAYS_USE)
        .count()
}

fn conditional_object_use_cases(frame: &FrameUnderCheck) -> usize {
    frame
        .object_use_cases()
        .filter(|code| *code != ALWAYS_USE)
        .count()
}

fn all_bed_meta_ids(frame: &Frame) -> BTreeSet<u32> {
    nested(&frame.beds, |bed| &bed.child_beds)
        .iter()
        .map(|bed| bed.element.meta_id)
        .collect()
}

fn bed_count_changes(frame: &FrameUnderCheck) -> usize {
    changed(frame, |frame| frame.beds.len())
}

// a change of bed count is its own finding
fn bed_meta_id_changes(frame: &FrameUnderCheck) -> usize {
    let Some(previous) = frame.previous else {
        return 0;
    };
    let same_count = previous.beds.len() == frame.frame.beds.len();
    usize::from(same_count && all_bed_meta_ids(previous) != all_bed_meta_ids(frame.frame))
}

// each IAFrame child bed with the bed of the previous IAFrame that has its MetaID
fn matched_beds<'a>(
    frame: &FrameUnderCheck<'a>,
) -> impl Iterator<Item = (&'a BedDefinition, &'a BedDefinition)> + 'a {
    let previous_beds = frame.previous.map_or(&[][..], |previous| &previous.beds);
    frame.frame.beds.iter().filter_map(move |bed| {
        previous_beds
            .iter()
            .find(|previous| previous.meta_id == bed.meta_id)
            .map(|previous| (previous, bed))
    })
}

fn channel_id_set(bed: &BedDefinition) -> BTreeSet<u32> {
    bed.channels
        .iter()
        .map(|channel| channel.channel_id)
        .collect()
}

fn bed_use_case_changes(frame: &FrameUnderCheck) -> usize {
    matched_beds(frame)
        .filter(|(previous, bed)| previous.use_case != bed.use_case)
        .count()
}

fn bed_channel_count_changes(frame: &FrameUnderCheck) -> usize {
    matched_beds(frame)
        .filter(|(previous, bed)| previous.channels.len() != bed.channels.len())
        .count()
}

// a change of channel count is its own finding
fn bed_channel_id_changes(frame: &FrameUnderCheck) -> usize {
    matched_beds(frame)
        .filter(|(previous, bed)| {
            previous.channels.len() == bed.channels.len()
                && channel_id_set(previous) != channel_id_set(bed)
        })
        .count()
}

fn spread_modes_other_than_one_dimension(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter(|pan| !matches!(pan.spread, ObjectSpread::OneDimension(_)))
        .count()
}

fn multiple_beds(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.beds.len() > 1)
}

fn profile_1_channel_ids(frame: &FrameUnderCheck) -> usize {
    frame
        .channel_ids()
        .filter(|id| !PROFILE_1_CHANNEL_IDS.contains(id))
        .count()
}

fn ninety_six_kilohertz_frame(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.sample_rate == SampleRate::Hz96000)
}

fn channel_gains_other_than_unity(frame: &FrameUnderCheck) -> usize {
    frame
        .all_beds()
        .flat_map(|bed| &bed.channels)
        .filter(|channel| channel.gain != Gain::Unity)
        .count()
}

fn object_gains_other_than_unity(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter(|pan| pan.gain != Gain::Unity)
        .count()
}

fn authoring_tool_infos(frame: &FrameUnderCheck) -> usize {
    frame.frame.authoring_tool_uris.len()
}

fn channel_decorrelation_info(frame: &FrameUnderCheck) -> usize {
    frame
        .all_beds()
        .flat_map(|bed| &bed.channels)
        .filter(|channel| channel.decorrelation.is_some())
        .count()
}

fn snap_tolerances(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter(|pan| matches!(pan.snap, ObjectSnap::Tolerance(_)))
        .count()
}

// None when a zone gain is coded, which the invalid zone gain prefix check reports
fn zone_gain_prefixes(zone_gains: &[ZoneGain; 9]) -> Option<[bool; 9]> {
    let mut prefixes = [false; 9];
    for (prefix, zone_gain) in prefixes.iter_mut().zip(zone_gains) {
        *prefix = match zone_gain {
            ZoneGain::Silent => false,
            ZoneGain::Full => true,
            ZoneGain::Coded(_) | ZoneGain::Reserved(_) => return None,
        };
    }
    Some(prefixes)
}

fn zone_gains_outside_table_4(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter_map(|pan| pan.zone_gains.as_ref())
        .filter_map(zone_gain_prefixes)
        .filter(|prefixes| !PROFILE_1_ZONE_GAIN_SETS.contains(prefixes))
        .count()
}

fn coded_zone_gains(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter_map(|pan| pan.zone_gains.as_ref())
        .flatten()
        .filter(|zone_gain| matches!(zone_gain, ZoneGain::Coded(_) | ZoneGain::Reserved(_)))
        .count()
}

fn coded_object_decorrelation(frame: &FrameUnderCheck) -> usize {
    frame
        .coded_pans()
        .filter(|pan| {
            matches!(
                pan.decorrelation,
                Decorrelation::Coded(_) | Decorrelation::Reserved(_)
            )
        })
        .count()
}

fn zone_19_definitions(frame: &FrameUnderCheck) -> usize {
    frame
        .all_objects()
        .map(|object| object.zone_definitions_19.len())
        .sum()
}

fn more_than_118_objects(frame: &FrameUnderCheck) -> usize {
    usize::from(frame.frame.objects.len() > PROFILE_1_LARGEST_OBJECT_COUNT)
}
