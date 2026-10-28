use crate::{
    compiler::{resolve_compiler_path, CheckResponse, ContextResponse, ProjectAgent},
    diagnostics::to_lsp_diagnostic,
    formatting::formatting_edits,
    hover::{format_context_hover, format_context_unavailable_hover, format_hole_hover},
    lsp::{
        jsonrpc_error, jsonrpc_notification, jsonrpc_response, read_lsp_message, write_lsp_message,
    },
    navigation::{
        convert_document_symbol, convert_prepare_rename_result, convert_references_to_locations,
        convert_rename_to_workspace_edit, convert_symbol_at_to_location,
        lsp_pos_to_compiler_coords,
    },
    project::{normalize_path, path_to_uri, uri_to_path, ProjectLayout},
    signature::{call_context, prelude_completions, signature_help},
    unicode::{
        apply_content_changes, byte_offset_to_position, extract_hover_target,
        find_enclosing_function_name, position_to_byte_offset, split_lines, HoverTarget, Position,
        Range,
    },
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub explicit_compiler_path: Option<PathBuf>,
    pub debounce_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            explicit_compiler_path: None,
            debounce_ms: 120,
        }
    }
}

#[derive(Clone, Debug)]
struct OpenDocument {
    client_uri: String,
    path: PathBuf,
    version: i64,
    text: String,
    layout: ProjectLayout,
}

#[derive(Clone, Debug)]
struct PendingCheck {
    deadline: Instant,
    generation: u64,
    layout: ProjectLayout,
    trigger_path: PathBuf,
}

