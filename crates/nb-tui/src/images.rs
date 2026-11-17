//! Terminal images through the kitty graphics protocol.
//!
//! Images are drawn with kitty's Unicode placeholders: every occupied cell of an
//! image is `U+10EEEE` followed by row and column diacritics, with the 24-bit
//! image id in the cell's RGB foreground color. To ratatui these are ordinary
//! text cells, so images scroll, clip vertically inside `OutputView::render_slice`,
//! and survive tmux pane/window switches (`allow-passthrough on`).
//!
//! Nothing in this module blocks the UI event loop:
//! - Producers (`math`, `mermaid`, `pictures`, and rich MIME outputs) decode,
//!   typeset, rasterize, PNG-compress, and base64-encode on background worker
//!   threads (`prepared` + `WorkQueue`).
//! - Inside tmux, capability detection, pane visibility, and client reattachment
//!   checks run on a dedicated background probe thread (`TmuxProbe`).
//! - Pre-sliced `< 4096`-byte kitty APC packets are stored in a bounded LRU
//!   registry and streamed by `terminal_output::TerminalWriter` on a dedicated
//!   stdout thread, yielding between packets to UI frames and pausing during
//!   active scrolling.

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use nb_sdk::Theme;
use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
    text::{Line, Span},
};
use resvg::{tiny_skia, usvg};

mod diacritics;
use diacritics::DIACRITICS;

pub const PLACEHOLDER: char = '\u{10EEEE}';
/// Most rows or columns one placeholder grid can address.
pub const MAX_CELLS: usize = DIACRITICS.len();
/// Base64 bytes per transmission chunk (multiple of 4), sized so each chunk
/// plus kitty control header and tmux DCS passthrough framing stays strictly
/// below the 4096-byte POSIX `PIPE_BUF` atomic write threshold.
const CHUNK_BYTES: usize = 3072;
/// Assumed cell size in pixels when the terminal does not report one (such as
/// over SSH when `TIOCGWINSZ` pixel fields are zero); 1:2 aspect at ~158 DPI
/// keeps scaled glyphs crisp on both standard and HiDPI displays.
const DEFAULT_CELL: (u16, u16) = (12, 24);
pub const MAX_IMAGE_PIXELS: usize = 4096 * 4096;
const TMUX_PROBE_INTERVAL: Duration = Duration::from_secs(1);
const FAILED_RETRY_COOLDOWN: Duration = Duration::from_secs(2);
const TRANSIENT_RETRY_BASE: Duration = Duration::from_secs(1);
const MAX_TRANSIENT_RETRIES: u8 = 6;
const MAX_PREPARED_JOBS: usize = 4_096;
const MAX_PREPARED_BYTES: usize = 64 * 1024 * 1024;
const MAX_STORED_PACKETS: usize = 4_096;
const MAX_STORED_PACKET_BYTES: usize = 64 * 1024 * 1024;

/// CommonMark fenced-code-block state machine (``` or ~~~, 3+ chars, up to 3
/// leading spaces) so image/math pre-passes never rewrite content inside
/// 4-backtick outer fences or tilde fences.
#[derive(Debug, Default, Clone, Copy)]
pub struct FenceTracker {
    open: Option<(u8, usize)>,
}

impl FenceTracker {
    /// Advance by one line. Returns true if `line` is part of a fenced block
    /// (including its opening and closing fence lines).
    pub fn step(&mut self, line: &str) -> bool {
        let trimmed_cr = line.trim_end_matches('\r');
        let indent = trimmed_cr.len() - trimmed_cr.trim_start_matches(' ').len();
        let rest = &trimmed_cr[indent..];
        if let Some((fence_ch, open_len)) = self.open {
            if indent <= 3 {
                let run = rest.bytes().take_while(|byte| *byte == fence_ch).count();
                if run >= open_len && rest[run..].trim_matches([' ', '\t']).is_empty() {
                    self.open = None;
                }
            }
            return true;
        }
        if indent <= 3
            && let Some(&first) = rest.as_bytes().first()
            && matches!(first, b'`' | b'~')
        {
            let run = rest.bytes().take_while(|byte| *byte == first).count();
            if run >= 3 && (first != b'`' || !rest[run..].contains('`')) {
                self.open = Some((first, run));
                return true;
            }
        }
        false
    }
}

/// Replace any raw `U+10EEEE` in untrusted markdown/output text so cell or
/// kernel strings cannot forge kitty placeholder cells.
pub fn sanitize_placeholders(text: &str) -> Cow<'_, str> {
    if text.contains(PLACEHOLDER) {
        Cow::Owned(text.replace(PLACEHOLDER, "\u{FFFD}"))
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageMode {
    /// Use images when the interactive terminal supports kitty placeholders.
    #[default]
    Auto,
    /// Always emit kitty placeholders.
    Kitty,
    /// Never emit terminal images; fall back to text/code.
    Off,
}

impl ImageMode {
    pub const OPTIONS: &'static [&'static str] = &["auto", "kitty", "off"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Off => "off",
        }
    }

    pub fn from_name(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "kitty" | "on" => Some(Self::Kitty),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }
}

/// What `auto` found out about the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub supported: bool,
    pub summary: String,
}

impl Detection {
    fn on(summary: impl Into<String>) -> Self {
        Self {
            supported: true,
            summary: summary.into(),
        }
    }

    fn off(summary: impl Into<String>) -> Self {
        Self {
            supported: false,
            summary: summary.into(),
        }
    }
}

/// An image already fitted to whole cells, PNG-compressed, and base64-encoded
/// on a background worker thread.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    hash: u64,
    base64: Arc<str>,
    pub cols: u16,
    pub rows: u16,
    /// Optional `rows * cols` mask marking cells that contain at least one
    /// non-transparent (`alpha > 0`) pixel. `None` treats all cells as occupied.
    non_empty_tiles: Option<Arc<[bool]>>,
}

impl PreparedImage {
    pub fn from_png(png: &[u8], cols: u16, rows: u16) -> Self {
        Self::from_png_with_mask(png, cols, rows, None)
    }

    fn from_png_with_mask(
        png: &[u8],
        cols: u16,
        rows: u16,
        non_empty_tiles: Option<Arc<[bool]>>,
    ) -> Self {
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let mut hasher = DefaultHasher::new();
        encoded.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            base64: encoded.into(),
            cols,
            rows,
            non_empty_tiles,
        }
    }
}

