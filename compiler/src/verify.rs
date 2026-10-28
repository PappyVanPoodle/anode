//! Deterministic finite boundary evidence, never a universal contract proof.
//! Execution is opt-in and uses only normally checked, explicitly supported IR.
//! This evaluator has no filesystem, process, FFI, console or thread operations.
use crate::{
    ir::*,
    numeric::Float,
    project::{Project, SourceFile},
    syntax::{Borrow, DataKind, Type},
    Error, Result,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct VerificationOptions {
    pub execute: bool,
    /// Return ordinary-checked source insertion proposals. Never writes files.
    pub emit_test: bool,
    pub max_cases: usize,
    pub max_steps: usize,
    pub timeout_ms: u64,
    pub functions: Vec<String>,
}
impl Default for VerificationOptions {
    fn default() -> Self {
        Self {
            execute: false,
            emit_test: false,
            max_cases: 128,
            max_steps: 100_000,
            timeout_ms: 1000,
            functions: Vec::new(),
        }
    }
}
impl VerificationOptions {
    pub fn from_request(request: &Value) -> Result<Self> {
        let mut options = Self::default();
        if let Some(value) = request.get("execute") {
            options.execute = value
                .as_bool()
                .ok_or_else(|| Error::new(0, "execute must be a boolean"))?;
        }
        if let Some(value) = request.get("emit_test") {
            options.emit_test = value
                .as_bool()
                .ok_or_else(|| Error::new(0, "emit_test must be a boolean"))?;
        }
        for (name, slot) in [
            ("max_cases", &mut options.max_cases),
            ("max_steps", &mut options.max_steps),
        ] {
            if let Some(value) = request.get(name) {
                *slot = value
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| Error::new(0, format!("{name} must be a positive integer")))?;
            }
        }
        if let Some(value) = request.get("timeout_ms") {
            options.timeout_ms = value
                .as_u64()
                .ok_or_else(|| Error::new(0, "timeout_ms must be a positive integer"))?;
        }
        if let Some(value) = request.get("functions") {
            options.functions = value
                .as_array()
                .ok_or_else(|| Error::new(0, "functions must be an array"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| Error::new(0, "function names must be strings"))
                })
                .collect::<Result<_>>()?;
        }
        options.validate()?;
        Ok(options)
    }
    fn validate(&self) -> Result<()> {
        if !(1..=4096).contains(&self.max_cases)
            || !(1..=1_000_000).contains(&self.max_steps)
            || !(1..=10_000).contains(&self.timeout_ms)
        {
            return Err(Error::new(0,"verification budgets: max_cases 1..4096, max_steps 1..1000000, timeout_ms 1..10000"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Sample {
    Int(i128),
    Float(f64),
    Bool(bool),
    Text(String),
    Sequence(Vec<Sample>),
    Record(Vec<Sample>),
    Variant(usize, Box<Sample>),
    Unit,
}
impl Sample {
    fn weight(&self) -> usize {
        match self {
            Self::Text(text) => 1usize.saturating_add(text.len()),
            Self::Sequence(items) | Self::Record(items) => items
                .iter()
                .fold(1usize, |n, v| n.saturating_add(v.weight())),
            Self::Variant(_, value) => 1usize.saturating_add(value.weight()),
            _ => 1,
        }
    }
    fn json(&self) -> Value {
        match self {
            Self::Int(n) => json!(n.to_string()),
            Self::Float(n) => json!(n.to_string()),
            Self::Bool(v) => json!(v),
            Self::Text(v) if v.len() > 1024 => {
                json!({"byte_length":v.len(),"prefix":v.chars().take(256).collect::<String>(),"truncated":true})
            }
            Self::Text(v) => json!(v),
            Self::Sequence(v) | Self::Record(v) if v.len() > 64 => {
                json!({"length":v.len(),"prefix":v.iter().take(64).map(Self::json).collect::<Vec<_>>(),"truncated":true})
            }
            Self::Sequence(v) | Self::Record(v) => {
                json!(v.iter().map(Self::json).collect::<Vec<_>>())
            }
            Self::Variant(tag, v) => json!({"variant":tag,"payload":v.json()}),
            Self::Unit => Value::Null,
        }
    }
    fn int(&self) -> EvalResult<i128> {
        if let Self::Int(n) = self {
            Ok(*n)
        } else {
            Err(Fault::unsupported("expected integer IR value"))
        }
    }
    fn boolean(&self) -> EvalResult<bool> {
        if let Self::Bool(v) = self {
            Ok(*v)
        } else {
            Err(Fault::unsupported("expected bool IR value"))
        }
    }
}
fn bounds(ty: Type) -> Option<(i128, i128)> {
    let n = ty.integer()?;
    Some(if n.signed() {
        (-(1i128 << (n.bits() - 1)), (1i128 << (n.bits() - 1)) - 1)
    } else {
        (0, (1i128 << n.bits()) - 1)
    })
}

// Explicit modular operations are distinct from ordinary checked arithmetic.
// Use an unsigned128 scratch value so every exact-width operation, including
// u64 multiplication and signed minimum values, has defined host semantics.
fn integer_intrinsic(name: &str, values: &[Sample], ty: Type) -> EvalResult<Sample> {
    let number = ty
        .integer()
        .ok_or_else(|| Fault::unsupported("integer intrinsic type"))?;
    let width = number.bits() as u32;
    let mask = (1u128 << width) - 1;
    let operand = |index: usize| -> EvalResult<i128> {
        values
            .get(index)
            .ok_or_else(|| Fault::unsupported("missing integer intrinsic argument"))?
            .int()
    };
    let a = (operand(0)? as u128) & mask;
    let signed = |bits: u128| {
        let bits = bits & mask;
        if number.signed() && bits & (1u128 << (width - 1)) != 0 {
            bits as i128 - (1i128 << width)
        } else {
            bits as i128
        }
    };
    let result = match name {
        "bit_not" => !a,
        "shl" | "shr" | "rotl" | "rotr" => {
            let count = (operand(1)? as u64 & u64::from(width - 1)) as u32;
            let reverse = (width - count) & (width - 1);
            match name {
                "shl" => a << count,
                "shr" if number.signed() => (signed(a) >> count) as u128,
                "shr" => a >> count,
                "rotl" => (a << count) | (a >> reverse),
                "rotr" => (a >> count) | (a << reverse),
                _ => unreachable!(),
            }
        }
        _ => {
            let b = (operand(1)? as u128) & mask;
            match name {
                "wrapping_add" => a.wrapping_add(b),
                "wrapping_sub" => a.wrapping_sub(b),
                "wrapping_mul" => a.wrapping_mul(b),
                "bit_and" => a & b,
                "bit_or" => a | b,
                "bit_xor" => a ^ b,
                _ => return Err(Fault::unsupported("unknown integer intrinsic")),
            }
        }
    };
    Ok(Sample::Int(signed(result)))
}

fn packet_read(name: &str, values: &[Sample]) -> EvalResult<Sample> {
    let width = match name {
        "read_u16_le" | "read_u16_be" => 2usize,
        "read_u32_le" | "read_u32_be" => 4,
        "read_u64_le" | "read_u64_be" => 8,
        _ => return Err(Fault::unsupported("unknown packet read")),
    };
    let Some(Sample::Sequence(bytes)) = values.first() else {
        return Err(Fault::unsupported(
            "packet reads require sampled byte storage",
        ));
    };
    let offset = values
        .get(1)
        .ok_or_else(|| Fault::unsupported("missing packet offset"))?
        .int()?;
    let offset =
        usize::try_from(offset).map_err(|_| Fault::runtime("packet read out of bounds"))?;
    let end = offset
        .checked_add(width)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| Fault::runtime("packet read out of bounds"))?;
    let mut value = 0u64;
    for (index, byte) in bytes[offset..end].iter().enumerate() {
        let byte = u8::try_from(byte.int()?)
            .map_err(|_| Fault::unsupported("sampled packet element is not a byte"))?;
        let shift = if name.ends_with("_be") {
            width - index - 1
        } else {
            index
        };
        value |= u64::from(byte) << (shift * 8);
    }
    Ok(Sample::Int(i128::from(value)))
}

/// The bytes of a sampled `text`/`str` or `[u8; N]`/`&[u8]` hash input.
fn sample_bytes(value: &Sample) -> EvalResult<Vec<u8>> {
    match value {
        Sample::Text(text) => Ok(text.as_bytes().to_vec()),
        Sample::Sequence(items) => items
            .iter()
            .map(|item| {
                u8::try_from(item.int()?)
                    .map_err(|_| Fault::unsupported("sampled byte buffer element is not a byte"))
            })
            .collect(),
        _ => Err(Fault::unsupported(
            "hash input requires text or sampled byte storage",
        )),
    }
}

/// Exact 64-bit FNV-1a (contract N3), evaluated independently of the runtime.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn normalize(value: Sample, ty: Type) -> EvalResult<Sample> {
    match (&value, ty) {
        (Sample::Int(n), _) if ty.integer().is_some() => {
            let (lo, hi) = bounds(ty).unwrap();
            if *n < lo || *n > hi {
                return Err(Fault::runtime(
                    "integer arithmetic overflow or conversion out of range",
                ));
            }
        }
        (Sample::Float(_), Type::Float(Float::F32)) => {
            if let Sample::Float(n) = value {
                return Ok(Sample::Float((n as f32) as f64));
            }
        }
        _ => {}
    }
    Ok(value)
}
fn zero(ty: Type, program: &CheckedProgram, depth: usize) -> EvalResult<Sample> {
    if depth > 32 {
        return Err(Fault::unsupported("sample type nesting exceeds 32"));
    }
    Ok(match ty {
        t if t.integer().is_some() => Sample::Int(0),
        Type::Float(_) => Sample::Float(0.0),
        Type::Bool => Sample::Bool(false),
        Type::Text | Type::TextView => Sample::Text(String::new()),
        Type::Ints | Type::Texts | Type::Slice(_) => Sample::Sequence(Vec::new()),
        Type::Fixed(_, n) => Sample::Sequence(vec![Sample::Int(0); n]),
        Type::Unit => Sample::Unit,
        Type::Named(id) => {
            match &program.types.definitions[id].kind {
                DataKind::Record(fields) => Sample::Record(
                    fields
                        .iter()
                        .map(|f| zero(f.ty, program, depth + 1))
                        .collect::<EvalResult<_>>()?,
                ),
                DataKind::Enum(fields) => {
                    Sample::Variant(0, Box::new(zero(fields[0].ty, program, depth + 1)?))
                }
                DataKind::Vector(_) | DataKind::SoaVector(_) | DataKind::Map(_) => {
                    Sample::Sequence(Vec::new())
                }
                DataKind::Arena(_) | DataKind::Channel(_) => return Err(Fault::unsupported(
                    "arena/channel input sampling is unsupported; live instance or synchronization required",
                )),
            }
        }
        _ => return Err(Fault::unsupported("task/scoped input is unsupported")),
    })
}
fn samples(ty: Type, program: &CheckedProgram) -> EvalResult<Vec<Sample>> {
    if let Some((lo, hi)) = bounds(ty) {
        let mut values = vec![0, 1];
        if lo < 0 {
            values.push(-1);
        }
        values.extend([lo, hi]);
        let mut seen = BTreeSet::new();
        return Ok(values
            .into_iter()
            .filter(|v| seen.insert(*v))
            .map(Sample::Int)
            .collect());
    }
    Ok(match ty {
        Type::Bool => vec![Sample::Bool(false), Sample::Bool(true)],
        Type::Float(n) => {
            let max = if n == Float::F32 {
                f32::MAX as f64
            } else {
                f64::MAX
            };
            vec![0.0, 1.0, -1.0, -max, max]
                .into_iter()
                .map(Sample::Float)
                .collect()
        }
        Type::Text => vec![Sample::Text(String::new()), Sample::Text("a".into())],
        Type::Ints => vec![
            Sample::Sequence(Vec::new()),
            Sample::Sequence(vec![Sample::Int(0)]),
        ],
        Type::Texts => vec![
            Sample::Sequence(Vec::new()),
            Sample::Sequence(vec![Sample::Text(String::new())]),
        ],
        Type::TextView | Type::Slice(_) => {
            return Err(Fault::unsupported(
                "scoped-view input sampling is unsupported",
            ))
        }
        _ => vec![zero(ty, program, 0)?],
    })
}
#[derive(Debug)]
struct Fault {
    kind: &'static str,
    message: String,
    line: Option<usize>,
    function: Option<String>,
}
type EvalResult<T> = std::result::Result<T, Fault>;
impl Fault {
    fn budget(message: impl Into<String>) -> Self {
        Self {
            kind: "budget_exhausted",
            message: message.into(),
            line: None,
            function: None,
        }
    }
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: "unsupported",
            message: message.into(),
            line: None,
            function: None,
        }
    }
    fn runtime(message: impl Into<String>) -> Self {
        Self {
            kind: "runtime_fault",
            message: message.into(),
            line: None,
            function: None,
        }
    }
    fn json(&self) -> Value {
        json!({"kind":self.kind,"message":self.message,"line":self.line,"function":self.function})
    }
}
enum Flow {
    Next,
    Return(Sample),
    Break(usize),
    Continue(usize),
}
struct Evaluator<'a> {
    program: &'a CheckedProgram,
    steps: usize,
    limit: usize,
    deadline: Instant,
    depth: usize,
    frame_weights: Vec<usize>,
}
impl Evaluator<'_> {
    fn tick(&mut self) -> EvalResult<()> {
        self.steps += 1;
        if self.steps > self.limit || Instant::now() >= self.deadline {
            let mut fault = Fault::runtime("execution budget exhausted");
            fault.kind = "budget_exhausted";
            return Err(fault);
        }
        Ok(())
    }
    fn get(locals: &[Sample], place: &Place) -> EvalResult<Sample> {
        let mut value = locals
            .get(place.local)
            .ok_or_else(|| Fault::unsupported("invalid checked local"))?;
        for &field in &place.fields {
            value = match value {
                Sample::Record(fields) => fields.get(field),
                Sample::Variant(_, payload) if field == 0 => Some(payload.as_ref()),
                _ => None,
            }
            .ok_or_else(|| Fault::unsupported("unsupported aggregate projection"))?;
        }
        Ok(value.clone())
    }
    fn set(locals: &mut [Sample], place: &Place, value: Sample) -> EvalResult<()> {
        let mut target = locals
            .get_mut(place.local)
            .ok_or_else(|| Fault::unsupported("invalid checked local"))?;
        for &field in &place.fields {
            target = match target {
                Sample::Record(fields) => fields.get_mut(field),
                Sample::Variant(_, payload) if field == 0 => Some(payload.as_mut()),
                _ => None,
            }
            .ok_or_else(|| Fault::unsupported("unsupported aggregate assignment"))?;
        }
        *target = value;
        Ok(())
    }
    fn call(&mut self, name: &str, args: Vec<Sample>) -> EvalResult<Sample> {
        self.tick()?;
        if self.depth >= 64 {
            return Err(Fault::unsupported("recursion depth exceeds 64"));
        }
        let f = self
            .program
            .functions
            .iter()
            .find(|f| f.source.name == name)
            .ok_or_else(|| Fault::unsupported("unknown/foreign callee"))?;
        if f.source.external_symbol.is_some() {
            return Err(Fault::unsupported("foreign functions are never executed"));
        }
        // Checked IR initializes every live local before use. Eagerly building
        // aggregate zeroes for dead temporaries would needlessly amplify memory.
        if f.locals.len() > 65536 {
            return Err(Fault::budget(
                "checked local count exceeds sampled memory budget",
            ));
        }
        let mut locals = vec![Sample::Unit; f.locals.len()];
        for (i, arg) in args.into_iter().enumerate() {
            locals[i] = arg;
        }
        self.depth += 1;
        self.frame_weights.push(0);
        let result = self.block(&f.body, &mut locals);
        self.frame_weights.pop();
        self.depth -= 1;
        match result {
            Ok(Flow::Return(value)) => Ok(value),
            Ok(_) => Ok(Sample::Unit),
            Err(mut e) => {
                if e.function.is_none() {
                    e.function = Some(name.into());
                }
                if e.kind == "precondition_rejected" && self.depth > 0 {
                    e.kind = "runtime_fault";
                }
                Err(e)
            }
        }
    }
    fn value(&mut self, value: &Rvalue, ty: Type, locals: &mut [Sample]) -> EvalResult<Sample> {
        let get = |p: &Place| Self::get(locals, p);
        let result = match value {
            Rvalue::Int(n) => Sample::Int(*n as i128),
            Rvalue::Float(n) => Sample::Float(f64::from_bits(*n)),
            Rvalue::Bool(v) => Sample::Bool(*v),
            Rvalue::Text(s) => Sample::Text(s.clone()),
            Rvalue::Zero => zero(ty, self.program, 0)?,
            Rvalue::Copy(p) | Rvalue::Reference(p) => get(p)?,
            Rvalue::Repeat(p, n) => {
                if *n > 32768 {
                    return Err(Fault::budget(
                        "repeated value exceeds sampled memory budget",
                    ));
                }
                Sample::Sequence(vec![get(p)?; *n])
            }
            Rvalue::Record(fields) => {
                if fields.iter().try_fold(1usize, |n, p| {
                    Ok::<_, Fault>(n.saturating_add(Self::get(locals, p)?.weight()))
                })? > 32768
                {
                    return Err(Fault::budget("record exceeds sampled memory budget"));
                }
                Sample::Record(fields.iter().map(get).collect::<EvalResult<_>>()?)
            }
            Rvalue::Variant { index, payload } => Sample::Variant(
                *index,
                Box::new(
                    payload
                        .as_ref()
                        .map(get)
                        .transpose()?
                        .unwrap_or(Sample::Unit),
                ),
            ),
            Rvalue::Convert(p) => match (get(p)?, ty) {
                (Sample::Int(n), Type::Float(_)) => Sample::Float(n as f64),
                (Sample::Float(n), t) if t.integer().is_some() => {
                    let (lo, hi) = bounds(t).unwrap();
                    let truncated = n.trunc();
                    if !truncated.is_finite()
                        || truncated < lo as f64
                        || truncated
                            >= if hi == i64::MAX as i128 || hi == u64::MAX as i128 {
                                hi as f64
                            } else {
                                hi as f64 + 1.0
                            }
                    {
                        return Err(Fault::runtime("float conversion out of range"));
                    }
                    Sample::Int(truncated as i128)
                }
                (v, _) => v,
            },
            Rvalue::Unary(op, p) => match (op.as_str(), get(p)?) {
                ("!", v) => Sample::Bool(!v.boolean()?),
                ("-", Sample::Int(n)) => Sample::Int(-n),
                ("-", Sample::Float(n)) => Sample::Float(-n),
                _ => return Err(Fault::unsupported("unsupported unary operator")),
            },
            Rvalue::Binary(op, a, b) => binary(op, get(a)?, get(b)?, a.ty)?,
            Rvalue::IntRange { value, low, high } => {
                let n = get(value)?.int()?;
                Sample::Bool((*low as i128) <= n && n <= (*high as i128))
            }
            Rvalue::Call { name, args, spawn } => {
                if *spawn {
                    return Err(Fault::unsupported("threads are never executed"));
                }
                let args = args
                    .iter()
                    .map(|a| match a {
                        Argument::Value(p) | Argument::Reference(p) => get(p),
                    })
                    .collect::<EvalResult<_>>()?;
                self.call(name, args)?
            }
            Rvalue::Builtin { name, args } => self.builtin(name, args, ty, locals)?,
            Rvalue::Element { .. } | Rvalue::Region { .. } => {
                return Err(Fault::unsupported(
                    "scoped element and region aliases are not sampled",
                ))
            }
        };
        if result.weight() > 32768 {
            return Err(Fault::budget("value exceeds sampled memory budget"));
        }
        normalize(result, ty)
    }
    fn builtin(
        &mut self,
        name: &str,
        args: &[Place],
        ty: Type,
        locals: &mut [Sample],
    ) -> EvalResult<Sample> {
        let values = args
            .iter()
            .map(|p| Self::get(locals, p))
            .collect::<EvalResult<Vec<_>>>()?;
        let first = || {
            values
                .first()
                .ok_or_else(|| Fault::unsupported("missing builtin argument"))
        };
        if crate::syntax::integer_intrinsic(name).is_some() {
            return integer_intrinsic(name, &values, ty);
        }
        if matches!(
            name,
            "read_u16_le"
                | "read_u32_le"
                | "read_u64_le"
                | "read_u16_be"
                | "read_u32_be"
                | "read_u64_be"
        ) {
            return packet_read(name, &values);
        }
        Ok(match name {
            "len" | "fixed_len" | "map_len" => Sample::Int(match first()? {
                Sample::Text(s) => s.len(),
                Sample::Sequence(s) => s.len(),
                _ => return Err(Fault::unsupported("unsupported len operand")),
            } as i128),
            "clone" => first()?.clone(),
            "at" | "fixed_at" | "vec_at" | "vec_clone_at" | "text_at" | "byte_at" => {
                let index = values[1].int()?;
                if index < 0 {
                    return Err(Fault::runtime("index out of bounds"));
                }
                match first()? {
                    Sample::Sequence(v) => v.get(index as usize).cloned(),
                    Sample::Text(s) => s
                        .as_bytes()
                        .get(index as usize)
                        .map(|b| Sample::Int(*b as i128)),
                    _ => return Err(Fault::unsupported("unsupported indexing operand")),
                }
                .ok_or_else(|| Fault::runtime("index out of bounds"))?
            }
            "push" | "texts_push" | "vec_push" => {
                let mut value = Self::get(locals, &args[0])?;
                let Sample::Sequence(items) = &mut value else {
                    return Err(Fault::unsupported("unsupported collection push"));
                };
                if items.len() >= 65536 {
                    return Err(Fault::runtime("sample collection budget exceeded"));
                }
                items.push(values[1].clone());
                Self::set(locals, &args[0], value)?;
                Sample::Unit
            }
            "set" | "fixed_set" | "fixed_try_set" => {
                let mut value = Self::get(locals, &args[0])?;
                let Sample::Sequence(items) = &mut value else {
                    return Err(Fault::unsupported("unsupported collection set"));
                };
                let n = values[1].int()?;
                if n < 0 || n as usize >= items.len() {
                    if name == "fixed_try_set" {
                        return Ok(Sample::Bool(false));
                    }
                    return Err(Fault::runtime("index out of bounds"));
                }
                items[n as usize] = values[2].clone();
                Self::set(locals, &args[0], value)?;
                if name == "fixed_try_set" {
                    Sample::Bool(true)
                } else {
                    Sample::Unit
                }
            }
            "try_write_u16_be" | "try_write_u32_be" | "try_write_u64_be" => {
                let width = match name {
                    "try_write_u16_be" => 2usize,
                    "try_write_u32_be" => 4,
                    _ => 8,
                };
                let mut value = Self::get(locals, &args[0])?;
                let Sample::Sequence(bytes) = &mut value else {
                    return Err(Fault::unsupported(
                        "packet writes require sampled byte storage",
                    ));
                };
                let offset = values
                    .get(1)
                    .ok_or_else(|| Fault::unsupported("missing packet offset"))?
                    .int()?;
                let raw = values
                    .get(2)
                    .ok_or_else(|| Fault::unsupported("missing packet value"))?
                    .int()?;
                let raw = u64::try_from(raw)
                    .map_err(|_| Fault::unsupported("packet value is not an unsigned integer"))?;
                // Contract N2: a negative offset, an offset past the end or a
                // short tail returns false and leaves the buffer unchanged.
                let Ok(offset) = usize::try_from(offset) else {
                    return Ok(Sample::Bool(false));
                };
                if offset > bytes.len() || bytes.len() - offset < width {
                    return Ok(Sample::Bool(false));
                }
                for (index, byte) in bytes[offset..offset + width].iter_mut().enumerate() {
                    let shift = 8 * (width - 1 - index);
                    *byte = Sample::Int(i128::from((raw >> shift) as u8));
                }
                Self::set(locals, &args[0], value)?;
                Sample::Bool(true)
            }
            "fnv1a_64" => Sample::Int(i128::from(fnv1a_64(&sample_bytes(first()?)?))),
            "sha256_text" => Sample::Text(crate::backend::sha256(&sample_bytes(first()?)?)),
            "texts" => Sample::Sequence(Vec::new()),
            "text_eq" => Sample::Bool(
                matches!((&values[0],&values[1]),(Sample::Text(a),Sample::Text(b)) if a==b),
            ),
            "is_finite" => Sample::Bool(matches!(first()?,Sample::Float(n) if n.is_finite())),
            "is_nan" => Sample::Bool(matches!(first()?,Sample::Float(n) if n.is_nan())),
            "sqrt" | "abs" => match first()? {
                Sample::Float(n) => Sample::Float(if name == "sqrt" { n.sqrt() } else { n.abs() }),
                _ => return Err(Fault::unsupported("unsupported float operand")),
            },
            "saturating_add" | "saturating_sub" => {
                let a = values[0].int()?;
                let b = values[1].int()?;
                let (lo, hi) =
                    bounds(ty).ok_or_else(|| Fault::unsupported("saturating arithmetic type"))?;
                Sample::Int(
                    if name == "saturating_add" {
                        a.saturating_add(b)
                    } else {
                        a.saturating_sub(b)
                    }
                    .clamp(lo, hi),
                )
            }
            _ => {
                return Err(Fault::unsupported(format!(
                    "builtin {name} is outside the safe sampled subset"
                )))
            }
        })
    }
    fn block(&mut self, ops: &[Operation], locals: &mut [Sample]) -> EvalResult<Flow> {
        for op in ops {
            self.tick()?;
            if let Some(weight) = self.frame_weights.last_mut() {
                *weight = locals
                    .iter()
                    .fold(0usize, |n, v| n.saturating_add(v.weight()));
            }
            if self.frame_weights.iter().sum::<usize>() > 65536 {
                return Err(Fault::budget("live values exceed sampled memory budget"));
            }
            match op {
                Operation::Init(place, value) => {
                    let value = self.value(value, place.ty, locals)?;
                    Self::set(locals, place, value)?;
                }
                Operation::Move {
                    destination,
                    source,
                    ..
                } => {
                    let value = Self::get(locals, source)?;
                    Self::set(locals, destination, value)?;
                    Self::set(locals, source, Sample::Unit)?;
                }
                Operation::Assign(dst, src) => Self::set(locals, dst, Self::get(locals, src)?)?,
                Operation::Drop(place) => Self::set(locals, place, Sample::Unit)?,
                Operation::Loan { .. } => {}
                Operation::Assert {
                    value,
                    line,
                    message,
                } => {
                    if !Self::get(locals, value)?.boolean()? {
                        return Err(Fault {
                            kind: if message == "precondition failed" {
                                "precondition_rejected"
                            } else if message == "postcondition failed" {
                                "counterexample"
                            } else {
                                "runtime_fault"
                            },
                            message: message.clone(),
                            line: Some(*line),
                            function: None,
                        });
                    }
                }
                Operation::If { condition, yes, no } => {
                    let flow = self.block(
                        if Self::get(locals, condition)?.boolean()? {
                            yes
                        } else {
                            no
                        },
                        locals,
                    )?;
                    if !matches!(flow, Flow::Next) {
                        return Ok(flow);
                    }
                }
                Operation::Loop {
                    id,
                    condition_code,
                    condition,
                    body,
                } => loop {
                    self.tick()?;
                    let flow = self.block(condition_code, locals)?;
                    if !matches!(flow, Flow::Next) {
                        return Ok(flow);
                    }
                    if !Self::get(locals, condition)?.boolean()? {
                        break;
                    }
                    match self.block(body, locals)? {
                        Flow::Next => {}
                        Flow::Continue(n) if n == *id => {}
                        Flow::Break(n) if n == *id => break,
                        other => return Ok(other),
                    }
                },
                Operation::Return(value) => {
                    return Ok(Flow::Return(
                        value
                            .as_ref()
                            .map(|p| Self::get(locals, p))
                            .transpose()?
                            .unwrap_or(Sample::Unit),
                    ))
                }
                Operation::Break(id) => return Ok(Flow::Break(*id)),
                Operation::Continue(id) => return Ok(Flow::Continue(*id)),
                Operation::Switch { value, arms } => {
                    let Sample::Variant(index, _) = Self::get(locals, value)? else {
                        return Err(Fault::unsupported("switch value is not an enum"));
                    };
                    let flow = self.block(
                        arms.get(index)
                            .ok_or_else(|| Fault::runtime("invalid enum tag"))?,
                        locals,
                    )?;
                    if !matches!(flow, Flow::Next) {
                        return Ok(flow);
                    }
                }
                _ => {
                    return Err(Fault::unsupported(
                        "alias/SoA/swap operations are not sampled",
                    ))
                }
            }
        }
        Ok(Flow::Next)
    }
}
fn binary(op: &str, a: Sample, b: Sample, input_type: Type) -> EvalResult<Sample> {
    if let (Sample::Bool(a), Sample::Bool(b)) = (&a, &b) {
        return Ok(Sample::Bool(match op {
            "&&" => *a && *b,
            "||" => *a || *b,
            "==" => a == b,
            "!=" => a != b,
            _ => return Err(Fault::unsupported("bool operator")),
        }));
    }
    if let (Sample::Float(a), Sample::Float(b)) = (&a, &b) {
        return Ok(match op {
            "+" => Sample::Float(a + b),
            "-" => Sample::Float(a - b),
            "*" | "*literal" => Sample::Float(a * b),
            "/" => Sample::Float(a / b),
            "%" => Sample::Float(a % b),
            "==" => Sample::Bool(a == b),
            "!=" => Sample::Bool(a != b),
            "<" => Sample::Bool(a < b),
            "<=" => Sample::Bool(a <= b),
            ">" => Sample::Bool(a > b),
            ">=" => Sample::Bool(a >= b),
            _ => return Err(Fault::unsupported("float operator")),
        });
    }
    let a = a.int()?;
    let b = b.int()?;
    if matches!(op, "/" | "%")
        && b == -1
        && input_type.integer().is_some_and(|n| n.signed())
        && bounds(input_type).is_some_and(|(low, _)| a == low)
    {
        return Err(Fault::runtime("integer overflow"));
    }
    Ok(match op {
        "+" => Sample::Int(
            a.checked_add(b)
                .ok_or_else(|| Fault::runtime("integer overflow"))?,
        ),
        "-" => Sample::Int(
            a.checked_sub(b)
                .ok_or_else(|| Fault::runtime("integer overflow"))?,
        ),
        "*" | "*literal" => Sample::Int(
            a.checked_mul(b)
                .ok_or_else(|| Fault::runtime("integer overflow"))?,
        ),
        "/" | "%" => {
            if b == 0 {
                return Err(Fault::runtime("division by zero"));
            }
            Sample::Int(if op == "/" { a / b } else { a % b })
        }
        "==" => Sample::Bool(a == b),
        "!=" => Sample::Bool(a != b),
        "<" => Sample::Bool(a < b),
        "<=" => Sample::Bool(a <= b),
        ">" => Sample::Bool(a > b),
        ">=" => Sample::Bool(a >= b),
        _ => return Err(Fault::unsupported(format!("operator {op}"))),
    })
}

