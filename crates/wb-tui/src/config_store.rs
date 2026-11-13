//! Comment-preserving `toml_edit` configuration store and live-reload `ConfigSnapshot`.

use crate::keymap::Keymap;
use ratatui::style::Color;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use toml::Value as TomlValue;
use toml_edit::{Array, DocumentMut, Item, Value, value as toml_value};
use wb_sdk::{
    ConfigWarning, ExtensionRegistry, KeyContextId, SettingId, SettingKind, SettingSpec,
    SettingValue, Theme,
};

pub const DEFAULT_CONFIG_TOML: &str = include_str!("../assets/default_config.toml");
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

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
    save_deadline: Option<Instant>,
}

impl SettingsStore {
    /// Resolve an explicit path, then `$WORKBENCH_CONFIG`, XDG config home,
    /// and finally `$HOME/.config/workbench/config.toml`.
    pub fn resolve_config_path(path: Option<PathBuf>) -> Option<PathBuf> {
        if path.is_some() {
            return path;
        }
        if let Some(path) = nonempty_env("WORKBENCH_CONFIG") {
            return Some(PathBuf::from(path));
        }
        if let Some(home) = nonempty_env("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(home).join("workbench/config.toml"));
        }
        nonempty_env("HOME").map(|home| PathBuf::from(home).join(".config/workbench/config.toml"))
    }

