//! Display and inline LaTeX math drawn as terminal images.
//!
//! `$$…$$` and `\[…\]` blocks on their own lines, ```math / ```latex fences,
//! and `text/latex` MIME outputs become centered display images; `$…$`,
//! `\(…\)`, and inline `$$…$$` within prose or tables become one-row inline
//! images. Formulas are typeset in batches with `latex` and `dvipng` on bounded
//! background workers (`nb-math`), cached on disk by SHA-256 of formula + DPI
//! using a theme-neutral sentinel ink so theme switches recolor in memory
//! without re-running TeX.

use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nb_core::codec::write_atomic;
use nb_sdk::Theme;
use ratatui::text::{Line, Span};
use sha2::{Digest, Sha256};

use crate::images;

const MAX_TEX_BYTES: usize = 4000;
const MAX_BLOCK_LINES: usize = 60;
const BATCH: usize = 24;
const MATH_MAX_WORKERS: usize = 2;
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const TEMPLATE_VERSION: u32 = 2;
const DEFAULT_INK_SENTINEL: (u8, u8, u8) = (1, 3, 2);
const NUMBERED_ENVIRONMENTS: [&str; 7] = [
    "equation", "align", "gather", "multline", "flalign", "alignat", "eqnarray",
];
const BLOCKED_COMMANDS: [&str; 26] = [
    "input",
    "include",
    "includeonly",
    "endinput",
    "openin",
    "openout",
    "read",
    "readline",
    "write",
    "immediate",
    "newread",
    "newwrite",
    "catcode",
    "csname",
    "def",
    "edef",
    "gdef",
    "xdef",
    "let",
    "futurelet",
    "special",
    "directlua",
    "scantokens",
    "everyjob",
    "usepackage",
    "documentclass",
];

const SENTINEL_BASE: u32 = 0xF_0000;
const MAX_INLINE_FORMULAS: usize = 0xFFFD;

#[derive(Debug, Clone)]
struct Formula {
    tex: String,
    inline: bool,
    ink: (u8, u8, u8),
    cell: (u16, u16),
}

impl Formula {
    fn key(&self) -> String {
        format!(
            "math\0{}\0{:?}\0{:?}\0{}",
            self.inline, self.ink, self.cell, self.tex
        )
    }
}

/// Whether `body` may contain math expressions or math code fences.
pub fn may_contain_math(body: &str) -> bool {
    body.contains('$')
        || body.contains("\\(")
        || body.contains("\\[")
        || body.lines().any(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed
                .strip_prefix("```")
                .or_else(|| trimmed.strip_prefix("~~~"));
            rest.is_some_and(|info| {
                let lang = info
                    .trim_start_matches(['`', '~', ' ', '\t'])
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                is_math_fence_lang(lang)
            })
        })
}

pub fn is_math_fence_lang(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "math" | "latex" | "tex"
    )
}

pub fn is_sentinel(ch: char) -> bool {
    sentinel_index(ch).is_some()
}

/// Placeholder lines drawing a display formula centered in `width` columns, or
/// `None` while pending or when TeX fails / images are off.
pub fn display_block_lines(tex: &str, width: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    display_block_lines_with_needle(tex, width, theme, None)
}

pub fn display_block_lines_with_needle(
    tex: &str,
    width: usize,
    theme: &Theme,
    needle_override: Option<&str>,
) -> Option<Vec<Line<'static>>> {
    let image = ready_image(tex, false, width, theme, needle_override)?;
    let id = images::place_image(&image)?;
    let indent = width.saturating_sub(image.cols as usize) / 2;
    Some(images::placeholder_lines_for(id, &image, indent))
}

