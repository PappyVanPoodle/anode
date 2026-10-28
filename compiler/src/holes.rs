//! Pinned synthesis over complete project snapshots; writes require explicit apply.
//! Typed assumptions are diagnostic-only and can never produce backend IR.
use crate::{
    navigation::{self, HoleSyntax},
    project::{ParseCache, Project},
    syntax::*,
    Error, Result,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

fn relative(project: &Project, index: usize) -> String {
    project.files[index].source_key.clone()
}
fn id(hole: &HoleSyntax) -> String {
    format!("{}#{}", hole.function, hole.ordinal)
}
fn fingerprint(project: &Project, hole: &HoleSyntax) -> String {
    navigation::source_fingerprint(&format!(
        "{}\n{}\n{}",
        id(hole),
        navigation::source_fingerprint(&project.files[hole.file_index].source),
        &project.files[hole.file_index].source[hole.range.start.byte..hole.range.end.byte]
    ))
}
fn syntax_type(project: &Project, hole: &HoleSyntax) -> String {
    if hole.expected_type == "Self" {
        let f = &project.files[hole.file_index].parsed.functions[hole.function_index];
        if let Some(method) = &f.method {
            if let Type::Named(i) = method.owner {
                return project.files[hole.file_index].parsed.type_names[i].clone();
            }
        }
    }
    hole.expected_type.clone()
}
fn file_fingerprints(project: &Project) -> BTreeMap<String, String> {
    project
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            (
                relative(project, i),
                navigation::source_fingerprint(&f.source),
            )
        })
        .collect()
}
fn source_type(ty: Type, names: &[String]) -> String {
    if let Some(result) = ty.task_result() {
        return format!("task<{}>", source_type(result, names));
    }
    match ty {
        Type::Named(i) => names.get(i).cloned().unwrap_or_else(|| "unknown".into()),
        Type::Id(i) => format!("id({})", names.get(i).map_or("unknown", String::as_str)),
        Type::VectorView(i) => format!("[{}]", names.get(i).map_or("unknown", String::as_str)),
        _ => ty.label(),
    }
}
fn has_main(project: &Project) -> bool {
    project.files.iter().any(|file| {
        file.module.is_empty()
            && file
                .parsed
                .functions
                .iter()
                .any(|f| f.name == "main" && f.method.is_none() && !f.is_test)
    })
}

fn canonical_expr(expr: &Expr, names: &[String]) -> Value {
    let child = |e: &Expr| canonical_expr(e, names);
    match &expr.kind {
        ExprKind::Int(n) => json!(["int", n]),
        ExprKind::Float(n) => json!(["float_bits", n]),
        ExprKind::Bool(n) => json!(["bool", n]),
        ExprKind::Text(n) => json!(["text", n]),
        ExprKind::Hole(ty, intent) => json!(["hole", source_type(*ty, names), intent]),
        ExprKind::Var(n) => json!(["variable", n]),
        ExprKind::Borrow(n, mutable) => json!(["borrow", n, mutable]),
        ExprKind::Array(items) => json!(["array", items.iter().map(child).collect::<Vec<_>>()]),
        ExprKind::Repeat(e, n) => json!(["repeat", child(e), n]),
        ExprKind::Call(n, args) => json!(["call", n, args.iter().map(child).collect::<Vec<_>>()]),
        ExprKind::MethodCall(e, n, args) => json!([
            "method",
            child(e),
            n,
            args.iter().map(child).collect::<Vec<_>>()
        ]),
        ExprKind::Spawn(n, args) => json!(["spawn", n, args.iter().map(child).collect::<Vec<_>>()]),
        ExprKind::Unary(op, e) => json!(["unary", op, child(e)]),
        ExprKind::Try(e) => json!(["try", child(e)]),
        ExprKind::Binary(op, a, b) => json!(["binary", op, child(a), child(b)]),
        ExprKind::Field(e, n) => json!(["field", child(e), n]),
        ExprKind::Record(n, fields) => json!([
            "record",
            n,
            fields
                .iter()
                .map(|(n, e)| json!([n, child(e)]))
                .collect::<Vec<_>>()
        ]),
    }
}

pub(crate) fn snapshot_sha256(project: &Project) -> String {
    // Length-delimited JSON avoids concatenation ambiguities. Paths are relative,
    // so identical project snapshots have the same receipt on different hosts.
    let files = project
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (relative(project, i), f.source.as_str()))
        .collect::<BTreeMap<_, _>>();
    crate::backend::sha256(
        json!({"manifest":project.manifest,"managed":project.managed,"files":files})
            .to_string()
            .as_bytes(),
    )
}

