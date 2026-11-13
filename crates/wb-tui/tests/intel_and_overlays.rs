use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wb_kernel::CompletionCandidate;
use wb_sdk::{ExtensionRegistry, KeyContextId, SettingId, SettingValue};
use wb_tui::BuiltinsPlugin;
use wb_tui::config_store::SettingsStore;
use wb_tui::intelligence::{
    CompletionPopup, InspectPager, SignaturePopup, scan_notebook_identifiers,
};
use wb_tui::keymap::Keymap;
use wb_tui::overlays::{
    CommandPaletteState, PdbDockState, PromptBar, PromptKind, SettingsModalOutcome,
    SettingsModalState,
};

fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    registry
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn candidate(text: &str) -> CompletionCandidate {
    CompletionCandidate {
        replace_text: text.into(),
        kind: "word".into(),
        detail: String::new(),
    }
}

#[test]
fn nested_python_calls_and_keyword_arguments_are_parsed() {
    let source = "f(g(1, 2), x=|)";
    let cursor = source.find('|').expect("cursor marker");
    let source = source.replace('|', "");
    let site = SignaturePopup::parse_enclosing_call(&source, cursor).expect("outer call");
    assert_eq!(site.callee, "f");
    assert_eq!(site.callee_end_char, 1);
    assert_eq!(site.active_arg, 1);
    assert_eq!(site.kwarg_name.as_deref(), Some("x"));

    let nested = "f(x[1, 2], '''a, b''', y=|)";
    let cursor = nested.find('|').expect("cursor marker");
    let nested = nested.replace('|', "");
    let site = SignaturePopup::parse_enclosing_call(&nested, cursor).expect("call");
    assert_eq!(site.active_arg, 2);
    assert_eq!(site.kwarg_name.as_deref(), Some("y"));
}

#[test]
fn ansi_inspect_signatures_are_cleaned_and_parameters_highlighted() {
    let doc = "\x1b[31mSignature:\x1b[39m callable(\n    first,\n    second=2,\n    *,\n    named=True,\n)\n\x1b[31mType:\x1b[39m function";
    let popup = SignaturePopup::from_inspect_doc(doc, 1, None).expect("signature");
    assert_eq!(
        popup.signature_text,
        "callable(first, second=2, *, named=True)"
    );
    let range = popup
        .active_param_byte_range
        .expect("active positional parameter");
    assert_eq!(&popup.signature_text[range], "second=2");

    let popup = SignaturePopup::from_inspect_doc(doc, 7, Some("named")).expect("keyword");
    let range = popup
        .active_param_byte_range
        .expect("active keyword parameter");
    assert_eq!(&popup.signature_text[range], "named=True");
}

#[test]
fn completion_popup_filters_live_and_rejects_newlines() {
    let mut popup = CompletionPopup::new(
        42,
        0,
        0,
        vec![
            candidate("print"),
            candidate("property"),
            candidate(".LinSpace"),
        ],
        "pr",
    );
    assert_eq!(popup.filtered.len(), 2);
    assert_eq!(popup.selected_candidate().unwrap().replace_text, "print");

    assert!(popup.update_filter("Pro", 3));
    assert_eq!(popup.replace_end_char, 3);
    assert_eq!(popup.filtered.len(), 1);
    assert_eq!(popup.selected_candidate().unwrap().replace_text, "property");

    assert!(!popup.update_filter("Pro\n", 4));
    assert_eq!(popup.replace_end_char, 3);

    let mut attrs = CompletionPopup::new(1, 2, 2, vec![candidate(".LinSpace")], "");
    assert!(attrs.update_filter("x.l", 3));
    assert_eq!(attrs.filtered.len(), 1);
    attrs.select_next(1);
    attrs.select_prev(1);
    assert_eq!(attrs.selected, 0);
}

#[test]
fn notebook_identifier_scan_skips_strings_comments_and_triple_quotes() {
    let sources = [
        "value_one = 1\ntext = 'hidden_word'  # comment_word\n",
        "'''also_hidden\nacross_lines'''\nsecond_value = value_one + 2",
    ];
    assert_eq!(
        scan_notebook_identifiers(&sources, 3),
        ["value_one", "text", "second_value"]
    );
}

#[test]
fn inspect_pager_scrolls_within_text_bounds() {
    let mut pager = InspectPager::new("docs", "one\ntwo\nthree");
    pager.scroll_down(10);
    assert_eq!(pager.scroll_top, 2);
    pager.scroll_up(1);
    assert_eq!(pager.scroll_top, 1);
    pager.scroll_top();
    assert_eq!(pager.scroll_top, 0);
    pager.scroll_bottom();
    assert_eq!(pager.scroll_top, 2);
}