fn supported(
    program: &CheckedProgram,
    name: &str,
    seen: &mut BTreeSet<String>,
) -> std::result::Result<(), String> {
    if !seen.insert(name.into()) {
        return Ok(());
    }
    let f = program
        .functions
        .iter()
        .find(|f| f.source.name == name)
        .ok_or("unknown effect/callee")?;
    if f.source.external_symbol.is_some() {
        return Err("foreign functions are never executed".into());
    }
    if f.source
        .params
        .iter()
        .any(|p| p.borrow == Borrow::Exclusive)
    {
        return Err("exclusive parameter execution is unsupported".into());
    }
    fn ops(
        program: &CheckedProgram,
        body: &[Operation],
        seen: &mut BTreeSet<String>,
    ) -> std::result::Result<(), String> {
        for op in body {
            match op {
                Operation::Init(_, Rvalue::Call { name, spawn, .. }) => {
                    if *spawn {
                        return Err("threads are never executed".into());
                    }
                    supported(program, name, seen)?;
                }
                Operation::Init(_, Rvalue::Builtin { name, .. }) => {
                    if name == "process_run" {
                        return Err("subprocesses are never executed by the pure verifier".into());
                    }
                    if name == "net_request" || name == "http_request" {
                        return Err(
                            "network requests are never executed by the pure verifier".into()
                        );
                    }
                    if crate::syntax::integer_intrinsic(name).is_none()
                        && ![
                            "read_u16_le",
                            "read_u32_le",
                            "read_u64_le",
                            "read_u16_be",
                            "read_u32_be",
                            "read_u64_be",
                            "len",
                            "fixed_len",
                            "map_len",
                            "clone",
                            "at",
                            "fixed_at",
                            "vec_at",
                            "vec_clone_at",
                            "text_at",
                            "byte_at",
                            "push",
                            "texts_push",
                            "vec_push",
                            "set",
                            "fixed_set",
                            "fixed_try_set",
                            "texts",
                            "text_eq",
                            "is_finite",
                            "is_nan",
                            "sqrt",
                            "abs",
                            "saturating_add",
                            "saturating_sub",
                            "try_write_u16_be",
                            "try_write_u32_be",
                            "try_write_u64_be",
                            "fnv1a_64",
                            "sha256_text",
                        ]
                        .contains(&name.as_str())
                    {
                        return Err(format!("builtin {name} is outside the safe sampled subset"));
                    }
                }
                Operation::Init(
                    _,
                    Rvalue::Element { .. } | Rvalue::Region { .. } | Rvalue::Reference(_),
                ) => return Err("scoped aliases are unsupported".into()),
                Operation::If { yes, no, .. } => {
                    ops(program, yes, seen)?;
                    ops(program, no, seen)?;
                }
                Operation::Loop {
                    condition_code,
                    body,
                    ..
                } => {
                    ops(program, condition_code, seen)?;
                    ops(program, body, seen)?;
                }
                Operation::Switch { arms, .. } => {
                    for arm in arms {
                        ops(program, arm, seen)?;
                    }
                }
                Operation::Swap(..)
                | Operation::ElementCheckPair { .. }
                | Operation::ElementWriteback { .. } => {
                    return Err("alias/SoA/swap operations are unsupported".into())
                }
                _ => {}
            }
        }
        Ok(())
    }
    ops(program, &f.body, seen)
}
fn function_names(project: &Project) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for (i, file) in project.files.iter().enumerate() {
        for (j, f) in file.parsed.functions.iter().enumerate() {
            let owner = if let Some(method) = &f.method {
                let Type::Named(id) = method.owner else {
                    continue;
                };
                Some(file.parsed.type_names[id].as_str())
            } else {
                None
            };
            let function = f
                .method
                .as_ref()
                .map_or(f.name.as_str(), |method| method.name.as_str());
            let name = crate::navigation::synthesis_function_name(&file.module, function, owner);
            names.insert(crate::project::function_symbol(i, j, &file.module, f), name);
        }
    }
    names
}

