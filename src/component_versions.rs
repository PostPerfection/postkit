use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentVersion {
    pub name: String,
    pub version: String,
}

impl ComponentVersion {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

pub fn installed_components(
    application_name: &str,
    application_version: &str,
) -> Vec<ComponentVersion> {
    let mut versions = vec![
        ComponentVersion::new(application_name, application_version),
        ComponentVersion::new("PostKit", env!("CARGO_PKG_VERSION")),
    ];

    #[cfg(feature = "grok-ffi")]
    versions.push(ComponentVersion::new("Grok", grok_version()));

    versions.push(ComponentVersion::new("FFmpeg", ffmpeg_version()));
    versions
}

#[cfg(feature = "grok-ffi")]
fn grok_version() -> String {
    let version = unsafe { grokj2k_sys::grk_version() };
    if version.is_null() {
        return "unavailable".to_string();
    }
    unsafe { std::ffi::CStr::from_ptr(version) }
        .to_string_lossy()
        .into_owned()
}

fn ffmpeg_version() -> String {
    let Ok(output) = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
    else {
        return "unavailable".to_string();
    };
    if !output.status.success() {
        return "unavailable".to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .and_then(parse_ffmpeg_version)
        .unwrap_or("unavailable")
        .to_string()
}

fn parse_ffmpeg_version(line: &str) -> Option<&str> {
    line.strip_prefix("ffmpeg version ")?
        .split_whitespace()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_version_is_taken_from_the_first_line() {
        assert_eq!(
            parse_ffmpeg_version("ffmpeg version 8.0.1 Copyright (c) FFmpeg"),
            Some("8.0.1")
        );
        assert_eq!(parse_ffmpeg_version("unknown"), None);
    }

    #[test]
    fn application_and_postkit_versions_are_always_present() {
        let versions = installed_components("Test Wizard", "1.2.3");
        assert_eq!(versions[0], ComponentVersion::new("Test Wizard", "1.2.3"));
        assert_eq!(
            versions[1],
            ComponentVersion::new("PostKit", env!("CARGO_PKG_VERSION"))
        );
    }
}
