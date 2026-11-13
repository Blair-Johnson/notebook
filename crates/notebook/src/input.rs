//! Terminal input read on its own thread.
//!
//! The terminal's input buffer is small (about 1 KiB on macOS). If input is
//! only read between frames, a frame write that blocks lets the buffer fill
//! with mouse reports; the terminal then blocks writing more input and stops
//! reading our output, and both sides wait forever. Reading here, always,
//! keeps the buffer drained no matter what the main loop is doing.

use crossterm::event::{self, Event};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub struct InputReader {
    pub events: UnboundedReceiver<std::io::Result<Event>>,
    paused: Arc<AtomicBool>,
    idle: Arc<AtomicBool>,
}

impl InputReader {
    pub fn spawn() -> Self {
        let (tx, events) = unbounded_channel();
        let paused = Arc::new(AtomicBool::new(false));
        let idle = Arc::new(AtomicBool::new(false));
        let (thread_paused, thread_idle) = (paused.clone(), idle.clone());
        thread::Builder::new()
            .name("nb-input".into())
            .spawn(move || read_loop(&tx, &thread_paused, &thread_idle))
            .expect("spawn input thread");
        Self {
            events,
            paused,
            idle,
        }
    }

    /// Stop reading so another program (an external editor) gets the keys.
    /// Returns once the thread is no longer inside a read.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        for _ in 0..100 {
            if self.idle.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn resume(&self) {
        self.idle.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
    }
}

fn read_loop(tx: &UnboundedSender<std::io::Result<Event>>, paused: &AtomicBool, idle: &AtomicBool) {
    loop {
        if paused.load(Ordering::SeqCst) {
            idle.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        // A short poll keeps pause requests responsive.
        let ready = match event::poll(Duration::from_millis(50)) {
            Ok(ready) => ready,
            Err(error) => {
                let _ = tx.send(Err(error));
                return;
            }
        };
        if !ready {
            continue;
        }
        let event = event::read();
        let failed = event.is_err();
        if tx.send(event).is_err() || failed {
            return;
        }
    }
}
