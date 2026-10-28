//! Agent protocol v1: source snapshots and full-project checks of in-memory
//! alternatives. `commit_holes` and `verify_campaign` require explicit apply
//! before guarded source transactions; all other requests are read-only.
//! Contract execution is explicit and finite. stdout is JSON Lines only.
use crate::{project::Project, syntax::*, Error, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
    path::Path,
    time::Instant,
};

pub fn diagnostic(error: &Error) -> Value {
    json!({"severity":"error","line":error.line,"file":error.path.as_ref().map(|p|p.to_string_lossy()),"message":error.message,"details":error.details})
}
pub fn context(project: &Project, requested: &str) -> Result<Value> {
    let (program, _) = project.checked(false)?;
    let effects = crate::effects::analyze(&program);
    let sources = program
        .functions
        .iter()
        .map(|f| (f.source.name.as_str(), &f.source))
        .collect::<BTreeMap<_, _>>();
    let mut names = BTreeMap::new();
    for (i, file) in project.files.iter().enumerate() {
        for (j, f) in file.parsed.functions.iter().enumerate() {
            let symbol = crate::project::function_symbol(i, j, &file.module, f);
            let key = if let Some(method) = sources
                .get(symbol.as_str())
                .and_then(|source| source.method.as_ref())
            {
                let Type::Named(owner) = method.owner else {
                    return Err(Error::new(f.line, "method owner is not a named type"));
                };
                format!("{}::{}", program.types.definitions[owner].name, method.name)
            } else if file.module.is_empty() {
                f.name.clone()
            } else {
                format!("{}::{}", file.module, f.name)
            };
            names.insert(symbol, key);
        }
    }
    let target = names
        .iter()
        .find(|(_, name)| name.as_str() == requested)
        .map(|(s, _)| s)
        .ok_or_else(|| {
            Error::new(
                0,
                format!("unknown function {requested}; use its module-qualified name"),
            )
        })?;
    let f = sources
        .get(target.as_str())
        .ok_or_else(|| Error::new(0, format!("no checked function for {requested}")))?;
    let location = project.map_error(Error::new(f.line, ""));
    let result_name = |t: Type| match t {
        Type::Named(id) => program.types.definitions[id].name.clone(),
        Type::Id(id) => format!("id({})", program.types.definitions[id].name),
        Type::VectorView(id) => format!("[{}]", program.types.definitions[id].name),
        other => other.label(),
    };
    let ty = |t: Type| match t.task_result() {
        Some(result) => format!("task<{}>", result_name(result)),
        None => result_name(t),
    };
    let e = &effects[target];
    let signature=f.params.iter().map(|p|json!({"name":p.name,"type":ty(p.ty),"borrow":format!("{:?}",p.borrow),"declared_view":p.view.as_ref().map(|v|v.iter().map(|field|json!({"path":field.path,"mutable":field.mutable})).collect::<Vec<_>>())})).collect::<Vec<_>>();
    let views = program.views[target]
        .iter()
        .map(|accesses| {
            accesses
                .iter()
                .map(|a| json!({"field_indices":a.fields,"mutable":a.mutable}))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut relevant = std::collections::BTreeSet::new();
    let mut work = f
        .params
        .iter()
        .map(|p| p.ty)
        .chain(std::iter::once(f.ret))
        .collect::<Vec<_>>();
    while let Some(t) = work.pop() {
        if let Some(result) = t.task_result() {
            work.push(result);
        }
        if let Type::Named(id) | Type::Id(id) | Type::VectorView(id) = t {
            if relevant.insert(id) {
                match &program.types.definitions[id].kind {
                    DataKind::Record(fields)
                    | DataKind::Enum(fields)
                    | DataKind::Vector(fields)
                    | DataKind::SoaVector(fields)
                    | DataKind::Map(fields)
                    | DataKind::Arena(fields)
                    | DataKind::Channel(fields) => work.extend(fields.iter().map(|f| f.ty)),
                }
            }
        }
    }
    let definitions = relevant
        .into_iter()
        .map(|id| {
            let d = &program.types.definitions[id];
            json!({"name":d.name,"definition":format!("{:?}",d.kind)})
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"function":requested,"file":location.path.map(|p|p.to_string_lossy().into_owned()),"line":location.line,"parameters":signature,"returns":ty(f.ret),"effects":{"may_allocate":e.alloc,"may_do_io":e.io,"contracts":f.effects},"intents":f.intents,"intents_are_advisory":true,"views":views,"types":definitions,"callees":e.calls.iter().map(|s|names.get(s).unwrap_or(s)).collect::<Vec<_>>(),"callers":effects.iter().filter(|(_,v)|v.calls.contains(target)).map(|(s,_)|names.get(s).unwrap_or(s)).collect::<Vec<_>>() }),
    )
}
fn request_method(request: &Value) -> Result<&str> {
    let method = request
        .get("method")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| Error::new(0, "method must be a string"))
        })
        .transpose()?;
    let command = request
        .get("command")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| Error::new(0, "command must be a string"))
        })
        .transpose()?;
    if command.is_some_and(|command| command != "campaign") {
        return Err(Error::new(
            0,
            "unknown agent command; command must be campaign",
        ));
    }
    if command == Some("campaign")
        || matches!(
            method,
            Some("campaign" | "plan_campaign" | "verify_campaign")
        )
    {
        let default_action = match method {
            Some("plan_campaign") => "plan",
            Some("verify_campaign") => "verify",
            _ if request.get("responses").is_some() => "verify",
            _ => "plan",
        };
        let action = request
            .get("action")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| Error::new(0, "campaign action must be a string"))
            })
            .transpose()?
            .unwrap_or(default_action);
        let resolved = match action {
            "plan" => "plan_campaign",
            "verify" => "verify_campaign",
            _ => return Err(Error::new(0, "campaign action must be plan or verify")),
        };
        if method.is_some_and(|m| m != "campaign" && m != resolved) {
            return Err(Error::new(
                0,
                "conflicting method and campaign command/action",
            ));
        }
        return Ok(resolved);
    }
    method.ok_or_else(||Error::new(0,"method is required (check, format, context, list_holes, skeleton_receipt, verify_holes, verify_batch, commit_holes, plan_campaign, verify_campaign, verify_contracts, symbol_at, symbols, references, prepare_rename, or rename); command:campaign is also accepted"))
}