struct StoredPacketEntry {
    packets: Arc<[Box<[u8]>]>,
    bytes: usize,
    last_used: u64,
}

#[derive(Default)]
struct PacketRegistry {
    entries: HashMap<u32, StoredPacketEntry>,
    total_bytes: usize,
    clock: u64,
}

impl PacketRegistry {
    fn insert(&mut self, id: u32, packets: Arc<[Box<[u8]>]>) {
        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        let bytes: usize = packets.iter().map(|chunk| chunk.len()).sum();
        if let Some(prev) = self.entries.insert(
            id,
            StoredPacketEntry {
                packets,
                bytes,
                last_used: stamp,
            },
        ) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.bytes);
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.evict_lru_except(id);
    }

    fn touch(&mut self, id: u32) -> bool {
        let next_clock = self.clock.wrapping_add(1);
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        self.clock = next_clock;
        entry.last_used = next_clock;
        true
    }

    fn evict_lru_except(&mut self, protected_id: u32) {
        while self.entries.len() > MAX_STORED_PACKETS
            || (self.total_bytes > MAX_STORED_PACKET_BYTES && self.entries.len() > 1)
        {
            let victim = self
                .entries
                .iter()
                .filter(|(candidate_id, _)| **candidate_id != protected_id)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(candidate_id, _)| *candidate_id);
            let Some(victim_id) = victim else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim_id) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
        }
    }
}

static PACKETS: OnceLock<RwLock<PacketRegistry>> = OnceLock::new();
static PROBING_ENABLED: AtomicBool = AtomicBool::new(false);

fn packet_store() -> &'static RwLock<PacketRegistry> {
    PACKETS.get_or_init(|| RwLock::new(PacketRegistry::default()))
}

pub fn packets_for(id: u32) -> Option<Arc<[Box<[u8]>]>> {
    packet_store()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .entries
        .get(&id)
        .map(|entry| Arc::clone(&entry.packets))
}

/// Enable live terminal/tmux probing for `ImageMode::Auto`. Called by the
/// interactive terminal runtime; headless `TestBackend` sessions leave this off
/// so `auto` never spawns `tmux` subprocesses during tests.
pub fn enable_terminal_probing() {
    PROBING_ENABLED.store(true, Ordering::Release);
}

fn probing_enabled() -> bool {
    !cfg!(test) && PROBING_ENABLED.load(Ordering::Acquire)
}

#[derive(Default)]
struct State {
    mode: Option<ImageMode>,
    direct_detection: Option<Detection>,
    enabled: bool,
    tmux: bool,
    cell: Option<(u16, u16)>,
    layout_epoch: u64,
    by_content: HashMap<(u64, u16, u16, bool), u32>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

/// Monotonic counter incremented whenever image availability or cell pixel
/// dimensions change, so `LayoutCache` and `OutputView`s invalidate cleanly.
pub fn layout_epoch() -> u64 {
    with(|state| state.layout_epoch)
}

/// Apply the configured mode without blocking on external processes. Returns
/// true when images turned on or off.
pub fn configure(mode: ImageMode) -> bool {
    with(|state| {
        state.mode = Some(mode);
        apply_mode_in_state(state)
    })
}

fn apply_mode_in_state(state: &mut State) -> bool {
    let probing = probing_enabled();
    let tmux = probing && in_tmux();
    let enabled = match state.mode.unwrap_or(ImageMode::Off) {
        ImageMode::Off => false,
        ImageMode::Kitty => true,
        ImageMode::Auto if !probing => false,
        ImageMode::Auto if tmux => tmux_probe()
            .detection()
            .is_some_and(|detection| detection.supported),
        ImageMode::Auto => {
            state
                .direct_detection
                .get_or_insert_with(detect_direct)
                .supported
        }
    };
    if enabled {
        state.tmux = tmux;
        if state.cell.is_none() && probing {
            state.cell = query_cell_size();
        }
    }
    let changed = state.enabled != enabled;
    state.enabled = enabled;
    if changed {
        state.layout_epoch = state.layout_epoch.wrapping_add(1);
    }
    changed
}

/// Check whether asynchronous terminal detection finished and flipped image
/// support on or off.
pub fn poll_probe() -> bool {
    if !probing_enabled() || !in_tmux() || !tmux_probe().take_detection_dirty() {
        return false;
    }
    with(apply_mode_in_state)
}

/// True while initial `auto` detection is still running in the background.
pub fn probe_pending() -> bool {
    probing_enabled()
        && in_tmux()
        && with(|state| state.mode == Some(ImageMode::Auto))
        && tmux_probe().detection().is_none()
}

pub fn enabled() -> bool {
    with(|state| state.enabled)
}

pub fn is_tmux() -> bool {
    with(|state| state.tmux)
}

pub fn pane_visible() -> bool {
    if !probing_enabled() || !in_tmux() {
        true
    } else {
        tmux_probe().pane_visible.load(Ordering::Acquire)
    }
}

pub fn client_epoch() -> u64 {
    if !probing_enabled() || !in_tmux() {
        0
    } else {
        tmux_probe().client_epoch.load(Ordering::Acquire)
    }
}

/// What `auto` finds out about this terminal, never blocking on subprocesses.
pub fn auto_detection() -> Detection {
    if !probing_enabled() {
        return Detection::off("headless session");
    }
    if in_tmux() {
        return tmux_probe()
            .detection()
            .unwrap_or_else(|| Detection::off("checking tmux…"));
    }
    with(|state| state.direct_detection.get_or_insert_with(detect_direct).clone())
}

/// Cell size in pixels, used to size images to whole cells.
pub fn cell_size() -> (u16, u16) {
    with(|state| state.cell.unwrap_or(DEFAULT_CELL))
}

/// After a resize or focus return: wake the background tmux probe and re-read
/// the local window's pixel geometry. Returns true when cell pixel size changed.
pub fn terminal_changed() -> bool {
    if probing_enabled() && in_tmux() {
        tmux_probe().request_refresh();
    }
    with(|state| {
        if !state.enabled || !probing_enabled() {
            return false;
        }
        let cell = query_cell_size();
        let changed = cell.is_some() && cell != state.cell;
        if changed {
            state.cell = cell;
            state.layout_epoch = state.layout_epoch.wrapping_add(1);
        }
        changed
    })
}

/// Register a prepared image and return its 24-bit image id. Pre-slices its
/// kitty wire packets once so the output thread can stream them directly.
pub fn place_image(image: &PreparedImage) -> Option<u32> {
    let (cols, rows) = (image.cols, image.rows);
    if cols == 0 || rows == 0 || cols as usize > MAX_CELLS || rows as usize > MAX_CELLS {
        return None;
    }
    with(|state| {
        if !state.enabled {
            return None;
        }
        let key = (image.hash, cols, rows, state.tmux);
        if let Some(&id) = state.by_content.get(&key) {
            let still_stored = packet_store()
                .write()
                .unwrap_or_else(|poison| poison.into_inner())
                .touch(id);
            if still_stored {
                return Some(id);
            }
            state.by_content.remove(&key);
        }
        let id = next_image_id();
        let packets = build_packets(id, &image.base64, cols, rows, state.tmux);
        {
            let mut store = packet_store()
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            store.insert(id, packets);
            if state.by_content.len() > MAX_STORED_PACKETS {
                state
                    .by_content
                    .retain(|_, mapped_id| store.entries.contains_key(mapped_id));
            }
        }
        state.by_content.insert(key, id);
        Some(id)
    })
}

fn next_image_id() -> u32 {
    static NEXT_ID: OnceLock<AtomicU32> = OnceLock::new();
    let counter = NEXT_ID.get_or_init(|| {
        AtomicU32::new(1 + std::process::id().wrapping_mul(2_654_435_761) % 0xF0_0000)
    });
    loop {
        let current = counter.load(Ordering::Relaxed);
        let next = if current >= 0xFF_FFFF { 1 } else { current + 1 };
        if counter
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

fn id_color(id: u32) -> Color {
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

/// Style for placeholder cells of image `id`: the id is the foreground color.
pub fn image_style(id: u32) -> Style {
    Style::default().fg(id_color(id))
}

/// Append the placeholder cell for `row`, `col` of an image.
pub fn push_placeholder_cell(cells: &mut String, row: usize, col: usize) {
    cells.push(PLACEHOLDER);
    cells.push(DIACRITICS[row.min(MAX_CELLS - 1)]);
    cells.push(DIACRITICS[col.min(MAX_CELLS - 1)]);
}

pub fn placeholder_lines(id: u32, cols: u16, rows: u16, indent: usize) -> Vec<Line<'static>> {
    placeholder_lines_with_mask(id, cols, rows, indent, None)
}

/// Rows of placeholder cells for `image`, emitting plain spaces for 100%
/// transparent `(row, col)` tiles while keeping exact `(DIACRITICS[row], DIACRITICS[col])`
/// coordinates on every occupied tile so scrolling skips empty background air.
pub fn placeholder_lines_for(id: u32, image: &PreparedImage, indent: usize) -> Vec<Line<'static>> {
    placeholder_lines_with_mask(
        id,
        image.cols,
        image.rows,
        indent,
        image.non_empty_tiles.as_deref(),
    )
}

fn placeholder_lines_with_mask(
    id: u32,
    cols: u16,
    rows: u16,
    indent: usize,
    mask: Option<&[bool]>,
) -> Vec<Line<'static>> {
    let cols = (cols as usize).min(MAX_CELLS);
    let rows = (rows as usize).min(MAX_CELLS);
    let style = image_style(id);
    (0..rows)
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut pending_spaces = indent;
            let mut pending_cells = String::new();

            for col in 0..cols {
                let occupied = mask
                    .and_then(|m| m.get(row * cols + col).copied())
                    .unwrap_or(true);
                if occupied {
                    if pending_spaces > 0 {
                        spans.push(Span::raw(" ".repeat(pending_spaces)));
                        pending_spaces = 0;
                    }
                    push_placeholder_cell(&mut pending_cells, row, col);
                } else {
                    if !pending_cells.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut pending_cells), style));
                    }
                    pending_spaces += 1;
                }
            }
            if !pending_cells.is_empty() {
                spans.push(Span::styled(pending_cells, style));
            }
            if spans.is_empty() {
                spans.push(Span::raw(""));
            }
            Line::from(spans)
        })
        .collect()
}

