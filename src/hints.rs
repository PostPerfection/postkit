//! Advisory findings gathered before the encode.
//!
//! A hint is not a refusal: everything here builds and packages. It says the
//! result is likely to be wrong for the audience, so the front ends print it and
//! let the build go on. The rules here hold for any package format. A wizard
//! adds its own rules over the same types, and reads a cue rule with
//! [`first_offence`].

/// One advisory finding, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub text: String,
}

/// Sound peaking above this clips on some playback chains.
const LOUD_TRUE_PEAK_DBTP: f64 = -3.0;

/// A first subtitle earlier than this is easy to miss.
const FIRST_CUE_SECONDS: f64 = 4.0;
/// A cue shorter than this is hard to read.
const SHORTEST_CUE_FRAMES: f64 = 15.0;
/// Two cues closer than this read as one flicker.
const SMALLEST_CUE_GAP_FRAMES: f64 = 2.0;
pub const MOST_CUE_LINES: usize = 3;
/// Line lengths, in characters: the length to aim for, and the one past which
/// the text will not fit at all.
const ADVISED_LINE_CHARACTERS: usize = 52;
const MOST_LINE_CHARACTERS: usize = 79;

const MILLISECONDS_PER_SECOND: f64 = 1000.0;
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct AudioLevel {
    pub file: String,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCues {
    pub file: String,
    pub cues: Vec<HintCue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HintCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<String>,
}

pub fn audio_level_hint(levels: &[AudioLevel]) -> Option<Hint> {
    let loud = levels
        .iter()
        .find(|level| level.true_peak_dbtp > LOUD_TRUE_PEAK_DBTP)?;
    Some(Hint {
        text: format!(
            "The audio level is very high ({:.1} dBTP in {}). Reduce the gain.",
            loud.true_peak_dbtp, loud.file
        ),
    })
}

pub fn audio_language_hint(has_audio: bool, language: Option<&str>) -> Option<Hint> {
    let named = language
        .map(str::trim)
        .is_some_and(|language| !language.is_empty());
    (has_audio && !named).then(|| Hint {
        text: "The sound has no language set. Set one unless it has no spoken parts.".to_string(),
    })
}

/// A cue with what the rules need around it.
pub struct CueInContext<'a> {
    pub cue: &'a HintCue,
    pub previous_end_ms: Option<u64>,
    pub is_first: bool,
    pub fps: f64,
}

/// How a rule words itself, given the file it found the fault in and the time of
/// the first cue that showed it.
pub type SayHint = fn(&str, &str) -> String;

/// One advisory rule over a cue: what counts as an offence, and what to say
/// about the first cue that offends.
pub struct CueRule {
    pub offends: fn(&CueInContext) -> bool,
    pub say: SayHint,
}

const SUBTITLE_RULES: [CueRule; 4] = [
    CueRule {
        offends: |context| {
            context.is_first && context.cue.start_ms < seconds_to_milliseconds(FIRST_CUE_SECONDS)
        },
        say: |file, at| {
            format!(
                "The first subtitle in {file} starts at {at}. Put it at least {FIRST_CUE_SECONDS:.0} seconds in, or it is easy to miss."
            )
        },
    },
    CueRule {
        offends: |context| {
            context.cue.end_ms.saturating_sub(context.cue.start_ms)
                < frames_to_milliseconds(SHORTEST_CUE_FRAMES, context.fps)
        },
        say: |file, at| {
            format!(
                "A subtitle in {file} at {at} lasts less than {SHORTEST_CUE_FRAMES:.0} frames. Make every subtitle at least that long."
            )
        },
    },
    CueRule {
        offends: |context| match context.previous_end_ms {
            Some(previous_end_ms) => {
                context.cue.start_ms
                    < previous_end_ms + frames_to_milliseconds(SMALLEST_CUE_GAP_FRAMES, context.fps)
            }
            None => false,
        },
        say: |file, at| {
            format!(
                "A subtitle in {file} at {at} starts less than {SMALLEST_CUE_GAP_FRAMES:.0} frames after the one before it ends. Leave at least that gap."
            )
        },
    },
    CueRule {
        offends: |context| context.cue.lines.len() > MOST_CUE_LINES,
        say: |file, at| {
            format!(
                "A subtitle in {file} at {at} has more than {MOST_CUE_LINES} lines. Use no more than {MOST_CUE_LINES}."
            )
        },
    },
];

/// Every hint the subtitles the audience reads raise.
pub fn subtitle_hints(subtitles: &[SubtitleCues], fps: f64) -> Vec<Hint> {
    let mut hints: Vec<Hint> = SUBTITLE_RULES
        .iter()
        .filter_map(|rule| first_offence(subtitles, fps, rule))
        .collect();
    hints.extend(line_length_hint(subtitles));
    hints
}

