//! Facts established by executed guards. Value identities are local versions,
//! never guesses about mutable memory. Loop entry/exit invalidate snapshots.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fact {
    Within(u64, i64, i64),
    BelowLength(u64, usize),
    NonnegativeLength(usize),
}

#[derive(Clone)]
pub(crate) struct Boolean {
    pub id: u64,
    pub inverted: bool,
    pub constant: Option<bool>,
    pub yes: BTreeSet<Fact>,
    pub no: BTreeSet<Fact>,
}
impl Boolean {
    pub fn unknown(id: u64) -> Self {
        Self {
            id,
            inverted: false,
            constant: None,
            yes: BTreeSet::new(),
            no: BTreeSet::new(),
        }
    }
    pub fn negate(mut self) -> Self {
        self.inverted = !self.inverted;
        self.constant = self.constant.map(|b| !b);
        std::mem::swap(&mut self.yes, &mut self.no);
        self
    }
    pub fn merge(id: u64, condition: &Self, yes: &Self, no: &Self) -> Self {
        fn path(
            gate: bool,
            want: bool,
            condition: &Boolean,
            value: &Boolean,
        ) -> Option<BTreeSet<Fact>> {
            if value.constant.is_some_and(|b| b != want) {
                return None;
            }
            if value.id == condition.id && (gate ^ (value.inverted != condition.inverted)) != want {
                return None;
            }
            let a = if gate { &condition.yes } else { &condition.no };
            let b = if want { &value.yes } else { &value.no };
            Some(a.union(b).cloned().collect())
        }
        fn combine(a: Option<BTreeSet<Fact>>, b: Option<BTreeSet<Fact>>) -> BTreeSet<Fact> {
            match (a, b) {
                (Some(a), Some(b)) => a.intersection(&b).take(24).cloned().collect(),
                (Some(a), None) | (None, Some(a)) => a.into_iter().take(24).collect(),
                _ => BTreeSet::new(),
            }
        }
        if yes.id == no.id && yes.inverted == no.inverted {
            return yes.clone();
        }
        let mut result = Self::unknown(id);
        result.yes = combine(
            path(true, true, condition, yes),
            path(false, true, condition, no),
        );
        result.no = combine(
            path(true, false, condition, yes),
            path(false, false, condition, no),
        );
        result
    }
}

