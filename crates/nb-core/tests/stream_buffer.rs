//! `StreamBuffer` against a reference terminal model, plus a linear-time check.

use nb_core::StreamBuffer;
use nb_core::output::TRUNCATION_MARKER;
use std::time::{Duration, Instant};

/// Character grid with a persistent cursor: what a terminal shows.
struct Model {
    lines: Vec<Vec<char>>,
    col: usize,
    pending: bool,
    truncated: usize,
    max_lines: usize,
    max_bytes: usize,
}

impl Model {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: vec![Vec::new()],
            col: 0,
            pending: false,
            truncated: 0,
            max_lines,
            max_bytes,
        }
    }

    fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        for ch in chunk.chars() {
            match ch {
                '\n' => {
                    self.lines.push(Vec::new());
                    self.col = 0;
                    self.pending = false;
                }
                '\r' => {
                    self.col = 0;
                    self.pending = true;
                }
                ch => {
                    let line = self.lines.last_mut().unwrap();
                    if self.col < line.len() {
                        line[self.col] = ch;
                    } else {
                        line.push(ch);
                    }
                    self.col += 1;
                    self.pending = false;
                }
            }
        }
        while self.lines.len() > self.max_lines {
            self.lines.remove(0);
            self.truncated += 1;
        }
        while self.lines.len() > 1 && self.body().len() > self.max_bytes {
            self.lines.remove(0);
            self.truncated += 1;
        }
    }

    fn body(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn text(&self) -> String {
        let mut text = String::new();
        if self.truncated > 0 {
            text.push_str(TRUNCATION_MARKER);
        }
        text.push_str(&self.body());
        if self.pending {
            text.push('\r');
        }
        text
    }

    /// Whether the cursor position survives a save: at the end, or at a pending `\r`.
    fn cursor_is_persistent(&self) -> bool {
        self.pending || self.col == self.lines.last().unwrap().len()
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % bound
    }
}

#[test]
fn matches_reference_terminal_model() {
    const ALPHABET: &[&str] = &["a", "b", "é", "漢", "\r", "\n", "\r\n", "xyz"];
    let mut rng = Lcg(7);
    for case in 0..400 {
        let max_lines = 1 + rng.next(8);
        let max_bytes = if case % 2 == 0 {
            usize::MAX / 2
        } else {
            4 + rng.next(30)
        };
        let mut buffer = StreamBuffer::with_caps(max_lines, max_bytes);
        let mut model = Model::new(max_lines, max_bytes);
        for step in 0..60 {
            let chunk: String = (0..rng.next(6))
                .map(|_| ALPHABET[rng.next(ALPHABET.len())])
                .collect();
            buffer.push_chunk(&chunk);
            model.push(&chunk);
            assert_eq!(
                buffer.as_str(),
                model.text(),
                "case {case} step {step} chunk {chunk:?}"
            );
            assert_eq!(
                buffer.truncated_lines(),
                model.truncated,
                "case {case} step {step}"
            );
            if rng.next(10) == 0 && model.cursor_is_persistent() {
                let json = serde_json::to_value(&buffer).unwrap();
                buffer = serde_json::from_value(json).unwrap();
            }
        }
    }
}

#[test]
fn revision_changes_on_same_length_rewrite() {
    let mut buffer = StreamBuffer::default();
    buffer.push_chunk("10%");
    let before = buffer.revision();
    buffer.push_chunk("\r20%");
    assert_eq!(buffer.as_str(), "20%");
    assert_ne!(buffer.revision(), before);
}

#[test]
fn byte_cap_never_drops_the_last_line() {
    let mut buffer = StreamBuffer::with_caps(100, 8);
    buffer.push_chunk("one\ntwo\n");
    buffer.push_chunk(&"x".repeat(50));
    assert_eq!(
        buffer.as_str(),
        format!("{TRUNCATION_MARKER}{}", "x".repeat(50))
    );
    assert_eq!(buffer.truncated_lines(), 2);
}

#[test]
fn many_small_chunks_take_linear_time() {
    let line = format!("{}\n", "y".repeat(79));
    let start = Instant::now();
    let mut buffer = StreamBuffer::with_cap(10_000);
    for _ in 0..20_000 {
        buffer.push_chunk(&line);
    }
    // The old buffer re-parsed everything per chunk: seconds, even in release.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(buffer.truncated_lines(), 10_001);
}

#[test]
fn by_default_nothing_is_dropped() {
    let mut buffer = StreamBuffer::default();
    for i in 0..30_000 {
        buffer.push_chunk(&format!("line {i}\n"));
    }
    let long = "z".repeat(12 << 20);
    buffer.push_chunk(&long);
    assert_eq!(buffer.truncated_lines(), 0);
    assert!(buffer.as_str().starts_with("line 0\n"));
    assert!(buffer.as_str().ends_with(&long));
}
