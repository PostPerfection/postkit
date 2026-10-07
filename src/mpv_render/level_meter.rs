use std::collections::HashMap;

use crate::audio_levels::{
    ChannelLevel, RMS_WINDOW_SECONDS, SILENCE_FLOOR_DBFS, at_least_the_floor,
    numbered_channel_label,
};

pub(super) const FILTER_LABEL: &str = "levelmeter";
pub(super) const DEFAULT_SAMPLE_RATE: u32 = 48_000;
// mpv's names for speakers it has no name for
const UNNAMED_SPEAKER: &str = "na";
const UNKNOWN_SPEAKER_PREFIX: &str = "sp";
const SPEAKER_SEPARATOR: char = '-';

// mpv's std_layout_names, the layout names audio-params/channels gives in place of the speakers
const MPV_LAYOUT_SPEAKERS: &[(&str, &str)] = &[
    ("mono", "fc"),
    ("1.0", "fc"),
    ("stereo", "fl-fr"),
    ("2.0", "fl-fr"),
    ("2.1", "fl-fr-lfe"),
    ("3.0", "fl-fr-fc"),
    ("3.0(back)", "fl-fr-bc"),
    ("4.0", "fl-fr-fc-bc"),
    ("quad", "fl-fr-bl-br"),
    ("quad(side)", "fl-fr-sl-sr"),
    ("3.1", "fl-fr-fc-lfe"),
    ("3.1(back)", "fl-fr-lfe-bc"),
    ("5.0", "fl-fr-fc-bl-br"),
    ("5.0(alsa)", "fl-fr-bl-br-fc"),
    ("5.0(side)", "fl-fr-fc-sl-sr"),
    ("4.1", "fl-fr-fc-lfe-bc"),
    ("4.1(alsa)", "fl-fr-bl-br-lfe"),
    ("5.1", "fl-fr-fc-lfe-bl-br"),
    ("5.1(alsa)", "fl-fr-bl-br-fc-lfe"),
    ("5.1(side)", "fl-fr-fc-lfe-sl-sr"),
    ("6.0", "fl-fr-fc-bc-sl-sr"),
    ("6.0(front)", "fl-fr-flc-frc-sl-sr"),
    ("hexagonal", "fl-fr-fc-bl-br-bc"),
    ("6.1", "fl-fr-fc-lfe-bc-sl-sr"),
    ("6.1(back)", "fl-fr-fc-lfe-bl-br-bc"),
    ("6.1(top)", "fl-fr-fc-lfe-bl-br-tc"),
    ("6.1(front)", "fl-fr-lfe-flc-frc-sl-sr"),
    ("7.0", "fl-fr-fc-bl-br-sl-sr"),
    ("7.0(front)", "fl-fr-fc-flc-frc-sl-sr"),
    ("7.0(rear)", "fl-fr-fc-bl-br-sdl-sdr"),
    ("7.1", "fl-fr-fc-lfe-bl-br-sl-sr"),
    ("7.1(alsa)", "fl-fr-bl-br-fc-lfe-sl-sr"),
    ("7.1(wide)", "fl-fr-fc-lfe-bl-br-flc-frc"),
    ("7.1(wide-side)", "fl-fr-fc-lfe-flc-frc-sl-sr"),
    ("7.1(top)", "fl-fr-fc-lfe-bl-br-tfl-tfr"),
    ("7.1(rear)", "fl-fr-fc-lfe-bl-br-sdl-sdr"),
    ("octagonal", "fl-fr-fc-bl-br-bc-sl-sr"),
    ("cube", "fl-fr-bl-br-tfl-tfr-tbl-tbr"),
    (
        "hexadecagonal",
        "fl-fr-fc-bl-br-bc-sl-sr-tfc-tfl-tfr-tbl-tbc-tbr-wl-wr",
    ),
    ("downmix", "fl-fr"),
    (
        "22.2",
        "fl-fr-fc-lfe-bl-br-flc-frc-bc-sl-sr-tc-tfl-tfc-tfr-tbl-tbc-tbr-lfe2-tsl-tsr-bfc-bfl-bfr",
    ),
];

