//! Rasterize a ratatui [`Buffer`] into SVG (and PNG via `resvg`).
//!
//! Each cell is drawn at a fixed grid position, so the image matches what a
//! terminal shows cell for cell. Block and box-drawing characters are drawn
//! as rectangles rather than font glyphs, as terminals do, so borders and
//! gutters stay continuous between rows.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorMark {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
}

/// Terminal look used for colors that the buffer leaves to the terminal
/// (`Color::Reset` and the 16 ANSI colors).
#[derive(Clone, Debug)]
pub struct TerminalLook {
    pub cell_w: f32,
    pub cell_h: f32,
    pub font_size: f32,
    pub font_family: String,
    pub default_fg: [u8; 3],
    pub default_bg: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

impl Default for TerminalLook {
    fn default() -> Self {
        Self {
            cell_w: 9.0,
            cell_h: 19.0,
            font_size: 15.0,
            font_family: "Menlo, 'SF Mono', 'DejaVu Sans Mono', monospace".into(),
            default_fg: [0xd4, 0xd7, 0xdd],
            default_bg: [0x0a, 0x0e, 0x1a],
            ansi: [
                [0x1d, 0x20, 0x29], // black
                [0xe0, 0x6c, 0x75], // red
                [0x98, 0xc3, 0x79], // green
                [0xe5, 0xc0, 0x7b], // yellow
                [0x61, 0xaf, 0xef], // blue
                [0xd5, 0x5f, 0xc8], // magenta
                [0x56, 0xb6, 0xc2], // cyan
                [0xab, 0xb2, 0xbf], // white (ratatui Gray)
                [0x5c, 0x63, 0x70], // bright black (DarkGray)
                [0xff, 0x7b, 0x86], // bright red
                [0xb5, 0xe8, 0x90], // bright green
                [0xff, 0xd8, 0x8a], // bright yellow
                [0x8c, 0xc8, 0xff], // bright blue
                [0xf0, 0x8c, 0xe6], // bright magenta
                [0x7f, 0xdb, 0xe6], // bright cyan
                [0xf0, 0xf2, 0xf5], // bright white
            ],
        }
    }
}

impl TerminalLook {
    fn rgb(&self, color: Color, default: [u8; 3]) -> [u8; 3] {
        match color {
            Color::Reset => default,
            Color::Black => self.ansi[0],
            Color::Red => self.ansi[1],
            Color::Green => self.ansi[2],
            Color::Yellow => self.ansi[3],
            Color::Blue => self.ansi[4],
            Color::Magenta => self.ansi[5],
            Color::Cyan => self.ansi[6],
            Color::Gray => self.ansi[7],
            Color::DarkGray => self.ansi[8],
            Color::LightRed => self.ansi[9],
            Color::LightGreen => self.ansi[10],
            Color::LightYellow => self.ansi[11],
            Color::LightBlue => self.ansi[12],
            Color::LightMagenta => self.ansi[13],
            Color::LightCyan => self.ansi[14],
            Color::White => self.ansi[15],
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Indexed(index) => self.indexed(index),
        }
    }