fn matches_function(requested: &str, symbol: &str, names: &BTreeMap<String, String>) -> bool {
    requested == symbol
        || names.get(symbol).is_some_and(|name| {
            requested.strip_prefix("crate::").unwrap_or(requested)
                == name.strip_prefix("crate::").unwrap_or(name)
        })
}

// Render only values produced by this evaluator, never user-supplied code or a
// JSON approximation of a value. The ordinary checker remains the final gate.
fn sample_source(
    sample: &Sample,
    ty: Type,
    program: &CheckedProgram,
    file: &SourceFile,
) -> std::result::Result<String, String> {
    let nominal = |id: usize| {
        let name = &program.types.definitions[id].name;
        if let Some(local) = name.strip_prefix(&format!("{}::", file.module)) {
            return local.to_owned();
        }
        if ["ReadResult", "WriteResult"].contains(&name.as_str()) {
            return name.clone();
        }
        let own = name
            .strip_prefix(&format!("{}::", file.package))
            .unwrap_or(name);
        format!("crate::{own}")
    };
    Ok(match (sample, ty) {
        (Sample::Int(value), _) if ty.integer().is_some() => {
            if *value > i64::MAX as i128 {
                return Ok(format!(
                    "(u64({}) * u64(2) + u64({}))",
                    value / 2,
                    value % 2
                ));
            }
            // The lexer stores positive integer tokens in u64; spelling the
            // signed minimum as a checked subtraction also works for i64.
            let expression = if *value == i64::MIN as i128 {
                "(-9223372036854775807 - 1)".into()
            } else {
                value.to_string()
            };
            if ty == Type::Int {
                expression
            } else {
                format!("{}({expression})", ty.label())
            }
        }
        (Sample::Float(value), Type::Float(_)) if value.is_finite() => {
            let text = value.to_string();
            let expression = if text.contains(['.', 'e', 'E']) {
                text
            } else {
                format!("{text}.0")
            };
            format!("{}({expression})", ty.label())
        }
        (Sample::Bool(value), Type::Bool) => value.to_string(),
        (Sample::Text(value), Type::Text) => serde_json::to_string(value).unwrap(),
        (Sample::Sequence(values), Type::Ints | Type::Texts | Type::Fixed(..)) => {
            let element = match ty {
                Type::Texts => Type::Text,
                Type::Fixed(n, _) => n.ty(),
                _ => Type::Int,
            };
            if let Type::Fixed(_, n) = ty {
                let Some(first) = values.first() else {
                    return Err("empty fixed witness".into());
                };
                if !values.iter().all(|v| v == first) {
                    return Err("nonuniform fixed witness is unsupported".into());
                }
                format!("[{}; {n}]", sample_source(first, element, program, file)?)
            } else if values.is_empty() && ty == Type::Texts {
                "texts()".into()
            } else {
                format!(
                    "[{}]",
                    values
                        .iter()
                        .map(|v| sample_source(v, element, program, file))
                        .collect::<std::result::Result<Vec<_>, _>>()?
                        .join(", ")
                )
            }
        }
        (Sample::Record(values), Type::Named(id)) => {
            let DataKind::Record(fields) = &program.types.definitions[id].kind else {
                return Err("record witness does not match its checked type".into());
            };
            format!(
                "{} {{ {} }}",
                nominal(id),
                fields
                    .iter()
                    .zip(values)
                    .map(|(field, value)| Ok(format!(
                        "{}: {}",
                        field.name,
                        sample_source(value, field.ty, program, file)?
                    )))
                    .collect::<std::result::Result<Vec<_>, String>>()?
                    .join(", ")
            )
        }
        (Sample::Variant(index, payload), Type::Named(id)) => {
            let DataKind::Enum(variants) = &program.types.definitions[id].kind else {
                return Err("enum witness does not match its checked type".into());
            };
            let variant = &variants[*index];
            let payload = if variant.ty == Type::Unit {
                String::new()
            } else {
                sample_source(payload, variant.ty, program, file)?
            };
            format!("{}::{}({payload})", nominal(id), variant.name)
        }
        (Sample::Sequence(values), Type::Named(id)) if values.is_empty() => {
            match program.types.definitions[id].kind {
                DataKind::Vector(_) | DataKind::SoaVector(_) | DataKind::Map(_) => {
                    format!("{}()", nominal(id))
                }
                _ => return Err("unsupported nominal witness constructor".into()),
            }
        }
        _ => return Err("sample has no supported self-contained source representation".into()),
    })
}

