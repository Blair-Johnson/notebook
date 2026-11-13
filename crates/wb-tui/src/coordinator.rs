//! Central `Coordinator` state machine and `Effect` chokepoint.

use crate::config_store::SettingsStore;
use crate::focus::FocusStack;
use crate::grammar::VimGrammar;
use crate::intelligence::{CompletionPopup, InspectPager, SignaturePopup};
use crate::layout::LayoutCache;
use crate::overlays::OverlayStates;
use crate::platform::{ClipboardSink, MemoryClipboard, NoopTerminalEffects, TerminalEffects};
use crate::render::{AppView, FrameGeometry, render_app};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use wb_core::{CellId, Document, OutputId};
use wb_kernel::{KernelClient, KernelCmd, KernelEvent};
use wb_sdk::{ExtensionRegistry, OutputView};

#[derive(Debug)]
pub enum Effect {
    None,
    OpenExternalEditor(CellId),
    Quit,
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
    pub running_cells: usize,
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
        let selected_cell = document.order()[0].clone();
        Self {
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
            running_cells: 0,
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
        }
    }

    pub fn headless(document: Document, registry: ExtensionRegistry, kernel: Box<dyn KernelClient>) -> Self {
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
        let view = AppView {
            path: self.path.as_deref(),
            document: &self.document,
            selected_cell: &self.selected_cell,
            focus: &self.focus,
            scroll_row: self.scroll_row,
            kernel_name: &self.kernel_name,
            kernel_busy: self.kernel_busy,
            kernel_alive: self.kernel_alive,
            running_cells: self.running_cells,
            notice: self.notice.as_deref(),
            settings: &snap.core,
            status_segments: &self.registry.status_segments,
            completion: self.completion.as_ref(),
            signature: self.signature.as_ref(),
            inspect_pager: self.inspect_pager.as_ref(),
            overlays: &self.overlays,
        };
        self.last_geometry = render_app(
            frame,
            &view,
            &mut self.output_views,
            &snap.theme,
            &snap.keymap,
            &mut self.layout_cache,
        );
    }

    pub fn on_key(&mut self, _key: KeyEvent, _now: Instant) -> Effect {
        todo!("wb-tui::coordinator: normalize key -> VimGrammar -> active FocusLayer -> execute")
    }

    pub fn on_paste(&mut self, _text: String) {
        todo!("wb-tui::coordinator: route bracketed paste to cell.insert, pdb_dock, or prompt")
    }

    pub fn on_mouse(&mut self, _mouse: MouseEvent) {
        todo!("wb-tui::coordinator: resolve hit against self.last_geometry and scroll or focus")
    }

    pub fn apply_kernel_event(&mut self, _event: KernelEvent) {
        let _ = KernelCmd::Interrupt;
        todo!("wb-tui::coordinator: apply KernelEvent, mount/update OutputViews, push PdbInput on stdin")
    }
}
