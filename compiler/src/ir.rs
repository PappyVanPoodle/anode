//! Backend-independent, ownership-elaborated IR. Only the checker constructs
//! CheckedProgram; backends receive typed places and explicit cleanup operations.
use crate::syntax::{Borrow, Function, Type};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Place {
    pub local: usize,
    pub fields: Vec<usize>,
    pub ty: Type,
}
impl Place {
    pub fn local(local: usize, ty: Type) -> Self {
        Self {
            local,
            fields: Vec::new(),
            ty,
        }
    }
}
#[derive(Clone, Debug, Hash)]
pub(crate) struct Local {
    pub ty: Type,
    pub borrow: Borrow,
}
#[derive(Clone, Debug, Hash)]
pub(crate) enum Argument {
    Value(Place),
    Reference(Place),
}
#[derive(Clone, Debug, Hash)]
pub(crate) enum Rvalue {
    Int(i64),
    Float(u64),
    Bool(bool),
    Text(String),
    Zero,
    Repeat(Place, usize),
    Convert(Place),
    /// Integer/text owners use half-open start/end; nominal vectors and
    /// VectorView owners use start/count (the `end` place carries the count).
    /// The source owner remains lexically loaned; the descriptor never owns it.
    Region {
        owner: Place,
        start: Place,
        end: Place,
    },
    /// Scoped value pointer into a vector (integer index) or nominal map
    /// (typed key lookup). Owner type determines the operation. The backing
    /// owner remains loaned until lexical exit; this is never an allocation.
    Element {
        owner: Place,
        index: Place,
    },
    /// Reference to a payload whose variant was checked by the enclosing
    /// exhaustive switch. The source enum stays loaned for the arm's lifetime.
    Reference(Place),
    Copy(Place),
    Unary(String, Place),
    Binary(String, Place, Place),
    /// Equivalent to low <= value && value <= high for a checked int place.
    /// Backends use unsigned wrapping subtraction for this comparison only.
    IntRange {
        value: Place,
        low: i64,
        high: i64,
    },
    Call {
        name: String,
        args: Vec<Argument>,
        spawn: bool,
    },
    Builtin {
        name: String,
        args: Vec<Place>,
    },
    Record(Vec<Place>),
    Variant {
        index: usize,
        payload: Option<Place>,
    },
}
#[derive(Clone, Debug, Hash)]
pub(crate) enum Operation {
    /// Bounds and distinctness must be checked before either alias is created.
    ElementCheckPair {
        owner: Place,
        left: Place,
        right: Place,
    },
    /// Return a nonowning stack projection to its SoA columns before loan end.
    ElementWriteback {
        alias: Place,
        owner: Place,
        index: Place,
    },
    /// Bitwise exchange of two checked mutable, disjoint places; no ownership
    /// transfer, allocation, destructor or clone takes place.
    Swap(Place, Place),
    Init(Place, Rvalue),
    /// Transfers ownership and empties the source so a later drop is harmless.
    Move {
        destination: Place,
        source: Place,
        initialize: bool,
    },
    Assign(Place, Place),
    Drop(Place),
    /// Auditable call-lifetime loans; no generated runtime instructions.
    Loan {
        place: Place,
        kind: Borrow,
        begin: bool,
    },
    Assert {
        value: Place,
        line: usize,
        message: String,
    },
    If {
        condition: Place,
        yes: Vec<Operation>,
        no: Vec<Operation>,
    },
    Loop {
        id: usize,
        condition_code: Vec<Operation>,
        condition: Place,
        body: Vec<Operation>,
    },
    Return(Option<Place>),
    /// Targets the enclosing source loop, even inside a match/switch.
    Break(usize),
    Continue(usize),
    Switch {
        value: Place,
        arms: Vec<Vec<Operation>>,
    },
}
#[derive(Clone, Debug, Hash)]
pub(crate) struct CheckedFunction {
    pub source: Function,
    pub locals: Vec<Local>,
    pub body: Vec<Operation>,
}
#[derive(Clone, Debug, Hash)]
pub struct CheckedProgram {
    pub(crate) functions: Vec<CheckedFunction>,
    pub(crate) types: crate::types::TypeTable,
    pub(crate) views: crate::views::Summaries,
    pub(crate) module_identities: std::collections::BTreeMap<String, String>,
    /// Explicit subprocess authority; legacy checker entrypoints deny it.
    pub(crate) allow_process: bool,
}
impl CheckedProgram {
    pub fn dump(&self) -> String {
        format!("{self:#?}\n")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ColumnLoop {
    pub owner: Place,
    pub alias: usize,
    /// Scalar fallback retains every original bounds and arithmetic check.
    pub simd: Option<ColumnSimd>,
}
#[derive(Clone, Debug)]
pub(crate) enum ColumnSimd {
    Constant {
        column: usize,
        float: crate::numeric::Float,
        operator: String,
        constant: u64,
    },
    Columns {
        updates: Vec<ColumnUpdate>,
        read_columns: Vec<usize>,
        write_columns: Vec<usize>,
    },
    Reduction(ColumnReduction),
}
#[derive(Clone, Debug)]
pub(crate) struct ColumnUpdate {
    pub column: usize,
    pub float: crate::numeric::Float,
    pub value: ColumnExpr,
}
#[derive(Clone, Debug)]
pub(crate) enum ColumnExpr {
    Column(usize),
    Invariant(Place),
    Constant(u64),
    Binary {
        operator: String,
        left: Box<ColumnExpr>,
        right: Box<ColumnExpr>,
    },
}
#[derive(Clone, Debug)]
pub(crate) struct ColumnReduction {
    pub accumulator: Place,
    pub element: Place,
    pub operator: String,
    pub preserve_order: bool,
}
/// Conservative loop preheaders. Unknown calls, container mutations, owners
/// initialized in the loop and whole-record access all retain the checked path.
pub(crate) fn column_loops(
    f: &CheckedFunction,
    types: &crate::types::TypeTable,
) -> std::collections::BTreeMap<usize, ColumnLoop> {
    use crate::syntax::{ExprKind, Stmt, StmtKind, Type};
    use std::collections::{BTreeMap, BTreeSet};
    fn source_loops<'a>(body: &'a [Stmt], out: &mut Vec<&'a Stmt>) -> bool {
        let mut one_to_one = true;
        let mut pending: Vec<_> = body.iter().rev().collect();
        while let Some(s) = pending.pop() {
            match &s.kind {
                StmtKind::ForEach { body, .. } => {
                    out.push(s);
                    pending.extend(body.iter().rev());
                }
                StmtKind::ForRange { body, .. } | StmtKind::While(_, body) => {
                    out.push(s);
                    pending.extend(body.iter().rev());
                }
                StmtKind::If(_, a, b) => {
                    pending.extend(b.iter().rev());
                    pending.extend(a.iter().rev());
                }
                StmtKind::Element { body, .. }
                | StmtKind::ElementPair { body, .. }
                | StmtKind::Region { body, .. } => pending.extend(body.iter().rev()),
                StmtKind::Match(_, arms) => {
                    for arm in arms.iter().rev() {
                        // Checking expands wildcard arms for each remaining
                        // variant. Their loops do not have one-to-one source
                        // IDs, so retain the entire function's checked path.
                        one_to_one &= arm.path != "_";
                        pending.extend(arm.body.iter().rev());
                    }
                }
                _ => {}
            }
        }
        one_to_one
    }
    fn safe(body: &[Operation], owner: &Place) -> bool {
        body.iter().all(|op| match op {
            Operation::Init(p, value) => {
                p.local != owner.local
                    && matches!(
                        value,
                        Rvalue::Int(_)
                            | Rvalue::Float(_)
                            | Rvalue::Bool(_)
                            | Rvalue::Copy(_)
                            | Rvalue::Unary(..)
                            | Rvalue::Binary(..)
                            | Rvalue::Convert(_)
                            | Rvalue::IntRange { .. }
                            | Rvalue::Element { .. }
                    )
            }
            Operation::Assign(p, _) => p.local != owner.local,
            Operation::Loan { .. }
            | Operation::ElementWriteback { .. }
            | Operation::Assert { .. } => true,
            Operation::If { yes, no, .. } => safe(yes, owner) && safe(no, owner),
            _ => false,
        })
    }
    fn simd(
        source: &Stmt,
        plan: &ColumnLoop,
        body: &[Operation],
        types: &crate::types::TypeTable,
    ) -> Option<ColumnSimd> {
        let StmtKind::ForEach {
            name, body: source, ..
        } = &source.kind
        else {
            return None;
        };
        if source.is_empty() || !source.iter().all(|s| matches!(&s.kind,
            StmtKind::AssignField(target,_) if matches!(&target.kind,ExprKind::Field(base,_) if matches!(&base.kind,ExprKind::Var(n) if n==name)))) {
            return None;
        }
        let record = types.soa_element(plan.owner.ty)?;
        let initialized: BTreeSet<_> = body
            .iter()
            .filter_map(|op| match op {
                Operation::Init(p, _) => Some(p.local),
                _ => None,
            })
            .collect();
        let changed: BTreeSet<_> = body
            .iter()
            .filter_map(|op| match op {
                Operation::Assign(p, _) => Some(p.local),
                _ => None,
            })
            .collect();
        let mut definitions = BTreeMap::new();
        fn constant(
            p: &Place,
            defs: &BTreeMap<usize, (&Place, &Rvalue)>,
            depth: usize,
        ) -> Option<f64> {
            if depth > 128 || !p.fields.is_empty() {
                return None;
            }
            let (_, v) = defs.get(&p.local)?;
            match v {
                Rvalue::Float(bits) => Some(f64::from_bits(*bits)),
                Rvalue::Int(n) => Some(*n as f64),
                Rvalue::Copy(v) => constant(v, defs, depth + 1),
                // Integer-to-integer and float-to-integer conversions may
                // trap; never drop them by folding the enclosing float cast.
                Rvalue::Convert(v) if matches!(p.ty, Type::Float(_)) => {
                    constant(v, defs, depth + 1).map(|n| {
                        if p.ty == Type::Float(crate::numeric::Float::F32) {
                            n as f32 as f64
                        } else {
                            n
                        }
                    })
                }
                _ => None,
            }
        }
        struct Expressions<'a> {
            alias: usize,
            float: crate::numeric::Float,
            defs: &'a BTreeMap<usize, (&'a Place, &'a Rvalue)>,
            initialized: &'a BTreeSet<usize>,
            changed: &'a BTreeSet<usize>,
        }
        impl Expressions<'_> {
            fn value(&self, p: &Place, depth: usize) -> Option<(ColumnExpr, bool)> {
                if depth > 128 || p.ty != Type::Float(self.float) {
                    return None;
                }
                if p.local == self.alias {
                    return (p.fields.len() == 1).then(|| (ColumnExpr::Column(p.fields[0]), true));
                }
                if let Some(n) = constant(p, self.defs, 0) {
                    return Some((
                        ColumnExpr::Constant(if self.float == crate::numeric::Float::F32 {
                            (n as f32).to_bits() as u64
                        } else {
                            n.to_bits()
                        }),
                        false,
                    ));
                }
                if !self.initialized.contains(&p.local) && !self.changed.contains(&p.local) {
                    return Some((ColumnExpr::Invariant(p.clone()), false));
                }
                let (_, v) = self.defs.get(&p.local)?;
                match v {
                    Rvalue::Copy(v) => self.value(v, depth + 1),
                    Rvalue::Binary(operator, a, b)
                        if ["+", "-", "*", "/"].contains(&operator.as_str()) =>
                    {
                        let (left, l) = self.value(a, depth + 1)?;
                        let (right, r) = self.value(b, depth + 1)?;
                        // Linear/affine expressions only; division by a
                        // varying column or products of columns stay scalar.
                        if (operator == "*" && l && r) || (operator == "/" && r) {
                            return None;
                        }
                        Some((
                            ColumnExpr::Binary {
                                operator: operator.clone(),
                                left: Box::new(left),
                                right: Box::new(right),
                            },
                            l || r,
                        ))
                    }
                    _ => None,
                }
            }
        }
        let mut updates = Vec::new();
        for op in body {
            match op {
                Operation::Init(p, v) => {
                    definitions.insert(p.local, (p, v));
                }
                Operation::Assign(p, v) if p.local == plan.alias => {
                    let [column] = p.fields.as_slice() else {
                        return None;
                    };
                    let Type::Float(float) = p.ty else {
                        return None;
                    };
                    let (value, _) = Expressions {
                        alias: plan.alias,
                        float,
                        defs: &definitions,
                        initialized: &initialized,
                        changed: &changed,
                    }
                    .value(v, 0)?;
                    updates.push(ColumnUpdate {
                        column: *column,
                        float,
                        value,
                    });
                }
                _ => {}
            }
        }
        if updates.len() != source.len() || updates.iter().any(|u| u.float != updates[0].float) {
            return None;
        }
        let mut reads = BTreeSet::new();
        let mut pending: Vec<_> = updates.iter().map(|u| &u.value).collect();
        while let Some(e) = pending.pop() {
            match e {
                ColumnExpr::Column(c) => {
                    reads.insert(*c);
                }
                ColumnExpr::Binary { left, right, .. } => {
                    pending.extend([left.as_ref(), right.as_ref()])
                }
                _ => {}
            }
        }
        // Validate all referenced columns against the exact record layout.
        let Type::Named(id) = record else { return None };
        let crate::syntax::DataKind::Record(fields) = &types.definitions[id].kind else {
            return None;
        };
        if reads.iter().any(|&c| {
            fields
                .get(c)
                .is_none_or(|f| f.ty != Type::Float(updates[0].float))
        }) {
            return None;
        }
        if let [ColumnUpdate {
            column,
            float,
            value:
                ColumnExpr::Binary {
                    operator,
                    left,
                    right,
                },
        }] = updates.as_slice()
        {
            if matches!(left.as_ref(),ColumnExpr::Column(c) if c==column) {
                if let ColumnExpr::Constant(constant) = right.as_ref() {
                    return Some(ColumnSimd::Constant {
                        column: *column,
                        float: *float,
                        operator: operator.clone(),
                        constant: *constant,
                    });
                }
            }
        }
        let writes = updates.iter().map(|u| u.column).collect::<BTreeSet<_>>();
        Some(ColumnSimd::Columns {
            updates,
            read_columns: reads.into_iter().collect(),
            write_columns: writes.into_iter().collect(),
        })
    }
    fn reduction(
        source: &Stmt,
        body: &[Operation],
        types: &crate::types::TypeTable,
    ) -> Option<ColumnLoop> {
        let (statements, each, indexed) = match &source.kind {
            StmtKind::ForEach { name, body, .. } => (body, Some(name.as_str()), None),
            StmtKind::ForRange {
                name,
                start,
                end,
                body,
            } => {
                if !matches!(start.kind, ExprKind::Int(0)) {
                    return None;
                }
                let ExprKind::Call(len, args) = &end.kind else {
                    return None;
                };
                if len != "len" || args.len() != 1 {
                    return None;
                }
                let ExprKind::Var(owner) = &args[0].kind else {
                    return None;
                };
                (body, None, Some((name.as_str(), owner.as_str())))
            }
            _ => return None,
        };
        let [Stmt {
            kind: StmtKind::Assign(accumulator, rhs),
            ..
        }] = statements.as_slice()
        else {
            return None;
        };
        let ExprKind::Binary(operator, left, right) = &rhs.kind else {
            return None;
        };
        if !["+", "-", "*"].contains(&operator.as_str())
            || !matches!(&left.kind,ExprKind::Var(n) if n==accumulator)
        {
            return None;
        }
        if let Some(each) = each {
            if !matches!(&right.kind,ExprKind::Var(n) if n==each) {
                return None;
            }
        } else {
            let (index, owner) = indexed?;
            let ExprKind::Call(at, args) = &right.kind else {
                return None;
            };
            if at != "at"
                || args.len() != 2
                || !matches!(&args[0].kind,ExprKind::Var(n) if n==owner)
                || !matches!(&args[1].kind,ExprKind::Var(n) if n==index)
            {
                return None;
            }
        }
        let defs: BTreeMap<_, _> = body
            .iter()
            .filter_map(|op| match op {
                Operation::Init(p, v) => Some((p.local, (p, v))),
                _ => None,
            })
            .collect();
        let copied = |p: &Place| -> Option<Place> {
            match defs.get(&p.local)?.1 {
                Rvalue::Copy(p) => Some(p.clone()),
                _ => None,
            }
        };
        for op in body {
            let Operation::Assign(acc, value) = op else {
                continue;
            };
            let Some((_, Rvalue::Binary(op, a, b))) = defs.get(&value.local) else {
                continue;
            };
            if op != operator || copied(a).as_ref() != Some(acc) {
                continue;
            }
            let element = copied(b).unwrap_or_else(|| b.clone());
            let Some((load, rvalue)) = defs.get(&element.local) else {
                continue;
            };
            let owner = match rvalue {
                Rvalue::Element { owner, .. } if each.is_some() => owner,
                Rvalue::Builtin { name, args }
                    if indexed.is_some()
                        && ["vec_at", "at", "fixed_at"].contains(&name.as_str()) =>
                {
                    &args[0]
                }
                _ => continue,
            };
            let scalar = types.vector_element(owner.ty).or_else(|| match owner.ty {
                Type::Ints => Some(Type::Int),
                Type::Slice(n) => Some(n.ty()),
                _ => None,
            })?;
            if types.soa_element(owner.ty).is_some()
                || scalar != acc.ty
                || !(scalar.integer().is_some() || matches!(scalar, Type::Float(_)))
            {
                return None;
            }
            // Source shape fixes every iteration to one load and one ordered
            // scalar update. Checked arithmetic and IEEE ordering are retained.
            return Some(ColumnLoop {
                owner: owner.clone(),
                alias: load.local,
                simd: Some(ColumnSimd::Reduction(ColumnReduction {
                    accumulator: acc.clone(),
                    element: (*load).clone(),
                    operator: operator.clone(),
                    preserve_order: true,
                })),
            });
        }
        None
    }
    fn walk(
        ops: &[Operation],
        source: &[&Stmt],
        direct: &std::collections::BTreeSet<usize>,
        types: &crate::types::TypeTable,
        out: &mut BTreeMap<usize, ColumnLoop>,
    ) {
        let mut pending: Vec<_> = ops.iter().rev().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Loop {
                    id,
                    body,
                    condition_code,
                    ..
                } => {
                    if let Some(plan) = source.get(*id).and_then(|s| reduction(s, body, types)) {
                        out.insert(*id, plan);
                    }
                    if let Some((alias, owner)) = body.iter().find_map(|op| match op {
                        Operation::Init(alias, Rvalue::Element { owner, .. })
                            if direct.contains(&alias.local)
                                && types.soa_element(owner.ty).is_some() =>
                        {
                            Some((alias, owner))
                        }
                        _ => None,
                    }) {
                        if source
                            .get(*id)
                            .is_some_and(|s| matches!(s.kind, StmtKind::ForEach { .. }))
                            && safe(body, owner)
                            && safe(condition_code, owner)
                        {
                            let mut plan = ColumnLoop {
                                owner: owner.clone(),
                                alias: alias.local,
                                simd: None,
                            };
                            plan.simd = simd(source[*id], &plan, body, types);
                            out.insert(*id, plan);
                        }
                    }
                    pending.extend(body.iter().rev());
                }
                Operation::If { yes, no, .. } => {
                    pending.extend(no.iter().rev());
                    pending.extend(yes.iter().rev());
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
    let mut source = Vec::new();
    if !source_loops(&f.source.body, &mut source) {
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    walk(
        &f.body,
        &source,
        &direct_soa_aliases(f, types),
        types,
        &mut out,
    );
    out
}

/// A record loan can address columns directly when every actual use selects a
/// field. Whole-record calls, assignments, clones and exchanges retain the
/// stack projection path. This is a backend optimization over checked loans;
/// it never changes which source accesses are permitted.
pub(crate) fn direct_soa_aliases(
    f: &CheckedFunction,
    types: &crate::types::TypeTable,
) -> std::collections::BTreeSet<usize> {
    use std::collections::BTreeSet;
    fn walk(
        ops: &[Operation],
        candidates: &mut BTreeSet<usize>,
        whole: &mut BTreeSet<usize>,
        types: &crate::types::TypeTable,
    ) {
        let use_place = |p: &Place, whole: &mut BTreeSet<usize>| {
            if p.fields.is_empty() {
                whole.insert(p.local);
            }
        };
        for op in ops {
            match op {
                Operation::Init(dst, value) => {
                    if matches!(value,Rvalue::Element{owner,..} if types.soa_element(owner.ty).is_some())
                    {
                        candidates.insert(dst.local);
                    } else {
                        use_place(dst, whole);
                    }
                    match value {
                        Rvalue::Element { owner, index } => {
                            use_place(owner, whole);
                            use_place(index, whole);
                        }
                        Rvalue::Region { owner, start, end } => {
                            use_place(owner, whole);
                            use_place(start, whole);
                            use_place(end, whole);
                        }
                        Rvalue::Repeat(p, _)
                        | Rvalue::Convert(p)
                        | Rvalue::Reference(p)
                        | Rvalue::Copy(p)
                        | Rvalue::Unary(_, p) => use_place(p, whole),
                        Rvalue::Binary(_, a, b) => {
                            use_place(a, whole);
                            use_place(b, whole);
                        }
                        Rvalue::IntRange { value, .. } => use_place(value, whole),
                        Rvalue::Call { args, .. } => {
                            for arg in args {
                                let (Argument::Value(p) | Argument::Reference(p)) = arg;
                                use_place(p, whole);
                            }
                        }
                        Rvalue::Builtin { args, .. } | Rvalue::Record(args) => {
                            for p in args {
                                use_place(p, whole);
                            }
                        }
                        Rvalue::Variant {
                            payload: Some(p), ..
                        } => use_place(p, whole),
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
                    use_place(destination, whole);
                    use_place(source, whole);
                }
                Operation::Drop(p)
                | Operation::Loan { place: p, .. }
                | Operation::Assert { value: p, .. }
                | Operation::Return(Some(p)) => use_place(p, whole),
                Operation::ElementCheckPair { owner, left, right } => {
                    use_place(owner, whole);
                    use_place(left, whole);
                    use_place(right, whole);
                }
                Operation::ElementWriteback { owner, index, .. } => {
                    use_place(owner, whole);
                    use_place(index, whole);
                }
                Operation::If { condition, yes, no } => {
                    use_place(condition, whole);
                    walk(yes, candidates, whole, types);
                    walk(no, candidates, whole, types);
                }
                Operation::Loop {
                    condition_code,
                    condition,
                    body,
                    ..
                } => {
                    use_place(condition, whole);
                    walk(condition_code, candidates, whole, types);
                    walk(body, candidates, whole, types);
                }
                Operation::Switch { value, arms } => {
                    use_place(value, whole);
                    for arm in arms {
                        walk(arm, candidates, whole, types);
                    }
                }
                _ => {}
            }
        }
    }
    let mut candidates = BTreeSet::new();
    let mut whole = BTreeSet::new();
    walk(&f.body, &mut candidates, &mut whole, types);
    candidates.retain(|id| !whole.contains(id));
    candidates
}
