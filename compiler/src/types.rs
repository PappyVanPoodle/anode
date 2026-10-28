//! Validated finite data layouts. Recursive owned-by-value layouts are rejected.
use crate::{
    syntax::{DataDecl, DataKind, Type},
    Error, Result,
};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Hash)]
pub struct Layout {
    pub size: usize,
    pub align: usize,
    pub offsets: Vec<usize>,
}
#[derive(Clone, Debug)]
pub(crate) struct MapEntryLayout {
    pub key: Type,
    pub value: Type,
    pub key_code: i64,
    pub value_offset: usize,
    pub width: usize,
}
#[derive(Clone, Debug, Default, Hash)]
pub struct TypeTable {
    pub(crate) definitions: Vec<DataDecl>,
    pub(crate) layouts: Vec<Layout>,
    pub(crate) order: Vec<usize>,
}
impl TypeTable {
    pub fn new(definitions: Vec<DataDecl>) -> Result<Self> {
        if definitions.len() > 256 {
            return Err(Error::new(0, "maximum 256 data types"));
        }
        let mut out = Self {
            layouts: vec![
                Layout {
                    size: 0,
                    align: 1,
                    offsets: Vec::new()
                };
                definitions.len()
            ],
            definitions,
            order: Vec::new(),
        };
        let mut seen = BTreeSet::new();
        for d in &out.definitions {
            if !seen.insert(d.name.clone()) {
                return Err(Error::new(d.line, "duplicate data type"));
            }
            let fields = match &d.kind {
                DataKind::Record(f)
                | DataKind::Enum(f)
                | DataKind::Vector(f)
                | DataKind::SoaVector(f)
                | DataKind::Map(f)
                | DataKind::Arena(f)
                | DataKind::Channel(f) => f,
            };
            let mut names = BTreeSet::new();
            if fields.len() > 256 {
                return Err(Error::new(d.line, "maximum 256 fields or variants"));
            }
            for f in fields {
                out.validate_type(f.ty, d.line)?;
                if f.ty.task_result().is_some() {
                    return Err(Error::new(
                        d.line,
                        "linear task handles cannot be stored in data types",
                    ));
                }
                if f.ty.scoped_view() {
                    return Err(Error::new(
                        d.line,
                        "scoped views cannot be stored in data types",
                    ));
                }
                if !names.insert(&f.name) {
                    return Err(Error::new(d.line, "duplicate field or variant"));
                }
            }
        }
        let mut state = vec![0; out.definitions.len()];
        for i in 0..state.len() {
            out.resolve(i, &mut state, 0)?;
        }
        Ok(out)
    }
    fn resolve(&mut self, id: usize, state: &mut [u8], depth: usize) -> Result<()> {
        if id >= state.len() {
            return Err(Error::new(0, "unknown data type"));
        }
        if state[id] == 2 {
            return Ok(());
        }
        if state[id] == 1 || depth > 64 {
            return Err(Error::new(
                self.definitions[id].line,
                "recursive by-value data type; use a finite owned layout",
            ));
        }
        state[id] = 1;
        let d = self.definitions[id].clone();
        if let DataKind::Map(fields) = &d.kind {
            if fields.len() != 2
                || !(fields[0].ty.integer().is_some()
                    || fields[0].ty == Type::Text
                    || matches!(fields[0].ty, Type::Id(_)))
                || fields[1].ty == Type::Unit
                || fields[1].ty.task_result().is_some()
                || fields[1].ty.scoped_view()
            {
                return Err(Error::new(
                    d.line,
                    "maps require an integer/text key and an owned value type",
                ));
            }
            for field in fields {
                if let Type::Named(child) = field.ty {
                    self.resolve(child, state, depth + 1)?;
                }
            }
            self.layouts[id] = Layout {
                size: 40,
                align: 8,
                offsets: Vec::new(),
            };
            state[id] = 2;
            self.order.push(id);
            return Ok(());
        }
        if let DataKind::Vector(fields)
        | DataKind::SoaVector(fields)
        | DataKind::Arena(fields)
        | DataKind::Channel(fields) = &d.kind
        {
            if fields.len() != 1
                || fields[0].ty == Type::Unit
                || fields[0].ty.task_result().is_some()
                || fields[0].ty.scoped_view()
            {
                return Err(Error::new(d.line, "vectors require one owned element type"));
            }
            if let Type::Named(child) = fields[0].ty {
                self.resolve(child, state, depth + 1)?;
            }
            if matches!(d.kind, DataKind::SoaVector(_))
                && !matches!(fields[0].ty, Type::Named(child) if matches!(self.definitions[child].kind, DataKind::Record(_)))
            {
                return Err(Error::new(
                    d.line,
                    "soa vectors require a named record element",
                ));
            }
            self.layouts[id] = Layout {
                size: if matches!(d.kind, DataKind::Channel(_)) {
                    8
                } else if matches!(d.kind, DataKind::Arena(_)) {
                    48
                } else {
                    24
                },
                align: 8,
                offsets: Vec::new(),
            };
            state[id] = 2;
            self.order.push(id);
            return Ok(());
        }
        let (fields, is_enum) = match d.kind {
            DataKind::Record(f) => (f, false),
            DataKind::Enum(f) => (f, true),
            DataKind::Vector(_)
            | DataKind::SoaVector(_)
            | DataKind::Map(_)
            | DataKind::Arena(_)
            | DataKind::Channel(_) => unreachable!(),
        };
        let mut size = 0usize;
        let mut align = if is_enum { 8 } else { 1 };
        let mut offsets = Vec::new();
        for f in &fields {
            if let Type::Named(child) = f.ty {
                self.resolve(child, state, depth + 1)?;
            }
            let l = self.layout(f.ty);
            align = align.max(l.align);
            if is_enum {
                size = size.max(l.size);
                offsets.push(8);
            } else {
                size = round(size, l.align);
                offsets.push(size);
                size += l.size;
            }
            if size > 65536 {
                return Err(Error::new(d.line, "data layout exceeds 64 KiB"));
            }
        }
        if is_enum {
            size = size.max(1) + 8;
        }
        size = round(size.max(1), align);
        if size > 65536 {
            return Err(Error::new(d.line, "data layout exceeds 64 KiB"));
        }
        self.layouts[id] = Layout {
            size,
            align,
            offsets,
        };
        state[id] = 2;
        self.order.push(id);
        Ok(())
    }
    pub fn layout(&self, ty: Type) -> Layout {
        match ty {
            Type::Float(n) => Layout {
                size: n.bits() / 8,
                align: n.bits() / 8,
                offsets: Vec::new(),
            },
            Type::Number(n) => Layout {
                size: n.bits() / 8,
                align: n.bits() / 8,
                offsets: Vec::new(),
            },
            Type::Fixed(n, len) => Layout {
                size: n.bits() / 8 * len,
                align: n.bits() / 8,
                offsets: Vec::new(),
            },
            Type::Int | Type::Task | Type::TaskOf(_) | Type::Id(_) => Layout {
                size: 8,
                align: 8,
                offsets: Vec::new(),
            },
            Type::Bool => Layout {
                size: 1,
                align: 1,
                offsets: Vec::new(),
            },
            Type::Text | Type::TextView | Type::Slice(_) | Type::VectorView(_) => Layout {
                size: 16,
                align: 8,
                offsets: Vec::new(),
            },
            Type::Ints | Type::Texts => Layout {
                size: 24,
                align: 8,
                offsets: Vec::new(),
            },
            Type::Unit => Layout {
                size: 0,
                align: 1,
                offsets: Vec::new(),
            },
            Type::Named(id) => self.layouts[id].clone(),
        }
    }
    pub(crate) fn vector_element(&self, ty: Type) -> Option<Type> {
        let id = match ty {
            Type::Named(id) | Type::VectorView(id) => id,
            _ => return None,
        };
        match &self.definitions.get(id)?.kind {
            DataKind::Vector(f) => Some(f[0].ty),
            DataKind::SoaVector(f) if !matches!(ty, Type::VectorView(_)) => Some(f[0].ty),
            _ => None,
        }
    }
    pub fn arena_element(&self, ty: Type) -> Option<Type> {
        let Type::Named(id) = ty else { return None };
        match &self.definitions.get(id)?.kind {
            DataKind::Arena(fields) => Some(fields[0].ty),
            _ => None,
        }
    }
    pub fn channel_element(&self, ty: Type) -> Option<Type> {
        let Type::Named(id) = ty else { return None };
        match &self.definitions.get(id)?.kind {
            DataKind::Channel(fields) => Some(fields[0].ty),
            _ => None,
        }
    }
    pub(crate) fn contains_channel(&self, ty: Type) -> bool {
        let Type::Named(id) = ty else { return false };
        match &self.definitions[id].kind {
            DataKind::Channel(_) => true,
            DataKind::Record(f)
            | DataKind::Enum(f)
            | DataKind::Vector(f)
            | DataKind::SoaVector(f)
            | DataKind::Map(f)
            | DataKind::Arena(f) => f.iter().any(|f| self.contains_channel(f.ty)),
        }
    }
    pub(crate) fn task_of(&self, result: Type) -> Option<Type> {
        if self.channel_element(result).is_some() {
            let Type::Named(id) = result else {
                unreachable!()
            };
            Some(Type::TaskOf(crate::syntax::TaskResult::Channel(id)))
        } else {
            Type::task_of(result)
        }
    }
    /// The process runtime writes this exact owned layout, including through
    /// public checker APIs whose callers supply a TypeTable directly.
    pub(crate) fn process_result(&self) -> Option<Type> {
        let output = self.id("ProcessOutput")?;
        let result = self.id("ProcessRun")?;
        let DataKind::Record(fields) = &self.definitions[output].kind else {
            return None;
        };
        if fields.len() != 3
            || fields[0].name != "status"
            || fields[0].ty != Type::Int
            || fields[1].name != "stdout"
            || fields[1].ty != Type::Text
            || fields[2].name != "stderr"
            || fields[2].ty != Type::Text
        {
            return None;
        }
        let DataKind::Enum(fields) = &self.definitions[result].kind else {
            return None;
        };
        if fields.len() != 2
            || fields[0].name != "Output"
            || fields[0].ty != Type::Named(output)
            || fields[1].name != "Error"
            || fields[1].ty != Type::Text
        {
            return None;
        }
        Some(Type::Named(result))
    }
    /// The network runtime writes this exact owned layout (`zn_net_result`),
    /// including through public checker APIs whose callers supply a TypeTable.
    pub(crate) fn net_result(&self) -> Option<Type> {
        let response = self.id("NetResponse")?;
        let result = self.id("NetResult")?;
        let DataKind::Record(fields) = &self.definitions[response].kind else {
            return None;
        };
        if fields.len() != 2
            || fields[0].name != "status"
            || fields[0].ty != Type::Int
            || fields[1].name != "body"
            || fields[1].ty != Type::Text
        {
            return None;
        }
        let DataKind::Enum(fields) = &self.definitions[result].kind else {
            return None;
        };
        if fields.len() != 2
            || fields[0].name != "Response"
            || fields[0].ty != Type::Named(response)
            || fields[1].name != "Error"
            || fields[1].ty != Type::Text
        {
            return None;
        }
        Some(Type::Named(result))
    }
    pub(crate) fn result_variants(&self, ty: Type) -> Option<(Type, Type)> {
        let Type::Named(id) = ty else { return None };
        let DataKind::Enum(fields) = &self.definitions.get(id)?.kind else {
            return None;
        };
        (fields.len() == 2).then(|| (fields[0].ty, fields[1].ty))
    }
    pub(crate) fn validate_type(&self, ty: Type, line: usize) -> Result<()> {
        let result = ty.task_result().unwrap_or(ty);
        match result {
            Type::Id(id)
                if !self
                    .definitions
                    .get(id)
                    .is_some_and(|d| matches!(d.kind, DataKind::Arena(_))) =>
            {
                Err(Error::new(line, "id requires a declared arena type"))
            }
            Type::VectorView(id)
                if !self
                    .definitions
                    .get(id)
                    .is_some_and(|d| matches!(d.kind, DataKind::Vector(_))) =>
            {
                Err(Error::new(
                    line,
                    "vector region type requires a contiguous nominal vector",
                ))
            }
            Type::Named(id) if id >= self.definitions.len() => {
                Err(Error::new(line, "unknown data type"))
            }
            _ => Ok(()),
        }
    }
    pub fn soa_element(&self, ty: Type) -> Option<Type> {
        let Type::Named(id) = ty else {
            return None;
        };
        match &self.definitions[id].kind {
            DataKind::SoaVector(f) => Some(f[0].ty),
            _ => None,
        }
    }
    /// (record offset, column element width), preserving declaration order.
    pub(crate) fn soa_columns(&self, ty: Type) -> Option<Vec<(usize, usize)>> {
        let Type::Named(id) = self.soa_element(ty)? else {
            return None;
        };
        let DataKind::Record(fields) = &self.definitions[id].kind else {
            return None;
        };
        Some(
            fields
                .iter()
                .enumerate()
                .map(|(i, field)| (self.layouts[id].offsets[i], self.layout(field.ty).size))
                .collect(),
        )
    }
    pub(crate) fn map_types(&self, ty: Type) -> Option<(Type, Type)> {
        let Type::Named(id) = ty else { return None };
        match &self.definitions[id].kind {
            DataKind::Map(fields) => Some((fields[0].ty, fields[1].ty)),
            _ => None,
        }
    }
    pub(crate) fn map_entry_layout(&self, ty: Type) -> Option<MapEntryLayout> {
        let (key, value) = self.map_types(ty)?;
        let k = self.layout(key);
        let v = self.layout(value);
        let value_offset = round(k.size, v.align);
        Some(MapEntryLayout {
            key,
            value,
            key_code: if key == Type::Text {
                0
            } else if matches!(key, Type::Id(_)) {
                crate::numeric::Integer::U64.code()
            } else {
                key.integer()?.code()
            },
            value_offset,
            width: round(value_offset + v.size, k.align.max(v.align)),
        })
    }
    pub(crate) fn id(&self, name: &str) -> Option<usize> {
        self.definitions.iter().position(|d| d.name == name)
    }
    pub(crate) fn variant(&self, name: &str) -> Option<(usize, usize, Type)> {
        let (ty, v) = name.rsplit_once("::")?;
        let id = self.id(ty)?;
        let DataKind::Enum(fields) = &self.definitions[id].kind else {
            return None;
        };
        fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == v)
            .map(|(i, f)| (id, i, f.ty))
    }
    pub(crate) fn field(&self, ty: Type, name: &str, line: usize) -> Result<(usize, Type)> {
        let Type::Named(id) = ty else {
            return Err(Error::new(line, "field access requires a record"));
        };
        let DataKind::Record(fields) = &self.definitions[id].kind else {
            let message = match &self.definitions[id].kind {
                DataKind::Map(_) => "map entries require scoped get or clone_get, not field access",
                DataKind::Arena(_) => {
                    "arena elements require scoped get or clone_get, not field access"
                }
                DataKind::Channel(_) => "channel endpoints have no source-visible fields",
                DataKind::Vector(_) | DataKind::SoaVector(_) => {
                    "vector elements require scoped at or clone_at, not field access"
                }
                _ => "enum payloads require an exhaustive match",
            };
            return Err(Error::new(line, message));
        };
        fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)
            .map(|(i, f)| (i, f.ty))
            .ok_or_else(|| Error::new(line, format!("unknown field {name}")))
    }
}
fn round(size: usize, align: usize) -> usize {
    size.div_ceil(align) * align
}

