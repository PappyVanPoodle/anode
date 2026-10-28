//! Dependency fingerprints for checked native modules. Source coordinates are
//! excluded unless emitted in checked IR (notably runtime assertion lines).
//! Dependencies include helper bodies used by inlining/range/effect analyses,
//! not just public signatures. This never substitutes for frontend validation.
use crate::{
    ir::*,
    syntax::{DataKind, Type},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
};
#[derive(Debug)]
pub(crate) struct Unit {
    pub name: String,
    pub functions: BTreeSet<String>,
    pub required: BTreeSet<String>,
    pub fingerprint: u64,
}
fn walk<'a>(ops: &'a [Operation], visit: &mut impl FnMut(&'a Operation)) {
    for op in ops {
        visit(op);
        match op {
            Operation::If { yes, no, .. } => {
                walk(yes, visit);
                walk(no, visit);
            }
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                walk(condition_code, visit);
                walk(body, visit);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    walk(arm, visit);
                }
            }
            _ => {}
        }
    }
}
fn worker_names(program: &CheckedProgram) -> BTreeSet<&str> {
    let mut workers = BTreeSet::new();
    for f in &program.functions {
        walk(&f.body, &mut |op| {
            if let Operation::Init(
                _,
                Rvalue::Call {
                    name, spawn: true, ..
                },
            ) = op
            {
                workers.insert(name.as_str());
            }
        });
    }
    workers
}
pub(crate) fn workers(program: &CheckedProgram) -> BTreeSet<String> {
    worker_names(program)
        .into_iter()
        .map(str::to_owned)
        .collect()
}
pub(crate) fn plan(program: &CheckedProgram, hot: &BTreeSet<String>) -> Vec<Unit> {
    let functions: BTreeMap<_, _> = program
        .functions
        .iter()
        .map(|f| (f.source.name.as_str(), f))
        .collect();
    let workers = worker_names(program);
    let mut modules = BTreeMap::<&str, BTreeSet<&str>>::new();
    for f in &program.functions {
        modules
            .entry(f.source.module.as_str())
            .or_default()
            .insert(f.source.name.as_str());
    }
    modules
        .into_iter()
        .map(|(name, defined)| {
            let mut required = defined.clone();
            let mut pending: Vec<_> = defined.iter().copied().collect();
            while let Some(name) = pending.pop() {
                if let Some(f) = functions.get(name) {
                    walk(&f.body, &mut |op| {
                        if let Operation::Init(_, Rvalue::Call { name, .. }) = op {
                            if required.insert(name.as_str()) {
                                pending.push(name.as_str());
                            }
                        }
                    });
                }
            }
            let mut key = std::collections::hash_map::DefaultHasher::new();
            "anode-native-unit-v028-xmm-static-text-pool".hash(&mut key);
            crate::backend::native_isa_features().hash(&mut key);
            program.allow_process.hash(&mut key);
            name.hash(&mut key);
            defined.hash(&mut key);
            let mut types = BTreeSet::new();
            for name in &required {
                if let Some(f) = functions.get(name) {
                    f.source.module.hash(&mut key);
                    program
                        .module_identities
                        .get(&f.source.module)
                        .hash(&mut key);
                    f.source.name.hash(&mut key);
                    f.source.ret.hash(&mut key);
                    f.source.params.hash(&mut key);
                    f.source.external_symbol.hash(&mut key);
                    f.source.effects.hash(&mut key);
                    f.locals.hash(&mut key);
                    f.body.hash(&mut key);
                    for ty in f
                        .source
                        .params
                        .iter()
                        .map(|p| p.ty)
                        .chain(std::iter::once(f.source.ret))
                        .chain(f.locals.iter().map(|l| l.ty))
                    {
                        if let Type::Named(id) | Type::Id(id) | Type::VectorView(id) =
                            ty.task_result().unwrap_or(ty)
                        {
                            types.insert(id);
                        }
                    }
                }
            }
            let mut pending: Vec<_> = types.iter().copied().collect();
            while let Some(id) = pending.pop() {
                let fields = match &program.types.definitions[id].kind {
                    DataKind::Record(f)
                    | DataKind::Enum(f)
                    | DataKind::Vector(f)
                    | DataKind::SoaVector(f)
                    | DataKind::Arena(f)
                    | DataKind::Channel(f)
                    | DataKind::Map(f) => f,
                };
                for field in fields {
                    if let Type::Named(child) | Type::Id(child) | Type::VectorView(child) =
                        field.ty.task_result().unwrap_or(field.ty)
                    {
                        if types.insert(child) {
                            pending.push(child);
                        }
                    }
                }
            }
            for id in types {
                id.hash(&mut key);
                program.types.definitions[id].name.hash(&mut key);
                program.types.definitions[id].kind.hash(&mut key);
                program.types.layouts[id].hash(&mut key);
            }
            // Incoming callers can change hotness, or introduce a worker adapter,
            // without changing this module's own checked function bodies.
            for name in &defined {
                hot.contains(*name).hash(&mut key);
                workers.contains(name).hash(&mut key);
            }
            Unit {
                name: name.to_owned(),
                functions: defined.into_iter().map(str::to_owned).collect(),
                required: required.into_iter().map(str::to_owned).collect(),
                fingerprint: key.finish(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fingerprints(source: &str) -> BTreeMap<String, u64> {
        let mut p = crate::project::Project::from_source(source)
            .unwrap()
            .checked(false)
            .unwrap()
            .0;
        for f in &mut p.functions {
            f.source.module = if f.source.name == "main" {
                String::new()
            } else {
                f.source.name.rsplit('_').next().unwrap().into()
            };
        }
        plan(&p, &crate::cgen::hot_names(&p))
            .into_iter()
            .map(|u| (u.name, u.fingerprint))
            .collect()
    }
    #[test]
    fn helper_changes_invalidate_transitive_users_but_not_unrelated_modules() {
        let source="fn leaf()->int{return 1;}fn middle()->int{return leaf();}fn unrelated()->int{return 9;}fn main(){print middle();}";
        let a = fingerprints(source);
        let b = fingerprints(&source.replace("return 1", "return 2"));
        for name in ["leaf", "middle", ""] {
            assert_ne!(a[name], b[name], "{name}");
        }
        assert_eq!(a["unrelated"], b["unrelated"]);
    }
    #[test]
    fn only_reachable_type_layouts_invalidate_units() {
        let source="struct Used{n:int}struct Other{n:int}fn used(a:&Used){}fn unrelated()->int{return 9;}fn main(){}";
        let a = fingerprints(source);
        let b = fingerprints(&source.replace("Other{n:int}", "Other{n:int,label:text}"));
        assert_eq!(a, b);
        let c = fingerprints(&source.replace("Used{n:int}", "Used{n:u8}"));
        assert_ne!(a["used"], c["used"]);
        assert_eq!(a["unrelated"], c["unrelated"]);
    }
    #[test]
    fn incoming_hotness_and_worker_adapter_changes_are_part_of_identity() {
        let source = "fn worker(n:int)->int{return n+1;}fn main(){print worker(1);}";
        let a = fingerprints(source);
        let b =
            fingerprints(&source.replace("print worker(1);", "for i in 0..2{print worker(i);}"));
        let c = fingerprints(
            &source.replace("print worker(1);", "let h=spawn worker(1);print join(h);"),
        );
        assert_ne!(a["worker"], b["worker"]);
        assert_ne!(a["worker"], c["worker"]);
    }
    #[test]
    fn unemitted_source_coordinates_do_not_invalidate_machine_code() {
        let mut p = crate::project::Project::from_source("fn main(){print 1;}")
            .unwrap()
            .checked(false)
            .unwrap()
            .0;
        let a = plan(&p, &crate::cgen::hot_names(&p))[0].fingerprint;
        p.functions[0].source.line += 10;
        p.functions[0].source.source_offset += 100;
        assert_eq!(a, plan(&p, &crate::cgen::hot_names(&p))[0].fingerprint);
    }
}