// with reset=1 a reading covers one frame, and asetnsamples makes every frame one RMS window long
pub(super) fn filter(sample_rate: u32) -> String {
    let window_samples = (RMS_WINDOW_SECONDS * f64::from(sample_rate)).round() as u32;
    format!(
        "@{FILTER_LABEL}:lavfi=[asetnsamples=n={window_samples}:p=0,astats=metadata=1:reset=1:measure_perchannel=Peak_level+RMS_level:measure_overall=none]"
    )
}

pub(super) fn metadata_property() -> String {
    format!("af-metadata/{FILTER_LABEL}")
}

pub(super) fn channel_labels(layout: &str, channel_count: usize) -> Vec<String> {
    let speakers = MPV_LAYOUT_SPEAKERS
        .iter()
        .find(|(name, _)| *name == layout)
        .map_or(layout, |(_, speakers)| speakers);
    let names: Vec<&str> = speakers.split(SPEAKER_SEPARATOR).collect();
    if names.len() != channel_count {
        return (0..channel_count).map(numbered_channel_label).collect();
    }
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let unnamed = *name == UNNAMED_SPEAKER || name.starts_with(UNKNOWN_SPEAKER_PREFIX);
            match unnamed {
                true => numbered_channel_label(index),
                false => name.to_ascii_uppercase(),
            }
        })
        .collect()
}

// af-metadata reads as a JSON object of strings, silence as "-inf"
pub(super) fn levels_of_metadata(metadata: &str, labels: Vec<String>) -> Option<Vec<ChannelLevel>> {
    let values: HashMap<String, String> = serde_json::from_str(metadata).ok()?;
    let level = |channel: usize, measure: &str| {
        values
            .get(&format!("lavfi.astats.{}.{measure}", channel + 1))
            .and_then(|value| value.parse::<f64>().ok())
            .map_or(SILENCE_FLOOR_DBFS, at_least_the_floor)
    };
    let levels = labels
        .into_iter()
        .enumerate()
        .map(|(channel, label)| ChannelLevel {
            label,
            peak_dbfs: level(channel, "Peak_level"),
            rms_dbfs: level(channel, "RMS_level"),
        })
        .collect();
    Some(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_name_reads_as_its_speakers() {
        assert_eq!(
            channel_labels("5.1(side)", 6),
            ["FL", "FR", "FC", "LFE", "SL", "SR"]
        );
        assert_eq!(channel_labels("fl-fr-lfe2", 3), ["FL", "FR", "LFE2"]);
    }

    #[test]
    fn channels_mpv_cannot_name_are_numbered() {
        assert_eq!(channel_labels("unknown3", 3), ["Ch 1", "Ch 2", "Ch 3"]);
        assert_eq!(channel_labels("fl-na-sp40", 3), ["FL", "Ch 2", "Ch 3"]);
        assert_eq!(channel_labels("stereo", 3), ["Ch 1", "Ch 2", "Ch 3"]);
    }

    #[test]
    fn the_metadata_reads_per_channel_with_silence_at_the_floor() {
        let metadata = r#"{"lavfi.astats.1.Peak_level":"-6.020600","lavfi.astats.1.RMS_level":"-9.030900","lavfi.astats.2.Peak_level":"-inf","lavfi.astats.2.RMS_level":"-inf"}"#;

        let levels =
            levels_of_metadata(metadata, vec!["FL".to_string(), "FR".to_string()]).unwrap();

        assert_eq!(levels[0].label, "FL");
        assert_eq!(levels[0].peak_dbfs, -6.0206);
        assert_eq!(levels[0].rms_dbfs, -9.0309);
        assert_eq!(levels[1].peak_dbfs, SILENCE_FLOOR_DBFS);
        assert_eq!(levels[1].rms_dbfs, SILENCE_FLOOR_DBFS);
    }

    #[test]
    fn the_filter_frames_one_window_of_samples() {
        assert!(filter(DEFAULT_SAMPLE_RATE).contains("asetnsamples=n=14400:p=0"));
        assert!(filter(44_100).contains("asetnsamples=n=13230:p=0"));
    }
}
