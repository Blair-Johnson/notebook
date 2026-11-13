//! Typed in-memory representation of cell and session outputs.
//!
//! Raw nbformat JSON exists only at serialization boundaries (`codec::ipynb`).

use crate::id::OutputId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DEFAULT_MAX_STREAM_LINES: usize = 10_000;
pub const DEFAULT_MAX_STREAM_BYTES: usize = 8 << 20;
pub const TRUNCATION_MARKER: &str = "[... output truncated ...]\n";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Coalesced terminal stream text supporting `\r` progress-bar overwrites and line caps.
///
/// `text` holds the logical lines joined by `\n`, prefixed by
/// [`TRUNCATION_MARKER`] once lines have been dropped, and suffixed by `\r`
/// while a carriage return waits to overwrite the last line. Appending only
/// touches the chunk and the last line, so streaming cost is linear in the
/// output, not quadratic.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamBuffer {
    text: String,
    max_lines: usize,
    truncated_lines: usize,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    #[serde(skip)]
    revision: u64,
    #[serde(skip)]
    tail: Option<Tail>,
}

/// Where appends land; derived from `text` on first use.
#[derive(Clone, Copy, Debug)]
struct Tail {
    /// Logical lines after the truncation marker.
    lines: usize,
    /// Byte offset of the last line.
    last_start: usize,
    /// Byte offset of the next write; inside the last line after a `\r`.
    cursor: usize,
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_STREAM_BYTES
}

impl PartialEq for StreamBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.max_lines == other.max_lines
            && self.max_bytes == other.max_bytes
            && self.truncated_lines == other.truncated_lines
    }
}

impl Eq for StreamBuffer {}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::with_cap(DEFAULT_MAX_STREAM_LINES)
    }
}

impl StreamBuffer {
    pub fn with_cap(max_lines: usize) -> Self {
        Self::with_caps(max_lines, DEFAULT_MAX_STREAM_BYTES)
    }

    /// Keep at most `max_lines` lines and roughly `max_bytes` bytes; the
    /// oldest whole lines go first, and the last line is never dropped.
    pub fn with_caps(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_lines: max_lines.max(1),
            truncated_lines: 0,
            max_bytes: max_bytes.max(1),
            revision: 0,
            tail: None,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn truncated_lines(&self) -> usize {
        self.truncated_lines
    }

    /// Changes on every append. Views compare it to know their index is stale,
    /// even when a `\r` rewrite left the length unchanged.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Append a streamed chunk using terminal `\r` overwrite semantics and line capping.
    pub fn push_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        let mut tail = self.take_tail();
        let pending = self.write(&mut tail, chunk);
        self.enforce_caps(&mut tail);
        if pending {
            self.text.push('\r');
        }
        self.tail = Some(tail);
        self.revision = self.revision.wrapping_add(1);
    }

    fn body_start(&self) -> usize {
        if self.text.starts_with(TRUNCATION_MARKER) {
            TRUNCATION_MARKER.len()
        } else {
            0
        }
    }

    /// The append state, with any pending `\r` removed from `text`.
    fn take_tail(&mut self) -> Tail {
        let pending = self.text.ends_with('\r');
        if pending {
            self.text.pop();
        }
        if let Some(tail) = self.tail.take() {
            return tail;
        }
        let body_start = self.body_start();
        if self.text[body_start..].contains('\r') {
            // Text that never went through `push_chunk` (deserialized): replay it.
            let raw = self.text.split_off(body_start);
            let mut tail = self.empty_tail();
            self.write(&mut tail, &raw);
            if pending {
                tail.cursor = tail.last_start;
            }
            return tail;
        }
        let body = &self.text[body_start..];
        let last_start = body.rfind('\n').map_or(body_start, |i| body_start + i + 1);
        Tail {
            lines: body.bytes().filter(|b| *b == b'\n').count() + 1,
            last_start,
            cursor: if pending { last_start } else { self.text.len() },
        }
    }

    fn empty_tail(&self) -> Tail {
        Tail {
            lines: 1,
            last_start: self.text.len(),
            cursor: self.text.len(),
        }
    }

    /// Apply `chunk` at the cursor; returns whether it ended on a pending `\r`.
    fn write(&mut self, tail: &mut Tail, chunk: &str) -> bool {
        let mut pending = false;
        for piece in chunk.split_inclusive(['\r', '\n']) {
            let (run, delimiter) = match piece.as_bytes().last() {
                Some(b'\r') => (&piece[..piece.len() - 1], Some('\r')),
                Some(b'\n') => (&piece[..piece.len() - 1], Some('\n')),
                _ => (piece, None),
            };
            if !run.is_empty() {
                self.write_run(tail, run);
                pending = false;
            }
            match delimiter {
                Some('\n') => {
                    self.text.push('\n');
                    tail.lines += 1;
                    tail.last_start = self.text.len();
                    tail.cursor = self.text.len();
                    pending = false;
                }
                Some(_) => {
                    tail.cursor = tail.last_start;
                    pending = true;
                }
                None => {}
            }
        }
        pending
    }