/// Collect distinct visible kitty image ids in top-to-bottom order and blank
/// not-yet-uploaded `U+10EEEE` placeholder cells in a single pass. `U+10EEEE`
/// begins with UTF-8 lead byte `0xF4`, so ordinary ASCII/BMP cells skip in one
/// byte comparison without UTF-8 decoding.
pub fn collect_and_strip_placeholders(
    buffer: &mut Buffer,
    mut should_strip: impl FnMut(u32) -> bool,
) -> (Vec<u32>, Vec<u32>) {
    let mut visible = Vec::new();
    let mut hidden_ids = Vec::new();
    let mut last_id = 0_u32;
    let mut cached_strip: Option<(u32, bool)> = None;

    for cell in &mut buffer.content {
        let sym = cell.symbol();
        if sym.as_bytes().first() != Some(&0xF4) || !sym.starts_with(PLACEHOLDER) {
            continue;
        }
        let id = match cell.fg {
            Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
            _ => 0,
        };
        if id != 0 && id != last_id {
            last_id = id;
            if !visible.contains(&id) {
                visible.push(id);
            }
        }
        let strip = match cached_strip {
            Some((cached_id, decision)) if cached_id == id => decision,
            _ => {
                let decision = id == 0 || should_strip(id);
                if decision && !hidden_ids.contains(&id) {
                    hidden_ids.push(id);
                }
                cached_strip = Some((id, decision));
                decision
            }
        };
        if strip {
            cell.set_symbol(" ");
            cell.fg = Color::Reset;
        }
    }
    (visible, hidden_ids)
}

/// Slice base64 PNG data into self-contained `< 4096`-byte kitty APC packets.
/// Base64 is pure ASCII without ESC, so tmux DCS passthrough needs no scanning.
pub fn build_packets(
    id: u32,
    base64_png: &str,
    cols: u16,
    rows: u16,
    tmux: bool,
) -> Arc<[Box<[u8]>]> {
    let chunks: Vec<&[u8]> = base64_png.as_bytes().chunks(CHUNK_BYTES).collect();
    let total = chunks.len().max(1);
    let mut packets = Vec::with_capacity(total);
    for (index, chunk) in chunks.into_iter().enumerate() {
        let more = u8::from(index + 1 < total);
        let control = if index == 0 {
            format!("a=T,U=1,f=100,q=2,i={id},c={cols},r={rows},m={more}")
        } else {
            format!("m={more},q=2")
        };
        packets.push(encode_apc(&control, chunk, tmux));
    }
    if packets.is_empty() {
        let control = format!("a=T,U=1,f=100,q=2,i={id},c={cols},r={rows},m=0");
        packets.push(encode_apc(&control, b"", tmux));
    }
    packets.into()
}