pub fn handle(project: &Project, request: &Value) -> Value {
    let start = Instant::now();
    let mut diagnostic_overlays = None;
    let result = (|| -> Result<Value> {
        let method = request_method(request)?;
        let mut overlays = BTreeMap::new();
        if let Some(map) = request.get("overlays") {
            for (path, source) in map
                .as_object()
                .ok_or_else(|| Error::new(0, "overlays must be a path-to-source object"))?
            {
                overlays.insert(
                    path.clone(),
                    source
                        .as_str()
                        .ok_or_else(|| Error::new(0, "overlay source must be a string"))?
                        .to_owned(),
                );
            }
        }
        if let Some(patches) = request.get("patches") {
            let patches = patches
                .as_array()
                .ok_or_else(|| Error::new(0, "patches must be an array"))?;
            if patches.len() > 32 {
                return Err(Error::new(0, "at most 32 patches per candidate"));
            }
            for patch in patches {
                let field = |key| {
                    patch
                        .get(key)
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::new(0, format!("patch requires string {key}")))
                };
                let path = field("path")?;
                let find = field("find")?;
                let replacement = field("replace")?;
                let original = project
                    .files
                    .iter()
                    .find(|f| f.source_key == path)
                    .ok_or_else(|| {
                        Error::new(0, "patch path must match a loaded project-relative file")
                    })?;
                let source = overlays
                    .entry(path.into())
                    .or_insert_with(|| original.source.clone());
                if find.is_empty() || source.match_indices(find).count() != 1 {
                    return Err(Error::new(
                        0,
                        "patch find must match exactly once; stale or ambiguous edit rejected",
                    ));
                }
                let size = source.len() - find.len();
                if replacement.len() > crate::project::SOURCE_FILE_LIMIT
                    || size + replacement.len() > crate::project::SOURCE_FILE_LIMIT
                {
                    return Err(Error::new(0, "patched file exceeds 16 MiB"));
                }
                *source = source.replacen(find, replacement, 1);
            }
        }
        diagnostic_overlays = Some(overlays.clone());
        if matches!(method, "commit_holes" | "recover_holes" | "verify_campaign")
            && request.get("apply") == Some(&Value::Bool(true))
            && !overlays.is_empty()
        {
            return Err(Error::new(
                0,
                "apply requires an unchanged disk snapshot; overlays and patches are dry-run only",
            ));
        }
        let overlay_candidate;
        let candidate = if overlays.is_empty() && project.syntax_current() {
            project
        } else {
            overlay_candidate = project.with_overlays(&overlays)?;
            &overlay_candidate
        };
        match method {
            "check" => {
                candidate.checked(false)?;
                Ok(
                    json!({"checked_files":candidate.files.len(),"writes":0,"executed_tests":false,"scope":"full_project"}),
                )
            }
            "format" => format(candidate, request),
            "list_holes" => crate::holes::list_holes(candidate),
            "skeleton_receipt" => crate::holes::skeleton_receipt(candidate),
            "verify_holes" => crate::holes::verify_holes(candidate, request),
            "verify_batch" => crate::holes::verify_batch(candidate, request),
            "plan_campaign" => crate::holes::plan_campaign(candidate, request),
            "verify_campaign" => crate::holes::verify_campaign(candidate, request),
            "commit_holes" => crate::holes::commit_holes(candidate, request),
            "inspect_hole_transaction" => crate::holes::inspect_hole_transaction(candidate, request),
            "recover_holes" => crate::holes::recover_holes(candidate, request),
            "verify_contracts" | "verify" => {
                if request.get("apply") == Some(&Value::Bool(true)) {
                    return Err(Error::new(0,"contract verification and test proposals are read-only"));
                }
                let pins = parse_file_fingerprints(request)?;
                let expected = request.get("snapshot_fingerprint").map(|value|value.as_str().ok_or_else(||Error::new(0,"snapshot_fingerprint must be a string"))).transpose()?;
                crate::navigation::verify_snapshot_fingerprints(
                    candidate,
                    expected,
                    pins.as_ref(),
                )?;
                if let Some(expected)=request.get("snapshot_sha256") {
                    if expected.as_str()!=Some(crate::holes::snapshot_sha256(candidate).as_str()) {
                        return Err(Error::new(0,"stale or invalid snapshot_sha256 for contract verification"));
                    }
                }
                crate::verify::verify_contracts(candidate,&crate::verify::VerificationOptions::from_request(request)?)
            }
            "context" => context(
                candidate,
                request
                    .get("function")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "context requires function"))?,
            ),
            "symbols" => crate::navigation::symbols(
                candidate,
                request.get("file").and_then(Value::as_str),
            ),
            "symbol_at" => {
                let file = request
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "symbol_at requires file"))?;
                let line = request
                    .get("line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "symbol_at requires 1-based line"))?
                    as usize;
                let column = request
                    .get("column")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "symbol_at requires 1-based column"))?
                    as usize;
                crate::navigation::symbol_at(candidate, file, line, column)
            }
            "references" => {
                let file = request
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "references requires file"))?;
                let line = request
                    .get("line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "references requires 1-based line"))?
                    as usize;
                let column = request
                    .get("column")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "references requires 1-based column"))?
                    as usize;
                let include_declaration = request
                    .get("include_declaration")
                    .or_else(|| request.get("includeDeclaration"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                crate::navigation::references(candidate, file, line, column, include_declaration)
            }
            "prepare_rename" => {
                let file = request
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "prepare_rename requires file"))?;
                let line = request
                    .get("line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "prepare_rename requires 1-based line"))?
                    as usize;
                let column = request
                    .get("column")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "prepare_rename requires 1-based column"))?
                    as usize;
                let expected_fp = request
                    .get("snapshot_fingerprint")
                    .or_else(|| request.get("expected_fingerprint"))
                    .and_then(Value::as_str);
                let expected_file_fps = parse_file_fingerprints(request)?;
                crate::navigation::prepare_rename(
                    candidate,
                    file,
                    line,
                    column,
                    expected_fp,
                    expected_file_fps.as_ref(),
                )
            }
            "rename" => {
                let file = request
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "rename requires file"))?;
                let line = request
                    .get("line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "rename requires 1-based line"))?
                    as usize;
                let column = request
                    .get("column")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::new(0, "rename requires 1-based column"))?
                    as usize;
                let new_name = request
                    .get("new_name")
                    .or_else(|| request.get("newName"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "rename requires new_name"))?;
                let expected_fp = request
                    .get("snapshot_fingerprint")
                    .or_else(|| request.get("expected_fingerprint"))
                    .and_then(Value::as_str);
                let expected_file_fps = parse_file_fingerprints(request)?;
                crate::navigation::rename(
                    candidate,
                    file,
                    line,
                    column,
                    new_name,
                    expected_fp,
                    expected_file_fps.as_ref(),
                )
            }
            _ => Err(Error::new(
                0,
                "unknown method; supported: check, format, context, list_holes, verify_holes, verify_contracts, symbol_at, symbols, references, prepare_rename, rename",
            )),
        }
    })();
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(value) => {
            json!({"protocol":1,"id":request.get("id"),"ok":true,"elapsed_ms":elapsed,"result":value})
        }
        Err(error) => {
            let limit = request
                .get("max_diagnostics")
                .and_then(Value::as_u64)
                .unwrap_or(32)
                .clamp(1, 32) as usize;
            let recovery = match (
                request.get("method").and_then(Value::as_str),
                diagnostic_overlays.as_ref(),
            ) {
                (Some("check"), Some(overlays)) => {
                    recover_diagnostics(project, overlays, error, limit)
                }
                _ => Recovery {
                    errors: vec![error],
                    probes: 0,
                    capped: false,
                },
            };
            json!({"protocol":1,"id":request.get("id"),"ok":false,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0,"diagnostics":recovery.errors.iter().map(diagnostic).collect::<Vec<_>>(),"diagnostics_limit":limit,"diagnostics_truncated":recovery.capped,"recovery":{"scope":"independent_declarations","probes":recovery.probes,"validated":false}})
        }
    }
}

