//! Modular code-intelligence controllers: completion popup, signature help, and inspect pager.

use wb_kernel::CompletionCandidate;

#[derive(Clone, Debug)]
pub struct CompletionPopup {
    pub req_id: u64,
    pub replace_start_char: usize,
    pub replace_end_char: usize,
    pub all_candidates: Vec<CompletionCandidate>,
    pub filtered: Vec<CompletionCandidate>,
    pub selected: usize,
    pub scroll_top: usize,
}

#[derive(Clone, Debug)]
pub struct SignaturePopup {
    pub signature_text: String,
    pub active_param_byte_range: Option<std::ops::Range<usize>>,
}

#[derive(Clone, Debug)]
pub struct InspectPager {
    pub title: String,
    pub ansi_text: String,
    pub scroll_top: usize,
}
