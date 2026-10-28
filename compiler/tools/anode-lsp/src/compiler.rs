use crate::{
    diagnostics::{parse_compiler_diagnostic, ConvertedDiagnostic},
    project::{normalize_path, ProjectLayout},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

#[derive(Clone, Debug)]
pub struct CheckResponse {
    pub ok: bool,
    pub diagnostics: Vec<ConvertedDiagnostic>,
    pub raw_diagnostics: Vec<Value>,
    pub restarted: bool,
    pub restart_count: u64,
}

#[derive(Clone, Debug)]
pub struct ContextResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub raw_diagnostics: Vec<Value>,
    pub restarted: bool,
    pub restart_count: u64,
}

struct LiveAgent {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LiveAgent {
    fn spawn(compiler_bin: &Path, target: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(compiler_bin);
        cmd.arg("agent")
            .arg(target)
            .arg("--json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn compiler agent '{} agent {}': {e}",
                compiler_bin.display(),
                target.display()
            )
        })?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| "failed to open compiler agent stdin".to_owned())?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| "failed to open compiler agent stdout".to_owned())?,
        );
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        eprintln!("[anode-lsp:compiler-stderr] {line}");
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    /// Send a single JSON Line request and read a single JSON Line response.
    fn send_request(&mut self, mut payload: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        payload["id"] = json!(id);

        let serialized = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
        if let Err(e) = writeln!(self.stdin, "{serialized}").and_then(|_| self.stdin.flush()) {
            return Err(format!("compiler agent stdin write error: {e}"));
        }

        let mut line = String::new();
        let bytes_read = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("compiler agent stdout read error: {e}"))?;
        if bytes_read == 0 {
            return Err("compiler agent closed stdout (EOF)".to_owned());
        }

        serde_json::from_str::<Value>(line.trim())
            .map_err(|e| format!("invalid JSON from compiler agent: {e} (raw: {line:?})"))
    }

    fn shutdown(&mut self) {
        // Dropping/closing stdin causes `anode agent` to exit cleanly on EOF;
        // kill only if still running after a brief wait.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveAgent {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct ProjectAgent {
    pub layout: ProjectLayout,
    live: Option<LiveAgent>,
    pub restart_count: u64,
    has_spawned_once: bool,
    needs_disk_reload: bool,
}

impl ProjectAgent {
    pub fn new(layout: ProjectLayout) -> Self {
        Self {
            layout,
            live: None,
            restart_count: 0,
            has_spawned_once: false,
            needs_disk_reload: false,
        }
    }

    pub fn mark_needs_reload(&mut self) {
        self.needs_disk_reload = true;
    }

    pub fn force_restart(&mut self) {
        self.live = None;
        self.needs_disk_reload = false;
    }

    /// Execute a `check` request with the given project-relative overlays (`{"src/main.an": "..."}`).
    pub fn check(
        &mut self,
        compiler_bin: &Path,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> CheckResponse {
        let mut restarted_this_call = false;
        let payload = build_check_payload(overlays);

        let raw_response =
            self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call);

        let mut response = match raw_response {
            Ok(val) => parse_check_value(&val, restarted_this_call, self.restart_count),
            Err(err_msg) => CheckResponse {
                ok: false,
                diagnostics: vec![ConvertedDiagnostic {
                    file: Some(self.layout.entry_file.to_string_lossy().into_owned()),
                    line: 1,
                    is_hole: false,
                    message: err_msg.clone(),
                    details: None,
                }],
                raw_diagnostics: vec![json!({
                    "severity": "error",
                    "line": 1,
                    "file": self.layout.entry_file.to_string_lossy(),
                    "message": err_msg,
                })],
                restarted: restarted_this_call,
                restart_count: self.restart_count,
            },
        };

        // If the compiler rejected `overlays` because one of the open files is not in the
        // loaded module graph (`overlay paths must exactly match loaded project-relative paths`),
        // filter to reachable module files so loaded project files still get checked, and attach
        // a diagnostic specifically on the unlinked file(s).
        if !response.ok
            && response.diagnostics.iter().any(|d| {
                d.message
                    .contains("overlay paths must exactly match loaded project-relative paths")
            })
        {
            let reachable = self.layout.discover_reachable_disk_files();
            let mut filtered_overlays = BTreeMap::new();
            let mut unlinked_files = Vec::new();

            for (rel_key, src) in overlays {
                if let Some(full_path) = overlay_full_paths.get(rel_key) {
                    if reachable.contains(&normalize_path(full_path)) {
                        filtered_overlays.insert(rel_key.clone(), src.clone());
                    } else {
                        unlinked_files.push((rel_key.clone(), full_path.clone()));
                    }
                }
            }

            if !unlinked_files.is_empty() && filtered_overlays.len() < overlays.len() {
                let filtered_payload = build_check_payload(&filtered_overlays);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &filtered_payload,
                    &mut restarted_this_call,
                ) {
                    response = parse_check_value(&val, restarted_this_call, self.restart_count);
                } else {
                    response.diagnostics.clear();
                    response.raw_diagnostics.clear();
                }

                for (rel_key, full_path) in unlinked_files {
                    let msg = format!(
                        "file '{rel_key}' is not part of the loaded project module graph (overlay paths must exactly match loaded project-relative paths; declare `mod ...;` and save to include it)"
                    );
                    let file_str = full_path.to_string_lossy().into_owned();
                    response.diagnostics.push(ConvertedDiagnostic {
                        file: Some(file_str.clone()),
                        line: 1,
                        is_hole: false,
                        message: msg.clone(),
                        details: None,
                    });
                    response.raw_diagnostics.push(json!({
                        "severity": "error",
                        "line": 1,
                        "file": file_str,
                        "message": msg,
                    }));
                }
                response.ok = response.diagnostics.is_empty();
            }
        }