/// Extract a standalone formula from a Jupyter `text/latex` MIME payload such
/// as `"$\\displaystyle \\int x^2 dx$"`, `"$$E=mc^2$$"`, `"\\[a+b\\]"`, or
/// `"\\begin{align*}…\\end{align*}"`.
pub fn unwrap_latex_mime(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(inner) = trimmed
        .strip_prefix("$$")
        .and_then(|rest| rest.strip_suffix("$$"))
    {
        let inner = inner.trim();
        if !inner.is_empty() && !inner.contains("$$") {
            return Some(inner);
        }
    }
    if let Some(inner) = trimmed
        .strip_prefix("\\[")
        .and_then(|rest| rest.strip_suffix("\\]"))
    {
        let inner = inner.trim();
        if !inner.is_empty() && !inner.contains("\\[") && !inner.contains("\\]") {
            return Some(inner);
        }
    }
    if let Some(inner) = trimmed
        .strip_prefix("\\(")
        .and_then(|rest| rest.strip_suffix("\\)"))
    {
        let inner = inner.trim();
        if !inner.is_empty() && !inner.contains("\\(") && !inner.contains("\\)") {
            return Some(inner);
        }
    }
    if let Some(inner) = trimmed
        .strip_prefix('$')
        .and_then(|rest| rest.strip_suffix('$'))
        && !inner.starts_with('$')
        && !inner.ends_with('$')
    {
        let inner = inner.trim();
        if !inner.is_empty() && !inner.contains('$') {
            return Some(inner);
        }
    }
    if trimmed.starts_with("\\begin{") {
        return Some(trimmed);
    }
    // Bare math without dollar delimiters (e.g. custom `_repr_latex_` returning TeX).
    (!trimmed.contains('$')).then_some(trimmed)
}

fn ready_image(
    tex: &str,
    inline: bool,
    width: usize,
    theme: &Theme,
    needle_override: Option<&str>,
) -> Option<images::PreparedImage> {
    if !images::enabled() {
        return None;
    }
    let tex = normalize(tex);
    if tex.is_empty() || !renderable(&tex) {
        return None;
    }
    let needle = needle_override
        .map(str::to_owned)
        .unwrap_or_else(|| formula_needle(&tex));
    let formula = Formula {
        tex,
        inline,
        ink: images::theme_ink_rgb(theme),
        cell: images::cell_size(),
    };
    let key = formula.key();
    let image = images::prepared(&key, &needle, move |completer| {
        queue().push(Request { formula, completer });
    })?;
    (image.cols as usize <= width).then_some(image)
}

fn formula_needle(tex: &str) -> String {
    let tex = tex.strip_prefix("\\displaystyle ").unwrap_or(tex);
    tex.lines()
        .max_by_key(|line| line.len())
        .unwrap_or(tex)
        .to_string()
}

struct Request {
    formula: Formula,
    completer: images::Completer,
}

fn queue() -> &'static images::WorkQueue<Request> {
    static QUEUE: OnceLock<Arc<images::WorkQueue<Request>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let workers = images::worker_threads().min(MATH_MAX_WORKERS).max(1);
        images::WorkQueue::start("nb-math", workers, BATCH, render_batch)
    })
}

/// Parse a display formula (`$$…$$` or `\[…\]`) opening at `lines[start]`:
/// returns `(tex, last_index)` inclusive.
pub fn display_block_in_visible(lines: &[(String, usize)], start: usize) -> Option<(String, usize)> {
    let raw: Vec<&str> = lines[start..]
        .iter()
        .take(MAX_BLOCK_LINES + 1)
        .map(|(text, _)| text.as_str())
        .collect();
    display_block(&raw, 0).map(|(tex, rel_end)| (tex, start + rel_end))
}