    fn indexed(&self, index: u8) -> [u8; 3] {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [level(i / 36), level((i / 6) % 6), level(i % 6)]
            }
            _ => {
                let v = 8 + (index - 232) * 10;
                [v, v, v]
            }
        }
    }

    /// Resolved (fg, bg) for a cell, applying `REVERSED`.
    fn cell_colors(&self, fg: Color, bg: Color, modifier: Modifier) -> ([u8; 3], [u8; 3]) {
        let fg = self.rgb(fg, self.default_fg);
        let bg = self.rgb(bg, self.default_bg);
        if modifier.contains(Modifier::REVERSED) {
            (bg, fg)
        } else {
            (fg, bg)
        }
    }
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Rectangles (in cell fractions: x, y, w, h) that draw a block or
/// box-drawing character; `None` for characters drawn from the font.
fn geometric(symbol: &str) -> Option<&'static [(f32, f32, f32, f32)]> {
    const T: f32 = 0.12; // vertical line thickness (cell-width fraction)
    const H: f32 = 0.5 - T / 2.0;
    const VT: f32 = 0.06; // horizontal line thickness (cell-height fraction)
    const V: f32 = 0.5 - VT / 2.0;
    Some(match symbol {
        "█" => &[(0.0, 0.0, 1.0, 1.0)],
        "▀" => &[(0.0, 0.0, 1.0, 0.5)],
        "▄" => &[(0.0, 0.5, 1.0, 0.5)],
        "▌" => &[(0.0, 0.0, 0.5, 1.0)],
        "▐" => &[(0.5, 0.0, 0.5, 1.0)],
        "▎" => &[(0.0, 0.0, 0.25, 1.0)],
        "▍" => &[(0.0, 0.0, 0.375, 1.0)],
        "▏" => &[(0.0, 0.0, 0.125, 1.0)],
        "▕" => &[(0.875, 0.0, 0.125, 1.0)],
        "▔" => &[(0.0, 0.0, 1.0, 0.125)],
        "▁" => &[(0.0, 0.875, 1.0, 0.125)],
        "│" | "┃" => &[(H, 0.0, T, 1.0)],
        "─" | "━" => &[(0.0, V, 1.0, VT)],
        "┌" | "╭" => &[(H, V, 0.5 + T / 2.0, VT), (H, V, T, 1.0 - V)],
        "┐" | "╮" => &[(0.0, V, 0.5 + T / 2.0, VT), (H, V, T, 1.0 - V)],
        "└" | "╰" => &[(H, V, 0.5 + T / 2.0, VT), (H, 0.0, T, V + VT)],
        "┘" | "╯" => &[(0.0, V, 0.5 + T / 2.0, VT), (H, 0.0, T, V + VT)],
        "├" => &[(H, 0.0, T, 1.0), (H, V, 0.5 + T / 2.0, VT)],
        "┤" => &[(H, 0.0, T, 1.0), (0.0, V, 0.5 + T / 2.0, VT)],
        "┬" => &[(0.0, V, 1.0, VT), (H, V, T, 1.0 - V)],
        "┴" => &[(0.0, V, 1.0, VT), (H, 0.0, T, V + VT)],
        "┼" => &[(0.0, V, 1.0, VT), (H, 0.0, T, 1.0)],
        _ => return None,
    })
}

#[derive(Clone, PartialEq)]
struct TextStyle {
    fill: [u8; 3],
    bold: bool,
    italic: bool,
    dim: bool,
}

