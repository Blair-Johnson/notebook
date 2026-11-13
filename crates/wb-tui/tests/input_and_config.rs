use std::borrow::Cow;
use std::fs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wb_sdk::{
    ExtensionRegistry, Key, KeyContextId, MotionOrObject, ResolvedInput, SettingId, SettingValue,
};
use wb_tui::builtins::BuiltinsPlugin;
use wb_tui::config_store::SettingsStore;
use wb_tui::grammar::{GrammarStep, VimGrammar};
use wb_tui::keymap::{Keymap, LookupResult, parse_key, parse_key_sequence};

fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    registry
}

fn context(id: &'static str) -> KeyContextId {
    KeyContextId::new_static(id)
}

fn unique_path(label: &str) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "workbench-{label}-{}-{stamp}.toml",
        std::process::id()
    ))
}

#[test]
fn builtins_defaults_compile_without_warnings() {
    let registry = registry();
    let keymap = Keymap::compile(&registry, "");
    assert!(keymap.warnings.is_empty(), "{:?}", keymap.warnings);
    for id in [
        "global",
        "vim.normal",
        "notebook.normal",
        "cell.normal",
        "cell.visual",
        "text.pending",
        "cell.insert",
        "output.stream",
        "output.traceback",
        "pdb.input",
        "modal.generic",
        "modal.settings",
    ] {
        assert!(keymap.context_spec(&context(id)).is_some(), "missing {id}");
    }
    assert_eq!(parse_key("alt+enter"), Some(Key::Alt('\n')));
    assert_eq!(parse_key("ctrl+\\"), Some(Key::Ctrl('\\')));
    assert_eq!(parse_key_sequence("g g").unwrap().len(), 2);
}

#[test]
fn vim_normal_rebinding_inherits_through_multiple_levels() {
    let registry = registry();
    let keymap = Keymap::compile(
        &registry,
        r#"
[keys."vim.normal"]
"motion.down" = ["J"]
"#,
    );
    for id in [
        "vim.normal",
        "notebook.normal",
        "cell.normal",
        "output.stream",
    ] {
        assert_eq!(
            keymap.lookup(&context(id), false, &[Key::Char('J')]),
            LookupResult::Exact("motion.down".into())
        );
        assert_eq!(
            keymap.lookup(&context(id), false, &[Key::Char('j')]),
            LookupResult::None
        );
    }
}

#[test]
fn operator_pending_distinguishes_diw_from_insert_i() {
    let registry = registry();
    let keymap = Keymap::compile(&registry, "");
    let now = Instant::now();

    let mut grammar = VimGrammar::default();
    assert!(matches!(
        grammar.feed(&keymap, &context("cell.normal"), Key::Char('d'), now)[0],
        GrammarStep::Pending
    ));
    assert!(matches!(
        grammar.feed(&keymap, &context("cell.normal"), Key::Char('i'), now)[0],
        GrammarStep::Pending
    ));
    let result = grammar.feed(&keymap, &context("cell.normal"), Key::Char('w'), now);
    assert!(matches!(
        result.as_slice(),
        [GrammarStep::Resolved(ResolvedInput::Operate {
            target: MotionOrObject::TextObject(_),
            ..
        })]
    ));

    let mut grammar = VimGrammar::default();
    let result = grammar.feed(&keymap, &context("cell.normal"), Key::Char('i'), now);
    assert!(matches!(
        result.as_slice(),
        [GrammarStep::Resolved(ResolvedInput::Command { id, .. })]
            if id.0.as_ref() == "cell.enter_insert"
    ));
}

#[test]
fn operator_and_motion_counts_multiply() {
    let registry = registry();
    let keymap = Keymap::compile(&registry, "");
    let now = Instant::now();
    let mut grammar = VimGrammar::default();
    for key in [Key::Char('2'), Key::Char('d'), Key::Char('3')] {
        assert!(matches!(
            grammar.feed(&keymap, &context("cell.normal"), key, now)[0],
            GrammarStep::Pending
        ));
    }
    let result = grammar.feed(&keymap, &context("cell.normal"), Key::Char('j'), now);
    assert!(matches!(
        result.as_slice(),
        [GrammarStep::Resolved(ResolvedInput::Operate {
            op,
            target: MotionOrObject::Motion(_),
            count: 6,
        })] if op.0.as_ref() == "op.delete"
    ));
}