fn encode_apc(control: &str, payload: &[u8], tmux: bool) -> Box<[u8]> {
    let mut out = Vec::with_capacity(control.len() + payload.len() + 20);
    if tmux {
        out.extend_from_slice(b"\x1bPtmux;\x1b\x1b_G");
        out.extend_from_slice(control.as_bytes());
        out.push(b';');
        out.extend_from_slice(payload);
        out.extend_from_slice(b"\x1b\x1b\\\x1b\\");
    } else {
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(control.as_bytes());
        out.push(b';');
        out.extend_from_slice(payload);
        out.extend_from_slice(b"\x1b\\");
    }
    out.into_boxed_slice()
}

/// Close an interrupted multi-chunk kitty transfer (`m=0`) and delete its
/// partial image id so the next image starts cleanly.
pub fn append_abort_packet(out: &mut Vec<u8>, id: u32, tmux: bool) {
    out.extend_from_slice(&encode_apc("m=0,q=2", b"", tmux));
    append_delete_packet(out, id, tmux);
}

/// Delete a sent image from the terminal's memory.
pub fn append_delete_packet(out: &mut Vec<u8>, id: u32, tmux: bool) {
    out.extend_from_slice(&encode_apc(&format!("a=d,d=I,i={id},q=2"), b"", tmux));
}

/// Extract 8-bit RGB from a `ratatui::style::Color` when known.
pub fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Black => Some((24, 24, 28)),
        Color::White => Some((235, 235, 240)),
        Color::DarkGray => Some((100, 108, 124)),
        Color::Gray => Some((164, 172, 186)),
        Color::Red | Color::LightRed => Some((224, 86, 98)),
        Color::Green | Color::LightGreen => Some((102, 198, 132)),
        Color::Yellow | Color::LightYellow => Some((226, 186, 88)),
        Color::Blue | Color::LightBlue => Some((98, 156, 242)),
        Color::Magenta | Color::LightMagenta => Some((188, 132, 244)),
        Color::Cyan | Color::LightCyan => Some((88, 196, 214)),
        Color::Reset | Color::Indexed(_) => None,
    }
}

/// True when `theme` uses a light background.
pub fn theme_is_light(theme: &Theme) -> bool {
    if theme.name.eq_ignore_ascii_case("light") {
        return true;
    }
    match color_rgb(theme.palette.bg) {
        Some((r, g, b)) => {
            let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
            luma >= 140.0
        }
        None => false,
    }
}

/// Ink RGB for uncolored formulas/SVGs under `theme` (never stored globally).
pub fn theme_ink_rgb(theme: &Theme) -> (u8, u8, u8) {
    if let Some(rgb) = color_rgb(theme.palette.fg) {
        return rgb;
    }
    if theme_is_light(theme) {
        (32, 36, 44)
    } else {
        (220, 224, 232)
    }
}

/// Jupyter `display_data` `metadata.needs_background` hint for transparent plots.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeedsBackground {
    #[default]
    None,
    Light,
    Dark,
}

impl NeedsBackground {
    pub fn from_metadata(metadata: &serde_json::Map<String, serde_json::Value>) -> Self {
        match metadata.get("needs_background").and_then(|v| v.as_str()) {
            Some(s) if s.eq_ignore_ascii_case("light") => Self::Light,
            Some(s) if s.eq_ignore_ascii_case("dark") => Self::Dark,
            _ => Self::None,
        }
    }

    pub fn matte_for_theme(self, theme_light: bool) -> Option<(u8, u8, u8)> {
        match (self, theme_light) {
            (Self::Light, false) => Some((248, 250, 252)),
            (Self::Dark, true) => Some((24, 28, 38)),
            _ => None,
        }
    }
}

/// Alpha-composite straight RGBA pixels onto an opaque `(mr, mg, mb)` matte.
pub fn composite_matte(rgba: &mut [u8], (mr, mg, mb): (u8, u8, u8)) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        if a == 255 {
            continue;
        }
        let inv = 255 - a;
        px[0] = ((u16::from(px[0]) * a + u16::from(mr) * inv + 127) / 255) as u8;
        px[1] = ((u16::from(px[1]) * a + u16::from(mg) * inv + 127) / 255) as u8;
        px[2] = ((u16::from(px[2]) * a + u16::from(mb) * inv + 127) / 255) as u8;
        px[3] = 255;
    }
}

/// Shared system font database loaded once on a background worker thread.
pub fn system_fontdb() -> &'static Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        db.set_sans_serif_family("Helvetica");
        Arc::new(db)
    })
}

