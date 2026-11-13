//! `nb-snap`: render nb frames to PNG.
//!
//! ```text
//! nb-snap headless --notebook demo.py --keys "jj<cr>i<snap:insert>" --out /tmp/snap/final.png
//! nb-snap tmux --pane %53 --keys "<esc>?" --out /tmp/snap/help.png
//! ```
//! `<snap:NAME>` steps write `NAME.png` next to `--out`; the final frame is
//! written to `--out`. `--text` also prints the frame's text rows.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ratatui::buffer::Buffer;
use std::path::{Path, PathBuf};
use std::time::Duration;
use nb_snapshot::image::{buffer_to_svg, svg_to_png};
use nb_snapshot::{CursorMark, HeadlessSession, KernelChoice, TerminalLook, buffer_text, parse_script};

#[derive(Parser)]
#[command(name = "nb-snap", about = "Render nb frames to PNG")]
struct Cli {
    #[command(subcommand)]
    command: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Run a Coordinator headlessly against a TestBackend.
    Headless {
        #[arg(long)]
        notebook: Option<PathBuf>,
        /// Terminal size as COLSxROWS.
        #[arg(long, default_value = "100x36")]
        size: String,
        /// `fake` (no kernel) or `real` (local ipykernel).
        #[arg(long, default_value = "fake")]
        kernel: String,
        /// Key script in vim notation (`<esc>`, `<cr>`, `<c-p>`, `<wait:MS>`, `<snap:NAME>`).
        #[arg(long, default_value = "")]
        keys: String,
        /// Settle time after the last key, in milliseconds.
        #[arg(long, default_value_t = 200)]
        settle_ms: u64,
        /// With a real kernel: wait up to this long for it to be ready first.
        #[arg(long, default_value_t = 20_000)]
        kernel_timeout_ms: u64,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        text: bool,
        /// Config file to use (e.g. one setting `display.theme`); default is
        /// a fresh temporary file with built-in defaults.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Send keys to a live tmux pane and capture it.
    Tmux {
        #[arg(long)]
        pane: String,
        #[arg(long, default_value = "")]
        keys: String,
        #[arg(long, default_value_t = 300)]
        settle_ms: u64,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        text: bool,
    },
}

fn parse_size(size: &str) -> Result<(u16, u16)> {
    let Some((w, h)) = size.split_once('x') else {
        bail!("--size must look like 100x36");
    };
    Ok((w.parse()?, h.parse()?))
}

fn snap_path(out: &Path, name: &str) -> PathBuf {
    out.parent().unwrap_or(Path::new(".")).join(format!("{name}.png"))
}

fn write(path: &Path, buffer: &Buffer, cursor: Option<CursorMark>, text: bool) -> Result<()> {
    let svg = buffer_to_svg(buffer, cursor, &TerminalLook::default());
    if std::env::var_os("NB_SNAP_SVG").is_some() {
        std::fs::write(path.with_extension("svg"), &svg)?;
    }
    svg_to_png(&svg, path)?;
    println!("wrote {}", path.display());
    if text {
        for row in buffer_text(buffer) {
            println!("|{row}");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Mode::Headless {
            notebook,
            size,
            kernel,
            keys,
            settle_ms,
            kernel_timeout_ms,
            out,
            text,
            config,
        } => {
            let (width, height) = parse_size(&size)?;
            let kernel = match kernel.as_str() {
                "fake" => KernelChoice::Fake,
                "real" => KernelChoice::Real,
                other => bail!("--kernel must be fake or real, not {other}"),
            };
            let config = config.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("nb-snap-{}-config.toml", std::process::id()))
            });
            let mut session = HeadlessSession::open(notebook.as_deref(), width, height, kernel, config)?;
            if kernel == KernelChoice::Real {
                let ready = session
                    .pump_until(Duration::from_millis(kernel_timeout_ms), |c| c.kernel_alive)
                    .await;
                if !ready {
                    eprintln!("warning: kernel not ready after {kernel_timeout_ms} ms");
                }
            } else {
                session.pump(Duration::from_millis(50)).await;
            }
            let steps = parse_script(&keys)?;
            session
                .run_script(&steps, Duration::from_millis(30), |name, buffer, cursor| {
                    write(&snap_path(&out, name), buffer, cursor, text)
                })
                .await?;
            session.pump(Duration::from_millis(settle_ms)).await;
            let (buffer, cursor) = session.render()?;
            write(&out, &buffer, cursor, text)?;
        }
        Mode::Tmux {
            pane,
            keys,
            settle_ms,
            out,
            text,
        } => {
            for step in parse_script(&keys)? {
                if let nb_snapshot::Step::Snap(name) = &step {
                    std::thread::sleep(Duration::from_millis(settle_ms));
                    let (buffer, cursor) = nb_snapshot::tmux::capture_pane(&pane)?;
                    write(&snap_path(&out, name), &buffer, cursor, text)?;
                } else {
                    nb_snapshot::tmux::send_step(&pane, &step)?;
                }
            }
            std::thread::sleep(Duration::from_millis(settle_ms));
            let (buffer, cursor) = nb_snapshot::tmux::capture_pane(&pane)?;
            write(&out, &buffer, cursor, text)?;
        }
    }
    Ok(())
}
