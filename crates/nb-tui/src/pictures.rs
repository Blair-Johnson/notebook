//! Markdown pictures and Jupyter MIME image/SVG payloads drawn as terminal images.
//!
//! Standalone markdown images `![alt](source)` (local files, `file://` URLs,
//! and `data:image/…` URIs) as well as rich MIME outputs (`image/png`,
//! `image/jpeg`, `image/gif`, `image/webp`, `image/svg+xml`) are decoded,
//! rasterized, optionally matted for `needs_background`, and scaled to fit cell
//! geometry on background worker threads (`nb-pictures`), newest request first.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant, UNIX_EPOCH},
};

use base64::Engine as _;
use nb_sdk::Theme;
use ratatui::text::Line;

use crate::images::{self, NeedsBackground};

/// Standard row budget for landscape/square figures (`width >= height`), chosen
/// to fit cleanly inside `display.max_output_rows = 18` without clipping axes.
pub const STANDARD_IMAGE_ROWS: usize = 18;
/// Expanded row budget for tall figures (`height > width`, such as stacked
/// multi-panel subplots) so their horizontal resolution stays readable.
pub const TALL_IMAGE_ROWS: usize = 36;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const WIDTH_STEP: usize = 8;
const FILE_STAMP_TTL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub enum RichImagePayload {
    /// Base64-encoded PNG, JPEG, GIF, or WebP bytes.
    RasterBase64(Arc<str>),
    /// Raw UTF-8 SVG XML document.
    SvgXml(Arc<str>),
}

enum Source {
    File(PathBuf),
    RasterBase64(Arc<str>),
    SvgBytes(Vec<u8>),
}

/// Whether `body` may contain a markdown image `![alt](src)`.
pub fn may_contain_pictures(body: &str) -> bool {
    body.contains("![")
}

/// Extract `source` when `line` (already trimmed) consists solely of a single
/// markdown image `![alt](source)` or `![alt](source "title")`.
pub fn block_image_source(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("![")?.strip_suffix(')')?;
    let (alt, destination) = rest.split_once("](")?;
    if alt.contains(']') {
        return None;
    }
    let destination = destination.trim();
    let source = match destination.strip_prefix('<') {
        Some(bracketed) => {
            let (inner, after) = bracketed.split_once('>')?;
            if after.contains(['(', ')']) {
                return None;
            }
            inner
        }
        None => {
            if destination.contains(['(', ')']) {
                return None;
            }
            destination.split_whitespace().next()?
        }
    };
    (!source.is_empty()).then_some(source)
}

/// Placeholder lines for a standalone markdown image `![alt](source)`, centered
/// in `width` columns, or `None` while loading or when unsupported/disabled.
pub fn picture_lines(
    source_text: &str,
    width: usize,
    base_dir: Option<&Path>,
    theme: &Theme,
) -> Option<Vec<Line<'static>>> {
    let image = ready_markdown_picture(source_text.trim(), width, base_dir, theme)?;
    let id = images::place_image(&image)?;
    let indent = width.saturating_sub(image.cols as usize) / 2;
    Some(images::placeholder_lines_for(id, &image, indent))
}

/// Placeholder lines for a rich Jupyter MIME output (`image/png`, `image/jpeg`,
/// `image/gif`, `image/webp`, `image/svg+xml`).
pub fn mime_image_lines(
    payload_hash: u64,
    payload: &RichImagePayload,
    needs_bg: NeedsBackground,
    width: usize,
    theme: &Theme,
    needle: &str,
) -> Option<Vec<Line<'static>>> {
    let image = ready_mime_image(payload_hash, payload, needs_bg, width, theme, needle)?;
    let id = images::place_image(&image)?;
    Some(images::placeholder_lines_for(id, &image, 0))
}

