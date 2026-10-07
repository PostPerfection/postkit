use serde::Serialize;

// what silence reads as, JSON has no -inf
pub const SILENCE_FLOOR_DBFS: f64 = -100.0;
pub const RMS_WINDOW_SECONDS: f64 = 0.3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelLevel {
    pub label: String,
    // the highest sample since the previous read
    pub peak_dbfs: f64,
    // over the last RMS_WINDOW_SECONDS
    pub rms_dbfs: f64,
}

impl ChannelLevel {
    pub fn silent(label: String) -> Self {
        ChannelLevel {
            label,
            peak_dbfs: SILENCE_FLOOR_DBFS,
            rms_dbfs: SILENCE_FLOOR_DBFS,
        }
    }
}

pub fn dbfs_of_amplitude(amplitude: f64) -> f64 {
    at_least_the_floor(20.0 * amplitude.log10())
}

// NaN and -inf read as the floor too
pub fn at_least_the_floor(dbfs: f64) -> f64 {
    dbfs.max(SILENCE_FLOOR_DBFS)
}

pub fn numbered_channel_label(index: usize) -> String {
    format!("Ch {}", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_scale_is_minus_six_dbfs() {
        assert!((dbfs_of_amplitude(0.5) - -6.0206).abs() < 1e-4);
        assert_eq!(dbfs_of_amplitude(1.0), 0.0);
    }

    #[test]
    fn silence_and_nonsense_read_as_the_floor() {
        assert_eq!(dbfs_of_amplitude(0.0), SILENCE_FLOOR_DBFS);
        assert_eq!(at_least_the_floor(f64::NEG_INFINITY), SILENCE_FLOOR_DBFS);
        assert_eq!(at_least_the_floor(f64::NAN), SILENCE_FLOOR_DBFS);
        assert_eq!(dbfs_of_amplitude(1e-9), SILENCE_FLOOR_DBFS);
    }
}
