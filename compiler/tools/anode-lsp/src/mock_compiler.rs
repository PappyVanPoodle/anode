//! Programmable mock implementation of `anode.exe agent PROJECT --json` used by protocol tests.
//! Activated only when the binary is invoked with `agent <PROJECT> [--json]` and
//! `ANODE_MOCK_COMPILER_MODE` is set in the environment.

use serde_json::{json, Value};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, Write},
    path::Path,
    thread,
    time::Duration,
};

pub fn run_mock_compiler_agent(args: &[String]) -> i32 {
    let target = args.get(1).cloned().unwrap_or_else(|| ".".to_owned());
    let mode = env::var("ANODE_MOCK_COMPILER_MODE").unwrap_or_else(|_| "default".to_owned());

    // Record each process spawn in `ANODE_MOCK_SPAWN_LOG` if configured.
    if let Ok(spawn_log) = env::var("ANODE_MOCK_SPAWN_LOG") {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spawn_log)
        {
            let _ = writeln!(f, "SPAWN:{target}");
        }
    }

    // Optional one-shot startup failure simulation (`Project::load` failure before entering loop).
    if let Ok(fail_marker) = env::var("ANODE_MOCK_FAIL_STARTUP_ONCE_FILE") {
        let marker_path = Path::new(&fail_marker);
        if marker_path.is_file() {
            let _ = fs::remove_file(marker_path);
            let out = json!({
                "ok": false,
                "diagnostics": [{
                    "severity": "error",
                    "line": 1,
                    "file": target,
                    "message": "simulated disk Project::load syntax error",
                    "details": null
                }]
            });
            println!("{out}");
            return 1;
        }
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut line = String::new();
    let mut req_counter = 0u64;

    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        req_counter += 1;

        if let Ok(req_log) = env::var("ANODE_MOCK_REQUEST_LOG") {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&req_log) {
                let _ = writeln!(f, "{}", line.trim());
            }
        }

        let Ok(req) = serde_json::from_str::<Value>(line.trim()) else {
            let resp = json!({
                "protocol": 1,
                "ok": false,
                "diagnostics": [{"severity": "error", "message": "invalid JSON"}]
            });
            let _ = writeln!(writer, "{resp}").and_then(|_| writer.flush());
            continue;
        };

        // Simulate crash on first request if marker file exists (to test automatic compiler restart).
        if let Ok(crash_marker) = env::var("ANODE_MOCK_CRASH_ONCE_FILE") {
            let p = Path::new(&crash_marker);
            if p.is_file() {
                let _ = fs::remove_file(p);
                std::process::exit(101);
            }
        }

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let overlays = req.get("overlays").and_then(Value::as_object);

        let combined_source: String = overlays
            .map(|map| {
                map.values()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        // If source contains `// SLOW_CHECK_MS:200`, sleep to test stale-response discarding.
        if let Some(idx) = combined_source.find("// SLOW_CHECK_MS:") {
            let digits: String = combined_source[idx + "// SLOW_CHECK_MS:".len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(ms) = digits.parse::<u64>() {
                thread::sleep(Duration::from_millis(ms));
            }
        }
        if matches!(
            method,
            "symbol_at"
                | "symbols"
                | "references"
                | "prepare_rename"
                | "rename"
                | "format"
                | "list_holes"
                | "verify_holes"
                | "verify_contracts"
                | "plan_campaign"
                | "verify_campaign"
        ) {
            if let Ok(delay_str) = env::var("ANODE_MOCK_DELAY_MS") {
                if let Ok(ms) = delay_str.parse::<u64>() {
                    thread::sleep(Duration::from_millis(ms));
                }
            }
        }

        let response = match method {
            "plan_campaign" => {
                json!({"protocol":1,"id":id,"ok":true,"result":{"schema":"anode.campaign.v1","campaign_id":"mock","workers":[],"writes":0,"executed":false}})
            }
            "verify_campaign" => {
                json!({"protocol":1,"id":id,"ok":true,"result":{"schema":"anode.campaign-receipt.v1","campaign_id":"mock","status":"incomplete","workers":[],"writes":0,"executed":false}})
            }
            "list_holes" => {
                json!({"protocol":1,"id":id,"ok":true,"result":{"holes":[],"snapshot_fingerprint":"mock","writes":0,"executed":false}})
            }
            "verify_contracts" => {
                json!({"protocol":1,"id":id,"ok":true,"result":{"status":"planned","executed":false}})
            }
            "format" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                let source = overlays
                    .and_then(|o| o.get(file))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let fingerprint = |text: &str| {
                    let mut hash = 0xcbf29ce484222325u64;
                    for byte in text.bytes() {
                        hash ^= u64::from(byte);
                        hash = hash.wrapping_mul(0x100000001b3);
                    }
                    format!("fnv1a64:{hash:016x}:len={}", text.len())
                };
                json!({"protocol":1,"id":id,"ok":true,"result":{"writes":0,"files":[{"file":file,"text":source,"file_fingerprint":fingerprint(source),"formatted_fingerprint":fingerprint(source)}]}})
            }
            "check" => handle_mock_check(&id, &target, overlays, &mode, req_counter),
            "context" => {
                let func = req.get("function").and_then(Value::as_str).unwrap_or("");
                handle_mock_context(&id, &target, func, &combined_source)
            }
            "symbols" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                handle_mock_symbols(&id, &target, file, overlays)
            }
            "symbol_at" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                let line_num = req.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let col_num = req.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
                handle_mock_symbol_at(&id, &target, file, line_num, col_num, overlays)
            }
            "references" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                let line_num = req.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let col_num = req.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
                let include_decl = req
                    .get("include_declaration")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                handle_mock_references(
                    &id,
                    &target,
                    file,
                    line_num,
                    col_num,
                    include_decl,
                    overlays,
                )
            }
            "prepare_rename" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                let line_num = req.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let col_num = req.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
                handle_mock_prepare_rename(&id, &target, file, line_num, col_num, overlays)
            }
            "rename" => {
                let file = req
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("src/main.an");
                let line_num = req.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
                let col_num = req.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
                let new_name = req.get("new_name").and_then(Value::as_str).unwrap_or("");
                handle_mock_rename(&id, &target, file, line_num, col_num, new_name, overlays)
            }
            _ => json!({
                "protocol": 1,
                "id": id,
                "ok": false,
                "elapsed_ms": 0.1,
                "diagnostics": [{"severity": "error", "line": 0, "message": "unknown method; supported: check, context, symbol_at, symbols, references, prepare_rename, rename"}]
            }),
        };

        if writeln!(writer, "{response}")
            .and_then(|_| writer.flush())
            .is_err()
        {
            break;
        }
    }

    0
}

