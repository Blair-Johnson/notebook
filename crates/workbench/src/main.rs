//! CLI entrypoint, plugin wiring list, panic-safe terminal guard, and `tokio::select!` loop.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use wb_core::{Document, FormatBacking};
use wb_kernel::{KernelLaunchSpec, LocalZmqKernel};
use wb_sdk::ExtensionRegistry;
use wb_tui::platform::{NoopTerminalEffects, SystemClipboard};
use wb_tui::{BuiltinsPlugin, Coordinator, SettingsStore};

#[derive(Parser, Debug)]
#[command(name = "workbench", version, about = "Fast terminal Jupyter notebook client")]
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
    let settings = SettingsStore::in_memory_default(); // v1: replace with SettingsStore::load_or_default(cli.config, &registry)

    let working_dir = cli
        .notebook
        .as_deref()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let doc = match &cli.notebook {
        Some(path) if path.exists() => wb_core::codec::open_path(path)?,
        Some(path) if path.extension().is_some_and(|e| e == "py") => {
            Document::new_empty(FormatBacking::PercentPy { header: None })
        }
        _ => Document::new_empty(FormatBacking::Ipynb {
            extra: Default::default(),
        }),
    };

    let kernel = LocalZmqKernel::spawn(KernelLaunchSpec {
        requested_name: cli
            .kernel
            .unwrap_or_else(|| settings.snapshot().core.default_kernel.clone()),
        working_dir,
        existing_connection_file: cli.existing,
    });

    let _coordinator = Coordinator::new(
        cli.notebook,
        doc,
        registry,
        settings,
        Box::new(kernel),
        Box::new(SystemClipboard),
        Box::new(NoopTerminalEffects),
    );

    // v1 main loop:
    // 1. ratatui::init() + clear_screen() + Kitty keyboard disambiguation probe + bracketed paste
    // 2. AssertUnwindSafe(run_event_loop(&mut terminal, &mut coordinator)).catch_unwind().await
    // 3. Emergency sidecar autosave on panic/SIGTERM/SIGHUP + fallible terminal restoration
    anyhow::bail!("workbench skeleton initialized; v1 event loop not yet implemented")
}