        response
    }

    /// Execute a `context` query for `function_name` with the given project-relative overlays.
    pub fn context(
        &mut self,
        compiler_bin: &Path,
        function_name: &str,
        overlays: &BTreeMap<String, String>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let payload = if overlays.is_empty() {
            json!({
                "method": "context",
                "function": function_name,
            })
        } else {
            json!({
                "method": "context",
                "function": function_name,
                "overlays": overlays,
            })
        };

        match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
            Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
            Err(err) => ContextResponse {
                ok: false,
                result: None,
                raw_diagnostics: vec![json!({
                    "severity": "error",
                    "line": 1,
                    "message": err,
                })],
                restarted: restarted_this_call,
                restart_count: self.restart_count,
            },
        }
    }

    /// Execute a `symbol_at` query for `(file, line, column)` (1-based line and UTF-8 byte column).
    pub fn symbol_at(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        line: usize,
        column: usize,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let make_payload = |ov: &BTreeMap<String, String>| {
            if ov.is_empty() {
                json!({
                    "method": "symbol_at",
                    "file": file,
                    "line": line,
                    "column": column,
                })
            } else {
                json!({
                    "method": "symbol_at",
                    "file": file,
                    "line": line,
                    "column": column,
                    "overlays": ov,
                })
            }
        };
        let payload = make_payload(overlays);
        let mut resp =
            match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
                Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
                Err(err) => ContextResponse {
                    ok: false,
                    result: None,
                    raw_diagnostics: vec![json!({
                        "severity": "error",
                        "line": 1,
                        "message": err,
                    })],
                    restarted: restarted_this_call,
                    restart_count: self.restart_count,
                },
            };

        if !resp.ok && has_unlinked_overlay_error(&resp.raw_diagnostics) {
            let filtered = self.filter_reachable_overlays(overlays, overlay_full_paths);
            if filtered.len() < overlays.len() {
                let retry_payload = make_payload(&filtered);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &retry_payload,
                    &mut restarted_this_call,
                ) {
                    resp = parse_context_value(&val, restarted_this_call, self.restart_count);
                }
            }
        }
        resp
    }

    /// Execute a read-only canonical formatting query for one source snapshot.
    pub fn synthesis_query(&mut self, compiler_bin: &Path, payload: &Value) -> ContextResponse {
        let mut restarted = false;
        match self.execute_with_recovery(compiler_bin, payload, &mut restarted) {
            Ok(value) => parse_context_value(&value, restarted, self.restart_count),
            Err(message) => ContextResponse {
                ok: false,
                result: None,
                raw_diagnostics: vec![json!({"severity":"error","message":message})],
                restarted,
                restart_count: self.restart_count,
            },
        }
    }

    pub fn format(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        overlays: &BTreeMap<String, String>,
    ) -> ContextResponse {
        let mut restarted = false;
        let payload = json!({"method":"format","file":file,"overlays":overlays});
        match self.execute_with_recovery(compiler_bin, &payload, &mut restarted) {
            Ok(value) => parse_context_value(&value, restarted, self.restart_count),
            Err(message) => ContextResponse {
                ok: false,
                result: None,
                raw_diagnostics: vec![json!({"severity":"error","message":message})],
                restarted,
                restart_count: self.restart_count,
            },
        }
    }

    /// Execute a `symbols` query for `file`.
    pub fn symbols(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let make_payload = |ov: &BTreeMap<String, String>| {
            if ov.is_empty() {
                json!({
                    "method": "symbols",
                    "file": file,
                })
            } else {
                json!({
                    "method": "symbols",
                    "file": file,
                    "overlays": ov,
                })
            }
        };
        let payload = make_payload(overlays);
        let mut resp =
            match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
                Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
                Err(err) => ContextResponse {
                    ok: false,
                    result: None,
                    raw_diagnostics: vec![json!({
                        "severity": "error",
                        "line": 1,
                        "message": err,
                    })],
                    restarted: restarted_this_call,
                    restart_count: self.restart_count,
                },
            };

        if !resp.ok && has_unlinked_overlay_error(&resp.raw_diagnostics) {
            let filtered = self.filter_reachable_overlays(overlays, overlay_full_paths);
            if filtered.len() < overlays.len() {
                let retry_payload = make_payload(&filtered);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &retry_payload,
                    &mut restarted_this_call,
                ) {
                    resp = parse_context_value(&val, restarted_this_call, self.restart_count);
                }
            }
        }
        resp
    }

    /// Execute a `references` query for `(file, line, column)`.
    #[allow(clippy::too_many_arguments)]
    pub fn references(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        line: usize,
        column: usize,
        include_declaration: bool,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let make_payload = |ov: &BTreeMap<String, String>| {
            if ov.is_empty() {
                json!({
                    "method": "references",
                    "file": file,
                    "line": line,
                    "column": column,
                    "include_declaration": include_declaration,
                })
            } else {
                json!({
                    "method": "references",
                    "file": file,
                    "line": line,
                    "column": column,
                    "include_declaration": include_declaration,
                    "overlays": ov,
                })
            }
        };
        let payload = make_payload(overlays);
        let mut resp =
            match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
                Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
                Err(err) => ContextResponse {
                    ok: false,
                    result: None,
                    raw_diagnostics: vec![json!({
                        "severity": "error",
                        "line": 1,
                        "message": err,
                    })],
                    restarted: restarted_this_call,
                    restart_count: self.restart_count,
                },
            };

        if !resp.ok && has_unlinked_overlay_error(&resp.raw_diagnostics) {
            let filtered = self.filter_reachable_overlays(overlays, overlay_full_paths);
            if filtered.len() < overlays.len() {
                let retry_payload = make_payload(&filtered);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &retry_payload,
                    &mut restarted_this_call,
                ) {
                    resp = parse_context_value(&val, restarted_this_call, self.restart_count);
                }
            }
        }
        resp
    }

    /// Execute a `prepare_rename` query for `(file, line, column)`.
    pub fn prepare_rename(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        line: usize,
        column: usize,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let make_payload = |ov: &BTreeMap<String, String>| {
            if ov.is_empty() {
                json!({
                    "method": "prepare_rename",
                    "file": file,
                    "line": line,
                    "column": column,
                })
            } else {
                json!({
                    "method": "prepare_rename",
                    "file": file,
                    "line": line,
                    "column": column,
                    "overlays": ov,
                })
            }
        };
        let payload = make_payload(overlays);
        let mut resp =
            match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
                Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
                Err(err) => ContextResponse {
                    ok: false,
                    result: None,
                    raw_diagnostics: vec![json!({
                        "severity": "error",
                        "line": 1,
                        "message": err,
                    })],
                    restarted: restarted_this_call,
                    restart_count: self.restart_count,
                },
            };

        if !resp.ok && has_unlinked_overlay_error(&resp.raw_diagnostics) {
            let filtered = self.filter_reachable_overlays(overlays, overlay_full_paths);
            if filtered.len() < overlays.len() {
                let retry_payload = make_payload(&filtered);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &retry_payload,
                    &mut restarted_this_call,
                ) {
                    resp = parse_context_value(&val, restarted_this_call, self.restart_count);
                }
            }
        }
        resp
    }

    /// Execute a `rename` query for `(file, line, column, new_name)`.
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &mut self,
        compiler_bin: &Path,
        file: &str,
        line: usize,
        column: usize,
        new_name: &str,
        snapshot_fingerprint: Option<&str>,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> ContextResponse {
        let mut restarted_this_call = false;
        let make_payload = |ov: &BTreeMap<String, String>| {
            let mut obj = serde_json::Map::new();
            obj.insert("method".to_owned(), json!("rename"));
            obj.insert("file".to_owned(), json!(file));
            obj.insert("line".to_owned(), json!(line));
            obj.insert("column".to_owned(), json!(column));
            obj.insert("new_name".to_owned(), json!(new_name));
            if let Some(fp) = snapshot_fingerprint {
                obj.insert("snapshot_fingerprint".to_owned(), json!(fp));
            }
            if !ov.is_empty() {
                obj.insert("overlays".to_owned(), json!(ov));
            }
            Value::Object(obj)
        };
        let payload = make_payload(overlays);
        let mut resp =
            match self.execute_with_recovery(compiler_bin, &payload, &mut restarted_this_call) {
                Ok(val) => parse_context_value(&val, restarted_this_call, self.restart_count),
                Err(err) => ContextResponse {
                    ok: false,
                    result: None,
                    raw_diagnostics: vec![json!({
                        "severity": "error",
                        "line": 1,
                        "message": err,
                    })],
                    restarted: restarted_this_call,
                    restart_count: self.restart_count,
                },
            };

        if !resp.ok && has_unlinked_overlay_error(&resp.raw_diagnostics) {
            let filtered = self.filter_reachable_overlays(overlays, overlay_full_paths);
            if filtered.len() < overlays.len() {
                let retry_payload = make_payload(&filtered);
                if let Ok(val) = self.execute_with_recovery(
                    compiler_bin,
                    &retry_payload,
                    &mut restarted_this_call,
                ) {
                    resp = parse_context_value(&val, restarted_this_call, self.restart_count);
                }
            }
        }
        resp
    }

    fn filter_reachable_overlays(
        &self,
        overlays: &BTreeMap<String, String>,
        overlay_full_paths: &BTreeMap<String, PathBuf>,
    ) -> BTreeMap<String, String> {
        let reachable = self.layout.discover_reachable_disk_files();
        let mut filtered = BTreeMap::new();
        for (rel_key, src) in overlays {
            if let Some(full_path) = overlay_full_paths.get(rel_key) {
                if reachable.contains(&normalize_path(full_path)) {
                    filtered.insert(rel_key.clone(), src.clone());
                }
            }
        }
        filtered
    }

    fn execute_with_recovery(
        &mut self,
        compiler_bin: &Path,
        payload: &Value,
        restarted_this_call: &mut bool,
    ) -> Result<Value, String> {
        // 1. If a disk reload was requested (e.g. after `didSave`), try spawning a fresh agent
        //    and testing it with `payload`. If the fresh agent fails startup (`Project::load` error
        //    because the saved disk file has a syntax error), keep our existing live agent if available!
        if self.needs_disk_reload {
            self.needs_disk_reload = false;
            // Re-discover layout in case Anode.toml `entry` changed on disk.
            self.layout = ProjectLayout::discover(&self.layout.agent_target);
            match LiveAgent::spawn(compiler_bin, &self.layout.agent_target) {
                Ok(mut fresh) => match fresh.send_request(payload.clone()) {
                    Ok(resp) if is_protocol_v1_response(&resp) => {
                        if self.has_spawned_once {
                            self.restart_count += 1;
                            *restarted_this_call = true;
                        }
                        self.has_spawned_once = true;
                        self.live = Some(fresh);
                        return Ok(resp);
                    }
                    Ok(startup_err_resp) => {
                        // Disk snapshot failed `Project::load`.
                        // If we have an existing live session and overlays are present, try the existing live session!
                        if let Some(existing) = self.live.as_mut() {
                            if let Ok(resp) = existing.send_request(payload.clone()) {
                                if is_protocol_v1_response(&resp) {
                                    return Ok(resp);
                                }
                            }
                        }
                        self.live = None;
                        return Ok(startup_err_resp);
                    }
                    Err(_) => {
                        // Fall through to existing session or normal spawn
                    }
                },
                Err(e) => {
                    if self.live.is_none() {
                        return Err(e);
                    }
                }
            }
        }

        // 2. If we have an existing live agent, send the request to it.
        if let Some(agent) = self.live.as_mut() {
            match agent.send_request(payload.clone()) {
                Ok(resp) if is_protocol_v1_response(&resp) => {
                    if should_reload_on_diagnostic(&resp) {
                        // Try restarting once from disk in case module graph or files changed on disk.
                        self.live = None;
                        return self.spawn_and_send(compiler_bin, payload, restarted_this_call);
                    }
                    return Ok(resp);
                }
                Ok(startup_err) => {
                    self.live = None;
                    return Ok(startup_err);
                }
                Err(e) => {
                    eprintln!("[anode-lsp] compiler session terminated ({e}); restarting...");
                    self.live = None;
                }
            }
        }

        // 3. No live agent (initial start or crashed): spawn and send.
        self.spawn_and_send(compiler_bin, payload, restarted_this_call)
    }

    fn spawn_and_send(
        &mut self,
        compiler_bin: &Path,
        payload: &Value,
        restarted_this_call: &mut bool,
    ) -> Result<Value, String> {
        self.layout = ProjectLayout::discover(&self.layout.agent_target);
        let mut agent = LiveAgent::spawn(compiler_bin, &self.layout.agent_target)?;
        if self.has_spawned_once {
            self.restart_count += 1;
            *restarted_this_call = true;
        }
        self.has_spawned_once = true;

        let resp = agent.send_request(payload.clone())?;
        if is_protocol_v1_response(&resp) {
            self.live = Some(agent);
        } else {
            // Startup failure from `Project::load`: process exits after emitting the JSON error line.
            self.live = None;
        }
        Ok(resp)
    }
}