pub fn display_block(lines: &[&str], start: usize) -> Option<(String, usize)> {
    let first = lines.get(start)?.trim().trim_end_matches('\r');
    let (open, close) = if first.starts_with("$$") {
        ("$$", "$$")
    } else if first.starts_with("\\[") {
        ("\\[", "\\]")
    } else {
        return None;
    };
    let rest = &first[open.len()..];
    if let Some(inner) = rest.strip_suffix(close) {
        let inner = inner.trim();
        return (!inner.is_empty() && !inner.contains(open) && !inner.contains(close))
            .then(|| (inner.to_string(), start));
    }
    if rest.contains(close) {
        return None;
    }
    let mut tex: Vec<&str> = Vec::new();
    if !rest.trim().is_empty() {
        tex.push(rest.trim());
    }
    for (index, line) in lines.iter().enumerate().skip(start + 1).take(MAX_BLOCK_LINES) {
        let line = line.trim().trim_end_matches('\r');
        if line.is_empty() || line.starts_with("```") || line.starts_with("~~~") {
            return None;
        }
        if let Some(before) = line.strip_suffix(close) {
            if before.contains(close) || before.contains(open) {
                return None;
            }
            if !before.trim().is_empty() {
                tex.push(before.trim());
            }
            return (!tex.is_empty()).then(|| (tex.join("\n"), index));
        }
        if line.contains(close) {
            return None;
        }
        tex.push(line);
    }
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InlineImages(Vec<(u32, u16)>);

impl InlineImages {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Replace each ready 1-row inline formula in `body` with a run of `image.cols`
/// sentinel characters so word-wrapping treats the formula as a single word of
/// exact column width; `place_inline_math` swaps sentinels for placeholder cells.
pub fn substitute_inline_math<'a>(
    body: &'a str,
    max_cols: usize,
    theme: &Theme,
) -> (Cow<'a, str>, InlineImages) {
    let mut found = InlineImages::default();
    if !images::enabled()
        || !(body.contains('$') || body.contains("\\("))
        || body.chars().any(is_sentinel)
    {
        return (Cow::Borrowed(body), found);
    }
    let max_cols = max_cols.max(1);
    let mut out = String::with_capacity(body.len());
    let mut fence = images::FenceTracker::default();
    for (number, line) in body.split('\n').enumerate() {
        if number > 0 {
            out.push('\n');
        }
        let trimmed = line.trim();
        if fence.step(line) || trimmed.starts_with("$$") || trimmed.starts_with("\\[") {
            out.push_str(line);
            continue;
        }
        let mut copied = 0;
        for (range, tex) in inline_formulas(line) {
            if found.0.len() >= MAX_INLINE_FORMULAS {
                break;
            }
            let Some(image) =
                ready_image(&tex, true, max_cols, theme, None).filter(|img| img.rows == 1)
            else {
                continue;
            };
            let Some(id) = images::place_image(&image) else {
                continue;
            };
            let sentinel = char::from_u32(SENTINEL_BASE + found.0.len() as u32)
                .expect("private-use code point");
            out.push_str(&line[copied..range.start]);
            out.extend(std::iter::repeat_n(sentinel, image.cols as usize));
            found.0.push((id, image.cols));
            copied = range.end;
        }
        out.push_str(&line[copied..]);
    }
    if found.is_empty() {
        (Cow::Borrowed(body), found)
    } else {
        (Cow::Owned(out), found)
    }
}

fn sentinel_index(ch: char) -> Option<usize> {
    let code = ch as u32;
    (SENTINEL_BASE..SENTINEL_BASE + MAX_INLINE_FORMULAS as u32)
        .contains(&code)
        .then(|| (code - SENTINEL_BASE) as usize)
}

/// Swap sentinel runs produced by `substitute_inline_math` for kitty Unicode
/// placeholder cells (`row = 0`, `col = 0..cols`), preserving any span `bg`.
pub fn place_inline_math(lines: &mut [Line<'static>], inline: &InlineImages) {
    if inline.is_empty() {
        return;
    }
    let mut next_col = vec![0_usize; inline.0.len()];
    for line in lines.iter_mut() {
        let has_sentinel = |span: &Span<'_>| span.content.chars().any(is_sentinel);
        if !line.spans.iter().any(has_sentinel) {
            continue;
        }
        let mut spans = Vec::with_capacity(line.spans.len() + 2);
        for span in std::mem::take(&mut line.spans) {
            if !has_sentinel(&span) {
                spans.push(span);
                continue;
            }
            let mut text = String::new();
            let mut cells: Option<(usize, String)> = None;
            let flush_cells =
                |cells: &mut Option<(usize, String)>, spans: &mut Vec<Span<'static>>| {
                    if let Some((index, content)) = cells.take() {
                        let (id, _) = inline.0[index];
                        let mut style = images::image_style(id);
                        style.bg = span.style.bg;
                        spans.push(Span::styled(content, style));
                    }
                };
            for ch in span.content.chars() {
                match sentinel_index(ch).filter(|index| *index < inline.0.len()) {
                    Some(index) => {
                        if !text.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut text), span.style));
                        }
                        if cells.as_ref().is_some_and(|(current, _)| *current != index) {
                            flush_cells(&mut cells, &mut spans);
                        }
                        let col = next_col[index];
                        next_col[index] += 1;
                        let entry = cells.get_or_insert_with(|| (index, String::new()));
                        images::push_placeholder_cell(&mut entry.1, 0, col);
                    }
                    None => {
                        flush_cells(&mut cells, &mut spans);
                        text.push(ch);
                    }
                }
            }
            flush_cells(&mut cells, &mut spans);
            if !text.is_empty() {
                spans.push(Span::styled(text, span.style));
            }
        }
        line.spans = spans;
    }
}