struct TestWitness {
    args: Vec<Sample>,
    result: Option<Sample>,
    fault: Option<Value>,
    status: &'static str,
    case_index: usize,
}

fn test_candidate(
    project: &Project,
    program: &CheckedProgram,
    function: &CheckedFunction,
    name: &str,
    witness: &TestWitness,
) -> Value {
    let located = project.files.iter().enumerate().find_map(|(i, file)| {
        file.parsed.functions.iter().enumerate().find_map(|(j, f)| {
            (crate::project::function_symbol(i, j, &file.module, f) == function.source.name)
                .then_some((file, f))
        })
    });
    let Some((file, raw)) = located else {
        return json!({"function":name,"status":"unavailable","checked":false,"reason":"source declaration unavailable"});
    };
    let rendered = (|| -> std::result::Result<String, String> {
        let args = witness
            .args
            .iter()
            .zip(&function.source.params)
            .map(|(v, p)| sample_source(v, p.ty, program, file))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut lines = Vec::new();
        let mut arguments = Vec::new();
        for (i, (expression, param)) in args.iter().zip(&function.source.params).enumerate() {
            lines.push(format!("    let input_{i} = {expression};"));
            arguments.push(format!(
                "{}input_{i}",
                if param.borrow == Borrow::Shared {
                    "&"
                } else {
                    ""
                }
            ));
        }
        let call_name = if let Some(method) = &raw.method {
            let Type::Named(owner) = method.owner else {
                return Err("unsupported method owner".into());
            };
            format!("{}::{}", file.parsed.type_names[owner], method.name)
        } else {
            raw.name.clone()
        };
        let call = format!("{call_name}({})", arguments.join(", "));
        if function.source.ret == Type::Unit {
            lines.push(format!("    {call};"));
        } else {
            lines.push(format!("    let observed = {call};"));
            if witness.status == "passed" {
                if let Some(value) = &witness.result {
                    if let Sample::Float(value) = value {
                        if value.is_nan() {
                            lines.push("    assert is_nan(observed);".into());
                            return Ok(lines.join("\n"));
                        }
                        if value.is_infinite() {
                            lines.push("    assert !is_finite(observed);".into());
                            lines.push(format!(
                                "    assert observed {} {}(0.0);",
                                if value.is_sign_positive() { ">" } else { "<" },
                                function.source.ret.label()
                            ));
                            return Ok(lines.join("\n"));
                        }
                    }
                    if function.source.ret.integer().is_some()
                        || matches!(function.source.ret, Type::Bool | Type::Float(_))
                    {
                        let expected = sample_source(value, function.source.ret, program, file)?;
                        lines.push(format!("    assert observed == {expected};"));
                    }
                }
            }
        }
        Ok(lines.join("\n"))
    })();
    let body = match rendered {
        Ok(body) => body,
        Err(reason) => {
            return json!({"function":name,"status":"unavailable","checked":false,"reason":reason})
        }
    };
    let expected = match witness.status {
        "counterexample" => "fails_contract",
        "runtime_fault" => "runtime_fault",
        _ => "passes_for_sample",
    };
    let file_key = file.source_key.clone();
    let pin = crate::navigation::source_fingerprint(&file.source);
    let seed = format!("{name}:{pin}:{}:{}", witness.status, witness.case_index);
    let digest = crate::backend::sha256(seed.as_bytes());
    let mut test_name = format!("anode_witness_{}", &digest[..16]);
    // Deterministic collision avoidance; never replace an existing declaration.
    while file.parsed.functions.iter().any(|f| f.name == test_name) {
        test_name.push('_');
    }
    let source = format!("\n// Finite verifier witness for {name}; expected {expected}.\n// This test is checked in memory and has not been executed.\ntest {test_name}() {{\n{body}\n}}\n");
    let mut overlays = BTreeMap::new();
    overlays.insert(file_key.clone(), format!("{}{source}", file.source));
    let checked = project
        .with_overlays(&overlays)
        .and_then(|p| p.checked_reload());
    let position = crate::navigation::NavPosition {
        byte: file.source.len(),
        line: file.source.bytes().filter(|b| *b == b'\n').count() + 1,
        column: file.source.len() - file.source.rfind('\n').map_or(0, |n| n + 1) + 1,
    };
    let mut result = json!({
        "function":name,"name":test_name,"file":file_key,"path":file.path,
        "status":if checked.is_ok(){"checked"}else{"rejected"},"checked":checked.is_ok(),
        "witness_status":witness.status,"case_index":witness.case_index,
        "expected_outcome":expected,"executed":false,"writes":0,"universal_proof":false,
        "source":source,"file_fingerprint":pin,
        "snapshot_fingerprint":crate::navigation::project_fingerprint(project),
        "snapshot_sha256":crate::holes::snapshot_sha256(project),
        "inputs":witness.args.iter().enumerate().map(|(i,v)|json!({"name":function.source.params[i].name,"value":v.json()})).collect::<Vec<_>>(),
        "observed_result":witness.result.as_ref().map(Sample::json),"fault":witness.fault,
        "validation_scope":"ordinary checker, complete project including test bodies; no test execution"
    });
    match checked {
        Ok(_) => {
            result["edits"] = json!([{"file":file_key,"path":file.path,"file_fingerprint":pin,
                "range":{"start":position.to_json(),"end":position.to_json()},"replacement":source}]);
        }
        Err(error) => result["diagnostics"] = json!([crate::agent::diagnostic(&error)]),
    }
    result
}

