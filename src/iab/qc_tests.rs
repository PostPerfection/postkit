use super::parse::split_frames;
use super::qc::{
    CheckingStopped, ConformanceState, ConstraintSet, PackageKind, SetResult, Severity,
    StreamChecker, StreamReport, check_stream,
};
use super::tests::{
    VALIDATION_STREAM_COUNT, committed_stream_numbers, fixture_directory, read_verdict,
    read_zipped_stream, stream_name, validation_stream_frames, wrap_atmos_track,
};
use super::{AtmosReader, BitDepth, FrameRate, SampleRate, parse_frame};
use crate::mxf_wrap::{MxfEncryption, MxfStandard};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// the validator stops at frame 1 of each, it counts frames from 1
const PARSE_FAILURE_FRAME_INDEXES: [(u32, u32); 4] = [(49, 0), (51, 0), (53, 0), (62, 0)];

// DbyCinema messages whose rule RDD 57 does not state
const DOLBY_CINEMA_MESSAGES_OUTSIDE_PROFILE_1: [&str; 4] = [
    "BedDefinition count must be persistent",
    "BedDefinition channel list must be persistent",
    "BedDefinition meta ID must be persistent",
    "ObjectDefinition meta ID must be sequential",
];

// the validator checks each pan sub block in its cinema pass and again in its IMF pass, and each
// pass reports every set
const MESSAGES_THE_VALIDATOR_COUNTS_TWICE: [(ConstraintSet, &str); 9] = [
    (
        ConstraintSet::CinemaSt2098_2,
        "BedRemap contains undefined or reserved channel ID",
    ),
    (
        ConstraintSet::ImfSt2098_2,
        "BedRemap contains undefined or reserved channel ID",
    ),
    (
        ConstraintSet::CinemaSt429_18,
        "BedRemap contains invalid or reserved destination channel ID",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition contains invalid spread mode",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition contains invalid gain prefix",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition SnapTolExist must = 0",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition zone gain is not a DbyCinema preset",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition contains invalid zone gain prefix",
    ),
    (
        ConstraintSet::Rdd57Profile1,
        "ObjectDefinition contains invalid decorrelation prefix",
    ),
];

// the validator ends all DbyCinema messages but its zone gain preset one with it
const DOLBY_CINEMA_SUFFIX: &str = " for DbyCinema";

fn reference_set_name(set: ConstraintSet) -> &'static str {
    match set {
        ConstraintSet::CinemaSt2098_2 => "Cinema 2098-2-2018 constraint",
        ConstraintSet::CinemaSt429_18 => "Cinema ST429-18-2019 constraint",
        ConstraintSet::Rdd57Profile1 => "DbyCinema constraint",
        ConstraintSet::ImfSt2098_2 => "IMF ST2098-2-2019 constraint",
        ConstraintSet::ImfSt2067_201 => "IMF ST2067-201-2019 constraint",
    }
}

#[derive(Debug, PartialEq)]
struct SetVerdict {
    state: ConformanceState,
    errors: BTreeMap<String, u64>,
    warnings: BTreeMap<String, u64>,
}

fn state_named(name: &str) -> ConformanceState {
    match name {
        "Valid" => ConformanceState::Valid,
        "ValidWithWarning" => ConformanceState::ValidWithWarning,
        "Invalid" => ConformanceState::Invalid,
        other => panic!("unknown ValidationState {other}"),
    }
}

fn entry_for<'a>(
    verdict: &'a serde_json::Value,
    list: &str,
    set_name: &str,
) -> &'a serde_json::Value {
    verdict[list]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["Constraint"] == set_name)
        .unwrap_or_else(|| panic!("{list} has no {set_name}"))
}

// Errors and Warnings are 0 when empty and an object of message to count otherwise
fn occurrences(value: &serde_json::Value) -> BTreeMap<String, u64> {
    match value.as_object() {
        None => {
            assert_eq!(value.as_u64(), Some(0));
            BTreeMap::new()
        }
        Some(messages) => messages
            .iter()
            .map(|(message, count)| (message.clone(), count.as_u64().unwrap()))
            .collect(),
    }
}

fn reference_set_verdict(verdict: &serde_json::Value, set: ConstraintSet) -> SetVerdict {
    let name = reference_set_name(set);
    let issues = entry_for(verdict, "IssueOccurrenceSummary", name);
    SetVerdict {
        state: state_named(
            entry_for(verdict, "ValidationResultSummary", name)["ValidationState"]
                .as_str()
                .unwrap(),
        ),
        errors: occurrences(&issues["Errors"]),
        warnings: occurrences(&issues["Warnings"]),
    }
}

