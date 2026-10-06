//! The job queue every wizard process runs: both wizard GUIs, the dcpwizard job
//! daemon and the imfwizard REST server. One JSON line per job is appended to a
//! jobs file when it is queued and on every state change, so a stopped process
//! does not lose what was queued. The last record for an id is the job.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a job left running when the program stopped reports on the next start.
pub const INTERRUPTED_MESSAGE: &str = "the program stopped while this job was running";

const STOP_FOR_EXIT_WAIT: Duration = Duration::from_secs(30);

/// What the queue reads from a wizard's job config to list it and record it.
pub trait QueueJob: Serialize + DeserializeOwned + Clone {
    fn id(&self) -> u64;
    fn title(&self) -> &str;
    fn output_dir(&self) -> Option<&Path>;
}

/// Where a process keeps its jobs file: `default_path`, unless
/// `environment_variable` points a second app, or a test, at a file of its own.
pub fn jobs_path(environment_variable: &str, default_path: PathBuf) -> PathBuf {
    match std::env::var(environment_variable) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => default_path,
    }
}

/// The states the queue moves a job through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Running,
    // jobs files written before the rename say Done
    #[serde(alias = "Done")]
    Completed,
    Failed,
    Cancelled,
}

/// One line of the jobs file.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredJob<C> {
    pub state: JobState,
    pub message: String,
    pub config: C,
}

