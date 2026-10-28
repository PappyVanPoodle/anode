//! Fixed-width integers. Registers carry canonical 64-bit values; storage uses
//! the declared width. Conversions and arithmetic never silently wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Integer {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Float {
    F32,
    F64,
}
impl Float {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "f32" => Some(Self::F32),
            "f64" => Some(Self::F64),
            _ => None,
        }
    }
    pub fn bits(self) -> usize {
        if self == Self::F32 {
            32
        } else {
            64
        }
    }
    pub fn name(self) -> &'static str {
        if self == Self::F32 {
            "f32"
        } else {
            "f64"
        }
    }
    pub fn c(self) -> &'static str {
        if self == Self::F32 {
            "float"
        } else {
            "double"
        }
    }
    pub fn code(self) -> i64 {
        200 + self.bits() as i64
    }
}
impl Integer {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "int" | "i64" => Self::I64,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
        }
    }
    pub fn bits(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::I64 | Self::U64 => 64,
        }
    }
    pub fn signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }
    pub fn code(self) -> i64 {
        self.bits() as i64 + if self.signed() { 0 } else { 100 }
    }
    pub fn c(self) -> String {
        format!(
            "{}int{}_t",
            if self.signed() { "" } else { "u" },
            self.bits()
        )
    }
    pub fn ty(self) -> crate::syntax::Type {
        if self == Self::I64 {
            crate::syntax::Type::Int
        } else {
            crate::syntax::Type::Number(self)
        }
    }
}
pub fn opcode(s: &str) -> i64 {
    match s {
        "+" => 0,
        "-" => 1,
        "*" | "*literal" => 2,
        "/" => 3,
        "%" => 4,
        "saturating_add" => 5,
        "saturating_sub" => 6,
        _ => unreachable!(),
    }
}

pub fn type_code(ty: crate::syntax::Type) -> i64 {
    match ty {
        crate::syntax::Type::Float(n) => n.code(),
        _ => ty.integer().unwrap().code(),
    }
}
