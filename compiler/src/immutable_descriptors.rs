//! Hoist only descriptors actually used through a shared parameter and a
//! record-only projection. Never speculate a read of an inactive enum payload,
//! an element alias, a local owner, or a sibling omitted by a field view.
use crate::{
    ir::{Argument, CheckedFunction, Operation, Place, Rvalue},
    syntax::{Borrow, DataKind, Type},
    types::TypeTable,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) type Key = (usize, Vec<usize>);
pub(crate) fn key(p: &Place) -> Key {
    (p.local, p.fields.clone())
}
pub(crate) fn descriptor(ty: Type, types: &TypeTable) -> bool {
    matches!(
        ty,
        Type::Text | Type::TextView | Type::Ints | Type::Slice(_) | Type::VectorView(_)
    ) || types.vector_element(ty).is_some()
        || types.soa_element(ty).is_some()
}
/// A scoped span may live in two SSA variables only when all uses are inline
/// descriptor reads. Calls, transfers and pair helpers retain an address.
pub(crate) fn local_views(f: &CheckedFunction) -> Vec<bool> {
    fn reject(p: &Place, eligible: &mut [bool]) {
        eligible[p.local] = false;
    }
    fn walk(ops: &[Operation], eligible: &mut [bool]) {
        let mut pending: Vec<_> = ops.iter().rev().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Init(d, value) => {
                    if !matches!(value, Rvalue::Region { .. } | Rvalue::Zero) {
                        reject(d, eligible);
                    }
                    match value {
                        Rvalue::Builtin { name, args } => {
                            let inline = matches!(
                                name.as_str(),
                                "len"
                                    | "fixed_len"
                                    | "fixed_at"
                                    | "fixed_set"
                                    | "fixed_try_set"
                                    | "vec_at"
                                    | "vec_clone_at"
                                    | "vec_replace"
                            ) || name.starts_with("read_u")
                                || name.starts_with("try_write_u");
                            // 1.3: try_read_bytes(path, dst) writes dst (argument 1)
                            // through the runtime. Like every other non-inline
                            // builtin it rejects all of its arguments, so dst is
                            // never treated as an immutable descriptor.
                            for (i, p) in args.iter().enumerate() {
                                if i != 0 || !inline {
                                    reject(p, eligible);
                                }
                            }
                        }
                        Rvalue::Call { args, .. } => {
                            for arg in args {
                                let (Argument::Value(p) | Argument::Reference(p)) = arg;
                                reject(p, eligible);
                            }
                        }
                        Rvalue::Copy(p)
                        | Rvalue::Reference(p)
                        | Rvalue::Convert(p)
                        | Rvalue::Unary(_, p)
                        | Rvalue::Repeat(p, _) => reject(p, eligible),
                        Rvalue::Binary(_, a, b) => {
                            reject(a, eligible);
                            reject(b, eligible);
                        }
                        Rvalue::Record(fields) => {
                            for p in fields {
                                reject(p, eligible);
                            }
                        }
                        Rvalue::Variant {
                            payload: Some(p), ..
                        }
                        | Rvalue::IntRange { value: p, .. } => reject(p, eligible),
                        _ => {}
                    }
                }
                Operation::Move {
                    destination,
                    source,
                    ..
                }
                | Operation::Assign(destination, source)
                | Operation::Swap(destination, source) => {
                    reject(destination, eligible);
                    reject(source, eligible);
                }
                Operation::ElementCheckPair { owner, .. }
                | Operation::ElementWriteback { owner, .. } => reject(owner, eligible),
                Operation::Return(Some(p)) => reject(p, eligible),
                Operation::If { yes, no, .. } => {
                    pending.extend(no.iter().rev());
                    pending.extend(yes.iter().rev());
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    pending.extend(body.iter().rev());
                    pending.extend(condition_code.iter().rev());
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms.iter().rev() {
                        pending.extend(arm.iter().rev());
                    }
                }
                _ => {}
            }
        }
    }
    let mut eligible: Vec<_> = f
        .locals
        .iter()
        .enumerate()
        .map(|(i, l)| {
            i >= f.source.params.len()
                && l.borrow == Borrow::Owned
                && matches!(l.ty, Type::Slice(_) | Type::VectorView(_))
        })
        .collect();
    walk(&f.body, &mut eligible);
    eligible
}
pub(crate) fn address_exposed(f: &CheckedFunction) -> Vec<bool> {
    fn walk(ops: &[Operation], exposed: &mut [bool]) {
        let mut pending: Vec<_> = ops.iter().rev().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Init(_, Rvalue::Call { args, .. }) => {
                    for arg in args {
                        if let Argument::Reference(p) = arg {
                            exposed[p.local] = true;
                        }
                    }
                }
                Operation::Init(_, Rvalue::Reference(p)) => exposed[p.local] = true,
                // try_read_bytes(path, dst) writes dst (argument 1) through the runtime.
                Operation::Init(_, Rvalue::Builtin { name, args })
                    if matches!(
                        name.as_str(),
                        "channel_recv" | "channel_try_recv" | "try_read_bytes"
                    ) =>
                {
                    if let Some(p) = args.get(1) {
                        exposed[p.local] = true;
                    }
                }
                Operation::If { yes, no, .. } => {
                    pending.extend(no.iter().rev());
                    pending.extend(yes.iter().rev());
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    pending.extend(body.iter().rev());
                    pending.extend(condition_code.iter().rev());
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms.iter().rev() {
                        pending.extend(arm.iter().rev());
                    }
                }
                _ => {}
            }
        }
    }
    let mut exposed = vec![false; f.locals.len()];
    walk(&f.body, &mut exposed);
    exposed
}

