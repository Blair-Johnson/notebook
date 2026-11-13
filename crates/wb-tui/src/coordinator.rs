//! Central `Coordinator` state machine and `Effect` chokepoint.

use crate::config_store::{ConfigSnapshot, SettingsStore};
use crate::focus::{CellMode, FocusLayer, FocusStack, ModalKind};
use crate::grammar::{GrammarStep, VimGrammar};
use crate::intelligence::{
    CompletionPopup, InspectPager, SignaturePopup, scan_notebook_identifiers,
};
use crate::keymap::normalize_crossterm_key;
use crate::layout::{LayoutCache, text_width};
use crate::overlays::{
    CommandPaletteState, ConfirmAction, OverlayStates, PdbDockState, PromptBar, PromptKind,
    SettingsModalOutcome,
};
use crate::platform::{
    ClipboardSink, CursorShape, MemoryClipboard, NoopTerminalEffects, TerminalEffects,
};
use crate::render::{
    AppView, FrameGeometry, HitTarget, KernelPhase, RunState, frame_layout, render_app,
};
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};
use wb_core::codec::ipynb::{parse_ipynb, serialize_ipynb};
use wb_core::codec::{reload_external, save_path, write_atomic};
use wb_core::{
    Cell, CellId, CellKind, CommId, CursorPos, DocOp, Document, OutputData, OutputId, OutputItem,
    Selection, StreamKind, TargetId, VisualKind,
};
use wb_kernel::{CompletionCandidate, KernelClient, KernelCmd, KernelEvent, Lane, WireOutput};
use wb_sdk::{
    ExtensionRegistry, InputOutcome, Key, KeyContextId, MotionOrObject, MountCtx, OutputHost,
    OutputView, ResolvedInput,
};

#[derive(Debug)]
pub enum Effect {
    None,
    OpenExternalEditor(CellId),
    Quit,
}

#[derive(Clone, Debug)]
struct CompletionRequest {
    cell_id: CellId,
    replace_start_char: usize,
    replace_end_char: usize,
    typed_prefix: String,
}

#[derive(Clone, Debug)]
enum PendingInspect {
    Signature {
        active_arg: usize,
        keyword: Option<String>,
    },
    Pager {
        title: String,
    },
}

pub struct Coordinator {
    pub path: Option<PathBuf>,
    pub document: Document,
    pub selected_cell: CellId,
    pub focus: FocusStack,
    pub grammar: VimGrammar,
    pub scroll_row: u32,
    pub should_quit: bool,
    pub kernel_name: String,
    pub kernel_busy: bool,
    pub kernel_alive: bool,
    /// Per-cell execution state shown in cell headers.
    pub run_states: HashMap<CellId, RunState>,
    pub kernel_phase: KernelPhase,
    spinner_tick: usize,
    /// Scroll to keep the selection (or edit cursor) visible on the next
    /// draw. Set by keyboard actions; cleared by mouse-wheel scrolling.
    follow_focus: bool,
    pub notice: Option<String>,
    pub registry: ExtensionRegistry,
    pub settings: SettingsStore,
    pub output_views: HashMap<OutputId, Box<dyn OutputView>>,
    pub layout_cache: LayoutCache,
    pub last_geometry: FrameGeometry,
    pub completion: Option<CompletionPopup>,
    pub signature: Option<SignaturePopup>,
    pub inspect_pager: Option<InspectPager>,
    pub overlays: OverlayStates,
    pub kernel: Box<dyn KernelClient>,
    pub clipboard: Box<dyn ClipboardSink>,
    pub term_effects: Box<dyn TerminalEffects>,
    marks: HashMap<char, (CellId, usize)>,
    yank_cells: Vec<Cell>,
    yank_text: String,
    cell_baselines: HashMap<CellId, String>,
    search_query: Option<String>,
    logs: VecDeque<String>,
    help_scroll: usize,
    queued_runs: VecDeque<CellId>,
    run_all_queued: bool,
    pending_completions: HashMap<u64, CompletionRequest>,
    pending_inspects: HashMap<u64, PendingInspect>,
    next_request_id: u64,
    pending_clear_wait: HashSet<CellId>,
    observed_file_mtime: Option<SystemTime>,
    last_autosave: Option<Instant>,
}

impl Coordinator {
    pub fn new(
        path: Option<PathBuf>,
        document: Document,
        registry: ExtensionRegistry,
        settings: SettingsStore,
        kernel: Box<dyn KernelClient>,
        clipboard: Box<dyn ClipboardSink>,
        term_effects: Box<dyn TerminalEffects>,
    ) -> Self {
        let selected_cell = document
            .order()
            .first()
            .cloned()
            .unwrap_or_else(CellId::new_random);
        let settings = if settings
            .snapshot()
            .keymap
            .context_spec(&KeyContextId::new_static("notebook.normal"))
            .is_none()
        {
            SettingsStore::load_or_default(settings.config_path().map(Path::to_path_buf), &registry)
        } else {
            settings
        };
        let mut coordinator = Self {
            path,
            document,
            selected_cell,
            focus: FocusStack::default(),
            grammar: VimGrammar::default(),
            scroll_row: 0,
            should_quit: false,
            kernel_name: settings.snapshot().core.default_kernel.clone(),
            kernel_busy: false,
            kernel_alive: false,
            run_states: HashMap::new(),
            kernel_phase: KernelPhase::Starting,
            spinner_tick: 0,
            follow_focus: true,
            notice: None,
            registry,
            settings,
            output_views: HashMap::new(),
            layout_cache: LayoutCache::default(),
            last_geometry: FrameGeometry::default(),
            completion: None,
            signature: None,
            inspect_pager: None,
            overlays: OverlayStates::default(),
            kernel,
            clipboard,
            term_effects,
            marks: HashMap::new(),
            yank_cells: Vec::new(),
            yank_text: String::new(),
            cell_baselines: HashMap::new(),
            search_query: None,
            logs: VecDeque::with_capacity(2_000),
            help_scroll: 0,
            queued_runs: VecDeque::new(),
            run_all_queued: false,
            pending_completions: HashMap::new(),
            pending_inspects: HashMap::new(),
            next_request_id: 1,
            pending_clear_wait: HashSet::new(),
            observed_file_mtime: None,
            last_autosave: None,
        };
        coordinator.sync_settings();
        coordinator.observed_file_mtime = coordinator.path.as_deref().and_then(file_mtime);
        coordinator.remount_output_views();
        coordinator
    }

    pub fn headless(
        document: Document,
        registry: ExtensionRegistry,
        kernel: Box<dyn KernelClient>,
    ) -> Self {
        Self::new(
            None,
            document,
            registry,
            SettingsStore::in_memory_default(),
            kernel,
            Box::new(MemoryClipboard::default()),
            Box::new(NoopTerminalEffects),
        )
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let snap = self.settings.snapshot().clone();
        self.keep_focus_visible(frame.area(), &snap);
        let view = AppView {
            path: self.path.as_deref(),
            document: &self.document,
            selected_cell: &self.selected_cell,
            focus: &self.focus,
            scroll_row: self.scroll_row,
            kernel_name: &self.kernel_name,
            kernel_busy: self.kernel_busy,
            kernel_alive: self.kernel_alive,
            kernel_phase: self.kernel_phase,
            running_cells: self.active_runs(),
            run_states: &self.run_states,
            spinner_tick: self.spinner_tick,
            notice: self.notice.as_deref(),
            settings: &snap.core,
            setting_values: &snap.plugin_values,
            registry: &self.registry,
            status_segments: &self.registry.status_segments,
            completion: self.completion.as_ref(),
            signature: self.signature.as_ref(),
            inspect_pager: self.inspect_pager.as_ref(),
            overlays: &self.overlays,
            help_scroll: self.help_scroll,
            logs: &self.logs,
        };
        self.last_geometry = render_app(
            frame,
            &view,
            &mut self.output_views,
            &snap.theme,
            &snap.keymap,
            &mut self.layout_cache,
        );
        // One place decides the cursor shape: a bar wherever text is typed.
        let shape = match self.focus.top() {
            FocusLayer::CellEdit {
                mode: CellMode::Insert,
                ..
            }
            | FocusLayer::PdbInput
            | FocusLayer::Modal(
                ModalKind::SearchPrompt | ModalKind::ExCommandBar | ModalKind::CommandPalette,
            ) => CursorShape::SteadyBar,
            _ => CursorShape::SteadyBlock,
        };
        self.term_effects.set_cursor_shape(shape);
    }

    /// Scroll so the edit cursor (or the selected cell) is visible after a
    /// keyboard action, and clamp scrolling to the content.
    fn keep_focus_visible(&mut self, area: Rect, snap: &ConfigSnapshot) {
        let layout = frame_layout(
            area,
            self.overlays.pdb_dock.is_some(),
            &self.focus,
            &snap.core,
        );
        let viewport = layout.notebook_area;
        let text_w = text_width(viewport.width);
        let editing = match self.focus.base() {
            FocusLayer::CellEdit { cell_id, .. } => Some(cell_id.clone()),
            _ => None,
        };
        self.layout_cache.sync_viewport(
            &self.document,
            &mut self.output_views,
            text_w,
            self.scroll_row,
            viewport.height,
            snap.core.max_output_rows,
            &snap.theme,
            editing.as_ref(),
        );
        let height = u32::from(viewport.height.max(1));
        if self.follow_focus {
            self.follow_focus = false;
            if let Some(index) = self.document.position_of(&self.selected_cell) {
                let heights = self.layout_cache.heights();
                if index < heights.len() {
                    let top = heights.prefix_sum(index);
                    let cell_rows = heights.height(index);
                    let (want_top, want_bottom) = match editing.as_ref() {
                        Some(id) if id == &self.selected_cell => {
                            let row = top + 1 + self.cursor_visual_row(id, text_w);
                            (row, row + 1)
                        }
                        _ => (top, top + cell_rows.min(height)),
                    };
                    if want_top < self.scroll_row {
                        self.scroll_row = want_top;
                    } else if want_bottom > self.scroll_row + height {
                        self.scroll_row = want_bottom - height;
                    }
                }
            }
        }
        let total = self.layout_cache.heights().total_height();
        self.scroll_row = self.scroll_row.min(total.saturating_sub(height));
    }

    /// Wrapped source row of the edit cursor within its cell (excluding the
    /// header row), mirroring the layout cache's character wrapping.
    fn cursor_visual_row(&self, cell_id: &CellId, text_w: u16) -> u32 {
        let Some(cell) = self.document.cell(cell_id) else {
            return 0;
        };
        let width = usize::from(text_w.max(1));
        let cursor = cell.buffer.cursor();
        let mut rows = 0u32;
        for (row, line) in cell.buffer.as_str().split('\n').enumerate() {
            let mut used = 0usize;
            let mut wraps = 0u32;
            for (index, ch) in line.chars().enumerate() {
                if row == cursor.row && index >= cursor.col {
                    break;
                }
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cw > 0 && used > 0 && used + cw > width {
                    wraps += 1;
                    used = 0;
                }
                used += cw;
            }
            if row == cursor.row {
                return rows + wraps;
            }
            rows += 1 + wraps;
        }
        rows
    }

