//! Checked field-access contracts. No stored references: all projections are
//! call-scoped. Private footprints refine conservative whole-owner summaries;
//! public signatures remain conservative unless they declare a view.
use crate::{syntax::*, types::TypeTable, Error, Result};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Access {
    pub fields: Vec<usize>,
    pub mutable: bool,
}
pub type Summaries = BTreeMap<String, Vec<Vec<Access>>>;

/// A C restrict parameter promises whole-object uniqueness for its entire
/// function scope. Refined field views can legally overlap, so they never
/// acquire that stronger promise. Public/foreign callers remain conservative.
pub(crate) fn unique_parameters(
    f: &Function,
    summaries: &Summaries,
) -> std::collections::BTreeSet<usize> {
    if f.public
        || f.external_symbol.is_some()
        || f.params
            .iter()
            .any(|p| p.borrow == Borrow::Shared || p.ty.scoped_view() || p.view.is_some())
    {
        return Default::default();
    }
    let Some(accesses) = summaries.get(&f.name) else {
        return Default::default();
    };
    if f.params
        .iter()
        .zip(accesses)
        .any(|(p, a)| p.borrow != Borrow::Owned && (a.len() != 1 || !a[0].fields.is_empty()))
    {
        return Default::default();
    }
    f.params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| (p.borrow == Borrow::Exclusive).then_some(i))
        .collect()
}

