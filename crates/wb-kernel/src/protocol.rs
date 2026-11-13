//! Typed commands and events crossing the UI <-> kernel actor boundary.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::Duration;
use wb_core::{CommId, ExecId, MimeBundle, StreamKind, TargetId};

/// Execution lane: `Main` runs user cells serially; `Inspect` routes silent helper
/// evaluations onto a dedicated JEP 91 kernel subshell when supported so queries
/// answer in ~10 ms even while a long training cell is running.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub enum Lane {
    #[default]
    Main,
    Inspect,
}

#[derive(Clone, Debug)]
pub struct KernelSessionInfo {
    pub generation: u64,
    pub kernel_name: String,
    pub display_name: String,
    pub spec_dir: PathBuf,
    pub connection_file: PathBuf,
    pub pid: Option<u32>,
    pub supports_subshells: bool,
    pub supports_debugger: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompletionCandidate {
    pub replace_text: String,
    pub kind: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub enum WireOutput {
    Stream {
        stream: StreamKind,
        text: String,
    },
    Rich {
        bundle: MimeBundle,
        metadata: Map<String, Value>,
        display_id: Option<String>,
        is_execute_result: bool,
        execution_count: Option<i64>,
    },
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

/// Outgoing wire header extension carrying JEP 91 `subshell_id` (omitted by
/// `jupyter-protocol` 2.0.x's closed `Header` struct).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubshellHeader {
    #[serde(flatten)]
    pub inner: jupyter_protocol::messaging::Header,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subshell_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum KernelCmd {
    Execute {
        target: TargetId,
        lane: Lane,
        code: String,
        silent: bool,
        store_history: bool,
        stop_on_error: bool,
    },
    Complete {
        req_id: u64,
        code: String,
        cursor_char_pos: usize,
    },
    Inspect {
        req_id: u64,
        code: String,
        cursor_char_pos: usize,
        detail_level: u8,
    },
    ReplyInput {
        value: String,
    },
    CommOpen {
        comm_id: CommId,
        target_name: String,
        data: Map<String, Value>,
    },
    CommMsg {
        comm_id: CommId,
        data: Map<String, Value>,
    },
    CommClose {
        comm_id: CommId,
        data: Map<String, Value>,
    },
    Interrupt,
    Restart,
    Shutdown {
        kill_process: bool,
    },
}

#[derive(Clone, Debug)]
pub enum KernelEvent {
    Ready {
        info: KernelSessionInfo,
    },
    LaunchFailed {
        generation: u64,
        reason: String,
    },
    ExecutionStarted {
        exec_id: ExecId,
        target: TargetId,
        execution_count: Option<i64>,
    },
    Output {
        exec_id: Option<ExecId>,
        target: TargetId,
        late: bool,
        payload: WireOutput,
    },
    UpdateDisplay {
        display_id: String,
        bundle: MimeBundle,
        metadata: Map<String, Value>,
    },
    ClearOutput {
        target: TargetId,
        wait: bool,
    },
    ExecutionFinished {
        exec_id: ExecId,
        target: TargetId,
        ok: bool,
        elapsed: Duration,
    },
    BusyStateChanged {
        busy: bool,
    },
    CompleteReply {
        req_id: u64,
        start_char: usize,
        items: Vec<CompletionCandidate>,
    },
    InspectReply {
        req_id: u64,
        doc: Option<String>,
    },
    InputRequested {
        prompt: String,
        password: bool,
    },
    CommOpened {
        comm_id: CommId,
        target_name: String,
        data: Map<String, Value>,
        parent_target: Option<TargetId>,
    },
    CommMessage {
        comm_id: CommId,
        data: Map<String, Value>,
    },
    CommClosed {
        comm_id: CommId,
    },
    ProcessLog {
        stream: StreamKind,
        line: String,
    },
    Died {
        generation: u64,
        exit_status: String,
        stderr_tail: Vec<String>,
    },
}
