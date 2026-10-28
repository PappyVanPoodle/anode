//! Conservative ranges of successful checked i64 results. Facts cover every
//! write, branch and loop iteration; escaped/borrowed storage stays unknown.
//! This is optimization evidence only, never an ownership acceptance rule.
use crate::{
    ir::*,
    syntax::{Borrow, Type},
};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub low: i64,
    pub high: i64,
}
impl Bounds {
    pub const FULL: Self = Self {
        low: i64::MIN,
        high: i64::MAX,
    };
    pub fn exact(n: i64) -> Self {
        Self { low: n, high: n }
    }
    fn union(self, other: Self) -> Self {
        Self {
            low: self.low.min(other.low),
            high: self.high.max(other.high),
        }
    }
    pub fn intersect(self, other: Self) -> Self {
        let low = self.low.max(other.low);
        let high = self.high.min(other.high);
        if low > high {
            Self::FULL
        } else {
            Self { low, high }
        }
    }
    /// The flag is true only if *all* mathematical results fit signed i64.
    pub fn arithmetic(self, op: &str, other: Self) -> (Self, bool) {
        let (a, b, c, d) = (
            self.low as i128,
            self.high as i128,
            other.low as i128,
            other.high as i128,
        );
        let (low, high) = match op {
            "+" => (a + c, b + d),
            "-" => (a - d, b - c),
            "*" | "*literal" => {
                let p = [a * c, a * d, b * c, b * d];
                (*p.iter().min().unwrap(), *p.iter().max().unwrap())
            }
            _ => return (Self::FULL, false),
        };
        let min = i64::MIN as i128;
        let max = i64::MAX as i128;
        if high < min || low > max {
            return (Self::FULL, false);
        }
        (
            Self {
                low: low.max(min) as i64,
                high: high.min(max) as i64,
            },
            low >= min && high <= max,
        )
    }
    /// One Mersenne fold is sufficient only when the folded high part is
    /// strictly below the modulus. The sum then lies in [0, 2*m), with no i64
    /// overflow; one conditional subtraction gives the exact remainder.
    pub fn mersenne_shift(self, divisor: Self) -> Option<u32> {
        if self.low < 0 || divisor.low != divisor.high || divisor.low < 3 {
            return None;
        }
        let m = divisor.low as u64;
        if !(m + 1).is_power_of_two() {
            return None;
        }
        let shift = (m + 1).trailing_zeros();
        let high = (self.high as u64) >> shift;
        (high < m && m as i128 + high as i128 <= i64::MAX as i128).then_some(shift)
    }
    /// Exact power-of-two divisor `2^k`, if this interval is one constant.
    pub fn power_of_two(self) -> Option<u32> {
        (self.low == self.high && self.low > 0 && self.low & (self.low - 1) == 0)
            .then(|| self.low.trailing_zeros())
    }
    /// An enclosure of every successful checked i64 `self op other` result.
    pub fn binary(self, op: &str, other: Self) -> Self {
        if ["+", "-", "*", "*literal"].contains(&op) {
            return self.arithmetic(op, other).0;
        }
        if other.low == other.high && other.low != 0 {
            let divisor = other.low as i128;
            if op == "%" {
                let m = (divisor.abs() - 1) as i64;
                return Self {
                    low: if self.low >= 0 { 0 } else { -m },
                    high: if self.high <= 0 { 0 } else { m },
                };
            }
            if op == "/" {
                let a = self.low as i128 / divisor;
                let b = self.high as i128 / divisor;
                let low = a.min(b);
                let high = a.max(b);
                return Self {
                    low: low.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                    high: high.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                };
            }
        }
        Self::FULL
    }
}

/// Canonical i64 values a scalar integer type can hold. `u64` (whose raw
/// values exceed i64) and non-integers are unconstrained here.
pub(crate) fn type_bounds(ty: Type) -> Bounds {
    match ty.integer() {
        Some(n) if n.bits() < 64 => {
            if n.signed() {
                let half = 1i64 << (n.bits() - 1);
                Bounds {
                    low: -half,
                    high: half - 1,
                }
            } else {
                Bounds {
                    low: 0,
                    high: (1i64 << n.bits()) - 1,
                }
            }
        }
        _ => Bounds::FULL,
    }
}