#[test]
fn prompt_bar_edits_utf8_without_splitting_scalars() {
    let mut prompt = PromptBar::new(PromptKind::Search, "héllo\nignored");
    assert_eq!(prompt.input, "héllo");
    prompt.move_home();
    prompt.move_right();
    assert_eq!(prompt.cursor_byte, "h".len());
    prompt.insert_char('🙂');
    assert_eq!(prompt.input, "h🙂éllo");
    prompt.backspace();
    assert_eq!(prompt.input, "héllo");
    prompt.delete_forward();
    assert_eq!(prompt.input, "hllo");
    prompt.insert_str("é\nnot inserted");
    assert_eq!(prompt.input, "héllo");
    prompt.move_end();
    assert_eq!(prompt.cursor_byte, prompt.input.len());
}

#[test]
fn command_palette_scores_substrings_and_subsequences() {
    let registry = registry();
    let keymap = Keymap::compile(&registry, "");
    let context = KeyContextId::new_static("global");
    let mut palette = CommandPaletteState::new();

    palette.query = "quit".into();
    let matches = palette.query_matches(&registry, &keymap, &context);
    assert_eq!(matches.first().map(|entry| entry.id), Some("app.quit"));

    palette.query = "ctrl+q".into();
    let matches = palette.query_matches(&registry, &keymap, &context);
    assert!(matches.iter().any(|entry| entry.id == "app.quit"));
    assert!(matches.iter().any(|entry| entry.keys.contains("ctrl+q")));

    palette.query = "qwb".into();
    let matches = palette.query_matches(&registry, &keymap, &context);
    assert!(matches.iter().any(|entry| entry.id == "app.quit"));
    palette.select_next(matches.len());
    palette.select_prev(matches.len());
    assert_eq!(palette.selected, 0);
}

#[test]
fn settings_modal_edits_values_filters_and_captures_keybinds() {
    let registry = registry();
    let mut store = SettingsStore::load_or_default(None, &registry);
    let mut modal = SettingsModalState::default();
    assert_eq!(
        SettingsModalState::tabs(),
        &[
            "Display", "Editor", "Kernel", "Theme", "Keybinds", "Plugins"
        ]
    );

    modal.active_tab = 1;
    modal.filter = "format on save".into();
    assert_eq!(modal.filtered_settings(&registry).len(), 1);
    assert_eq!(
        modal.handle_raw_key(key(KeyCode::Enter), &mut store, &registry),
        SettingsModalOutcome::Changed
    );
    assert_eq!(
        store
            .snapshot()
            .plugin_values
            .get(&SettingId(Cow::Borrowed("editor.format_on_save"))),
        Some(&SettingValue::Bool(true))
    );

    modal.active_tab = 0;
    modal.filter = "preview rows".into();
    modal.selected_row = 0;
    assert_eq!(
        modal.handle_raw_key(key(KeyCode::Char('+')), &mut store, &registry),
        SettingsModalOutcome::Changed
    );
    assert_eq!(
        store
            .snapshot()
            .plugin_values
            .get(&SettingId(Cow::Borrowed("display.max_output_rows"))),
        Some(&SettingValue::Int(19))
    );

    modal.active_tab = 3;
    modal.filter = "theme".into();
    modal.selected_row = 0;
    assert_eq!(
        modal.handle_raw_key(key(KeyCode::Right), &mut store, &registry),
        SettingsModalOutcome::Changed
    );
    assert_eq!(
        store
            .snapshot()
            .plugin_values
            .get(&SettingId(Cow::Borrowed("display.theme"))),
        Some(&SettingValue::Text("light".into()))
    );

    modal.active_tab = 4;
    modal.filter = "open settings".into();
    modal.selected_row = 0;
    assert_eq!(
        modal.handle_raw_key(key(KeyCode::Enter), &mut store, &registry),
        SettingsModalOutcome::CaptureStarted
    );
    let outcome = modal.handle_raw_key(key(KeyCode::F(8)), &mut store, &registry);
    assert_eq!(
        outcome,
        SettingsModalOutcome::KeybindCaptured {
            action_id: "app.settings".into(),
            key: "f8".into(),
        }
    );
    assert!(store.document().to_string().contains("f8"));

    modal.active_tab = 0;
    modal.filter = "stale".into();
    assert_eq!(
        modal.handle_raw_key(key(KeyCode::Char('/')), &mut store, &registry),
        SettingsModalOutcome::FilterChanged
    );
    assert!(modal.filter.is_empty());
}

#[test]
fn pdb_history_preserves_draft_and_deduplicates_submissions() {
    let mut pdb = PdbDockState::new(
        "debug",
        false,
        vec!["step".into(), "step".into(), "next".into()],
    );
    pdb.composer.replace_all("draft");
    pdb.history_prev();
    assert_eq!(pdb.composer.as_str(), "next");
    assert_eq!(pdb.unsent_draft.as_deref(), Some("draft"));
    pdb.history_next();
    assert_eq!(pdb.composer.as_str(), "draft");
    assert!(pdb.history_index.is_none());

    pdb.composer.replace_all("next");
    let (submission, history) = pdb.take_submission();
    assert_eq!(submission, "next");
    assert_eq!(history, ["step", "next"]);

    let mut password = PdbDockState::new("Password", true, vec!["old".into()]);
    password.composer.replace_all("secret");
    let (_, history) = password.take_submission();
    assert_eq!(history, ["old"]);
}