enum LoopEvent {
    LspMessage(Value),
    InputClosed,
    CheckCompleted {
        project_target: PathBuf,
        generation: u64,
        response: CheckResponse,
        trigger_path: PathBuf,
        doc_versions: BTreeMap<PathBuf, i64>,
    },
    HoverCompleted {
        request_id: Value,
        hover_value: Value,
    },
    CustomContextCompleted {
        request_id: Value,
        response: ContextResponse,
    },
    SynthesisCompleted {
        request_id: Value,
        method: String,
        query_file: String,
        params: Value,
        layout: ProjectLayout,
        sources: BTreeMap<PathBuf, String>,
        versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
    DefinitionCompleted {
        request_id: Value,
        layout: ProjectLayout,
        source_file_path: PathBuf,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
    DocumentSymbolsCompleted {
        request_id: Value,
        file_path: PathBuf,
        expected_version: Option<i64>,
        doc_text: String,
        response: ContextResponse,
    },
    FormattingCompleted {
        request_id: Value,
        layout: ProjectLayout,
        file_path: PathBuf,
        query_file: String,
        doc_text: String,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
    ReferencesCompleted {
        request_id: Value,
        layout: ProjectLayout,
        source_file_path: PathBuf,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
    PrepareRenameCompleted {
        request_id: Value,
        layout: ProjectLayout,
        source_file_path: PathBuf,
        doc_text: String,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
    RenameCompleted {
        request_id: Value,
        layout: ProjectLayout,
        source_file_path: PathBuf,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
        response: ContextResponse,
    },
}

enum WorkerRequest {
    Synthesis {
        request_id: Value,
        method: String,
        query_file: String,
        params: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        sources: BTreeMap<PathBuf, String>,
        versions: BTreeMap<PathBuf, i64>,
        payload: Value,
    },
    Check {
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        generation: u64,
        latest_gen: Arc<AtomicU64>,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
        trigger_path: PathBuf,
        doc_versions: BTreeMap<PathBuf, i64>,
    },
    HoverContext {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        candidates: Vec<String>,
        overlays: BTreeMap<String, String>,
        range: Range,
        cached_hole_details: Option<Value>,
        is_direct_hole: bool,
        query_file: String,
        line: usize,
        column: usize,
    },
    CustomContext {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        function_name: String,
        overlays: BTreeMap<String, String>,
    },
    SignatureHelp {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        query_file: String,
        line: usize,
        column: usize,
        active: usize,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
    },
    Definition {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        source_file_path: PathBuf,
        query_file: String,
        line: usize,
        column: usize,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
    },
    DocumentSymbols {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        file_path: PathBuf,
        query_file: String,
        doc_text: String,
        expected_version: Option<i64>,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
    },
    Formatting {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        file_path: PathBuf,
        query_file: String,
        doc_text: String,
        overlays: BTreeMap<String, String>,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
    },
    References {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        source_file_path: PathBuf,
        query_file: String,
        line: usize,
        column: usize,
        include_declaration: bool,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
    },
    PrepareRename {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        source_file_path: PathBuf,
        query_file: String,
        doc_text: String,
        line: usize,
        column: usize,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
    },
    Rename {
        request_id: Value,
        layout: ProjectLayout,
        compiler_bin: PathBuf,
        source_file_path: PathBuf,
        query_file: String,
        line: usize,
        column: usize,
        new_name: String,
        snapshot_fingerprint: Option<String>,
        overlays: BTreeMap<String, String>,
        overlay_full_paths: BTreeMap<String, PathBuf>,
        expected_doc_versions: BTreeMap<PathBuf, i64>,
    },
    MarkReload {
        project_target: PathBuf,
    },
    ForceRestart {
        project_target: PathBuf,
    },
    ForceRestartAll,
    CloseProject {
        project_target: PathBuf,
    },
    Shutdown,
}

/// Run the LSP server over standard input and standard output.
/// Returns the desired process exit code (`0` on normal shutdown, `1` if `exit` arrived without `shutdown`).
pub fn serve_stdio(config: ServerConfig) -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = std::io::BufReader::new(stdin);
    let writer = std::io::BufWriter::new(stdout);
    serve_streams(reader, writer, config)
}

/// Run the LSP server over arbitrary `BufRead` / `Write` streams (used by both stdio and integration tests).
pub fn serve_streams<R, W>(mut reader: R, mut writer: W, config: ServerConfig) -> i32
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let (event_tx, event_rx) = mpsc::channel::<LoopEvent>();
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerRequest>();
    let cancelled_ids = Arc::new(Mutex::new(BTreeSet::<String>::new()));

    // 1. Spawn the LSP input reader thread.
    let reader_event_tx = event_tx.clone();
    thread::spawn(move || loop {
        match read_lsp_message(&mut reader) {
            Ok(Some(msg)) => {
                if reader_event_tx.send(LoopEvent::LspMessage(msg)).is_err() {
                    break;
                }
            }
            Ok(None) => {
                let _ = reader_event_tx.send(LoopEvent::InputClosed);
                break;
            }
            Err(e) => {
                eprintln!("[anode-lsp] input stream error: {e}");
                let _ = reader_event_tx.send(LoopEvent::InputClosed);
                break;
            }
        }
    });

    // 2. Spawn the compiler worker thread managing per-project `anode.exe agent` processes.
    let worker_event_tx = event_tx.clone();
    let worker_cancelled_ids = cancelled_ids.clone();
    let worker_handle = thread::spawn(move || {
        run_compiler_worker(worker_rx, worker_event_tx, worker_cancelled_ids);
    });

    // 3. Run the coordinator / debounce event loop.
    let mut state = CoordinatorState::new(config, worker_tx, cancelled_ids);
    let exit_code = state.run_loop(event_rx, &mut writer);

    let _ = state.worker_tx.send(WorkerRequest::Shutdown);
    let _ = worker_handle.join();
    exit_code
}

fn req_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn is_req_cancelled(cancelled_ids: &Arc<Mutex<BTreeSet<String>>>, id: &Value) -> bool {
    cancelled_ids
        .lock()
        .map(|set| set.contains(&req_id_key(id)))
        .unwrap_or(false)
}

fn run_compiler_worker(
    rx: Receiver<WorkerRequest>,
    event_tx: Sender<LoopEvent>,
    cancelled_ids: Arc<Mutex<BTreeSet<String>>>,
) {
    let mut agents: BTreeMap<PathBuf, ProjectAgent> = BTreeMap::new();

    while let Ok(req) = rx.recv() {
        match req {
            WorkerRequest::Check {
                layout,
                compiler_bin,
                generation,
                latest_gen,
                overlays,
                overlay_full_paths,
                trigger_path,
                doc_versions,
            } => {
                let target_key = layout.agent_target.clone();
                if generation < latest_gen.load(Ordering::SeqCst) {
                    eprintln!(
                        "[anode-lsp] skipping queued stale check for {} (gen {} < latest {})",
                        target_key.display(),
                        generation,
                        latest_gen.load(Ordering::SeqCst)
                    );
                    continue;
                }

                let agent = agents
                    .entry(target_key.clone())
                    .or_insert_with(|| ProjectAgent::new(layout));
                let response = agent.check(&compiler_bin, &overlays, &overlay_full_paths);

                if generation < latest_gen.load(Ordering::SeqCst) {
                    eprintln!(
                        "[anode-lsp] discarding stale compiler check response for {} (gen {} < latest {})",
                        target_key.display(),
                        generation,
                        latest_gen.load(Ordering::SeqCst)
                    );
                    continue;
                }

                let _ = event_tx.send(LoopEvent::CheckCompleted {
                    project_target: target_key,
                    generation,
                    response,
                    trigger_path,
                    doc_versions,
                });
            }
            WorkerRequest::HoverContext {
                request_id,
                layout,
                compiler_bin,
                candidates,
                overlays,
                range,
                cached_hole_details,
                is_direct_hole,
                query_file,
                line,
                column,
            } => {
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout));

                if is_direct_hole {
                    if let Some(details) = cached_hole_details.as_ref() {
                        let hover = format_hole_hover(details, Some(range));
                        let _ = event_tx.send(LoopEvent::HoverCompleted {
                            request_id,
                            hover_value: hover,
                        });
                        continue;
                    }
                }

                let mut chosen_hover: Option<Value> = None;
                if !is_direct_hole {
                    let response = agent.symbol_at(
                        &compiler_bin,
                        &query_file,
                        line,
                        column,
                        &overlays,
                        &BTreeMap::new(),
                    );
                    if let Some(symbol) = response
                        .result
                        .as_ref()
                        .and_then(|v| v.get("symbol"))
                        .filter(|s| !s.is_null())
                    {
                        if !matches!(
                            symbol.get("kind").and_then(Value::as_str),
                            Some("function" | "test")
                        ) {
                            chosen_hover = Some(
                                json!({"contents":{"kind":"markdown","value":format!("```anode\n{}\n```",symbol.get("detail").and_then(Value::as_str).unwrap_or(""))},"range":range.to_json()}),
                            );
                        }
                    }
                }
                for candidate in &candidates {
                    if chosen_hover.is_some() {
                        break;
                    }
                    let resp = agent.context(&compiler_bin, candidate, &overlays);
                    if resp.ok {
                        if let Some(res_val) = resp.result.as_ref() {
                            chosen_hover = Some(format_context_hover(res_val, Some(range)));
                            break;
                        }
                    } else if let Some(first_diag) = resp.raw_diagnostics.first() {
                        let msg = first_diag
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if msg.starts_with("unknown function ") {
                            // Try next candidate
                            continue;
                        }
                        // Project failed `checked(false)` (e.g. unresolved hole or compile error).
                        if is_direct_hole {
                            if let Some(details) =
                                first_diag.get("details").filter(|d| !d.is_null())
                            {
                                if details.get("kind").and_then(Value::as_str)
                                    == Some("synthesis_hole")
                                {
                                    chosen_hover = Some(format_hole_hover(details, Some(range)));
                                    break;
                                }
                            }
                        }
                        chosen_hover = Some(format_context_unavailable_hover(
                            candidate,
                            first_diag,
                            Some(range),
                        ));
                        break;
                    }
                }

                let hover_value = chosen_hover
                    .or_else(|| {
                        cached_hole_details
                            .as_ref()
                            .map(|d| format_hole_hover(d, Some(range)))
                    })
                    .unwrap_or(Value::Null);

                let _ = event_tx.send(LoopEvent::HoverCompleted {
                    request_id,
                    hover_value,
                });
            }
            WorkerRequest::CustomContext {
                request_id,
                layout,
                compiler_bin,
                function_name,
                overlays,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout));
                let response = agent.context(&compiler_bin, &function_name, &overlays);
                let _ = event_tx.send(LoopEvent::CustomContextCompleted {
                    request_id,
                    response,
                });
            }
            WorkerRequest::SignatureHelp {
                request_id,
                layout,
                compiler_bin,
                query_file,
                line,
                column,
                active,
                overlays,
                overlay_full_paths,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout));
                let response = agent.symbol_at(
                    &compiler_bin,
                    &query_file,
                    line,
                    column,
                    &overlays,
                    &overlay_full_paths,
                );
                let help = response
                    .result
                    .as_ref()
                    .and_then(|v| v.get("symbol"))
                    .filter(|s| !s.is_null())
                    .and_then(|symbol| signature_help(symbol, active))
                    .unwrap_or(Value::Null);
                // Same delivery as hover: one value, cancellation-checked.
                let _ = event_tx.send(LoopEvent::HoverCompleted {
                    request_id,
                    hover_value: help,
                });
            }
            WorkerRequest::Definition {
                request_id,
                layout,
                compiler_bin,
                source_file_path,
                query_file,
                line,
                column,
                overlays,
                overlay_full_paths,
                expected_doc_versions,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.symbol_at(
                    &compiler_bin,
                    &query_file,
                    line,
                    column,
                    &overlays,
                    &overlay_full_paths,
                );
                let _ = event_tx.send(LoopEvent::DefinitionCompleted {
                    request_id,
                    layout,
                    source_file_path,
                    expected_doc_versions,
                    response,
                });
            }
            WorkerRequest::DocumentSymbols {
                request_id,
                layout,
                compiler_bin,
                file_path,
                query_file,
                doc_text,
                expected_version,
                overlays,
                overlay_full_paths,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout));
                let response =
                    agent.symbols(&compiler_bin, &query_file, &overlays, &overlay_full_paths);
                let _ = event_tx.send(LoopEvent::DocumentSymbolsCompleted {
                    request_id,
                    file_path,
                    expected_version,
                    doc_text,
                    response,
                });
            }
            WorkerRequest::Synthesis {
                request_id,
                method,
                query_file,
                params,
                layout,
                compiler_bin,
                sources,
                versions,
                payload,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let agent = agents
                    .entry(layout.agent_target.clone())
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.synthesis_query(&compiler_bin, &payload);
                let _ = event_tx.send(LoopEvent::SynthesisCompleted {
                    request_id,
                    method,
                    query_file,
                    params,
                    layout,
                    sources,
                    versions,
                    response,
                });
            }
            WorkerRequest::Formatting {
                request_id,
                layout,
                compiler_bin,
                file_path,
                query_file,
                doc_text,
                overlays,
                expected_doc_versions,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let agent = agents
                    .entry(layout.agent_target.clone())
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.format(&compiler_bin, &query_file, &overlays);
                let _ = event_tx.send(LoopEvent::FormattingCompleted {
                    request_id,
                    layout,
                    file_path,
                    query_file,
                    doc_text,
                    expected_doc_versions,
                    response,
                });
            }
            WorkerRequest::References {
                request_id,
                layout,
                compiler_bin,
                source_file_path,
                query_file,
                line,
                column,
                include_declaration,
                overlays,
                overlay_full_paths,
                expected_doc_versions,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.references(
                    &compiler_bin,
                    &query_file,
                    line,
                    column,
                    include_declaration,
                    &overlays,
                    &overlay_full_paths,
                );
                let _ = event_tx.send(LoopEvent::ReferencesCompleted {
                    request_id,
                    layout,
                    source_file_path,
                    expected_doc_versions,
                    response,
                });
            }
            WorkerRequest::PrepareRename {
                request_id,
                layout,
                compiler_bin,
                source_file_path,
                query_file,
                doc_text,
                line,
                column,
                overlays,
                overlay_full_paths,
                expected_doc_versions,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.prepare_rename(
                    &compiler_bin,
                    &query_file,
                    line,
                    column,
                    &overlays,
                    &overlay_full_paths,
                );
                let _ = event_tx.send(LoopEvent::PrepareRenameCompleted {
                    request_id,
                    layout,
                    source_file_path,
                    doc_text,
                    expected_doc_versions,
                    response,
                });
            }
            WorkerRequest::Rename {
                request_id,
                layout,
                compiler_bin,
                source_file_path,
                query_file,
                line,
                column,
                new_name,
                snapshot_fingerprint,
                overlays,
                overlay_full_paths,
                expected_doc_versions,
            } => {
                if is_req_cancelled(&cancelled_ids, &request_id) {
                    continue;
                }
                let target_key = layout.agent_target.clone();
                let agent = agents
                    .entry(target_key)
                    .or_insert_with(|| ProjectAgent::new(layout.clone()));
                let response = agent.rename(
                    &compiler_bin,
                    &query_file,
                    line,
                    column,
                    &new_name,
                    snapshot_fingerprint.as_deref(),
                    &overlays,
                    &overlay_full_paths,
                );
                let _ = event_tx.send(LoopEvent::RenameCompleted {
                    request_id,
                    layout,
                    source_file_path,
                    expected_doc_versions,
                    response,
                });
            }
            WorkerRequest::MarkReload { project_target } => {
                if let Some(agent) = agents.get_mut(&project_target) {
                    agent.mark_needs_reload();
                }
            }
            WorkerRequest::ForceRestart { project_target } => {
                if let Some(agent) = agents.get_mut(&project_target) {
                    agent.force_restart();
                }
            }
            WorkerRequest::ForceRestartAll => {
                for agent in agents.values_mut() {
                    agent.force_restart();
                }
            }
            WorkerRequest::CloseProject { project_target } => {
                agents.remove(&project_target);
            }
            WorkerRequest::Shutdown => {
                agents.clear();
                break;
            }
        }
    }
}

struct CoordinatorState {
    config: ServerConfig,
    workspace_roots: Vec<PathBuf>,
    open_docs: BTreeMap<PathBuf, OpenDocument>,
    project_generations: BTreeMap<PathBuf, Arc<AtomicU64>>,
    pending_checks: BTreeMap<PathBuf, PendingCheck>,
    published_files_per_project: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    latest_hole_details: BTreeMap<(PathBuf, usize), Value>,
    cancelled_ids: Arc<Mutex<BTreeSet<String>>>,
    responded_cancelled_ids: BTreeSet<String>,
    shutdown_requested: bool,
    worker_tx: Sender<WorkerRequest>,
}

impl CoordinatorState {
    fn new(
        config: ServerConfig,
        worker_tx: Sender<WorkerRequest>,
        cancelled_ids: Arc<Mutex<BTreeSet<String>>>,
    ) -> Self {
        Self {
            config,
            workspace_roots: Vec::new(),
            open_docs: BTreeMap::new(),
            project_generations: BTreeMap::new(),
            pending_checks: BTreeMap::new(),
            published_files_per_project: BTreeMap::new(),
            latest_hole_details: BTreeMap::new(),
            cancelled_ids,
            responded_cancelled_ids: BTreeSet::new(),
            shutdown_requested: false,
            worker_tx,
        }
    }