/// Rasterize an SVG document into straight RGBA `(width, height, rgba)` scaled
/// crisply to fit within `(max_cols, max_rows)` cells.
pub fn rasterize_svg(
    svg_bytes: &[u8],
    cell: (u16, u16),
    max_cols: usize,
    max_rows: usize,
    ideal_scale: f32,
) -> Result<(usize, usize, Vec<u8>), String> {
    let usvg_options = usvg::Options {
        font_family: "Helvetica".to_string(),
        fontdb: Arc::clone(system_fontdb()),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(svg_bytes, &usvg_options)
        .map_err(|error| format!("SVG parse error: {error}"))?;
    let svg_size = tree.size();
    let (svg_w, svg_h) = (svg_size.width(), svg_size.height());
    if svg_w <= 0.0 || svg_h <= 0.0 || !svg_w.is_finite() || !svg_h.is_finite() {
        return Err("SVG produced empty bounds".to_string());
    }
    let (cell_w, cell_h) = (f32::from(cell.0.max(1)), f32::from(cell.1.max(1)));
    let max_px_w = (max_cols.max(1) as f32) * cell_w;
    let max_px_h = (max_rows.max(1) as f32) * cell_h;
    let scale = ideal_scale
        .min(max_px_w / svg_w)
        .min(max_px_h / svg_h);
    let px_w = ((svg_w * scale).ceil() as u32).max(1);
    let px_h = ((svg_h * scale).ceil() as u32).max(1);
    if (px_w as usize) * (px_h as usize) > MAX_IMAGE_PIXELS {
        return Err(format!("SVG raster size {px_w}x{px_h} exceeds pixel cap"));
    }
    let mut pixmap = tiny_skia::Pixmap::new(px_w, px_h)
        .ok_or_else(|| format!("SVG pixmap {px_w}x{px_h} allocation failed"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = Vec::with_capacity((px_w as usize) * (px_h as usize) * 4);
    for pixel in pixmap.pixels() {
        let straight = pixel.demultiply();
        rgba.extend_from_slice(&[
            straight.red(),
            straight.green(),
            straight.blue(),
            straight.alpha(),
        ]);
    }
    Ok((px_w as usize, px_h as usize, rgba))
}

/// Decode a PNG, JPEG, GIF, or WebP into 8-bit RGBA: `(width, height, pixels)`.
pub fn decode_image(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let image =
        image::load_from_memory(bytes).map_err(|error| format!("unreadable image: {error}"))?;
    let (width, height) = (image.width() as usize, image.height() as usize);
    if width == 0 || height == 0 || width * height > MAX_IMAGE_PIXELS {
        return Err(format!("image size {width}x{height} is not drawable"));
    }
    Ok((width, height, image.into_rgba8().into_raw()))
}

/// Scale RGBA pixels to `width` x `height`.
pub fn resize_rgba(
    source_width: usize,
    source_height: usize,
    rgba: Vec<u8>,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, String> {
    let image = image::RgbaImage::from_raw(source_width as u32, source_height as u32, rgba)
        .ok_or("pixel buffer has the wrong size")?;
    let resized = image::imageops::resize(
        &image,
        width.max(1) as u32,
        height.max(1) as u32,
        image::imageops::FilterType::Triangle,
    );
    Ok(resized.into_raw())
}

/// Pad RGBA pixels with transparency to whole cells, centered, and encode
/// them as a compressed `PreparedImage` on the calling worker thread.
pub fn fit_rgba(
    width: usize,
    height: usize,
    rgba: &[u8],
    cell: (u16, u16),
) -> Result<PreparedImage, String> {
    let (cell_width, cell_height) = (cell.0.max(1) as usize, cell.1.max(1) as usize);
    if width == 0 || height == 0 || rgba.len() != width * height * 4 {
        return Err("image has no pixels".to_string());
    }
    let cols = width.div_ceil(cell_width);
    let rows = height.div_ceil(cell_height);
    if cols > MAX_CELLS || rows > MAX_CELLS {
        return Err(format!("image needs {cols}x{rows} cells"));
    }
    let (canvas_width, canvas_height) = (cols * cell_width, rows * cell_height);
    let (left, top) = ((canvas_width - width) / 2, (canvas_height - height) / 2);
    let mut canvas = vec![0_u8; canvas_width * canvas_height * 4];
    for (y, row) in rgba.chunks_exact(width * 4).enumerate() {
        let offset = ((top + y) * canvas_width + left) * 4;
        canvas[offset..offset + row.len()].copy_from_slice(row);
    }
    let mut non_empty = vec![false; rows * cols];
    let mut any_occupied = false;
    for tile_row in 0..rows {
        let py_start = tile_row * cell_height;
        let py_end = py_start + cell_height;
        for tile_col in 0..cols {
            let px_start = tile_col * cell_width;
            let px_end = px_start + cell_width;
            let mut occupied = false;
            'tile: for py in py_start..py_end {
                let row_base = py * canvas_width;
                for px in px_start..px_end {
                    if canvas[(row_base + px) * 4 + 3] != 0 {
                        occupied = true;
                        break 'tile;
                    }
                }
            }
            non_empty[tile_row * cols + tile_col] = occupied;
            any_occupied |= occupied;
        }
    }
    if !any_occupied && !non_empty.is_empty() {
        non_empty[0] = true;
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, canvas_width as u32, canvas_height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Default);
        encoder.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("PNG encode failed: {error}"))?;
        writer
            .write_image_data(&canvas)
            .map_err(|error| format!("PNG encode failed: {error}"))?;
    }
    Ok(PreparedImage::from_png_with_mask(
        &out,
        cols as u16,
        rows as u16,
        Some(non_empty.into()),
    ))
}

pub type Rendered = PreparedImage;

enum Prepared {
    Pending {
        prior_attempts: u8,
    },
    Ready(PreparedImage),
    RetryableFailure {
        retry_at: Instant,
        attempts: u8,
        needle: String,
        notified: bool,
    },
    Failed(Instant),
}

struct JobEntry {
    state: Prepared,
    bytes: usize,
    last_used: u64,
}

struct Done {
    key: String,
    needle: String,
    result: Result<Rendered, String>,
    retryable: bool,
}

/// Delivers one background image back to the main thread.
pub struct Completer {
    key: String,
    needle: String,
    done: mpsc::Sender<Done>,
}

impl Completer {
    pub fn finish(self, result: Result<Rendered, String>) {
        self.finish_with_retry(result, false);
    }

    pub fn finish_with_retry(self, result: Result<Rendered, String>, retryable: bool) {
        let _ = self.done.send(Done {
            key: self.key,
            needle: self.needle,
            result,
            retryable,
        });
    }
}

struct Jobs {
    entries: HashMap<String, JobEntry>,
    total_bytes: usize,
    clock: u64,
    done_sender: mpsc::Sender<Done>,
    done: mpsc::Receiver<Done>,
}

impl Default for Jobs {
    fn default() -> Self {
        let (done_sender, done) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            clock: 0,
            done_sender,
            done,
        }
    }
}

impl Jobs {
    fn next_stamp(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn insert_entry(&mut self, key: String, state: Prepared) {
        let bytes = match &state {
            Prepared::Ready(image) => image.base64.len(),
            _ => 0,
        };
        let stamp = self.next_stamp();
        if let Some(prev) = self.entries.insert(
            key.clone(),
            JobEntry {
                state,
                bytes,
                last_used: stamp,
            },
        ) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.bytes);
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.evict_lru_except(&key);
    }

