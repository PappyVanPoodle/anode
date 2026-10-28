//! Layout-verified native reload at a controlled DLL function-pointer boundary.
//! Legacy hosts own scalar/POD state and use layout-strict replacement. Dynamic
//! slots own aligned POD/fixed bytes and execute explicit checked migrations
//! while calls are quiescent. Code stays loaded while any LoadedPatch owns it.
//! This does not rewrite direct calls in a running EXE.
use crate::{
    backend::{self, BuildOptions},
    ir::CheckedProgram,
    lib_export::{self, Export},
    project::Project,
    syntax::{Borrow, DataKind, Expr, ExprKind, Type},
    Error, Result,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};

#[derive(Clone, Debug, Default)]
pub struct HotReloadOptions {
    pub optimize: bool,
    pub force: bool,
    pub cache_bytes: u64,
    pub ffi_libraries: Vec<PathBuf>,
    /// Checked public `@effect(no_io)` owned POD/fixed -> POD/fixed hook. This
    /// permits allocation and does not imply termination or a wall-clock bound.
    /// CLI spelling: `--migrate-fn module::function`.
    pub migrate_fn: Option<String>,
}
#[derive(Debug)]
pub struct HotReloadPatch {
    pub output: PathBuf,
    pub object: PathBuf,
    pub header: PathBuf,
    pub manifest: PathBuf,
    pub symbols: Vec<Export>,
    pub bytes: u64,
    pub backend_ms: f64,
    pub proof: Value,
}
fn io(error: std::io::Error) -> Error {
    Error::new(0, error.to_string())
}
fn type_name(program: &CheckedProgram, ty: Type) -> String {
    if let Some(result) = ty.task_result() {
        return format!("task<{}>", type_name(program, result));
    }
    match ty {
        Type::Named(id) => program.types.definitions[id].name.clone(),
        Type::Id(id) => format!("id({})", program.types.definitions[id].name),
        Type::VectorView(id) => format!("[{}]", program.types.definitions[id].name),
        _ => ty.label(),
    }
}
fn fields(kind: &DataKind) -> &[crate::syntax::FieldDecl] {
    match kind {
        DataKind::Record(f)
        | DataKind::Enum(f)
        | DataKind::Vector(f)
        | DataKind::SoaVector(f)
        | DataKind::Arena(f)
        | DataKind::Channel(f)
        | DataKind::Map(f) => f,
    }
}
fn dependencies(program: &CheckedProgram, id: usize, out: &mut BTreeSet<String>) {
    for field in fields(&program.types.definitions[id].kind) {
        if let Type::Named(child) | Type::Id(child) | Type::VectorView(child) = field.ty {
            if out.insert(program.types.definitions[child].name.clone()) {
                dependencies(program, child, out);
            }
        }
    }
}
fn layouts(program: &CheckedProgram) -> BTreeMap<String, Value> {
    program.types.definitions.iter().enumerate().map(|(id, definition)| {
        let layout = &program.types.layouts[id];
        let kind = match &definition.kind {
            DataKind::Record(_) => "record", DataKind::Enum(_) => "enum",
            DataKind::Vector(_) => "vector", DataKind::SoaVector(_) => "soa_vector", DataKind::Map(_) => "map",
            DataKind::Arena(_) => "arena",
            DataKind::Channel(_) => "channel",
        };
        let members: Vec<_> = fields(&definition.kind).iter().enumerate().map(|(index, field)| {
            let field_layout = program.types.layout(field.ty);
            json!({"name":field.name,"type":type_name(program,field.ty),"size":field_layout.size,
                "align":field_layout.align,"offset":layout.offsets.get(index),
                "tag":if kind == "enum" { Some(index) } else { None }})
        }).collect();
        let storage = match &definition.kind {
            DataKind::Vector(f) => json!({"kind":"contiguous_elements","descriptor_offsets":[0,8,16],
                "element_stride":program.types.layout(f[0].ty).size}),
            DataKind::SoaVector(f) => {
                let Type::Named(element) = f[0].ty else { unreachable!() };
                let columns:Vec<_> = fields(&program.types.definitions[element].kind).iter().enumerate().map(|(i,field)|
                    json!({"field":field.name,"type":type_name(program,field.ty),"record_offset":program.types.layouts[element].offsets[i],
                        "stride":program.types.layout(field.ty).size,"column_start_alignment":8})
                ).collect();
                json!({"kind":"field_columns","descriptor_offsets":[0,8,16],"columns":columns})
            },
            DataKind::Map(_) => {
                let entry = program.types.map_entry_layout(Type::Named(id)).unwrap();
                json!({"kind":"open_addressed_entries","descriptor_offsets":[0,8,16,24,32],
                    "entry_stride":entry.width,"value_offset":entry.value_offset,"key_code":entry.key_code})
            },
            DataKind::Arena(f) => json!({"kind":"generational_slots","descriptor_offsets":[0,8,16,24,32,40],"element_stride":program.types.layout(f[0].ty).size,"slot_bits":32,"generation_bits":32,"instance_relative":true,"exhausted_slots":"retired"}),
            DataKind::Channel(f) => json!({"kind":"shared_bounded_fifo","descriptor_offsets":[0],"element_stride":program.types.layout(f[0].ty).size,"element_align":program.types.layout(f[0].ty).align,"owns_unread_elements":true}),
            _ => json!({"kind":kind}),
        };
        let mut nested = BTreeSet::new(); dependencies(program,id,&mut nested);
        (definition.name.clone(), json!({"name":definition.name,"kind":kind,"size":layout.size,"align":layout.align,
            "fields":members,"storage":storage,"dependencies":nested}))
    }).collect()
}
fn semantic_functions(project: &Project, program: &CheckedProgram) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for (file_id, file) in project.files.iter().enumerate() {
        for (function_id, original) in file.parsed.functions.iter().enumerate() {
            let internal =
                crate::project::function_symbol(file_id, function_id, &file.module, original);
            let Some(checked) = program.functions.iter().find(|f| f.source.name == internal) else {
                continue;
            };
            let semantic = if let Some(method) = &checked.source.method {
                format!("{}::{}", type_name(program, method.owner), method.name)
            } else if file.module.is_empty() {
                original.name.clone()
            } else {
                format!("{}::{}", file.module, original.name)
            };
            names.insert(internal, semantic);
        }
    }
    names
}
fn expression(e: &Expr, program: &CheckedProgram, names: &BTreeMap<String, String>) -> Value {
    let expr = |e: &Expr| expression(e, program, names);
    let qualified = |name: &String| names.get(name).unwrap_or(name).clone();
    match &e.kind {
        ExprKind::Int(n) => json!(["int", n]),
        ExprKind::Float(n) => json!(["float_bits", n]),
        ExprKind::Bool(b) => json!(["bool", b]),
        ExprKind::Text(t) => json!(["text", t]),
        ExprKind::Var(n) => json!(["var", n]),
        ExprKind::Borrow(n, m) => json!(["borrow", n, m]),
        ExprKind::Hole(t, i) => json!(["hole", type_name(program, *t), i]),
        ExprKind::Array(a) => json!(["array", a.iter().map(expr).collect::<Vec<_>>()]),
        ExprKind::Repeat(e, n) => json!(["repeat", expr(e), n]),
        ExprKind::Call(n, a) => {
            json!(["call", qualified(n), a.iter().map(expr).collect::<Vec<_>>()])
        }
        ExprKind::Spawn(n, a) => json!([
            "spawn",
            qualified(n),
            a.iter().map(expr).collect::<Vec<_>>()
        ]),
        ExprKind::MethodCall(e, n, a) => {
            json!(["method", expr(e), n, a.iter().map(expr).collect::<Vec<_>>()])
        }
        ExprKind::Unary(n, e) => json!(["unary", n, expr(e)]),
        ExprKind::Try(e) => json!(["try", expr(e)]),
        ExprKind::Binary(n, a, b) => json!(["binary", n, expr(a), expr(b)]),
        ExprKind::Field(e, n) => json!(["field", expr(e), n]),
        ExprKind::Record(n, f) => json!([
            "record",
            n,
            f.iter()
                .map(|(n, e)| json!([n, expr(e)]))
                .collect::<Vec<_>>()
        ]),
    }
}
fn contracts(project: &Project, program: &CheckedProgram) -> BTreeMap<String, Value> {
    let names = semantic_functions(project, program);
    let effects = crate::effects::analyze(program);
    program.functions.iter().filter(|f|f.source.public && !f.source.is_test && f.source.name!="main").map(|checked| {
        let f=&checked.source;
        let params:Vec<_> = f.params.iter().map(|p| {
            let view=p.view.as_ref().map(|fields|fields.iter().map(|v|json!({"path":v.path,"mutable":v.mutable})).collect::<Vec<_>>());
            json!({"name":p.name,"type":type_name(program,p.ty),"borrow":format!("{:?}",p.borrow),"mutable":p.mutable,"view":view})
        }).collect();
        let mut declared_effects=f.effects.clone(); declared_effects.sort(); declared_effects.dedup();
        let semantic=names.get(&f.name).cloned().unwrap_or_else(||f.name.clone());
        let views=program.views.get(&f.name).map(|parameters| parameters.iter().enumerate().map(|(i,accesses)|{
            accesses.iter().map(|access| {
                let mut ty=f.params[i].ty;
                let mut path=Vec::new();
                for index in &access.fields {
                    let Type::Named(id)=ty else { unreachable!() };
                    let field=&fields(&program.types.definitions[id].kind)[*index];
                    path.push(field.name.clone()); ty=field.ty;
                }
                json!({"path":path,"mutable":access.mutable})
            }).collect::<Vec<_>>()
        }).collect::<Vec<_>>());
        (semantic, json!({"params":params,"return":type_name(program,f.ret),"effects":declared_effects,
            "inferred_effects":{"alloc":effects[&f.name].alloc,"io":effects[&f.name].io,"bounded":effects[&f.name].bounded},"views":views,
            "requires":f.requires.iter().map(|e|expression(e,program,&names)).collect::<Vec<_>>(),
            "ensures":f.ensures.iter().map(|e|expression(e,program,&names)).collect::<Vec<_>>(),
            "external_symbol":f.external_symbol}))
    }).collect()
}
fn source_snapshot(project: &Project) -> Value {
    let mut files = BTreeMap::new();
    for file in &project.files {
        files.insert(
            file.module.clone(),
            json!({"sha256":backend::sha256(file.source.as_bytes()),"bytes":file.source.len(),"source_key":file.source_key,"package":file.package}),
        );
    }
    let value = json!({"manifest_sha256":backend::sha256(project.manifest.as_bytes()),"process_allowed":project.process_allowed(),"files":files});
    json!({"sha256":backend::sha256(value.to_string().as_bytes()),"contents":value})
}
fn diff_value(path: &str, before: &Value, after: &Value, out: &mut Vec<Value>) {
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                diff_value(
                    &format!("{path}/{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for i in 0..a.len().max(b.len()) {
                diff_value(
                    &format!("{path}/{i}"),
                    a.get(i).unwrap_or(&Value::Null),
                    b.get(i).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if before != after => out.push(json!({"path":path,"baseline":before,"updated":after})),
        _ => (),
    }
}
fn compatibility(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
    category: &str,
    out: &mut Vec<Value>,
) {
    for (name, old) in before {
        if let Some(new) = after.get(name) {
            let mut changes = Vec::new();
            diff_value("", old, new, &mut changes);
            if !changes.is_empty() {
                out.push(
                    json!({"category":category,"name":name,"reason":"changed","changes":changes}),
                );
            }
        } else {
            out.push(json!({"category":category,"name":name,"reason":"removed","baseline":old}));
        }
    }
}
fn incompatible(differences: Vec<Value>) -> Error {
    let mut e = Error::new(
        0,
        "hot reload is incompatible with live layouts or public contracts",
    );
    e.details = Some(json!({"kind":"hot_reload_incompatible","differences":differences}));
    e
}
fn unsupported_boundary(mut error: Error) -> Error {
    error.details = Some(json!({"kind":"hot_reload_unsupported_boundary",
        "abi":"windows-x64-msvc-c-pod-v1","reason":error.message,
        "state_contract":"host-owned scalar/POD only; opaque owning state, enums, views and tasks cannot cross reload generations"}));
    error
}
fn protect(baseline: &Project, updated: &Project, output: &Path) -> Result<()> {
    let protected = baseline
        .files
        .iter()
        .chain(updated.files.iter())
        .map(|f| f.path.clone())
        .chain(baseline.manifest_paths())
        .chain(updated.manifest_paths());
    let protected: BTreeSet<_> = protected.filter_map(|p| p.canonicalize().ok()).collect();
    for ext in ["dll", "obj", "h", "json", "zn-lock"] {
        let path = output.with_extension(ext);
        if path.canonicalize().is_ok_and(|p| protected.contains(&p)) {
            return Err(Error::new(
                0,
                "reload artifacts must not overwrite source files or manifests",
            )
            .in_file(path));
        }
    }
    Ok(())
}

/// An exact, name-independent root POD layout. Record field names, ordering,
/// offsets, scalar kinds, fixed lengths and nested layouts are part of its
/// SHA256; arena handle nominal names remain significant.
#[derive(Clone, Debug, PartialEq)]
pub struct StateLayout {
    pub name: String,
    pub digest: String,
    pub size: usize,
    pub align: usize,
    pub schema: Value,
}
impl StateLayout {
    fn value(&self) -> Value {
        json!({"name":self.name,"digest":self.digest,"size":self.size,"align":self.align,"schema":self.schema})
    }
    fn parse(value: &Value) -> Result<Self> {
        let name = value["name"]
            .as_str()
            .ok_or_else(|| Error::new(0, "invalid state layout name"))?
            .to_owned();
        let schema = value["schema"].clone();
        let digest = backend::sha256(schema.to_string().as_bytes());
        let size = value["size"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Error::new(0, "invalid state size"))?;
        let align = value["align"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Error::new(0, "invalid state alignment"))?;
        if value["digest"].as_str() != Some(&digest)
            || size == 0
            || size > 65536
            || ![1, 2, 4, 8].contains(&align)
            || schema["size"].as_u64() != Some(size as u64)
            || schema["align"].as_u64() != Some(align as u64)
        {
            return Err(Error::new(0, "state layout receipt is invalid or stale"));
        }
        Ok(Self {
            name,
            digest,
            size,
            align,
            schema,
        })
    }
}
fn pod_schema(program: &CheckedProgram, ty: Type) -> Result<Value> {
    if let Some(channel) = lib_export::owning_channel(program, ty) {
        return Err(Error::new(0, format!("reload state contains owning channel {channel}; channel endpoints cannot cross reload generations")));
    }
    let l = program.types.layout(ty);
    let kind = match ty {
        Type::Int | Type::Number(_) => {
            json!({"kind":"integer","bits":ty.integer().unwrap().bits(),"signed":ty.integer().unwrap().signed()})
        }
        Type::Float(n) => json!({"kind":"float","bits":n.bits()}),
        Type::Bool => json!({"kind":"bool","bits":8}),
        Type::Id(id) => {
            if !program.types.definitions[id].public
                || program.types.arena_element(Type::Named(id)).is_none()
            {
                return Err(Error::new(
                    0,
                    "reload state requires public nominal arena handles",
                ));
            }
            json!({"kind":"arena_id","arena":program.types.definitions[id].name,"slot_bits":32,"generation_bits":32})
        }
        Type::Fixed(n, len) => json!({"kind":"fixed","integer":n.name(),"length":len}),
        Type::Named(id) => {
            let d = &program.types.definitions[id];
            let DataKind::Record(fields) = &d.kind else {
                return Err(Error::new(d.line,"reload state must contain only POD records, scalars, handles and fixed buffers; owning arenas/containers are unsupported"));
            };
            if !d.public {
                return Err(Error::new(
                    d.line,
                    "reload state requires public POD records",
                ));
            }
            let mut members = Vec::new();
            for (i, f) in fields.iter().enumerate() {
                members.push(
                    json!({"name":f.name,"offset":l.offsets[i],"layout":pod_schema(program,f.ty)?}),
                );
            }
            json!({"kind":"record","fields":members})
        }
        _ => {
            return Err(Error::new(
                0,
                "reload state cannot own heap data or retain views/tasks",
            ))
        }
    };
    let mut schema = kind;
    schema["size"] = json!(l.size);
    schema["align"] = json!(l.align);
    Ok(schema)
}
fn state_layout(program: &CheckedProgram, ty: Type) -> Result<StateLayout> {
    if !matches!(ty, Type::Named(_) | Type::Fixed(..)) {
        return Err(Error::new(
            0,
            "dynamic state roots must be POD records or fixed integer buffers",
        ));
    }
    let schema = pod_schema(program, ty)?;
    let l = program.types.layout(ty);
    Ok(StateLayout {
        name: type_name(program, ty),
        digest: backend::sha256(schema.to_string().as_bytes()),
        size: l.size,
        align: l.align,
        schema,
    })
}
fn state_layouts(program: &CheckedProgram) -> BTreeMap<String, Value> {
    let mut types: BTreeSet<_> = program
        .types
        .definitions
        .iter()
        .enumerate()
        .filter_map(|(id, d)| matches!(d.kind, DataKind::Record(_)).then_some(Type::Named(id)))
        .collect();
    for f in &program.functions {
        types.extend(
            f.source
                .params
                .iter()
                .map(|p| p.ty)
                .chain(std::iter::once(f.source.ret))
                .filter(|t| matches!(t, Type::Fixed(..))),
        );
    }
    types
        .into_iter()
        .filter_map(|ty| state_layout(program, ty).ok())
        .map(|l| (l.name.clone(), l.value()))
        .collect()
}
fn migration_plan(
    old: &CheckedProgram,
    updated: &Project,
    new: &CheckedProgram,
    name: &str,
) -> Result<Value> {
    let semantic = semantic_functions(updated, new);
    let checked = new
        .functions
        .iter()
        .find(|f| semantic.get(&f.source.name).is_some_and(|n| n == name))
        .ok_or_else(|| {
            Error::new(
                0,
                format!("migration function {name} does not exist in the updated project"),
            )
        })?;
    let f = &checked.source;
    let effects = crate::effects::analyze(new);
    let effect = &effects[&f.name];
    if !f.public
        || f.external_symbol.is_some()
        || f.method.is_some()
        || f.is_test
        || f.params.len() != 1
        || f.params[0].borrow != Borrow::Owned
        || !f.effects.iter().any(|e| e == "no_io")
        || effect.io
    {
        return Err(Error::new(f.line,"migration requires a public checked owned-POD/fixed -> POD/fixed function with @effect(no_io) and no transitive IO"));
    }
    let input = state_layout(new, f.params[0].ty)?;
    let output = state_layout(new, f.ret)?;
    let candidates: Vec<_> = state_layouts(old)
        .into_values()
        .filter_map(|v| StateLayout::parse(&v).ok())
        .filter(|l| l.digest == input.digest)
        .collect();
    let baseline = candidates
        .iter()
        .find(|l| l.name == input.name)
        .or_else(|| candidates.iter().find(|l| l.name == output.name))
        .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
        .ok_or_else(|| {
            Error::new(
                f.line,
                "migration input layout does not uniquely match a baseline POD/fixed state layout",
            )
        })?;
    Ok(
        json!({"function":name,"internal_function":f.name,"abi":"const-input-pointer-output-pointer-last","input":input.value(),"output":output.value(),"baseline_state":baseline.value(),"checked_no_io":true,"inferred_alloc":effect.alloc}),
    )
}
fn substitute_state(value: &Value, name: &str) -> Value {
    // Only the explicitly migrated signature type is substituted. Parameter
    // names, field views and requires/ensures expressions remain exact; a
    // coincidentally identical string there must never mask a contract edit.
    let mut out = value.clone();
    if let Some(params) = out["params"].as_array_mut() {
        for param in params {
            if param["type"].as_str() == Some(name) {
                param["type"] = json!("@dynamic_state");
            }
        }
    }
    if out["return"].as_str() == Some(name) {
        out["return"] = json!("@dynamic_state");
    }
    out
}
fn migration_compatibility(
    old: &BTreeMap<String, Value>,
    new: &BTreeMap<String, Value>,
    old_contracts: &BTreeMap<String, Value>,
    new_contracts: &BTreeMap<String, Value>,
    migration: &Value,
) -> Vec<Value> {
    let name = migration["baseline_state"]["name"].as_str().unwrap();
    let mut authorized = BTreeSet::from([name.to_owned()]);
    if let Some(d) = old.get(name).and_then(|v| v["dependencies"].as_array()) {
        authorized.extend(d.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    // Historical migration inputs are not active state. A later generation
    // may reuse that hook's auxiliary type name for its new old-state schema.
    let input_name = migration["input"]["name"].as_str().unwrap();
    authorized.insert(input_name.to_owned());
    if let Some(d) = old
        .get(input_name)
        .and_then(|v| v["dependencies"].as_array())
    {
        authorized.extend(d.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    let retained: BTreeMap<_, _> = old
        .iter()
        .filter(|(n, _)| !authorized.contains(*n))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let mut out = Vec::new();
    compatibility(&retained, new, "layout", &mut out);
    // Auxiliary old-state schemas may change for the hook, but cannot silently
    // change another exported function's independent POD ABI. Only the selected
    // dynamic state root is allowed to acquire the output state representation.
    for (function, contract) in old_contracts {
        if migration["function"].as_str() == Some(function.as_str()) {
            continue;
        }
        let mut exposed = Vec::new();
        if let Some(params) = contract["params"].as_array() {
            exposed.extend(params.iter().filter_map(|p| p["type"].as_str()));
        }
        if let Some(ret) = contract["return"].as_str() {
            exposed.push(ret);
        }
        for ty in exposed.into_iter().filter(|ty| *ty != name) {
            let mut closure = BTreeSet::from([ty.to_owned()]);
            if let Some(deps) = old.get(ty).and_then(|v| v["dependencies"].as_array()) {
                closure.extend(deps.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            if closure.iter().any(|ty| {
                old.get(ty)
                    .is_some_and(|layout| new.get(ty) != Some(layout))
            }) {
                out.push(json!({"category":"public_contract","name":function,"reason":"independent parameter/result layout changed","type":ty}));
            }
        }
    }
    let a = old_contracts
        .iter()
        .filter(|(n, _)| migration["function"].as_str() != Some(n.as_str()))
        .map(|(n, v)| (n.clone(), substitute_state(v, name)))
        .collect();
    let b = new_contracts
        .iter()
        .map(|(n, v)| {
            (
                n.clone(),
                substitute_state(v, migration["output"]["name"].as_str().unwrap()),
            )
        })
        .collect();
    compatibility(&a, &b, "public_contract", &mut out);
    out
}
fn metadata(proof: &Value) -> Value {
    let mut value = json!({"format":proof["format"],"abi":proof["abi"],"baseline":proof["baseline"],"updated":proof["updated"],"symbols":proof["symbols"],"migration":proof["migration"]});
    // Keep old v2 metadata byte-for-byte compatible when no cleanup export
    // exists, while binding new DLL lifetime code into the compiled marker.
    if let Some(symbol) = proof["runtime_shutdown_symbol"].as_str() {
        value["runtime_shutdown_symbol"] = json!(symbol);
    }
    value
}
fn validate_pod_bytes(schema: &Value, bytes: &[u8]) -> Result<()> {
    let size = schema["size"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::new(0, "invalid POD schema size"))?;
    if bytes.len() != size {
        return Err(Error::new(
            0,
            "POD bytes do not match their exact layout size",
        ));
    }
    match schema["kind"].as_str() {
        Some("bool") if bytes.len() == 1 && bytes[0] <= 1 => Ok(()),
        Some("bool") => Err(Error::new(
            0,
            "POD state bool bytes must be canonical 0 or 1",
        )),
        Some("record") => {
            for field in schema["fields"]
                .as_array()
                .ok_or_else(|| Error::new(0, "invalid POD record schema"))?
            {
                let offset = field["offset"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| Error::new(0, "invalid POD field offset"))?;
                let width = field["layout"]["size"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| Error::new(0, "invalid POD field width"))?;
                let end = offset
                    .checked_add(width)
                    .ok_or_else(|| Error::new(0, "invalid POD field range"))?;
                validate_pod_bytes(
                    &field["layout"],
                    bytes
                        .get(offset..end)
                        .ok_or_else(|| Error::new(0, "POD field is outside state storage"))?,
                )?;
            }
            Ok(())
        }
        Some("integer" | "float" | "fixed" | "arena_id") => Ok(()),
        _ => Err(Error::new(0, "unsupported POD state representation")),
    }
}

/// Fully check both snapshots and emit a real native DLL, object, header and
/// artifact-bound proof. Changed state layouts require an explicit no_io hook;
/// remaining public contracts/layouts stay compatible. Legacy typed slots still
/// reject all layout changes. Owning containers, views, enums and tasks cannot
/// cross the state boundary; migrations operate on public POD/fixed layouts.
pub fn plan_and_build(
    baseline: &Project,
    updated: &Project,
    output: &Path,
    options: &HotReloadOptions,
) -> Result<HotReloadPatch> {
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("dll"))
    {
        return Err(Error::new(
            0,
            "hot reload output must have a .dll extension",
        ));
    }
    protect(baseline, updated, output)?;
    let old = baseline.checked_reload()?;
    let new = updated.checked_reload()?;
    let old_layouts = layouts(&old);
    let new_layouts = layouts(&new);
    let old_contracts = contracts(baseline, &old);
    let new_contracts = contracts(updated, &new);
    let mut migration = options
        .migrate_fn
        .as_deref()
        .map(|name| migration_plan(&old, updated, &new, name))
        .transpose()?;
    let differences = if let Some(migration) = &migration {
        migration_compatibility(
            &old_layouts,
            &new_layouts,
            &old_contracts,
            &new_contracts,
            migration,
        )
    } else {
        let mut differences = Vec::new();
        compatibility(&old_layouts, &new_layouts, "layout", &mut differences);
        compatibility(
            &old_contracts,
            &new_contracts,
            "public_contract",
            &mut differences,
        );
        differences
    };
    if !differences.is_empty() {
        return Err(incompatible(differences));
    }
    let baseline_snapshot = source_snapshot(baseline);
    let updated_snapshot = source_snapshot(updated);
    let prefix = format!(
        "anode_reload_{}",
        &backend::sha256(
            json!([baseline_snapshot, updated_snapshot, options.migrate_fn])
                .to_string()
                .as_bytes()
        )[..24]
    );
    // Validate the old public boundary as well: accepting a new POD shim must
    // never accidentally validate an old opaque owning-state contract.
    lib_export::prepare_library(baseline, &old, &prefix, "reload").map_err(unsupported_boundary)?;
    let mut prepared = lib_export::prepare_library(updated, &new, &prefix, "reload")
        .map_err(unsupported_boundary)?;
    if let Some(migration) = &mut migration {
        migration["symbol"] = json!(
            prepared
                .exports
                .iter()
                .find(|e| e.source_name == migration["function"].as_str().unwrap())
                .ok_or_else(|| Error::new(0, "compiled migration export is missing"))?
                .symbol
        );
    }
    let public_exports = prepared.exports.clone();
    let symbols: Vec<_> = public_exports
        .iter()
        .map(|e| json!({"source_name":e.source_name,"symbol":e.symbol}))
        .collect();
    let mut proof = json!({"format":"anode-hot-reload-v2","abi":"windows-x64-msvc-c-pod-v1",
        "baseline":{"snapshot":baseline_snapshot,"layouts":old_layouts,"public_contracts":old_contracts,"state_layouts":state_layouts(&old)},
        "updated":{"snapshot":updated_snapshot,"layouts":new_layouts,"public_contracts":new_contracts,"state_layouts":state_layouts(&new)},
        "symbols":symbols,"migration":migration});
    proof["native_codegen"] = backend::native_codegen_receipt();
    proof["native_codegen"]["effective_ltcg"] = json!(options.optimize);
    let shutdown_symbol = format!("{prefix}_runtime_shutdown_receipt");
    prepared
        .runtime_preamble
        .push_str("\n#define ZN_TASK_POOL_NO_ATEXIT 1\n");
    prepared.c.push_str(&format!(
        "\nvoid __cdecl {shutdown_symbol}(void){{zn_task_pool_shutdown();}}\n"
    ));
    prepared.header.push_str(&format!("\n/* Raw DLL hosts: after all calls/callbacks are quiescent, invoke this\n   cleanup entrypoint before FreeLibrary. LoadedPatch handles it automatically. */\n#ifdef __cplusplus\nextern \"C\" {{\n#endif\nvoid __cdecl {shutdown_symbol}(void);\n#ifdef __cplusplus\n}}\n#endif\n"));
    prepared.exports.push(Export {
        source_name: "@runtime_shutdown".into(),
        symbol: shutdown_symbol.clone(),
        rust_name: "runtime_shutdown".into(),
    });
    proof["runtime_shutdown_symbol"] = json!(shutdown_symbol);
    let metadata_digest = backend::sha256(metadata(&proof).to_string().as_bytes());
    let marker = format!("anode-metadata-v2:{metadata_digest}");
    let metadata_symbol = format!("{prefix}_metadata_receipt");
    prepared.c.push_str(&format!(
        "\nconst char* __cdecl {metadata_symbol}(void){{return \"{marker}\";}}\n"
    ));
    prepared.exports.push(Export {
        source_name: "@metadata_receipt".into(),
        symbol: metadata_symbol.clone(),
        rust_name: "metadata_receipt".into(),
    });
    proof["metadata_sha256"] = json!(metadata_digest);
    proof["metadata_symbol"] = json!(metadata_symbol);
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        std::env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "reload output has no parent"))?,
    )
    .map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.with_extension("zn-lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let (build, providers) = backend::build_reload_dll(
        &prepared,
        &output,
        &BuildOptions {
            optimize: options.optimize,
            force: options.force,
            cache_bytes: options.cache_bytes,
            sanitize: false,
            ffi_libraries: options.ffi_libraries.clone(),
        },
    )?;
    let object = output.with_extension("obj");
    let header = output.with_extension("h");
    let manifest = output.with_extension("json");
    let artifacts:Vec<_>=[&output,&object,&header].iter().map(|path| {
        let bytes=fs::read(path).map_err(io)?;
        Ok(json!({"file":path.file_name().unwrap().to_string_lossy(),"sha256":backend::sha256(&bytes),"bytes":bytes.len()}))
    }).collect::<Result<_>>()?;
    proof["artifacts"] = json!(artifacts);
    proof["ffi_libraries"] = providers;
    proof["reload_contract"] = json!({
            "boundary":"DLL function pointers through ReloadSlot; direct/inlined executable calls are not patched",
            "state":"legacy typed slots reject layout changes; dynamic slots own aligned POD/fixed bytes, hold all state borrows under the slot lock, and require an exact checked migration receipt for layout changes",
            "quiescence":"ReloadSlot replacement waits until all with_entry calls finish; host also synchronizes every state borrow and any external callbacks/threads",
            "lifetime":"old DLL stays loaded while any LoadedPatch clone/repeated load exists; final release drains its workers before unload; raw hosts call the verified runtime_shutdown_symbol after quiescence before FreeLibrary; raw addresses must not escape with_entry",
            "addresses":"identical function bodies may share an address; trusted hosts and providers use symbol names and receipts for identity",
            "trusted_native":"FFI providers execute trusted native code; source checks do not prove their memory/effect safety",
            "failures":"source contract/arithmetic/runtime failure terminates the host process"});
    proof["proof_sha256"] = json!(backend::sha256(proof.to_string().as_bytes()));
    backend::publish_library_artifact(
        &manifest,
        serde_json::to_vec_pretty(&proof).unwrap().as_slice(),
    )?;
    Ok(HotReloadPatch {
        output,
        object,
        header,
        manifest,
        symbols: public_exports,
        bytes: build.bytes,
        backend_ms: build.backend_ms,
        proof,
    })
}

fn manifest_artifact(manifest: &Path, entry: &Value) -> Result<PathBuf> {
    let file = entry["file"]
        .as_str()
        .ok_or_else(|| Error::new(0, "invalid reload artifact manifest"))?;
    if Path::new(file).components().count() != 1
        || !Path::new(file)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::new(
            0,
            "reload manifest artifact must be a sibling filename",
        ));
    }
    Ok(manifest.parent().unwrap_or(Path::new(".")).join(file))
}
fn verify_bytes(path: &Path, bytes: &[u8], entry: &Value) -> Result<()> {
    let actual = backend::sha256(bytes);
    if entry["sha256"].as_str() != Some(&actual)
        || entry["bytes"].as_u64() != Some(bytes.len() as u64)
    {
        let mut e =
            Error::new(0, "reload artifact does not match its persisted proof").in_file(path);
        e.details = Some(
            json!({"kind":"hot_reload_stale_artifact","expected":entry,"actual_sha256":actual,"actual_bytes":bytes.len()}),
        );
        return Err(e);
    }
    Ok(())
}
/// Validate exact native bytes before a host loads a previously built patch.
pub fn verify_artifact(manifest: &Path) -> Result<Value> {
    let proof: Value = serde_json::from_slice(&fs::read(manifest).map_err(io)?)
        .map_err(|e| Error::new(0, format!("invalid reload manifest: {e}")))?;
    if ![json!("anode-hot-reload-v1"), json!("anode-hot-reload-v2")].contains(&proof["format"])
        || proof["abi"] != "windows-x64-msvc-c-pod-v1"
    {
        return Err(Error::new(0, "unsupported reload manifest or ABI"));
    }
    let mut unsigned = proof.clone();
    let claimed = unsigned.as_object_mut().unwrap().remove("proof_sha256");
    if claimed.as_ref().and_then(Value::as_str)
        != Some(backend::sha256(unsigned.to_string().as_bytes()).as_str())
    {
        let mut error = Error::new(
            0,
            "reload manifest does not match its persisted proof digest",
        )
        .in_file(manifest);
        error.details = Some(json!({"kind":"hot_reload_stale_manifest"}));
        return Err(error);
    }
    let artifacts = proof["artifacts"]
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| Error::new(0, "invalid reload artifact manifest"))?;
    for entry in artifacts {
        let path = manifest_artifact(manifest, entry)?;
        verify_bytes(&path, &fs::read(&path).map_err(io)?, entry)?;
    }
    if proof["format"] == "anode-hot-reload-v2" {
        let digest = backend::sha256(metadata(&proof).to_string().as_bytes());
        let marker = format!("anode-metadata-v2:{digest}");
        let dll = manifest_artifact(manifest, &artifacts[0])?;
        let bytes = fs::read(&dll).map_err(io)?;
        if proof["metadata_sha256"].as_str() != Some(&digest)
            || !bytes.windows(marker.len()).any(|w| w == marker.as_bytes())
        {
            return Err(Error::new(
                0,
                "reload state/contract proof does not match its compiled metadata receipt",
            ));
        }
        for side in ["baseline", "updated"] {
            for value in proof[side]["state_layouts"]
                .as_object()
                .ok_or_else(|| Error::new(0, "missing state layout receipts"))?
                .values()
            {
                StateLayout::parse(value)?;
            }
        }
    }
    Ok(proof)
}

/// Require the proof to describe these exact fully checked source snapshots,
/// in addition to validating its native artifact bytes.
pub fn verify_artifact_for_sources(
    manifest: &Path,
    baseline: &Project,
    updated: &Project,
) -> Result<Value> {
    baseline.checked_reload()?;
    updated.checked_reload()?;
    let proof = verify_artifact(manifest)?;
    for (side, project) in [("baseline", baseline), ("updated", updated)] {
        let actual = source_snapshot(project);
        if proof[side]["snapshot"] != actual {
            let mut error = Error::new(0, "reload proof describes a different source snapshot")
                .in_file(manifest);
            error.details = Some(
                json!({"kind":"hot_reload_stale_sources","side":side,"expected":proof[side]["snapshot"],"actual":actual}),
            );
            return Err(error);
        }
    }
    Ok(proof)
}

#[derive(Clone, Debug)]
pub struct LoadedPatch {
    module: Arc<LoadedModule>,
    proof: Value,
}
#[derive(Debug)]
struct LoadedModule {
    handle: usize,
    shutdown: Option<unsafe extern "C" fn()>,
    // On Windows this open file denies writes/deletion while code is live.
    _file: fs::File,
}
#[cfg(windows)]
fn loaded_modules() -> &'static std::sync::Mutex<BTreeMap<usize, std::sync::Weak<LoadedModule>>> {
    static MODULES: std::sync::OnceLock<
        std::sync::Mutex<BTreeMap<usize, std::sync::Weak<LoadedModule>>>,
    > = std::sync::OnceLock::new();
    MODULES.get_or_init(Default::default)
}
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(
        path: *const u16,
        file: *mut std::ffi::c_void,
        flags: u32,
    ) -> *mut std::ffi::c_void;
    fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
    fn FreeLibrary(module: *mut std::ffi::c_void) -> i32;
}
impl Drop for LoadedModule {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let mut modules = loaded_modules().lock().unwrap_or_else(|e| e.into_inner());
            modules.remove(&self.handle);
            // Final Arc release is after all ReloadSlot call guards. Stop and
            // join DLL workers before their code or TLS storage is unloaded.
            if let Some(shutdown) = self.shutdown {
                shutdown();
            }
            FreeLibrary(self.handle as *mut std::ffi::c_void);
        }
    }
}
impl LoadedPatch {
    pub fn receipt(&self) -> &Value {
        &self.proof
    }
    pub fn state_layout(&self, name: &str) -> Result<StateLayout> {
        let value = &self.proof["updated"]["state_layouts"][name];
        if value.is_null() {
            return Err(Error::new(
                0,
                format!("patch has no public POD/fixed state layout {name}"),
            ));
        }
        StateLayout::parse(value)
    }
    /// Load verified native bytes. Keep this object or a ReloadSlot alive while
    /// calling its entrypoints; raw C caller alignment/alias rules still apply.
    #[cfg(windows)]
    pub fn load(manifest: &Path) -> Result<Self> {
        use std::{
            io::Read,
            os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
        };
        let proof = verify_artifact(manifest)?;
        let entry = &proof["artifacts"][0];
        let path = manifest_artifact(manifest, entry)?
            .canonicalize()
            .map_err(io)?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .map_err(io)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        verify_bytes(&path, &bytes, entry)?;
        let wide: Vec<_> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module =
            unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0x00000100 | 0x00001000) };
        if module.is_null() {
            return Err(Error::new(
                0,
                format!("LoadLibraryExW failed: {}", std::io::Error::last_os_error()),
            )
            .in_file(path));
        }
        let mut modules = loaded_modules().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(loaded) = modules
            .get(&(module as usize))
            .and_then(std::sync::Weak::upgrade)
        {
            // The shared owner already holds the code and a locked source file.
            unsafe {
                FreeLibrary(module);
            }
            return Ok(Self {
                module: loaded,
                proof,
            });
        }
        // Resolve while serialized with final unload. On error, release this
        // OS reference directly: constructing a Drop owner here would relock.
        let mut shutdown = None;
        if let Some(symbol) = proof["runtime_shutdown_symbol"].as_str() {
            let symbol = match std::ffi::CString::new(symbol) {
                Ok(symbol) => symbol,
                Err(_) => {
                    unsafe {
                        FreeLibrary(module);
                    }
                    return Err(Error::new(0, "invalid reload cleanup symbol"));
                }
            };
            let address = unsafe { GetProcAddress(module, symbol.as_ptr().cast()) };
            if address.is_null() {
                unsafe {
                    FreeLibrary(module);
                }
                return Err(Error::new(
                    0,
                    "reload DLL does not export its verified cleanup entrypoint",
                ));
            }
            shutdown = Some(unsafe {
                std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn()>(address)
            });
        }
        let loaded = Arc::new(LoadedModule {
            handle: module as usize,
            shutdown,
            _file: file,
        });
        modules.insert(module as usize, Arc::downgrade(&loaded));
        Ok(Self {
            module: loaded,
            proof,
        })
    }
    #[cfg(not(windows))]
    pub fn load(_manifest: &Path) -> Result<Self> {
        Err(Error::new(0, "reload DLL loading requires Windows x64"))
    }
    fn address(&self, name: &str) -> Result<*mut std::ffi::c_void> {
        let symbol = self.proof["symbols"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["source_name"] == name))
            .and_then(|s| s["symbol"].as_str())
            .ok_or_else(|| Error::new(0, format!("reload entrypoint {name} is not public")))?;
        #[cfg(windows)]
        {
            let symbol = std::ffi::CString::new(symbol)
                .map_err(|_| Error::new(0, "invalid reload symbol"))?;
            let address = unsafe {
                GetProcAddress(
                    self.module.handle as *mut std::ffi::c_void,
                    symbol.as_ptr().cast(),
                )
            };
            if address.is_null() {
                return Err(Error::new(
                    0,
                    "reload DLL does not export its declared entrypoint",
                ));
            }
            Ok(address)
        }
        #[cfg(not(windows))]
        {
            let _ = symbol;
            Err(Error::new(0, "reload DLL loading requires Windows x64"))
        }
    }
}
/// A controlled function-pointer boundary. The read guard spans the complete
/// call; replacement acquires the write guard and waits for all calls to end.
pub struct ReloadSlot {
    current: RwLock<SlotGeneration>,
}
struct SlotGeneration {
    patch: LoadedPatch,
    state: Option<DynamicState>,
}
struct DynamicState {
    layout: StateLayout,
    storage: Vec<u64>,
}
impl DynamicState {
    fn new(layout: StateLayout, initial: Option<&[u8]>) -> Result<Self> {
        let mut out = Self {
            storage: vec![0; layout.size.div_ceil(8)],
            layout,
        };
        if let Some(initial) = initial {
            if initial.len() != out.layout.size {
                return Err(Error::new(
                    0,
                    "initial state bytes must exactly match the verified state layout",
                ));
            }
            out.bytes_mut().copy_from_slice(initial);
        }
        validate_pod_bytes(&out.layout.schema, out.bytes())?;
        Ok(out)
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.storage.as_ptr().cast(), self.layout.size) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.storage.as_mut_ptr().cast(), self.layout.size)
        }
    }
}
/// Independent copied bytes. Old snapshots never acquire a replacement's layout.
#[derive(Clone, Debug)]
pub struct StateSnapshot {
    pub layout: StateLayout,
    pub bytes: Vec<u8>,
    /// Code/source proof copied under the same guard as the layout and bytes.
    pub receipt: Value,
}
fn strict_patch_compatibility(current: &LoadedPatch, next: &LoadedPatch) -> Result<()> {
    let mut differences = Vec::new();
    for (key, category) in [
        ("layouts", "layout"),
        ("public_contracts", "public_contract"),
    ] {
        let old: BTreeMap<String, Value> =
            serde_json::from_value(current.proof["updated"][key].clone())
                .map_err(|_| Error::new(0, "invalid live reload proof"))?;
        let new: BTreeMap<String, Value> =
            serde_json::from_value(next.proof["updated"][key].clone())
                .map_err(|_| Error::new(0, "invalid replacement reload proof"))?;
        compatibility(&old, &new, category, &mut differences);
    }
    if !differences.is_empty() {
        return Err(incompatible(differences));
    }
    Ok(())
}
impl ReloadSlot {
    pub fn new(patch: LoadedPatch) -> Self {
        Self {
            current: RwLock::new(SlotGeneration { patch, state: None }),
        }
    }
    /// Own aligned dynamic POD/fixed state. Hosts may use the bytes only inside
    /// with_state_entry and must match that call's exact layout receipt. No
    /// borrowed or raw pointer into the owned state may outlive the closure.
    pub fn new_with_state(patch: LoadedPatch, state_type: &str, initial: &[u8]) -> Result<Self> {
        let layout = patch.state_layout(state_type)?;
        let state = DynamicState::new(layout, Some(initial))?;
        Ok(Self {
            current: RwLock::new(SlotGeneration {
                patch,
                state: Some(state),
            }),
        })
    }
    pub fn state_snapshot(&self) -> Result<StateSnapshot> {
        let current = self
            .current
            .read()
            .map_err(|_| Error::new(0, "reload state lock was poisoned"))?;
        let state = current
            .state
            .as_ref()
            .ok_or_else(|| Error::new(0, "slot has no owning dynamic state"))?;
        validate_pod_bytes(&state.layout.schema, state.bytes())?;
        Ok(StateSnapshot {
            layout: state.layout.clone(),
            bytes: state.bytes().to_vec(),
            receipt: current.patch.proof.clone(),
        })
    }
    /// The write guard spans the complete state call, so state mutation and a
    /// migration cannot overlap. Unsafe C ABI casts remain a host obligation;
    /// the closure receives the active layout, never a stale typed reference.
    pub fn with_state_entry<R>(
        &self,
        name: &str,
        call: impl FnOnce(*mut std::ffi::c_void, &mut [u8], &StateLayout) -> R,
    ) -> Result<R> {
        let mut current = self
            .current
            .write()
            .map_err(|_| Error::new(0, "reload state call lock was poisoned"))?;
        let address = current.patch.address(name)?;
        let state = current
            .state
            .as_mut()
            .ok_or_else(|| Error::new(0, "slot has no owning dynamic state"))?;
        let layout = state.layout.clone();
        validate_pod_bytes(&layout.schema, state.bytes())?;
        let previous = state.bytes().to_vec();
        let result = call(address, state.bytes_mut(), &layout);
        if let Err(error) = validate_pod_bytes(&layout.schema, state.bytes()) {
            state.bytes_mut().copy_from_slice(&previous);
            return Err(error);
        }
        Ok(result)
    }
    /// The closure receives the exact generated C ABI address. Calling it
    /// requires an unsafe cast with the matching header signature. Addresses
    /// must not escape the closure; synchronize host state aliases and any
    /// external callbacks/threads before returning from the closure.
    pub fn with_entry<R>(
        &self,
        name: &str,
        call: impl FnOnce(*mut std::ffi::c_void) -> R,
    ) -> Result<R> {
        let current = self
            .current
            .read()
            .map_err(|_| Error::new(0, "reload call lock was poisoned"))?;
        Ok(call(current.patch.address(name)?))
    }
    /// Wait for quiescence, verify the actual live patch's layouts/contracts,
    /// then switch subsequent calls. The returned old patch may be retained.
    pub fn replace(&self, next: LoadedPatch) -> Result<LoadedPatch> {
        let mut current = self
            .current
            .write()
            .map_err(|_| Error::new(0, "reload swap lock was poisoned"))?;
        strict_patch_compatibility(&current.patch, &next)?;
        Ok(std::mem::replace(&mut current.patch, next))
    }
    /// Wait for every live call, require the exact active source/layout proof,
    /// execute the compiled no_io migration into disjoint aligned storage, then
    /// atomically publish code, state and receipt under this same write guard.
    /// Legacy typed slots cannot use this method. Retired DLLs remain owned by
    /// the returned patch and any previously retained LoadedPatch clones.
    pub fn replace_state(&self, next: LoadedPatch) -> Result<LoadedPatch> {
        let mut current = self
            .current
            .write()
            .map_err(|_| Error::new(0, "reload migration lock was poisoned"))?;
        if current.patch.proof["updated"]["snapshot"] != next.proof["baseline"]["snapshot"] {
            return Err(Error::new(
                0,
                "migration proof baseline is stale for the active slot generation",
            ));
        }
        let state=current.state.as_ref().ok_or_else(||Error::new(0,"migration requires an owning dynamic-state slot; typed host references cannot change layout"))?;
        validate_pod_bytes(&state.layout.schema, state.bytes())?;
        let migration = &next.proof["migration"];
        let new_state = if migration.is_null() {
            strict_patch_compatibility(&current.patch, &next)?;
            let output = next.state_layout(&state.layout.name)?;
            if output != state.layout {
                return Err(Error::new(
                    0,
                    "changed state layout requires a compiled checked migration",
                ));
            }
            None
        } else {
            let baseline = StateLayout::parse(&migration["baseline_state"])?;
            let input = StateLayout::parse(&migration["input"])?;
            let output = StateLayout::parse(&migration["output"])?;
            if baseline != state.layout
                || input.digest != state.layout.digest
                || next.state_layout(&output.name)? != output
                || migration["checked_no_io"] != true
                || migration["abi"] != "const-input-pointer-output-pointer-last"
            {
                return Err(Error::new(
                    0,
                    "migration input/output receipts do not match the exact active state layout",
                ));
            }
            let function = migration["function"]
                .as_str()
                .ok_or_else(|| Error::new(0, "migration function receipt is missing"))?;
            let address = next.address(function)?;
            let mut output_state = DynamicState::new(output, None)?;
            // The compiled, artifact-bound receipt proves this exact aggregate
            // C ABI. POD input/output allocations are disjoint and aligned.
            let migrate: unsafe extern "C" fn(*const u8, *mut u8) =
                unsafe { std::mem::transmute(address) };
            unsafe {
                migrate(
                    state.bytes().as_ptr(),
                    output_state.bytes_mut().as_mut_ptr(),
                );
            }
            validate_pod_bytes(&output_state.layout.schema, output_state.bytes())?;
            Some(output_state)
        };
        if let Some(state) = new_state {
            current.state = Some(state);
        }
        Ok(std::mem::replace(&mut current.patch, next))
    }
}
