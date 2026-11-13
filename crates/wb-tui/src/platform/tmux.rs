//! Background `TMUX_PANE` title updater that restores the original title on exit.

#[derive(Default)]
pub struct TmuxPaneTitle {
    pane_id: Option<String>,
}

impl TmuxPaneTitle {
    pub fn detect() -> Self {
        Self {
            pane_id: std::env::var("TMUX_PANE").ok(),
        }
    }

    pub fn update(&mut self, _title: &str) {
        let _ = &self.pane_id;
        todo!("wb-tui::platform::tmux: asynchronously update pane title and restore original on Drop")
    }
}
