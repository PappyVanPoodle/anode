use crate::{Error, Result};
use std::cell::Cell;

/// Exact-width modular integer operations. The numeric tags are private C
/// emitter opcodes; checked IR retains the source builtin names unchanged.
pub(crate) fn integer_intrinsic(name: &str) -> Option<u8> {
    Some(match name {
        "wrapping_add" => 0,
        "wrapping_sub" => 1,
        "wrapping_mul" => 2,
        "bit_and" => 3,
        "bit_or" => 4,
        "bit_xor" => 5,
        "bit_not" => 6,
        "shl" => 7,
        "shr" => 8,
        "rotl" => 9,
        "rotr" => 10,
        _ => return None,
    })
}

/// Optional, thread-local counters for controlled frontend measurements.
/// These count syntax work and full semantic passes, never accepted results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrontendProfile {
    pub module_parses: u64,
    pub navigation_parses: u64,
    pub cache_budget_fallbacks: u64,
    pub syntax_reuses: u64,
    pub function_clones: u64,
    pub expression_clones: u64,
    pub semantic_checks: u64,
}
thread_local! {
    static FRONTEND_PROFILE: Cell<Option<FrontendProfile>> = const { Cell::new(None) };
}
pub(crate) fn profile_work(update: impl FnOnce(&mut FrontendProfile)) {
    FRONTEND_PROFILE.with(|slot| {
        if let Some(mut profile) = slot.get() {
            update(&mut profile);
            slot.set(Some(profile));
        }
    });
}
/// Measure only the calling thread and restore an enclosing measurement even
/// when the measured operation unwinds. No global state or cache is retained.
pub fn with_frontend_profile<T>(run: impl FnOnce() -> T) -> (T, FrontendProfile) {
    struct Restore(Option<FrontendProfile>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FRONTEND_PROFILE.with(|slot| slot.set(self.0));
        }
    }
    let restore =
        Restore(FRONTEND_PROFILE.with(|slot| slot.replace(Some(FrontendProfile::default()))));
    let result = run();
    let profile = FRONTEND_PROFILE.with(|slot| slot.get().unwrap());
    drop(restore);
    (result, profile)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Type {
    Int,
    Number(crate::numeric::Integer),
    Float(crate::numeric::Float),
    Fixed(crate::numeric::Integer, usize),
    Slice(crate::numeric::Integer),
    /// Nonowning contiguous element span for one nominal vector declaration.
    VectorView(usize),
    Bool,
    Text,
    TextView,
    Ints,
    Texts,
    Task,
    TaskOf(TaskResult),
    Unit,
    Named(usize),
    /// Instance-relative slot/generation handle for one nominal arena type.
    Id(usize),
}
/// Finite, nonrecursive task result metadata keeps `Type` small and Copy.
/// `Task` retains the original int-handle spelling for API compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskResult {
    Number(crate::numeric::Integer),
    Float(crate::numeric::Float),
    Fixed(crate::numeric::Integer, usize),
    Bool,
    Text,
    Ints,
    Texts,
    Unit,
    Named(usize),
    Id(usize),
    Channel(usize),
}
impl Type {
    pub fn task_of(result: Type) -> Option<Self> {
        Some(Self::TaskOf(match result {
            Self::Int => return Some(Self::Task),
            Self::Number(n) => TaskResult::Number(n),
            Self::Float(n) => TaskResult::Float(n),
            Self::Fixed(n, len) => TaskResult::Fixed(n, len),
            Self::Bool => TaskResult::Bool,
            Self::Text => TaskResult::Text,
            Self::Ints => TaskResult::Ints,
            Self::Texts => TaskResult::Texts,
            Self::Unit => TaskResult::Unit,
            Self::Named(id) => TaskResult::Named(id),
            Self::Id(id) => TaskResult::Id(id),
            Self::Task
            | Self::TaskOf(_)
            | Self::TextView
            | Self::Slice(_)
            | Self::VectorView(_) => return None,
        }))
    }
    pub fn task_result(self) -> Option<Self> {
        Some(match self {
            Self::Task => Self::Int,
            Self::TaskOf(result) => match result {
                TaskResult::Number(n) => Self::Number(n),
                TaskResult::Float(n) => Self::Float(n),
                TaskResult::Fixed(n, len) => Self::Fixed(n, len),
                TaskResult::Bool => Self::Bool,
                TaskResult::Text => Self::Text,
                TaskResult::Ints => Self::Ints,
                TaskResult::Texts => Self::Texts,
                TaskResult::Unit => Self::Unit,
                TaskResult::Named(id) => Self::Named(id),
                TaskResult::Id(id) => Self::Id(id),
                TaskResult::Channel(id) => Self::Named(id),
            },
            _ => return None,
        })
    }
    pub fn label(self) -> String {
        match self {
            Self::Number(n) => n.name().into(),
            Self::Float(n) => n.name().into(),
            Self::Fixed(n, len) => format!("[{}; {len}]", n.name()),
            Self::TextView => "str".into(),
            Self::Slice(n) => format!("[{}]", n.name()),
            Self::VectorView(id) => format!("[vector {id}]"),
            Self::Id(id) => format!("id({id})"),
            Self::Task | Self::TaskOf(_) => {
                format!("task<{}>", self.task_result().unwrap().label())
            }
            _ => format!("{self:?}").to_lowercase(),
        }
    }
    pub fn integer(self) -> Option<crate::numeric::Integer> {
        match self {
            Self::Int => Some(crate::numeric::Integer::I64),
            Self::Number(n) => Some(n),
            _ => None,
        }
    }
    pub fn scoped_view(self) -> bool {
        matches!(self, Self::Slice(_) | Self::TextView | Self::VectorView(_))
    }
    pub fn owned(self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::TextView
                | Self::Ints
                | Self::Texts
                | Self::Task
                | Self::TaskOf(_)
                | Self::Named(_)
                | Self::Fixed(..)
                | Self::Slice(_)
                | Self::VectorView(_)
        )
    }
    pub fn c(self) -> String {
        match self {
            Self::Int => "int64_t",
            Self::Number(n) => return n.c(),
            Self::Float(n) => n.c(),
            Self::Fixed(n, len) => return format!("zn_fixed_{}_{len}", n.name()),
            Self::Slice(_) | Self::VectorView(_) => "zn_span",
            Self::Bool => "bool",
            Self::Text | Self::TextView => "zn_text",
            Self::Ints => "zn_ints",
            Self::Texts => "zn_texts",
            Self::Task | Self::TaskOf(_) => "zn_task*",
            Self::Unit => "void",
            Self::Named(id) => return format!("zn_type_{id}"),
            Self::Id(_) => "uint64_t",
        }
        .into()
    }
    pub fn zero(self) -> String {
        match self {
            Self::Text | Self::TextView => "(zn_text){0}",
            Self::Ints => "(zn_ints){0}",
            Self::Texts => "(zn_texts){0}",
            Self::Task | Self::TaskOf(_) => "NULL",
            Self::Named(id) => return format!("(zn_type_{id}){{0}}"),
            Self::Fixed(..) => return format!("({}){{0}}", self.c()),
            Self::Slice(_) | Self::VectorView(_) => "(zn_span){0}",
            _ => "0",
        }
        .into()
    }
    pub fn drop_fn(self) -> String {
        match self {
            Self::Text => "zn_text_drop",
            Self::TextView => "zn_text_view_drop",
            Self::Ints => "zn_ints_drop",
            Self::Texts => "zn_texts_drop",
            Self::Task | Self::TaskOf(_) => "zn_task_drop",
            Self::Named(id) => return format!("zn_type_{id}_drop"),
            Self::Fixed(..) => return format!("{}_drop", self.c()),
            Self::Slice(_) | Self::VectorView(_) => "zn_span_drop",
            _ => unreachable!(),
        }
        .into()
    }
}
#[derive(Debug, Hash)]
pub struct Expr {
    pub line: usize,
    pub kind: ExprKind,
}
impl Clone for Expr {
    fn clone(&self) -> Self {
        profile_work(|profile| profile.expression_clones += 1);
        Self {
            line: self.line,
            kind: self.kind.clone(),
        }
    }
}
#[derive(Clone, Debug, Hash)]
pub enum ExprKind {
    Int(i64),
    Float(u64),
    Bool(bool),
    Text(String),
    Hole(Type, String),
    Var(String),
    Borrow(String, bool),
    Array(Vec<Expr>),
    Repeat(Box<Expr>, usize),
    Call(String, Vec<Expr>),
    MethodCall(Box<Expr>, String, Vec<Expr>),
    Spawn(String, Vec<Expr>),
    Try(Box<Expr>),
    Unary(String, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Field(Box<Expr>, String),
    Record(String, Vec<(String, Expr)>),
}
#[derive(Debug, Hash)]
pub struct Stmt {
    pub line: usize,
    pub kind: StmtKind,
}
impl Clone for Stmt {
    fn clone(&self) -> Self {
        if matches!(self.kind, StmtKind::If(..)) {
            return clone_conditional(self);
        }
        Self {
            line: self.line,
            kind: self.kind.clone(),
        }
    }
}
fn clone_conditional(first: &Stmt) -> Stmt {
    // A derived StmtKind::clone reserves space for every variant, including
    // several expressions in a paired borrow. One such frame per else-if can
    // exhaust a Windows main thread's stack while cloning an otherwise valid
    // source tree. Keep the original nested AST, but clone its false spine in
    // one frame, just as parsing and checking already do.
    let mut branches = Vec::new();
    let mut cursor = first;
    let mut tail = loop {
        let StmtKind::If(condition, yes, no) = &cursor.kind else {
            unreachable!()
        };
        branches.push((cursor.line, condition.clone(), yes.clone()));
        if let [next] = no.as_slice() {
            if matches!(next.kind, StmtKind::If(..)) {
                cursor = next;
                continue;
            }
        }
        break no.clone();
    };
    for (line, condition, yes) in branches.into_iter().rev() {
        tail = vec![Stmt {
            line,
            kind: StmtKind::If(condition, yes, tail),
        }];
    }
    tail.pop().expect("conditional has at least one branch")
}
#[derive(Clone, Debug, Hash)]
pub enum StmtKind {
    ElementPair {
        names: [String; 2],
        owner: Expr,
        indices: [Expr; 2],
        body: Vec<Stmt>,
    },
    ForRange {
        name: String,
        start: Expr,
        end: Expr,
        body: Vec<Stmt>,
    },
    ForEach {
        name: String,
        mutable: bool,
        owner: Expr,
        body: Vec<Stmt>,
    },
    Element {
        access: ElementAccess,
        name: String,
        mutable: bool,
        owner: Expr,
        index: Expr,
        body: Vec<Stmt>,
    },
    Region {
        name: String,
        mutable: bool,
        owner: Expr,
        start: Expr,
        end: Expr,
        body: Vec<Stmt>,
    },
    Let(String, bool, Expr),
    Assign(String, Expr),
    Expr(Expr),
    Print(Expr),
    Assert(Expr),
    Return(Option<Expr>),
    Break,
    Continue,
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    AssignField(Expr, Expr),
    Match(Expr, Vec<MatchArm>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElementAccess {
    Index,
    Key,
}
#[derive(Clone, Debug, Hash)]
pub struct MatchArm {
    pub path: String,
    pub binding: Option<String>,
    pub body: Vec<Stmt>,
    pub line: usize,
}
#[derive(Clone, Debug, Hash)]
pub struct FieldDecl {
    pub name: String,
    pub ty: Type,
}
#[derive(Clone, Debug, Hash)]
pub enum DataKind {
    Record(Vec<FieldDecl>),
    Enum(Vec<FieldDecl>),
    Vector(Vec<FieldDecl>),
    /// A vector of records stored in contiguous, independently padded columns.
    SoaVector(Vec<FieldDecl>),
    /// Exactly two synthetic fields, named key and value. Opaque to source
    /// field projections; represented as an owning hash-table descriptor.
    Map(Vec<FieldDecl>),
    /// Exactly one synthetic element field; owning generational slot storage.
    Arena(Vec<FieldDecl>),
    /// One synthetic element field; an owning endpoint of a shared bounded FIFO.
    Channel(Vec<FieldDecl>),
}
#[derive(Clone, Debug, Hash)]
pub struct DataDecl {
    pub name: String,
    pub public: bool,
    pub kind: DataKind,
    pub line: usize,
}
#[derive(Clone, Debug, Hash)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
    pub borrow: Borrow,
    pub view: Option<Vec<ViewField>>,
}
#[derive(Clone, Debug, Hash)]
pub struct ViewField {
    pub path: Vec<String>,
    pub mutable: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Borrow {
    Owned,
    Shared,
    Exclusive,
}
impl Param {
    pub fn c_type(&self) -> String {
        match self.borrow {
            Borrow::Owned => self.ty.c(),
            Borrow::Shared => format!("const {}*", self.ty.c()),
            Borrow::Exclusive => format!("{}*", self.ty.c()),
        }
    }
}
#[derive(Debug, Hash)]
pub struct Function {
    /// Unmangled C symbol. Module remapping changes `name`, never this symbol.
    pub external_symbol: Option<String>,
    pub method: Option<Method>,
    pub module: String,
    pub effects: Vec<String>,
    pub intents: Vec<String>,
    pub public: bool,
    pub is_test: bool,
    pub source_offset: usize,
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub body: Vec<Stmt>,
    pub requires: Vec<Expr>,
    pub ensures: Vec<Expr>,
    pub line: usize,
}
impl Clone for Function {
    fn clone(&self) -> Self {
        profile_work(|profile| profile.function_clones += 1);
        Self {
            external_symbol: self.external_symbol.clone(),
            method: self.method.clone(),
            module: self.module.clone(),
            effects: self.effects.clone(),
            intents: self.intents.clone(),
            public: self.public,
            is_test: self.is_test,
            source_offset: self.source_offset,
            name: self.name.clone(),
            params: self.params.clone(),
            ret: self.ret,
            body: self.body.clone(),
            requires: self.requires.clone(),
            ensures: self.ensures.clone(),
            line: self.line,
        }
    }
}
#[derive(Clone, Debug, Hash)]
pub struct Method {
    pub owner: Type,
    pub name: String,
}
#[derive(Clone, Debug, Hash)]
pub struct Implementation {
    pub owner: Type,
    pub line: usize,
}
#[derive(Clone, Debug, Hash)]
pub struct ModuleDecl {
    pub name: String,
    pub line: usize,
}
#[derive(Clone, Debug, Hash)]
pub struct Module {
    pub implementations: Vec<Implementation>,
    pub functions: Vec<Function>,
    pub modules: Vec<ModuleDecl>,
    pub types: Vec<DataDecl>,
    pub type_names: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Hash)]
enum Kind {
    Word(String),
    Number(i64),
    Float(u64),
    Text(String),
    Symbol(String),
    End,
}
#[derive(Clone, Debug, Hash)]
struct Token {
    kind: Kind,
    line: usize,
}
fn lex(source: &str) -> Result<Vec<Token>> {
    if source.len() > crate::project::SOURCE_FILE_LIMIT {
        return Err(Error::new(1, "source file limit is 16 MiB"));
    }
    let mut it = source.chars().peekable();
    let mut out = Vec::new();
    let mut line = 1;
    while let Some(ch) = it.next() {
        if ch == '\n' {
            line += 1;
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        if ch == '/' && it.peek() == Some(&'/') {
            for c in it.by_ref() {
                if c == '\n' {
                    line += 1;
                    break;
                }
            }
            continue;
        }
        let start = line;
        let kind = if ch.is_ascii_alphabetic() || ch == '_' {
            let mut s = ch.to_string();
            while it
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                s.push(it.next().unwrap());
            }
            Kind::Word(s)
        } else if ch.is_ascii_digit() {
            let mut s = ch.to_string();
            while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                s.push(it.next().unwrap());
            }
            let mut floating = false;
            if it.peek() == Some(&'.') && it.clone().nth(1).is_some_and(|c| c.is_ascii_digit()) {
                floating = true;
                s.push(it.next().unwrap());
                while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                    s.push(it.next().unwrap());
                }
            }
            if it.peek().is_some_and(|c| matches!(c, 'e' | 'E')) {
                floating = true;
                s.push(it.next().unwrap());
                if it.peek().is_some_and(|c| matches!(c, '+' | '-')) {
                    s.push(it.next().unwrap());
                }
                if !it.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return Err(Error::new(line, "exponent requires digits"));
                }
                while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                    s.push(it.next().unwrap());
                }
            }
            if floating {
                let value = s
                    .parse::<f64>()
                    .map_err(|_| Error::new(line, "invalid floating literal"))?;
                if !value.is_finite() {
                    return Err(Error::new(
                        line,
                        "floating literal exceeds finite f64 range",
                    ));
                }
                Kind::Float(value.to_bits())
            } else {
                Kind::Number(
                    s.parse().map_err(|_| {
                        Error::new(line, "integer literal exceeds signed 64-bit range")
                    })?,
                )
            }
        } else if ch == '"' {
            let mut s = String::new();
            let mut closed = false;
            while let Some(c) = it.next() {
                if c == '"' {
                    closed = true;
                    break;
                }
                let c = if c == '\\' {
                    match it.next() {
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('"') => '"',
                        Some('\\') => '\\',
                        _ => return Err(Error::new(line, "unsupported string escape")),
                    }
                } else {
                    c
                };
                if c == '\0' {
                    return Err(Error::new(line, "NUL is not supported inside text"));
                }
                if c == '\n' {
                    line += 1;
                }
                s.push(c);
            }
            if !closed {
                return Err(Error::new(start, "unclosed text literal"));
            }
            Kind::Text(s)
        } else {
            let mut s = ch.to_string();
            if let Some(c) = it.peek() {
                let pair = format!("{ch}{c}");
                if [
                    "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "::", "??", "..",
                ]
                .contains(&pair.as_str())
                {
                    it.next();
                    s = pair;
                }
            }
            if ![
                "(", ")", "{", "}", "[", "]", ",", ":", ";", "+", "-", "*", "/", "%", "=", "!",
                "<", ">", "->", "=>", ".", "==", "!=", "<=", ">=", "&&", "||", "&", "::", "@",
                "??", "..",
            ]
            .contains(&s.as_str())
            {
                return Err(Error::new(line, format!("unsupported token {s:?}; raw pointers and references are not in this subset")));
            }
            Kind::Symbol(s)
        };
        out.push(Token { kind, line: start });
    }
    out.push(Token {
        kind: Kind::End,
        line,
    });
    Ok(out)
}
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
    type_names: Vec<String>,
    impl_owner: Option<usize>,
}
impl Parser {
    fn line(&self) -> usize {
        self.tokens[self.pos].line
    }
    fn at(&self, s: &str) -> bool {
        matches!(&self.tokens[self.pos].kind, Kind::Word(x) | Kind::Symbol(x) if x == s)
    }
    fn eat(&mut self, s: &str) -> bool {
        if self.at(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, s: &str) -> Result<()> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(Error::new(self.line(), format!("expected {s:?}")))
        }
    }
    fn name(&mut self) -> Result<String> {
        if let Kind::Word(n) = &self.tokens[self.pos].kind {
            let n = n.clone();
            self.pos += 1;
            if [
                "fn", "let", "mut", "return", "break", "continue", "if", "else", "while", "true",
                "false", "spawn", "requires", "ensures", "assert", "print", "result", "mod", "pub",
                "test", "crate", "struct", "enum", "vector", "soa", "extern", "map", "match",
                "impl", "for", "in", "try", "channel",
            ]
            .contains(&n.as_str())
            {
                return Err(Error::new(self.line(), "reserved identifier"));
            }
            Ok(n)
        } else {
            Err(Error::new(self.line(), "expected identifier"))
        }
    }
    fn ty(&mut self) -> Result<Type> {
        if self.eat("[") {
            let element = self.path()?;
            let Some(n) = crate::numeric::Integer::parse(&element) else {
                self.need("]")?;
                let id = if let Some(id) = self.type_names.iter().position(|s| s == &element) {
                    id
                } else {
                    self.type_names.push(element);
                    self.type_names.len() - 1
                };
                return Ok(Type::VectorView(id));
            };
            if self.eat("]") {
                return Ok(Type::Slice(n));
            }
            self.need(";")?;
            let len = self.capacity()?;
            self.need("]")?;
            if len > 65536 / (n.bits() / 8) {
                return Err(Error::new(self.line(), "fixed array exceeds 64 KiB"));
            }
            return Ok(Type::Fixed(n, len));
        }
        let n = self.path()?;
        if n == "id" {
            self.need("(")?;
            let Type::Named(id) = self.ty()? else {
                return Err(Error::new(self.line(), "id requires a nominal arena name"));
            };
            self.need(")")?;
            return Ok(Type::Id(id));
        }
        if let Some(n) = crate::numeric::Integer::parse(&n) {
            return Ok(n.ty());
        }
        if let Some(n) = crate::numeric::Float::parse(&n) {
            return Ok(Type::Float(n));
        }
        match n.as_str() {
            "int" => Ok(Type::Int),
            "bool" => Ok(Type::Bool),
            "text" => Ok(Type::Text),
            "str" => Ok(Type::TextView),
            "ints" => Ok(Type::Ints),
            "texts" => Ok(Type::Texts),
            _ => {
                let index = if let Some(i) = self.type_names.iter().position(|s| s == &n) {
                    i
                } else {
                    self.type_names.push(n);
                    self.type_names.len() - 1
                };
                Ok(Type::Named(index))
            }
        }
    }
    fn capacity(&mut self) -> Result<usize> {
        if let Kind::Number(n) = self.tokens[self.pos].kind {
            if n > 0 && n <= 65536 {
                self.pos += 1;
                return Ok(n as usize);
            }
        }
        Err(Error::new(
            self.line(),
            "fixed capacity must be a literal integer from 1 to 65536",
        ))
    }
    fn path(&mut self) -> Result<String> {
        let mut name = if self.eat("crate") {
            "crate".into()
        } else {
            self.name()?
        };
        while self.eat("::") {
            name.push_str("::");
            name.push_str(&self.name()?);
        }
        if let Some(owner) = self.impl_owner {
            if name == "Self" {
                name = self.type_names[owner].clone();
            } else if let Some(tail) = name.strip_prefix("Self::") {
                name = format!("{}::{tail}", self.type_names[owner]);
            }
        }
        Ok(name)
    }
    fn function(&mut self, public: bool, is_test: bool, external: bool) -> Result<Function> {
        let line = self.line();
        self.need(if is_test { "test" } else { "fn" })?;
        let name = self.name()?;
        self.need("(")?;
        let mut params = Vec::new();
        if !self.at(")") {
            loop {
                let mutable = self.eat("mut");
                let name = self.name()?;
                self.need(":")?;
                let borrow = if self.eat("&") {
                    if self.eat("mut") {
                        Borrow::Exclusive
                    } else {
                        Borrow::Shared
                    }
                } else {
                    Borrow::Owned
                };
                let ty = self.ty()?;
                if ty.scoped_view() && borrow == Borrow::Owned {
                    return Err(Error::new(
                        line,
                        "slice parameters require & or &mut; str requires &str; scoped views cannot be owned",
                    ));
                }
                let view = if self.eat(".") {
                    if borrow == Borrow::Owned || !matches!(ty, Type::Named(_)) {
                        return Err(Error::new(
                            line,
                            "view contracts require a borrowed record parameter",
                        ));
                    }
                    self.need("{")?;
                    let mut fields = Vec::new();
                    loop {
                        let mutable = self.eat("mut");
                        if mutable && borrow != Borrow::Exclusive {
                            return Err(Error::new(
                                line,
                                "mutable view field requires an &mut parameter",
                            ));
                        }
                        let mut path = vec![self.name()?];
                        while self.eat(".") {
                            path.push(self.name()?);
                        }
                        fields.push(ViewField { path, mutable });
                        if !self.eat(",") {
                            break;
                        }
                    }
                    self.need("}")?;
                    Some(fields)
                } else {
                    None
                };
                if borrow != Borrow::Owned && !ty.owned() {
                    return Err(Error::new(
                        line,
                        "borrowed parameters support ints and text",
                    ));
                }
                if borrow != Borrow::Owned && mutable {
                    return Err(Error::new(line, "use name: &mut ints, not mut name: &ints"));
                }
                if borrow == Borrow::Exclusive
                    && !matches!(
                        ty,
                        Type::Ints
                            | Type::Texts
                            | Type::Named(_)
                            | Type::Fixed(..)
                            | Type::Slice(_)
                            | Type::VectorView(_)
                    )
                {
                    return Err(Error::new(
                        line,
                        "exclusive borrowing currently supports ints",
                    ));
                }
                params.push(Param {
                    name,
                    ty,
                    mutable,
                    borrow,
                    view,
                });
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.need(")")?;
        let ret = if self.eat("->") {
            self.ty()?
        } else {
            Type::Unit
        };
        if ret.scoped_view() {
            return Err(Error::new(
                line,
                "scoped slices cannot be returned; str views cannot escape either",
            ));
        }
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            if self.eat("requires") {
                requires.push(self.expr(0)?);
                self.need(";")?;
            } else if self.eat("ensures") {
                ensures.push(self.expr(0)?);
                self.need(";")?;
            } else {
                break;
            }
        }
        let body = if external {
            self.need(";")?;
            Vec::new()
        } else {
            self.block()?
        };
        if external
            && (is_test
                || self.impl_owner.is_some()
                || name == "main"
                || !requires.is_empty()
                || !ensures.is_empty())
        {
            return Err(Error::new(
                line,
                "extern functions cannot be tests, methods, main or carry source contracts",
            ));
        }
        if is_test && (public || !params.is_empty() || ret != Type::Unit) {
            return Err(Error::new(
                line,
                "tests must be private, take no arguments and return no value",
            ));
        }
        let method = if let Some(owner) = self.impl_owner {
            if is_test || ["self", "Self"].contains(&name.as_str()) {
                return Err(Error::new(
                    line,
                    "impl blocks contain named methods, not tests or receiver names",
                ));
            }
            if !params
                .first()
                .is_some_and(|p| p.name == "self" && p.ty == Type::Named(owner))
            {
                return Err(Error::new(
                    line,
                    "method requires first parameter self: Self, self: &Self or self: &mut Self",
                ));
            }
            Some(Method {
                owner: Type::Named(owner),
                name: name.clone(),
            })
        } else {
            None
        };
        let name = if let Some(owner) = self.impl_owner {
            format!("{}::{name}", self.type_names[owner])
        } else {
            name
        };
        Ok(Function {
            external_symbol: if external { Some(name.clone()) } else { None },
            method,
            module: String::new(),
            effects: Vec::new(),
            intents: Vec::new(),
            public,
            is_test,
            source_offset: 0,
            name,
            params,
            ret,
            body,
            requires,
            ensures,
            line,
        })
    }
    fn block(&mut self) -> Result<Vec<Stmt>> {
        self.depth += 1;
        if self.depth > 96 {
            return Err(Error::new(self.line(), "nesting limit exceeded"));
        }
        self.need("{")?;
        let mut body = Vec::new();
        while !self.at("}") {
            if self.tokens[self.pos].kind == Kind::End {
                return Err(Error::new(self.line(), "unclosed block"));
            }
            body.push(self.stmt()?);
        }
        self.need("}")?;
        self.depth -= 1;
        Ok(body)
    }
    fn stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        let kind = if self.eat("borrow") {
            let mutable = self.eat("mut");
            if self.eat("(") {
                if !mutable {
                    return Err(Error::new(line, "at_pair requires borrow mut"));
                }
                let left = self.name()?;
                self.need(",")?;
                let right = self.name()?;
                self.need(")")?;
                self.need("=")?;
                if !self.eat("get_pair") {
                    self.need("at_pair")?;
                }
                self.need("(")?;
                let owner = self.expr(0)?;
                self.need(",")?;
                let left_index = self.expr(0)?;
                self.need(",")?;
                let right_index = self.expr(0)?;
                self.need(")")?;
                return Ok(Stmt {
                    line,
                    kind: StmtKind::ElementPair {
                        names: [left, right],
                        owner,
                        indices: [left_index, right_index],
                        body: self.block()?,
                    },
                });
            }
            let name = self.name()?;
            self.need("=")?;
            let access = if self.eat("at") {
                Some(ElementAccess::Index)
            } else if self.eat("get") {
                Some(ElementAccess::Key)
            } else {
                None
            };
            if let Some(access) = access {
                self.need("(")?;
                let owner = self.expr(0)?;
                self.need(",")?;
                let index = self.expr(0)?;
                self.need(")")?;
                return Ok(Stmt {
                    line,
                    kind: StmtKind::Element {
                        access,
                        name,
                        mutable,
                        owner,
                        index,
                        body: self.block()?,
                    },
                });
            }
            self.need("region")?;
            self.need("(")?;
            let owner = self.expr(0)?;
            self.need(",")?;
            let start = self.expr(0)?;
            self.need(",")?;
            let end = self.expr(0)?;
            self.need(")")?;
            StmtKind::Region {
                name,
                mutable,
                owner,
                start,
                end,
                body: self.block()?,
            }
        } else if self.eat("let") {
            let mutable = self.eat("mut");
            let name = self.name()?;
            self.need("=")?;
            let value = self.expr(0)?;
            self.need(";")?;
            StmtKind::Let(name, mutable, value)
        } else if self.eat("if") {
            // Read a chain iteratively, then build the existing nested If AST.
            // Recursive stmt calls can exhaust a small worker thread's stack
            // before the normal nesting limit is reached.
            let mut branches = Vec::new();
            let mut branch_line = line;
            let mut extra_depth = 0;
            let mut alternative = loop {
                let cond = self.expr(0)?;
                let yes = self.block()?;
                branches.push((branch_line, cond, yes));
                if !self.eat("else") {
                    break Vec::new();
                }
                branch_line = self.line();
                if !self.eat("if") {
                    break self.block()?;
                }
                self.depth += 1;
                extra_depth += 1;
                if self.depth > 96 {
                    return Err(Error::new(self.line(), "nesting limit exceeded"));
                }
            };
            self.depth -= extra_depth;
            for (line, cond, yes) in branches.into_iter().rev() {
                alternative = vec![Stmt {
                    line,
                    kind: StmtKind::If(cond, yes, alternative),
                }];
            }
            alternative.pop().unwrap().kind
        } else if self.eat("match") {
            let value = self.expr(0)?;
            self.need("{")?;
            let mut arms = Vec::new();
            while !self.at("}") {
                let line = self.line();
                let path = self.path()?;
                let binding = if self.eat("(") {
                    if path == "_" {
                        return Err(Error::new(
                            line,
                            "wildcard match arms cannot bind a payload",
                        ));
                    }
                    let name = self.name()?;
                    self.need(")")?;
                    Some(name)
                } else {
                    None
                };
                self.need("=>")?;
                let body = self.block()?;
                self.eat(",");
                arms.push(MatchArm {
                    path,
                    binding,
                    body,
                    line,
                });
            }
            self.need("}")?;
            StmtKind::Match(value, arms)
        } else if self.eat("for") {
            let name = self.name()?;
            self.need("in")?;
            let source = self.expr(0)?;
            if self.eat("..") {
                let end = self.expr(0)?;
                StmtKind::ForRange {
                    name,
                    start: source,
                    end,
                    body: self.block()?,
                }
            } else if let ExprKind::Borrow(path, mutable) = source.kind {
                let mut parts = path.split('.');
                let mut owner = Expr {
                    line,
                    kind: ExprKind::Var(parts.next().unwrap().into()),
                };
                for field in parts {
                    owner = Expr {
                        line,
                        kind: ExprKind::Field(Box::new(owner), field.into()),
                    };
                }
                StmtKind::ForEach {
                    name,
                    mutable,
                    owner,
                    body: self.block()?,
                }
            } else {
                return Err(Error::new(
                    line,
                    "for requires an int range start..end or &vector / &mut vector",
                ));
            }
        } else if self.eat("while") {
            let cond = self.expr(0)?;
            StmtKind::While(cond, self.block()?)
        } else if self.eat("break") {
            self.need(";")?;
            StmtKind::Break
        } else if self.eat("continue") {
            self.need(";")?;
            StmtKind::Continue
        } else if self.eat("return") {
            let value = if self.at(";") {
                None
            } else {
                Some(self.expr(0)?)
            };
            self.need(";")?;
            StmtKind::Return(value)
        } else if self.eat("print") {
            let e = self.expr(0)?;
            self.need(";")?;
            StmtKind::Print(e)
        } else if self.eat("assert") {
            let e = self.expr(0)?;
            self.need(";")?;
            StmtKind::Assert(e)
        } else {
            let e = self.expr(0)?;
            if self.eat("=") {
                let value = self.expr(0)?;
                self.need(";")?;
                match e.kind {
                    ExprKind::Var(name) => StmtKind::Assign(name, value),
                    ExprKind::Field(_, _) => StmtKind::AssignField(e, value),
                    _ => {
                        return Err(Error::new(
                            line,
                            "assignment target must be a variable or record field",
                        ))
                    }
                }
            } else {
                self.need(";")?;
                StmtKind::Expr(e)
            }
        };
        Ok(Stmt { line, kind })
    }
    fn expr(&mut self, min: u8) -> Result<Expr> {
        self.depth += 1;
        if self.depth > 96 {
            return Err(Error::new(self.line(), "expression nesting limit exceeded"));
        }
        let line = self.line();
        let mut e = if self.eat("??") {
            self.need("(")?;
            let ty = self.ty()?;
            self.need(",")?;
            let Kind::Text(intent) = self.tokens[self.pos].kind.clone() else {
                return Err(Error::new(line, "hole syntax: ??(type, \"intent\")"));
            };
            self.pos += 1;
            self.need(")")?;
            Expr {
                line,
                kind: ExprKind::Hole(ty, intent),
            }
        } else if self.eat("&") {
            let mutable = self.eat("mut");
            let mut name = self.name()?;
            while self.eat(".") {
                name.push('.');
                name.push_str(&self.name()?);
            }
            Expr {
                line,
                kind: ExprKind::Borrow(name, mutable),
            }
        } else if self.eat("!") {
            Expr {
                line,
                kind: ExprKind::Unary("!".into(), Box::new(self.expr(7)?)),
            }
        } else if self.eat("-") {
            Expr {
                line,
                kind: ExprKind::Unary("-".into(), Box::new(self.expr(7)?)),
            }
        } else if self.eat("try") {
            Expr {
                line,
                kind: ExprKind::Try(Box::new(self.expr(7)?)),
            }
        } else if self.eat("spawn") {
            let name = self.path()?;
            let args = self.args()?;
            Expr {
                line,
                kind: ExprKind::Spawn(name, args),
            }
        } else if self.eat("(") {
            let e = self.expr(0)?;
            self.need(")")?;
            e
        } else if self.eat("[") {
            let mut values = Vec::new();
            let mut repeat = None;
            if !self.at("]") {
                loop {
                    values.push(self.expr(0)?);
                    if values.len() == 1 && self.eat(";") {
                        repeat = Some(self.capacity()?);
                        break;
                    }
                    if !self.eat(",") {
                        break;
                    }
                }
            }
            self.need("]")?;
            Expr {
                line,
                kind: if let Some(count) = repeat {
                    ExprKind::Repeat(Box::new(values.pop().unwrap()), count)
                } else {
                    ExprKind::Array(values)
                },
            }
        } else {
            let t = self.tokens[self.pos].clone();
            self.pos += 1;
            let kind = match t.kind {
                Kind::Number(n) => ExprKind::Int(n),
                Kind::Float(n) => ExprKind::Float(n),
                Kind::Text(s) => ExprKind::Text(s),
                Kind::Word(s) if s == "true" || s == "false" => ExprKind::Bool(s == "true"),
                Kind::Word(mut s) => {
                    while self.eat("::") {
                        s.push_str("::");
                        s.push_str(&self.name()?);
                    }
                    if let Some(owner) = self.impl_owner {
                        if s == "Self" {
                            s = self.type_names[owner].clone();
                        } else if let Some(tail) = s.strip_prefix("Self::") {
                            s = format!("{}::{tail}", self.type_names[owner]);
                        }
                    }
                    if self.at("{")
                        && matches!(
                            self.tokens.get(self.pos + 1).map(|t| &t.kind),
                            Some(Kind::Word(_))
                        )
                        && matches!(self.tokens.get(self.pos+2).map(|t|&t.kind),Some(Kind::Symbol(v)) if v==":")
                    {
                        self.need("{")?;
                        let mut fields = Vec::new();
                        while !self.at("}") {
                            let name = self.name()?;
                            self.need(":")?;
                            fields.push((name, self.expr(0)?));
                            if !self.eat(",") {
                                break;
                            }
                        }
                        self.need("}")?;
                        ExprKind::Record(s, fields)
                    } else if self.at("(") {
                        ExprKind::Call(s, self.args()?)
                    } else {
                        if s.contains("::") {
                            return Err(Error::new(
                                line,
                                "qualified paths must name a function call",
                            ));
                        }
                        ExprKind::Var(s)
                    }
                }
                _ => return Err(Error::new(line, "expected expression")),
            };
            Expr { line, kind }
        };
        let mut operators = 0;
        while self.eat(".") {
            let name = self.name()?;
            let kind = if self.at("(") {
                ExprKind::MethodCall(Box::new(e), name, self.args()?)
            } else {
                ExprKind::Field(Box::new(e), name)
            };
            e = Expr { line, kind };
        }
        while let Kind::Symbol(s) = &self.tokens[self.pos].kind {
            let op = s.clone();
            let precedence = match op.as_str() {
                "||" => 1,
                "&&" => 2,
                "==" | "!=" => 3,
                "<" | ">" | "<=" | ">=" => 4,
                "+" | "-" => 5,
                "*" | "/" | "%" => 6,
                _ => break,
            };
            if precedence < min {
                break;
            }
            operators += 1;
            if operators > 128 {
                return Err(Error::new(
                    line,
                    "split long expressions into intermediate bindings",
                ));
            }
            self.pos += 1;
            let right = self.expr(precedence + 1)?;
            e = Expr {
                line,
                kind: ExprKind::Binary(op, Box::new(e), Box::new(right)),
            };
        }
        // Bound the resulting tree too: precedence and nested calls can create
        // a deeper tree than the parser's call depth alone would indicate.
        let mut pending = vec![(&e, 1usize)];
        while let Some((node, depth)) = pending.pop() {
            if depth > 128 {
                return Err(Error::new(
                    line,
                    "expression tree too deep; use intermediate bindings",
                ));
            }
            match &node.kind {
                ExprKind::Unary(_, value) | ExprKind::Try(value) => {
                    pending.push((value, depth + 1))
                }
                ExprKind::Field(value, _) => pending.push((value, depth + 1)),
                ExprKind::MethodCall(value, _, args) => {
                    pending.push((value, depth + 1));
                    pending.extend(args.iter().map(|v| (v, depth + 1)));
                }
                ExprKind::Record(_, fields) => {
                    pending.extend(fields.iter().map(|(_, e)| (e, depth + 1)))
                }
                ExprKind::Binary(_, left, right) => {
                    pending.push((left, depth + 1));
                    pending.push((right, depth + 1));
                }
                ExprKind::Array(values)
                | ExprKind::Call(_, values)
                | ExprKind::Spawn(_, values) => {
                    pending.extend(values.iter().map(|v| (v, depth + 1)))
                }
                _ => {}
            }
        }
        self.depth -= 1;
        Ok(e)
    }
    fn args(&mut self) -> Result<Vec<Expr>> {
        self.need("(")?;
        let mut args = Vec::new();
        if !self.at(")") {
            loop {
                args.push(self.expr(0)?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.need(")")?;
        Ok(args)
    }
    fn data(&mut self, public: bool) -> Result<DataDecl> {
        let line = self.line();
        let is_soa = self.eat("soa");
        let is_vector = self.eat("vector");
        if is_soa && !is_vector {
            return Err(Error::new(line, "soa requires vector"));
        }
        let is_map = !is_vector && self.eat("map");
        let is_arena = !is_vector && !is_map && self.eat("arena");
        let is_channel = !is_vector && !is_map && !is_arena && self.eat("channel");
        let is_enum = !is_vector && !is_map && !is_arena && !is_channel && self.eat("enum");
        if !is_enum && !is_vector && !is_map && !is_arena && !is_channel {
            self.need("struct")?;
        }
        let name = self.name()?;
        if !name.as_bytes()[0].is_ascii_uppercase() {
            return Err(Error::new(
                line,
                "data type names must start with an uppercase letter",
            ));
        }
        if is_vector || is_map || is_arena || is_channel {
            self.need("(")?;
            let ty = self.ty()?;
            let value = if is_map {
                self.need(",")?;
                Some(self.ty()?)
            } else {
                None
            };
            self.need(")")?;
            self.need(";")?;
            return Ok(DataDecl {
                name,
                public,
                line,
                kind: if let Some(value) = value {
                    DataKind::Map(vec![
                        FieldDecl {
                            name: "key".into(),
                            ty,
                        },
                        FieldDecl {
                            name: "value".into(),
                            ty: value,
                        },
                    ])
                } else {
                    let fields = vec![FieldDecl {
                        name: "element".into(),
                        ty,
                    }];
                    if is_channel {
                        DataKind::Channel(fields)
                    } else if is_arena {
                        DataKind::Arena(fields)
                    } else if is_soa {
                        DataKind::SoaVector(fields)
                    } else {
                        DataKind::Vector(fields)
                    }
                },
            });
        }
        self.need("{")?;
        let mut fields = Vec::new();
        while !self.at("}") {
            let field = self.name()?;
            let ty = if is_enum {
                if self.eat("(") {
                    let ty = self.ty()?;
                    self.need(")")?;
                    ty
                } else {
                    Type::Unit
                }
            } else {
                self.need(":")?;
                self.ty()?
            };
            if ty.scoped_view() {
                return Err(Error::new(
                    line,
                    "scoped slices cannot be stored in data types",
                ));
            }
            fields.push(FieldDecl { name: field, ty });
            if !self.eat(",") {
                break;
            }
        }
        self.need("}")?;
        if fields.is_empty() {
            return Err(Error::new(
                line,
                "data types require at least one field or variant",
            ));
        }
        Ok(DataDecl {
            name,
            public,
            kind: if is_enum {
                DataKind::Enum(fields)
            } else {
                DataKind::Record(fields)
            },
            line,
        })
    }
}
pub fn parse(source: &str) -> Result<Vec<Function>> {
    let module = parse_module(source)?;
    if !module.modules.is_empty() {
        return Err(Error::new(
            module.modules[0].line,
            "modules require a file/project build",
        ));
    }
    Ok(module.functions)
}
pub fn parse_module(source: &str) -> Result<Module> {
    profile_work(|profile| profile.module_parses += 1);
    let mut p = Parser {
        tokens: lex(source)?,
        pos: 0,
        depth: 0,
        type_names: Vec::new(),
        impl_owner: None,
    };
    let mut functions = Vec::new();
    let mut modules = Vec::new();
    let mut types = Vec::new();
    let mut implementations = Vec::new();
    while p.tokens[p.pos].kind != Kind::End {
        let line = p.line();
        if p.eat("impl") {
            if p.impl_owner.is_some() {
                return Err(Error::new(line, "nested impl blocks are not supported"));
            }
            let owner = p.ty()?;
            let Type::Named(id) = owner else {
                return Err(Error::new(
                    line,
                    "impl requires a named record, enum, vector or map",
                ));
            };
            p.need("{")?;
            p.impl_owner = Some(id);
            implementations.push(Implementation { owner, line });
            continue;
        }
        if p.impl_owner.is_some() && p.eat("}") {
            p.impl_owner = None;
            continue;
        }
        if p.eat("mod") {
            if p.impl_owner.is_some() {
                return Err(Error::new(line, "modules cannot be declared inside impl"));
            }
            modules.push(ModuleDecl {
                name: p.name()?,
                line,
            });
            p.need(";")?;
        } else {
            let mut effects = Vec::new();
            let mut intents = Vec::new();
            while p.eat("@") {
                let annotation = p.name()?;
                p.need("(")?;
                match annotation.as_str() {
                    "effect" => loop {
                        let effect = p.name()?;
                        if !["no_alloc", "no_io", "bounded"].contains(&effect.as_str()) {
                            return Err(Error::new(
                                line,
                                "supported effects: no_alloc, no_io, bounded",
                            ));
                        }
                        if effects.contains(&effect) {
                            return Err(Error::new(line, "duplicate effect contract"));
                        }
                        effects.push(effect);
                        if !p.eat(",") {
                            break;
                        }
                    },
                    "intent" => {
                        if let Kind::Text(text) = &p.tokens[p.pos].kind {
                            intents.push(text.clone());
                            p.pos += 1;
                        } else {
                            return Err(Error::new(line, "intent requires a text literal"));
                        }
                    }
                    _ => {
                        return Err(Error::new(
                            line,
                            "unknown annotation; use @effect or @intent",
                        ))
                    }
                }
                p.need(")")?;
            }
            let public = p.eat("pub");
            if p.at("struct")
                || p.at("enum")
                || p.at("vector")
                || p.at("soa")
                || p.at("map")
                || p.at("arena")
                || p.at("channel")
            {
                if p.impl_owner.is_some() {
                    return Err(Error::new(
                        line,
                        "data types cannot be declared inside impl",
                    ));
                }
                if !effects.is_empty() || !intents.is_empty() {
                    return Err(Error::new(line, "annotations apply to functions"));
                }
                types.push(p.data(public)?);
                continue;
            }
            let is_test = p.at("test");
            let external = p.eat("extern");
            let mut f = p.function(public, is_test, external)?;
            f.effects = effects;
            f.intents = intents;
            functions.push(f);
        }
    }
    if p.impl_owner.is_some() {
        return Err(Error::new(p.line(), "unclosed impl block"));
    }
    Ok(Module {
        implementations,
        functions,
        modules,
        types,
        type_names: p.type_names,
    })
}
