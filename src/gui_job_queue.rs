//! The queue of builds a wizard GUI runs, with its on-disk record: one JSON
//! line per job, appended when a job is queued and on every state change after
//! it, so closing the window or losing the process does not lose what was
//! queued. The last record for an id is the job.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// What a job left running when the window closed reports on the next launch.
pub const INTERRUPTED_MESSAGE: &str = "the app closed while this job was running";

const GUI_JOBS_FILE_NAME: &str = "gui-jobs.jsonl";

/// What the queue reads from a wizard's job config to list it and record it.
pub trait GuiJob: Serialize + DeserializeOwned + Clone {
    fn id(&self) -> u64;
    fn title(&self) -> &str;
    fn output_dir(&self) -> &Path;
}

/// Where a wizard's Jobs panel keeps its queue. `environment_variable` points a
/// second app, or a test, at a file of its own.
pub fn jobs_path(environment_variable: &str, data_dir: PathBuf) -> PathBuf {
    match std::env::var(environment_variable) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => data_dir.join(GUI_JOBS_FILE_NAME),
    }
}

/// The states the queue moves a job through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredJobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One line of the jobs file.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredJob<C> {
    pub state: StoredJobState,
    pub message: String,
    pub config: C,
}

/// Append one record as a JSON line, creating the file and its parent dir.
fn append_record<C: GuiJob>(path: &Path, record: &StoredJob<C>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).map_err(|e| format!("serialize job: {e}"))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("cannot append: {e}"))
}

/// Record a job at the state it has just reached.
pub fn record<C: GuiJob>(path: &Path, state: StoredJobState, message: &str, config: &C) {
    let stored = StoredJob {
        state,
        message: message.to_string(),
        config: config.clone(),
    };
    if let Err(e) = append_record(path, &stored) {
        report(&format!(
            "could not record job {} in {}: {e}",
            config.id(),
            path.display()
        ));
    }
}

/// The GUI has no tracing subscriber, so an error goes where the job log goes.
fn report(message: &str) {
    eprintln!("[jobs] {message}");
}

/// What the jobs file held: the last record per job id, ordered by id, with a
/// job left running failed, plus how many lines could not be read.
pub struct LoadedJobs<C> {
    pub jobs: Vec<StoredJob<C>>,
    pub skipped: usize,
}