fn state_of_messages(
    errors: &BTreeMap<String, u64>,
    warnings: &BTreeMap<String, u64>,
) -> ConformanceState {
    if !errors.is_empty() {
        ConformanceState::Invalid
    } else if !warnings.is_empty() {
        ConformanceState::ValidWithWarning
    } else {
        ConformanceState::Valid
    }
}

// DbyCinema without the messages RDD 57 has no rule for, on top of the two cinema sets
fn expected_profile_1_verdict(verdict: &serde_json::Value) -> SetVerdict {
    let dolby_cinema = reference_set_verdict(verdict, ConstraintSet::Rdd57Profile1);
    let profile_1_messages = |messages: BTreeMap<String, u64>| -> BTreeMap<String, u64> {
        messages
            .into_iter()
            .map(|(message, count)| {
                let message = match message.strip_suffix(DOLBY_CINEMA_SUFFIX) {
                    Some(stem) => stem.to_string(),
                    None => message,
                };
                (message, count)
            })
            .filter(|(message, _)| {
                !DOLBY_CINEMA_MESSAGES_OUTSIDE_PROFILE_1.contains(&message.as_str())
            })
            .collect()
    };
    let errors = profile_1_messages(dolby_cinema.errors);
    let warnings = profile_1_messages(dolby_cinema.warnings);
    let state = [ConstraintSet::CinemaSt2098_2, ConstraintSet::CinemaSt429_18]
        .into_iter()
        .map(|set| reference_set_verdict(verdict, set).state)
        .fold(state_of_messages(&errors, &warnings), ConformanceState::max);
    SetVerdict {
        state,
        errors,
        warnings,
    }
}

// an odd count stays as it is and shows as a mismatch
fn counted_once(set: ConstraintSet, verdict: SetVerdict) -> SetVerdict {
    let halve = |messages: BTreeMap<String, u64>| -> BTreeMap<String, u64> {
        messages
            .into_iter()
            .map(|(message, count)| {
                let counted_twice = MESSAGES_THE_VALIDATOR_COUNTS_TWICE
                    .contains(&(set, message.as_str()))
                    && count % 2 == 0;
                let count = if counted_twice { count / 2 } else { count };
                (message, count)
            })
            .collect()
    };
    SetVerdict {
        state: verdict.state,
        errors: halve(verdict.errors),
        warnings: halve(verdict.warnings),
    }
}

fn our_set_verdict(result: &SetResult) -> SetVerdict {
    let with_severity = |severity| {
        result
            .findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .map(|finding| (finding.message.to_string(), finding.occurrence_count))
            .collect()
    };
    SetVerdict {
        state: result.state,
        errors: with_severity(Severity::Error),
        warnings: with_severity(Severity::Warning),
    }
}

fn parse_failure_mismatches(name: &str, number: u32, report: &StreamReport) -> Vec<String> {
    let expected = PARSE_FAILURE_FRAME_INDEXES
        .iter()
        .find(|(stream, _)| *stream == number)
        .map(|(_, frame_index)| *frame_index);
    let ours = report
        .parse_failure
        .as_ref()
        .map(|failure| failure.frame_index);
    let mut mismatches = Vec::new();
    if ours != expected {
        mismatches.push(format!(
            "{name}: our parse failure is at frame {ours:?}, the expectation is {expected:?}"
        ));
    }
    if report
        .sets
        .iter()
        .any(|result| result.state != ConformanceState::Invalid)
    {
        mismatches.push(format!("{name}: a set stays valid after the parse failure"));
    }
    mismatches
}

fn qc_mismatches(directory: &Path, number: u32) -> Vec<String> {
    let name = stream_name(number);
    let stream = read_zipped_stream(&directory.join(format!("{name}.iab.zip")));
    let frames = split_frames(&stream).unwrap_or_else(|error| panic!("{name}: {error}"));
    let verdict = read_verdict(&directory.join(format!("Stream{number}_cA.json")));
    let mut mismatches = Vec::new();
    for package in [PackageKind::Dcp, PackageKind::Imp] {
        let report = check_stream(&frames, package);
        if verdict.get("ParserState").is_some() {
            mismatches.extend(parse_failure_mismatches(&name, number, &report));
            continue;
        }
        if let Some(failure) = &report.parse_failure {
            mismatches.push(format!(
                "{name}: frame {} does not parse: {}",
                failure.frame_index, failure.error
            ));
        }
        for result in &report.sets {
            let reference = match result.set {
                ConstraintSet::Rdd57Profile1 => expected_profile_1_verdict(&verdict),
                set => reference_set_verdict(&verdict, set),
            };
            let expected = counted_once(result.set, reference);
            let ours = our_set_verdict(result);
            if ours != expected {
                mismatches.push(format!(
                    "{name} {:?}: ours {ours:?}, reference {expected:?}",
                    result.set
                ));
            }
        }
    }
    mismatches
}

