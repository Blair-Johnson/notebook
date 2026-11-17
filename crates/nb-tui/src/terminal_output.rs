//! Non-blocking terminal and kitty-graphics output pipeline.
//!
//! `TerminalWriter` decouples the UI event loop from `stdout` backpressure and
//! interleaves kitty image uploads safely at escape-sequence boundaries on a
//! dedicated `nb-tty-out` thread:
//! - Every `Terminal::draw` buffers its cell diffs and cursor updates into one
//!   synchronized update (`CSI ? 2026 h` … `CSI ? 2026 l`) in memory and
//!   enqueues it without blocking on PTY drains.
//! - UI frames and mode escapes always preempt image packets between ~4 KB
//!   chunks so typing and navigation never wait behind plot uploads.
//! - Active scrolling holds new image uploads until the viewport settles and
//!   aborts any partial multi-chunk upload that scrolls off screen.
//! - Inside tmux over SSH, `TmuxPacer` paces passthrough bursts below tmux's
//!   `TTY_BLOCK_START` discard threshold so WAN links never drop image chunks.

use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    io::{self, Write},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ratatui::{CompletedFrame, Frame, Terminal, backend::CrosstermBackend};

use crate::images;

thread_local! {
    static ACTIVE_STDOUT_WRITER: RefCell<Option<TerminalWriter>> = const { RefCell::new(None) };
}

/// Enqueue an escape sequence (such as OSC 52 or cursor shape/mouse mode) on
/// the active `TerminalWriter` if one is running on this thread, returning
/// `true` when queued.
pub fn write_escape_via_active_writer(sequence: &[u8]) -> bool {
    ACTIVE_STDOUT_WRITER.with(|slot| {
        let Some(writer) = slot.borrow().clone() else {
            return false;
        };
        writer.enqueue_raw(sequence.to_vec());
        true
    })
}

/// Flush and join the active `TerminalWriter` on this thread if one is running.
pub fn shutdown_active_writer() {
    let writer = ACTIVE_STDOUT_WRITER.with(|slot| slot.borrow_mut().take());
    if let Some(writer) = writer {
        writer.shutdown();
    }
}

pub struct AppTerminal {
    terminal: Terminal<CrosstermBackend<TerminalWriter>>,
    writer: TerminalWriter,
}

impl AppTerminal {
    pub fn spawn_stdout() -> io::Result<Self> {
        images::enable_terminal_probing();
        let writer = TerminalWriter::spawn_stdout();
        let terminal = Terminal::new(CrosstermBackend::new(writer.clone()))?;
        Ok(Self { terminal, writer })
    }

    pub fn draw(
        &mut self,
        render_callback: impl FnOnce(&mut Frame<'_>),
    ) -> io::Result<CompletedFrame<'_>> {
        self.terminal.draw(render_callback)
    }

    pub fn writer(&self) -> &TerminalWriter {
        &self.writer
    }

    pub fn shutdown(&self) {
        self.writer.shutdown();
    }
}

#[cfg(not(test))]
const SCROLL_SETTLE_DELAY: Duration = Duration::from_millis(160);
#[cfg(test)]
const SCROLL_SETTLE_DELAY: Duration = Duration::from_millis(40);

