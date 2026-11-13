//! Work Package 4 coverage for output views, virtualized layout, and pure rendering.

use std::collections::{BTreeMap, HashMap};

use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Color, Modifier},
    text::Line,
};
use serde_json::{Map, Value, json};
use nb_core::{
    Cell, CellId, CellKind, CommId, Document, FormatBacking, OutputData, OutputItem, StreamBuffer,
};
use nb_sdk::{CommandId, MountCtx, OutputHost, OutputView, ResolvedInput, Theme};
use nb_tui::Keymap;
use nb_tui::builtins::markdown::render_markdown_cell;
use nb_tui::builtins::plain::PlainTextView;
use nb_tui::builtins::stream::StreamOutputView;
use nb_tui::builtins::traceback::TracebackView;
use nb_tui::config_store::CoreSettings;
use nb_tui::focus::FocusStack;
use nb_tui::layout::{LayoutCache, MIN_HISTORY_ROWS, TailFollow, ViewportLayout};
use nb_tui::overlays::OverlayStates;
use nb_tui::platform::ClipboardSink;
use nb_tui::render::{AppView, HitTarget, KernelPhase, render_app};

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn stream_item(text: &str) -> OutputItem {
    let buffer: StreamBuffer = serde_json::from_value(json!({
        "text": text,
        "max_lines": 100_000,
        "truncated_lines": 0
    }))
    .expect("StreamBuffer serde fixture");
    OutputItem::new(
        OutputData::Stream {
            stream: nb_core::StreamKind::Stdout,
            buffer,
        },
        false,
    )
}

#[derive(Default)]
struct RecordingHost {
    jump: Option<(CellId, Option<usize>)>,
}

impl OutputHost for RecordingHost {
    fn send_comm(&mut self, _comm_id: CommId, _data: Map<String, Value>) {}

