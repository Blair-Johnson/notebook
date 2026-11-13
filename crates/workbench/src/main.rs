//! CLI entrypoint, plugin wiring list, panic-safe terminal guard, and `tokio::select!` loop.

mod editor;
mod runtime;
mod terminal;

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use futures::{FutureExt, StreamExt};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wb_core::{CellKind, Document, FormatBacking};
use wb_kernel::{KernelLaunchSpec, LocalZmqKernel};
use wb_sdk::ExtensionRegistry;
use wb_tui::platform::SystemClipboard;
use wb_tui::{BuiltinsPlugin, Coordinator, Effect, SettingsStore};

#[derive(Parser, Debug)]
#[command(
    name = "workbench",
    version,
    about = "Fast terminal Jupyter notebook client"
)]
pub struct Cli {
    /// Notebook path (`.py` Jupytext percent script or `.ipynb`). Omit for a scratchpad.
    pub notebook: Option<PathBuf>,

    /// Kernelspec name override (defaults to notebook metadata, then `python3`).
    #[arg(long)]
    pub kernel: Option<String>,

    /// Attach to an existing Jupyter connection JSON file without owning the kernel process.
    #[arg(long, value_name = "CONNECTION_FILE")]
    pub existing: Option<PathBuf>,

    /// Explicit path to `config.toml` (overrides `$WORKBENCH_CONFIG` and XDG config dir).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

/// Central compile-time plugin registration table.
///
/// Future domain crates (e.g. `wb-plugin-jax`) are added exclusively here.
pub fn build_extension_registry() -> ExtensionRegistry {
    let mut reg = ExtensionRegistry::default();
    reg.install(BuiltinsPlugin);
    reg
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let registry = build_extension_registry();
    let settings = SettingsStore::load_or_default(cli.config.clone(), &registry);
    let config_warning = settings.snapshot().warnings.first().map(|warning| {
        if warning.source_key.is_empty() {
            warning.message.clone()
        } else {
            format!("{}: {}", warning.source_key, warning.message)
        }
    });
    let default_kernel = settings.snapshot().core.default_kernel.clone();
    let notebook_path = cli.notebook.clone();

    let working_dir = notebook_path
        .as_deref()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let doc = match notebook_path.as_deref() {
        Some(path) if path.exists() => wb_core::codec::open_path(path)?,
        Some(path) if path.extension().is_some_and(|e| e == "py") => {
            Document::new_empty(FormatBacking::PercentPy { header: None })
        }
        _ => Document::new_empty(FormatBacking::Ipynb {
            extra: Default::default(),
        }),
    };

    let kernel = LocalZmqKernel::spawn(KernelLaunchSpec {
        requested_name: cli.kernel.unwrap_or(default_kernel),
        working_dir,
        existing_connection_file: cli.existing,
    });

    let mut coordinator = Coordinator::new(
        notebook_path,
        doc,
        registry,
        settings,
        Box::new(kernel),
        Box::new(SystemClipboard),
        Box::new(terminal::CrosstermTerminalEffects::default()),
    );
    coordinator.notice = config_warning;
    coordinator.check_autosave_recovery();

    let mouse_capture = coordinator.settings.snapshot().core.mouse_capture;
    let mut terminal = ratatui::init();
    terminal::clear_screen();
    terminal::enter_extras(mouse_capture);

    let result =
        std::panic::AssertUnwindSafe(run_loop(&mut terminal, &mut coordinator, mouse_capture))
            .catch_unwind()
            .await;

    terminal::leave_extras();
    terminal::reset_cursor_shape();
    let _ = ratatui::try_restore();
    let _ = terminal.show_cursor();
    // `Terminal::drop` reports restoration errors with `eprintln!`; on a closed
    // PTY that can panic again through ratatui's panic hook.
    std::mem::forget(terminal);

    match result {
        Ok(result) => result,
        Err(payload) => {
            coordinator.emergency_autosave();
            std::panic::resume_unwind(payload)
        }
    }
}

async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
) -> Result<()> {
    let mut term_events = EventStream::new();
    let mut fast_tick = tokio::time::interval(Duration::from_millis(50));
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let autosave_secs = coordinator.settings.snapshot().core.autosave_secs.max(1);
    let mut autosave_tick = tokio::time::interval(Duration::from_secs(autosave_secs));
    autosave_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut kernel_open = true;

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    draw_frame(terminal, coordinator)?;

    while !coordinator.should_quit {
        let mut redraw = false;
        tokio::select! {
            biased;
            _ = sigterm.recv() => {
                coordinator.on_signal("SIGTERM");
            }
            _ = sighup.recv() => {
                coordinator.on_signal("SIGHUP");
            }
            _ = sigint.recv() => {
                coordinator.on_signal("SIGINT");
            }
            maybe_event = term_events.next() => {
                let Some(event_result) = maybe_event else { break };
                let event = event_result?;
                handle_terminal_event(
                    terminal,
                    coordinator,
                    mouse_capture,
                    event,
                )?;
                redraw = !coordinator.should_quit;
            }
            kernel_event = wb_kernel::recv_event(coordinator.kernel.as_mut()), if kernel_open => {
                match kernel_event {
                    Some(first) => {
                        let mut queued = vec![first];
                        while queued.len() < 64 {
                            let Some(event) = coordinator.kernel.try_recv() else { break };
                            runtime::queue_kernel_event(&mut queued, event);
                        }
                        for event in queued {
                            coordinator.apply_kernel_event(event);
                        }
                        redraw = !coordinator.should_quit;
                    }
                    None => kernel_open = false,
                }
            }
            _ = fast_tick.tick() => {
                redraw = coordinator.on_tick(Instant::now());
            }
            _ = autosave_tick.tick() => {
                coordinator.on_autosave_tick();
            }
        }
        if redraw && !coordinator.should_quit {
            draw_frame(terminal, coordinator)?;
        }
    }
    Ok(())
}