fn handle_mock_check(
    id: &Value,
    target: &str,
    overlays: Option<&serde_json::Map<String, Value>>,
    _mode: &str,
    _req_counter: u64,
) -> Value {
    if let Some(map) = overlays {
        for (rel_path, val) in map {
            let Some(src) = val.as_str() else {
                continue;
            };
            let full_file = resolve_target_file(target, rel_path);

            // 1. Check for simulated module graph change error (resolved after restart if marker removed)
            if src.contains("mod trigger_reload_once;") {
                if let Ok(reload_marker) = env::var("ANODE_MOCK_RELOAD_ONCE_FILE") {
                    let p = Path::new(&reload_marker);
                    if p.is_file() {
                        let _ = fs::remove_file(p);
                        return json!({
                            "protocol": 1,
                            "id": id,
                            "ok": false,
                            "elapsed_ms": 0.2,
                            "diagnostics": [{
                                "severity": "error",
                                "line": 0,
                                "file": full_file,
                                "message": "overlay cannot change the module graph; reload the project",
                                "details": null
                            }]
                        });
                    }
                }
            }

            // 2. Check for typed hole `??(`
            if let Some((line_idx, line_str)) = src
                .lines()
                .enumerate()
                .find(|(_, l)| !l.trim_start().starts_with("//") && l.contains("??("))
            {
                let (declared_type, intent) = parse_hole_from_line(line_str);
                return json!({
                    "protocol": 1,
                    "id": id,
                    "ok": false,
                    "elapsed_ms": 0.3,
                    "diagnostics": [{
                        "severity": "error",
                        "line": line_idx + 1,
                        "file": full_file,
                        "message": "unresolved typed hole; implement it before checking or building",
                        "details": {
                            "kind": "synthesis_hole",
                            "declared_type": declared_type,
                            "declared_type_is_not_inferred": true,
                            "intent": intent,
                            "effect_contracts": ["no_alloc", "no_io"],
                            "capabilities_unrestricted": false,
                            "project_capabilities": ["file_read"],
                            "scope": [
                                {
                                    "name": "values",
                                    "type": "ints",
                                    "borrow": "Shared",
                                    "mutable": false,
                                    "moved": false,
                                    "moved_fields": [],
                                    "borrowed_in_expression": false,
                                    "exclusive_in_expression": false
                                },
                                {
                                    "name": "scratch",
                                    "type": "text",
                                    "borrow": "Owned",
                                    "mutable": true,
                                    "moved": true,
                                    "moved_fields": [],
                                    "borrowed_in_expression": false,
                                    "exclusive_in_expression": false
                                }
                            ]
                        }
                    }]
                });
            }

            // 3. Check for explicit error marker `ERROR_HERE`
            if let Some((line_idx, line_str)) = src
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains("ERROR_HERE"))
            {
                let msg = if let Some(tag_idx) = line_str.find("ERROR_HERE:") {
                    format!(
                        "mock error: {}",
                        line_str[tag_idx + "ERROR_HERE:".len()..].trim()
                    )
                } else {
                    "mock compiler error on ERROR_HERE".to_owned()
                };
                return json!({
                    "protocol": 1,
                    "id": id,
                    "ok": false,
                    "elapsed_ms": 0.2,
                    "diagnostics": [{
                        "severity": "error",
                        "line": line_idx + 1,
                        "file": full_file,
                        "message": msg,
                        "details": null
                    }]
                });
            }
        }
    }

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": {
            "checked_files": overlays.map_or(1, |m| m.len().max(1)),
            "writes": 0,
            "executed_tests": false,
            "scope": "full_project"
        }
    })
}

