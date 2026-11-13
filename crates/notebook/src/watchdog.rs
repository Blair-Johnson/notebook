//! Stall watchdog for the UI loop.
//!
//! The loop marks which phase it is in (handling input, a kernel batch, a
//! tick, autosave, drawing) and when it started. A background thread checks
//! a few times a second; when one phase runs past `STALL_AFTER` it appends a
//! line to `stalls.log` in the state directory and, on macOS, saves a stack
//! sample of the process next to it. Nothing here runs on the UI thread
//! beyond two atomic stores per phase.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STALL_AFTER: Duration = Duration::from_millis(1000);
const SAMPLE_AFTER: Duration = Duration::from_millis(2000);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    Waiting = 0,
    Input = 1,
    Kernel = 2,
    Tick = 3,
    Autosave = 4,
    Draw = 5,
    Editor = 6,
}

impl Phase {
    fn name(code: u8) -> &'static str {
        match code {
            1 => "handling input",
            2 => "handling kernel events",
            3 => "tick",
            4 => "autosave",
            5 => "drawing (writing to the terminal)",
            6 => "external editor",
            _ => "waiting",
        }
    }
}

pub struct Watchdog {
    phase: AtomicU8,
    since_ms: AtomicU64,
    epoch: Instant,
}

impl Watchdog {
    pub fn spawn() -> Arc<Self> {
        let dog = Arc::new(Self {
            phase: AtomicU8::new(Phase::Waiting as u8),
            since_ms: AtomicU64::new(0),
            epoch: Instant::now(),
        });
        let watched = dog.clone();
        let _ = thread::Builder::new()
            .name("nb-watchdog".into())
            .spawn(move || watched.watch());
        dog
    }

    pub fn enter(&self, phase: Phase) {
        self.since_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.phase.store(phase as u8, Ordering::Release);
    }

    fn watch(&self) {
        let mut reported: Option<(u8, u64)> = None;
        let mut sampled = false;
        loop {
            thread::sleep(Duration::from_millis(250));
            let phase = self.phase.load(Ordering::Acquire);
            let since = self.since_ms.load(Ordering::Relaxed);
            let stuck = Duration::from_millis(
                (self.epoch.elapsed().as_millis() as u64).saturating_sub(since),
            );
            let waiting = phase == Phase::Waiting as u8 || phase == Phase::Editor as u8;
            if waiting || stuck < STALL_AFTER {
                if let Some((phase, since)) = reported.take() {
                    let lasted = (self.epoch.elapsed().as_millis() as u64).saturating_sub(since);
                    log_line(&format!(
                        "stall ended: {} took {lasted} ms",
                        Phase::name(phase)
                    ));
                }
                sampled = false;
                continue;
            }
            if reported != Some((phase, since)) {
                reported = Some((phase, since));
                sampled = false;
                log_line(&format!(
                    "stall: {} for over {} ms",
                    Phase::name(phase),
                    stuck.as_millis()
                ));
            }
            if !sampled && stuck >= SAMPLE_AFTER {
                sampled = true;
                sample_stack();
            }
        }
    }
}

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("NB_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .filter(|v| !v.is_empty())
                .map(|p| PathBuf::from(p).join("notebook"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/notebook"))
        })
}

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn log_line(line: &str) {
    let Some(dir) = state_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stalls.log"))
    {
        let _ = writeln!(file, "{} pid {} {line}", stamp(), std::process::id());
    }
}

/// Save a one-second stack sample of this process (macOS `sample`).
fn sample_stack() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some(dir) = state_dir() else { return };
    let path = dir.join(format!("stall-{}-{}.txt", stamp(), std::process::id()));
    let spawned = Command::new("/usr/bin/sample")
        .arg(std::process::id().to_string())
        .arg("1")
        .arg("-file")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        // Reap it from this thread; the UI thread never waits on it.
        let _ = child.wait();
        log_line(&format!("stack sample saved to {}", path.display()));
    }
}
