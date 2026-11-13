use crate::encode::InputType;
use crate::webhook::{WebhookConfig, WebhookEvent};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DONE_DIRECTORY_NAME: &str = "done";
pub const FAILED_DIRECTORY_NAME: &str = "failed";
pub const AUDIO_SIDECAR_EXTENSION: &str = "wav";
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
const MINIMUM_POLL_INTERVAL_SECONDS: u64 = 1;

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

#[derive(clap::Args)]
pub struct WatchArgs {
    #[arg(help = "Directory to watch for masters")]
    pub dir: PathBuf,
    #[arg(
        short,
        long,
        help = "Directory each package is written to, under the master's file stem"
    )]
    pub output: PathBuf,
    #[arg(
        long,
        help = "POST a JSON notification to this URL when a package is built or fails"
    )]
    pub webhook_url: Option<String>,
    #[arg(
        long,
        default_value_t = DEFAULT_POLL_INTERVAL_SECONDS,
        value_parser = clap::value_parser!(u64).range(MINIMUM_POLL_INTERVAL_SECONDS..),
        help = "Seconds between polls"
    )]
    pub interval: u64,
    #[arg(
        last = true,
        help = "Flags passed to `create` for every package, after a `--` separator"
    )]
    pub create_arguments: Vec<String>,
}

impl WatchArgs {
    pub fn command<'a>(
        &'a self,
        subtitle_extension: &'a str,
        webhook_event_prefix: &'a str,
    ) -> WatchCommand<'a> {
        WatchCommand {
            watch_dir: &self.dir,
            output_dir: &self.output,
            interval: Duration::from_secs(self.interval),
            subtitle_extension,
            create_arguments: &self.create_arguments,
            webhook_url: self.webhook_url.clone(),
            webhook_event_prefix,
        }
    }
}

pub struct WatchCommand<'a> {
    pub watch_dir: &'a Path,
    pub output_dir: &'a Path,
    pub interval: Duration,
    pub subtitle_extension: &'a str,
    pub create_arguments: &'a [String],
    pub webhook_url: Option<String>,
    pub webhook_event_prefix: &'a str,
}

pub struct PackageTarget {
    pub package_dir: PathBuf,
    pub output_argument: PathBuf,
}

pub fn watch_directory(
    command: &WatchCommand,
    package_target: impl Fn(&str) -> Result<PackageTarget, String>,
) -> Result<(), String> {
    if !command.watch_dir.is_dir() {
        return Err(format!(
            "watch directory does not exist: {}",
            command.watch_dir.display()
        ));
    }
    std::fs::create_dir_all(command.output_dir)
        .map_err(|e| format!("cannot create {}: {e}", command.output_dir.display()))?;
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot find the running binary: {e}"))?;
    let webhook = command.webhook_url.clone().map(|url| WebhookConfig {
        url,
        ..Default::default()
    });

    tracing::info!(
        "watching {} for masters, polling every {:?}",
        command.watch_dir.display(),
        command.interval
    );

    let mut states = CandidateStates::default();
    loop {
        let masters = find_masters(command.watch_dir);
        states.retain_present(&masters);

        for master in &masters {
            let Some(measurement) = measure(master) else {
                continue;
            };
            if !states.observe(master, measurement) {
                continue;
            }
            states.mark_built(master);
            build_master(
                command,
                &executable,
                webhook.as_ref(),
                master,
                &package_target,
            );
        }

        std::thread::sleep(command.interval);
    }
}

