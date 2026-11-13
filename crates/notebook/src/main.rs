//! CLI entrypoint, plugin wiring list, panic-safe terminal guard, and `tokio::select!` loop.

mod editor;
mod input;
mod runtime;
mod terminal;
mod watchdog;

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::event::{Event, KeyEventKind};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use futures::FutureExt;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use nb_core::{CellKind, Document, FormatBacking};
use nb_kernel::{KernelLaunchSpec, LocalZmqKernel};
use nb_sdk::ExtensionRegistry;
use nb_tui::platform::SystemClipboard;
use nb_tui::{BuiltinsPlugin, Coordinator, Effect, SettingsStore};

#[derive(Parser, Debug)]
#[command(
    name = "nb",
    version,
    about = "Jupyter notebooks in the terminal",
    after_help = "Examples:\n  \
        nb analysis.ipynb     open (or create) a notebook\n  \
        nb quick_test.py      percent-format script: cells start with `# %%`\n  \
        nb                    scratch notebook\n\n\
        The kernel comes from the active environment (pixi shell, conda, venv),\n\
        which needs ipykernel installed: `pixi add ipykernel`."
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

    /// Explicit path to `config.toml` (overrides `$NB_CONFIG` and XDG config dir).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

/// Central compile-time plugin registration table.
///
/// Future domain crates (e.g. `nb-plugin-jax`) are added exclusively here.
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
    if let Some(path) = notebook_path.as_deref() {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(extension, "ipynb" | "py") {
            return Err(anyhow!(
                "{}: nb opens .ipynb notebooks and .py percent-format scripts",
                path.display()
            ));
        }
    }

    let working_dir = kernel_working_dir(notebook_path.as_deref());

    let doc = match notebook_path.as_deref() {
        Some(path) if path.exists() => nb_core::codec::open_path(path)?,
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
        Box::new(SystemClipboard::default()),
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

    // Usually the terminal closed mid-draw or mid-read.
    if !matches!(result, Ok(Ok(()))) {
        coordinator.emergency_autosave();
    }
    // Returning ends the runtime, which would SIGKILL the kernel and leak its
    // connection file; an attached kernel is only disconnected.
    coordinator.kernel.shutdown(KERNEL_SHUTDOWN_TIMEOUT).await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// How long quitting waits for the kernel to shut down before giving up on it.
const KERNEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// The kernel runs next to the notebook: its folder, or the current
/// directory for a bare file name (`nb demo.py`, whose parent is "").
fn kernel_working_dir(notebook: Option<&std::path::Path>) -> PathBuf {
    notebook
        .and_then(|path| path.parent())
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::kernel_working_dir;
    use std::path::Path;

    #[test]
    fn a_bare_file_name_runs_the_kernel_in_the_current_directory() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(kernel_working_dir(Some(Path::new("demo.py"))), here);
        assert_eq!(kernel_working_dir(None), here);
        assert_eq!(
            kernel_working_dir(Some(Path::new("/data/run/exp.ipynb"))),
            Path::new("/data/run")
        );
    }
}

