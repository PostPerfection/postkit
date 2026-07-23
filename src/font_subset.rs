//! Font subsetting to a set of code points (dom#1023).
//!
//! Strips unused glyphs so an embedded subtitle font fits the SMPTE/interop
//! 640 KB limit (the TI "font compressor" equivalent). allsorts subsets into a
//! usable standalone OpenType font (keeps cmap), unlike PDF-only subsetters.

use allsorts::binary::read::ReadScope;
use allsorts::font::{Font, MatchingPresentation};
use allsorts::font_data::FontData;
use allsorts::subset::{subset, CmapTarget, SubsetProfile};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FontSubsetError {
    #[error("font parse error: {0}")]
    Parse(String),
    #[error("no requested character is present in the font")]
    NoGlyphs,
    #[error("subset failed: {0}")]
    Subset(String),
}

/// Subset a TTF/OTF font to the glyphs needed for `chars`.
///
/// Returns a standalone OpenType font with .notdef plus the glyphs for the
/// requested code points (and their composite dependencies). Chars absent from
/// the font are skipped; if none are present it errors.
pub fn subset_font(
    font_bytes: &[u8],
    chars: impl IntoIterator<Item = char>,
) -> Result<Vec<u8>, FontSubsetError> {
    let font_file = ReadScope::new(font_bytes)
        .read::<FontData>()
        .map_err(|e| FontSubsetError::Parse(e.to_string()))?;
    let provider = font_file
        .table_provider(0)
        .map_err(|e| FontSubsetError::Parse(e.to_string()))?;
    let mut font = Font::new(provider).map_err(|e| FontSubsetError::Parse(e.to_string()))?;

    let mut gids: Vec<u16> = vec![0]; // .notdef must be present
    for ch in chars {
        let gid = font
            .lookup_glyph_index(ch, MatchingPresentation::NotRequired, None)
            .0;
        if gid != 0 && !gids.contains(&gid) {
            gids.push(gid);
        }
    }
    if gids.len() == 1 {
        return Err(FontSubsetError::NoGlyphs);
    }
    gids.sort_unstable();

    let provider = font_file
        .table_provider(0)
        .map_err(|e| FontSubsetError::Parse(e.to_string()))?;
    // Minimal keeps a usable standalone font; Unicode cmap so renderers can map chars
    subset(&provider, &gids, &SubsetProfile::Minimal, CmapTarget::Unicode)
        .map_err(|e| FontSubsetError::Subset(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use skrifa::{FontRef, MetadataProvider};
    use std::path::PathBuf;

    fn find_ttf() -> Option<PathBuf> {
        let mut stack = vec![PathBuf::from("/usr/share/fonts")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ttf")) {
                    return Some(p);
                }
            }
        }
        None
    }

    #[test]
    fn subset_keeps_requested_chars_and_shrinks() {
        let Some(path) = find_ttf() else {
            eprintln!("skipping subset test: no .ttf under /usr/share/fonts");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let chars = ['A', 'b', 'C', '1', ' '];
        let out = subset_font(&bytes, chars).unwrap();
        assert!(
            out.len() < bytes.len(),
            "subset {} not smaller than input {}",
            out.len(),
            bytes.len()
        );
        // independent check via skrifa: the subset still maps the requested chars
        let font = FontRef::new(&out).unwrap();
        let cmap = font.charmap();
        for ch in chars {
            assert!(cmap.map(ch).is_some(), "subset dropped char {ch:?}");
        }
    }

    #[test]
    fn subset_errors_when_no_chars_present() {
        let Some(path) = find_ttf() else {
            eprintln!("skipping subset test: no .ttf under /usr/share/fonts");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        // a code point no Latin font will contain
        assert!(matches!(
            subset_font(&bytes, ['\u{10FFFD}']),
            Err(FontSubsetError::NoGlyphs)
        ));
    }
}