fn handle_mock_context(id: &Value, target: &str, func: &str, combined_source: &str) -> Value {
    if combined_source.contains("ERROR_HERE") || combined_source.contains("??(") {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": 1,
                "file": target,
                "message": "unresolved typed hole; implement it before checking or building",
                "details": null
            }]
        });
    }

    match func {
        "search::matching" | "positive_count" | "compute" | "stats::compute" => json!({
            "protocol": 1,
            "id": id,
            "ok": true,
            "elapsed_ms": 0.2,
            "result": {
                "function": func,
                "file": target,
                "line": 3,
                "parameters": [
                    {
                        "name": "values",
                        "type": "ints",
                        "borrow": "Shared",
                        "declared_view": null
                    }
                ],
                "returns": "int",
                "effects": {
                    "contracts": ["no_alloc", "no_io"],
                    "may_allocate": false,
                    "may_do_io": false
                },
                "intents": ["Mock function intent for testing"],
                "intents_are_advisory": true,
                "views": [
                    [{"field_indices": [], "mutable": false}]
                ],
                "types": [],
                "callees": [],
                "callers": ["main"]
            }
        }),
        _ => json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": 0,
                "file": null,
                "message": format!("unknown function {func}; use its module-qualified name"),
                "details": null
            }]
        }),
    }
}