fn relative_file(project: &Project, path: &Path) -> String {
    project
        .source_key(path)
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}

fn format(project: &Project, request: &Value) -> Result<Value> {
    let snapshot = crate::navigation::project_fingerprint(project);
    if let Some(expected) = request
        .get("snapshot_fingerprint")
        .or_else(|| request.get("expected_fingerprint"))
        .and_then(Value::as_str)
    {
        if expected != snapshot {
            return Err(Error::new(0, "stale project snapshot for formatting"));
        }
    }
    if let Some(expected) = parse_file_fingerprints(request)? {
        for (path, fingerprint) in expected {
            let file = project
                .files
                .iter()
                .find(|f| relative_file(project, &f.path) == path)
                .ok_or_else(|| Error::new(0, format!("unknown project file {path}")))?;
            if crate::navigation::source_fingerprint(&file.source) != fingerprint {
                return Err(Error::new(0, format!("stale file snapshot for {path}")));
            }
        }
    }
    let requested = request.get("file").and_then(Value::as_str);
    let mut files = Vec::new();
    for file in &project.files {
        let relative = relative_file(project, &file.path);
        if requested.is_some_and(|path| path != relative) {
            continue;
        }
        let text = crate::fmt::format_source(&file.source).map_err(|e| e.in_file(&file.path))?;
        files.push(json!({"file":relative,"path":file.path.to_string_lossy(),"changed":text!=file.source,"text":text,"file_fingerprint":crate::navigation::source_fingerprint(&file.source),"formatted_fingerprint":crate::navigation::source_fingerprint(&text)}));
    }
    if files.is_empty() && requested.is_some() {
        return Err(Error::new(
            0,
            "format file must match a loaded project-relative path",
        ));
    }
    Ok(
        json!({"writes":0,"coordinate_encoding":crate::navigation::COORDINATE_ENCODING,"snapshot_fingerprint":snapshot,"files":files}),
    )
}