fn inline_formulas(line: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if bytes.get(index + 1) == Some(&b'(') => {
                let start = index + 2;
                match line[start..].find("\\)") {
                    Some(length) => {
                        let tex = line[start..start + length].trim();
                        if !tex.is_empty() {
                            found.push((index..start + length + 2, tex.to_string()));
                        }
                        index = start + length + 2;
                    }
                    None => index = start,
                }
            }
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => {
                let run = bytes[index..].iter().take_while(|byte| **byte == b'`').count();
                let fence = &line[index..index + run];
                index = match line[index + run..].find(fence) {
                    Some(offset) => index + run + offset + run,
                    None => index + run,
                };
            }
            b'$' if bytes.get(index + 1) == Some(&b'$') => {
                let start = index + 2;
                match line[start..].find("$$") {
                    Some(length) => {
                        let tex = line[start..start + length].trim();
                        if !tex.is_empty() {
                            found.push((index..start + length + 2, format!("\\displaystyle {tex}")));
                        }
                        index = start + length + 2;
                    }
                    None => index = start,
                }
            }
            b'$' => match inline_dollar_len(&line[index..]) {
                Some(length) => {
                    found.push((
                        index..index + length,
                        line[index + 1..index + length - 1].to_string(),
                    ));
                    index += length;
                }
                None => index += 1,
            },
            _ => index += 1,
        }
    }
    found
}

fn inline_dollar_len(text: &str) -> Option<usize> {
    let rest = &text[1..];
    let first = rest.chars().next()?;
    if first.is_whitespace() {
        return None;
    }
    let mut previous = first;
    let mut escaped = first == '\\';
    for (offset, ch) in rest.char_indices().skip(1) {
        if ch == '$' && !escaped {
            let next = rest[offset + 1..].chars().next();
            if !previous.is_whitespace() && !next.is_some_and(|next| next.is_ascii_digit()) {
                return Some(1 + offset + 1);
            }
        }
        escaped = ch == '\\' && !escaped;
        previous = ch;
    }
    None
}

fn normalize(tex: &str) -> String {
    tex.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn renderable(tex: &str) -> bool {
    if tex.len() > MAX_TEX_BYTES || tex.contains("^^") || tex.contains("{document}") {
        return false;
    }
    let mut rest = tex;
    while let Some(position) = rest.find('\\') {
        rest = &rest[position + 1..];
        let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        if BLOCKED_COMMANDS.contains(&&rest[..name_len]) {
            return false;
        }
        let skip = if name_len > 0 {
            name_len
        } else {
            rest.chars().next().map_or(0, char::len_utf8)
        };
        rest = &rest[skip..];
    }
    true
}

fn dpi_for_cell(cell: (u16, u16)) -> u32 {
    (f64::from(cell.1) * 6.6).round().clamp(60.0, 600.0) as u32
}

struct Tools {
    latex: PathBuf,
    dvipng: PathBuf,
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Option<Tools>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            Some(Tools {
                latex: find_program("latex")?,
                dvipng: find_program("dvipng")?,
            })
        })
        .as_ref()
}