    fn evict_lru_except(&mut self, protected_key: &str) {
        while self.entries.len() > MAX_PREPARED_JOBS
            || (self.total_bytes > MAX_PREPARED_BYTES && self.entries.len() > 1)
        {
            let victim = self
                .entries
                .iter()
                .filter(|(candidate, entry)| {
                    candidate.as_str() != protected_key
                        && !matches!(
                            entry.state,
                            Prepared::Pending { .. }
                                | Prepared::RetryableFailure {
                                    notified: false,
                                    ..
                                }
                        )
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(candidate, _)| candidate.clone());
            let Some(victim_key) = victim else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim_key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
        }
    }
}

thread_local! {
    static JOBS: RefCell<Jobs> = RefCell::new(Jobs::default());
}

/// Look up or schedule a background `PreparedImage` for `key`. `needle`
/// identifies which markdown cells or output views (`"output:<id>"`) should
/// invalidate when the background job completes.
pub fn prepared(key: &str, needle: &str, start: impl FnOnce(Completer)) -> Option<PreparedImage> {
    if !enabled() {
        return None;
    }
    let completer = JOBS.with(|jobs| {
        let mut jobs = jobs.borrow_mut();
        let next_clock = jobs.clock.wrapping_add(1);
        let mut prior_attempts = 0_u8;
        if let Some(entry) = jobs.entries.get_mut(key) {
            match &entry.state {
                Prepared::Ready(image) => {
                    let cloned = image.clone();
                    entry.last_used = next_clock;
                    jobs.clock = next_clock;
                    return Err(Some(cloned));
                }
                Prepared::Pending { .. } => return Err(None),
                Prepared::RetryableFailure {
                    retry_at, attempts, ..
                } => {
                    if Instant::now() < *retry_at {
                        return Err(None);
                    }
                    prior_attempts = *attempts;
                }
                Prepared::Failed(at) if at.elapsed() < FAILED_RETRY_COOLDOWN => return Err(None),
                Prepared::Failed(_) => {}
            }
        }
        jobs.insert_entry(key.to_string(), Prepared::Pending { prior_attempts });
        Ok(Completer {
            key: key.to_string(),
            needle: needle.to_string(),
            done: jobs.done_sender.clone(),
        })
    });
    match completer {
        Ok(completer) => {
            start(completer);
            None
        }
        Err(ready) => ready,
    }
}

/// Collect finished background images and wake any transient failures whose
/// retry backoff elapsed. Returns the needles of cells/outputs to re-render.
pub fn poll_prepared() -> Vec<String> {
    JOBS.with(|jobs| {
        let mut jobs = jobs.borrow_mut();
        let mut ready = Vec::new();
        while let Ok(Done {
            key,
            needle,
            result,
            retryable,
        }) = jobs.done.try_recv()
        {
            let prior_attempts = match jobs.entries.get(&key).map(|entry| &entry.state) {
                Some(Prepared::Pending { prior_attempts }) => *prior_attempts,
                _ => 0,
            };
            let entry = match result {
                Ok(image) => {
                    ready.push(needle);
                    Prepared::Ready(image)
                }
                Err(_) if retryable && prior_attempts < MAX_TRANSIENT_RETRIES => {
                    let attempts = prior_attempts + 1;
                    let shift = u32::from((attempts - 1).min(4));
                    let backoff = TRANSIENT_RETRY_BASE.saturating_mul(1_u32 << shift);
                    Prepared::RetryableFailure {
                        retry_at: Instant::now() + backoff,
                        attempts,
                        needle,
                        notified: false,
                    }
                }
                Err(_) => Prepared::Failed(Instant::now()),
            };
            jobs.insert_entry(key, entry);
        }
        let now = Instant::now();
        for entry in jobs.entries.values_mut() {
            if let Prepared::RetryableFailure {
                retry_at,
                needle,
                notified,
                ..
            } = &mut entry.state
                && !*notified
                && now >= *retry_at
            {
                *notified = true;
                ready.push(needle.clone());
            }
        }
        ready
    })
}

/// True when any background job is currently pending or waiting on retry backoff.
pub fn has_pending_jobs() -> bool {
    JOBS.with(|jobs| {
        jobs.borrow().entries.values().any(|entry| {
            matches!(
                entry.state,
                Prepared::Pending { .. } | Prepared::RetryableFailure { notified: false, .. }
            )
        })
    })
}

/// A last-in, first-out queue served by bounded worker threads so newly
/// visible cells and outputs jump ahead of older off-screen items.
pub struct WorkQueue<T> {
    items: Mutex<Vec<T>>,
    ready: Condvar,
}

impl<T: Send + 'static> WorkQueue<T> {
    pub fn start(
        name: &str,
        threads: usize,
        batch: usize,
        work: impl Fn(Vec<T>) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let queue = Arc::new(Self {
            items: Mutex::new(Vec::new()),
            ready: Condvar::new(),
        });
        if cfg!(test) {
            return queue;
        }
        let work = Arc::new(work);
        for index in 0..threads.max(1) {
            let (queue, work) = (queue.clone(), work.clone());
            let _ = thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || {
                    loop {
                        let items = queue.take(batch.max(1));
                        work(items);
                    }
                });
        }
        queue
    }

    pub fn push(&self, item: T) {
        if let Ok(mut items) = self.items.lock() {
            items.push(item);
            self.ready.notify_one();
        }
    }

    fn take(&self, batch: usize) -> Vec<T> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while items.is_empty() {
            items = self
                .ready
                .wait(items)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        let keep = items.len().saturating_sub(batch);
        let mut taken = items.split_off(keep);
        taken.reverse();
        taken
    }
}

pub fn worker_threads() -> usize {
    thread::available_parallelism().map_or(2, |count| (count.get() / 2).clamp(1, 4))
}

#[doc(hidden)]
pub fn enable_for_test(cell: (u16, u16)) {
    with(|state| {
        state.mode = Some(ImageMode::Kitty);
        state.enabled = true;
        state.tmux = false;
        state.cell = Some(cell);
        state.layout_epoch = state.layout_epoch.wrapping_add(1);
    });
}

#[doc(hidden)]
pub fn insert_prepared_for_test(key: &str, png: Vec<u8>, cols: u16, rows: u16) {
    JOBS.with(|jobs| {
        jobs.borrow_mut().insert_entry(
            key.to_string(),
            Prepared::Ready(PreparedImage::from_png(&png, cols, rows)),
        );
    });
}

