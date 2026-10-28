//! Direct COFF backend. Takes only ownership-checked IR; dynamic checks share
//! the C backend's runtime. Internal aggregates use an explicit pointer ABI.
use crate::{
    guard_facts::{self, Boolean, Fact, State as Guards},
    int_ranges::{Bounds, FunctionRanges, IntRanges},
    ir::*,
    syntax::{Borrow, DataKind, Type},
    types::TypeTable,
    Error, Result,
};
use cranelift_codegen::{
    ir::{
        self as cl, condcodes::IntCC, types::*, AbiParam, InstBuilder, MemFlagsData, StackSlotData,
        StackSlotKind, Value,
    },
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

// Conservative, transitive effect summary: these functions cannot replace or
// resize any descriptor. Unknown calls and ownership transfers stop the proof.
fn stable_descriptors(ops: &[Operation], known: &BTreeSet<String>) -> bool {
    ops.iter().all(|op| match op {
        Operation::Init(
            _,
            Rvalue::Call {
                name, spawn: false, ..
            },
        ) => known.contains(name),
        Operation::Init(destination, Rvalue::Builtin { name, .. }) => {
            descriptor_builtin(name, destination.ty)
                || ["print_int", "print_bool"].contains(&name.as_str())
        }
        Operation::Init(
            _,
            Rvalue::Int(_)
            | Rvalue::Float(_)
            | Rvalue::Bool(_)
            | Rvalue::Copy(_)
            | Rvalue::Convert(_)
            | Rvalue::Element { .. }
            | Rvalue::Reference(_)
            | Rvalue::Unary(..)
            | Rvalue::Binary(..)
            | Rvalue::IntRange { .. },
        ) => true,
        Operation::Assign(p, _) => matches!(
            p.ty,
            Type::Int | Type::Number(_) | Type::Float(_) | Type::Bool | Type::Id(_)
        ),
        Operation::Loan { .. }
        | Operation::ElementCheckPair { .. }
        | Operation::ElementWriteback { .. }
        | Operation::Assert { .. }
        | Operation::Return(_)
        | Operation::Break(_)
        | Operation::Continue(_) => true,
        Operation::If { yes, no, .. } => {
            stable_descriptors(yes, known) && stable_descriptors(no, known)
        }
        Operation::Loop {
            condition_code,
            body,
            ..
        } => stable_descriptors(condition_code, known) && stable_descriptors(body, known),
        Operation::Switch { arms, .. } => arms.iter().all(|a| stable_descriptors(a, known)),
        _ => false,
    })
}

/// Locals that `ops` may write directly. Guard facts only track `Variable`
/// locals, which are never address-exposed, so these direct writes (and a
/// channel receive's output place) are the only ways their values change.
fn written_locals(ops: &[Operation], out: &mut BTreeSet<usize>) {
    for op in ops {
        match op {
            Operation::Init(p, value) => {
                out.insert(p.local);
                if let Rvalue::Builtin { name, args } = value {
                    // try_read_bytes(path, dst) writes dst (argument 1), like
                    // a channel receive writes its output place.
                    if matches!(
                        name.as_str(),
                        "channel_recv" | "channel_try_recv" | "try_read_bytes"
                    ) {
                        if let Some(p) = args.get(1) {
                            out.insert(p.local);
                        }
                    }
                }
            }
            Operation::Assign(p, _) | Operation::Drop(p) => {
                out.insert(p.local);
            }
            Operation::Move {
                destination,
                source,
                ..
            } => {
                out.insert(destination.local);
                out.insert(source.local);
            }
            Operation::Swap(a, b)
            | Operation::ElementWriteback {
                alias: a, owner: b, ..
            } => {
                out.insert(a.local);
                out.insert(b.local);
            }
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
            _ => {}
        }
    }
}

/// Whether `ops` contain a `break` out of loop `id` (possibly nested).
fn breaks_out(ops: &[Operation], id: usize) -> bool {
    ops.iter().any(|op| match op {
        Operation::Break(target) => *target == id,
        Operation::If { yes, no, .. } => breaks_out(yes, id) || breaks_out(no, id),
        Operation::Loop {
            condition_code,
            body,
            ..
        } => breaks_out(condition_code, id) || breaks_out(body, id),
        Operation::Switch { arms, .. } => arms.iter().any(|arm| breaks_out(arm, id)),
        _ => false,
    })
}

/// Owners whose (data, len) descriptor `ops` read through inline element and
/// length accesses.
fn descriptor_reads(
    ops: &[Operation],
    out: &mut BTreeMap<crate::immutable_descriptors::Key, Place>,
) {
    for op in ops {
        match op {
            Operation::Init(_, Rvalue::Builtin { name, args })
                if matches!(
                    name.as_str(),
                    "len" | "at" | "byte_at" | "set" | "vec_at" | "vec_replace" | "vec_clone_at"
                ) =>
            {
                out.insert(crate::immutable_descriptors::key(&args[0]), args[0].clone());
            }
            Operation::Init(_, Rvalue::Element { owner, .. })
            | Operation::ElementCheckPair { owner, .. } => {
                out.insert(crate::immutable_descriptors::key(owner), owner.clone());
            }
            Operation::If { yes, no, .. } => {
                descriptor_reads(yes, out);
                descriptor_reads(no, out);
            }
            Operation::Loop {
                condition_code,
                body,
                ..
            } => {
                descriptor_reads(condition_code, out);
                descriptor_reads(body, out);
            }
            Operation::Switch { arms, .. } => {
                for arm in arms {
                    descriptor_reads(arm, out);
                }
            }
            _ => {}
        }
    }
}

// These reads neither resize an owner nor replace a descriptor. An arena ID
// stays a scalar value, never a proof that its slot is still live: all arena
// reads retain the runtime's generation/occupancy validation inline.
fn descriptor_builtin(name: &str, result: Type) -> bool {
    [
        "len",
        "at",
        "byte_at",
        "set",
        "fixed_len",
        "fixed_at",
        "fixed_set",
        "fixed_try_set",
    ]
    .contains(&name)
        || crate::syntax::integer_intrinsic(name).is_some()
        || name.starts_with("read_u")
        || name.starts_with("try_write_u")
        || (matches!(
            result,
            Type::Int | Type::Number(_) | Type::Float(_) | Type::Bool | Type::Id(_) | Type::Unit
        ) && [
            "vec_at",
            "vec_clone_at",
            "vec_replace",
            "vec_capacity",
            "arena_len",
            "arena_contains_id",
            "arena_clone_get",
        ]
        .contains(&name))
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(0, format!("direct native backend: {e}"))
}
fn scalar(t: Type) -> bool {
    matches!(
        t,
        Type::Int
            | Type::Number(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Task
            | Type::TaskOf(_)
            | Type::Id(_)
    )
}
fn ct(t: Type) -> cl::Type {
    match t {
        Type::Bool => I8,
        Type::Float(crate::numeric::Float::F32) => F32,
        Type::Float(crate::numeric::Float::F64) => F64,
        _ => I64,
    }
}
// Runtime numeric helpers and packed memory carry canonical integer bits.
// Only the private user-call ABI and SSA float variables use XMM values.
fn raw_type(t: Type) -> cl::Type {
    if t == Type::Bool {
        I8
    } else {
        I64
    }
}
fn integer_type(n: crate::numeric::Integer) -> cl::Type {
    match n.bits() {
        8 => I8,
        16 => I16,
        32 => I32,
        _ => I64,
    }
}
fn abi_zero(b: &mut FunctionBuilder<'_>, ty: Type) -> Value {
    match ct(ty) {
        F32 => b.ins().f32const(cl::immediates::Ieee32::with_bits(0)),
        F64 => b.ins().f64const(cl::immediates::Ieee64::with_bits(0)),
        ty => b.ins().iconst(ty, 0),
    }
}
fn signature(m: &ObjectModule, params: &[cl::Type], ret: Option<cl::Type>) -> cl::Signature {
    let mut s = m.make_signature();
    s.params = params.iter().map(|t| AbiParam::new(*t)).collect();
    if let Some(t) = ret {
        s.returns.push(AbiParam::new(t));
    }
    s
}
fn user_sig(m: &ObjectModule, f: &CheckedFunction) -> cl::Signature {
    let mut ps = Vec::new();
    if f.source.ret != Type::Unit && !scalar(f.source.ret) {
        ps.push(I64);
    }
    ps.extend(f.source.params.iter().map(|p| {
        if p.borrow != Borrow::Owned || !scalar(p.ty) {
            I64
        } else if f.source.external_symbol.is_some() {
            ffi_type(p.ty)
        } else {
            ct(p.ty)
        }
    }));
    signature(
        m,
        &ps,
        if scalar(f.source.ret) {
            Some(if f.source.external_symbol.is_some() {
                ffi_type(f.source.ret)
            } else {
                ct(f.source.ret)
            })
        } else {
            None
        },
    )
}
fn ffi_type(ty: Type) -> cl::Type {
    match ty {
        Type::Float(crate::numeric::Float::F32) => F32,
        Type::Float(_) => F64,
        Type::Bool => I8,
        Type::Number(n) => match n.bits() {
            8 => I8,
            16 => I16,
            32 => I32,
            _ => I64,
        },
        _ => I64,
    }
}
/// Conversions emitted inline (contract M3): integer to float and float to
/// float. Each is bit-identical to `zn_fp_convert` for every input (see
/// `Writer::float_conversion`); float to integer keeps the runtime call with
/// its exact range and non-finite faults.
fn exact_float_conversion(from: Type, to: Type) -> bool {
    matches!(to, Type::Float(_))
        && (matches!(from, Type::Float(_)) || matches!(from, Type::Int | Type::Number(_)))
}
/// Contract M3 (1.0 B's rule): dropping a value of `ty` has no effect other
/// than zeroing its own storage: scalars, fixed buffers, and records/enums of
/// such values. Cloning such a value is a byte copy. Text, ints, containers,
/// tasks, channels and views are not trivially dropped.
fn trivially_dropped(types: &TypeTable, ty: Type, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    if !ty.owned() || matches!(ty, Type::Fixed(..)) {
        return true;
    }
    let Type::Named(id) = ty else {
        return false;
    };
    match &types.definitions[id].kind {
        DataKind::Record(fields) | DataKind::Enum(fields) => fields
            .iter()
            .all(|f| trivially_dropped(types, f.ty, depth + 1)),
        _ => false,
    }
}
// Inline only small, already-checked scalar/borrowed helpers in hot functions.
// No owned temporaries, allocation, task transfer or recursive expansion.
// Small structured loops share the same code/expansion budget as other work.
fn inline_cost(f: &CheckedFunction) -> Option<usize> {
    let exposed = crate::immutable_descriptors::address_exposed(f);
    if f.locals
        .iter()
        .enumerate()
        .any(|(i, l)| exposed[i] && l.borrow == Borrow::Owned && scalar(l.ty))
    {
        return None;
    }
    if !matches!(
        f.source.ret,
        Type::Int | Type::Bool | Type::Id(_) | Type::Unit
    ) || f.locals.iter().any(|l| {
        l.borrow == Borrow::Owned
            && !matches!(l.ty, Type::Int | Type::Bool | Type::Id(_) | Type::Unit)
    }) {
        return None;
    }
    fn cost(ops: &[Operation]) -> Option<usize> {
        let mut total = 0;
        for op in ops {
            total += 1;
            match op {
                Operation::Init(
                    _,
                    Rvalue::Int(_)
                    | Rvalue::Bool(_)
                    | Rvalue::Zero
                    | Rvalue::Copy(_)
                    | Rvalue::Unary(..)
                    | Rvalue::Binary(..)
                    | Rvalue::IntRange { .. }
                    | Rvalue::Call { spawn: false, .. },
                ) => {}
                Operation::Init(destination, Rvalue::Builtin { name, .. })
                    if descriptor_builtin(name, destination.ty) => {}
                Operation::Assign(..)
                | Operation::Loan { .. }
                | Operation::Assert { .. }
                | Operation::Return(_) => {}
                Operation::If { yes, no, .. } => total += cost(yes)? + cost(no)?,
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => total += cost(condition_code)? + cost(body)?,
                Operation::Break(_) | Operation::Continue(_) => {}
                _ => return None,
            }
        }
        (total <= 80).then_some(total)
    }
    cost(&f.body)
}
struct Environment<'a> {
    module: ObjectModule,
    types: &'a TypeTable,
    funcs: BTreeMap<String, (FuncId, &'a CheckedFunction)>,
    imports: BTreeMap<String, FuncId>,
    workers: BTreeMap<String, FuncId>,
    channel_drops: BTreeMap<usize, FuncId>,
    serial: usize,
    strings: BTreeMap<String, cranelift_module::DataId>,
    unwind: Vec<(FuncId, u32, Vec<u8>)>,
    inline_costs: &'a BTreeMap<String, usize>,
    descriptor_stable: &'a BTreeSet<String>,
    pinned_views: &'a BTreeMap<String, Vec<usize>>,
    predicates: &'a BTreeMap<String, crate::predicates::Expr>,
    integer_functions: &'a FunctionRanges<'a>,
    /// Pre-optimization CLIF text of user functions, collected only by
    /// [`clif`] for codegen introspection; object emission never reads it.
    clif: Option<String>,
}
impl Environment<'_> {
    fn define(&mut self, id: FuncId, ctx: &mut cranelift_codegen::Context) -> Result<()> {
        self.module.define_function(id, ctx).map_err(err)?;
        let code = ctx
            .compiled_code()
            .ok_or_else(|| err("missing machine code"))?;
        if let Some(cranelift_codegen::isa::unwind::UnwindInfo::WindowsX64(info)) =
            code.create_unwind_info(self.module.isa()).map_err(err)?
        {
            let mut bytes = vec![0; info.emit_size()];
            info.emit(&mut bytes);
            self.unwind.push((id, code.code_info().total_size, bytes));
        }
        Ok(())
    }
}
/// Contract M4 (1.1.0 A, 1.0 B's C rule): `sum(coefficient * local) + x`
/// with every local a tracked `int` the loop never writes (so the sum is the
/// same on every iteration), coefficients in {-1, +1}, and `x` in the proven
/// interval `[low, high]`.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// `self + sign * other`, or `None` when a coefficient leaves {-1, 0, 1}.
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
/// One hoisted guard: `sum(coefficient * local)` is exact and
/// `sum + [low, high]` lies within i64.
type VersionCheck = (Vec<(usize, i64)>, i64, i64);
const VERSION_MAX_OPERATIONS: usize = 256;
const VERSION_MAX_CHECKS: usize = 16;
/// State while emitting the fast copy of a versioned innermost loop.
struct Versioning {
    /// Locals the loop writes; every other tracked `int` is invariant.
    written: BTreeSet<usize>,
    /// Forms of tracked locals defined earlier in the current straight-line
    /// region (the condition, or the body) of this emission.
    forms: BTreeMap<usize, Affine>,
    /// The form of the definition currently being emitted.
    current: Option<Affine>,
    checks: Vec<VersionCheck>,
}
#[derive(Clone, Copy)]
enum Storage {
    Variable(Variable),
    Descriptor {
        data: Variable,
        len: Variable,
    },
    Address(Value),
    Unit,
    Columns {
        data: Value,
        capacity: Value,
        index: Value,
        vector: Type,
    },
}
struct Writer<'a, 'b> {
    env: &'a mut Environment<'b>,
    b: FunctionBuilder<'a>,
    locals: Vec<Storage>,
    local_types: Vec<Type>,
    out: Option<Value>,
    hot: bool,
    inline_return: Option<cl::Block>,
    inline_stack: Vec<String>,
    inline_budget: usize,
    array_metadata: BTreeMap<crate::immutable_descriptors::Key, (Value, Value)>,
    /// Entry-loaded descriptors that no operation in this call can replace
    /// (contract L3); they survive every metadata invalidation.
    pinned: Vec<(crate::immutable_descriptors::Key, (Value, Value))>,
    /// SSA lengths nonnegative by construction (masked text lengths). A pure
    /// property of the value, valid wherever the value itself is available.
    nonnegative: BTreeSet<Value>,
    /// (data, len, width) of view descriptors that this (versioned) body
    /// only runs with when they satisfy the runtime invariant (contract L4).
    valid_spans: BTreeSet<(Value, Value, usize)>,
    loop_targets: BTreeMap<usize, (cl::Block, cl::Block)>,
    integer_ranges: IntRanges,
    integer_overrides: BTreeMap<usize, Bounds>,
    guards: Guards,
    guard_serial: u64,
    direct_soa_aliases: BTreeSet<usize>,
    column_loops: BTreeMap<usize, ColumnLoop>,
    hoisted_columns: BTreeMap<usize, Vec<Value>>,
    hoisted_lengths: BTreeMap<usize, Value>,
    trap_blocks: BTreeMap<String, cl::Block>,
    /// Hoisted VectorView validity limits, scoped by dominance in `block`.
    spans: guard_facts::Spans<Value>,
    /// Nonzero while emitting a loop condition, which rotation may emit twice:
    /// specializations recorded there must not depend on path-local facts.
    condition_depth: usize,
    /// Contract M4: set while emitting the fast copy of a versioned loop.
    versioning: Option<Versioning>,
}
impl Writer<'_, '_> {
    // Runtime helpers and packed storage use IEEE bits; user calls use XMM.
    fn float_value(&mut self, raw: Value, n: crate::numeric::Float) -> Value {
        if n == crate::numeric::Float::F32 {
            let narrow = self.b.ins().ireduce(I32, raw);
            self.b.ins().bitcast(F32, MemFlagsData::new(), narrow)
        } else {
            self.b.ins().bitcast(F64, MemFlagsData::new(), raw)
        }
    }
    fn float_bits(&mut self, value: Value, n: crate::numeric::Float) -> Value {
        if n == crate::numeric::Float::F32 {
            let narrow = self.b.ins().bitcast(I32, MemFlagsData::new(), value);
            self.b.ins().uextend(I64, narrow)
        } else {
            self.b.ins().bitcast(I64, MemFlagsData::new(), value)
        }
    }
    fn float_binary(
        &mut self,
        op: &str,
        a: &Place,
        b: &Place,
        n: crate::numeric::Float,
    ) -> Result<Value> {
        use cranelift_codegen::ir::condcodes::FloatCC;
        let ar = self.load(a);
        let br = self.load(b);
        if op == "%" {
            let code = self.b.ins().iconst(I64, n.code());
            return Ok(self
                .call("zn_fp_remainder", &[ar, br, code], Some(I64))?
                .unwrap());
        }
        let av = self.float_value(ar, n);
        let bv = self.float_value(br, n);
        let v = match op {
            "+" => self.b.ins().fadd(av, bv),
            "-" => self.b.ins().fsub(av, bv),
            "*" => self.b.ins().fmul(av, bv),
            "/" => self.b.ins().fdiv(av, bv),
            _ => {
                let cc = match op {
                    "==" => FloatCC::Equal,
                    "!=" => FloatCC::NotEqual,
                    "<" => FloatCC::LessThan,
                    "<=" => FloatCC::LessThanOrEqual,
                    ">" => FloatCC::GreaterThan,
                    ">=" => FloatCC::GreaterThanOrEqual,
                    _ => return Err(err("unknown floating operation")),
                };
                return Ok(self.b.ins().fcmp(cc, av, bv));
            }
        };
        Ok(self.float_bits(v, n))
    }
    fn cache_borrowed_metadata(
        &mut self,
        f: &CheckedFunction,
        forwarded: &BTreeMap<crate::immutable_descriptors::Key, (Value, Value)>,
    ) {
        let stable = self.env.descriptor_stable.contains(&f.source.name);
        let pinned = self
            .env
            .pinned_views
            .get(&f.source.name)
            .cloned()
            .unwrap_or_default();
        // Contract L3: an entry descriptor survives metadata invalidation only
        // if nothing reachable during this call can replace it. Shared
        // parameters and their record projections are frozen for the call by
        // the checker; exclusive ones only under the whole-function `stable`
        // proof; `pinned` views by the whole-program write analysis. Any
        // place this function itself replaces or passes mutably is excluded.
        let funcs = &self.env.funcs;
        let replaced = crate::immutable_descriptors::replaced(f, &|name, i| {
            funcs.get(name).is_none_or(|(_, callee)| {
                callee
                    .source
                    .params
                    .get(i)
                    .is_none_or(|p| p.borrow == Borrow::Exclusive)
            })
        });
        let mut places = crate::immutable_descriptors::used(f, self.env.types);
        for (i, p) in f.source.params.iter().enumerate() {
            if crate::immutable_descriptors::descriptor(p.ty, self.env.types)
                && (p.borrow == Borrow::Shared
                    || (p.borrow == Borrow::Exclusive && stable)
                    || pinned.contains(&i))
            {
                places.push(Place::local(i, p.ty));
            }
        }
        for p in places {
            let key = crate::immutable_descriptors::key(&p);
            if self.array_metadata.contains_key(&key) {
                continue;
            }
            let metadata = if let Some(metadata) = forwarded.get(&key) {
                *metadata
            } else {
                let a = self.address(&p);
                let flags = self.array_flags(false);
                let data = self.b.ins().load(I64, flags, a, 0);
                let raw_len = self.b.ins().load(I64, flags, a, 8);
                let len = if p.ty == Type::Text {
                    self.text_length(raw_len)
                } else {
                    raw_len
                };
                (data, len)
            };
            let borrow = f.source.params.get(p.local).map(|param| param.borrow);
            let frozen = !replaced.contains(&p.local)
                && f.locals
                    .get(p.local)
                    .is_some_and(|l| l.borrow != Borrow::Owned)
                && (borrow == Some(Borrow::Shared)
                    || (borrow == Some(Borrow::Exclusive) && stable && p.fields.is_empty()));
            if frozen || (p.fields.is_empty() && pinned.contains(&p.local)) {
                self.pinned.push((key.clone(), metadata));
            }
            self.array_metadata.insert(key, metadata);
            // Hoist the pure descriptor-validity limit to this dominating
            // entry point; unused limits are removed as dead code.
            if let Type::VectorView(id) = p.ty {
                let element = self.env.types.vector_element(Type::Named(id)).unwrap();
                let size = self.env.types.layout(element).size;
                if size != 0 {
                    self.span_limit(metadata.0, metadata.1, size);
                }
            }
        }
    }
    /// A text length is `raw & INT64_MAX` (the top bit is the static tag), so
    /// this exact SSA value is nonnegative wherever it is available.
    fn text_length(&mut self, raw: Value) -> Value {
        let len = self.b.ins().band_imm_s(raw, i64::MAX);
        self.nonnegative.insert(len);
        self.guards
            .active
            .insert(Fact::NonnegativeLength(len.as_u32() as usize));
        len
    }
    /// Facts established on the success path of an executed (or already
    /// proven) element access or pair check: `0 <= index <= high`, and, when
    /// the check also proves `len <= INT64_MAX` (so `index <u len` is the
    /// signed `index < len` that `BelowLength` means), `index < len` for
    /// this exact SSA length.
    fn index_facts(&mut self, index: &Place, len: Value, high: i64, below: bool) {
        if let Some(origin) = self.origin(index) {
            self.guards.active.insert(Fact::Within(origin, 0, high));
            if below {
                self.guards
                    .active
                    .insert(Fact::BelowLength(origin, len.as_u32() as usize));
            }
        }
    }
    /// `len >= 0` is proven for this exact SSA length: by construction (a
    /// masked text length) or by a dominating executed check.
    fn known_nonnegative(&self, len: Value) -> bool {
        self.nonnegative.contains(&len)
            || self
                .guards
                .active
                .contains(&Fact::NonnegativeLength(len.as_u32() as usize))
    }
    /// Contract L4 versioning candidates: pinned (call-invariant) whole view
    /// parameters that loops of `f` read, as (data, len, element width).
    fn versioned_views(&self, f: &CheckedFunction) -> Vec<(Value, Value, usize)> {
        fn size(ops: &[Operation]) -> usize {
            ops.iter()
                .map(|op| {
                    1 + match op {
                        Operation::If { yes, no, .. } => size(yes) + size(no),
                        Operation::Loop {
                            condition_code,
                            body,
                            ..
                        } => size(condition_code) + size(body),
                        Operation::Switch { arms, .. } => arms.iter().map(|a| size(a)).sum(),
                        _ => 0,
                    }
                })
                .sum()
        }
        fn looped(ops: &[Operation], out: &mut BTreeMap<crate::immutable_descriptors::Key, Place>) {
            for op in ops {
                match op {
                    Operation::Loop {
                        condition_code,
                        body,
                        ..
                    } => {
                        descriptor_reads(condition_code, out);
                        descriptor_reads(body, out);
                    }
                    Operation::If { yes, no, .. } => {
                        looped(yes, out);
                        looped(no, out);
                    }
                    Operation::Switch { arms, .. } => {
                        for arm in arms {
                            looped(arm, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        if size(&f.body) > 400 {
            return Vec::new();
        }
        let mut reads = BTreeMap::new();
        looped(&f.body, &mut reads);
        self.pinned
            .iter()
            .filter(|(key, _)| key.1.is_empty() && reads.contains_key(key))
            .filter_map(|(key, (data, len))| {
                let Type::VectorView(id) = f.locals[key.0].ty else {
                    return None;
                };
                let element = self.env.types.vector_element(Type::Named(id))?;
                let width = self.env.types.layout(element).size;
                (width != 0).then_some((*data, *len, width))
            })
            .collect()
    }
    /// Emit `f`'s body; with versioning candidates, once per version: a body
    /// specialized for invariant-satisfying views, selected by one entry
    /// test, and the unchanged checked body for every other call. Neither
    /// version can fault or return differently from the original.
    fn function_body(&mut self, f: &CheckedFunction) -> Result<()> {
        let versions = self.versioned_views(f);
        if versions.is_empty() {
            if !self.block(&f.body)? {
                self.b.ins().return_(&[]);
            }
            return Ok(());
        }
        let mut valid = None;
        for &(data, len, width) in &versions {
            // The hoisted limit is `valid ? len : 0`, and an invalid view
            // always has `len != 0`: valid exactly when `limit == len`.
            let limit = self.span_limit(data, len, width);
            let ok = self.b.ins().icmp(IntCC::Equal, limit, len);
            valid = Some(match valid {
                Some(all) => self.b.ins().band(all, ok),
                None => ok,
            });
        }
        let fast = self.b.create_block();
        let checked = self.b.create_block();
        self.b.ins().brif(valid.unwrap(), fast, &[], checked, &[]);
        let locals = self.locals.clone();
        let metadata = self.array_metadata.clone();
        let spans = self.spans.clone();
        let guards = self.guards.clone();
        let overrides = self.integer_overrides.clone();
        let budget = self.inline_budget;
        self.b.switch_to_block(fast);
        self.valid_spans = versions.into_iter().collect();
        if !self.block(&f.body)? {
            self.b.ins().return_(&[]);
        }
        self.valid_spans.clear();
        self.locals = locals;
        self.array_metadata = metadata;
        self.spans = spans;
        self.guards = guards;
        self.integer_overrides = overrides;
        self.inline_budget = budget;
        // Views of runtime vectors satisfy the invariant; the checked body
        // serves only unusual foreign views, so it is laid out as cold code.
        let first = self.b.func.dfg.num_blocks();
        self.b.switch_to_block(checked);
        if !self.block(&f.body)? {
            self.b.ins().return_(&[]);
        }
        self.b.set_cold_block(checked);
        for index in first..self.b.func.dfg.num_blocks() {
            self.b.set_cold_block(cl::Block::from_u32(index as u32));
        }
        Ok(())
    }
    /// Before a loop whose iterations keep every descriptor stable (no call
    /// or builtin that can replace one runs inside it), each owner it reads
    /// but never writes has the same (data, len) on every iteration. Load
    /// those once here, at the point dominating the loop. Reading a local's
    /// or a borrowed place's own descriptor storage is always a valid load,
    /// even if the loop then runs zero times.
    fn hoist_descriptors(
        &mut self,
        condition: &[Operation],
        body: &[Operation],
        written: &BTreeSet<usize>,
    ) {
        let mut owners = BTreeMap::new();
        descriptor_reads(condition, &mut owners);
        descriptor_reads(body, &mut owners);
        for (key, owner) in owners {
            if written.contains(&owner.local)
                || self.array_metadata.contains_key(&key)
                || !crate::immutable_descriptors::descriptor(owner.ty, self.env.types)
                || self.env.types.soa_element(owner.ty).is_some()
                || !matches!(self.locals[owner.local], Storage::Address(_))
            {
                continue;
            }
            let (data, len) = self.descriptor(&owner);
            if let Type::VectorView(id) = owner.ty {
                let element = self.env.types.vector_element(Type::Named(id)).unwrap();
                let size = self.env.types.layout(element).size;
                if size != 0 {
                    self.span_limit(data, len, size);
                }
            }
        }
    }
    /// A loop test cheap enough to emit twice (rotation): at most `size`
    /// operations (counting nested short-circuit branches) and `accesses`
    /// element reads, each on a descriptor already in registers; pure scalar
    /// work, integer conversions and calls of pure inlined predicates only.
    /// Rotation changes only layout: the test and its own dynamic checks
    /// still run exactly once before every iteration.
    fn cheap_condition(&self, ops: &[Operation], size: &mut usize, accesses: &mut usize) -> bool {
        ops.iter().all(|op| {
            if *size == 0 {
                return false;
            }
            *size -= 1;
            match op {
                Operation::Init(
                    _,
                    Rvalue::Copy(_)
                    | Rvalue::Bool(_)
                    | Rvalue::Int(_)
                    | Rvalue::Unary(..)
                    | Rvalue::IntRange { .. }
                    | Rvalue::Binary(..),
                ) => true,
                Operation::Init(d, Rvalue::Convert(p)) => {
                    p.ty.integer().is_some() && d.ty.integer().is_some()
                }
                Operation::Assign(d, _) => scalar(d.ty) && d.fields.is_empty(),
                Operation::Init(
                    _,
                    Rvalue::Call {
                        name, spawn: false, ..
                    },
                ) => self.env.predicates.contains_key(name),
                Operation::Init(_, Rvalue::Builtin { name, args })
                    if matches!(
                        name.as_str(),
                        "len" | "fixed_len" | "at" | "byte_at" | "vec_at"
                    ) =>
                {
                    if !matches!(name.as_str(), "len" | "fixed_len") {
                        if *accesses == 0 {
                            return false;
                        }
                        *accesses -= 1;
                    }
                    name == "fixed_len"
                        || matches!(self.locals[args[0].local], Storage::Descriptor { .. })
                        || self
                            .array_metadata
                            .contains_key(&crate::immutable_descriptors::key(&args[0]))
                }
                Operation::If { yes, no, .. } => {
                    self.cheap_condition(yes, size, accesses)
                        && self.cheap_condition(no, size, accesses)
                }
                _ => false,
            }
        })
    }
    /// Forget transient descriptor metadata, keeping only pinned descriptors
    /// whose storage no operation of this call can replace.
    fn clear_metadata(&mut self) {
        self.array_metadata.clear();
        for (key, metadata) in &self.pinned {
            self.array_metadata.insert(key.clone(), *metadata);
        }
    }
    fn array_flags(&mut self, elements: bool) -> MemFlagsData {
        // Array descriptors never overlap their separately allocated i64 buffer.
        // Other accesses remain unclassified and therefore conservatively alias
        // both regions. Runtime calls still invalidate memory knowledge.
        let region = self.b.func.dfg.alias_regions.insert(cl::AliasRegionData {
            user_id: if elements { 2 } else { 1 },
            description: if elements {
                "anode array elements"
            } else {
                "anode array descriptor"
            }
            .into(),
        });
        MemFlagsData::new().with_alias_region(Some(region))
    }
    fn inline_call(
        &mut self,
        f: &CheckedFunction,
        args: &[Value],
        source_args: &[Argument],
    ) -> Result<(Option<Value>, Bounds)> {
        let input_bounds = source_args
            .iter()
            .map(|arg| match arg {
                Argument::Value(p) => self.integer_ranges.get(p, &self.integer_overrides),
                Argument::Reference(_) => Bounds::FULL,
            })
            .collect::<Vec<_>>();
        if let Some(predicate) = self.env.predicates.get(&f.source.name).cloned() {
            let value = self.predicate(&predicate, args, &mut BTreeMap::new());
            return Ok((Some(value), Bounds::FULL));
        }
        let ranges = self.env.integer_functions.analyze(f, &input_bounds);
        let forwarded: BTreeMap<_, _> = source_args
            .iter()
            .enumerate()
            .flat_map(|(i, arg)| {
                let mut projected = Vec::new();
                if let Argument::Reference(p) = arg {
                    for ((local, fields), &metadata) in &self.array_metadata {
                        if *local == p.local && fields.starts_with(&p.fields) {
                            projected.push(((i, fields[p.fields.len()..].to_vec()), metadata));
                        }
                    }
                }
                projected
            })
            .collect();
        let exit = self.b.create_block();
        if f.source.ret != Type::Unit {
            self.b.append_block_param(exit, ct(f.source.ret));
        }
        let mut locals = Vec::with_capacity(f.locals.len());
        for (i, l) in f.locals.iter().enumerate() {
            locals.push(if l.ty == Type::Unit {
                Storage::Unit
            } else if l.borrow != Borrow::Owned {
                Storage::Address(args[i])
            } else {
                let v = self.b.declare_var(ct(l.ty));
                let init = if i < args.len() {
                    args[i]
                } else {
                    abi_zero(&mut self.b, l.ty)
                };
                self.b.def_var(v, init);
                Storage::Variable(v)
            });
        }
        let saved_locals = std::mem::replace(&mut self.locals, locals);
        let saved_types = std::mem::replace(
            &mut self.local_types,
            f.locals.iter().map(|l| l.ty).collect(),
        );
        let saved_return = self.inline_return.replace(exit);
        let saved_metadata = std::mem::take(&mut self.array_metadata);
        let saved_pinned = std::mem::take(&mut self.pinned);
        let saved_spans = self.spans.clone();
        let saved_ranges = std::mem::replace(&mut self.integer_ranges, ranges);
        let saved_overrides = std::mem::take(&mut self.integer_overrides);
        let saved_guards = std::mem::take(&mut self.guards);
        // Checked loop IDs are function-local; a callee's break/continue must
        // never overwrite an enclosing caller's loop target with the same ID.
        let saved_loop_targets = std::mem::take(&mut self.loop_targets);
        let saved_column_loops = std::mem::replace(
            &mut self.column_loops,
            crate::ir::column_loops(f, self.env.types),
        );
        let saved_hoisted_columns = std::mem::take(&mut self.hoisted_columns);
        let saved_hoisted_lengths = std::mem::take(&mut self.hoisted_lengths);
        // Affine forms are keyed by the caller's locals: an inlined body is
        // emitted fully checked (its result is an ordinary definition).
        let saved_versioning = self.versioning.take();
        // A callee may reuse only projections it independently proves stable.
        self.cache_borrowed_metadata(f, &forwarded);
        self.inline_stack.push(f.source.name.clone());
        if !self.block(&f.body)? {
            debug_assert_eq!(f.source.ret, Type::Unit);
            self.b.ins().jump(exit, &[]);
        }
        self.inline_stack.pop();
        let return_bounds = self.integer_ranges.result(&self.integer_overrides);
        self.inline_return = saved_return;
        self.locals = saved_locals;
        self.local_types = saved_types;
        self.array_metadata = saved_metadata;
        self.pinned = saved_pinned;
        self.spans.retain_inherited(&saved_spans);
        self.integer_ranges = saved_ranges;
        self.integer_overrides = saved_overrides;
        self.guards = saved_guards;
        self.loop_targets = saved_loop_targets;
        self.column_loops = saved_column_loops;
        self.hoisted_columns = saved_hoisted_columns;
        self.hoisted_lengths = saved_hoisted_lengths;
        self.versioning = saved_versioning;
        self.b.switch_to_block(exit);
        Ok((self.b.block_params(exit).first().copied(), return_bounds))
    }
    fn predicate(
        &mut self,
        expr: &crate::predicates::Expr,
        args: &[Value],
        memo: &mut BTreeMap<usize, Value>,
    ) -> Value {
        use crate::predicates::Node;
        let key = std::sync::Arc::as_ptr(expr) as usize;
        if let Some(value) = memo.get(&key) {
            return *value;
        }
        let result = match expr.as_ref() {
            Node::Input(i) => args[*i],
            Node::Int(n) => self.b.ins().iconst(I64, *n),
            Node::Bool(b) => self.b.ins().iconst(I8, i64::from(*b)),
            Node::Compare(op, ty, a, b) => {
                let a = self.predicate(a, args, memo);
                let b = self.predicate(b, args, memo);
                let (a, b) = if let Type::Number(n) = ty {
                    (self.narrow_integer(a, *n), self.narrow_integer(b, *n))
                } else {
                    (a, b)
                };
                let unsigned = ty.integer().is_some_and(|n| !n.signed());
                let cc = match (op.as_str(), unsigned) {
                    ("==", _) => IntCC::Equal,
                    ("!=", _) => IntCC::NotEqual,
                    ("<", false) => IntCC::SignedLessThan,
                    ("<", true) => IntCC::UnsignedLessThan,
                    ("<=", false) => IntCC::SignedLessThanOrEqual,
                    ("<=", true) => IntCC::UnsignedLessThanOrEqual,
                    (">", false) => IntCC::SignedGreaterThan,
                    (">", true) => IntCC::UnsignedGreaterThan,
                    (">=", false) => IntCC::SignedGreaterThanOrEqual,
                    (">=", true) => IntCC::UnsignedGreaterThanOrEqual,
                    _ => unreachable!(),
                };
                self.b.ins().icmp(cc, a, b)
            }
            Node::Range(input, low, high) => {
                let value = self.predicate(input, args, memo);
                let low_value = self.b.ins().iconst(I64, *low);
                let shifted = self.b.ins().isub(value, low_value);
                let span = self
                    .b
                    .ins()
                    .iconst(I64, (*high as i128 - *low as i128) as u64 as i64);
                self.b
                    .ins()
                    .icmp(IntCC::UnsignedLessThanOrEqual, shifted, span)
            }
            Node::Not(a) => {
                let a = self.predicate(a, args, memo);
                self.b.ins().icmp_imm_s(IntCC::Equal, a, 0)
            }
            Node::And(a, b) | Node::Or(a, b) => {
                let av = self.predicate(a, args, memo);
                let bv = self.predicate(b, args, memo);
                if matches!(expr.as_ref(), Node::And(..)) {
                    self.b.ins().band(av, bv)
                } else {
                    self.b.ins().bor(av, bv)
                }
            }
            Node::Select(c, y, n) => {
                let c = self.predicate(c, args, memo);
                let y = self.predicate(y, args, memo);
                let n = self.predicate(n, args, memo);
                self.b.ins().select(c, y, n)
            }
        };
        memo.insert(key, result);
        result
    }
    fn trap_block(&mut self, message: &str) -> cl::Block {
        if let Some(&bad) = self.trap_blocks.get(message) {
            bad
        } else {
            let bad = self.b.create_block();
            self.b.set_cold_block(bad);
            self.trap_blocks.insert(message.into(), bad);
            bad
        }
    }
    fn fail_if(&mut self, condition: Value, message: &str) -> Result<()> {
        let bad = self.trap_block(message);
        let good = self.b.create_block();
        self.b.ins().brif(condition, bad, &[], good, &[]);
        self.b.switch_to_block(good);
        Ok(())
    }
    fn finish_traps(&mut self) -> Result<()> {
        for (message, block) in std::mem::take(&mut self.trap_blocks) {
            self.b.switch_to_block(block);
            let m = self.string(&message)?;
            self.call("zn_fail", &[m], None)?;
            self.b.ins().trap(cl::TrapCode::user(1).unwrap());
        }
        Ok(())
    }
    fn arithmetic(&mut self, op: &str, a: Value, b: Value) -> Result<Value> {
        let (value, overflow) = match op {
            "+" => self.b.ins().sadd_overflow(a, b),
            "-" => self.b.ins().ssub_overflow(a, b),
            "*" | "*literal" => self.b.ins().smul_overflow(a, b),
            "/" | "%" => {
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                self.fail_if(
                    zero,
                    if op == "/" {
                        "division by zero"
                    } else {
                        "remainder by zero"
                    },
                )?;
                let min = self.b.ins().icmp_imm_s(IntCC::Equal, a, i64::MIN);
                let neg = self.b.ins().icmp_imm_s(IntCC::Equal, b, -1);
                let ov = self.b.ins().band(min, neg);
                self.fail_if(ov, "integer overflow")?;
                return Ok(if op == "/" {
                    self.b.ins().sdiv(a, b)
                } else {
                    self.b.ins().srem(a, b)
                });
            }
            _ => unreachable!(),
        };
        self.fail_if(overflow, "integer overflow")?;
        Ok(value)
    }
    /// Checked i64 `+`/`-` when one operand's sign is proven: the wrapped
    /// result then overflowed exactly when it moved the wrong way (e.g. for
    /// `b >= 0`, `a + b` overflowed iff the wrapped sum is `< a`). One
    /// compare-and-branch replaces the overflow-flag materialization, with the
    /// same `integer overflow` fault on exactly the same inputs.
    fn signed_step(
        &mut self,
        op: &str,
        a: Value,
        b: Value,
        left: Bounds,
        right: Bounds,
    ) -> Result<Option<Value>> {
        let (value, overflow) = match op {
            "+" => {
                let (base, step) = if right.low >= 0 || right.high < 0 {
                    (a, right)
                } else if left.low >= 0 || left.high < 0 {
                    (b, left)
                } else {
                    return Ok(None);
                };
                let sum = self.b.ins().iadd(a, b);
                let cc = if step.low >= 0 {
                    IntCC::SignedLessThan
                } else {
                    IntCC::SignedGreaterThan
                };
                (sum, self.b.ins().icmp(cc, sum, base))
            }
            "-" if right.low >= 0 || right.high < 0 => {
                let difference = self.b.ins().isub(a, b);
                let cc = if right.low >= 0 {
                    IntCC::SignedGreaterThan
                } else {
                    IntCC::SignedLessThan
                };
                (difference, self.b.ins().icmp(cc, difference, a))
            }
            _ => return Ok(None),
        };
        self.fail_if(overflow, "integer overflow")?;
        Ok(Some(value))
    }
    fn call(&mut self, name: &str, args: &[Value], ret: Option<cl::Type>) -> Result<Option<Value>> {
        let id = if let Some(id) = self.env.imports.get(name) {
            *id
        } else {
            let ps: Vec<_> = args
                .iter()
                .map(|v| self.b.func.dfg.value_type(*v))
                .collect();
            let id = self
                .env
                .module
                .declare_function(
                    name,
                    Linkage::Import,
                    &signature(&self.env.module, &ps, ret),
                )
                .map_err(err)?;
            self.env.imports.insert(name.into(), id);
            id
        };
        Ok(self.call_id(id, args))
    }
    fn call_id(&mut self, id: FuncId, args: &[Value]) -> Option<Value> {
        let r = self.env.module.declare_func_in_func(id, self.b.func);
        if self
            .env
            .funcs
            .values()
            .any(|(user, f)| *user == id && f.source.external_symbol.is_none())
        {
            // All checked user functions link into this one native image,
            // including definitions emitted in other cached COFF units.
            self.b.func.dfg.ext_funcs[r].colocated = true;
        }
        let i = self.b.ins().call(r, args);
        self.b.inst_results(i).first().copied()
    }
    fn string(&mut self, s: &str) -> Result<Value> {
        if let Some(&id) = self.env.strings.get(s) {
            let g = self.env.module.declare_data_in_func(id, self.b.func);
            return Ok(self.b.ins().symbol_value(I64, g));
        }
        let name = format!("zn_literal_{}", self.env.serial);
        self.env.serial += 1;
        let id = self
            .env
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .map_err(err)?;
        let mut d = DataDescription::new();
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        d.define(bytes.into_boxed_slice());
        self.env.module.define_data(id, &d).map_err(err)?;
        self.env.strings.insert(s.into(), id);
        let g = self.env.module.declare_data_in_func(id, self.b.func);
        Ok(self.b.ins().symbol_value(I64, g))
    }
    fn address(&mut self, p: &Place) -> Value {
        let mut ty = self.local_types[p.local];
        let direct = matches!(self.locals[p.local], Storage::Columns { .. });
        let mut a = match self.locals[p.local] {
            Storage::Address(a) => a,
            Storage::Columns {
                data,
                capacity,
                index,
                vector,
            } => {
                let column = p.fields[0];
                let columns = self.env.types.soa_columns(vector).unwrap();
                if let Some(bases) = self.hoisted_columns.get(&p.local) {
                    let base = bases[column];
                    let slot = self.b.ins().imul_imm_s(index, columns[column].1 as i64);
                    self.b.ins().iadd(base, slot)
                } else {
                    let mut offset = self.b.ins().iconst(I64, 0);
                    for (_, width) in columns.iter().take(column) {
                        let bytes = self.b.ins().imul_imm_s(capacity, *width as i64);
                        let rounded = self.b.ins().iadd_imm_s(bytes, 7);
                        let rounded = self.b.ins().band_imm_s(rounded, -8);
                        offset = self.b.ins().iadd(offset, rounded);
                    }
                    let slot = self.b.ins().imul_imm_s(index, columns[column].1 as i64);
                    let offset = self.b.ins().iadd(offset, slot);
                    self.b.ins().iadd(data, offset)
                }
            }
            _ => panic!("checked place has no address"),
        };
        let mut offset = 0;
        for (depth, &index) in p.fields.iter().enumerate() {
            let Type::Named(id) = ty else { unreachable!() };
            if !direct || depth != 0 {
                offset += self.env.types.layouts[id].offsets[index];
            }
            let fields = match &self.env.types.definitions[id].kind {
                DataKind::Record(f)
                | DataKind::Enum(f)
                | DataKind::Vector(f)
                | DataKind::SoaVector(f)
                | DataKind::Map(f)
                | DataKind::Arena(f)
                | DataKind::Channel(f) => f,
            };
            ty = fields[index].ty;
        }
        if offset != 0 {
            a = self.b.ins().iadd_imm_s(a, offset as i64);
        }
        a
    }
    fn load(&mut self, p: &Place) -> Value {
        if p.fields.is_empty() {
            if let Storage::Variable(v) = self.locals[p.local] {
                let value = self.b.use_var(v);
                return if let Type::Float(n) = p.ty {
                    self.float_bits(value, n)
                } else {
                    value
                };
            }
        }
        let a = self.address(p);
        self.load_memory(p.ty, a)
    }
    fn buffer(&mut self, p: &Place) -> (Value, Value, crate::numeric::Integer) {
        if let Storage::Descriptor { data, len } = self.locals[p.local] {
            let Type::Slice(n) = p.ty else {
                unreachable!("scalar buffer descriptor")
            };
            return (self.b.use_var(data), self.b.use_var(len), n);
        }
        let a = self.address(p);
        match p.ty {
            Type::Fixed(n, len) => (a, self.b.ins().iconst(I64, len as i64), n),
            Type::Slice(_) | Type::Ints => {
                let n = if let Type::Slice(n) = p.ty {
                    n
                } else {
                    crate::numeric::Integer::I64
                };
                // Match the alias region used by native push/len. Loading an
                // ints descriptor without its region can reuse pre-push data.
                let flags = if p.ty == Type::Ints {
                    self.array_flags(false)
                } else {
                    MemFlagsData::new()
                };
                let data = self.b.ins().load(I64, flags, a, 0);
                let len = self.b.ins().load(I64, flags, a, 8);
                (data, len, n)
            }
            _ => unreachable!(),
        }
    }
    fn load_memory(&mut self, ty: Type, a: Value) -> Value {
        let mem = match ty.integer().map(|n| n.bits()).or_else(|| {
            if let Type::Float(n) = ty {
                Some(n.bits())
            } else {
                None
            }
        }) {
            Some(8) => I8,
            Some(16) => I16,
            Some(32) => I32,
            _ => raw_type(ty),
        };
        let v = self.b.ins().load(mem, MemFlagsData::new(), a, 0);
        if mem == raw_type(ty) {
            v
        } else if ty.integer().is_some_and(|n| n.signed()) {
            self.b.ins().sextend(I64, v)
        } else {
            self.b.ins().uextend(I64, v)
        }
    }
    fn store_memory(&mut self, ty: Type, a: Value, v: Value) {
        let mem = match ty.integer().map(|n| n.bits()).or_else(|| {
            if let Type::Float(n) = ty {
                Some(n.bits())
            } else {
                None
            }
        }) {
            Some(8) => I8,
            Some(16) => I16,
            Some(32) => I32,
            _ => raw_type(ty),
        };
        let v = if mem == raw_type(ty) {
            v
        } else {
            self.b.ins().ireduce(mem, v)
        };
        self.b.ins().store(MemFlagsData::new(), v, a, 0);
    }
    fn number_op(&mut self, ty: Type, op: &str, a: Value, b: Value) -> Result<Value> {
        let n = ty.integer().unwrap();
        let a = self.narrow_integer(a, n);
        let b = self.narrow_integer(b, n);
        let (value, overflow) = match (op, n.signed()) {
            ("+" | "saturating_add", true) => self.b.ins().sadd_overflow(a, b),
            ("-" | "saturating_sub", true) => self.b.ins().ssub_overflow(a, b),
            ("*" | "*literal", true) => self.b.ins().smul_overflow(a, b),
            ("+" | "saturating_add", false) => self.b.ins().uadd_overflow(a, b),
            ("-" | "saturating_sub", false) => self.b.ins().usub_overflow(a, b),
            ("*" | "*literal", false) => self.b.ins().umul_overflow(a, b),
            ("/" | "%", signed) => {
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                self.fail_if(
                    zero,
                    if op == "/" {
                        "division by zero"
                    } else {
                        "remainder by zero"
                    },
                )?;
                if signed {
                    let min = if n.bits() == 64 {
                        i64::MIN
                    } else {
                        -(1i64 << (n.bits() - 1))
                    };
                    let min = self.b.ins().icmp_imm_s(IntCC::Equal, a, min);
                    let neg = self.b.ins().icmp_imm_s(IntCC::Equal, b, -1);
                    let overflow = self.b.ins().band(min, neg);
                    self.fail_if(overflow, "integer overflow")?;
                }
                let value = match (op, signed) {
                    ("/", true) => self.b.ins().sdiv(a, b),
                    ("%", true) => self.b.ins().srem(a, b),
                    ("/", false) => self.b.ins().udiv(a, b),
                    _ => self.b.ins().urem(a, b),
                };
                return Ok(self.canonical_integer(value, n));
            }
            _ => return Err(err("unknown checked integer operation")),
        };
        let value = if op.starts_with("saturating_") {
            let width = integer_type(n);
            let min = if n.signed() {
                1u64 << (n.bits() - 1)
            } else {
                0
            };
            let max = if n.signed() {
                min - 1
            } else {
                u64::MAX >> (64 - n.bits())
            };
            let min = self.b.ins().iconst(width, min as i64);
            let max = self.b.ins().iconst(width, max as i64);
            let limit = if n.signed() {
                let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, b, 0);
                if op == "saturating_add" {
                    self.b.ins().select(negative, min, max)
                } else {
                    self.b.ins().select(negative, max, min)
                }
            } else if op == "saturating_add" {
                max
            } else {
                min
            };
            self.b.ins().select(overflow, limit, value)
        } else {
            self.fail_if(overflow, "integer overflow")?;
            value
        };
        Ok(self.canonical_integer(value, n))
    }
    fn narrow_integer(&mut self, value: Value, n: crate::numeric::Integer) -> Value {
        if n.bits() == 64 {
            value
        } else {
            self.b.ins().ireduce(integer_type(n), value)
        }
    }
    fn canonical_integer(&mut self, value: Value, n: crate::numeric::Integer) -> Value {
        if n.bits() == 64 {
            value
        } else if n.signed() {
            self.b.ins().sextend(I64, value)
        } else {
            self.b.ins().uextend(I64, value)
        }
    }
    /// Checked integer conversion with `zn_num_cast` semantics: the canonical
    /// source value is returned unchanged when it lies in the target's range,
    /// otherwise `integer conversion out of range`. The test is omitted only
    /// when the proven source interval already lies inside the target range.
    fn convert_integer(&mut self, source: &Place, value: Value, target: Type) -> Result<Value> {
        use crate::numeric::Integer;
        let from = source.ty.integer().unwrap();
        let to = target.integer().unwrap();
        let (to_low, to_high) = if to == Integer::U64 {
            (0, u64::MAX as i128)
        } else if to == Integer::I64 {
            (i64::MIN as i128, i64::MAX as i128)
        } else {
            let b = crate::int_ranges::type_bounds(target);
            (b.low as i128, b.high as i128)
        };
        // Contract M6: an unsigned source may carry a proven mask bound
        // (`bit_and(x, K)` lies in [0, K]); a raw u64 has no other fact.
        let mask = self.integer_ranges.unsigned_high(source).map(i128::from);
        let (low, high) = if from == Integer::U64 {
            (0, mask.unwrap_or(u64::MAX as i128))
        } else {
            let b = self.guarded_bounds(source);
            (
                b.low as i128,
                mask.map_or(b.high as i128, |m| m.min(b.high as i128)),
            )
        };
        if to_low <= low && high <= to_high {
            return Ok(value);
        }
        let invalid = if from == Integer::U64 {
            // Raw unsigned source: only an upper limit applies.
            self.b
                .ins()
                .icmp_imm_s(IntCC::UnsignedGreaterThan, value, to_high as u64 as i64)
        } else if to == Integer::U64 {
            self.b.ins().icmp_imm_s(IntCC::SignedLessThan, value, 0)
        } else {
            // Signed canonical source: one unsigned test of value - MIN.
            let shifted = self.b.ins().iadd_imm_s(value, (-to_low) as i64);
            self.b.ins().icmp_imm_s(
                IntCC::UnsignedGreaterThan,
                shifted,
                (to_high - to_low) as u64 as i64,
            )
        };
        self.fail_if(invalid, "integer conversion out of range")?;
        Ok(value)
    }
    /// An integer-to-float or float-to-float conversion that
    /// [`exact_float_conversion`] accepts, as canonical IEEE bits. The runtime
    /// (`zn_fp_convert`, MSVC x64) evaluates these with the same SSE
    /// conversions: `cvtsi2ss`/`cvtsi2sd` of the canonical signed value, the
    /// correctly rounded halving sequence for a u64 with its top bit set, and
    /// `cvtps2pd`/`cvtpd2ps`, whose scalar forms `fpromote`/`fdemote` lower to
    /// (identical rounding, infinities, signed zeros and NaN quieting and
    /// payload handling); same-width float conversions return the raw bits.
    fn float_conversion(&mut self, value: Value, from: Type, to: Type) -> Value {
        let Type::Float(target) = to else {
            unreachable!("float conversion target")
        };
        let ty = ct(to);
        let converted = match from {
            Type::Float(source) if source == target => return value,
            Type::Float(source) => {
                let float = self.float_value(value, source);
                if target == crate::numeric::Float::F64 {
                    self.b.ins().fpromote(ty, float)
                } else {
                    self.b.ins().fdemote(ty, float)
                }
            }
            Type::Number(crate::numeric::Integer::U64) => self.b.ins().fcvt_from_uint(ty, value),
            // Every other integer is a canonical (sign- or zero-extended) i64
            // whose value the signed conversion takes exactly.
            _ => self.b.ins().fcvt_from_sint(ty, value),
        };
        self.float_bits(converted, target)
    }
    fn integer_intrinsic(&mut self, name: &str, args: &[Place]) -> Value {
        let n = args[0].ty.integer().unwrap();
        let raw = self.load(&args[0]);
        let a = self.narrow_integer(raw, n);
        let b = args.get(1).map(|p| {
            let raw = self.load(p);
            if matches!(name, "shl" | "shr" | "rotl" | "rotr") {
                self.b.ins().band_imm_s(raw, (n.bits() - 1) as i64)
            } else {
                self.narrow_integer(raw, n)
            }
        });
        let value = match name {
            "wrapping_add" => self.b.ins().iadd(a, b.unwrap()),
            "wrapping_sub" => self.b.ins().isub(a, b.unwrap()),
            "wrapping_mul" => self.b.ins().imul(a, b.unwrap()),
            "bit_and" => self.b.ins().band(a, b.unwrap()),
            "bit_or" => self.b.ins().bor(a, b.unwrap()),
            "bit_xor" => self.b.ins().bxor(a, b.unwrap()),
            "bit_not" => self.b.ins().bnot(a),
            "shl" => self.b.ins().ishl(a, b.unwrap()),
            "shr" if n.signed() => self.b.ins().sshr(a, b.unwrap()),
            "shr" => self.b.ins().ushr(a, b.unwrap()),
            "rotl" => self.b.ins().rotl(a, b.unwrap()),
            "rotr" => self.b.ins().rotr(a, b.unwrap()),
            _ => unreachable!("checked integer intrinsic"),
        };
        self.canonical_integer(value, n)
    }
    fn store(&mut self, p: &Place, v: Value) {
        if p.fields.is_empty() {
            if let Storage::Variable(var) = self.locals[p.local] {
                let value = if let Type::Float(n) = p.ty {
                    self.float_value(v, n)
                } else {
                    v
                };
                self.b.def_var(var, value);
                return;
            }
        }
        let a = self.address(p);
        self.store_memory(p.ty, a, v);
    }
    fn descriptor(&mut self, owner: &Place) -> (Value, Value) {
        if let Storage::Descriptor { data, len } = self.locals[owner.local] {
            return (self.b.use_var(data), self.b.use_var(len));
        }
        if let Some(&metadata) = self
            .array_metadata
            .get(&crate::immutable_descriptors::key(owner))
        {
            return metadata;
        }
        let address = self.address(owner);
        let flags = self.array_flags(false);
        let data = self.b.ins().load(I64, flags, address, 0);
        let len = self.b.ins().load(I64, flags, address, 8);
        let len = if owner.ty == Type::Text {
            self.text_length(len)
        } else {
            len
        };
        // Like `len`, remember the loaded descriptor until an operation that
        // may replace it (every such operation invalidates its owner's entry
        // or all transient metadata; nested regions drop it in `block`).
        if crate::immutable_descriptors::descriptor(owner.ty, self.env.types)
            && !matches!(self.locals[owner.local], Storage::Columns { .. })
        {
            self.array_metadata
                .insert(crate::immutable_descriptors::key(owner), (data, len));
        }
        (data, len)
    }
    /// `valid ? len : 0` for a span descriptor, where valid is the runtime's
    /// view invariant (contract S5) `len <= INT64_MAX / size` and
    /// `(len == 0 || data != NULL)`. Under it, `position <u limit` alone also
    /// rejects every negative position. Pure, so one dominating computation
    /// serves every later access to the same descriptor values.
    fn span_limit(&mut self, data: Value, len: Value, size: usize) -> Value {
        if let Some(&limit) = self.spans.limits.get(&(data, len, size)) {
            return limit;
        }
        let too_long = self.b.ins().icmp_imm_s(
            IntCC::UnsignedGreaterThan,
            len,
            (i64::MAX as u64 / size as u64) as i64,
        );
        let null = self.b.ins().icmp_imm_s(IntCC::Equal, data, 0);
        let nonempty = self.b.ins().icmp_imm_s(IntCC::NotEqual, len, 0);
        let missing = self.b.ins().band(null, nonempty);
        let invalid = self.b.ins().bor(too_long, missing);
        let zero = self.b.ins().iconst(I64, 0);
        let limit = self.b.ins().select(invalid, zero, len);
        self.spans.limits.insert((data, len, size), limit);
        limit
    }
    fn vector_slot(&mut self, owner: &Place, index: &Place, size: usize) -> Result<Value> {
        if size != 0 {
            return self.checked_slot(owner, index, size);
        }
        let (data, len) = self.descriptor(owner);
        let position = self.load(index);
        let span = matches!(owner.ty, Type::VectorView(_));
        if !self.guarded_index(owner, index) {
            let outside = self
                .b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, position, len);
            let invalid = if span {
                let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, position, 0);
                self.b.ins().bor(outside, negative)
            } else {
                outside
            };
            self.fail_if(
                invalid,
                if span {
                    "vector region index out of bounds"
                } else {
                    "vector index out of bounds"
                },
            )?;
        }
        // Span runtime validates the entire length before its address formula;
        // nominal vectors validate the indexed byte offset with zn_vec_bytes.
        let extent = if span { len } else { position };
        let too_long = if size == 0 {
            self.b.ins().iconst(I8, 1)
        } else {
            self.b.ins().icmp_imm_s(
                IntCC::UnsignedGreaterThan,
                extent,
                (u64::MAX / size as u64) as i64,
            )
        };
        let invalid = if span {
            let null = self.b.ins().icmp_imm_s(IntCC::Equal, data, 0);
            self.b.ins().bor(too_long, null)
        } else {
            let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, position, 0);
            self.b.ins().bor(too_long, negative)
        };
        self.fail_if(
            invalid,
            if span {
                "vector region storage overflow"
            } else {
                "vector capacity overflow"
            },
        )?;
        let offset = self.b.ins().imul_imm_s(position, size as i64);
        Ok(self.b.ins().iadd(data, offset))
    }
    /// Nonzero-width element slot with exactly one unsigned compare on the
    /// hot path. Nominal `zn_vec` descriptors always satisfy the runtime
    /// invariant `len <= INT64_MAX / size`, so `position >=u len` also rejects
    /// every negative position and no address overflow remains. Views may be
    /// foreign, so their hot compare uses the hoisted `span_limit` (0 for any
    /// descriptor outside the runtime invariant); a cold block then replays
    /// the exact original checks: index first (unless proven), including the
    /// explicit negative guard, then storage, and otherwise performs the
    /// access, so even an unusual but storable foreign view keeps its result.
    fn checked_slot(&mut self, owner: &Place, index: &Place, size: usize) -> Result<Value> {
        let (data, len) = self.descriptor(owner);
        let position = self.load(index);
        let guarded = self.guarded_index(owner, index);
        let valid = self.valid_spans.contains(&(data, len, size));
        if !matches!(owner.ty, Type::VectorView(_)) {
            if !guarded {
                let outside = self
                    .b
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, position, len);
                self.fail_if(outside, "vector index out of bounds")?;
            }
        } else if valid {
            // Versioned body: this view satisfies the runtime invariant, so
            // `position >=u len` is exactly the index fault (it rejects every
            // negative position too) and no storage fault can follow; a
            // proven index needs no test at all.
            if !guarded {
                let outside = self
                    .b
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, position, len);
                self.fail_if(outside, "vector region index out of bounds")?;
            }
        } else {
            let limit = self.span_limit(data, len, size);
            let outside = self
                .b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, position, limit);
            let diagnose = self.b.create_block();
            let good = self.b.create_block();
            self.b.set_cold_block(diagnose);
            self.b.ins().brif(outside, diagnose, &[], good, &[]);
            self.b.switch_to_block(diagnose);
            if !guarded {
                let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, position, 0);
                let beyond = self
                    .b
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, position, len);
                let bad_index = self.b.ins().bor(negative, beyond);
                self.fail_if(bad_index, "vector region index out of bounds")?;
                if let Some(current) = self.b.current_block() {
                    self.b.set_cold_block(current);
                }
            }
            let too_long = self.b.ins().icmp_imm_s(
                IntCC::UnsignedGreaterThan,
                len,
                (u64::MAX / size as u64) as i64,
            );
            let null = self.b.ins().icmp_imm_s(IntCC::Equal, data, 0);
            let storage = self.b.ins().bor(too_long, null);
            self.fail_if(storage, "vector region storage overflow")?;
            if let Some(current) = self.b.current_block() {
                self.b.set_cold_block(current);
            }
            self.b.ins().jump(good, &[]);
            self.b.switch_to_block(good);
        }
        // Both paths reaching here proved `0 <= position <u len`: nominal
        // vectors by the unsigned compare under the runtime invariant
        // `len <= INT64_MAX / size`; views by the hot compare against the
        // valid limit, or by the replayed index and storage checks
        // (`len <= UINT64_MAX / size`, below 2^63 only for size >= 2) on
        // the cold path.
        let span = matches!(owner.ty, Type::VectorView(_)) && !valid;
        let high = if span {
            (u64::MAX / size as u64 - 1).min(i64::MAX as u64) as i64
        } else {
            i64::MAX / size as i64 - 1
        };
        self.index_facts(index, len, high, !span || size >= 2);
        let offset = self.b.ins().imul_imm_s(position, size as i64);
        Ok(self.b.ins().iadd(data, offset))
    }
    fn arena_present(&mut self, container: Value, id: Value) -> Value {
        let slot = self.b.ins().band_imm_s(id, u32::MAX as i64);
        let generation = self.b.ins().ushr_imm_s(id, 32);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, generation, 0);
        let slots = self.b.ins().load(I64, MemFlagsData::new(), container, 32);
        let inside = self.b.ins().icmp(IntCC::UnsignedLessThan, slot, slots);
        let valid = self.b.ins().band(nonzero, inside);
        let check = self.b.create_block();
        let absent = self.b.create_block();
        let end = self.b.create_block();
        self.b.append_block_param(end, I8);
        self.b.ins().brif(valid, check, &[], absent, &[]);
        self.b.switch_to_block(absent);
        let no = self.b.ins().iconst(I8, 0);
        self.b.ins().jump(end, &[no.into()]);
        self.b.switch_to_block(check);
        let states = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
        let state_address = self.b.ins().iadd(states, slot);
        let state = self.b.ins().load(I8, MemFlagsData::new(), state_address, 0);
        let occupied = self.b.ins().icmp_imm_s(IntCC::Equal, state, 1);
        // Keep the occupied check ahead of the generation-array load, matching
        // the runtime's short-circuit reads even on a rejected free slot.
        let generation_check = self.b.create_block();
        self.b
            .ins()
            .brif(occupied, generation_check, &[], absent, &[]);
        self.b.switch_to_block(generation_check);
        let generations = self.b.ins().load(I64, MemFlagsData::new(), container, 8);
        let offset = self.b.ins().ishl_imm_s(slot, 2);
        let address = self.b.ins().iadd(generations, offset);
        let actual = self.b.ins().load(I32, MemFlagsData::new(), address, 0);
        let expected = self.b.ins().ireduce(I32, generation);
        let matches = self.b.ins().icmp(IntCC::Equal, actual, expected);
        self.b.ins().jump(end, &[matches.into()]);
        self.b.switch_to_block(end);
        self.b.block_params(end)[0]
    }
    fn arena_slot(&mut self, container: Value, id: Value, size: usize) -> Result<Value> {
        let present = self.arena_present(container, id);
        let invalid = self.b.ins().icmp_imm_s(IntCC::Equal, present, 0);
        self.fail_if(invalid, "stale or invalid arena handle")?;
        let data = self.b.ins().load(I64, MemFlagsData::new(), container, 0);
        let slot = self.b.ins().band_imm_s(id, u32::MAX as i64);
        let offset = self.b.ins().imul_imm_s(slot, size as i64);
        Ok(self.b.ins().iadd(data, offset))
    }
    fn fixed_builtin(&mut self, d: &Place, name: &str, args: &[Place]) -> Result<()> {
        let (data, count, number) = self.buffer(&args[0]);
        if name == "fixed_len" {
            self.store(d, count);
            return Ok(());
        }
        let index = self.load(&args[1]);
        let wire = name.starts_with("read_u") || name.starts_with("try_write_u");
        let width = if wire {
            if name.starts_with("read_u") {
                d.ty.integer().unwrap().bits() / 8
            } else {
                args[2].ty.integer().unwrap().bits() / 8
            }
        } else {
            number.bits() / 8
        };
        let write =
            matches!(name, "fixed_set" | "fixed_try_set") || name.starts_with("try_write_u");
        let trying = name == "fixed_try_set" || name.starts_with("try_write_u");
        // A compile-time buffer length with a proven index interval: every
        // access is in bounds, so the check (and a try_* failure path) is dead.
        let proven = if let Type::Fixed(_, length) = args[0].ty {
            let bounds = self.guarded_bounds(&args[1]);
            let limit = if wire {
                length as i128 - width as i128
            } else {
                length as i128 - 1
            };
            bounds.low >= 0 && bounds.high as i128 <= limit
        } else {
            false
        };
        // Contract N2: a big-endian write is inline only under the same proof
        // as the little-endian one (`0 <= low && high + width <= N` on a
        // fixed buffer). Otherwise the runtime's `zn_wire_write_be` applies the
        // exact N2 predicate and leaves the buffer unchanged on failure.
        let big_endian_write = write && name.ends_with("_be");
        if big_endian_write && !proven {
            let value = self.load(&args[2]);
            let width = self.b.ins().iconst(I64, width as i64);
            // Argument order (N2): value before width, unlike zn_wire_write.
            let stored = self
                .call(
                    "zn_wire_write_be",
                    &[data, count, index, value, width],
                    Some(I8),
                )?
                .unwrap();
            self.store(d, stored);
            return Ok(());
        }
        if proven {
            let offset = if wire {
                index
            } else {
                self.b.ins().imul_imm_s(index, width as i64)
            };
            let address = self.b.ins().iadd(data, offset);
            if big_endian_write {
                // Proven in bounds: store the value's low `width` bytes most
                // significant first (ireduce + bswap + store), result true.
                let n = args[2].ty.integer().unwrap();
                let value = self.load(&args[2]);
                let value = self.narrow_integer(value, n);
                let value = self.b.ins().bswap(value);
                self.b.ins().store(MemFlagsData::new(), value, address, 0);
                let stored = self.b.ins().iconst(I8, 1);
                self.store(d, stored);
            } else if write {
                let value = self.load(&args[2]);
                self.store_memory(args[2].ty, address, value);
                if trying {
                    let stored = self.b.ins().iconst(I8, 1);
                    self.store(d, stored);
                }
            } else {
                let mut value = self.load_memory(d.ty, address);
                if name.ends_with("_be") {
                    let n = d.ty.integer().unwrap();
                    value = self.narrow_integer(value, n);
                    value = self.b.ins().bswap(value);
                    value = self.canonical_integer(value, n);
                }
                self.store(d, value);
            }
            return Ok(());
        }
        let invalid = if wire {
            let short = self
                .b
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThan, count, width as i64);
            let last = self.b.ins().iadd_imm_s(count, -(width as i64));
            let outside = self.b.ins().icmp(IntCC::UnsignedGreaterThan, index, last);
            self.b.ins().bor(short, outside)
        } else {
            self.b
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, index, count)
        };
        let end = if trying {
            let write = self.b.create_block();
            let end = self.b.create_block();
            self.b.ins().brif(invalid, end, &[], write, &[]);
            self.b.switch_to_block(write);
            Some(end)
        } else {
            self.fail_if(
                invalid,
                if wire {
                    "packet read out of bounds"
                } else {
                    "array index out of bounds"
                },
            )?;
            None
        };
        let offset = if wire {
            index
        } else {
            self.b.ins().imul_imm_s(index, width as i64)
        };
        let address = self.b.ins().iadd(data, offset);
        if write {
            let value = self.load(&args[2]);
            self.store_memory(args[2].ty, address, value);
        } else {
            let mut value = self.load_memory(d.ty, address);
            if name.ends_with("_be") {
                let n = d.ty.integer().unwrap();
                value = self.narrow_integer(value, n);
                value = self.b.ins().bswap(value);
                value = self.canonical_integer(value, n);
            }
            self.store(d, value);
        }
        if let Some(end) = end {
            self.b.ins().jump(end, &[]);
            self.b.switch_to_block(end);
            let valid = self.b.ins().icmp_imm_s(IntCC::Equal, invalid, 0);
            self.store(d, valid);
        }
        Ok(())
    }
    /// 1.3 OS/runtime builtins. Texts are borrowed by address (`const zn_text*`)
    /// and every result comes back through the `zn_abi_*` wrappers. A byte buffer
    /// passes its (data, len): the runtime reads or writes those bytes, never the
    /// descriptor. `None` for every other builtin.
    fn os_builtin(&mut self, d: &Place, name: &str, args: &[Place]) -> Result<Option<()>> {
        match name {
            "try_read_bytes" | "try_write_bytes" => {
                let path = self.address(&args[0]);
                let (data, len, _) = self.buffer(&args[1]);
                let (helper, ty) = if name == "try_read_bytes" {
                    ("zn_abi_try_read_bytes", I64)
                } else {
                    ("zn_abi_try_write_bytes", I8)
                };
                let result = self.call(helper, &[path, data, len], Some(ty))?.unwrap();
                self.store(d, result);
            }
            "create_dir" | "remove_file" => {
                let path = self.address(&args[0]);
                let helper = if name == "create_dir" {
                    "zn_abi_create_dir"
                } else {
                    "zn_abi_remove_file"
                };
                let result = self.call(helper, &[path], Some(I8))?.unwrap();
                self.store(d, result);
            }
            "env_var" => {
                let out = self.address(d);
                let variable = self.address(&args[0]);
                self.call("zn_abi_env_var", &[out, variable], None)?;
            }
            "net_serve_once" => {
                // Mirrors net_request: one `zn_net_result` is written to the
                // NetResult destination.
                let out = self.address(d);
                let endpoint = self.address(&args[0]);
                let status = self.load(&args[1]);
                let reply = self.address(&args[2]);
                self.call(
                    "zn_abi_net_serve_once",
                    &[out, endpoint, status, reply],
                    None,
                )?;
            }
            "http_request" => {
                // Same shape as net_request: url, body and the optional texts
                // of header lines are borrowed by address (null when absent),
                // and one `zn_net_result` lands in the destination.
                let out = self.address(d);
                let url = self.address(&args[0]);
                let body = self.address(&args[1]);
                let headers = match args.get(2) {
                    Some(headers) => self.address(headers),
                    None => self.b.ins().iconst(I64, 0),
                };
                self.call("zn_abi_http_request", &[out, url, body, headers], None)?;
            }
            _ => return Ok(None),
        }
        Ok(Some(()))
    }
    /// `(data, byte count)` of a borrowed `text`/`str`, `[u8; N]` or `&[u8]`
    /// hash input (contracts N3/N4). A text length carries the static tag in
    /// its top bit, so the byte count is `len & ~ZN_TEXT_STATIC_TAG`.
    fn byte_input(&mut self, p: &Place) -> (Value, Value) {
        match p.ty {
            Type::Text => self.descriptor(p),
            Type::TextView => {
                let (data, raw) = self.descriptor(p);
                (data, self.text_length(raw))
            }
            _ => {
                let (data, len, _) = self.buffer(p);
                (data, len)
            }
        }
    }
    fn copy_bytes(&mut self, d: Value, s: Value, n: usize) -> Result<()> {
        if n <= 64 {
            let mut off = 0;
            while off < n {
                let t = if n - off >= 8 { I64 } else { I8 };
                let v = self.b.ins().load(t, MemFlagsData::new(), s, off as i32);
                self.b.ins().store(MemFlagsData::new(), v, d, off as i32);
                off += t.bytes() as usize;
            }
        } else {
            let size = self.b.ins().iconst(I64, n as i64);
            self.call("memmove", &[d, s, size], Some(I64))?;
        }
        Ok(())
    }
    /// `memcpy(d, s, n)` of one SoA column value, in the widest naturally
    /// sized chunks (the runtime copies the same bytes with memcpy).
    fn copy_column(&mut self, d: Value, s: Value, n: usize) -> Result<()> {
        if n > 64 {
            return self.copy_bytes(d, s, n);
        }
        let mut off = 0;
        while off < n {
            let t = match n - off {
                8.. => I64,
                4..=7 => I32,
                2..=3 => I16,
                _ => I8,
            };
            let v = self.b.ins().load(t, MemFlagsData::new(), s, off as i32);
            self.b.ins().store(MemFlagsData::new(), v, d, off as i32);
            off += t.bytes() as usize;
        }
        Ok(())
    }
    fn zero_bytes(&mut self, d: Value, n: usize) -> Result<()> {
        if n <= 64 {
            let mut off = 0;
            while off < n {
                let t = if n - off >= 8 { I64 } else { I8 };
                let z = self.b.ins().iconst(t, 0);
                self.b.ins().store(MemFlagsData::new(), z, d, off as i32);
                off += t.bytes() as usize;
            }
        } else {
            let size = self.b.ins().iconst(I64, n as i64);
            let z = self.b.ins().iconst(I32, 0);
            self.call("memset", &[d, z, size], Some(I64))?;
        }
        Ok(())
    }
    fn zero(&mut self, p: &Place) -> Result<()> {
        if let Storage::Descriptor { data, len } = self.locals[p.local] {
            let zero = self.b.ins().iconst(I64, 0);
            self.b.def_var(data, zero);
            self.b.def_var(len, zero);
        } else if scalar(p.ty) {
            let z = self.b.ins().iconst(raw_type(p.ty), 0);
            self.store(p, z);
        } else if p.ty != Type::Unit {
            let a = self.address(p);
            self.zero_bytes(a, self.env.types.layout(p.ty).size)?;
        }
        Ok(())
    }
    fn transfer(&mut self, d: &Place, s: &Place) -> Result<()> {
        if scalar(d.ty) {
            let v = self.load(s);
            self.store(d, v);
        } else if d.ty != Type::Unit {
            let da = self.address(d);
            let sa = self.address(s);
            self.copy_bytes(da, sa, self.env.types.layout(d.ty).size)?;
        }
        Ok(())
    }
    fn field(&self, p: &Place, index: usize, ty: Type) -> Place {
        let mut q = p.clone();
        q.fields.push(index);
        q.ty = ty;
        q
    }
    fn scratch_address(&mut self, size: usize, align: usize) -> Value {
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size as u32,
            align.trailing_zeros() as u8,
        ));
        self.b.ins().stack_addr(I64, slot, 0)
    }
    fn operand_address(&mut self, p: &Place) -> Value {
        if !scalar(p.ty) {
            return self.address(p);
        }
        let layout = self.env.types.layout(p.ty);
        let address = self.scratch_address(layout.size, layout.align);
        let value = self.load(p);
        self.store_memory(p.ty, address, value);
        address
    }
    fn copy_operand_at(&mut self, dst: Value, p: &Place) -> Result<()> {
        if scalar(p.ty) {
            let value = self.load(p);
            self.store_memory(p.ty, dst, value);
        } else {
            let source = self.address(p);
            self.copy_bytes(dst, source, self.env.types.layout(p.ty).size)?;
        }
        Ok(())
    }
    fn arena_elements(&mut self, ty: Type, dst: Value, src: Option<Value>) -> Result<()> {
        let element = self.env.types.arena_element(ty).unwrap();
        if !element.owned() {
            return Ok(());
        }
        let cursor = self.scratch_address(8, 8);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().store(MemFlagsData::new(), zero, cursor, 0);
        let width = self
            .b
            .ins()
            .iconst(I64, self.env.types.layout(element).size as i64);
        let head = self.b.create_block();
        let body = self.b.create_block();
        let end = self.b.create_block();
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let entry = self
            .call("zn_arena_next_slot", &[dst, cursor, width], Some(I64))?
            .unwrap();
        let more = self.b.ins().icmp_imm_s(IntCC::NotEqual, entry, 0);
        self.b.ins().brif(more, body, &[], end, &[]);
        self.b.switch_to_block(body);
        if let Some(src) = src {
            let next = self.b.ins().load(I64, MemFlagsData::new(), cursor, 0);
            let index = self.b.ins().iadd_imm_s(next, -1);
            let original = self
                .call("zn_arena_slot_at", &[src, index, width], Some(I64))?
                .unwrap();
            self.clone_at(element, entry, original)?;
        } else {
            self.drop_at(element, entry)?;
        }
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(end);
        Ok(())
    }
    fn arena_builtin(&mut self, d: &Place, name: &str, args: &[Place]) -> Result<()> {
        let element = self.env.types.arena_element(args[0].ty).unwrap();
        let container = self.address(&args[0]);
        if name == "arena_len" {
            let len = self.b.ins().load(I64, MemFlagsData::new(), container, 24);
            self.store(d, len);
            return Ok(());
        }
        let width = self
            .b
            .ins()
            .iconst(I64, self.env.types.layout(element).size as i64);
        if name == "arena_alloc" {
            let id = self
                .call("zn_arena_alloc_slot", &[container, width], Some(I64))?
                .unwrap();
            let slot = self.arena_slot(container, id, self.env.types.layout(element).size)?;
            self.copy_operand_at(slot, &args[1])?;
            self.store(d, id);
            return Ok(());
        }
        let id = self.load(&args[1]);
        match name {
            "arena_contains_id" => {
                let present = self.arena_present(container, id);
                self.store(d, present);
            }
            "arena_free" => {
                let present = self.arena_present(container, id);
                let remove = self.b.create_block();
                let end = self.b.create_block();
                self.b.ins().brif(present, remove, &[], end, &[]);
                self.b.switch_to_block(remove);
                if element.owned() {
                    let slot =
                        self.arena_slot(container, id, self.env.types.layout(element).size)?;
                    self.drop_at(element, slot)?;
                }
                self.call("zn_arena_erase_slot", &[container, id, width], None)?;
                self.b.ins().jump(end, &[]);
                self.b.switch_to_block(end);
                self.store(d, present);
            }
            "arena_clone_get" => {
                let slot = self.arena_slot(container, id, self.env.types.layout(element).size)?;
                if scalar(element) {
                    let value = self.load_memory(element, slot);
                    self.store(d, value);
                } else {
                    let dest = self.address(d);
                    self.clone_at(element, dest, slot)?;
                }
            }
            _ => unreachable!("unknown checked arena operation"),
        }
        Ok(())
    }
    fn map_elements(&mut self, ty: Type, dst: Value, src: Option<Value>) -> Result<()> {
        let layout = self.env.types.map_entry_layout(ty).unwrap();
        if !layout.key.owned() && !layout.value.owned() {
            return Ok(());
        }
        let cursor = self.scratch_address(8, 8);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().store(MemFlagsData::new(), zero, cursor, 0);
        let width = self.b.ins().iconst(I64, layout.width as i64);
        let head = self.b.create_block();
        let body = self.b.create_block();
        let end = self.b.create_block();
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let entry = self
            .call("zn_map_next_slot", &[dst, cursor, width], Some(I64))?
            .unwrap();
        let more = self.b.ins().icmp_imm_s(IntCC::NotEqual, entry, 0);
        self.b.ins().brif(more, body, &[], end, &[]);
        self.b.switch_to_block(body);
        if let Some(src) = src {
            let next = self.b.ins().load(I64, MemFlagsData::new(), cursor, 0);
            let index = self.b.ins().iadd_imm_s(next, -1);
            let original = self
                .call("zn_map_slot_at", &[src, index, width], Some(I64))?
                .unwrap();
            if layout.key.owned() {
                self.clone_at(layout.key, entry, original)?;
            }
            if layout.value.owned() {
                let dest = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
                let source = self
                    .b
                    .ins()
                    .iadd_imm_s(original, layout.value_offset as i64);
                self.clone_at(layout.value, dest, source)?;
            }
        } else {
            if layout.value.owned() {
                let value = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
                self.drop_at(layout.value, value)?;
            }
            if layout.key.owned() {
                self.drop_at(layout.key, entry)?;
            }
        }
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(end);
        Ok(())
    }
    fn map_builtin(&mut self, d: &Place, name: &str, args: &[Place]) -> Result<()> {
        let layout = self.env.types.map_entry_layout(args[0].ty).unwrap();
        let container = self.address(&args[0]);
        if name == "map_len" {
            let len = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
            self.store(d, len);
            return Ok(());
        }
        if name == "map_keys" {
            // The runtime walks the occupied slots and writes a new texts/ints
            // owner (keys are cloned) to the destination.
            let out = self.address(d);
            let width = self.b.ins().iconst(I64, layout.width as i64);
            let symbol = if layout.key == Type::Text {
                "zn_abi_map_keys_text"
            } else {
                "zn_abi_map_keys_int"
            };
            self.call(symbol, &[out, container, width], None)?;
            return Ok(());
        }
        let key = self.operand_address(&args[1]);
        let code = self.b.ins().iconst(I64, layout.key_code);
        let width = self.b.ins().iconst(I64, layout.width as i64);
        if name == "map_insert" {
            let replaced = self.scratch_address(1, 1);
            let entry = self
                .call(
                    "zn_map_insert_slot",
                    &[container, key, code, width, replaced],
                    Some(I64),
                )?
                .unwrap();
            if layout.key.owned() || layout.value.owned() {
                let drop = self.b.create_block();
                let ready = self.b.create_block();
                let old = self.b.ins().load(I8, MemFlagsData::new(), replaced, 0);
                self.b.ins().brif(old, drop, &[], ready, &[]);
                self.b.switch_to_block(drop);
                if layout.value.owned() {
                    let value = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
                    self.drop_at(layout.value, value)?;
                }
                if layout.key.owned() {
                    self.drop_at(layout.key, entry)?;
                }
                self.b.ins().jump(ready, &[]);
                self.b.switch_to_block(ready);
            }
            self.copy_operand_at(entry, &args[1])?;
            let value = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
            self.copy_operand_at(value, &args[2])?;
            return Ok(());
        }
        let entry = self
            .call("zn_map_find", &[container, key, code, width], Some(I64))?
            .unwrap();
        let present = self.b.ins().icmp_imm_s(IntCC::NotEqual, entry, 0);
        match name {
            "map_contains_key" => self.store(d, present),
            "map_remove" => {
                let remove = self.b.create_block();
                let end = self.b.create_block();
                self.b.ins().brif(present, remove, &[], end, &[]);
                self.b.switch_to_block(remove);
                if layout.value.owned() {
                    let value = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
                    self.drop_at(layout.value, value)?;
                }
                if layout.key.owned() {
                    self.drop_at(layout.key, entry)?;
                }
                self.call("zn_map_erase_slot", &[container, entry, width], None)?;
                self.b.ins().jump(end, &[]);
                self.b.switch_to_block(end);
                self.store(d, present);
            }
            "map_clone_get" => {
                let missing = self.b.ins().icmp_imm_s(IntCC::Equal, entry, 0);
                self.fail_if(missing, "map key not found")?;
                let value = self.b.ins().iadd_imm_s(entry, layout.value_offset as i64);
                if scalar(layout.value) {
                    let result = self.load_memory(layout.value, value);
                    self.store(d, result);
                } else {
                    let destination = self.address(d);
                    self.clone_at(layout.value, destination, value)?;
                }
            }
            _ => unreachable!("unknown checked map builtin"),
        }
        Ok(())
    }
    /// The push/pop slot of a nominal `zn_vec` {data, len, cap}. The common
    /// case is inline: push with spare capacity, pop of a nonempty vector,
    /// whenever the runtime's `zn_vec_bytes(index, width)` would accept the
    /// index (`index <= min(INT64_MAX, SIZE_MAX / width)`). Then the result
    /// and memory effects equal zn_vec_push_slot/zn_vec_pop_slot exactly;
    /// every other case (growth, empty pop, any fault) calls the runtime.
    fn vec_end_slot(
        &mut self,
        container: Value,
        width: Value,
        size: usize,
        push: bool,
    ) -> Result<Value> {
        let symbol = if push {
            "zn_vec_push_slot"
        } else {
            "zn_vec_pop_slot"
        };
        if size == 0 {
            return Ok(self.call(symbol, &[container, width], Some(I64))?.unwrap());
        }
        let limit = (u64::MAX / size as u64).min(i64::MAX as u64) as i64;
        let flags = self.array_flags(false);
        let len = self.b.ins().load(I64, flags, container, 8);
        let (index, fast) = if push {
            let cap = self.b.ins().load(I64, flags, container, 16);
            let room = self.b.ins().icmp(IntCC::NotEqual, len, cap);
            let fits = self
                .b
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, len, limit);
            (len, self.b.ins().band(room, fits))
        } else {
            // An empty vector wraps to UINT64_MAX, beyond every limit.
            let index = self.b.ins().iadd_imm_s(len, -1);
            let fits = self
                .b
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, index, limit);
            (index, fits)
        };
        let inline = self.b.create_block();
        let runtime = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, I64);
        self.b.set_cold_block(runtime);
        self.b.ins().brif(fast, inline, &[], runtime, &[]);
        self.b.switch_to_block(inline);
        let next = if push {
            self.b.ins().iadd_imm_s(len, 1)
        } else {
            index
        };
        self.b.ins().store(flags, next, container, 8);
        let data = self.b.ins().load(I64, flags, container, 0);
        let offset = self.b.ins().imul_imm_s(index, size as i64);
        let slot = self.b.ins().iadd(data, offset);
        self.b.ins().jump(join, &[slot.into()]);
        self.b.switch_to_block(runtime);
        let slot = self.call(symbol, &[container, width], Some(I64))?.unwrap();
        self.b.ins().jump(join, &[slot.into()]);
        self.b.switch_to_block(join);
        Ok(self.b.block_params(join)[0])
    }
    fn vector_elements(&mut self, element: Type, dst: Value, src: Option<Value>) -> Result<()> {
        if !element.owned() {
            return Ok(());
        }
        let head = self.b.create_block();
        let body = self.b.create_block();
        let end = self.b.create_block();
        self.b.append_block_param(head, I64);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(head, &[zero.into()]);
        self.b.switch_to_block(head);
        let i = self.b.block_params(head)[0];
        let len = self.b.ins().load(I64, MemFlagsData::new(), dst, 8);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThan, i, len);
        self.b.ins().brif(more, body, &[], end, &[]);
        self.b.switch_to_block(body);
        let offset = self
            .b
            .ins()
            .imul_imm_s(i, self.env.types.layout(element).size as i64);
        let data = self.b.ins().load(I64, MemFlagsData::new(), dst, 0);
        let dp = self.b.ins().iadd(data, offset);
        if let Some(src) = src {
            let data = self.b.ins().load(I64, MemFlagsData::new(), src, 0);
            let sp = self.b.ins().iadd(data, offset);
            self.clone_at(element, dp, sp)?;
        } else {
            self.drop_at(element, dp)?;
        }
        let next = self.b.ins().iadd_imm_s(i, 1);
        self.b.ins().jump(head, &[next.into()]);
        self.b.switch_to_block(end);
        Ok(())
    }
    fn stack_value(&mut self, ty: Type) -> Value {
        let layout = self.env.types.layout(ty);
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            layout.size as u32,
            layout.align.trailing_zeros() as u8,
        ));
        self.b.ins().stack_addr(I64, slot, 0)
    }
    fn soa_metadata(&mut self, ty: Type) -> (Value, Value) {
        let columns = self.env.types.soa_columns(ty).unwrap();
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (columns.len() * 16) as u32,
            3,
        ));
        let pointer = self.b.ins().stack_addr(I64, slot, 0);
        for (i, (offset, width)) in columns.iter().enumerate() {
            let offset = self.b.ins().iconst(I64, *offset as i64);
            let width = self.b.ins().iconst(I64, *width as i64);
            self.b
                .ins()
                .store(MemFlagsData::new(), offset, pointer, (i * 16) as i32);
            self.b
                .ins()
                .store(MemFlagsData::new(), width, pointer, (i * 16 + 8) as i32);
        }
        (self.b.ins().iconst(I64, columns.len() as i64), pointer)
    }
    fn soa_elements(&mut self, ty: Type, dst: Value, src: Option<Value>) -> Result<()> {
        let Type::Named(record) = self.env.types.soa_element(ty).unwrap() else {
            unreachable!()
        };
        let DataKind::Record(fields) = self.env.types.definitions[record].kind.clone() else {
            unreachable!()
        };
        let (count, meta) = self.soa_metadata(ty);
        let head = self.b.create_block();
        let body = self.b.create_block();
        let end = self.b.create_block();
        self.b.append_block_param(head, I64);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(head, &[zero.into()]);
        self.b.switch_to_block(head);
        let index = self.b.block_params(head)[0];
        let len = self.b.ins().load(I64, MemFlagsData::new(), dst, 8);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThan, index, len);
        self.b.ins().brif(more, body, &[], end, &[]);
        self.b.switch_to_block(body);
        for (i, field) in fields.iter().enumerate().rev() {
            if !field.ty.owned() {
                continue;
            }
            let column = self.b.ins().iconst(I64, i as i64);
            let dp = self
                .call(
                    "zn_soa_column_slot",
                    &[dst, index, column, count, meta],
                    Some(I64),
                )?
                .unwrap();
            if let Some(src) = src {
                let sp = self
                    .call(
                        "zn_soa_column_slot",
                        &[src, index, column, count, meta],
                        Some(I64),
                    )?
                    .unwrap();
                self.clone_at(field.ty, dp, sp)?;
            } else {
                self.drop_at(field.ty, dp)?;
            }
        }
        let next = self.b.ins().iadd_imm_s(index, 1);
        self.b.ins().jump(head, &[next.into()]);
        self.b.switch_to_block(end);
        Ok(())
    }
    fn soa_builtin(&mut self, d: &Place, name: &str, args: &[Place]) -> Result<()> {
        let ty = args[0].ty;
        let element = self.env.types.soa_element(ty).unwrap();
        let container = self.address(&args[0]);
        let (count, meta) = self.soa_metadata(ty);
        if name == "vec_capacity" {
            let n = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
            self.store(d, n);
            return Ok(());
        }
        if name == "vec_push" {
            let index = self
                .call("zn_soa_push_index", &[container, count, meta], Some(I64))?
                .unwrap();
            let value = self.address(args.last().unwrap());
            let columns = self.env.types.soa_columns(ty).unwrap();
            if (1..=256).contains(&columns.len()) && columns.iter().all(|&(_, width)| width > 0) {
                // Contract M3/M7, runtime invariant R1: SoA storage exists
                // only after the runtime's validated layout computation for
                // the current capacity, so every column offset (the sum over
                // earlier columns of round_up_8(cap * width)) and the row
                // offset `index * width` (index = len - 1 < cap) are in range;
                // zn_soa_store's per-column index and capacity re-checks
                // cannot fail. Store the pushed row directly. The descriptor
                // is reloaded: the push may have reallocated it.
                let data = self.b.ins().load(I64, MemFlagsData::new(), container, 0);
                let capacity = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
                let mut offset: Option<Value> = None;
                for &(field, width) in &columns {
                    let base = match offset {
                        Some(offset) => self.b.ins().iadd(data, offset),
                        None => data,
                    };
                    let row = self.b.ins().imul_imm_s(index, width as i64);
                    let slot = self.b.ins().iadd(base, row);
                    let source = self.b.ins().iadd_imm_s(value, field as i64);
                    self.copy_column(slot, source, width)?;
                    let bytes = self.b.ins().imul_imm_s(capacity, width as i64);
                    let padded = self.b.ins().iadd_imm_s(bytes, 7);
                    let padded = self.b.ins().band_imm_s(padded, -8);
                    offset = Some(match offset {
                        Some(offset) => self.b.ins().iadd(offset, padded),
                        None => padded,
                    });
                }
            } else {
                self.call(
                    "zn_soa_store",
                    &[container, index, value, count, meta],
                    None,
                )?;
            }
        } else if name == "vec_pop" {
            let dest = self.address(d);
            self.call("zn_soa_pop", &[dest, container, count, meta], None)?;
        } else {
            let index = self.load(&args[1]);
            let projection = self.stack_value(element);
            self.call(
                "zn_soa_load",
                &[projection, container, index, count, meta],
                None,
            )?;
            if name == "vec_replace" {
                self.drop_at(element, projection)?;
                let value = self.address(args.last().unwrap());
                self.call(
                    "zn_soa_store",
                    &[container, index, value, count, meta],
                    None,
                )?;
            } else {
                let dest = self.address(d);
                self.clone_at(element, dest, projection)?;
            }
        }
        Ok(())
    }
    /// One row (or packed group of rows) of a column expression. Column `c`
    /// is read at `rows[c] + offset`; a column an earlier update of the same
    /// rows already wrote reads that stored value (`updated`), exactly what
    /// reloading it would return.
    fn column_expr(
        &mut self,
        expr: &ColumnExpr,
        rows: &[Value],
        offset: i32,
        updated: &BTreeMap<usize, Value>,
        float: crate::numeric::Float,
        packed: bool,
    ) -> Value {
        let ty = match (float, packed) {
            (crate::numeric::Float::F32, true) => F32X4,
            (crate::numeric::Float::F32, false) => F32,
            (_, true) => F64X2,
            (_, false) => F64,
        };
        match expr {
            ColumnExpr::Column(column) => match updated.get(column) {
                Some(&value) => value,
                None => self
                    .b
                    .ins()
                    .load(ty, MemFlagsData::new(), rows[*column], offset),
            },
            ColumnExpr::Invariant(place) => {
                let raw = self.load(place);
                let value = self.float_value(raw, float);
                if packed {
                    self.b.ins().splat(ty, value)
                } else {
                    value
                }
            }
            ColumnExpr::Constant(bits) => {
                let raw = self.b.ins().iconst(I64, *bits as i64);
                let value = self.float_value(raw, float);
                if packed {
                    self.b.ins().splat(ty, value)
                } else {
                    value
                }
            }
            ColumnExpr::Binary {
                operator,
                left,
                right,
            } => {
                let left = self.column_expr(left, rows, offset, updated, float, packed);
                let right = self.column_expr(right, rows, offset, updated, float, packed);
                match operator.as_str() {
                    "+" => self.b.ins().fadd(left, right),
                    "-" => self.b.ins().fsub(left, right),
                    "*" => self.b.ins().fmul(left, right),
                    _ => self.b.ins().fdiv(left, right),
                }
            }
        }
    }
    /// Element-wise column updates over rows `0..length`, in row order: two
    /// packed groups of `lanes` rows per loop iteration, at most one more
    /// packed group, then `length % lanes` scalar rows; each row's updates
    /// run in source order with unchanged arithmetic (rows are independent;
    /// no reassociation). Every column the updates touch advances its own row
    /// pointer, carried as a loop block parameter, so the loop body never
    /// recomputes a column base; both loops test their count at the tail.
    fn column_simd(
        &mut self,
        updates: &[ColumnUpdate],
        bases: &[Value],
        length: Value,
    ) -> Result<()> {
        fn columns(expr: &ColumnExpr, out: &mut BTreeSet<usize>) {
            match expr {
                ColumnExpr::Column(column) => {
                    out.insert(*column);
                }
                ColumnExpr::Binary { left, right, .. } => {
                    columns(left, out);
                    columns(right, out);
                }
                ColumnExpr::Invariant(_) | ColumnExpr::Constant(_) => {}
            }
        }
        let float = updates[0].float;
        let narrow = float == crate::numeric::Float::F32;
        let lanes: i64 = if narrow { 4 } else { 2 };
        let width: i64 = if narrow { 4 } else { 8 };
        let mut used = BTreeSet::new();
        for update in updates {
            used.insert(update.column);
            columns(&update.value, &mut used);
        }
        let used: Vec<usize> = used.into_iter().collect();
        // Rows in groups: `pairs` iterations of two packed groups, at most
        // one more packed group, then `length % lanes` scalar rows, in order.
        let shift = i64::from(lanes.trailing_zeros());
        let pairs = self.b.ins().ushr_imm_s(length, shift + 1);
        let odd = self.b.ins().band_imm_s(length, lanes);
        let rest = self.b.ins().band_imm_s(length, lanes - 1);
        let twice = self.b.create_block();
        let single_head = self.b.create_block();
        let single = self.b.create_block();
        let tail = self.b.create_block();
        let scalar = self.b.create_block();
        let end = self.b.create_block();
        for _ in &used {
            for block in [twice, single_head, single, tail, scalar] {
                self.b.append_block_param(block, I64);
            }
        }
        self.b.append_block_param(twice, I64);
        self.b.append_block_param(scalar, I64);
        let arguments = |values: &[Value], count: Option<Value>| -> Vec<cl::BlockArg> {
            values
                .iter()
                .chain(count.iter())
                .map(|&v| v.into())
                .collect()
        };
        let start: Vec<Value> = used.iter().map(|&column| bases[column]).collect();
        let any = self.b.ins().icmp_imm_s(IntCC::NotEqual, pairs, 0);
        self.b.ins().brif(
            any,
            twice,
            &arguments(&start, Some(pairs)),
            single_head,
            &arguments(&start, None),
        );
        for (block, groups, step, packed) in [
            (twice, 2, 2 * lanes * width, true),
            (single, 1, lanes * width, true),
            (scalar, 1, width, false),
        ] {
            self.b.switch_to_block(block);
            let params = self.b.block_params(block).to_vec();
            let pointers = &params[..used.len()];
            let mut row = bases.to_vec();
            for (slot, &column) in used.iter().enumerate() {
                row[column] = pointers[slot];
            }
            for group in 0..groups {
                let offset = (group * lanes * width) as i32;
                // Every load of these rows precedes every store to them (a
                // later update reads an earlier update's value directly). The
                // columns of one allocation may lie a multiple of 4 KiB
                // apart, so a load issued after a store to another column's
                // same row could wait on a false (4K-aliased) store
                // dependence. Stores keep update order, so the final memory
                // contents are unchanged.
                let mut updated = BTreeMap::new();
                let mut stores = Vec::new();
                for update in updates {
                    let value =
                        self.column_expr(&update.value, &row, offset, &updated, float, packed);
                    updated.insert(update.column, value);
                    stores.push((update.column, value));
                }
                for (column, value) in stores {
                    self.b
                        .ins()
                        .store(MemFlagsData::new(), value, row[column], offset);
                }
            }
            let next: Vec<Value> = pointers
                .iter()
                .map(|&p| self.b.ins().iadd_imm_s(p, step))
                .collect();
            if block == single {
                self.b.ins().jump(tail, &arguments(&next, None));
                continue;
            }
            let remaining = self.b.ins().iadd_imm_s(params[used.len()], -1);
            let more = self.b.ins().icmp_imm_s(IntCC::NotEqual, remaining, 0);
            if block == twice {
                self.b.ins().brif(
                    more,
                    twice,
                    &arguments(&next, Some(remaining)),
                    single_head,
                    &arguments(&next, None),
                );
                // At most one packed group remains before the scalar rows.
                self.b.switch_to_block(single_head);
                let pointers = self.b.block_params(single_head).to_vec();
                let one = self.b.ins().icmp_imm_s(IntCC::NotEqual, odd, 0);
                self.b.ins().brif(
                    one,
                    single,
                    &arguments(&pointers, None),
                    tail,
                    &arguments(&pointers, None),
                );
                self.b.switch_to_block(tail);
                let pointers = self.b.block_params(tail).to_vec();
                let any = self.b.ins().icmp_imm_s(IntCC::NotEqual, rest, 0);
                self.b
                    .ins()
                    .brif(any, scalar, &arguments(&pointers, Some(rest)), end, &[]);
            } else {
                self.b
                    .ins()
                    .brif(more, scalar, &arguments(&next, Some(remaining)), end, &[]);
            }
        }
        self.b.switch_to_block(end);
        Ok(())
    }
    fn channel_builtin(&mut self, destination: &Place, name: &str, args: &[Place]) -> Result<()> {
        if name == "channel_new" {
            let Type::Named(id) = destination.ty else {
                unreachable!()
            };
            let element = self.env.types.channel_element(destination.ty).unwrap();
            let out = self.address(destination);
            let capacity = self.load(&args[0]);
            let width = self
                .b
                .ins()
                .iconst(I64, self.env.types.layout(element).size as i64);
            let callback = self
                .env
                .module
                .declare_func_in_func(self.env.channel_drops[&id], self.b.func);
            self.b.func.dfg.ext_funcs[callback].colocated = true;
            let callback = self.b.ins().func_addr(I64, callback);
            self.call("zn_channel_new", &[out, capacity, width, callback], None)?;
            return Ok(());
        }
        let channel = self.address(&args[0]);
        let result = if matches!(name, "channel_send" | "channel_recv" | "channel_try_recv") {
            let value = &args[1];
            let slot = if scalar(value.ty) {
                let slot = self.stack_value(value.ty);
                let raw = self.load(value);
                self.store_memory(value.ty, slot, raw);
                slot
            } else {
                self.address(value)
            };
            if name == "channel_send" {
                self.call("zn_channel_send", &[channel, slot], Some(I8))?
            } else {
                let blocking = self.b.ins().iconst(I8, i64::from(name == "channel_recv"));
                let result = self.call("zn_channel_recv", &[channel, slot, blocking], Some(I8))?;
                if scalar(value.ty) {
                    let raw = self.load_memory(value.ty, slot);
                    self.store(value, raw);
                }
                self.integer_overrides.remove(&value.local);
                self.guards.clear_preserving_values();
                result
            }
        } else {
            self.call(
                &format!("zn_{name}"),
                &[channel],
                match destination.ty {
                    Type::Unit => None,
                    Type::Bool => Some(I8),
                    _ => Some(I64),
                },
            )?
        };
        if let Some(result) = result {
            self.store(destination, result);
        }
        Ok(())
    }
    fn drop_at(&mut self, ty: Type, a: Value) -> Result<()> {
        if matches!(
            ty,
            Type::Fixed(..) | Type::Slice(_) | Type::VectorView(_) | Type::TextView
        ) {
            return Ok(());
        }
        if let Type::Named(id) = ty {
            let def = self.env.types.definitions[id].clone();
            match def.kind {
                DataKind::Channel(_) => {
                    self.call("zn_channel_drop", &[a], None)?;
                }
                // Contract M3: elements that own nothing need no element
                // loop; their drop would only zero storage released here.
                DataKind::SoaVector(_) => {
                    let element = self.env.types.soa_element(ty).unwrap();
                    if !trivially_dropped(self.env.types, element, 0) {
                        self.soa_elements(ty, a, None)?;
                    }
                    self.call("zn_vec_release", &[a], None)?;
                }
                DataKind::Vector(fields) => {
                    if !trivially_dropped(self.env.types, fields[0].ty, 0) {
                        self.vector_elements(fields[0].ty, a, None)?;
                    }
                    self.call("zn_vec_release", &[a], None)?;
                }
                DataKind::Map(_) => {
                    self.map_elements(ty, a, None)?;
                    self.call("zn_map_release", &[a], None)?;
                }
                DataKind::Arena(_) => {
                    self.arena_elements(ty, a, None)?;
                    self.call("zn_arena_release", &[a], None)?;
                }
                DataKind::Record(fields) => {
                    for (i, f) in fields.iter().enumerate().rev() {
                        if f.ty.owned() {
                            let p = self
                                .b
                                .ins()
                                .iadd_imm_s(a, self.env.types.layouts[id].offsets[i] as i64);
                            self.drop_at(f.ty, p)?;
                        }
                    }
                }
                DataKind::Enum(fields) => {
                    let tag = self.b.ins().load(I64, MemFlagsData::new(), a, 0);
                    let end = self.b.create_block();
                    for (i, f) in fields.iter().enumerate() {
                        if !f.ty.owned() {
                            continue;
                        }
                        let yes = self.b.create_block();
                        let next = self.b.create_block();
                        let c = self.b.ins().icmp_imm_s(IntCC::Equal, tag, (i + 1) as i64);
                        self.b.ins().brif(c, yes, &[], next, &[]);
                        self.b.switch_to_block(yes);
                        let p = self.b.ins().iadd_imm_s(a, 8);
                        self.drop_at(f.ty, p)?;
                        self.b.ins().jump(end, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(end, &[]);
                    self.b.switch_to_block(end);
                }
            }
            self.zero_bytes(a, self.env.types.layout(ty).size)?;
        } else {
            self.call(&ty.drop_fn(), &[a], None)?;
        }
        Ok(())
    }
    fn clone_at(&mut self, ty: Type, d: Value, s: Value) -> Result<()> {
        if self.env.types.channel_element(ty).is_some() {
            self.call("zn_channel_clone", &[d, s], None)?;
            return Ok(());
        }
        // The runtime clone copies every element's bytes; a trivially dropped
        // element's clone is exactly that byte copy, so no element loop runs.
        if let Some(element) = self.env.types.soa_element(ty) {
            let (count, meta) = self.soa_metadata(ty);
            self.call("zn_soa_clone_storage", &[d, s, count, meta], None)?;
            if trivially_dropped(self.env.types, element, 0) {
                return Ok(());
            }
            return self.soa_elements(ty, d, Some(s));
        }
        if let Some(element) = self.env.types.vector_element(ty) {
            let width = self
                .b
                .ins()
                .iconst(I64, self.env.types.layout(element).size as i64);
            self.call("zn_vec_clone_storage", &[d, s, width], None)?;
            if trivially_dropped(self.env.types, element, 0) {
                return Ok(());
            }
            return self.vector_elements(element, d, Some(s));
        }
        if matches!(ty, Type::Fixed(..)) {
            return self.copy_bytes(d, s, self.env.types.layout(ty).size);
        }
        if let Type::Named(id) = ty {
            let def = self.env.types.definitions[id].clone();
            match def.kind {
                DataKind::Vector(_) | DataKind::SoaVector(_) | DataKind::Channel(_) => {
                    unreachable!()
                }
                DataKind::Map(_) => {
                    let layout = self.env.types.map_entry_layout(ty).unwrap();
                    let width = self.b.ins().iconst(I64, layout.width as i64);
                    self.call("zn_map_clone_storage", &[d, s, width], None)?;
                    self.map_elements(ty, d, Some(s))?;
                }
                DataKind::Arena(_) => {
                    let element = self.env.types.arena_element(ty).unwrap();
                    let width = self
                        .b
                        .ins()
                        .iconst(I64, self.env.types.layout(element).size as i64);
                    self.call("zn_arena_clone_storage", &[d, s, width], None)?;
                    self.arena_elements(ty, d, Some(s))?;
                }
                DataKind::Record(fields) => {
                    self.copy_bytes(d, s, self.env.types.layout(ty).size)?;
                    for (i, f) in fields.iter().enumerate() {
                        if f.ty.owned() {
                            let off = self.env.types.layouts[id].offsets[i] as i64;
                            let dp = self.b.ins().iadd_imm_s(d, off);
                            let sp = self.b.ins().iadd_imm_s(s, off);
                            self.clone_at(f.ty, dp, sp)?;
                        }
                    }
                }
                DataKind::Enum(fields) => {
                    self.copy_bytes(d, s, self.env.types.layout(ty).size)?;
                    let tag = self.b.ins().load(I64, MemFlagsData::new(), s, 0);
                    let end = self.b.create_block();
                    for (i, f) in fields.iter().enumerate() {
                        if !f.ty.owned() {
                            continue;
                        }
                        let yes = self.b.create_block();
                        let next = self.b.create_block();
                        let c = self.b.ins().icmp_imm_s(IntCC::Equal, tag, (i + 1) as i64);
                        self.b.ins().brif(c, yes, &[], next, &[]);
                        self.b.switch_to_block(yes);
                        let dp = self.b.ins().iadd_imm_s(d, 8);
                        let sp = self.b.ins().iadd_imm_s(s, 8);
                        self.clone_at(f.ty, dp, sp)?;
                        self.b.ins().jump(end, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(end, &[]);
                    self.b.switch_to_block(end);
                }
            }
        } else {
            self.call(
                if ty == Type::Texts {
                    "zn_abi_texts_clone"
                } else if ty == Type::Text {
                    "zn_abi_text_clone"
                } else {
                    "zn_abi_ints_clone"
                },
                &[d, s],
                None,
            )?;
        }
        Ok(())
    }
    fn init(&mut self, d: &Place, v: &Rvalue) -> Result<()> {
        let value = match v {
            Rvalue::Int(n) => Some(self.b.ins().iconst(I64, *n)),
            Rvalue::Float(n) => Some(self.b.ins().iconst(I64, *n as i64)),
            Rvalue::Bool(n) => Some(self.b.ins().iconst(I8, i64::from(*n))),
            Rvalue::Zero => {
                self.zero(d)?;
                None
            }
            Rvalue::Region { owner, start, end } => {
                let in_registers = matches!(self.locals[d.local], Storage::Descriptor { .. });
                let dest = if in_registers {
                    self.stack_value(d.ty)
                } else {
                    self.address(d)
                };
                let lo = self.load(start);
                let hi = self.load(end);
                if d.ty == Type::TextView {
                    let source = self.address(owner);
                    self.call("zn_abi_text_region", &[dest, source, lo, hi], None)?;
                } else if let Type::VectorView(id) = d.ty {
                    let (data, len) = self.descriptor(owner);
                    let element = self.env.types.vector_element(Type::Named(id)).unwrap();
                    let width = self
                        .b
                        .ins()
                        .iconst(I64, self.env.types.layout(element).size as i64);
                    self.call(
                        "zn_vector_view_region",
                        &[dest, data, len, lo, hi, width],
                        None,
                    )?;
                } else {
                    let (data, len, number) = self.buffer(owner);
                    let width = self.b.ins().iconst(I64, (number.bits() / 8) as i64);
                    self.call("zn_span_region", &[dest, data, len, lo, hi, width], None)?;
                }
                if let Storage::Descriptor { data, len } = self.locals[d.local] {
                    let pointer = self.b.ins().load(I64, MemFlagsData::new(), dest, 0);
                    let count = self.b.ins().load(I64, MemFlagsData::new(), dest, 8);
                    self.b.def_var(data, pointer);
                    self.b.def_var(len, count);
                }
                None
            }
            Rvalue::Element { owner, index } => {
                let container = if matches!(self.locals[owner.local], Storage::Descriptor { .. }) {
                    self.b.ins().iconst(I64, 0)
                } else {
                    self.address(owner)
                };
                if self.direct_soa_aliases.contains(&d.local)
                    && self.env.types.soa_element(owner.ty).is_some()
                {
                    let guarded = self.guarded_index(owner, index);
                    let index = self.load(index);
                    let len = if let Some(len) = self.hoisted_lengths.get(&d.local) {
                        *len
                    } else {
                        self.b.ins().load(I64, MemFlagsData::new(), container, 8)
                    };
                    let invalid = self
                        .b
                        .ins()
                        .icmp(IntCC::UnsignedGreaterThanOrEqual, index, len);
                    if !guarded {
                        self.fail_if(invalid, "vector index out of bounds")?;
                    }
                    let data = self.b.ins().load(I64, MemFlagsData::new(), container, 0);
                    let capacity = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
                    self.locals[d.local] = Storage::Columns {
                        data,
                        capacity,
                        index,
                        vector: owner.ty,
                    };
                    return Ok(());
                }
                let pointer = if self.env.types.soa_element(owner.ty).is_some() {
                    let index = self.load(index);
                    let projection = self.stack_value(d.ty);
                    let (count, meta) = self.soa_metadata(owner.ty);
                    self.call(
                        "zn_soa_load",
                        &[projection, container, index, count, meta],
                        None,
                    )?;
                    projection
                } else if self.env.types.arena_element(owner.ty).is_some() {
                    let id = self.load(index);
                    self.arena_slot(container, id, self.env.types.layout(d.ty).size)?
                } else if let Some(layout) = self.env.types.map_entry_layout(owner.ty) {
                    let key = self.operand_address(index);
                    let code = self.b.ins().iconst(I64, layout.key_code);
                    let width = self.b.ins().iconst(I64, layout.width as i64);
                    let entry = self
                        .call("zn_map_find", &[container, key, code, width], Some(I64))?
                        .unwrap();
                    let missing = self.b.ins().icmp_imm_s(IntCC::Equal, entry, 0);
                    self.fail_if(missing, "map key not found")?;
                    self.b.ins().iadd_imm_s(entry, layout.value_offset as i64)
                } else {
                    self.vector_slot(owner, index, self.env.types.layout(d.ty).size)?
                };
                self.locals[d.local] = Storage::Address(pointer);
                None
            }
            Rvalue::Reference(source) => {
                let pointer = self.address(source);
                self.locals[d.local] = Storage::Address(pointer);
                None
            }
            Rvalue::Repeat(p, len) => {
                let a = self.address(d);
                let v = self.load(p);
                let len = self.b.ins().iconst(I64, *len as i64);
                let code = self.b.ins().iconst(I64, p.ty.integer().unwrap().code());
                self.call("zn_fixed_fill", &[a, v, len, code], None)?;
                None
            }
            Rvalue::Convert(p) if p.ty.integer().is_some() && d.ty.integer().is_some() => {
                let v = self.load(p);
                Some(self.convert_integer(p, v, d.ty)?)
            }
            Rvalue::Convert(p) if exact_float_conversion(p.ty, d.ty) => {
                let v = self.load(p);
                Some(self.float_conversion(v, p.ty, d.ty))
            }
            Rvalue::Convert(p) => {
                let v = self.load(p);
                let from = self.b.ins().iconst(I64, crate::numeric::type_code(p.ty));
                let to = self.b.ins().iconst(I64, crate::numeric::type_code(d.ty));
                let symbol = if matches!(p.ty, Type::Float(_)) || matches!(d.ty, Type::Float(_)) {
                    "zn_fp_convert"
                } else {
                    "zn_num_cast"
                };
                self.call(symbol, &[v, from, to], Some(I64))?
            }
            Rvalue::Text(s) => {
                let a = self.address(d);
                let s = self.string(s)?;
                let Rvalue::Text(text) = v else {
                    unreachable!()
                };
                let bytes = self.b.ins().iconst(I64, text.len() as i64);
                self.call("zn_abi_text_literal", &[a, s, bytes], None)?;
                None
            }
            Rvalue::Copy(p) => {
                self.transfer(d, p)?;
                None
            }
            Rvalue::Unary(op, p) => {
                let a = self.load(p);
                if op == "!" {
                    Some(self.b.ins().icmp_imm_s(IntCC::Equal, a, 0))
                } else if let Type::Float(n) = p.ty {
                    let float = self.float_value(a, n);
                    let neg = self.b.ins().fneg(float);
                    Some(self.float_bits(neg, n))
                } else {
                    let zero = self.b.ins().iconst(I64, 0);
                    Some(if matches!(p.ty, Type::Number(_)) {
                        self.number_op(p.ty, "-", zero, a)?
                    } else {
                        self.arithmetic("-", zero, a)?
                    })
                }
            }
            Rvalue::IntRange { value, low, high } => {
                let input = self.load(value);
                let lower = self.b.ins().iconst(I64, *low);
                let shifted = self.b.ins().isub(input, lower);
                let span = self
                    .b
                    .ins()
                    .iconst(I64, (*high as i128 - *low as i128) as u64 as i64);
                Some(
                    self.b
                        .ins()
                        .icmp(IntCC::UnsignedLessThanOrEqual, shifted, span),
                )
            }
            Rvalue::Binary(op, a, b) => {
                if let Type::Float(n) = a.ty {
                    let value = self.float_binary(op, a, b, n)?;
                    self.store(d, value);
                    return Ok(());
                }
                let av = self.load(a);
                let bv = self.load(b);
                let helper = match op.as_str() {
                    "+" => "add",
                    "-" => "sub",
                    "*" => "mul",
                    "*literal" => "mul_literal",
                    "/" => "div",
                    "%" => "mod",
                    _ => "",
                };
                if !helper.is_empty() {
                    let (left, right) = (self.guarded_bounds(a), self.guarded_bounds(b));
                    let (_, safe) = left.arithmetic(op, right);
                    if a.ty == Type::Int && self.condition_depth == 0 {
                        // This single definition executes only here, under
                        // these dominating facts, so its result is enclosed
                        // by the guarded interval on every execution. A loop
                        // condition may be emitted twice (rotation), so it
                        // keeps the flow-insensitive bound below.
                        self.integer_ranges.refine(
                            d,
                            left.binary(op, right),
                            &mut self.integer_overrides,
                        );
                    } else if a.ty == Type::Int
                        && ["+", "-", "*", "*literal"].contains(&op.as_str())
                    {
                        let (bound, _) = self
                            .integer_ranges
                            .get(a, &self.integer_overrides)
                            .arithmetic(op, self.integer_ranges.get(b, &self.integer_overrides));
                        self.integer_ranges
                            .refine(d, bound, &mut self.integer_overrides);
                    }
                    // Contract M4: in a versioned loop's fast copy, an affine
                    // `+`/`-` whose hoisted guard was recorded cannot overflow.
                    let versioned = !safe
                        && a.ty == Type::Int
                        && (op == "+" || op == "-")
                        && self.version_operation();
                    Some(if a.ty == Type::Int && (safe || versioned) {
                        // The global/contextual interval covers every possible
                        // execution of this definition, including loop writes
                        // (or the versioning guard proved this one exact).
                        match op.as_str() {
                            "+" => self.b.ins().iadd(av, bv),
                            "-" => self.b.ins().isub(av, bv),
                            "*" | "*literal" => self.b.ins().imul(av, bv),
                            _ => unreachable!(),
                        }
                    } else if a.ty == Type::Int
                        && ["/", "%"].contains(&op.as_str())
                        && self.guarded_bounds(a).low >= 0
                        && self.guarded_bounds(b).low > 0
                    {
                        // Nonnegative dividend and strictly positive divisor:
                        // unsigned and signed results coincide; neither zero
                        // division nor MIN/-1 can occur on this path.
                        if let Some(k) = self.guarded_bounds(b).power_of_two() {
                            // Exactly 2^k: a >= 0 makes truncating division a
                            // logical shift and the remainder a low-bit mask.
                            if op == "/" {
                                if k == 0 {
                                    av
                                } else {
                                    self.b.ins().ushr_imm_s(av, i64::from(k))
                                }
                            } else {
                                self.b.ins().band_imm_s(av, (1i64 << k) - 1)
                            }
                        } else if op == "/" {
                            self.b.ins().udiv(av, bv)
                        } else if let Some(shift) = self
                            .guarded_bounds(a)
                            .mersenne_shift(self.guarded_bounds(b))
                        {
                            let modulus = self.guarded_bounds(b).low;
                            let mask = self.b.ins().iconst(I64, modulus);
                            let low = self.b.ins().band(av, mask);
                            let shift_value = self.b.ins().iconst(I64, shift as i64);
                            let high = self.b.ins().ushr(av, shift_value);
                            let folded = self.b.ins().iadd(low, high);
                            let large =
                                self.b
                                    .ins()
                                    .icmp(IntCC::UnsignedGreaterThanOrEqual, folded, mask);
                            if (self.guarded_bounds(a).high as u64 >> shift)
                                <= modulus as u64 / 1024
                            {
                                // Sparse correction range: leave subtraction
                                // off the recurrence's common critical path.
                                let correction = self.b.create_block();
                                let ready = self.b.create_block();
                                self.b.append_block_param(ready, I64);
                                self.b.set_cold_block(correction);
                                self.b
                                    .ins()
                                    .brif(large, correction, &[], ready, &[folded.into()]);
                                self.b.switch_to_block(correction);
                                let reduced = self.b.ins().isub(folded, mask);
                                self.b.ins().jump(ready, &[reduced.into()]);
                                self.b.switch_to_block(ready);
                                self.b.block_params(ready)[0]
                            } else {
                                let reduced = self.b.ins().isub(folded, mask);
                                self.b.ins().select(large, reduced, folded)
                            }
                        } else {
                            self.b.ins().urem(av, bv)
                        }
                    } else if matches!(a.ty, Type::Number(_)) {
                        self.number_op(a.ty, op, av, bv)?
                    } else if let Some(value) = self.signed_step(op, av, bv, left, right)? {
                        value
                    } else {
                        self.arithmetic(op, av, bv)?
                    })
                } else {
                    let (av, bv) = if let Type::Number(n) = a.ty {
                        (self.narrow_integer(av, n), self.narrow_integer(bv, n))
                    } else {
                        (av, bv)
                    };
                    let unsigned = a.ty.integer().is_some_and(|n| !n.signed());
                    let cc = match op.as_str() {
                        "==" => IntCC::Equal,
                        "!=" => IntCC::NotEqual,
                        "<" => {
                            if unsigned {
                                IntCC::UnsignedLessThan
                            } else {
                                IntCC::SignedLessThan
                            }
                        }
                        "<=" => {
                            if unsigned {
                                IntCC::UnsignedLessThanOrEqual
                            } else {
                                IntCC::SignedLessThanOrEqual
                            }
                        }
                        ">" => {
                            if unsigned {
                                IntCC::UnsignedGreaterThan
                            } else {
                                IntCC::SignedGreaterThan
                            }
                        }
                        ">=" => {
                            if unsigned {
                                IntCC::UnsignedGreaterThanOrEqual
                            } else {
                                IntCC::SignedGreaterThanOrEqual
                            }
                        }
                        _ => return Err(err("unknown binary operation")),
                    };
                    Some(self.b.ins().icmp(cc, av, bv))
                }
            }
            Rvalue::Record(values) => {
                self.zero(d)?;
                for (i, p) in values.iter().enumerate() {
                    let q = self.field(d, i, p.ty);
                    self.transfer(&q, p)?;
                }
                None
            }
            Rvalue::Variant { index, payload } => {
                self.zero(d)?;
                let a = self.address(d);
                let tag = self.b.ins().iconst(I64, (index + 1) as i64);
                self.b.ins().store(MemFlagsData::new(), tag, a, 0);
                if let Some(p) = payload {
                    let q = self.field(d, *index, p.ty);
                    self.transfer(&q, p)?;
                }
                None
            }
            Rvalue::Call { name, args, spawn } => {
                let (id, f) = self.env.funcs[name];
                let mut av = Vec::new();
                if d.ty != Type::Unit && !scalar(d.ty) {
                    av.push(self.address(d));
                }
                if f.source.external_symbol.is_some() {
                    for (arg, param) in args.iter().zip(&f.source.params) {
                        let p = match arg {
                            Argument::Value(p) | Argument::Reference(p) => p,
                        };
                        let value = if param.borrow != Borrow::Owned || !scalar(param.ty) {
                            self.address(p)
                        } else {
                            let value = self.load(p);
                            if let Type::Float(n) = param.ty {
                                self.float_value(value, n)
                            } else if ffi_type(param.ty) != ct(param.ty) {
                                self.b.ins().ireduce(ffi_type(param.ty), value)
                            } else {
                                value
                            }
                        };
                        av.push(value);
                    }
                    if let Some(mut value) = self.call_id(id, &av) {
                        if let Type::Float(n) = d.ty {
                            value = self.float_bits(value, n);
                        } else if ffi_type(d.ty) != ct(d.ty) {
                            value = if d.ty.integer().is_some_and(|n| n.signed()) {
                                self.b.ins().sextend(I64, value)
                            } else {
                                self.b.ins().uextend(I64, value)
                            };
                        }
                        self.store(d, value);
                    }
                    return Ok(());
                }
                if *spawn {
                    let size = packet_size(f, self.env.types);
                    let n = self.b.ins().iconst(I64, size as i64);
                    let packet = self.call("zn_alloc", &[n], Some(I64))?.unwrap();
                    let mut off = 0;
                    for (arg, param) in args.iter().zip(&f.source.params) {
                        off = align(off, self.env.types.layout(param.ty).align);
                        let dp = self.b.ins().iadd_imm_s(packet, off as i64);
                        let Argument::Value(p) = arg else {
                            unreachable!()
                        };
                        if scalar(p.ty) {
                            let v = self.load(p);
                            self.store_memory(p.ty, dp, v);
                        } else {
                            let sp = self.address(p);
                            self.copy_bytes(dp, sp, self.env.types.layout(p.ty).size)?;
                        }
                        off += self.env.types.layout(param.ty).size;
                    }
                    let id = self.env.workers[name];
                    let fr = self.env.module.declare_func_in_func(id, self.b.func);
                    self.b.func.dfg.ext_funcs[fr].colocated = true;
                    let fp = self.b.ins().func_addr(I64, fr);
                    let width = self
                        .b
                        .ins()
                        .iconst(I64, self.env.types.layout(f.source.ret).size as i64);
                    self.call("zn_start_owned", &[fp, packet, width], Some(I64))?
                } else {
                    for (arg, param) in args.iter().zip(&f.source.params) {
                        let value = match arg {
                            Argument::Reference(p) => self.address(p),
                            Argument::Value(p) => {
                                if scalar(p.ty) {
                                    self.load(p)
                                } else {
                                    self.address(p)
                                }
                            }
                        };
                        av.push(if param.borrow == Borrow::Owned {
                            if let Type::Float(n) = param.ty {
                                self.float_value(value, n)
                            } else {
                                value
                            }
                        } else {
                            value
                        });
                    }
                    let cost = self.env.inline_costs.get(name).copied();
                    if self.hot
                        && self.inline_stack.len() < 3
                        && !self.inline_stack.contains(name)
                        && cost.is_some_and(|c| c <= self.inline_budget)
                    {
                        self.inline_budget -= cost.unwrap();
                        let (result, bound) = self.inline_call(f, &av, args)?;
                        self.integer_ranges
                            .refine(d, bound, &mut self.integer_overrides);
                        result
                    } else {
                        self.call_id(id, &av).map(|value| {
                            if let Type::Float(n) = f.source.ret {
                                self.float_bits(value, n)
                            } else {
                                value
                            }
                        })
                    }
                }
            }
            Rvalue::Builtin { name, args } => {
                if crate::syntax::integer_intrinsic(name).is_some() {
                    let value = self.integer_intrinsic(name, args);
                    self.store(d, value);
                    return Ok(());
                }
                if name == "process_run" {
                    let out = self.address(d);
                    let program = self.address(&args[0]);
                    let argv = self.address(&args[1]);
                    let data = self.b.ins().load(I64, MemFlagsData::new(), argv, 0);
                    let count = self.b.ins().load(I64, MemFlagsData::new(), argv, 8);
                    self.call("zn_abi_process_run", &[out, program, data, count], None)?;
                    return Ok(());
                }
                if name == "net_request" {
                    // Mirrors zn_abi_process_run: the runtime writes one
                    // `zn_net_result` (tag + {status, body} | error text),
                    // which is exactly the checked NetResult enum layout.
                    // Endpoint and payload are borrowed by address.
                    let out = self.address(d);
                    let endpoint = self.address(&args[0]);
                    let payload = self.address(&args[1]);
                    self.call("zn_abi_net_request", &[out, endpoint, payload], None)?;
                    return Ok(());
                }
                if name == "fnv1a_64" || name == "sha256_text" {
                    let (data, len) = self.byte_input(&args[0]);
                    if name == "fnv1a_64" {
                        let hash = self.call("zn_fnv1a_64", &[data, len], Some(I64))?.unwrap();
                        self.store(d, hash);
                    } else {
                        let out = self.address(d);
                        self.call("zn_abi_sha256_bytes", &[out, data, len], None)?;
                    }
                    return Ok(());
                }
                if let Some(done) = self.os_builtin(d, name, args)? {
                    return Ok(done);
                }
                if name.starts_with("channel_") {
                    return self.channel_builtin(d, name, args);
                }
                if name == "join" {
                    let task = self.load(&args[0]);
                    let layout = self.env.types.layout(d.ty);
                    let out = if d.ty == Type::Unit {
                        self.b.ins().iconst(I64, 0)
                    } else if scalar(d.ty) {
                        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            layout.size as u32,
                            layout.align.trailing_zeros() as u8,
                        ));
                        self.b.ins().stack_addr(I64, slot, 0)
                    } else {
                        self.address(d)
                    };
                    let width = self.b.ins().iconst(I64, layout.size as i64);
                    self.call("zn_join_owned", &[out, task, width], None)?;
                    self.zero(&args[0])?;
                    if scalar(d.ty) {
                        let value = self.load_memory(d.ty, out);
                        self.store(d, value);
                    }
                    return Ok(());
                }
                if name.starts_with("arena_") {
                    return self.arena_builtin(d, name, args);
                }
                if name.starts_with("map_") {
                    return self.map_builtin(d, name, args);
                }
                if name.starts_with("vec_") {
                    if self.env.types.soa_element(args[0].ty).is_some() {
                        return self.soa_builtin(d, name, args);
                    }
                    let container =
                        if matches!(name.as_str(), "vec_capacity" | "vec_push" | "vec_pop") {
                            self.address(&args[0])
                        } else {
                            self.b.ins().iconst(I64, 0)
                        };
                    let element = self.env.types.vector_element(args[0].ty).unwrap();
                    let size = self.env.types.layout(element).size;
                    let width = self.b.ins().iconst(I64, size as i64);
                    if name == "vec_capacity" {
                        let n = self.b.ins().load(I64, MemFlagsData::new(), container, 16);
                        self.store(d, n);
                        return Ok(());
                    }
                    let slot = if name == "vec_push" || name == "vec_pop" {
                        self.vec_end_slot(container, width, size, name == "vec_push")?
                    } else {
                        self.vector_slot(&args[0], &args[1], size)?
                    };
                    if name == "vec_push" || name == "vec_replace" {
                        if name == "vec_replace" && element.owned() {
                            self.drop_at(element, slot)?;
                        }
                        let v = args.last().unwrap();
                        if scalar(element) {
                            let value = self.load(v);
                            self.store_memory(element, slot, value);
                        } else {
                            let source = self.address(v);
                            self.copy_bytes(slot, source, size)?;
                        }
                    } else {
                        if scalar(element) {
                            let value = self.load_memory(element, slot);
                            self.store(d, value);
                        } else {
                            let dest = self.address(d);
                            if name == "vec_pop" {
                                self.copy_bytes(dest, slot, size)?;
                            } else {
                                self.clone_at(element, dest, slot)?;
                            }
                        }
                        if name == "vec_pop" {
                            self.zero_bytes(slot, size)?;
                        }
                    }
                    return Ok(());
                }
                if matches!(
                    name.as_str(),
                    "sqrt" | "abs" | "is_finite" | "is_nan" | "print_float" | "float_text"
                ) {
                    let raw = self.load(&args[0]);
                    let code = self
                        .b
                        .ins()
                        .iconst(I64, crate::numeric::type_code(args[0].ty));
                    let result = match name.as_str() {
                        "float_text" => {
                            let out = self.address(d);
                            self.call("zn_abi_float_text", &[out, raw, code], None)?;
                            None
                        }
                        "print_float" => self.call("zn_print_float", &[raw, code], None)?,
                        "is_finite" | "is_nan" => {
                            self.call(&format!("zn_fp_{name}"), &[raw, code], Some(I8))?
                        }
                        _ => self.call(&format!("zn_fp_{name}"), &[raw, code], Some(I64))?,
                    };
                    if let Some(value) = result {
                        self.store(d, value);
                    }
                    return Ok(());
                }
                if ["saturating_add", "saturating_sub"].contains(&name.as_str()) {
                    let a = self.load(&args[0]);
                    let b = self.load(&args[1]);
                    Some(self.number_op(d.ty, name, a, b)?)
                } else if name.starts_with("fixed_")
                    || name.starts_with("read_u")
                    || name.starts_with("try_write_u")
                {
                    self.fixed_builtin(d, name, args)?;
                    return Ok(());
                } else if ["len", "at", "byte_at", "set"].contains(&name.as_str()) {
                    let elements = self.array_flags(true);
                    let (data, len) = self.descriptor(&args[0]);
                    if name == "len" {
                        // Only a dominating successful check of this exact SSA
                        // length (or its construction as a masked text length)
                        // can discharge this test.
                        if !self.known_nonnegative(len) {
                            let bad = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, len, 0);
                            self.fail_if(bad, "length exceeds int range")?;
                            self.guards
                                .active
                                .insert(Fact::NonnegativeLength(len.as_u32() as usize));
                        }
                        Some(len)
                    } else {
                        let i = self.load(&args[1]);
                        let bad = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, i, len);
                        if !self.guarded_index(&args[0], &args[1]) {
                            self.fail_if(
                                bad,
                                if name == "byte_at" {
                                    "text index out of bounds"
                                } else {
                                    "array index out of bounds"
                                },
                            )?;
                        }
                        // The executed check proves `i <u len`; with a proven
                        // nonnegative length that is `0 <= i < len`.
                        if self.known_nonnegative(len) {
                            self.index_facts(&args[1], len, i64::MAX - 1, true);
                        }
                        let shift = if name == "byte_at" {
                            i
                        } else {
                            self.b.ins().ishl_imm_s(i, 3)
                        };
                        let p = self.b.ins().iadd(data, shift);
                        if name == "byte_at" {
                            // Text bytes are unsigned; bounds remain checked above.
                            let byte = self.b.ins().load(I8, MemFlagsData::new(), p, 0);
                            Some(self.b.ins().uextend(I64, byte))
                        } else if name == "at" {
                            Some(self.b.ins().load(I64, elements, p, 0))
                        } else {
                            let v = self.load(&args[2]);
                            self.b.ins().store(elements, v, p, 0);
                            None
                        }
                    }
                } else if name == "push" {
                    let a = self.address(&args[0]);
                    let value = self.load(&args[1]);
                    let descriptor = self.array_flags(false);
                    let elements = self.array_flags(true);
                    let len = self.b.ins().load(I64, descriptor, a, 8);
                    let cap = self.b.ins().load(I64, descriptor, a, 16);
                    let full = self.b.ins().icmp(IntCC::Equal, len, cap);
                    let grow = self.b.create_block();
                    let append = self.b.create_block();
                    self.b.set_cold_block(grow);
                    self.b.ins().brif(full, grow, &[], append, &[]);
                    self.b.switch_to_block(grow);
                    self.call("zn_ints_grow", &[a], None)?;
                    self.b.ins().jump(append, &[]);
                    self.b.switch_to_block(append);
                    // Growth can reallocate. Reload the descriptor after the call.
                    // The checked runtime limits capacity to SIZE_MAX / sizeof(i64).
                    let data = self.b.ins().load(I64, descriptor, a, 0);
                    let len = self.b.ins().load(I64, descriptor, a, 8);
                    let offset = self.b.ins().ishl_imm_s(len, 3);
                    let p = self.b.ins().iadd(data, offset);
                    self.b.ins().store(elements, value, p, 0);
                    let next = self.b.ins().iadd_imm_s(len, 1);
                    self.b.ins().store(descriptor, next, a, 8);
                    None
                } else if name == "clone" {
                    let dp = self.address(d);
                    let sp = self.address(&args[0]);
                    self.clone_at(d.ty, dp, sp)?;
                    None
                } else {
                    let mut av = Vec::new();
                    if d.ty != Type::Unit && !scalar(d.ty) {
                        av.push(self.address(d));
                    }
                    for p in args {
                        av.push(if scalar(p.ty) {
                            self.load(p)
                        } else {
                            self.address(p)
                        });
                    }
                    let symbol = match name.as_str() {
                        "print_int" | "print_uint" | "print_bool" | "args_count"
                        | "monotonic_ms" | "exit_code" => {
                            format!("zn_{name}")
                        }
                        "len" => "zn_abi_length".into(),
                        _ => format!("zn_abi_{name}"),
                    };
                    self.call(
                        &symbol,
                        &av,
                        if scalar(d.ty) {
                            Some(raw_type(d.ty))
                        } else {
                            None
                        },
                    )?
                }
            }
        };
        if let Some(v) = value {
            self.store(d, v);
        }
        Ok(())
    }
    fn origin(&mut self, p: &Place) -> Option<u64> {
        if p.ty != Type::Int
            || !p.fields.is_empty()
            || !matches!(self.locals[p.local], Storage::Variable(_))
        {
            return None;
        }
        Some(
            *self
                .guards
                .integers
                .entry(p.local)
                .or_insert_with(|| guard_facts::fresh(&mut self.guard_serial)),
        )
    }
    fn guarded_bounds(&self, p: &Place) -> Bounds {
        // Narrow integers are always canonical (sign/zero extended) values of
        // their exact width; u64 raw values are not i64 facts.
        let mut bounds = self
            .integer_ranges
            .get(p, &self.integer_overrides)
            .intersect(crate::int_ranges::type_bounds(p.ty));
        if p.fields.is_empty() {
            if let Some(&origin) = self.guards.integers.get(&p.local) {
                for fact in &self.guards.active {
                    match *fact {
                        Fact::Within(id, low, high) if id == origin => {
                            bounds = bounds.intersect(Bounds { low, high })
                        }
                        Fact::BelowLength(id, _) if id == origin => {
                            bounds = bounds.intersect(Bounds {
                                low: i64::MIN,
                                high: i64::MAX - 1,
                            })
                        }
                        _ => {}
                    }
                }
            }
        }
        bounds
    }
    fn guarded_index(&mut self, owner: &Place, index: &Place) -> bool {
        let Some(&(_, len)) = self
            .array_metadata
            .get(&crate::immutable_descriptors::key(owner))
        else {
            return false;
        };
        let Some(origin) = self.origin(index) else {
            return false;
        };
        self.guarded_bounds(index).low >= 0
            && self
                .guards
                .active
                .contains(&Fact::BelowLength(origin, len.as_u32() as usize))
    }
    fn compare_guard(&mut self, op: &str, a: &Place, b: &Place) -> Boolean {
        let mut info = Boolean::unknown(guard_facts::fresh(&mut self.guard_serial));
        if a.ty != Type::Int || b.ty != Type::Int {
            return info;
        }
        let left = self.origin(a);
        let right = self.origin(b);
        let left_len = if a.fields.is_empty() {
            self.guards.lengths.get(&a.local).copied()
        } else {
            None
        };
        let right_len = if b.fields.is_empty() {
            self.guards.lengths.get(&b.local).copied()
        } else {
            None
        };
        match op {
            "<" => {
                if let (Some(index), Some(owner)) = (left, right_len) {
                    info.yes.insert(Fact::BelowLength(index, owner));
                }
            }
            ">=" => {
                if let (Some(index), Some(owner)) = (left, right_len) {
                    info.no.insert(Fact::BelowLength(index, owner));
                }
            }
            ">" => {
                if let (Some(owner), Some(index)) = (left_len, right) {
                    info.yes.insert(Fact::BelowLength(index, owner));
                }
            }
            "<=" => {
                if let (Some(owner), Some(index)) = (left_len, right) {
                    info.no.insert(Fact::BelowLength(index, owner));
                }
            }
            _ => {}
        }
        let mut add_bound = |origin: Option<u64>, bound: Bounds, op: &str| {
            let Some(origin) = origin else {
                return;
            };
            let low = bound.low as i128;
            let high = bound.high as i128;
            let min = i64::MIN as i128;
            let max = i64::MAX as i128;
            let pair = match op {
                "<" => Some(((min, high - 1), (low, max))),
                "<=" => Some(((min, high), (low + 1, max))),
                ">" => Some(((low + 1, max), (min, high))),
                ">=" => Some(((low, max), (min, high - 1))),
                _ => None,
            };
            if let Some((yes, no)) = pair {
                for (set, (low, high)) in [(&mut info.yes, yes), (&mut info.no, no)] {
                    let low = low.max(min);
                    let high = high.min(max);
                    if low <= high {
                        set.insert(Fact::Within(origin, low as i64, high as i64));
                    }
                }
            }
        };
        add_bound(
            left,
            self.integer_ranges.get(b, &self.integer_overrides),
            op,
        );
        let swapped = match op {
            "<" => ">",
            ">" => "<",
            "<=" => ">=",
            ">=" => "<=",
            _ => op,
        };
        add_bound(
            right,
            self.integer_ranges.get(a, &self.integer_overrides),
            swapped,
        );
        info
    }
    fn observe_init(&mut self, d: &Place, value: &Rvalue) {
        if !d.fields.is_empty() || !matches!(self.locals[d.local], Storage::Variable(_)) {
            return;
        }
        if d.ty == Type::Int {
            let from = if let Rvalue::Copy(p) = value {
                self.origin(p)
            } else {
                None
            };
            let length = match value {
                Rvalue::Copy(p) if p.fields.is_empty() => {
                    self.guards.lengths.get(&p.local).copied()
                }
                Rvalue::Builtin { name, args } if name == "len" => self
                    .array_metadata
                    .get(&crate::immutable_descriptors::key(&args[0]))
                    .map(|(_, len)| len.as_u32() as usize),
                _ => None,
            };
            self.guards.integers.insert(
                d.local,
                from.unwrap_or_else(|| guard_facts::fresh(&mut self.guard_serial)),
            );
            self.guards.lengths.remove(&d.local);
            if let Some(owner) = length {
                self.guards.lengths.insert(d.local, owner);
            }
        } else if d.ty == Type::Bool {
            let info = match value {
                Rvalue::Bool(b) => {
                    let mut info = Boolean::unknown(guard_facts::fresh(&mut self.guard_serial));
                    info.constant = Some(*b);
                    info
                }
                Rvalue::Copy(p) if p.fields.is_empty() => {
                    self.guards.boolean(p.local, &mut self.guard_serial)
                }
                Rvalue::Unary(op, p) if op == "!" && p.fields.is_empty() => self
                    .guards
                    .boolean(p.local, &mut self.guard_serial)
                    .negate(),
                Rvalue::Binary(op, a, b) => self.compare_guard(op, a, b),
                Rvalue::IntRange { value, low, high } => {
                    let mut info = Boolean::unknown(guard_facts::fresh(&mut self.guard_serial));
                    if let Some(origin) = self.origin(value) {
                        info.yes.insert(Fact::Within(origin, *low, *high));
                    }
                    info
                }
                _ => Boolean::unknown(guard_facts::fresh(&mut self.guard_serial)),
            };
            self.guards.booleans.insert(d.local, info);
        }
    }
    /// An `int` local held in an SSA variable: never address-exposed, so
    /// only its own direct definitions change it.
    fn tracked(&self, p: &Place) -> bool {
        p.fields.is_empty()
            && p.ty == Type::Int
            && matches!(self.locals.get(p.local), Some(Storage::Variable(_)))
    }
    /// The affine form of an operand while versioning (contract M4): a
    /// tracked local the loop never writes is a term; a tracked local
    /// defined earlier in this region has that definition's form; anything
    /// else is its proven interval at this point.
    fn affine(&self, p: &Place) -> Affine {
        if let Some(versioning) = &self.versioning {
            if self.tracked(p) {
                if !versioning.written.contains(&p.local) {
                    return Affine {
                        terms: BTreeMap::from([(p.local, 1)]),
                        low: 0,
                        high: 0,
                    };
                }
                if let Some(form) = versioning.forms.get(&p.local) {
                    return form.clone();
                }
            }
        }
        Affine::constant(self.guarded_bounds(p))
    }
    /// The form of the `int` value an `Init`/`Assign` stores, if affine.
    fn affine_operation(&self, op: &Operation) -> Option<Affine> {
        match op {
            Operation::Init(p, value) if p.fields.is_empty() && p.ty == Type::Int => match value {
                Rvalue::Copy(source) => Some(self.affine(source)),
                Rvalue::Int(n) => Some(Affine::constant(Bounds::exact(*n))),
                Rvalue::Binary(operator, a, b)
                    if (operator == "+" || operator == "-")
                        && a.ty == Type::Int
                        && b.ty == Type::Int =>
                {
                    let sign = if operator == "+" { 1 } else { -1 };
                    self.affine(a)
                        .combine(&self.affine(b), sign)
                        .filter(|f| f.low >= i64::MIN as i128 && f.high <= i64::MAX as i128)
                }
                _ => None,
            },
            Operation::Assign(p, source) if p.fields.is_empty() && p.ty == Type::Int => {
                Some(self.affine(source))
            }
            _ => None,
        }
    }
    /// After a definition: remember its form for later reads in the region.
    fn record_form(&mut self, p: &Place, form: Option<Affine>) {
        let tracked = self.tracked(p);
        if let Some(versioning) = self.versioning.as_mut() {
            versioning.current = None;
            if let (Some(form), true) = (form, tracked) {
                versioning.forms.insert(p.local, form);
            }
        }
    }
    /// Whether the checked `+`/`-` being emitted is versioned: its form has
    /// at least one invariant term and its guard fits the 16-guard budget.
    /// The guard is recorded for the pre-loop test.
    fn version_operation(&mut self) -> bool {
        let Some(versioning) = self.versioning.as_mut() else {
            return false;
        };
        let Some(form) = versioning.current.as_ref() else {
            return false;
        };
        // Versioning pays only when the guard admits a wide range of
        // invariant sums: with an interval of unknown values (`[MIN, MAX]`)
        // it would hold only for a zero sum, and the checked copy would run.
        // Keeping the 1.0 check is always allowed.
        const LIMIT: i128 = 1 << 62;
        if form.terms.is_empty() || form.low < -LIMIT || form.high > LIMIT {
            return false;
        }
        let check = (
            form.terms.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
            form.low as i64,
            form.high as i64,
        );
        if versioning.checks.contains(&check) {
            return true;
        }
        if versioning.checks.len() >= VERSION_MAX_CHECKS {
            return false;
        }
        versioning.checks.push(check);
        true
    }
    /// Contract M4 candidates: an innermost loop with a straight-line
    /// condition and body of at most 256 operations (no column plan) that
    /// contains a checked `int` `+`/`-` affine in a tracked local it never
    /// writes. Whether each such operation is versioned is decided while the
    /// fast copy is emitted, with the facts holding at that operation.
    /// Returns the invariant tracked locals any versioned form can name
    /// (operands of the loop's `int` `+`/`-` and copies), or `None`.
    fn versionable(
        &self,
        id: usize,
        condition_code: &[Operation],
        body: &[Operation],
        written: &BTreeSet<usize>,
    ) -> Option<BTreeSet<usize>> {
        // Straight-line (1.0 B's rule) and, for profitability, inline compute
        // only: a loop that calls a function or an out-of-line runtime helper
        // on every iteration gains nothing measurable from dropping an
        // overflow branch, while its second copy costs code size and
        // register pressure in the surrounding hot code.
        let straight = |ops: &[Operation]| {
            let types = self.env.types;
            let plain =
                |ty: Type| types.soa_element(ty).is_none() && types.map_entry_layout(ty).is_none();
            let scalar_vector = |ty: Type| {
                let element = match ty {
                    Type::VectorView(id) => types.vector_element(Type::Named(id)),
                    _ => types.vector_element(ty),
                };
                plain(ty) && element.is_some_and(scalar)
            };
            ops.iter().all(|op| match op {
                Operation::Init(destination, value) => match value {
                    Rvalue::Int(_)
                    | Rvalue::Bool(_)
                    | Rvalue::Float(_)
                    | Rvalue::Copy(_)
                    | Rvalue::Unary(..)
                    | Rvalue::IntRange { .. }
                    | Rvalue::Reference(_) => true,
                    Rvalue::Binary(operator, a, _) => {
                        !(operator == "%" && matches!(a.ty, Type::Float(_)))
                    }
                    Rvalue::Convert(source) => {
                        !matches!(source.ty, Type::Float(_))
                            || matches!(destination.ty, Type::Float(_))
                    }
                    Rvalue::Element { owner, .. } => plain(owner.ty),
                    Rvalue::Builtin { name, args } => {
                        crate::syntax::integer_intrinsic(name).is_some()
                            || name.starts_with("read_u")
                            || name.starts_with("try_write_u")
                            || matches!(
                                name.as_str(),
                                "len"
                                    | "at"
                                    | "byte_at"
                                    | "set"
                                    | "push"
                                    | "fixed_len"
                                    | "fixed_at"
                                    | "fixed_set"
                                    | "fixed_try_set"
                                    | "saturating_add"
                                    | "saturating_sub"
                            )
                            || (matches!(
                                name.as_str(),
                                "vec_at" | "vec_replace" | "vec_push" | "vec_pop" | "vec_capacity"
                            ) && args.first().is_some_and(|owner| scalar_vector(owner.ty)))
                    }
                    _ => false,
                },
                Operation::ElementCheckPair { owner, .. } => {
                    plain(owner.ty) && types.arena_element(owner.ty).is_none()
                }
                Operation::Assign(..) | Operation::Assert { .. } | Operation::Loan { .. } => true,
                _ => false,
            })
        };
        if self.versioning.is_some()
            || self.column_loops.contains_key(&id)
            || !straight(condition_code)
            || !straight(body)
            || condition_code.len() + body.len() > VERSION_MAX_OPERATIONS
        {
            return None;
        }
        let invariant = |p: &Place| self.tracked(p) && !written.contains(&p.local);
        let mut candidate = false;
        let mut terms = BTreeSet::new();
        for region in [condition_code, body] {
            let mut affine = BTreeSet::new();
            for op in region {
                let term = |p: &Place, affine: &BTreeSet<usize>| {
                    invariant(p) || (self.tracked(p) && affine.contains(&p.local))
                };
                let (destination, has) = match op {
                    Operation::Init(p, Rvalue::Binary(operator, a, b))
                        if (operator == "+" || operator == "-")
                            && a.ty == Type::Int
                            && b.ty == Type::Int =>
                    {
                        let has = term(a, &affine) || term(b, &affine);
                        candidate |= has;
                        (p, has)
                    }
                    Operation::Init(p, Rvalue::Copy(source)) | Operation::Assign(p, source) => {
                        (p, term(source, &affine))
                    }
                    Operation::Init(p, _) => (p, false),
                    _ => continue,
                };
                if let Operation::Init(_, Rvalue::Binary(_, a, b)) = op {
                    terms.extend([a, b].into_iter().filter(|p| invariant(p)).map(|p| p.local));
                }
                if let Operation::Init(_, Rvalue::Copy(source)) | Operation::Assign(_, source) = op
                {
                    if invariant(source) {
                        terms.insert(source.local);
                    }
                }
                if has {
                    affine.insert(destination.local);
                } else {
                    affine.remove(&destination.local);
                }
            }
        }
        candidate.then_some(terms)
    }
    /// Emit one loop (head test, body, rotated tail test) from the current
    /// state, and return the facts holding at its exit.
    fn emit_loop(
        &mut self,
        id: usize,
        condition_code: &[Operation],
        condition: &Place,
        body: &[Operation],
    ) -> Result<Guards> {
        // Every fact kept here concerns a value no iteration changes, and
        // this point dominates the loop exit.
        let entry_guards = self.guards.clone();
        let head = self.b.create_block();
        let loop_body = self.b.create_block();
        let end = self.b.create_block();
        self.loop_targets.insert(id, (head, end));
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        self.new_region();
        self.condition_depth += 1;
        let emitted = self.block(condition_code);
        self.condition_depth -= 1;
        emitted?;
        let predicate = self.guards.boolean(condition.local, &mut self.guard_serial);
        let c = self.load(condition);
        self.b.ins().brif(c, loop_body, &[], end, &[]);
        // Without a `break`, the exit is reached only from a false test: the
        // state after that test, on each edge.
        let mut exit = self.guards.clone();
        exit.active.extend(predicate.no.iter().cloned());
        self.b.switch_to_block(loop_body);
        self.guards.active.extend(predicate.yes.iter().cloned());
        self.new_region();
        if !self.block(body)? {
            // Rotate cheap loop tests: keep the initial zero-iteration check,
            // then branch directly from the tail to the body. Every test and
            // its dynamic checks still run once per iteration;
            // expensive/control-flow conditions stay shared.
            let (mut size, mut accesses) = (24, 4);
            let rotate = self.cheap_condition(condition_code, &mut size, &mut accesses);
            if rotate {
                self.new_region();
                self.condition_depth += 1;
                let emitted = self.block(condition_code);
                self.condition_depth -= 1;
                emitted?;
                let predicate = self.guards.boolean(condition.local, &mut self.guard_serial);
                let c = self.load(condition);
                self.b.ins().brif(c, loop_body, &[], end, &[]);
                let mut tail = self.guards.clone();
                tail.active.extend(predicate.no.iter().cloned());
                exit = Guards::join_by_local(&exit, &tail, &mut self.guard_serial);
            } else {
                // The shared head is the only exiting test.
                self.b.ins().jump(head, &[]);
            }
        }
        self.loop_targets.remove(&id);
        self.b.switch_to_block(end);
        Ok(if breaks_out(condition_code, id) || breaks_out(body, id) {
            entry_guards
        } else {
            exit
        })
    }
    /// Affine forms describe values computed earlier in the same straight-line
    /// region of the same execution; a new region (the head test, the body,
    /// the rotated tail test) starts without any.
    fn new_region(&mut self) {
        if let Some(versioning) = self.versioning.as_mut() {
            versioning.forms.clear();
            versioning.current = None;
        }
    }
    /// Contract M4: one pre-loop guard selects a fast copy of the loop whose
    /// versioned `+`/`-` run unchecked, or the original checked loop with its
    /// exact faults, messages and order. Both copies start from the same
    /// state and meet after the loop.
    fn versioned_loop(
        &mut self,
        id: usize,
        condition_code: &[Operation],
        condition: &Place,
        body: &[Operation],
        written: BTreeSet<usize>,
        terms: BTreeSet<usize>,
    ) -> Result<Guards> {
        // The loop-invariant values every possible guard term can name, read
        // here (unused ones are dead code), so the guard emitted after both
        // copies needs no new variable lookups.
        let values: BTreeMap<usize, Value> = terms
            .into_iter()
            .filter_map(|local| match self.locals[local] {
                Storage::Variable(variable) => Some((local, self.b.use_var(variable))),
                _ => None,
            })
            .collect();
        let before = self.b.current_block().expect("loop entry block");
        let guard = self.b.create_block();
        let fast = self.b.create_block();
        let checked = self.b.create_block();
        let join = self.b.create_block();
        self.b.ins().jump(guard, &[]);
        // The guard is emitted once the fast copy has named its checks; lay
        // it out where it executes, right before both copies.
        self.b.insert_block_after(guard, before);
        let locals = self.locals.clone();
        let metadata = self.array_metadata.clone();
        let spans = self.spans.clone();
        let guards = self.guards.clone();
        let overrides = self.integer_overrides.clone();
        let budget = self.inline_budget;
        self.b.switch_to_block(fast);
        self.versioning = Some(Versioning {
            written,
            forms: BTreeMap::new(),
            current: None,
            checks: Vec::new(),
        });
        let fast_exit = self.emit_loop(id, condition_code, condition, body);
        let versioning = self.versioning.take().expect("versioning state");
        let fast_exit = fast_exit?;
        self.b.ins().jump(join, &[]);
        let fast_metadata = std::mem::replace(&mut self.array_metadata, metadata);
        self.spans = spans;
        if versioning.checks.is_empty() {
            // Nothing was versioned: the only copy is the original loop.
            self.b.switch_to_block(guard);
            self.b.ins().jump(fast, &[]);
            self.b.switch_to_block(join);
            self.array_metadata
                .retain(|key, value| fast_metadata.get(key) == Some(value));
            return Ok(fast_exit);
        }
        let spans = self.spans.clone();
        self.locals = locals;
        self.guards = guards;
        self.integer_overrides = overrides;
        self.inline_budget = budget;
        // The checked copy stays in place (not cold): cold blocks are sunk
        // to the end of the function, which would keep every value it uses
        // live across all the hot code in between.
        self.b.switch_to_block(checked);
        let checked_exit = self.emit_loop(id, condition_code, condition, body)?;
        self.b.ins().jump(join, &[]);
        self.spans = spans;
        self.b.switch_to_block(guard);
        let valid = self.version_guard(&versioning.checks, &values);
        self.b.ins().brif(valid, fast, &[], checked, &[]);
        self.b.switch_to_block(join);
        self.array_metadata
            .retain(|key, value| fast_metadata.get(key) == Some(value));
        Ok(Guards::join_by_local(
            &fast_exit,
            &checked_exit,
            &mut self.guard_serial,
        ))
    }
    /// The pre-loop test of every recorded guard: each invariant sum is
    /// computed exactly (wrapping arithmetic plus a sign test for signed
    /// overflow) and `sum + [low, high]` must lie within i64.
    fn version_guard(&mut self, checks: &[VersionCheck], values: &BTreeMap<usize, Value>) -> Value {
        let mut valid: Option<Value> = None;
        let mut all = |b: &mut FunctionBuilder<'_>, condition: Value| {
            valid = Some(match valid {
                Some(all) => b.ins().band(all, condition),
                None => condition,
            });
        };
        for (terms, low, high) in checks {
            let mut sum: Option<Value> = None;
            for &(local, coefficient) in terms {
                let value = values[&local];
                sum = Some(match sum {
                    None if coefficient > 0 => value,
                    None => {
                        // 0 - v overflows only for v == i64::MIN.
                        let exact = self.b.ins().icmp_imm_s(IntCC::NotEqual, value, i64::MIN);
                        all(&mut self.b, exact);
                        self.b.ins().ineg(value)
                    }
                    Some(sum) => {
                        let (result, overflow) = if coefficient > 0 {
                            let result = self.b.ins().iadd(sum, value);
                            let x = self.b.ins().bxor(result, sum);
                            let y = self.b.ins().bxor(result, value);
                            (result, self.b.ins().band(x, y))
                        } else {
                            let result = self.b.ins().isub(sum, value);
                            let x = self.b.ins().bxor(sum, value);
                            let y = self.b.ins().bxor(sum, result);
                            (result, self.b.ins().band(x, y))
                        };
                        let exact =
                            self.b
                                .ins()
                                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, overflow, 0);
                        all(&mut self.b, exact);
                        result
                    }
                });
            }
            let sum = sum.expect("versioned guard has a term");
            if *low < 0 {
                let fits =
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, sum, i64::MIN - *low);
                all(&mut self.b, fits);
            }
            if *high > 0 {
                let fits =
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::SignedLessThanOrEqual, sum, i64::MAX - *high);
                all(&mut self.b, fits);
            }
        }
        valid.unwrap_or_else(|| self.b.ins().iconst(I8, 1))
    }
    fn conditional<'c>(
        &mut self,
        mut condition: &'c Place,
        mut yes: &'c [Operation],
        mut no: &'c [Operation],
    ) -> Result<bool> {
        // Checked else-if conditions live at the end of each false branch,
        // after their own condition prefix. Emit that spine without retaining
        // the large general operation-emission frame for every alternative.
        let mut pending = Vec::new();
        let mut no_returns = loop {
            let predicate = self.guards.boolean(condition.local, &mut self.guard_serial);
            let before_guards = self.guards.clone();
            let y = self.b.create_block();
            let n = self.b.create_block();
            let end = self.b.create_block();
            let c = self.load(condition);
            self.b.ins().brif(c, y, &[], n, &[]);
            self.b.switch_to_block(y);
            self.guards.active.extend(predicate.yes.iter().cloned());
            let yes_returns = self.block(yes)?;
            let yes_guards = self.guards.clone();
            if !yes_returns {
                self.b.ins().jump(end, &[]);
            }
            self.b.switch_to_block(n);
            self.guards = before_guards;
            self.guards.active.extend(predicate.no.iter().cloned());
            pending.push((predicate, yes_guards, yes_returns, end));
            if let Some((
                Operation::If {
                    condition: next_condition,
                    yes: next_yes,
                    no: next_no,
                },
                prefix,
            )) = no.split_last()
            {
                if self.block(prefix)? {
                    break true;
                }
                condition = next_condition;
                yes = next_yes;
                no = next_no;
            } else {
                break self.block(no)?;
            }
        };
        for (predicate, yes_guards, yes_returns, end) in pending.into_iter().rev() {
            let no_guards = self.guards.clone();
            self.guards = match (yes_returns, no_returns) {
                (true, false) => no_guards,
                (false, true) => yes_guards,
                _ => Guards::merge(&predicate, &yes_guards, &no_guards, &mut self.guard_serial),
            };
            if !no_returns {
                self.b.ins().jump(end, &[]);
            }
            no_returns &= yes_returns;
            if !no_returns {
                self.b.switch_to_block(end);
            }
        }
        Ok(no_returns)
    }
    fn block(&mut self, ops: &[Operation]) -> Result<bool> {
        let inherited = self.array_metadata.clone();
        let inherited_spans = self.spans.clone();
        let result = self.block_inner(ops);
        // A descriptor learned only on one conditional path cannot dominate
        // its siblings. Invalidation in a nested block remains invalidation.
        self.array_metadata
            .retain(|key, value| inherited.get(key) == Some(value));
        // Span validity/limits are pure SSA facts; only their dominance is
        // scoped, exactly like the descriptor values they are keyed by.
        self.spans.retain_inherited(&inherited_spans);
        result
    }
    fn block_inner(&mut self, ops: &[Operation]) -> Result<bool> {
        for op in ops {
            // Contract M4: the affine form of this definition, read before it
            // executes (its destination may be an operand).
            let form = self
                .versioning
                .is_some()
                .then(|| self.affine_operation(op))
                .flatten();
            if let Some(versioning) = self.versioning.as_mut() {
                let mut written = BTreeSet::new();
                written_locals(std::slice::from_ref(op), &mut written);
                for local in written {
                    versioning.forms.remove(&local);
                }
                versioning.current = form.clone();
            }
            match op {
                Operation::Init(p, v) => {
                    self.array_metadata
                        .retain(|(local, _), _| *local != p.local);
                    let stable = match v {
                        Rvalue::Builtin { name, .. } => {
                            descriptor_builtin(name, p.ty)
                                || matches!(name.as_str(), "print_int" | "print_bool")
                        }
                        Rvalue::Call {
                            name, spawn: false, ..
                        } => self.env.descriptor_stable.contains(name),
                        Rvalue::Call { .. } => false,
                        _ => true,
                    };
                    if !stable {
                        self.clear_metadata();
                    }
                    if let Rvalue::Call { name, args, .. } = v {
                        // A shared call cannot mutate the caller's tracked
                        // scalar versions or immutable descriptors. Exclusive
                        // references still invalidate all transient facts.
                        let (_, callee) = self.env.funcs[name];
                        if args.iter().zip(&callee.source.params).any(|(arg, param)| {
                            matches!(arg, Argument::Reference(_))
                                && param.borrow == Borrow::Exclusive
                        }) {
                            self.guards.clear_preserving_values();
                        }
                    }
                    // The enclosure of this checked integer result under the
                    // facts holding here (read before the destination, which
                    // may be an operand, is rewritten).
                    let result = match v {
                        Rvalue::Binary(op, a, b)
                            if p.ty == Type::Int
                                && a.ty == Type::Int
                                && ["+", "-", "*", "*literal", "/", "%"].contains(&op.as_str()) =>
                        {
                            Some(self.guarded_bounds(a).binary(op, self.guarded_bounds(b)))
                        }
                        _ => None,
                    };
                    self.init(p, v)?;
                    // Anything learned about the destination while computing
                    // its new value describes the overwritten one.
                    self.array_metadata
                        .retain(|(local, _), _| *local != p.local);
                    self.observe_init(p, v);
                    if let Some(bounds) = result.filter(|b| *b != Bounds::FULL) {
                        if let Some(origin) = self.origin(p) {
                            self.guards.active.insert(Fact::Within(
                                origin,
                                bounds.low,
                                bounds.high,
                            ));
                        }
                    }
                    self.record_form(p, form);
                }
                Operation::Move {
                    destination,
                    source,
                    ..
                } => {
                    self.array_metadata.retain(|(local, _), _| {
                        *local != source.local && *local != destination.local
                    });
                    self.transfer(destination, source)?;
                    self.zero(source)?;
                }
                Operation::Assign(d, s) => {
                    self.array_metadata
                        .retain(|(local, _), _| *local != d.local);
                    self.transfer(d, s)?;
                    self.observe_init(d, &Rvalue::Copy(s.clone()));
                    self.record_form(d, form);
                }
                Operation::ElementCheckPair { owner, left, right }
                    if self.env.types.arena_element(owner.ty).is_none()
                        && self.env.types.soa_element(owner.ty).is_none() =>
                {
                    // Inline zn_vec_check_pair / zn_span_check_pair with their
                    // exact order: one index fault for either index, then the
                    // distinctness fault. Views also reject negatives
                    // explicitly (a foreign length may exceed INT64_MAX).
                    let span = matches!(owner.ty, Type::VectorView(_));
                    let (data, len) = self.descriptor(owner);
                    let element = self.env.types.vector_element(owner.ty).unwrap();
                    let size = self.env.types.layout(element).size;
                    let valid = self.valid_spans.contains(&(data, len, size));
                    let l = self.load(left);
                    let r = self.load(right);
                    let mut bad = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, l, len);
                    let right_bad = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, r, len);
                    bad = self.b.ins().bor(bad, right_bad);
                    if span {
                        let negative = self.b.ins().bor(l, r);
                        let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, negative, 0);
                        bad = self.b.ins().bor(bad, negative);
                    }
                    self.fail_if(
                        bad,
                        if span {
                            "vector region index out of bounds"
                        } else {
                            "vector index out of bounds"
                        },
                    )?;
                    let same = self.b.ins().icmp(IntCC::Equal, l, r);
                    self.fail_if(
                        same,
                        if span {
                            "vector pair indices must be distinct"
                        } else {
                            "at_pair indices must be distinct"
                        },
                    )?;
                    // Both indices are now `0 <= i <u len`: below INT64_MAX
                    // for nominal vectors (runtime invariant `len <=
                    // INT64_MAX / width`); a foreign view's length may exceed
                    // INT64_MAX unless a dominating check proved otherwise.
                    let (high, below) = if span && !valid {
                        (i64::MAX, self.known_nonnegative(len))
                    } else {
                        (i64::MAX / size.max(1) as i64 - 1, true)
                    };
                    self.index_facts(left, len, high, below);
                    self.index_facts(right, len, high, below);
                }
                Operation::ElementCheckPair { owner, left, right } => {
                    let helper = if self.env.types.arena_element(owner.ty).is_some() {
                        "zn_arena_check_pair"
                    } else if matches!(owner.ty, Type::VectorView(_)) {
                        "zn_span_check_pair"
                    } else {
                        "zn_vec_check_pair"
                    };
                    let owner = self.address(owner);
                    let left = self.load(left);
                    let right = self.load(right);
                    self.call(helper, &[owner, left, right], None)?;
                }
                Operation::ElementWriteback {
                    alias,
                    owner,
                    index,
                } => {
                    self.array_metadata
                        .retain(|(local, _), _| *local != owner.local && *local != alias.local);
                    if self.direct_soa_aliases.contains(&alias.local) {
                        continue;
                    }
                    let source = self.address(alias);
                    let container = self.address(owner);
                    let index = self.load(index);
                    let (count, meta) = self.soa_metadata(owner.ty);
                    self.call(
                        "zn_soa_store",
                        &[container, index, source, count, meta],
                        None,
                    )?;
                    self.guards.clear_preserving_values();
                }
                Operation::Swap(a, b) => {
                    self.array_metadata
                        .retain(|(local, _), _| *local != a.local && *local != b.local);
                    if scalar(a.ty) {
                        let av = self.load(a);
                        let bv = self.load(b);
                        self.store(a, bv);
                        self.store(b, av);
                    } else {
                        let ap = self.address(a);
                        let bp = self.address(b);
                        let temp = self.stack_value(a.ty);
                        let size = self.env.types.layout(a.ty).size;
                        self.copy_bytes(temp, ap, size)?;
                        self.copy_bytes(ap, bp, size)?;
                        self.copy_bytes(bp, temp, size)?;
                    }
                    self.integer_overrides.clear();
                    // Only the two swapped places change. Guard facts track
                    // scalar `Variable` locals only, so a swap of memory
                    // places (element aliases) leaves every tracked version
                    // intact; swapped scalar locals get fresh versions.
                    for local in [a.local, b.local] {
                        self.guards.integers.remove(&local);
                        self.guards.lengths.remove(&local);
                        self.guards.booleans.remove(&local);
                    }
                }
                Operation::Drop(p) => {
                    self.array_metadata
                        .retain(|(local, _), _| *local != p.local);
                    if matches!(self.locals[p.local], Storage::Descriptor { .. }) {
                        self.zero(p)?;
                    } else {
                        let a = self.address(p);
                        self.drop_at(p.ty, a)?;
                    }
                }
                Operation::Loan { .. } => {}
                Operation::Assert {
                    value,
                    line,
                    message,
                } => {
                    // zn_assert(true, ..) has no effect, so only the failing
                    // path calls it (it reports the line and never returns).
                    let v = self.load(value);
                    let fail = self.b.create_block();
                    let ok = self.b.create_block();
                    self.b.set_cold_block(fail);
                    self.b.ins().brif(v, ok, &[], fail, &[]);
                    self.b.switch_to_block(fail);
                    let l = self.b.ins().iconst(I32, *line as i64);
                    let m = self.string(message)?;
                    self.call("zn_assert", &[v, l, m], None)?;
                    self.b.ins().trap(cl::TrapCode::user(1).unwrap());
                    self.b.switch_to_block(ok);
                    // zn_assert never returns for a false value, so every
                    // fact of the asserted predicate holds from here on.
                    if value.fields.is_empty() {
                        let predicate = self.guards.boolean(value.local, &mut self.guard_serial);
                        self.guards.active.extend(predicate.yes.iter().cloned());
                    }
                }
                Operation::Break(id) | Operation::Continue(id) => {
                    let &(head, end) = self
                        .loop_targets
                        .get(id)
                        .ok_or_else(|| err("missing checked loop target"))?;
                    self.b.ins().jump(
                        if matches!(op, Operation::Break(_)) {
                            end
                        } else {
                            head
                        },
                        &[],
                    );
                    return Ok(true);
                }
                Operation::Return(v) => {
                    if let Some(exit) = self.inline_return {
                        if let Some(p) = v {
                            let v = self.load(p);
                            self.b.ins().jump(exit, &[v.into()]);
                        } else {
                            self.b.ins().jump(exit, &[]);
                        }
                        return Ok(true);
                    }
                    if let Some(p) = v {
                        if scalar(p.ty) {
                            let raw = self.load(p);
                            let v = if let Type::Float(n) = p.ty {
                                self.float_value(raw, n)
                            } else {
                                raw
                            };
                            self.b.ins().return_(&[v]);
                        } else {
                            let d = self.out.unwrap();
                            let s = self.address(p);
                            self.copy_bytes(d, s, self.env.types.layout(p.ty).size)?;
                            self.b.ins().return_(&[]);
                        }
                    } else {
                        self.b.ins().return_(&[]);
                    }
                    return Ok(true);
                }
                Operation::If { condition, yes, no } => {
                    if self.conditional(condition, yes, no)? {
                        return Ok(true);
                    }
                }
                Operation::Loop {
                    id,
                    condition_code,
                    condition,
                    body,
                } => {
                    // Metadata captured before a repeating region may only
                    // survive if every iteration keeps descriptors stable.
                    let stable = stable_descriptors(condition_code, self.env.descriptor_stable)
                        && stable_descriptors(body, self.env.descriptor_stable);
                    if !stable {
                        self.clear_metadata();
                    }
                    let saved_columns = self.hoisted_columns.clone();
                    let saved_lengths = self.hoisted_lengths.clone();
                    if let Some(plan) = self
                        .column_loops
                        .get(id)
                        .cloned()
                        .filter(|plan| self.env.types.soa_element(plan.owner.ty).is_some())
                    {
                        let pointer = self.address(&plan.owner);
                        let data = self.b.ins().load(I64, MemFlagsData::new(), pointer, 0);
                        let length = self.b.ins().load(I64, MemFlagsData::new(), pointer, 8);
                        let capacity = self.b.ins().load(I64, MemFlagsData::new(), pointer, 16);
                        let mut offset = self.b.ins().iconst(I64, 0);
                        let mut bases = Vec::new();
                        for (_, width) in self.env.types.soa_columns(plan.owner.ty).unwrap() {
                            bases.push(self.b.ins().iadd(data, offset));
                            let bytes = self.b.ins().imul_imm_s(capacity, width as i64);
                            let padded = self.b.ins().iadd_imm_s(bytes, 7);
                            let padded = self.b.ins().band_imm_s(padded, -8);
                            offset = self.b.ins().iadd(offset, padded);
                        }
                        let updates = match &plan.simd {
                            Some(ColumnSimd::Constant {
                                column,
                                float,
                                operator,
                                constant,
                            }) => Some(vec![ColumnUpdate {
                                column: *column,
                                float: *float,
                                value: ColumnExpr::Binary {
                                    operator: operator.clone(),
                                    left: Box::new(ColumnExpr::Column(*column)),
                                    right: Box::new(ColumnExpr::Constant(*constant)),
                                },
                            }]),
                            Some(ColumnSimd::Columns { updates, .. }) => Some(updates.clone()),
                            _ => None,
                        };
                        if let Some(updates) = updates.filter(|updates| !updates.is_empty()) {
                            self.column_simd(&updates, &bases, length)?;
                            self.guards.clear_preserving_values();
                            continue;
                        }
                        self.hoisted_columns.insert(plan.alias, bases);
                        self.hoisted_lengths.insert(plan.alias, length);
                    }
                    // Incoming facts may describe only the first iteration.
                    // Re-establish facts from this loop's executed condition.
                    // Versions held by never-rewritten locals are the same
                    // values on every iteration and keep their facts.
                    // A local this loop never writes holds the same value on
                    // every iteration too.
                    let mut written = BTreeSet::new();
                    written_locals(condition_code, &mut written);
                    written_locals(body, &mut written);
                    if stable {
                        self.hoist_descriptors(condition_code, body, &written);
                    }
                    let ranges = &self.integer_ranges;
                    self.guards.enter_loop_preserving(|local| {
                        ranges.immutable(local) || !written.contains(&local)
                    });
                    let exit = if let Some(terms) =
                        self.versionable(*id, condition_code, body, &written)
                    {
                        self.versioned_loop(*id, condition_code, condition, body, written, terms)?
                    } else {
                        self.emit_loop(*id, condition_code, condition, body)?
                    };
                    self.hoisted_columns = saved_columns;
                    self.hoisted_lengths = saved_lengths;
                    self.guards = exit;
                }
                Operation::Switch { value, arms } => {
                    let before_guards = self.guards.clone();
                    let a = self.address(value);
                    let tag = self.b.ins().load(I64, MemFlagsData::new(), a, 0);
                    let end = self.b.create_block();
                    let mut any = false;
                    if arms.len() >= 4 {
                        let index = self.b.ins().iadd_imm_s(tag, -1);
                        let invalid = self.b.ins().icmp_imm_s(
                            IntCC::UnsignedGreaterThanOrEqual,
                            index,
                            arms.len() as i64,
                        );
                        self.fail_if(invalid, "invalid enum tag")?;
                        // Validate all 64 tag bits before the jump-table's I32
                        // index, so huge/zero tags cannot alias a valid arm.
                        let index = self.b.ins().ireduce(I32, index);
                        let blocks: Vec<_> = arms.iter().map(|_| self.b.create_block()).collect();
                        let otherwise = self.trap_blocks["invalid enum tag"];
                        let default = self.b.func.dfg.block_call(otherwise, &[]);
                        let targets: Vec<_> = blocks
                            .iter()
                            .map(|&block| self.b.func.dfg.block_call(block, &[]))
                            .collect();
                        let table = self
                            .b
                            .create_jump_table(cl::JumpTableData::new(default, &targets));
                        self.b.ins().br_table(index, table);
                        for (block, ops) in blocks.into_iter().zip(arms) {
                            self.b.switch_to_block(block);
                            self.guards = before_guards.clone();
                            if !self.block(ops)? {
                                self.b.ins().jump(end, &[]);
                                any = true;
                            }
                        }
                        if !any {
                            return Ok(true);
                        }
                        self.b.switch_to_block(end);
                        self.guards.clear();
                        self.guards.active = before_guards.value_facts();
                        continue;
                    }
                    for (i, ops) in arms.iter().enumerate() {
                        let arm = self.b.create_block();
                        let next = self.b.create_block();
                        let c = self.b.ins().icmp_imm_s(IntCC::Equal, tag, (i + 1) as i64);
                        self.b.ins().brif(c, arm, &[], next, &[]);
                        self.b.switch_to_block(arm);
                        self.guards = before_guards.clone();
                        if !self.block(ops)? {
                            self.b.ins().jump(end, &[]);
                            any = true;
                        }
                        self.b.switch_to_block(next);
                    }
                    let m = self.string("invalid enum tag")?;
                    self.call("zn_fail", &[m], None)?;
                    self.b.ins().trap(cl::TrapCode::user(1).unwrap());
                    if !any {
                        return Ok(true);
                    }
                    self.b.switch_to_block(end);
                    self.guards.clear();
                    self.guards.active = before_guards.value_facts();
                }
            }
        }
        Ok(false)
    }
}
/// Seal every block once all code is emitted, latest-created first. Blocks
/// are created in emission order, so a loop's body and tail are sealed before
/// its header: the back edge's value of a variable the loop never assigns
/// then resolves to the header's own placeholder, and the SSA builder drops
/// that parameter instead of carrying the invariant around the loop as a
/// block argument. (Sealing order affects only SSA minimality, never which
/// value a variable reads.)
fn seal_inner_first(b: &mut FunctionBuilder<'_>) {
    for index in (0..b.func.dfg.num_blocks()).rev() {
        b.seal_block(cl::Block::from_u32(index as u32));
    }
}
fn align(n: usize, a: usize) -> usize {
    n.div_ceil(a) * a
}
fn packet_size(f: &CheckedFunction, types: &TypeTable) -> usize {
    let mut n = 0;
    for p in &f.source.params {
        let l = types.layout(p.ty);
        n = align(n, l.align) + l.size;
    }
    n.max(1)
}
#[cfg(test)]
thread_local! {
    static ISA_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(crate) struct Analysis<'a> {
    pub hot: BTreeSet<String>,
    descriptor_stable: BTreeSet<String>,
    pinned_views: BTreeMap<String, Vec<usize>>,
    inline_costs: BTreeMap<String, usize>,
    predicates: BTreeMap<String, crate::predicates::Expr>,
    pub workers: BTreeSet<String>,
    integer_functions: FunctionRanges<'a>,
    isa: Result<Arc<dyn cranelift_codegen::isa::TargetIsa>>,
}
fn target_isa() -> Result<Arc<dyn cranelift_codegen::isa::TargetIsa>> {
    #[cfg(test)]
    ISA_BUILDS.with(|count| count.set(count.get() + 1));
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").map_err(err)?;
    // Keep entry layout stable across whole-program and module emission.
    flags.set("log2_min_function_alignment", "4").map_err(err)?;
    flags.set("enable_verifier", "true").map_err(err)?;
    flags.set("enable_probestack", "true").map_err(err)?;
    flags.set("probestack_strategy", "inline").map_err(err)?;
    flags.set("unwind_info", "true").map_err(err)?;
    let triple = "x86_64-pc-windows-msvc".parse().map_err(err)?;
    let mut isa = cranelift_codegen::isa::lookup(triple).map_err(err)?;
    let features = crate::backend::native_isa_features();
    for (name, bit) in [("has_popcnt", 1), ("has_lzcnt", 2), ("has_bmi1", 4)] {
        isa.set(name, if features & bit != 0 { "true" } else { "false" })
            .map_err(err)?;
    }
    isa.finish(settings::Flags::new(flags)).map_err(err)
}