pub(crate) fn builtin_return_type(
    name: &str,
    arg_tys: &[Option<Type>],
    types: &TypeTable,
) -> Option<Type> {
    if crate::syntax::integer_intrinsic(name).is_some() {
        return arg_tys
            .first()
            .copied()
            .flatten()
            .filter(|t| t.integer().is_some());
    }
    if name == "join" {
        return arg_tys.first().copied().flatten()?.task_result();
    }
    if name == "clone_get" {
        if let Some(element) = types.arena_element(arg_tys.first().copied().flatten()?) {
            return Some(element);
        }
        return types
            .map_types(arg_tys.first().copied().flatten()?)
            .map(|(_, value)| value);
    }
    if name == "alloc" {
        let owner = arg_tys.first().copied().flatten()?;
        types.arena_element(owner)?;
        let Type::Named(id) = owner else { return None };
        return Some(Type::Id(id));
    }
    if ["at", "clone_at", "pop"].contains(&name) {
        if let Some(element) = arg_tys
            .first()
            .copied()
            .flatten()
            .and_then(|t| types.vector_element(t))
        {
            return Some(element);
        }
    }
    if let Some(int_ty) = crate::numeric::Integer::parse(name) {
        return Some(int_ty.ty());
    }
    if let Some(float_ty) = crate::numeric::Float::parse(name) {
        return Some(Type::Float(float_ty));
    }
    match name {
        "capacity" | "len" | "args_count" | "monotonic_ms" | "byte_at" | "parse_int"
        | "char_count" | "codepoint_at" | "next_char_boundary" | "text_find" => Some(Type::Int),
        "read_text" | "arg" | "slice" | "concat" | "int_text" | "float_text" | "text_at"
        | "to_text" | "codepoint_text" | "text_trim" | "text_replace" | "text_join"
        | "sha256_text" | "env_var" => Some(Type::Text),
        "process_run" => types.process_result(),
        "net_request" | "net_serve_once" | "http_request" => types.net_result(),
        "try_read_bytes" => Some(Type::Int),
        "fnv1a_64" => Some(Type::Number(crate::numeric::Integer::U64)),
        "try_read_text" => Some(Type::Named(0)),
        "try_write_text" => Some(Type::Named(1)),
        "split" | "texts" | "text_lines" | "list_dir" => Some(Type::Texts),
        "contains_id"
        | "free"
        | "contains_key"
        | "remove"
        | "contains"
        | "text_eq"
        | "is_int"
        | "is_f64"
        | "is_finite"
        | "is_nan"
        | "is_char_boundary"
        | "text_starts_with"
        | "text_ends_with"
        | "try_set"
        | "try_write_u16_le"
        | "try_write_u32_le"
        | "try_write_u64_le"
        | "try_write_u16_be"
        | "try_write_u32_be"
        | "try_write_u64_be"
        | "wire_try_write_u16_be"
        | "wire_try_write_u32_be"
        | "wire_try_write_u64_be"
        | "file_exists"
        | "try_write_bytes"
        | "create_dir"
        | "remove_file"
        | "send"
        | "recv"
        | "try_recv"
        | "is_closed" => Some(Type::Bool),
        "parse_f64" => Some(Type::Float(crate::numeric::Float::F64)),
        "read_u16_le" | "read_u16_be" | "wire_read_u16_be" => {
            Some(Type::Number(crate::numeric::Integer::U16))
        }
        "read_u32_le" | "read_u32_be" | "wire_read_u32_be" => {
            Some(Type::Number(crate::numeric::Integer::U32))
        }
        "read_u64_le" | "read_u64_be" | "wire_read_u64_be" => {
            Some(Type::Number(crate::numeric::Integer::U64))
        }
        "abs" | "sqrt" | "clone" | "saturating_add" | "saturating_sub" => {
            arg_tys.first().copied().flatten()
        }
        "at" => match arg_tys.first().copied().flatten()? {
            Type::Ints => Some(Type::Int),
            Type::Texts => Some(Type::Text),
            Type::Fixed(n, _) | Type::Slice(n) => Some(n.ty()),
            _ => None,
        },
        _ => Some(Type::Unit),
    }
}