fn cache_directory() -> Option<&'static Path> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let base = std::env::var_os("NB_STATE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("XDG_CACHE_HOME")
                        .filter(|v| !v.is_empty())
                        .map(|p| PathBuf::from(p).join("notebook"))
                })
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|v| !v.is_empty())
                        .map(|p| PathBuf::from(p).join(".cache").join("notebook"))
                })?;
            Some(base.join("math"))
        })
        .as_deref()
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut extras = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for arch in [
            "x86_64-linux",
            "aarch64-linux",
            "universal-darwin",
            "x86_64-darwin",
        ] {
            extras.push(home.join(".TinyTeX/bin").join(arch));
        }
    }
    extras.extend(
        [
            "/Library/TeX/texbin",
            "/usr/texbin",
            "/opt/texbin",
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/usr/bin",
        ]
        .map(PathBuf::from),
    );
    std::env::split_paths(&path)
        .chain(extras)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl Formula {
    fn dpi(&self) -> u32 {
        dpi_for_cell(self.cell)
    }

    fn max_height(&self) -> u32 {
        if self.inline { u32::from(self.cell.1) } else { 0 }
    }

    fn cache_path(&self) -> Option<PathBuf> {
        let digest = Sha256::digest(format!(
            "{TEMPLATE_VERSION}\0{}\0{}\0{}\0{}",
            self.inline,
            self.max_height(),
            self.dpi(),
            self.tex
        ));
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        cache_directory().map(|directory| directory.join(format!("{name}.png")))
    }
}

struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn create() -> Result<Self, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0_u128, |d| d.as_nanos());
        for _ in 0..32 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("nb-math-{}-{nanos}-{seq}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self { path: candidate }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("no temporary directory: {err}")),
            }
        }
        Err("no temporary directory: exhausted retries".to_string())
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn render_batch(requests: Vec<Request>) {
    let mut todo = Vec::with_capacity(requests.len());
    for request in requests {
        match request.formula.cache_path().and_then(|path| fs::read(path).ok()) {
            Some(png) => finish(request, Ok(png)),
            None => todo.push(request),
        }
    }
    let Some(tools) = tools() else {
        for request in todo {
            request
                .completer
                .finish(Err("latex and dvipng were not found".to_string()));
        }
        return;
    };
    while let Some(first) = todo.first() {
        let dpi = first.formula.dpi();
        let (group, rest): (Vec<_>, Vec<_>) =
            todo.into_iter().partition(|request| request.formula.dpi() == dpi);
        todo = rest;
        render_dpi_group(tools, group, dpi);
    }
}

fn render_dpi_group(tools: &Tools, mut group: Vec<Request>, dpi: u32) {
    let formulas: Vec<&Formula> = group.iter().map(|request| &request.formula).collect();
    match typeset(tools, &formulas, dpi) {
        Ok(pngs) => {
            for (request, png) in group.into_iter().zip(pngs) {
                if let Some(path) = request.formula.cache_path() {
                    store(&path, &png);
                }
                finish(request, Ok(png));
            }
        }
        Err(_) if group.len() > 1 => {
            let right = group.split_off(group.len() / 2);
            render_dpi_group(tools, group, dpi);
            render_dpi_group(tools, right, dpi);
        }
        Err(error) => {
            for request in group {
                finish(request, Err(error.clone()));
            }
        }
    }
}

fn is_retryable_tex_error(error: &str) -> bool {
    error.contains("timed out")
        || error.contains("could not start")
        || error.contains("no temporary directory")
        || error.contains("could not write")
}

fn finish(request: Request, png: Result<Vec<u8>, String>) {
    let inline = request.formula.inline;
    let max_h = request.formula.max_height() as usize;
    let result = png.and_then(|png| {
        let (mut width, mut height, mut rgba) = images::decode_image(&png)?;
        recolor_default_ink(&mut rgba, request.formula.ink);
        if inline && max_h > 0 && height > max_h {
            let scaled_w = (width * max_h / height).max(1);
            rgba = images::resize_rgba(width, height, rgba, scaled_w, max_h)?;
            width = scaled_w;
            height = max_h;
        }
        images::fit_rgba(width, height, &rgba, request.formula.cell)
    });
    let retryable = result
        .as_ref()
        .err()
        .is_some_and(|error| is_retryable_tex_error(error));
    request.completer.finish_with_retry(result, retryable);
}

