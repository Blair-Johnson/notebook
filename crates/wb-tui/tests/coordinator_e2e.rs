use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use serde_json::{Map, Value};
use wb_core::{
    Cell, CommId, Document, FormatBacking, MimeBundle, OutputData, OutputItem, TargetId,
};
use wb_kernel::{FakeKernel, KernelCmd, KernelEvent, KernelSessionInfo, WireOutput};
use wb_sdk::{
    ExtensionRegistry, InputOutcome, KeyContextId, MountCtx, OutputHost, OutputMetrics,
    OutputRenderer, OutputView, Plugin, ResolvedInput, Theme,
};
use wb_tui::{BuiltinsPlugin, CellMode, Coordinator, FocusLayer};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn unique_path(ext: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "workbench-coordinator-e2e-{}-{stamp}.{ext}",
        std::process::id()
    ))
}

fn document_with_sources(sources: &[&str], backing: FormatBacking) -> Document {
    let cells = sources
        .iter()
        .map(|source| {
            let mut cell = Cell::new_code();
            cell.buffer.replace_all((*source).to_string());
            cell
        })
        .collect();
    Document::from_cells(cells, backing)
}

fn registry() -> ExtensionRegistry {
    let mut registry = ExtensionRegistry::default();
    registry.install(BuiltinsPlugin);
    registry
}

#[test]
fn counted_navigation_cell_edit_and_run_are_headless() {
    let (kernel, mut handle) = FakeKernel::pair();
    let mut coordinator = Coordinator::headless(
        document_with_sources(
            &["a\nb\nc", "second", "third", "fourth"],
            FormatBacking::PercentPy { header: None },
        ),
        registry(),
        Box::new(kernel),
    );
    let now = Instant::now();

    coordinator.on_key(key(KeyCode::Char('3')), now);
    coordinator.on_key(key(KeyCode::Char('j')), now);
    assert_eq!(coordinator.selected_cell, coordinator.document.order()[3]);

    coordinator.on_key(key(KeyCode::Char('i')), now);
    assert!(matches!(
        coordinator.focus.top(),
        FocusLayer::CellEdit {
            mode: CellMode::Normal,
            ..
        }
    ));
    coordinator.on_key(key(KeyCode::Char('1')), now);
    coordinator.on_key(key(KeyCode::Char('0')), now);
    coordinator.on_key(key(KeyCode::Char('j')), now);
    let selected = coordinator
        .document
        .cell(&coordinator.selected_cell)
        .unwrap();
    assert_eq!(
        selected.buffer.cursor().row,
        0,
        "vertical motion clamps to one-line cell"
    );

    coordinator.on_key(key(KeyCode::Char('i')), now);
    coordinator.on_paste("typed".to_string());
    coordinator.on_key(key(KeyCode::Char('j')), now);
    coordinator.on_key(key(KeyCode::Char('j')), now);
    assert!(matches!(
        coordinator.focus.top(),
        FocusLayer::CellEdit {
            mode: CellMode::Normal,
            ..
        }
    ));
    assert!(
        coordinator
            .document
            .cell(&coordinator.selected_cell)
            .unwrap()
            .buffer
            .as_str()
            .contains("typed")
    );
    assert!(
        !coordinator
            .document
            .cell(&coordinator.selected_cell)
            .unwrap()
            .buffer
            .as_str()
            .contains("typedj"),
        "completed jj rolls back its speculative j"
    );

    coordinator.on_key(key(KeyCode::Esc), now);
    assert!(matches!(
        coordinator.focus.top(),
        FocusLayer::NotebookNormal
    ));
    coordinator.apply_kernel_event(KernelEvent::Ready {
        info: KernelSessionInfo {
            generation: 1,
            kernel_name: "python".into(),
            display_name: "Python".into(),
            spec_dir: PathBuf::new(),
            connection_file: PathBuf::new(),
            pid: None,
            supports_subshells: false,
            supports_debugger: false,
        },
    });
    let _ = handle.drain_commands();
    coordinator.on_key(key(KeyCode::Enter), now);
    coordinator.on_key(key(KeyCode::Enter), now);
    let commands = handle.drain_commands();
    assert!(commands.into_iter().any(|command| matches!(
        command,
        KernelCmd::Execute {
            target: TargetId::Cell(_),
            ..
        }
    )));
    coordinator.on_key(key(KeyCode::Esc), now);
    assert!(matches!(
        coordinator.focus.top(),
        FocusLayer::NotebookNormal
    ));

    coordinator.on_key(key(KeyCode::Char('d')), now);
    coordinator.on_key(key(KeyCode::Char('d')), now);
    assert_eq!(coordinator.document.order().len(), 3);
}

#[derive(Clone, Default)]
struct RecordingState {
    mounts: usize,
    inputs: usize,
    comms: Vec<Map<String, Value>>,
}