fn build_master(
    command: &WatchCommand,
    executable: &Path,
    webhook: Option<&WebhookConfig>,
    master: &Path,
    package_target: &impl Fn(&str) -> Result<PackageTarget, String>,
) {
    let Some(stem) = master.file_stem().and_then(|s| s.to_str()) else {
        tracing::error!("cannot read a file stem from {}", master.display());
        return;
    };
    let target = match package_target(stem) {
        Ok(target) => target,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };
    let log_path = match crate::job_log::job_log_path(&target.package_dir) {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };
    let sidecar = |extension: &str| {
        let path = command.watch_dir.join(format!("{stem}.{extension}"));
        path.is_file().then_some(path)
    };
    let audio = sidecar(AUDIO_SIDECAR_EXTENSION);
    let subtitle = sidecar(command.subtitle_extension);

    let mut arguments: Vec<OsString> = vec![
        "create".into(),
        "--title".into(),
        stem.into(),
        "--video".into(),
        master.into(),
        "--output".into(),
        target.output_argument.as_os_str().into(),
    ];
    if let Some(audio) = audio.as_deref() {
        arguments.push("--audio".into());
        arguments.push(audio.into());
    }
    if let Some(subtitle) = subtitle.as_deref() {
        arguments.push("--subtitle".into());
        arguments.push(subtitle.into());
    }
    arguments.extend(command.create_arguments.iter().map(OsString::from));

    // dcpwizard create truncates the job log as it starts
    let captured_output = match tempfile::tempfile() {
        Ok(file) => file,
        Err(e) => {
            tracing::error!("cannot create a file for the create output: {e}");
            return;
        }
    };
    let (captured_stdout, captured_stderr) =
        match (captured_output.try_clone(), captured_output.try_clone()) {
            (Ok(stdout), Ok(stderr)) => (stdout, stderr),
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!("cannot create a file for the create output: {e}");
                return;
            }
        };
    if let Err(e) = std::fs::File::create(&log_path) {
        tracing::error!("cannot write {}: {e}", log_path.display());
        return;
    }

    tracing::info!(
        "building {} from {}",
        target.package_dir.display(),
        master.display()
    );
    let started = Instant::now();
    let outcome = Command::new(executable)
        .args(&arguments)
        .stdout(Stdio::from(captured_stdout))
        .stderr(Stdio::from(captured_stderr))
        .status();
    let elapsed_seconds = started.elapsed().as_secs_f64();
    append_captured_output(captured_output, &log_path);

    let failure = match outcome {
        Ok(status) if status.success() => None,
        Ok(status) => Some(match status.code() {
            Some(code) => format!("create exited {code}, log at {}", log_path.display()),
            None => format!(
                "create was killed by a signal, log at {}",
                log_path.display()
            ),
        }),
        Err(e) => Some(format!("could not run create: {e}")),
    };
    let sidecars: Vec<&Path> = [audio.as_deref(), subtitle.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let prefix = command.webhook_event_prefix;

    let Some(message) = failure else {
        move_job_aside(
            master,
            &sidecars,
            &command.watch_dir.join(DONE_DIRECTORY_NAME),
        );
        tracing::info!(
            "built {} in {elapsed_seconds:.1} s",
            target.package_dir.display()
        );
        post_event(
            webhook,
            &format!("{prefix}.created"),
            stem,
            crate::webhook::build_job_completed_payload(stem, &target.package_dir, elapsed_seconds),
        );
        return;
    };

    move_job_aside(
        master,
        &sidecars,
        &command.watch_dir.join(FAILED_DIRECTORY_NAME),
    );
    tracing::error!("create failed for {stem}: see {}", log_path.display());
    post_event(
        webhook,
        &format!("{prefix}.failed"),
        stem,
        crate::webhook::build_job_failed_payload(stem, &message),
    );
}

fn append_captured_output(mut captured_output: std::fs::File, log_path: &Path) {
    use std::io::Seek;
    let appended = captured_output
        .rewind()
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
        })
        .and_then(|mut log| std::io::copy(&mut captured_output, &mut log));
    if let Err(e) = appended {
        tracing::error!("cannot write {}: {e}", log_path.display());
    }
}

fn move_job_aside(master: &Path, sidecars: &[&Path], directory: &Path) {
    if let Err(e) = std::fs::create_dir_all(directory) {
        tracing::error!("cannot create {}: {e}", directory.display());
        return;
    }
    // a master in done/ or failed/ means its sidecars are already there
    for source in sidecars.iter().chain([&master]) {
        let Some(file_name) = source.file_name() else {
            tracing::error!("cannot read a file name from {}", source.display());
            continue;
        };
        let destination = crate::fs::unused_path(directory, file_name);
        if let Err(e) = std::fs::rename(source, &destination) {
            tracing::error!(
                "cannot move {} into {}: {e}",
                source.display(),
                directory.display()
            );
        }
    }
}

fn post_event(
    webhook: Option<&WebhookConfig>,
    event_type: &str,
    job_id: &str,
    payload_json: String,
) {
    let Some(config) = webhook else {
        return;
    };
    let event = WebhookEvent {
        event_type: event_type.to_string(),
        job_id: job_id.to_string(),
        payload_json,
        timestamp: String::new(),
    };
    let result = crate::webhook::send_webhook(config, &event);
    if !result.success {
        tracing::warn!("webhook delivery failed: {}", result.error);
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

    #[derive(clap::Parser)]
    struct WatchCli {
        #[command(flatten)]
        watch: WatchArgs,
    }

    #[test]
    fn the_poll_interval_defaults_and_refuses_a_zero() {
        use clap::Parser;
        let parsed = WatchCli::try_parse_from(["watch", "in", "--output", "out"]).unwrap();
        assert_eq!(parsed.watch.interval, DEFAULT_POLL_INTERVAL_SECONDS);
        assert!(
            WatchCli::try_parse_from(["watch", "in", "--output", "out", "--interval", "0"])
                .is_err()
        );
    }
}