    fn take_if_cancelled(&mut self, request_id: &Value) -> bool {
        let key = req_id_key(request_id);
        if let Ok(mut set) = self.cancelled_ids.lock() {
            set.remove(&key);
        }
        self.responded_cancelled_ids.remove(&key)
    }

    fn run_loop<W: Write>(&mut self, event_rx: Receiver<LoopEvent>, writer: &mut W) -> i32 {
        loop {
            let next_deadline = self.pending_checks.values().map(|p| p.deadline).min();
            let event = if let Some(deadline) = next_deadline {
                let wait = deadline.saturating_duration_since(Instant::now());
                match event_rx.recv_timeout(wait) {
                    Ok(ev) => Some(ev),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break 0,
                }
            } else {
                match event_rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => break 0,
                }
            };

            if let Some(ev) = event {
                match ev {
                    LoopEvent::InputClosed => {
                        break 0;
                    }
                    LoopEvent::LspMessage(msg) => {
                        if let Some(exit_code) = self.handle_lsp_message(msg, writer) {
                            break exit_code;
                        }
                    }
                    LoopEvent::CheckCompleted {
                        project_target,
                        generation,
                        response,
                        trigger_path,
                        doc_versions,
                    } => {
                        self.handle_check_completed(
                            &project_target,
                            generation,
                            response,
                            &trigger_path,
                            &doc_versions,
                            writer,
                        );
                    }
                    LoopEvent::HoverCompleted {
                        request_id,
                        hover_value,
                    } => {
                        if !self.take_if_cancelled(&request_id) {
                            let resp = jsonrpc_response(&request_id, hover_value);
                            let _ = write_lsp_message(writer, &resp);
                        }
                    }
                    LoopEvent::CustomContextCompleted {
                        request_id,
                        response,
                    } => {
                        if !self.take_if_cancelled(&request_id) {
                            let body = json!({
                                "ok": response.ok,
                                "result": response.result,
                                "diagnostics": response.raw_diagnostics,
                                "restarted": response.restarted,
                                "restart_count": response.restart_count,
                            });
                            let resp = jsonrpc_response(&request_id, body);
                            let _ = write_lsp_message(writer, &resp);
                        }
                    }
                    LoopEvent::DefinitionCompleted {
                        request_id,
                        layout,
                        source_file_path,
                        expected_doc_versions,
                        response,
                    } => {
                        self.handle_definition_completed(
                            request_id,
                            &layout,
                            &source_file_path,
                            &expected_doc_versions,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::DocumentSymbolsCompleted {
                        request_id,
                        file_path,
                        expected_version,
                        doc_text,
                        response,
                    } => {
                        self.handle_document_symbols_completed(
                            request_id,
                            &file_path,
                            expected_version,
                            &doc_text,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::SynthesisCompleted {
                        request_id,
                        method,
                        query_file,
                        params,
                        layout,
                        sources,
                        versions,
                        response,
                    } => {
                        self.handle_synthesis_completed(
                            request_id,
                            &method,
                            &query_file,
                            &params,
                            &layout,
                            &sources,
                            &versions,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::FormattingCompleted {
                        request_id,
                        layout,
                        file_path,
                        query_file,
                        doc_text,
                        expected_doc_versions,
                        response,
                    } => {
                        self.handle_formatting_completed(
                            request_id,
                            &layout,
                            &file_path,
                            &query_file,
                            &doc_text,
                            &expected_doc_versions,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::ReferencesCompleted {
                        request_id,
                        layout,
                        source_file_path,
                        expected_doc_versions,
                        response,
                    } => {
                        self.handle_references_completed(
                            request_id,
                            &layout,
                            &source_file_path,
                            &expected_doc_versions,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::PrepareRenameCompleted {
                        request_id,
                        layout,
                        source_file_path,
                        doc_text,
                        expected_doc_versions,
                        response,
                    } => {
                        self.handle_prepare_rename_completed(
                            request_id,
                            &layout,
                            &source_file_path,
                            &doc_text,
                            &expected_doc_versions,
                            response,
                            writer,
                        );
                    }
                    LoopEvent::RenameCompleted {
                        request_id,
                        layout,
                        source_file_path,
                        expected_doc_versions,
                        response,
                    } => {
                        self.handle_rename_completed(
                            request_id,
                            &layout,
                            &source_file_path,
                            &expected_doc_versions,
                            response,
                            writer,
                        );
                    }
                }
            }

            self.flush_due_checks();
        }
    }

    fn handle_lsp_message<W: Write>(&mut self, msg: Value, writer: &mut W) -> Option<i32> {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match (method, id) {
            (Some("initialize"), Some(req_id)) => {
                self.handle_initialize(&params);
                let result = json!({
                    "capabilities": {
                        "positionEncoding": "utf-16",
                        "textDocumentSync": {
                            "openClose": true,
                            "change": 1, // Full sync advertised; incremental range changes also supported
                            "save": { "includeText": true }
                        },
                        "hoverProvider": true,
                        "signatureHelpProvider": {
                            "triggerCharacters": ["(", ","],
                            "retriggerCharacters": [","]
                        },
                        "completionProvider": {
                            "triggerCharacters": [":"],
                            "resolveProvider": false
                        },
                        "definitionProvider": true,
                        "documentSymbolProvider": true,
                        "documentFormattingProvider": true,
                        "codeActionProvider": {"codeActionKinds":["quickfix.anode.hole"]},
                        "experimental": {"anodeSynthesis":{"holes":"anode/holes","listHoles":"anode/listHoles","skeletonReceipts":true,"verifyHoles":"anode/verifyHoles","verifyContracts":"anode/verifyContracts","verify":"anode/verify","planCampaign":"anode/planCampaign","verifyCampaign":"anode/verifyCampaign","campaignMaxWorkers":16,"readOnly":true,"contractExecution":false,"boundedPureExecution":true,"checkedTestEdits":true,"coordinates":"utf-16","multiHole":true}},
                        "referencesProvider": true,
                        "renameProvider": {
                            "prepareProvider": true
                        },
                        "executeCommandProvider": {
                            "commands": [
                                "anode.restartServer",
                                "anode.functionContext"
                            ]
                        }
                    },
                    "serverInfo": {
                        "name": "anode-lsp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                });
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, result));
            }
            (Some("initialized"), None) => {}
            (Some("$/cancelRequest"), None) => {
                if let Some(cancel_id) = params.get("id").cloned() {
                    let key = req_id_key(&cancel_id);
                    if let Ok(mut set) = self.cancelled_ids.lock() {
                        set.insert(key.clone());
                    }
                    self.responded_cancelled_ids.insert(key);
                    let err = jsonrpc_error(&cancel_id, -32800, "request cancelled", None);
                    let _ = write_lsp_message(writer, &err);
                }
            }
            (Some("shutdown"), Some(req_id)) => {
                self.shutdown_requested = true;
                self.pending_checks.clear();
                let _ = self.worker_tx.send(WorkerRequest::Shutdown);
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            }
            (Some("exit"), None) => {
                return Some(if self.shutdown_requested { 0 } else { 1 });
            }
            (Some(m), Some(req_id)) if self.shutdown_requested && m != "exit" => {
                let err = jsonrpc_error(
                    &req_id,
                    -32600,
                    "server is shutting down (InvalidRequest)",
                    None,
                );
                let _ = write_lsp_message(writer, &err);
            }
            (Some("textDocument/didOpen"), None) => {
                self.handle_did_open(&params);
            }
            (Some("textDocument/didChange"), None) => {
                self.handle_did_change(&params);
            }
            (Some("textDocument/didSave"), None) => {
                self.handle_did_save(&params);
            }
            (Some("textDocument/didClose"), None) => {
                self.handle_did_close(&params, writer);
            }
            (Some("workspace/didChangeWatchedFiles"), None) => {
                self.handle_watched_files(&params);
            }
            (Some("workspace/didChangeConfiguration"), None) => {
                self.handle_change_configuration(&params);
            }
            (Some("textDocument/hover"), Some(req_id)) => {
                self.handle_hover(req_id, &params, writer);
            }
            (Some("textDocument/signatureHelp"), Some(req_id)) => {
                self.handle_signature_help(req_id, &params, writer);
            }
            (Some("textDocument/completion"), Some(req_id)) => {
                self.handle_completion(req_id, &params, writer);
            }
            (Some("textDocument/definition"), Some(req_id)) => {
                self.handle_definition(req_id, &params, writer);
            }
            (Some("textDocument/documentSymbol"), Some(req_id)) => {
                self.handle_document_symbol(req_id, &params, writer);
            }
            (Some("textDocument/formatting"), Some(req_id)) => {
                self.handle_formatting(req_id, &params, writer);
            }
            (
                Some(
                    method @ ("anode/holes"
                    | "anode/listHoles"
                    | "anode/verifyHoles"
                    | "anode/verifyContracts"
                    | "anode/verify"
                    | "anode/planCampaign"
                    | "anode/verifyCampaign"
                    | "textDocument/codeAction"),
                ),
                Some(req_id),
            ) => {
                self.handle_synthesis(req_id, method, &params, writer);
            }
            (Some("textDocument/references"), Some(req_id)) => {
                self.handle_references(req_id, &params, writer);
            }
            (Some("textDocument/prepareRename"), Some(req_id)) => {
                self.handle_prepare_rename(req_id, &params, writer);
            }
            (Some("textDocument/rename"), Some(req_id)) => {
                self.handle_rename(req_id, &params, writer);
            }
            (Some("anode/context"), Some(req_id)) => {
                self.handle_custom_context(req_id, &params, writer);
            }
            (Some("anode/projectStatus"), Some(req_id)) => {
                self.handle_project_status(req_id, &params, writer);
            }
            (Some("workspace/executeCommand"), Some(req_id)) => {
                self.handle_execute_command(req_id, &params, writer);
            }
            (Some(unsupported_method), Some(req_id)) => {
                let (code, message, missing_interface) =
                    describe_missing_compiler_interface(unsupported_method);
                let data = json!({
                    "method": unsupported_method,
                    "missing_compiler_interface": missing_interface,
                    "supported_compiler_methods": ["check", "format", "context", "symbol_at", "symbols", "references", "prepare_rename", "rename"],
                });
                let err = jsonrpc_error(&req_id, code, message, Some(data));
                let _ = write_lsp_message(writer, &err);
            }
            _ => {
                // Unknown notification or response: ignore safely.
            }
        }
        None
    }

    fn handle_initialize(&mut self, params: &Value) {
        if let Some(root_uri) = params.get("rootUri").and_then(Value::as_str) {
            if let Some(p) = uri_to_path(root_uri) {
                self.workspace_roots.push(p);
            }
        } else if let Some(root_path) = params.get("rootPath").and_then(Value::as_str) {
            self.workspace_roots
                .push(normalize_path(Path::new(root_path)));
        }

        if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
            for f in folders {
                if let Some(u) = f.get("uri").and_then(Value::as_str) {
                    if let Some(p) = uri_to_path(u) {
                        if !self.workspace_roots.contains(&p) {
                            self.workspace_roots.push(p);
                        }
                    }
                }
            }
        }

        if let Some(init_opts) = params.get("initializationOptions") {
            self.apply_settings_json(init_opts);
        }
    }

    fn handle_change_configuration(&mut self, params: &Value) {
        if let Some(settings) = params.get("settings") {
            let anode_settings = settings.get("anode").unwrap_or(settings);
            self.apply_settings_json(anode_settings);
        }
    }

    fn apply_settings_json(&mut self, opts: &Value) {
        if let Some(cp) = opts.get("compilerPath").and_then(Value::as_str) {
            let trimmed = cp.trim();
            if !trimmed.is_empty() {
                self.config.explicit_compiler_path = Some(PathBuf::from(trimmed));
            }
        }
        if let Some(ms) = opts.get("debounceMs").and_then(Value::as_u64) {
            self.config.debounce_ms = ms.min(5_000);
        }
    }

    fn handle_did_open(&mut self, params: &Value) {
        let Some(td) = params.get("textDocument") else {
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let version = td.get("version").and_then(Value::as_i64).unwrap_or(1);
        let text = td
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let layout = ProjectLayout::discover(&path);
        let doc = OpenDocument {
            client_uri: uri.to_owned(),
            path: path.clone(),
            version,
            text,
            layout: layout.clone(),
        };
        self.open_docs.insert(path.clone(), doc);

        // Schedule immediate check on `didOpen` (delay 0ms).
        self.schedule_project_check(layout, path, Duration::ZERO);
    }

    fn handle_did_change(&mut self, params: &Value) {
        let Some(td) = params.get("textDocument") else {
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let version = td.get("version").and_then(Value::as_i64).unwrap_or(1);
        let Some(changes) = params.get("contentChanges").and_then(Value::as_array) else {
            return;
        };

        let layout = if let Some(doc) = self.open_docs.get_mut(&path) {
            doc.client_uri = uri.to_owned();
            doc.version = version;
            apply_content_changes(&mut doc.text, changes);
            doc.layout.clone()
        } else {
            let mut text = String::new();
            apply_content_changes(&mut text, changes);
            let layout = ProjectLayout::discover(&path);
            self.open_docs.insert(
                path.clone(),
                OpenDocument {
                    client_uri: uri.to_owned(),
                    path: path.clone(),
                    version,
                    text,
                    layout: layout.clone(),
                },
            );
            layout
        };

        let debounce = Duration::from_millis(self.config.debounce_ms);
        self.schedule_project_check(layout, path, debounce);
    }

    fn handle_did_save(&mut self, params: &Value) {
        let Some(td) = params.get("textDocument") else {
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };

        let is_manifest = path
            .file_name()
            .is_some_and(|n| n == "Anode.toml" || n == "Zenith.toml");

        let layout = if let Some(doc) = self.open_docs.get_mut(&path) {
            if let Some(saved_text) = params.get("text").and_then(Value::as_str) {
                doc.text = saved_text.to_owned();
            }
            doc.layout = ProjectLayout::discover(&path);
            doc.layout.clone()
        } else {
            ProjectLayout::discover(&path)
        };

        if is_manifest {
            let _ = self.worker_tx.send(WorkerRequest::ForceRestart {
                project_target: layout.agent_target.clone(),
            });
            // Update layout for all open docs in this project root.
            for doc in self.open_docs.values_mut() {
                if doc.layout.root == layout.root {
                    doc.layout = ProjectLayout::discover(&doc.path);
                }
            }
        } else {
            let _ = self.worker_tx.send(WorkerRequest::MarkReload {
                project_target: layout.agent_target.clone(),
            });
        }

        self.schedule_project_check(layout, path, Duration::ZERO);
    }

    fn handle_did_close<W: Write>(&mut self, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };

        let Some(removed) = self.open_docs.remove(&path) else {
            return;
        };
        let project_target = removed.layout.agent_target.clone();

        let still_has_open_docs = self
            .open_docs
            .values()
            .any(|d| d.layout.agent_target == project_target);

        if !still_has_open_docs {
            self.pending_checks.remove(&project_target);
            // Clear diagnostics for closed standalone/project files.
            let mut to_clear = self
                .published_files_per_project
                .remove(&project_target)
                .unwrap_or_default();
            to_clear.insert(path);
            for file_path in to_clear {
                let clear_uri = if file_path == removed.path {
                    removed.client_uri.clone()
                } else {
                    path_to_uri(&file_path)
                };
                let notif = jsonrpc_notification(
                    "textDocument/publishDiagnostics",
                    json!({
                        "uri": clear_uri,
                        "diagnostics": [],
                    }),
                );
                let _ = write_lsp_message(writer, &notif);
            }
            let _ = self
                .worker_tx
                .send(WorkerRequest::CloseProject { project_target });
        } else {
            // Another file in the same project is still open; reload from disk so the closed file's
            // unsaved overlay is dropped and the on-disk version is checked.
            let _ = self.worker_tx.send(WorkerRequest::MarkReload {
                project_target: project_target.clone(),
            });
            if let Some(remaining_doc) = self
                .open_docs
                .values()
                .find(|d| d.layout.agent_target == project_target)
                .cloned()
            {
                self.schedule_project_check(
                    remaining_doc.layout,
                    remaining_doc.path,
                    Duration::ZERO,
                );
            }
        }
    }

    fn handle_watched_files(&mut self, params: &Value) {
        let Some(changes) = params.get("changes").and_then(Value::as_array) else {
            return;
        };
        let mut affected_projects = BTreeSet::new();
        for change in changes {
            if let Some(u) = change.get("uri").and_then(Value::as_str) {
                if let Some(path) = uri_to_path(u) {
                    let layout = ProjectLayout::discover(&path);
                    affected_projects.insert(layout.agent_target.clone());
                    let _ = self.worker_tx.send(WorkerRequest::MarkReload {
                        project_target: layout.agent_target,
                    });
                }
            }
        }
        for target in affected_projects {
            if let Some(doc) = self
                .open_docs
                .values()
                .find(|d| d.layout.agent_target == target)
                .cloned()
            {
                self.schedule_project_check(doc.layout, doc.path, Duration::ZERO);
            }
        }
    }

    fn schedule_project_check(
        &mut self,
        layout: ProjectLayout,
        trigger_path: PathBuf,
        delay: Duration,
    ) {
        let target_key = layout.agent_target.clone();
        let gen_counter = self
            .project_generations
            .entry(target_key.clone())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        let new_gen = gen_counter.fetch_add(1, Ordering::SeqCst) + 1;

        self.pending_checks.insert(
            target_key,
            PendingCheck {
                deadline: Instant::now() + delay,
                generation: new_gen,
                layout,
                trigger_path,
            },
        );
    }

    fn flush_due_checks(&mut self) {
        let now = Instant::now();
        let due_keys: Vec<PathBuf> = self
            .pending_checks
            .iter()
            .filter(|&(_, v)| v.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in due_keys {
            let Some(pending) = self.pending_checks.remove(&key) else {
                continue;
            };
            let Some(gen_arc) = self.project_generations.get(&key).cloned() else {
                continue;
            };
            if pending.generation < gen_arc.load(Ordering::SeqCst) {
                continue;
            }

            let (overlays, overlay_full_paths, doc_versions) =
                self.collect_project_overlays(&pending.layout);
            let compiler_bin = resolve_compiler_path(
                self.config.explicit_compiler_path.as_deref(),
                &self.workspace_roots,
                Some(&pending.trigger_path),
            );

            let _ = self.worker_tx.send(WorkerRequest::Check {
                layout: pending.layout,
                compiler_bin,
                generation: pending.generation,
                latest_gen: gen_arc,
                overlays,
                overlay_full_paths,
                trigger_path: pending.trigger_path,
                doc_versions,
            });
        }
    }

    fn collect_project_overlays(
        &self,
        layout: &ProjectLayout,
    ) -> (
        BTreeMap<String, String>,
        BTreeMap<String, PathBuf>,
        BTreeMap<PathBuf, i64>,
    ) {
        let mut overlays = BTreeMap::new();
        let mut full_paths = BTreeMap::new();
        let mut versions = BTreeMap::new();

        for doc in self.open_docs.values() {
            if doc.layout.agent_target != layout.agent_target
                && !layout
                    .overlay_keys(&doc.path)
                    .iter()
                    .any(|key| key.starts_with("@dependencies/"))
            {
                continue;
            }
            versions.insert(doc.path.clone(), doc.version);
            let is_source = doc.path.extension().is_some_and(|e| e == "an" || e == "zn");
            if !is_source {
                continue;
            }
            for rel_key in layout.overlay_keys(&doc.path) {
                overlays.insert(rel_key.clone(), doc.text.clone());
                full_paths.insert(rel_key, doc.path.clone());
            }
        }

        (overlays, full_paths, versions)
    }

    fn handle_check_completed<W: Write>(
        &mut self,
        project_target: &Path,
        generation: u64,
        response: CheckResponse,
        trigger_path: &Path,
        doc_versions: &BTreeMap<PathBuf, i64>,
        writer: &mut W,
    ) {
        // Verify generation is still the latest for this project.
        if let Some(gen_arc) = self.project_generations.get(project_target) {
            let latest = gen_arc.load(Ordering::SeqCst);
            if generation < latest {
                eprintln!(
                    "[anode-lsp] coordinator discarded stale check response for {} (gen {} < latest {})",
                    project_target.display(),
                    generation,
                    latest
                );
                return;
            }
        }

        // Clear cached hole details for files in this project before repopulating.
        self.latest_hole_details.retain(|(file_path, _), _| {
            ProjectLayout::discover(file_path).agent_target != *project_target
        });

        let mut diags_by_file: BTreeMap<PathBuf, Vec<Value>> = BTreeMap::new();

        for diag in &response.diagnostics {
            let target_file = diag
                .file
                .as_deref()
                .map(|f| normalize_path(Path::new(f)))
                .unwrap_or_else(|| normalize_path(trigger_path));

            if diag.is_hole {
                if let Some(details) = &diag.details {
                    self.latest_hole_details
                        .insert((target_file.clone(), diag.line), details.clone());
                }
            }

            let source_text = self
                .open_docs
                .get(&target_file)
                .map(|d| d.text.clone())
                .or_else(|| fs::read_to_string(&target_file).ok())
                .unwrap_or_default();

            let lsp_diag = to_lsp_diagnostic(diag, &source_text);
            diags_by_file.entry(target_file).or_default().push(lsp_diag);
        }

        let mut files_to_publish: BTreeSet<PathBuf> = BTreeSet::new();
        for doc in self.open_docs.values() {
            if doc.layout.agent_target == *project_target {
                files_to_publish.insert(doc.path.clone());
            }
        }
        if let Some(prev_files) = self.published_files_per_project.get(project_target) {
            files_to_publish.extend(prev_files.iter().cloned());
        }
        files_to_publish.extend(diags_by_file.keys().cloned());

        let mut new_published_set = BTreeSet::new();
        for file_path in files_to_publish {
            let diags = diags_by_file.remove(&file_path).unwrap_or_default();
            if !diags.is_empty() {
                new_published_set.insert(file_path.clone());
            }

            let uri = self
                .open_docs
                .get(&file_path)
                .map(|d| d.client_uri.clone())
                .unwrap_or_else(|| path_to_uri(&file_path));

            let mut params = json!({
                "uri": uri,
                "diagnostics": diags,
            });
            if let Some(ver) = doc_versions
                .get(&file_path)
                .or_else(|| self.open_docs.get(&file_path).map(|d| &d.version))
            {
                params["version"] = json!(ver);
            }

            let notif = jsonrpc_notification("textDocument/publishDiagnostics", params);
            let _ = write_lsp_message(writer, &notif);
        }

        self.published_files_per_project
            .insert(project_target.to_path_buf(), new_published_set);
    }

    fn handle_hover<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(pos_val) = params.get("position") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(position) = Position::from_json(pos_val) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };

        let (text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let Some(target) = extract_hover_target(&text, position) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(query_file) = layout.overlay_key(&file_path) else {
            return;
        };
        let Some((line, column)) = lsp_pos_to_compiler_coords(&text, position) else {
            return;
        };

        let (overlays, _, _) = self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        match target {
            HoverTarget::TypedHole {
                range,
                one_based_line,
                enclosing_function,
            } => {
                let cached = self
                    .latest_hole_details
                    .get(&(file_path.clone(), one_based_line))
                    .cloned();
                let candidates = enclosing_function
                    .map(|f| layout.candidate_function_names(&file_path, &f))
                    .unwrap_or_else(|| vec!["main".to_owned()]);
                let _ = self.worker_tx.send(WorkerRequest::HoverContext {
                    request_id: req_id,
                    layout,
                    compiler_bin,
                    candidates,
                    overlays,
                    range,
                    cached_hole_details: cached,
                    is_direct_hole: true,
                    query_file,
                    line,
                    column,
                });
            }
            HoverTarget::Function {
                symbol,
                range,
                one_based_line,
            } => {
                let candidates = layout.candidate_function_names(&file_path, &symbol);
                let cached = self
                    .latest_hole_details
                    .get(&(file_path, one_based_line))
                    .cloned();
                let _ = self.worker_tx.send(WorkerRequest::HoverContext {
                    request_id: req_id,
                    layout,
                    compiler_bin,
                    candidates,
                    overlays,
                    range,
                    cached_hole_details: cached,
                    is_direct_hole: false,
                    query_file,
                    line,
                    column,
                });
            }
        }
    }

    /// The open (or on-disk) text and layout of the document a position
    /// request names, plus the request position; `None` for malformed or
    /// unreadable requests.
    fn request_document(
        &self,
        params: &Value,
    ) -> Option<(PathBuf, String, ProjectLayout, Position)> {
        let uri = params.get("textDocument")?.get("uri")?.as_str()?;
        let position = Position::from_json(params.get("position")?)?;
        let file_path = uri_to_path(uri)?;
        let (text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            (doc.text.clone(), doc.layout.clone())
        } else {
            (
                fs::read_to_string(&file_path).ok()?,
                ProjectLayout::discover(&file_path),
            )
        };
        Some((file_path, text, layout, position))
    }

    /// 1.2 (contract N9): signature help for the innermost open call. The
    /// callee is resolved by the compiler's `symbol_at`; its detail gives
    /// the signature and documentation.
    fn handle_signature_help<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some((file_path, text, layout, position)) = self.request_document(params) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let cursor = position_to_byte_offset(&text, position);
        let Some((callee_start, _, active)) = call_context(&text, cursor) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let callee = byte_offset_to_position(&text, callee_start);
        let Some((line, column)) = lsp_pos_to_compiler_coords(&text, callee) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, _) = self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );
        let _ = self.worker_tx.send(WorkerRequest::SignatureHelp {
            request_id: req_id,
            layout,
            compiler_bin,
            query_file,
            line,
            column,
            active,
            overlays,
            overlay_full_paths,
        });
    }

    /// 1.2 (contract N9): prelude completion (builtins, standard result
    /// types and their variants). Visible bindings and fields need a
    /// compiler `complete` method that agent protocol v1 does not have.
    fn handle_completion<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let result = self
            .request_document(params)
            .map(|(_, text, _, position)| {
                prelude_completions(&text, position_to_byte_offset(&text, position))
            })
            .unwrap_or(Value::Null);
        let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, result));
    }

    fn handle_custom_context<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            let err = jsonrpc_error(&req_id, -32602, "anode/context requires 'uri'", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let err = jsonrpc_error(&req_id, -32602, "invalid file:// URI", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };

        let (text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            (doc.text.clone(), doc.layout.clone())
        } else {
            let disk_text = fs::read_to_string(&file_path).unwrap_or_default();
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let function_name = if let Some(f) = params.get("function").and_then(Value::as_str) {
            f.to_owned()
        } else if let Some(pos_val) = params.get("position") {
            let Some(pos) = Position::from_json(pos_val) else {
                let err = jsonrpc_error(&req_id, -32602, "invalid position", None);
                let _ = write_lsp_message(writer, &err);
                return;
            };
            if let Some(HoverTarget::Function { symbol, .. }) = extract_hover_target(&text, pos) {
                layout
                    .candidate_function_names(&file_path, &symbol)
                    .into_iter()
                    .next()
                    .unwrap_or(symbol)
            } else {
                let lines = split_lines(&text);
                if let Some(enclosing) = find_enclosing_function_name(&lines, pos.line as usize) {
                    layout
                        .candidate_function_names(&file_path, &enclosing)
                        .into_iter()
                        .next()
                        .unwrap_or(enclosing)
                } else {
                    let err = jsonrpc_error(
                        &req_id,
                        -32602,
                        "no function found at the requested position",
                        None,
                    );
                    let _ = write_lsp_message(writer, &err);
                    return;
                }
            }
        } else {
            let err = jsonrpc_error(
                &req_id,
                -32602,
                "anode/context requires either 'function' or 'position'",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        };

        let (overlays, _, _) = self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::CustomContext {
            request_id: req_id,
            layout,
            compiler_bin,
            function_name,
            overlays,
        });
    }

    fn handle_project_status<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            let err = jsonrpc_error(&req_id, -32602, "anode/projectStatus requires 'uri'", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let err = jsonrpc_error(&req_id, -32602, "invalid file:// URI", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let layout = self
            .open_docs
            .get(&file_path)
            .map(|d| d.layout.clone())
            .unwrap_or_else(|| ProjectLayout::discover(&file_path));

        let resp = jsonrpc_response(
            &req_id,
            json!({
                "root": layout.root.to_string_lossy(),
                "agentTarget": layout.agent_target.to_string_lossy(),
                "entryFile": layout.entry_file.to_string_lossy(),
                "managed": layout.managed,
                "module": layout.module_path_for_file(&file_path),
                "overlayKey": layout.overlay_key(&file_path),
            }),
        );
        let _ = write_lsp_message(writer, &resp);
    }

    fn handle_execute_command<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let command = params.get("command").and_then(Value::as_str).unwrap_or("");
        match command {
            "anode.restartServer" => {
                let _ = self.worker_tx.send(WorkerRequest::ForceRestartAll);
                let docs: Vec<(ProjectLayout, PathBuf)> = self
                    .open_docs
                    .values()
                    .map(|d| (d.layout.clone(), d.path.clone()))
                    .collect();
                for (layout, path) in docs {
                    self.schedule_project_check(layout, path, Duration::ZERO);
                }
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_response(&req_id, json!({"restarted": true})),
                );
            }
            "anode.functionContext" => {
                let args = params
                    .get("arguments")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(Value::Null);
                self.handle_custom_context(req_id, &args, writer);
            }
            other => {
                let err =
                    jsonrpc_error(&req_id, -32601, format!("unknown command '{other}'"), None);
                let _ = write_lsp_message(writer, &err);
            }
        }
    }

    fn handle_definition<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(pos_val) = params.get("position") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(position) = Position::from_json(pos_val) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };

        let req_version = td.get("version").and_then(Value::as_i64);
        let (text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            if let Some(v) = req_version {
                if v != doc.version {
                    let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
                    return;
                }
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let Some((line, column)) = lsp_pos_to_compiler_coords(&text, position) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };

        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, expected_doc_versions) =
            self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::Definition {
            request_id: req_id,
            layout,
            compiler_bin,
            source_file_path: file_path,
            query_file,
            line,
            column,
            overlays,
            overlay_full_paths,
            expected_doc_versions,
        });
    }

    fn handle_definition_completed<W: Write>(
        &mut self,
        request_id: Value,
        layout: &ProjectLayout,
        source_file_path: &Path,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }

        // Verify that no open document in this project changed version while the request was in flight.
        if self.is_project_snapshot_stale(layout, expected_doc_versions) {
            eprintln!(
                "[anode-lsp] discarding stale definition response for {} due to document version advancement",
                source_file_path.display()
            );
            let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, Value::Null));
            return;
        }

        let location = if response.ok {
            response.result.as_ref().and_then(|res_val| {
                convert_symbol_at_to_location(
                    res_val,
                    source_file_path,
                    &layout.root,
                    |target_path| {
                        if let Some(doc) = self.open_docs.get(target_path) {
                            (doc.client_uri.clone(), doc.text.clone())
                        } else {
                            let disk = fs::read_to_string(target_path).unwrap_or_default();
                            (path_to_uri(target_path), disk)
                        }
                    },
                )
            })
        } else {
            None
        };

        let _ = write_lsp_message(
            writer,
            &jsonrpc_response(&request_id, location.unwrap_or(Value::Null)),
        );
    }

    fn handle_document_symbol<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };

        let req_version = td.get("version").and_then(Value::as_i64);
        let (doc_text, layout, expected_version) = if let Some(doc) = self.open_docs.get(&file_path)
        {
            if let Some(v) = req_version {
                if v != doc.version {
                    let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
                    return;
                }
            }
            (doc.text.clone(), doc.layout.clone(), Some(doc.version))
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path), None)
        };

        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, _) = self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::DocumentSymbols {
            request_id: req_id,
            layout,
            compiler_bin,
            file_path,
            query_file,
            doc_text,
            expected_version,
            overlays,
            overlay_full_paths,
        });
    }

    fn handle_document_symbols_completed<W: Write>(
        &mut self,
        request_id: Value,
        file_path: &Path,
        expected_version: Option<i64>,
        doc_text: &str,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }

        let current_version = self.open_docs.get(file_path).map(|d| d.version);
        if expected_version.is_some() && current_version != expected_version {
            eprintln!(
                "[anode-lsp] discarding stale documentSymbol response for {} (expected {:?}, current {:?})",
                file_path.display(),
                expected_version,
                current_version
            );
            let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, json!([])));
            return;
        }

        let symbols_json = if response.ok {
            response
                .result
                .as_ref()
                .and_then(|res| res.get("symbols"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| convert_document_symbol(s, doc_text))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, json!(symbols_json)));
    }

    fn handle_synthesis<W: Write>(
        &mut self,
        req_id: Value,
        method: &str,
        params: &Value,
        writer: &mut W,
    ) {
        if matches!(
            method,
            "anode/verify" | "anode/planCampaign" | "anode/verifyCampaign"
        ) && params.get("patches").is_some()
        {
            let _=write_lsp_message(writer,&jsonrpc_error(&req_id,-32602,format!("{method} reads the active document overlay; speculative patches use the agent protocol"),None));
            return;
        }
        if params.get("apply") == Some(&Value::Bool(true)) {
            let _=write_lsp_message(writer,&jsonrpc_error(&req_id,-32602,"editor synthesis queries are read-only; guarded disk commits use anode verify --batch or --campaign",None));
            return;
        }
        let td = params.get("textDocument").unwrap_or(params);
        let Some(path) = td.get("uri").and_then(Value::as_str).and_then(uri_to_path) else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(&req_id, -32602, "synthesis query requires a file URI", None),
            );
            return;
        };
        if let Some(expected) = params.get("document_versions") {
            let Some(expected) = expected.as_object() else {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(
                        &req_id,
                        -32602,
                        "document_versions must be a URI-to-version object",
                        None,
                    ),
                );
                return;
            };
            if expected.iter().any(|(uri, version)| {
                version.as_i64().is_none()
                    || uri_to_path(uri)
                        .and_then(|p| self.open_docs.get(&p))
                        .map(|d| d.version)
                        != version.as_i64()
            }) {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(
                        &req_id,
                        -32801,
                        "project document versions changed before synthesis query",
                        None,
                    ),
                );
                return;
            }
        }
        if (method == "anode/verifyContracts" && params.get("execute") == Some(&Value::Bool(true)))
            || (method != "anode/verifyCampaign"
                && params
                    .get("verify_contracts")
                    .and_then(|v| v.get("execute"))
                    == Some(&Value::Bool(true)))
        {
            let _=write_lsp_message(writer,&jsonrpc_error(&req_id,-32602,"editor contract queries are static; use anode verify --execute for bounded execution",None));
            return;
        }
        let (text, layout) = if let Some(doc) = self.open_docs.get(&path) {
            if td
                .get("version")
                .and_then(Value::as_i64)
                .is_some_and(|v| v != doc.version)
            {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(
                        &req_id,
                        -32801,
                        "document changed before synthesis query",
                        None,
                    ),
                );
                return;
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(text) = fs::read_to_string(&path) else {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(&req_id, -32602, "cannot read synthesis document", None),
                );
                return;
            };
            (text, ProjectLayout::discover(&path))
        };
        let Some(query_file) = layout.overlay_key(&path) else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(&req_id, -32602, "document is outside the project", None),
            );
            return;
        };
        let (mut overlays, _, versions) = self.collect_project_overlays(&layout);
        overlays.insert(query_file.clone(), text.clone());
        let mut sources = BTreeMap::new();
        for file in layout.snapshot_files() {
            if let Ok(source) = fs::read_to_string(&file) {
                sources.insert(normalize_path(&file), source);
            }
        }
        for doc in self.open_docs.values().filter(|doc| {
            doc.layout.agent_target == layout.agent_target
                || layout
                    .overlay_keys(&doc.path)
                    .iter()
                    .any(|key| key.starts_with("@dependencies/"))
        }) {
            sources.insert(doc.path.clone(), doc.text.clone());
        }
        sources.insert(path.clone(), text);
        let mut payload = params.clone();
        payload["method"] = json!(match method {
            "anode/verifyHoles" if params.get("skeleton_sha256").is_some() => "verify_batch",
            "anode/verifyHoles" => "verify_holes",
            "anode/verifyContracts" => "verify_contracts",
            "anode/verify" => "verify_contracts",
            "anode/planCampaign" => "plan_campaign",
            "anode/verifyCampaign" => "verify_campaign",
            _ => "list_holes",
        });
        payload["overlays"] = json!(overlays);
        payload["execute"] = if method == "anode/verify" {
            params.get("execute").cloned().unwrap_or(json!(true))
        } else {
            json!(false)
        };
        payload["file"] = json!(query_file);
        if method == "anode/verify" && payload.get("functions").is_none() {
            if let Some(function) = params.get("function").and_then(Value::as_str) {
                payload["functions"] = json!([function]);
            } else if let Some(position) = params.get("position").and_then(Position::from_json) {
                let Some((_, source)) = sources.iter().find(|(p, _)| **p == path) else {
                    return;
                };
                let lines = split_lines(source);
                let Some(function) = find_enclosing_function_name(&lines, position.line as usize)
                else {
                    let _ = write_lsp_message(
                        writer,
                        &jsonrpc_error(
                            &req_id,
                            -32602,
                            "verification position is not inside a function",
                            None,
                        ),
                    );
                    return;
                };
                let candidates = layout.candidate_function_names(&path, &function);
                payload["functions"] = json!([candidates.first().unwrap_or(&function)]);
            }
        }
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&path),
        );
        let _ = self.worker_tx.send(WorkerRequest::Synthesis {
            request_id: req_id,
            method: method.into(),
            query_file,
            params: params.clone(),
            layout,
            compiler_bin,
            sources,
            versions,
            payload,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_synthesis_completed<W: Write>(
        &mut self,
        request_id: Value,
        method: &str,
        query_file: &str,
        params: &Value,
        layout: &ProjectLayout,
        sources: &BTreeMap<PathBuf, String>,
        versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }
        let changed = self.is_project_snapshot_stale(layout, versions)
            || sources.iter().any(|(path, text)| {
                self.open_docs
                    .get(path)
                    .map(|d| d.text.clone())
                    .or_else(|| fs::read_to_string(path).ok())
                    .as_ref()
                    != Some(text)
            });
        if changed {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(
                    &request_id,
                    -32801,
                    "project changed during synthesis query",
                    None,
                ),
            );
            return;
        }
        if !response.ok {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(
                    &request_id,
                    -32803,
                    "compiler rejected synthesis query",
                    Some(json!({"diagnostics":response.raw_diagnostics})),
                ),
            );
            return;
        }
        let Some(mut result) = response.result else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(
                    &request_id,
                    -32803,
                    "compiler synthesis result is missing",
                    None,
                ),
            );
            return;
        };
        let source_for = |file: &str| {
            sources
                .iter()
                .find(|(path, _)| layout.overlay_keys(path).iter().any(|key| key == file))
        };
        if let Some(holes) = result.get_mut("holes").and_then(Value::as_array_mut) {
            for hole in holes {
                if let Some((path, source)) = hole
                    .get("file")
                    .and_then(Value::as_str)
                    .and_then(source_for)
                {
                    hole["uri"] = json!(path_to_uri(path));
                    hole["document_version"] = json!(versions.get(path));
                    if let Some(range) = hole
                        .get("range")
                        .and_then(|r| crate::navigation::compiler_range_to_lsp_range(source, r))
                    {
                        hole["lsp_range"] = range.to_json();
                    }
                }
            }
        }
        if method == "anode/planCampaign" {
            // Sidecar editor coordinates must not change the pinned worker
            // briefs returned to verify_campaign for exact reconstruction.
            let mut editor_holes = Vec::new();
            for mut hole in result
                .get("workers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|w| {
                    w.pointer("/brief/holes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .cloned()
            {
                if let Some((path, source)) = hole
                    .get("file")
                    .and_then(Value::as_str)
                    .and_then(source_for)
                {
                    hole["uri"] = json!(path_to_uri(path));
                    hole["document_version"] = json!(versions.get(path));
                    if let Some(range) = hole
                        .get("range")
                        .and_then(|r| crate::navigation::compiler_range_to_lsp_range(source, r))
                    {
                        hole["lsp_range"] = range.to_json();
                    }
                }
                editor_holes.push(hole);
            }
            result["editor_holes"] = json!(editor_holes);
        }
        result["document_versions"] = json!(versions
            .iter()
            .map(|(path, version)| (path_to_uri(path), version))
            .collect::<BTreeMap<_, _>>());
        if method == "anode/verify" {
            crate::verification::decorate_verification(&mut result, layout, sources, versions);
        }
        if method == "textDocument/codeAction" {
            let wanted = params.get("range").and_then(Range::from_json);
            let actions=result.get("holes").and_then(Value::as_array).into_iter().flatten().filter(|h|h["file"]==query_file&&!h["in_contract"].as_bool().unwrap_or(false)).filter(|h|match(wanted,h.get("lsp_range").and_then(Range::from_json)){(Some(w),Some(h))=>(h.start.line,h.start.character)<=(w.end.line,w.end.character)&&(w.start.line,w.start.character)<=(h.end.line,h.end.character),_=>true}).map(|hole|json!({"title":format!("Propose checked expression for {}",hole["id"].as_str().unwrap_or("hole")),"kind":"quickfix.anode.hole","command":{"title":"Propose checked expression","command":"anode.proposeHole","arguments":[{"hole":hole,"skeleton_sha256":result["skeleton_sha256"],"snapshot_sha256":result["snapshot_sha256"],"snapshot_fingerprint":result["snapshot_fingerprint"],"file_fingerprints":result["file_fingerprints"],"document_versions":result["document_versions"]}]}})).collect::<Vec<_>>();
            let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, json!(actions)));
            return;
        }
        if matches!(method, "anode/verifyHoles" | "anode/verifyCampaign")
            && matches!(
                result["composed"]["status"].as_str(),
                Some("valid" | "conditional")
            )
        {
            let changes=result.get("files").and_then(Value::as_array).into_iter().flatten().filter(|file|method!="anode/verifyCampaign"||!file["file"].as_str().is_some_and(|key|key.starts_with("@dependencies/"))).filter_map(|file|{
                let(path,source)=source_for(file.get("file")?.as_str()?)?;
                let lines=split_lines(source);let end=Position::new((lines.len()-1)as u32,lines.last()?.encode_utf16().count()as u32);
                Some(json!({"textDocument":{"uri":path_to_uri(path),"version":versions.get(path)},"edits":[{"range":Range::new(Position::new(0,0),end).to_json(),"newText":file.get("text")?}]}))
            }).collect::<Vec<_>>();
            result["workspace_edit"] = json!({"documentChanges":changes});
        }
        let _ = write_lsp_message(
            writer,
            &jsonrpc_response(
                &request_id,
                json!({"ok":true,"result":result,"diagnostics":response.raw_diagnostics,"restarted":response.restarted,"restart_count":response.restart_count}),
            ),
        );
    }

    fn handle_formatting<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(&req_id, -32602, "formatting requires textDocument", None),
            );
            return;
        };
        let Some(path) = td.get("uri").and_then(Value::as_str).and_then(uri_to_path) else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(&req_id, -32602, "formatting requires a file URI", None),
            );
            return;
        };
        let (doc_text, layout) = if let Some(doc) = self.open_docs.get(&path) {
            if td
                .get("version")
                .and_then(Value::as_i64)
                .is_some_and(|v| v != doc.version)
            {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(&req_id, -32801, "document changed before formatting", None),
                );
                return;
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(text) = fs::read_to_string(&path) else {
                let _ = write_lsp_message(
                    writer,
                    &jsonrpc_error(&req_id, -32602, "cannot read formatting document", None),
                );
                return;
            };
            (text, ProjectLayout::discover(&path))
        };
        let Some(query_file) = layout.overlay_key(&path) else {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(&req_id, -32602, "document is outside the project", None),
            );
            return;
        };
        let (mut overlays, _, expected_doc_versions) = self.collect_project_overlays(&layout);
        // Pin even closed files to the exact text used to compute the edit range.
        overlays.insert(query_file.clone(), doc_text.clone());
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&path),
        );
        let _ = self.worker_tx.send(WorkerRequest::Formatting {
            request_id: req_id,
            layout,
            compiler_bin,
            file_path: path,
            query_file,
            doc_text,
            overlays,
            expected_doc_versions,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_formatting_completed<W: Write>(
        &mut self,
        request_id: Value,
        layout: &ProjectLayout,
        file_path: &Path,
        query_file: &str,
        doc_text: &str,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }
        let current_text = self
            .open_docs
            .get(file_path)
            .map(|d| d.text.clone())
            .or_else(|| fs::read_to_string(file_path).ok());
        if self.is_project_snapshot_stale(layout, expected_doc_versions)
            || current_text.as_deref() != Some(doc_text)
        {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(
                    &request_id,
                    -32801,
                    "document changed during formatting",
                    None,
                ),
            );
            return;
        }
        if !response.ok {
            let _ = write_lsp_message(
                writer,
                &jsonrpc_error(
                    &request_id,
                    -32803,
                    "compiler could not format the document",
                    Some(json!({"diagnostics":response.raw_diagnostics})),
                ),
            );
            return;
        }
        match response
            .result
            .as_ref()
            .ok_or_else(|| "compiler format response is missing result".to_owned())
            .and_then(|result| formatting_edits(result, query_file, doc_text))
        {
            Ok(edits) => {
                let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, edits));
            }
            Err(message) => {
                let _ =
                    write_lsp_message(writer, &jsonrpc_error(&request_id, -32801, message, None));
            }
        }
    }

    fn handle_references<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(pos_val) = params.get("position") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(position) = Position::from_json(pos_val) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };

        let include_declaration = params
            .get("context")
            .and_then(|c| c.get("includeDeclaration"))
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let req_version = td.get("version").and_then(Value::as_i64);
        let (text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            if let Some(v) = req_version {
                if v != doc.version {
                    let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
                    return;
                }
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let Some((line, column)) = lsp_pos_to_compiler_coords(&text, position) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, json!([])));
            return;
        };

        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, expected_doc_versions) =
            self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::References {
            request_id: req_id,
            layout,
            compiler_bin,
            source_file_path: file_path,
            query_file,
            line,
            column,
            include_declaration,
            overlays,
            overlay_full_paths,
            expected_doc_versions,
        });
    }

    fn handle_references_completed<W: Write>(
        &mut self,
        request_id: Value,
        layout: &ProjectLayout,
        source_file_path: &Path,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }

        if self.is_project_snapshot_stale(layout, expected_doc_versions) {
            eprintln!(
                "[anode-lsp] discarding stale references response for {} due to document version advancement",
                source_file_path.display()
            );
            let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, json!([])));
            return;
        }

        let locations = if response.ok {
            response
                .result
                .as_ref()
                .map(|res_val| {
                    convert_references_to_locations(
                        res_val,
                        source_file_path,
                        &layout.root,
                        |target_path| {
                            if let Some(doc) = self.open_docs.get(target_path) {
                                (doc.client_uri.clone(), doc.text.clone())
                            } else {
                                let disk = fs::read_to_string(target_path).unwrap_or_default();
                                (path_to_uri(target_path), disk)
                            }
                        },
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, json!(locations)));
    }

    fn handle_prepare_rename<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(pos_val) = params.get("position") else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(position) = Position::from_json(pos_val) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };

        let req_version = td.get("version").and_then(Value::as_i64);
        let (doc_text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            if let Some(v) = req_version {
                if v != doc.version {
                    let err = jsonrpc_error(
                        &req_id,
                        -32801,
                        "stale document version: source changed before prepareRename",
                        None,
                    );
                    let _ = write_lsp_message(writer, &err);
                    return;
                }
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let Some((line, column)) = lsp_pos_to_compiler_coords(&doc_text, position) else {
            let _ = write_lsp_message(writer, &jsonrpc_response(&req_id, Value::Null));
            return;
        };

        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, expected_doc_versions) =
            self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::PrepareRename {
            request_id: req_id,
            layout,
            compiler_bin,
            source_file_path: file_path,
            query_file,
            doc_text,
            line,
            column,
            overlays,
            overlay_full_paths,
            expected_doc_versions,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_prepare_rename_completed<W: Write>(
        &mut self,
        request_id: Value,
        layout: &ProjectLayout,
        source_file_path: &Path,
        doc_text: &str,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }

        if self.is_project_snapshot_stale(layout, expected_doc_versions) {
            eprintln!(
                "[anode-lsp] discarding stale prepareRename response for {} due to document version advancement",
                source_file_path.display()
            );
            let err = jsonrpc_error(
                &request_id,
                -32801,
                "stale document version: source changed during prepareRename",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        }

        if !response.ok {
            let msg = response
                .raw_diagnostics
                .first()
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("rename is not available at this position");
            let err = jsonrpc_error(
                &request_id,
                -32602,
                msg,
                Some(json!({ "diagnostics": response.raw_diagnostics })),
            );
            let _ = write_lsp_message(writer, &err);
            return;
        }

        let prep = response
            .result
            .as_ref()
            .and_then(|res_val| convert_prepare_rename_result(res_val, doc_text))
            .unwrap_or(Value::Null);

        let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, prep));
    }

    fn handle_rename<W: Write>(&mut self, req_id: Value, params: &Value, writer: &mut W) {
        let Some(td) = params.get("textDocument") else {
            let err = jsonrpc_error(
                &req_id,
                -32602,
                "textDocument/rename requires 'textDocument'",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(uri) = td.get("uri").and_then(Value::as_str) else {
            let err = jsonrpc_error(&req_id, -32602, "textDocument/rename requires 'uri'", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(pos_val) = params.get("position") else {
            let err = jsonrpc_error(
                &req_id,
                -32602,
                "textDocument/rename requires 'position'",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(position) = Position::from_json(pos_val) else {
            let err = jsonrpc_error(&req_id, -32602, "invalid position", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(new_name) = params.get("newName").and_then(Value::as_str) else {
            let err = jsonrpc_error(
                &req_id,
                -32602,
                "textDocument/rename requires 'newName'",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        };
        let Some(file_path) = uri_to_path(uri) else {
            let err = jsonrpc_error(&req_id, -32602, "invalid file:// URI", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };

        let snapshot_fingerprint = params
            .get("snapshotFingerprint")
            .or_else(|| params.get("snapshot_fingerprint"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let req_version = td.get("version").and_then(Value::as_i64);
        let (doc_text, layout) = if let Some(doc) = self.open_docs.get(&file_path) {
            if let Some(v) = req_version {
                if v != doc.version {
                    let err = jsonrpc_error(
                        &req_id,
                        -32801,
                        "stale document version: source changed before rename planning",
                        None,
                    );
                    let _ = write_lsp_message(writer, &err);
                    return;
                }
            }
            (doc.text.clone(), doc.layout.clone())
        } else {
            let Ok(disk_text) = fs::read_to_string(&file_path) else {
                let err = jsonrpc_error(&req_id, -32602, "unable to read target source file", None);
                let _ = write_lsp_message(writer, &err);
                return;
            };
            (disk_text, ProjectLayout::discover(&file_path))
        };

        let Some((line, column)) = lsp_pos_to_compiler_coords(&doc_text, position) else {
            let err = jsonrpc_error(&req_id, -32602, "position out of bounds", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };

        let query_file = layout
            .overlay_key(&file_path)
            .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));
        let (overlays, overlay_full_paths, expected_doc_versions) =
            self.collect_project_overlays(&layout);
        let compiler_bin = resolve_compiler_path(
            self.config.explicit_compiler_path.as_deref(),
            &self.workspace_roots,
            Some(&file_path),
        );

        let _ = self.worker_tx.send(WorkerRequest::Rename {
            request_id: req_id,
            layout,
            compiler_bin,
            source_file_path: file_path,
            query_file,
            line,
            column,
            new_name: new_name.to_owned(),
            snapshot_fingerprint,
            overlays,
            overlay_full_paths,
            expected_doc_versions,
        });
    }

    fn handle_rename_completed<W: Write>(
        &mut self,
        request_id: Value,
        layout: &ProjectLayout,
        source_file_path: &Path,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
        response: ContextResponse,
        writer: &mut W,
    ) {
        if self.take_if_cancelled(&request_id) {
            return;
        }

        if self.is_project_snapshot_stale(layout, expected_doc_versions) {
            eprintln!(
                "[anode-lsp] discarding stale rename response for {} due to document version advancement",
                source_file_path.display()
            );
            let err = jsonrpc_error(
                &request_id,
                -32801,
                "stale document version: source changed during rename planning",
                None,
            );
            let _ = write_lsp_message(writer, &err);
            return;
        }

        if !response.ok {
            let msg = response
                .raw_diagnostics
                .first()
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("rename rejected by compiler");
            let err = jsonrpc_error(
                &request_id,
                -32602,
                msg,
                Some(json!({ "diagnostics": response.raw_diagnostics })),
            );
            let _ = write_lsp_message(writer, &err);
            return;
        }

        let Some(res_val) = response.result.as_ref() else {
            let err = jsonrpc_error(&request_id, -32603, "missing compiler rename result", None);
            let _ = write_lsp_message(writer, &err);
            return;
        };

        if let Some(warning) = res_val.get("scope_warning").and_then(Value::as_str) {
            eprintln!("[anode-lsp] rename scope notice: {warning}");
        }

        let workspace_edit = convert_rename_to_workspace_edit(
            res_val,
            source_file_path,
            &layout.root,
            |target_path| {
                if let Some(doc) = self.open_docs.get(target_path) {
                    (doc.client_uri.clone(), doc.text.clone(), Some(doc.version))
                } else {
                    let disk = fs::read_to_string(target_path).unwrap_or_default();
                    (path_to_uri(target_path), disk, None)
                }
            },
        )
        .unwrap_or_else(|| json!({ "changes": {}, "documentChanges": [] }));

        let _ = write_lsp_message(writer, &jsonrpc_response(&request_id, workspace_edit));
    }

    fn is_project_snapshot_stale(
        &self,
        layout: &ProjectLayout,
        expected_doc_versions: &BTreeMap<PathBuf, i64>,
    ) -> bool {
        for (path, expected_ver) in expected_doc_versions {
            match self.open_docs.get(path) {
                Some(doc) if doc.version == *expected_ver => {}
                _ => return true,
            }
        }
        for doc in self.open_docs.values() {
            if (doc.layout.agent_target == layout.agent_target
                || layout
                    .overlay_keys(&doc.path)
                    .iter()
                    .any(|key| key.starts_with("@dependencies/")))
                && !expected_doc_versions.contains_key(&doc.path)
            {
                return true;
            }
        }
        false
    }
}

fn describe_missing_compiler_interface(method: &str) -> (i64, String, String) {
    match method {
        "textDocument/completion" => (
            -32601,
            "Operation 'textDocument/completion' is not supported by the current Anode compiler: agent protocol v1 provides 'check', 'context', 'symbol_at', 'symbols', 'references', 'prepare_rename', and 'rename'.".to_owned(),
            "Agent protocol method {\"method\":\"complete\",\"file\":\"...\",\"line\":N,\"column\":M,\"overlays\":{...}} returning visible bindings, record fields, enum variants, and callable functions at an incomplete token.".to_owned(),
        ),
        "textDocument/declaration" | "textDocument/typeDefinition" => (
            -32601,
            format!("Operation '{method}' is not supported; use 'textDocument/definition' (backed by compiler 'symbol_at')."),
            "Dedicated typeDefinition target in compiler 'symbol_at'.".to_owned(),
        ),
        "workspace/symbol" => (
            -32601,
            "Operation 'workspace/symbol' is not supported; use 'textDocument/documentSymbol' (backed by compiler 'symbols').".to_owned(),
            "Workspace-wide symbol search query across unloaded projects.".to_owned(),
        ),
        "textDocument/codeAction" => (
            -32601,
            "Operation 'textDocument/codeAction' is not supported by the current Anode compiler: automatic hole synthesis and structured fix-it edits are not implemented.".to_owned(),
            "Structured fix-it replacement spans in compiler diagnostics or an explicit agent synthesis endpoint.".to_owned(),
        ),
        "textDocument/rangeFormatting" => (
            -32601,
            format!("Operation '{method}' is not supported; use whole-document formatting."),
            "A syntax-aware selected-range formatter; canonical whole-document formatting is available.".to_owned(),
        ),
        _ => (
            -32601,
            format!("LSP method '{method}' is not supported: the Anode compiler agent protocol v1 supports 'check', 'format', 'context', 'symbol_at', 'symbols', 'references', 'prepare_rename', and 'rename'."),
            format!("Compiler agent protocol support for '{method}'."),
        ),
    }
}