fn query_cell_size() -> Option<(u16, u16)> {
    let size = crossterm::terminal::window_size().ok()?;
    (size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0)
        .then(|| ((size.width / size.columns).max(1), (size.height / size.rows).max(1)))
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

struct TmuxProbe {
    detection: Mutex<Option<Detection>>,
    detection_dirty: AtomicBool,
    pane_visible: AtomicBool,
    client_epoch: AtomicU64,
    wake: (Mutex<bool>, Condvar),
}

impl TmuxProbe {
    fn detection(&self) -> Option<Detection> {
        self.detection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn take_detection_dirty(&self) -> bool {
        self.detection_dirty.swap(false, Ordering::AcqRel)
    }

    fn request_refresh(&self) {
        let (lock, cv) = &self.wake;
        if let Ok(mut pending) = lock.lock() {
            *pending = true;
            cv.notify_one();
        }
    }
}

fn tmux_probe() -> &'static Arc<TmuxProbe> {
    static PROBE: OnceLock<Arc<TmuxProbe>> = OnceLock::new();
    PROBE.get_or_init(|| {
        let probe = Arc::new(TmuxProbe {
            detection: Mutex::new(None),
            detection_dirty: AtomicBool::new(false),
            pane_visible: AtomicBool::new(true),
            client_epoch: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
        });
        if !cfg!(test) {
            let worker_probe = Arc::clone(&probe);
            let _ = thread::Builder::new()
                .name("nb-tmux-probe".to_string())
                .spawn(move || run_tmux_probe(worker_probe));
        }
        probe
    })
}

fn run_tmux_probe(probe: Arc<TmuxProbe>) {
    let initial = detect_tmux();
    *probe
        .detection
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(initial);
    probe.detection_dirty.store(true, Ordering::Release);

    let mut last_client: Option<String> = None;
    loop {
        if let Some(status) = tmux(&[
            "display-message",
            "-p",
            "#{window_active} #{session_attached} #{window_zoomed_flag} #{pane_active}\t#{client_tty} #{client_created}",
        ]) {
            let (flags, client_raw) = status.split_once('\t').unwrap_or((status.as_str(), ""));
            probe
                .pane_visible
                .store(pane_flags_visible(flags), Ordering::Release);
            let client = client_raw.trim();
            if !client.is_empty() {
                let reattached = last_client
                    .as_deref()
                    .is_some_and(|previous| previous != client);
                if reattached {
                    let _ = probe.client_epoch.fetch_add(1, Ordering::AcqRel);
                    let updated = detect_tmux();
                    let changed = {
                        let mut slot = probe
                            .detection
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        let differs = slot.as_ref() != Some(&updated);
                        *slot = Some(updated);
                        differs
                    };
                    if changed {
                        probe.detection_dirty.store(true, Ordering::Release);
                    }
                }
                last_client = Some(client.to_string());
            }
        }

        let (lock, cv) = &probe.wake;
        let guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        let (mut guard, _) = cv
            .wait_timeout_while(guard, TMUX_PROBE_INTERVAL, |pending| !*pending)
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = false;
    }
}

fn tmux(args: &[&str]) -> Option<String> {
    let (subcommand, rest) = args.split_first()?;
    let mut command = std::process::Command::new("tmux");
    command.arg(subcommand);
    if let Ok(pane) = std::env::var("TMUX_PANE") {
        command.args(["-t", &pane]);
    }
    command.args(rest);
    let output = command.stderr(std::process::Stdio::null()).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn pane_flags_visible(flags: &str) -> bool {
    match flags.split_whitespace().collect::<Vec<_>>().as_slice() {
        [window_active, attached, zoomed, pane_active] => {
            *window_active == "1" && *attached != "0" && !(*zoomed == "1" && *pane_active != "1")
        }
        _ => true,
    }
}

fn detect_direct() -> Detection {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return Detection::off("output is not a terminal");
    }
    match terminal_from_env(|name| std::env::var(name).ok().filter(|value| !value.is_empty())) {
        Some((name, version)) => verdict(&name, version.as_deref(), ""),
        None => Detection::off("terminal not recognized"),
    }
}

fn detect_tmux() -> Detection {
    let passthrough = tmux(&["show-options", "-Apqv", "allow-passthrough"]);
    if !matches!(passthrough.as_deref(), Some("on" | "all")) {
        return Detection::off("tmux allow-passthrough is off");
    }
    let Some(info) = tmux(&[
        "display-message",
        "-p",
        "#{client_termtype}\t#{client_termname}\t#{client_termfeatures}",
    ]) else {
        return Detection::off("could not ask tmux about the terminal");
    };
    let mut parts = info.splitn(3, '\t');
    let termtype = parts.next().unwrap_or("");
    let termname = parts.next().unwrap_or("");
    let features = parts.next().unwrap_or("");
    let overrides = tmux(&["show-options", "-g", "terminal-overrides"]).unwrap_or_default();
    if !tmux_passes_truecolor(termname, features, &overrides) {
        return Detection::off("tmux is not passing 24-bit color (RGB / Tc)");
    }
    let tmux_env = tmux_environment_map();
    let env_lookup = |key: &str| {
        if matches!(key, "TERM" | "TERM_PROGRAM") {
            return None;
        }
        tmux_env
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    };
    match tmux_terminal_identity(termtype, termname, env_lookup) {
        Some((name, version)) => verdict(&name, version.as_deref(), " via tmux"),
        None => Detection::off("tmux did not report the terminal"),
    }
}

fn tmux_environment_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(lines) = tmux(&["show-environment"]) {
        for line in lines.lines() {
            if !line.starts_with('-')
                && let Some((key, value)) = line.split_once('=')
            {
                let _ = map.insert(key.to_string(), value.to_string());
            }
        }
    }
    map
}

fn tmux_terminal_identity(
    termtype: &str,
    termname: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Option<(String, Option<String>)> {
    if let Some(found) = parse_xtversion(termtype) {
        return Some(found);
    }
    match termname.trim() {
        "xterm-kitty" => return Some(("kitty".to_string(), None)),
        "xterm-ghostty" | "ghostty" => return Some(("ghostty".to_string(), None)),
        _ => {}
    }
    terminal_from_env(var)
}

fn tmux_passes_truecolor(termname: &str, features: &str, overrides: &str) -> bool {
    let termname = termname.trim();
    if features
        .split(',')
        .any(|feature| matches!(feature.trim(), "RGB" | "Tc"))
    {
        return true;
    }
    if matches!(termname, "xterm-kitty" | "xterm-ghostty" | "ghostty")
        || termname.ends_with("-direct")
    {
        return true;
    }
    for line in overrides.lines() {
        let value = line
            .split_once(char::is_whitespace)
            .map_or(line, |(_, rest)| rest)
            .trim()
            .trim_matches(['"', '\'']);
        for entry in value.split(',').filter(|entry| !entry.is_empty()) {
            let Some((pattern, caps)) = entry.split_once(':') else {
                continue;
            };
            let has_tc = caps
                .split(':')
                .any(|cap| matches!(cap.trim(), "Tc" | "RGB"));
            if has_tc && glob_matches(pattern.trim(), termname) {
                return true;
            }
        }
    }
    false
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(after) = rest.strip_prefix(part) else {
                return false;
            };
            rest = after;
        } else if index + 1 == parts.len() && !pattern.ends_with('*') {
            return rest.ends_with(part);
        } else {
            let Some(pos) = rest.find(part) else {
                return false;
            };
            rest = &rest[pos + part.len()..];
        }
    }
    true
}