pub fn verify_contracts(project: &Project, options: &VerificationOptions) -> Result<Value> {
    options.validate()?;
    let started = Instant::now();
    let deadline = started + Duration::from_millis(options.timeout_ms);
    let has_main = project.files.iter().any(|file| {
        file.module.is_empty()
            && file
                .parsed
                .functions
                .iter()
                .any(|f| f.name == "main" && f.method.is_none() && !f.is_test)
    });
    let checked = if has_main {
        project.checked(false).map(|(program, _)| program)
    } else {
        project.checked_reload()
    };
    let program = match checked {
        Ok(program) => program,
        Err(e) => {
            return Ok(
                json!({"checked":false,"status":"invalid","universal_proof":false,"executed":false,"writes":0,"diagnostics":[crate::agent::diagnostic(&e)],"functions":[],"test_candidates":[],"emit_test_requested":options.emit_test,"evidence":"ordinary checker rejected the unchanged project"}),
            )
        }
    };
    let names = function_names(project);
    let effects = crate::effects::analyze(&program);
    let selected = program
        .functions
        .iter()
        .filter(|f| {
            if options.functions.is_empty() {
                !f.source.requires.is_empty()
                    || !f.source.ensures.is_empty()
                    || !f.source.effects.is_empty()
            } else {
                options
                    .functions
                    .iter()
                    .any(|name| matches_function(name, &f.source.name, &names))
            }
        })
        .collect::<Vec<_>>();
    for name in &options.functions {
        if !selected
            .iter()
            .any(|f| matches_function(name, &f.source.name, &names))
        {
            return Err(Error::new(
                0,
                format!("unknown verification function: {name}"),
            ));
        }
    }
    let mut results = Vec::new();
    let mut test_candidates = Vec::new();
    let mut total = 0usize;
    let (mut passed, mut rejected, mut counterexamples, mut faults, mut skipped, mut exhausted) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut no_accepted_inputs = 0usize;
    for f in selected {
        let name = names.get(&f.source.name).unwrap_or(&f.source.name);
        let effect = &effects[&f.source.name];
        let static_result = json!({"effect_contracts":f.source.effects,"requires":f.source.requires.len(),"ensures":f.source.ensures.len(),"effects_checked":true,"may_allocate":effect.alloc,"may_do_io":effect.io});
        let reason = if effect.io {
            Some("IO/process/thread/unknown effects are never executed".into())
        } else {
            supported(&program, &f.source.name, &mut BTreeSet::new()).err()
        };
        if let Some(reason) = reason {
            skipped += 1;
            results.push(json!({"function":name,"status":"skipped","reason":reason,"static":static_result,"cases":[]}));
            continue;
        }
        let domains = match f
            .source
            .params
            .iter()
            .map(|p| samples(p.ty, &program))
            .collect::<EvalResult<Vec<_>>>()
        {
            Ok(v) => v,
            Err(e) => {
                skipped += 1;
                results.push(json!({"function":name,"status":"skipped","reason":e.message,"static":static_result,"cases":[]}));
                continue;
            }
        };
        if !options.execute {
            results.push(json!({"function":name,"status":"planned","static":static_result,"boundary_domains":domains.iter().enumerate().map(|(i,d)|json!({"parameter":f.source.params[i].name,"type":f.source.params[i].ty.label(),"values":d.iter().map(Sample::json).collect::<Vec<_>>()})).collect::<Vec<_>>(),"cases":[]}));
            continue;
        }
        let mut indices = vec![0usize; domains.len()];
        let mut cases = Vec::new();
        let mut complete = false;
        let mut function_failed = false;
        let mut budget = false;
        let mut first_failure = None;
        let mut last_success = None;
        loop {
            if total >= options.max_cases || Instant::now() >= deadline {
                budget = true;
                exhausted += 1;
                break;
            }
            let args = domains
                .iter()
                .zip(&indices)
                .map(|(d, i)| d[*i].clone())
                .collect::<Vec<_>>();
            let inputs=args.iter().enumerate().map(|(i,v)|json!({"name":f.source.params[i].name,"type":f.source.params[i].ty.label(),"value":v.json()})).collect::<Vec<_>>();
            let mut evaluator = Evaluator {
                program: &program,
                steps: 0,
                limit: options.max_steps,
                deadline,
                depth: 0,
                frame_weights: Vec::new(),
            };
            total += 1;
            match evaluator.call(&f.source.name, args.clone()) {
                Ok(value) => {
                    passed += 1;
                    cases.push(json!({"inputs":inputs,"status":"passed","result":value.json(),"steps":evaluator.steps}));
                    if options.emit_test {
                        last_success = Some(TestWitness {
                            args,
                            result: Some(value),
                            fault: None,
                            status: "passed",
                            case_index: cases.len() - 1,
                        });
                    }
                }
                Err(e) => {
                    match e.kind {
                        "precondition_rejected" => rejected += 1,
                        "counterexample" => {
                            counterexamples += 1;
                            function_failed = true;
                        }
                        "runtime_fault" => {
                            faults += 1;
                            function_failed = true;
                        }
                        "budget_exhausted" => {
                            exhausted += 1;
                            budget = true;
                        }
                        _ => skipped += 1,
                    }
                    let diagnostic = project.map_error(Error::new(
                        e.line.unwrap_or(f.source.line),
                        e.message.clone(),
                    ));
                    cases.push(json!({"inputs":inputs,"status":e.kind,"fault":e.json(),"diagnostic":crate::agent::diagnostic(&diagnostic),"steps":evaluator.steps}));
                    if options.emit_test
                        && first_failure.is_none()
                        && matches!(e.kind, "counterexample" | "runtime_fault")
                    {
                        first_failure = Some(TestWitness {
                            args,
                            result: None,
                            fault: Some(e.json()),
                            status: e.kind,
                            case_index: cases.len() - 1,
                        });
                    }
                    if e.kind == "unsupported" || budget {
                        break;
                    }
                }
            }
            if indices.is_empty() {
                complete = true;
                break;
            }
            let mut carry = true;
            for i in (0..indices.len()).rev() {
                indices[i] += 1;
                if indices[i] < domains[i].len() {
                    carry = false;
                    break;
                }
                indices[i] = 0;
            }
            if carry {
                complete = true;
                break;
            }
        }
        let accepted = cases.iter().any(|v| {
            v["status"] == "passed"
                || v["status"] == "counterexample"
                || v["status"] == "runtime_fault"
        });
        if complete && !accepted {
            no_accepted_inputs += 1;
        }
        if options.emit_test {
            if let Some(witness) = first_failure.or(if complete { last_success } else { None }) {
                test_candidates.push(test_candidate(project, &program, f, name, &witness));
            }
        }
        results.push(json!({"function":name,"status":if function_failed{"failed"}else if budget||!complete{"incomplete"}else if !accepted{"no_accepted_inputs"}else{"sampled_pass"},"static":static_result,"boundary_grid_complete":complete,"cases":cases}));
    }
    Ok(
        json!({"checked":true,"status":if counterexamples+faults>0{"failed"}else if !options.execute{"planned"}else if skipped+exhausted+no_accepted_inputs>0||total==0{"incomplete"}else{"sampled"},"strategy":"bounded_checked_ir_evaluator","evidence":"finite deterministic boundary samples; this is not a universal proof or a native-backend equivalence claim","universal_proof":false,"execution_requested":options.execute,"executed":total>0,"executed_user_tests":false,"writes":0,"snapshot_fingerprint":crate::navigation::project_fingerprint(project),"snapshot_sha256":crate::holes::snapshot_sha256(project),"file_fingerprints":project.files.iter().map(|f|(f.source_key.clone(),crate::navigation::source_fingerprint(&f.source))).collect::<BTreeMap<_,_>>(),"coordinate_encoding":crate::navigation::COORDINATE_ENCODING,"emit_test_requested":options.emit_test,"test_candidates":test_candidates,"budgets":{"max_cases":options.max_cases,"max_steps_per_case":options.max_steps,"timeout_ms":options.timeout_ms},"totals":{"executed_cases":total,"passed":passed,"precondition_rejected":rejected,"counterexamples":counterexamples,"runtime_faults":faults,"skipped":skipped,"budget_exhausted":exhausted,"no_accepted_inputs":no_accepted_inputs},"functions":results,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0}),
    )
}