fn resolve_target_file(target: &str, rel_path: &str) -> String {
    let target_path = Path::new(target);
    let base = if target_path.is_dir() {
        target_path.to_path_buf()
    } else {
        target_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };
    base.join(rel_path).to_string_lossy().into_owned()
}

fn parse_hole_from_line(line: &str) -> (String, String) {
    let Some(idx) = line.find("??(") else {
        return ("int".to_owned(), String::new());
    };
    let inside = &line[idx + 3..];
    let Some((ty_part, rest)) = inside.split_once(',') else {
        return ("int".to_owned(), String::new());
    };
    let declared_type = ty_part.trim().to_owned();
    let intent = rest
        .split('"')
        .nth(1)
        .unwrap_or("implement hole")
        .to_owned();
    (declared_type, intent)
}

fn load_mock_file(
    target: &str,
    rel_file: &str,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> (String, String) {
    let full = resolve_target_file(target, rel_file);
    if let Some(ov) = overlays {
        if let Some(s) = ov.get(rel_file).and_then(Value::as_str) {
            return (full, s.to_owned());
        }
    }
    let disk = fs::read_to_string(&full).unwrap_or_default();
    (full, disk)
}

fn handle_mock_symbols(
    id: &Value,
    target: &str,
    file: &str,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let (full_path, src) = load_mock_file(target, file, overlays);
    if src.contains("ERROR_HERE") || src.contains("SYNTAX_ERROR") {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": 1,
                "file": full_path,
                "message": "mock syntax error in symbols",
                "details": null
            }]
        });
    }

    let mut symbols = Vec::new();
    let mut byte_offset = 0usize;
    for (idx, raw_line) in src.split('\n').enumerate() {
        let line_num = idx + 1;
        let line_str = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        for (kw, kind) in [
            ("fn ", "function"),
            ("pub fn ", "function"),
            ("struct ", "struct"),
            ("enum ", "enum"),
        ] {
            if let Some(pos) = line_str.find(kw) {
                let name_start = pos + kw.len();
                let name: String = line_str[name_start..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    let col_start = name_start + 1;
                    let col_end = col_start + name.len();
                    symbols.push(json!({
                        "id": format!("{kind}:{name}"),
                        "name": name,
                        "qualified_name": name,
                        "kind": kind,
                        "detail": format!("{kw}{name}"),
                        "file": file,
                        "path": full_path,
                        "line": line_num,
                        "column": col_start,
                        "range": {
                            "start": { "line": line_num, "column": pos + 1, "byte": byte_offset + pos },
                            "end": { "line": line_num, "column": line_str.len() + 1, "byte": byte_offset + line_str.len() }
                        },
                        "selection_range": {
                            "start": { "line": line_num, "column": col_start, "byte": byte_offset + name_start },
                            "end": { "line": line_num, "column": col_end, "byte": byte_offset + name_start + name.len() }
                        },
                        "children": []
                    }));
                    break;
                }
            }
        }
        byte_offset += raw_line.len() + 1;
    }

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": {
            "coordinate_encoding": "utf8-1based",
            "file": file,
            "path": full_path,
            "symbols": symbols,
        }
    })
}