/// Successful integer conversion results: the target `int` holds the source
/// value unchanged, and a `u64` source is nonnegative after its range check.
fn conversion_bounds(source: Type, value: Bounds) -> Bounds {
    match source {
        Type::Int => value,
        Type::Number(crate::numeric::Integer::U64) => Bounds {
            low: 0,
            high: i64::MAX,
        },
        Type::Number(_) => type_bounds(source),
        _ => Bounds::FULL,
    }
}

/// The scalar local written by a range-tracked definition.
fn definition(op: &Operation) -> Option<&Place> {
    match op {
        Operation::Init(p, _) | Operation::Assign(p, _) if p.fields.is_empty() => Some(p),
        _ => None,
    }
}

/// The enclosure of one definition's successful results, or `None` while an
/// operand has no bound yet.
fn transfer(
    op: &Operation,
    read: &dyn Fn(&Place) -> Option<Bounds>,
    call: &mut dyn FnMut(&str, &[Bounds]) -> Bounds,
) -> Option<Bounds> {
    let value = match op {
        Operation::Assign(_, source) => return read(source),
        Operation::Init(_, value) => value,
        _ => return None,
    };
    match value {
        Rvalue::Int(n) => Some(Bounds::exact(*n)),
        Rvalue::Copy(p) => read(p),
        Rvalue::Binary(op, a, b) => match (read(a), read(b)) {
            (Some(a), Some(b)) => Some(a.binary(op, b)),
            (_, Some(b)) if op == "%" => Some(Bounds::FULL.binary(op, b)),
            _ => None,
        },
        Rvalue::Convert(p) => read(p).map(|value| conversion_bounds(p.ty, value)),
        Rvalue::Unary(op, p) if op == "-" => read(p).map(|b| Bounds::exact(0).arithmetic("-", b).0),
        Rvalue::Builtin { name, args } => Some(match name.as_str() {
            "byte_at" => Bounds { low: 0, high: 255 },
            "len" | "map_len" | "arena_len" | "fixed_len" | "vec_capacity" => Bounds {
                low: 0,
                high: i64::MAX,
            },
            // AND with a nonnegative mask cannot set the sign bit, even when
            // the other operand is negative. Keep the symbolic owner/length
            // relationship in guard_facts.
            "bit_and" => args
                .iter()
                .filter_map(read)
                .filter(|b| b.low >= 0)
                .map(|b| Bounds {
                    low: 0,
                    high: b.high,
                })
                .min_by_key(|b| b.high)
                .unwrap_or(Bounds::FULL),
            _ => Bounds::FULL,
        }),
        Rvalue::Call {
            name,
            args,
            spawn: false,
        } => {
            let parameters = args
                .iter()
                .map(|arg| match arg {
                    Argument::Value(p) => read(p),
                    Argument::Reference(_) => Some(Bounds::FULL),
                })
                .collect::<Option<Vec<_>>>();
            parameters.map(|parameters| call(name, &parameters))
        }
        _ => Some(Bounds::FULL),
    }
}