#[cfg(test)]
mod v028_tests {
    use super::*;
    use crate::numeric::Integer;

    #[test]
    fn integer_intrinsics_keep_exact_width_edges_and_mask_every_count() {
        for number in [
            Integer::I8,
            Integer::I16,
            Integer::I32,
            Integer::I64,
            Integer::U8,
            Integer::U16,
            Integer::U32,
            Integer::U64,
        ] {
            let ty = number.ty();
            let (min, max) = bounds(ty).unwrap();
            let eval = |name, a, b| {
                integer_intrinsic(name, &[Sample::Int(a), Sample::Int(b)], ty)
                    .unwrap()
                    .int()
                    .unwrap()
            };
            assert_eq!(
                eval("wrapping_add", max, 1),
                if number.signed() { min } else { 0 }
            );
            assert_eq!(eval("wrapping_sub", min, 1), max);
            assert_eq!(
                eval("wrapping_mul", max, 2),
                if number.signed() { -2 } else { max - 1 }
            );
            assert_eq!(
                integer_intrinsic("bit_not", &[Sample::Int(0)], ty)
                    .unwrap()
                    .int()
                    .unwrap(),
                if number.signed() { -1 } else { max }
            );
            let top = if number.signed() {
                min
            } else {
                1i128 << (number.bits() - 1)
            };
            assert_eq!(eval("rotl", 1, -1), top);
            assert_eq!(eval("rotr", 1, 1), top);
            assert_eq!(eval("shl", 1, number.bits() as i128), 1);
            assert_eq!(eval("shr", top, -1), if number.signed() { -1 } else { 1 });
            assert_eq!(eval("bit_and", max, 1), 1);
            assert_eq!(eval("bit_xor", max, max), 0);
        }
    }