    pub fn load_or_default(path: Option<PathBuf>, reg: &ExtensionRegistry) -> Self {
        let path = Self::resolve_config_path(path);
        let mut warnings = Vec::new();
        let (doc, last_mtime) = match path.as_deref() {
            Some(path) => match fs::read_to_string(path) {
                Ok(source) => match source.parse::<DocumentMut>() {
                    Ok(doc) => (doc, file_mtime(path)),
                    Err(error) => {
                        warnings.push(ConfigWarning {
                            source_key: path.display().to_string(),
                            message: format!("invalid TOML; using defaults: {error}"),
                        });
                        (default_document(), file_mtime(path))
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => (default_document(), None),
                Err(error) => {
                    warnings.push(ConfigWarning {
                        source_key: path.display().to_string(),
                        message: format!("could not read config; using defaults: {error}"),
                    });
                    (default_document(), file_mtime(path))
                }
            },
            None => (default_document(), None),
        };
        let snapshot = Arc::new(build_snapshot(&doc, reg, warnings));
        Self {
            path,
            doc,
            last_mtime,
            snapshot,
            save_deadline: None,
        }
    }

    pub fn in_memory_default() -> Self {
        Self {
            path: None,
            doc: default_document(),
            last_mtime: None,
            snapshot: Arc::new(ConfigSnapshot::default()),
            save_deadline: None,
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

    pub fn pending_save(&self) -> bool {
        self.save_deadline.is_some()
    }

    pub fn set_value(&mut self, id: &SettingId, val: SettingValue, reg: &ExtensionRegistry) {
        let (section, key) = setting_path(id, reg);
        self.doc[section.as_str()][key.as_str()] = setting_value_item(&val);
        self.rebuild(reg, Vec::new());
        self.save_deadline = Some(Instant::now() + SAVE_DEBOUNCE);
    }

    /// Replace one context/action array without serializing or rebuilding the
    /// surrounding document, preserving comments and untouched formatting.
    pub fn set_keybinding<I, S>(
        &mut self,
        ctx: &KeyContextId,
        action: &str,
        keys: I,
        reg: &ExtensionRegistry,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut array = Array::new();
        for key in keys {
            array.push(key.as_ref());
        }
        self.doc["keys"][ctx.as_str()][action] = Item::Value(Value::Array(array));
        self.rebuild(reg, Vec::new());
        self.save_deadline = Some(Instant::now() + SAVE_DEBOUNCE);
    }

    /// Flush a pending edit once the debounce interval has elapsed.
    pub fn flush_if_due(&mut self) -> io::Result<bool> {
        if self
            .save_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.flush()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Save immediately using a sibling temporary file and atomic rename.
    pub fn flush(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            self.save_deadline = None;
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_path = path.with_extension(format!("tmp-{}-{stamp}", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        let write_result = (|| -> io::Result<()> {
            file.write_all(self.doc.to_string().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp_path, &path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result?;
        self.last_mtime = file_mtime(&path);
        self.save_deadline = None;
        Ok(())
    }

    /// Alias suitable for event-loop callers.
    pub fn save(&mut self) -> io::Result<()> {
        self.flush()
    }

    /// Alias suitable for event-loop callers that do not want to name the debounce helper.
    pub fn save_if_due(&mut self) -> io::Result<bool> {
        self.flush_if_due()
    }

    pub fn poll_external_reload(&mut self, reg: &ExtensionRegistry) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let Some(mtime) = file_mtime(&path) else {
            return false;
        };
        if self.last_mtime.is_some_and(|last| mtime <= last) {
            return false;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            return false;
        };
        let Ok(doc) = source.parse::<DocumentMut>() else {
            return false;
        };
        self.doc = doc;
        self.last_mtime = Some(mtime);
        self.save_deadline = None;
        self.rebuild(reg, Vec::new());
        true
    }

    fn rebuild(&mut self, reg: &ExtensionRegistry, warnings: Vec<ConfigWarning>) {
        self.snapshot = Arc::new(build_snapshot(&self.doc, reg, warnings));
    }
}

fn default_document() -> DocumentMut {
    DEFAULT_CONFIG_TOML
        .parse::<DocumentMut>()
        .expect("bundled default_config.toml must parse")
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn build_snapshot(
    doc: &DocumentMut,
    reg: &ExtensionRegistry,
    mut warnings: Vec<ConfigWarning>,
) -> ConfigSnapshot {
    let source = doc.to_string();
    let keymap = Keymap::compile(reg, &source);
    warnings.extend(keymap.warnings.clone());

    let parsed = toml::from_str::<TomlValue>(&source).unwrap_or_else(|error| {
        warnings.push(ConfigWarning {
            source_key: "config".into(),
            message: format!("could not validate configuration: {error}"),
        });
        TomlValue::Table(toml::map::Map::new())
    });
    let specs = reg
        .settings
        .iter()
        .map(|spec| (spec.id.0.to_string(), spec))
        .collect::<HashMap<_, _>>();
    let mut values = HashMap::new();
    for spec in &reg.settings {
        let id = spec.id.0.to_string();
        let (section, key) = setting_path_from_spec(spec);
        let raw = parsed
            .get(section.as_str())
            .and_then(TomlValue::as_table)
            .and_then(|table| table.get(key.as_str()));
        let setting = match raw {
            Some(raw) => validate_setting(spec, raw, &id, &mut warnings),
            None => setting_default(&spec.kind),
        };
        values.insert(spec.id.clone(), setting);
    }
    warn_unknown_values(&parsed, &specs, &mut warnings);

    let theme_name = setting_text(&values, "display.theme").unwrap_or_else(|| "terminal".into());
    let mut theme = theme_preset(&theme_name).unwrap_or_else(|| Theme::default());
    if let Some(syntect) = setting_text(&values, "display.syntect_theme") {
        theme.syntect_theme = syntect;
    }
    let core = CoreSettings {
        max_output_rows: setting_int(&values, "display.max_output_rows", 18)
            .clamp(0, u16::MAX as i64) as u16,
        wheel_scroll_lines: setting_int(&values, "display.wheel_scroll_lines", 3)
            .clamp(0, u16::MAX as i64) as u16,
        insert_escape_chord_ms: setting_int(&values, "editor.insert_escape_chord_ms", 350).max(0)
            as u64,
        composer_expanded_ratio_pct: setting_int(&values, "editor.composer_expanded_ratio_pct", 50)
            .clamp(0, u16::MAX as i64) as u16,
        mouse_capture: setting_bool(&values, "editor.mouse_capture", true),
        formatter: setting_text(&values, "editor.formatter")
            .unwrap_or_else(|| "ruff format -".into()),
        format_on_save: setting_bool(&values, "editor.format_on_save", false),
        default_kernel: setting_text(&values, "kernel.default_kernel")
            .unwrap_or_else(|| "python3".into()),
        use_jedi: setting_bool(&values, "kernel.use_jedi", false),
        complete_on_dot: setting_bool(&values, "kernel.complete_on_dot", true),
        signature_help: setting_bool(&values, "kernel.signature_help", true),
        max_stream_lines: setting_int(&values, "kernel.max_stream_lines", 10_000).max(0) as usize,
        autosave_secs: setting_int(&values, "kernel.autosave_secs", 30).max(0) as u64,
    };
    ConfigSnapshot {
        theme,
        keymap,
        core,
        plugin_values: values,
        warnings,
    }
}

fn setting_path(id: &SettingId, reg: &ExtensionRegistry) -> (String, String) {
    if let Some(spec) = reg.settings.iter().find(|spec| spec.id == *id) {
        return setting_path_from_spec(spec);
    }
    split_setting_id(id.0.as_ref())
}

fn setting_path_from_spec(spec: &SettingSpec) -> (String, String) {
    let id = spec.id.0.as_ref();
    let key = id
        .strip_prefix(spec.section.as_ref())
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or_else(|| id.rsplit('.').next().unwrap_or(id));
    (spec.section.to_string(), key.to_string())
}

fn split_setting_id(id: &str) -> (String, String) {
    match id.rsplit_once('.') {
        Some((section, key)) => (section.to_string(), key.to_string()),
        None => ("general".into(), id.into()),
    }
}

fn validate_setting(
    spec: &SettingSpec,
    raw: &TomlValue,
    source_key: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> SettingValue {
    match &spec.kind {
        SettingKind::Bool { default } => match raw.as_bool() {
            Some(value) => SettingValue::Bool(value),
            None => {
                warn_type(warnings, source_key, "expected a boolean");
                SettingValue::Bool(*default)
            }
        },
        SettingKind::Int { default, min, max } => match raw.as_integer() {
            Some(value) if (*min..=*max).contains(&value) => SettingValue::Int(value),
            Some(value) => {
                warnings.push(ConfigWarning {
                    source_key: source_key.into(),
                    message: format!("value {value} is outside the allowed range {min}..={max}"),
                });
                SettingValue::Int(*default)
            }
            None => {
                warn_type(warnings, source_key, "expected an integer");
                SettingValue::Int(*default)
            }
        },
        SettingKind::Choice {
            default, options, ..
        } => match raw.as_str() {
            Some(value) if options.iter().any(|option| *option == value) => {
                SettingValue::Text(value.into())
            }
            Some(value) => {
                warnings.push(ConfigWarning {
                    source_key: source_key.into(),
                    message: format!("unknown choice {value:?}"),
                });
                SettingValue::Text((*default).into())
            }
            None => {
                warn_type(warnings, source_key, "expected a string choice");
                SettingValue::Text((*default).into())
            }
        },
        SettingKind::Text { default } => match raw.as_str() {
            Some(value) => SettingValue::Text(value.into()),
            None => {
                warn_type(warnings, source_key, "expected a string");
                SettingValue::Text((*default).into())
            }
        },
    }
}

fn warn_type(warnings: &mut Vec<ConfigWarning>, source_key: &str, message: &str) {
    warnings.push(ConfigWarning {
        source_key: source_key.into(),
        message: message.into(),
    });
}

fn setting_default(kind: &SettingKind) -> SettingValue {
    match kind {
        SettingKind::Bool { default } => SettingValue::Bool(*default),
        SettingKind::Int { default, .. } => SettingValue::Int(*default),
        SettingKind::Choice { default, .. } => SettingValue::Text((*default).into()),
        SettingKind::Text { default } => SettingValue::Text((*default).into()),
    }
}

fn warn_unknown_values(
    parsed: &TomlValue,
    specs: &HashMap<String, &SettingSpec>,
    warnings: &mut Vec<ConfigWarning>,
) {
    let Some(table) = parsed.as_table() else {
        return;
    };
    for (section, item) in table {
        if section == "keys" {
            continue;
        }
        let Some(section_table) = item.as_table() else {
            warnings.push(ConfigWarning {
                source_key: section.clone(),
                message: "expected a table".into(),
            });
            continue;
        };
        for key in section_table.keys() {
            let id = format!("{section}.{key}");
            if !specs.contains_key(&id) {
                warnings.push(ConfigWarning {
                    source_key: id,
                    message: "unknown setting".into(),
                });
            }
        }
    }
}

fn setting_value_item(value_to_write: &SettingValue) -> Item {
    match value_to_write {
        SettingValue::Bool(item) => toml_value(*item),
        SettingValue::Int(item) => toml_value(*item),
        SettingValue::Text(item) => toml_value(item.as_str()),
    }
}

fn setting_text(values: &HashMap<SettingId, SettingValue>, id: &str) -> Option<String> {
    values.iter().find_map(|(key, value)| {
        (key.0.as_ref() == id).then(|| match value {
            SettingValue::Text(value) => value.clone(),
            _ => String::new(),
        })
    })
}

fn setting_int(values: &HashMap<SettingId, SettingValue>, id: &str, default: i64) -> i64 {
    values
        .iter()
        .find_map(|(key, value)| (key.0.as_ref() == id).then_some(value))
        .and_then(|value| match value {
            SettingValue::Int(value) => Some(*value),
            _ => None,
        })
        .unwrap_or(default)
}

fn setting_bool(values: &HashMap<SettingId, SettingValue>, id: &str, default: bool) -> bool {
    values
        .iter()
        .find_map(|(key, value)| (key.0.as_ref() == id).then_some(value))
        .and_then(|value| match value {
            SettingValue::Bool(value) => Some(*value),
            _ => None,
        })
        .unwrap_or(default)
}

fn theme_preset(name: &str) -> Option<Theme> {
    let (syntect_theme, palette) = match name {
        "terminal" => ("base16-ocean.dark", Theme::default().palette),
        "midnight" => (
            "base16-ocean.dark",
            palette(
                Color::Rgb(8, 12, 24),
                Color::Rgb(18, 24, 42),
                Color::Rgb(12, 18, 34),
                Color::Rgb(32, 48, 82),
                Color::Rgb(224, 232, 255),
                Color::Rgb(132, 148, 184),
                Color::Rgb(66, 86, 128),
                Color::Rgb(96, 180, 255),
            ),
        ),
        "light" => (
            "InspiredGitHub",
            palette(
                Color::Rgb(250, 250, 248),
                Color::Rgb(238, 240, 244),
                Color::Rgb(255, 255, 255),
                Color::Rgb(214, 220, 230),
                Color::Rgb(32, 36, 42),
                Color::Rgb(102, 110, 122),
                Color::Rgb(156, 164, 176),
                Color::Rgb(28, 104, 180),
            ),
        ),
        "gruvbox" => (
            "base16-eighties.dark",
            palette(
                Color::Rgb(40, 40, 32),
                Color::Rgb(60, 56, 40),
                Color::Rgb(48, 45, 38),
                Color::Rgb(102, 82, 54),
                Color::Rgb(235, 219, 178),
                Color::Rgb(168, 153, 132),
                Color::Rgb(124, 111, 96),
                Color::Rgb(215, 153, 33),
            ),
        ),
        "solarized-dark" => (
            "Solarized (dark)",
            palette(
                Color::Rgb(0, 43, 54),
                Color::Rgb(7, 54, 66),
                Color::Rgb(0, 43, 54),
                Color::Rgb(38, 72, 78),
                Color::Rgb(238, 232, 213),
                Color::Rgb(147, 161, 161),
                Color::Rgb(88, 110, 117),
                Color::Rgb(42, 161, 152),
            ),
        ),
        _ => return None,
    };
    Some(Theme {
        name: name.into(),
        revision: 1,
        syntect_theme: syntect_theme.into(),
        palette,
    })
}

fn palette(
    bg: Color,
    surface: Color,
    panel_bg: Color,
    panel_selection: Color,
    fg: Color,
    muted: Color,
    border: Color,
    accent: Color,
) -> wb_sdk::Palette {
    wb_sdk::Palette {
        bg,
        surface,
        panel_bg,
        panel_selection,
        fg,
        muted,
        border,
        border_focused: accent,
        accent_code: accent,
        accent_markdown: Color::Magenta,
        accent_raw: Color::Yellow,
        success: Color::Green,
        warning: Color::Yellow,
        error: Color::Red,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::BuiltinsPlugin;
    use std::borrow::Cow;

    #[test]
    fn defaults_validate_without_warnings() {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        let store = SettingsStore::load_or_default(None, &registry);
        assert!(
            store.snapshot().warnings.is_empty(),
            "{:?}",
            store.snapshot().warnings
        );
        assert_eq!(store.snapshot().theme.name, "terminal");
    }

    #[test]
    fn comments_survive_a_mutation() {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        let path =
            std::env::temp_dir().join(format!("workbench-config-{}.toml", std::process::id()));
        fs::write(
            &path,
            "# keep this\n[display]\n# keep theme\ntheme = \"terminal\"\n",
        )
        .unwrap();
        let mut store = SettingsStore::load_or_default(Some(path.clone()), &registry);
        store.set_value(
            &SettingId(Cow::Borrowed("display.max_output_rows")),
            SettingValue::Int(24),
            &registry,
        );
        assert!(store.document().to_string().contains("# keep this"));
        assert!(store.document().to_string().contains("# keep theme"));
        let _ = fs::remove_file(path);
    }
}