fn parse_markdown_source(text: &str, base_dir: Option<&Path>) -> Option<Source> {
    if let Some(data) = text.strip_prefix("data:image/") {
        if let Some((meta, payload)) = data.split_once(";base64,") {
            let trimmed = strip_ascii_whitespace(payload);
            if meta.starts_with("svg+xml") {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(trimmed.as_bytes())
                    .ok()?;
                return Some(Source::SvgBytes(bytes));
            }
            return Some(Source::RasterBase64(trimmed.into()));
        }
        if let Some(rest) = data.strip_prefix("svg+xml") {
            let (_, raw_svg) = rest.split_once(',')?;
            let decoded = percent_decode(raw_svg).unwrap_or_else(|| raw_svg.to_string());
            return Some(Source::SvgBytes(decoded.into_bytes()));
        }
        return None;
    }
    if let Some(path) = text.strip_prefix("file://") {
        return Some(Source::File(PathBuf::from(percent_decode(path)?)));
    }
    if text.contains("://") || text.starts_with("data:") {
        // Remote URLs are never fetched so viewing a notebook never phones home.
        return None;
    }
    let path = match text.strip_prefix("~/") {
        Some(rest) => home_dir()?.join(rest),
        None => PathBuf::from(text),
    };
    if path.is_absolute() {
        return Some(Source::File(path));
    }
    if let Some(dir) = base_dir {
        let candidate = dir.join(&path);
        if candidate.exists() {
            return Some(Source::File(candidate));
        }
    }
    if let Some(candidate) = NOTEBOOK_DIR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|dir| dir.join(&path))
            .filter(|p| p.exists())
    }) {
        return Some(Source::File(candidate));
    }
    Some(Source::File(initial_cwd()?.join(path)))
}

pub fn strip_ascii_whitespace(text: &str) -> String {
    if text.bytes().any(|b| b.is_ascii_whitespace()) {
        text.chars().filter(|c| !c.is_ascii_whitespace()).collect()
    } else {
        text.to_string()
    }
}

fn home_dir() -> Option<&'static PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| std::env::var_os("HOME").map(PathBuf::from))
        .as_ref()
}

fn initial_cwd() -> Option<&'static PathBuf> {
    static CWD: OnceLock<Option<PathBuf>> = OnceLock::new();
    CWD.get_or_init(|| std::env::current_dir().ok()).as_ref()
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

thread_local! {
    static FILE_STAMPS: RefCell<HashMap<PathBuf, (Instant, u64)>> = RefCell::new(HashMap::new());
    static NOTEBOOK_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn set_notebook_dir(dir: Option<PathBuf>) {
    NOTEBOOK_DIR.with(|slot| *slot.borrow_mut() = dir);
}

fn cached_file_stamp(path: &Path) -> u64 {
    FILE_STAMPS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((checked_at, stamp)) = cache.get(path)
            && checked_at.elapsed() < FILE_STAMP_TTL
        {
            return *stamp;
        }
        let stamp = fs::metadata(path)
            .ok()
            .map(|meta| {
                let nanos = meta
                    .modified()
                    .ok()
                    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                    .map_or(0_u128, |dur| dur.as_nanos());
                let mut hasher = DefaultHasher::new();
                meta.len().hash(&mut hasher);
                nanos.hash(&mut hasher);
                hasher.finish()
            })
            .unwrap_or(0);
        if cache.len() >= 512 {
            cache.clear();
        }
        let _ = cache.insert(path.to_path_buf(), (Instant::now(), stamp));
        stamp
    })
}

fn quantize_cols(width: usize) -> usize {
    if width >= WIDTH_STEP {
        width / WIDTH_STEP * WIDTH_STEP
    } else {
        width.max(1)
    }
}

fn ready_markdown_picture(
    source_text: &str,
    width: usize,
    base_dir: Option<&Path>,
    theme: &Theme,
) -> Option<images::PreparedImage> {
    if !images::enabled() {
        return None;
    }
    let source = parse_markdown_source(source_text, base_dir)?;
    let cell = images::cell_size();
    let max_cols = quantize_cols(width);
    let light = images::theme_is_light(theme);
    let identity = match &source {
        Source::File(path) => format!("file\0{}\0{:x}", path.display(), cached_file_stamp(path)),
        Source::RasterBase64(payload) => {
            let mut hasher = DefaultHasher::new();
            payload.hash(&mut hasher);
            format!("data\0{}\0{:x}", payload.len(), hasher.finish())
        }
        Source::SvgBytes(bytes) => {
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            format!("svg\0{light}\0{}\0{:x}", bytes.len(), hasher.finish())
        }
    };
    let key = format!("picture\0{cell:?}\0{max_cols}\0{identity}");
    let image = images::prepared(&key, source_text, move |completer| {
        queue().push(Request {
            source,
            cell,
            max_cols,
            matte: None,
            completer,
        });
    })?;
    (image.cols as usize <= width).then_some(image)
}

