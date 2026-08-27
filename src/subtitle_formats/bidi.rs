//! RTL bidi reordering (dom#860).
//!
//! Some cinema servers (e.g. Dolby DSS200) render subtitle text in logical
//! order without applying the Unicode bidi algorithm, so Hebrew/Arabic comes
//! out mirrored. `to_visual` returns visual-order text the wizards can feed to
//! the renderer/burn-in instead. Arabic presentation-form shaping is applied
//! via ar-reshaper before reordering; Hebrew needs only the reorder.

use ar_reshaper::ArabicReshaper;
use unicode_bidi::BidiInfo;

/// Reorder logical-order text to visual order, line by line.
///
/// Each line is shaped (Arabic joining forms) then reordered per the Unicode
/// bidi algorithm with auto-detected base direction. Pure; no allocation of
/// external state beyond the returned string.
pub fn to_visual(logical: &str) -> String {
    let reshaper = ArabicReshaper::default();
    logical
        .split('\n')
        .map(|line| reorder_line(line, &reshaper))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does the text hold any right-to-left run, and so need reordering at all?
///
/// The bidi algorithm answers rather than a list of script ranges, so every RTL
/// script Unicode defines counts, and so does a Latin line with one Hebrew
/// phrase in it: the server that does not reorder mirrors that phrase too.
pub fn has_rtl(text: &str) -> bool {
    BidiInfo::new(text, None).has_rtl()
}

fn reorder_line(line: &str, reshaper: &ArabicReshaper) -> String {
    let shaped = reshaper.reshape(line);
    let info = BidiInfo::new(&shaped, None);
    let Some(para) = info.paragraphs.first() else {
        return shaped;
    };
    info.reorder_line(para, para.range.clone()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hebrew_line_is_reversed_to_visual_order() {
        // pure RTL: logical alef-bet-gimel should display gimel-bet-alef
        let logical = "\u{05d0}\u{05d1}\u{05d2}";
        let visual = to_visual(logical);
        let rev: String = logical.chars().rev().collect();
        assert_eq!(visual, rev);
    }

    #[test]
    fn only_text_with_an_rtl_run_needs_reordering() {
        assert!(!has_rtl("hello world"));
        assert!(has_rtl("\u{05d0}\u{05d1}"));
        // a latin sentence with one hebrew word in it still has an rtl run
        assert!(has_rtl("read \u{05d0}\u{05d1} now"));
        // thaana, an rtl script that is neither hebrew nor arabic
        assert!(has_rtl("\u{0780}\u{0781}"));
    }

    #[test]
    fn latin_line_is_unchanged() {
        assert_eq!(to_visual("hello world"), "hello world");
    }

    #[test]
    fn per_line_reorder_keeps_line_breaks() {
        let logical = "\u{05d0}\u{05d1}\nab";
        let out = to_visual(logical);
        let mut lines = out.split('\n');
        assert_eq!(lines.next().unwrap(), "\u{05d1}\u{05d0}");
        assert_eq!(lines.next().unwrap(), "ab");
    }

    #[test]
    fn arabic_is_shaped_to_presentation_forms() {
        // isolated arabic letters get joined to contextual forms, so the shaped
        // output differs from the raw input (and reorders for RTL)
        let logical = "\u{0627}\u{0644}\u{0633}\u{0644}\u{0627}\u{0645}";
        let out = to_visual(logical);
        assert_ne!(out, logical);
    }
}
