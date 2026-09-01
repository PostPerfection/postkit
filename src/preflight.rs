//! The refusals a create job makes before the encode, for what every package
//! format refuses alike.
//!
//! The rule: a refusal that fires once the encode has run must also fire from
//! here, so nothing spends a whole encode to find out it cannot be packaged.

use std::path::{Path, PathBuf};

/// What the encode would hand a burn, as the pre-encode checks see it.
pub struct BurnTarget<'a> {
    /// Every timed-text file the composition packages.
    pub timed_text: &'a [PathBuf],
    /// What hands the encoder X'Y'Z' frames, named as the caller's own flags,
    /// when the job is on one of those routes.
    pub frames_already_xyz: Option<&'a str>,
    /// The picture is a J2K directory, so nothing decodes.
    pub input_is_codestreams: bool,
}

/// Refuse a burnt-in subtitle the encode cannot honour, before anything is
/// encoded.
///
/// Text is drawn in display RGB, so a source that reaches the encoder as X'Y'Z'
/// would land it in the wrong space. P3 and Rec.2020 sources are fine: the burn
/// goes on first and the transform to X'Y'Z' converts it with the picture.
pub fn check_burn_supported(burn_path: &Path, target: &BurnTarget) -> Result<(), String> {
    if !burn_path.is_file() {
        return Err(format!(
            "--burn-subtitle file not found: {}",
            burn_path.display()
        ));
    }
    if target
        .timed_text
        .iter()
        .any(|packaged| same_file(burn_path, packaged))
    {
        return Err(format!(
            "{} is given to both --burn-subtitle and --subtitle: a burnt-in subtitle must not \
             also be a timed-text track, so pick one",
            burn_path.display()
        ));
    }
    if target.input_is_codestreams {
        return Err(
            "--burn-subtitle needs frames to draw on, and a J2K directory is already compressed"
                .to_string(),
        );
    }
    if let Some(routes) = target.frames_already_xyz {
        return Err(format!(
            "--burn-subtitle draws in display RGB, but this source reaches the encoder as \
             X'Y'Z' already ({routes}): burn from the display-RGB master instead"
        ));
    }
    Ok(())
}

/// Whether two paths name the same file, falling back to the paths themselves
/// when either cannot be canonicalised.
fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XYZ_ROUTES: &str = "--source-colourspace xyz";

    fn drawable(timed_text: &[PathBuf]) -> BurnTarget<'_> {
        BurnTarget {
            timed_text,
            frames_already_xyz: None,
            input_is_codestreams: false,
        }
    }

    #[test]
    fn a_burn_is_refused_wherever_it_would_be_drawn_in_the_wrong_place() {
        let dir = tempfile::tempdir().unwrap();
        let srt = dir.path().join("cues.srt");
        std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nfirst line\n\n").unwrap();
        let elsewhere = vec![dir.path().join("other.ttml")];

        check_burn_supported(&srt, &drawable(&[])).expect("a plain display-RGB burn is fine");
        check_burn_supported(&srt, &drawable(&elsewhere))
            .expect("a different timed-text file is fine");

        let missing = dir.path().join("nope.srt");
        let same = vec![srt.clone()];
        for (label, result, needle) in [
            (
                "missing file",
                check_burn_supported(&missing, &drawable(&[])),
                "not found",
            ),
            (
                "same file as a timed-text track",
                check_burn_supported(&srt, &drawable(&same)),
                "pick one",
            ),
            (
                "J2K input",
                check_burn_supported(
                    &srt,
                    &BurnTarget {
                        input_is_codestreams: true,
                        ..drawable(&[])
                    },
                ),
                "already compressed",
            ),
            (
                "frames already X'Y'Z'",
                check_burn_supported(
                    &srt,
                    &BurnTarget {
                        frames_already_xyz: Some(XYZ_ROUTES),
                        ..drawable(&[])
                    },
                ),
                "X'Y'Z' already",
            ),
        ] {
            let error = result.expect_err(label);
            assert!(error.contains(needle), "{label}: got {error}");
        }
    }

    /// The refusal names the caller's own flags, since they are what the reader
    /// would have to change.
    #[test]
    fn the_xyz_refusal_names_the_routes_the_caller_gave_it() {
        let dir = tempfile::tempdir().unwrap();
        let srt = dir.path().join("cues.srt");
        std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nline\n\n").unwrap();

        let error = check_burn_supported(
            &srt,
            &BurnTarget {
                frames_already_xyz: Some("--hdr-already-pq, or the HDR-to-DCI LUT"),
                ..drawable(&[])
            },
        )
        .unwrap_err();
        assert!(
            error.contains("--hdr-already-pq, or the HDR-to-DCI LUT"),
            "{error}"
        );
    }
}