    /// Popups belong to insert mode: drop them as soon as it is left, and
    /// never keep an empty completion list on screen.
    fn reconcile_popups(&mut self) {
        let in_insert = matches!(
            self.focus.top(),
            FocusLayer::CellEdit {
                mode: CellMode::Insert,
                ..
            }
        );
        if !in_insert {
            self.completion = None;
            self.signature = None;
            self.pending_completions.clear();
        }
        if self
            .completion
            .as_ref()
            .is_some_and(|popup| popup.filtered.is_empty())
        {
            self.completion = None;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Effect {
        self.follow_focus = true;
        let effect = self.on_key_inner(key, now);
        self.reconcile_popups();
        effect
    }

    fn on_key_inner(&mut self, key: KeyEvent, now: Instant) -> Effect {
        if matches!(self.focus.top(), FocusLayer::Modal(ModalKind::Settings)) {
            return self.on_settings_key(key);
        }
        if let Some(effect) = self.on_modal_key(key) {
            return effect;
        }
        if matches!(
            self.focus.top(),
            FocusLayer::CellEdit {
                mode: CellMode::Insert,
                ..
            }
        ) && self.completion.is_some()
        {
            if let Some(effect) = self.on_completion_key(key) {
                return effect;
            }
        }
        let Some(normalized) = normalize_crossterm_key(key) else {
            return Effect::None;
        };
        if matches!(normalized, Key::Esc) && matches!(self.focus.top(), FocusLayer::PdbInput) {
            self.pop_pdb();
            return Effect::None;
        }
        let context = self.focus.top().key_context();
        let steps = {
            let keymap = &self.settings.snapshot().keymap;
            self.grammar.feed(keymap, &context, normalized, now)
        };
        let mut effect = Effect::None;
        for step in steps {
            if let Some(next) = self.dispatch_grammar_step(step) {
                effect = next;
            }
        }
        effect
    }

    pub fn on_paste(&mut self, text: String) {
        match self.focus.top().clone() {
            FocusLayer::CellEdit {
                cell_id,
                mode: CellMode::Insert,
                ..
            } => {
                self.insert_cell_text(&cell_id, &text);
                self.after_cell_insert(&cell_id);
            }
            FocusLayer::PdbInput => {
                if let Some(pdb) = self.overlays.pdb_dock.as_mut() {
                    pdb.composer.insert_str(&text);
                }
            }
            FocusLayer::Modal(ModalKind::SearchPrompt | ModalKind::ExCommandBar) => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.insert_str(&text);
                }
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let wheel_lines = u32::from(self.settings.snapshot().core.wheel_scroll_lines.max(1));
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.follow_focus = false;
                self.scroll_row = self.scroll_row.saturating_sub(wheel_lines);
                return;
            }
            MouseEventKind::ScrollDown => {
                self.follow_focus = false;
                self.scroll_row = self.scroll_row.saturating_add(wheel_lines);
                return;
            }
            MouseEventKind::Down(_) => {}
            _ => return,
        }
        let Some(target) = self.last_geometry.hit_at(mouse.column, mouse.row).cloned() else {
            return;
        };
        match target {
            HitTarget::CellHeader { cell_id } => self.select_cell(cell_id),
            HitTarget::CellSource {
                cell_id,
                line,
                char_col,
            } => {
                self.select_cell(cell_id.clone());
                if let Some(cell) = self.document.cell_mut(&cell_id) {
                    cell.buffer.set_cursor(CursorPos {
                        row: line,
                        col: char_col,
                    });
                }
            }
            HitTarget::CellOutput {
                cell_id, output_id, ..
            } => {
                self.select_cell(cell_id.clone());
                if let Some(view) = self.output_views.get(&output_id) {
                    if let Some(context) = view.key_context() {
                        self.focus.push(FocusLayer::OutputFocus {
                            cell_id,
                            output_id,
                            context,
                        });
                    }
                }
            }
            HitTarget::FoldToggle { cell_id } => {
                if let Some(cell) = self.document.cell_mut(&cell_id) {
                    cell.collapsed = !cell.collapsed;
                    cell.bump_chrome();
                }
                self.document.dirty_source = true;
            }
            HitTarget::Hyperlink { url } => self.notice = Some(url),
        }
    }

    pub fn apply_kernel_event(&mut self, event: KernelEvent) {
        match event {
            KernelEvent::Ready { info } => {
                self.kernel_alive = true;
                self.kernel_busy = false;
                self.kernel_phase = KernelPhase::Ready;
                self.kernel_name = info.kernel_name;
                self.notice = Some("kernel ready".into());
                if !self.settings.snapshot().core.use_jedi {
                    self.send_command(KernelCmd::Execute {
                        target: TargetId::Silent("config.completer".to_string()),
                        lane: Lane::Main,
                        code: "%config Completer.use_jedi = False".into(),
                        silent: true,
                        store_history: false,
                        stop_on_error: false,
                    });
                }
                for (plugin, code) in self.registry.python_bootstraps.clone() {
                    self.send_command(KernelCmd::Execute {
                        target: TargetId::Silent(plugin.to_string()),
                        lane: Lane::Main,
                        code: code.into(),
                        silent: true,
                        store_history: false,
                        stop_on_error: false,
                    });
                }
                self.flush_queued_runs();
            }
            KernelEvent::LaunchFailed { reason, .. } => {
                self.kernel_alive = false;
                self.kernel_busy = false;
                self.kernel_phase = KernelPhase::Dead;
                self.notice = Some(format!("kernel launch failed: {reason}"));
            }
            KernelEvent::ExecutionStarted {
                target,
                execution_count,
                ..
            } => {
                self.kernel_busy = true;
                if let TargetId::Cell(cell_id) = target {
                    // The actor reports a start twice: on submission (no count
                    // yet, so still queued behind earlier work) and on the
                    // kernel's `execute_input` (count known, really running).
                    let state = if execution_count.is_some() {
                        RunState::Running
                    } else {
                        RunState::Queued
                    };
                    self.run_states.insert(cell_id.clone(), state);
                    if let Some(cell) = self.document.cell_mut(&cell_id)
                        && execution_count.is_some()
                    {
                        cell.execution_count = execution_count;
                        cell.bump_chrome();
                    }
                }
            }
            KernelEvent::Output {
                target,
                late,
                payload,
                ..
            } => self.apply_output(target, late, payload),
            KernelEvent::UpdateDisplay {
                display_id,
                bundle,
                metadata,
            } => {
                let affected = self.document.update_display(&display_id, bundle, metadata);
                for cell_id in affected {
                    self.update_views_for_cell(&cell_id);
                }
            }
            KernelEvent::ClearOutput { target, wait } => {
                if let TargetId::Cell(cell_id) = target {
                    if wait {
                        self.pending_clear_wait.insert(cell_id);
                    } else {
                        self.clear_outputs(&cell_id);
                    }
                }
            }
            KernelEvent::ExecutionFinished {
                target,
                ok,
                elapsed,
                ..
            } => {
                if let TargetId::Cell(cell_id) = target {
                    self.run_states.insert(
                        cell_id.clone(),
                        if ok { RunState::Done } else { RunState::Failed },
                    );
                    if let Some(cell) = self.document.cell_mut(&cell_id) {
                        cell.last_elapsed = Some(elapsed);
                        cell.bump_chrome();
                    }
                    if !ok {
                        self.notice = Some("execution failed".into());
                    }
                }
                self.kernel_busy = self.active_runs() > 0;
                if matches!(self.focus.top(), FocusLayer::PdbInput) {
                    self.pop_pdb();
                }
            }
            KernelEvent::BusyStateChanged { busy } => self.kernel_busy = busy,
            KernelEvent::CompleteReply {
                req_id,
                start_char,
                items,
            } => self.apply_completion_reply(req_id, start_char, items),
            KernelEvent::InspectReply { req_id, doc } => self.apply_inspect_reply(req_id, doc),
            KernelEvent::InputRequested { prompt, password } => {
                if !matches!(self.focus.top(), FocusLayer::PdbInput) {
                    self.focus.push(FocusLayer::PdbInput);
                }
                self.overlays.pdb_dock = Some(PdbDockState::new(prompt, password, Vec::new()));
                self.term_effects.set_cursor_shape(CursorShape::SteadyBar);
            }
            KernelEvent::CommOpened { .. } => {}
            KernelEvent::CommMessage { comm_id, data } => self.forward_comm(&comm_id, &data),
            KernelEvent::CommClosed { .. } => {}
            KernelEvent::ProcessLog { stream, line } => {
                let prefix = match stream {
                    StreamKind::Stdout => "stdout",
                    StreamKind::Stderr => "stderr",
                };
                self.push_log(format!("[{prefix}] {line}"));
            }
            KernelEvent::Died {
                exit_status,
                stderr_tail,
                ..
            } => {
                self.kernel_alive = false;
                self.kernel_busy = false;
                self.kernel_phase = KernelPhase::Dead;
                for state in self.run_states.values_mut() {
                    if matches!(state, RunState::Running | RunState::Queued) {
                        *state = RunState::Failed;
                    }
                }
                self.notice = Some(format!("kernel died: {exit_status}"));
                for line in stderr_tail {
                    self.push_log(format!("[kernel] {line}"));
                }
            }
        }
    }

    pub fn check_autosave_recovery(&mut self) {
        let Some(path) = Self::autosave_path_for(self.path.as_deref()) else {
            return;
        };
        if path.is_file() {
            self.overlays.confirm = Some(ConfirmAction::RecoverAutosaveSidecar);
            if !matches!(self.focus.top(), FocusLayer::Modal(ModalKind::ConfirmBar)) {
                self.focus.push(FocusLayer::Modal(ModalKind::ConfirmBar));
            }
        }
    }

    pub fn on_tick(&mut self, now: Instant) -> bool {
        let mut changed = self.grammar.expire_chord(now);
        if self.active_runs() > 0 {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            changed = true;
        }
        changed |= self.check_external_disk_change();
        if self.settings.save_if_due().is_err() {
            self.notice = Some("could not save settings".into());
            changed = true;
        }
        let snap = self.settings.snapshot().clone();
        changed |= self.layout_cache.warm_idle_chunk(
            &self.document,
            &mut self.output_views,
            2,
            snap.core.max_output_rows,
            &snap.theme,
        );
        changed
    }