/// Canonical v1 contract skeleton. Locations, comments and function body tokens
/// are excluded, except unresolved hole signatures. All declaration visibility,
/// imports, field layouts, views, effects, intents and contracts are pinned.
/// A receipt is a source contract identity, never evidence that holes are valid.
pub fn skeleton_receipt(project: &Project) -> Result<Value> {
    let sites = navigation::hole_syntax(project)?;
    skeleton_receipt_sites(project, &sites)
}
fn skeleton_receipt_sites(project: &Project, sites: &[HoleSyntax]) -> Result<Value> {
    let mut modules = BTreeMap::new();
    for (index, file) in project.files.iter().enumerate() {
        let names = &file.parsed.type_names;
        let ty = |t| source_type(t, names);
        let declarations = file.parsed.types.iter().map(|d| {
            let (kind, fields) = match &d.kind {
                DataKind::Record(f) => ("struct",f),
                DataKind::Enum(f) => ("enum",f),
                DataKind::Vector(f) => ("vector",f),
                DataKind::SoaVector(f) => ("soa_vector",f),
                DataKind::Map(f) => ("map",f),
                DataKind::Arena(f) => ("arena",f),
                DataKind::Channel(f) => ("channel",f),
            };
            json!({"name":d.name,"public":d.public,"kind":kind,"fields":fields.iter().map(|f|json!([f.name,ty(f.ty)])).collect::<Vec<_>>()})
        }).collect::<Vec<_>>();
        let functions = file.parsed.functions.iter().map(|f| {
            let mut effects = f.effects.clone(); effects.sort(); effects.dedup();
            json!({"name":f.name,"owner":f.method.as_ref().map(|m|ty(m.owner)),"public":f.public,"test":f.is_test,"foreign":f.external_symbol,"parameters":f.params.iter().map(|p|json!({"name":p.name,"type":ty(p.ty),"mutable":p.mutable,"borrow":format!("{:?}",p.borrow),"view":p.view.as_ref().map(|v|v.iter().map(|f|json!([f.path,f.mutable])).collect::<Vec<_>>())})).collect::<Vec<_>>(),"returns":ty(f.ret),"effects":effects,"intents":f.intents,"requires":f.requires.iter().map(|e|canonical_expr(e,names)).collect::<Vec<_>>(),"ensures":f.ensures.iter().map(|e|canonical_expr(e,names)).collect::<Vec<_>>()})
        }).collect::<Vec<_>>();
        let holes = sites.iter().filter(|s|s.file_index==index).map(|s|json!({"id":id(s),"type":crate::fmt::tokenize(&syntax_type(project,s)).into_iter().filter(|t|!t.comment).map(|t|t.text).collect::<Vec<_>>(),"intent":s.intent,"in_contract":s.in_contract})).collect::<Vec<_>>();
        modules.insert(relative(project,index), json!({"module":file.module,"imports":file.parsed.modules.iter().map(|m|&m.name).collect::<Vec<_>>(),"declarations":declarations,"functions":functions,"holes":holes}));
    }
    let canonical = json!({"schema":"anode.contract-skeleton.v1","managed":project.managed,"manifest":project.manifest,"modules":modules});
    let file_sha256 = project
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            (
                relative(project, i),
                crate::backend::sha256(f.source.as_bytes()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(
        json!({"schema":"anode.contract-skeleton.v1","skeleton_sha256":crate::backend::sha256(canonical.to_string().as_bytes()),"snapshot_sha256":snapshot_sha256(project),"snapshot_fingerprint":navigation::project_fingerprint(project),"manifest_sha256":crate::backend::sha256(project.manifest.as_bytes()),"file_sha256":file_sha256,"file_fingerprints":file_fingerprints(project),"canonical":canonical,"writes":0,"executed":false,"validated":false,"evidence":"source contract identity only; unresolved holes are not executable proofs"}),
    )
}

pub fn list_holes(project: &Project) -> Result<Value> {
    list_holes_cached(project, &mut ParseCache::new(project))
}
fn list_holes_cached(project: &Project, cache: &mut ParseCache<'_>) -> Result<Value> {
    let sites = cache.hole_syntax(project)?;
    let receipt = skeleton_receipt_sites(project, &sites)?;
    let probes = project
        .probe_holes()
        .unwrap_or_else(|error| crate::checker::HoleProbeReport {
            holes: Vec::new(),
            diagnostics: vec![error],
        });
    let mut functions = Vec::new();
    for (i, file) in project.files.iter().enumerate() {
        for (j, f) in file
            .parsed
            .functions
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_test && (f.name != "main" || f.method.is_some()))
        {
            let owner = f
                .method
                .as_ref()
                .map(|m| source_type(m.owner, &file.parsed.type_names));
            let function = f
                .method
                .as_ref()
                .map_or(f.name.as_str(), |m| m.name.as_str());
            let qualified =
                navigation::synthesis_function_name(&file.module, function, owner.as_deref());
            let prefix = if file.module.is_empty() {
                "crate::".into()
            } else {
                format!("{}::", file.module)
            };
            let name = qualified
                .strip_prefix(prefix.as_str())
                .unwrap_or(&qualified)
                .to_owned();
            functions.push(json!({"symbol":crate::project::function_symbol(i,j,&file.module,f),"qualified_name":qualified,"module":file.module,"name":name,"public":f.public,"foreign":f.external_symbol.is_some(),"parameters":f.params.iter().map(|p|json!({"name":p.name,"type":source_type(p.ty,&file.parsed.type_names),"borrow":format!("{:?}",p.borrow),"view":p.view.as_ref().map(|v|v.iter().map(|field|json!({"path":field.path,"mutable":field.mutable})).collect::<Vec<_>>())})).collect::<Vec<_>>(),"returns":source_type(f.ret,&file.parsed.type_names),"effects":f.effects,"requires":f.requires.iter().map(|e|canonical_expr(e,&file.parsed.type_names)).collect::<Vec<_>>(),"ensures":f.ensures.iter().map(|e|canonical_expr(e,&file.parsed.type_names)).collect::<Vec<_>>()}));
        }
    }
    let holes = sites.iter().map(|site| {
        let file = &project.files[site.file_index];
        let f = &file.parsed.functions[site.function_index];
        let symbol = crate::project::function_symbol(site.file_index,site.function_index,&file.module,f);
        // Probe traversal may revisit ensures on several returns. Source slots
        // are always lexical; diagnostic traversal ordinals are never edit IDs.
        let same_site=|other:&&HoleSyntax|other.file_index==site.file_index&&other.function_index==site.function_index&&other.range.start.line==site.range.start.line&&other.intent==site.intent;
        let peers=sites.iter().filter(same_site).collect::<Vec<_>>();
        let ambiguous=peers.iter().any(|s|s.in_contract)&&peers.iter().any(|s|!s.in_contract);
        let occurrence=peers.iter().take_while(|s|s.range.start.byte!=site.range.start.byte).count();
        let probe=if ambiguous{None}else{probes.holes.iter().filter(|p|p.function==symbol&&p.line==site.range.start.line+file.offset&&p.intent==site.intent).nth(if site.in_contract{0}else{occurrence})};
        let context = probe.map(|p|p.context.clone());
        json!({"id":id(site),"function":site.function,"file":relative(project,site.file_index),"path":file.path.to_string_lossy(),"range":site.range.to_json(),"expected_type":syntax_type(project,site),"intent":site.intent,"function_intents":f.intents,"effect_contracts":f.effects,"intents_are_advisory":true,"requires":site.requires,"ensures":site.ensures,"in_contract":site.in_contract,"active_views":f.params.iter().filter(|p|p.borrow!=Borrow::Owned).map(|p|json!({"parameter":p.name,"borrow":format!("{:?}",p.borrow),"fields":p.view.as_ref().map(|v|v.iter().map(|field|json!({"path":field.path,"mutable":field.mutable})).collect::<Vec<_>>())})).collect::<Vec<_>>(),"checked_context":context,"context_available":probe.is_some(),"in_scope_functions":functions.iter().filter(|v|v["module"]==file.module || v["public"]==true).collect::<Vec<_>>(),"file_fingerprint":navigation::source_fingerprint(&file.source),"hole_fingerprint":fingerprint(project,site)})
    }).collect::<Vec<_>>();
    Ok(
        json!({"writes":0,"executed":false,"coordinate_encoding":navigation::COORDINATE_ENCODING,"skeleton_sha256":receipt["skeleton_sha256"],"snapshot_sha256":receipt["snapshot_sha256"],"skeleton_receipt":receipt,"snapshot_fingerprint":navigation::project_fingerprint(project),"file_fingerprints":file_fingerprints(project),"holes":holes,"diagnostics":probes.diagnostics.iter().map(crate::agent::diagnostic).collect::<Vec<_>>(),"validated":false,"status":if sites.is_empty(){"no_holes"}else{"incomplete"},"assumptions":"unresolved holes have their declared types only; no executable IR is returned"}),
    )
}

#[derive(Clone)]
struct Proposal {
    site: HoleSyntax,
    text: String,
    helper: String,
    typed_text: String,
    typed_helper: String,
}
fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn prepare(
    project: &Project,
    site: &HoleSyntax,
    candidate: &Value,
    index: usize,
) -> Result<Proposal> {
    if site.in_contract {
        let mut error = Error::new(
            site.range.start.line,
            "contracts are locked; synthesis replacements must be in a function body",
        );
        error.details = Some(
            json!({"kind":"unsupported_boundary","boundary":"requires_or_ensures","hole_id":id(site)}),
        );
        return Err(error);
    }
    let text = candidate
        .get("replacement")
        .or_else(|| candidate.get("body"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::new(
                0,
                "candidate requires replacement (expression or block source)",
            )
        })?;
    if text.len() > 65536 {
        return Err(Error::new(0, "candidate exceeds 64 KiB"));
    }
    if crate::fmt::tokenize(text)
        .iter()
        .any(|t| !t.comment && t.text == "??")
    {
        return Err(Error::new(0, "candidate contains an unresolved typed hole"));
    }
    let ty = syntax_type(project, site);
    let nonce = fingerprint(project, site).replace([':', '=', '-'], "_");
    let helper_name = format!("anode_synthesis_{index}_{nonce}");
    if project.files[site.file_index].source.contains(&helper_name) {
        return Err(Error::new(0, "generated helper name collides with source"));
    }
    let kind = candidate
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("expression");
    let (replacement, helper) = match kind {
        "expression" => {
            let wrapper = format!("fn {helper_name}() -> {ty} {{ return {text}; }}");
            let parsed = parse_module(&wrapper)?;
            if parsed.functions.len() != 1
                || !parsed.types.is_empty()
                || !parsed.modules.is_empty()
                || !parsed.implementations.is_empty()
                || !matches!(
                    parsed.functions[0].body.as_slice(),
                    [Stmt {
                        kind: StmtKind::Return(Some(_)),
                        ..
                    }]
                )
            {
                return Err(Error::new(
                    0,
                    "candidate must contain exactly one expression",
                ));
            }
            // Preserve the expression slot's precedence in its original parent.
            (format!("({text})"), String::new())
        }
        "block" => {
            let captures = candidate
                .get("captures")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::new(
                        0,
                        "block candidates require an explicit captures array (possibly empty)",
                    )
                })?;
            if captures.len() > 64 {
                return Err(Error::new(0, "at most 64 explicit block captures"));
            }
            let mut names = BTreeSet::new();
            let mut params = Vec::new();
            let mut args = Vec::new();
            for capture in captures {
                let name = capture
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "capture requires name"))?;
                let capture_type = capture
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(0, "capture requires its source type"))?;
                if crate::fmt::tokenize(capture_type).iter().any(|t| {
                    t.comment
                        || t.text.chars().any(|c| {
                            !(c.is_ascii_alphanumeric()
                                || c.is_whitespace()
                                || "_:[];,.{}".contains(c))
                        })
                }) {
                    return Err(Error::new(
                        0,
                        "capture type must contain only type and field-view syntax",
                    ));
                }
                if !identifier(name) || !names.insert(name) {
                    return Err(Error::new(0, "capture names must be distinct identifiers"));
                }
                // Parse all type/view syntax as a signature, then lock the resulting
                // parameter count and names below. No raw source escapes the helper.
                let (prefix, arg) = match capture
                    .get("borrow")
                    .and_then(Value::as_str)
                    .unwrap_or("owned")
                {
                    "owned" => ("", name.to_owned()),
                    "shared" => ("&", format!("&{name}")),
                    "exclusive" => ("&mut ", format!("&mut {name}")),
                    _ => {
                        return Err(Error::new(
                            0,
                            "capture borrow must be owned, shared, or exclusive",
                        ))
                    }
                };
                params.push(format!("{name}: {prefix}{capture_type}"));
                args.push(arg);
            }
            let helper = format!("\nfn {helper_name}({}) -> {ty} {text}\n", params.join(", "));
            let parsed = parse_module(&helper)?;
            if parsed.functions.len() != 1
                || !parsed.types.is_empty()
                || !parsed.modules.is_empty()
                || !parsed.implementations.is_empty()
            {
                return Err(Error::new(
                    0,
                    "block must be exactly one braced function body",
                ));
            }
            let f = &parsed.functions[0];
            if f.name != helper_name
                || f.params.len() != captures.len()
                || f.params
                    .iter()
                    .zip(captures)
                    .any(|(p, c)| Some(p.name.as_str()) != c.get("name").and_then(Value::as_str))
                || !f.requires.is_empty()
                || !f.ensures.is_empty()
                || !f.effects.is_empty()
                || !f.intents.is_empty()
                || f.public
                || f.is_test
            {
                return Err(Error::new(
                    0,
                    "block candidate changed its locked helper signature or contracts",
                ));
            }
            (format!("{helper_name}({})", args.join(", ")), helper)
        }
        _ => return Err(Error::new(0, "candidate kind must be expression or block")),
    };
    let identity = format!("{helper_name}_type");
    let typed_helper =
        format!("{helper}\n@effect(no_alloc, no_io, bounded)\nfn {identity}(value: {ty}) -> {ty} {{ return value; }}\n");
    Ok(Proposal {
        site: site.clone(),
        typed_text: format!("{identity}({replacement})"),
        text: replacement,
        helper,
        typed_helper,
    })
}
fn overlays(
    project: &Project,
    proposals: &[Proposal],
    typed: bool,
) -> Result<BTreeMap<String, String>> {
    let mut grouped = BTreeMap::<usize, Vec<&Proposal>>::new();
    for p in proposals {
        grouped.entry(p.site.file_index).or_default().push(p);
    }
    let mut result = BTreeMap::new();
    for (index, mut items) in grouped {
        items.sort_by_key(|p| std::cmp::Reverse(p.site.range.start.byte));
        let mut source = project.files[index].source.clone();
        let mut last = source.len();
        for p in &items {
            let range = p.site.range;
            if range.end.byte > last
                || !source.is_char_boundary(range.start.byte)
                || !source.is_char_boundary(range.end.byte)
            {
                return Err(Error::new(0, "overlapping or invalid UTF-8 hole ranges"));
            }
            source.replace_range(
                range.start.byte..range.end.byte,
                if typed { &p.typed_text } else { &p.text },
            );
            last = range.start.byte;
        }
        for p in items {
            source.push_str(if typed { &p.typed_helper } else { &p.helper });
        }
        if source.len() > crate::project::SOURCE_FILE_LIMIT {
            return Err(Error::new(0, "composed source exceeds 16 MiB"));
        }
        result.insert(relative(project, index), source);
    }
    Ok(result)
}
fn check(
    project: &Project,
    sources: &BTreeMap<String, String>,
    cache: &mut ParseCache<'_>,
) -> Result<Value> {
    let snapshot = project.with_overlays_cached(sources, cache)?;
    let unresolved = cache.hole_syntax(&snapshot)?.len();
    if unresolved == 0 {
        let checked = if has_main(&snapshot) {
            snapshot.checked(false).map(|(program, _)| program)
        } else {
            snapshot.checked_reload()
        };
        match checked {
            Ok(_) => {
                Ok(json!({"validated":true,"status":"valid","unresolved_holes":0,"diagnostics":[]}))
            }
            Err(e) => Ok(
                json!({"validated":false,"status":"invalid","unresolved_holes":0,"diagnostics":[crate::agent::diagnostic(&e)]}),
            ),
        }
    } else {
        let report = snapshot.probe_holes()?;
        Ok(
            json!({"validated":false,"status":if report.diagnostics.is_empty(){"conditional"}else{"invalid"},"conditional_on_unresolved_holes":true,"unresolved_holes":unresolved,"diagnostics":report.diagnostics.iter().map(crate::agent::diagnostic).collect::<Vec<_>>()}),
        )
    }
}
fn combined_check(
    project: &Project,
    proposals: &[Proposal],
    cache: &mut ParseCache<'_>,
) -> Result<Value> {
    let typed = check(project, &overlays(project, proposals, true)?, cache)?;
    if typed["status"] == "invalid" {
        return Ok(typed);
    }
    check(project, &overlays(project, proposals, false)?, cache)
}

