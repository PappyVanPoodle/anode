//! Conservative, transitive effects over ownership-checked IR. Intent prose
//! is never interpreted as a proof or a permission to skip checking.
use crate::{ir::*, syntax::Type, Error, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub alloc: bool,
    pub io: bool,
    /// A finite source control-flow/call-graph proof, never a wall-clock limit.
    pub bounded: bool,
    pub calls: BTreeSet<String>,
}
fn inspect(ops: &[Operation], out: &mut Effects, types: &crate::types::TypeTable) {
    for op in ops {
        match op {
            Operation::Init(dst, rvalue) => match rvalue {
                Rvalue::Text(_) => out.alloc = true,
                Rvalue::Call { name, spawn, .. } => {
                    out.calls.insert(name.clone());
                    if *spawn {
                        out.alloc = true;
                        out.io = true;
                    }
                }
                Rvalue::Builtin { name, args } => {
                    if name == "vec_replace"
                        && args
                            .first()
                            .and_then(|p| types.vector_element(p.ty))
                            .is_some_and(|element| types.contains_channel(element))
                    {
                        out.io = true;
                    }
                    if name.starts_with("channel_")
                        || (["clone", "vec_clone_at", "map_clone_get", "arena_clone_get"]
                            .contains(&name.as_str())
                            && types.contains_channel(dst.ty))
                    {
                        out.io = true;
                    }
                    if matches!(
                        name.as_str(),
                        "process_run"
                            | "net_request"
                            | "net_serve_once"
                            | "http_request"
                            | "try_read_bytes"
                            | "try_write_bytes"
                            | "env_var"
                            | "create_dir"
                            | "remove_file"
                            | "read_text"
                            | "write_text"
                            | "try_read_text"
                            | "try_write_text"
                            | "arg"
                            | "args_count"
                            | "join"
                            | "monotonic_ms"
                            | "file_exists"
                            | "list_dir"
                            | "exit_code"
                            | "print_text"
                            | "print_text_view"
                            | "print_int"
                            | "print_uint"
                            | "print_float"
                            | "print_bool"
                    ) {
                        out.io = true;
                    }
                    if !matches!(
                        name.as_str(),
                        "vec_replace"
                            | "vec_at"
                            | "arena_len"
                            | "arena_contains_id"
                            | "arena_free"
                            | "vec_capacity"
                            | "map_len"
                            | "map_contains_key"
                            | "map_remove"
                            | "len"
                            | "at"
                            | "set"
                            | "byte_at"
                            | "parse_int"
                            | "sqrt"
                            | "abs"
                            | "is_finite"
                            | "is_nan"
                            | "is_int"
                            | "text_eq"
                            | "contains"
                            | "view_contains"
                            | "char_count"
                            | "is_char_boundary"
                            | "codepoint_at"
                            | "next_char_boundary"
                            | "text_find"
                            | "text_starts_with"
                            | "text_ends_with"
                            | "args_count"
                            | "join"
                            | "monotonic_ms"
                            | "exit_code"
                            | "fixed_len"
                            | "fixed_at"
                            | "fixed_set"
                            | "fixed_try_set"
                            | "saturating_add"
                            | "saturating_sub"
                            | "read_u16_le"
                            | "read_u32_le"
                            | "read_u64_le"
                            | "read_u16_be"
                            | "read_u32_be"
                            | "read_u64_be"
                            | "try_write_u16_le"
                            | "try_write_u32_le"
                            | "try_write_u64_le"
                            // 1.2: big-endian wire writes (the checker
                            // canonicalizes the wire_ aliases; both spellings
                            // are listed) and the pure FNV-1a hash. sha256_text
                            // returns new text and net_request allocates its
                            // result, so both stay allocating.
                            | "try_write_u16_be"
                            | "try_write_u32_be"
                            | "try_write_u64_be"
                            | "wire_try_write_u16_be"
                            | "wire_try_write_u32_be"
                            | "wire_try_write_u64_be"
                            | "fnv1a_64"
                            // 1.3: byte file I/O and directory/file operations
                            // transfer into or out of caller storage only.
                            // env_var and net_serve_once return new text.
                            | "try_read_bytes"
                            | "try_write_bytes"
                            | "create_dir"
                            | "remove_file"
                    ) && crate::syntax::integer_intrinsic(name).is_none()
                        && !(name.starts_with("channel_") && name != "channel_new")
                        && !(name == "clone"
                            && (matches!(dst.ty, Type::Fixed(..))
                                || types.channel_element(dst.ty).is_some()))
                        && !(name == "vec_clone_at" && !dst.ty.owned())
                        && !(name == "map_clone_get" && !dst.ty.owned())
                        && !(name == "arena_clone_get" && !dst.ty.owned())
                    {
                        // clone of any data type may transitively own heap data.
                        // push may grow; file/console libraries may allocate.
                        out.alloc = true;
                    }
                }
                _ => {
                    let _ = dst;
                }
            },
            Operation::Drop(p) if p.ty.task_result().is_some() => out.io = true,
            Operation::Drop(p) if types.contains_channel(p.ty) => out.io = true,
            Operation::If { yes, no, .. } => {
                inspect(yes, out, types);
                inspect(no, out, types);
            }
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                inspect(condition_code, out, types);
                inspect(body, out, types);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    inspect(arm, out, types);
                }
            }
            _ => {}
        }
    }
}
fn source_has_while(body: &[crate::syntax::Stmt]) -> bool {
    use crate::syntax::StmtKind;
    let mut pending: Vec<_> = body.iter().collect();
    while let Some(stmt) = pending.pop() {
        match &stmt.kind {
            StmtKind::While(..) => return true,
            StmtKind::If(_, yes, no) => pending.extend(yes.iter().chain(no)),
            StmtKind::Match(_, arms) => {
                for arm in arms {
                    pending.extend(&arm.body);
                }
            }
            StmtKind::ForRange { body, .. }
            | StmtKind::ForEach { body, .. }
            | StmtKind::Element { body, .. }
            | StmtKind::ElementPair { body, .. }
            | StmtKind::Region { body, .. } => pending.extend(body),
            _ => {}
        }
    }
    false
}
fn may_wait(ops: &[Operation]) -> bool {
    ops.iter().any(|op| match op {
        Operation::Init(_, Rvalue::Builtin { name, .. }) => {
            // A network request waits on a peer (like a child process), so it
            // never contributes to a `bounded` proof.
            [
                "channel_send",
                "channel_recv",
                "join",
                "process_run",
                "net_request",
                "net_serve_once",
                "http_request",
            ]
            .contains(&name.as_str())
        }
        Operation::Init(_, Rvalue::Call { spawn: true, .. }) => true,
        Operation::Drop(p) => p.ty.task_result().is_some(),
        Operation::If { yes, no, .. } => may_wait(yes) || may_wait(no),
        Operation::Loop {
            condition_code,
            body,
            ..
        } => may_wait(condition_code) || may_wait(body),
        Operation::Switch { arms, .. } => arms.iter().any(|a| may_wait(a)),
        _ => false,
    })
}
pub fn analyze(program: &CheckedProgram) -> BTreeMap<String, Effects> {
    let mut result = BTreeMap::new();
    for f in &program.functions {
        let mut e = Effects::default();
        if f.source.external_symbol.is_some() {
            e.alloc = true;
            e.io = true;
        }
        inspect(&f.body, &mut e, &program.types);
        result.insert(f.source.name.clone(), e);
    }
    loop {
        let old = result.clone();
        for effect in result.values_mut() {
            for call in &effect.calls {
                if let Some(callee) = old.get(call) {
                    effect.alloc |= callee.alloc;
                    effect.io |= callee.io;
                } else {
                    effect.alloc = true;
                    effect.io = true;
                }
            }
        }
        if result == old {
            break;
        }
    }
    // Prove bottom-up from finite leaves. Cycles never acquire a proof, even
    // when hidden behind a branch or a different exported function. Every
    // user callee must explicitly publish bounded as part of its contract.
    loop {
        let old = result.clone();
        for f in &program.functions {
            let bounded = f.source.external_symbol.is_none()
                && !source_has_while(&f.source.body)
                && !may_wait(&f.body)
                && result[&f.source.name].calls.iter().all(|call| {
                    old.get(call).is_some_and(|e| e.bounded)
                        && program.functions.iter().any(|callee| {
                            callee.source.name == *call
                                && callee.source.effects.iter().any(|e| e == "bounded")
                        })
                });
            result.get_mut(&f.source.name).unwrap().bounded = bounded;
        }
        if result == old {
            return result;
        }
    }
}
pub fn validate(program: &CheckedProgram) -> Result<()> {
    // Existing code without contracts pays no effect-analysis cost.
    if program
        .functions
        .iter()
        .all(|f| f.source.effects.is_empty())
    {
        return Ok(());
    }
    let summary = analyze(program);
    for f in &program.functions {
        let e = &summary[&f.source.name];
        for effect in &f.source.effects {
            if (effect == "no_alloc" && e.alloc)
                || (effect == "no_io" && e.io)
                || (effect == "bounded" && !e.bounded)
            {
                return Err(Error::new(
                    f.source.line,
                    format!(
                        "effect contract {effect} violated in {} (including transitive calls)",
                        f.source.name
                    ),
                ));
            }
        }
    }
    Ok(())
}