/// Borrowed `VectorView` parameters of `f` whose descriptor storage no
/// operation of `f` writes, moves, swaps or drops. The checker already rejects
/// assignment to any borrowed binding; this re-verifies it on the IR.
fn readonly_views(f: &CheckedFunction) -> Vec<usize> {
    fn walk(ops: &[Operation], written: &mut BTreeSet<usize>) {
        for op in ops {
            match op {
                Operation::Init(p, _) | Operation::Assign(p, _) | Operation::Drop(p) => {
                    written.insert(p.local);
                }
                Operation::Move {
                    destination,
                    source,
                    ..
                }
                | Operation::Swap(destination, source) => {
                    written.insert(destination.local);
                    written.insert(source.local);
                }
                Operation::ElementWriteback { alias, owner, .. } => {
                    written.insert(alias.local);
                    written.insert(owner.local);
                }
                Operation::If { yes, no, .. } => {
                    walk(yes, written);
                    walk(no, written);
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    walk(condition_code, written);
                    walk(body, written);
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms {
                        walk(arm, written);
                    }
                }
                _ => {}
            }
        }
    }
    let mut written = BTreeSet::new();
    walk(&f.body, &mut written);
    f.source
        .params
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            matches!(p.ty, Type::VectorView(_))
                && p.borrow != Borrow::Owned
                && f.locals.get(*i).is_some_and(|l| l.borrow != Borrow::Owned)
                && !written.contains(i)
        })
        .map(|(i, _)| i)
        .collect()
}
/// Borrowed view parameters whose (data, len) descriptor is fixed for a whole
/// call. Sound only when *every* checked function leaves all of its borrowed
/// view parameters unwritten: a view lent onward by reference can then never
/// be replaced by a callee either. Element stores never touch the descriptor.
pub(crate) fn pinned_views<'a>(
    functions: impl Iterator<Item = &'a CheckedFunction> + Clone,
) -> BTreeMap<String, Vec<usize>> {
    let every = functions.clone().all(|f| {
        f.source.external_symbol.is_some()
            || f.source
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| matches!(p.ty, Type::VectorView(_)) && p.borrow != Borrow::Owned)
                .count()
                == readonly_views(f).len()
    });
    if !every {
        return BTreeMap::new();
    }
    functions
        .filter(|f| f.source.external_symbol.is_none())
        .map(|f| (f.source.name.clone(), readonly_views(f)))
        .filter(|(_, views)| !views.is_empty())
        .collect()
}
/// Locals whose storage (and so any descriptor reached through them) `f`
/// itself may replace: written, moved, swapped, written back, dropped,
/// exclusively loaned, passed by value or by exclusive reference, or handed to
/// a resizing/consuming builtin. `exclusive(callee, i)` reports whether
/// parameter `i` of `callee` is an exclusive borrow. The checker already
/// freezes shared borrows for a call; this re-verifies it on the IR, so a
/// pinned descriptor never depends on checker behavior alone.
pub(crate) fn replaced(
    f: &CheckedFunction,
    exclusive: &dyn Fn(&str, usize) -> bool,
) -> BTreeSet<usize> {
    fn walk(ops: &[Operation], exclusive: &dyn Fn(&str, usize) -> bool, out: &mut BTreeSet<usize>) {
        for op in ops {
            match op {
                Operation::Init(p, value) => {
                    out.insert(p.local);
                    match value {
                        Rvalue::Call { name, args, spawn } => {
                            for (i, arg) in args.iter().enumerate() {
                                match arg {
                                    Argument::Reference(p) if *spawn || exclusive(name, i) => {
                                        out.insert(p.local);
                                    }
                                    Argument::Value(p) => {
                                        out.insert(p.local);
                                    }
                                    Argument::Reference(_) => {}
                                }
                            }
                        }
                        Rvalue::Builtin { name, args } => {
                            let consuming = matches!(
                                name.as_str(),
                                "push" | "texts_push" | "vec_push" | "vec_pop" | "join"
                            ) || name.starts_with("map_")
                                || name.starts_with("arena_")
                                || name.starts_with("channel_");
                            if consuming {
                                for p in args {
                                    out.insert(p.local);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Operation::Assign(a, b)
                | Operation::Swap(a, b)
                | Operation::Move {
                    destination: a,
                    source: b,
                    ..
                }
                | Operation::ElementWriteback {
                    alias: a, owner: b, ..
                } => {
                    out.insert(a.local);
                    out.insert(b.local);
                }
                Operation::Drop(p) => {
                    out.insert(p.local);
                }
                Operation::Loan {
                    place,
                    kind: Borrow::Exclusive,
                    ..
                } => {
                    out.insert(place.local);
                }
                Operation::If { yes, no, .. } => {
                    walk(yes, exclusive, out);
                    walk(no, exclusive, out);
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    walk(condition_code, exclusive, out);
                    walk(body, exclusive, out);
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms {
                        walk(arm, exclusive, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&f.body, exclusive, &mut out);
    out
}
pub(crate) fn used(f: &CheckedFunction, types: &TypeTable) -> Vec<Place> {
    fn mutations(ops: &[Operation], locals: &mut BTreeSet<usize>) {
        for op in ops {
            match op {
                Operation::Swap(a, b) => {
                    locals.insert(a.local);
                    locals.insert(b.local);
                }
                Operation::ElementWriteback { alias, owner, .. } => {
                    locals.insert(alias.local);
                    locals.insert(owner.local);
                }
                Operation::If { yes, no, .. } => {
                    mutations(yes, locals);
                    mutations(no, locals);
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    mutations(condition_code, locals);
                    mutations(body, locals);
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms {
                        mutations(arm, locals);
                    }
                }
                _ => {}
            }
        }
    }
    fn safe(p: &Place, f: &CheckedFunction, types: &TypeTable) -> bool {
        let Some(param) = f.source.params.get(p.local) else {
            return false;
        };
        if param.borrow != Borrow::Shared || !descriptor(p.ty, types) {
            return false;
        }
        let mut ty = param.ty;
        for &field in &p.fields {
            let Type::Named(id) = ty else { return false };
            let DataKind::Record(fields) = &types.definitions[id].kind else {
                return false;
            };
            ty = fields[field].ty;
        }
        ty == p.ty
    }
    fn walk(
        ops: &[Operation],
        f: &CheckedFunction,
        types: &TypeTable,
        out: &mut BTreeMap<Key, Place>,
    ) {
        for op in ops {
            match op {
                Operation::Init(_, Rvalue::Builtin { name, args })
                    if [
                        "len",
                        "at",
                        "byte_at",
                        "set",
                        "vec_at",
                        "vec_clone_at",
                        "vec_replace",
                        "fixed_len",
                        "fixed_at",
                        "fixed_set",
                    ]
                    .contains(&name.as_str()) =>
                {
                    if let Some(p) = args.first().filter(|p| safe(p, f, types)) {
                        out.insert(key(p), p.clone());
                    }
                }
                Operation::If { yes, no, .. } => {
                    walk(yes, f, types, out);
                    walk(no, f, types, out);
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    walk(condition_code, f, types, out);
                    walk(body, f, types, out);
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms {
                        walk(arm, f, types, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut places = BTreeMap::new();
    walk(&f.body, f, types, &mut places);
    let mut mutated = BTreeSet::new();
    mutations(&f.body, &mut mutated);
    places
        .into_values()
        .filter(|p| !mutated.contains(&p.local))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    #[test]
    fn shared_record_paths_include_used_fields_only() {
        let project = Project::from_source(
            "struct Inner{a:ints,b:ints} struct Outer{inner:Inner,label:text}
             fn read(x:&Outer.{inner})->int{return len(x.inner.a);}
             fn main(){}",
        )
        .unwrap();
        let (p, _) = project.checked(false).unwrap();
        let f = p
            .functions
            .iter()
            .find(|f| f.source.name.ends_with("_read"))
            .unwrap();
        let places = used(f, &p.types);
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].fields, [0, 0]);
    }
    #[test]
    fn replaced_locals_cover_writes_exclusive_loans_and_resizing_builtins() {
        let project = Project::from_source(
            "fn grow(v:&mut ints,s:&ints)->int{push(v,len(s));return len(v);}
             fn read(s:&ints,t:&text)->int{return len(s)+len(t);}
             fn main(){}",
        )
        .unwrap();
        let (p, _) = project.checked(false).unwrap();
        let find = |suffix: &str| {
            p.functions
                .iter()
                .find(|f| f.source.name.ends_with(suffix))
                .unwrap()
        };
        let grow = replaced(find("_grow"), &|_, _| true);
        assert!(grow.contains(&0) && !grow.contains(&1), "{grow:?}");
        let read = replaced(find("_read"), &|_, _| true);
        assert!(!read.contains(&0) && !read.contains(&1), "{read:?}");
    }
    #[test]
    fn mutable_records_and_enum_payloads_are_never_hoisted() {
        let project = Project::from_source(
            "struct Data{a:ints} enum Choice{Some(Data),None}
             fn read(x:&mut Data)->int{return len(x.a);}
             fn variant(x:&Choice)->int{match &x{Choice::Some(d)=>{return len(d.a);}Choice::None=>{return 0;}}}
             fn main(){}",
        ).unwrap();
        let (p, _) = project.checked(false).unwrap();
        for f in &p.functions {
            assert!(used(f, &p.types).is_empty());
        }
    }
}
