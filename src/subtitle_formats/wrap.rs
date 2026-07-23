//! Auto line-wrapping for long subtitles (dom#1626).
//!
//! Wraps at a max column count, breaking on whitespace and never mid-word (an
//! overlong single word stays whole on its own line). Existing '\n' breaks are
//! preserved. `wrap_styled` keeps per-run styling; `wrap_plain` works on bare
//! strings.

use super::{Rgba, StyledCue, StyledRun};

/// Wrap a plain string at `max_cols`, collapsing runs of whitespace to one
/// space and never splitting a word.
pub fn wrap_plain(text: &str, max_cols: usize) -> String {
    text.split('\n')
        .map(|line| wrap_line(line, max_cols))
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_line(line: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for word in line.split_whitespace() {
        let wlen = word.chars().count();
        if col == 0 {
            out.push_str(word);
            col = wlen;
        } else if col + 1 + wlen > max_cols {
            out.push('\n');
            out.push_str(word);
            col = wlen;
        } else {
            out.push(' ');
            out.push_str(word);
            col += 1 + wlen;
        }
    }
    out
}

/// Wrap a cue's styled text at `max_cols`, preserving run styling and boundaries.
pub fn wrap_styled(cue: &StyledCue, max_cols: usize) -> StyledCue {
    let mut flat: Vec<(char, Key)> = Vec::new();
    for run in &cue.runs {
        let key = Key::from_run(run);
        for ch in run.text.chars() {
            flat.push((ch, key.clone()));
        }
    }
    let wrapped = wrap_flat(&flat, max_cols);
    StyledCue {
        runs: rebuild_runs(wrapped),
        ..cue.clone()
    }
}

#[derive(Clone, PartialEq, Default)]
struct Key {
    italic: bool,
    bold: bool,
    underline: bool,
    color: Option<Rgba>,
}

impl Key {
    fn from_run(r: &StyledRun) -> Self {
        Key {
            italic: r.italic,
            bold: r.bold,
            underline: r.underline,
            color: r.color,
        }
    }
}

fn wrap_flat(flat: &[(char, Key)], max_cols: usize) -> Vec<(char, Key)> {
    let mut out: Vec<(char, Key)> = Vec::new();
    for (li, line) in split_on_newline(flat).into_iter().enumerate() {
        if li > 0 {
            let key = out.last().map(|(_, k)| k.clone()).unwrap_or_default();
            out.push(('\n', key));
        }
        let mut col = 0usize;
        for word in words_of(&line) {
            let wlen = word.len();
            if col == 0 {
                out.extend(word.iter().cloned());
                col = wlen;
            } else if col + 1 + wlen > max_cols {
                let key = out.last().map(|(_, k)| k.clone()).unwrap_or_default();
                out.push(('\n', key));
                out.extend(word.iter().cloned());
                col = wlen;
            } else {
                out.push((' ', word[0].1.clone()));
                out.extend(word.iter().cloned());
                col += 1 + wlen;
            }
        }
    }
    out
}

fn split_on_newline(flat: &[(char, Key)]) -> Vec<Vec<(char, Key)>> {
    let mut lines = vec![Vec::new()];
    for (ch, key) in flat {
        if *ch == '\n' {
            lines.push(Vec::new());
        } else {
            lines.last_mut().unwrap().push((*ch, key.clone()));
        }
    }
    lines
}

/// Split a styled line into words (maximal non-whitespace char runs).
fn words_of(line: &[(char, Key)]) -> Vec<Vec<(char, Key)>> {
    let mut words = Vec::new();
    let mut cur: Vec<(char, Key)> = Vec::new();
    for (ch, key) in line {
        if ch.is_whitespace() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push((*ch, key.clone()));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn rebuild_runs(flat: Vec<(char, Key)>) -> Vec<StyledRun> {
    let mut runs: Vec<StyledRun> = Vec::new();
    for (ch, key) in flat {
        match runs.last_mut() {
            Some(last) if Key::from_run(last) == key => last.text.push(ch),
            _ => runs.push(StyledRun {
                text: ch.to_string(),
                italic: key.italic,
                bold: key.bold,
                underline: key.underline,
                color: key.color,
            }),
        }
    }
    if runs.is_empty() {
        runs.push(StyledRun::plain(String::new()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle_formats::StyledCue;

    #[test]
    fn wrap_plain_breaks_on_whitespace() {
        assert_eq!(wrap_plain("aaa bbb ccc ddd", 7), "aaa bbb\nccc ddd");
    }

    #[test]
    fn wrap_plain_keeps_overlong_word_whole() {
        assert_eq!(
            wrap_plain("supercalifragilistic hi", 5),
            "supercalifragilistic\nhi"
        );
    }

    #[test]
    fn wrap_plain_preserves_existing_breaks() {
        assert_eq!(wrap_plain("aa bb\ncc dd", 5), "aa bb\ncc dd");
    }

    #[test]
    fn wrap_styled_keeps_run_boundaries_and_breaks_between_words() {
        // "hello " plain + "world foo" italic, wrap at 8
        let cue = StyledCue::text(
            0,
            1000,
            vec![
                StyledRun::plain("hello "),
                StyledRun {
                    text: "world foo".into(),
                    italic: true,
                    bold: false,
                    underline: false,
                    color: None,
                },
            ],
        );
        let wrapped = wrap_styled(&cue, 8);
        // no line exceeds 8 columns, no word split
        for line in wrapped.plain_text().split('\n') {
            assert!(line.chars().count() <= 8, "line too long: {line:?}");
        }
        assert!(wrapped.plain_text().contains('\n'));
        // the italic word keeps its styling
        assert!(
            wrapped
                .runs
                .iter()
                .any(|r| r.italic && r.text.contains("world"))
        );
        // words stay whole
        let joined: String = wrapped
            .plain_text()
            .split('\n')
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("world"));
        assert!(joined.contains("foo"));
    }
}