/// The first cue in reading order that breaks a rule, said once for the whole
/// job rather than once per cue.
pub fn first_offence(files: &[SubtitleCues], fps: f64, rule: &CueRule) -> Option<Hint> {
    for subtitle in files {
        let mut previous_end_ms = None;
        for (index, cue) in subtitle.cues.iter().enumerate() {
            let context = CueInContext {
                cue,
                previous_end_ms,
                is_first: index == 0,
                fps,
            };
            if (rule.offends)(&context) {
                return Some(Hint {
                    text: (rule.say)(&subtitle.file, &format_cue_time(cue.start_ms)),
                });
            }
            previous_end_ms = Some(cue.end_ms);
        }
    }
    None
}

/// A line past the hard limit is the same fault as one past the advised length,
/// said more strongly, so only the stronger hint is raised.
fn line_length_hint(subtitles: &[SubtitleCues]) -> Option<Hint> {
    let limits: [(usize, SayHint); 2] = [
        (MOST_LINE_CHARACTERS, |file, at| {
            format!(
                "A subtitle line in {file} at {at} is longer than {MOST_LINE_CHARACTERS} characters. Cut it to {MOST_LINE_CHARACTERS} at most."
            )
        }),
        (ADVISED_LINE_CHARACTERS, |file, at| {
            format!(
                "A subtitle line in {file} at {at} is longer than {ADVISED_LINE_CHARACTERS} characters. Keep lines to {ADVISED_LINE_CHARACTERS} where you can."
            )
        }),
    ];
    for (characters, say) in limits {
        for subtitle in subtitles {
            let offender = subtitle.cues.iter().find(|cue| {
                cue.lines
                    .iter()
                    .any(|line| line.chars().count() > characters)
            });
            if let Some(cue) = offender {
                return Some(Hint {
                    text: say(&subtitle.file, &format_cue_time(cue.start_ms)),
                });
            }
        }
    }
    None
}

const fn seconds_to_milliseconds(seconds: f64) -> u64 {
    (seconds * MILLISECONDS_PER_SECOND) as u64
}

fn frames_to_milliseconds(frames: f64, fps: f64) -> u64 {
    (frames / fps.max(1.0) * MILLISECONDS_PER_SECOND).round() as u64
}