#[derive(Clone, Default)]
pub(crate) struct State {
    pub integers: BTreeMap<usize, u64>,
    pub lengths: BTreeMap<usize, usize>,
    pub booleans: BTreeMap<usize, Boolean>,
    pub active: BTreeSet<Fact>,
}
pub(crate) fn fresh(serial: &mut u64) -> u64 {
    let id = *serial;
    *serial = serial.checked_add(1).expect("guard identity limit");
    id
}
impl State {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    /// Per-iteration integer versions, the length snapshots they hold and
    /// executed predicates do not survive a back edge, except for a local
    /// that can never be rewritten (`keep`): it holds the same value on every
    /// iteration, so its facts remain true. (A rewritten local's length
    /// association is stale on a `continue`/unrotated back edge to the head.)
    pub fn enter_loop_preserving(&mut self, keep: impl Fn(usize) -> bool) {
        self.integers.retain(|&local, _| keep(local));
        self.lengths.retain(|&local, _| keep(local));
        let kept: BTreeSet<u64> = self.integers.values().copied().collect();
        self.booleans.clear();
        self.active.retain(|fact| match *fact {
            Fact::Within(id, ..) | Fact::BelowLength(id, _) => kept.contains(&id),
            // Established by a dominating check of one fixed SSA length.
            Fact::NonnegativeLength(_) => true,
        });
    }
    /// Facts about fixed SSA values (`NonnegativeLength`): they stay true
    /// everywhere their dominating check dominates, whatever later code
    /// rewrites in memory or in scalar locals.
    pub fn value_facts(&self) -> BTreeSet<Fact> {
        self.active
            .iter()
            .filter(|fact| matches!(fact, Fact::NonnegativeLength(_)))
            .cloned()
            .collect()
    }
    /// Forget local versions and predicates after an operation that may
    /// rewrite scalar locals (exclusive-reference call, swap, write-back),
    /// keeping only the SSA value facts, which no such operation can change.
    pub fn clear_preserving_values(&mut self) {
        let kept = self.value_facts();
        self.clear();
        self.active = kept;
    }
    /// The interval every `Within` fact of version `id` implies, if any.
    fn within(&self, id: u64) -> Option<(i64, i64)> {
        self.active.iter().fold(None, |range, fact| match *fact {
            Fact::Within(origin, low, high) if origin == id => Some(match range {
                Some((l, h)) => (low.max(l), high.min(h)),
                None => (low, high),
            }),
            _ => range,
        })
    }
    /// Facts at a join of two predecessor states that may hold *different*
    /// versions of the same local (a rotated loop's two exit tests). Shared
    /// facts are kept; a local with two versions gets a fresh version with
    /// the union of its two intervals, and a `BelowLength` both versions
    /// share. Each input must describe its own edge into the join.
    pub fn join_by_local(a: &Self, b: &Self, serial: &mut u64) -> Self {
        let mut result = Self {
            active: a.active.intersection(&b.active).cloned().collect(),
            ..Self::default()
        };
        for (&local, &x) in &a.integers {
            let Some(&y) = b.integers.get(&local) else {
                continue;
            };
            if x == y {
                result.integers.insert(local, x);
                continue;
            }
            let id = fresh(serial);
            result.integers.insert(local, id);
            if let (Some((l1, h1)), Some((l2, h2))) = (a.within(x), b.within(y)) {
                if l1 <= h1 && l2 <= h2 {
                    result
                        .active
                        .insert(Fact::Within(id, l1.min(l2), h1.max(h2)));
                }
            }
            for fact in &a.active {
                if let Fact::BelowLength(origin, owner) = *fact {
                    if origin == x && b.active.contains(&Fact::BelowLength(y, owner)) {
                        result.active.insert(Fact::BelowLength(id, owner));
                    }
                }
            }
        }
        for (&local, &owner) in &a.lengths {
            if b.lengths.get(&local) == Some(&owner) {
                result.lengths.insert(local, owner);
            }
        }
        result
    }
    pub fn boolean(&self, local: usize, serial: &mut u64) -> Boolean {
        self.booleans
            .get(&local)
            .cloned()
            .unwrap_or_else(|| Boolean::unknown(fresh(serial)))
    }
    pub fn merge(condition: &Boolean, yes: &Self, no: &Self, serial: &mut u64) -> Self {
        let mut result = Self {
            active: yes.active.intersection(&no.active).cloned().collect(),
            ..Self::default()
        };
        for &local in yes.integers.keys().chain(no.integers.keys()) {
            let origin = match (yes.integers.get(&local), no.integers.get(&local)) {
                (Some(a), Some(b)) if a == b => *a,
                _ => fresh(serial),
            };
            result.integers.insert(local, origin);
        }
        for (&local, &owner) in &yes.lengths {
            if no.lengths.get(&local) == Some(&owner) {
                result.lengths.insert(local, owner);
            }
        }
        for &local in yes.booleans.keys().chain(no.booleans.keys()) {
            let y = yes.boolean(local, serial);
            let n = no.boolean(local, serial);
            result
                .booleans
                .insert(local, Boolean::merge(fresh(serial), condition, &y, &n));
        }
        result
    }
}

