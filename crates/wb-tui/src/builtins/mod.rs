//! Core contexts, actions, settings, and built-in output renderers registered via `wb_sdk::Plugin`.
//!
//! Dogfooding rule: if a core output type cannot be implemented cleanly against
//! `OutputRenderer` and `OutputView`, `wb-sdk` is missing an abstraction.

pub mod markdown;
pub mod plain;
pub mod stream;
pub mod traceback;

use wb_sdk::{ExtensionRegistry, KeyContextId, KeyContextSpec, Plugin};

pub const DEFAULT_KEYS_TOML: &str = include_str!("../../assets/default_keys.toml");

pub struct BuiltinsPlugin;

impl Plugin for BuiltinsPlugin {
    fn id(&self) -> &'static str {
        "core.builtins"
    }

    fn register(&self, reg: &mut ExtensionRegistry) {
        reg.add_context(KeyContextSpec {
            id: KeyContextId::new_static("global"),
            title: "Global".into(),
            parents: Vec::new(),
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("vim.normal"),
            title: "Shared Vim Motions & Operators".into(),
            parents: Vec::new(),
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("notebook.normal"),
            title: "Notebook (Cell Navigation)".into(),
            parents: vec![KeyContextId::new_static("vim.normal")],
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("cell.normal"),
            title: "Cell Normal Mode".into(),
            parents: vec![KeyContextId::new_static("vim.normal")],
            text_entry: false,
            operator_pending: Some(KeyContextId::new_static("text.pending")),
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("text.pending"),
            title: "Cell Operator-Pending Text Objects".into(),
            parents: Vec::new(),
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("cell.insert"),
            title: "Cell Insert Mode".into(),
            parents: Vec::new(),
            text_entry: true,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("output.stream"),
            title: "Stream / Text Output".into(),
            parents: vec![KeyContextId::new_static("vim.normal")],
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("output.traceback"),
            title: "Traceback Output".into(),
            parents: vec![KeyContextId::new_static("vim.normal")],
            text_entry: false,
            operator_pending: None,
        })
        .add_context(KeyContextSpec {
            id: KeyContextId::new_static("pdb.input"),
            title: "PDB Debugger Input".into(),
            parents: Vec::new(),
            text_entry: true,
            operator_pending: None,
        })
        .add_default_keys_toml("core.builtins", DEFAULT_KEYS_TOML)
        .add_renderer(stream::StreamRenderer)
        .add_renderer(plain::PlainTextRenderer)
        .add_renderer(traceback::TracebackRenderer);
    }
}