fn parse_xtversion(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let split = text.find([' ', '(']).unwrap_or(text.len());
    let name = text[..split].to_string();
    let version = text[split..].trim_matches([' ', '(', ')']).to_string();
    Some((name, (!version.is_empty()).then_some(version)))
}

fn terminal_from_env(var: impl Fn(&str) -> Option<String>) -> Option<(String, Option<String>)> {
    let term = var("TERM").unwrap_or_default();
    let program = var("TERM_PROGRAM").unwrap_or_default();
    if program == "iTerm.app" {
        return Some(("iTerm2".to_string(), var("TERM_PROGRAM_VERSION")));
    }
    if var("LC_TERMINAL").as_deref() == Some("iTerm2") {
        return Some(("iTerm2".to_string(), var("LC_TERMINAL_VERSION")));
    }
    if term == "xterm-kitty" || var("KITTY_WINDOW_ID").is_some() || var("KITTY_PID").is_some() {
        return Some(("kitty".to_string(), None));
    }
    if program.eq_ignore_ascii_case("ghostty")
        || term == "xterm-ghostty"
        || term == "ghostty"
        || var("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return Some(("ghostty".to_string(), var("TERM_PROGRAM_VERSION")));
    }
    (!program.is_empty()).then(|| (program, var("TERM_PROGRAM_VERSION")))
}

fn verdict(name: &str, version: Option<&str>, suffix: &str) -> Detection {
    let label = match version {
        Some(version) => format!("{name} {version}{suffix}"),
        None => format!("{name}{suffix}"),
    };
    let at_least = |minimum: &[u32]| version.is_none_or(|version| version_at_least(version, minimum));
    match name.to_ascii_lowercase().as_str() {
        "kitty" if at_least(&[0, 28]) => Detection::on(label),
        "kitty" => Detection::off(format!("{label} needs 0.28 or newer")),
        "ghostty" => Detection::on(label),
        "iterm2" if version.is_some() && at_least(&[3, 7]) => Detection::on(label),
        "iterm2" => Detection::off(format!("{label} needs 3.7 or newer")),
        _ => Detection::off(format!("{label} is not known to draw kitty placeholders")),
    }
}

fn version_at_least(version: &str, minimum: &[u32]) -> bool {
    let parts: Vec<u32> = version
        .split(['.', '-', '+'])
        .map_while(|part| part.parse().ok())
        .collect();
    parts.as_slice() >= minimum
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::io::Cursor;

    fn png_of(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::GrayscaleAlpha);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![200; (width * height * 2) as usize])
            .unwrap();
        drop(writer);
        out
    }

    #[test]
    fn placeholder_rows_and_stripping_preserve_sent_and_blank_unsent() {
        enable_for_test((10, 20));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        for (row, line) in placeholder_lines(0x01_02_03, 3, 1, 0).iter().enumerate() {
            buffer.set_line(0, row as u16, line, 10);
        }
        for (row, line) in placeholder_lines(0x04_05_06, 3, 1, 0).iter().enumerate() {
            buffer.set_line(0, (row + 1) as u16, line, 10);
        }
        let (visible, hidden) = collect_and_strip_placeholders(&mut buffer, |id| id == 0x01_02_03);
        assert_eq!(visible, vec![0x01_02_03, 0x04_05_06]);
        assert_eq!(hidden, vec![0x01_02_03]);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert!(buffer[(0, 1)].symbol().starts_with(PLACEHOLDER));
    }

    #[test]
    fn wrapped_tmux_packets_fit_within_posix_pipe_buf() {
        let base64 = "A".repeat(20_000);
        let packets = build_packets(0x12_34_56, &base64, 12, 3, true);
        assert!(packets.len() >= 7);
        for (idx, pkt) in packets.iter().enumerate() {
            let wire_len = pkt.len() + if idx == 0 { 6 } else { 0 };
            assert!(
                wire_len < 4096,
                "packet {idx} wire length {wire_len} must stay below 4096-byte PIPE_BUF"
            );
        }
    }

    #[test]
    fn fitted_png_and_svg_rasterize_cleanly() {
        let (width, height, rgba) = decode_image(&png_of(25, 21)).unwrap();
        let fitted = fit_rgba(width, height, &rgba, (10, 20)).unwrap();
        assert_eq!((fitted.cols, fitted.rows), (3, 2));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&*fitted.base64)
            .unwrap();
        let decoder = png::Decoder::new(Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (30, 40));

        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"><rect width="120" height="60" fill="#3366ff"/></svg>"##;
        let (sw, sh, srgba) = rasterize_svg(svg, (10, 20), 40, 18, 1.0).unwrap();
        let svg_fitted = fit_rgba(sw, sh, &srgba, (10, 20)).unwrap();
        assert_eq!((svg_fitted.cols, svg_fitted.rows), (12, 3));
    }

    #[test]
    fn terminal_detection_covers_kitty_ghostty_iterm2_ssh_and_tmux_truecolor() {
        assert!(verdict("iTerm2", Some("3.7.7"), " via tmux").supported);
        assert!(!verdict("iTerm2", Some("3.6.9"), "").supported);
        assert!(verdict("kitty", Some("0.35.2"), "").supported);
        assert!(verdict("ghostty", Some("1.1.3"), "").supported);
        assert!(tmux_passes_truecolor("xterm-256color", "", "terminal-overrides[0] xterm*:Tc"));
        assert!(tmux_passes_truecolor("alacritty", "256,RGB,mouse", ""));
        assert!(!tmux_passes_truecolor("xterm-256color", "256,mouse", ""));
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(
            terminal_from_env(env(&[
                ("LC_TERMINAL", "iTerm2"),
                ("LC_TERMINAL_VERSION", "3.7.7")
            ])),
            Some(("iTerm2".into(), Some("3.7.7".into())))
        );
    }
}