fn recolor_default_ink(rgba: &mut [u8], ink: (u8, u8, u8)) {
    for pixel in rgba.chunks_exact_mut(4) {
        if pixel[3] == 0 {
            pixel.copy_from_slice(&[0, 0, 0, 0]);
        } else if (pixel[0], pixel[1], pixel[2]) == DEFAULT_INK_SENTINEL {
            pixel[0] = ink.0;
            pixel[1] = ink.1;
            pixel[2] = ink.2;
        }
    }
}

fn store(path: &Path, png: &[u8]) {
    if let Some(directory) = path.parent()
        && fs::create_dir_all(directory).is_ok()
    {
        let _ = write_atomic(path, png);
    }
}

fn typeset(tools: &Tools, formulas: &[&Formula], dpi: u32) -> Result<Vec<Vec<u8>>, String> {
    let directory = TempWorkDir::create()?;
    let work = &directory.path;
    let bodies: Vec<String> = formulas
        .iter()
        .map(|formula| formula_body(&formula.tex, formula.inline))
        .collect();
    fs::write(work.join("formula.tex"), pages_document(&bodies))
        .map_err(|error| format!("could not write formula.tex: {error}"))?;
    let mut latex = Command::new(&tools.latex);
    latex
        .args([
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-no-shell-escape",
            "formula.tex",
        ])
        .current_dir(work)
        .env("openin_any", "p")
        .env("openout_any", "p");
    run(&mut latex, "latex")?;
    let mut dvipng = dvipng_command(tools, work, dpi);
    dvipng.args(["-o", "page%d.png", "formula.dvi"]);
    run(&mut dvipng, "dvipng")?;
    let mut pngs = Vec::with_capacity(formulas.len());
    for (index, formula) in formulas.iter().enumerate() {
        let page = index + 1;
        let png = fs::read(work.join(format!("page{page}.png")))
            .map_err(|_| format!("dvipng wrote {} of {} pages", pngs.len(), formulas.len()))?;
        pngs.push(fit_height(
            tools,
            work,
            png,
            page,
            dpi,
            formula.max_height(),
        )?);
    }
    if work.join(format!("page{}.png", formulas.len() + 1)).exists() {
        return Err("dvipng wrote more pages than formulas".to_string());
    }
    Ok(pngs)
}

fn fit_height(
    tools: &Tools,
    work: &Path,
    png: Vec<u8>,
    page: usize,
    dpi: u32,
    max_height: u32,
) -> Result<Vec<u8>, String> {
    if max_height == 0 {
        return Ok(png);
    }
    let (_, height, _) = images::decode_image(&png)?;
    if height as u32 <= max_height {
        return Ok(png);
    }
    let smaller = (u64::from(dpi) * u64::from(max_height) / height as u64).max(20) as u32;
    let mut dvipng = dvipng_command(tools, work, smaller);
    dvipng.args(["-pp", &page.to_string(), "-o", "refit.png", "formula.dvi"]);
    run(&mut dvipng, "dvipng")?;
    fs::read(work.join("refit.png")).map_err(|error| format!("dvipng wrote no image: {error}"))
}

fn dvipng_command(tools: &Tools, work: &Path, dpi: u32) -> Command {
    let mut dvipng = Command::new(&tools.dvipng);
    dvipng
        .args(["-q", "-T", "tight", "-D", &dpi.to_string()])
        .args(["-bg", "Transparent", "-z", "9"])
        .current_dir(work);
    dvipng
}

fn run(command: &mut Command, what: &str) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start {what}: {error}"))?;
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{what} failed ({status})")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what} timed out"));
            }
            Err(error) => return Err(format!("{what}: {error}")),
        }
    }
}