fn build_check_payload(overlays: &BTreeMap<String, String>) -> Value {
    if overlays.is_empty() {
        json!({
            "method": "check",
        })
    } else {
        json!({
            "method": "check",
            "overlays": overlays,
        })
    }
}

fn is_protocol_v1_response(resp: &Value) -> bool {
    resp.get("protocol").and_then(Value::as_u64) == Some(1)
}

fn should_reload_on_diagnostic(resp: &Value) -> bool {
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let Some(diags) = resp.get("diagnostics").and_then(Value::as_array) else {
        return false;
    };
    diags.iter().any(|d| {
        let Some(msg) = d.get("message").and_then(Value::as_str) else {
            return false;
        };
        msg.contains("overlay cannot change the module graph; reload the project")
            || msg.contains("overlay paths must exactly match loaded project-relative paths")
    })
}

fn parse_check_value(val: &Value, restarted: bool, restart_count: u64) -> CheckResponse {
    let ok = val.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let raw_diagnostics = val
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let diagnostics = raw_diagnostics
        .iter()
        .map(parse_compiler_diagnostic)
        .collect();
    CheckResponse {
        ok,
        diagnostics,
        raw_diagnostics,
        restarted,
        restart_count,
    }
}

fn parse_context_value(val: &Value, restarted: bool, restart_count: u64) -> ContextResponse {
    let ok = val.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = val.get("result").cloned();
    let raw_diagnostics = val
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ContextResponse {
        ok,
        result,
        raw_diagnostics,
        restarted,
        restart_count,
    }
}

