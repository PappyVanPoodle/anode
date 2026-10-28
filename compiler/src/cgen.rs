//! C backend for checked IR. No type inference or ownership decisions live here.
use crate::{ir::*, syntax::*};
use std::collections::{BTreeMap, BTreeSet};

pub struct Generated {
    pub preamble: String,
    pub functions: Vec<(String, String)>,
    pub entry: String,
}
impl Generated {
    pub fn flatten(&self) -> String {
        let mut out = self.preamble.clone();
        for (_, code) in &self.functions {
            out.push_str(code);
        }
        out.push_str(&self.entry);
        out
    }
}
pub fn c_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.bytes().map(|b| format!("\\{b:03o}")).collect::<String>()
    )
}
fn text_literal(s: &str, large: &BTreeMap<String, usize>) -> String {
    if s.len() <= 1024 {
        return format!("zn_text_literal({},{})", c_string(s), s.len());
    }
    format!("zn_text_literal(zn_v028_text_{},{})", large[s], s.len())
}
fn params(f: &Function, views: &crate::views::Summaries) -> String {
    if f.params.is_empty() {
        "void".into()
    } else {
        param_list(f, views).join(", ")
    }
}
/// One C declaration per parameter, as `params` joins them.
fn param_list(f: &Function, views: &crate::views::Summaries) -> Vec<String> {
    let unique = crate::views::unique_parameters(f, views);
    f.params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            format!(
                "{}{} p{i}",
                p.c_type(),
                if unique.contains(&i) {
                    " __restrict"
                } else {
                    ""
                }
            )
        })
        .collect()
}
struct Writer<'a> {
    f: &'a CheckedFunction,
    hot: bool,
    types: &'a crate::types::TypeTable,
    externals: &'a BTreeMap<String, &'a Function>,
    direct_soa_aliases: BTreeSet<usize>,
    column_loops: BTreeMap<usize, ColumnLoop>,
    large_literals: &'a BTreeMap<String, usize>,
    /// Whole-function integer facts only (contract S1): no parameter bounds,
    /// no overrides, no native guard facts.
    ranges: crate::int_ranges::IntRanges,
    /// C-side flow facts per operand read site (1.0 B, contract L2).
    flow: BTreeMap<usize, Bounds>,
    /// Innermost loops emitted twice: a guarded fast copy and the original.
    versions: BTreeMap<usize, VersionPlan>,
    /// Set while the fast copy of that loop is being emitted.
    fast: std::cell::Cell<Option<usize>>,
    /// Pure loop-condition temporaries substituted into a `while` header.
    substitutions: std::cell::RefCell<BTreeMap<usize, String>>,
    /// Number of `Place` occurrences of each local in the function.
    uses: Vec<usize>,
    /// 1.1 B (contract M5): pinned view parameters (local -> element type)
    /// tested once at entry; the function body is emitted twice.
    view_versions: BTreeMap<usize, Type>,
    /// Set while the validated (fast) body is being emitted.
    view_fast: std::cell::Cell<bool>,
    /// Flow facts of the validated body (a superset of `flow`'s).
    fast_flow: BTreeMap<usize, Bounds>,
    /// View indices proven `0 <= i < len(view)` in the validated body.
    fast_proven: BTreeSet<usize>,
    /// Suffix that keeps the validated body's loop labels distinct.
    label_suffix: std::cell::Cell<&'static str>,
    /// 1.2 B (contract N8): parameters kept by `zn_v120_fast_<name>`, the
    /// validated body as its own function, when the validated body calls
    /// the function itself with its validated views (`fast_self_call`).
    self_fast: Option<Vec<usize>>,
    /// 1.2 B (contract N8): qualifying local view definitions (`local_view_plans`).
    local_views: BTreeMap<usize, LocalView>,
    /// The local view (local, proven minimum length, element) whose cached
    /// copy of the rest of its block is being emitted.
    active_local: std::cell::Cell<Option<(usize, i64, Type)>>,
    /// Loops inside the versioned rest of a block (their cached copies get
    /// the `_v120` label suffix; outer loops keep their labels).
    local_loops: std::cell::RefCell<BTreeSet<usize>>,
    /// Set while either copy of a versioned rest is emitted (no nesting).
    in_local: std::cell::Cell<bool>,
    /// Set while the M5 checked fallback body is emitted: it stays exactly
    /// the 1.1.0 lowering, so no local view is versioned there.
    fallback: std::cell::Cell<bool>,
}
/// Pending emission work; nested control flow stays on the heap.
enum Emission<'a> {
    Operations(&'a [Operation]),
    Text(String),
    /// Emit this loop operation without versioning it again.
    Loop(&'a Operation),
    /// Enter (`Some`) or leave (`None`) a versioned fast copy.
    Fast(Option<usize>),
    /// 1.2 B (contract N8): enter (`Some`) or leave (`None`) the cached copy
    /// of a local view's rest of block.
    LocalView(Option<(usize, i64, Type)>),
    /// End of both copies of a versioned rest of block.
    LocalDone,
}
/// Every `Place` an operation mentions directly (nested blocks excluded).
fn operation_places(op: &Operation) -> Vec<&Place> {
    match op {
        Operation::Init(p, value) => {
            let mut out = vec![p];
            match value {
                Rvalue::Repeat(p, _)
                | Rvalue::Convert(p)
                | Rvalue::Copy(p)
                | Rvalue::Unary(_, p)
                | Rvalue::Reference(p)
                | Rvalue::IntRange { value: p, .. } => out.push(p),
                Rvalue::Binary(_, a, b) => out.extend([a, b]),
                Rvalue::Region { owner, start, end } => out.extend([owner, start, end]),
                Rvalue::Element { owner, index } => out.extend([owner, index]),
                Rvalue::Call { args, .. } => out.extend(args.iter().map(|a| match a {
                    Argument::Value(p) | Argument::Reference(p) => p,
                })),
                Rvalue::Builtin { args, .. } | Rvalue::Record(args) => out.extend(args),
                Rvalue::Variant {
                    payload: Some(p), ..
                } => out.push(p),
                _ => {}
            }
            out
        }
        Operation::ElementCheckPair { owner, left, right } => vec![owner, left, right],
        Operation::ElementWriteback {
            alias,
            owner,
            index,
        } => vec![alias, owner, index],
        Operation::Swap(a, b) | Operation::Assign(a, b) => vec![a, b],
        Operation::Move {
            destination,
            source,
            ..
        } => vec![destination, source],
        Operation::Drop(p)
        | Operation::Loan { place: p, .. }
        | Operation::Assert { value: p, .. }
        | Operation::Return(Some(p))
        | Operation::If { condition: p, .. }
        | Operation::Loop { condition: p, .. }
        | Operation::Switch { value: p, .. } => vec![p],
        _ => Vec::new(),
    }
}
fn count_uses(ops: &[Operation], uses: &mut [usize]) {
    for op in ops {
        for p in operation_places(op) {
            uses[p.local] += 1;
        }
        match op {
            Operation::If { yes, no, .. } => {
                count_uses(yes, uses);
                count_uses(no, uses);
            }
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                count_uses(condition_code, uses);
                count_uses(body, uses);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    count_uses(arm, uses);
                }
            }
            _ => {}
        }
    }
}
/// Whether any `break`/`continue` (at any depth) targets loop `id`.
fn loop_targets(ops: &[Operation], id: usize) -> (bool, bool) {
    let (mut breaks, mut continues) = (false, false);
    let mut pending: Vec<&Operation> = ops.iter().collect();
    while let Some(op) = pending.pop() {
        match op {
            Operation::Break(target) => breaks |= *target == id,
            Operation::Continue(target) => continues |= *target == id,
            Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
            Operation::Loop {
                condition_code,
                body,
                ..
            } => pending.extend(condition_code.iter().chain(body)),
            Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
            _ => {}
        }
    }
    (breaks, continues)
}
/// `n` as a C `int64_t` constant expression (no out-of-range literal).
fn c_int(n: i64) -> String {
    if n == i64::MIN {
        "(-INT64_MAX-1)".into()
    } else {
        format!("INT64_C({n})")
    }
}
/// `(sign-weighted invariant locals, low, high)` of one hoisted guard.
type VersionCheck = (Vec<(usize, i64)>, i64, i64);
/// 1.0 B loop versioning. For an innermost loop with straight-line condition
/// and body, a checked `int` `+`/`-` whose operands are affine in locals the
/// loop never writes, `t = S + x` with `S = sum(+-v_k)` fixed for the whole
/// loop and `x` in a proven interval, cannot overflow when `S` itself is
/// representable and `S + [low, high]` fits i64. Those guards are evaluated
/// once before the loop; only when all hold does the fast copy (with those
/// operations unchecked) run, otherwise the original checked loop runs.
struct VersionPlan {
    /// Addresses of the `Rvalue`s emitted unchecked in the fast copy.
    ops: BTreeSet<usize>,
    /// `(terms, low, high)`: require `sum(sign*local)` exact and
    /// `sum + [low, high]` within i64.
    checks: Vec<VersionCheck>,
}
#[derive(Clone)]
struct Affine {
    terms: BTreeMap<usize, i64>,
    low: i128,
    high: i128,
}
impl Affine {
    fn constant(b: Bounds) -> Self {
        Affine {
            terms: BTreeMap::new(),
            low: b.low as i128,
            high: b.high as i128,
        }
    }
    /// `self + sign*other`, or `None` when a coefficient leaves {-1,0,+1}.
    fn combine(&self, other: &Affine, sign: i64) -> Option<Affine> {
        let mut terms = self.terms.clone();
        for (local, coefficient) in &other.terms {
            let c = terms.get(local).copied().unwrap_or(0) + sign * coefficient;
            if c == 0 {
                terms.remove(local);
            } else if c.abs() == 1 {
                terms.insert(*local, c);
            } else {
                return None;
            }
        }
        let (low, high) = if sign > 0 {
            (self.low + other.low, self.high + other.high)
        } else {
            (self.low - other.high, self.high - other.low)
        };
        Some(Affine { terms, low, high })
    }
}
const VERSION_MAX_OPERATIONS: usize = 256;
const VERSION_MAX_CHECKS: usize = 16;
fn version_plans(
    f: &CheckedFunction,
    tracked: &[bool],
    operands: &BTreeMap<usize, Bounds>,
    whole: &crate::int_ranges::IntRanges,
    columns: &BTreeMap<usize, ColumnLoop>,
) -> BTreeMap<usize, VersionPlan> {
    fn straight(ops: &[Operation]) -> bool {
        ops.iter().all(|op| {
            matches!(
                op,
                Operation::Init(..)
                    | Operation::Assign(..)
                    | Operation::Assert { .. }
                    | Operation::Loan { .. }
                    | Operation::ElementWriteback { .. }
                    | Operation::ElementCheckPair { .. }
            )
        })
    }
    let bound = |p: &Place| {
        operands
            .get(&operand_key(p))
            .copied()
            .unwrap_or_else(|| whole.get(p, &BTreeMap::new()))
    };
    let mut plans = BTreeMap::new();
    let mut pending: Vec<&Operation> = f.body.iter().collect();
    while let Some(op) = pending.pop() {
        let (id, condition_code, body) = match op {
            Operation::If { yes, no, .. } => {
                pending.extend(yes.iter().chain(no));
                continue;
            }
            Operation::Switch { arms, .. } => {
                pending.extend(arms.iter().flatten());
                continue;
            }
            Operation::Loop {
                id,
                condition_code,
                body,
                ..
            } => {
                pending.extend(condition_code.iter().chain(body));
                (*id, condition_code, body)
            }
            _ => continue,
        };
        if columns.contains_key(&id)
            || !straight(condition_code)
            || !straight(body)
            || condition_code.len() + body.len() > VERSION_MAX_OPERATIONS
        {
            continue;
        }
        let mut written = BTreeSet::new();
        written_locals(condition_code, &mut written);
        written_locals(body, &mut written);
        let invariant = |p: &Place| {
            p.fields.is_empty()
                && p.ty == Type::Int
                && tracked[p.local]
                && !written.contains(&p.local)
        };
        let mut forms: BTreeMap<usize, Option<Affine>> = BTreeMap::new();
        let read = |p: &Place, forms: &BTreeMap<usize, Option<Affine>>| {
            if invariant(p) {
                return Affine {
                    terms: BTreeMap::from([(p.local, 1)]),
                    low: 0,
                    high: 0,
                };
            }
            if p.fields.is_empty() && tracked[p.local] {
                if let Some(Some(form)) = forms.get(&p.local) {
                    return form.clone();
                }
            }
            Affine::constant(bound(p))
        };
        let mut plan = VersionPlan {
            ops: BTreeSet::new(),
            checks: Vec::new(),
        };
        for op in condition_code.iter().chain(body) {
            let form = match op {
                Operation::Init(p, value) if p.fields.is_empty() && p.ty == Type::Int => {
                    match value {
                        Rvalue::Copy(source) => Some(read(source, &forms)),
                        Rvalue::Int(n) => Some(Affine::constant(Bounds::exact(*n))),
                        Rvalue::Binary(operator, a, b)
                            if a.ty == Type::Int
                                && b.ty == Type::Int
                                && (operator == "+" || operator == "-") =>
                        {
                            let sign = if operator == "+" { 1 } else { -1 };
                            let form =
                                read(a, &forms).combine(&read(b, &forms), sign).filter(|f| {
                                    f.low >= i64::MIN as i128 && f.high <= i64::MAX as i128
                                });
                            let proven = bound(a).arithmetic(operator, bound(b)).1;
                            if let Some(form) = &form {
                                let check = (
                                    form.terms.iter().map(|(k, v)| (*k, *v)).collect(),
                                    form.low as i64,
                                    form.high as i64,
                                );
                                let known = plan.checks.contains(&check);
                                if !proven
                                    && !form.terms.is_empty()
                                    && (known || plan.checks.len() < VERSION_MAX_CHECKS)
                                {
                                    plan.ops.insert(value as *const Rvalue as usize);
                                    if !known {
                                        plan.checks.push(check);
                                    }
                                }
                            }
                            form
                        }
                        _ => None,
                    }
                }
                Operation::Assign(p, source) if p.fields.is_empty() && p.ty == Type::Int => {
                    Some(read(source, &forms))
                }
                _ => None,
            };
            for local in leaf_writes(op) {
                forms.insert(local, None);
            }
            if let Some(form) = form {
                if let Operation::Init(p, _) | Operation::Assign(p, _) = op {
                    // Only tracked locals: every write to them is explicit.
                    if tracked[p.local] {
                        forms.insert(p.local, Some(form));
                    }
                }
            }
        }
        if !plan.ops.is_empty() {
            plans.insert(id, plan);
        }
    }
    plans
}
/// Exactly `IntRanges::analyze(f, &[])`: an empty function table makes every
/// call result unknown, so only facts proven inside `f` are used.
fn whole_function_ranges(f: &CheckedFunction) -> crate::int_ranges::IntRanges {
    crate::int_ranges::FunctionRanges::new(std::iter::empty()).analyze(f, &[])
}
/// Functions above this many IR operations are never view-versioned (M5).
const VIEW_MAX_OPERATIONS: usize = 400;
/// IR operations of a body, nested control flow included.
fn operation_count(ops: &[Operation]) -> usize {
    ops.iter()
        .map(|op| {
            1 + match op {
                Operation::If { yes, no, .. } => operation_count(yes) + operation_count(no),
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => operation_count(condition_code) + operation_count(body),
                Operation::Switch { arms, .. } => arms.iter().map(|a| operation_count(a)).sum(),
                _ => 0,
            }
        })
        .sum()
}
/// Borrowed `VectorView` parameters of `f`: their descriptors live in a caller.
fn borrowed_view_params(f: &CheckedFunction) -> BTreeSet<usize> {
    f.source
        .params
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            matches!(p.ty, Type::VectorView(_))
                && f.locals.get(*i).is_some_and(|l| l.borrow != Borrow::Owned)
        })
        .map(|(i, _)| i)
        .collect()
}
/// Whether `ops` may replace the descriptor storage of a local in `views`:
/// write, move, swap, drop or write back the place itself, take its address
/// (`Rvalue::Reference`), hand it to a resizing/consuming builtin, or pass it
/// by reference to a spawned task, an external symbol or an unknown callee.
/// Element accesses, pair checks, regions and by-reference calls of checked
/// functions only read it.
fn replaces_view(ops: &[Operation], views: &BTreeSet<usize>, internal: &BTreeSet<&str>) -> bool {
    let hit = |p: &Place| views.contains(&p.local);
    ops.iter().any(|op| match op {
        Operation::Init(p, value) => {
            hit(p)
                || match value {
                    Rvalue::Reference(q) => hit(q),
                    Rvalue::Call { name, args, spawn } => args.iter().any(|a| {
                        matches!(a, Argument::Reference(q)
                            if hit(q) && (*spawn || !internal.contains(name.as_str())))
                    }),
                    Rvalue::Builtin { name, args } => {
                        (matches!(
                            name.as_str(),
                            "push" | "texts_push" | "vec_push" | "vec_pop" | "join"
                        ) || name.starts_with("map_")
                            || name.starts_with("arena_")
                            || name.starts_with("channel_"))
                            && args.iter().any(hit)
                    }
                    _ => false,
                }
        }
        Operation::Assign(a, _) | Operation::Drop(a) => hit(a),
        Operation::Move {
            destination,
            source,
            ..
        } => hit(destination) || hit(source),
        Operation::Swap(a, b) => hit(a) || hit(b),
        Operation::ElementWriteback { alias, owner, .. } => hit(alias) || hit(owner),
        Operation::If { yes, no, .. } => {
            replaces_view(yes, views, internal) || replaces_view(no, views, internal)
        }
        Operation::Loop {
            condition_code,
            body,
            ..
        } => replaces_view(condition_code, views, internal) || replaces_view(body, views, internal),
        Operation::Switch { arms, .. } => arms.iter().any(|a| replaces_view(a, views, internal)),
        _ => false,
    })
}
/// 1.1 B (contracts M5, L3 pinning; the whole-program rule of 1.0's
/// `pinned_views`, re-derived here on the IR with stricter exclusions): true
/// when no checked function replaces the descriptor of any of its borrowed
/// `VectorView` parameters. Then no call can replace one either: a view lent
/// onward by reference reaches only parameters that are themselves never
/// replaced (external symbols, spawned tasks and unknown callees count as
/// replacing). Element stores, swaps of elements and pair checks write
/// element storage, never the `(data, len)` descriptor.
fn views_pinned(program: &CheckedProgram) -> bool {
    let internal = internal_names(program);
    program
        .functions
        .iter()
        .filter(|f| f.source.external_symbol.is_none())
        .all(|f| {
            let views = borrowed_view_params(f);
            views.is_empty() || !replaces_view(&f.body, &views, &internal)
        })
}
/// Locals of views read (element access, pair check, `len`) inside a loop.
fn looped_view_reads(ops: &[Operation], in_loop: bool, out: &mut BTreeSet<usize>) {
    for op in ops {
        match op {
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                looped_view_reads(condition_code, true, out);
                looped_view_reads(body, true, out);
            }
            Operation::If { yes, no, .. } => {
                looped_view_reads(yes, in_loop, out);
                looped_view_reads(no, in_loop, out);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    looped_view_reads(arm, in_loop, out);
                }
            }
            Operation::Init(_, Rvalue::Element { owner, .. })
            | Operation::ElementCheckPair { owner, .. }
                if in_loop && owner.fields.is_empty() =>
            {
                out.insert(owner.local);
            }
            Operation::Init(_, Rvalue::Builtin { name, args })
                if in_loop
                    && matches!(
                        name.as_str(),
                        "vec_at" | "vec_clone_at" | "vec_replace" | "len"
                    )
                    && args.first().is_some_and(|p| p.fields.is_empty()) =>
            {
                out.insert(args[0].local);
            }
            _ => {}
        }
    }
}
/// 1.1 B (contract M5): the pinned view parameters of `f` (local -> element
/// type) whose descriptors one entry test validates. Only functions of at
/// most 400 IR operations whose loops read such a view qualify; views of
/// zero-width or SoA elements are never versioned.
fn select_view_versions(
    f: &CheckedFunction,
    types: &crate::types::TypeTable,
    pinned: bool,
) -> BTreeMap<usize, Type> {
    if !pinned
        || f.source.external_symbol.is_some()
        || operation_count(&f.body) > VIEW_MAX_OPERATIONS
    {
        return BTreeMap::new();
    }
    let mut reads = BTreeSet::new();
    looped_view_reads(&f.body, false, &mut reads);
    borrowed_view_params(f)
        .into_iter()
        .filter(|i| reads.contains(i))
        .filter_map(|i| {
            let Type::VectorView(id) = f.locals[i].ty else {
                return None;
            };
            let vector = Type::Named(id);
            if types.soa_element(vector).is_some() {
                return None;
            }
            let element = types.vector_element(vector)?;
            (types.layout(element).size > 0).then_some((i, element))
        })
        .collect()
}
/// Names of the program's checked (non-external) functions.
fn internal_names(program: &CheckedProgram) -> BTreeSet<&str> {
    program
        .functions
        .iter()
        .filter(|f| f.source.external_symbol.is_none())
        .map(|f| f.source.name.as_str())
        .collect()
}
/// 1.2 B (contract N8): a call of the function itself that lends every
/// entry-validated view parameter `k` onward unchanged, as its own argument
/// `k`. The callee then receives the very descriptor the caller validated.
fn fast_self_call(args: &[Argument], versions: &BTreeMap<usize, Type>) -> bool {
    versions.keys().all(|&k| {
        matches!(args.get(k), Some(Argument::Reference(p)) if p.local == k && p.fields.is_empty())
    })
}
/// Whether `ops` contain an eligible self-call of `name` (see `fast_self_call`).
fn has_fast_self_call(ops: &[Operation], name: &str, versions: &BTreeMap<usize, Type>) -> bool {
    let mut pending: Vec<&Operation> = ops.iter().collect();
    while let Some(op) = pending.pop() {
        match op {
            Operation::Init(
                _,
                Rvalue::Call {
                    name: callee,
                    args,
                    spawn: false,
                },
            ) if callee == name && fast_self_call(args, versions) => return true,
            Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
            Operation::Loop {
                condition_code,
                body,
                ..
            } => pending.extend(condition_code.iter().chain(body)),
            Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
            _ => {}
        }
    }
    false
}
/// 1.2 B (contract N8): whether the validated body needs the descriptor
/// pointer of the entry-validated view parameter `k`, that is, mentions it
/// other than through the cached element accesses, pair checks, `len`,
/// `vec_*` reads/replacements, loan markers (comments only) or as argument
/// `k` of an eligible self-call. Anything else keeps the pointer parameter.
fn view_pointer_used(
    ops: &[Operation],
    k: usize,
    name: &str,
    versions: &BTreeMap<usize, Type>,
) -> bool {
    let hit = |p: &Place| p.local == k;
    let recurse = |ops: &[Operation]| view_pointer_used(ops, k, name, versions);
    ops.iter().any(|op| match op {
        Operation::Init(dst, Rvalue::Element { owner, index })
            if hit(owner) && owner.fields.is_empty() =>
        {
            hit(dst) || hit(index)
        }
        Operation::Init(
            dst,
            Rvalue::Builtin {
                name: builtin,
                args,
            },
        ) if matches!(
            builtin.as_str(),
            "vec_at" | "vec_clone_at" | "vec_replace" | "len"
        ) && args.first().is_some_and(|p| {
            hit(p) && p.fields.is_empty() && matches!(p.ty, Type::VectorView(_))
        }) =>
        {
            hit(dst) || args[1..].iter().any(hit)
        }
        Operation::Init(
            dst,
            Rvalue::Call {
                name: callee,
                args,
                spawn: false,
            },
        ) if callee == name && fast_self_call(args, versions) => {
            hit(dst)
                || args.iter().enumerate().any(|(j, a)| {
                    j != k
                        && match a {
                            Argument::Value(p) | Argument::Reference(p) => hit(p),
                        }
                })
        }
        Operation::ElementCheckPair { owner, left, right }
            if hit(owner) && owner.fields.is_empty() =>
        {
            hit(left) || hit(right)
        }
        Operation::Loan { .. } => false,
        Operation::If { condition, yes, no } => hit(condition) || recurse(yes) || recurse(no),
        Operation::Loop {
            condition_code,
            condition,
            body,
            ..
        } => hit(condition) || recurse(condition_code) || recurse(body),
        Operation::Switch { value, arms } => hit(value) || arms.iter().any(|a| recurse(a)),
        other => operation_places(other).into_iter().any(hit),
    })
}
/// Ids of every loop inside `ops` (nested ones included).
fn loop_ids(ops: &[Operation], out: &mut BTreeSet<usize>) {
    for op in ops {
        match op {
            Operation::Loop {
                id,
                condition_code,
                body,
                ..
            } => {
                out.insert(*id);
                loop_ids(condition_code, out);
                loop_ids(body, out);
            }
            Operation::If { yes, no, .. } => {
                loop_ids(yes, out);
                loop_ids(no, out);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    loop_ids(arm, out);
                }
            }
            _ => {}
        }
    }
}
/// One local VectorView versioned for the rest of its block (contract N8):
/// the view local and its element type.
type LocalView = (usize, Type);
/// 1.2 B (contract N8): local views versioned like M5's pinned parameters.
/// A qualifying `Init(view, Region { .. })` of an owned, non-parameter
/// `VectorView` local (nonzero, non-SoA element) in a body block (function
/// body, `if` arm, loop body or switch arm, never a loop condition) whose
/// remaining block (at most 400 IR operations) reads the view in a loop and
/// never replaces its descriptor (the M5 `replaces_view` rule; the program
/// must be pinned, so checked callees that borrow it cannot replace it
/// either). The descriptor is then constant for the rest of the block, so
/// one validity test right after the definition selects a copy of that rest
/// that caches `(data, len)`; the other copy is the unchanged lowering.
/// Keyed by the address of the defining operation.
fn local_view_plans(
    f: &CheckedFunction,
    types: &crate::types::TypeTable,
    pinned: bool,
    internal: &BTreeSet<&str>,
) -> BTreeMap<usize, LocalView> {
    let mut plans = BTreeMap::new();
    if !pinned || f.source.external_symbol.is_some() {
        return plans;
    }
    let mut blocks: Vec<&[Operation]> = vec![&f.body];
    while let Some(block) = blocks.pop() {
        for (i, op) in block.iter().enumerate() {
            match op {
                Operation::If { yes, no, .. } => {
                    blocks.push(yes);
                    blocks.push(no);
                }
                Operation::Loop { body, .. } => blocks.push(body),
                Operation::Switch { arms, .. } => blocks.extend(arms.iter().map(Vec::as_slice)),
                Operation::Init(view, Rvalue::Region { .. })
                    if view.fields.is_empty()
                        && view.local >= f.source.params.len()
                        && f.locals[view.local].borrow == Borrow::Owned =>
                {
                    let Type::VectorView(id) = f.locals[view.local].ty else {
                        continue;
                    };
                    let vector = Type::Named(id);
                    let Some(element) = types.vector_element(vector) else {
                        continue;
                    };
                    if types.soa_element(vector).is_some() || types.layout(element).size == 0 {
                        continue;
                    }
                    let rest = &block[i + 1..];
                    let mut reads = BTreeSet::new();
                    looped_view_reads(rest, false, &mut reads);
                    if reads.contains(&view.local)
                        && operation_count(rest) <= VIEW_MAX_OPERATIONS
                        && !replaces_view(rest, &BTreeSet::from([view.local]), internal)
                    {
                        plans.insert(op as *const Operation as usize, (view.local, element));
                    }
                }
                _ => {}
            }
        }
    }
    plans
}
/// Inclusive value range of an integer type.
fn integer_range(number: crate::numeric::Integer) -> (i128, i128) {
    let bits = number.bits() as u32;
    if number.signed() {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

type Bounds = crate::int_ranges::Bounds;
/// Identity of one operand `Place` inside the borrowed checked IR. The IR is
/// immutable and alive for the whole emission, so the address is stable.
fn operand_key(p: &Place) -> usize {
    p as *const Place as usize
}
/// A boolean local's defining relation over tracked `int` locals (1.0 B).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Relation {
    /// `left op right` for a checked integer comparison.
    Compare(String, usize, usize),
    /// `low <= value && value <= high` (`Rvalue::IntRange`).
    Range(usize, i64, i64),
    /// Logical negation of another boolean's relation.
    Not(Box<Relation>),
}
impl Relation {
    fn mentions(&self, local: usize) -> bool {
        match self {
            Relation::Compare(_, a, b) => *a == local || *b == local,
            Relation::Range(value, ..) => *value == local,
            Relation::Not(inner) => inner.mentions(local),
        }
    }
}
/// Facts that hold at one program point on every path reaching it.
/// `bounds` entries are already intersected with the whole-function facts;
/// `equal` maps a local to the copy source it still equals (neither written
/// since the copy); `relations` keep the comparison that defined a boolean.
/// 1.1 B: in the view-validated body only (contract M5), `below[x]` holds the
/// pinned views `v` with `x < len(v)` and `lengths[x] = v` records `x ==
/// len(v)`. A pinned view's descriptor is constant for the whole call, so
/// these facts are killed only by writes to `x`.
#[derive(Clone, Default)]
struct FlowState {
    reachable: bool,
    bounds: BTreeMap<usize, Bounds>,
    equal: BTreeMap<usize, usize>,
    relations: BTreeMap<usize, Relation>,
    below: BTreeMap<usize, BTreeSet<usize>>,
    lengths: BTreeMap<usize, usize>,
}
impl FlowState {
    fn kill(&mut self, local: usize) {
        self.bounds.remove(&local);
        self.equal.retain(|k, v| *k != local && *v != local);
        self.relations
            .retain(|k, r| *k != local && !r.mentions(local));
        self.below.remove(&local);
        self.lengths.remove(&local);
    }
    /// Control-flow merge: keep only what holds on both incoming paths.
    fn join(self, other: Self) -> Self {
        if !self.reachable {
            return other;
        }
        if !other.reachable {
            return self;
        }
        let bounds = self
            .bounds
            .iter()
            .filter_map(|(k, a)| {
                other.bounds.get(k).map(|b| {
                    (
                        *k,
                        Bounds {
                            low: a.low.min(b.low),
                            high: a.high.max(b.high),
                        },
                    )
                })
            })
            .collect();
        let equal = self
            .equal
            .into_iter()
            .filter(|(k, v)| other.equal.get(k) == Some(v))
            .collect();
        let relations = self
            .relations
            .into_iter()
            .filter(|(k, r)| other.relations.get(k) == Some(r))
            .collect();
        let below = self
            .below
            .into_iter()
            .filter_map(|(k, views)| {
                let both: BTreeSet<usize> =
                    other.below.get(&k)?.intersection(&views).copied().collect();
                (!both.is_empty()).then_some((k, both))
            })
            .collect();
        let lengths = self
            .lengths
            .into_iter()
            .filter(|(k, v)| other.lengths.get(k) == Some(v))
            .collect();
        FlowState {
            reachable: true,
            bounds,
            equal,
            relations,
            below,
            lengths,
        }
    }
}
/// Every local an operation (including nested control flow) may write.
fn written_locals(ops: &[Operation], out: &mut BTreeSet<usize>) {
    for op in ops {
        match op {
            Operation::If { yes, no, .. } => {
                written_locals(yes, out);
                written_locals(no, out);
            }
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                written_locals(condition_code, out);
                written_locals(body, out);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    written_locals(arm, out);
                }
            }
            leaf => out.extend(leaf_writes(leaf)),
        }
    }
}
/// Locals written by one non-control operation. Escaping arguments (call
/// references, channel endpoints) are listed too; they are never tracked.
fn leaf_writes(op: &Operation) -> Vec<usize> {
    match op {
        Operation::Init(p, value) => {
            let mut out = vec![p.local];
            match value {
                Rvalue::Call { args, .. } => out.extend(args.iter().filter_map(|a| match a {
                    Argument::Reference(p) => Some(p.local),
                    Argument::Value(_) => None,
                })),
                Rvalue::Builtin { name, args } if name.starts_with("channel_") => {
                    out.extend(args.iter().map(|p| p.local))
                }
                _ => {}
            }
            out
        }
        Operation::Assign(p, _) | Operation::Drop(p) => vec![p.local],
        Operation::Move {
            destination,
            source,
            ..
        } => vec![destination.local, source.local],
        Operation::Swap(a, b) => vec![a.local, b.local],
        Operation::Loan { place, .. } => vec![place.local],
        _ => Vec::new(),
    }
}
/// C-side flow-sensitive integer facts (contracts L1/L2). A forward walk over
/// the structured checked IR in emission order:
/// - only owned `int`/`bool` locals whose address never escapes are tracked,
///   so every write to them is an explicit IR definition;
/// - a write kills every fact, copy equality and relation that mentions the
///   written local;
/// - `if` arms and executed `assert`s refine by their condition, merges keep
///   what holds on both paths, and `return`/`break`/`continue` end a path;
/// - a loop head forgets every local written anywhere in the loop, so its
///   state holds on entry, on every back edge (`continue`, fallthrough) and
///   at every exit; the body additionally assumes the loop condition.
///
/// Every bound is intersected with the whole-function `IntRanges` fact. The
/// result is the proven range of each `int` operand at its exact read site.
///
/// 1.1 B (contract M6): owned unsigned integer locals are tracked too. Their
/// facts are kept only while they fit `[0, i64::MAX]`: conversions from a
/// proven source, copies, and `bit_and`, whose unsigned result never exceeds
/// either operand (so `bit_and(x, K)` lies in `[0, K]`). An unsigned operand
/// without a fact has its type's full range.
///
/// 1.1 B (contract M5): `views` names pinned view parameters whose
/// descriptors passed the entry validity test (`len <= INT64_MAX/width`).
/// Only that analysis, which feeds only the validated body, adds:
/// - a successful access `v[i]` gives `0 <= i <= INT64_MAX/width - 1` and
///   `i < len(v)`; `len(v)` lies in `[0, INT64_MAX/width]`;
/// - `i < n` with `n == len(v)` or `n < len(v)`, and `i <= n` with `n <
///   len(v)`, give `i < len(v)`;
/// - a loop that no `break` leaves exits with the facts of its final test
///   (evaluated from the loop-head state, which holds on every iteration).
struct Flow<'a> {
    whole: &'a crate::int_ranges::IntRanges,
    tracked: Vec<bool>,
    operands: BTreeMap<usize, Bounds>,
    abandoned: bool,
    views: &'a BTreeMap<usize, u64>,
    /// Operand keys of view indices proven `0 <= i < len(view)` (M5 only).
    proven: BTreeSet<usize>,
}
/// An unsigned integer type (contract M6 facts).
fn unsigned(ty: Type) -> bool {
    matches!(ty, Type::Number(n) if !n.signed())
}
/// Exact facts of one function's flow analysis: operand ranges, the tracked
/// local mask, and view indices proven in bounds (validated body only).
type FlowFacts = (BTreeMap<usize, Bounds>, Vec<bool>, BTreeSet<usize>);
impl<'a> Flow<'a> {
    /// Nesting limit for the recursive walk; deeper functions keep only the
    /// whole-function facts.
    const DEPTH: usize = 256;
    /// Returns the operand facts and the tracked-local mask.
    fn analyze(
        f: &CheckedFunction,
        whole: &crate::int_ranges::IntRanges,
    ) -> (BTreeMap<usize, Bounds>, Vec<bool>) {
        let (operands, tracked, _) = Flow::analyze_views(f, whole, &BTreeMap::new());
        (operands, tracked)
    }
    /// The same analysis for the body that runs only when every view in
    /// `views` (local -> element width) passed the entry validity test.
    fn analyze_views(
        f: &CheckedFunction,
        whole: &'a crate::int_ranges::IntRanges,
        views: &'a BTreeMap<usize, u64>,
    ) -> FlowFacts {
        let mut escaped = vec![false; f.locals.len()];
        let mut pending: Vec<&Operation> = f.body.iter().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => pending.extend(condition_code.iter().chain(body)),
                Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
                Operation::Swap(a, b) => {
                    escaped[a.local] = true;
                    escaped[b.local] = true;
                }
                Operation::Loan { place, .. } => escaped[place.local] = true,
                Operation::Init(_, value) => match value {
                    Rvalue::Call { args, .. } => {
                        for arg in args {
                            if let Argument::Reference(p) = arg {
                                escaped[p.local] = true;
                            }
                        }
                    }
                    Rvalue::Builtin { name, args } if name.starts_with("channel_") => {
                        for p in args {
                            escaped[p.local] = true;
                        }
                    }
                    Rvalue::Reference(p) => escaped[p.local] = true,
                    Rvalue::Element { owner, .. } | Rvalue::Region { owner, .. } => {
                        escaped[owner.local] = true
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        let tracked = f
            .locals
            .iter()
            .zip(&escaped)
            .map(|(l, &e)| {
                !e && l.borrow == Borrow::Owned
                    && (matches!(l.ty, Type::Int | Type::Bool) || unsigned(l.ty))
            })
            .collect();
        let mut flow = Flow {
            whole,
            tracked,
            operands: BTreeMap::new(),
            abandoned: false,
            views,
            proven: BTreeSet::new(),
        };
        let start = FlowState {
            reachable: true,
            ..Default::default()
        };
        flow.block(&f.body, start, 0);
        if flow.abandoned {
            return (
                BTreeMap::new(),
                vec![false; f.locals.len()],
                BTreeSet::new(),
            );
        }
        (flow.operands, flow.tracked, flow.proven)
    }
    fn tracked_int(&self, p: &Place) -> bool {
        p.fields.is_empty() && p.ty == Type::Int && self.tracked[p.local]
    }
    fn tracked_bool(&self, p: &Place) -> bool {
        p.fields.is_empty() && p.ty == Type::Bool && self.tracked[p.local]
    }
    fn tracked_unsigned(&self, p: &Place) -> bool {
        p.fields.is_empty() && unsigned(p.ty) && self.tracked[p.local]
    }
    /// M6: the proven range of an unsigned operand, or `None` when it may be
    /// any u64 (no fact, or a value above `i64::MAX`).
    fn read_unsigned(&self, s: &FlowState, p: &Place) -> Option<Bounds> {
        let Type::Number(n) = p.ty else {
            return None;
        };
        if n.signed() {
            return None;
        }
        if self.tracked_unsigned(p) {
            if let Some(b) = s.bounds.get(&p.local) {
                return Some(*b);
            }
        }
        (n.bits() < 64).then(|| crate::int_ranges::type_bounds(p.ty))
    }
    /// M6: successful results of a definition of unsigned type `ty`, clamped
    /// to `[0, min(type max, i64::MAX)]`; `None` when no such bound is known.
    fn unsigned_bounds(&self, s: &FlowState, value: &Rvalue, ty: Type) -> Option<Bounds> {
        let Type::Number(n) = ty else {
            return None;
        };
        let max = if n.bits() >= 64 {
            i64::MAX as i128
        } else {
            (1i128 << n.bits()) - 1
        };
        let clamp = |b: Bounds| {
            let (low, high) = ((b.low as i128).max(0), (b.high as i128).min(max));
            (low <= high).then_some(Bounds {
                low: low as i64,
                high: high as i64,
            })
        };
        let fact = match value {
            Rvalue::Copy(p) if p.ty == ty => self.read_unsigned(s, p),
            // A successful conversion preserves the value, so it lies in
            // the source's proven range and in the target type.
            Rvalue::Convert(p) => match p.ty {
                Type::Int => clamp(self.read(s, p)),
                Type::Number(m) if !m.signed() => self.read_unsigned(s, p).and_then(clamp),
                Type::Number(_) => clamp(crate::int_ranges::type_bounds(p.ty)),
                _ => None,
            },
            // Unsigned `a & b <= min(a, b)`; with a constant K, `[0, K]`.
            Rvalue::Builtin { name, args }
                if name == "bit_and" && args.len() == 2 && args.iter().all(|a| a.ty == ty) =>
            {
                args.iter()
                    .filter_map(|a| self.read_unsigned(s, a))
                    .map(|b| b.high)
                    .min()
                    .map(|high| Bounds { low: 0, high })
            }
            _ => None,
        };
        fact.or_else(|| (n.bits() < 64).then(|| crate::int_ranges::type_bounds(ty)))
    }
    /// The pinned, entry-validated view `p` names (M5 analysis only).
    fn view_of(&self, p: &Place) -> Option<usize> {
        (p.fields.is_empty() && self.views.contains_key(&p.local)).then_some(p.local)
    }
    /// The view and index of an element access by `value`, if any.
    fn view_access<'v>(&self, value: &'v Rvalue) -> Option<(usize, &'v Place)> {
        match value {
            Rvalue::Element { owner, index } => self.view_of(owner).map(|v| (v, index)),
            Rvalue::Builtin { name, args }
                if args.len() >= 2
                    && matches!(name.as_str(), "vec_at" | "vec_clone_at" | "vec_replace") =>
            {
                self.view_of(&args[0]).map(|v| (v, &args[1]))
            }
            _ => None,
        }
    }
    /// Record `x < len(view)` for `x` and every local still equal to it.
    fn mark_below(&self, s: &mut FlowState, local: usize, view: usize) {
        let root = s.equal.get(&local).copied().unwrap_or(local);
        let mut members: Vec<usize> = s
            .equal
            .iter()
            .filter(|(_, v)| **v == root)
            .map(|(k, _)| *k)
            .collect();
        members.push(root);
        for m in members {
            s.below.entry(m).or_default().insert(view);
        }
    }
    /// `small < large` (`strict`) or `small <= large` holds: `small` is below
    /// every view length `large` is below, and below `len(v)` when `large ==
    /// len(v)` and the comparison is strict.
    fn below_from(&self, s: &mut FlowState, small: usize, large: usize, strict: bool) {
        let mut views = s.below.get(&large).cloned().unwrap_or_default();
        if strict {
            views.extend(s.lengths.get(&large).copied());
        }
        for view in views {
            self.mark_below(s, small, view);
        }
    }
    /// Note whether `index` is proven `0 <= index < len(view)` at this read.
    fn note_proven(&mut self, s: &FlowState, view: usize, index: &Place) {
        if s.reachable
            && self.tracked_int(index)
            && self.read(s, index).low >= 0
            && s.below
                .get(&index.local)
                .is_some_and(|views| views.contains(&view))
        {
            self.proven.insert(operand_key(index));
        }
    }
    /// After a successful access `view[index]`: `0 <= index < len(view) <=
    /// INT64_MAX/width`.
    fn accessed(&self, s: &mut FlowState, view: usize, index: &Place) {
        if !s.reachable || !self.tracked_int(index) {
            return;
        }
        let width = self.views[&view].max(1) as i128;
        self.narrow(s, index.local, 0, i64::MAX as i128 / width - 1);
        self.mark_below(s, index.local, view);
    }
    fn whole_local(&self, local: usize) -> Bounds {
        self.whole
            .get(&Place::local(local, Type::Int), &BTreeMap::new())
    }
    fn read(&self, s: &FlowState, p: &Place) -> Bounds {
        if !self.tracked_int(p) {
            return self.whole.get(p, &BTreeMap::new());
        }
        s.bounds
            .get(&p.local)
            .copied()
            .unwrap_or_else(|| self.whole_local(p.local))
    }
    /// Successful results of one definition (the same transfer rules as the
    /// whole-function analysis, evaluated with the facts at this point).
    fn rvalue_bounds(&self, s: &FlowState, value: &Rvalue) -> Bounds {
        let read = |p: &Place| self.read(s, p);
        match value {
            Rvalue::Int(n) => Bounds::exact(*n),
            Rvalue::Copy(p) => read(p),
            Rvalue::Binary(op, a, b) if a.ty == Type::Int && b.ty == Type::Int => {
                read(a).binary(op, read(b))
            }
            Rvalue::Convert(p) => match p.ty {
                Type::Int => read(p),
                // M6: an unsigned source's proven range (a successful
                // conversion to int is at most i64::MAX).
                Type::Number(n) if !n.signed() => self.read_unsigned(s, p).unwrap_or(Bounds {
                    low: 0,
                    high: i64::MAX,
                }),
                Type::Number(_) => crate::int_ranges::type_bounds(p.ty),
                _ => Bounds::FULL,
            },
            Rvalue::Unary(op, p) if op == "-" && p.ty == Type::Int => {
                Bounds::exact(0).arithmetic("-", read(p)).0
            }
            Rvalue::Builtin { name, args } => match name.as_str() {
                // M5 (validated body only): `len <= INT64_MAX/width`.
                "len" if args.len() == 1 && self.view_of(&args[0]).is_some() => Bounds {
                    low: 0,
                    high: i64::MAX / self.views[&args[0].local].max(1) as i64,
                },
                "byte_at" => Bounds { low: 0, high: 255 },
                "fixed_len" => match args.first().map(|p| p.ty) {
                    Some(Type::Fixed(_, n)) => Bounds::exact(n as i64),
                    _ => Bounds {
                        low: 0,
                        high: i64::MAX,
                    },
                },
                "len" | "map_len" | "arena_len" | "vec_capacity" => Bounds {
                    low: 0,
                    high: i64::MAX,
                },
                // AND with a nonnegative operand cannot set the sign bit.
                "bit_and" if args.iter().all(|p| p.ty == Type::Int) => args
                    .iter()
                    .map(read)
                    .filter(|b| b.low >= 0)
                    .map(|b| Bounds {
                        low: 0,
                        high: b.high,
                    })
                    .min_by_key(|b| b.high)
                    .unwrap_or(Bounds::FULL),
                _ => Bounds::FULL,
            },
            _ => Bounds::FULL,
        }
    }
    fn relation(&self, s: &FlowState, value: &Rvalue) -> Option<Relation> {
        match value {
            Rvalue::Binary(op, a, b)
                if ["<", "<=", ">", ">=", "==", "!="].contains(&op.as_str())
                    && self.tracked_int(a)
                    && self.tracked_int(b) =>
            {
                Some(Relation::Compare(op.clone(), a.local, b.local))
            }
            Rvalue::IntRange { value, low, high } if self.tracked_int(value) => {
                Some(Relation::Range(value.local, *low, *high))
            }
            Rvalue::Unary(op, p) if op == "!" && self.tracked_bool(p) => s
                .relations
                .get(&p.local)
                .map(|r| Relation::Not(Box::new(r.clone()))),
            Rvalue::Copy(p) if self.tracked_bool(p) => s.relations.get(&p.local).cloned(),
            _ => None,
        }
    }
    /// Intersect a local and every local still equal to it with `[low, high]`.
    /// An empty intersection leaves the facts unchanged (always sound).
    fn narrow(&self, s: &mut FlowState, local: usize, low: i128, high: i128) {
        let root = s.equal.get(&local).copied().unwrap_or(local);
        let mut members: Vec<usize> = s
            .equal
            .iter()
            .filter(|(_, v)| **v == root)
            .map(|(k, _)| *k)
            .collect();
        members.push(root);
        let (mut low, mut high) = (low.max(i64::MIN as i128), high.min(i64::MAX as i128));
        for &m in &members {
            let b = s
                .bounds
                .get(&m)
                .copied()
                .unwrap_or_else(|| self.whole_local(m));
            low = low.max(b.low as i128);
            high = high.min(b.high as i128);
        }
        if low > high {
            return;
        }
        for m in members {
            s.bounds.insert(
                m,
                Bounds {
                    low: low as i64,
                    high: high as i64,
                },
            );
        }
    }
    fn apply(&self, s: &mut FlowState, relation: &Relation, truth: bool) {
        const MIN: i128 = i64::MIN as i128;
        const MAX: i128 = i64::MAX as i128;
        match relation {
            Relation::Not(inner) => self.apply(s, inner, !truth),
            Relation::Range(value, low, high) => {
                if truth {
                    self.narrow(s, *value, *low as i128, *high as i128);
                }
            }
            Relation::Compare(op, a, b) => {
                let op = match (truth, op.as_str()) {
                    (true, op) => op,
                    (false, "<") => ">=",
                    (false, "<=") => ">",
                    (false, ">") => "<=",
                    (false, ">=") => "<",
                    (false, "==") => "!=",
                    (false, _) => "==",
                };
                let x = s
                    .bounds
                    .get(a)
                    .copied()
                    .unwrap_or_else(|| self.whole_local(*a));
                let y = s
                    .bounds
                    .get(b)
                    .copied()
                    .unwrap_or_else(|| self.whole_local(*b));
                let (xl, xh, yl, yh) =
                    (x.low as i128, x.high as i128, y.low as i128, y.high as i128);
                // `below`/`lengths` exist only in the M5 validated analysis.
                match op {
                    "<" => {
                        self.narrow(s, *a, MIN, yh - 1);
                        self.narrow(s, *b, xl + 1, MAX);
                        self.below_from(s, *a, *b, true);
                    }
                    "<=" => {
                        self.narrow(s, *a, MIN, yh);
                        self.narrow(s, *b, xl, MAX);
                        self.below_from(s, *a, *b, false);
                    }
                    ">" => {
                        self.narrow(s, *a, yl + 1, MAX);
                        self.narrow(s, *b, MIN, xh - 1);
                        self.below_from(s, *b, *a, true);
                    }
                    ">=" => {
                        self.narrow(s, *a, yl, MAX);
                        self.narrow(s, *b, MIN, xh);
                        self.below_from(s, *b, *a, false);
                    }
                    "==" => {
                        self.narrow(s, *a, yl, yh);
                        self.narrow(s, *b, xl, xh);
                        self.below_from(s, *a, *b, false);
                        self.below_from(s, *b, *a, false);
                    }
                    _ => {}
                }
            }
        }
    }
    fn refine(&self, s: &mut FlowState, condition: &Place, truth: bool) {
        if !s.reachable || !self.tracked_bool(condition) {
            return;
        }
        if let Some(relation) = s.relations.get(&condition.local).cloned() {
            self.apply(s, &relation, truth);
        }
    }
    fn record(&mut self, s: &FlowState, value: &Rvalue) {
        let mut places: Vec<&Place> = Vec::new();
        match value {
            Rvalue::Repeat(p, _)
            | Rvalue::Convert(p)
            | Rvalue::Copy(p)
            | Rvalue::Unary(_, p)
            | Rvalue::IntRange { value: p, .. } => places.push(p),
            Rvalue::Binary(_, a, b) => places.extend([a, b]),
            Rvalue::Builtin { args, .. } | Rvalue::Record(args) => places.extend(args),
            Rvalue::Region { start, end, .. } => places.extend([start, end]),
            Rvalue::Element { index, .. } => places.push(index),
            _ => {}
        }
        for p in places {
            if self.tracked_int(p) {
                let b = self.read(s, p);
                self.operands.insert(operand_key(p), b);
            } else if self.tracked_unsigned(p) {
                // M6: only a derived fact; otherwise the type range applies.
                if let Some(b) = s.bounds.get(&p.local) {
                    self.operands.insert(operand_key(p), *b);
                }
            }
        }
    }
    fn block(&mut self, ops: &[Operation], mut s: FlowState, depth: usize) -> FlowState {
        if depth > Self::DEPTH {
            self.abandoned = true;
            return s;
        }
        for op in ops {
            s = self.operation(op, s, depth);
        }
        s
    }
    fn operation(&mut self, op: &Operation, mut s: FlowState, depth: usize) -> FlowState {
        match op {
            Operation::Init(destination, value) => {
                if s.reachable {
                    self.record(&s, value);
                }
                let access = self.view_access(value);
                if let Some((view, index)) = access {
                    self.note_proven(&s, view, index);
                }
                let bound = if self.tracked_int(destination) {
                    Some(
                        self.rvalue_bounds(&s, value)
                            .intersect(self.whole_local(destination.local)),
                    )
                } else if self.tracked_unsigned(destination) {
                    self.unsigned_bounds(&s, value, destination.ty)
                } else {
                    None
                };
                let relation = if self.tracked_bool(destination) {
                    self.relation(&s, value)
                } else {
                    None
                };
                let source = match value {
                    Rvalue::Copy(source)
                        if self.tracked_int(destination)
                            && self.tracked_int(source)
                            && source.local != destination.local =>
                    {
                        Some(s.equal.get(&source.local).copied().unwrap_or(source.local))
                    }
                    _ => None,
                };
                // M5 view facts travel with copies; `len(v)` names its view.
                let carried = match value {
                    Rvalue::Copy(p) if source.is_some() => self.view_facts(&s, p.local),
                    Rvalue::Builtin { name, args }
                        if name == "len"
                            && args.len() == 1
                            && self.tracked_int(destination)
                            && self.view_of(&args[0]).is_some() =>
                    {
                        (None, Some(args[0].local))
                    }
                    _ => (None, None),
                };
                self.define(&mut s, op, bound, source, relation);
                self.carry(&mut s, destination.local, carried);
                if let Some((view, index)) = access {
                    if index.local != destination.local {
                        self.accessed(&mut s, view, index);
                    }
                }
            }
            Operation::Assign(destination, source) => {
                let bound = if self.tracked_int(destination) {
                    Some(
                        self.read(&s, source)
                            .intersect(self.whole_local(destination.local)),
                    )
                } else if self.tracked_unsigned(destination) && source.ty == destination.ty {
                    self.read_unsigned(&s, source)
                } else {
                    None
                };
                let relation = if self.tracked_bool(destination) && self.tracked_bool(source) {
                    s.relations.get(&source.local).cloned()
                } else {
                    None
                };
                let copy = (self.tracked_int(destination)
                    && self.tracked_int(source)
                    && source.local != destination.local)
                    .then(|| s.equal.get(&source.local).copied().unwrap_or(source.local));
                let carried = if copy.is_some() {
                    self.view_facts(&s, source.local)
                } else {
                    (None, None)
                };
                self.define(&mut s, op, bound, copy, relation);
                self.carry(&mut s, destination.local, carried);
            }
            // M5 (validated body only): a passing pair check puts both
            // indices in `[0, len(view))`.
            Operation::ElementCheckPair { owner, left, right } if self.view_of(owner).is_some() => {
                let view = owner.local;
                self.note_proven(&s, view, left);
                self.note_proven(&s, view, right);
                self.accessed(&mut s, view, left);
                self.accessed(&mut s, view, right);
            }
            Operation::Assert { value, .. } => self.refine(&mut s, value, true),
            Operation::If { condition, yes, no } => {
                let mut taken = s.clone();
                self.refine(&mut taken, condition, true);
                let taken = self.block(yes, taken, depth + 1);
                self.refine(&mut s, condition, false);
                let other = self.block(no, s, depth + 1);
                s = taken.join(other);
            }
            Operation::Loop {
                id,
                condition_code,
                condition,
                body,
            } => {
                let mut written = BTreeSet::new();
                written_locals(condition_code, &mut written);
                written_locals(body, &mut written);
                for local in written {
                    s.kill(local);
                }
                let tested = self.block(condition_code, s.clone(), depth + 1);
                let mut inner = tested.clone();
                self.refine(&mut inner, condition, true);
                self.block(body, inner, depth + 1);
                // M5 (validated body only): without a `break` the only exit
                // is a false final test, evaluated from the head state that
                // holds on entry and on every back edge.
                if !self.views.is_empty()
                    && !loop_targets(condition_code, *id).0
                    && !loop_targets(body, *id).0
                {
                    s = tested;
                    self.refine(&mut s, condition, false);
                }
            }
            Operation::Switch { arms, .. } => {
                let mut merged = FlowState::default();
                for arm in arms {
                    merged = merged.join(self.block(arm, s.clone(), depth + 1));
                }
                s = merged;
            }
            Operation::Return(_) | Operation::Break(_) | Operation::Continue(_) => {
                s.reachable = false
            }
            leaf => {
                for local in leaf_writes(leaf) {
                    s.kill(local);
                }
            }
        }
        s
    }
    /// The M5 view facts of `local` (`below`, `lengths`).
    fn view_facts(&self, s: &FlowState, local: usize) -> (Option<BTreeSet<usize>>, Option<usize>) {
        (s.below.get(&local).cloned(), s.lengths.get(&local).copied())
    }
    /// Give a freshly defined copy the view facts of its source.
    fn carry(
        &self,
        s: &mut FlowState,
        local: usize,
        (below, length): (Option<BTreeSet<usize>>, Option<usize>),
    ) {
        if let Some(views) = below {
            s.below.insert(local, views);
        }
        if let Some(view) = length {
            s.lengths.insert(local, view);
        }
    }
    fn define(
        &self,
        s: &mut FlowState,
        op: &Operation,
        bound: Option<Bounds>,
        source: Option<usize>,
        relation: Option<Relation>,
    ) {
        for local in leaf_writes(op) {
            s.kill(local);
        }
        let Some(destination) = (match op {
            Operation::Init(p, _) | Operation::Assign(p, _) => Some(p.local),
            _ => None,
        }) else {
            return;
        };
        if let Some(b) = bound {
            s.bounds.insert(destination, b);
        }
        if let Some(source) = source {
            s.equal.insert(destination, source);
        }
        if let Some(relation) = relation {
            s.relations.insert(destination, relation);
        }
    }
}
impl Writer<'_> {
    /// The proven range of an `int` operand: the flow fact at this exact read
    /// site when one was derived, otherwise the whole-function fact.
    fn bounds(&self, p: &Place) -> Bounds {
        if let Some(b) = self.current_flow().get(&operand_key(p)) {
            return *b;
        }
        self.ranges.get(p, &BTreeMap::new())
    }
    /// The validated body's facts while it is emitted (M5), else the
    /// ordinary facts (the checked body and unversioned functions).
    fn current_flow(&self) -> &BTreeMap<usize, Bounds> {
        if self.view_fast.get() {
            &self.fast_flow
        } else {
            &self.flow
        }
    }
    /// The entry-validated view `p` names while the fast body is emitted,
    /// or the local view whose cached rest of block is emitted (N8).
    fn fast_view(&self, p: &Place) -> Option<usize> {
        if !p.fields.is_empty() {
            return None;
        }
        if self.view_fast.get() && self.view_versions.contains_key(&p.local) {
            return Some(p.local);
        }
        self.active_local
            .get()
            .and_then(|(k, _, _)| (k == p.local).then_some(k))
    }
    /// Element type of a view `fast_view` returned.
    fn fast_element(&self, k: usize) -> Type {
        match self.view_versions.get(&k) {
            Some(element) if self.view_fast.get() => *element,
            _ => self.active_local.get().unwrap().2,
        }
    }
    /// `index` is proven `0 <= index < len(view)` at this read: M5 facts for
    /// entry-validated parameters; for a cached local view (N8), proven
    /// bounds `[0, min_len)` where `min_len` bounds the region's count, which
    /// is the view's length (`zn_vector_view_region` sets `len = count`).
    fn view_proven(&self, k: usize, index: &Place) -> bool {
        if let Some((local, min_len, _)) = self.active_local.get() {
            if local == k {
                let b = self.bounds(index);
                return index.ty == Type::Int && b.low >= 0 && b.high < min_len;
            }
        }
        self.view_fast.get() && self.fast_proven.contains(&operand_key(index))
    }
    /// The label of loop `id`: `_v110` in the validated body, plus `_v120`
    /// for loops inside the cached rest of a local view's block (N8).
    fn loop_label(&self, id: usize) -> String {
        let local = self.active_local.get().is_some() && self.local_loops.borrow().contains(&id);
        format!(
            "zn_loop_{id}{}{}",
            self.label_suffix.get(),
            if local { "_v120" } else { "" }
        )
    }
    /// Arguments of `zn_v120_fast_<name>` for an eligible self-call (N8):
    /// the kept parameters' arguments, then each validated view's cached
    /// `(data, len)`.
    fn fast_call_args(&self, args: &[Argument]) -> Vec<String> {
        let kept = self.self_fast.as_ref().unwrap();
        let mut out: Vec<String> = kept
            .iter()
            .map(|&j| match &args[j] {
                Argument::Value(v) => self.place(v),
                Argument::Reference(v) => format!("&{}", self.place(v)),
            })
            .collect();
        for &k in self.view_versions.keys() {
            out.push(format!("zn_v110_data_{k}"));
            out.push(format!("zn_v110_len_{k}"));
        }
        out
    }
    /// Entry test of a versioned function: every pinned view satisfies the
    /// runtime invariant; the validated body then caches `data`/`len`.
    fn view_entry(&self) -> String {
        let mut tests = Vec::new();
        let mut locals = String::new();
        for (&k, element) in &self.view_versions {
            let place = self.place(&Place::local(k, self.f.locals[k].ty));
            let e = element.c();
            tests.push(format!(
                "zn_v110_view_valid(&{place},{})",
                self.types.layout(*element).size
            ));
            locals.push_str(&format!(
                "{e}* const zn_v110_data_{k}=({e}*){place}.data;\nconst uint64_t zn_v110_len_{k}={place}.len;\n"
            ));
        }
        format!(
            "/* 1.1 pinned VectorView entry versioning (contract M5): runtime invariant tested once */\nif ({}) {{\n{locals}",
            tests.join(" && ")
        )
    }
    /// Proven `0 <= p` and `p + width <= len` on every path (contract S4).
    fn proven_in_bounds(&self, p: &Place, len: usize, width: usize) -> bool {
        let b = self.bounds(p);
        p.ty == Type::Int && b.low >= 0 && (b.high as i128) + (width as i128) <= len as i128
    }
    /// Proven source range for an integer conversion: the IntRanges fact for
    /// `int` places, otherwise the source type's own exact range.
    fn proven_range(&self, p: &Place) -> (i128, i128) {
        let number = p.ty.integer().unwrap();
        if p.ty == Type::Int {
            let b = self.bounds(p);
            (b.low as i128, b.high as i128)
        } else if let Some(b) = self.current_flow().get(&operand_key(p)) {
            // M6: a flow-proven unsigned range (within the type's own).
            (b.low as i128, b.high as i128)
        } else {
            integer_range(number)
        }
    }
    fn external_args(&self, args: &[Argument]) -> Vec<String> {
        args.iter()
            .map(|arg| {
                let p = match arg {
                    Argument::Value(p) | Argument::Reference(p) => p,
                };
                if matches!(p.ty, Type::Fixed(..)) {
                    format!("{}.data", self.place(p))
                } else {
                    match arg {
                        Argument::Value(_) => self.place(p),
                        Argument::Reference(_) => format!("&{}", self.place(p)),
                    }
                }
            })
            .collect()
    }
    fn number_bits(&self, p: &Place) -> String {
        let v = self.place(p);
        if let Type::Float(n) = p.ty {
            format!("zn_{}_bits({v})", n.name())
        } else {
            format!("(uint64_t){v}")
        }
    }
    fn number_from_bits(&self, bits: String, ty: Type) -> String {
        if let Type::Float(n) = ty {
            format!("zn_{}_value({bits})", n.name())
        } else {
            numeric_result(bits, ty)
        }
    }
    fn buffer(&self, p: &Place) -> (String, String, crate::numeric::Integer) {
        let value = self.place(p);
        match p.ty {
            Type::Fixed(n, len) => (format!("&{value}"), len.to_string(), n),
            Type::Slice(n) => (format!("{value}.data"), format!("{value}.len"), n),
            Type::Ints => (
                format!("{value}.data"),
                format!("{value}.len"),
                crate::numeric::Integer::I64,
            ),
            _ => unreachable!(),
        }
    }
    /// 1.3 OS/runtime builtins: borrowed `text`/`str` arguments by value,
    /// byte buffers as (data, len), NetResult through the layout-checked
    /// `zn_v130_net_serve_once` wrapper. `None` for every other builtin.
    fn os_builtin(&self, name: &str, args: &[Place], a: &[String]) -> Option<String> {
        match name {
            "try_read_bytes" | "try_write_bytes" if args.len() == 2 => {
                let (data, len, _) = self.buffer(&args[1]);
                Some(format!("zn_{name}({},{data},(uint64_t)({len}))", a[0]))
            }
            "env_var" | "create_dir" | "remove_file" if args.len() == 1 => {
                Some(format!("zn_{name}({})", a[0]))
            }
            "net_serve_once" if args.len() == 3 => Some(format!(
                "zn_v130_net_serve_once({},{},{})",
                a[0], a[1], a[2]
            )),
            "http_request" if args.len() == 2 => {
                Some(format!("zn_v130_http_request({},{},NULL)", a[0], a[1]))
            }
            "http_request" if args.len() == 3 => Some(format!(
                "zn_v130_http_request({},{},&({}))",
                a[0], a[1], a[2]
            )),
            _ => None,
        }
    }
    fn place(&self, p: &Place) -> String {
        if p.fields.is_empty() {
            if let Some(expression) = self.substitutions.borrow().get(&p.local) {
                return expression.clone();
            }
        }
        let direct = self.direct_soa_aliases.contains(&p.local);
        let mut value = if direct {
            format!("(*zn_column_{}_{})", p.local, p.fields[0])
        } else if p.local < self.f.source.params.len() {
            if self.f.locals[p.local].borrow == Borrow::Owned {
                format!("p{}", p.local)
            } else {
                format!("(*p{})", p.local)
            }
        } else {
            if self.f.locals[p.local].borrow == Borrow::Owned {
                format!("z{}", p.local)
            } else {
                format!("(*z{})", p.local)
            }
        };
        let mut ty = self.f.locals[p.local].ty;
        for (depth, index) in p.fields.iter().enumerate() {
            let Type::Named(id) = ty else { unreachable!() };
            match &self.types.definitions[id].kind {
                DataKind::Record(fields) => {
                    if !direct || depth != 0 {
                        value.push_str(&format!(".f{index}"));
                    }
                    ty = fields[*index].ty;
                }
                DataKind::Enum(fields) => {
                    value.push_str(&format!(".data.v{index}"));
                    ty = fields[*index].ty;
                }
                _ => unreachable!("containers have no exposed fields"),
            }
        }
        value
    }
    fn arithmetic(&self, n: &str) -> String {
        if self.hot {
            n.into()
        } else {
            n.replacen("zn_", "zn_cold_", 1)
        }
    }
    /// Inline fixed-buffer and wire access (contracts S4/S6). Unproven
    /// accesses call checked `zn_v029_*` helpers with the exact runtime fault
    /// strings; only `Type::Fixed` indices proven in bounds by whole-function
    /// facts use the unchecked load/store helpers.
    fn inline_buffer_access(
        &self,
        name: &str,
        args: &[Place],
        a: &[String],
        ty: Type,
    ) -> Option<String> {
        let (data, len, number) = self.buffer(&args[0]);
        let fixed_len = match args[0].ty {
            Type::Fixed(_, n) => Some(n),
            _ => None,
        };
        let proven = |index: &Place, width: usize| {
            fixed_len.is_some_and(|n| self.proven_in_bounds(index, n, width))
        };
        let t = number.name();
        let c = number.c();
        match name {
            "fixed_at" if ty.integer() == Some(number) => Some(if proven(&args[1], 1) {
                format!("zn_v029_fixed_load_{t}({data},{})", a[1])
            } else {
                format!("zn_v029_fixed_at_{t}({data},{len},{})", a[1])
            }),
            "fixed_set" | "fixed_try_set" if args[2].ty.integer() == Some(number) => {
                Some(match (name, proven(&args[1], 1)) {
                    ("fixed_set", true) => {
                        format!("zn_v029_fixed_store_{t}({data},{},({c}){})", a[1], a[2])
                    }
                    ("fixed_try_set", true) => format!(
                        "(zn_v029_fixed_store_{t}({data},{},({c}){}),true)",
                        a[1], a[2]
                    ),
                    _ => format!("zn_v029_{name}_{t}({data},{len},{},({c}){})", a[1], a[2]),
                })
            }
            _ if number != crate::numeric::Integer::U8 => None,
            n if n.starts_with("read_u") => {
                let width = ty.integer()?.bits();
                let wire = &n["read_".len()..];
                if !wire.starts_with(&format!("u{width}_")) {
                    return None;
                }
                Some(if proven(&args[1], width / 8) {
                    format!("zn_v029_load_{wire}({data},{})", a[1])
                } else {
                    format!("zn_v029_read_{wire}({data},{len},{})", a[1])
                })
            }
            n if n.starts_with("try_write_u") => {
                let value = args[2].ty.integer()?;
                let wire = &n["try_write_".len()..];
                if !wire.starts_with(&format!("u{}_", value.bits())) || value.signed() {
                    return None;
                }
                Some(if proven(&args[1], value.bits() / 8) {
                    // Contract S4 (and N2 for `_be`): `0 <= low` and
                    // `high + width <= N` on `[u8; N]`, so the store is in
                    // bounds and the result is `true`.
                    format!("(zn_v029_store_{wire}({data},{},{}),true)", a[1], a[2])
                } else if wire.ends_with("_be") {
                    // 1.2 B (contract N2): the checked big-endian write.
                    format!("zn_v120_try_write_{wire}({data},{len},{},{})", a[1], a[2])
                } else {
                    format!("zn_v029_try_write_{wire}({data},{len},{},{})", a[1], a[2])
                })
            }
            _ => None,
        }
    }
    fn column_value(
        &self,
        expression: &ColumnExpr,
        float: crate::numeric::Float,
        alias: usize,
        packed: bool,
        serial: &mut usize,
        out: &mut String,
    ) -> String {
        let suffix = if float == crate::numeric::Float::F32 {
            "ps"
        } else {
            "pd"
        };
        let scalar = |bits: u64| format!("zn_{}_value(UINT64_C({bits}))", float.name());
        match expression {
            ColumnExpr::Column(column) => {
                if packed {
                    format!("_mm_loadu_{suffix}(zn_soa_base_{alias}_{column}+zn_i)")
                } else {
                    format!("zn_soa_base_{alias}_{column}[zn_i]")
                }
            }
            ColumnExpr::Constant(bits) => {
                if packed {
                    format!("_mm_set1_{suffix}({})", scalar(*bits))
                } else {
                    scalar(*bits)
                }
            }
            ColumnExpr::Invariant(p) => {
                if packed {
                    format!("_mm_set1_{suffix}({})", self.place(p))
                } else {
                    self.place(p)
                }
            }
            ColumnExpr::Binary {
                operator,
                left,
                right,
            } => {
                let left = self.column_value(left, float, alias, packed, serial, out);
                let right = self.column_value(right, float, alias, packed, serial, out);
                let name = format!("zn_lane_{}", *serial);
                *serial += 1;
                let (ty, value) = if packed {
                    let op = match operator.as_str() {
                        "+" => "add",
                        "-" => "sub",
                        "*" => "mul",
                        _ => "div",
                    };
                    (
                        if suffix == "ps" { "__m128" } else { "__m128d" },
                        format!("_mm_{op}_{suffix}({left},{right})"),
                    )
                } else {
                    (float.c(), format!("({left}) {operator} ({right})"))
                };
                out.push_str(&format!("{ty} {name}={value};\n"));
                name
            }
        }
    }
    fn column_simd(&self, simd: &ColumnSimd, alias: usize, out: &mut String) -> bool {
        let updates = match simd {
            ColumnSimd::Constant {
                column,
                float,
                operator,
                constant,
            } => std::borrow::Cow::Owned(vec![ColumnUpdate {
                column: *column,
                float: *float,
                value: ColumnExpr::Binary {
                    operator: operator.clone(),
                    left: Box::new(ColumnExpr::Column(*column)),
                    right: Box::new(ColumnExpr::Constant(*constant)),
                },
            }]),
            ColumnSimd::Columns {
                updates,
                read_columns,
                write_columns,
            } => {
                out.push_str(&format!("/* proven disjoint columns: read {read_columns:?}, write {write_columns:?}; source-ordered stores */\n"));
                std::borrow::Cow::Borrowed(updates.as_slice())
            }
            ColumnSimd::Reduction(_) => return false,
        };
        let float = updates[0].float;
        let lanes = if float == crate::numeric::Float::F32 {
            4
        } else {
            2
        };
        let suffix = if lanes == 4 { "ps" } else { "pd" };
        out.push_str(&format!("/* independent IEEE lane operations; no reassociation or FMA */\n{{size_t zn_i=0;for(;zn_soa_len_{alias}-zn_i>={lanes};zn_i+={lanes}){{\n"));
        let mut serial = 0;
        for update in updates.iter() {
            let value = self.column_value(&update.value, float, alias, true, &mut serial, out);
            out.push_str(&format!(
                "_mm_storeu_{suffix}(zn_soa_base_{alias}_{}+zn_i,{value});\n",
                update.column
            ));
        }
        out.push_str(&format!("}}for(;zn_i<zn_soa_len_{alias};zn_i++){{\n"));
        for update in updates.iter() {
            let value = self.column_value(&update.value, float, alias, false, &mut serial, out);
            out.push_str(&format!(
                "zn_soa_base_{alias}_{}[zn_i]={value};\n",
                update.column
            ));
        }
        out.push_str("}}\n");
        true
    }
    fn ordered_reduction(&self, plan: &ColumnLoop, reduction: &ColumnReduction, out: &mut String) {
        assert!(reduction.preserve_order);
        let owner = self.place(&plan.owner);
        let accumulator = self.place(&reduction.accumulator);
        let ty = reduction.element.ty;
        let op = &reduction.operator;
        let element = "zn_reduce_value";
        let update = if matches!(ty, Type::Float(_)) {
            format!("({accumulator}) {op} {element}")
        } else if ty == Type::Int {
            let name = match op.as_str() {
                "+" => "zn_add",
                "-" => "zn_sub",
                _ => "zn_mul",
            };
            format!("{}({accumulator},{element})", self.arithmetic(name))
        } else {
            numeric_result(
                format!(
                    "zn_num_binary((uint64_t)({accumulator}),(uint64_t){element},{},{})",
                    ty.integer().unwrap().code(),
                    crate::numeric::opcode(op)
                ),
                ty,
            )
        };
        let data = if matches!(plan.owner.ty, Type::VectorView(_)) {
            format!(
                "(zn_reduce_len?zn_v028_span_slot(&{owner},0,sizeof({})):NULL)",
                ty.c()
            )
        } else {
            format!("{owner}.data")
        };
        // A read-only pointer deliberately has no restrict: the accumulator
        // might legally be a distinct field in the same borrowed record.
        out.push_str(&format!("/* ordered contiguous scalar reduction: checked traps and IEEE order */\n{{size_t zn_reduce_len={owner}.len;const {}* zn_reduce_data=(const {}*){data};for(size_t zn_reduce_i=0;zn_reduce_i<zn_reduce_len;zn_reduce_i++){{{} {element}=zn_reduce_data[zn_reduce_i];{accumulator}={update};}}}}\n",ty.c(),ty.c(),ty.c()));
    }
    fn value(&self, v: &Rvalue, ty: Type) -> String {
        let p = |v: &Place| self.place(v);
        match v {
            Rvalue::Int(n) => format!("INT64_C({n})"),
            Rvalue::Float(n) => format!("zn_f64_value(UINT64_C({n}))"),
            Rvalue::Bool(b) => b.to_string(),
            Rvalue::IntRange { value, low, high } => format!(
                "(((uint64_t)({}) - UINT64_C({})) <= UINT64_C({}))",
                p(value),
                *low as u64,
                (*high as i128 - *low as i128) as u64
            ),
            Rvalue::Text(s) => text_literal(s, self.large_literals),
            Rvalue::Zero => ty.zero(),
            Rvalue::Region { owner, start, end } => {
                if ty == Type::TextView {
                    return format!("zn_text_view_make({},{},{})", p(owner), p(start), p(end));
                }
                if let Type::VectorView(id) = ty {
                    let width = self
                        .types
                        .layout(self.types.vector_element(Type::Named(id)).unwrap())
                        .size;
                    return format!(
                        "zn_vector_view_make({}.data,{}.len,{},{},{width})",
                        p(owner),
                        p(owner),
                        p(start),
                        p(end)
                    );
                }
                let (data, len, number) = self.buffer(owner);
                format!(
                    "zn_span_make({data},{len},{},{},{})",
                    p(start),
                    p(end),
                    number.bits() / 8
                )
            }
            Rvalue::Element { owner, index } => {
                if self.types.map_types(owner.ty).is_some()
                    || self.types.arena_element(owner.ty).is_some()
                {
                    format!("{}_get(&{}, {})", owner.ty.c(), p(owner), p(index))
                } else if let Some(k) = self.fast_view(owner) {
                    // M5 validated body: one unsigned compare, none if proven.
                    if self.view_proven(k, index) {
                        format!("(zn_v110_data_{k}+(uint64_t)({}))", p(index))
                    } else {
                        format!(
                            "(({}*)zn_v110_span_slot_fast(zn_v110_data_{k},zn_v110_len_{k},{},sizeof({})))",
                            ty.c(),
                            p(index),
                            ty.c()
                        )
                    }
                } else if matches!(owner.ty, Type::VectorView(_)) {
                    format!(
                        "({}*)zn_v028_span_slot(&{}, {}, sizeof({}))",
                        ty.c(),
                        p(owner),
                        p(index),
                        ty.c()
                    )
                } else {
                    format!(
                        "({}*)zn_v028_vec_slot(&{}, {}, sizeof({}))",
                        ty.c(),
                        p(owner),
                        p(index),
                        ty.c()
                    )
                }
            }
            Rvalue::Reference(source) => format!("&{}", p(source)),
            Rvalue::Repeat(value, _) => format!("{}_fill((uint64_t){})", ty.c(), p(value)),
            Rvalue::Convert(value)
                if matches!(value.ty, Type::Float(_)) || matches!(ty, Type::Float(_)) =>
            {
                // 1.0 B: conversions that cannot fault are the exact C
                // expressions `zn_fp_convert` evaluates (int64/uint64 source
                // to float, float to float), inlined without the bit round trip.
                if let Type::Float(target) = ty {
                    let source = match value.ty {
                        Type::Float(_) => Some(""),
                        Type::Int => Some("(int64_t)"),
                        Type::Number(n) if n.signed() => Some("(int64_t)"),
                        Type::Number(_) => Some("(uint64_t)"),
                        _ => None,
                    };
                    if let Some(source) = source {
                        return format!("(({}){source}({}))", target.c(), p(value));
                    }
                }
                self.number_from_bits(
                    format!(
                        "zn_fp_convert({},{},{})",
                        self.number_bits(value),
                        crate::numeric::type_code(value.ty),
                        crate::numeric::type_code(ty)
                    ),
                    ty,
                )
            }
            Rvalue::Convert(value)
                if {
                    // Contract S3: the whole proven source range lies inside
                    // the target type, so the value-preserving C conversion
                    // is exact and the range fault is unreachable.
                    let (low, high) = self.proven_range(value);
                    let (min, max) = integer_range(ty.integer().unwrap());
                    min <= low && high <= max
                } =>
            {
                format!("({})({})", ty.c(), p(value))
            }
            Rvalue::Convert(value) => numeric_result(
                format!(
                    "zn_num_cast((uint64_t){},{},{})",
                    p(value),
                    value.ty.integer().unwrap().code(),
                    ty.integer().unwrap().code()
                ),
                ty,
            ),
            Rvalue::Copy(v) => p(v),
            Rvalue::Record(fields) => format!(
                "({}){{ {} }}",
                ty.c(),
                fields.iter().map(p).collect::<Vec<_>>().join(", ")
            ),
            Rvalue::Variant { .. } => ty.zero(),
            Rvalue::Unary(op, v) => {
                if op == "!" {
                    format!("!{}", p(v))
                } else if matches!(ty, Type::Float(_)) {
                    format!("-({})", p(v))
                } else if matches!(ty, Type::Number(_)) {
                    numeric_result(
                        format!(
                            "zn_num_binary(0,(uint64_t){},{},1)",
                            p(v),
                            ty.integer().unwrap().code()
                        ),
                        ty,
                    )
                } else {
                    format!("{}({})", self.arithmetic("zn_neg"), p(v))
                }
            }
            Rvalue::Binary(op, a, b) => {
                if matches!(a.ty, Type::Float(_)) {
                    return if op == "%" {
                        self.number_from_bits(
                            format!(
                                "zn_fp_remainder({},{},{})",
                                self.number_bits(a),
                                self.number_bits(b),
                                crate::numeric::type_code(a.ty)
                            ),
                            ty,
                        )
                    } else {
                        format!("({} {op} {})", p(a), p(b))
                    };
                }
                if a.ty == Type::Int && b.ty == Type::Int && (op == "/" || op == "%") {
                    // Contract S2: proven non-negative dividend and an exact
                    // power-of-two divisor; every other case stays checked.
                    let (dividend, divisor) = (self.bounds(a), self.bounds(b));
                    if dividend.low >= 0
                        && divisor.low == divisor.high
                        && divisor.low > 0
                        && divisor.low & (divisor.low - 1) == 0
                    {
                        let k = divisor.low.trailing_zeros();
                        return match (op.as_str(), k) {
                            ("%", 0) => "INT64_C(0)".into(),
                            ("%", _) => format!("({} & INT64_C({}))", p(a), divisor.low - 1),
                            (_, 0) => p(a),
                            _ => format!("({} >> {k})", p(a)),
                        };
                    }
                }
                let versioned = self.fast.get().is_some_and(|id| {
                    self.versions
                        .get(&id)
                        .is_some_and(|plan| plan.ops.contains(&(v as *const Rvalue as usize)))
                });
                if a.ty == Type::Int
                    && b.ty == Type::Int
                    && ["+", "-", "*", "*literal"].contains(&op.as_str())
                    && (versioned || self.bounds(a).arithmetic(op, self.bounds(b)).1)
                {
                    // `versioned`: this is the fast copy of a loop whose guard
                    // (evaluated once before the loop) proves this result fits.
                    // 1.0 B: every mathematical result for the proven operand
                    // ranges fits i64, so the overflow fault is unreachable and
                    // plain C signed arithmetic is exact (no UB).
                    let op = if op == "*literal" { "*" } else { op.as_str() };
                    return format!("({} {op} {})", p(a), p(b));
                }
                let helper = match op.as_str() {
                    "+" => "zn_add",
                    "-" => "zn_sub",
                    "*" => "zn_mul",
                    "*literal" => "zn_mul_literal",
                    "/" => "zn_div",
                    "%" => "zn_mod",
                    _ => "",
                };
                if helper.is_empty() {
                    format!("{} {op} {}", p(a), p(b))
                } else if matches!(a.ty, Type::Number(_)) {
                    numeric_result(
                        format!(
                            "zn_num_binary((uint64_t){},(uint64_t){},{},{})",
                            p(a),
                            p(b),
                            a.ty.integer().unwrap().code(),
                            crate::numeric::opcode(op)
                        ),
                        ty,
                    )
                } else {
                    format!("{}({}, {})", self.arithmetic(helper), p(a), p(b))
                }
            }
            Rvalue::Call { name, args, .. } if self.externals.contains_key(name) => {
                format!(
                    "{}({})",
                    self.externals[name].external_symbol.as_ref().unwrap(),
                    self.external_args(args).join(", ")
                )
            }
            // 1.2 B (contract N8): inside the validated body, a self-call that
            // lends each validated view onward unchanged enters the validated
            // body directly with the cached descriptors. Proof: the views are
            // pinned (M5/L3: no function replaces a borrowed view parameter),
            // so the callee would read these same `(data, len)` values, its
            // entry test would pass exactly as ours did, and it would run its
            // validated body with these cached values.
            Rvalue::Call {
                name,
                args,
                spawn: false,
            } if self.view_fast.get()
                && self.self_fast.is_some()
                && *name == self.f.source.name
                && fast_self_call(args, &self.view_versions) =>
            {
                format!(
                    "zn_v120_fast_{name}({})",
                    self.fast_call_args(args).join(", ")
                )
            }
            Rvalue::Call { name, args, spawn } => format!(
                "zn_{}_{name}({})",
                if *spawn { "spawn" } else { "fn" },
                args.iter()
                    .map(|a| match a {
                        Argument::Value(v) => p(v),
                        Argument::Reference(v) => format!("&{}", p(v)),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Rvalue::Builtin { name, args } => {
                let a = args.iter().map(p).collect::<Vec<_>>();
                if let Some(op) = integer_intrinsic(name) {
                    let number = ty.integer().unwrap();
                    // Width-specialized inline helper: the operands already
                    // have the exact checked type T (shift counts are int).
                    let shaped = match op {
                        6 => args.len() == 1,
                        7..=10 => args.len() == 2 && args[1].ty == Type::Int,
                        _ => args.len() == 2 && args[1].ty.integer() == Some(number),
                    };
                    if shaped && args[0].ty.integer() == Some(number) {
                        return format!(
                            "zn_v029_{}_{}({})",
                            V029_INTRINSICS[op as usize],
                            number.name(),
                            a.join(",")
                        );
                    }
                    let bits = number.bits();
                    let second = a.get(1).map(String::as_str).unwrap_or("0");
                    let raw = format!(
                        "zn_v028_intrinsic((uint64_t){},(uint64_t){second},{bits},{},{op})",
                        a[0],
                        number.signed()
                    );
                    return if number.signed() {
                        format!("({})zn_v028_wrap_signed({raw},{bits})", ty.c())
                    } else {
                        format!("({})({raw})", ty.c())
                    };
                }
                if name == "process_run" {
                    return format!(
                        "zn_process_run({},(const zn_text*){}.data,{}.len)",
                        a[0], a[1], a[1]
                    );
                }
                if let Some(code) = frontier3_builtin(name, args, &a) {
                    return code;
                }
                if let Some(code) = self.os_builtin(name, args, &a) {
                    return code;
                }
                if name.starts_with("channel_") {
                    return match name.as_str() {
                        "channel_new" => format!("{}_new({})", ty.c(), a[0]),
                        "channel_send" => format!("zn_channel_send(&{}, &{})", a[0], a[1]),
                        "channel_recv" | "channel_try_recv" => format!(
                            "zn_channel_recv(&{}, &{}, {})",
                            a[0],
                            a[1],
                            name == "channel_recv"
                        ),
                        _ => format!("zn_{name}(&{})", a[0]),
                    };
                }
                if args
                    .first()
                    .is_some_and(|p| matches!(p.ty, Type::Fixed(..) | Type::Slice(_)))
                {
                    let (data, len, number) = self.buffer(&args[0]);
                    if let Some(value) = self.inline_buffer_access(name, args, &a, ty) {
                        return value;
                    }
                    let code = number.code();
                    let fixed = match name.as_str() {
                        "fixed_len" => Some(format!("zn_length({len})")),
                        "fixed_at" => Some(numeric_result(
                            format!("zn_v028_fixed_at({data},{len},{},{code})", a[1]),
                            ty,
                        )),
                        "fixed_set" | "fixed_try_set" => Some(format!(
                            "zn_v028_{name}({data},{len},{},{code},(uint64_t){})",
                            a[1], a[2]
                        )),
                        n if n.starts_with("read_u") => Some(format!(
                            "({}){}({data},{len},{},{})",
                            ty.c(),
                            if n.ends_with("_be") {
                                "zn_v028_wire_read_be"
                            } else {
                                "zn_wire_read"
                            },
                            a[1],
                            ty.integer().unwrap().bits() / 8
                        )),
                        // 1.2 B (contract N2): the runtime's big-endian
                        // write takes the value BEFORE the width.
                        n if n.starts_with("try_write_u") && n.ends_with("_be") => {
                            // The width comes from the builtin's name.
                            let bits = n["try_write_u".len()..n.len() - "_be".len()]
                                .parse::<usize>()
                                .unwrap_or(0);
                            Some(format!(
                                "zn_wire_write_be({data},{len},{},(uint64_t){},{})",
                                a[1],
                                a[2],
                                bits / 8
                            ))
                        }
                        n if n.starts_with("try_write_u") => Some(format!(
                            "zn_wire_write({data},{len},{},{},(uint64_t){})",
                            a[1],
                            args[2].ty.integer().unwrap().bits() / 8,
                            a[2]
                        )),
                        _ => None,
                    };
                    if let Some(value) = fixed {
                        return value;
                    }
                }
                if name.starts_with("arena_") {
                    let c = args[0].ty.c();
                    return match name.as_str() {
                        "arena_len" => format!("zn_length({}.len)", a[0]),
                        "arena_contains_id" => {
                            format!("zn_v028_arena_contains(&{}, {})", a[0], a[1])
                        }
                        "arena_alloc" => format!("{c}_alloc(&{}, {})", a[0], a[1]),
                        "arena_free" => format!("{c}_free(&{}, {})", a[0], a[1]),
                        "arena_clone_get" => format!("{c}_clone_get(&{}, {})", a[0], a[1]),
                        _ => unreachable!("checked arena operation"),
                    };
                }
                if name.starts_with("map_") {
                    let c = args[0].ty.c();
                    return match name.as_str() {
                        "map_len" => format!("zn_length({}.len)", a[0]),
                        "map_contains_key" => format!("{c}_contains_key(&{}, {})", a[0], a[1]),
                        "map_insert" => format!("{c}_insert(&{}, {}, {})", a[0], a[1], a[2]),
                        "map_remove" => format!("{c}_remove(&{}, {})", a[0], a[1]),
                        "map_clone_get" => format!("{c}_clone_get(&{}, {})", a[0], a[1]),
                        "map_keys" => {
                            let text_keys = self
                                .types
                                .map_types(args[0].ty)
                                .is_some_and(|(key, _)| key == Type::Text);
                            let kind = if text_keys { "text" } else { "int" };
                            format!("zn_map_keys_{kind}(&{}, sizeof({c}_entry))", a[0])
                        }
                        _ => unreachable!("unknown checked map builtin"),
                    };
                }
                if let Some((k, Type::VectorView(id))) =
                    args.first().and_then(|v| Some((self.fast_view(v)?, v.ty)))
                {
                    // M5 validated body: same element, clone and fault.
                    let element = self.fast_element(k);
                    let e = element.c();
                    match name.as_str() {
                        "vec_at" | "vec_clone_at" => {
                            let slot = if self.view_proven(k, &args[1]) {
                                format!("zn_v110_data_{k}[(uint64_t)({})]", a[1])
                            } else {
                                format!(
                                    "(*({e}*)zn_v110_span_slot_fast(zn_v110_data_{k},zn_v110_len_{k},{},sizeof({e})))",
                                    a[1]
                                )
                            };
                            return if element.owned() {
                                format!("{e}_clone({slot})")
                            } else {
                                slot
                            };
                        }
                        "vec_replace" => {
                            return format!(
                                "zn_type_{id}_view_replace_v110(zn_v110_data_{k},zn_v110_len_{k},{},{})",
                                a[1], a[2]
                            )
                        }
                        "len" => return format!("((int64_t)zn_v110_len_{k})"),
                        _ => {}
                    }
                }
                if name.starts_with("vec_") {
                    let c = if let Type::VectorView(id) = args[0].ty {
                        format!("zn_type_{id}_view")
                    } else {
                        args[0].ty.c()
                    };
                    return match name.as_str() {
                        "vec_capacity" => format!("zn_length({}.cap)", a[0]),
                        "vec_at" | "vec_clone_at" => format!("{c}_clone_at(&{}, {})", a[0], a[1]),
                        "vec_push" => format!("{c}_push(&{}, {})", a[0], a[1]),
                        "vec_pop" => format!("{c}_pop(&{})", a[0]),
                        "vec_replace" => format!("{c}_replace(&{}, {}, {})", a[0], a[1], a[2]),
                        _ => unreachable!(),
                    };
                }
                match name.as_str() {
                    "sqrt" | "abs" => self.number_from_bits(
                        format!(
                            "zn_fp_{name}({},{})",
                            self.number_bits(&args[0]),
                            crate::numeric::type_code(args[0].ty)
                        ),
                        ty,
                    ),
                    "is_finite" | "is_nan" => format!(
                        "zn_fp_{name}({},{})",
                        self.number_bits(&args[0]),
                        crate::numeric::type_code(args[0].ty)
                    ),
                    "print_float" | "float_text" => format!(
                        "zn_{name}({},{})",
                        self.number_bits(&args[0]),
                        crate::numeric::type_code(args[0].ty)
                    ),
                    "saturating_add" | "saturating_sub" => numeric_result(
                        format!(
                            "zn_num_binary((uint64_t){},(uint64_t){},{},{})",
                            a[0],
                            a[1],
                            ty.integer().unwrap().code(),
                            crate::numeric::opcode(name)
                        ),
                        ty,
                    ),
                    "len" if args[0].ty == Type::Text => {
                        format!("zn_length(zn_text_size({}))", a[0])
                    }
                    "len" => format!("zn_length({}.len)", a[0]),
                    "at" | "push" => format!("zn_{name}(&{}, {})", a[0], a[1]),
                    "texts_push" => format!("zn_texts_push(&{}, {})", a[0], a[1]),
                    "set" => format!("zn_set(&{}, {}, {})", a[0], a[1], a[2]),
                    "clone" => format!("{}_clone({})", ty.c(), a[0]),
                    _ => format!("zn_{name}({})", a.join(", ")),
                }
            }
        }
    }
    /// A loop condition computed only by pure, non-faulting definitions of
    /// temporaries that nothing outside the condition reads, as one C
    /// expression. Substitution cannot reorder side effects or faults.
    fn pure_condition(&self, condition_code: &[Operation], condition: &Place) -> Option<String> {
        let mut local_uses = vec![0usize; self.uses.len()];
        count_uses(condition_code, &mut local_uses);
        local_uses[condition.local] += 1;
        let scalar = |ty: Type| {
            matches!(
                ty,
                Type::Int | Type::Number(_) | Type::Float(_) | Type::Bool | Type::Id(_)
            )
        };
        let mut expressions = BTreeMap::new();
        for op in condition_code {
            let Operation::Init(temporary, value) = op else {
                return None;
            };
            let pure = match value {
                Rvalue::Copy(_)
                | Rvalue::Int(_)
                | Rvalue::Bool(_)
                | Rvalue::Float(_)
                | Rvalue::IntRange { .. } => true,
                Rvalue::Unary(op, _) => op == "!",
                Rvalue::Binary(op, a, b) => {
                    ["<", "<=", ">", ">=", "==", "!="].contains(&op.as_str())
                        && scalar(a.ty)
                        && scalar(b.ty)
                }
                _ => false,
            };
            if !pure
                || !temporary.fields.is_empty()
                || !scalar(temporary.ty)
                || self.f.locals[temporary.local].borrow != Borrow::Owned
                || self.direct_soa_aliases.contains(&temporary.local)
                || self.uses[temporary.local] != local_uses[temporary.local]
            {
                return None;
            }
            *self.substitutions.borrow_mut() = expressions.clone();
            let expression = self.value(value, temporary.ty);
            self.substitutions.borrow_mut().clear();
            expressions.insert(temporary.local, format!("({expression})"));
        }
        if condition.fields.is_empty() {
            expressions.remove(&condition.local)
        } else {
            None
        }
    }
    /// One loop. A pure condition becomes the `while` header (the shape
    /// MSVC's loop optimizer recognizes); labels are emitted only when a
    /// `break`/`continue` targets them. `continue` jumps to the end of the
    /// body, which re-evaluates the condition exactly like the loop head.
    fn emit_loop<'o>(
        &self,
        op: &'o Operation,
        column_scope: bool,
        out: &mut String,
        pending: &mut Vec<Emission<'o>>,
    ) {
        let Operation::Loop {
            id,
            condition_code,
            condition,
            body,
        } = op
        else {
            unreachable!()
        };
        let (breaks, continues) = {
            let (a, b) = loop_targets(body, *id);
            let (c, d) = loop_targets(condition_code, *id);
            (a || c, b || d)
        };
        let close = if column_scope { "}\n" } else { "" };
        let label = self.loop_label(*id);
        let end = if breaks {
            format!("{label}_end:;\n")
        } else {
            String::new()
        };
        if let Some(header) = self.pure_condition(condition_code, condition) {
            out.push_str(&format!("while ({header}) {{\n"));
            pending.push(Emission::Text(format!(
                "{}}}\n{end}{close}",
                if continues {
                    format!("{label}_head:;\n")
                } else {
                    String::new()
                }
            )));
            pending.push(Emission::Operations(body));
            return;
        }
        out.push_str("for (;;) {\n");
        if continues {
            out.push_str(&format!("{label}_head:;\n"));
        }
        pending.push(Emission::Text(format!("}}\n{end}{close}")));
        pending.push(Emission::Operations(body));
        pending.push(Emission::Text(format!(
            "if (!{}) break;\n",
            self.place(condition)
        )));
        pending.push(Emission::Operations(condition_code));
    }
    /// Loop-invariant guard of a versioned loop; opens `if (fast) {`.
    fn version_guard(&self, id: usize, plan: &VersionPlan) -> String {
        let mut guard = format!(
            "/* loop {id} versioned: invariant overflow guards hoisted */\n{{bool zn_fast_{id}=true;\n"
        );
        for (terms, low, high) in &plan.checks {
            guard.push_str("{int64_t zn_sum=0;");
            for (local, sign) in terms {
                guard.push_str(&format!(
                    "zn_sum=zn_v100_term(zn_sum,{},{sign},&zn_fast_{id});",
                    self.place(&Place::local(*local, Type::Int))
                ));
            }
            guard.push_str(&format!(
                "zn_fast_{id}=zn_fast_{id}&&zn_v100_fits(zn_sum,{},{});}}\n",
                c_int(*low),
                c_int(*high)
            ));
        }
        guard.push_str(&format!("if (zn_fast_{id}) {{\n"));
        guard
    }
    /// 1.2 B (contract N8): local view `k` was just defined by `region`; the
    /// rest of its block never replaces the descriptor (`local_view_plans`).
    /// One test of M5's runtime invariant selects a copy of `rest` that caches
    /// `(data, len)`, with M5's one-compare accesses (none when proven); the
    /// other copy is the unchanged lowering. The copy is a subset of the
    /// executions, so every flow fact used there holds.
    fn version_local<'o>(
        &self,
        k: usize,
        element: Type,
        region: &Rvalue,
        rest: &'o [Operation],
        out: &mut String,
        pending: &mut Vec<Emission<'o>>,
    ) {
        let Rvalue::Region { end, .. } = region else {
            unreachable!("local views are defined by regions")
        };
        // `zn_vector_view_region` returns `len = count` (or faults), and the
        // descriptor is constant afterwards: len(view) >= the count's lower
        // bound at this read.
        let min_len = self.bounds(end).low.max(0);
        let rest_pending = pending.pop();
        debug_assert!(matches!(rest_pending, Some(Emission::Operations(_))));
        let mut loops = BTreeSet::new();
        loop_ids(rest, &mut loops);
        *self.local_loops.borrow_mut() = loops;
        let place = self.place(&Place::local(k, self.f.locals[k].ty));
        let e = element.c();
        out.push_str(&format!(
            "/* 1.2 local VectorView versioning (contract N8): descriptor constant for the rest of its block */\nif (zn_v110_view_valid(&{place},{})) {{\n{e}* const zn_v110_data_{k}=({e}*){place}.data;\nconst uint64_t zn_v110_len_{k}={place}.len;\n",
            self.types.layout(element).size
        ));
        self.in_local.set(true);
        self.active_local.set(Some((k, min_len, element)));
        pending.push(Emission::Text("}\n".into()));
        pending.push(Emission::LocalDone);
        pending.push(Emission::Operations(rest));
        pending.push(Emission::Text("} else {\n".into()));
        pending.push(Emission::LocalView(None));
        pending.push(Emission::Operations(rest));
    }
    fn block(&self, ops: &[Operation], out: &mut String) {
        // Backend arms can need substantial formatting scratch space. Keep
        // nested source control flow on the heap so every false alternative
        // does not reserve another complete emitter frame on the main stack.
        let mut pending = vec![Emission::Operations(ops)];
        while let Some(next) = pending.pop() {
            let operations = match next {
                Emission::Text(text) => {
                    out.push_str(&text);
                    continue;
                }
                Emission::Fast(id) => {
                    self.fast.set(id);
                    continue;
                }
                Emission::Loop(op) => {
                    self.emit_loop(op, false, out, &mut pending);
                    continue;
                }
                Emission::LocalView(view) => {
                    self.active_local.set(view);
                    continue;
                }
                Emission::LocalDone => {
                    self.in_local.set(false);
                    self.local_loops.borrow_mut().clear();
                    continue;
                }
                Emission::Operations(operations) => operations,
            };
            let Some((op, rest)) = operations.split_first() else {
                continue;
            };
            if !rest.is_empty() {
                pending.push(Emission::Operations(rest));
            }
            let p = |v: &Place| self.place(v);
            match op {
                Operation::Init(dst, v) => {
                    if let Rvalue::Call { name, args, .. } = v {
                        if let Some(f) = self.externals.get(name) {
                            if matches!(dst.ty, Type::Fixed(..)) {
                                let mut args = self.external_args(args);
                                args.insert(0, format!("{}.data", p(dst)));
                                out.push_str(&format!(
                                    "{} {} = {{0}};\n{}({});\n",
                                    dst.ty.c(),
                                    p(dst),
                                    f.external_symbol.as_ref().unwrap(),
                                    args.join(", ")
                                ));
                                continue;
                            }
                        }
                    }
                    if let Rvalue::Element { owner, index } = v {
                        if self.types.soa_element(owner.ty).is_some() {
                            if self.direct_soa_aliases.contains(&dst.local) {
                                let hoisted = self
                                    .column_loops
                                    .values()
                                    .any(|plan| plan.alias == dst.local);
                                let length = if hoisted {
                                    format!("zn_soa_len_{}", dst.local)
                                } else {
                                    format!("{}.len", p(owner))
                                };
                                out.push_str(&format!("if((uint64_t)({}) >= {length})zn_fail(\"vector index out of bounds\");\n",p(index)));
                                let Type::Named(record) = dst.ty else {
                                    unreachable!()
                                };
                                let DataKind::Record(fields) = &self.types.definitions[record].kind
                                else {
                                    unreachable!()
                                };
                                let mut offset = "0".to_string();
                                for (column, field) in fields.iter().enumerate() {
                                    let width = self.types.layout(field.ty).size;
                                    if hoisted {
                                        out.push_str(&format!("{}{}* zn_column_{}_{column}=zn_soa_base_{}_{column}+(uint64_t)({});\n",if self.f.locals[dst.local].borrow==Borrow::Shared {"const "}else{""},field.ty.c(),dst.local,dst.local,p(index)));
                                        continue;
                                    }
                                    out.push_str(&format!("{}{}* zn_column_{}_{column}=({}{}*)((char*){}.data+({offset})+(uint64_t)({})*{width});\n",if self.f.locals[dst.local].borrow==Borrow::Shared {"const "}else{""},field.ty.c(),dst.local,if self.f.locals[dst.local].borrow==Borrow::Shared {"const "}else{""},field.ty.c(),p(owner),p(index)));
                                    offset = format!(
                                        "{offset}+(({}.cap*{width}+7)&~(size_t)7)",
                                        p(owner)
                                    );
                                }
                                continue;
                            }
                            out.push_str(&format!("{} zn_projection_{} = {}_load(&{}, {});\n{}{}* z{} = &zn_projection_{};\n",dst.ty.c(),dst.local,owner.ty.c(),p(owner),p(index),if self.f.locals[dst.local].borrow==Borrow::Shared {"const "} else {""},dst.ty.c(),dst.local,dst.local));
                            continue;
                        }
                    }
                    if let Rvalue::Builtin { name, args } = v {
                        if name == "join" {
                            let (address, width) = if dst.ty == Type::Unit {
                                ("NULL".into(), "0".into())
                            } else {
                                out.push_str(&format!(
                                    "{} {} = {};\n",
                                    dst.ty.c(),
                                    p(dst),
                                    dst.ty.zero()
                                ));
                                (format!("&{}", p(dst)), format!("sizeof({})", dst.ty.c()))
                            };
                            out.push_str(&format!(
                                "zn_join_owned({address}, {}, {width}); {} = NULL;\n",
                                p(&args[0]),
                                p(&args[0])
                            ));
                            continue;
                        }
                    }
                    let value = self.value(v, dst.ty);
                    if matches!(v, Rvalue::Element { .. } | Rvalue::Reference(_)) {
                        out.push_str(&format!(
                            "{}{}* z{} = {value};\n",
                            if self.f.locals[dst.local].borrow == Borrow::Shared {
                                "const "
                            } else {
                                ""
                            },
                            dst.ty.c(),
                            dst.local
                        ));
                        continue;
                    }
                    out.push_str(&if dst.ty == Type::Unit {
                        format!("{value};\n")
                    } else {
                        format!("{} {} = {value};\n", dst.ty.c(), p(dst))
                    });
                    if let Some(&(k, element)) =
                        self.local_views.get(&(op as *const Operation as usize))
                    {
                        if !self.in_local.get() && !self.fallback.get() {
                            self.version_local(k, element, v, rest, out, &mut pending);
                            continue;
                        }
                    }
                    if let Rvalue::Variant { index, payload } = v {
                        out.push_str(&format!("{}.tag = {};\n", p(dst), index + 1));
                        if let Some(v) = payload {
                            out.push_str(&format!("{}.data.v{index} = {};\n", p(dst), p(v)));
                        }
                    }
                }
                Operation::Move {
                    destination,
                    source,
                    initialize,
                } => out.push_str(&format!(
                    "{}{} = {}; {} = {};\n",
                    if *initialize {
                        format!("{} ", destination.ty.c())
                    } else {
                        String::new()
                    },
                    p(destination),
                    p(source),
                    p(source),
                    source.ty.zero()
                )),
                Operation::Assign(a, b) => out.push_str(&format!("{} = {};\n", p(a), p(b))),
                Operation::Swap(a, b) => out.push_str(&format!(
                    "{{ {} zn_swap_value={};{}={};{}=zn_swap_value; }}\n",
                    a.ty.c(),
                    p(a),
                    p(a),
                    p(b),
                    p(b)
                )),
                Operation::ElementCheckPair { owner, left, right }
                    if self.fast_view(owner).is_some() =>
                {
                    // M5 validated body: zn_span_check_pair's checks, message
                    // and order inline; proven indices keep only distinctness.
                    let k = owner.local;
                    out.push_str(
                        &if self.view_proven(k, left) && self.view_proven(k, right) {
                            format!(
                            "if(({})==({}))zn_fail(\"vector pair indices must be distinct\");\n",
                            p(left),
                            p(right)
                        )
                        } else {
                            format!(
                                "zn_v110_span_check_pair(zn_v110_len_{k},{},{});\n",
                                p(left),
                                p(right)
                            )
                        },
                    );
                }
                Operation::ElementCheckPair { owner, left, right } => out.push_str(&format!(
                    "{}(&{}, {}, {});\n",
                    if self.types.arena_element(owner.ty).is_some() {
                        "zn_arena_check_pair"
                    } else if matches!(owner.ty, Type::VectorView(_)) {
                        "zn_span_check_pair"
                    } else {
                        "zn_vec_check_pair"
                    },
                    p(owner),
                    p(left),
                    p(right)
                )),
                Operation::ElementWriteback {
                    alias,
                    owner,
                    index,
                } => {
                    if !self.direct_soa_aliases.contains(&alias.local) {
                        out.push_str(&format!(
                            "{}_store(&{}, {}, &{});\n",
                            owner.ty.c(),
                            p(owner),
                            p(index),
                            p(alias)
                        ));
                    }
                }
                Operation::Drop(v) => out.push_str(&format!("{}(&{});\n", v.ty.drop_fn(), p(v))),
                Operation::Loan { place, kind, begin } => out.push_str(&format!(
                    "/* loan {:?} {} {} */\n",
                    kind,
                    if *begin { "begin" } else { "end" },
                    p(place)
                )),
                Operation::Assert {
                    value,
                    line,
                    message,
                } => out.push_str(&format!(
                    // Same call and fault output; the passing case stays inline.
                    "if (!{}) zn_assert(false, {line}, {});\n",
                    p(value),
                    c_string(message)
                )),
                Operation::If { condition, yes, no } => {
                    out.push_str(&format!("if ({}) {{\n", p(condition)));
                    pending.push(Emission::Text("}\n".into()));
                    pending.push(Emission::Operations(no));
                    pending.push(Emission::Text("} else {\n".into()));
                    pending.push(Emission::Operations(yes));
                }
                Operation::Loop { id, .. } => {
                    let mut column_scope = false;
                    if let Some(plan) = self.column_loops.get(id) {
                        if let Some(ColumnSimd::Reduction(reduction)) = &plan.simd {
                            self.ordered_reduction(plan, reduction, out);
                            continue;
                        }
                        column_scope = true;
                        // Restrict's C block lifetime exactly matches this
                        // checked lexical owner loan, including scalar fallback.
                        out.push_str("{\n");
                        let alias = plan.alias;
                        let owner = p(&plan.owner);
                        let Type::Named(record) = self.types.soa_element(plan.owner.ty).unwrap()
                        else {
                            unreachable!()
                        };
                        let DataKind::Record(fields) = &self.types.definitions[record].kind else {
                            unreachable!()
                        };
                        out.push_str(&format!("/* invariant SoA columns and natural strides */\nsize_t zn_soa_len_{alias}={owner}.len;\nsize_t zn_soa_cap_{alias}={owner}.cap;\n"));
                        let mut offset = "0".to_string();
                        for (column, field) in fields.iter().enumerate() {
                            let width = self.types.layout(field.ty).size;
                            let qualifier = if self.f.locals[alias].borrow == Borrow::Shared {
                                "const "
                            } else {
                                ""
                            };
                            out.push_str(&format!("{qualifier}{}* __restrict zn_soa_base_{alias}_{column}={owner}.data?({qualifier}{}*)((char*){owner}.data+({offset})):NULL;\n",field.ty.c(),field.ty.c()));
                            offset =
                                format!("{offset}+((zn_soa_cap_{alias}*{width}+7)&~(size_t)7)");
                        }
                        if let Some(simd) = &plan.simd {
                            if self.column_simd(simd, alias, out) {
                                out.push_str("}\n");
                                continue;
                            }
                        }
                    }
                    if let (false, None, Some(plan)) =
                        (column_scope, self.fast.get(), self.versions.get(id))
                    {
                        out.push_str(&self.version_guard(*id, plan));
                        pending.push(Emission::Text("}\n}\n".into()));
                        pending.push(Emission::Loop(op));
                        pending.push(Emission::Text("} else {\n".into()));
                        pending.push(Emission::Fast(None));
                        pending.push(Emission::Loop(op));
                        pending.push(Emission::Fast(Some(*id)));
                        continue;
                    }
                    self.emit_loop(op, column_scope, out, &mut pending);
                }
                Operation::Break(id) => {
                    out.push_str(&format!("goto {}_end;\n", self.loop_label(*id)))
                }
                Operation::Continue(id) => {
                    out.push_str(&format!("goto {}_head;\n", self.loop_label(*id)))
                }
                Operation::Return(v) => out.push_str(&format!(
                    "return{};\n",
                    v.as_ref().map(|v| format!(" {}", p(v))).unwrap_or_default()
                )),
                Operation::Switch { value, arms } => {
                    out.push_str(&format!("switch ({}.tag) {{\n", p(value)));
                    pending.push(Emission::Text(
                        "default: zn_fail(\"invalid enum tag\");\n}\n".into(),
                    ));
                    for (i, ops) in arms.iter().enumerate().rev() {
                        pending.push(Emission::Text("break; }\n".into()));
                        pending.push(Emission::Operations(ops));
                        pending.push(Emission::Text(format!("case {}: {{\n", i + 1)));
                    }
                }
            }
        }
    }
}
pub fn generate(program: &CheckedProgram, inline_functions: bool) -> Generated {
    let sources: Vec<_> = program.functions.iter().map(|f| f.source.clone()).collect();
    let externals: BTreeMap<_, _> = sources
        .iter()
        .filter(|f| f.external_symbol.is_some())
        .map(|f| (f.name.clone(), f))
        .collect();
    let workers = worker_targets(&sources);
    let inline = inline_helpers(&sources);
    let hot = hot_functions(&sources, &inline);
    let mut preamble = runtime_header(uses_builtin(program, V130_BUILTINS));
    preamble.push_str("\n#include <emmintrin.h>\n#pragma fp_contract(off)\n");
    preamble.push_str(inline_access_helpers());
    preamble.push_str(&v029_helpers());
    preamble.push_str(v100_helpers());
    preamble.push_str(v110_helpers());
    if uses_be_writes(program) {
        preamble.push_str(v120_wire_helpers());
    }
    // Byte initializers bypass MSVC's string-literal limit and have static
    // lifetime, including a trailing NUL for supported C pointer access.
    let mut large_literals = BTreeMap::new();
    for f in &program.functions {
        let mut pending: Vec<_> = f.body.iter().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Init(_, Rvalue::Text(s)) if s.len() > 1024 => {
                    let next = large_literals.len();
                    large_literals.entry(s.clone()).or_insert(next);
                }
                Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => pending.extend(condition_code.iter().chain(body)),
                Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
                _ => {}
            }
        }
    }
    for (s, id) in &large_literals {
        preamble.push_str(&format!("static const char zn_v028_text_{id}[]={{"));
        for b in s.bytes() {
            preamble.push_str(&format!("(char){b},"));
        }
        preamble.push_str("0};\n");
    }
    let mut arrays = BTreeSet::new();
    for f in &program.functions {
        arrays.extend(
            f.locals
                .iter()
                .map(|l| l.ty)
                .filter(|t| matches!(t, Type::Fixed(..))),
        );
        if matches!(f.source.ret, Type::Fixed(..)) {
            arrays.insert(f.source.ret);
        }
    }
    for def in &program.types.definitions {
        let fields = match &def.kind {
            DataKind::Record(v)
            | DataKind::Enum(v)
            | DataKind::Vector(v)
            | DataKind::SoaVector(v)
            | DataKind::Map(v)
            | DataKind::Arena(v)
            | DataKind::Channel(v) => v,
        };
        arrays.extend(
            fields
                .iter()
                .map(|f| f.ty)
                .filter(|t| matches!(t, Type::Fixed(..))),
        );
    }
    for ty in arrays {
        let Type::Fixed(number, len) = ty else {
            unreachable!()
        };
        let c = ty.c();
        let code = number.code();
        preamble.push_str(&format!("typedef struct {{ {} data[{len}]; }} {c};\nstatic {c} {c}_fill(uint64_t v) {{ {c} out; zn_fixed_fill(&out,v,{len},{code}); return out; }}\nstatic void {c}_drop({c}* p) {{ (void)p; }}\nstatic {c} {c}_clone({c} v) {{ return v; }}\n",number.c()));
    }
    preamble.push_str(&data_declarations(
        &program.types,
        &unused_net_types(program),
    ));
    if uses_builtin(program, &["net_serve_once"]) {
        preamble.push_str(&v130_net_serve_wrapper(program));
    }
    if uses_builtin(program, &["http_request"]) {
        preamble.push_str(&v130_http_request_wrapper(program));
    }
    for f in &sources {
        if let Some(symbol) = &f.external_symbol {
            let mut args = Vec::new();
            let ret = if let Type::Fixed(n, _) = f.ret {
                args.push(format!("{}* result", n.c()));
                "void".into()
            } else {
                f.ret.c()
            };
            for (i, param) in f.params.iter().enumerate() {
                args.push(if let Type::Fixed(n, _) = param.ty {
                    format!(
                        "{}{}* p{i}",
                        if param.borrow == Borrow::Exclusive {
                            ""
                        } else {
                            "const "
                        },
                        n.c()
                    )
                } else {
                    format!("{} p{i}", param.c_type())
                });
            }
            preamble.push_str(&format!(
                "\nextern {ret} {symbol}({});\n",
                if args.is_empty() {
                    "void".into()
                } else {
                    args.join(", ")
                }
            ));
            continue;
        }
        preamble.push_str(&format!(
            "\n{}{} zn_fn_{}({});\n",
            if inline_functions && inline.contains(f.name.as_str()) {
                "__inline "
            } else {
                ""
            },
            f.ret.c(),
            f.name,
            params(f, &program.views)
        ));
        if workers.contains(f.name.as_str()) {
            preamble.push_str(&format!(
                "zn_task* zn_spawn_{}({});\n",
                f.name,
                params(f, &program.views)
            ));
        }
    }
    let mut functions = Vec::new();
    let pinned = views_pinned(program);
    let internal = internal_names(program);
    for checked in &program.functions {
        let f = &checked.source;
        if f.external_symbol.is_some() {
            continue;
        }
        let ranges = whole_function_ranges(checked);
        let (flow, tracked) = Flow::analyze(checked, &ranges);
        let column_loops = crate::ir::column_loops(checked, &program.types);
        let versions = version_plans(checked, &tracked, &flow, &ranges, &column_loops);
        let mut uses = vec![0; checked.locals.len()];
        count_uses(&checked.body, &mut uses);
        let view_versions = select_view_versions(checked, &program.types, pinned);
        let widths: BTreeMap<usize, u64> = view_versions
            .iter()
            .map(|(&k, &element)| (k, program.types.layout(element).size as u64))
            .collect();
        let (fast_flow, _, fast_proven) = if widths.is_empty() {
            Default::default()
        } else {
            Flow::analyze_views(checked, &ranges, &widths)
        };
        // 1.2 B (contract N8): a validated body that calls the function
        // itself with its validated views also becomes `zn_v120_fast_<name>`,
        // without the descriptor pointers it never needs.
        let self_fast = (!view_versions.is_empty()
            && has_fast_self_call(&checked.body, &f.name, &view_versions))
        .then(|| {
            (0..f.params.len())
                .filter(|&j| {
                    !view_versions.contains_key(&j)
                        || column_loops.values().any(|plan| plan.owner.local == j)
                        || view_pointer_used(&checked.body, j, &f.name, &view_versions)
                })
                .collect::<Vec<usize>>()
        });
        let local_views = local_view_plans(checked, &program.types, pinned, &internal);
        let w = Writer {
            f: checked,
            hot: hot.contains(f.name.as_str()),
            types: &program.types,
            externals: &externals,
            direct_soa_aliases: crate::ir::direct_soa_aliases(checked, &program.types),
            column_loops,
            large_literals: &large_literals,
            ranges,
            flow,
            versions,
            fast: std::cell::Cell::new(None),
            substitutions: std::cell::RefCell::new(BTreeMap::new()),
            uses,
            view_versions,
            view_fast: std::cell::Cell::new(false),
            fast_flow,
            fast_proven,
            label_suffix: std::cell::Cell::new(""),
            self_fast,
            local_views,
            active_local: std::cell::Cell::new(None),
            local_loops: std::cell::RefCell::new(BTreeSet::new()),
            in_local: std::cell::Cell::new(false),
            fallback: std::cell::Cell::new(false),
        };
        if let Some(kept) = &w.self_fast {
            // The validated body as a static function, emitted as its own
            // entry ahead of the wrapper (same module, so defined before use).
            let declarations = param_list(f, &program.views);
            let mut list: Vec<String> = kept.iter().map(|&j| declarations[j].clone()).collect();
            for (&k, element) in &w.view_versions {
                list.push(format!("{}* const zn_v110_data_{k}", element.c()));
                list.push(format!("const uint64_t zn_v110_len_{k}"));
            }
            let mut fast = format!(
                "\n/* 1.2 validated body of zn_fn_{} (contract N8): eligible self-calls pass the cached descriptors */\nstatic {} zn_v120_fast_{}({}) {{\n",
                f.name,
                f.ret.c(),
                f.name,
                list.join(", ")
            );
            w.view_fast.set(true);
            w.label_suffix.set("_v110");
            w.block(&checked.body, &mut fast);
            w.view_fast.set(false);
            w.label_suffix.set("");
            fast.push_str("}\n");
            functions.push((format!("{}_v120", f.name), fast));
        }
        let mut out = format!(
            "\n{}{} zn_fn_{}({}) {{\n",
            if inline_functions && inline.contains(f.name.as_str()) {
                "__inline "
            } else {
                ""
            },
            f.ret.c(),
            f.name,
            params(f, &program.views)
        );
        if w.view_versions.is_empty() {
            w.block(&checked.body, &mut out);
        } else {
            // 1.1 B (contract M5): one entry test selects the validated body
            // (cached descriptors, single unsigned compares) or the unchanged
            // checked body, which serves every other (foreign) view.
            out.push_str(&w.view_entry());
            w.view_fast.set(true);
            w.label_suffix.set("_v110");
            w.block(&checked.body, &mut out);
            w.view_fast.set(false);
            w.label_suffix.set("");
            out.push_str("} else {\n/* checked body: unchanged lowering */\n");
            w.fallback.set(true);
            w.block(&checked.body, &mut out);
            w.fallback.set(false);
            out.push_str("}\n");
        }
        out.push_str("}\n");
        if workers.contains(f.name.as_str()) {
            let fields = f
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| format!("{} a{i};", p.ty.c()))
                .collect::<String>();
            let args = (0..f.params.len())
                .map(|i| format!("p->a{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let init = (0..f.params.len())
                .map(|i| format!("p->a{i}=p{i};"))
                .collect::<String>();
            let drop = if f.ret.owned() {
                format!("{}(({}*)result);", f.ret.drop_fn(), f.ret.c())
            } else {
                String::new()
            };
            let (store, width) = if f.ret == Type::Unit {
                (format!("zn_fn_{}({args});", f.name), "0".into())
            } else {
                (
                    format!("*(({}*)result)=zn_fn_{}({args});", f.ret.c(), f.name),
                    format!("sizeof({})", f.ret.c()),
                )
            };
            out.push_str(&format!("\ntypedef struct {{ unsigned char reserved; {fields} }} zn_packet_{n};\nvoid zn_worker_{n}(void* data,void* result) {{ if(!data){{{drop}return;}} zn_packet_{n}* p=(zn_packet_{n}*)data; {store} zn_release(p); }}\nzn_task* zn_spawn_{n}({params}) {{ zn_packet_{n}* p=(zn_packet_{n}*)zn_alloc(sizeof(*p)); {init} return zn_start_owned(zn_worker_{n},p,{width}); }}\n",n=f.name,params=params(f, &program.views)));
        }
        functions.push((f.name.clone(), out));
    }
    Generated{preamble,functions,entry:"\nint wmain(int argc, wchar_t** argv) { zn_args_init(argc, argv); zn_fn_main(); return zn_process_exit_code(); }\n".into()}
}
include!("cgen_helpers.rs");
pub(crate) fn hot_names(program: &CheckedProgram) -> BTreeSet<String> {
    let sources: Vec<_> = program.functions.iter().map(|f| f.source.clone()).collect();
    hot_functions(&sources, &inline_helpers(&sources))
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// `(data, byte count)` of a borrowed byte source of `fnv1a_64` and
/// `sha256_text` (contracts N3/N4): `text`/`str` (the static-literal tag is
/// masked by `zn_text_size`), `&[u8]` and `[u8; N]`.
fn byte_source(p: &Place, value: &str) -> Option<(String, String)> {
    use crate::numeric::Integer::U8;
    match p.ty {
        Type::Text | Type::TextView => {
            Some((format!("{value}.data"), format!("zn_text_size({value})")))
        }
        Type::Slice(U8) => Some((format!("{value}.data"), format!("{value}.len"))),
        Type::Fixed(U8, n) => Some((format!("{value}.data"), format!("UINT64_C({n})"))),
        _ => None,
    }
}
/// 1.2 B (contracts N3-N5): hashing over borrowed bytes and the networking
/// request (borrowed `text`/`str` arguments, NetResult through the
/// layout-checked wrapper). `None` for every other builtin.
fn frontier3_builtin(name: &str, args: &[Place], a: &[String]) -> Option<String> {
    match name {
        "fnv1a_64" | "sha256_text" if args.len() == 1 => {
            let (data, len) = byte_source(&args[0], &a[0])?;
            let helper = if name == "fnv1a_64" {
                "zn_fnv1a_64"
            } else {
                "zn_sha256_bytes"
            };
            Some(format!("{helper}({data},{len})"))
        }
        "net_request"
            if args.len() == 2
                && args
                    .iter()
                    .all(|p| matches!(p.ty, Type::Text | Type::TextView)) =>
        {
            Some(format!("zn_v120_net_request({},{})", a[0], a[1]))
        }
        _ => None,
    }
}
fn numeric_result(raw: String, ty: Type) -> String {
    if ty.integer().unwrap().signed() {
        format!("({})zn_num_signed({raw})", ty.c())
    } else {
        format!("({})({raw})", ty.c())
    }
}
/// The nominal type a value of `ty` names, if any.
fn named_type(ty: Type) -> Option<usize> {
    match ty {
        Type::Named(id) | Type::VectorView(id) | Type::Id(id) => Some(id),
        Type::TaskOf(TaskResult::Named(id) | TaskResult::Id(id) | TaskResult::Channel(id)) => {
            Some(id)
        }
        _ => None,
    }
}
/// 1.2 B (contracts N5/N7): the networking prelude types (`NetResponse`,
/// `NetResult`, registered after every user type) when no function local,
/// parameter, result or other type definition names either of them. Their
/// C definitions and the `zn_net_request` wrapper are then omitted, so a
/// program without networking emits the 1.1.0 C.
fn unused_net_types(program: &CheckedProgram) -> BTreeSet<usize> {
    let types = &program.types;
    let net: BTreeSet<usize> = ["NetResponse", "NetResult"]
        .iter()
        .filter_map(|name| types.id(name))
        .collect();
    let named = |ty: Type| named_type(ty).is_some_and(|id| net.contains(&id));
    let in_functions = program.functions.iter().any(|f| {
        named(f.source.ret)
            || f.source.params.iter().any(|p| named(p.ty))
            || f.locals.iter().any(|l| named(l.ty))
    });
    let in_types = types.definitions.iter().enumerate().any(|(id, def)| {
        !net.contains(&id)
            && match &def.kind {
                DataKind::Record(v)
                | DataKind::Enum(v)
                | DataKind::Vector(v)
                | DataKind::SoaVector(v)
                | DataKind::Map(v)
                | DataKind::Arena(v)
                | DataKind::Channel(v) => v.iter().any(|field| named(field.ty)),
            }
    });
    if in_functions || in_types {
        BTreeSet::new()
    } else {
        net
    }
}
/// The 1.3 OS/runtime builtins. Their runtime.h declarations (the
/// `zn-v130-begin`..`zn-v130-end` block) and the net_serve_once wrapper are
/// emitted only into a program that calls one, so every other program emits
/// exactly the C it did under 1.2.
const V130_BUILTINS: &[&str] = &[
    "try_read_bytes",
    "try_write_bytes",
    "env_var",
    "create_dir",
    "remove_file",
    "net_serve_once",
    "map_keys",
    "http_request",
];
/// The embedded runtime header, without its 1.3 block unless `v130`.
fn runtime_header(v130: bool) -> String {
    let header = include_str!("runtime.h");
    if v130 {
        return header.to_string();
    }
    let begin = header
        .find("/* zn-v130-begin")
        .expect("runtime.h has its 1.3 block");
    let end_marker = "/* zn-v130-end */";
    let end = header[begin..]
        .find(end_marker)
        .expect("runtime.h closes its 1.3 block")
        + begin
        + end_marker.len();
    let end = end + header[end..].find('\n').map_or(0, |i| i + 1);
    format!("{}{}", &header[..begin], &header[end..])
}
/// Whether any function calls one of `names`.
fn uses_builtin(program: &CheckedProgram, names: &[&str]) -> bool {
    program.functions.iter().any(|f| {
        let mut pending: Vec<&Operation> = f.body.iter().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Init(_, Rvalue::Builtin { name, .. })
                    if names.contains(&name.as_str()) =>
                {
                    return true
                }
                Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => pending.extend(condition_code.iter().chain(body)),
                Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
                _ => {}
            }
        }
        false
    })
}
/// 1.3: net_serve_once returns `zn_net_result` (tag = variant index + 1) and
/// shares NetResult's checked layout with the 1.2 net_request wrapper.
fn v130_net_serve_wrapper(program: &CheckedProgram) -> String {
    let Some(ty) = program.types.net_result() else {
        return String::new();
    };
    let c = ty.c();
    format!("static {c} zn_v130_net_serve_once(zn_text endpoint,int64_t status,zn_text reply){{zn_net_result result=zn_net_serve_once(endpoint,status,reply);{c} out={{0}};_Static_assert(sizeof(out)==sizeof(result),\"net result layout\");memcpy(&out,&result,sizeof(out));return out;}}\n")
}
/// 1.3: http_request returns `zn_net_result` like net_serve_once, through the
/// same layout check.
fn v130_http_request_wrapper(program: &CheckedProgram) -> String {
    let Some(ty) = program.types.net_result() else {
        return String::new();
    };
    let c = ty.c();
    format!("static {c} zn_v130_http_request(zn_text url,zn_text body,const zn_texts* headers){{zn_net_result result=zn_http_request(url,body,headers);{c} out={{0}};_Static_assert(sizeof(out)==sizeof(result),\"net result layout\");memcpy(&out,&result,sizeof(out));return out;}}
")
}
/// 1.2 B (contract N2): whether any function uses a big-endian wire write.
fn uses_be_writes(program: &CheckedProgram) -> bool {
    program.functions.iter().any(|f| {
        let mut pending: Vec<&Operation> = f.body.iter().collect();
        while let Some(op) = pending.pop() {
            match op {
                Operation::Init(_, Rvalue::Builtin { name, .. })
                    if name.starts_with("try_write_u") && name.ends_with("_be") =>
                {
                    return true
                }
                Operation::If { yes, no, .. } => pending.extend(yes.iter().chain(no)),
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => pending.extend(condition_code.iter().chain(body)),
                Operation::Switch { arms, .. } => pending.extend(arms.iter().flatten()),
                _ => {}
            }
        }
        false
    })
}
fn data_declarations(types: &crate::types::TypeTable, omitted: &BTreeSet<usize>) -> String {
    let mut out = String::new();
    for &id in &types.order {
        if omitted.contains(&id) {
            continue;
        }
        let ty = Type::Named(id);
        let c = ty.c();
        let def = &types.definitions[id];
        if types.channel_element(ty).is_some() {
            out.push_str(&channel_declarations(types, ty));
            continue;
        }
        if types.arena_element(ty).is_some() {
            out.push_str(&arena_declarations(types, ty));
            continue;
        }
        if types.map_types(ty).is_some() {
            out.push_str(&map_declarations(types, ty));
            continue;
        }
        if types.soa_element(ty).is_some() {
            out.push_str(&soa_declarations(types, ty));
            continue;
        }
        if let DataKind::Vector(fields) = &def.kind {
            let element = fields[0].ty;
            let e = element.c();
            let drop = if element.owned() {
                format!("{}(slot);", element.drop_fn())
            } else {
                String::new()
            };
            let cloned = if element.owned() {
                format!("{e}_clone(*slot)")
            } else {
                "*slot".into()
            };
            out.push_str(&format!("typedef zn_vec {c};\n_Static_assert(sizeof({c})==24,\"vector layout mismatch\");\n"));
            if trivially_dropped(types, element, 0) && types.layout(element).size > 0 {
                // 1.0 B: element drops only zero storage that is released
                // next, and popping a nominal vector (runtime invariant
                // len <= INT64_MAX/width) cannot fault: release directly.
                out.push_str(&format!(
                    "static void {c}_drop({c}* p){{zn_vec_release(p);}}\n"
                ));
            } else {
                out.push_str(&format!("static void {c}_drop({c}* p){{while(p->len){{{e}* slot=({e}*)zn_vec_pop_slot(p,sizeof({e}));{drop}}}zn_vec_release(p);}}\n"));
            }
            out.push_str(&format!("static {c} {c}_clone({c} value){{{c} out={{0}};zn_vec_clone_storage(&out,&value,sizeof({e}));"));
            if element.owned() {
                out.push_str(&format!("for(size_t i=0;i<value.len;i++)(({e}*)out.data)[i]={e}_clone((({e}*)value.data)[i]);"));
            }
            out.push_str("return out;}\n");
            if types.layout(element).size > 0 {
                // 1.1 B: inline push/pop exactly when zn_vec_push_slot /
                // zn_vec_pop_slot would neither grow nor fault: spare
                // capacity (push) or a nonempty vector (pop; 0-1 wraps past
                // the limit), and an index zn_vec_bytes accepts (`index <=
                // min(INT64_MAX, UINT64_MAX/width)`). The slot, the new `len`
                // and the stored bytes are then the runtime's exactly; growth
                // and every fault stay in the runtime call.
                let limit = format!("(UINT64_MAX/sizeof({e})<(uint64_t)INT64_MAX?UINT64_MAX/sizeof({e}):(uint64_t)INT64_MAX)");
                out.push_str(&format!("static __forceinline void {c}_push({c}* p,{e} value){{{e}* slot;if(p->len!=p->cap && p->len<={limit}){{slot=({e}*)((char*)p->data+p->len*sizeof({e}));p->len++;}}else slot=({e}*)zn_vec_push_slot(p,sizeof({e}));*slot=value;}}\n"));
                out.push_str(&format!("static __forceinline {e} {c}_clone_at({c}* p,int64_t i){{{e}* slot=({e}*)zn_v028_vec_slot(p,i,sizeof({e}));return {cloned};}}\n"));
                out.push_str(&format!("static __forceinline {e} {c}_pop({c}* p){{{e}* slot;if(p->len-1<={limit}){{p->len--;slot=({e}*)((char*)p->data+p->len*sizeof({e}));}}else slot=({e}*)zn_vec_pop_slot(p,sizeof({e}));{e} out=*slot;memset(slot,0,sizeof(*slot));return out;}}\n"));
            } else {
                out.push_str(&format!("static void {c}_push({c}* p,{e} value){{*({e}*)zn_vec_push_slot(p,sizeof({e}))=value;}}\n"));
                out.push_str(&format!("static __forceinline {e} {c}_clone_at({c}* p,int64_t i){{{e}* slot=({e}*)zn_v028_vec_slot(p,i,sizeof({e}));return {cloned};}}\n"));
                out.push_str(&format!("static {e} {c}_pop({c}* p){{{e}* slot=({e}*)zn_vec_pop_slot(p,sizeof({e}));{e} out=*slot;memset(slot,0,sizeof(*slot));return out;}}\n"));
            }
            out.push_str(&format!("static __forceinline void {c}_replace({c}* p,int64_t i,{e} value){{{e}* slot=({e}*)zn_v028_vec_slot(p,i,sizeof({e}));{drop}*slot=value;}}\n"));
            out.push_str(&format!("static __forceinline {e} {c}_view_clone_at(const zn_span* p,int64_t i){{{e}* slot=({e}*)zn_v028_span_slot(p,i,sizeof({e}));return {cloned};}}\n"));
            out.push_str(&format!("static __forceinline void {c}_view_replace(zn_span* p,int64_t i,{e} value){{{e}* slot=({e}*)zn_v028_span_slot(p,i,sizeof({e}));{drop}*slot=value;}}\n"));
            // 1.1 B (contract M5): the same replacement for a view whose
            // descriptor passed the entry validity test.
            out.push_str(&format!("static __forceinline void {c}_view_replace_v110(void* data,uint64_t len,int64_t i,{e} value){{{e}* slot=({e}*)zn_v110_span_slot_fast(data,len,i,sizeof({e}));{drop}*slot=value;}}\n"));
            continue;
        }
        let fields = match &def.kind {
            DataKind::Record(fields) | DataKind::Enum(fields) => fields,
            _ => continue, // Container declarations and ownership glue are emitted above.
        };
        let is_enum = matches!(&def.kind, DataKind::Enum(_));
        if is_enum {
            out.push_str("typedef struct { uint64_t tag; union { unsigned char empty; ");
            for (i, f) in fields.iter().enumerate() {
                if f.ty != Type::Unit {
                    out.push_str(&format!("{} v{i}; ", f.ty.c()));
                }
            }
            out.push_str(&format!("}} data; }} {c};\n"));
        } else {
            out.push_str("typedef struct { ");
            for (i, f) in fields.iter().enumerate() {
                out.push_str(&format!("{} f{i}; ", f.ty.c()));
            }
            out.push_str(&format!("}} {c};\n"));
        }
        out.push_str(&format!(
            "_Static_assert(sizeof({c}) == {}, \"data layout mismatch\");\n",
            types.layouts[id].size
        ));
        out.push_str(&format!("static void {c}_drop({c}* p) {{\n"));
        if is_enum {
            out.push_str("switch(p->tag) {\n");
            for (i, f) in fields.iter().enumerate() {
                if f.ty.owned() {
                    out.push_str(&format!(
                        "case {}: {}(&p->data.v{i}); break;\n",
                        i + 1,
                        f.ty.drop_fn()
                    ));
                }
            }
            out.push_str("default: break; }\n");
        } else {
            for (i, f) in fields.iter().enumerate().rev() {
                if f.ty.owned() {
                    out.push_str(&format!("{}(&p->f{i});\n", f.ty.drop_fn()));
                }
            }
        }
        out.push_str(&format!(
            "*p = ({c}){{0}}; }}\nstatic {c} {c}_clone({c} value) {{ {c} out = value;\n"
        ));
        if is_enum {
            out.push_str("switch(value.tag) {\n");
            for (i, f) in fields.iter().enumerate() {
                if f.ty.owned() {
                    out.push_str(&format!(
                        "case {}: out.data.v{i} = {}_clone(value.data.v{i}); break;\n",
                        i + 1,
                        f.ty.c()
                    ));
                }
            }
            out.push_str("default: break; }\n");
        } else {
            for (i, f) in fields.iter().enumerate() {
                if f.ty.owned() {
                    out.push_str(&format!("out.f{i} = {}_clone(value.f{i});\n", f.ty.c()));
                }
            }
        }
        out.push_str("return out; }\n");
        if def.name == "ProcessRun" {
            out.push_str(&format!("static {c} zn_process_run(zn_text program,const zn_text* args,uint64_t count){{zn_process_result result=zn_process_capture(program,args,count);{c} out={{0}};_Static_assert(sizeof(out)==sizeof(result),\"process result layout\");memcpy(&out,&result,sizeof(out));return out;}}\n"));
        }
        if def.name == "NetResult" && types.net_result() == Some(ty) {
            // 1.2 B (contract N5), mirroring ProcessRun: zn_net_result uses the
            // Anode enum encoding (tag = variant index + 1: 1 Response, 2
            // Error) and the same field layout, so the bytes copy over. The
            // arguments stay borrowed; every result text is a fresh owner.
            let response = fields[0].ty.c();
            out.push_str(&format!("_Static_assert(sizeof({response})==sizeof(zn_net_response) && offsetof({response},f0)==offsetof(zn_net_response,status) && offsetof({response},f1)==offsetof(zn_net_response,body),\"net response layout\");\n_Static_assert(offsetof({c},data)==offsetof(zn_net_result,data),\"net result layout\");\nstatic {c} zn_v120_net_request(zn_text endpoint,zn_text payload){{zn_net_result result=zn_net_request(endpoint,payload);{c} out={{0}};_Static_assert(sizeof(out)==sizeof(result),\"net result layout\");memcpy(&out,&result,sizeof(out));return out;}}\n"));
        }
        if def.name == "ReadResult" || def.name == "WriteResult" {
            let (name, params, args) = if def.name == "ReadResult" {
                ("read", "zn_text p", "p")
            } else {
                ("write", "zn_text p, zn_text t", "p,t")
            };
            out.push_str(&format!("static {c} zn_try_{name}_text({params}) {{ zn_io_result r=zn_{name}_result({args}); {c} out={{0}}; memcpy(&out,&r,sizeof(out)); return out; }}\n"));
        }
    }
    out
}