fn handle_terminal_event(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
    event: Event,
) -> Result<()> {
    let first_is_coalescable = is_coalescable(&event);
    let mut effect = dispatch_event(coordinator, event);
    let mut stop_batch = !first_is_coalescable;
    if handle_effect(terminal, coordinator, mouse_capture, effect)? {
        stop_batch = true;
    }

    if !stop_batch && !coordinator.should_quit {
        for _ in 1..16 {
            if !crossterm::event::poll(Duration::ZERO)? {
                break;
            }
            let next = crossterm::event::read()?;
            let next_is_coalescable = is_coalescable(&next);
            effect = dispatch_event(coordinator, next);
            if handle_effect(terminal, coordinator, mouse_capture, effect)? {
                break;
            }
            if !next_is_coalescable || coordinator.should_quit {
                break;
            }
        }
    }
    Ok(())
}

fn dispatch_event(coordinator: &mut Coordinator, event: Event) -> Effect {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            coordinator.on_key(key, Instant::now())
        }
        Event::Paste(text) => {
            coordinator.on_paste(text);
            Effect::None
        }
        Event::Mouse(mouse) => {
            coordinator.on_mouse(mouse);
            Effect::None
        }
        Event::Resize(_, _) => {
            coordinator.on_resize();
            Effect::None
        }
        Event::FocusGained => {
            coordinator.on_focus_gained();
            Effect::None
        }
        _ => Effect::None,
    }
}

fn is_coalescable(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
    ) || matches!(event, Event::Paste(_))
}

fn handle_effect(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
    effect: Effect,
) -> Result<bool> {
    match effect {
        Effect::None => Ok(false),
        Effect::Quit => {
            coordinator.should_quit = true;
            Ok(true)
        }
        Effect::OpenExternalEditor(cell_id) => {
            external_edit(terminal, coordinator, mouse_capture, cell_id)?;
            Ok(true)
        }
    }
}

fn draw_frame(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    let begin = queue!(&mut stdout, BeginSynchronizedUpdate);
    let drawn = terminal.draw(|frame| coordinator.draw(frame));
    // Always attempt to close the synchronized update, even when drawing fails.
    let end = crossterm::execute!(&mut stdout, EndSynchronizedUpdate);
    begin?;
    drawn?;
    end?;
    Ok(())
}

fn external_edit(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
    cell_id: wb_core::CellId,
) -> Result<()> {
    let Some(cell) = coordinator.document.cell(&cell_id) else {
        return Ok(());
    };
    let extension = if cell.kind == CellKind::Markdown {
        "md"
    } else {
        "py"
    };
    let source = cell.buffer.as_str().to_owned();
    let temp_path = temporary_cell_path(extension)?;
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());

    if let Err(error) = fs::write(&temp_path, source) {
        let _ = fs::remove_file(&temp_path);
        coordinator.notice = Some(format!("could not prepare external editor: {error}"));
        return Ok(());
    }

    terminal::leave_extras();
    ratatui::restore();
    let status = editor::editor_command(&editor, &temp_path).status();
    *terminal = ratatui::init();
    terminal::clear_screen();
    terminal::enter_extras(mouse_capture);

    match status {
        Ok(status) if status.success() => match fs::read_to_string(&temp_path) {
            Ok(new_source) => coordinator.apply_external_edit(&cell_id, new_source),
            Err(error) => {
                coordinator.notice = Some(format!("external editor read-back failed: {error}"));
            }
        },
        Ok(status) if status.code() == Some(127) => {
            coordinator.notice = Some(format!(
                "{editor}: command not found (check $VISUAL/$EDITOR)"
            ));
        }
        Ok(status) => {
            coordinator.notice = Some(format!("{editor} exited with {status}"));
        }
        Err(error) => {
            coordinator.notice = Some(format!("{editor} failed: {error}"));
        }
    }
    let _ = fs::remove_file(&temp_path);
    Ok(())
}

fn temporary_cell_path(extension: &str) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock before UNIX_EPOCH: {error}"))?
        .as_nanos();
    for attempt in 0..16u8 {
        let path = std::env::temp_dir().join(format!(
            "workbench-cell-{}-{stamp}-{attempt}.{extension}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!(
        "could not allocate a unique external-editor temp file"
    ))
}
