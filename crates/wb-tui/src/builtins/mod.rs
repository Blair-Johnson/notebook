//! Core contexts, actions, settings, and built-in output renderers registered via `wb_sdk::Plugin`.
//!
//! Dogfooding rule: if a core output type cannot be implemented cleanly against
//! `OutputRenderer` and `OutputView`, `wb-sdk` is missing an abstraction.

pub mod markdown;
pub mod plain;
pub mod stream;
pub mod traceback;

use std::borrow::Cow;
use wb_sdk::{
    ActionKind, ActionSpec, ExtensionRegistry, KeyContextId, KeyContextSpec, Plugin, SettingId,
    SettingKind, SettingSpec,
};

pub const DEFAULT_KEYS_TOML: &str = include_str!("../../assets/default_keys.toml");

const THEME_OPTIONS: &[&str] = &["terminal", "midnight", "light", "gruvbox", "solarized-dark"];

fn context(
    reg: &mut ExtensionRegistry,
    id: &'static str,
    title: &'static str,
    parents: &[&'static str],
    text_entry: bool,
    operator_pending: Option<&'static str>,
) {
    reg.add_context(KeyContextSpec {
        id: KeyContextId::new_static(id),
        title: title.into(),
        parents: parents
            .iter()
            .copied()
            .map(KeyContextId::new_static)
            .collect(),
        text_entry,
        operator_pending: operator_pending.map(KeyContextId::new_static),
        visual: id.ends_with(".visual"),
    });
}

fn action(
    reg: &mut ExtensionRegistry,
    id: &'static str,
    kind: ActionKind,
    category: &'static str,
    order: u16,
    title: &'static str,
    cannot_unbind: bool,
) {
    reg.add_action(ActionSpec {
        id: id.to_owned(),
        kind,
        category: category.into(),
        order,
        title: title.into(),
        cannot_unbind,
    });
}

fn setting(
    reg: &mut ExtensionRegistry,
    id: &'static str,
    section: &'static str,
    title: &'static str,
    description: &'static str,
    kind: SettingKind,
) {
    reg.add_setting(SettingSpec {
        id: SettingId(Cow::Borrowed(id)),
        section: section.into(),
        title: title.into(),
        description: description.into(),
        kind,
    });
}

pub struct BuiltinsPlugin;

impl Plugin for BuiltinsPlugin {
    fn id(&self) -> &'static str {
        "core.builtins"
    }

    fn register(&self, reg: &mut ExtensionRegistry) {
        context(reg, "global", "Global", &[], false, None);
        context(
            reg,
            "vim.normal",
            "Shared Vim Motions & Operators",
            &[],
            false,
            None,
        );
        context(
            reg,
            "notebook.normal",
            "Notebook (Cell Navigation)",
            &["vim.normal"],
            false,
            None,
        );
        context(
            reg,
            "text.motions",
            "In-Text Motions (cells and outputs)",
            &[],
            false,
            None,
        );
        context(
            reg,
            "cell.normal",
            "Cell Normal Mode",
            &["text.motions", "vim.normal"],
            false,
            Some("text.pending"),
        );
        context(
            reg,
            "cell.visual",
            "Cell Visual Mode",
            &["text.motions", "vim.normal"],
            false,
            Some("text.pending"),
        );
        context(
            reg,
            "output.normal",
            "Output (read-only text)",
            &["text.motions", "vim.normal"],
            false,
            Some("text.pending"),
        );
        context(
            reg,
            "output.visual",
            "Output Visual Mode",
            &["text.motions", "vim.normal"],
            false,
            Some("text.pending"),
        );
        context(
            reg,
            "text.pending",
            "Cell Operator-Pending Text Objects",
            &[],
            false,
            None,
        );
        context(
            reg,
            "text.insert",
            "Text Entry Editing Keys",
            &[],
            true,
            None,
        );
        context(
            reg,
            "cell.insert",
            "Cell Insert Mode",
            &["text.insert"],
            true,
            None,
        );
        context(
            reg,
            "output.stream",
            "Stream / Text Output",
            &["vim.normal"],
            false,
            None,
        );
        context(
            reg,
            "output.traceback",
            "Traceback Output",
            &["vim.normal"],
            false,
            None,
        );
        context(
            reg,
            "pdb.input",
            "PDB Debugger Input",
            &["text.insert"],
            true,
            None,
        );
        context(
            reg,
            "modal.generic",
            "Modal Dialog",
            &["global"],
            false,
            None,
        );
        context(
            reg,
            "modal.settings",
            "Settings Modal",
            &["global"],
            false,
            None,
        );

        action(
            reg,
            "app.quit",
            ActionKind::Command,
            "Application",
            10,
            "Quit Workbench",
            true,
        );
        action(
            reg,
            "app.palette",
            ActionKind::Command,
            "Application",
            20,
            "Search commands and settings",
            false,
        );
        action(
            reg,
            "app.settings",
            ActionKind::Command,
            "Application",
            30,
            "Open settings",
            false,
        );
        action(
            reg,
            "app.help",
            ActionKind::Command,
            "Application",
            40,
            "Show key help",
            false,
        );
        action(
            reg,
            "app.command_bar",
            ActionKind::Command,
            "Application",
            50,
            "Open command bar",
            false,
        );

        action(
            reg,
            "motion.down",
            ActionKind::Motion,
            "Navigation",
            10,
            "Move down",
            false,
        );
        action(
            reg,
            "motion.up",
            ActionKind::Motion,
            "Navigation",
            20,
            "Move up",
            false,
        );
        action(
            reg,
            "motion.left",
            ActionKind::Motion,
            "Navigation",
            30,
            "Move left",
            false,
        );
        action(
            reg,
            "motion.right",
            ActionKind::Motion,
            "Navigation",
            40,
            "Move right",
            false,
        );
        action(
            reg,
            "motion.top",
            ActionKind::Motion,
            "Navigation",
            50,
            "Move to top",
            false,
        );
        action(
            reg,
            "motion.bottom",
            ActionKind::Motion,
            "Navigation",
            60,
            "Move to bottom",
            false,
        );
        action(
            reg,
            "motion.half_page_down",
            ActionKind::Motion,
            "Navigation",
            70,
            "Half page down",
            false,
        );
        action(
            reg,
            "motion.half_page_up",
            ActionKind::Motion,
            "Navigation",
            80,
            "Half page up",
            false,
        );
        action(
            reg,
            "motion.word_next",
            ActionKind::Motion,
            "Navigation",
            90,
            "Next word",
            false,
        );
        action(
            reg,
            "motion.word_back",
            ActionKind::Motion,
            "Navigation",
            100,
            "Previous word",
            false,
        );
        action(
            reg,
            "motion.word_end",
            ActionKind::Motion,
            "Navigation",
            110,
            "End of word",
            false,
        );
        action(
            reg,
            "motion.line_start",
            ActionKind::Motion,
            "Navigation",
            120,
            "Line start",
            false,
        );
        action(
            reg,
            "motion.line_first_nonblank",
            ActionKind::Motion,
            "Navigation",
            130,
            "First non-blank character",
            false,
        );
        action(
            reg,
            "motion.line_end",
            ActionKind::Motion,
            "Navigation",
            140,
            "Line end",
            false,
        );

        action(
            reg,
            "op.delete",
            ActionKind::Operator,
            "Editing",
            10,
            "Delete",
            false,
        );
        action(
            reg,
            "op.yank",
            ActionKind::Operator,
            "Editing",
            20,
            "Yank",
            false,
        );
        action(
            reg,
            "op.change",
            ActionKind::Operator,
            "Editing",
            30,
            "Change",
            false,
        );
        action(
            reg,
            "edit.paste_after",
            ActionKind::Command,
            "Editing",
            40,
            "Paste after",
            false,
        );
        action(
            reg,
            "edit.paste_before",
            ActionKind::Command,
            "Editing",
            50,
            "Paste before",
            false,
        );
        action(
            reg,
            "edit.undo",
            ActionKind::Command,
            "Editing",
            60,
            "Undo",
            false,
        );
        action(
            reg,
            "edit.redo",
            ActionKind::Command,
            "Editing",
            70,
            "Redo",
            false,
        );
        action(
            reg,
            "edit.delete_char",
            ActionKind::Command,
            "Editing",
            80,
            "Delete character",
            false,
        );
        for (order, id, title) in [
            (81, "edit.backspace", "Delete previous character"),
            (82, "edit.delete_word_back", "Delete word before cursor"),
            (83, "edit.delete_to_line_start", "Delete to start of line"),
            (84, "edit.delete_to_line_end", "Delete to end of line"),
            (101, "edit.indent", "Indent"),
            (102, "edit.dedent", "Dedent"),
        ] {
            action(reg, id, ActionKind::Command, "Typing", order, title, false);
        }
        for (order, id, title) in [
            (85, "edit.delete_char_back", "Delete previous character"),
            (86, "edit.substitute", "Change character"),
            (87, "edit.substitute_line", "Change line"),
            (88, "edit.delete_to_eol", "Delete to end of line"),
            (89, "edit.change_to_eol", "Change to end of line"),
            (90, "edit.yank_line", "Yank line"),
            (91, "edit.toggle_case", "Toggle case"),
            (92, "edit.join_lines", "Join lines"),
            (93, "edit.insert_line_start", "Insert at line start"),
            (94, "edit.append_line_end", "Insert at line end"),
            (95, "edit.open_below", "New line below"),
            (96, "edit.open_above", "New line above"),
            (97, "visual.swap_ends", "Swap selection ends"),
        ] {
            action(reg, id, ActionKind::Command, "Editing", order, title, false);
        }
        for (order, id, title) in [
            (98, "output.jump", "Go to traceback line"),
            (99, "output.cycle_view", "Expand / collapse output"),
            (100, "output.next_tab", "Switch stdout / stderr"),
        ] {
            action(reg, id, ActionKind::Command, "Outputs", order, title, false);
        }
        for (order, id, title) in [
            (60, "motion.current_line", "Current line"),
            (61, "motion.bigword_next", "Next WORD"),
            (62, "motion.bigword_back", "Previous WORD"),
            (63, "motion.bigword_end", "End of WORD"),
            (64, "motion.paragraph_next", "Next paragraph"),
            (65, "motion.paragraph_prev", "Previous paragraph"),
            (66, "motion.match_pair", "Matching bracket"),
            (67, "motion.repeat_find", "Repeat find"),
            (68, "motion.repeat_find_reverse", "Repeat find backward"),
        ] {
            action(reg, id, ActionKind::Motion, "Navigation", order, title, false);
        }
        for (order, id, title) in [
            (70, "motion.find_forward", "Find character"),
            (71, "motion.find_backward", "Find character backward"),
            (72, "motion.till_forward", "Up to character"),
            (73, "motion.till_backward", "Up to character backward"),
        ] {
            action(reg, id, ActionKind::CharArgMotion, "Navigation", order, title, false);
        }
        for (order, id, title) in [
            (40, "op.indent", "Indent lines"),
            (41, "op.dedent", "Dedent lines"),
        ] {
            action(reg, id, ActionKind::Operator, "Editing", order, title, false);
        }
        for (order, name, title) in [
            (10, "bigword", "WORD"),
            (11, "dquote", "double-quoted string"),
            (12, "squote", "single-quoted string"),
            (13, "backtick", "backtick string"),
            (14, "paren", "parentheses"),
            (15, "bracket", "square brackets"),
            (16, "brace", "braces"),
            (17, "angle", "angle brackets"),
            (18, "paragraph", "paragraph"),
        ] {
            for (around, prefix) in [(false, "inner"), (true, "around")] {
                reg.add_action(ActionSpec {
                    id: format!("textobj.{prefix}_{name}"),
                    kind: ActionKind::TextObject,
                    category: "Text Objects".into(),
                    order: order * 2 + u16::from(around),
                    title: format!("{} {title}", if around { "Around" } else { "Inner" }).into(),
                    cannot_unbind: false,
                });
            }
        }
        action(
            reg,
            "edit.replace_char",
            ActionKind::CharArgCommand,
            "Editing",
            90,
            "Replace character",
            false,
        );
        action(
            reg,
            "edit.newline",
            ActionKind::Command,
            "Typing",
            100,
            "New line",
            false,
        );

        action(
            reg,
            "search.open",
            ActionKind::Command,
            "Search",
            10,
            "Open search",
            false,
        );
        action(
            reg,
            "search.next",
            ActionKind::Command,
            "Search",
            20,
            "Next search match",
            false,
        );
        action(
            reg,
            "search.prev",
            ActionKind::Command,
            "Search",
            30,
            "Previous search match",
            false,
        );

        action(
            reg,
            "focus.up",
            ActionKind::Command,
            "Navigation",
            150,
            "Back out one level",
            false,
        );
        action(
            reg,
            "mark.set",
            ActionKind::CharArgCommand,
            "Marks",
            10,
            "Set mark",
            false,
        );
        action(
            reg,
            "mark.jump",
            ActionKind::CharArgCommand,
            "Marks",
            20,
            "Jump to mark",
            false,
        );
        action(
            reg,
            "visual.char",
            ActionKind::Command,
            "Selection",
            10,
            "Select characters",
            false,
        );
        action(
            reg,
            "visual.line",
            ActionKind::Command,
            "Selection",
            20,
            "Select lines",
            false,
        );
        action(
            reg,
            "textobj.inner_word",
            ActionKind::TextObject,
            "Text Objects",
            10,
            "Inner word",
            false,
        );
        action(
            reg,
            "textobj.around_word",
            ActionKind::TextObject,
            "Text Objects",
            20,
            "Around word",
            false,
        );

        action(
            reg,
            "doc.save",
            ActionKind::Command,
            "Document",
            10,
            "Save notebook",
            false,
        );
        action(
            reg,
            "doc.save_force",
            ActionKind::Command,
            "Document",
            20,
            "Force save notebook",
            false,
        );
        action(
            reg,
            "cell.enter_normal",
            ActionKind::Command,
            "Cells",
            10,
            "Open cell",
            false,
        );
        action(
            reg,
            "cell.enter_insert",
            ActionKind::Command,
            "Cells",
            20,
            "Insert at cursor",
            false,
        );
        action(
            reg,
            "cell.append_end",
            ActionKind::Command,
            "Cells",
            30,
            "Insert at end of cell",
            false,
        );
        action(
            reg,
            "cell.append_char",
            ActionKind::Command,
            "Cells",
            40,
            "Insert after cursor",
            false,
        );
        action(
            reg,
            "cell.insert_below",
            ActionKind::Command,
            "Cells",
            50,
            "New cell below",
            false,
        );
        action(
            reg,
            "cell.insert_above",
            ActionKind::Command,
            "Cells",
            60,
            "New cell above",
            false,
        );
        action(
            reg,
            "cell.run",
            ActionKind::Command,
            "Cells",
            90,
            "Run cell",
            false,
        );
        action(
            reg,
            "cell.run_and_advance",
            ActionKind::Command,
            "Cells",
            100,
            "Run cell and select next",
            false,
        );
        action(
            reg,
            "cell.run_all",
            ActionKind::Command,
            "Cells",
            110,
            "Run all cells",
            false,
        );
        action(
            reg,
            "cell.toggle_kind",
            ActionKind::Command,
            "Cells",
            120,
            "Switch code / markdown",
            false,
        );
        action(
            reg,
            "cell.toggle_collapse",
            ActionKind::Command,
            "Cells",
            130,
            "Fold / unfold cell",
            false,
        );
        action(
            reg,
            "cell.focus_output",
            ActionKind::Command,
            "Outputs",
            140,
            "Read output",
            false,
        );
        action(
            reg,
            "cell.edit_external",
            ActionKind::Command,
            "Cells",
            150,
            "Edit cell in $EDITOR",
            false,
        );
        action(
            reg,
            "cell.clear_outputs",
            ActionKind::Command,
            "Outputs",
            160,
            "Clear outputs",
            false,
        );
        action(
            reg,
            "cell.format",
            ActionKind::Command,
            "Cells",
            170,
            "Format cell",
            false,
        );
        action(
            reg,
            "cell.toggle_expand_split",
            ActionKind::Command,
            "Cells",
            180,
            "Expand / shrink editor",
            false,
        );
        action(
            reg,
            "cell.split_at_cursor",
            ActionKind::Command,
            "Cells",
            190,
            "Split cell at cursor",
            false,
        );

        action(
            reg,
            "kernel.interrupt",
            ActionKind::Command,
            "Kernel",
            10,
            "Interrupt running cell",
            false,
        );
        action(
            reg,
            "kernel.restart",
            ActionKind::Command,
            "Kernel",
            20,
            "Restart kernel",
            false,
        );
        action(
            reg,
            "pdb.submit",
            ActionKind::Command,
            "Debugger",
            30,
            "Send input",
            false,
        );
        action(
            reg,
            "pdb.newline",
            ActionKind::Command,
            "Debugger",
            40,
            "New line in input",
            false,
        );
        action(
            reg,
            "pdb.history_prev",
            ActionKind::Command,
            "Debugger",
            50,
            "Previous input",
            false,
        );
        action(
            reg,
            "pdb.history_next",
            ActionKind::Command,
            "Debugger",
            60,
            "Next input",
            false,
        );
        action(
            reg,
            "intel.inspect_doc",
            ActionKind::Command,
            "Code help",
            10,
            "Show documentation",
            false,
        );
        action(
            reg,
            "intel.complete",
            ActionKind::Command,
            "Code help",
            20,
            "Complete",
            false,
        );
        action(
            reg,
            "ui.toggle_mouse_capture",
            ActionKind::Command,
            "UI",
            10,
            "Toggle mouse capture",
            false,
        );

        setting(
            reg,
            "display.theme",
            "display",
            "Theme",
            "Color palette preset",
            SettingKind::Choice {
                default: "midnight",
                options: THEME_OPTIONS,
            },
        );
        setting(
            reg,
            "display.syntect_theme",
            "display",
            "Syntax theme",
            "Syntect theme for code; `auto` follows the color theme",
            SettingKind::Text { default: "auto" },
        );
        setting(
            reg,
            "display.max_output_rows",
            "display",
            "Preview rows",
            "Rows each output shows in preview; outputs that fit have no separate preview",
            SettingKind::Int {
                default: 18,
                min: 1,
                max: 100_000,
            },
        );
        setting(
            reg,
            "display.output_default_view",
            "display",
            "Default output view",
            "How outputs show until changed with space: preview, full or hidden",
            SettingKind::Choice {
                default: "preview",
                options: wb_core::OutputDisplay::NAMES,
            },
        );
        setting(
            reg,
            "display.notice_secs",
            "display",
            "Notice duration",
            "Seconds a status-bar message stays before the key hints return",
            SettingKind::Int {
                default: 4,
                min: 1,
                max: 600,
            },
        );
        setting(
            reg,
            "editor.indent_width",
            "editor",
            "Indent width",
            "Spaces inserted by tab and >, removed by shift+tab and <",
            SettingKind::Int {
                default: 4,
                min: 1,
                max: 16,
            },
        );
        setting(
            reg,
            "display.wheel_scroll_lines",
            "display",
            "Wheel scroll lines",
            "Rows moved by one mouse-wheel tick",
            SettingKind::Int {
                default: 3,
                min: 1,
                max: 1_000,
            },
        );
        setting(
            reg,
            "editor.insert_escape_chord_ms",
            "editor",
            "Insert escape chord",
            "Timeout for a speculative insert-mode chord",
            SettingKind::Int {
                default: 500,
                min: 1,
                max: 10_000,
            },
        );
        setting(
            reg,
            "editor.composer_expanded_ratio_pct",
            "editor",
            "Expanded composer ratio",
            "Percentage of the viewport used by an expanded composer",
            SettingKind::Int {
                default: 50,
                min: 10,
                max: 90,
            },
        );
        setting(
            reg,
            "editor.mouse_capture",
            "editor",
            "Mouse capture",
            "Capture terminal mouse events",
            SettingKind::Bool { default: true },
        );
        setting(
            reg,
            "editor.formatter",
            "editor",
            "Formatter",
            "Command used to format code",
            SettingKind::Text {
                default: "ruff format -",
            },
        );
        setting(
            reg,
            "editor.format_on_save",
            "editor",
            "Format on save",
            "Run the formatter before saving",
            SettingKind::Bool { default: false },
        );
        setting(
            reg,
            "kernel.default_kernel",
            "kernel",
            "Default kernel",
            "Kernel name used for new sessions",
            SettingKind::Text { default: "python3" },
        );
        setting(
            reg,
            "kernel.use_jedi",
            "kernel",
            "Use Jedi",
            "Use Jedi for completion when available",
            SettingKind::Bool { default: false },
        );
        setting(
            reg,
            "kernel.complete_on_dot",
            "kernel",
            "Complete on dot",
            "Trigger completion after a dot",
            SettingKind::Bool { default: true },
        );
        setting(
            reg,
            "kernel.signature_help",
            "kernel",
            "Signature help",
            "Show call signatures while typing",
            SettingKind::Bool { default: true },
        );
        setting(
            reg,
            "kernel.max_stream_lines",
            "kernel",
            "Maximum stream lines",
            "Retain at most this many stream lines",
            SettingKind::Int {
                default: 10_000,
                min: 1,
                max: 1_000_000,
            },
        );
        setting(
            reg,
            "kernel.autosave_secs",
            "kernel",
            "Autosave interval",
            "Autosave interval in seconds",
            SettingKind::Int {
                default: 30,
                min: 0,
                max: 86_400,
            },
        );

        reg.add_default_keys_toml("core.builtins", DEFAULT_KEYS_TOML)
            .add_renderer(stream::StreamRenderer)
            .add_renderer(plain::PlainTextRenderer)
            .add_renderer(traceback::TracebackRenderer);
    }
}