fn has_unlinked_overlay_error(raw_diagnostics: &[Value]) -> bool {
    raw_diagnostics.iter().any(|d| {
        d.get("message").and_then(Value::as_str).is_some_and(|m| {
            m.contains("overlay paths must exactly match loaded project-relative paths")
        })
    })
}

/// Resolve the path to `bin/anode.exe` (or custom/mock compiler binary).
pub fn resolve_compiler_path(
    explicit_path: Option<&Path>,
    workspace_roots: &[PathBuf],
    hint_file: Option<&Path>,
) -> PathBuf {
    if let Some(p) = explicit_path {
        if !p.as_os_str().is_empty() {
            return p.to_path_buf();
        }
    }

    if let Ok(env_path) = env::var("ANODE_COMPILER") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    let mut search_bases = Vec::new();
    for root in workspace_roots {
        search_bases.push(root.clone());
    }
    if let Some(f) = hint_file {
        if let Some(parent) = f.parent() {
            search_bases.push(parent.to_path_buf());
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            search_bases.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = env::current_dir() {
        search_bases.push(cwd);
    }

    for base in &search_bases {
        for ancestor in base.ancestors() {
            let candidate = ancestor.join("bin").join(compiler_exe_name());
            if candidate.is_file() {
                return normalize_path(&candidate);
            }
            let legacy = ancestor.join("bin").join(legacy_exe_name());
            if legacy.is_file() {
                return normalize_path(&legacy);
            }
        }
    }

    PathBuf::from(compiler_exe_name())
}

#[cfg(windows)]
const fn compiler_exe_name() -> &'static str {
    "anode.exe"
}
#[cfg(not(windows))]
const fn compiler_exe_name() -> &'static str {
    "anode"
}

#[cfg(windows)]
const fn legacy_exe_name() -> &'static str {
    "zn.exe"
}
#[cfg(not(windows))]
const fn legacy_exe_name() -> &'static str {
    "zn"
}