    fn eval_inspect_subshell(&mut self, _tag: &'static str, _python_code: String) {}

    fn copy_text(&mut self, _text: String) {}

    fn jump_to_cell(&mut self, cell_id: CellId, line: Option<usize>) {
        self.jump = Some((cell_id, line));
    }

    fn post_notice(&mut self, _message: String) {}
}

#[test]
fn stream_view_indexes_and_slices_large_output_without_copying_text() {
    let text = (0..50_000)
        .map(|index| format!("line-{index:05}"))
        .collect::<Vec<_>>()
        .join("\n");
    let item = stream_item(&text);
    let theme = Theme::default();
    let mut view = StreamOutputView::default();
    let metrics = view.measure(&item, 32, &theme, Some(7));

    assert_eq!(view.line_row_starts().len(), 50_001);
    assert_eq!(metrics.shown_rows, 7);
    assert!(metrics.clipped);

    let mut rendered = Vec::new();
    view.render_slice(&item, 32, 0..7, false, &theme, &mut rendered);
    assert_eq!(rendered.len(), 7);
    assert_eq!(line_text(&rendered[0]), "line-49993");
    assert_eq!(line_text(rendered.last().expect("tail row")), "line-49999");

    let ansi = stream_item("\x1b[33mwarning\x1b[0m\nplain");
    let mut ansi_view = StreamOutputView::default();
    ansi_view.measure(&ansi, 40, &theme, None);
    let mut ansi_lines = Vec::new();
    ansi_view.render_slice(&ansi, 40, 0..2, false, &theme, &mut ansi_lines);
    assert_eq!(line_text(&ansi_lines[0]), "warning");
    assert_eq!(ansi_lines[0].spans[0].style.fg, Some(Color::Yellow));
}

#[test]
fn plain_view_renders_text_plain_and_mime_fallback_with_tail_follow() {
    let mut bundle = BTreeMap::new();
    bundle.insert(
        "text/plain".to_string(),
        Value::String("first\nsecond\nthird".to_string()),
    );
    let item = OutputItem::new(
        OutputData::Rich {
            bundle,
            metadata: Default::default(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
        false,
    );
    let theme = Theme::default();
    let mut view = PlainTextView::default();
    let metrics = view.measure(&item, 20, &theme, Some(2));
    assert_eq!(metrics.total_rows, 3);
    assert_eq!(metrics.shown_rows, 2);
    let mut lines = Vec::new();
    view.render_slice(&item, 20, 0..2, false, &theme, &mut lines);
    assert_eq!(line_text(&lines[0]), "second");
    assert_eq!(line_text(&lines[1]), "third");

    let mut fallback_bundle = BTreeMap::new();
    fallback_bundle.insert("application/json".to_string(), json!({"ok": true}));
    let fallback = OutputItem::new(
        OutputData::Rich {
            bundle: fallback_bundle,
            metadata: Default::default(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
        false,
    );
    let mut fallback_view = PlainTextView::default();
    fallback_view.measure(&fallback, 40, &theme, None);
    let mut fallback_lines = Vec::new();
    fallback_view.render_slice(&fallback, 40, 0..1, false, &theme, &mut fallback_lines);
    assert_eq!(
        line_text(&fallback_lines[0]),
        "[application/json] {\"ok\":true}"
    );
}

#[test]
fn traceback_view_folds_internal_frames_fixes_yellow_background_and_jumps() {
    let traceback = vec![
        "Traceback (most recent call last):".to_string(),
        "  File \"<cell-1>\", line 4, in <module>\n    answer = 42".to_string(),
        "  File \"/site-packages/jax/core.py\", line 9, in apply\n    return f(x)".to_string(),
        "  File \"/site-packages/jaxlib/xla.py\", line 12, in lower\n    return x".to_string(),
        "\x1b[43mValueError: bad value\x1b[0m".to_string(),
    ];
    let item = OutputItem::new(
        OutputData::Error {
            ename: "ValueError".into(),
            evalue: "bad value".into(),
            traceback,
        },
        false,
    );
    let theme = Theme::default();
    let cell_id = CellId("cell-test".into());
    let ctx = MountCtx {
        cell_id: cell_id.clone(),
        output_id: item.id,
    };
    let mut view = TracebackView::default();
    view.on_output_updated(&item, &ctx);
    view.set_fold_internal_frames(true);
    let metrics = view.measure(&item, 100, &theme, None);
    assert!(metrics.total_rows >= 3);

    let mut lines = Vec::new();
    view.render_slice(&item, 100, 0..metrics.total_rows, false, &theme, &mut lines);
    assert!(
        lines
            .iter()
            .any(|line| line_text(line).contains("internal frame(s) folded"))
    );
    assert!(lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
        span.content.contains("ValueError") && span.style.fg == Some(Color::Black)
    }));

    view.set_fold_internal_frames(false);
    view.measure(&item, 100, &theme, None);
    let mut host = RecordingHost::default();
    let outcome = view.handle_input(
        &item,
        ResolvedInput::Command {
            id: CommandId("focus.enter".into()),
            count: None,
            arg: None,
        },
        &mut host,
    );
    assert_eq!(outcome, nb_sdk::InputOutcome::Consumed);
    assert_eq!(host.jump, Some((cell_id, Some(3))));
}

#[test]
fn markdown_renderer_handles_inline_styles_fences_tables_and_source_mapping() {
    let source = "# Heading\n\n**bold** _italic_ `code` [link](https://example.test)\n\n> quote\n\n| Name | Value |\n| ---- | ----- |\n| alpha | a very long value |\n\n```python\nprint('hello')\n```";
    let rendered = render_markdown_cell(source, 24, &Theme::default());
    assert_eq!(rendered.lines.len(), rendered.source_line_map.len());
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line_text(line).contains("Heading"))
    );
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line_text(line).contains("alpha"))
    );
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line_text(line).contains("python"))
    );
    assert!(
        rendered
            .lines
            .iter()
            .any(|line| line_text(line).contains("quote"))
    );
    assert!(
        rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| {
                span.content == "bold" && span.style.add_modifier(Modifier::BOLD) == span.style
            })
    );
    assert!(
        rendered
            .lines
            .iter()
            .all(|line| line.width() <= 24 || line_text(line).contains("http"))
    );
}