async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
) -> Result<()> {
    let mut input = input::InputReader::spawn();
    let mut fast_tick = tokio::time::interval(Duration::from_millis(50));
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let autosave_secs = coordinator.settings.snapshot().core.autosave_secs.max(1);
    let mut autosave_tick = tokio::time::interval(Duration::from_secs(autosave_secs));
    autosave_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut kernel_open = true;

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let mut trace = LoopTrace::from_env();
    let dog = watchdog::Watchdog::spawn();
    dog.enter(watchdog::Phase::Draw);
    draw_frame(terminal, coordinator)?;
    let mut last_draw = Instant::now();
    let mut pending_draw = false;

    while !coordinator.should_quit {
        let mut redraw = false;
        let mut kind = "idle";
        let mut started = Instant::now();
        dog.enter(watchdog::Phase::Waiting);
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
            maybe_event = input.events.recv() => {
                let Some(event_result) = maybe_event else {
                    // The input thread stopped (the terminal went away): like SIGHUP.
                    coordinator.on_signal("input closed");
                    break;
                };
                let event = event_result?;
                kind = "input";
                dog.enter(watchdog::Phase::Input);
                started = Instant::now();
                handle_terminal_events(
                    terminal,
                    coordinator,
                    mouse_capture,
                    &mut input,
                    event,
                )?;
                redraw = !coordinator.should_quit;
            }
            // Ticks before kernel events: they are rarely ready, and a flood
            // of output must not starve them (autosave, notices).
            _ = fast_tick.tick() => {
                kind = "tick";
                dog.enter(watchdog::Phase::Tick);
                started = Instant::now();
                redraw = coordinator.on_tick(Instant::now());
            }
            _ = autosave_tick.tick() => {
                kind = "autosave";
                dog.enter(watchdog::Phase::Autosave);
                started = Instant::now();
                coordinator.on_autosave_tick();
            }
            kernel_event = nb_kernel::recv_event(coordinator.kernel.as_mut()), if kernel_open => {
                match kernel_event {
                    Some(first) => {
                        kind = "kernel";
                        dog.enter(watchdog::Phase::Kernel);
                        started = Instant::now();
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
            _ = tokio::time::sleep_until((last_draw + FRAME_INTERVAL).into()), if pending_draw => {
                kind = "frame";
                started = Instant::now();
                redraw = true;
            }
        }
        let handled = started.elapsed();
        if redraw && !coordinator.should_quit {
            // Input draws at once; output and ticks at most once a frame.
            if kind == "input" || last_draw.elapsed() >= FRAME_INTERVAL {
                dog.enter(watchdog::Phase::Draw);
                draw_frame(terminal, coordinator)?;
                last_draw = Instant::now();
                pending_draw = false;
            } else {
                pending_draw = true;
            }
        }
        trace.record(coordinator, kind, handled, started.elapsed() - handled);
    }
    Ok(())
}

/// Redraws caused by kernel output or ticks come at most this often.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Loop iterations slower than this are reported in the log overlay.
const SLOW_ITERATION: Duration = Duration::from_millis(100);

/// Per-iteration timing: slow iterations go to the in-app log; with
/// `NB_TRACE=FILE`, every iteration is also appended to FILE.
struct LoopTrace {
    file: Option<std::io::BufWriter<fs::File>>,
    start: Instant,
}

impl LoopTrace {
    fn from_env() -> Self {
        let file = std::env::var_os("NB_TRACE")
            .and_then(|path| fs::File::create(path).ok())
            .map(std::io::BufWriter::new);
        Self {
            file,
            start: Instant::now(),
        }
    }

    fn record(
        &mut self,
        coordinator: &mut Coordinator,
        kind: &str,
        handled: Duration,
        drawn: Duration,
    ) {
        use std::io::Write;
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(
                file,
                "{:.3} {kind} handle_ms={:.1} draw_ms={:.1}",
                self.start.elapsed().as_secs_f64(),
                handled.as_secs_f64() * 1000.0,
                drawn.as_secs_f64() * 1000.0
            );
            let _ = file.flush();
        }
        if handled + drawn >= SLOW_ITERATION {
            coordinator.push_log(format!(
                "slow {kind}: handle {} ms, draw {} ms",
                handled.as_millis(),
                drawn.as_millis()
            ));
        }
    }
}

/// Handle `first` and everything else already queued, then draw once: a
/// burst of trackpad scroll events costs one frame, not one per event.
fn handle_terminal_events(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
    input: &mut input::InputReader,
    first: Event,
) -> Result<()> {
    let mut next = Some(first);
    for _ in 0..512 {
        let Some(event) = next.take() else { break };
        let effect = dispatch_event(coordinator, event);
        if handle_effect(terminal, coordinator, mouse_capture, input, effect)? {
            break;
        }
        if coordinator.should_quit {
            break;
        }
        next = match input.events.try_recv() {
            Ok(event) => Some(event?),
            Err(_) => None,
        };
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

fn handle_effect(
    terminal: &mut ratatui::DefaultTerminal,
    coordinator: &mut Coordinator,
    mouse_capture: bool,
    input: &input::InputReader,
    effect: Effect,
) -> Result<bool> {
    match effect {
        Effect::None => Ok(false),
        Effect::Quit => {
            coordinator.should_quit = true;
            Ok(true)
        }
        Effect::OpenExternalEditor(cell_id) => {
            // The editor reads the terminal itself: stop our reader meanwhile.
            input.pause();
            let edited = external_edit(terminal, coordinator, mouse_capture, cell_id);
            input.resume();
            edited?;
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
    cell_id: nb_core::CellId,
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
            "nb-cell-{}-{stamp}-{attempt}.{extension}",
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
