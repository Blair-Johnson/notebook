//! Terminal UI runtime, input grammar, virtualized layout cache, built-in plugins,
//! and pure rendering pipeline for Workbench.
//!
//! Architectural invariants:
//! - `render::render_app` is read-only over model state and returns `FrameGeometry`.
//! - `Theme`, `Keymap`, and `CoreSettings` flow via `ConfigSnapshot`, never globals.
//! - Built-in outputs register through `wb_sdk::Plugin` via `builtins::BuiltinsPlugin`.

pub mod builtins;
pub mod config_store;
pub mod coordinator;
pub mod focus;
pub mod grammar;
pub mod intelligence;
pub mod keymap;
pub mod layout;
pub mod overlays;
pub mod platform;
pub mod render;
pub mod vim;

pub use builtins::BuiltinsPlugin;
pub use config_store::{ConfigSnapshot, CoreSettings, SettingsStore};
pub use coordinator::{Coordinator, Effect};
pub use focus::{CellMode, FocusLayer, FocusStack, ModalKind};
pub use grammar::{GrammarStep, VimGrammar};
pub use keymap::{HelpRow, Keymap};
pub use render::{AppView, FrameGeometry, render_app};