fn formula_body(tex: &str, inline: bool) -> String {
    if inline {
        format!("${tex}$")
    } else {
        display_body(tex)
    }
}

fn pages_document(bodies: &[String]) -> String {
    let (r, g, b) = DEFAULT_INK_SENTINEL;
    let reset_bodies: Vec<String> = bodies
        .iter()
        .map(|body| format!("{{\\color{{nbdefaultink}}\n{body}\n}}"))
        .collect();
    format!(
        "\\documentclass{{article}}\n\
         \\usepackage{{amsmath,amssymb}}\n\
         \\usepackage[dvipsnames,svgnames]{{xcolor}}\n\
         \\definecolor{{nbdefaultink}}{{RGB}}{{{r},{g},{b}}}\n\
         \\color{{nbdefaultink}}\n\
         \\pagestyle{{empty}}\n\
         \\begin{{document}}\n{}\n\\end{{document}}\n",
        reset_bodies.join("\n\\clearpage\n")
    )
}

fn display_body(tex: &str) -> String {
    let environment = tex
        .strip_prefix("\\begin{")
        .and_then(|rest| rest.split_once('}'))
        .map(|(name, _)| name.trim_end_matches('*'));
    match environment {
        Some(name) if NUMBERED_ENVIRONMENTS.contains(&name) => star_environment(tex, name),
        _ if tex.contains("\\\\") && environment.is_none() => {
            format!("\\[\n\\begin{{gathered}}\n{tex}\n\\end{{gathered}}\n\\]")
        }
        _ => format!("\\[\n{tex}\n\\]"),
    }
}

fn star_environment(tex: &str, name: &str) -> String {
    tex.replace(&format!("\\begin{{{name}}}"), &format!("\\begin{{{name}*}}"))
        .replace(&format!("\\end{{{name}}}"), &format!("\\end{{{name}*}}"))
}

#[doc(hidden)]
pub fn insert_ready_for_test(
    tex: &str,
    inline: bool,
    theme: &Theme,
    png: Vec<u8>,
    cols: u16,
    rows: u16,
) {
    let formula = Formula {
        tex: normalize(tex),
        inline,
        ink: images::theme_ink_rgb(theme),
        cell: images::cell_size(),
    };
    images::insert_prepared_for_test(&formula.key(), png, cols, rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_inline_formulas_parse_and_block_unsafe_commands() {
        let lines = ["$$", "a + b", "  = c", "$$", "after"];
        assert_eq!(display_block(&lines, 0), Some(("a + b\n= c".to_string(), 3)));
        assert_eq!(display_block(&["$$x^2$$"], 0), Some(("x^2".to_string(), 0)));
        assert_eq!(
            unwrap_latex_mime("$\\displaystyle \\int_0^1 x^2 dx$"),
            Some("\\displaystyle \\int_0^1 x^2 dx")
        );
        assert!(renderable("\\frac{a}{b}"));
        assert!(!renderable("\\input{/etc/passwd}"));
        assert!(!renderable("\\def\\x{1}"));
        let texts = |line: &str| {
            inline_formulas(line)
                .into_iter()
                .map(|(_, tex)| tex)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts("Let $x^2$ and \\(y\\) hold."), ["x^2", "y"]);
        assert!(texts("Costs $5 and $10 today").is_empty());
    }

    #[test]
    fn latex_typesets_when_installed() {
        let Some(tools) = tools() else {
            return;
        };
        let formula = Formula {
            tex: normalize("E = mc^2 + \\textcolor{red}{\\Delta}"),
            inline: false,
            ink: (20, 40, 60),
            cell: (12, 24),
        };
        let mut pngs = typeset(tools, &[&formula], 158).expect("latex + dvipng succeeds");
        let (w, h, mut rgba) = images::decode_image(&pngs.remove(0)).unwrap();
        assert!(w > 0 && h > 0);
        recolor_default_ink(&mut rgba, formula.ink);
        assert!(
            rgba.chunks_exact(4)
                .any(|p| p[3] > 180 && (p[0], p[1], p[2]) == (20, 40, 60))
        );
    }
}
