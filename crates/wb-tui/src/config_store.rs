//! Comment-preserving `toml_edit` configuration store and live-reload `ConfigSnapshot`.

use crate::keymap::Keymap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use toml_edit::DocumentMut;
use wb_sdk::{ConfigWarning, ExtensionRegistry, SettingId, SettingValue, Theme};

pub const DEFAULT_CONFIG_TOML: &str = include_str!("../assets/default_config.toml");

#[derive(Clone, Debug)]
pub struct CoreSettings {
    pub max_output_rows: u16,
    pub wheel_scroll_lines: u16,
    pub insert_escape_chord_ms: u64,
    pub composer_expanded_ratio_pct: u16,
    pub mouse_capture: bool,
    pub formatter: String,
    pub format_on_save: bool,
    pub default_kernel: String,
    pub use_jedi: bool,
    pub complete_on_dot: bool,
    pub signature_help: bool,
    pub max_stream_lines: usize,
    pub autosave_secs: u64,
}

impl Default for CoreSettings {
    fn default() -> Self {
        Self {
            max_output_rows: 18,
            wheel_scroll_lines: 3,
            insert_escape_chord_ms: 350,
            composer_expanded_ratio_pct: 50,
            mouse_capture: true,
            formatter: "ruff format -".into(),
            format_on_save: false,
            default_kernel: "python3".into(),
            use_jedi: false,
            complete_on_dot: true,
            signature_help: true,
            max_stream_lines: 10_000,
            autosave_secs: 30,
        }
    }
}

/// Immutable snapshot of active theme, keymap, core settings, and plugin settings.
#[derive(Debug, Default)]
pub struct ConfigSnapshot {
    pub theme: Theme,
    pub keymap: Keymap,
    pub core: CoreSettings,
    pub plugin_values: HashMap<SettingId, SettingValue>,
    pub warnings: Vec<ConfigWarning>,
}

/// Manages `$XDG_CONFIG_HOME/workbench/config.toml` via `toml_edit::DocumentMut`
/// so interactive `/config` edits preserve user comments and key formatting.
pub struct SettingsStore {
    path: Option<PathBuf>,
    doc: DocumentMut,
    last_mtime: Option<SystemTime>,
    snapshot: Arc<ConfigSnapshot>,
}

impl SettingsStore {
    pub fn load_or_default(_path: Option<PathBuf>, _reg: &ExtensionRegistry) -> Self {
        todo!("wb-tui::config_store: read config.toml via toml_edit, validate leniently, build ConfigSnapshot")
    }

    pub fn in_memory_default() -> Self {
        Self {
            path: None,
            doc: DEFAULT_CONFIG_TOML
                .parse::<DocumentMut>()
                .expect("bundled default_config.toml must parse"),
            last_mtime: None,
            snapshot: Arc::new(ConfigSnapshot::default()),
        }
    }

    pub fn snapshot(&self) -> &Arc<ConfigSnapshot> {
        &self.snapshot
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn document(&self) -> &DocumentMut {
        &self.doc
    }

    pub fn last_mtime(&self) -> Option<SystemTime> {
        self.last_mtime
    }

    pub fn set_value(&mut self, _id: &SettingId, _val: SettingValue, _reg: &ExtensionRegistry) {
        todo!("wb-tui::config_store: mutate self.doc in place, rebuild snapshot, arm 400ms save debounce")
    }

    pub fn poll_external_reload(&mut self, _reg: &ExtensionRegistry) -> bool {
        todo!("wb-tui::config_store: check path mtime and rebuild snapshot if modified externally")
    }
}
