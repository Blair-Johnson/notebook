//! Visual snapshot fixture for nb.
//!
//! Render any frame to PNG so layout and styling can be reviewed as images:
//! - [`driver::HeadlessSession`] runs a real `Coordinator` against a
//!   `TestBackend`, driven by a key script, with a fake or real kernel;
//! - [`tmux::capture_pane`] captures a live pane running the real binary.
//!
//! Both produce a ratatui `Buffer`, which [`image`] rasterizes.

pub mod driver;
pub mod image;
pub mod script;
pub mod tmux;

pub use driver::{HeadlessSession, KernelChoice};
pub use image::{CursorMark, CursorShape, TerminalLook, buffer_text, buffer_to_png, buffer_to_svg};
pub use script::{Step, parse_script};