    /// Write text without line breaks at the cursor, overwriting characters
    /// of the last line one for one and appending past its end.
    fn write_run(&mut self, tail: &mut Tail, run: &str) {
        if tail.cursor == self.text.len() {
            self.text.push_str(run);
        } else {
            let overwritten = run.chars().count();
            let end = self.text[tail.cursor..]
                .char_indices()
                .nth(overwritten)
                .map_or(self.text.len(), |(offset, _)| tail.cursor + offset);
            self.text.replace_range(tail.cursor..end, run);
        }
        tail.cursor += run.len();
    }

    /// Drop the oldest whole lines beyond the line and byte caps, in one
    /// splice that also (re)writes the truncation marker.
    fn enforce_caps(&mut self, tail: &mut Tail) {
        let body_start = self.body_start();
        let mut cut = body_start;
        let mut dropped = 0;
        if tail.lines > self.max_lines {
            dropped = tail.lines - self.max_lines;
            cut = self.text[body_start..]
                .match_indices('\n')
                .nth(dropped - 1)
                .map_or(tail.last_start, |(offset, _)| body_start + offset + 1);
        }
        let kept = self.text.len() - cut;
        if kept > self.max_bytes && cut < tail.last_start {
            let target = cut + (kept - self.max_bytes);
            let next = if target > tail.last_start {
                tail.last_start
            } else {
                // Bytes, not str: `target` may fall inside a multi-byte char.
                self.text.as_bytes()[target - 1..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(tail.last_start, |offset| target + offset)
            };
            dropped += self.text[cut..next].bytes().filter(|b| *b == b'\n').count();
            cut = next;
        }
        if dropped == 0 {
            return;
        }
        self.text.replace_range(..cut, TRUNCATION_MARKER);
        let rebase = |offset: usize| offset - cut + TRUNCATION_MARKER.len();
        tail.last_start = rebase(tail.last_start);
        tail.cursor = rebase(tail.cursor);
        tail.lines -= dropped;
        self.truncated_lines = self.truncated_lines.saturating_add(dropped);
    }
}

/// MIME-keyed representation map (e.g. `"text/plain"`, `"application/vnd.jaxtree+json"`).
pub type MimeBundle = BTreeMap<String, Value>;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum OutputData {
    Stream {
        stream: StreamKind,
        buffer: StreamBuffer,
    },
    Rich {
        bundle: MimeBundle,
        metadata: Map<String, Value>,
        display_id: Option<String>,
        is_execute_result: bool,
        execution_count: Option<i64>,
    },
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct OutputItem {
    pub id: OutputId,
    pub rev: u64,
    /// True if `iopub` delivered this output after the parent execution reached `idle`.
    pub transient_late: bool,
    pub data: OutputData,
    /// Preserves unknown nbformat output fields across load/save cycles.
    pub extra: Map<String, Value>,
}

impl OutputItem {
    pub fn new(data: OutputData, transient_late: bool) -> Self {
        Self {
            id: OutputId::next(),
            rev: 1,
            transient_late,
            data,
            extra: Map::new(),
        }
    }

    pub fn bump_rev(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// The output as plain text, as a reader would copy it: stream text,
    /// `text/plain` of rich outputs, or the traceback without ANSI colors.
    pub fn plain_text(&self) -> String {
        let text = match &self.data {
            OutputData::Stream { buffer, .. } => buffer.as_str().to_string(),
            OutputData::Rich { bundle, .. } => match bundle.get("text/plain") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<String>(),
                _ => bundle
                    .keys()
                    .next()
                    .map_or_else(String::new, |mime| format!("<{mime}>")),
            },
            OutputData::Error {
                ename,
                evalue,
                traceback,
            } => {
                if traceback.is_empty() {
                    format!("{ename}: {evalue}")
                } else {
                    traceback.join("\n")
                }
            }
        };
        strip_ansi(text.trim_end_matches('\n'))
    }

    /// `plain_text().lines().count()` without building the text: cheap
    /// enough to call every frame on a large output.
    pub fn line_count(&self) -> usize {
        match &self.data {
            OutputData::Stream { buffer, .. } => count_lines([buffer.as_str()].into_iter()),
            OutputData::Rich { bundle, .. } => match bundle.get("text/plain") {
                Some(Value::String(text)) => count_lines([text.as_str()].into_iter()),
                Some(Value::Array(parts)) => count_lines(parts.iter().filter_map(Value::as_str)),
                _ => usize::from(!bundle.is_empty()),
            },
            OutputData::Error {
                ename,
                evalue,
                traceback,
            } => {
                if traceback.is_empty() {
                    count_lines([ename.as_str(), ": ", evalue.as_str()].into_iter())
                } else {
                    let separated = traceback.iter().enumerate().flat_map(|(index, line)| {
                        [if index == 0 { "" } else { "\n" }, line.as_str()]
                    });
                    count_lines(separated)
                }
            }
        }
    }
}

/// Lines in the concatenation of `pieces`, ignoring trailing newlines, as
/// `str::lines` counts them.
fn count_lines<'a>(pieces: impl DoubleEndedIterator<Item = &'a str> + Clone) -> usize {
    let newlines: usize = pieces.clone().map(|piece| piece.matches('\n').count()).sum();
    let mut trailing = 0;
    for piece in pieces.rev() {
        let content = piece.trim_end_matches('\n');
        trailing += piece.len() - content.len();
        if !content.is_empty() {
            return newlines - trailing + 1;
        }
    }
    0
}

/// Remove ANSI CSI/OSC escape sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