/// Hoisted span (`VectorView`) validity limits, `valid ? len : 0` with the
/// runtime invariant `len <= INT64_MAX/width && (len == 0 || data != NULL)`.
/// Keys are the SSA identities of the descriptor's data and length values
/// plus the element width, so a limit is a pure function of values that never
/// change. Callers scope entries by dominance: a limit created inside a nested
/// region is discarded when that region ends ([`Spans::retain_inherited`]).
/// No access ever records the descriptor as validated: a foreign view outside
/// the invariant may still be storable, so each access keeps its own compare.
#[derive(Clone)]
pub(crate) struct Spans<V> {
    pub limits: BTreeMap<(V, V, usize), V>,
}
impl<V> Default for Spans<V> {
    fn default() -> Self {
        Self {
            limits: BTreeMap::new(),
        }
    }
}
impl<V: Ord + Copy> Spans<V> {
    pub fn retain_inherited(&mut self, inherited: &Self) {
        self.limits
            .retain(|key, value| inherited.limits.get(key) == Some(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loop_entry_keeps_only_facts_of_never_rewritten_versions() {
        let mut state = State::default();
        state.integers.insert(1, 10);
        state.integers.insert(2, 20);
        state.active.insert(Fact::Within(10, 1, i64::MAX));
        state.active.insert(Fact::Within(20, 0, 7));
        state.active.insert(Fact::BelowLength(20, 3));
        state.active.insert(Fact::NonnegativeLength(3));
        state.booleans.insert(4, Boolean::unknown(99));
        state.enter_loop_preserving(|local| local == 1);
        assert_eq!(state.integers.into_iter().collect::<Vec<_>>(), [(1, 10)]);
        assert_eq!(
            state.active.into_iter().collect::<Vec<_>>(),
            [Fact::Within(10, 1, i64::MAX), Fact::NonnegativeLength(3)]
        );
        assert!(state.booleans.is_empty());
    }
    #[test]
    fn loop_entry_drops_length_snapshots_of_rewritable_locals() {
        let mut state = State::default();
        state.lengths.insert(1, 7);
        state.lengths.insert(2, 9);
        state.enter_loop_preserving(|local| local == 1);
        assert_eq!(state.lengths.into_iter().collect::<Vec<_>>(), [(1, 7)]);
    }
    #[test]
    fn clearing_scalar_facts_keeps_ssa_value_facts_only() {
        let mut state = State::default();
        state.integers.insert(1, 10);
        state.lengths.insert(1, 3);
        state.active.insert(Fact::Within(10, 0, 5));
        state.active.insert(Fact::BelowLength(10, 3));
        state.active.insert(Fact::NonnegativeLength(3));
        state.booleans.insert(2, Boolean::unknown(4));
        state.clear_preserving_values();
        assert!(state.integers.is_empty() && state.lengths.is_empty() && state.booleans.is_empty());
        assert_eq!(
            state.active.into_iter().collect::<Vec<_>>(),
            [Fact::NonnegativeLength(3)]
        );
    }
    #[test]
    fn exit_join_renames_differing_versions_with_the_union_of_their_facts() {
        let mut a = State::default();
        let mut b = State::default();
        a.integers.insert(1, 10);
        b.integers.insert(1, 20);
        a.integers.insert(2, 30);
        b.integers.insert(2, 30);
        a.integers.insert(3, 40);
        a.active.insert(Fact::Within(10, 0, 9));
        a.active.insert(Fact::Within(10, 2, 100));
        b.active.insert(Fact::Within(20, 1, 50));
        a.active.insert(Fact::BelowLength(10, 7));
        b.active.insert(Fact::BelowLength(20, 7));
        a.active.insert(Fact::BelowLength(10, 8));
        a.active.insert(Fact::Within(30, 5, 5));
        b.active.insert(Fact::Within(30, 5, 5));
        a.active.insert(Fact::Within(40, 0, 1));
        a.lengths.insert(4, 7);
        b.lengths.insert(4, 7);
        a.lengths.insert(5, 7);
        let mut serial = 100;
        let joined = State::join_by_local(&a, &b, &mut serial);
        assert_eq!(joined.integers[&1], 100);
        assert_eq!(joined.integers[&2], 30);
        assert!(!joined.integers.contains_key(&3));
        assert_eq!(
            joined.active.into_iter().collect::<Vec<_>>(),
            [
                Fact::Within(30, 5, 5),
                Fact::Within(100, 1, 50),
                Fact::BelowLength(100, 7)
            ]
        );
        assert_eq!(joined.lengths.into_iter().collect::<Vec<_>>(), [(4, 7)]);
    }
    #[test]
    fn nested_span_limits_do_not_escape_their_region() {
        let mut spans = Spans::<u32>::default();
        spans.limits.insert((1, 2, 8), 3);
        spans.limits.insert((9, 9, 1), 9);
        let inherited = spans.clone();
        spans.limits.insert((4, 5, 8), 6);
        spans.limits.insert((1, 2, 8), 7);
        spans.retain_inherited(&inherited);
        assert_eq!(
            spans.limits.into_iter().collect::<Vec<_>>(),
            [((9, 9, 1), 9)]
        );
    }
}