#[derive(Clone)]
struct RecordingPlugin {
    state: Arc<Mutex<RecordingState>>,
}

struct RecordingRenderer {
    state: Arc<Mutex<RecordingState>>,
}

struct RecordingView {
    state: Arc<Mutex<RecordingState>>,
}

impl Plugin for RecordingPlugin {
    fn id(&self) -> &'static str {
        "coordinator-e2e-recording"
    }

    fn register(&self, registry: &mut ExtensionRegistry) {
        registry.add_renderer(RecordingRenderer {
            state: self.state.clone(),
        });
    }
}

impl OutputRenderer for RecordingRenderer {
    fn id(&self) -> &'static str {
        "coordinator-e2e-renderer"
    }

    fn claimed_mimes(&self) -> &'static [(&'static str, u16)] {
        &[("application/x-workbench-e2e", 10_000)]
    }

    fn can_render(&self, item: &OutputItem) -> Option<u16> {
        match &item.data {
            OutputData::Rich { bundle, .. }
                if bundle.contains_key("application/x-workbench-e2e") =>
            {
                Some(10_000)
            }
            _ => None,
        }
    }

    fn mount(&self, _item: &OutputItem, _ctx: &MountCtx) -> Box<dyn OutputView> {
        self.state.lock().unwrap().mounts += 1;
        Box::new(RecordingView {
            state: self.state.clone(),
        })
    }
}

impl OutputView for RecordingView {
    fn on_output_updated(&mut self, _item: &OutputItem, _ctx: &MountCtx) {}