fn referenced_channels(
    program: &CheckedProgram,
    unit: Option<&crate::native_units::Unit>,
) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<_> = program
        .functions
        .iter()
        .filter(|f| {
            f.source.external_symbol.is_none()
                && unit.is_none_or(|u| u.functions.contains(&f.source.name))
        })
        .flat_map(|f| {
            f.locals
                .iter()
                .map(|l| l.ty)
                .chain(f.source.params.iter().map(|p| p.ty))
                .chain(std::iter::once(f.source.ret))
        })
        .collect();
    while let Some(ty) = pending.pop() {
        let Type::Named(id) = ty.task_result().unwrap_or(ty) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        // Include payloads recursively, including channels inside records,
        // variants, vectors, maps, arenas and a worker's owned result packet.
        let fields = match &program.types.definitions[id].kind {
            DataKind::Record(fields)
            | DataKind::Enum(fields)
            | DataKind::Vector(fields)
            | DataKind::SoaVector(fields)
            | DataKind::Map(fields)
            | DataKind::Arena(fields)
            | DataKind::Channel(fields) => fields,
        };
        pending.extend(fields.iter().map(|f| f.ty));
    }
    program
        .types
        .order
        .iter()
        .copied()
        .filter(|id| seen.contains(id) && program.types.channel_element(Type::Named(*id)).is_some())
        .collect()
}
pub(crate) fn analyze(program: &CheckedProgram) -> Analysis<'_> {
    let hot = crate::cgen::hot_names(program);
    let mut descriptor_stable = BTreeSet::new();
    loop {
        let before = descriptor_stable.len();
        for f in &program.functions {
            if f.source.external_symbol.is_none() && stable_descriptors(&f.body, &descriptor_stable)
            {
                descriptor_stable.insert(f.source.name.clone());
            }
        }
        if descriptor_stable.len() == before {
            break;
        }
    }
    let integer_functions = FunctionRanges::new(program.functions.iter());
    for f in program
        .functions
        .iter()
        .filter(|f| f.source.external_symbol.is_none())
    {
        integer_functions.analyze(f, &[]);
    }
    Analysis {
        hot,
        descriptor_stable,
        pinned_views: crate::immutable_descriptors::pinned_views(program.functions.iter()),
        inline_costs: program
            .functions
            .iter()
            .filter(|f| f.source.external_symbol.is_none())
            .filter_map(|f| inline_cost(f).map(|c| (f.source.name.clone(), c)))
            .collect(),
        predicates: crate::predicates::analyze(program),
        workers: crate::native_units::workers(program),
        integer_functions,
        isa: target_isa(),
    }
}
pub fn object(program: &CheckedProgram) -> Result<Vec<u8>> {
    object_impl(program, &analyze(program), None, None)
}
/// Codegen introspection: the verified, pre-optimization Cranelift IR of every
/// user function, exactly as the native backend emits it for [`object`].
pub fn clif(program: &CheckedProgram) -> Result<String> {
    let mut text = String::new();
    object_impl(program, &analyze(program), None, Some(&mut text))?;
    Ok(text)
}
pub(crate) fn object_unit(
    program: &CheckedProgram,
    analysis: &Analysis<'_>,
    unit: &crate::native_units::Unit,
) -> Result<Vec<u8>> {
    object_impl(program, analysis, Some(unit), None)
}
fn object_impl(
    program: &CheckedProgram,
    analysis: &Analysis<'_>,
    unit: Option<&crate::native_units::Unit>,
    clif: Option<&mut String>,
) -> Result<Vec<u8>> {
    let defines = |name: &str| unit.is_none_or(|u| u.functions.contains(name));
    let isa = Arc::clone(analysis.isa.as_ref().map_err(Clone::clone)?);
    let builder =
        ObjectBuilder::new(isa, "anode", cranelift_module::default_libcall_names()).map_err(err)?;
    let mut env = Environment {
        module: ObjectModule::new(builder),
        types: &program.types,
        funcs: BTreeMap::new(),
        imports: BTreeMap::new(),
        workers: BTreeMap::new(),
        channel_drops: BTreeMap::new(),
        serial: 0,
        strings: BTreeMap::new(),
        unwind: Vec::new(),
        inline_costs: &analysis.inline_costs,
        descriptor_stable: &analysis.descriptor_stable,
        pinned_views: &analysis.pinned_views,
        predicates: &analysis.predicates,
        integer_functions: &analysis.integer_functions,
        clif: clif.as_ref().map(|_| String::new()),
    };
    debug_assert!(std::ptr::eq(
        env.module.isa(),
        analysis.isa.as_ref().unwrap().as_ref()
    ));
    for f in program
        .functions
        .iter()
        .filter(|f| unit.is_none_or(|u| u.required.contains(&f.source.name)))
    {
        let id = env
            .module
            .declare_function(
                &f.source
                    .external_symbol
                    .clone()
                    .unwrap_or_else(|| format!("zn_fn_{}", f.source.name)),
                if f.source.external_symbol.is_some() {
                    Linkage::Import
                } else if unit.is_none() {
                    Linkage::Local
                } else if defines(&f.source.name) {
                    Linkage::Export
                } else {
                    Linkage::Import
                },
                &user_sig(&env.module, f),
            )
            .map_err(err)?;
        env.funcs.insert(f.source.name.clone(), (id, f));
    }
    let targets: Vec<_> = analysis
        .workers
        .iter()
        .filter(|n| unit.is_none_or(|u| u.required.contains(*n)))
        .cloned()
        .collect();
    for n in &targets {
        let sig = signature(&env.module, &[I64, I64], None);
        let id = env
            .module
            .declare_function(
                &format!("zn_worker_{n}"),
                if unit.is_none() {
                    Linkage::Local
                } else if defines(n) {
                    Linkage::Export
                } else {
                    Linkage::Import
                },
                &sig,
            )
            .map_err(err)?;
        env.workers.insert(n.clone(), id);
    }
    let channels = referenced_channels(program, unit);
    for id in &channels {
        let callback = env
            .module
            .declare_function(
                &format!("zn_channel_payload_drop_{id}"),
                Linkage::Local,
                &signature(&env.module, &[I64], None),
            )
            .map_err(err)?;
        env.channel_drops.insert(*id, callback);
    }
    for f in program
        .functions
        .iter()
        .filter(|f| f.source.external_symbol.is_none() && defines(&f.source.name))
    {
        let mut ctx = env.module.make_context();
        ctx.func.signature = user_sig(&env.module, f);
        let mut bc = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut bc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let parameters = b.block_params(entry).to_vec();
        let ret_aggregate = f.source.ret != Type::Unit && !scalar(f.source.ret);
        let shift = usize::from(ret_aggregate);
        let out = if ret_aggregate {
            Some(parameters[0])
        } else {
            None
        };
        let mut locals = Vec::new();
        let local_views = crate::immutable_descriptors::local_views(f);
        let address_exposed = crate::immutable_descriptors::address_exposed(f);
        for (i, l) in f.locals.iter().enumerate() {
            if l.ty == Type::Unit {
                locals.push(Storage::Unit);
            } else if l.borrow != Borrow::Owned {
                // Scoped aliases are initialized at their checked loan
                // operation, possibly inside a loop or conditional block.
                let address = if i < f.source.params.len() {
                    parameters[i + shift]
                } else {
                    b.ins().iconst(I64, 0)
                };
                locals.push(Storage::Address(address));
            } else if local_views[i] {
                let data = b.declare_var(I64);
                let len = b.declare_var(I64);
                let zero = b.ins().iconst(I64, 0);
                b.def_var(data, zero);
                b.def_var(len, zero);
                locals.push(Storage::Descriptor { data, len });
            } else if !address_exposed[i]
                && matches!(
                    l.ty,
                    Type::Int | Type::Number(_) | Type::Float(_) | Type::Bool | Type::Id(_)
                )
            {
                let v = b.declare_var(ct(l.ty));
                let init = if i < f.source.params.len() {
                    parameters[i + shift]
                } else {
                    abi_zero(&mut b, l.ty)
                };
                b.def_var(v, init);
                locals.push(Storage::Variable(v));
            } else {
                let layout = env.types.layout(l.ty);
                let ss = b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    layout.size as u32,
                    layout.align.trailing_zeros() as u8,
                ));
                let a = b.ins().stack_addr(I64, ss, 0);
                locals.push(Storage::Address(a));
            }
        }
        let mut w = Writer {
            env: &mut env,
            b,
            locals,
            local_types: f.locals.iter().map(|l| l.ty).collect(),
            integer_ranges: analysis.integer_functions.analyze(f, &[]),
            integer_overrides: BTreeMap::new(),
            guards: Guards::default(),
            guard_serial: 1,
            out,
            hot: analysis.hot.contains(&f.source.name),
            inline_return: None,
            inline_stack: vec![f.source.name.clone()],
            inline_budget: 256,
            array_metadata: BTreeMap::new(),
            pinned: Vec::new(),
            nonnegative: BTreeSet::new(),
            valid_spans: BTreeSet::new(),
            loop_targets: BTreeMap::new(),
            direct_soa_aliases: crate::ir::direct_soa_aliases(f, &program.types),
            column_loops: crate::ir::column_loops(f, &program.types),
            hoisted_columns: BTreeMap::new(),
            hoisted_lengths: BTreeMap::new(),
            trap_blocks: BTreeMap::new(),
            spans: guard_facts::Spans::default(),
            condition_depth: 0,
            versioning: None,
        };
        w.cache_borrowed_metadata(f, &BTreeMap::new());
        for (i, l) in f.locals.iter().enumerate() {
            if l.borrow != Borrow::Owned
                || matches!(
                    w.locals[i],
                    Storage::Variable(_) | Storage::Descriptor { .. } | Storage::Unit
                )
            {
                continue;
            }
            let p = Place::local(i, l.ty);
            let a = w.address(&p);
            if i < f.source.params.len() {
                if scalar(l.ty) {
                    let value = if let Type::Float(n) = l.ty {
                        w.float_bits(parameters[i + shift], n)
                    } else {
                        parameters[i + shift]
                    };
                    w.store(&p, value);
                } else {
                    w.copy_bytes(a, parameters[i + shift], w.env.types.layout(l.ty).size)?;
                }
            }
        }
        w.function_body(f)?;
        w.finish_traps()?;
        seal_inner_first(&mut w.b);
        w.b.finalize(w.env.module.target_config());
        if let Some(text) = env.clif.as_mut() {
            text.push_str(&format!(
                ";; zn_fn_{}\n{}\n",
                f.source.name,
                ctx.func.display()
            ));
        }
        env.define(env.funcs[&f.source.name].0, &mut ctx)?;
    }
    for n in targets.into_iter().filter(|n| defines(n)) {
        let (id, f) = env.funcs[&n];
        let mut ctx = env.module.make_context();
        ctx.func.signature = signature(&env.module, &[I64, I64], None);
        let mut bc = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut bc);
        let e = b.create_block();
        b.append_block_params_for_function_params(e);
        b.switch_to_block(e);
        let packet = b.block_params(e)[0];
        let result_storage = b.block_params(e)[1];
        let mut w = Writer {
            env: &mut env,
            b,
            locals: Vec::new(),
            local_types: Vec::new(),
            integer_ranges: IntRanges::unknown(0),
            integer_overrides: BTreeMap::new(),
            guards: Guards::default(),
            guard_serial: 1,
            out: None,
            hot: false,
            inline_return: None,
            inline_stack: Vec::new(),
            inline_budget: 0,
            array_metadata: BTreeMap::new(),
            pinned: Vec::new(),
            nonnegative: BTreeSet::new(),
            valid_spans: BTreeSet::new(),
            loop_targets: BTreeMap::new(),
            direct_soa_aliases: BTreeSet::new(),
            column_loops: BTreeMap::new(),
            hoisted_columns: BTreeMap::new(),
            hoisted_lengths: BTreeMap::new(),
            trap_blocks: BTreeMap::new(),
            spans: guard_facts::Spans::default(),
            condition_depth: 0,
            versioning: None,
        };
        let execute = w.b.create_block();
        let destroy = w.b.create_block();
        let dropping = w.b.ins().icmp_imm_s(IntCC::Equal, packet, 0);
        w.b.ins().brif(dropping, destroy, &[], execute, &[]);
        w.b.switch_to_block(destroy);
        if f.source.ret.owned() {
            w.drop_at(f.source.ret, result_storage)?;
        }
        w.b.ins().return_(&[]);
        w.b.switch_to_block(execute);
        let mut args = Vec::new();
        if f.source.ret != Type::Unit && !scalar(f.source.ret) {
            args.push(result_storage);
        }
        let mut offset = 0;
        for p in &f.source.params {
            let l = w.env.types.layout(p.ty);
            offset = align(offset, l.align);
            let a = w.b.ins().iadd_imm_s(packet, offset as i64);
            args.push(if scalar(p.ty) {
                let raw = w.load_memory(p.ty, a);
                if let Type::Float(n) = p.ty {
                    w.float_value(raw, n)
                } else {
                    raw
                }
            } else {
                a
            });
            offset += l.size;
        }
        if let Some(result) = w.call_id(id, &args) {
            let result = if let Type::Float(n) = f.source.ret {
                w.float_bits(result, n)
            } else {
                result
            };
            w.store_memory(f.source.ret, result_storage, result);
        }
        w.call("zn_release", &[packet], None)?;
        w.b.ins().return_(&[]);
        w.finish_traps()?;
        w.b.seal_all_blocks();
        w.b.finalize(w.env.module.target_config());
        env.define(env.workers[&n], &mut ctx)?;
    }
    for id in channels {
        let mut ctx = env.module.make_context();
        ctx.func.signature = signature(&env.module, &[I64], None);
        let mut bc = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut bc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let pointer = b.block_params(entry)[0];
        let element = program.types.channel_element(Type::Named(id)).unwrap();
        let mut w = Writer {
            env: &mut env,
            b,
            locals: Vec::new(),
            local_types: Vec::new(),
            out: None,
            hot: false,
            inline_return: None,
            inline_stack: Vec::new(),
            inline_budget: 0,
            array_metadata: BTreeMap::new(),
            pinned: Vec::new(),
            nonnegative: BTreeSet::new(),
            valid_spans: BTreeSet::new(),
            loop_targets: BTreeMap::new(),
            integer_ranges: IntRanges::unknown(0),
            integer_overrides: BTreeMap::new(),
            guards: Guards::default(),
            guard_serial: 1,
            direct_soa_aliases: BTreeSet::new(),
            column_loops: BTreeMap::new(),
            hoisted_columns: BTreeMap::new(),
            hoisted_lengths: BTreeMap::new(),
            trap_blocks: BTreeMap::new(),
            spans: guard_facts::Spans::default(),
            condition_depth: 0,
            versioning: None,
        };
        if element.owned() {
            w.drop_at(element, pointer)?;
        }
        w.zero_bytes(pointer, program.types.layout(element).size)?;
        w.b.ins().return_(&[]);
        w.finish_traps()?;
        w.b.seal_all_blocks();
        w.b.finalize(w.env.module.target_config());
        env.define(env.channel_drops[&id], &mut ctx)?;
    }
    if defines("main") {
        let mut ctx = env.module.make_context();
        ctx.func.signature = signature(&env.module, &[I32, I64], Some(I32));
        let entry_id = env
            .module
            .declare_function("wmain", Linkage::Export, &ctx.func.signature)
            .map_err(err)?;
        let mut bc = FunctionBuilderContext::new();
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut bc);
        let e = b.create_block();
        b.append_block_params_for_function_params(e);
        b.switch_to_block(e);
        let args = b.block_params(e).to_vec();
        let mut w = Writer {
            env: &mut env,
            b,
            locals: Vec::new(),
            local_types: Vec::new(),
            integer_ranges: IntRanges::unknown(0),
            integer_overrides: BTreeMap::new(),
            guards: Guards::default(),
            guard_serial: 1,
            out: None,
            hot: false,
            inline_return: None,
            inline_stack: Vec::new(),
            inline_budget: 0,
            array_metadata: BTreeMap::new(),
            pinned: Vec::new(),
            nonnegative: BTreeSet::new(),
            valid_spans: BTreeSet::new(),
            loop_targets: BTreeMap::new(),
            direct_soa_aliases: BTreeSet::new(),
            column_loops: BTreeMap::new(),
            hoisted_columns: BTreeMap::new(),
            hoisted_lengths: BTreeMap::new(),
            trap_blocks: BTreeMap::new(),
            spans: guard_facts::Spans::default(),
            condition_depth: 0,
            versioning: None,
        };
        w.call("zn_args_init", &args, None)?;
        w.call_id(w.env.funcs["main"].0, &[]);
        let z = w.call("zn_process_exit_code", &[], Some(I32))?.unwrap();
        w.b.ins().return_(&[z]);
        w.finish_traps()?;
        w.b.seal_all_blocks();
        w.b.finalize(w.env.module.target_config());
        env.define(entry_id, &mut ctx)?;
    }
    if let (Some(out), Some(text)) = (clif, env.clif.take()) {
        *out = text;
    }
    let mut product = env.module.finish();
    // cranelift-object does not emit Windows unwind tables itself. Supply the
    // native .pdata/.xdata records so Windows can walk every generated frame.
    let xdata = product.object.add_section(
        Vec::new(),
        b".xdata".to_vec(),
        object::SectionKind::ReadOnlyData,
    );
    let pdata = product.object.add_section(
        Vec::new(),
        b".pdata".to_vec(),
        object::SectionKind::ReadOnlyData,
    );
    let xsym = product.object.section_symbol(xdata);
    for (id, size, bytes) in env.unwind {
        let xoff = product.object.append_section_data(xdata, &bytes, 4);
        let poff = product.object.append_section_data(pdata, &[0; 12], 4);
        let symbol = product.function_symbol(id);
        for (offset, sym, addend) in [
            (0, symbol, 0),
            (4, symbol, i64::from(size)),
            (8, xsym, xoff as i64),
        ] {
            product
                .object
                .add_relocation(
                    pdata,
                    object::write::Relocation {
                        offset: poff + offset,
                        symbol: sym,
                        addend,
                        flags: object::RelocationFlags::Coff {
                            typ: object::pe::IMAGE_REL_AMD64_ADDR32NB,
                        },
                    },
                )
                .map_err(err)?;
        }
    }
    product.emit().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_lookup_and_isa_are_reused_for_parallel_units_and_parameterized_inline_ranges() {
        let mut program = crate::project::Project::from_source(
            "fn bounded(n:int)->int{return n%31;}fn caller(n:int)->int{return bounded(n)+bounded(n);}fn main(){for i in 0..8{print caller(i);}}",
        ).unwrap().checked(false).unwrap().0;
        for f in &mut program.functions {
            f.source.module = f.source.name.clone();
        }
        let before = ISA_BUILDS.with(std::cell::Cell::get);
        let lookups_before = FunctionRanges::lookup_builds();
        let analysis = analyze(&program);
        assert_eq!(ISA_BUILDS.with(std::cell::Cell::get), before + 1);
        assert_eq!(FunctionRanges::lookup_builds(), lookups_before + 1);
        let units = crate::native_units::plan(&program, &analysis.hot);
        let serial: Vec<_> = units
            .iter()
            .map(|unit| object_unit(&program, &analysis, unit).unwrap())
            .collect();
        let f = program
            .functions
            .iter()
            .find(|f| f.source.name.ends_with("caller"))
            .unwrap();
        let bounds = [Bounds { low: 0, high: 7 }];
        assert_eq!(
            analysis
                .integer_functions
                .analyze(f, &bounds)
                .result(&BTreeMap::new()),
            // Remainder analysis deliberately uses [0, divisor-1], as in0.26.
            Bounds { low: 0, high: 60 }
        );
        let negative_bounds = [Bounds { low: -7, high: 0 }];
        assert_eq!(
            analysis
                .integer_functions
                .analyze(f, &negative_bounds)
                .result(&BTreeMap::new()),
            Bounds { low: -60, high: 0 }
        );
        let sizes = analysis.integer_functions.cache_sizes();
        std::thread::scope(|scope| {
            let handles: Vec<_> = units
                .iter()
                .zip(&serial)
                .map(|(unit, expected)| {
                    let analysis = &analysis;
                    let program = &program;
                    scope.spawn(move || {
                        assert_eq!(ISA_BUILDS.with(std::cell::Cell::get), 0);
                        assert_eq!(FunctionRanges::lookup_builds(), 0);
                        assert_eq!(&object_unit(program, analysis, unit).unwrap(), expected);
                        assert_eq!(ISA_BUILDS.with(std::cell::Cell::get), 0);
                        assert_eq!(FunctionRanges::lookup_builds(), 0);
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        for _ in 0..8 {
            analysis.integer_functions.analyze(f, &bounds);
        }
        assert_eq!(analysis.integer_functions.cache_sizes(), sizes);
        assert_eq!(ISA_BUILDS.with(std::cell::Cell::get), before + 1);
        assert_eq!(FunctionRanges::lookup_builds(), lookups_before + 1);
    }
}
