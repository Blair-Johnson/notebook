//! Background `TMUX_PANE` title updater that restores the original title on exit.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

const MAX_TITLE_CHARS: usize = 80;

#[derive(Default)]
pub struct TmuxPaneTitle {
    pane_id: Option<String>,
    original_title: Option<String>,
    command_tx: Option<Sender<TmuxCommand>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum TmuxCommand {
    Set(String),
    Shutdown,
}

impl TmuxPaneTitle {
    pub fn detect() -> Self {
        if std::env::var_os("TMUX").is_none() {
            return Self::default();
        }
        let Some(pane_id) = std::env::var("TMUX_PANE")
            .ok()
            .map(|pane| pane.trim().to_string())
            .filter(|pane| !pane.is_empty())
        else {
            return Self::default();
        };
        let original_title = pane_title(&pane_id).unwrap_or_default();
        let (command_tx, worker) = spawn_worker(pane_id.clone(), original_title.clone());
        Self {
            pane_id: Some(pane_id),
            original_title: Some(original_title),
            command_tx: Some(command_tx),
            worker: Some(worker),
        }
    }

    pub fn update(&mut self, title: &str) {
        let Some(pane_id) = self.pane_id.clone() else {
            return;
        };
        let title = sanitize_title(title);
        if self.command_tx.is_none() {
            let original = self.original_title.clone().unwrap_or_default();
            let (command_tx, worker) = spawn_worker(pane_id, original);
            self.command_tx = Some(command_tx);
            self.worker = Some(worker);
        }
        if let Some(command_tx) = self.command_tx.as_ref() {
            let _ = command_tx.send(TmuxCommand::Set(title));
        }
    }

    pub fn pane_id(&self) -> Option<&str> {
        self.pane_id.as_deref()
    }
}

impl Drop for TmuxPaneTitle {
    fn drop(&mut self) {
        if let Some(command_tx) = self.command_tx.take() {
            let _ = command_tx.send(TmuxCommand::Shutdown);
        } else if let (Some(pane_id), Some(original)) =
            (self.pane_id.as_deref(), self.original_title.as_deref())
        {
            set_pane_title(pane_id, original);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_worker(pane_id: String, original_title: String) -> (Sender<TmuxCommand>, JoinHandle<()>) {
    let (command_tx, command_rx) = mpsc::channel();
    let worker = thread::spawn(move || run_worker(pane_id, original_title, command_rx));
    (command_tx, worker)
}

fn run_worker(pane_id: String, original_title: String, command_rx: Receiver<TmuxCommand>) {
    while let Ok(command) = command_rx.recv() {
        match command {
            TmuxCommand::Set(title) => {
                let mut latest = title;
                while let Ok(next) = command_rx.try_recv() {
                    match next {
                        TmuxCommand::Set(next_title) => latest = next_title,
                        TmuxCommand::Shutdown => {
                            set_pane_title(&pane_id, &original_title);
                            return;
                        }
                    }
                }
                set_pane_title(&pane_id, &latest);
            }
            TmuxCommand::Shutdown => {
                set_pane_title(&pane_id, &original_title);
                return;
            }
        }
    }
    set_pane_title(&pane_id, &original_title);
}

fn pane_title(pane_id: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", pane_id, "#{pane_title}"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string()
    })
}

fn set_pane_title(pane_id: &str, title: &str) {
    let _ = Command::new("tmux")
        .args(["select-pane", "-t", pane_id, "-T", title])
        .status();
}

fn sanitize_title(title: &str) -> String {
    let mut sanitized = String::new();
    for ch in title.trim().chars() {
        if ch.is_control() {
            if matches!(ch, '\n' | '\r' | '\t') && !sanitized.ends_with(' ') {
                sanitized.push(' ');
            }
            continue;
        }
        sanitized.push(ch);
        if sanitized.chars().count() >= MAX_TITLE_CHARS {
            break;
        }
    }
    if sanitized.trim().is_empty() {
        "workbench".to_string()
    } else {
        sanitized.trim().to_string()
    }
}