fn ready_mime_image(
    payload_hash: u64,
    payload: &RichImagePayload,
    needs_bg: NeedsBackground,
    width: usize,
    theme: &Theme,
    needle: &str,
) -> Option<images::PreparedImage> {
    if !images::enabled() {
        return None;
    }
    let cell = images::cell_size();
    let max_cols = quantize_cols(width);
    let matte = needs_bg.matte_for_theme(images::theme_is_light(theme));
    let kind_tag = match payload {
        RichImagePayload::RasterBase64(_) => "raster",
        RichImagePayload::SvgXml(_) => "svg",
    };
    let key = format!("mime_image\0{kind_tag}\0{cell:?}\0{max_cols}\0{matte:?}\0{payload_hash:x}");
    let source = match payload {
        RichImagePayload::RasterBase64(b64) => Source::RasterBase64(Arc::clone(b64)),
        RichImagePayload::SvgXml(xml) => Source::SvgBytes(xml.as_bytes().to_vec()),
    };
    let image = images::prepared(&key, needle, move |completer| {
        queue().push(Request {
            source,
            cell,
            max_cols,
            matte,
            completer,
        });
    })?;
    (image.cols as usize <= width).then_some(image)
}

struct Request {
    source: Source,
    cell: (u16, u16),
    max_cols: usize,
    matte: Option<(u8, u8, u8)>,
    completer: images::Completer,
}

fn queue() -> &'static images::WorkQueue<Request> {
    static QUEUE: OnceLock<Arc<images::WorkQueue<Request>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        images::WorkQueue::start("nb-pictures", 2, 1, |requests: Vec<Request>| {
            for request in requests {
                let result = load(
                    request.source,
                    request.cell,
                    request.max_cols,
                    request.matte,
                );
                request.completer.finish(result);
            }
        })
    })
}

pub fn load_mime_sync_for_test(
    payload: &RichImagePayload,
    cell: (u16, u16),
    max_cols: usize,
    matte: Option<(u8, u8, u8)>,
) -> Result<images::Rendered, String> {
    let source = match payload {
        RichImagePayload::RasterBase64(b64) => Source::RasterBase64(Arc::clone(b64)),
        RichImagePayload::SvgXml(xml) => Source::SvgBytes(xml.as_bytes().to_vec()),
    };
    load(source, cell, max_cols, matte)
}

fn load(
    source: Source,
    cell: (u16, u16),
    max_cols: usize,
    matte: Option<(u8, u8, u8)>,
) -> Result<images::Rendered, String> {
    match source {
        Source::SvgBytes(bytes) => render_svg_bytes(&bytes, cell, max_cols, matte),
        Source::File(path) => {
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                return Err(format!("{} is not a drawable image file", path.display()));
            }
            let bytes = fs::read(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            let is_svg = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"));
            if is_svg {
                render_svg_bytes(&bytes, cell, max_cols, matte)
            } else {
                render_raster_bytes(&bytes, cell, max_cols, matte)
            }
        }
        Source::RasterBase64(payload) => {
            let cleaned = strip_ascii_whitespace(&payload);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(cleaned.as_bytes())
                .map_err(|error| format!("bad base64 image payload: {error}"))?;
            render_raster_bytes(&bytes, cell, max_cols, matte)
        }
    }
}

fn render_svg_bytes(
    bytes: &[u8],
    cell: (u16, u16),
    max_cols: usize,
    matte: Option<(u8, u8, u8)>,
) -> Result<images::Rendered, String> {
    let ideal_scale = (f32::from(cell.1.max(1)) / 16.0).clamp(1.0, 2.5);
    let (width, height, mut rgba) =
        images::rasterize_svg(bytes, cell, max_cols, STANDARD_IMAGE_ROWS, ideal_scale)?;
    if let Some(matte_rgb) = matte {
        images::composite_matte(&mut rgba, matte_rgb);
    }
    images::fit_rgba(width, height, &rgba, cell)
}