fn handle_mock_symbol_at(
    id: &Value,
    target: &str,
    file: &str,
    line_1: usize,
    col_1: usize,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let (full_path, src) = load_mock_file(target, file, overlays);
    if src.contains("SYNTAX_ERROR") {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": 1,
                "file": full_path,
                "message": "mock syntax error in symbol_at",
                "details": null
            }]
        });
    }

    let lines: Vec<&str> = src
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let Some(cur_line) = lines.get(line_1.saturating_sub(1)) else {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": true,
            "elapsed_ms": 0.1,
            "result": { "coordinate_encoding": "utf8-1based", "symbol": null }
        });
    };
    let byte_idx = col_1.saturating_sub(1);
    if byte_idx >= cur_line.len() {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": true,
            "elapsed_ms": 0.1,
            "result": { "coordinate_encoding": "utf8-1based", "symbol": null }
        });
    }
    let bytes = cur_line.as_bytes();
    if !(bytes[byte_idx].is_ascii_alphanumeric() || bytes[byte_idx] == b'_') {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": true,
            "elapsed_ms": 0.1,
            "result": { "coordinate_encoding": "utf8-1based", "symbol": null }
        });
    }
    let mut start = byte_idx;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    let mut end = byte_idx;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    let word = &cur_line[start..end];

    // Find declaration of `fn <word>` or `let <word>` in current file
    for (idx, l) in lines.iter().enumerate() {
        for pat in [
            format!("fn {word}"),
            format!("let {word}"),
            format!("let mut {word}"),
        ] {
            if let Some(pos) = l.find(&pat) {
                let decl_start = pos + pat.len() - word.len();
                let decl_line = idx + 1;
                let decl_col = decl_start + 1;
                return json!({
                    "protocol": 1,
                    "id": id,
                    "ok": true,
                    "elapsed_ms": 0.1,
                    "result": {
                        "coordinate_encoding": "utf8-1based",
                        "symbol": {
                            "id": format!("mock:{word}"),
                            "name": word,
                            "qualified_name": word,
                            "kind": if pat.starts_with("fn") { "function" } else { "local" },
                            "detail": pat,
                            "file": file,
                            "path": full_path,
                            "line": decl_line,
                            "column": decl_col,
                            "range": {
                                "start": { "line": decl_line, "column": pos + 1, "byte": 0 },
                                "end": { "line": decl_line, "column": decl_col + word.len(), "byte": 0 }
                            },
                            "selection_range": {
                                "start": { "line": decl_line, "column": decl_col, "byte": 0 },
                                "end": { "line": decl_line, "column": decl_col + word.len(), "byte": 0 }
                            },
                            "reference_range": {
                                "start": { "line": line_1, "column": start + 1, "byte": 0 },
                                "end": { "line": line_1, "column": end + 1, "byte": 0 }
                            }
                        }
                    }
                });
            }
        }
    }

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": { "coordinate_encoding": "utf8-1based", "symbol": null }
    })
}

fn find_mock_word_ranges_in_line(line: &str, word: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        if !in_string && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            break;
        }
        if bytes[i] == b'"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if &line[start..i] == word {
                out.push((start + 1, i + 1));
            }
        } else {
            i += 1;
        }
    }
    out
}

fn handle_mock_references(
    id: &Value,
    target: &str,
    file: &str,
    line_1: usize,
    col_1: usize,
    include_decl: bool,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let sym_resp = handle_mock_symbol_at(id, target, file, line_1, col_1, overlays);
    if sym_resp.get("ok") != Some(&Value::Bool(true)) {
        return sym_resp;
    }
    let Some(symbol) = sym_resp
        .get("result")
        .and_then(|r| r.get("symbol"))
        .filter(|s| !s.is_null())
        .cloned()
    else {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": true,
            "elapsed_ms": 0.1,
            "result": {
                "coordinate_encoding": "utf8-1based",
                "include_declaration": include_decl,
                "symbol": null,
                "references": []
            }
        });
    };

    let word = symbol.get("name").and_then(Value::as_str).unwrap_or("");
    let decl_line = symbol.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let decl_col = symbol.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;
    let (full_path, src) = load_mock_file(target, file, overlays);

    let mut references = Vec::new();
    for (idx, raw_line) in src.split('\n').enumerate() {
        let l_num = idx + 1;
        let line_str = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        for (c_start, c_end) in find_mock_word_ranges_in_line(line_str, word) {
            let is_decl = l_num == decl_line && c_start == decl_col;
            if !include_decl && is_decl {
                continue;
            }
            references.push(json!({
                "file": file,
                "path": full_path,
                "line": l_num,
                "column": c_start,
                "range": {
                    "start": { "line": l_num, "column": c_start, "byte": 0 },
                    "end": { "line": l_num, "column": c_end, "byte": 0 }
                },
                "is_declaration": is_decl,
                "access": if is_decl { "declaration" } else { "read" }
            }));
        }
    }

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": {
            "coordinate_encoding": "utf8-1based",
            "include_declaration": include_decl,
            "symbol": symbol,
            "references": references
        }
    })
}