struct Recovery {
    errors: Vec<Error>,
    probes: usize,
    capped: bool,
}
impl Recovery {
    fn push(&mut self, error: Error, limit: usize) {
        if self.errors.iter().any(|old| {
            old.path == error.path && old.line == error.line && old.message == error.message
        }) {
            return;
        }
        if self.errors.len() == limit {
            self.capped = true;
        } else {
            self.errors.push(error);
        }
    }
}

/// Recovery never replaces an invalid candidate by valid masked code. Syntax
/// probes inspect independent original declaration slices. Semantic probes only
/// vary visitation order on a fresh AST of the complete, unchanged candidate.
/// Their results are diagnostics only; the strict failure remains `ok: false`.
fn recover_diagnostics(
    project: &Project,
    overlays: &BTreeMap<String, String>,
    first: Error,
    limit: usize,
) -> Recovery {
    let mut recovery = Recovery {
        errors: vec![first],
        probes: 0,
        capped: false,
    };
    let paths = project
        .files
        .iter()
        .map(|f| relative_file(project, &f.path))
        .collect::<BTreeSet<_>>();
    if overlays.keys().any(|p| !paths.contains(p)) {
        return recovery;
    }
    let total: usize = project
        .files
        .iter()
        .map(|f| {
            overlays
                .get(&relative_file(project, &f.path))
                .unwrap_or(&f.source)
                .len()
        })
        .sum();
    if total > crate::project::PROJECT_SOURCE_LIMIT {
        return recovery;
    }
    let mut syntax_failed = false;
    for file in &project.files {
        let source = overlays
            .get(&relative_file(project, &file.path))
            .unwrap_or(&file.source);
        if source.len() > crate::project::SOURCE_FILE_LIMIT {
            return recovery;
        }
        if let Err(error) = parse_module(source) {
            syntax_failed = true;
            recovery.push(error.in_file(&file.path), limit);
            let tokens = crate::fmt::tokenize(source);
            // `fn` and `test` cannot occur as nested declarations in valid Anode.
            // Strings and comments are whole lexemes, so their contents never
            // become spurious synchronization boundaries.
            let starts = tokens
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    !t.comment
                        && matches!(
                            t.text,
                            "fn" | "test" | "struct" | "enum" | "vector" | "map" | "mod"
                        )
                })
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            for (ordinal, &start) in starts.iter().enumerate() {
                if recovery.probes == 32 {
                    recovery.capped = true;
                    return recovery;
                }
                let boundary = starts.get(ordinal + 1).copied().unwrap_or(tokens.len());
                let mut braces = 0usize;
                let mut parens = 0usize;
                let mut body = false;
                let mut end = source.len();
                for token in &tokens[start..boundary] {
                    if token.comment {
                        continue;
                    }
                    match token.text {
                        "(" | "[" => parens += 1,
                        ")" | "]" => parens = parens.saturating_sub(1),
                        "{" if parens == 0 || body => {
                            body = true;
                            braces += 1;
                        }
                        "}" if body => {
                            braces = braces.saturating_sub(1);
                            if braces == 0 {
                                end = token.end;
                                break;
                            }
                        }
                        ";" if !body && parens == 0 => {
                            end = token.end;
                            break;
                        }
                        _ => {}
                    }
                }
                end = end.min(tokens.get(boundary).map_or(source.len(), |t| t.start));
                recovery.probes += 1;
                if let Err(mut error) = parse_module(&source[tokens[start].start..end]) {
                    error.line += tokens[start].line - 1;
                    recovery.push(error.in_file(&file.path), limit);
                }
            }
        }
    }
    if syntax_failed {
        return recovery;
    }
    let Ok(candidate) = project.with_overlays(overlays) else {
        return recovery;
    };
    let visits = candidate
        .files
        .iter()
        .enumerate()
        .flat_map(|(file, f)| (0..f.parsed.functions.len()).map(move |function| (file, function)))
        .collect::<Vec<_>>();
    for (file, function) in visits {
        if recovery.probes == 32 {
            recovery.capped = true;
            break;
        }
        let Ok(mut probe) = candidate.with_overlays(&BTreeMap::new()) else {
            break;
        };
        std::sync::Arc::make_mut(&mut probe.files[file].parsed)
            .functions
            .rotate_left(function);
        probe.files.rotate_left(file);
        // Project::map_error relies on monotonically increasing bundle offsets.
        // Recompute only the detached probe's offsets after changing visit order;
        // the AST's lines and every diagnostic remain local to the original file.
        let mut offset = 0;
        for file in &mut probe.files {
            file.offset = offset;
            offset += file.source.lines().count() + 2;
        }
        recovery.probes += 1;
        if let Err(error) = probe.checked(false) {
            recovery.push(error, limit);
        }
    }
    recovery
}