#[test]
fn committed_streams_match_the_validator_verdicts() {
    let directory = fixture_directory();
    let numbers = committed_stream_numbers();
    assert!(!numbers.is_empty());
    let mismatches: Vec<String> = numbers
        .into_iter()
        .flat_map(|number| qc_mismatches(&directory, number))
        .collect();
    assert_eq!(mismatches, Vec::<String>::new());
}

#[test]
fn every_validation_stream_matches_the_validator_verdicts() {
    let directory = std::env::var_os("POSTKIT_IAB_VALIDATION_STREAMS").expect(
        "POSTKIT_IAB_VALIDATION_STREAMS names the directory scripts/fetch-iab-validation-streams.sh fills",
    );
    let directory = PathBuf::from(directory);
    let mismatches: Vec<String> = (1..=VALIDATION_STREAM_COUNT)
        .flat_map(|number| qc_mismatches(&directory, number))
        .collect();
    assert_eq!(mismatches, Vec::<String>::new());
}

#[test]
fn the_checker_fed_from_an_atmos_track_reports_what_the_raw_stream_does() {
    const CONTENT_KEY: [u8; 16] = [0x5a; 16];
    let encryption = MxfEncryption {
        content_key: CONTENT_KEY,
        key_id: [0x17; 16],
    };
    // stream 16 breaks Profile 1 and ST 2067-201
    let frames = validation_stream_frames(16);
    let wraps = [
        (MxfStandard::AsDcp, None, PackageKind::Dcp),
        (MxfStandard::AsDcp, Some(encryption), PackageKind::Dcp),
        (MxfStandard::As02, None, PackageKind::Imp),
    ];
    for (standard, encryption, package) in wraps {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("atmos.mxf");
        let content_key = encryption.as_ref().map(|encryption| encryption.content_key);
        wrap_atmos_track(&frames, &path, standard, encryption);
        let mut reader = AtmosReader::open(&path, content_key.as_ref()).unwrap();
        let mut checker = StreamChecker::new(package);
        for frame_number in 0..reader.frame_count() {
            checker
                .push_frame(reader.read_frame(frame_number).unwrap())
                .unwrap();
        }
        let from_track = checker.finish();
        let from_stream = check_stream(&frames, package);
        assert!(
            from_stream
                .sets
                .iter()
                .any(|result| !result.findings.is_empty())
        );
        assert_eq!(from_track, from_stream, "{standard:?} {package:?}");
    }
}

#[test]
fn a_frame_that_does_not_parse_stops_checking() {
    let frame = validation_stream_frames(1).swap_remove(0);
    let truncated = &frame[..frame.len() / 2];
    let parse_error = parse_frame(truncated).unwrap_err();
    for package in [PackageKind::Dcp, PackageKind::Imp] {
        let mut checker = StreamChecker::new(package);
        checker.push_frame(&frame).unwrap();
        let stopped = Err(CheckingStopped { frame_index: 1 });
        assert_eq!(checker.push_frame(truncated), stopped);
        assert_eq!(checker.push_frame(&frame), stopped);
        let report = checker.finish();
        let failure = report.parse_failure.unwrap();
        assert_eq!(failure.frame_index, 1);
        assert_eq!(failure.error, parse_error);
        assert_eq!(report.summary.frame_count, 1);
        assert!(!report.sets.is_empty());
        assert!(
            report
                .sets
                .iter()
                .all(|result| result.state == ConformanceState::Invalid)
        );
    }
}

#[test]
fn the_summary_of_stream_1_matches_its_reference() {
    let reference =
        read_verdict(&fixture_directory().join("Stream1_cA.json"))["BitStreamSummary"].clone();
    assert_eq!(reference["SampleRate"], "48kHz");
    assert_eq!(reference["BitDepth"], "24-bit");
    assert_eq!(reference["FrameRate"], "24 fps");
    let frames = validation_stream_frames(1);
    let summary = check_stream(&frames, PackageKind::Dcp).summary;
    let format = summary.first_frame_format.unwrap();
    assert_eq!(format.sample_rate, SampleRate::Hz48000);
    assert_eq!(format.bit_depth, BitDepth::Bits24);
    assert_eq!(format.frame_rate, FrameRate::Fps24);
    assert_eq!(summary.frame_count.to_string(), reference["FrameCount"]);
    assert_eq!(
        summary.largest_max_rendered.to_string(),
        reference["MaxRendered"]
    );
    // every frame carries one 9.1OH bed and five objects, the 15 MaxRendered counts
    assert_eq!(summary.largest_bed_count, 1);
    assert_eq!(summary.largest_bed_channel_count, 10);
    assert_eq!(summary.largest_object_count, 5);
    let largest_frame = frames.iter().map(Vec::len).max().unwrap();
    assert_eq!(summary.largest_frame_bytes, largest_frame);
}
