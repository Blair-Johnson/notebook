//! Compile-time `Plugin` trait and `ExtensionRegistry` accumulator.

use crate::command::ActionSpec;
use crate::key::KeyContextSpec;
use crate::output_view::OutputRenderer;
use crate::settings::SettingSpec;
use crate::status::StatusSegment;

pub trait Plugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn register(&self, reg: &mut ExtensionRegistry);
    /// Optional Python bootstrap snippet executed silently (`store_history = false`)
    /// each time an `ipykernel` session becomes `Ready`.
    fn python_bootstrap(&self) -> Option<&'static str> {
        None
    }
}

#[derive(Default)]
pub struct ExtensionRegistry {
    pub contexts: Vec<KeyContextSpec>,
    pub actions: Vec<ActionSpec>,
    pub default_keymap_toml_fragments: Vec<(&'static str, &'static str)>,
    pub settings: Vec<SettingSpec>,
    pub output_renderers: Vec<Box<dyn OutputRenderer>>,
    pub status_segments: Vec<Box<dyn StatusSegment>>,
    pub python_bootstraps: Vec<(&'static str, &'static str)>,
}

impl ExtensionRegistry {
    pub fn install<P: Plugin>(&mut self, plugin: P) {
        let id = plugin.id();
        if let Some(py) = plugin.python_bootstrap() {
            self.python_bootstraps.push((id, py));
        }
        plugin.register(self);
    }

    pub fn add_context(&mut self, spec: KeyContextSpec) -> &mut Self {
        self.contexts.push(spec);
        self
    }

    pub fn add_action(&mut self, spec: ActionSpec) -> &mut Self {
        self.actions.push(spec);
        self
    }

    pub fn add_default_keys_toml(&mut self, owner: &'static str, toml_src: &'static str) -> &mut Self {
        self.default_keymap_toml_fragments.push((owner, toml_src));
        self
    }

    pub fn add_setting(&mut self, spec: SettingSpec) -> &mut Self {
        self.settings.push(spec);
        self
    }

    pub fn add_renderer(&mut self, renderer: impl OutputRenderer) -> &mut Self {
        self.output_renderers.push(Box::new(renderer));
        self
    }

    pub fn add_status_segment(&mut self, segment: impl StatusSegment) -> &mut Self {
        self.status_segments.push(Box::new(segment));
        self
    }
}