fn parse_file_fingerprints(request: &Value) -> Result<Option<BTreeMap<String, String>>> {
    let Some(map_val) = request
        .get("file_fingerprints")
        .or_else(|| request.get("expected_file_fingerprints"))
    else {
        return Ok(None);
    };
    if map_val.is_null() {
        return Ok(None);
    }
    let obj = map_val
        .as_object()
        .ok_or_else(|| Error::new(0, "file_fingerprints must be an object"))?;
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        let s = v
            .as_str()
            .ok_or_else(|| Error::new(0, "file_fingerprints values must be strings"))?;
        out.insert(k.clone(), s.to_owned());
    }
    Ok(Some(out))
}

pub fn serve(path: &Path) -> Result<()> {
    let project = Project::load(path)?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    const LIMIT: usize = 16 * 1024 * 1024;
    loop {
        // Bound memory even if a client never sends a newline.
        let mut line = Vec::new();
        loop {
            let buf = input.fill_buf().map_err(|e| Error::new(0, e.to_string()))?;
            if buf.is_empty() {
                break;
            }
            let n = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if line.len() + n > LIMIT {
                return Err(Error::new(0, "agent request exceeds 16 MiB limit"));
            }
            let end = buf[n - 1] == b'\n';
            line.extend_from_slice(&buf[..n]);
            input.consume(n);
            if end {
                break;
            }
        }
        if line.is_empty() {
            return Ok(());
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => handle(&project, &request),
            Err(error) => {
                json!({"protocol":1,"ok":false,"diagnostics":[{"severity":"error","message":format!("invalid JSON: {error}")}]})
            }
        };
        writeln!(output, "{response}")
            .and_then(|_| output.flush())
            .map_err(|e| Error::new(0, e.to_string()))?;
    }
}