pub fn verify_holes(project: &Project, request: &Value) -> Result<Value> {
    verify_holes_limit(project, request, 32, &mut ParseCache::new(project))
}

fn verify_holes_limit(
    project: &Project,
    request: &Value,
    limit: usize,
    cache: &mut ParseCache<'_>,
) -> Result<Value> {
    let expected = request
        .get("snapshot_fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::new(
                0,
                "verify_holes requires snapshot_fingerprint from list_holes",
            )
        })?;
    if expected != navigation::project_fingerprint(project) {
        return Err(Error::new(
            0,
            "stale project snapshot; enumerate holes again",
        ));
    }
    if let Some(fps) = request.get("file_fingerprints") {
        let actual = file_fingerprints(project);
        for (file, fp) in fps
            .as_object()
            .ok_or_else(|| Error::new(0, "file_fingerprints must be an object"))?
        {
            if actual.get(file).map(String::as_str) != fp.as_str() {
                return Err(Error::new(0, format!("stale file fingerprint for {file}")));
            }
        }
    }
    let sites = cache.hole_syntax(project)?;
    let candidates = request
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new(0, "verify_holes requires candidates array"))?;
    if candidates.is_empty() || candidates.len() > limit {
        return Err(Error::new(
            0,
            format!("provide 1 to {limit} hole candidates"),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut proposals = Vec::new();
    let mut independent = Vec::new();
    let mut preparation_failed = false;
    for (index, candidate) in candidates.iter().enumerate() {
        let candidate_id = candidate
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(0, "candidate requires stable hole id"))?;
        if !seen.insert(candidate_id) {
            return Err(Error::new(0, "duplicate hole id in candidate composition"));
        }
        let site = sites
            .iter()
            .find(|h| id(h) == candidate_id)
            .ok_or_else(|| Error::new(0, format!("stale or unknown hole id {candidate_id}")))?;
        if candidate
            .get("range")
            .is_some_and(|range| range != &site.range.to_json())
        {
            return Err(Error::new(
                0,
                "candidate range does not match the exact UTF-8 hole span; enumerate holes again",
            ));
        }
        if candidate.get("hole_fingerprint").and_then(Value::as_str)
            != Some(fingerprint(project, site).as_str())
        {
            return Err(Error::new(
                0,
                format!("stale or missing hole_fingerprint for {candidate_id}"),
            ));
        }
        match prepare(project, site, candidate, index)
            .and_then(|p| Ok((combined_check(project, std::slice::from_ref(&p), cache)?, p)))
        {
            Ok((result, p)) => {
                independent.push(json!({"id":candidate_id,"result":result}));
                proposals.push(p);
            }
            Err(e) => {
                preparation_failed = true;
                independent.push(json!({"id":candidate_id,"result":{"validated":false,"status":"invalid","diagnostics":[crate::agent::diagnostic(&e)]}}));
            }
        }
    }
    let composed = if preparation_failed {
        json!({"validated":false,"status":"invalid","diagnostics":[{"message":"composition includes rejected candidates"}]})
    } else {
        combined_check(project, &proposals, cache).unwrap_or_else(|e|json!({"validated":false,"status":"invalid","diagnostics":[crate::agent::diagnostic(&e)]}))
    };
    let sources = if preparation_failed {
        BTreeMap::new()
    } else {
        overlays(project, &proposals, false)?
    };
    let mut edits=proposals.iter().map(|p|json!({"id":id(&p.site),"file":relative(project,p.site.file_index),"range":p.site.range.to_json(),"replacement":p.text,"hole_fingerprint":fingerprint(project,&p.site)})).collect::<Vec<_>>();
    let mut helper_edits = BTreeMap::<usize, Vec<&Proposal>>::new();
    for proposal in &proposals {
        if !proposal.helper.is_empty() {
            helper_edits
                .entry(proposal.site.file_index)
                .or_default()
                .push(proposal);
        }
    }
    for (index, mut items) in helper_edits {
        items.sort_by_key(|p| std::cmp::Reverse(p.site.range.start.byte));
        let source = &project.files[index].source;
        let line = source.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = source.len() - source.rfind('\n').map_or(0, |i| i + 1) + 1;
        let position = json!({"byte":source.len(),"line":line,"column":column});
        edits.push(json!({"kind":"private_helper","file":relative(project,index),"range":{"start":position,"end":position},"replacement":items.into_iter().map(|p|p.helper.as_str()).collect::<String>()}));
    }
    edits.sort_by(|a, b| {
        a["file"].as_str().cmp(&b["file"].as_str()).then_with(|| {
            b["range"]["start"]["byte"]
                .as_u64()
                .cmp(&a["range"]["start"]["byte"].as_u64())
        })
    });
    Ok(
        json!({"writes":0,"executed":false,"coordinate_encoding":navigation::COORDINATE_ENCODING,"snapshot_fingerprint":expected,"document_versions":request.get("document_versions"),"independent":independent,"composed":composed,"edits":edits,"files":sources.into_iter().map(|(file,text)|json!({"file":file,"text":text,"source_fingerprint":navigation::source_fingerprint(&text)})).collect::<Vec<_>>(),"helpers":proposals.iter().filter(|p|!p.helper.is_empty()).map(|p|json!({"file":relative(project,p.site.file_index),"text":p.helper,"placement":"append_at_eof"})).collect::<Vec<_>>(),"locked_boundaries":["imports","function_signatures","views","effects","requires","ensures"],"validation_scope":"full_project; conditional results are not proofs"}),
    )
}

fn bool_option(request: &Value, name: &str) -> Result<bool> {
    request.get(name).map_or(Ok(false), |v| {
        v.as_bool()
            .ok_or_else(|| Error::new(0, format!("{name} must be a boolean")))
    })
}

/// Verify independent workers and their exact hole-only composition. This API
/// always writes zero files, even if `apply` is supplied; use `commit_holes`.
pub fn verify_batch(project: &Project, request: &Value) -> Result<Value> {
    verify_batch_limit(project, request, 32, &mut ParseCache::new(project))
}

fn verify_batch_limit(
    project: &Project,
    request: &Value,
    limit: usize,
    cache: &mut ParseCache<'_>,
) -> Result<Value> {
    bool_option(request, "apply")?;
    let sites = cache.hole_syntax(project)?;
    let receipt = skeleton_receipt_sites(project, &sites)?;
    for key in ["skeleton_sha256", "snapshot_sha256"] {
        let expected = request.get(key).and_then(Value::as_str).ok_or_else(|| {
            Error::new(
                0,
                format!("verify_batch requires {key} from skeleton_receipt or list_holes"),
            )
        })?;
        if receipt[key].as_str() != Some(expected) {
            return Err(Error::new(
                0,
                format!("stale {key}; enumerate the contract skeleton again"),
            ));
        }
    }
    let candidates = request
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new(0, "verify_batch requires candidates array"))?;
    // 1.3: pinned batches and campaigns accept expression and captured-block
    // candidates. A block's signature-locked private helper is appended at EOF
    // by `overlays`, and its explicit captures are the only values it can see.
    for c in candidates {
        match c.get("kind").map(Value::as_str) {
            None | Some(Some("expression")) => {
                if c.get("captures").is_some() {
                    return Err(Error::new(
                        0,
                        "captures are only valid for block candidates",
                    ));
                }
            }
            Some(Some("block")) => {
                if !c.get("captures").is_some_and(Value::is_array) {
                    return Err(Error::new(
                        0,
                        "block candidates in pinned batches require an explicit captures array (possibly empty)",
                    ));
                }
            }
            _ => return Err(Error::new(0, "candidate kind must be expression or block")),
        }
    }
    let mut legacy = request.clone();
    legacy["snapshot_fingerprint"] = receipt["snapshot_fingerprint"].clone();
    // An optional old pin must still be honored, rather than silently replaced.
    if request
        .get("snapshot_fingerprint")
        .is_some_and(|v| v != &receipt["snapshot_fingerprint"])
    {
        return Err(Error::new(
            0,
            "stale snapshot_fingerprint; enumerate holes again",
        ));
    }
    let mut result = verify_holes_limit(project, &legacy, limit, cache)?;
    result["skeleton_sha256"] = receipt["skeleton_sha256"].clone();
    result["snapshot_sha256"] = receipt["snapshot_sha256"].clone();
    result["applied"] = json!(false);
    // Captured-block candidates also append their private helpers at EOF.
    result["exact_hole_only"] = json!(result["helpers"]
        .as_array()
        .is_none_or(|helpers| helpers.is_empty()));
    result["commit_eligible"] = json!(
        result["composed"]["status"] == "valid"
            && result["independent"].as_array().is_some_and(|items| items
                .iter()
                .all(|item| item["result"]["status"] != "invalid"))
    );
    if result["commit_eligible"] == true {
        let sources = result["files"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                (
                    f["file"].as_str().unwrap().to_owned(),
                    f["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        result["proposed_snapshot_sha256"] = json!(snapshot_sha256(
            &project.with_overlays_cached(&sources, cache)?
        ));
    }
    if let Some(verification) = request.get("verify_contracts") {
        if !verification.is_object() {
            return Err(Error::new(0, "verify_contracts must be an options object"));
        }
        let mut options = crate::verify::VerificationOptions::from_request(verification)?;
        let completed = result["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["id"].as_str())
            .filter_map(|s| s.rsplit_once('#').map(|(f, _)| f.to_owned()))
            .collect::<BTreeSet<_>>();
        if options.functions.is_empty() {
            options.functions = completed.into_iter().collect();
        }
        if result["composed"]["status"] == "valid" {
            let sources = result["files"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    Ok((
                        f["file"]
                            .as_str()
                            .ok_or_else(|| Error::new(0, "missing composed file"))?
                            .into(),
                        f["text"]
                            .as_str()
                            .ok_or_else(|| Error::new(0, "missing composed text"))?
                            .into(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let evidence = crate::verify::verify_contracts(
                &project.with_overlays_cached(&sources, cache)?,
                &options,
            )?;
            result["executed"] = evidence["executed"].clone();
            if evidence["status"] == "failed"
                || (options.execute && evidence["status"] != "sampled")
            {
                result["commit_eligible"] = json!(false);
            }
            result["contract_evidence"] = evidence;
        } else {
            result["contract_evidence"] = json!({"status":"deferred","executed":false,"universal_proof":false,"reason":"full normal checking requires every project hole to be filled; typed diagnostic assumptions are never executed"});
        }
    }
    result["transaction_guarantees"] = transaction_guarantees();
    Ok(result)
}

const CAMPAIGN_LIMIT: usize = 4096;

fn campaign_workers(request: &Value) -> Result<usize> {
    let count = request.get("max_workers").map_or(Some(4), Value::as_u64);
    count
        .filter(|n| (1..=16).contains(n))
        .map(|n| n as usize)
        .ok_or_else(|| Error::new(0, "max_workers must be an integer from 1 to 16"))
}

/// Deterministic, snapshot-pinned whole-function assignments. A worker can only
/// propose expression or captured-block replacements at the exact holes listed
/// in its brief (a block adds only its own signature-locked private helper).
/// Whole-function groups prevent separate workers from editing one function;
/// the largest groups are assigned first to the least-loaded worker.
pub fn plan_campaign(project: &Project, request: &Value) -> Result<Value> {
    plan_campaign_cached(project, request, &mut ParseCache::new(project))
}
fn plan_campaign_cached(
    project: &Project,
    request: &Value,
    cache: &mut ParseCache<'_>,
) -> Result<Value> {
    let max_workers = campaign_workers(request)?;
    let listing = list_holes_cached(project, cache)?;
    for key in ["skeleton_sha256", "snapshot_sha256", "snapshot_fingerprint"] {
        if request.get(key).is_some_and(|pin| pin != &listing[key]) {
            return Err(Error::new(
                0,
                format!("stale or invalid {key} for campaign planning"),
            ));
        }
    }
    if let Some(pins) = request.get("file_fingerprints") {
        for (file, pin) in pins
            .as_object()
            .ok_or_else(|| Error::new(0, "file_fingerprints must be an object"))?
        {
            if listing["file_fingerprints"].get(file) != Some(pin) || !pin.is_string() {
                return Err(Error::new(0, format!("stale file fingerprint for {file}")));
            }
        }
    }
    let holes = listing["holes"].as_array().unwrap();
    if holes.len() > CAMPAIGN_LIMIT {
        return Err(Error::new(
            0,
            format!("campaign exceeds {CAMPAIGN_LIMIT} holes"),
        ));
    }
    let mut grouped = BTreeMap::<String, Vec<Value>>::new();
    for hole in holes {
        grouped
            .entry(hole["function"].as_str().unwrap().to_owned())
            .or_default()
            .push(hole.clone());
    }
    let mut groups = grouped.into_iter().collect::<Vec<_>>();
    groups.sort_by(|(an, a), (bn, b)| b.len().cmp(&a.len()).then(an.cmp(bn)));
    let mut assignments = vec![Vec::<Value>::new(); max_workers.min(groups.len())];
    for (_, group) in groups {
        let worker = assignments
            .iter()
            .enumerate()
            .min_by_key(|(i, holes)| (holes.len(), *i))
            .unwrap()
            .0;
        assignments[worker].extend(group);
    }
    for assignment in &mut assignments {
        assignment.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    }
    let identities = assignments.iter().enumerate().map(|(i, holes)| {
        json!({"worker_id":format!("worker-{}",i+1),"hole_ids":holes.iter().map(|h|&h["id"]).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    let identity = json!({"schema":"anode.campaign.v1","max_workers":max_workers,"skeleton_sha256":listing["skeleton_sha256"],"snapshot_sha256":listing["snapshot_sha256"],"assignments":identities});
    let campaign_id = crate::backend::sha256(identity.to_string().as_bytes());
    let type_declarations = listing["skeleton_receipt"]["canonical"]["modules"]
        .as_object().unwrap().iter().map(|(file,module)|json!({"file":file,"module":module["module"],"declarations":module["declarations"]})).collect::<Vec<_>>();
    let workers = assignments.into_iter().enumerate().map(|(i, holes)| {
        let worker_id = format!("worker-{}",i+1);
        let mut helpers = BTreeMap::new();
        let mut files = BTreeSet::new();
        let mut writable = true;
        for hole in &holes {
            let key = hole["file"].as_str().unwrap();
            files.insert(key.to_owned());
            if project.files.iter().any(|f|f.source_key==key&&!f.package.is_empty()) {
                writable = false;
            }
            for helper in hole["in_scope_functions"].as_array().into_iter().flatten() {
                helpers.insert(helper["qualified_name"].as_str().unwrap().to_owned(),helper.clone());
            }
        }
        json!({"worker_id":worker_id,"campaign_id":campaign_id,"skeleton_sha256":listing["skeleton_sha256"],"snapshot_sha256":listing["snapshot_sha256"],"hole_ids":holes.iter().map(|h|&h["id"]).collect::<Vec<_>>(),"files":files,"root_writable":writable,"brief":{"schema":"anode.campaign-worker.v1","worker_id":worker_id,"campaign_id":campaign_id,"skeleton_sha256":listing["skeleton_sha256"],"snapshot_sha256":listing["snapshot_sha256"],"holes":holes,"type_declarations":type_declarations,"reachable_helper_signatures":helpers.into_values().collect::<Vec<_>>(),"helper_reachability":"signatures callable from the assigned hole scope; ordinary checking resolves and validates actual calls","edit_policy":"expression or captured-block replacements at assigned exact hole spans; signatures, imports, views, effects and contracts are locked","dependency_policy":"dependency proposals are read-only; commit from the dependency's own project","writes":0}})
    }).collect::<Vec<_>>();
    Ok(
        json!({"schema":"anode.campaign.v1","campaign_id":campaign_id,"max_workers":max_workers,"skeleton_sha256":listing["skeleton_sha256"],"snapshot_sha256":listing["snapshot_sha256"],"snapshot_fingerprint":listing["snapshot_fingerprint"],"file_fingerprints":listing["file_fingerprints"],"coordinate_encoding":navigation::COORDINATE_ENCODING,"workers":workers,"total_holes":holes.len(),"assignment_policy":"whole_function_largest_first_least_loaded","status":if holes.is_empty(){"no_holes"}else{"planned"},"writes":0,"executed":false,"universal_proof":false}),
    )
}

/// Validate every response against a freshly reconstructed campaign, then check
/// each worker and the full composition of expression and captured-block
/// candidates. Finite pure contract
/// sampling is enabled by default and covers every newly completed function.
/// Explicit application uses the same locked, fresh-disk transaction as batches.
pub fn verify_campaign(project: &Project, request: &Value) -> Result<Value> {
    let mut cache = ParseCache::new(project);
    bool_option(request, "apply")?;
    let supplied = request
        .get("campaign")
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            Error::new(
                0,
                "verify_campaign requires the unchanged campaign plan object",
            )
        })?;
    let plan = plan_campaign_cached(project, supplied, &mut cache)?;
    for key in [
        "campaign_id",
        "skeleton_sha256",
        "snapshot_sha256",
        "snapshot_fingerprint",
    ] {
        if request
            .get(key)
            .is_some_and(|pin| Some(pin) != plan.get(key))
        {
            return Err(Error::new(0, format!("stale or foreign campaign {key}")));
        }
    }
    if let Some(pins) = request.get("file_fingerprints") {
        plan_campaign_cached(project, &json!({"file_fingerprints":pins}), &mut cache)?;
    }
    let mut verification = request
        .get("verify_contracts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !verification.is_object() {
        return Err(Error::new(0, "verify_contracts must be an options object"));
    }
    if verification.get("execute").is_none() {
        verification["execute"] = json!(true);
    }
    crate::verify::VerificationOptions::from_request(&verification)?;
    for key in [
        "schema",
        "campaign_id",
        "max_workers",
        "skeleton_sha256",
        "snapshot_sha256",
        "workers",
    ] {
        if supplied.get(key) != plan.get(key) {
            return Err(Error::new(
                0,
                format!("stale, foreign or modified campaign {key}; plan the campaign again"),
            ));
        }
    }
    let workers = plan["workers"].as_array().unwrap();
    let responses = request
        .get("responses")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new(0, "verify_campaign requires responses array"))?;
    if responses.len() > workers.len() {
        return Err(Error::new(
            0,
            "campaign has more responses than assigned workers",
        ));
    }
    let mut seen_workers = BTreeSet::new();
    let mut seen_holes = BTreeSet::new();
    let mut candidates = Vec::new();
    let mut independent_workers = Vec::new();
    for response in responses {
        let worker_id = response
            .get("worker_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(0, "campaign response requires worker_id"))?;
        if !seen_workers.insert(worker_id.to_owned()) {
            return Err(Error::new(
                0,
                format!("duplicate campaign worker response {worker_id}"),
            ));
        }
        let worker = workers
            .iter()
            .find(|w| w["worker_id"] == worker_id)
            .ok_or_else(|| {
                Error::new(
                    0,
                    format!("foreign or unassigned campaign worker {worker_id}"),
                )
            })?;
        for key in ["campaign_id", "skeleton_sha256", "snapshot_sha256"] {
            if response.get(key) != plan.get(key) {
                return Err(Error::new(
                    0,
                    format!("stale or foreign {key} in response from {worker_id}"),
                ));
            }
        }
        let batch = response
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new(0, "worker response requires candidates array"))?;
        if batch.is_empty() {
            return Err(Error::new(
                0,
                "worker response must propose at least one assigned hole",
            ));
        }
        for candidate in batch {
            let hole_id = candidate
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::new(0, "campaign candidate requires assigned hole id"))?;
            if !worker["hole_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == hole_id)
            {
                return Err(Error::new(
                    0,
                    format!("hole {hole_id} is unassigned to worker {worker_id}"),
                ));
            }
            if !seen_holes.insert(hole_id.to_owned()) {
                return Err(Error::new(
                    0,
                    format!("duplicate or overlapping campaign hole {hole_id}"),
                ));
            }
        }
        let pinned = json!({"skeleton_sha256":plan["skeleton_sha256"],"snapshot_sha256":plan["snapshot_sha256"],"candidates":batch});
        let independent = verify_batch_limit(project, &pinned, CAMPAIGN_LIMIT, &mut cache)?;
        independent_workers.push(json!({"worker_id":worker_id,"hole_ids":batch.iter().map(|c|&c["id"]).collect::<Vec<_>>(),"independent":independent["independent"],"composed":independent["composed"],"writes":0}));
        candidates.extend(batch.iter().cloned());
    }
    let missing = workers
        .iter()
        .flat_map(|w| w["hole_ids"].as_array().unwrap())
        .filter(|id| !seen_holes.contains(id.as_str().unwrap()))
        .cloned()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        if bool_option(request, "apply")? {
            return Err(Error::new(0, "campaign has no candidates to apply"));
        }
        return Ok(
            json!({"schema":"anode.campaign-receipt.v1","campaign_id":plan["campaign_id"],"skeleton_sha256":plan["skeleton_sha256"],"snapshot_sha256":plan["snapshot_sha256"],"status":"incomplete","workers":independent_workers,"missing_holes":missing,"composed":{"status":"conditional","validated":false},"contract_evidence":{"status":"deferred","executed":false,"universal_proof":false,"reason":"no completed candidate composition"},"commit_eligible":false,"writes":0,"applied":false,"executed":false,"universal_proof":false}),
        );
    }
    let mut selected = candidates
        .iter()
        .filter_map(|c| c["id"].as_str())
        .filter_map(|id| id.rsplit_once('#').map(|(f, _)| f.to_owned()))
        .collect::<BTreeSet<_>>();
    // A caller may add functions, but cannot bypass evidence for a completed one.
    if let Some(names) = verification.get("functions") {
        for name in names
            .as_array()
            .ok_or_else(|| Error::new(0, "functions must be an array"))?
        {
            selected.insert(
                name.as_str()
                    .ok_or_else(|| Error::new(0, "function names must be strings"))?
                    .to_owned(),
            );
        }
    }
    verification["functions"] = json!(selected);
    let pinned = json!({"skeleton_sha256":plan["skeleton_sha256"],"snapshot_sha256":plan["snapshot_sha256"],"candidates":candidates,"verify_contracts":verification});
    let mut result = verify_batch_limit(project, &pinned, CAMPAIGN_LIMIT, &mut cache)?;
    result["schema"] = json!("anode.campaign-receipt.v1");
    result["campaign_id"] = plan["campaign_id"].clone();
    result["workers"] = json!(independent_workers);
    result["missing_holes"] = json!(missing);
    result["universal_proof"] = json!(false);
    result["status"] = json!(if result["composed"]["status"] == "invalid"
        || result["contract_evidence"]["status"] == "failed"
    {
        "failed"
    } else if !missing.is_empty()
        || result["contract_evidence"]["status"] == "incomplete"
        || result["contract_evidence"]["status"] == "deferred"
    {
        "incomplete"
    } else if result["contract_evidence"]["status"] == "planned" {
        "planned"
    } else {
        "sampled"
    });
    if !missing.is_empty() {
        result["commit_eligible"] = json!(false);
    }
    apply_verified_batch(project, request, result)
}

fn transaction_guarantees() -> Value {
    json!({"preflight":"all candidates independently and composed, all source bytes and manifest, before source writes","locking":"cooperative project and per-source locks atomically published as non-overwriting hard links to synced owner metadata; unrelated editors must honor locks or remain quiescent","visibility":"each same-filesystem file rename is atomic; multiple files do not have simultaneous OS-wide visibility","failure":"on an ordinary replacement error all changed files are restored; rollback failure retains journal and locks for recovery","crash":"no crash-atomic multi-file guarantee; synced journal contents and original backups are retained until success; a crash may leave a partial set and stale locks","durability":"file contents are synced before replacement; directory metadata durability depends on the operating system"})
}

struct SourceLocks {
    paths: Vec<std::path::PathBuf>,
    preserve: bool,
}
impl Drop for SourceLocks {
    fn drop(&mut self) {
        if !self.preserve {
            for path in self.paths.iter().rev() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
impl SourceLocks {
    fn acquire(paths: Vec<std::path::PathBuf>, owner: &Value) -> Result<Self> {
        use std::io::Write;
        let mut locks = Self {
            paths: Vec::new(),
            preserve: false,
        };
        let root = paths
            .first()
            .and_then(|p| p.parent())
            .ok_or_else(|| Error::new(0, "missing project lock root"))?;
        let template = root.join(format!("{}.owner", owner["transaction"].as_str().unwrap()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&template)
            .map_err(|e| Error::new(0, e.to_string()).in_file(&template))?;
        locks.paths.push(template.clone());
        file.write_all(owner.to_string().as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| Error::new(0, e.to_string()).in_file(&template))?;
        drop(file);
        // Publishing only fully written metadata closes the otherwise possible
        // crash window between create_new(empty lock) and writing its owner.
        for path in paths {
            std::fs::hard_link(&template,&path).map_err(|e|Error::new(0,format!("source transaction lock unavailable (another writer, recovery, or hard-link support required): {e}")).in_file(&path))?;
            locks.paths.push(path.clone());
        }
        Ok(locks)
    }
}

fn fresh_disk(project: &Project) -> Result<Project> {
    let entry = project
        .files
        .first()
        .ok_or_else(|| Error::new(0, "cannot commit an empty project"))?;
    if !entry.path.is_absolute() {
        return Err(Error::new(
            0,
            "apply requires a file-backed project; in-memory snapshots are dry-run only",
        ));
    }
    let fresh = Project::load(if project.managed {
        &project.root
    } else {
        &entry.path
    })?;
    if snapshot_sha256(&fresh) != snapshot_sha256(project) || fresh.root != project.root {
        return Err(Error::new(
            0,
            "fresh disk sources or manifest changed; refusing stale hole commit",
        ));
    }
    Ok(fresh)
}

fn write_synced(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| Error::new(0, e.to_string()).in_file(path))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| Error::new(0, e.to_string()).in_file(path))
}

/// Explicit guarded disk application. Default is a zero-write dry-run. Source
/// writers using this operation serialize; external writers must honor locks.
/// Ordinary I/O failure rolls back, but a process/OS crash can expose a partial
/// multi-file set. The journal, original backups and stale locks enable recovery.
pub fn commit_holes(project: &Project, request: &Value) -> Result<Value> {
    let result = verify_batch(project, request)?;
    apply_verified_batch(project, request, result)
}

fn apply_verified_batch(project: &Project, request: &Value, mut result: Value) -> Result<Value> {
    let apply = bool_option(request, "apply")?;
    if !apply {
        return Ok(result);
    }
    if result["commit_eligible"] != true {
        return Err(Error::new(0,"batch is invalid, conditional, or requested contract evidence is incomplete; no sources written"));
    }
    if result["files"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| {
            item.get("file").and_then(Value::as_str).is_some_and(|key| {
                project
                    .files
                    .iter()
                    .any(|file| file.source_key == key && !file.package.is_empty())
            })
        })
    {
        return Err(Error::new(
            0,
            "dependency edits are read-only proposals; commit from the dependency's own project",
        ));
    }
    fresh_disk(project)?;
    let mut paths = project
        .files
        .iter()
        .map(|f| {
            f.path.with_file_name(format!(
                "{}.anode-hole.lock",
                f.path.file_name().unwrap().to_string_lossy()
            ))
        })
        .collect::<Vec<_>>();
    if project.managed {
        for name in ["Anode.toml", "Zenith.toml"] {
            if project.root.join(name).exists() {
                paths.push(project.root.join(format!("{name}.anode-hole.lock")));
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths.insert(0, project.root.join(".anode-hole-batch.lock"));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::new(0, e.to_string()))?
        .as_nanos();
    let transaction = format!(".anode-hole-batch-{}-{nonce}", std::process::id());
    let owner = json!({"schema":"anode.hole-lock.v1","pid":std::process::id(),"transaction":transaction,"snapshot_sha256":result["snapshot_sha256"]});
    let mut locks = SourceLocks::acquire(paths, &owner)?;
    fresh_disk(project)?;
    let journal = project.root.join(&transaction);
    std::fs::create_dir(&journal).map_err(|e| Error::new(0, e.to_string()).in_file(&journal))?;
    let mut artifacts = Vec::new();
    let outcome = (|| -> Result<usize> {
        let files = result["files"]
            .as_array()
            .ok_or_else(|| Error::new(0, "batch has no composed sources"))?;
        let mut targets = Vec::new();
        for (i, item) in files.iter().enumerate() {
            let relative_path = item["file"]
                .as_str()
                .ok_or_else(|| Error::new(0, "missing batch file"))?;
            let source = project
                .files
                .iter()
                .enumerate()
                .find(|(i, _)| relative(project, *i) == relative_path)
                .map(|(_, f)| f)
                .ok_or_else(|| Error::new(0, "batch file is outside original project"))?;
            let metadata = std::fs::metadata(&source.path)
                .map_err(|e| Error::new(0, e.to_string()).in_file(&source.path))?;
            if metadata.permissions().readonly() {
                return Err(
                    Error::new(0, "read-only source; no sources written").in_file(&source.path)
                );
            }
            let backup = journal.join(format!("{i}.original"));
            let staged = journal.join(format!("{i}.next"));
            artifacts.push(backup.clone());
            write_synced(&backup, source.source.as_bytes())?;
            artifacts.push(staged.clone());
            write_synced(
                &staged,
                item["text"]
                    .as_str()
                    .ok_or_else(|| Error::new(0, "missing batch text"))?
                    .as_bytes(),
            )?;
            std::fs::set_permissions(&staged, metadata.permissions())
                .map_err(|e| Error::new(0, e.to_string()).in_file(&staged))?;
            targets.push((source.path.clone(), backup, staged));
        }
        let receipt = journal.join("transaction.json");
        artifacts.push(receipt.clone());
        let draft = journal.join("transaction.prepared");
        artifacts.push(draft.clone());
        write_synced(&draft,json!({"schema":"anode.hole-transaction.v1","state":"prepared","root":project.root,"owner":owner,"snapshot_sha256":result["snapshot_sha256"],"skeleton_sha256":result["skeleton_sha256"],"manifest_sha256":crate::backend::sha256(project.manifest.as_bytes()),"original_files":project.files.iter().map(|f|json!({"path":f.path,"sha256":crate::backend::sha256(f.source.as_bytes())})).collect::<Vec<_>>(),"files":targets.iter().enumerate().map(|(i,(path,backup,staged))|json!({"path":path,"original":backup,"replacement":staged,"original_sha256":crate::backend::sha256(project.files.iter().find(|f|&f.path==path).unwrap().source.as_bytes()),"replacement_sha256":crate::backend::sha256(files[i]["text"].as_str().unwrap().as_bytes())})).collect::<Vec<_>>(),"recovery":"use inspect_hole_transaction then recover_holes with apply:true; recovery requires a dead owner and unchanged original/proposed hashes"}).to_string().as_bytes())?;
        std::fs::rename(&draft, &receipt)
            .map_err(|e| Error::new(0, e.to_string()).in_file(&receipt))?;
        // Recheck after staging as well: expensive checking and I/O never open a
        // stale-write window for another cooperating commit.
        fresh_disk(project)?;
        for (changed, (path, _, staged)) in targets.iter().enumerate() {
            if let Err(cause) = std::fs::rename(staged, path) {
                let mut rollback_failures = Vec::new();
                for (original, backup, _) in targets[..changed].iter().rev() {
                    if let Err(e) = std::fs::rename(backup, original) {
                        rollback_failures.push(json!({"path":original,"error":e.to_string()}));
                    }
                }
                let mut error = Error::new(
                    0,
                    format!(
                        "hole transaction replacement failed: {cause}; {}",
                        if rollback_failures.is_empty() {
                            "original sources restored"
                        } else {
                            "rollback incomplete; journal and locks retained"
                        }
                    ),
                )
                .in_file(path);
                if !rollback_failures.is_empty() {
                    locks.preserve = true;
                    error.details = Some(
                        json!({"journal":journal,"rollback_failures":rollback_failures,"recovery_required":true}),
                    );
                }
                return Err(error);
            }
        }
        Ok(targets.len())
    })();
    let preserve = locks.preserve;
    // Once every replacement succeeded, release source locks before deleting
    // rollback data. A cleanup crash can leave private artifacts, but cannot
    // strand a lock after its original backup has already been discarded.
    if outcome.is_ok() {
        drop(locks);
    }
    if !preserve {
        for path in artifacts.iter().rev() {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&journal);
    }
    let writes = outcome?;
    result["writes"] = json!(writes);
    result["applied"] = json!(true);
    result["post_snapshot_sha256"] = result["proposed_snapshot_sha256"].clone();
    Ok(result)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn GetExitCodeProcess(process: *mut std::ffi::c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    // Access denied is conservatively live. A reused PID also remains live,
    // never authorizing deletion of a possibly active writer's lock.
    unsafe {
        let handle = OpenProcess(0x1000, 0, pid);
        if handle.is_null() {
            return std::io::Error::last_os_error().raw_os_error() != Some(87);
        }
        let mut code = 259;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok == 0 || code == 259
    }
}
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if pid > i32::MAX as u32 {
        return false;
    }
    unsafe { kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(3) }
}
#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

fn read_json(path: &std::path::Path) -> Result<Value> {
    if std::fs::symlink_metadata(path)
        .map_err(|e| Error::new(0, e.to_string()).in_file(path))?
        .file_type()
        .is_symlink()
    {
        return Err(Error::new(0, "transaction metadata must not be a symlink").in_file(path));
    }
    let bytes = read_limited(path, 2 * 1024 * 1024)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::new(0, format!("invalid transaction metadata: {e}")).in_file(path))
}
fn read_limited(path: &std::path::Path, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| Error::new(0, e.to_string()).in_file(path))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::new(0, e.to_string()).in_file(path))?;
    if bytes.len() > limit {
        return Err(
            Error::new(0, "transaction input exceeds its bounded byte limit").in_file(path),
        );
    }
    Ok(bytes)
}
fn current_sha256(path: &std::path::Path) -> Result<String> {
    read_limited(path, crate::project::SOURCE_FILE_LIMIT).map(|bytes| crate::backend::sha256(&bytes))
}
struct RecoveryPlan {
    owner: Value,
    journal: std::path::PathBuf,
    locks: Vec<std::path::PathBuf>,
    files: Vec<(std::path::PathBuf, std::path::PathBuf, String, String)>,
    artifacts: Vec<std::path::PathBuf>,
    prepared: bool,
    alive: bool,
}
fn recovery_plan(project: &Project, request: &Value) -> Result<RecoveryPlan> {
    if !project.root.is_absolute() {
        return Err(Error::new(0, "recovery requires a file-backed project"));
    }
    let root_lock = project.root.join(".anode-hole-batch.lock");
    let owner = read_json(&root_lock)?;
    let transaction = owner["transaction"]
        .as_str()
        .ok_or_else(|| Error::new(0, "lock is missing transaction identity"))?;
    if !transaction.starts_with(".anode-hole-batch-")
        || transaction.len() > 128
        || !transaction
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        || request
            .get("journal")
            .is_some_and(|v| v.as_str() != Some(transaction))
    {
        return Err(Error::new(
            0,
            "journal must be the exact confined transaction basename in the project lock",
        ));
    }
    if owner["schema"] != "anode.hole-lock.v1" {
        return Err(Error::new(0, "unsupported lock schema; inspect manually"));
    }
    let pid = owner["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| Error::new(0, "invalid lock owner PID"))?;
    let alive = process_alive(pid);
    let journal = project.root.join(transaction);
    if journal.exists()
        && (std::fs::symlink_metadata(&journal)
            .map_err(|e| Error::new(0, e.to_string()))?
            .file_type()
            .is_symlink()
            || journal
                .canonicalize()
                .map_err(|e| Error::new(0, e.to_string()))?
                != journal)
    {
        return Err(Error::new(
            0,
            "journal directory must be canonical and inside the project root",
        ));
    }
    let metadata = journal.join("transaction.json");
    let prepared = metadata.exists();
    let mut plan = RecoveryPlan {
        owner: owner.clone(),
        journal: journal.clone(),
        locks: Vec::new(),
        files: Vec::new(),
        artifacts: Vec::new(),
        prepared,
        alive,
    };
    let owner_template = project.root.join(format!("{transaction}.owner"));
    if owner_template.exists() {
        if read_json(&owner_template)? != owner {
            return Err(Error::new(
                0,
                "owner template differs from transaction lock; recovery refused",
            ));
        }
        plan.artifacts.push(owner_template);
    }
    if prepared {
        let record = read_json(&metadata)?;
        if record["schema"] != "anode.hole-transaction.v1"
            || record["root"] != json!(project.root)
            || record["owner"] != owner
            || record["snapshot_sha256"] != owner["snapshot_sha256"]
            || record["manifest_sha256"] != crate::backend::sha256(project.manifest.as_bytes())
        {
            return Err(Error::new(
                0,
                "journal root, owner, source receipt or manifest does not match; recovery refused",
            ));
        }
        let entries = record["files"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 32)
            .ok_or_else(|| Error::new(0, "invalid recovery target list"))?;
        let mut seen = BTreeSet::new();
        for (i, entry) in entries.iter().enumerate() {
            let path = std::path::PathBuf::from(
                entry["path"]
                    .as_str()
                    .ok_or_else(|| Error::new(0, "invalid journal source path"))?,
            );
            if !project.files.iter().any(|f| f.path == path)
                || !seen.insert(path.clone())
                || path
                    .canonicalize()
                    .map_err(|e| Error::new(0, e.to_string()))?
                    != path
            {
                return Err(Error::new(
                    0,
                    "journal source path is outside the loaded project or aliased",
                ));
            }
            let backup = journal.join(format!("{i}.original"));
            let staged = journal.join(format!("{i}.next"));
            if entry["original"] != json!(backup) || entry["replacement"] != json!(staged) {
                return Err(Error::new(0, "journal backup paths are not confined"));
            }
            let original = entry["original_sha256"]
                .as_str()
                .filter(|s| s.len() == 64)
                .ok_or_else(|| Error::new(0, "invalid original hash"))?
                .to_owned();
            let next = entry["replacement_sha256"]
                .as_str()
                .filter(|s| s.len() == 64)
                .ok_or_else(|| Error::new(0, "invalid proposed hash"))?
                .to_owned();
            let current = current_sha256(&path)?;
            if current != original && current != next {
                return Err(Error::new(0,"source changed outside the interrupted transaction; recovery refuses to overwrite it").in_file(&path));
            }
            if backup.exists() {
                if std::fs::symlink_metadata(&backup)
                    .map_err(|e| Error::new(0, e.to_string()))?
                    .file_type()
                    .is_symlink()
                    || current_sha256(&backup)? != original
                {
                    return Err(Error::new(0, "original backup hash mismatch").in_file(&backup));
                }
            } else if current != original {
                return Err(
                    Error::new(0, "original backup is missing for a changed source")
                        .in_file(&backup),
                );
            }
            if staged.exists()
                && (std::fs::symlink_metadata(&staged)
                    .map_err(|e| Error::new(0, e.to_string()))?
                    .file_type()
                    .is_symlink()
                    || current_sha256(&staged)? != next)
            {
                return Err(Error::new(0, "staged replacement hash mismatch").in_file(&staged));
            }
            plan.artifacts
                .extend([backup.clone(), staged, journal.join(format!("{i}.recover"))]);
            plan.files.push((path, backup, original, next));
        }
        let originals = record["original_files"]
            .as_array()
            .filter(|v| v.len() == project.files.len())
            .ok_or_else(|| Error::new(0, "journal module graph differs from loaded project"))?;
        for source in &project.files {
            let expected = originals
                .iter()
                .find(|e| e["path"] == json!(source.path))
                .and_then(|e| e["sha256"].as_str())
                .ok_or_else(|| Error::new(0, "missing original project source hash"))?;
            if let Some((_, _, original, _)) = plan
                .files
                .iter()
                .find(|(path, _, _, _)| path == &source.path)
            {
                if expected != original {
                    return Err(Error::new(
                        0,
                        "original source receipt disagrees with backup hash",
                    ));
                }
            } else if current_sha256(&source.path)? != expected {
                return Err(
                    Error::new(0, "untouched project source changed; recovery refused")
                        .in_file(&source.path),
                );
            }
        }
        // Validate the entire reconstructed original snapshot before restoring
        // any byte, rather than trusting independently edited journal hashes.
        let originals = plan
            .files
            .iter()
            .map(|(path, backup, _, _)| {
                let index = project.files.iter().position(|f| &f.path == path).unwrap();
                let text = std::fs::read_to_string(if backup.exists() { backup } else { path })
                    .map_err(|e| Error::new(0, e.to_string()).in_file(path))?;
                Ok((relative(project, index), text))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        if snapshot_sha256(&project.with_overlays(&originals)?)
            != owner["snapshot_sha256"].as_str().unwrap_or("")
        {
            return Err(Error::new(
                0,
                "reconstructed original snapshot does not match pinned SHA256; recovery refused",
            ));
        }
        plan.artifacts.push(metadata);
    } else {
        if owner["snapshot_sha256"].as_str() != Some(snapshot_sha256(project).as_str()) {
            return Err(Error::new(
                0,
                "preparing transaction source snapshot changed; recovery refused",
            ));
        }
        // Before transaction.json is synced no source replacement has begun.
        // Confine cleanup to our staging names, never arbitrary directory data.
        if journal.is_dir() {
            for (count, entry) in std::fs::read_dir(&journal)
                .map_err(|e| Error::new(0, e.to_string()))?
                .enumerate()
            {
                if count >= 65 {
                    return Err(Error::new(
                        0,
                        "preparing journal exceeds bounded artifact count",
                    ));
                }
                let path = entry.map_err(|e| Error::new(0, e.to_string()))?.path();
                let name = path.file_name().unwrap().to_string_lossy();
                let valid = name == "transaction.prepared"
                    || name.split_once('.').is_some_and(|(n, ext)| {
                        n.parse::<usize>().is_ok_and(|i| i < 32)
                            && ["original", "next"].contains(&ext)
                    });
                if !valid
                    || !path.is_file()
                    || std::fs::symlink_metadata(&path)
                        .map_err(|e| Error::new(0, e.to_string()))?
                        .file_type()
                        .is_symlink()
                {
                    return Err(Error::new(
                        0,
                        "unknown preparing journal artifact; recovery refused",
                    ));
                }
                plan.artifacts.push(path);
            }
        }
    }
    for source in &project.files {
        plan.locks.push(source.path.with_file_name(format!(
            "{}.anode-hole.lock",
            source.path.file_name().unwrap().to_string_lossy()
        )));
    }
    for name in ["Anode.toml", "Zenith.toml"] {
        if project.managed && project.root.join(name).exists() {
            plan.locks
                .push(project.root.join(format!("{name}.anode-hole.lock")));
        }
    }
    plan.locks.push(root_lock);
    for lock in &plan.locks {
        if lock.exists() && read_json(lock)? != owner {
            return Err(
                Error::new(0, "source lock belongs to another writer; recovery refused")
                    .in_file(lock),
            );
        }
    }
    Ok(plan)
}

/// Read-only recovery inspection. `journal` may select the exact lock basename.
pub fn inspect_hole_transaction(project: &Project, request: &Value) -> Result<Value> {
    let plan = recovery_plan(project, request)?;
    Ok(
        json!({"writes":0,"executed":false,"journal":plan.journal.file_name().unwrap().to_string_lossy(),"owner":plan.owner,"owner_alive":plan.alive,"recoverable":!plan.alive,"state":if plan.prepared{"prepared_or_partially_replaced"}else{"preparing_no_source_replacements"},"files":plan.files.iter().map(|(path,_,original,next)|json!({"path":path,"original_sha256":original,"replacement_sha256":next})).collect::<Vec<_>>(),"transaction_guarantees":transaction_guarantees()}),
    )
}

/// Roll an abandoned transaction back to its original snapshot. Dry-run by
/// default; live owners, changed files, invalid paths or corrupt backups refuse.
pub fn recover_holes(project: &Project, request: &Value) -> Result<Value> {
    if !bool_option(request, "apply")? {
        return inspect_hole_transaction(project, request);
    }
    let initial = recovery_plan(project, request)?;
    if initial.alive {
        return Err(Error::new(
            0,
            "transaction owner is still alive; live writer locks are never removed",
        ));
    }
    // Kernel lock serializes recovery attempts; process death releases it. The
    // gate inode is retained to avoid unlink/reopen races between recoverers.
    // Ordinary writers still serialize through the published project lock.
    let gate_path = project.root.join(".anode-recovery.lock");
    if gate_path.exists()
        && std::fs::symlink_metadata(&gate_path)
            .map_err(|e| Error::new(0, e.to_string()))?
            .file_type()
            .is_symlink()
    {
        return Err(Error::new(0, "recovery gate must not be a symlink"));
    }
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(gate_path)
        .map_err(|e| Error::new(0, e.to_string()))?;
    gate.try_lock()
        .map_err(|e| Error::new(0, format!("another recovery is active: {e}")))?;
    let plan = recovery_plan(project, request)?;
    if plan.alive {
        return Err(Error::new(
            0,
            "transaction owner is still alive; live writer locks are never removed",
        ));
    }
    let mut writes = 0;
    for (i, (path, backup, original, next)) in plan.files.iter().enumerate() {
        let current = current_sha256(path)?;
        if &current == original {
            continue;
        }
        if &current != next {
            return Err(Error::new(
                0,
                "source changed during recovery; journal and locks retained",
            )
            .in_file(path));
        }
        let staged = plan.journal.join(format!("{i}.recover"));
        if staged.exists() {
            std::fs::remove_file(&staged)
                .map_err(|e| Error::new(0, e.to_string()).in_file(&staged))?;
        }
        write_synced(
            &staged,
            &std::fs::read(backup).map_err(|e| Error::new(0, e.to_string()).in_file(backup))?,
        )?;
        std::fs::set_permissions(
            &staged,
            std::fs::metadata(path)
                .map_err(|e| Error::new(0, e.to_string()))?
                .permissions(),
        )
        .map_err(|e| Error::new(0, e.to_string()))?;
        std::fs::rename(&staged, path).map_err(|e| {
            Error::new(
                0,
                format!("recovery replacement failed; original backups and locks retained: {e}"),
            )
            .in_file(path)
        })?;
        writes += 1;
    }
    let fresh = Project::load(if project.managed {
        &project.root
    } else {
        &project.files[0].path
    })?;
    if plan.owner["snapshot_sha256"].as_str() != Some(snapshot_sha256(&fresh).as_str()) {
        return Err(Error::new(
            0,
            "recovered snapshot differs from original; journal and locks retained",
        ));
    }
    for artifact in &plan.artifacts {
        if artifact.exists() {
            std::fs::remove_file(artifact)
                .map_err(|e| Error::new(0, e.to_string()).in_file(artifact))?;
        }
    }
    if plan.journal.exists() {
        std::fs::remove_dir(&plan.journal)
            .map_err(|e| Error::new(0, e.to_string()).in_file(&plan.journal))?;
    }
    for lock in &plan.locks {
        if lock.exists() {
            if read_json(lock)? != plan.owner {
                return Err(Error::new(
                    0,
                    "lock owner changed during recovery; refusing removal",
                ));
            }
            std::fs::remove_file(lock).map_err(|e| Error::new(0, e.to_string()).in_file(lock))?;
        }
    }
    Ok(
        json!({"writes":writes,"applied":true,"recovered":true,"snapshot_sha256":snapshot_sha256(&fresh),"executed":false,"transaction_guarantees":transaction_guarantees()}),
    )
}
