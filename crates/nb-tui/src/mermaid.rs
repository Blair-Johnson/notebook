//! Mermaid diagrams rendered as crisp vector-scaled terminal images.
//!
//! Fenced ```mermaid blocks and `text/vnd.mermaid` outputs are parsed and laid
//! out in pure Rust with `mermaid-rs-renderer`, styled from the explicit `Theme`
//! over a transparent background, and rasterized at the terminal's cell geometry
//! with `resvg` on background worker threads (`nb-mermaid`).

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, OnceLock},
};

use mermaid_rs_renderer::{LayoutConfig, RenderOptions, Theme as MermaidTheme, render_with_options};
use nb_sdk::Theme;
use ratatui::text::Line;

use crate::images;

const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_ROWS: usize = 28;
/// Quantize target column width so minor window resizes reuse cached diagrams.
const WIDTH_STEP: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiagramStyle {
    light: bool,
    text: (u8, u8, u8),
    accent: (u8, u8, u8),
    muted: (u8, u8, u8),
}

impl DiagramStyle {
    fn from_theme(theme: &Theme) -> Self {
        let light = images::theme_is_light(theme);
        let text = images::color_rgb(theme.palette.fg).unwrap_or(if light {
            (32, 36, 44)
        } else {
            (226, 230, 239)
        });
        let accent = images::color_rgb(theme.palette.accent_code).unwrap_or(if light {
            (28, 104, 180)
        } else {
            (108, 168, 255)
        });
        let muted = images::color_rgb(theme.palette.muted).unwrap_or(if light {
            (100, 116, 139)
        } else {
            (148, 163, 184)
        });
        Self {
            light,
            text,
            accent,
            muted,
        }
    }

    fn to_mermaid_theme(self) -> MermaidTheme {
        let mut out = if self.light {
            MermaidTheme::modern()
        } else {
            MermaidTheme::dark()
        };
        out.background = "none".to_string();
        out.primary_text_color = hex_rgb(self.text);
        out.text_color = hex_rgb(self.text);
        out.primary_border_color = hex_rgb(self.accent);
        out.line_color = hex_rgb(self.muted);
        out.sequence_actor_border = hex_rgb(self.accent);
        out.sequence_actor_line = hex_rgb(self.muted);
        if self.light {
            out.primary_color = blend_hex(self.accent, (255, 255, 255), 0.86);
            out.secondary_color = "#f1f5f9".to_string();
            out.cluster_background = "rgba(241, 245, 249, 0.75)".to_string();
            out.cluster_border = hex_rgb(self.muted);
            out.edge_label_background = "rgba(248, 250, 252, 0.90)".to_string();
            out.sequence_actor_fill = blend_hex(self.accent, (255, 255, 255), 0.88);
        } else {
            out.primary_color = blend_hex(self.accent, (24, 28, 38), 0.80);
            out.secondary_color = "#232836".to_string();
            out.cluster_background = "rgba(30, 35, 48, 0.72)".to_string();
            out.cluster_border = hex_rgb(self.muted);
            out.edge_label_background = "rgba(24, 27, 36, 0.88)".to_string();
            out.sequence_actor_fill = blend_hex(self.accent, (24, 28, 38), 0.82);
        }
        out
    }
}

fn hex_rgb((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn blend_hex(fg: (u8, u8, u8), bg: (u8, u8, u8), bg_weight: f32) -> String {
    let mix = |a: u8, b: u8| -> u8 {
        (f32::from(a) * (1.0 - bg_weight) + f32::from(b) * bg_weight)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    hex_rgb((mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2)))
}

/// Whether `body` may contain a fenced Mermaid diagram.
pub fn may_contain_mermaid(body: &str) -> bool {
    body.lines().any(|line| {
        let trimmed = line.trim_start();
        let rest = trimmed
            .strip_prefix("```")
            .or_else(|| trimmed.strip_prefix("~~~"));
        rest.is_some_and(|info| {
            info.trim_start_matches(['`', '~', ' ', '\t'])
                .split_whitespace()
                .next()
                .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
        })
    })
}

/// Placeholder lines drawing a ready Mermaid diagram centered in `width`
/// columns, or `None` to fall back to syntax-highlighted Mermaid source.
pub fn diagram_lines(source: &str, width: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    diagram_lines_with_needle(source, width, theme, None)
}

pub fn diagram_lines_with_needle(
    source: &str,
    width: usize,
    theme: &Theme,
    needle_override: Option<&str>,
) -> Option<Vec<Line<'static>>> {
    let image = ready_diagram(source, width, theme, needle_override)?;
    let id = images::place_image(&image)?;
    let indent = width.saturating_sub(image.cols as usize) / 2;
    Some(images::placeholder_lines_for(id, &image, indent))
}

fn ready_diagram(
    source: &str,
    width: usize,
    theme: &Theme,
    needle_override: Option<&str>,
) -> Option<images::PreparedImage> {
    if !images::enabled() {
        return None;
    }
    let trimmed = source.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let cell = images::cell_size();
    let max_cols = if width >= WIDTH_STEP {
        width / WIDTH_STEP * WIDTH_STEP
    } else {
        width.max(1)
    };
    let style = DiagramStyle::from_theme(theme);
    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    style.hash(&mut hasher);
    let key = format!(
        "mermaid\0{cell:?}\0{max_cols}\0{}\0{:x}",
        trimmed.len(),
        hasher.finish()
    );
    let needle = needle_override
        .map(str::to_owned)
        .unwrap_or_else(|| diagram_needle(source));
    let owned_source = trimmed.to_string();
    let image = images::prepared(&key, &needle, move |completer| {
        queue().push(Request {
            source: owned_source,
            style,
            cell,
            max_cols,
            completer,
        });
    })?;
    (image.cols as usize <= width).then_some(image)
}

fn diagram_needle(source: &str) -> String {
    source
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.trim().is_empty())
        .max_by_key(|line| line.len())
        .unwrap_or_else(|| source.trim())
        .to_string()
}