fn handle_mock_prepare_rename(
    id: &Value,
    target: &str,
    file: &str,
    line_1: usize,
    col_1: usize,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let sym_resp = handle_mock_symbol_at(id, target, file, line_1, col_1, overlays);
    if sym_resp.get("ok") != Some(&Value::Bool(true)) {
        return sym_resp;
    }
    let Some(symbol) = sym_resp
        .get("result")
        .and_then(|r| r.get("symbol"))
        .filter(|s| !s.is_null())
        .cloned()
    else {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": line_1,
                "file": resolve_target_file(target, file),
                "message": "rename is not available at this position",
                "details": null
            }]
        });
    };

    let name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "main" {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": line_1,
                "file": resolve_target_file(target, file),
                "message": "cannot rename entry point function 'main'",
                "details": null
            }]
        });
    }

    let range = symbol
        .get("reference_range")
        .or_else(|| symbol.get("selection_range"))
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": {
            "coordinate_encoding": "utf8-1based",
            "can_rename": true,
            "placeholder": name,
            "range": range,
            "symbol": symbol,
            "scope": "loaded_project",
            "public_symbol": false,
            "scope_warning": null,
            "snapshot_fingerprint": "mock-fp-1"
        }
    })
}

fn handle_mock_rename(
    id: &Value,
    target: &str,
    file: &str,
    line_1: usize,
    col_1: usize,
    new_name: &str,
    overlays: Option<&serde_json::Map<String, Value>>,
) -> Value {
    let prep = handle_mock_prepare_rename(id, target, file, line_1, col_1, overlays);
    if prep.get("ok") != Some(&Value::Bool(true)) {
        return prep;
    }

    let is_valid_ident = !new_name.is_empty()
        && (new_name.as_bytes()[0].is_ascii_alphabetic() || new_name.as_bytes()[0] == b'_')
        && new_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !is_valid_ident
        || matches!(
            new_name,
            "fn" | "let"
                | "mut"
                | "return"
                | "struct"
                | "enum"
                | "match"
                | "main"
                | "int"
                | "COLLIDE"
                | "CAPTURE"
        )
    {
        return json!({
            "protocol": 1,
            "id": id,
            "ok": false,
            "elapsed_ms": 0.1,
            "diagnostics": [{
                "severity": "error",
                "line": line_1,
                "file": resolve_target_file(target, file),
                "message": format!("invalid or conflicting rename target '{new_name}'"),
                "details": null
            }]
        });
    }

    let refs_resp = handle_mock_references(id, target, file, line_1, col_1, true, overlays);
    let symbol = refs_resp["result"]["symbol"].clone();
    let old_name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
    let refs = refs_resp["result"]["references"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let edits: Vec<Value> = refs
        .into_iter()
        .map(|r| {
            json!({
                "file": r["file"],
                "path": r["path"],
                "line": r["line"],
                "column": r["column"],
                "range": r["range"],
                "original_text": old_name,
                "new_text": new_name,
                "is_declaration": r["is_declaration"],
                "file_fingerprint": "mock-file-fp"
            })
        })
        .collect();

    json!({
        "protocol": 1,
        "id": id,
        "ok": true,
        "elapsed_ms": 0.1,
        "result": {
            "coordinate_encoding": "utf8-1based",
            "symbol": symbol,
            "new_name": new_name,
            "writes": 0,
            "scope": "loaded_project",
            "public_symbol": false,
            "scope_warning": null,
            "snapshot_fingerprint": "mock-fp-1",
            "file_fingerprints": { file: "mock-file-fp" },
            "edits": edits
        }
    })
}