pub fn overlaps(a: &[usize], b: &[usize]) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
pub fn borrow_place(name: &str, line: usize) -> Expr {
    let mut parts = name.split('.');
    let mut e = Expr {
        line,
        kind: ExprKind::Var(parts.next().unwrap_or("").into()),
    };
    for part in parts {
        e = Expr {
            line,
            kind: ExprKind::Field(Box::new(e), part.into()),
        };
    }
    e
}
fn path(mut e: &Expr) -> Option<(&str, Vec<&str>)> {
    let mut suffix = Vec::new();
    loop {
        match &e.kind {
            ExprKind::Var(name) => {
                suffix.reverse();
                return Some((name, suffix));
            }
            ExprKind::Borrow(name, _) => {
                let mut parts = name.split('.');
                let root = parts.next()?;
                let mut fields: Vec<_> = parts.collect();
                fields.extend(suffix.into_iter().rev());
                return Some((root, fields));
            }
            ExprKind::Field(base, name) => {
                suffix.push(name.as_str());
                e = base;
            }
            _ => return None,
        }
    }
}
fn resolve(
    types: &TypeTable,
    mut ty: Type,
    names: &[impl AsRef<str>],
    line: usize,
) -> Result<Vec<usize>> {
    let mut fields = Vec::with_capacity(names.len());
    for name in names {
        let (i, t) = types.field(ty, name.as_ref(), line)?;
        fields.push(i);
        ty = t;
    }
    Ok(fields)
}
fn insert(out: &mut Vec<Access>, access: Access) {
    if out
        .iter()
        .any(|a| access.fields.starts_with(&a.fields) && (!access.mutable || a.mutable))
    {
        return;
    }
    out.retain(|a| !(a.fields.starts_with(&access.fields) && (!a.mutable || access.mutable)));
    out.push(access);
    out.sort();
}
struct Analyzer<'a> {
    f: &'a Function,
    funcs: &'a BTreeMap<String, &'a Function>,
    known: &'a Summaries,
    types: &'a TypeTable,
    out: Vec<Vec<Access>>,
}
impl Analyzer<'_> {
    fn access(&mut self, e: &Expr, mutable: bool, suffix: &[usize]) -> Result<()> {
        if let Some((name, names)) = path(e) {
            if let Some((i, p)) = self
                .f
                .params
                .iter()
                .enumerate()
                .find(|(_, p)| p.name == name && p.borrow != Borrow::Owned)
            {
                let mut fields = resolve(self.types, p.ty, &names, e.line)?;
                // Validate composed paths too: malformed recursive calls must
                // fail rather than grow an unbounded symbolic field chain.
                let mut ty = p.ty;
                for &index in fields.iter().chain(suffix) {
                    let Type::Named(id) = ty else {
                        return Err(Error::new(e.line, "incompatible field-view argument type"));
                    };
                    let DataKind::Record(members) = &self.types.definitions[id].kind else {
                        return Err(Error::new(e.line, "views require record fields"));
                    };
                    ty = members
                        .get(index)
                        .ok_or_else(|| Error::new(e.line, "incompatible view field"))?
                        .ty;
                }
                fields.extend_from_slice(suffix);
                insert(&mut self.out[i], Access { fields, mutable });
            }
        }
        Ok(())
    }
    fn expr(&mut self, e: &Expr) -> Result<()> {
        match &e.kind {
            ExprKind::MethodCall(_, name, _) => {
                return Err(Error::new(
                    e.line,
                    format!("unknown method {name}; declare it in impl"),
                ))
            }
            ExprKind::Var(_) | ExprKind::Field(..) => self.access(e, false, &[])?,
            ExprKind::Borrow(_, m) => self.access(e, *m, &[])?,
            ExprKind::Unary(_, a) | ExprKind::Repeat(a, _) | ExprKind::Try(a) => self.expr(a)?,
            ExprKind::Binary(_, a, b) => {
                self.expr(a)?;
                self.expr(b)?;
            }
            ExprKind::Record(_, fields) => {
                for (_, v) in fields {
                    self.expr(v)?;
                }
            }
            ExprKind::Array(args) => {
                for a in args {
                    self.expr(a)?;
                }
            }
            ExprKind::Call(name, args) | ExprKind::Spawn(name, args) => {
                if let Some(f) = self.funcs.get(name) {
                    for (i, a) in args.iter().enumerate() {
                        if let (Some(p), ExprKind::Borrow(_, _)) = (f.params.get(i), &a.kind) {
                            if p.borrow != Borrow::Owned {
                                for effect in &self.known[name][i] {
                                    self.access(a, effect.mutable, &effect.fields)?;
                                }
                                continue;
                            }
                        }
                        self.expr(a)?;
                    }
                } else {
                    for (i, a) in args.iter().enumerate() {
                        if name == "swap"
                            || (i == 0
                                && [
                                    "alloc",
                                    "free",
                                    "push",
                                    "insert",
                                    "remove",
                                    "pop",
                                    "replace",
                                    "set",
                                    "try_set",
                                    "try_write_u16_le",
                                    "try_write_u32_le",
                                    "try_write_u64_le",
                                    "try_write_u16_be",
                                    "try_write_u32_be",
                                    "try_write_u64_be",
                                    "wire_try_write_u16_be",
                                    "wire_try_write_u32_be",
                                    "wire_try_write_u64_be",
                                ]
                                .contains(&name.as_str()))
                            // 1.3: try_read_bytes(path, dst) writes its second argument.
                            || (i == 1 && name == "try_read_bytes")
                        {
                            self.access(a, true, &[])?;
                        } else {
                            self.expr(a)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn body(&mut self, body: &[Stmt]) -> Result<()> {
        // Statement alternatives can form a legal 96-level spine. Keep the
        // traversal on the heap; a debug frame includes all access-analysis
        // branches, including the larger paired-borrow variant.
        let mut pending: Vec<_> = body.iter().rev().collect();
        while let Some(s) = pending.pop() {
            match &s.kind {
                StmtKind::ForRange {
                    start, end, body, ..
                } => {
                    self.expr(start)?;
                    self.expr(end)?;
                    pending.extend(body.iter().rev());
                }
                StmtKind::ForEach {
                    owner,
                    mutable,
                    body,
                    ..
                } => {
                    self.access(owner, *mutable, &[])?;
                    pending.extend(body.iter().rev());
                }
                StmtKind::ElementPair {
                    owner,
                    indices,
                    body,
                    ..
                } => {
                    self.access(owner, true, &[])?;
                    for index in indices {
                        self.expr(index)?;
                    }
                    pending.extend(body.iter().rev());
                }
                StmtKind::Element {
                    owner,
                    index,
                    mutable,
                    body,
                    ..
                } => {
                    self.access(owner, *mutable, &[])?;
                    self.expr(index)?;
                    pending.extend(body.iter().rev());
                }
                StmtKind::Region {
                    mutable,
                    owner,
                    start,
                    end,
                    body,
                    ..
                } => {
                    // The source footprint includes the whole selected field.
                    // Local slice use cannot weaken a public borrowing contract.
                    self.access(owner, *mutable, &[])?;
                    self.expr(start)?;
                    self.expr(end)?;
                    pending.extend(body.iter().rev());
                }
                StmtKind::Assign(n, e) => {
                    self.access(&borrow_place(n, s.line), true, &[])?;
                    self.expr(e)?;
                }
                StmtKind::AssignField(p, e) => {
                    self.access(p, true, &[])?;
                    self.expr(e)?;
                }
                StmtKind::Let(_, _, e)
                | StmtKind::Expr(e)
                | StmtKind::Print(e)
                | StmtKind::Assert(e) => self.expr(e)?,
                StmtKind::Return(e) => {
                    if let Some(e) = e {
                        self.expr(e)?;
                    }
                }
                StmtKind::Break | StmtKind::Continue => {}
                StmtKind::If(e, a, b) => {
                    self.expr(e)?;
                    pending.extend(b.iter().rev());
                    pending.extend(a.iter().rev());
                }
                StmtKind::While(e, b) => {
                    self.expr(e)?;
                    pending.extend(b.iter().rev());
                }
                StmtKind::Match(e, arms) => {
                    self.expr(e)?;
                    for a in arms.iter().rev() {
                        pending.extend(a.body.iter().rev());
                    }
                }
            }
        }
        Ok(())
    }
}
fn actual(
    f: &Function,
    funcs: &BTreeMap<String, &Function>,
    known: &Summaries,
    types: &TypeTable,
) -> Result<Vec<Vec<Access>>> {
    let mut a = Analyzer {
        f,
        funcs,
        known,
        types,
        out: vec![Vec::new(); f.params.len()],
    };
    for e in f.requires.iter().chain(&f.ensures) {
        a.expr(e)?;
    }
    a.body(&f.body)?;
    Ok(a.out)
}
pub fn analyze(functions: &[Function], types: &TypeTable) -> Result<Summaries> {
    let funcs: BTreeMap<_, _> = functions.iter().map(|f| (f.name.clone(), f)).collect();
    let mut known = Summaries::new();
    let mut contracts = Summaries::new();
    for f in functions {
        let mut bounds = Vec::with_capacity(f.params.len());
        for p in &f.params {
            let mut fields = Vec::new();
            if let Some(view) = &p.view {
                if p.borrow == Borrow::Owned || !matches!(p.ty, Type::Named(_)) {
                    return Err(Error::new(
                        f.line,
                        "view contract requires a borrowed record",
                    ));
                }
                for v in view {
                    let indices = resolve(types, p.ty, &v.path, f.line)?;
                    if fields
                        .iter()
                        .any(|a: &Access| overlaps(&a.fields, &indices))
                    {
                        return Err(Error::new(
                            f.line,
                            "view contract has duplicate or overlapping field paths",
                        ));
                    }
                    if v.mutable && p.borrow != Borrow::Exclusive {
                        return Err(Error::new(f.line, "shared view cannot grant mutation"));
                    }
                    fields.push(Access {
                        fields: indices,
                        mutable: v.mutable,
                    });
                }
                fields.sort();
            } else if p.borrow != Borrow::Owned {
                fields.push(Access {
                    fields: Vec::new(),
                    mutable: p.borrow == Borrow::Exclusive,
                });
            }
            bounds.push(fields);
        }
        if f.params.iter().any(|p| p.view.is_some()) {
            contracts.insert(f.name.clone(), bounds.clone());
        }
        known.insert(f.name.clone(), bounds);
    }
    // No references can be stored in locals or aggregates. Thus all aliases of
    // parameters are explicit call arguments, which this fixed point composes.
    loop {
        let mut changed = false;
        for f in functions {
            if f.external_symbol.is_some() {
                continue;
            }
            let effects = actual(f, &funcs, &known, types)?;
            for (i, (p, mut effects)) in f.params.iter().zip(effects).enumerate() {
                if p.borrow == Borrow::Owned || p.view.is_some() || f.public {
                    continue;
                }
                let out = &mut known.get_mut(&f.name).unwrap()[i];
                if effects.is_empty() {
                    effects.push(Access {
                        fields: Vec::new(),
                        mutable: p.borrow == Borrow::Exclusive,
                    });
                }
                if effects.len() > 1024 {
                    return Err(Error::new(
                        f.line,
                        "field summary exceeds 1024 paths; use an explicit coarser view",
                    ));
                }
                if *out != effects {
                    *out = effects;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for f in functions {
        let effects = actual(f, &funcs, &known, types)?;
        for (i, p) in f.params.iter().enumerate() {
            if p.view.is_none() {
                continue;
            }
            for effect in &effects[i] {
                if !contracts[&f.name][i].iter().any(|bound| {
                    effect.fields.starts_with(&bound.fields) && (!effect.mutable || bound.mutable)
                }) {
                    return Err(Error::new(f.line,format!("view contract for {}.{} does not permit {} at field path {:?}; narrow the helper call or declare that field",f.name,p.name,if effect.mutable {"mutation"}else{"reading"},effect.fields)));
                }
            }
        }
    }
    Ok(known)
}