fn format_cue_time(milliseconds: u64) -> String {
    let total_seconds = milliseconds / MILLISECONDS_PER_SECOND as u64;
    let hours = total_seconds / (SECONDS_PER_MINUTE * MINUTES_PER_HOUR);
    let minutes = total_seconds / SECONDS_PER_MINUTE % MINUTES_PER_HOUR;
    let seconds = total_seconds % SECONDS_PER_MINUTE;
    format!(
        "{hours:02}:{minutes:02}:{seconds:02}.{:03}",
        milliseconds % MILLISECONDS_PER_SECOND as u64
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 24.0;

    fn cue(start_ms: u64, end_ms: u64, lines: &[&str]) -> HintCue {
        HintCue {
            start_ms,
            end_ms,
            lines: lines.iter().map(|line| line.to_string()).collect(),
        }
    }

    fn one_file(cues: Vec<HintCue>) -> Vec<SubtitleCues> {
        vec![SubtitleCues {
            file: "subs.srt".to_string(),
            cues,
        }]
    }

    fn texts(cues: Vec<HintCue>) -> Vec<String> {
        subtitle_hints(&one_file(cues), FPS)
            .into_iter()
            .map(|hint| hint.text)
            .collect()
    }

    fn mentions(cues: Vec<HintCue>, needle: &str) -> bool {
        texts(cues).iter().any(|text| text.contains(needle))
    }

    #[test]
    fn a_loud_track_is_named_with_its_peak_and_a_quiet_one_is_not() {
        let loud = [AudioLevel {
            file: "sound.wav".to_string(),
            true_peak_dbtp: -0.4,
        }];
        let hint = audio_level_hint(&loud).expect("a peak above the line is hinted");
        assert!(hint.text.contains("-0.4 dBTP in sound.wav"), "{hint:?}");

        let quiet = [AudioLevel {
            file: "sound.wav".to_string(),
            true_peak_dbtp: LOUD_TRUE_PEAK_DBTP,
        }];
        assert_eq!(audio_level_hint(&quiet), None);
        assert_eq!(audio_level_hint(&[]), None);
    }

    #[test]
    fn sound_without_a_language_is_hinted_and_sound_with_one_is_not() {
        assert!(audio_language_hint(true, None).is_some());
        assert!(audio_language_hint(true, Some("  ")).is_some());
        assert_eq!(audio_language_hint(true, Some("de-DE")), None);
        assert_eq!(audio_language_hint(false, None), None);
    }

    #[test]
    fn a_first_cue_before_four_seconds_is_hinted_and_one_at_four_is_not() {
        let early = vec![cue(3_999, 10_000, &["hello"])];
        assert!(
            mentions(early.clone(), "starts at 00:00:03.999"),
            "{:?}",
            texts(early)
        );

        let late = vec![cue(4_000, 10_000, &["hello"])];
        assert!(
            !mentions(late.clone(), "at least 4 seconds"),
            "{:?}",
            texts(late)
        );
    }

    /// 15 frames at 24 fps is 625 ms.
    #[test]
    fn a_cue_shorter_than_fifteen_frames_is_hinted_and_one_exactly_that_long_is_not() {
        let short = vec![cue(10_000, 10_624, &["hello"])];
        assert!(
            mentions(short.clone(), "less than 15 frames"),
            "{:?}",
            texts(short)
        );

        let long_enough = vec![cue(10_000, 10_625, &["hello"])];
        assert!(
            !mentions(long_enough.clone(), "less than 15 frames"),
            "{:?}",
            texts(long_enough)
        );
    }

    /// 2 frames at 24 fps is 83 ms.
    #[test]
    fn cues_closer_than_two_frames_are_hinted_and_an_overlap_counts() {
        let tight = vec![
            cue(10_000, 12_000, &["first"]),
            cue(12_082, 14_000, &["second"]),
        ];
        assert!(
            mentions(tight.clone(), "less than 2 frames after"),
            "{:?}",
            texts(tight)
        );

        let overlapping = vec![
            cue(10_000, 12_000, &["first"]),
            cue(11_000, 14_000, &["second"]),
        ];
        assert!(mentions(overlapping, "less than 2 frames after"));

        let spaced = vec![
            cue(10_000, 12_000, &["first"]),
            cue(12_083, 14_000, &["second"]),
        ];
        assert!(
            !mentions(spaced.clone(), "less than 2 frames after"),
            "{:?}",
            texts(spaced)
        );
    }

    #[test]
    fn more_than_three_lines_is_hinted_and_three_is_not() {
        let four = vec![cue(10_000, 12_000, &["a", "b", "c", "d"])];
        assert!(
            mentions(four.clone(), "more than 3 lines"),
            "{:?}",
            texts(four)
        );

        let three = vec![cue(10_000, 12_000, &["a", "b", "c"])];
        assert!(!mentions(three, "more than 3 lines"));
    }

    #[test]
    fn a_long_line_is_hinted_and_the_hard_limit_replaces_the_advised_one() {
        let advised = vec![cue(10_000, 12_000, &["x".repeat(53).as_str()])];
        assert!(
            mentions(advised.clone(), "longer than 52 characters"),
            "{:?}",
            texts(advised.clone())
        );
        assert!(!mentions(advised, "longer than 79 characters"));

        let at_the_limit = vec![cue(10_000, 12_000, &["x".repeat(52).as_str()])];
        assert!(!mentions(at_the_limit, "characters"));

        let hard = vec![cue(10_000, 12_000, &["x".repeat(80).as_str()])];
        assert!(
            mentions(hard.clone(), "longer than 79 characters"),
            "{:?}",
            texts(hard.clone())
        );
        assert!(
            !mentions(hard.clone(), "longer than 52 characters"),
            "the 79 hint replaces the 52 one: {:?}",
            texts(hard)
        );
    }

    /// Characters, not bytes: a line of accented letters is as long as it looks.
    #[test]
    fn line_length_counts_characters_not_bytes() {
        let accented = vec![cue(10_000, 12_000, &["é".repeat(52).as_str()])];
        assert!(
            !mentions(accented.clone(), "characters"),
            "{:?}",
            texts(accented)
        );
    }

    /// Each rule speaks once for the whole job, however many cues break it.
    #[test]
    fn a_rule_is_said_once_however_many_cues_break_it() {
        let said = texts(vec![
            cue(10_000, 10_100, &["first"]),
            cue(20_000, 20_100, &["second"]),
            cue(30_000, 30_100, &["third"]),
        ])
        .iter()
        .filter(|text| text.contains("less than 15 frames"))
        .count();
        assert_eq!(said, 1);
    }

    #[test]
    fn well_spaced_subtitles_raise_nothing() {
        assert_eq!(
            subtitle_hints(
                &one_file(vec![
                    cue(5_000, 7_000, &["a line", "another"]),
                    cue(8_000, 10_000, &["one more"]),
                ]),
                FPS
            ),
            vec![]
        );
    }

    /// A rule of a format's own reads the same cue list through the same walk.
    #[test]
    fn a_caller_rule_is_read_over_the_cues_like_a_shared_one() {
        const SHOUTING: CueRule = CueRule {
            offends: |context| context.cue.lines.iter().any(|line| line.ends_with('!')),
            say: |file, at| format!("{file} shouts at {at}"),
        };
        let cues = one_file(vec![
            cue(5_000, 7_000, &["calm"]),
            cue(8_000, 10_000, &["loud!"]),
        ]);
        let hint = first_offence(&cues, FPS, &SHOUTING).expect("the second cue offends");
        assert_eq!(hint.text, "subs.srt shouts at 00:00:08.000");
        assert_eq!(first_offence(&one_file(Vec::new()), FPS, &SHOUTING), None);
    }
}
