//! Compile-time extension SDK for nb.
//!
//! Architectural invariant: `nb-sdk` has zero global state and never depends on
//! `nb-tui` or `nb-kernel`. Built-in renderers and future domain plugins (such
//! as JAX pytree or tensor views) implement the exact same traits defined here.

pub mod command;
pub mod key;
pub mod output_view;
pub mod plugin;
pub mod settings;
pub mod status;
pub mod theme;

pub use command::{
    ActionKind, ActionSpec, CommandId, MotionId, MotionOrObject, OperatorId, ResolvedInput,
    TextObjectId,
};
pub use key::{Key, KeyContextId, KeyContextSpec, KeySeq};
pub use output_view::{
    InputOutcome, MountCtx, OutputHost, OutputMetrics, OutputRenderer, OutputView,
};
pub use plugin::{ExtensionRegistry, Plugin};
pub use settings::{ConfigWarning, SettingId, SettingKind, SettingSpec, SettingValue};
pub use status::{BarSlot, StatusCtx, StatusSegment};
pub use theme::{Palette, Theme};