/// Append one record as a JSON line, creating the file and its parent dir.
fn append_record<C: QueueJob>(path: &Path, record: &StoredJob<C>) -> Result<(), String> {
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
pub fn record<C: QueueJob>(path: &Path, state: JobState, message: &str, config: &C) {
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

/// What the jobs file held: the last record per job id, in the order of those
/// last records, with a job left running failed, plus how many lines could not
/// be read.
pub struct LoadedJobs<C> {
    pub jobs: Vec<StoredJob<C>>,
    pub skipped: usize,
}

/// Read the jobs file and rewrite it with one line per job.
pub fn load<C: QueueJob>(path: &Path) -> LoadedJobs<C> {
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
                if stored.state == JobState::Running {
                    stored.state = JobState::Failed;
                    stored.message = INTERRUPTED_MESSAGE.to_string();
                }
                if let Some(at) = jobs
                    .iter()
                    .position(|job| job.config.id() == stored.config.id())
                {
                    jobs.remove(at);
                }
                jobs.push(stored);
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

    write_all(path, &jobs);
    LoadedJobs { jobs, skipped }
}

/// Replace the file with one line per job.
fn write_all<C: QueueJob>(path: &Path, jobs: &[StoredJob<C>]) {
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

/// One row of a jobs list.
#[derive(Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: u64,
    pub title: String,
    pub state: JobState,
    pub percent: f64,
    pub message: String,
}

/// One job that has reached Completed, Failed or Cancelled, as a list shows it.
fn finished_job_info(id: u64, title: String, state: JobState, message: String) -> JobInfo {
    JobInfo {
        id,
        title,
        state,
        percent: if state == JobState::Completed {
            100.0
        } else {
            0.0
        },
        message,
    }
}

pub struct JobQueue<C> {
    queue: Mutex<VecDeque<C>>,
    next_id: AtomicU64,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    current_id: AtomicU64,
    current_title: Mutex<String>,
    current_state: Mutex<JobState>,
    current_percent: Mutex<f64>,
    current_message: Mutex<String>,
    /// Output folder of the running job, so a second build cannot write into it
    current_output: Mutex<Option<PathBuf>>,
    /// Jobs that are neither running nor queued any more, oldest first. Read from
    /// the jobs file once at startup and appended to as jobs finish, because
    /// loading the file rewrites it.
    history: Mutex<Vec<JobInfo>>,
    jobs_file: PathBuf,
    closing: AtomicBool,
    worker_idle: (Mutex<()>, Condvar),
}

impl<C: QueueJob> JobQueue<C> {
    pub fn new(jobs_file: PathBuf) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            current_id: AtomicU64::new(0),
            current_title: Mutex::new(String::new()),
            current_state: Mutex::new(JobState::Queued),
            current_percent: Mutex::new(0.0),
            current_message: Mutex::new(String::new()),
            current_output: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            jobs_file,
            closing: AtomicBool::new(false),
            worker_idle: (Mutex::new(()), Condvar::new()),
        }
    }

    fn record(&self, state: JobState, message: &str, job: &C) {
        record(&self.jobs_file, state, message, job);
    }

    /// Put a finished job in the history, under the lock `snapshot` holds.
    fn record_finished(&self, history: &mut Vec<JobInfo>, state: JobState, message: &str, job: &C) {
        history.push(finished_job_info(
            job.id(),
            job.title().to_string(),
            state,
            message.to_string(),
        ));
    }

    /// The id the next job gets. Taken before the job config is built, so a
    /// build the panel then refuses spends an id and never queues it.
    pub fn reserve_job_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record the job as queued and put it at the back of the queue.
    pub fn submit(&self, job: C) {
        // held across the write so a move cannot record its order ahead of this job
        let _listed = self.history.lock().unwrap();
        self.record(JobState::Queued, "", &job);
        self.queue.lock().unwrap().push_back(job);
    }

    /// Move the queued job `id` so it runs before the queued job `before`, or
    /// next when `before` is None, and record the new order. False when either
    /// is not in the queue.
    pub fn move_before(&self, id: u64, before: Option<u64>) -> bool {
        let _listed = self.history.lock().unwrap();
        let mut queue = self.queue.lock().unwrap();
        let position = |id: u64| queue.iter().position(|job| job.id() == id);
        let Some(from) = position(id) else {
            return false;
        };
        if before == Some(id) {
            return true;
        }
        let Some(to) = before.map_or(Some(0), position) else {
            return false;
        };
        let jobs = queue.make_contiguous();
        if from < to {
            jobs[from..to].rotate_left(1);
        } else {
            jobs[to..=from].rotate_right(1);
        }
        // held across the writes or a job cancelled meanwhile comes back queued after a restart
        for job in queue.iter() {
            self.record(JobState::Queued, "", job);
        }
        true
    }

    pub fn has_running_job(&self) -> bool {
        self.current_id.load(Ordering::Relaxed) != 0
    }

    /// Flag the running job, or drop a queued one and record it cancelled.
    /// False when no job has that id.
    pub fn cancel(&self, id: u64) -> bool {
        let mut history = self.history.lock().unwrap();
        if self.current_id.load(Ordering::Relaxed) == id {
            // the slot still names a finished job until the worker clears it
            let state = *self.current_state.lock().unwrap();
            if state != JobState::Queued && state != JobState::Running {
                return false;
            }
            self.cancel.store(true, Ordering::Relaxed);
            return true;
        }
        let cancelled = {
            let mut queue = self.queue.lock().unwrap();
            let at = queue.iter().position(|job| job.id() == id);
            at.and_then(|at| queue.remove(at))
        };
        let Some(job) = cancelled else {
            return false;
        };
        self.record_finished(&mut history, JobState::Cancelled, "", &job);
        drop(history);
        self.record(JobState::Cancelled, "", &job);
        true
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

    /// Take the front job into the current slot, where it is listed Queued and
    /// can be cancelled until `start` marks it running.
    pub fn take_next(&self) -> Option<C> {
        if self.closing.load(Ordering::Relaxed) {
            return None;
        }
        // the move from the queue to the slot happens under the list lock
        let _listed = self.history.lock().unwrap();
        let job = self.queue.lock().unwrap().pop_front()?;
        self.current_id.store(job.id(), Ordering::Relaxed);
        *self.current_title.lock().unwrap() = job.title().to_string();
        *self.current_output.lock().unwrap() = job.output_dir().map(Path::to_path_buf);
        *self.current_state.lock().unwrap() = JobState::Queued;
        *self.current_percent.lock().unwrap() = 0.0;
        self.current_message.lock().unwrap().clear();
        // a job taken as the app closes starts cancelled
        let closing = self.closing.load(Ordering::Relaxed);
        self.cancel.store(closing, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
        Some(job)
    }

    /// Mark the job running and record it.
    pub fn start(&self, job: &C) {
        *self.current_state.lock().unwrap() = JobState::Running;
        self.record(JobState::Running, "", job);
    }

    /// Record the state the job ended at and put it in the history.
    pub fn finish(&self, job: &C, state: JobState, message: &str) {
        // the history takes the job before the slot stops listing it
        let mut history = self.history.lock().unwrap();
        self.record_finished(&mut history, state, message, job);
        *self.current_state.lock().unwrap() = state;
        drop(history);
        self.record(state, message, job);
    }

    pub fn set_progress(&self, percent: f64, message: &str) {
        *self.current_percent.lock().unwrap() = percent;
        *self.current_message.lock().unwrap() = message.to_string();
    }

    /// Leave no job running, so the next build starts a worker.
    pub fn clear_current(&self) {
        let (lock, idle) = &self.worker_idle;
        let _guard = lock.lock().unwrap();
        self.current_id.store(0, Ordering::Relaxed);
        *self.current_output.lock().unwrap() = None;
        idle.notify_all();
    }

    // exiting with encoder threads still compressing crashes in grok's teardown
    pub fn stop_for_exit(&self) {
        self.closing.store(true, Ordering::Relaxed);
        let mut history = self.history.lock().unwrap();
        let queued: Vec<C> = self.queue.lock().unwrap().drain(..).collect();
        for job in &queued {
            self.record_finished(&mut history, JobState::Cancelled, "", job);
        }
        drop(history);
        for job in &queued {
            self.record(JobState::Cancelled, "", job);
        }
        self.cancel.store(true, Ordering::Relaxed);

        let (lock, idle) = &self.worker_idle;
        let mut guard = lock.lock().unwrap();
        let deadline = Instant::now() + STOP_FOR_EXIT_WAIT;
        while self.has_running_job() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                report(&format!(
                    "the running job did not stop within {} s, exiting anyway",
                    STOP_FOR_EXIT_WAIT.as_secs()
                ));
                return;
            };
            guard = idle.wait_timeout(guard, left).unwrap().0;
        }
    }

    /// Every job as a list shows it: the running job, then the queued ones,
    /// then the finished ones newest first.
    pub fn snapshot(&self) -> Vec<JobInfo> {
        let mut jobs = Vec::new();
        // held while the slot and the queue are read, so a job moving to the
        // history is in exactly one of the three
        let history = self.history.lock().unwrap();

        let current_id = self.current_id.load(Ordering::Relaxed);
        let state = *self.current_state.lock().unwrap();
        // between a job finishing and the worker picking up the next one the
        // current slot still holds the finished job, which history already has
        if current_id > 0 && (state == JobState::Queued || state == JobState::Running) {
            jobs.push(JobInfo {
                id: current_id,
                title: self.current_title.lock().unwrap().clone(),
                state,
                percent: *self.current_percent.lock().unwrap(),
                message: self.current_message.lock().unwrap().clone(),
            });
        }

        for job in self.queue.lock().unwrap().iter() {
            jobs.push(JobInfo {
                id: job.id(),
                title: job.title().to_string(),
                state: JobState::Queued,
                percent: 0.0,
                message: String::new(),
            });
        }

        jobs.extend(history.iter().rev().cloned());
        jobs
    }

    pub fn get(&self, id: u64) -> Option<JobInfo> {
        self.snapshot().into_iter().find(|job| job.id == id)
    }

    /// Put the jobs the last run left queued back in the queue and rewrite the
    /// file with one line per job. Nothing is started here: a restored job runs
    /// when the queue worker next runs, as a queued job always has.
    pub fn load_jobs_file(&self) -> usize {
        let loaded = load::<C>(&self.jobs_file);
        let mut history = self.history.lock().unwrap();
        let mut queue = self.queue.lock().unwrap();
        let mut highest_id = 0;
        for stored in loaded.jobs {
            highest_id = highest_id.max(stored.config.id());
            if stored.state == JobState::Queued {
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
            .any(|job| job.output_dir() == Some(output))
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

    impl QueueJob for TestJob {
        fn id(&self) -> u64 {
            self.id
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn output_dir(&self) -> Option<&Path> {
            Some(&self.output_dir)
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
        record(&path, JobState::Queued, "", &queued);
        record(&path, JobState::Queued, "", &interrupted);
        record(&path, JobState::Running, "", &interrupted);

        let queue: JobQueue<TestJob> = JobQueue::new(path.clone());
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
        assert_eq!(failed.state, JobState::Failed);
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
        record(&path, JobState::Running, "", &interrupted);
        record(&path, JobState::Completed, "", &finished);

        let queue: JobQueue<TestJob> = JobQueue::new(path);
        assert_eq!(queue.load_jobs_file(), 0);

        let listed = queue.snapshot();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|job| job.state != JobState::Queued));

        assert_eq!(listed[0].id, 8);
        assert_eq!(listed[0].title, "Finished");
        assert_eq!(listed[0].state, JobState::Completed);
        assert_eq!(listed[0].percent, 100.0);
        assert_eq!(listed[0].message, "");

        assert_eq!(listed[1].id, 7);
        assert_eq!(listed[1].title, "Interrupted");
        assert_eq!(listed[1].state, JobState::Failed);
        assert_eq!(listed[1].message, INTERRUPTED_MESSAGE);
    }

    #[test]
    fn a_second_build_into_the_same_folder_is_refused() {
        // clicking Build twice must not queue a second job into the first
        // job's folder.
        let dir = tempfile::tempdir().unwrap();
        let queue: JobQueue<TestJob> = JobQueue::new(dir.path().join("gui-jobs.jsonl"));
        let output = PathBuf::from("/out");
        assert!(!queue.is_building_into(&output));

        queue.submit(test_job());
        assert!(queue.is_building_into(&output));
        assert!(!queue.is_building_into(&PathBuf::from("/other")));

        // a taken job still owns its folder
        let job = queue.take_next().unwrap();
        assert!(queue.is_building_into(&output));

        queue.start(&job);
        assert!(queue.is_building_into(&output));

        queue.finish(&job, JobState::Completed, "");
        queue.clear_current();
        assert!(!queue.is_building_into(&output));
    }

    #[test]
    fn the_environment_variable_wins_over_the_default_path() {
        const VARIABLE: &str = "POSTKIT_JOB_QUEUE_TEST_FILE";
        let default_path = PathBuf::from("/data/wizard/gui-jobs.jsonl");
        assert_eq!(jobs_path(VARIABLE, default_path.clone()), default_path);

        unsafe { std::env::set_var(VARIABLE, "/elsewhere/jobs.jsonl") };
        assert_eq!(
            jobs_path(VARIABLE, default_path.clone()),
            PathBuf::from("/elsewhere/jobs.jsonl")
        );

        // an empty value is not a path
        unsafe { std::env::set_var(VARIABLE, "") };
        assert_eq!(jobs_path(VARIABLE, default_path.clone()), default_path);
        unsafe { std::env::remove_var(VARIABLE) };
    }

    fn worker_that_stops_on_cancel(
        queue: Arc<JobQueue<TestJob>>,
        job: TestJob,
    ) -> std::thread::JoinHandle<()> {
        let cancel = queue.cancel_flag();
        std::thread::spawn(move || {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            queue.finish(&job, JobState::Cancelled, "Cancelled");
            assert!(
                queue.take_next().is_none(),
                "the worker was handed a job after the close"
            );
            queue.clear_current();
        })
    }

    #[test]
    fn a_close_cancels_the_running_job_and_the_queued_ones_and_waits_for_the_worker() {
        const RETURN_WITHIN: Duration = Duration::from_secs(5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-jobs.jsonl");
        let queue = Arc::new(JobQueue::new(path.clone()));
        let running = test_job();
        let mut queued = test_job();
        queued.id = 2;
        queue.submit(running);
        queue.submit(queued);
        let job = queue.take_next().unwrap();
        queue.start(&job);
        let worker = worker_that_stops_on_cancel(queue.clone(), job);

        let started = Instant::now();
        queue.stop_for_exit();
        assert!(
            started.elapsed() < RETURN_WITHIN,
            "the close waited on the timeout, not the worker"
        );
        assert!(!queue.has_running_job());
        worker.join().unwrap();

        let loaded = load::<TestJob>(&path);
        let states: Vec<(u64, JobState)> = loaded
            .jobs
            .iter()
            .map(|job| (job.config.id, job.state))
            .collect();
        // the queued job is cancelled before the running one stops
        assert_eq!(
            states,
            vec![(2, JobState::Cancelled), (1, JobState::Cancelled)]
        );
    }

    #[test]
    fn progress_shows_on_the_running_row() {
        let dir = tempfile::tempdir().unwrap();
        let queue: JobQueue<TestJob> = JobQueue::new(dir.path().join("gui-jobs.jsonl"));
        queue.submit(test_job());
        let job = queue.take_next().unwrap();
        queue.start(&job);
        queue.set_progress(42.5, "frame 10 of 24");

        let listed = queue.snapshot();
        assert_eq!(listed[0].state, JobState::Running);
        assert_eq!(listed[0].percent, 42.5);
        assert_eq!(listed[0].message, "frame 10 of 24");

        let running = queue.get(job.id).unwrap();
        assert_eq!(running.state, JobState::Running);
        assert_eq!(running.percent, 42.5);
        assert_eq!(running.message, "frame 10 of 24");

        assert!(queue.get(99).is_none());
    }

    #[test]
    fn a_done_line_loads_as_completed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-jobs.jsonl");
        let line = r#"{"state":"Done","message":"","config":{"id":3,"title":"Old","output_dir":"/out","note":""}}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let loaded = load::<TestJob>(&path);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(loaded.jobs[0].state, JobState::Completed);
    }

    #[test]
    fn a_job_taken_as_the_app_closes_starts_cancelled() {
        const RETURN_WITHIN: Duration = Duration::from_secs(5);
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(JobQueue::new(dir.path().join("gui-jobs.jsonl")));
        queue.submit(test_job());
        let job = queue.take_next().unwrap();
        assert!(!queue.is_cancelled());
        let worker = std::thread::spawn({
            let queue = queue.clone();
            move || {
                while !queue.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                queue.start(&job);
                assert!(queue.is_cancelled());
                queue.finish(&job, JobState::Cancelled, "");
                assert!(queue.take_next().is_none());
                queue.clear_current();
            }
        });

        let started = Instant::now();
        queue.stop_for_exit();
        assert!(
            started.elapsed() < RETURN_WITHIN,
            "the close waited on the timeout, not the worker"
        );
        worker.join().unwrap();
    }

    #[test]
    fn a_job_cancelled_between_take_next_and_start_stays_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let queue = JobQueue::new(dir.path().join("gui-jobs.jsonl"));
        queue.submit(test_job());
        let job = queue.take_next().unwrap();
        let listed = queue.get(job.id).expect("a taken job is still listed");
        assert_eq!(listed.state, JobState::Queued);
        assert!(queue.cancel(job.id));
        queue.start(&job);
        assert!(queue.is_cancelled());
    }

    #[test]
    fn a_finished_job_cannot_be_cancelled_before_the_slot_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let queue = JobQueue::new(dir.path().join("gui-jobs.jsonl"));
        queue.submit(test_job());
        let job = queue.take_next().unwrap();
        queue.start(&job);
        queue.finish(&job, JobState::Completed, "");
        assert!(!queue.cancel(job.id));
        assert!(!queue.is_cancelled());
        assert_eq!(queue.get(job.id).unwrap().state, JobState::Completed);
    }

    fn queue_with_jobs(path: PathBuf, ids: &[u64]) -> JobQueue<TestJob> {
        let queue = JobQueue::new(path);
        for &id in ids {
            let mut job = test_job();
            job.id = id;
            queue.submit(job);
        }
        queue
    }

    fn listed_ids(queue: &JobQueue<TestJob>) -> Vec<u64> {
        queue.snapshot().iter().map(|job| job.id).collect()
    }

    #[test]
    fn a_queued_job_moved_to_the_front_runs_next() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_with_jobs(dir.path().join("gui-jobs.jsonl"), &[1, 2, 3]);
        assert!(queue.move_before(3, None));
        let taken: Vec<u64> = std::iter::from_fn(|| queue.take_next())
            .map(|job| job.id)
            .collect();
        assert_eq!(taken, vec![3, 1, 2]);
    }

    #[test]
    fn a_queued_job_moved_before_another_sits_just_ahead_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_with_jobs(dir.path().join("gui-jobs.jsonl"), &[1, 2, 3, 4]);
        assert!(queue.move_before(4, Some(2)));
        assert_eq!(listed_ids(&queue), vec![1, 4, 2, 3]);
        assert!(queue.move_before(1, Some(3)));
        assert_eq!(listed_ids(&queue), vec![4, 2, 1, 3]);
        assert!(queue.move_before(2, Some(2)));
        assert_eq!(listed_ids(&queue), vec![4, 2, 1, 3]);
    }

    #[test]
    fn a_move_naming_a_job_that_is_not_queued_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_with_jobs(dir.path().join("gui-jobs.jsonl"), &[1, 2, 3]);
        let running = queue.take_next().unwrap();
        queue.start(&running);
        assert!(!queue.move_before(1, None));
        assert!(!queue.move_before(1, Some(3)));
        assert!(!queue.move_before(99, None));
        assert!(!queue.move_before(3, Some(99)));
        assert!(!queue.move_before(3, Some(1)));
        assert_eq!(listed_ids(&queue), vec![1, 2, 3]);
    }

    #[test]
    fn the_moved_order_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-jobs.jsonl");
        let queue = queue_with_jobs(path.clone(), &[1, 2, 3]);
        assert!(queue.move_before(3, Some(1)));
        assert!(queue.move_before(2, Some(1)));
        assert_eq!(listed_ids(&queue), vec![3, 2, 1]);

        let restarted: JobQueue<TestJob> = JobQueue::new(path);
        assert_eq!(restarted.load_jobs_file(), 0);
        assert_eq!(listed_ids(&restarted), vec![3, 2, 1]);
    }

    #[test]
    fn a_job_finishing_after_a_move_is_loaded_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-jobs.jsonl");
        let queue = queue_with_jobs(path.clone(), &[1, 2]);
        assert!(queue.move_before(2, None));
        let job = queue.take_next().unwrap();
        queue.start(&job);
        queue.finish(&job, JobState::Completed, "");

        let loaded = load::<TestJob>(&path);
        let states: Vec<(u64, JobState)> = loaded
            .jobs
            .iter()
            .map(|job| (job.config.id, job.state))
            .collect();
        assert_eq!(
            states,
            vec![(1, JobState::Queued), (2, JobState::Completed)]
        );
    }

    #[test]
    fn a_job_is_listed_at_every_moment_of_its_life() {
        const JOBS: u64 = 200;
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(JobQueue::new(dir.path().join("gui-jobs.jsonl")));
        for id in 1..=JOBS {
            let mut job = test_job();
            job.id = id;
            queue.submit(job);
        }

        let worker = std::thread::spawn({
            let queue = queue.clone();
            move || {
                while let Some(job) = queue.take_next() {
                    queue.start(&job);
                    let state = if job.id % 2 == 0 {
                        JobState::Completed
                    } else {
                        JobState::Failed
                    };
                    queue.finish(&job, state, "ended");
                    queue.clear_current();
                }
            }
        });

        for id in 1..=JOBS {
            loop {
                let job = queue
                    .get(id)
                    .unwrap_or_else(|| panic!("job {id} vanished from the list"));
                if job.state != JobState::Queued && job.state != JobState::Running {
                    assert_eq!(job.message, "ended");
                    break;
                }
            }
        }
        worker.join().unwrap();
        assert_eq!(queue.snapshot().len() as u64, JOBS);
    }

    #[test]
    fn a_cancelled_queued_job_is_listed_at_every_moment() {
        const JOBS: u64 = 200;
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(JobQueue::new(dir.path().join("gui-jobs.jsonl")));
        for id in 1..=JOBS {
            let mut job = test_job();
            job.id = id;
            queue.submit(job);
        }

        let canceller = std::thread::spawn({
            let queue = queue.clone();
            move || {
                for id in 1..=JOBS {
                    assert!(queue.cancel(id));
                }
            }
        });

        for id in 1..=JOBS {
            loop {
                let job = queue
                    .get(id)
                    .unwrap_or_else(|| panic!("job {id} vanished from the list"));
                if job.state == JobState::Cancelled {
                    break;
                }
            }
        }
        canceller.join().unwrap();
        assert_eq!(queue.snapshot().len() as u64, JOBS);
    }
}