    pub fn on_autosave_tick(&mut self) {
        let interval = self.settings.snapshot().core.autosave_secs;
        if self.document.dirty_source || self.document.dirty_outputs {
            let due = interval == 0
                || self
                    .last_autosave
                    .is_none_or(|last| last.elapsed() >= Duration::from_secs(interval));
            if due {
                self.emergency_autosave();
                self.last_autosave = Some(Instant::now());
            }
        }
        self.check_external_disk_change();
    }

    pub fn on_resize(&mut self) {}

    pub fn on_focus_gained(&mut self) {
        if self.settings.poll_external_reload(&self.registry) {
            self.sync_settings();
        }
        self.check_external_disk_change();
    }

    pub fn on_signal(&mut self, _signal_name: &str) {
        self.emergency_autosave();
        self.should_quit = true;
    }

    pub fn emergency_autosave(&mut self) {
        let Some(path) = Self::autosave_path_for(self.path.as_deref()) else {
            return;
        };
        let Ok(bytes) = serialize_ipynb(&self.document, true) else {
            self.notice = Some("could not serialize emergency autosave".into());
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if write_atomic(&path, bytes.as_bytes()).is_err() {
            self.notice = Some("could not write emergency autosave".into());
        }
    }

    pub fn apply_external_edit(&mut self, cell_id: &CellId, new_source: String) {
        let Some(old_source) = self
            .document
            .cell(cell_id)
            .map(|cell| cell.buffer.as_str().to_string())
        else {
            return;
        };
        if old_source != new_source {
            self.document.apply(DocOp::CommitCellSource {
                cell_id: cell_id.clone(),
                old_source,
                new_source,
            });
        }
    }

    /// Deterministic sidecar path under `$WORKBENCH_STATE_DIR` or `$XDG_STATE_HOME/workbench/autosave/`.
    pub fn autosave_path_for(path: Option<&Path>) -> Option<PathBuf> {
        let state_root = std::env::var_os("WORKBENCH_STATE_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_STATE_HOME")
                    .filter(|v| !v.is_empty())
                    .map(|p| PathBuf::from(p).join("workbench"))
            })
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/state/workbench"))
            })?;
        let raw_key = match path {
            Some(p) => {
                let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
                abs.to_string_lossy().replace('%', "%%").replace('/', "%")
            }
            None => "scratchpad".to_string(),
        };
        let capped = match raw_key.char_indices().rev().nth(199) {
            Some((idx, _)) => raw_key[idx..].to_string(),
            None => raw_key,
        };
        Some(state_root.join("autosave").join(format!("{capped}.ipynb")))
    }

    fn sync_settings(&mut self) {
        let snap = self.settings.snapshot().clone();
        self.grammar.insert_escape_chord = Duration::from_millis(snap.core.insert_escape_chord_ms);
        self.term_effects.set_mouse_capture(snap.core.mouse_capture);
    }

    fn dispatch_grammar_step(&mut self, step: GrammarStep) -> Option<Effect> {
        match step {
            GrammarStep::Resolved(input) => Some(self.dispatch_input(input)),
            GrammarStep::Pending | GrammarStep::Cancelled | GrammarStep::Unhandled(_) => None,
        }
    }

    fn dispatch_input(&mut self, input: ResolvedInput) -> Effect {
        match self.focus.top().clone() {
            FocusLayer::NotebookNormal => self.dispatch_notebook(input),
            FocusLayer::CellEdit { cell_id, mode, .. } => {
                self.dispatch_cell_edit(&cell_id, mode, input)
            }
            FocusLayer::OutputFocus {
                cell_id, output_id, ..
            } => self.dispatch_output(&cell_id, output_id, input),
            FocusLayer::PdbInput => {
                self.dispatch_pdb(input);
                Effect::None
            }
            FocusLayer::Modal(_) => Effect::None,
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) -> Effect {
        let outcome =
            self.overlays
                .settings
                .handle_raw_key(key, &mut self.settings, &self.registry);
        if !matches!(outcome, SettingsModalOutcome::Noop) {
            self.sync_settings();
        }
        if matches!(outcome, SettingsModalOutcome::Close) {
            self.focus.pop();
        }
        Effect::None
    }

    fn on_modal_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let layer = match self.focus.top() {
            FocusLayer::Modal(kind) => *kind,
            _ => return None,
        };
        let normalized = normalize_crossterm_key(key)?;
        match layer {
            ModalKind::CommandPalette => Some(self.on_palette_key(normalized)),
            ModalKind::SearchPrompt | ModalKind::ExCommandBar => {
                Some(self.on_prompt_key(normalized))
            }
            ModalKind::ConfirmBar => Some(self.on_confirm_key(normalized)),
            ModalKind::Help | ModalKind::Logs | ModalKind::InspectPager => {
                Some(self.on_pager_key(layer, normalized))
            }
            ModalKind::Settings => None,
        }
    }

    fn on_palette_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Esc => {
                self.overlays.palette = None;
                self.focus.pop();
            }
            Key::Up | Key::Ctrl('p') => {
                let count = self.palette_len();
                self.overlays
                    .palette
                    .get_or_insert_with(CommandPaletteState::new)
                    .select_prev(count);
            }
            Key::Down | Key::Ctrl('n') => {
                let count = self.palette_len();
                self.overlays
                    .palette
                    .get_or_insert_with(CommandPaletteState::new)
                    .select_next(count);
            }
            Key::Backspace | Key::Ctrl('h') => {
                if let Some(palette) = self.overlays.palette.as_mut() {
                    palette.query.pop();
                    palette.selected = 0;
                }
            }
            Key::Enter => {
                let Some(palette) = self.overlays.palette.as_ref() else {
                    return Effect::None;
                };
                let keymap = &self.settings.snapshot().keymap;
                let entries = palette.query_matches(
                    &self.registry,
                    keymap,
                    &self.focus_context_below_modal(),
                );
                let Some(entry) =
                    entries.get(palette.selected.min(entries.len().saturating_sub(1)))
                else {
                    return Effect::None;
                };
                let action = entry.id.to_string();
                self.overlays.palette = None;
                self.focus.pop();
                return self.dispatch_input(ResolvedInput::Command {
                    id: wb_sdk::CommandId(std::borrow::Cow::Owned(action)),
                    count: None,
                    arg: None,
                });
            }
            Key::Char(c) if !c.is_control() => {
                if let Some(palette) = self.overlays.palette.as_mut() {
                    palette.query.push(c);
                    palette.selected = 0;
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn palette_len(&self) -> usize {
        let Some(palette) = self.overlays.palette.as_ref() else {
            return 0;
        };
        let keymap = &self.settings.snapshot().keymap;
        palette
            .query_matches(&self.registry, keymap, &self.focus_context_below_modal())
            .len()
    }

    fn focus_context_below_modal(&self) -> KeyContextId {
        KeyContextId::new_static("global")
    }

    fn on_prompt_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Esc => {
                self.overlays.prompt = None;
                self.focus.pop();
            }
            Key::Enter => {
                let Some(prompt) = self.overlays.prompt.take() else {
                    return Effect::None;
                };
                self.focus.pop();
                return match prompt.kind {
                    PromptKind::Search => {
                        self.search_query = Some(prompt.input.clone());
                        self.jump_search(&prompt.input, true);
                        Effect::None
                    }
                    PromptKind::ExCommand => self.execute_ex_command(prompt.input),
                    PromptKind::SaveAs => {
                        let path = PathBuf::from(prompt.input);
                        self.save_as(path, false);
                        Effect::None
                    }
                };
            }
            Key::Backspace | Key::Ctrl('h') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.backspace();
                }
            }
            Key::Delete | Key::Ctrl('d') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.delete_forward();
                }
            }
            Key::Left | Key::Ctrl('b') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.move_left();
                }
            }
            Key::Right | Key::Ctrl('f') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.move_right();
                }
            }
            Key::Home | Key::Ctrl('a') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.move_home();
                }
            }
            Key::End | Key::Ctrl('e') => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.move_end();
                }
            }
            Key::Char(c) if !c.is_control() => {
                if let Some(prompt) = self.overlays.prompt.as_mut() {
                    prompt.insert_char(c);
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn on_confirm_key(&mut self, key: Key) -> Effect {
        let Some(action) = self.overlays.confirm.clone() else {
            self.focus.pop();
            return Effect::None;
        };
        match action {
            ConfirmAction::QuitUnsaved => match key {
                Key::Char('s') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    if self.save_document(false) {
                        return self.finish_quit();
                    }
                }
                Key::Char('q') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.document.dirty_source = false;
                    self.document.dirty_outputs = false;
                    self.delete_autosave();
                    return self.finish_quit();
                }
                Key::Esc => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                }
                _ => {}
            },
            ConfirmAction::ReloadExternalDiskChange => match key {
                Key::Char('l') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.reload_document_from_disk();
                }
                Key::Esc | Key::Char('k') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                }
                _ => {}
            },
            ConfirmAction::RestartLiveKernel { run_all_after } => match key {
                Key::Char('R') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.restart_kernel(run_all_after);
                }
                Key::Char('a') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.restart_kernel(true);
                }
                Key::Esc => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                }
                _ => {}
            },
            ConfirmAction::RecoverAutosaveSidecar => match key {
                Key::Char('r') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.restore_autosave();
                }
                Key::Char('d') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                    self.delete_autosave();
                }
                Key::Esc | Key::Char('k') => {
                    self.overlays.confirm = None;
                    self.focus.pop();
                }
                _ => {}
            },
        }
        Effect::None
    }

    fn on_pager_key(&mut self, layer: ModalKind, key: Key) -> Effect {
        let half = usize::from(self.last_geometry.viewport_rect.height.max(1)) / 2;
        match key {
            Key::Esc | Key::Char('q') => {
                if matches!(layer, ModalKind::InspectPager) {
                    self.inspect_pager = None;
                }
                self.focus.pop();
            }
            Key::Char('j') | Key::Down => self.scroll_modal(layer, half.max(1) / half.max(1)),
            Key::Char('k') | Key::Up => self.scroll_modal_up(layer, 1),
            Key::Ctrl('d') | Key::PageDown => self.scroll_modal(layer, half.max(1)),
            Key::Ctrl('u') | Key::PageUp => self.scroll_modal_up(layer, half.max(1)),
            Key::Char('g') | Key::Home => self.scroll_modal_top(layer),
            Key::Char('G') | Key::End => self.scroll_modal_bottom(layer),
            _ => {}
        }
        Effect::None
    }

    fn scroll_modal(&mut self, layer: ModalKind, rows: usize) {
        match layer {
            ModalKind::Help => self.help_scroll = self.help_scroll.saturating_add(rows),
            ModalKind::Logs => {
                self.overlays.log_scroll_from_bottom = self
                    .overlays
                    .log_scroll_from_bottom
                    .saturating_add(rows)
                    .min(self.logs.len().saturating_sub(1));
            }
            ModalKind::InspectPager => {
                if let Some(pager) = self.inspect_pager.as_mut() {
                    pager.scroll_down(rows);
                }
            }
            _ => {}
        }
    }

    fn scroll_modal_up(&mut self, layer: ModalKind, rows: usize) {
        match layer {
            ModalKind::Help => self.help_scroll = self.help_scroll.saturating_sub(rows),
            ModalKind::Logs => {
                self.overlays.log_scroll_from_bottom =
                    self.overlays.log_scroll_from_bottom.saturating_sub(rows);
            }
            ModalKind::InspectPager => {
                if let Some(pager) = self.inspect_pager.as_mut() {
                    pager.scroll_up(rows);
                }
            }
            _ => {}
        }
    }

    fn scroll_modal_top(&mut self, layer: ModalKind) {
        match layer {
            ModalKind::Help => self.help_scroll = 0,
            ModalKind::Logs => self.overlays.log_scroll_from_bottom = self.logs.len(),
            ModalKind::InspectPager => {
                if let Some(pager) = self.inspect_pager.as_mut() {
                    pager.scroll_top();
                }
            }
            _ => {}
        }
    }

    fn scroll_modal_bottom(&mut self, layer: ModalKind) {
        match layer {
            ModalKind::Help => self.help_scroll = usize::MAX,
            ModalKind::Logs => self.overlays.log_scroll_from_bottom = 0,
            ModalKind::InspectPager => {
                if let Some(pager) = self.inspect_pager.as_mut() {
                    pager.scroll_bottom();
                }
            }
            _ => {}
        }
    }

    fn on_completion_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let normalized = normalize_crossterm_key(key)?;
        match normalized {
            Key::Esc => {
                self.completion = None;
                Some(Effect::None)
            }
            Key::Up => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_prev(8);
                }
                Some(Effect::None)
            }
            Key::Down => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_next(8);
                }
                Some(Effect::None)
            }
            Key::Enter | Key::Tab => {
                self.accept_completion();
                Some(Effect::None)
            }
            _ => None,
        }
    }

    fn dispatch_pdb(&mut self, input: ResolvedInput) {
        let Some(pdb) = self.overlays.pdb_dock.as_mut() else {
            return;
        };
        match input {
            ResolvedInput::InsertText(text) => pdb.composer.insert_str(&text),
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "pdb.submit" => {
                let (value, history) = pdb.take_submission();
                pdb.history = history;
                self.send_command(KernelCmd::ReplyInput { value });
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "pdb.newline" => {
                pdb.composer.insert_newline();
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "pdb.history_prev" => {
                pdb.history_prev();
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "pdb.history_next" => {
                pdb.history_next();
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "kernel.interrupt" => {
                self.send_command(KernelCmd::Interrupt);
                self.pop_pdb();
            }
            _ => {}
        }
    }

    fn dispatch_notebook(&mut self, input: ResolvedInput) -> Effect {
        match input {
            ResolvedInput::Move { motion, count } => {
                self.move_selected(motion.0.as_ref(), count);
                Effect::None
            }
            ResolvedInput::OperateSelf { op, count } => {
                self.apply_notebook_operator(op.0.as_ref(), None, count);
                Effect::None
            }
            ResolvedInput::Operate { op, target, count } => {
                self.apply_notebook_operator(op.0.as_ref(), Some(target), count);
                Effect::None
            }
            ResolvedInput::Command { id, count, arg } => {
                self.dispatch_notebook_command(id.0.as_ref(), count.unwrap_or(1), arg)
            }
            ResolvedInput::InsertText(_) | ResolvedInput::RollbackChars(_) => Effect::None,
        }
    }

    fn dispatch_notebook_command(&mut self, id: &str, count: u32, arg: Option<char>) -> Effect {
        match id {
            "app.quit" => self.request_quit(),
            "app.palette" => {
                self.overlays.palette = Some(CommandPaletteState::new());
                self.focus
                    .push(FocusLayer::Modal(ModalKind::CommandPalette));
                Effect::None
            }
            "app.settings" => {
                self.focus.push(FocusLayer::Modal(ModalKind::Settings));
                Effect::None
            }
            "app.help" => {
                self.help_scroll = 0;
                self.focus.push(FocusLayer::Modal(ModalKind::Help));
                Effect::None
            }
            "app.command_bar" => {
                self.overlays.prompt = Some(PromptBar::new(PromptKind::ExCommand, ""));
                self.focus.push(FocusLayer::Modal(ModalKind::ExCommandBar));
                Effect::None
            }
            "search.open" => {
                self.overlays.prompt = Some(PromptBar::new(
                    PromptKind::Search,
                    self.search_query.clone().unwrap_or_default(),
                ));
                self.focus.push(FocusLayer::Modal(ModalKind::SearchPrompt));
                Effect::None
            }
            "search.next" => {
                if let Some(query) = self.search_query.clone() {
                    self.jump_search(&query, true);
                }
                Effect::None
            }
            "search.prev" => {
                if let Some(query) = self.search_query.clone() {
                    self.jump_search(&query, false);
                }
                Effect::None
            }
            "cell.enter_normal" => {
                self.begin_cell_edit(CellMode::Normal);
                Effect::None
            }
            "cell.enter_insert" => {
                self.begin_cell_edit(CellMode::Insert);
                Effect::None
            }
            "cell.append_end" => {
                self.begin_cell_edit(CellMode::Insert);
                if let Some(cell) = self.document.cell_mut(&self.selected_cell) {
                    let row = cell.buffer.line_count().saturating_sub(1);
                    let col = cell.buffer.line_char_len(row);
                    cell.buffer.set_cursor(CursorPos { row, col });
                }
                self.term_effects.set_cursor_shape(CursorShape::SteadyBar);
                Effect::None
            }
            "cell.insert_below" => {
                self.insert_cell_at(self.selected_index().saturating_add(1));
                Effect::None
            }
            "cell.insert_above" => {
                self.insert_cell_at(self.selected_index());
                Effect::None
            }
            "cell.run" => {
                self.run_selected();
                Effect::None
            }
            "cell.run_and_advance" => {
                self.run_selected();
                self.move_selected("motion.down", 1);
                Effect::None
            }
            "cell.run_all" => {
                self.run_all();
                Effect::None
            }
            "cell.toggle_kind" => {
                self.toggle_kind();
                Effect::None
            }
            "cell.toggle_collapse" => {
                if let Some(cell) = self.document.cell_mut(&self.selected_cell) {
                    cell.collapsed = !cell.collapsed;
                    cell.bump_chrome();
                    self.document.dirty_source = true;
                }
                Effect::None
            }
            "cell.focus_output" => {
                self.focus_first_output();
                Effect::None
            }
            "cell.edit_external" => Effect::OpenExternalEditor(self.selected_cell.clone()),
            "cell.clear_outputs" => {
                let id = self.selected_cell.clone();
                self.clear_outputs(&id);
                Effect::None
            }
            "cell.format" => {
                self.format_selected();
                Effect::None
            }
            "doc.save" => {
                self.save_document(false);
                Effect::None
            }
            "doc.save_force" => {
                self.save_document(true);
                Effect::None
            }
            "edit.paste_after" => {
                self.paste_cells(true);
                Effect::None
            }
            "edit.paste_before" => {
                self.paste_cells(false);
                Effect::None
            }
            "edit.undo" => {
                if self.document.undo() {
                    self.reconcile_selection();
                    self.remount_output_views();
                }
                Effect::None
            }
            "edit.redo" => {
                if self.document.redo() {
                    self.reconcile_selection();
                    self.remount_output_views();
                }
                Effect::None
            }
            "kernel.interrupt" => {
                self.send_command(KernelCmd::Interrupt);
                Effect::None
            }
            "kernel.restart" => {
                if self.kernel_busy {
                    self.overlays.confirm = Some(ConfirmAction::RestartLiveKernel {
                        run_all_after: false,
                    });
                    self.focus.push(FocusLayer::Modal(ModalKind::ConfirmBar));
                } else {
                    self.restart_kernel(false);
                }
                Effect::None
            }
            "ui.toggle_mouse_capture" => {
                let enabled = !self.settings.snapshot().core.mouse_capture;
                let id = wb_sdk::SettingId(std::borrow::Cow::Borrowed("editor.mouse_capture"));
                self.settings
                    .set_value(&id, wb_sdk::SettingValue::Bool(enabled), &self.registry);
                self.sync_settings();
                Effect::None
            }
            "mark.set" => {
                if let Some(mark) = arg {
                    let row = self
                        .document
                        .cell(&self.selected_cell)
                        .map_or(0, |cell| cell.buffer.cursor().row);
                    self.marks.insert(mark, (self.selected_cell.clone(), row));
                }
                Effect::None
            }
            "mark.jump" => {
                if let Some(mark) = arg {
                    if let Some((cell_id, row)) = self.marks.get(&mark).cloned() {
                        self.select_cell(cell_id.clone());
                        if let Some(cell) = self.document.cell_mut(&cell_id) {
                            cell.buffer.set_cursor(CursorPos { row, col: 0 });
                        }
                    }
                }
                Effect::None
            }
            _ => {
                let _ = count;
                Effect::None
            }
        }
    }

    fn dispatch_cell_edit(
        &mut self,
        cell_id: &CellId,
        mode: CellMode,
        input: ResolvedInput,
    ) -> Effect {
        match mode {
            CellMode::Insert => self.dispatch_cell_insert(cell_id, input),
            CellMode::Normal | CellMode::Visual(_) => {
                self.dispatch_cell_normal(cell_id, mode, input)
            }
        }
    }

    fn dispatch_cell_insert(&mut self, cell_id: &CellId, input: ResolvedInput) -> Effect {
        match input {
            ResolvedInput::InsertText(text) => {
                self.insert_cell_text(cell_id, &text);
                self.after_cell_insert(cell_id);
            }
            ResolvedInput::RollbackChars(count) => {
                self.mutate_cell_buffer(cell_id, |buffer| buffer.rollback_chars(count));
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "edit.newline" => {
                self.mutate_cell_buffer(cell_id, |buffer| buffer.insert_newline());
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "intel.complete" => {
                self.request_completion(cell_id);
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "focus.up" => {
                self.set_cell_mode(CellMode::Normal);
            }
            ResolvedInput::Command { id, .. } if id.0.as_ref() == "cell.toggle_expand_split" => {
                self.toggle_split();
            }
            _ => {}
        }
        Effect::None
    }

    fn dispatch_cell_normal(
        &mut self,
        cell_id: &CellId,
        mode: CellMode,
        input: ResolvedInput,
    ) -> Effect {
        match input {
            ResolvedInput::Move { motion, count } => {
                self.move_cell_cursor(cell_id, motion.0.as_ref(), count);
                if let CellMode::Visual(_) = mode {
                    self.update_visual_head(cell_id);
                }
            }
            ResolvedInput::OperateSelf { op, count } => {
                self.apply_cell_operator(cell_id, mode, op.0.as_ref(), None, count);
            }
            ResolvedInput::Operate { op, target, count } => {
                self.apply_cell_operator(cell_id, mode, op.0.as_ref(), Some(target), count);
            }
            ResolvedInput::Command { id, count, arg } => {
                self.dispatch_cell_command(cell_id, mode, id.0.as_ref(), count.unwrap_or(1), arg);
            }
            ResolvedInput::InsertText(_) | ResolvedInput::RollbackChars(_) => {}
        }
        Effect::None
    }

    fn dispatch_cell_command(
        &mut self,
        cell_id: &CellId,
        mode: CellMode,
        id: &str,
        _count: u32,
        arg: Option<char>,
    ) {
        match id {
            "cell.enter_insert" | "cell.append_char" => self.set_cell_mode(CellMode::Insert),
            "cell.append_end" => {
                if let Some(cell) = self.document.cell_mut(cell_id) {
                    let row = cell.buffer.line_count().saturating_sub(1);
                    let col = cell.buffer.line_char_len(row);
                    cell.buffer.set_cursor(CursorPos { row, col });
                }
                self.set_cell_mode(CellMode::Insert);
            }
            "cell.open_line_below" => {
                if let Some(cell) = self.document.cell_mut(cell_id) {
                    let row = cell.buffer.cursor().row;
                    let col = cell.buffer.line_char_len(row);
                    cell.buffer.set_cursor(CursorPos { row, col });
                    cell.buffer.checkpoint();
                    cell.buffer.insert_newline();
                    cell.bump_source();
                    self.document.dirty_source = true;
                }
                self.set_cell_mode(CellMode::Insert);
            }
            "cell.open_line_above" => {
                if let Some(cell) = self.document.cell_mut(cell_id) {
                    let row = cell.buffer.cursor().row;
                    cell.buffer.set_cursor(CursorPos { row, col: 0 });
                    cell.buffer.checkpoint();
                    cell.buffer.insert_newline();
                    cell.buffer.set_cursor(CursorPos { row, col: 0 });
                    cell.bump_source();
                    self.document.dirty_source = true;
                }
                self.set_cell_mode(CellMode::Insert);
            }
            "cell.run" => {
                self.commit_cell(cell_id);
                self.run_cell(cell_id);
                self.cell_baselines
                    .insert(cell_id.clone(), self.cell_source(cell_id));
            }
            "focus.up" => {
                if matches!(mode, CellMode::Visual(_)) {
                    if let Some(cell) = self.document.cell_mut(cell_id) {
                        cell.buffer.clear_selection();
                    }
                    self.set_cell_mode(CellMode::Normal);
                } else {
                    self.commit_cell(cell_id);
                    self.focus.pop();
                    self.term_effects.set_cursor_shape(CursorShape::SteadyBlock);
                }
            }
            "cell.toggle_expand_split" => self.toggle_split(),
            "cell.split_at_cursor" => self.split_cell(cell_id),
            "intel.inspect_doc" => self.request_inspect(cell_id, false),
            "edit.delete_char" => {
                self.mutate_cell_buffer(cell_id, |buffer| buffer.delete_char_forward());
            }
            "edit.replace_char" => {
                if let Some(ch) = arg {
                    self.mutate_cell_buffer(cell_id, |buffer| buffer.replace_char(ch));
                }
            }
            "edit.paste_after" => self.paste_text_in_cell(cell_id, true),
            "edit.paste_before" => self.paste_text_in_cell(cell_id, false),
            "edit.undo" => self.mutate_cell_buffer(cell_id, |buffer| {
                let _ = buffer.undo();
            }),
            "edit.redo" => self.mutate_cell_buffer(cell_id, |buffer| {
                let _ = buffer.redo();
            }),
            "visual.char" => self.enter_visual(cell_id, VisualKind::Char),
            "visual.line" => self.enter_visual(cell_id, VisualKind::Line),
            _ => {}
        }
    }

    fn dispatch_output(
        &mut self,
        cell_id: &CellId,
        output_id: OutputId,
        input: ResolvedInput,
    ) -> Effect {
        let Some(item) = self
            .document
            .cell(cell_id)
            .and_then(|cell| cell.outputs.iter().find(|item| item.id == output_id))
            .cloned()
        else {
            self.focus.pop();
            return Effect::None;
        };
        let mut jumped = None;
        let outcome = if let Some(view) = self.output_views.get_mut(&output_id) {
            let mut host = CoordinatorOutputHost {
                kernel: &mut *self.kernel,
                clipboard: &mut *self.clipboard,
                selected_cell: &mut self.selected_cell,
                notice: &mut self.notice,
                jumped: &mut jumped,
            };
            view.handle_input(&item, input.clone(), &mut host)
        } else {
            InputOutcome::Ignored
        };
        if let Some((jump_cell, line)) = jumped {
            self.select_cell(jump_cell.clone());
            if let Some(cell) = self.document.cell_mut(&jump_cell) {
                cell.buffer.set_cursor(CursorPos {
                    row: line.unwrap_or(0),
                    col: 0,
                });
            }
        }
        if matches!(outcome, InputOutcome::PopFocus)
            || matches!(input, ResolvedInput::Command { ref id, .. } if id.0.as_ref() == "focus.up")
        {
            self.focus.pop();
        }
        Effect::None
    }

    fn move_selected(&mut self, motion: &str, count: u32) {
        let len = self.document.order().len();
        if len == 0 {
            return;
        }
        let current = self.selected_index();
        let next = match motion {
            "motion.down" => current.saturating_add(count as usize).min(len - 1),
            "motion.up" => current.saturating_sub(count as usize),
            "motion.top" => 0,
            "motion.bottom" => len - 1,
            "motion.half_page_down" => current
                .saturating_add(
                    (count as usize)
                        .saturating_mul(self.last_geometry.viewport_rect.height as usize / 2)
                        .max(1),
                )
                .min(len - 1),
            "motion.half_page_up" => current.saturating_sub(
                (count as usize)
                    .saturating_mul(self.last_geometry.viewport_rect.height as usize / 2)
                    .max(1),
            ),
            _ => current,
        };
        if let Some(cell_id) = self.document.order().get(next).cloned() {
            self.select_cell(cell_id);
        }
    }

    fn move_cell_cursor(&mut self, cell_id: &CellId, motion: &str, count: u32) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        let current = cell.buffer.cursor();
        let target = match motion {
            "motion.down" => CursorPos {
                row: current
                    .row
                    .saturating_add(count as usize)
                    .min(cell.buffer.line_count().saturating_sub(1)),
                col: current.col,
            },
            "motion.up" => CursorPos {
                row: current.row.saturating_sub(count as usize),
                col: current.col,
            },
            "motion.top" => CursorPos { row: 0, col: 0 },
            "motion.bottom" => CursorPos {
                row: cell.buffer.line_count().saturating_sub(1),
                col: 0,
            },
            "motion.half_page_down" => CursorPos {
                row: current
                    .row
                    .saturating_add((count as usize).saturating_mul(10))
                    .min(cell.buffer.line_count().saturating_sub(1)),
                col: current.col,
            },
            "motion.half_page_up" => CursorPos {
                row: current
                    .row
                    .saturating_sub((count as usize).saturating_mul(10)),
                col: current.col,
            },
            "motion.left" => CursorPos {
                row: current.row,
                col: current.col.saturating_sub(count as usize),
            },
            "motion.right" => CursorPos {
                row: current.row,
                col: current.col.saturating_add(count as usize),
            },
            "motion.word_next" => cell.buffer.word_next(count),
            "motion.word_back" => cell.buffer.word_back(count),
            "motion.word_end" => cell.buffer.word_end(count),
            "motion.line_start" => CursorPos {
                row: current.row,
                col: 0,
            },
            "motion.line_first_nonblank" => CursorPos {
                row: current.row,
                col: cell.buffer.first_nonblank_col(current.row),
            },
            "motion.line_end" => CursorPos {
                row: current.row,
                col: cell.buffer.line_char_len(current.row),
            },
            _ => current,
        };
        cell.buffer.set_cursor(target);
    }

    fn apply_notebook_operator(&mut self, op: &str, target: Option<MotionOrObject>, count: u32) {
        let start = self.selected_index();
        let last = self.document.order().len().saturating_sub(1);
        let moving_up = matches!(
            target.as_ref(),
            Some(MotionOrObject::Motion(motion)) if motion.0.as_ref() == "motion.up"
        );
        let end = match target {
            None => start
                .saturating_add(count.saturating_sub(1) as usize)
                .min(last),
            Some(MotionOrObject::Motion(motion)) if motion.0.as_ref() == "motion.down" => {
                start.saturating_add(count as usize).min(last)
            }
            Some(MotionOrObject::Motion(motion)) if motion.0.as_ref() == "motion.up" => start,
            Some(MotionOrObject::Motion(motion)) if motion.0.as_ref() == "motion.bottom" => last,
            _ => start,
        };
        let (range_start, range_end) = if moving_up {
            (start.saturating_sub(count as usize), start)
        } else {
            (start.min(end), start.max(end))
        };
        let ids = self.document.order()[range_start..=range_end].to_vec();
        match op {
            "op.yank" => {
                self.yank_cells = ids
                    .iter()
                    .filter_map(|id| self.document.cell(id).cloned())
                    .collect();
                self.yank_text = self
                    .yank_cells
                    .iter()
                    .map(|cell| cell.buffer.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = self.clipboard.copy(&self.yank_text);
            }
            "op.delete" => {
                let removed = ids
                    .iter()
                    .filter_map(|id| self.document.cell(id).cloned())
                    .collect::<Vec<_>>();
                self.document.apply(DocOp::DeleteCells {
                    at_index: range_start,
                    removed,
                });
                if self.document.order().is_empty() {
                    self.insert_cell_at(0);
                } else {
                    let index = range_start.min(self.document.order().len() - 1);
                    self.select_cell(self.document.order()[index].clone());
                }
                self.remount_output_views();
            }
            _ => {}
        }
    }

    fn apply_cell_operator(
        &mut self,
        cell_id: &CellId,
        mode: CellMode,
        op: &str,
        target: Option<MotionOrObject>,
        count: u32,
    ) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        if matches!(mode, CellMode::Visual(_)) && target.is_none() {
            let Some(selection) = cell.buffer.selection() else {
                return;
            };
            let text = match op {
                "op.yank" => cell.buffer.yank_span(selection.anchor, selection.head),
                "op.delete" | "op.change" => {
                    cell.buffer.checkpoint();
                    let text = cell.buffer.delete_span(selection.anchor, selection.head);
                    cell.bump_source();
                    text
                }
                _ => String::new(),
            };
            if op == "op.yank" {
                self.yank_text = text.clone();
                let _ = self.clipboard.copy(&text);
            }
            if op == "op.change" {
                cell.buffer.clear_selection();
                self.set_cell_mode(CellMode::Insert);
            } else if op == "op.delete" {
                cell.buffer.clear_selection();
                self.set_cell_mode(CellMode::Normal);
            }
            self.document.dirty_source = true;
            return;
        }
        let cursor = cell.buffer.cursor();
        match target {
            Some(MotionOrObject::TextObject(object)) => {
                let (a, b) = if object.0.as_ref() == "textobj.around_word" {
                    cell.buffer.around_word_span()
                } else {
                    cell.buffer.inner_word_span()
                };
                self.apply_text_span(cell_id, op, a, b);
            }
            Some(MotionOrObject::Motion(motion)) => {
                if matches!(
                    motion.0.as_ref(),
                    "motion.word_next" | "motion.word_back" | "motion.word_end"
                ) {
                    let target_pos = match motion.0.as_ref() {
                        "motion.word_next" => cell.buffer.word_next(count),
                        "motion.word_back" => cell.buffer.word_back(count),
                        _ => cell.buffer.word_end(count),
                    };
                    self.apply_text_span(cell_id, op, cursor, target_pos);
                } else {
                    let last = cell.buffer.line_count().saturating_sub(1);
                    let target_row = match motion.0.as_ref() {
                        "motion.down" => cursor.row.saturating_add(count as usize).min(last),
                        "motion.up" => cursor.row.saturating_sub(count as usize),
                        "motion.bottom" => last,
                        _ => cursor.row,
                    };
                    let start = cursor.row.min(target_row);
                    let end = cursor.row.max(target_row).saturating_add(1);
                    self.apply_line_span(cell_id, op, start..end);
                }
            }
            None => {
                let end = cursor
                    .row
                    .saturating_add(count.saturating_sub(1) as usize)
                    .saturating_add(1);
                self.apply_line_span(cell_id, op, cursor.row..end);
            }
        }
    }

    fn apply_line_span(&mut self, cell_id: &CellId, op: &str, rows: std::ops::Range<usize>) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        let rows = rows.start..rows.end.min(cell.buffer.line_count());
        let text = match op {
            "op.yank" => cell.buffer.yank_line_range(rows),
            "op.delete" | "op.change" => {
                cell.buffer.checkpoint();
                cell.buffer.delete_line_range(rows)
            }
            _ => String::new(),
        };
        if op == "op.yank" {
            self.yank_text = text.clone();
            let _ = self.clipboard.copy(&text);
        } else if matches!(op, "op.delete" | "op.change") {
            cell.bump_source();
            self.document.dirty_source = true;
            if op == "op.change" {
                self.set_cell_mode(CellMode::Insert);
            }
        }
    }

    fn apply_text_span(&mut self, cell_id: &CellId, op: &str, a: CursorPos, b: CursorPos) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        let text = match op {
            "op.yank" => cell.buffer.yank_span(a, b),
            "op.delete" | "op.change" => {
                cell.buffer.checkpoint();
                cell.buffer.delete_span(a, b)
            }
            _ => String::new(),
        };
        if op == "op.yank" {
            self.yank_text = text.clone();
            let _ = self.clipboard.copy(&text);
        } else if matches!(op, "op.delete" | "op.change") {
            cell.bump_source();
            self.document.dirty_source = true;
            if op == "op.change" {
                self.set_cell_mode(CellMode::Insert);
            }
        }
    }

    fn begin_cell_edit(&mut self, mode: CellMode) {
        let id = self.selected_cell.clone();
        self.cell_baselines
            .insert(id.clone(), self.cell_source(&id));
        self.focus.push(FocusLayer::CellEdit {
            cell_id: id,
            mode,
            expanded_split: false,
        });
        self.term_effects
            .set_cursor_shape(if matches!(mode, CellMode::Insert) {
                CursorShape::SteadyBar
            } else {
                CursorShape::SteadyBlock
            });
    }

    fn set_cell_mode(&mut self, mode: CellMode) {
        if let FocusLayer::CellEdit { mode: current, .. } = self.focus.top_mut() {
            *current = mode;
            self.term_effects
                .set_cursor_shape(if matches!(mode, CellMode::Insert) {
                    CursorShape::SteadyBar
                } else {
                    CursorShape::SteadyBlock
                });
        }
    }

    fn toggle_split(&mut self) {
        if let FocusLayer::CellEdit { expanded_split, .. } = self.focus.top_mut() {
            *expanded_split = !*expanded_split;
        }
    }

    fn enter_visual(&mut self, cell_id: &CellId, kind: VisualKind) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        let cursor = cell.buffer.cursor();
        cell.buffer.set_selection(Selection {
            kind,
            anchor: cursor,
            head: cursor,
        });
        self.set_cell_mode(CellMode::Visual(kind));
    }

    fn update_visual_head(&mut self, cell_id: &CellId) {
        let Some(cell) = self.document.cell_mut(cell_id) else {
            return;
        };
        let Some(mut selection) = cell.buffer.selection() else {
            return;
        };
        selection.head = cell.buffer.cursor();
        cell.buffer.set_selection(selection);
    }

    fn insert_cell_text(&mut self, cell_id: &CellId, text: &str) {
        self.mutate_cell_buffer(cell_id, |buffer| {
            buffer.checkpoint();
            buffer.insert_str(text);
        });
    }

    fn mutate_cell_buffer<F>(&mut self, cell_id: &CellId, mutator: F)
    where
        F: FnOnce(&mut wb_core::TextBuffer),
    {
        if let Some(cell) = self.document.cell_mut(cell_id) {
            mutator(&mut cell.buffer);
            cell.bump_source();
            self.document.dirty_source = true;
        }
    }

    fn after_cell_insert(&mut self, cell_id: &CellId) {
        let source = self.cell_source(cell_id);
        if let Some(completion) = self.completion.as_mut() {
            let cursor = self
                .document
                .cell(cell_id)
                .map_or(0, |cell| cell.buffer.char_offset_of(cell.buffer.cursor()));
            if !completion.update_filter(&source, cursor) {
                self.completion = None;
            }
        }
        if self.settings.snapshot().core.complete_on_dot && source.ends_with('.') {
            self.request_completion(cell_id);
        }
        if self.settings.snapshot().core.signature_help {
            let cursor = self
                .document
                .cell(cell_id)
                .map_or(0, |cell| cell.buffer.char_offset_of(cell.buffer.cursor()));
            if SignaturePopup::parse_enclosing_call(&source, cursor).is_some() {
                self.request_inspect(cell_id, true);
            } else {
                self.signature = None;
            }
        }
    }

    fn accept_completion(&mut self) {
        let Some(popup) = self.completion.take() else {
            return;
        };
        let Some(candidate) = popup.selected_candidate().cloned() else {
            return;
        };
        let id = match self.focus.top() {
            FocusLayer::CellEdit { cell_id, .. } => cell_id.clone(),
            _ => return,
        };
        if let Some(cell) = self.document.cell_mut(&id) {
            let start = cursor_at_char_offset(&cell.buffer, popup.replace_start_char);
            let end = cursor_at_char_offset(&cell.buffer, popup.replace_end_char);
            cell.buffer.checkpoint();
            cell.buffer.delete_span(start, end);
            cell.buffer.insert_str(&candidate.replace_text);
            cell.bump_source();
            self.document.dirty_source = true;
        }
    }

    fn request_completion(&mut self, cell_id: &CellId) {
        let Some(cell) = self.document.cell(cell_id) else {
            return;
        };
        let cursor = cell.buffer.cursor();
        let source = cell.buffer.as_str().to_string();
        let cursor_char = cell.buffer.char_offset_of(cursor);
        let chars = source.chars().collect::<Vec<_>>();
        let mut start = cursor_char.min(chars.len());
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }
        let typed_prefix = chars[start..cursor_char.min(chars.len())]
            .iter()
            .collect::<String>();
        let req_id = self.take_request_id();
        self.pending_completions.insert(
            req_id,
            CompletionRequest {
                cell_id: cell_id.clone(),
                replace_start_char: start,
                replace_end_char: cursor_char,
                typed_prefix,
            },
        );
        self.send_command(KernelCmd::Complete {
            req_id,
            code: source,
            cursor_char_pos: cursor_char,
        });
    }

    fn request_inspect(&mut self, cell_id: &CellId, signature: bool) {
        let Some(cell) = self.document.cell(cell_id) else {
            return;
        };
        let source = cell.buffer.as_str().to_string();
        let cursor = cell.buffer.char_offset_of(cell.buffer.cursor());
        let req_id = self.take_request_id();
        if signature {
            let site = SignaturePopup::parse_enclosing_call(&source, cursor);
            if let Some(site) = site {
                self.pending_inspects.insert(
                    req_id,
                    PendingInspect::Signature {
                        active_arg: site.active_arg,
                        keyword: site.kwarg_name,
                    },
                );
                self.send_command(KernelCmd::Inspect {
                    req_id,
                    code: source,
                    cursor_char_pos: site.callee_end_char,
                    detail_level: 0,
                });
            }
        } else {
            self.pending_inspects.insert(
                req_id,
                PendingInspect::Pager {
                    title: "Inspect".into(),
                },
            );
            self.send_command(KernelCmd::Inspect {
                req_id,
                code: source,
                cursor_char_pos: cursor,
                detail_level: 1,
            });
        }
    }

    fn apply_completion_reply(
        &mut self,
        req_id: u64,
        start_char: usize,
        mut items: Vec<CompletionCandidate>,
    ) {
        let Some(request) = self.pending_completions.remove(&req_id) else {
            return;
        };
        // The reply is only useful if its cell is still being typed in.
        let still_typing = matches!(
            self.focus.top(),
            FocusLayer::CellEdit {
                cell_id,
                mode: CellMode::Insert,
                ..
            } if cell_id == &request.cell_id
        );
        if !still_typing {
            return;
        }
        let from_kernel = !items.is_empty();
        if items.is_empty() {
            let sources = self
                .document
                .cells_in_order()
                .map(|cell| cell.buffer.as_str())
                .collect::<Vec<_>>();
            items = scan_notebook_identifiers(&sources, 1)
                .into_iter()
                .map(|replace_text| CompletionCandidate {
                    replace_text,
                    kind: "notebook".into(),
                    detail: String::new(),
                })
                .collect();
        }
        // The kernel owns the replacement range: IPython may start it before
        // our identifier scan would (e.g. `jnp.li` -> start at `.`, match
        // `.linalg`). Fall back to our own word start if the reply is bogus.
        let (start, typed_prefix) = if from_kernel && start_char <= request.replace_end_char {
            let typed = self
                .document
                .cell(&request.cell_id)
                .map(|cell| {
                    cell.buffer
                        .as_str()
                        .chars()
                        .skip(start_char)
                        .take(request.replace_end_char - start_char)
                        .collect::<String>()
                })
                .unwrap_or_default();
            (start_char, typed)
        } else {
            (request.replace_start_char, request.typed_prefix)
        };
        self.completion = Some(CompletionPopup::new(
            req_id,
            start,
            request.replace_end_char,
            items,
            typed_prefix,
        ))
        .filter(|popup| !popup.filtered.is_empty());
    }

    fn apply_inspect_reply(&mut self, req_id: u64, doc: Option<String>) {
        let Some(pending) = self.pending_inspects.remove(&req_id) else {
            return;
        };
        let Some(doc) = doc else {
            return;
        };
        match pending {
            PendingInspect::Signature {
                active_arg,
                keyword,
                ..
            } => {
                self.signature =
                    SignaturePopup::from_inspect_doc(&doc, active_arg, keyword.as_deref());
            }
            PendingInspect::Pager { title } => {
                self.signature = None;
                self.inspect_pager = Some(InspectPager::new(title, doc));
                self.focus.push(FocusLayer::Modal(ModalKind::InspectPager));
            }
        }
    }

    fn apply_output(&mut self, target: TargetId, late: bool, payload: WireOutput) {
        let cell_id = match target.clone() {
            TargetId::Cell(cell_id) => cell_id,
            TargetId::SessionLog | TargetId::ConsoleEntry(_) | TargetId::Silent(_) => {
                let item = wire_output_item(payload, late);
                self.document.append_session_log(item);
                return;
            }
        };
        if self.pending_clear_wait.remove(&cell_id) {
            self.clear_outputs(&cell_id);
        }
        match payload {
            WireOutput::Stream { stream, text } => {
                self.document.append_stream(&cell_id, stream, &text, late);
            }
            WireOutput::Rich {
                bundle,
                metadata,
                display_id,
                is_execute_result,
                execution_count,
            } => {
                self.document.push_output(
                    &cell_id,
                    OutputItem::new(
                        OutputData::Rich {
                            bundle,
                            metadata,
                            display_id,
                            is_execute_result,
                            execution_count,
                        },
                        late,
                    ),
                );
            }
            WireOutput::Error {
                ename,
                evalue,
                traceback,
            } => self.document.push_output(
                &cell_id,
                OutputItem::new(
                    OutputData::Error {
                        ename,
                        evalue,
                        traceback,
                    },
                    late,
                ),
            ),
        }
        self.update_views_for_cell(&cell_id);
    }

    fn remount_output_views(&mut self) {
        self.output_views.clear();
        let outputs = self
            .document
            .cells_in_order()
            .flat_map(|cell| {
                cell.outputs
                    .iter()
                    .cloned()
                    .map(move |item| (cell.id.clone(), item))
            })
            .collect::<Vec<_>>();
        for (cell_id, item) in outputs {
            self.mount_output(&cell_id, &item);
        }
    }

    fn update_views_for_cell(&mut self, cell_id: &CellId) {
        let outputs = self
            .document
            .cell(cell_id)
            .map(|cell| cell.outputs.clone())
            .unwrap_or_default();
        for item in &outputs {
            if self.output_views.contains_key(&item.id) {
                let ctx = MountCtx {
                    cell_id: cell_id.clone(),
                    output_id: item.id,
                };
                if let Some(view) = self.output_views.get_mut(&item.id) {
                    view.on_output_updated(item, &ctx);
                }
            } else {
                self.mount_output(cell_id, item);
            }
        }
    }

    fn mount_output(&mut self, cell_id: &CellId, item: &OutputItem) {
        let Some((_, renderer)) = self
            .registry
            .output_renderers
            .iter()
            .enumerate()
            .filter_map(|(index, renderer)| {
                renderer.can_render(item).map(|priority| (priority, index))
            })
            .max_by_key(|(priority, index)| (*priority, *index))
        else {
            return;
        };
        let ctx = MountCtx {
            cell_id: cell_id.clone(),
            output_id: item.id,
        };
        let view = self.registry.output_renderers[renderer].mount(item, &ctx);
        self.output_views.insert(item.id, view);
    }

    fn forward_comm(&mut self, comm_id: &CommId, data: &Map<String, Value>) {
        let mut jumped = None;
        for view in self.output_views.values_mut() {
            let mut host = CoordinatorOutputHost {
                kernel: &mut *self.kernel,
                clipboard: &mut *self.clipboard,
                selected_cell: &mut self.selected_cell,
                notice: &mut self.notice,
                jumped: &mut jumped,
            };
            view.on_comm_message(comm_id, data, &mut host);
        }
        if let Some((cell_id, line)) = jumped {
            self.select_cell(cell_id.clone());
            if let Some(cell) = self.document.cell_mut(&cell_id) {
                cell.buffer.set_cursor(CursorPos {
                    row: line.unwrap_or(0),
                    col: 0,
                });
            }
        }
    }

    fn clear_outputs(&mut self, cell_id: &CellId) {
        let cleared = self.document.clear_cell_outputs(cell_id);
        for item in cleared {
            self.output_views.remove(&item.id);
        }
    }

    fn insert_cell_at(&mut self, index: usize) {
        let cell = Cell::new_code();
        let id = cell.id.clone();
        self.document.apply(DocOp::InsertCells {
            at_index: index,
            cells: vec![cell],
        });
        self.selected_cell = id;
        self.remount_output_views();
    }

    fn paste_cells(&mut self, after: bool) {
        if self.yank_cells.is_empty() {
            return;
        }
        let index = self.selected_index().saturating_add(usize::from(after));
        let mut cells = Vec::with_capacity(self.yank_cells.len());
        for source in &self.yank_cells {
            let mut cell = source.clone();
            cell.id = CellId::new_random();
            cell.outputs.clear();
            cell.execution_count = None;
            cell.last_elapsed = None;
            cells.push(cell);
        }
        let selected = cells.first().map(|cell| cell.id.clone());
        self.document.apply(DocOp::InsertCells {
            at_index: index,
            cells,
        });
        if let Some(selected) = selected {
            self.selected_cell = selected;
        }
        self.remount_output_views();
    }

    fn paste_text_in_cell(&mut self, cell_id: &CellId, after: bool) {
        if self.yank_text.is_empty() {
            return;
        }
        if let Some(cell) = self.document.cell_mut(cell_id) {
            let cursor = cell.buffer.cursor();
            let mut offset = cell.buffer.char_offset_of(cursor);
            if after {
                offset = offset.saturating_add(1);
            }
            let pos = cursor_at_char_offset(&cell.buffer, offset);
            cell.buffer.set_cursor(pos);
            cell.buffer.checkpoint();
            cell.buffer.insert_str(&self.yank_text.clone());
            cell.bump_source();
            self.document.dirty_source = true;
        }
    }

    fn toggle_kind(&mut self) {
        let id = self.selected_cell.clone();
        let Some(cell) = self.document.cell(&id).cloned() else {
            return;
        };
        let new_kind = match cell.kind {
            CellKind::Code => CellKind::Markdown,
            CellKind::Markdown => CellKind::Raw,
            CellKind::Raw => CellKind::Code,
        };
        self.document.apply(DocOp::SetCellKind {
            cell_id: id,
            old_kind: cell.kind,
            new_kind,
            cleared_outputs: cell.outputs,
        });
        self.remount_output_views();
    }

    fn focus_first_output(&mut self) {
        let id = self.selected_cell.clone();
        let Some(cell) = self.document.cell(&id) else {
            return;
        };
        for item in &cell.outputs {
            if let Some(view) = self.output_views.get(&item.id) {
                if view.is_focusable() {
                    if let Some(context) = view.key_context() {
                        self.focus.push(FocusLayer::OutputFocus {
                            cell_id: id,
                            output_id: item.id,
                            context,
                        });
                    }
                    break;
                }
            }
        }
    }

    fn split_cell(&mut self, cell_id: &CellId) {
        let Some(original) = self.document.cell(cell_id).cloned() else {
            return;
        };
        let index = self.document.position_of(cell_id).unwrap_or(0);
        let left_id = CellId::new_random();
        let right_id = CellId::new_random();
        self.document.apply(DocOp::SplitCell {
            original,
            at_index: index,
            left_id,
            right_id: right_id.clone(),
        });
        self.focus.pop();
        self.selected_cell = right_id;
        self.remount_output_views();
    }

    fn format_selected(&mut self) {
        let id = self.selected_cell.clone();
        let source = self.cell_source(&id);
        let formatter = self.settings.snapshot().core.formatter.clone();
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(formatter)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                self.notice = Some(format!("formatter failed to start: {error}"));
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(source.as_bytes());
        }
        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                let formatted = String::from_utf8_lossy(&output.stdout).to_string();
                if formatted != source {
                    self.document.apply(DocOp::CommitCellSource {
                        cell_id: id,
                        old_source: source,
                        new_source: formatted,
                    });
                }
            }
            Ok(output) => {
                self.notice = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(error) => self.notice = Some(format!("formatter failed: {error}")),
        }
    }

    fn run_selected(&mut self) {
        self.run_cell(&self.selected_cell.clone());
    }

    fn run_all(&mut self) {
        let ids = self
            .document
            .cells_in_order()
            .filter(|cell| matches!(cell.kind, CellKind::Code))
            .map(|cell| cell.id.clone())
            .collect::<Vec<_>>();
        if self.kernel_alive {
            for id in ids {
                self.send_cell_execute(&id);
            }
        } else {
            self.run_all_queued = true;
            self.queued_runs = ids.into_iter().collect();
            self.notice = Some("queued all cells until kernel is ready".into());
        }
    }

    fn run_cell(&mut self, cell_id: &CellId) {
        let Some(cell) = self.document.cell(cell_id) else {
            return;
        };
        if !matches!(cell.kind, CellKind::Code) {
            return;
        }
        if self.kernel_alive {
            self.send_cell_execute(cell_id);
        } else if !self.queued_runs.contains(cell_id) {
            self.queued_runs.push_back(cell_id.clone());
            self.run_states.insert(cell_id.clone(), RunState::Queued);
            self.notice = Some("queued cell until kernel is ready".into());
        }
    }

    fn send_cell_execute(&mut self, cell_id: &CellId) {
        let Some(cell) = self.document.cell(cell_id) else {
            return;
        };
        let code = cell.buffer.as_str().to_string();
        self.run_states.insert(cell_id.clone(), RunState::Queued);
        self.send_command(KernelCmd::Execute {
            target: TargetId::Cell(cell_id.clone()),
            lane: Lane::Main,
            code,
            silent: false,
            store_history: true,
            stop_on_error: false,
        });
    }

    fn flush_queued_runs(&mut self) {
        if self.run_all_queued {
            self.run_all_queued = false;
        }
        let ids = self.queued_runs.drain(..).collect::<Vec<_>>();
        for id in ids {
            self.send_cell_execute(&id);
        }
    }

    fn restart_kernel(&mut self, run_all_after: bool) {
        self.kernel_alive = false;
        self.kernel_phase = KernelPhase::Starting;
        self.run_states.clear();
        self.send_command(KernelCmd::Restart);
        if run_all_after {
            self.run_all_queued = true;
        }
    }

    fn send_command(&mut self, command: KernelCmd) {
        if let Err(error) = self.kernel.send(command) {
            self.notice = Some(format!("kernel command failed: {error}"));
        }
    }

    fn request_quit(&mut self) -> Effect {
        if self.document.dirty_source || self.document.dirty_outputs {
            self.overlays.confirm = Some(ConfirmAction::QuitUnsaved);
            self.focus.push(FocusLayer::Modal(ModalKind::ConfirmBar));
            Effect::None
        } else {
            self.finish_quit()
        }
    }

    fn finish_quit(&mut self) -> Effect {
        self.should_quit = true;
        Effect::Quit
    }

    fn select_cell(&mut self, cell_id: CellId) {
        if self.document.cell(&cell_id).is_some() {
            self.selected_cell = cell_id;
            self.ensure_selected_visible();
        }
    }

    fn selected_index(&self) -> usize {
        self.document.position_of(&self.selected_cell).unwrap_or(0)
    }

    fn reconcile_selection(&mut self) {
        if self.document.cell(&self.selected_cell).is_none() {
            if let Some(id) = self.document.order().first().cloned() {
                self.selected_cell = id;
            }
        }
        self.remount_output_views();
    }

    fn ensure_selected_visible(&mut self) {
        // Applied on the next draw, against that frame's layout.
        self.follow_focus = true;
    }

    /// Cells queued or executing. Derived from `run_states` rather than
    /// counted from events, because silent executions (bootstraps, config)
    /// also emit start events and an event counter drifts.
    fn active_runs(&self) -> usize {
        self.run_states
            .values()
            .filter(|state| matches!(state, RunState::Running | RunState::Queued))
            .count()
    }

    fn cell_source(&self, cell_id: &CellId) -> String {
        self.document
            .cell(cell_id)
            .map(|cell| cell.buffer.as_str().to_string())
            .unwrap_or_default()
    }

    fn commit_cell(&mut self, cell_id: &CellId) {
        let new_source = self.cell_source(cell_id);
        let old_source = self
            .cell_baselines
            .get(cell_id)
            .cloned()
            .unwrap_or_else(|| new_source.clone());
        if old_source != new_source {
            self.document.apply(DocOp::CommitCellSource {
                cell_id: cell_id.clone(),
                old_source,
                new_source: new_source.clone(),
            });
        }
        self.cell_baselines.remove(cell_id);
    }

    fn pop_pdb(&mut self) {
        self.overlays.pdb_dock = None;
        if matches!(self.focus.top(), FocusLayer::PdbInput) {
            self.focus.pop();
        }
        self.term_effects.set_cursor_shape(CursorShape::SteadyBlock);
    }

    fn jump_search(&mut self, query: &str, forward: bool) {
        if query.is_empty() || self.document.order().is_empty() {
            return;
        }
        let start = self.selected_index();
        let len = self.document.order().len();
        for step in 1..=len {
            let offset = if forward {
                step
            } else {
                len.saturating_sub(step)
            };
            let index = (start + offset) % len;
            let Some(cell) = self.document.cell(&self.document.order()[index]) else {
                continue;
            };
            if cell.buffer.as_str().contains(query) {
                self.select_cell(cell.id.clone());
                return;
            }
        }
        self.notice = Some(format!("pattern not found: {query}"));
    }

    fn execute_ex_command(&mut self, raw: String) -> Effect {
        let command = raw.trim();
        if command == "panic" {
            panic!("intentional panic via :panic");
        }
        if command == "q!" {
            self.document.dirty_source = false;
            self.document.dirty_outputs = false;
            self.delete_autosave();
            return self.finish_quit();
        }
        if command == "q" {
            return self.request_quit();
        }
        if command == "wq" || command == "x" || command == "wq!" || command == "x!" {
            let force = command.ends_with('!');
            if self.save_document(force) {
                return self.finish_quit();
            }
            return Effect::None;
        }
        if let Some(rest) = command.strip_prefix("w ") {
            self.save_as(self.resolve_save_path(rest.trim()), false);
            return Effect::None;
        }
        if command == "w" || command == "w!" {
            self.save_document(command.ends_with('!'));
            return Effect::None;
        }
        if let Ok(number) = command.parse::<usize>() {
            if let Some(id) = self.document.order().get(number.saturating_sub(1)).cloned() {
                self.select_cell(id);
            }
        }
        Effect::None
    }

    fn resolve_save_path(&self, raw: &str) -> PathBuf {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else if let Some(current) = self.path.as_ref().and_then(|path| path.parent()) {
            current.join(path)
        } else {
            path
        }
    }

    fn save_document(&mut self, force: bool) -> bool {
        let Some(path) = self.path.clone() else {
            self.overlays.prompt = Some(PromptBar::new(PromptKind::SaveAs, ""));
            self.focus.push(FocusLayer::Modal(ModalKind::ExCommandBar));
            self.notice = Some("enter a path to save".into());
            return false;
        };
        self.save_to_path(&path, force)
    }

    fn save_as(&mut self, path: PathBuf, force: bool) -> bool {
        if self.save_to_path(&path, force) {
            self.path = Some(path);
            self.observed_file_mtime = self.path.as_deref().and_then(file_mtime);
            self.delete_autosave();
            true
        } else {
            false
        }
    }

    fn save_to_path(&mut self, path: &Path, _force: bool) -> bool {
        match save_path(&self.document, path) {
            Ok(()) => {
                self.document.dirty_source = false;
                self.document.dirty_outputs = false;
                self.observed_file_mtime = file_mtime(path);
                self.delete_autosave();
                self.notice = Some(format!("saved {}", path.display()));
                true
            }
            Err(error) => {
                self.notice = Some(format!("save failed: {error}"));
                false
            }
        }
    }

    fn delete_autosave(&mut self) {
        if let Some(path) = Self::autosave_path_for(self.path.as_deref()) {
            let _ = fs::remove_file(path);
        }
    }

    fn check_external_disk_change(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let Some(mtime) = file_mtime(&path) else {
            return false;
        };
        if self.observed_file_mtime.is_some_and(|old| mtime <= old) {
            return false;
        }
        self.observed_file_mtime = Some(mtime);
        if self.document.dirty_source || self.document.dirty_outputs {
            self.overlays.confirm = Some(ConfirmAction::ReloadExternalDiskChange);
            if !matches!(self.focus.top(), FocusLayer::Modal(ModalKind::ConfirmBar)) {
                self.focus.push(FocusLayer::Modal(ModalKind::ConfirmBar));
            }
        } else {
            self.reload_document_from_disk();
        }
        true
    }

    fn reload_document_from_disk(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        match reload_external(&mut self.document, &path) {
            Ok(_) => {
                self.reconcile_selection();
                self.notice = Some(format!("reloaded {}", path.display()));
            }
            Err(error) => self.notice = Some(format!("reload failed: {error}")),
        }
    }

    fn restore_autosave(&mut self) {
        let Some(path) = Self::autosave_path_for(self.path.as_deref()) else {
            return;
        };
        match fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|source| parse_ipynb(&source).map_err(|error| error.to_string()))
        {
            Ok(document) => {
                self.document = document;
                self.selected_cell = self
                    .document
                    .order()
                    .first()
                    .cloned()
                    .unwrap_or_else(CellId::new_random);
                self.remount_output_views();
                self.delete_autosave();
                self.notice = Some("restored autosave".into());
            }
            Err(error) => self.notice = Some(format!("autosave recovery failed: {error}")),
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    fn push_log(&mut self, line: String) {
        if self.logs.len() >= 2_000 {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }
}

struct CoordinatorOutputHost<'a> {
    kernel: &'a mut dyn KernelClient,
    clipboard: &'a mut dyn ClipboardSink,
    selected_cell: &'a mut CellId,
    notice: &'a mut Option<String>,
    jumped: &'a mut Option<(CellId, Option<usize>)>,
}