struct Request {
    source: String,
    style: DiagramStyle,
    cell: (u16, u16),
    max_cols: usize,
    completer: images::Completer,
}

fn queue() -> &'static images::WorkQueue<Request> {
    static QUEUE: OnceLock<Arc<images::WorkQueue<Request>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        images::WorkQueue::start("nb-mermaid", 2, 1, |requests: Vec<Request>| {
            for request in requests {
                let result = render_diagram(
                    &request.source,
                    request.style,
                    request.cell,
                    request.max_cols,
                );
                request.completer.finish(result);
            }
        })
    })
}

pub fn render_diagram_sync_for_test(
    source: &str,
    theme: &Theme,
    cell: (u16, u16),
    max_cols: usize,
) -> Result<images::Rendered, String> {
    render_diagram(source, DiagramStyle::from_theme(theme), cell, max_cols)
}

fn render_diagram(
    source: &str,
    style: DiagramStyle,
    cell: (u16, u16),
    max_cols: usize,
) -> Result<images::Rendered, String> {
    let options = RenderOptions {
        theme: style.to_mermaid_theme(),
        layout: LayoutConfig::default(),
    };
    let svg = render_with_options(source, options)
        .map_err(|error| format!("mermaid parse/layout error: {error}"))?;
    let ideal_scale = (f32::from(cell.1.max(1)) / 18.0).clamp(1.0, 2.25);
    let (px_w, px_h, rgba) =
        images::rasterize_svg(svg.as_bytes(), cell, max_cols, MAX_ROWS, ideal_scale)?;
    images::fit_rgba(px_w, px_h, &rgba, cell)
}

#[doc(hidden)]
pub fn insert_ready_for_test(
    source: &str,
    width: usize,
    theme: &Theme,
    png: Vec<u8>,
    cols: u16,
    rows: u16,
) {
    let trimmed = source.trim();
    let cell = images::cell_size();
    let max_cols = if width >= WIDTH_STEP {
        width / WIDTH_STEP * WIDTH_STEP
    } else {
        width.max(1)
    };
    let style = DiagramStyle::from_theme(theme);
    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    style.hash(&mut hasher);
    let key = format!(
        "mermaid\0{cell:?}\0{max_cols}\0{}\0{:x}",
        trimmed.len(),
        hasher.finish()
    );
    images::insert_prepared_for_test(&key, png, cols, rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_renders_flowchart_and_sequence_diagrams() {
        let theme = Theme::default();
        let flow = render_diagram_sync_for_test(
            "flowchart LR\n    A[Cell] --> B{Kernel}\n    B -->|PNG| C[Terminal]",
            &theme,
            (12, 24),
            72,
        )
        .expect("flowchart renders");
        assert!(flow.cols > 0 && flow.cols <= 72);
        assert!(flow.rows > 0 && flow.rows <= MAX_ROWS as u16);

        let seq = render_diagram_sync_for_test(
            "sequenceDiagram\n    nb->>ipykernel: execute_request\n    ipykernel-->>nb: display_data",
            &theme,
            (12, 24),
            72,
        )
        .expect("sequence renders");
        assert!(seq.cols > 0 && seq.cols <= 72);
        assert!(seq.rows > 0 && seq.rows <= MAX_ROWS as u16);
    }

    #[test]
    fn invalid_mermaid_returns_err() {
        assert!(
            render_diagram_sync_for_test("not a valid diagram %%%", &Theme::default(), (12, 24), 60)
                .is_err()
        );
    }
}