#[test]
fn layout_cache_warms_offscreen_cells_and_evicts_over_budget() {
    let source = "x".repeat(100_000);
    let cells = (0..700)
        .map(|_| Cell::new(CellKind::Raw, source.clone()))
        .collect::<Vec<_>>();
    let document = Document::from_cells(cells, FormatBacking::PercentPy { header: None });
    let theme = Theme::default();
    let mut cache = LayoutCache::default();
    let mut output_views = HashMap::new();
    cache.sync_viewport(&document, &mut output_views, 80, 0, 10, 4, &theme, None);
    assert!(cache.idle_queue_len() > 0);
    assert!(cache.warm_idle_chunk(&document, &mut output_views, 700, 4, &theme) == false);
    assert!(cache.approx_bytes() <= nb_tui::layout::cache::RENDER_CACHE_BUDGET_BYTES);
    assert!(cache.cached_len() < document.order().len());
}

#[test]
fn viewport_layout_and_tail_follow_preserve_required_bands() {
    let layout = ViewportLayout::measure(Rect::new(0, 0, 120, 40), 4, Some(50));
    assert_eq!(layout.top_bar.height, 1);
    assert_eq!(layout.status_bar.height, 1);
    assert_eq!(layout.dock_area.expect("dock").height, 4);
    assert!(layout.notebook_area.height >= MIN_HISTORY_ROWS);
    assert!(layout.expanded_editor_area.is_some());
    assert_eq!(TailFollow::Bottom.offset(100, 20), Some(80));
    assert_eq!(TailFollow::LatestCellTopOrBottom.offset(100, 20), Some(0));
}

#[test]
fn renderer_populates_frame_geometry_on_test_backend_without_mutating_document() {
    let cell = Cell::new(CellKind::Markdown, "# Render me\nbody");
    let selected = cell.id.clone();
    let document = Document::from_cells(vec![cell], FormatBacking::PercentPy { header: None });
    let before = document
        .cell(&selected)
        .expect("selected cell")
        .buffer
        .as_str()
        .to_string();
    let focus = FocusStack::default();
    let overlays = OverlayStates::default();
    let settings = CoreSettings::default();
    let status_segments: Vec<Box<dyn nb_sdk::StatusSegment>> = Vec::new();
    let run_states = HashMap::new();
    let setting_values = HashMap::new();
    let registry = nb_sdk::ExtensionRegistry::default();
    let logs = std::collections::VecDeque::new();
    let view = AppView {
        notebook_row: None,
        path: None,
        document: &document,
        selected_cell: &selected,
        selected_part: nb_tui::focus::CellPart::Source,
        output_text: None,
        focus: &focus,
        scroll_row: 0,
        kernel_name: "python3",
        kernel_busy: false,
        kernel_alive: true,
        kernel_phase: KernelPhase::Ready,
        running_cells: 0,
        run_states: &run_states,
        spinner_tick: 0,
        notice: Some("ready"),
        settings: &settings,
        setting_values: &setting_values,
        registry: &registry,
        status_segments: &status_segments,
        completion: None,
        signature: None,
        inspect_pager: None,
        overlays: &overlays,
        help_scroll: 0,
        logs: &logs,
    };
    let mut output_views = HashMap::new();
    let mut cache = LayoutCache::default();
    let keymap = Keymap::default();
    let theme = Theme::default();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
    let mut geometry = None;
    terminal
        .draw(|frame| {
            geometry = Some(render_app(
                frame,
                &view,
                &mut output_views,
                &theme,
                &keymap,
                &mut cache,
            ));
        })
        .expect("render app");
    let geometry = geometry.expect("geometry");
    assert_eq!(geometry.viewport_rect.width, 120);
    assert!(geometry.row_hits.iter().any(|hit| {
        matches!(hit.target, HitTarget::CellHeader { ref cell_id } if cell_id == &selected)
    }));
    assert!(
        geometry
            .row_hits
            .iter()
            .any(|hit| matches!(hit.target, HitTarget::CellSource { .. }))
    );
    assert!(geometry.hit_at(0, geometry.viewport_rect.y).is_some());
    assert_eq!(
        document
            .cell(&selected)
            .expect("selected cell")
            .buffer
            .as_str(),
        before
    );
}

#[test]
fn clipboard_is_safe_when_tmux_is_not_active() {
    let mut clipboard = nb_tui::platform::SystemClipboard::default();
    let result = clipboard.copy(&"x".repeat(101 * 1024));
    if let Err(error) = result {
        assert!(error.contains("payload") || error.contains("clipboard"));
    }
}