impl OutputHost for CoordinatorOutputHost<'_> {
    fn send_comm(&mut self, comm_id: CommId, data: Map<String, Value>) {
        if let Err(error) = self.kernel.send(KernelCmd::CommMsg { comm_id, data }) {
            *self.notice = Some(format!("comm send failed: {error}"));
        }
    }

    fn eval_inspect_subshell(&mut self, tag: &'static str, python_code: String) {
        if let Err(error) = self.kernel.send(KernelCmd::Execute {
            target: TargetId::Silent(tag.to_string()),
            lane: Lane::Inspect,
            code: python_code,
            silent: true,
            store_history: false,
            stop_on_error: false,
        }) {
            *self.notice = Some(format!("inspect evaluation failed: {error}"));
        }
    }

    fn copy_text(&mut self, text: String) {
        if let Err(error) = self.clipboard.copy(&text) {
            *self.notice = Some(format!("copy failed: {error}"));
        }
    }

    fn jump_to_cell(&mut self, cell_id: CellId, line: Option<usize>) {
        *self.selected_cell = cell_id.clone();
        *self.jumped = Some((cell_id, line));
    }

    fn post_notice(&mut self, message: String) {
        *self.notice = Some(message);
    }
}

fn wire_output_item(payload: WireOutput, late: bool) -> OutputItem {
    let data = match payload {
        WireOutput::Stream { stream, text } => {
            let mut buffer = wb_core::StreamBuffer::default();
            buffer.push_chunk(&text);
            OutputData::Stream { stream, buffer }
        }
        WireOutput::Rich {
            bundle,
            metadata,
            display_id,
            is_execute_result,
            execution_count,
        } => OutputData::Rich {
            bundle,
            metadata,
            display_id,
            is_execute_result,
            execution_count,
        },
        WireOutput::Error {
            ename,
            evalue,
            traceback,
        } => OutputData::Error {
            ename,
            evalue,
            traceback,
        },
    };
    OutputItem::new(data, late)
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn cursor_at_char_offset(buffer: &wb_core::TextBuffer, offset: usize) -> CursorPos {
    let mut remaining = offset;
    for (row, line) in buffer.as_str().split('\n').enumerate() {
        let len = line.chars().count();
        if remaining <= len {
            return CursorPos {
                row,
                col: remaining,
            };
        }
        remaining = remaining.saturating_sub(len + 1);
    }
    let row = buffer.line_count().saturating_sub(1);
    CursorPos {
        row,
        col: buffer.line_char_len(row),
    }
}
