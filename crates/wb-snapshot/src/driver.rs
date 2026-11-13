//! Drive a real [`Coordinator`] headlessly and snapshot its frames.

use crate::image::{CursorMark, CursorShape};
use crate::script::Step;
use anyhow::Result;
use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wb_core::{Document, FormatBacking};
use wb_kernel::{FakeKernel, FakeKernelHandle, KernelClient, KernelEvent, KernelLaunchSpec, LocalZmqKernel};
use wb_sdk::ExtensionRegistry;
use wb_tui::focus::{CellMode, FocusLayer};
use wb_tui::platform::{MemoryClipboard, NoopTerminalEffects};
use wb_tui::{BuiltinsPlugin, Coordinator, Effect, SettingsStore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelChoice {
    /// No kernel process; `FakeKernelHandle` can inject events.
    Fake,
    /// A real local ipykernel (run inside an environment that has one).
    Real,
}

pub struct HeadlessSession {
    pub coordinator: Coordinator,
    pub terminal: Terminal<TestBackend>,
    pub fake: Option<FakeKernelHandle>,
}

impl HeadlessSession {
    /// Open `notebook` (or an empty scratchpad) at `width`x`height` cells.
    /// Settings come from `config` (a missing file means defaults), never the
    /// user's own config.
    pub fn open(
        notebook: Option<&Path>,
        width: u16,
        height: u16,
        kernel: KernelChoice,
        config: PathBuf,
    ) -> Result<Self> {
        let mut registry = ExtensionRegistry::default();
        registry.install(BuiltinsPlugin);
        let settings = SettingsStore::load_or_default(Some(config), &registry);
        let document = match notebook {
            Some(path) if path.exists() => wb_core::codec::open_path(path)?,
            _ => Document::new_empty(FormatBacking::PercentPy { header: None }),
        };
        let (client, fake): (Box<dyn KernelClient>, Option<FakeKernelHandle>) = match kernel {
            KernelChoice::Fake => {
                let (client, handle) = FakeKernel::pair();
                (Box::new(client), Some(handle))
            }
            KernelChoice::Real => {
                let working_dir = notebook
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let client = LocalZmqKernel::spawn(KernelLaunchSpec {
                    requested_name: settings.snapshot().core.default_kernel.clone(),
                    working_dir,
                    existing_connection_file: None,
                });
                (Box::new(client), None)
            }
        };
        let coordinator = Coordinator::new(
            notebook.map(Path::to_path_buf),
            document,
            registry,
            settings,
            client,
            Box::new(MemoryClipboard::default()),
            Box::new(NoopTerminalEffects),
        );
        Ok(Self {
            coordinator,
            terminal: Terminal::new(TestBackend::new(width, height))?,
            fake,
        })
    }

    /// Inject an event as if the (fake) kernel sent it.
    pub fn emit(&mut self, event: KernelEvent) {
        self.coordinator.apply_kernel_event(event);
    }

    pub fn press(&mut self, key: KeyEvent) -> Effect {
        self.coordinator.on_key(key, Instant::now())
    }

    /// Let the app run for `duration`: apply kernel events and ticks, as the
    /// runtime's select loop would.
    pub async fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            while let Some(event) = self.coordinator.kernel.try_recv() {
                self.coordinator.apply_kernel_event(event);
            }
            self.coordinator.on_tick(Instant::now());
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait (pumping) until `done` holds or `timeout` passes.
    pub async fn pump_until(&mut self, timeout: Duration, done: impl Fn(&Coordinator) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(50)).await;
            if done(&self.coordinator) {
                return true;
            }
        }
        false
    }

    /// Run a parsed script; `on_snap` receives each `<snap:NAME>`.
    pub async fn run_script(
        &mut self,
        steps: &[Step],
        key_gap: Duration,
        mut on_snap: impl FnMut(&str, &Buffer, Option<CursorMark>) -> Result<()>,
    ) -> Result<()> {
        for step in steps {
            match step {
                Step::Key(key) => {
                    if matches!(self.press(*key), Effect::Quit) {
                        break;
                    }
                    self.pump(key_gap).await;
                }
                Step::Wait(ms) => self.pump(Duration::from_millis(*ms)).await,
                Step::Snap(name) => {
                    let (buffer, cursor) = self.render()?;
                    on_snap(name, &buffer, cursor)?;
                }
            }
        }
        Ok(())
    }

    /// Draw one frame; returns the buffer and the cursor the app placed (if any).
    pub fn render(&mut self) -> Result<(Buffer, Option<CursorMark>)> {
        let coordinator = &mut self.coordinator;
        self.terminal.draw(|frame| coordinator.draw(frame))?;
        let backend = self.terminal.backend();
        let shape = match self.coordinator.focus.top() {
            FocusLayer::CellEdit {
                mode: CellMode::Insert,
                ..
            }
            | FocusLayer::PdbInput => CursorShape::Bar,
            _ => CursorShape::Block,
        };
        let cursor = backend.cursor_visible().then(|| {
            let position = backend.cursor_position();
            CursorMark {
                x: position.x,
                y: position.y,
                shape,
            }
        });
        Ok((backend.buffer().clone(), cursor))
    }
}
