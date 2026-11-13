//! The notebook file's modification time, watched from a background thread:
//! `stat` on a slow or unreachable file system must not block the UI.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Watched {
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
    stopped: bool,
}

pub struct MtimeWatch {
    shared: Arc<Mutex<Watched>>,
}

impl MtimeWatch {
    pub fn spawn(path: Option<PathBuf>) -> Self {
        let shared = Arc::new(Mutex::new(Watched {
            path,
            ..Watched::default()
        }));
        let watched = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name("nb-mtime-watch".into())
            .spawn(move || {
                loop {
                    let path = {
                        let state = watched.lock().unwrap_or_else(|e| e.into_inner());
                        if state.stopped {
                            return;
                        }
                        state.path.clone()
                    };
                    let mtime = path.as_deref().and_then(mtime_of);
                    let mut state = watched.lock().unwrap_or_else(|e| e.into_inner());
                    // Saved as another file meanwhile: this result is stale.
                    if state.path == path {
                        state.mtime = mtime;
                    }
                    drop(state);
                    thread::sleep(POLL);
                }
            });
        Self { shared }
    }

    /// The last modification time seen for `path` (at most `POLL` old), if
    /// any. Asking about another path starts watching that one instead.
    pub fn latest(&self, path: &Path) -> Option<SystemTime> {
        let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if state.path.as_deref() != Some(path) {
            state.path = Some(path.to_path_buf());
            state.mtime = None;
        }
        state.mtime
    }
}

impl Drop for MtimeWatch {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped = true;
    }
}

pub fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}