/// Read the jobs file and rewrite it with one line per job.
pub fn load<C: GuiJob>(path: &Path) -> LoadedJobs<C> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LoadedJobs {
                jobs: Vec::new(),
                skipped: 0,
            };
        }
        Err(e) => {
            report(&format!("could not read {}: {e}", path.display()));
            return LoadedJobs {
                jobs: Vec::new(),
                skipped: 0,
            };
        }
    };

    let mut jobs: Vec<StoredJob<C>> = Vec::new();
    let mut skipped = 0;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoredJob<C>>(line) {
            Ok(mut stored) => {
                if stored.state == StoredJobState::Running {
                    stored.state = StoredJobState::Failed;
                    stored.message = INTERRUPTED_MESSAGE.to_string();
                }
                match jobs
                    .iter()
                    .position(|job| job.config.id() == stored.config.id())
                {
                    Some(at) => jobs[at] = stored,
                    None => jobs.push(stored),
                }
            }
            Err(e) => {
                skipped += 1;
                report(&format!(
                    "{} line {}: not a job record: {e}",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
    if skipped > 0 {
        report(&format!(
            "skipped {skipped} unreadable lines in {}",
            path.display()
        ));
    }

    jobs.sort_by_key(|job| job.config.id());
    write_all(path, &jobs);
    LoadedJobs { jobs, skipped }
}

/// Replace the file with one line per job.
fn write_all<C: GuiJob>(path: &Path, jobs: &[StoredJob<C>]) {
    let mut text = String::new();
    for job in jobs {
        match serde_json::to_string(job) {
            Ok(line) => {
                text.push_str(&line);
                text.push('\n');
            }
            Err(e) => report(&format!("could not serialize job {}: {e}", job.config.id())),
        }
    }
    if let Err(e) = std::fs::write(path, text) {
        report(&format!("could not rewrite {}: {e}", path.display()));
    }
}

/// The states the Jobs panel prints for a GUI job. The daemon's `batch list`
/// prints the same words, so one row reads the same whichever queue it came from.
const STATUS_RUNNING: &str = "running";
const STATUS_QUEUED: &str = "queued";
const STATUS_DONE: &str = "done";
const STATUS_FAILED: &str = "failed";
const STATUS_CANCELLED: &str = "cancelled";

fn status_of(state: StoredJobState) -> &'static str {
    match state {
        StoredJobState::Queued => STATUS_QUEUED,
        StoredJobState::Running => STATUS_RUNNING,
        StoredJobState::Done => STATUS_DONE,
        StoredJobState::Failed => STATUS_FAILED,
        StoredJobState::Cancelled => STATUS_CANCELLED,
    }
}

/// One row of the Jobs panel.
#[derive(Clone, Serialize)]
pub struct JobInfo {
    pub id: u64,
    pub title: String,
    pub status: String,
    pub percent: f64,
    pub message: String,
}

/// One job that has reached Done, Failed or Cancelled, as the panel lists it.
fn finished_job_info(id: u64, title: String, state: StoredJobState, message: String) -> JobInfo {
    JobInfo {
        id,
        title,
        status: status_of(state).to_string(),
        percent: if state == StoredJobState::Done {
            100.0
        } else {
            0.0
        },
        message,
    }
}

pub struct GuiJobQueue<C> {
    queue: Mutex<VecDeque<C>>,
    next_id: AtomicU64,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    current_id: AtomicU64,
    current_title: Mutex<String>,
    current_status: Mutex<String>,
    /// Output folder of the running job, so a second build cannot write into it
    current_output: Mutex<Option<PathBuf>>,
    /// Jobs that are neither running nor queued any more, oldest first. Read from
    /// the jobs file once at startup and appended to as jobs finish, because
    /// loading the file rewrites it.
    history: Mutex<Vec<JobInfo>>,
    jobs_file: PathBuf,
}

impl<C: GuiJob> GuiJobQueue<C> {
    pub fn new(jobs_file: PathBuf) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            current_id: AtomicU64::new(0),
            current_title: Mutex::new(String::new()),
            current_status: Mutex::new(String::new()),
            current_output: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            jobs_file,
        }
    }

    fn record(&self, state: StoredJobState, message: &str, job: &C) {
        record(&self.jobs_file, state, message, job);
        if state != StoredJobState::Queued && state != StoredJobState::Running {
            self.history.lock().unwrap().push(finished_job_info(
                job.id(),
                job.title().to_string(),
                state,
                message.to_string(),
            ));
        }
    }

    /// The id the next job gets. Taken before the job config is built, so a
    /// build the panel then refuses spends an id and never queues it.
    pub fn reserve_job_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record the job as queued and put it at the back of the queue.
    pub fn submit(&self, job: C) {
        self.record(StoredJobState::Queued, "", &job);
        self.queue.lock().unwrap().push_back(job);
    }

    pub fn has_running_job(&self) -> bool {
        self.current_id.load(Ordering::Relaxed) != 0
    }

    /// Flag the running job, or drop a queued one and record it cancelled.
    /// False when no job has that id.
    pub fn cancel(&self, id: u64) -> bool {
        if self.current_id.load(Ordering::Relaxed) == id {
            self.cancel.store(true, Ordering::Relaxed);
            return true;
        }
        let cancelled = {
            let mut queue = self.queue.lock().unwrap();
            let at = queue.iter().position(|job| job.id() == id);
            at.and_then(|at| queue.remove(at))
        };
        match cancelled {
            Some(job) => {
                self.record(StoredJobState::Cancelled, "", &job);
                true
            }
            None => false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn pause(&self) {
        self.pause.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.pause.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }

    /// The flag the encode polls to stop early.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// The flag the encode polls to wait.
    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        self.pause.clone()
    }

    pub fn take_next(&self) -> Option<C> {
        self.queue.lock().unwrap().pop_front()
    }

    /// Mark the job running and record it, with the cancel and pause flags clear.
    pub fn start(&self, job: &C) {
        self.current_id.store(job.id(), Ordering::Relaxed);
        *self.current_title.lock().unwrap() = job.title().to_string();
        *self.current_output.lock().unwrap() = Some(job.output_dir().to_path_buf());
        *self.current_status.lock().unwrap() = STATUS_RUNNING.to_string();
        self.cancel.store(false, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
        self.record(StoredJobState::Running, "", job);
    }

    /// Record the state the job ended at and put it in the history.
    pub fn finish(&self, job: &C, state: StoredJobState, message: &str) {
        *self.current_status.lock().unwrap() = status_of(state).to_string();
        self.record(state, message, job);
    }

    /// Leave no job running, so the next build starts a worker.
    pub fn clear_current(&self) {
        self.current_id.store(0, Ordering::Relaxed);
        *self.current_output.lock().unwrap() = None;
    }

    /// What the Jobs panel lists: the running job, then the queued ones, then
    /// the finished ones newest first.
    pub fn snapshot(&self) -> Vec<JobInfo> {
        let mut jobs = Vec::new();

        let current_id = self.current_id.load(Ordering::Relaxed);
        let status = self.current_status.lock().unwrap().clone();
        // between a job finishing and the worker picking up the next one the
        // current slot still holds the finished job, which history already has
        if current_id > 0 && status == STATUS_RUNNING {
            jobs.push(JobInfo {
                id: current_id,
                title: self.current_title.lock().unwrap().clone(),
                status,
                percent: 0.0,
                message: String::new(),
            });
        }

        for job in self.queue.lock().unwrap().iter() {
            jobs.push(JobInfo {
                id: job.id(),
                title: job.title().to_string(),
                status: STATUS_QUEUED.to_string(),
                percent: 0.0,
                message: String::new(),
            });
        }

        jobs.extend(self.history.lock().unwrap().iter().rev().cloned());
        jobs
    }

    /// Put the jobs the last run left queued back in the queue and rewrite the
    /// file with one line per job. Nothing is started here: a restored job runs
    /// when the queue worker next runs, as a queued job always has.
    pub fn load_jobs_file(&self) -> usize {
        let loaded = load::<C>(&self.jobs_file);
        let mut queue = self.queue.lock().unwrap();
        let mut history = self.history.lock().unwrap();
        let mut highest_id = 0;
        for stored in loaded.jobs {
            highest_id = highest_id.max(stored.config.id());
            if stored.state == StoredJobState::Queued {
                queue.push_back(stored.config);
                continue;
            }
            history.push(finished_job_info(
                stored.config.id(),
                stored.config.title().to_string(),
                stored.state,
                stored.message,
            ));
        }
        self.next_id.store(highest_id + 1, Ordering::Relaxed);
        loaded.skipped
    }

    /// Is a job already running or queued that writes into `output`?
    pub fn is_building_into(&self, output: &Path) -> bool {
        if self.current_output.lock().unwrap().as_deref() == Some(output) {
            return true;
        }
        self.queue
            .lock()
            .unwrap()
            .iter()
            .any(|job| job.output_dir() == output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Serialize, Deserialize)]
    struct TestJob {
        id: u64,
        title: String,
        output_dir: PathBuf,
        note: String,
    }

    impl GuiJob for TestJob {
        fn id(&self) -> u64 {
            self.id
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn output_dir(&self) -> &Path {
            &self.output_dir
        }
    }

    fn test_job() -> TestJob {
        TestJob {
            id: 1,
            title: "Test".into(),
            output_dir: PathBuf::from("/out"),
            note: String::new(),
        }
    }

    #[test]
    fn a_queued_job_comes_back_and_a_running_one_is_failed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("gui-jobs.jsonl");

        let mut queued = test_job();
        queued.id = 4;
        queued.title = "Restored".into();
        queued.note = "carried through the file".into();

        let mut interrupted = test_job();
        interrupted.id = 5;
        record(&path, StoredJobState::Queued, "", &queued);
        record(&path, StoredJobState::Queued, "", &interrupted);
        record(&path, StoredJobState::Running, "", &interrupted);

        let queue: GuiJobQueue<TestJob> = GuiJobQueue::new(path.clone());
        assert_eq!(queue.load_jobs_file(), 0);

        let restored = queue.take_next().unwrap();
        assert_eq!(restored.id, 4);
        assert_eq!(restored.title, "Restored");
        assert_eq!(restored.note, "carried through the file");
        assert!(queue.take_next().is_none());
        // a new build must not reuse a restored job's id
        assert_eq!(queue.reserve_job_id(), 6);

        let saved = load::<TestJob>(&path);
        assert_eq!(saved.jobs.len(), 2);
        let failed = saved.jobs.iter().find(|job| job.config.id == 5).unwrap();
        assert_eq!(failed.state, StoredJobState::Failed);
        assert_eq!(failed.message, INTERRUPTED_MESSAGE);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn finished_jobs_from_the_last_run_are_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("gui-jobs.jsonl");

        let mut interrupted = test_job();
        interrupted.id = 7;
        interrupted.title = "Interrupted".into();
        let mut finished = test_job();
        finished.id = 8;
        finished.title = "Finished".into();
        record(&path, StoredJobState::Running, "", &interrupted);
        record(&path, StoredJobState::Done, "", &finished);

        let queue: GuiJobQueue<TestJob> = GuiJobQueue::new(path);
        assert_eq!(queue.load_jobs_file(), 0);

        let listed = queue.snapshot();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|job| job.status != "queued"));

        assert_eq!(listed[0].id, 8);
        assert_eq!(listed[0].title, "Finished");
        assert_eq!(listed[0].status, "done");
        assert_eq!(listed[0].percent, 100.0);
        assert_eq!(listed[0].message, "");

        assert_eq!(listed[1].id, 7);
        assert_eq!(listed[1].title, "Interrupted");
        assert_eq!(listed[1].status, "failed");
        assert_eq!(listed[1].message, INTERRUPTED_MESSAGE);
    }

    #[test]
    fn a_second_build_into_the_same_folder_is_refused() {
        // clicking Build twice must not queue a second job into the first
        // job's folder.
        let dir = tempfile::tempdir().unwrap();
        let queue: GuiJobQueue<TestJob> = GuiJobQueue::new(dir.path().join("gui-jobs.jsonl"));
        let output = PathBuf::from("/out");
        assert!(!queue.is_building_into(&output));

        queue.submit(test_job());
        assert!(queue.is_building_into(&output));
        assert!(!queue.is_building_into(&PathBuf::from("/other")));

        let job = queue.take_next().unwrap();
        assert!(!queue.is_building_into(&output));

        queue.start(&job);
        assert!(queue.is_building_into(&output));

        queue.finish(&job, StoredJobState::Done, "");
        queue.clear_current();
        assert!(!queue.is_building_into(&output));
    }

    #[test]
    fn the_environment_variable_wins_over_the_data_dir() {
        const VARIABLE: &str = "POSTKIT_GUI_JOB_QUEUE_TEST_FILE";
        let data_dir = PathBuf::from("/data/wizard");
        assert_eq!(
            jobs_path(VARIABLE, data_dir.clone()),
            data_dir.join("gui-jobs.jsonl")
        );

        unsafe { std::env::set_var(VARIABLE, "/elsewhere/jobs.jsonl") };
        assert_eq!(
            jobs_path(VARIABLE, data_dir.clone()),
            PathBuf::from("/elsewhere/jobs.jsonl")
        );

        // an empty value is not a path
        unsafe { std::env::set_var(VARIABLE, "") };
        assert_eq!(
            jobs_path(VARIABLE, data_dir.clone()),
            data_dir.join("gui-jobs.jsonl")
        );
        unsafe { std::env::remove_var(VARIABLE) };
    }
}