const MAX_PENDING_FRAMES: usize = 4;
const TMUX_POST_FRAME_SETTLE: Duration = Duration::from_millis(4);
const TMUX_BURST_BYTES: f64 = 8_192.0;
const TMUX_BYTES_PER_SEC: f64 = 512.0 * 1024.0;
const BEGIN_SYNC_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNC_UPDATE: &[u8] = b"\x1b[?2026l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const PARK_CURSOR_HOME: &[u8] = b"\x1b[H";

#[derive(Default)]
struct Queue {
    pending_frames: VecDeque<Vec<u8>>,
    desired_visible: Vec<u32>,
    images_enabled: bool,
    had_images_enabled: bool,
    tmux: bool,
    clear_sent: bool,
    hold_until: Option<Instant>,
    shutdown: bool,
}

struct SharedOutput {
    queue: Mutex<Queue>,
    work_cv: Condvar,
    space_cv: Condvar,
    in_flight_frames: AtomicUsize,
    sent_ids: RwLock<HashSet<u32>>,
    newly_sent_dirty: AtomicBool,
}

#[derive(Clone)]
pub struct TerminalWriter {
    frame_buf: Rc<RefCell<Vec<u8>>>,
    shared: Arc<SharedOutput>,
    worker: Rc<RefCell<Option<JoinHandle<()>>>>,
}

impl TerminalWriter {
    pub fn spawn_stdout() -> Self {
        let writer = Self::spawn_with_sink(|bytes| {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(bytes);
            let _ = stdout.flush();
        });
        ACTIVE_STDOUT_WRITER.with(|slot| {
            *slot.borrow_mut() = Some(writer.clone());
        });
        writer
    }

    pub fn spawn_with_sink(mut sink: impl FnMut(&[u8]) + Send + 'static) -> Self {
        let shared = Arc::new(SharedOutput {
            queue: Mutex::new(Queue::default()),
            work_cv: Condvar::new(),
            space_cv: Condvar::new(),
            in_flight_frames: AtomicUsize::new(0),
            sent_ids: RwLock::new(HashSet::new()),
            newly_sent_dirty: AtomicBool::new(false),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("nb-tty-out".to_string())
            .spawn(move || run_output_worker(worker_shared, &mut sink))
            .expect("spawn terminal output worker");
        Self {
            frame_buf: Rc::new(RefCell::new(Vec::with_capacity(64 * 1024))),
            shared,
            worker: Rc::new(RefCell::new(Some(worker))),
        }
    }

    pub fn commit_frame(
        &self,
        visible_images: Vec<u32>,
        images_enabled: bool,
        tmux: bool,
        hold_images: bool,
    ) {
        let frame = {
            let mut buf = self.frame_buf.borrow_mut();
            if buf.is_empty() {
                None
            } else {
                let park = buf.ends_with(HIDE_CURSOR);
                let mut framed = Vec::with_capacity(
                    BEGIN_SYNC_UPDATE.len()
                        + buf.len()
                        + if park { PARK_CURSOR_HOME.len() } else { 0 }
                        + END_SYNC_UPDATE.len(),
                );
                framed.extend_from_slice(BEGIN_SYNC_UPDATE);
                framed.extend_from_slice(&buf);
                if park {
                    framed.extend_from_slice(PARK_CURSOR_HOME);
                }
                framed.extend_from_slice(END_SYNC_UPDATE);
                buf.clear();
                Some(framed)
            }
        };

        let mut q = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while frame.is_some() && q.pending_frames.len() >= MAX_PENDING_FRAMES && !q.shutdown {
            q = self
                .shared
                .space_cv
                .wait(q)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        if let Some(frame) = frame {
            let _ = self.shared.in_flight_frames.fetch_add(1, Ordering::AcqRel);
            q.pending_frames.push_back(frame);
        }
        if !images_enabled && q.had_images_enabled {
            q.clear_sent = true;
        }
        q.had_images_enabled = images_enabled;
        q.images_enabled = images_enabled;
        q.tmux = tmux;
        q.desired_visible = visible_images;
        if hold_images {
            q.hold_until = Some(Instant::now() + SCROLL_SETTLE_DELAY);
        }
        self.shared.work_cv.notify_one();
    }

    pub fn is_busy(&self) -> bool {
        self.shared.in_flight_frames.load(Ordering::Acquire) > 0
    }

    pub fn is_image_sent(&self, id: u32) -> bool {
        self.shared
            .sent_ids
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&id)
    }

    pub fn has_newly_sent(&self) -> bool {
        self.shared.newly_sent_dirty.load(Ordering::Acquire)
    }

    pub fn take_newly_sent(&self) -> bool {
        self.shared.newly_sent_dirty.swap(false, Ordering::AcqRel)
    }

    pub fn hold_images(&self) {
        let mut q = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        q.hold_until = Some(Instant::now() + SCROLL_SETTLE_DELAY);
        self.shared.work_cv.notify_one();
    }

    pub fn wake(&self) {
        self.shared.work_cv.notify_one();
    }

    pub fn enqueue_raw(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let mut q = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if q.shutdown {
            return;
        }
        let _ = self.shared.in_flight_frames.fetch_add(1, Ordering::AcqRel);
        q.pending_frames.push_back(bytes);
        self.shared.work_cv.notify_one();
    }

    pub fn shutdown(&self) {
        ACTIVE_STDOUT_WRITER.with(|slot| {
            if slot
                .borrow()
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.shared, &self.shared))
            {
                *slot.borrow_mut() = None;
            }
        });
        let leftover = {
            let mut buf = self.frame_buf.borrow_mut();
            (!buf.is_empty()).then(|| std::mem::take(&mut *buf))
        };
        {
            let mut q = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(leftover) = leftover {
                let _ = self.shared.in_flight_frames.fetch_add(1, Ordering::AcqRel);
                q.pending_frames.push_back(leftover);
            }
            q.shutdown = true;
            self.shared.work_cv.notify_one();
        }
        if let Some(worker) = self.worker.borrow_mut().take() {
            let _ = worker.join();
        }
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.frame_buf.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        if Rc::strong_count(&self.worker) == 1 {
            self.shutdown();
        }
    }
}

enum OutputStep {
    WriteFrame(Vec<u8>),
    WriteImage {
        bytes: Vec<u8>,
        completed_id: Option<u32>,
    },
    Shutdown(Vec<u8>),
}

struct TmuxPacer {
    tokens: f64,
    last_refill: Instant,
    post_frame_until: Option<Instant>,
}

impl TmuxPacer {
    fn new() -> Self {
        Self {
            tokens: TMUX_BURST_BYTES,
            last_refill: Instant::now(),
            post_frame_until: None,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * TMUX_BYTES_PER_SEC).min(TMUX_BURST_BYTES);
    }

    fn note_frame_written(&mut self, bytes: usize) {
        let now = Instant::now();
        self.refill(now);
        self.tokens = (self.tokens - (bytes.min(6_144) as f64)).max(0.0);
        self.post_frame_until = Some(now + TMUX_POST_FRAME_SETTLE);
    }

    fn delay_for_packet(&mut self, packet_bytes: usize) -> Option<Duration> {
        let now = Instant::now();
        self.refill(now);
        if let Some(until) = self.post_frame_until {
            if now < until {
                return Some(until.saturating_duration_since(now));
            }
            self.post_frame_until = None;
        }
        let needed = (packet_bytes as f64).min(TMUX_BURST_BYTES);
        if self.tokens >= needed {
            self.tokens -= needed;
            None
        } else {
            let deficit = needed - self.tokens;
            Some(
                Duration::from_secs_f64(deficit / TMUX_BYTES_PER_SEC)
                    .max(Duration::from_millis(1)),
            )
        }
    }
}

fn run_output_worker(shared: Arc<SharedOutput>, sink: &mut dyn FnMut(&[u8])) {
    let mut sent: HashSet<u32> = HashSet::new();
    let mut in_flight: Option<(u32, usize)> = None;
    let mut seen_client_epoch = images::client_epoch();
    let mut tmux_pacer = TmuxPacer::new();
    let mut active_tmux;

    loop {
        let step = {
            let mut q = shared
                .queue
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            active_tmux = q.tmux;
            loop {
                if let Some(frame) = q.pending_frames.pop_front() {
                    active_tmux = q.tmux;
                    shared.space_cv.notify_one();
                    break OutputStep::WriteFrame(frame);
                }

                if q.shutdown {
                    let cleanup = drain_cleanup(&mut in_flight, &mut sent, q.tmux);
                    break OutputStep::Shutdown(cleanup);
                }

                if q.clear_sent {
                    q.clear_sent = false;
                    shared
                        .sent_ids
                        .write()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .clear();
                    let cleanup = drain_cleanup(&mut in_flight, &mut sent, q.tmux);
                    if !cleanup.is_empty() {
                        break OutputStep::WriteImage {
                            bytes: cleanup,
                            completed_id: None,
                        };
                    }
                }

                let epoch = images::client_epoch();
                if epoch != seen_client_epoch {
                    seen_client_epoch = epoch;
                    sent.clear();
                    shared
                        .sent_ids
                        .write()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .clear();
                    in_flight = None;
                }

                if !q.images_enabled {
                    q = shared
                        .work_cv
                        .wait(q)
                        .unwrap_or_else(|poison| poison.into_inner());
                    continue;
                }

                if let Some((partial_id, _)) = in_flight {
                    let holding = q.hold_until.is_some_and(|until| Instant::now() < until);
                    if holding || !q.desired_visible.contains(&partial_id) {
                        in_flight = None;
                        let mut abort = Vec::with_capacity(96);
                        abort.extend_from_slice(HIDE_CURSOR);
                        images::append_abort_packet(&mut abort, partial_id, q.tmux);
                        break OutputStep::WriteImage {
                            bytes: abort,
                            completed_id: None,
                        };
                    }
                }

                if let Some(until) = q.hold_until {
                    let now = Instant::now();
                    if now < until {
                        let (next_q, _) = shared
                            .work_cv
                            .wait_timeout(q, until.saturating_duration_since(now))
                            .unwrap_or_else(|poison| poison.into_inner());
                        q = next_q;
                        continue;
                    }
                    q.hold_until = None;
                }

                let next_id = in_flight.map(|(id, _)| id).or_else(|| {
                    q.desired_visible
                        .iter()
                        .copied()
                        .filter(|id| !sent.contains(id))
                        .filter_map(|id| {
                            images::packets_for(id).map(|packets| (id, packets.len()))
                        })
                        .min_by_key(|(_, count)| *count)
                        .map(|(id, _)| id)
                });

                let Some(id) = next_id else {
                    q = shared
                        .work_cv
                        .wait(q)
                        .unwrap_or_else(|poison| poison.into_inner());
                    continue;
                };

                if q.tmux && !images::pane_visible() {
                    let (next_q, _) = shared
                        .work_cv
                        .wait_timeout(q, Duration::from_millis(250))
                        .unwrap_or_else(|poison| poison.into_inner());
                    q = next_q;
                    continue;
                }

                let Some(packets) = images::packets_for(id) else {
                    in_flight = None;
                    continue;
                };
                let index = in_flight.map_or(0, |(_, idx)| idx);
                if index >= packets.len() {
                    in_flight = None;
                    let _ = sent.insert(id);
                    continue;
                }
                let raw = &packets[index];
                let packet_len = raw.len() + if index == 0 { HIDE_CURSOR.len() } else { 0 };
                if q.tmux
                    && let Some(delay) = tmux_pacer.delay_for_packet(packet_len)
                {
                    let (next_q, _) = shared
                        .work_cv
                        .wait_timeout(q, delay)
                        .unwrap_or_else(|poison| poison.into_inner());
                    q = next_q;
                    continue;
                }
                let mut packet = Vec::with_capacity(packet_len);
                if index == 0 {
                    packet.extend_from_slice(HIDE_CURSOR);
                }
                packet.extend_from_slice(raw);
                let completed_id = if index + 1 >= packets.len() {
                    in_flight = None;
                    let _ = sent.insert(id);
                    Some(id)
                } else {
                    in_flight = Some((id, index + 1));
                    None
                };
                break OutputStep::WriteImage {
                    bytes: packet,
                    completed_id,
                };
            }
        };

        match step {
            OutputStep::WriteFrame(bytes) => {
                if !bytes.is_empty() {
                    sink(&bytes);
                    if active_tmux {
                        tmux_pacer.note_frame_written(bytes.len());
                    }
                }
                let _ = shared.in_flight_frames.fetch_sub(1, Ordering::AcqRel);
            }
            OutputStep::WriteImage {
                bytes,
                completed_id,
            } => {
                if !bytes.is_empty() {
                    sink(&bytes);
                }
                if let Some(id) = completed_id {
                    let _ = shared
                        .sent_ids
                        .write()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(id);
                    let q = shared
                        .queue
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let any_single_packet_remaining = q.desired_visible.iter().any(|candidate| {
                        !sent.contains(candidate)
                            && images::packets_for(*candidate)
                                .is_some_and(|packets| packets.len() <= 1)
                    });
                    if !any_single_packet_remaining {
                        shared.newly_sent_dirty.store(true, Ordering::Release);
                    }
                }
            }
            OutputStep::Shutdown(cleanup) => {
                if !cleanup.is_empty() {
                    sink(&cleanup);
                }
                shared
                    .sent_ids
                    .write()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clear();
                shared.in_flight_frames.store(0, Ordering::Release);
                break;
            }
        }
    }
}

fn drain_cleanup(
    in_flight: &mut Option<(u32, usize)>,
    sent: &mut HashSet<u32>,
    tmux: bool,
) -> Vec<u8> {
    let mut cleanup = Vec::new();
    if let Some((partial_id, _)) = in_flight.take() {
        images::append_abort_packet(&mut cleanup, partial_id, tmux);
    }
    for id in sent.drain() {
        images::append_delete_packet(&mut cleanup, id, tmux);
    }
    cleanup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_holds_transient_images_and_deletes_settled_uploads_on_shutdown() {
        images::enable_for_test((10, 20));
        let transient =
            images::place_image(&images::PreparedImage::from_png(&vec![1; 9000], 4, 2)).unwrap();
        let settled =
            images::place_image(&images::PreparedImage::from_png(&vec![2; 64], 2, 1)).unwrap();

        let recorded: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&recorded);
        let mut writer = TerminalWriter::spawn_with_sink(move |bytes| {
            sink_log.lock().unwrap().push(bytes.to_vec());
        });

        writer.write_all(b"frame-1").unwrap();
        writer.commit_frame(vec![transient], true, false, true);
        writer.write_all(b"frame-2").unwrap();
        writer.commit_frame(vec![settled], true, false, false);

        thread::sleep(SCROLL_SETTLE_DELAY + Duration::from_millis(40));
        writer.shutdown();

        let writes = recorded.lock().unwrap();
        let combined = String::from_utf8_lossy(&writes.concat()).into_owned();
        assert!(combined.contains("frame-1"));
        assert!(combined.contains("frame-2"));
        assert!(!combined.contains(&format!("a=T,U=1,f=100,q=2,i={transient},")));
        assert!(combined.contains(&format!("a=T,U=1,f=100,q=2,i={settled},")));
        assert!(combined.contains(&format!("a=d,d=I,i={settled},q=2")));
    }

    #[test]
    fn tmux_pacer_caps_bursts_and_enforces_post_frame_settle() {
        let mut pacer = TmuxPacer::new();
        assert!(pacer.delay_for_packet(3_120).is_none());
        assert!(pacer.delay_for_packet(3_120).is_none());
        assert!(pacer.delay_for_packet(3_120).is_some());

        let mut pacer = TmuxPacer::new();
        pacer.note_frame_written(1_024);
        assert!(pacer.delay_for_packet(512).is_some());
    }
}