fn render_raster_bytes(
    bytes: &[u8],
    cell: (u16, u16),
    max_cols: usize,
    matte: Option<(u8, u8, u8)>,
) -> Result<images::Rendered, String> {
    let (width, height, mut rgba) = images::decode_image(bytes)?;
    let max_rows = if height > width {
        TALL_IMAGE_ROWS
    } else {
        STANDARD_IMAGE_ROWS
    };
    let (cell_width, cell_height) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let scale = (max_cols as f64 * cell_width / width as f64)
        .min(max_rows as f64 * cell_height / height as f64)
        .min(1.0);
    let (final_w, final_h) = if scale < 1.0 {
        let scaled_width = ((width as f64 * scale).floor() as usize).max(1);
        let scaled_height = ((height as f64 * scale).floor() as usize).max(1);
        rgba = images::resize_rgba(width, height, rgba, scaled_width, scaled_height)?;
        (scaled_width, scaled_height)
    } else {
        (width, height)
    };
    if let Some(matte_rgb) = matte {
        images::composite_matte(&mut rgba, matte_rgb);
    }
    images::fit_rgba(final_w, final_h, &rgba, cell)
}

#[doc(hidden)]
pub fn insert_mime_ready_for_test(
    payload_hash: u64,
    is_svg: bool,
    needs_bg: NeedsBackground,
    width: usize,
    theme: &Theme,
    png: Vec<u8>,
    cols: u16,
    rows: u16,
) {
    let cell = images::cell_size();
    let max_cols = quantize_cols(width);
    let matte = needs_bg.matte_for_theme(images::theme_is_light(theme));
    let kind_tag = if is_svg { "svg" } else { "raster" };
    let key = format!("mime_image\0{kind_tag}\0{cell:?}\0{max_cols}\0{matte:?}\0{payload_hash:x}");
    images::insert_prepared_for_test(&key, png, cols, rows);
}

#[doc(hidden)]
pub fn insert_markdown_ready_for_test(
    source_text: &str,
    width: usize,
    png: Vec<u8>,
    cols: u16,
    rows: u16,
) {
    let Some(source) = parse_markdown_source(source_text, None) else {
        return;
    };
    let cell = images::cell_size();
    let max_cols = quantize_cols(width);
    let identity = match &source {
        Source::File(path) => format!("file\0{}\0{:x}", path.display(), cached_file_stamp(path)),
        Source::RasterBase64(payload) => {
            let mut hasher = DefaultHasher::new();
            payload.hash(&mut hasher);
            format!("data\0{}\0{:x}", payload.len(), hasher.finish())
        }
        Source::SvgBytes(bytes) => {
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            format!("svg\0false\0{}\0{:x}", bytes.len(), hasher.finish())
        }
    };
    let key = format!("picture\0{cell:?}\0{max_cols}\0{identity}");
    images::insert_prepared_for_test(&key, png, cols, rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![120; (w * h * 4) as usize])
            .unwrap();
        png
    }

    #[test]
    fn block_image_parsing_rejects_prose_and_remote_urls() {
        assert_eq!(
            block_image_source("![plot](/tmp/plot.png)"),
            Some("/tmp/plot.png")
        );
        assert_eq!(
            block_image_source("![plot](out/a.png \"Title\")"),
            Some("out/a.png")
        );
        assert_eq!(block_image_source("See ![plot](/tmp/plot.png)"), None);
        assert!(parse_markdown_source("https://example.test/a.png", None).is_none());
        assert!(parse_markdown_source("data:image/png;base64,AAAA", None).is_some());
    }

    #[test]
    fn matplotlib_default_640x480_fits_within_18_preview_rows_while_tall_figures_stay_wide() {
        let std_b64 = base64::engine::general_purpose::STANDARD.encode(make_png(640, 480));
        let fitted = load_mime_sync_for_test(
            &RichImagePayload::RasterBase64(std_b64.into()),
            (12, 24),
            80,
            None,
        )
        .expect("640x480 decodes");
        assert_eq!(fitted.rows, 18, "4:3 plot scales cleanly to 18 rows");
        assert_eq!(fitted.cols, 48);

        let tall_b64 = base64::engine::general_purpose::STANDARD.encode(make_png(600, 1200));
        let tall = load_mime_sync_for_test(
            &RichImagePayload::RasterBase64(tall_b64.into()),
            (12, 24),
            80,
            None,
        )
        .expect("tall figure decodes");
        assert_eq!(tall.rows, 36, "tall stacked subplot figure uses 36 rows");
        assert_eq!(tall.cols, 36);
    }
}
