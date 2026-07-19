use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Watch event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WatchEventKind {
    Created,
    Modified,
    Removed,
}

/// A file system watch event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEvent {
    pub kind: WatchEventKind,
    pub paths: Vec<PathBuf>,
}

/// File system watcher.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<Result<Event, notify::Error>>,
}

impl FileWatcher {
    /// Create a new watcher on the given directory.
    pub fn new(dir: &Path) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default(),
        )?;
        watcher.watch(dir, RecursiveMode::Recursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Poll for the next event (blocking).
    pub fn next_event(&self) -> Option<WatchEvent> {
        match self.rx.recv() {
            Ok(Ok(event)) => {
                let kind = match event.kind {
                    notify::EventKind::Create(_) => WatchEventKind::Created,
                    notify::EventKind::Modify(_) => WatchEventKind::Modified,
                    notify::EventKind::Remove(_) => WatchEventKind::Removed,
                    _ => return None,
                };
                Some(WatchEvent {
                    kind,
                    paths: event.paths,
                })
            }
            _ => None,
        }
    }

    /// Poll with timeout (non-blocking).
    pub fn try_next_event(&self, timeout: std::time::Duration) -> Option<WatchEvent> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                let kind = match event.kind {
                    notify::EventKind::Create(_) => WatchEventKind::Created,
                    notify::EventKind::Modify(_) => WatchEventKind::Modified,
                    notify::EventKind::Remove(_) => WatchEventKind::Removed,
                    _ => return None,
                };
                Some(WatchEvent {
                    kind,
                    paths: event.paths,
                })
            }
            _ => None,
        }
    }
}
