use crate::encode::InputType;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DONE_DIRECTORY_NAME: &str = "done";
pub const FAILED_DIRECTORY_NAME: &str = "failed";
pub const AUDIO_SIDECAR_EXTENSION: &str = "wav";
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
pub const MINIMUM_POLL_INTERVAL_SECONDS: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    size: u64,
    modified: Duration,
    entry_count: usize,
}

#[derive(Debug, Clone, Copy)]
struct CandidateState {
    measurement: Measurement,
    built: bool,
}

#[derive(Default)]
struct CandidateStates {
    states: HashMap<PathBuf, CandidateState>,
}

impl CandidateStates {
    fn observe(&mut self, path: &Path, measurement: Measurement) -> bool {
        let Some(state) = self.states.get_mut(path) else {
            self.states.insert(
                path.to_path_buf(),
                CandidateState {
                    measurement,
                    built: false,
                },
            );
            return false;
        };
        if state.built {
            return false;
        }
        let ready = state.measurement == measurement;
        state.measurement = measurement;
        ready
    }

    fn mark_built(&mut self, path: &Path) {
        if let Some(state) = self.states.get_mut(path) {
            state.built = true;
        }
    }

    fn retain_present(&mut self, present: &[PathBuf]) {
        self.states.retain(|path, _| present.contains(path));
    }
}

fn measure(path: &Path) -> Option<Measurement> {
    if path.is_dir() {
        let mut size = 0;
        let mut entry_count = 0;
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            entry_count += 1;
            if let Ok(metadata) = entry.metadata()
                && metadata.is_file()
            {
                size += metadata.len();
            }
        }
        return Some(Measurement {
            size,
            modified: Duration::ZERO,
            entry_count,
        });
    }

    std::fs::File::open(path).ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(Measurement {
        size: metadata.len(),
        modified,
        entry_count: 0,
    })
}

fn find_masters(watch_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(watch_dir) else {
        return Vec::new();
    };

    let mut masters = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with('.')
            || file_name == DONE_DIRECTORY_NAME
            || file_name == FAILED_DIRECTORY_NAME
        {
            continue;
        }
        let path = entry.path();
        let input_type = crate::encode::detect_input_type(&path);
        if matches!(
            input_type,
            InputType::Video
                | InputType::ImageSequence
                | InputType::J2kSequence
                | InputType::PictureMxf
        ) {
            masters.push(path);
        }
    }
    masters.sort();
    masters
}

pub fn watch_directory<F>(
    watch_dir: &Path,
    interval: Duration,
    should_stop: &dyn Fn() -> bool,
    on_master_ready: F,
) where
    F: Fn(&Path),
{
    if !watch_dir.is_dir() {
        tracing::error!("watch directory does not exist: {}", watch_dir.display());
        return;
    }

    tracing::info!(
        "watching {} for masters, polling every {:?}",
        watch_dir.display(),
        interval
    );

    let mut states = CandidateStates::default();

    loop {
        if should_stop() {
            tracing::info!("watch stopping");
            return;
        }

        let masters = find_masters(watch_dir);
        states.retain_present(&masters);

        for master in &masters {
            let Some(measurement) = measure(master) else {
                continue;
            };
            if !states.observe(master, measurement) {
                continue;
            }
            states.mark_built(master);
            on_master_ready(master);
        }

        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_measurement(size: u64) -> Measurement {
        Measurement {
            size,
            modified: Duration::from_secs(size),
            entry_count: 0,
        }
    }

    #[test]
    fn a_master_is_ready_only_after_two_equal_measurements() {
        let mut states = CandidateStates::default();
        let path = Path::new("/watch/feature.mp4");

        assert!(!states.observe(path, file_measurement(10)));
        assert!(!states.observe(path, file_measurement(20)));
        assert!(!states.observe(path, file_measurement(30)));
        assert!(states.observe(path, file_measurement(30)));
    }

    #[test]
    fn a_master_already_built_is_never_ready_again() {
        let mut states = CandidateStates::default();
        let path = Path::new("/watch/feature.mp4");

        assert!(!states.observe(path, file_measurement(10)));
        assert!(states.observe(path, file_measurement(10)));
        states.mark_built(path);

        assert!(!states.observe(path, file_measurement(10)));
        assert!(!states.observe(path, file_measurement(10)));
    }

    #[test]
    fn a_directory_that_gains_a_frame_is_not_ready() {
        let mut states = CandidateStates::default();
        let path = Path::new("/watch/frames");
        let ten_frames = Measurement {
            size: 1000,
            modified: Duration::ZERO,
            entry_count: 10,
        };
        let eleven_frames = Measurement {
            size: 1100,
            modified: Duration::ZERO,
            entry_count: 11,
        };

        assert!(!states.observe(path, ten_frames));
        assert!(!states.observe(path, eleven_frames));
        assert!(states.observe(path, eleven_frames));
    }

    #[test]
    fn a_master_that_leaves_the_folder_is_forgotten() {
        let mut states = CandidateStates::default();
        let path = Path::new("/watch/feature.mp4");

        assert!(!states.observe(path, file_measurement(10)));
        states.retain_present(&[]);
        assert!(!states.observe(path, file_measurement(10)));
    }
}
