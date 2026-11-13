//! `CellId`-keyed layout cache with 64 MiB LRU eviction, idle resize warming, and per-line syntax cache.

use crate::layout::fenwick::FenwickTree;
use ratatui::text::Line;
use std::collections::{HashMap, VecDeque};
use wb_core::{CellId, CellRevisions, Document, OutputId};
use wb_sdk::{OutputView, Theme};

pub const RENDER_CACHE_BUDGET_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CellLayoutKey {
    pub revs: CellRevisions,
    pub output_views_rev: u64,
    pub width: u16,
    pub theme_rev: u64,
}

#[derive(Debug)]
pub struct CachedCellLayout {
    pub key: CellLayoutKey,
    pub wrapped_source: Vec<Line<'static>>,
    pub total_rows: u32,
    pub last_seen_frame: u64,
    pub approx_bytes: usize,
}

#[derive(Default)]
pub struct LayoutCache {
    cells: HashMap<CellId, CachedCellLayout>,
    heights: FenwickTree,
    idle_warm_queue: VecDeque<CellId>,
    frame_counter: u64,
    last_width: u16,
}

impl LayoutCache {
    pub fn heights(&self) -> &FenwickTree {
        &self.heights
    }

    pub fn last_width(&self) -> u16 {
        self.last_width
    }

    /// Synchronize visible cells immediately for `width` and enqueue off-screen cells
    /// into `idle_warm_queue` on terminal resize so frame times stay well below 16 ms.
    pub fn sync_viewport(
        &mut self,
        _doc: &Document,
        _output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        _width: u16,
        _scroll_row: u32,
        _viewport_height: u16,
        _max_output_rows: u16,
        _theme: &Theme,
    ) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
        todo!("wb-tui::layout::cache: measure visible cells, update FenwickTree, evict beyond 64 MiB")
    }

    /// Measure up to `max_cells` off-screen cells during idle ticks after a terminal resize.
    pub fn warm_idle_chunk(
        &mut self,
        _doc: &Document,
        _output_views: &mut HashMap<OutputId, Box<dyn OutputView>>,
        _max_cells: usize,
        _max_output_rows: u16,
        _theme: &Theme,
    ) -> bool {
        let _ = &self.cells;
        let _ = &self.idle_warm_queue;
        todo!("wb-tui::layout::cache: drain up to max_cells from idle_warm_queue")
    }
}