/// Render `buffer` (optionally with a cursor) to an SVG document.
pub fn buffer_to_svg(buffer: &Buffer, cursor: Option<CursorMark>, look: &TerminalLook) -> String {
    let area = buffer.area;
    let width = f32::from(area.width) * look.cell_w;
    let height = f32::from(area.height) * look.cell_h;
    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" shape-rendering="crispEdges">"#
    );
    let _ = write!(
        svg,
        r#"<rect x="0" y="0" width="{width}" height="{height}" fill="{}"/>"#,
        hex(look.default_bg)
    );

    // Backgrounds, merged into horizontal runs.
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let cell = &buffer[(area.x + x, area.y + y)];
            let (_, bg) = look.cell_colors(cell.fg, cell.bg, cell.modifier);
            let start = x;
            x += 1;
            while x < area.width {
                let next = &buffer[(area.x + x, area.y + y)];
                let (_, next_bg) = look.cell_colors(next.fg, next.bg, next.modifier);
                if next_bg != bg {
                    break;
                }
                x += 1;
            }
            if bg != look.default_bg {
                let _ = write!(
                    svg,
                    r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{}"/>"#,
                    f32::from(start) * look.cell_w,
                    f32::from(y) * look.cell_h,
                    f32::from(x - start) * look.cell_w,
                    look.cell_h,
                    hex(bg)
                );
            }
        }
    }

    // Foreground: geometric glyphs as rects, text as positioned runs.
    let baseline = look.cell_h * 0.76;
    for y in 0..area.height {
        let mut run: Option<(TextStyle, String, Vec<f32>)> = None;
        let flush = |svg: &mut String, run: &mut Option<(TextStyle, String, Vec<f32>)>| {
            if let Some((style, text, xs)) = run.take() {
                if text.is_empty() {
                    return;
                }
                let xs = xs.iter().map(|x| format!("{x:.1}")).collect::<Vec<_>>().join(" ");
                let _ = write!(
                    svg,
                    r#"<text x="{xs}" y="{:.1}" font-family="{}" font-size="{}" fill="{}"{}{}{}>{}</text>"#,
                    f32::from(y) * look.cell_h + baseline,
                    look.font_family,
                    look.font_size,
                    hex(style.fill),
                    if style.bold { r#" font-weight="bold""# } else { "" },
                    if style.italic { r#" font-style="italic""# } else { "" },
                    if style.dim { r#" fill-opacity="0.6""# } else { "" },
                    escape(&text)
                );
            }
        };
        for x in 0..area.width {
            let cell = &buffer[(area.x + x, area.y + y)];
            if cell.modifier.contains(Modifier::HIDDEN) {
                continue;
            }
            let symbol = cell.symbol();
            let (fg, _) = look.cell_colors(cell.fg, cell.bg, cell.modifier);
            let px = f32::from(x) * look.cell_w;
            let py = f32::from(y) * look.cell_h;
            if cell.modifier.contains(Modifier::UNDERLINED) {
                let _ = write!(
                    svg,
                    r#"<rect x="{px}" y="{:.1}" width="{}" height="1" fill="{}"/>"#,
                    py + baseline + 2.0,
                    look.cell_w,
                    hex(fg)
                );
            }
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                let _ = write!(
                    svg,
                    r#"<rect x="{px}" y="{:.1}" width="{}" height="1" fill="{}"/>"#,
                    py + look.cell_h * 0.5,
                    look.cell_w,
                    hex(fg)
                );
            }
            if let Some(rects) = geometric(symbol) {
                for (rx, ry, rw, rh) in rects {
                    let _ = write!(
                        svg,
                        r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{}"/>"#,
                        px + rx * look.cell_w,
                        py + ry * look.cell_h,
                        (rw * look.cell_w).max(1.0),
                        (rh * look.cell_h).max(1.0),
                        hex(fg)
                    );
                }
                continue;
            }
            if symbol.trim().is_empty() {
                continue;
            }
            let style = TextStyle {
                fill: fg,
                bold: cell.modifier.contains(Modifier::BOLD),
                italic: cell.modifier.contains(Modifier::ITALIC),
                dim: cell.modifier.contains(Modifier::DIM),
            };
            let single = symbol.chars().count() == 1;
            match run.as_mut() {
                Some((current, text, xs)) if *current == style && single => {
                    text.push_str(symbol);
                    xs.push(px);
                }
                _ => {
                    flush(&mut svg, &mut run);
                    run = Some((style, symbol.to_string(), vec![px]));
                    if !single {
                        flush(&mut svg, &mut run);
                    }
                }
            }
        }
        flush(&mut svg, &mut run);
    }

    if let Some(mark) = cursor {
        if mark.x < area.width && mark.y < area.height {
            let cell = &buffer[(area.x + mark.x, area.y + mark.y)];
            let (fg, _) = look.cell_colors(cell.fg, cell.bg, cell.modifier);
            let px = f32::from(mark.x) * look.cell_w;
            let py = f32::from(mark.y) * look.cell_h;
            let (w, h, oy) = match mark.shape {
                CursorShape::Block => (look.cell_w, look.cell_h, 0.0),
                CursorShape::Bar => (2.0, look.cell_h, 0.0),
                CursorShape::Underline => (look.cell_w, 2.0, look.cell_h - 2.0),
            };
            let _ = write!(
                svg,
                r#"<rect x="{px}" y="{:.1}" width="{w}" height="{h}" fill="{}" fill-opacity="{}"/>"#,
                py + oy,
                hex(fg),
                if matches!(mark.shape, CursorShape::Block) { "0.55" } else { "1" }
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

/// Rasterize an SVG document to a PNG file, loading system fonts.
pub fn svg_to_png(svg: &str, path: &Path) -> anyhow::Result<()> {
    let mut options = resvg::usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_str(svg, &options)?;
    let size = tree.size().to_int_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| anyhow::anyhow!("empty image {}x{}", size.width(), size.height()))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    pixmap.save_png(path)?;
    Ok(())
}

/// Convenience: `buffer` (and cursor) straight to a PNG file.
pub fn buffer_to_png(
    buffer: &Buffer,
    cursor: Option<CursorMark>,
    look: &TerminalLook,
    path: &Path,
) -> anyhow::Result<()> {
    svg_to_png(&buffer_to_svg(buffer, cursor, look), path)
}

/// Plain-text rows of a buffer, for assertions and quick diffs.
pub fn buffer_text(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..area.width {
                let cell = &buffer[(area.x + x, area.y + y)];
                row.push_str(cell.symbol());
            }
            row.trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn last_column_block_glyph_is_drawn() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        buffer.set_string(0, 0, "ab█", Style::default());
        let svg = buffer_to_svg(&buffer, None, &TerminalLook::default());
        assert!(svg.contains(r#"<rect x="18.00" y="0.00" width="9.00" height="19.00""#), "{svg}");
        assert!(svg.contains(">ab</text>"), "{svg}");
    }
}