#[cfg(test)]
mod map_layout_tests {
    use super::*;
    use crate::{
        numeric::{Float, Integer},
        syntax::FieldDecl,
    };
    fn map(name: &str, key: Type, value: Type) -> DataDecl {
        DataDecl {
            name: name.into(),
            public: true,
            line: 1,
            kind: DataKind::Map(vec![
                FieldDecl {
                    name: "key".into(),
                    ty: key,
                },
                FieldDecl {
                    name: "value".into(),
                    ty: value,
                },
            ]),
        }
    }
    #[test]
    fn map_descriptor_and_entry_layouts_match_natural_value_alignment() {
        let record = DataDecl {
            name: "Envelope".into(),
            public: true,
            line: 1,
            kind: DataKind::Record(vec![
                FieldDecl {
                    name: "tag".into(),
                    ty: Type::Number(Integer::U8),
                },
                FieldDecl {
                    name: "items".into(),
                    ty: Type::Ints,
                },
            ]),
        };
        let types = TypeTable::new(vec![
            record,
            map("TextMap", Type::Text, Type::Named(0)),
            map("Nested", Type::Number(Integer::U64), Type::Named(1)),
            map("Tiny", Type::Number(Integer::U8), Type::Bool),
        ])
        .unwrap();
        for id in 1..4 {
            let layout = types.layout(Type::Named(id));
            assert_eq!((layout.size, layout.align), (40, 8));
            assert!(layout.offsets.is_empty());
        }
        let text = types.map_entry_layout(Type::Named(1)).unwrap();
        assert_eq!((text.key_code, text.value_offset, text.width), (0, 16, 48));
        let nested = types.map_entry_layout(Type::Named(2)).unwrap();
        assert_eq!((nested.value_offset, nested.width), (8, 48));
        let tiny = types.map_entry_layout(Type::Named(3)).unwrap();
        assert_eq!((tiny.value_offset, tiny.width), (1, 2));
        assert_eq!(types.order, [0, 1, 2, 3]);
        assert!(types.field(Type::Named(1), "value", 1).is_err());
    }
    #[test]
    fn integer_map_keys_preserve_the_declared_width_and_runtime_code() {
        for key in [
            Integer::I8,
            Integer::I16,
            Integer::I32,
            Integer::I64,
            Integer::U8,
            Integer::U16,
            Integer::U32,
            Integer::U64,
        ] {
            let types =
                TypeTable::new(vec![map("Entries", key.ty(), Type::Float(Float::F64))]).unwrap();
            let entry = types.map_entry_layout(Type::Named(0)).unwrap();
            assert_eq!(entry.key_code, key.code());
            assert_eq!((entry.value_offset, entry.width), (8, 16));
            assert_eq!(types.layout(entry.key).size, key.bits() / 8);
        }
    }
    #[test]
    fn map_layouts_reject_unsupported_keys_values_and_recursive_cycles() {
        for key in [
            Type::Bool,
            Type::Float(Float::F32),
            Type::Ints,
            Type::TextView,
        ] {
            assert!(TypeTable::new(vec![map("Bad", key, Type::Int)]).is_err());
        }
        for value in [
            Type::Unit,
            Type::Task,
            Type::Slice(Integer::U8),
            Type::TextView,
        ] {
            assert!(TypeTable::new(vec![map("Bad", Type::Text, value)]).is_err());
        }
        assert!(TypeTable::new(vec![map("Cycle", Type::Text, Type::Named(0))]).is_err());
    }
}