fn walk(ops: &[Operation], visit: &mut impl FnMut(&Operation)) {
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

#[derive(Clone)]
pub(crate) struct IntRanges {
    ranges: Vec<Option<Bounds>>,
    roots: Vec<usize>,
    immutable: Vec<bool>,
    returns: Vec<Place>,
    /// Proven upper bounds of owned, never-escaped unsigned integer locals
    /// (their values are always in `[0, bound]`), see [`unsigned_bounds`].
    unsigned: Vec<Option<u64>>,
}

/// Largest value of an unsigned integer type, `None` for any other type.
fn unsigned_max(ty: Type) -> Option<u64> {
    ty.integer()
        .filter(|n| !n.signed())
        .map(|n| u64::MAX >> (64 - n.bits()))
}

/// Contract M6 (1.1.0 A): flow-insensitive upper bounds of unsigned locals.
/// A local's value is always zero (its initial value) or the result of one of
/// its definitions, so the maximum over its definitions bounds every read:
/// - `bit_and(a, b)` of an unsigned width lies in `[0, min(upper(a),
///   upper(b))]`, because a set result bit is set in both operands; with a
///   compile-time constant mask `K` that is `[0, K]`;
/// - a successful conversion of an `int` with a proven nonnegative interval
///   returns that value unchanged (so `u64(K)` is exactly `K`);
/// - parameters, moves and every other definition keep the type's maximum.
///   No other bit operation gains a fact. An escaped local (swap, loan,
///   reference argument, channel output) or one that fails to converge has no
///   bound.
fn unsigned_bounds(
    f: &CheckedFunction,
    escaped: &[bool],
    ranges: &[Option<Bounds>],
    int_eligible: &[bool],
) -> Vec<Option<u64>> {
    let n = f.locals.len();
    let eligible: Vec<_> = f
        .locals
        .iter()
        .enumerate()
        .map(|(i, l)| unsigned_max(l.ty).is_some() && l.borrow == Borrow::Owned && !escaped[i])
        .collect();
    let mut upper: Vec<u64> = (0..n)
        .map(|i| {
            if eligible[i] && i < f.source.params.len() {
                unsigned_max(f.locals[i].ty).unwrap()
            } else {
                0
            }
        })
        .collect();
    let mut opaque = vec![false; n];
    walk(&f.body, &mut |op| {
        if let Operation::Move { destination, .. } = op {
            opaque[destination.local] = true;
        }
    });
    for i in 0..n {
        if eligible[i] && opaque[i] {
            upper[i] = unsigned_max(f.locals[i].ty).unwrap();
        }
    }
    for _ in 0..32 {
        let mut changed = false;
        walk(&f.body, &mut |op| {
            let (destination, value) = match op {
                Operation::Init(p, value) if p.fields.is_empty() => (p, Some(value)),
                Operation::Assign(p, _) if p.fields.is_empty() => (p, None),
                _ => return,
            };
            if !eligible[destination.local] {
                return;
            }
            let max = unsigned_max(destination.ty).unwrap();
            let read = |p: &Place| -> u64 {
                if p.fields.is_empty() && eligible[p.local] && p.ty == destination.ty {
                    upper[p.local]
                } else {
                    unsigned_max(p.ty).unwrap_or(u64::MAX)
                }
            };
            let bound = match value {
                Some(Rvalue::Builtin { name, args }) if name == "bit_and" => {
                    args.iter().map(read).min().unwrap_or(max)
                }
                Some(Rvalue::Convert(q)) if q.ty == Type::Int => {
                    match (q.fields.is_empty() && int_eligible[q.local])
                        .then(|| ranges[q.local])
                        .flatten()
                    {
                        Some(b) if b.low >= 0 => (b.high as u64).min(max),
                        _ => max,
                    }
                }
                Some(Rvalue::Copy(q)) => read(q),
                None => match op {
                    Operation::Assign(_, q) => read(q),
                    _ => max,
                },
                _ => max,
            }
            .min(max);
            if bound > upper[destination.local] {
                upper[destination.local] = bound;
                changed = true;
            }
        });
        if !changed {
            return (0..n).map(|i| eligible[i].then_some(upper[i])).collect();
        }
    }
    vec![None; n]
}
type Parameters = Vec<(i64, i64)>;
type SummaryKey = (String, Parameters, usize, usize);
#[cfg(test)]
thread_local! {
    static LOOKUP_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[derive(Default)]
struct RangeCache {
    functions: BTreeMap<(String, Parameters), IntRanges>,
    summaries: BTreeMap<SummaryKey, (Bounds, usize)>,
}
/// One immutable program's lookup and memoized range proofs. The cache keys
/// include recursion depth and the remaining work budget: query order and
/// parallel scheduling must never change a proof or the amount of analysis.
pub(crate) struct FunctionRanges<'a> {
    functions: BTreeMap<&'a str, &'a CheckedFunction>,
    cache: Mutex<RangeCache>,
}
impl<'a> FunctionRanges<'a> {
    pub fn new(functions: impl Iterator<Item = &'a CheckedFunction>) -> Self {
        #[cfg(test)]
        LOOKUP_BUILDS.with(|count| count.set(count.get() + 1));
        Self {
            functions: functions.map(|f| (f.source.name.as_str(), f)).collect(),
            cache: Mutex::new(RangeCache::default()),
        }
    }
    pub fn analyze(&self, f: &CheckedFunction, parameters: &[Bounds]) -> IntRanges {
        let key = (f.source.name.clone(), parameter_key(parameters));
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ranges) = cache.functions.get(&key) {
            return ranges.clone();
        }
        let ranges = self.analyze_inner(f, parameters, &mut cache.summaries, &mut 64, 2);
        cache.functions.insert(key, ranges.clone());
        ranges
    }
    #[cfg(test)]
    pub(crate) fn lookup_builds() -> usize {
        LOOKUP_BUILDS.with(std::cell::Cell::get)
    }
    #[cfg(test)]
    pub(crate) fn cache_sizes(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        (cache.functions.len(), cache.summaries.len())
    }
    fn analyze_inner(
        &self,
        f: &CheckedFunction,
        parameters: &[Bounds],
        cache: &mut BTreeMap<SummaryKey, (Bounds, usize)>,
        budget: &mut usize,
        depth: usize,
    ) -> IntRanges {
        IntRanges::analyze_with_calls(f, parameters, &mut |name, parameters| {
            if depth == 0 {
                return Bounds::FULL;
            }
            let Some(f) = self.functions.get(name) else {
                return Bounds::FULL;
            };
            if *budget == 0 || f.source.ret != Type::Int {
                return Bounds::FULL;
            }
            let key = (name.to_string(), parameter_key(parameters), depth, *budget);
            if let Some(&(result, remaining)) = cache.get(&key) {
                *budget = remaining;
                return result;
            }
            *budget -= 1;
            let result = self
                .analyze_inner(f, parameters, cache, budget, depth - 1)
                .result(&BTreeMap::new());
            cache.insert(key, (result, *budget));
            result
        })
    }
}
fn parameter_key(parameters: &[Bounds]) -> Parameters {
    parameters.iter().map(|b| (b.low, b.high)).collect()
}
impl IntRanges {
    pub fn unknown(n: usize) -> Self {
        Self {
            ranges: vec![Some(Bounds::FULL); n],
            roots: (0..n).collect(),
            immutable: vec![false; n],
            returns: Vec::new(),
            unsigned: vec![None; n],
        }
    }
    #[cfg(test)]
    pub fn analyze(f: &CheckedFunction, parameters: &[Bounds]) -> Self {
        Self::analyze_with_calls(f, parameters, &mut |_, _| Bounds::FULL)
    }
    #[cfg(test)]
    pub fn with_functions<'a>(
        f: &CheckedFunction,
        parameters: &[Bounds],
        functions: impl Iterator<Item = &'a CheckedFunction>,
    ) -> Self {
        FunctionRanges::new(functions).analyze(f, parameters)
    }
    fn analyze_with_calls(
        f: &CheckedFunction,
        parameters: &[Bounds],
        call: &mut impl FnMut(&str, &[Bounds]) -> Bounds,
    ) -> Self {
        let n = f.locals.len();
        let mut writes = vec![0usize; n];
        let mut escaped = vec![false; n];
        let mut copies = vec![None; n];
        let mut returns = Vec::new();
        for count in writes.iter_mut().take(f.source.params.len()) {
            *count = 1;
        }
        walk(&f.body, &mut |op| {
            let destination = match op {
                Operation::Init(p, _) | Operation::Assign(p, _) => Some(p),
                Operation::Move { destination, .. } => Some(destination),
                _ => None,
            };
            if let Some(p) = destination {
                if p.fields.is_empty() {
                    writes[p.local] += 1;
                }
            }
            match op {
                Operation::Swap(a, b) => {
                    escaped[a.local] = true;
                    escaped[b.local] = true;
                }
                Operation::Init(p, Rvalue::Copy(source))
                    if p.fields.is_empty() && source.fields.is_empty() =>
                {
                    copies[p.local] = Some(source.local)
                }
                Operation::Init(_, Rvalue::Call { args, .. }) => {
                    for arg in args {
                        if let Argument::Reference(p) = arg {
                            escaped[p.local] = true;
                        }
                    }
                }
                Operation::Init(_, Rvalue::Builtin { name, args })
                    if matches!(name.as_str(), "channel_recv" | "channel_try_recv") =>
                {
                    if let Some(output) = args.get(1) {
                        escaped[output.local] = true;
                    }
                }
                Operation::Loan { place, .. } => escaped[place.local] = true,
                Operation::Return(Some(p)) if p.ty == Type::Int => returns.push(p.clone()),
                _ => {}
            }
        });
        let eligible: Vec<_> = f
            .locals
            .iter()
            .enumerate()
            .map(|(i, l)| l.ty == Type::Int && l.borrow == Borrow::Owned && !escaped[i])
            .collect();
        let immutable: Vec<_> = eligible
            .iter()
            .enumerate()
            .map(|(i, &ok)| ok && writes[i] == 1)
            .collect();
        let roots = (0..n)
            .map(|i| {
                if !immutable[i] {
                    return i;
                }
                let mut root = i;
                for _ in 0..n {
                    match copies[root] {
                        Some(source) if immutable[source] && source != root => root = source,
                        _ => break,
                    }
                }
                root
            })
            .collect();
        let mut ranges = vec![None; n];
        for i in 0..f.source.params.len() {
            ranges[i] = Some(if eligible[i] {
                parameters.get(i).copied().unwrap_or(Bounds::FULL)
            } else {
                Bounds::FULL
            });
        }
        for i in 0..n {
            if !eligible[i] {
                ranges[i] = Some(Bounds::FULL);
            }
        }
        let mut updates = vec![0u8; n];
        let mut converged = false;
        // Widen expanding cycles; if a larger graph still fails to converge,
        // discard its evidence rather than using a provisional range.
        for _ in 0..32 {
            let mut changed = false;
            walk(&f.body, &mut |op| {
                let Some(dest) = definition(op).filter(|p| eligible[p.local]) else {
                    return;
                };
                let read = |p: &Place| {
                    if p.fields.is_empty() && eligible[p.local] {
                        ranges[p.local]
                    } else {
                        Some(Bounds::FULL)
                    }
                };
                let Some(mut next) = transfer(op, &read, call) else {
                    return;
                };
                if let Some(old) = ranges[dest.local] {
                    next = old.union(next);
                    if next != old && updates[dest.local] >= 4 {
                        next = Bounds {
                            low: if next.low < old.low {
                                i64::MIN
                            } else {
                                old.low
                            },
                            high: if next.high > old.high {
                                i64::MAX
                            } else {
                                old.high
                            },
                        };
                    }
                }
                if ranges[dest.local] != Some(next) {
                    ranges[dest.local] = Some(next);
                    updates[dest.local] = updates[dest.local].saturating_add(1);
                    changed = true;
                }
            });
            if !changed {
                converged = true;
                break;
            }
        }
        if !converged {
            ranges.fill(Some(Bounds::FULL));
        } else {
            // Widening may jump a bounded definition (a mask, a constant
            // remainder, a narrow conversion) straight to i64::MAX. Two
            // descending passes re-evaluate every definition from the sound
            // fixpoint and intersect: each local's values lie in the union of
            // its definitions' results, so every pass keeps soundness. Calls
            // are not re-analyzed; their definitions stay unconstrained here.
            for _ in 0..2 {
                let mut defined: Vec<Option<Bounds>> = (0..n)
                    .map(|i| {
                        (i < f.source.params.len())
                            .then(|| parameters.get(i).copied().unwrap_or(Bounds::FULL))
                    })
                    .collect();
                let mut opaque = vec![false; n];
                walk(&f.body, &mut |op| {
                    if let Operation::Move { destination, .. } = op {
                        opaque[destination.local] = true;
                    }
                    let Some(dest) = definition(op).filter(|p| eligible[p.local]) else {
                        return;
                    };
                    let read = |p: &Place| {
                        if p.fields.is_empty() && eligible[p.local] {
                            ranges[p.local]
                        } else {
                            Some(Bounds::FULL)
                        }
                    };
                    match transfer(op, &read, &mut |_, _| Bounds::FULL) {
                        Some(value) => {
                            defined[dest.local] =
                                Some(defined[dest.local].map_or(value, |d| d.union(value)));
                        }
                        None => opaque[dest.local] = true,
                    }
                });
                for i in 0..n {
                    if let (true, false, Some(range), Some(value)) =
                        (eligible[i], opaque[i], ranges[i], defined[i])
                    {
                        ranges[i] = Some(range.intersect(value));
                    }
                }
            }
        }
        let unsigned = unsigned_bounds(f, &escaped, &ranges, &eligible);
        Self {
            ranges,
            roots,
            immutable,
            returns,
            unsigned,
        }
    }
    /// Contract M6: a proven upper bound of an unsigned integer local (every
    /// value it holds lies in `[0, bound]`), or `None` when nothing is known.
    pub fn unsigned_high(&self, p: &Place) -> Option<u64> {
        if !p.fields.is_empty() || unsigned_max(p.ty).is_none() {
            return None;
        }
        self.unsigned.get(p.local).copied().flatten()
    }
    pub fn get(&self, p: &Place, overrides: &BTreeMap<usize, Bounds>) -> Bounds {
        if p.ty != Type::Int || !p.fields.is_empty() {
            return Bounds::FULL;
        }
        let bound = self
            .ranges
            .get(p.local)
            .copied()
            .flatten()
            .unwrap_or(Bounds::FULL);
        if self.immutable.get(p.local) == Some(&true) {
            if let Some(n) = overrides.get(&self.roots[p.local]) {
                return bound.intersect(*n);
            }
        }
        bound
    }
    pub fn refine(&self, p: &Place, bound: Bounds, overrides: &mut BTreeMap<usize, Bounds>) {
        // Only the result of one immutable definition may be specialized.
        // Mutable locals and address-exposed storage retain global invariants.
        if p.ty == Type::Int
            && p.fields.is_empty()
            && self.immutable.get(p.local) == Some(&true)
            && self.roots[p.local] == p.local
        {
            overrides.insert(p.local, bound);
        }
    }
    /// An owned, never-escaped `int` local with exactly one write: once that
    /// definition executes, its value can no longer change in this frame.
    pub fn immutable(&self, local: usize) -> bool {
        self.immutable.get(local) == Some(&true)
    }
    pub fn result(&self, overrides: &BTreeMap<usize, Bounds>) -> Bounds {
        self.returns
            .iter()
            .map(|p| self.get(p, overrides))
            .reduce(Bounds::union)
            .unwrap_or(Bounds::FULL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_fold_remainder_matches_signed_modulo_with_proven_bound() {
        let mut state = 75321u64;
        for shift in 2..=63 {
            let m = ((1u64 << shift) - 1) as i64;
            let max = ((m as i128) * (1i128 << shift) - 1).min(i64::MAX as i128) as i64;
            let range = Bounds { low: 0, high: max };
            assert_eq!(range.mersenne_shift(Bounds::exact(m)), Some(shift));
            let mut values = vec![0, 1, m - 1, m, max];
            for _ in 0..1000 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                values.push((state % (max as u64 + 1)) as i64);
            }
            for value in values {
                let folded = (value & m) + (value >> shift);
                let reduced = if folded >= m { folded - m } else { folded };
                assert_eq!(reduced, value % m, "{value} % {m}");
            }
            assert_eq!(
                Bounds { low: -1, high: max }.mersenne_shift(Bounds::exact(m)),
                None
            );
            if max < i64::MAX {
                assert_eq!(
                    Bounds {
                        low: 0,
                        high: max + 1
                    }
                    .mersenne_shift(Bounds::exact(m)),
                    None
                );
            }
        }
        assert_eq!(
            Bounds { low: 0, high: 255 }.mersenne_shift(Bounds::exact(10)),
            None
        );
    }
    #[test]
    fn interval_results_enclose_checked_integer_arithmetic() {
        let intervals = [
            Bounds {
                low: i64::MIN,
                high: i64::MIN + 3,
            },
            Bounds {
                low: -131,
                high: -1,
            },
            Bounds { low: -3, high: 4 },
            Bounds { low: 0, high: 255 },
            Bounds {
                low: i64::MAX - 3,
                high: i64::MAX,
            },
            Bounds::FULL,
        ];
        for a in intervals {
            for b in intervals {
                for op in ["+", "-", "*", "/", "%"] {
                    let result = a.binary(op, b);
                    let safe = a.arithmetic(op, b).1;
                    for x in [a.low, ((a.low as i128 + a.high as i128) / 2) as i64, a.high] {
                        for y in [b.low, ((b.low as i128 + b.high as i128) / 2) as i64, b.high] {
                            let value = match op {
                                "+" => x.checked_add(y),
                                "-" => x.checked_sub(y),
                                "*" => x.checked_mul(y),
                                "/" => x.checked_div(y),
                                "%" => x.checked_rem(y),
                                _ => unreachable!(),
                            };
                            if let Some(value) = value {
                                assert!(
                                    result.low <= value && value <= result.high,
                                    "{a:?} {op} {b:?}: {value} outside {result:?}"
                                );
                            } else {
                                assert!(!safe);
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn bounded_helpers_preserve_nonnegative_hash_invariant() {
        let p=crate::project::Project::from_source(r#"
fn lower(c:int)->int{if c>=65 && c<=90{return c+32;}return c;}
fn hash(s:&text)->int{let mut h=1;let mut i=0;while i<len(s){h=(h*131+lower(byte_at(s,i)))%2147483647;i=i+1;}return h;}
fn main(){}
"#).unwrap().checked(false).unwrap().0;
        let f = p
            .functions
            .iter()
            .find(|f| f.source.name.ends_with("hash"))
            .unwrap();
        let r = IntRanges::with_functions(f, &[], p.functions.iter()).result(&BTreeMap::new());
        assert_eq!(
            r,
            Bounds {
                low: 0,
                high: 2147483646
            }
        );
    }
    #[test]
    fn unconverged_graph_discards_all_provisional_bounds() {
        let mut source = String::from("fn chain()->int{");
        for i in 0..72 {
            source.push_str(&format!("let mut a{i}=0;"));
        }
        source.push_str("while false{");
        for i in 0..71 {
            source.push_str(&format!("a{i}=a{};", i + 1));
        }
        source.push_str("a71=1;}return a0;}fn main(){}");
        let p = crate::project::Project::from_source(&source)
            .unwrap()
            .checked(false)
            .unwrap()
            .0;
        let f = p
            .functions
            .iter()
            .find(|f| f.source.name.ends_with("chain"))
            .unwrap();
        let r = IntRanges::analyze(f, &[]);
        assert!(r.ranges.iter().all(|b| *b == Some(Bounds::FULL)));
    }
    fn function_ranges(source: &str, name: &str) -> (IntRanges, CheckedFunction) {
        let p = crate::project::Project::from_source(source)
            .unwrap()
            .checked(false)
            .unwrap()
            .0;
        let f = p
            .functions
            .iter()
            .find(|f| f.source.name.ends_with(name))
            .unwrap()
            .clone();
        (IntRanges::with_functions(&f, &[], p.functions.iter()), f)
    }
    #[test]
    fn masks_remainders_and_conversions_survive_loop_widening() {
        let (r, f) = function_ranges(
            "fn fill(n:int)->int{let mut s=0;for i in 0..n{let m=bit_and(i*37+11,255);let q=(i*3)%64;let w=int(u8(m));s=wrapping_add(s,m+q+w);}return s;}fn main(){}",
            "fill",
        );
        let local = |init: &dyn Fn(&Rvalue) -> bool| {
            let mut found = None;
            walk(&f.body, &mut |op| {
                if let Operation::Init(p, value) = op {
                    if init(value) {
                        found = Some(p.clone());
                    }
                }
            });
            r.get(&found.unwrap(), &BTreeMap::new())
        };
        assert_eq!(
            local(&|v| matches!(v, Rvalue::Builtin { name, .. } if name == "bit_and")),
            Bounds { low: 0, high: 255 }
        );
        // The dividend (i*3) is proven nonnegative, so the remainder is too.
        assert_eq!(
            local(&|v| matches!(v, Rvalue::Binary(op, ..) if op == "%")),
            Bounds { low: 0, high: 63 }
        );
        assert_eq!(
            local(&|v| matches!(v, Rvalue::Convert(p) if p.ty != Type::Int)),
            Bounds { low: 0, high: 255 }
        );
    }
    #[test]
    fn unsigned_masks_bound_only_bit_and_with_bounded_operands() {
        let (r, f) = function_ranges(
            "fn f(x:u64,y:u8,s:u64)->int{let m=bit_and(x,u64(255));let n=bit_and(u64(1000),x);let w=wrapping_add(x,u64(1));let z=bit_and(w,x);let q=bit_and(y,u8(15));let o=bit_or(x,u64(3));let a0=bit_and(x,u64(7));let mut a=a0;if y>u8(3){a=s;}let mut l=u64(0);for i in 0..3 {let t=bit_and(l,x);l=t;l=u64(i);}return int(m)+int(n)+int(z)+int(q)+int(o)+int(a)+int(l);}fn main(){}",
            "_f",
        );
        let local = |pick: &dyn Fn(&Rvalue) -> bool| {
            let mut found = None;
            walk(&f.body, &mut |op| {
                if let Operation::Init(p, value) = op {
                    if pick(value) && found.is_none() {
                        found = Some(p.clone());
                    }
                }
            });
            r.unsigned_high(&found.unwrap())
        };
        let mut masks = Vec::new();
        walk(&f.body, &mut |op| {
            if let Operation::Init(p, Rvalue::Builtin { name, .. }) = op {
                if name == "bit_and" {
                    masks.push(r.unsigned_high(p));
                }
            }
        });
        // m, n, z, q, a0 and the loop's mask of `l` (in source order): `l`
        // holds 0, the mask itself or u64(i), and the flow-insensitive
        // range of the loop counter `i` is [0, i64::MAX].
        assert_eq!(
            masks,
            [
                Some(255),
                Some(1000),
                Some(u64::MAX),
                Some(15),
                Some(7),
                Some(i64::MAX as u64)
            ]
        );
        // A local also assigned from a parameter is unbounded.
        let mut assigned = None;
        walk(&f.body, &mut |op| {
            if let Operation::Assign(p, source) | Operation::Init(p, Rvalue::Copy(source)) = op {
                if source.local == 2 && source.fields.is_empty() {
                    assigned = Some(p.clone());
                }
            }
        });
        assert_eq!(r.unsigned_high(&assigned.unwrap()), Some(u64::MAX));
        // `bit_or`, `wrapping_add` and parameters keep the type maximum.
        assert_eq!(
            local(&|v| matches!(v, Rvalue::Builtin { name, .. } if name == "bit_or")),
            Some(u64::MAX)
        );
        assert_eq!(
            local(&|v| matches!(v, Rvalue::Builtin { name, .. } if name == "wrapping_add")),
            Some(u64::MAX)
        );
        assert_eq!(
            r.unsigned_high(&Place::local(0, Type::Number(crate::numeric::Integer::U64))),
            Some(u64::MAX)
        );
        assert_eq!(
            r.unsigned_high(&Place::local(1, Type::Number(crate::numeric::Integer::U8))),
            Some(255)
        );
        // An int place has no unsigned bound.
        assert_eq!(r.unsigned_high(&Place::local(2, Type::Int)), None);
    }
    #[test]
    fn narrowing_never_drops_a_reachable_definition() {
        // `x` is redefined with an unbounded value; the narrowing pass must
        // keep the union of all definitions, including parameters.
        let (r, f) = function_ranges(
            "fn f(a:int,b:int)->int{let mut x=bit_and(a,15);if b>0{x=b;}let y=x;return y;}fn main(){}",
            "_f",
        );
        let mut y = None;
        walk(&f.body, &mut |op| {
            if let Operation::Init(p, Rvalue::Copy(_)) = op {
                y = Some(p.clone());
            }
        });
        let bound = r.get(&y.unwrap(), &BTreeMap::new());
        assert_eq!(bound.high, i64::MAX, "{bound:?}");
    }
    #[test]
    fn conversion_and_power_of_two_helpers() {
        use crate::numeric::Integer;
        assert_eq!(
            type_bounds(Type::Number(Integer::I8)),
            Bounds {
                low: -128,
                high: 127
            }
        );
        assert_eq!(
            type_bounds(Type::Number(Integer::U32)),
            Bounds {
                low: 0,
                high: u32::MAX as i64
            }
        );
        assert_eq!(type_bounds(Type::Number(Integer::U64)), Bounds::FULL);
        assert_eq!(type_bounds(Type::Int), Bounds::FULL);
        assert_eq!(
            conversion_bounds(Type::Number(Integer::U64), Bounds::FULL),
            Bounds {
                low: 0,
                high: i64::MAX
            }
        );
        for (value, shift) in [
            (1, Some(0)),
            (2, Some(1)),
            (256, Some(8)),
            (1 << 62, Some(62)),
        ] {
            assert_eq!(Bounds::exact(value).power_of_two(), shift);
        }
        for value in [0, -1, -256, 3, 6, i64::MIN, i64::MAX] {
            assert_eq!(Bounds::exact(value).power_of_two(), None);
        }
        assert_eq!(Bounds { low: 2, high: 4 }.power_of_two(), None);
        // Power-of-two rewrites are exact for every nonnegative dividend.
        for k in 0..63 {
            let d = 1i64 << k;
            for a in [0, 1, d - 1, d, d + 1, i64::MAX, i64::MAX - d] {
                assert_eq!(a / d, ((a as u64) >> k) as i64);
                assert_eq!(a % d, a & (d - 1));
            }
        }
    }
}