    #[test]
    fn modular_contracts_produce_checked_zero_write_finite_witnesses() {
        let source = "fn mix(x:u8)->u8 ensures result==x;{return rotr(rotl(x,-1),-1);}fn main(){}";
        let project = Project::from_source(source).unwrap();
        let options = VerificationOptions {
            execute: true,
            emit_test: true,
            functions: vec!["mix".into()],
            ..Default::default()
        };
        let report = verify_contracts(&project, &options).unwrap();
        assert_eq!(report["status"], "sampled", "{report}");
        assert_eq!(report["writes"], 0);
        assert_eq!(report["universal_proof"], false);
        assert_eq!(report["test_candidates"][0]["checked"], true, "{report}");
        assert_eq!(project.files[0].source, source);
        for source in [
            "fn mix(x:u8)->u8 ensures result==x;{return wrapping_add(x,u8(1));}fn main(){}",
            "fn mix(x:i8)->i8 ensures result==x;{return x+i8(1);}fn main(){}",
        ] {
            let report =
                verify_contracts(&Project::from_source(source).unwrap(), &options).unwrap();
            assert_eq!(report["status"], "failed", "{report}");
            assert_eq!(report["totals"]["skipped"], 0);
        }
    }

    #[test]
    fn packet_endianness_and_faults_are_evaluated_without_native_or_io_calls() {
        let options = VerificationOptions {
            execute: true,
            emit_test: true,
            functions: vec!["packet".into()],
            ..Default::default()
        };
        let source="fn packet()->u32 ensures result==u32(16909060);{let mut bytes=[u8(0);4];set(bytes,0,u8(1));set(bytes,1,u8(2));set(bytes,2,u8(3));set(bytes,3,u8(4));return wire_read_u32_be(bytes,0);}fn main(){}";
        let report = verify_contracts(&Project::from_source(source).unwrap(), &options).unwrap();
        assert_eq!(report["status"], "sampled", "{report}");
        assert_eq!(report["test_candidates"][0]["checked"], true);
        let bad = source.replace("be(bytes,0)", "be(bytes,1)");
        let report = verify_contracts(&Project::from_source(&bad).unwrap(), &options).unwrap();
        assert_eq!(report["status"], "failed", "{report}");
        assert!(report["totals"]["runtime_faults"].as_u64().unwrap() > 0);
        assert!(report.to_string().contains("packet read out of bounds"));
    }
}