#[test]
fn insert_jj_rolls_back_only_when_completed_before_timeout() {
    let registry = registry();
    let keymap = Keymap::compile(&registry, "");
    let start = Instant::now();

    let mut grammar = VimGrammar::default();
    let first = grammar.feed(&keymap, &context("cell.insert"), Key::Char('j'), start);
    assert_eq!(
        first,
        vec![GrammarStep::Resolved(ResolvedInput::InsertText("j".into()))]
    );
    let second = grammar.feed(
        &keymap,
        &context("cell.insert"),
        Key::Char('j'),
        start + Duration::from_millis(100),
    );
    assert!(matches!(
        second.as_slice(),
        [
            GrammarStep::Resolved(ResolvedInput::RollbackChars(1)),
            GrammarStep::Resolved(ResolvedInput::Command { id, .. }),
        ] if id.0.as_ref() == "focus.up"
    ));

    let mut grammar = VimGrammar::default();
    grammar.feed(&keymap, &context("cell.insert"), Key::Char('j'), start);
    assert!(grammar.expire_chord(start + Duration::from_millis(550)));
    let typed = grammar.feed(
        &keymap,
        &context("cell.insert"),
        Key::Char('j'),
        start + Duration::from_millis(551),
    );
    assert_eq!(
        typed,
        vec![GrammarStep::Resolved(ResolvedInput::InsertText("j".into()))]
    );
}

#[test]
fn conflicts_unbind_safety_and_insert_safety_are_warned() {
    let registry = registry();
    let conflict = Keymap::compile(
        &registry,
        r#"
[keys."vim.normal"]
"motion.down" = ["g"]
"#,
    );
    assert!(
        conflict
            .warnings
            .iter()
            .any(|warning| warning.message.contains("prefix conflict"))
    );

    let cannot_unbind = Keymap::compile(
        &registry,
        r#"
[keys.global]
"app.quit" = []
"#,
    );
    assert!(
        cannot_unbind
            .warnings
            .iter()
            .any(|warning| warning.message.contains("cannot be unbound"))
    );
    assert_eq!(
        cannot_unbind.lookup(&context("cell.normal"), false, &[Key::Ctrl('q')]),
        LookupResult::Exact("app.quit".into())
    );

    let text_safety = Keymap::compile(
        &registry,
        r#"
[keys."cell.insert"]
"focus.up" = ["j"]
"#,
    );
    assert!(
        text_safety
            .warnings
            .iter()
            .any(|warning| warning.message.contains("single printable"))
    );
}

#[test]
fn settings_round_trip_preserves_comments_and_updates_keybindings() {
    let registry = registry();
    let path = unique_path("comments");
    fs::write(
        &path,
        "# file comment\n[display]\n# theme comment\ntheme = \"terminal\"\nmax_output_rows = 18\n",
    )
    .unwrap();

    let mut store = SettingsStore::load_or_default(Some(path.clone()), &registry);
    store.set_value(
        &SettingId(Cow::Borrowed("display.max_output_rows")),
        SettingValue::Int(24),
        &registry,
    );
    store.set_keybinding(
        &context("cell.normal"),
        "cell.run",
        ["ctrl+enter"],
        &registry,
    );
    let rendered = store.document().to_string();
    assert!(rendered.contains("# file comment"));
    assert!(rendered.contains("# theme comment"));
    assert!(rendered.contains("max_output_rows = 24"));
    assert!(rendered.contains("ctrl+enter"));

    store.flush().unwrap();
    let on_disk = fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("# file comment"));
    assert!(on_disk.contains("max_output_rows = 24"));
    assert!(!store.pending_save());
    let _ = fs::remove_file(path);
}

#[test]
fn settings_default_snapshot_is_lenient_and_has_builtin_theme() {
    let registry = registry();
    let path = unique_path("default");
    let store = SettingsStore::load_or_default(Some(path.clone()), &registry);
    assert!(
        store.snapshot().warnings.is_empty(),
        "{:?}",
        store.snapshot().warnings
    );
    assert_eq!(store.snapshot().theme.name, "midnight");
    assert_eq!(store.snapshot().core.max_output_rows, 18);
    let _ = fs::remove_file(path);
}