    fn on_comm_message(
        &mut self,
        _comm_id: &CommId,
        data: &Map<String, Value>,
        _host: &mut dyn OutputHost,
    ) {
        self.state.lock().unwrap().comms.push(data.clone());
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn key_context(&self) -> Option<KeyContextId> {
        Some(KeyContextId::new_static("output.stream"))
    }

    fn view_revision(&self) -> u64 {
        1
    }

    fn measure(
        &mut self,
        _item: &OutputItem,
        _width: u16,
        _theme: &Theme,
        _cap_rows: Option<u16>,
    ) -> OutputMetrics {
        OutputMetrics {
            total_rows: 1,
            shown_rows: 1,
            clipped: false,
        }
    }

    fn render_slice(
        &self,
        _item: &OutputItem,
        _width: u16,
        _rows: Range<u32>,
        _focused: bool,
        _theme: &Theme,
        output: &mut Vec<Line<'static>>,
    ) {
        output.push(Line::from("recording output"));
    }

    fn handle_input(
        &mut self,
        _item: &OutputItem,
        _input: ResolvedInput,
        host: &mut dyn OutputHost,
    ) -> InputOutcome {
        self.state.lock().unwrap().inputs += 1;
        let mut data = Map::new();
        data.insert("from_view".into(), Value::Bool(true));
        host.send_comm(CommId("e2e".into()), data);
        InputOutcome::Consumed
    }
}

#[test]
fn custom_output_mount_focus_and_comm_round_trip() {
    let state = Arc::new(Mutex::new(RecordingState::default()));
    let mut registry = registry();
    registry.install(RecordingPlugin {
        state: state.clone(),
    });
    let (kernel, mut handle) = FakeKernel::pair();
    let document =
        document_with_sources(&["print('x')"], FormatBacking::PercentPy { header: None });
    let cell_id = document.order()[0].clone();
    let mut coordinator = Coordinator::headless(document, registry, Box::new(kernel));

    let bundle: MimeBundle = BTreeMap::from([(
        "application/x-workbench-e2e".to_string(),
        Value::String("payload".into()),
    )]);
    coordinator.apply_kernel_event(KernelEvent::Output {
        exec_id: None,
        target: TargetId::Cell(cell_id.clone()),
        late: false,
        payload: WireOutput::Rich {
            bundle,
            metadata: Map::new(),
            display_id: None,
            is_execute_result: false,
            execution_count: None,
        },
    });
    assert_eq!(state.lock().unwrap().mounts, 1);

    let now = Instant::now();
    coordinator.on_key(key(KeyCode::Char('z')), now);
    coordinator.on_key(key(KeyCode::Char('o')), now);
    assert!(matches!(
        coordinator.focus.top(),
        FocusLayer::OutputFocus { .. }
    ));
    coordinator.on_key(key(KeyCode::Char('j')), now);
    assert!(handle.drain_commands().into_iter().any(|command| {
        matches!(command, KernelCmd::CommMsg { comm_id, data } if comm_id == CommId("e2e".into()) && data.get("from_view") == Some(&Value::Bool(true)))
    }));

    let mut incoming = Map::new();
    incoming.insert("from_kernel".into(), Value::Bool(true));
    coordinator.apply_kernel_event(KernelEvent::CommMessage {
        comm_id: CommId("e2e".into()),
        data: incoming,
    });
    assert_eq!(state.lock().unwrap().comms.len(), 1);
    assert_eq!(
        state.lock().unwrap().comms[0].get("from_kernel"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn input_requested_opens_pdb_and_submits_reply_input() {
    let (kernel, mut handle) = FakeKernel::pair();
    let mut coordinator = Coordinator::headless(
        document_with_sources(&["x = input()"], FormatBacking::PercentPy { header: None }),
        registry(),
        Box::new(kernel),
    );
    coordinator.apply_kernel_event(KernelEvent::InputRequested {
        prompt: "value: ".into(),
        password: false,
    });
    assert!(matches!(coordinator.focus.top(), FocusLayer::PdbInput));
    coordinator.on_paste("42".into());
    coordinator.on_key(key(KeyCode::Enter), Instant::now());
    assert!(
        handle
            .drain_commands()
            .into_iter()
            .any(|command| { matches!(command, KernelCmd::ReplyInput { value } if value == "42") })
    );
}

fn execute_ex_command(coordinator: &mut Coordinator, command: &str) {
    let now = Instant::now();
    coordinator.on_key(key(KeyCode::Char(':')), now);
    coordinator.on_paste(command.to_string());
    coordinator.on_key(key(KeyCode::Enter), now);
}

#[test]
fn ex_command_saves_py_and_ipynb_and_recovers_autosave_sidecar() {
    let py_path = unique_path("py");
    let (kernel, _handle) = FakeKernel::pair();
    let mut py = Coordinator::headless(
        document_with_sources(&["print('py')"], FormatBacking::PercentPy { header: None }),
        registry(),
        Box::new(kernel),
    );
    py.path = Some(py_path.clone());
    execute_ex_command(&mut py, "w");
    assert!(
        fs::read_to_string(&py_path)
            .unwrap()
            .contains("print('py')")
    );

    let ipynb_path = unique_path("ipynb");
    let (kernel, _handle) = FakeKernel::pair();
    let mut ipynb = Coordinator::headless(
        document_with_sources(
            &["print('ipynb')"],
            FormatBacking::Ipynb { extra: Map::new() },
        ),
        registry(),
        Box::new(kernel),
    );
    ipynb.path = Some(ipynb_path.clone());
    execute_ex_command(&mut ipynb, "w");
    assert!(
        fs::read_to_string(&ipynb_path)
            .unwrap()
            .contains("\"cells\"")
    );

    let recovery_path = unique_path("ipynb");
    let (kernel, _handle) = FakeKernel::pair();
    let mut recovery = Coordinator::headless(
        document_with_sources(&["original"], FormatBacking::Ipynb { extra: Map::new() }),
        registry(),
        Box::new(kernel),
    );
    recovery.path = Some(recovery_path.clone());
    let cell_id = recovery.selected_cell.clone();
    recovery.emergency_autosave();
    let sidecar = Coordinator::autosave_path_for(Some(&recovery_path)).unwrap();
    assert!(sidecar.is_file());
    recovery.apply_external_edit(&cell_id, "changed".into());
    recovery.check_autosave_recovery();
    assert!(matches!(recovery.focus.top(), FocusLayer::Modal(_)));
    recovery.on_key(key(KeyCode::Char('r')), Instant::now());
    assert_eq!(
        recovery
            .document
            .cell(&recovery.selected_cell)
            .unwrap()
            .buffer
            .as_str(),
        "original"
    );

    let _ = fs::remove_file(&py_path);
    let _ = fs::remove_file(&ipynb_path);
    let _ = fs::remove_file(&recovery_path);
    let _ = fs::remove_file(sidecar);
}

#[test]
fn rerunning_a_cell_replaces_its_outputs() {
    let (kernel, mut handle) = FakeKernel::pair();
    let document =
        document_with_sources(&["print('x')"], FormatBacking::PercentPy { header: None });
    let cell_id = document.order()[0].clone();
    let mut coordinator = Coordinator::headless(document, registry(), Box::new(kernel));
    coordinator.apply_kernel_event(KernelEvent::Ready {
        info: KernelSessionInfo {
            generation: 1,
            kernel_name: "python".into(),
            display_name: "Python".into(),
            spec_dir: PathBuf::new(),
            connection_file: PathBuf::new(),
            pid: None,
            supports_subshells: false,
            supports_debugger: false,
        },
    });
    let now = Instant::now();
    for run in ["first\n", "second\n"] {
        coordinator.on_key(key(KeyCode::Enter), now);
        let _ = handle.drain_commands();
        coordinator.apply_kernel_event(KernelEvent::Output {
            exec_id: None,
            target: TargetId::Cell(cell_id.clone()),
            late: false,
            payload: WireOutput::Stream {
                stream: wb_core::StreamKind::Stdout,
                text: run.into(),
            },
        });
    }
    let outputs = &coordinator.document.cell(&cell_id).unwrap().outputs;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].plain_text(), "second");
}
