//! Compiler-backed code navigation (`symbols` and `symbol_at`).
//!
//! Coordinate convention (`coordinate_encoding = "utf8-1based"`):
//! - `line`: 1-based line number (`1` is the first line; `\n` and `\r\n` both advance lines).
//! - `column`: 1-based UTF-8 byte offset within `line` (`1` is the first byte of the line).
//! - `byte`: 0-based UTF-8 byte offset from the start of the file.
//! - All ranges (`range`, `selection_range`, `reference_range`) are half-open `[start, end)`.

use crate::{
    checker,
    numeric::{Float, Integer},
    project::{Project, SourceFile},
    syntax::{Borrow, DataDecl, DataKind, ElementAccess, FieldDecl, Type},
    types::TypeTable,
    Error, Result,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const COORDINATE_ENCODING: &str = "utf8-1based";

const CHANNEL_BUILTIN_DETAILS: &[(&str, &str)] = &[
    ("send", "send(channel, value) -> bool; transfers value even on closed-channel failure; blocks while full; IO"),
    ("recv", "recv(channel, &mut output) -> bool; FIFO receive, replaces output only on success; blocks until available or closed; IO"),
    ("try_recv", "try_recv(channel, &mut output) -> bool; nonblocking FIFO receive, leaves output unchanged on false; IO"),
    ("close", "close(channel); idempotently closes the shared queue and wakes blocked endpoints; IO"),
    ("is_closed", "is_closed(channel) -> bool; observes shared closed state; IO"),
    ("len", "len(collection or channel) -> int; channel queue length is synchronized; IO for channels"),
    ("capacity", "capacity(collection or channel) -> int; channel capacity is fixed and positive; IO for channels"),
    ("clone", "clone(value) -> same type; cloning a channel creates an endpoint sharing its queue; IO for channels"),
];

const INTEGER_BUILTIN_DETAILS: &[(&str, &str)] = &[
    ("wrapping_add", "wrapping_add(a: T, b: T) -> T; explicit modulo 2^width addition; T is int/i8..u64; pure, no_alloc, no_io, bounded"),
    ("wrapping_sub", "wrapping_sub(a: T, b: T) -> T; explicit modulo 2^width subtraction; T is int/i8..u64; pure, no_alloc, no_io, bounded"),
    ("wrapping_mul", "wrapping_mul(a: T, b: T) -> T; explicit modulo 2^width multiplication; T is int/i8..u64; pure, no_alloc, no_io, bounded"),
    ("bit_and", "bit_and(a: T, b: T) -> T; exact-width bitwise AND; same integer T; pure"),
    ("bit_or", "bit_or(a: T, b: T) -> T; exact-width bitwise OR; same integer T; pure"),
    ("bit_xor", "bit_xor(a: T, b: T) -> T; exact-width bitwise XOR; same integer T; pure"),
    ("bit_not", "bit_not(value: T) -> T; exact-width bitwise complement; integer T; pure"),
    ("shl", "shl(value: T, bits: int) -> T; exact-width left shift, discarded high bits; count masked by width-1, including negative counts; pure"),
    ("shr", "shr(value: T, bits: int) -> T; arithmetic right shift for signed T, logical for unsigned T; count masked by width-1; pure"),
    ("rotl", "rotl(value: T, bits: int) -> T; exact-width rotate left; count masked by width-1, including negative counts; pure"),
    ("rotr", "rotr(value: T, bits: int) -> T; exact-width rotate right; count masked by width-1, including negative counts; pure"),
    ("read_u16_be", "read_u16_be(bytes, offset: int) -> u16; checked big-endian byte read; packet read out of bounds traps; pure"),
    ("wire_read_u16_be", "wire_read_u16_be(bytes, offset: int) -> u16; checked big-endian byte read; packet read out of bounds traps; pure"),
    ("read_u32_be", "read_u32_be(bytes, offset: int) -> u32; checked big-endian byte read; packet read out of bounds traps; pure"),
    ("wire_read_u32_be", "wire_read_u32_be(bytes, offset: int) -> u32; checked big-endian byte read; packet read out of bounds traps; pure"),
    ("read_u64_be", "read_u64_be(bytes, offset: int) -> u64; checked big-endian byte read; packet read out of bounds traps; pure"),
    ("wire_read_u64_be", "wire_read_u64_be(bytes, offset: int) -> u64; checked big-endian byte read; packet read out of bounds traps; pure"),
];

const PROCESS_BUILTIN_DETAILS: &[(&str, &str)] = &[
    ("process_run", "process_run(program: text/str, args: &texts or &VectorOfText) -> ProcessRun; direct subprocess execution with captured status/stdout/stderr; requires explicit process capability; IO; never executed by the pure verifier"),
    ("ProcessOutput", "ProcessOutput { status: int, stdout: text, stderr: text }; owned captured output; a nonzero child exit status is still Output"),
    ("ProcessRun", "enum ProcessRun { Output(ProcessOutput), Error(text) }; recoverable launch/capture errors; try unwraps Output or propagates Error"),
];

/// 1.2 Frontier 3 builtins and prelude types (contracts N2-N5, N9). Each
/// detail starts with the call signature that hover and signature help show.
const FRONTIER3_BUILTIN_DETAILS: &[(&str, &str)] = &[
    ("try_write_u16_be", "try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool; checked big-endian write, most significant byte first; false leaves buf unchanged when offset < 0 or offset + 2 > len; rejected inside contracts; no_alloc, no_io, bounded"),
    ("try_write_u32_be", "try_write_u32_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u32) -> bool; checked big-endian write, most significant byte first; false leaves buf unchanged when offset < 0 or offset + 4 > len; rejected inside contracts; no_alloc, no_io, bounded"),
    ("try_write_u64_be", "try_write_u64_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u64) -> bool; checked big-endian write, most significant byte first; false leaves buf unchanged when offset < 0 or offset + 8 > len; rejected inside contracts; no_alloc, no_io, bounded"),
    ("wire_try_write_u16_be", "wire_try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool; alias of try_write_u16_be; checked big-endian write; false leaves buf unchanged; no_alloc, no_io, bounded"),
    ("wire_try_write_u32_be", "wire_try_write_u32_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u32) -> bool; alias of try_write_u32_be; checked big-endian write; false leaves buf unchanged; no_alloc, no_io, bounded"),
    ("wire_try_write_u64_be", "wire_try_write_u64_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u64) -> bool; alias of try_write_u64_be; checked big-endian write; false leaves buf unchanged; no_alloc, no_io, bounded"),
    ("fnv1a_64", "fnv1a_64(input: text/str/&[u8]/[u8; N]) -> u64; exact 64-bit FNV-1a (offset basis 0xcbf29ce484222325, prime 0x100000001b3) over the borrowed bytes; pure, no_alloc, no_io, bounded; allowed in contracts"),
    ("sha256_text", "sha256_text(input: text/str/&[u8]/[u8; N]) -> text; FIPS 180-4 SHA-256 of the borrowed bytes as a new owned 64-character lowercase hex text; allocating, no_io, bounded"),
    ("net_request", "net_request(endpoint: text/str, payload: text/str) -> NetResult; borrowed arguments; loopback:// routes (echo, sha256, head-fnv1a, status/NNN) or http:// and tcp:// to 127.0.0.1 only; requires the net capability (default false); IO, allocating; never executed by the pure verifier"),
    ("NetResponse", "NetResponse { status: int, body: text }; owned network response; status is 0 for tcp://"),
    ("NetResult", "enum NetResult { Response(NetResponse), Error(text) }; recoverable request errors; try unwraps Response or propagates Error"),
];

/// 1.3 OS/runtime primitives: binary file I/O, environment, directories and a
/// one-shot loopback server. Their ordinals follow the 1.2 entries.
const V130_BUILTIN_DETAILS: &[(&str, &str)] = &[
    ("try_read_bytes", "try_read_bytes(path: text/str, dst: &mut [u8; N] or &mut [u8]) -> int; reads the whole file into dst[0..n] and returns n; -1 leaves dst unchanged when the path has a NUL, cannot be read, or the file is larger than dst or 16 MiB; requires file_read; io, no_alloc, bounded"),
    ("try_write_bytes", "try_write_bytes(path: text/str, src: [u8; N] or &[u8]) -> bool; writes all of the borrowed bytes (at most 16 MiB), replacing the file; false on a NUL in the path or any open/write failure; requires file_write; io, no_alloc, bounded"),
    ("env_var", "env_var(name: text/str) -> text; the process environment variable as new owned UTF-8 text; empty when the name is empty or contains = or NUL, or the value is unset, invalid or over 16 MiB; requires process_args; io, allocating, bounded"),
    ("create_dir", "create_dir(path: text/str) -> bool; creates one directory; true when it was created or already exists as a directory; requires file_write; io, no_alloc, bounded"),
    ("remove_file", "remove_file(path: text/str) -> bool; deletes one file; true when it was deleted; requires file_write; io, no_alloc, bounded"),
    ("net_serve_once", "net_serve_once(endpoint: text/str, status: int, reply_body: text/str) -> NetResult; loopback://serve/<payload> in memory, or accepts one connection on http:// or tcp:// 127.0.0.1 within 5 s, replies, and returns the request body; status 100..599 (0 for tcp://); requires the net capability; io, allocating, never bounded"),
    ("keys", "keys(m) -> texts or ints; a new collection of the keys of a map whose key type is text (texts) or int (ints), in unspecified order; the map is borrowed, not consumed; allocating, no_io, bounded; rejected inside contracts"),
    ("http_request", "http_request(url: text/str, body: text/str) -> NetResult; GET when body is empty, POST otherwise, to an http:// or https:// URL through WinHTTP (loopback:// goes to net_request); 16 MiB limit each way, strict UTF-8 response, 30 s timeouts; requires the net capability (default false); io, allocating, never bounded"),
];

/// Names of the networking prelude types (registered after user types).
const NET_PRELUDE_TYPES: [&str; 2] = ["NetResponse", "NetResult"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NavPosition {
    pub line: usize,
    pub column: usize,
    pub byte: usize,
}

impl NavPosition {
    pub fn to_json(self) -> Value {
        json!({
            "line": self.line,
            "column": self.column,
            "byte": self.byte,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavRange {
    pub start: NavPosition,
    pub end: NavPosition,
}

impl NavRange {
    pub fn contains(self, line: usize, column: usize) -> bool {
        (self.start.line, self.start.column) <= (line, column)
            && (line, column) < (self.end.line, self.end.column)
    }

    pub fn byte_len(self) -> usize {
        self.end.byte.saturating_sub(self.start.byte)
    }

    pub fn to_json(self) -> Value {
        json!({
            "start": self.start.to_json(),
            "end": self.end.to_json(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: &'static str,
    pub detail: String,
    pub public: bool,
    pub file: String,
    pub path: String,
    pub range: NavRange,
    pub selection_range: NavRange,
    pub children: Vec<SymbolInfo>,
}

impl SymbolInfo {
    pub fn to_outline_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "qualified_name": self.qualified_name,
            "kind": self.kind,
            "detail": self.detail,
            "public": self.public,
            "file": self.file,
            "path": self.path,
            "line": self.selection_range.start.line,
            "column": self.selection_range.start.column,
            "range": self.range.to_json(),
            "selection_range": self.selection_range.to_json(),
            "children": self.children.iter().map(Self::to_outline_json).collect::<Vec<_>>(),
        })
    }

    pub fn to_target_json(&self, reference_range: NavRange) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "qualified_name": self.qualified_name,
            "kind": self.kind,
            "detail": self.detail,
            "public": self.public,
            "file": self.file,
            "path": self.path,
            "line": self.selection_range.start.line,
            "column": self.selection_range.start.column,
            "range": self.range.to_json(),
            "selection_range": self.selection_range.to_json(),
            "reference_range": reference_range.to_json(),
        })
    }
}

#[derive(Clone, Debug)]
struct ReferenceSpan {
    range: NavRange,
    symbol_id: String,
    is_declaration: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeclKey {
    file_idx: usize,
    decl_ordinal: usize,
    kind: &'static str,
}

pub fn source_fingerprint(source: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in source.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}:len={}", source.len())
}

pub fn project_fingerprint(project: &Project) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    // Capabilities and entry configuration are part of the checked boundary,
    // even when their source files happen to be byte-for-byte unchanged.
    for &b in project.manifest.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= 0xfd;
    hash = hash.wrapping_mul(0x100000001b3);
    for file in &project.files {
        let rel = relative_file_path(project, file);
        for &b in rel.as_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
        for &b in file.source.as_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xfe;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}:files={}", project.files.len())
}

/// Lossless source coordinates for synthesis. The checker remains responsible
/// for all semantic context; navigation only supplies the exact original spans.
#[derive(Clone, Debug)]
pub(crate) struct HoleSyntax {
    pub file_index: usize,
    pub function_index: usize,
    pub function: String,
    pub ordinal: usize,
    pub range: NavRange,
    pub expected_type: String,
    pub intent: String,
    pub requires: Vec<String>,
    pub ensures: Vec<String>,
    pub in_contract: bool,
}

pub(crate) fn hole_syntax(project: &Project) -> Result<Vec<HoleSyntax>> {
    let mut result = Vec::new();
    for (file_index, file) in project.files.iter().enumerate() {
        result.extend(file_hole_syntax(file_index, file)?);
    }
    Ok(result)
}

/// Raw, source-local syntax only. Callers may retain this with its exact bytes;
/// resolved symbols, inferred types, loans and checked bodies are never retained.
pub(crate) fn file_hole_syntax(
    file_index: usize,
    file: &crate::project::SourceFile,
) -> Result<Vec<HoleSyntax>> {
    fn expression<'a>(expr: &'a NavExpr, out: &mut Vec<(&'a NavExpr, bool)>, contract: bool) {
        match &expr.kind {
            NavExprKind::Hole(..) => out.push((expr, contract)),
            NavExprKind::Array(items)
            | NavExprKind::Call(_, items)
            | NavExprKind::Spawn(_, items) => {
                for item in items {
                    expression(item, out, contract);
                }
            }
            NavExprKind::MethodCall(owner, _, items) => {
                expression(owner, out, contract);
                for item in items {
                    expression(item, out, contract);
                }
            }
            NavExprKind::Repeat(value, _)
            | NavExprKind::Try(_, value)
            | NavExprKind::Unary(_, value)
            | NavExprKind::Field(value, _) => expression(value, out, contract),
            NavExprKind::Binary(_, left, right) => {
                expression(left, out, contract);
                expression(right, out, contract);
            }
            NavExprKind::Record(_, fields) => {
                for (_, value) in fields {
                    expression(value, out, contract);
                }
            }
            _ => {}
        }
    }
    fn block<'a>(body: &'a [NavStmt], out: &mut Vec<(&'a NavExpr, bool)>) {
        for stmt in body {
            match &stmt.kind {
                NavStmtKind::ScopedBinding {
                    owner,
                    selection,
                    body,
                    ..
                } => {
                    expression(owner, out, false);
                    match selection {
                        NavSelection::Region { start, end } => {
                            expression(start, out, false);
                            expression(end, out, false);
                        }
                        NavSelection::Element { index, .. } | NavSelection::IntegerRange(index) => {
                            expression(index, out, false)
                        }
                        NavSelection::Each => {}
                    }
                    block(body, out);
                }
                NavStmtKind::Let { value, .. }
                | NavStmtKind::Assign { value, .. }
                | NavStmtKind::Expr(value)
                | NavStmtKind::Print(value)
                | NavStmtKind::Assert(value) => expression(value, out, false),
                NavStmtKind::AssignField { target, value } => {
                    expression(target, out, false);
                    expression(value, out, false);
                }
                NavStmtKind::Return(Some(value)) => expression(value, out, false),
                NavStmtKind::If(value, yes, no) => {
                    expression(value, out, false);
                    block(yes, out);
                    block(no, out);
                }
                NavStmtKind::While(value, body) => {
                    expression(value, out, false);
                    block(body, out);
                }
                NavStmtKind::Match(value, arms) => {
                    expression(value, out, false);
                    for arm in arms {
                        block(&arm.body, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut result = Vec::new();
    let module = parse_nav_module(&file.source).map_err(|e| e.in_file(&file.path))?;
    for (function_index, f) in module.functions.iter().enumerate() {
        let owner = f.method_owner.as_ref().map(|o| match o.ty {
            Type::Named(i) => module.type_names[i].clone(),
            other => other.label(),
        });
        let function = synthesis_function_name(&file.module, &f.ident.name, owner.as_deref());
        let source = |e: &NavExpr| file.source[e.range.start.byte..e.range.end.byte].to_owned();
        let mut holes = Vec::new();
        for expr in f.requires.iter().chain(&f.ensures) {
            expression(expr, &mut holes, true);
        }
        block(&f.body, &mut holes);
        holes.sort_by_key(|(e, _)| e.range.start.byte);
        // Pair navigation shares the original owner between two lexical
        // aliases. A shared source expression is still exactly one slot.
        holes.dedup_by_key(|(e, _)| (e.range.start.byte, e.range.end.byte));
        for (ordinal, (expr, in_contract)) in holes.into_iter().enumerate() {
            let NavExprKind::Hole(ty, intent) = &expr.kind else {
                unreachable!()
            };
            result.push(HoleSyntax {
                file_index,
                function_index,
                function: function.clone(),
                ordinal,
                range: expr.range,
                expected_type: file.source[ty.range.start.byte..ty.range.end.byte].to_owned(),
                intent: intent.clone(),
                requires: f.requires.iter().map(source).collect(),
                ensures: f.ensures.iter().map(source).collect(),
                in_contract,
            });
        }
    }
    Ok(result)
}

#[derive(Clone, Debug, PartialEq)]
enum TokKind {
    Word(String),
    Number(i64),
    Float(u64),
    Text(String),
    Symbol(String),
    End,
}

#[derive(Clone, Debug)]
struct SpannedToken {
    kind: TokKind,
    range: NavRange,
}

const RESERVED_WORDS: &[&str] = &[
    "fn", "let", "mut", "return", "break", "continue", "if", "else", "while", "true", "false",
    "spawn", "requires", "ensures", "assert", "print", "result", "mod", "pub", "test", "crate",
    "struct", "enum", "vector", "soa", "arena", "channel", "try", "extern", "map", "impl", "match",
    "for", "in",
];

fn lex_spanned(source: &str) -> Result<Vec<SpannedToken>> {
    if source.len() > crate::project::SOURCE_FILE_LIMIT {
        return Err(Error::new(1, "source file limit is 16 MiB"));
    }
    let mut it = source.char_indices().peekable();
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut line_start_byte = 0usize;

    let make_pos = |byte: usize, line: usize, line_start_byte: usize| NavPosition {
        line,
        column: (byte - line_start_byte) + 1,
        byte,
    };

    while let Some((byte_idx, ch)) = it.next() {
        if ch == '\n' {
            line += 1;
            line_start_byte = byte_idx + 1;
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        if ch == '/' && it.peek().is_some_and(|(_, c)| *c == '/') {
            for (b, c) in it.by_ref() {
                if c == '\n' {
                    line += 1;
                    line_start_byte = b + 1;
                    break;
                }
            }
            continue;
        }

        let start_pos = make_pos(byte_idx, line, line_start_byte);
        let mut end_byte = byte_idx + ch.len_utf8();

        let kind = if ch.is_ascii_alphabetic() || ch == '_' {
            let mut s = ch.to_string();
            while let Some(&(b, c)) = it.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    s.push(c);
                    end_byte = b + c.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
            TokKind::Word(s)
        } else if ch.is_ascii_digit() {
            let mut s = ch.to_string();
            while let Some(&(b, c)) = it.peek() {
                if c.is_ascii_digit() {
                    s.push(c);
                    end_byte = b + c.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
            let mut floating = false;
            if it.peek().is_some_and(|(_, c)| *c == '.')
                && it.clone().nth(1).is_some_and(|(_, c)| c.is_ascii_digit())
            {
                floating = true;
                let (b, dot) = it.next().unwrap();
                s.push(dot);
                end_byte = b + dot.len_utf8();
                while let Some(&(b, c)) = it.peek() {
                    if c.is_ascii_digit() {
                        s.push(c);
                        end_byte = b + c.len_utf8();
                        it.next();
                    } else {
                        break;
                    }
                }
            }
            if it.peek().is_some_and(|(_, c)| matches!(c, 'e' | 'E')) {
                floating = true;
                let (b, exp_ch) = it.next().unwrap();
                s.push(exp_ch);
                end_byte = b + exp_ch.len_utf8();
                if let Some(&(b, sign)) = it.peek() {
                    if matches!(sign, '+' | '-') {
                        s.push(sign);
                        end_byte = b + sign.len_utf8();
                        it.next();
                    }
                }
                if !it.peek().is_some_and(|(_, c)| c.is_ascii_digit()) {
                    return Err(Error::new(line, "exponent requires digits"));
                }
                while let Some(&(b, c)) = it.peek() {
                    if c.is_ascii_digit() {
                        s.push(c);
                        end_byte = b + c.len_utf8();
                        it.next();
                    } else {
                        break;
                    }
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
                TokKind::Float(value.to_bits())
            } else {
                TokKind::Number(
                    s.parse().map_err(|_| {
                        Error::new(line, "integer literal exceeds signed 64-bit range")
                    })?,
                )
            }
        } else if ch == '"' {
            let mut s = String::new();
            let mut closed = false;
            while let Some((b, c)) = it.next() {
                end_byte = b + c.len_utf8();
                if c == '"' {
                    closed = true;
                    break;
                }
                let decoded = if c == '\\' {
                    let Some((eb, esc)) = it.next() else {
                        return Err(Error::new(start_pos.line, "unclosed text literal"));
                    };
                    end_byte = eb + esc.len_utf8();
                    match esc {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '"' => '"',
                        '\\' => '\\',
                        _ => return Err(Error::new(line, "unsupported string escape")),
                    }
                } else {
                    c
                };
                if decoded == '\0' {
                    return Err(Error::new(line, "NUL is not supported inside text"));
                }
                if c == '\n' {
                    line += 1;
                    line_start_byte = b + 1;
                }
                s.push(decoded);
            }
            if !closed {
                return Err(Error::new(start_pos.line, "unclosed text literal"));
            }
            TokKind::Text(s)
        } else {
            let mut s = ch.to_string();
            if let Some(&(b2, c2)) = it.peek() {
                let pair = format!("{ch}{c2}");
                if [
                    "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "::", "??", "..",
                ]
                .contains(&pair.as_str())
                {
                    it.next();
                    end_byte = b2 + c2.len_utf8();
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
                return Err(Error::new(
                    line,
                    format!(
                        "unsupported token {s:?}; raw pointers and references are not in this subset"
                    ),
                ));
            }
            TokKind::Symbol(s)
        };

        let end_pos = make_pos(end_byte, line, line_start_byte);
        out.push(SpannedToken {
            kind,
            range: NavRange {
                start: start_pos,
                end: end_pos,
            },
        });
    }

    let eof_pos = make_pos(source.len(), line, line_start_byte);
    out.push(SpannedToken {
        kind: TokKind::End,
        range: NavRange {
            start: eof_pos,
            end: eof_pos,
        },
    });
    Ok(out)
}

#[derive(Clone, Debug)]
struct IdentSpan {
    name: String,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct PathSpan {
    segments: Vec<IdentSpan>,
    range: NavRange,
    resolved: Option<String>,
}

impl PathSpan {
    fn joined(&self) -> String {
        if let Some(name) = &self.resolved {
            return name.clone();
        }
        self.segments
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::")
    }
}

#[derive(Clone, Debug)]
struct TypeSyntax {
    ty: Type,
    path: Option<PathSpan>,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavModuleDecl {
    ident: IdentSpan,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavFieldDecl {
    ident: IdentSpan,
    ty: TypeSyntax,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavVariantDecl {
    ident: IdentSpan,
    payload: Option<TypeSyntax>,
    range: NavRange,
}

#[derive(Clone, Debug)]
enum NavDataKind {
    Arena(TypeSyntax),
    Channel(TypeSyntax),
    Vector(TypeSyntax),
    SoaVector(TypeSyntax),
    Map(TypeSyntax, TypeSyntax),
    Record(Vec<NavFieldDecl>),
    Enum(Vec<NavVariantDecl>),
}

#[derive(Clone, Debug)]
struct NavDataDecl {
    public: bool,
    ident: IdentSpan,
    kind: NavDataKind,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavViewField {
    segments: Vec<IdentSpan>,
    mutable: bool,
}

#[derive(Clone, Debug)]
struct NavParam {
    ident: IdentSpan,
    ty: TypeSyntax,
    mutable: bool,
    borrow: Borrow,
    view: Option<Vec<NavViewField>>,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavFunction {
    method_owner: Option<TypeSyntax>,
    public: bool,
    is_test: bool,
    ident: IdentSpan,
    params: Vec<NavParam>,
    ret: Option<TypeSyntax>,
    requires: Vec<NavExpr>,
    ensures: Vec<NavExpr>,
    body: Vec<NavStmt>,
    range: NavRange,
}

#[derive(Clone, Debug)]
struct NavMatchArm {
    path: PathSpan,
    binding: Option<IdentSpan>,
    body: Vec<NavStmt>,
}

#[derive(Clone, Debug)]
struct NavStmt {
    range: NavRange,
    kind: NavStmtKind,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum NavSelection {
    Region {
        start: NavExpr,
        end: NavExpr,
    },
    Element {
        access: ElementAccess,
        index: NavExpr,
    },
    Each,
    IntegerRange(NavExpr),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum NavStmtKind {
    ScopedBinding {
        ident: IdentSpan,
        mutable: bool,
        owner: NavExpr,
        selection: NavSelection,
        body: Vec<NavStmt>,
    },
    Let {
        ident: IdentSpan,
        mutable: bool,
        value: NavExpr,
    },
    Assign {
        target: IdentSpan,
        value: NavExpr,
    },
    AssignField {
        target: NavExpr,
        value: NavExpr,
    },
    Expr(NavExpr),
    Print(NavExpr),
    Assert(NavExpr),
    Return(Option<NavExpr>),
    LoopJump,
    If(NavExpr, Vec<NavStmt>, Vec<NavStmt>),
    While(NavExpr, Vec<NavStmt>),
    Match(NavExpr, Vec<NavMatchArm>),
}

#[derive(Clone, Debug)]
struct NavExpr {
    range: NavRange,
    kind: NavExprKind,
}

#[derive(Clone, Debug)]
enum NavExprKind {
    Int,
    Float,
    Bool,
    Text,
    Hole(TypeSyntax, String),
    Var(IdentSpan),
    Borrow(Vec<IdentSpan>, bool),
    Array(Vec<NavExpr>),
    Repeat(Box<NavExpr>, usize),
    Call(PathSpan, Vec<NavExpr>),
    MethodCall(Box<NavExpr>, IdentSpan, Vec<NavExpr>),
    Spawn(PathSpan, Vec<NavExpr>),
    Try(IdentSpan, Box<NavExpr>),
    Unary(String, Box<NavExpr>),
    Binary(String, Box<NavExpr>, Box<NavExpr>),
    Field(Box<NavExpr>, IdentSpan),
    Record(PathSpan, Vec<(IdentSpan, NavExpr)>),
}

#[derive(Clone, Debug)]
struct NavModule {
    implementations: Vec<TypeSyntax>,
    modules: Vec<NavModuleDecl>,
    types: Vec<NavDataDecl>,
    functions: Vec<NavFunction>,
    type_names: Vec<String>,
}

struct NavParser {
    impl_owner: Option<TypeSyntax>,
    tokens: Vec<SpannedToken>,
    pos: usize,
    depth: usize,
    type_names: Vec<String>,
}

impl NavParser {
    fn line(&self) -> usize {
        self.tokens[self.pos].range.start.line
    }

    fn cur_range(&self) -> NavRange {
        self.tokens[self.pos].range
    }

    fn prev_end(&self) -> NavPosition {
        self.tokens[self.pos.saturating_sub(1)].range.end
    }

    fn at(&self, s: &str) -> bool {
        matches!(&self.tokens[self.pos].kind, TokKind::Word(x) | TokKind::Symbol(x) if x == s)
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.at(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn need(&mut self, s: &str) -> Result<NavRange> {
        if self.at(s) {
            let r = self.tokens[self.pos].range;
            self.pos += 1;
            Ok(r)
        } else {
            Err(Error::new(self.line(), format!("expected {s:?}")))
        }
    }

    fn name(&mut self) -> Result<IdentSpan> {
        if let TokKind::Word(n) = &self.tokens[self.pos].kind {
            let n = n.clone();
            let range = self.tokens[self.pos].range;
            self.pos += 1;
            if RESERVED_WORDS.contains(&n.as_str()) {
                return Err(Error::new(self.line(), "reserved identifier"));
            }
            Ok(IdentSpan { name: n, range })
        } else {
            Err(Error::new(self.line(), "expected identifier"))
        }
    }

    fn path(&mut self) -> Result<PathSpan> {
        let first = if self.at("crate") {
            let range = self.tokens[self.pos].range;
            self.pos += 1;
            IdentSpan {
                name: "crate".into(),
                range,
            }
        } else {
            self.name()?
        };
        let start = first.range.start;
        let mut segments = vec![first];
        while self.eat("::") {
            segments.push(self.name()?);
        }
        let end = segments.last().unwrap().range.end;
        Ok(PathSpan {
            segments,
            range: NavRange { start, end },
            resolved: None,
        })
    }

    fn capacity(&mut self) -> Result<usize> {
        if let TokKind::Number(n) = self.tokens[self.pos].kind {
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

    fn ty(&mut self) -> Result<TypeSyntax> {
        let start = self.cur_range().start;
        if self.eat("[") {
            let element = self.path()?;
            let elem_name = element.joined();
            if Integer::parse(&elem_name).is_none() && self.eat("]") {
                let index = if let Some(i) = self.type_names.iter().position(|s| s == &elem_name) {
                    i
                } else {
                    self.type_names.push(elem_name);
                    self.type_names.len() - 1
                };
                return Ok(TypeSyntax {
                    ty: Type::VectorView(index),
                    path: Some(element),
                    range: NavRange {
                        start,
                        end: self.prev_end(),
                    },
                });
            }
            let n = Integer::parse(&elem_name).ok_or_else(|| {
                Error::new(self.line(), "fixed arrays require an integer element type")
            })?;
            if self.eat("]") {
                return Ok(TypeSyntax {
                    ty: Type::Slice(n),
                    path: None,
                    range: NavRange {
                        start,
                        end: self.prev_end(),
                    },
                });
            }
            self.need(";")?;
            let len = self.capacity()?;
            let end = self.need("]")?.end;
            if len > 65536 / (n.bits() / 8) {
                return Err(Error::new(self.line(), "fixed array exceeds 64 KiB"));
            }
            return Ok(TypeSyntax {
                ty: Type::Fixed(n, len),
                path: None,
                range: NavRange { start, end },
            });
        }

        let path = self.path()?;
        let n = path.joined();
        if n == "id" {
            self.need("(")?;
            let inner = self.ty()?;
            let Type::Named(arena) = inner.ty else {
                return Err(Error::new(self.line(), "id requires a nominal arena type"));
            };
            let end = self.need(")")?.end;
            return Ok(TypeSyntax {
                ty: Type::Id(arena),
                path: inner.path,
                range: NavRange { start, end },
            });
        }
        if n == "task" {
            self.need("<")?;
            let inner = self.ty()?;
            let ty = Type::task_of(inner.ty)
                .ok_or_else(|| Error::new(self.line(), "unsupported task result type"))?;
            let end = self.need(">")?.end;
            return Ok(TypeSyntax {
                ty,
                path: inner.path,
                range: NavRange { start, end },
            });
        }
        let range = path.range;
        if n == "Self" {
            if let Some(owner) = &self.impl_owner {
                return Ok(TypeSyntax {
                    ty: owner.ty,
                    path: None,
                    range,
                });
            }
        }
        if let Some(int_ty) = Integer::parse(&n) {
            return Ok(TypeSyntax {
                ty: int_ty.ty(),
                path: None,
                range,
            });
        }
        if let Some(float_ty) = Float::parse(&n) {
            return Ok(TypeSyntax {
                ty: Type::Float(float_ty),
                path: None,
                range,
            });
        }
        let ty = match n.as_str() {
            "int" => Type::Int,
            "bool" => Type::Bool,
            "text" => Type::Text,
            "str" => Type::TextView,
            "ints" => Type::Ints,
            "texts" => Type::Texts,
            _ => {
                let index = if let Some(i) = self.type_names.iter().position(|s| s == &n) {
                    i
                } else {
                    self.type_names.push(n);
                    self.type_names.len() - 1
                };
                return Ok(TypeSyntax {
                    ty: Type::Named(index),
                    path: Some(path),
                    range,
                });
            }
        };
        Ok(TypeSyntax {
            ty,
            path: None,
            range,
        })
    }

    fn data(&mut self, public: bool, start_pos: NavPosition) -> Result<NavDataDecl> {
        let line = self.line();
        let is_soa = self.eat("soa");
        let is_vector = self.eat("vector");
        if is_soa && !is_vector {
            return Err(Error::new(line, "soa requires vector"));
        }
        let is_arena = !is_vector && self.eat("arena");
        let is_channel = !is_vector && !is_arena && self.eat("channel");
        let is_map = !is_vector && !is_arena && !is_channel && self.eat("map");
        let is_enum = !is_vector && !is_arena && !is_map && !is_channel && self.eat("enum");
        if !is_enum && !is_vector && !is_map && !is_arena && !is_channel {
            self.need("struct")?;
        }
        let ident = self.name()?;
        if !ident.name.as_bytes()[0].is_ascii_uppercase() {
            return Err(Error::new(
                line,
                "data type names must start with an uppercase letter",
            ));
        }
        if is_vector || is_map || is_arena || is_channel {
            self.need("(")?;
            let ty = self.ty()?;
            let kind = if is_map {
                self.need(",")?;
                NavDataKind::Map(ty, self.ty()?)
            } else if is_arena {
                NavDataKind::Arena(ty)
            } else if is_channel {
                NavDataKind::Channel(ty)
            } else if is_soa {
                NavDataKind::SoaVector(ty)
            } else {
                NavDataKind::Vector(ty)
            };
            self.need(")")?;
            let end = self.need(";")?.end;
            return Ok(NavDataDecl {
                public,
                ident,
                kind,
                range: NavRange {
                    start: start_pos,
                    end,
                },
            });
        }
        self.need("{")?;
        if is_enum {
            let mut variants = Vec::new();
            while !self.at("}") {
                let v_ident = self.name()?;
                let v_start = v_ident.range.start;
                let (payload, v_end) = if self.eat("(") {
                    let ty = self.ty()?;
                    let end = self.need(")")?.end;
                    if ty.ty.scoped_view() {
                        return Err(Error::new(
                            line,
                            "scoped slices cannot be stored in data types",
                        ));
                    }
                    (Some(ty), end)
                } else {
                    (None, v_ident.range.end)
                };
                variants.push(NavVariantDecl {
                    ident: v_ident,
                    payload,
                    range: NavRange {
                        start: v_start,
                        end: v_end,
                    },
                });
                if !self.eat(",") {
                    break;
                }
            }
            let end_pos = self.need("}")?.end;
            if variants.is_empty() {
                return Err(Error::new(
                    line,
                    "data types require at least one field or variant",
                ));
            }
            Ok(NavDataDecl {
                public,
                ident,
                kind: NavDataKind::Enum(variants),
                range: NavRange {
                    start: start_pos,
                    end: end_pos,
                },
            })
        } else {
            let mut fields = Vec::new();
            while !self.at("}") {
                let f_ident = self.name()?;
                let f_start = f_ident.range.start;
                self.need(":")?;
                let ty = self.ty()?;
                if ty.ty.scoped_view() {
                    return Err(Error::new(
                        line,
                        "scoped slices cannot be stored in data types",
                    ));
                }
                let f_end = ty.range.end;
                fields.push(NavFieldDecl {
                    ident: f_ident,
                    ty,
                    range: NavRange {
                        start: f_start,
                        end: f_end,
                    },
                });
                if !self.eat(",") {
                    break;
                }
            }
            let end_pos = self.need("}")?.end;
            if fields.is_empty() {
                return Err(Error::new(
                    line,
                    "data types require at least one field or variant",
                ));
            }
            Ok(NavDataDecl {
                public,
                ident,
                kind: NavDataKind::Record(fields),
                range: NavRange {
                    start: start_pos,
                    end: end_pos,
                },
            })
        }
    }

    fn function(
        &mut self,
        public: bool,
        is_test: bool,
        external: bool,
        start_pos: NavPosition,
    ) -> Result<NavFunction> {
        let line = self.line();
        self.need(if is_test { "test" } else { "fn" })?;
        let ident = self.name()?;
        self.need("(")?;
        let mut params = Vec::new();
        if !self.at(")") {
            loop {
                let p_start = self.cur_range().start;
                let mutable = self.eat("mut");
                let p_ident = self.name()?;
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
                if ty.ty.scoped_view() && borrow == Borrow::Owned {
                    return Err(Error::new(
                        line,
                        "slice parameters require & or &mut; slices cannot be owned",
                    ));
                }
                let mut p_end = ty.range.end;
                let view = if self.eat(".") {
                    if borrow == Borrow::Owned || !matches!(ty.ty, Type::Named(_)) {
                        return Err(Error::new(
                            line,
                            "view contracts require a borrowed record parameter",
                        ));
                    }
                    self.need("{")?;
                    let mut fields = Vec::new();
                    loop {
                        let v_mut = self.eat("mut");
                        if v_mut && borrow != Borrow::Exclusive {
                            return Err(Error::new(
                                line,
                                "mutable view field requires an &mut parameter",
                            ));
                        }
                        let mut segments = vec![self.name()?];
                        while self.eat(".") {
                            segments.push(self.name()?);
                        }
                        fields.push(NavViewField {
                            segments,
                            mutable: v_mut,
                        });
                        if !self.eat(",") {
                            break;
                        }
                    }
                    p_end = self.need("}")?.end;
                    Some(fields)
                } else {
                    None
                };
                params.push(NavParam {
                    ident: p_ident,
                    ty,
                    mutable,
                    borrow,
                    view,
                    range: NavRange {
                        start: p_start,
                        end: p_end,
                    },
                });
                if !self.eat(",") {
                    break;
                }
            }
        }
        self.need(")")?;
        let ret = if self.eat("->") {
            Some(self.ty()?)
        } else {
            None
        };
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
        let (body, end_pos) = if external {
            (Vec::new(), self.need(";")?.end)
        } else {
            self.block()?
        };
        Ok(NavFunction {
            method_owner: self.impl_owner.clone(),
            public,
            is_test,
            ident,
            params,
            ret,
            requires,
            ensures,
            body,
            range: NavRange {
                start: start_pos,
                end: end_pos,
            },
        })
    }

    fn block(&mut self) -> Result<(Vec<NavStmt>, NavPosition)> {
        self.depth += 1;
        if self.depth > 96 {
            return Err(Error::new(self.line(), "nesting limit exceeded"));
        }
        self.need("{")?;
        let mut body = Vec::new();
        while !self.at("}") {
            if self.tokens[self.pos].kind == TokKind::End {
                return Err(Error::new(self.line(), "unclosed block"));
            }
            body.push(self.stmt()?);
        }
        let end_pos = self.need("}")?.end;
        self.depth -= 1;
        Ok((body, end_pos))
    }

    fn stmt(&mut self) -> Result<NavStmt> {
        let start = self.cur_range().start;
        let line = self.line();
        if self.eat("for") {
            let ident = self.name()?;
            self.need("in")?;
            let source = self.expr(0)?;
            let (owner, mutable, selection) = if self.eat("..") {
                (source, false, NavSelection::IntegerRange(self.expr(0)?))
            } else if let NavExprKind::Borrow(segments, mutable) = source.kind {
                let mut parts = segments.into_iter();
                let first = parts.next().unwrap();
                let mut owner = NavExpr {
                    range: first.range,
                    kind: NavExprKind::Var(first),
                };
                for field in parts {
                    owner = NavExpr {
                        range: NavRange {
                            start: owner.range.start,
                            end: field.range.end,
                        },
                        kind: NavExprKind::Field(Box::new(owner), field),
                    };
                }
                (owner, mutable, NavSelection::Each)
            } else {
                return Err(Error::new(
                    line,
                    "for requires an int range or borrowed vector",
                ));
            };
            let (body, end) = self.block()?;
            return Ok(NavStmt {
                range: NavRange { start, end },
                kind: NavStmtKind::ScopedBinding {
                    ident,
                    mutable,
                    owner,
                    selection,
                    body,
                },
            });
        }
        if self.eat("borrow") {
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
                let access = if self.eat("get_pair") {
                    ElementAccess::Key
                } else {
                    self.need("at_pair")?;
                    ElementAccess::Index
                };
                self.need("(")?;
                let owner = self.expr(0)?;
                self.need(",")?;
                let left_index = self.expr(0)?;
                self.need(",")?;
                let right_index = self.expr(0)?;
                self.need(")")?;
                let (body, end) = self.block()?;
                // Navigation is lexical: expose both aliases in one nested
                // scope; all disjointness and writeback checks stay in checker.
                let inner = NavStmt {
                    range: NavRange {
                        start: right.range.start,
                        end,
                    },
                    kind: NavStmtKind::ScopedBinding {
                        ident: right,
                        mutable: true,
                        owner: owner.clone(),
                        selection: NavSelection::Element {
                            access,
                            index: right_index,
                        },
                        body,
                    },
                };
                return Ok(NavStmt {
                    range: NavRange { start, end },
                    kind: NavStmtKind::ScopedBinding {
                        ident: left,
                        mutable: true,
                        owner,
                        selection: NavSelection::Element {
                            access,
                            index: left_index,
                        },
                        body: vec![inner],
                    },
                });
            }
            let ident = self.name()?;
            self.need("=")?;
            let access = if self.eat("at") {
                Some(ElementAccess::Index)
            } else if self.eat("get") {
                Some(ElementAccess::Key)
            } else {
                None
            };
            if access.is_none() {
                self.need("region")?;
            }
            self.need("(")?;
            let owner = self.expr(0)?;
            self.need(",")?;
            let first = self.expr(0)?;
            let selection = if let Some(access) = access {
                NavSelection::Element {
                    access,
                    index: first,
                }
            } else {
                self.need(",")?;
                let end = self.expr(0)?;
                NavSelection::Region { start: first, end }
            };
            self.need(")")?;
            let (body, end_pos) = self.block()?;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::ScopedBinding {
                    ident,
                    mutable,
                    owner,
                    selection,
                    body,
                },
            });
        }
        if self.eat("let") {
            let mutable = self.eat("mut");
            let ident = self.name()?;
            self.need("=")?;
            let value = self.expr(0)?;
            let end_pos = self.need(";")?.end;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Let {
                    ident,
                    mutable,
                    value,
                },
            });
        }
        if self.eat("if") {
            // Retain nested If ranges without putting every alternative on
            // the Rust stack. This mirrors the source parser's nesting limit.
            let mut branches = Vec::new();
            let mut branch_start = start;
            let mut extra_depth = 0;
            let (mut alternative, end) = loop {
                let cond = self.expr(0)?;
                let (yes, yes_end) = self.block()?;
                branches.push((branch_start, cond, yes));
                if !self.eat("else") {
                    break (Vec::new(), yes_end);
                }
                branch_start = self.cur_range().start;
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
            for (start, cond, yes) in branches.into_iter().rev() {
                alternative = vec![NavStmt {
                    range: NavRange { start, end },
                    kind: NavStmtKind::If(cond, yes, alternative),
                }];
            }
            return Ok(alternative.pop().unwrap());
        }
        if self.eat("match") {
            let value = self.expr(0)?;
            self.need("{")?;
            let mut arms = Vec::new();
            while !self.at("}") {
                let path = self.path()?;
                let binding = if self.eat("(") {
                    let b = self.name()?;
                    self.need(")")?;
                    Some(b)
                } else {
                    None
                };
                self.need("=>")?;
                let (body, _) = self.block()?;
                self.eat(",");
                arms.push(NavMatchArm {
                    path,
                    binding,
                    body,
                });
            }
            let end_pos = self.need("}")?.end;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Match(value, arms),
            });
        }
        if self.eat("while") {
            let cond = self.expr(0)?;
            let (body, end_pos) = self.block()?;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::While(cond, body),
            });
        }
        if self.eat("break") || self.eat("continue") {
            let end = self.need(";")?.end;
            return Ok(NavStmt {
                range: NavRange { start, end },
                kind: NavStmtKind::LoopJump,
            });
        }
        if self.eat("return") {
            let value = if self.at(";") {
                None
            } else {
                Some(self.expr(0)?)
            };
            let end_pos = self.need(";")?.end;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Return(value),
            });
        }
        if self.eat("print") {
            let e = self.expr(0)?;
            let end_pos = self.need(";")?.end;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Print(e),
            });
        }
        if self.eat("assert") {
            let e = self.expr(0)?;
            let end_pos = self.need(";")?.end;
            return Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Assert(e),
            });
        }

        let e = self.expr(0)?;
        if self.eat("=") {
            let value = self.expr(0)?;
            let end_pos = self.need(";")?.end;
            let kind = match e.kind {
                NavExprKind::Var(ident) => NavStmtKind::Assign {
                    target: ident,
                    value,
                },
                NavExprKind::Field(_, _) => NavStmtKind::AssignField { target: e, value },
                _ => {
                    return Err(Error::new(
                        line,
                        "assignment target must be a variable or record field",
                    ))
                }
            };
            Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind,
            })
        } else {
            let end_pos = self.need(";")?.end;
            Ok(NavStmt {
                range: NavRange {
                    start,
                    end: end_pos,
                },
                kind: NavStmtKind::Expr(e),
            })
        }
    }

    fn expr(&mut self, min: u8) -> Result<NavExpr> {
        self.depth += 1;
        if self.depth > 96 {
            return Err(Error::new(self.line(), "expression nesting limit exceeded"));
        }
        let line = self.line();
        let start = self.cur_range().start;
        let mut e = if self.eat("??") {
            self.need("(")?;
            let ty = self.ty()?;
            self.need(",")?;
            let TokKind::Text(intent) = &self.tokens[self.pos].kind else {
                return Err(Error::new(line, "hole syntax: ??(type, \"intent\")"));
            };
            let intent = intent.clone();
            self.pos += 1;
            let end = self.need(")")?.end;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Hole(ty, intent),
            }
        } else if self.eat("&") {
            let mutable = self.eat("mut");
            let mut segments = vec![self.name()?];
            while self.eat(".") {
                segments.push(self.name()?);
            }
            let end = segments.last().unwrap().range.end;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Borrow(segments, mutable),
            }
        } else if self.eat("!") {
            let inner = self.expr(7)?;
            let end = inner.range.end;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Unary("!".into(), Box::new(inner)),
            }
        } else if self.eat("-") {
            let inner = self.expr(7)?;
            let end = inner.range.end;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Unary("-".into(), Box::new(inner)),
            }
        } else if self.at("try") {
            let token = self.need("try")?;
            let inner = self.expr(7)?;
            let end = inner.range.end;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Try(
                    IdentSpan {
                        name: "try".into(),
                        range: token,
                    },
                    Box::new(inner),
                ),
            }
        } else if self.eat("spawn") {
            let path = self.path()?;
            let (args, end) = self.args()?;
            NavExpr {
                range: NavRange { start, end },
                kind: NavExprKind::Spawn(path, args),
            }
        } else if self.eat("(") {
            let mut inner = self.expr(0)?;
            let end = self.need(")")?.end;
            inner.range = NavRange { start, end };
            inner
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
            let end = self.need("]")?.end;
            NavExpr {
                range: NavRange { start, end },
                kind: if let Some(count) = repeat {
                    NavExprKind::Repeat(Box::new(values.pop().unwrap()), count)
                } else {
                    NavExprKind::Array(values)
                },
            }
        } else {
            let t = self.tokens[self.pos].clone();
            self.pos += 1;
            match t.kind {
                TokKind::Number(_) => NavExpr {
                    range: t.range,
                    kind: NavExprKind::Int,
                },
                TokKind::Float(_) => NavExpr {
                    range: t.range,
                    kind: NavExprKind::Float,
                },
                TokKind::Text(_) => NavExpr {
                    range: t.range,
                    kind: NavExprKind::Text,
                },
                TokKind::Word(s) if s == "true" || s == "false" => NavExpr {
                    range: t.range,
                    kind: NavExprKind::Bool,
                },
                TokKind::Word(s) => {
                    let first_ident = IdentSpan {
                        name: s,
                        range: t.range,
                    };
                    let mut segments = vec![first_ident];
                    while self.eat("::") {
                        segments.push(self.name()?);
                    }
                    let path_end = segments.last().unwrap().range.end;
                    let path = PathSpan {
                        segments,
                        resolved: None,
                        range: NavRange {
                            start: t.range.start,
                            end: path_end,
                        },
                    };
                    if self.at("{")
                        && matches!(
                            self.tokens.get(self.pos + 1).map(|tok| &tok.kind),
                            Some(TokKind::Word(_))
                        )
                        && matches!(
                            self.tokens.get(self.pos + 2).map(|tok| &tok.kind),
                            Some(TokKind::Symbol(v)) if v == ":"
                        )
                    {
                        self.need("{")?;
                        let mut fields = Vec::new();
                        while !self.at("}") {
                            let f_ident = self.name()?;
                            self.need(":")?;
                            let val = self.expr(0)?;
                            fields.push((f_ident, val));
                            if !self.eat(",") {
                                break;
                            }
                        }
                        let end = self.need("}")?.end;
                        NavExpr {
                            range: NavRange { start, end },
                            kind: NavExprKind::Record(path, fields),
                        }
                    } else if self.at("(") {
                        let (args, end) = self.args()?;
                        NavExpr {
                            range: NavRange { start, end },
                            kind: NavExprKind::Call(path, args),
                        }
                    } else {
                        if path.segments.len() > 1 {
                            return Err(Error::new(
                                line,
                                "qualified paths must name a function call",
                            ));
                        }
                        let ident = path.segments.into_iter().next().unwrap();
                        NavExpr {
                            range: ident.range,
                            kind: NavExprKind::Var(ident),
                        }
                    }
                }
                _ => return Err(Error::new(line, "expected expression")),
            }
        };

        let mut operators = 0usize;
        while self.eat(".") {
            let field = self.name()?;
            let start = e.range.start;
            let (kind, end) = if self.at("(") {
                let (args, end) = self.args()?;
                (NavExprKind::MethodCall(Box::new(e), field, args), end)
            } else {
                let end = field.range.end;
                (NavExprKind::Field(Box::new(e), field), end)
            };
            e = NavExpr {
                range: NavRange { start, end },
                kind,
            };
        }

        while let TokKind::Symbol(s) = &self.tokens[self.pos].kind {
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
            let end = right.range.end;
            e = NavExpr {
                range: NavRange {
                    start: e.range.start,
                    end,
                },
                kind: NavExprKind::Binary(op, Box::new(e), Box::new(right)),
            };
        }

        self.depth -= 1;
        Ok(e)
    }

    fn args(&mut self) -> Result<(Vec<NavExpr>, NavPosition)> {
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
        let end = self.need(")")?.end;
        Ok((args, end))
    }
}

fn parse_nav_module(source: &str) -> Result<NavModule> {
    crate::syntax::profile_work(|profile| profile.navigation_parses += 1);
    let mut p = NavParser {
        tokens: lex_spanned(source)?,
        pos: 0,
        depth: 0,
        type_names: Vec::new(),
        impl_owner: None,
    };
    let mut modules = Vec::new();
    let mut types = Vec::new();
    let mut functions = Vec::new();
    let mut implementations = Vec::new();

    while p.tokens[p.pos].kind != TokKind::End {
        let start_pos = p.cur_range().start;
        let line = p.line();
        if p.eat("impl") {
            if p.impl_owner.is_some() {
                return Err(Error::new(line, "nested impl block"));
            }
            let owner = p.ty()?;
            p.need("{")?;
            p.impl_owner = Some(owner.clone());
            implementations.push(owner);
            continue;
        }
        if p.impl_owner.is_some() && p.eat("}") {
            p.impl_owner = None;
            continue;
        }
        if p.eat("mod") {
            let ident = p.name()?;
            let end_pos = p.need(";")?.end;
            modules.push(NavModuleDecl {
                ident,
                range: NavRange {
                    start: start_pos,
                    end: end_pos,
                },
            });
        } else {
            while p.eat("@") {
                let annotation = p.name()?;
                p.need("(")?;
                match annotation.name.as_str() {
                    "effect" => loop {
                        let _effect = p.name()?;
                        if !p.eat(",") {
                            break;
                        }
                    },
                    "intent" => {
                        if matches!(p.tokens[p.pos].kind, TokKind::Text(_)) {
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
                types.push(p.data(public, start_pos)?);
                continue;
            }
            let is_test = p.at("test");
            let external = p.eat("extern");
            functions.push(p.function(public, is_test, external, start_pos)?);
        }
    }

    if p.impl_owner.is_some() {
        return Err(Error::new(p.line(), "unclosed impl block"));
    }
    Ok(NavModule {
        implementations,
        modules,
        types,
        functions,
        type_names: p.type_names,
    })
}

fn relative_file_path(_project: &Project, file: &SourceFile) -> String {
    file.source_key.clone()
}

fn match_file_index(project: &Project, requested: &str) -> Option<usize> {
    let normalized_req = requested.replace('\\', "/");
    for (idx, f) in project.files.iter().enumerate() {
        let rel = relative_file_path(project, f);
        if rel == normalized_req || f.path.to_string_lossy().replace('\\', "/") == normalized_req {
            return Some(idx);
        }
    }
    if project.files.len() == 1 && (requested.is_empty() || requested == "<source>") {
        return Some(0);
    }
    let req_path = std::path::Path::new(requested);
    if let Ok(canon) = req_path.canonicalize() {
        for (idx, f) in project.files.iter().enumerate() {
            if f.path == canon {
                return Some(idx);
            }
        }
    }
    None
}

fn canonical_name(module: &str, name: &str, self_type: Option<Type>, types: &TypeTable) -> String {
    if ["ProcessOutput", "ProcessRun"].contains(&name)
        || name.starts_with("ProcessRun::")
        || NET_PRELUDE_TYPES.contains(&name)
        || name.starts_with("NetResult::")
    {
        return name.into();
    }
    if let Some(Type::Named(id)) = self_type {
        if name == "Self" {
            return types.definitions[id].name.clone();
        }
        if let Some(tail) = name.strip_prefix("Self::") {
            return format!("{}::{tail}", types.definitions[id].name);
        }
    }
    qualify(module, name)
}
fn function_name(f: &NavFunction, module: &str, map: &[Option<Type>], types: &TypeTable) -> String {
    if let Some(TypeSyntax {
        ty: Type::Named(local),
        ..
    }) = &f.method_owner
    {
        if let Some(Some(Type::Named(id))) = map.get(*local) {
            return format!("{}::{}", types.definitions[*id].name, f.ident.name);
        }
    }
    qualify(module, &f.ident.name)
}
pub(crate) fn synthesis_function_name(module: &str, name: &str, owner: Option<&str>) -> String {
    let qualified = owner.map_or_else(
        || qualify(module, name),
        |owner| format!("{}::{name}", qualify(module, owner)),
    );
    if module.is_empty() {
        format!("crate::{qualified}")
    } else {
        qualified
    }
}

fn qualify(module: &str, name: &str) -> String {
    if let Some(root) = name.strip_prefix("crate::") {
        root.into()
    } else if module.is_empty() {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}

struct FileNavigation {
    file_rel: String,
    file_path: String,
    outline: Vec<SymbolInfo>,
    references: Vec<ReferenceSpan>,
}

struct ProjectNavigation {
    fixed_receivers: BTreeSet<String>,
    files: Vec<FileNavigation>,
    symbols_by_id: BTreeMap<String, SymbolInfo>,
    decl_keys_by_id: BTreeMap<String, DeclKey>,
    overlapping_local_collision: Option<(usize, usize, String)>,
}

#[derive(Clone, Debug)]
struct FnSig {
    method_owner: Option<usize>,
    id: String,
    owner: String,
    public: bool,
    ret: Type,
    is_test: bool,
}

#[derive(Clone, Debug)]
struct LocalBinding {
    id: String,
    ty: Option<Type>,
}

struct ScopeStack {
    self_type: Option<Type>,
    frames: Vec<BTreeMap<String, LocalBinding>>,
}

impl ScopeStack {
    fn new() -> Self {
        Self {
            frames: vec![BTreeMap::new()],
            self_type: None,
        }
    }

    fn push(&mut self) {
        self.frames.push(BTreeMap::new());
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn insert(&mut self, name: String, binding: LocalBinding) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, binding);
        }
    }

    fn lookup(&self, name: &str) -> Option<&LocalBinding> {
        for frame in self.frames.iter().rev() {
            if let Some(b) = frame.get(name) {
                return Some(b);
            }
        }
        None
    }
}

fn format_type_label(ty: Type, types: &TypeTable) -> String {
    if let Some(result) = ty.task_result() {
        return format!("task<{}>", format_type_label(result, types));
    }
    match ty {
        Type::Id(id) => format!(
            "id({})",
            types
                .definitions
                .get(id)
                .map_or("unknown", |d| d.name.as_str())
        ),
        Type::Named(id) => types
            .definitions
            .get(id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "unknown".into()),
        Type::VectorView(id) => format!(
            "[{}]",
            types
                .definitions
                .get(id)
                .map_or("unknown", |d| d.name.as_str())
        ),
        other => other.label(),
    }
}

fn format_param_detail(param: &NavParam, remapped_ty: Type, types: &TypeTable) -> String {
    let prefix = match param.borrow {
        Borrow::Owned => {
            if param.mutable {
                "mut "
            } else {
                ""
            }
        }
        Borrow::Shared => "&",
        Borrow::Exclusive => "&mut ",
    };
    let ty_str = format_type_label(remapped_ty, types);
    let view_str = if let Some(view) = &param.view {
        let items = view
            .iter()
            .map(|vf| {
                let path = vf
                    .segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                if vf.mutable {
                    format!("mut {path}")
                } else {
                    path
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(".{{{items}}}")
    } else {
        String::new()
    };
    format!("{prefix}{ty_str}{view_str}")
}

// Keep original token spans while resolving package aliases exactly as the
// ordinary project loader does. The explicit work stack also handles long
// else-if chains without adding recursive traversal frames.
fn qualify_navigation_paths(project: &Project, file: &SourceFile, module: &mut NavModule) {
    fn path(project: &Project, file: &SourceFile, path: &mut PathSpan) {
        let raw = path.joined();
        if raw.contains("::")
            && !raw.starts_with("Self::")
            && !raw.starts_with("ReadResult::")
            && !raw.starts_with("WriteResult::")
            && !raw.starts_with("FileRead::")
            && !raw.starts_with("ProcessRun::")
            && !raw.starts_with("NetResult::")
        {
            path.resolved = Some(format!("crate::{}", project.qualified_for(file, &raw)));
        }
    }
    fn ty(project: &Project, file: &SourceFile, ty: &mut TypeSyntax) {
        if let Some(p) = &mut ty.path {
            path(project, file, p);
        }
    }
    enum Item<'a> {
        Expr(&'a mut NavExpr),
        Stmt(&'a mut NavStmt),
    }
    let mut work = Vec::new();
    for name in &mut module.type_names {
        if name.contains("::") && !name.starts_with("Self::") {
            *name = format!("crate::{}", project.qualified_for(file, name));
        }
    }
    for implementation in &mut module.implementations {
        ty(project, file, implementation);
    }
    for declaration in &mut module.types {
        match &mut declaration.kind {
            NavDataKind::Arena(t)
            | NavDataKind::Channel(t)
            | NavDataKind::Vector(t)
            | NavDataKind::SoaVector(t) => ty(project, file, t),
            NavDataKind::Map(a, b) => {
                ty(project, file, a);
                ty(project, file, b);
            }
            NavDataKind::Record(fields) => {
                for field in fields {
                    ty(project, file, &mut field.ty);
                }
            }
            NavDataKind::Enum(variants) => {
                for variant in variants {
                    if let Some(t) = &mut variant.payload {
                        ty(project, file, t);
                    }
                }
            }
        }
    }
    for function in &mut module.functions {
        if let Some(owner) = &mut function.method_owner {
            ty(project, file, owner);
        }
        for param in &mut function.params {
            ty(project, file, &mut param.ty);
        }
        if let Some(ret) = &mut function.ret {
            ty(project, file, ret);
        }
        work.extend(
            function
                .requires
                .iter_mut()
                .chain(&mut function.ensures)
                .map(Item::Expr),
        );
        work.extend(function.body.iter_mut().map(Item::Stmt));
    }
    while let Some(item) = work.pop() {
        match item {
            Item::Expr(expr) => match &mut expr.kind {
                NavExprKind::Call(p, args) | NavExprKind::Spawn(p, args) => {
                    path(project, file, p);
                    work.extend(args.iter_mut().map(Item::Expr));
                }
                NavExprKind::Record(p, fields) => {
                    path(project, file, p);
                    work.extend(fields.iter_mut().map(|(_, v)| Item::Expr(v)));
                }
                NavExprKind::Hole(t, _) => ty(project, file, t),
                NavExprKind::Array(values) => work.extend(values.iter_mut().map(Item::Expr)),
                NavExprKind::MethodCall(owner, _, args) => {
                    work.push(Item::Expr(owner));
                    work.extend(args.iter_mut().map(Item::Expr));
                }
                NavExprKind::Try(_, e)
                | NavExprKind::Repeat(e, _)
                | NavExprKind::Unary(_, e)
                | NavExprKind::Field(e, _) => work.push(Item::Expr(e)),
                NavExprKind::Binary(_, a, b) => {
                    work.push(Item::Expr(a));
                    work.push(Item::Expr(b));
                }
                _ => {}
            },
            Item::Stmt(stmt) => match &mut stmt.kind {
                NavStmtKind::ScopedBinding {
                    owner,
                    selection,
                    body,
                    ..
                } => {
                    work.push(Item::Expr(owner));
                    match selection {
                        NavSelection::Region { start, end } => {
                            work.push(Item::Expr(start));
                            work.push(Item::Expr(end));
                        }
                        NavSelection::Element { index, .. } | NavSelection::IntegerRange(index) => {
                            work.push(Item::Expr(index))
                        }
                        NavSelection::Each => {}
                    }
                    work.extend(body.iter_mut().map(Item::Stmt));
                }
                NavStmtKind::Let { value, .. }
                | NavStmtKind::Assign { value, .. }
                | NavStmtKind::Expr(value)
                | NavStmtKind::Print(value)
                | NavStmtKind::Assert(value) => work.push(Item::Expr(value)),
                NavStmtKind::AssignField { target, value } => {
                    work.push(Item::Expr(target));
                    work.push(Item::Expr(value));
                }
                NavStmtKind::Return(value) => {
                    if let Some(value) = value {
                        work.push(Item::Expr(value));
                    }
                }
                NavStmtKind::If(e, yes, no) => {
                    work.push(Item::Expr(e));
                    work.extend(yes.iter_mut().chain(no).map(Item::Stmt));
                }
                NavStmtKind::While(e, body) => {
                    work.push(Item::Expr(e));
                    work.extend(body.iter_mut().map(Item::Stmt));
                }
                NavStmtKind::Match(e, arms) => {
                    work.push(Item::Expr(e));
                    for arm in arms {
                        path(project, file, &mut arm.path);
                        work.extend(arm.body.iter_mut().map(Item::Stmt));
                    }
                }
                NavStmtKind::LoopJump => {}
            },
        }
    }
}

fn build_project_navigation(project: &Project) -> Result<ProjectNavigation> {
    let mut parsed_files = Vec::with_capacity(project.files.len());
    for file in &project.files {
        let mut nav_mod = parse_nav_module(&file.source).map_err(|e| e.in_file(&file.path))?;
        qualify_navigation_paths(project, file, &mut nav_mod);
        parsed_files.push(nav_mod);
    }

    // 1. Build type registry and TypeTable so all record/enum IDs and field types are known.
    let mut type_registry = BTreeMap::<String, (usize, String, bool, String)>::new();
    for name in ["ReadResult", "WriteResult", "ProcessOutput", "ProcessRun"] {
        type_registry.insert(
            name.into(),
            (
                type_registry.len(),
                String::new(),
                true,
                if name.starts_with("Process") {
                    format!("builtin:{name}")
                } else {
                    String::new()
                },
            ),
        );
    }

    for (file_idx, file) in project.files.iter().enumerate() {
        let nav_mod = &parsed_files[file_idx];
        for d in &nav_mod.types {
            let key = qualify(&file.module, &d.ident.name);
            let is_enum = matches!(d.kind, NavDataKind::Enum(_));
            let sym_id = format!("{}:{key}", if is_enum { "enum" } else { "struct" });
            let id = type_registry.len();
            type_registry.insert(key, (id, file.module.clone(), d.public, sym_id));
        }
    }
    // 1.2 (contract N5): the networking prelude takes the last two ids, after
    // every user type, exactly like the compiler's type registry.
    let net_response = type_registry.len();
    for name in NET_PRELUDE_TYPES {
        type_registry.insert(
            name.into(),
            (
                type_registry.len(),
                String::new(),
                true,
                format!("builtin:{name}"),
            ),
        );
    }

    let resolve_type_key = |name: &str, module: &str| -> Option<(usize, String, String)> {
        let key = if name == "FileRead" {
            "ReadResult".to_owned()
        } else if ["ReadResult", "WriteResult", "ProcessOutput", "ProcessRun"].contains(&name)
            || NET_PRELUDE_TYPES.contains(&name)
        {
            name.to_owned()
        } else {
            qualify(module, name)
        };
        let (id, _, _, sym_id) = type_registry.get(&key)?;
        Some((*id, key, sym_id.clone()))
    };

    // Map per-file `type_names` to `Option<Type>` (with `Type::Named(global_id)`).
    let mut file_type_maps: Vec<Vec<Option<Type>>> = Vec::with_capacity(project.files.len());
    for (file_idx, file) in project.files.iter().enumerate() {
        let nav_mod = &parsed_files[file_idx];
        let map = nav_mod
            .type_names
            .iter()
            .map(|name| resolve_type_key(name, &file.module).map(|(id, _, _)| Type::Named(id)))
            .collect::<Vec<_>>();
        file_type_maps.push(map);
    }

    fn remap_opt(ty: Type, map: &[Option<Type>]) -> Option<Type> {
        if let Some(result) = ty.task_result() {
            return Type::task_of(remap_opt(result, map)?);
        }
        match ty {
            Type::Named(local_idx) => map.get(local_idx).copied().flatten(),
            Type::Id(local_idx) => match map.get(local_idx).copied().flatten()? {
                Type::Named(id) => Some(Type::Id(id)),
                _ => None,
            },
            Type::VectorView(local_idx) => match map.get(local_idx).copied().flatten()? {
                Type::Named(id) => Some(Type::VectorView(id)),
                _ => None,
            },
            other => Some(other),
        }
    }

    let mut definitions = vec![
        DataDecl {
            name: "ReadResult".into(),
            public: true,
            line: 0,
            kind: DataKind::Enum(vec![
                FieldDecl {
                    name: "Ok".into(),
                    ty: Type::Text,
                },
                FieldDecl {
                    name: "Err".into(),
                    ty: Type::Text,
                },
            ]),
        },
        DataDecl {
            name: "WriteResult".into(),
            public: true,
            line: 0,
            kind: DataKind::Enum(vec![
                FieldDecl {
                    name: "Ok".into(),
                    ty: Type::Unit,
                },
                FieldDecl {
                    name: "Err".into(),
                    ty: Type::Text,
                },
            ]),
        },
    ];
    definitions.extend([
        DataDecl {
            name: "ProcessOutput".into(),
            public: true,
            line: 0,
            kind: DataKind::Record(vec![
                FieldDecl {
                    name: "status".into(),
                    ty: Type::Int,
                },
                FieldDecl {
                    name: "stdout".into(),
                    ty: Type::Text,
                },
                FieldDecl {
                    name: "stderr".into(),
                    ty: Type::Text,
                },
            ]),
        },
        DataDecl {
            name: "ProcessRun".into(),
            public: true,
            line: 0,
            kind: DataKind::Enum(vec![
                FieldDecl {
                    name: "Output".into(),
                    ty: Type::Named(2),
                },
                FieldDecl {
                    name: "Error".into(),
                    ty: Type::Text,
                },
            ]),
        },
    ]);
    definitions.resize(
        type_registry.len(),
        DataDecl {
            name: String::new(),
            public: true,
            line: 0,
            kind: DataKind::Record(Vec::new()),
        },
    );
    // The exact layout `TypeTable::net_result` requires (contract N5).
    definitions[net_response] = DataDecl {
        name: "NetResponse".into(),
        public: true,
        line: 0,
        kind: DataKind::Record(vec![
            FieldDecl {
                name: "status".into(),
                ty: Type::Int,
            },
            FieldDecl {
                name: "body".into(),
                ty: Type::Text,
            },
        ]),
    };
    definitions[net_response + 1] = DataDecl {
        name: "NetResult".into(),
        public: true,
        line: 0,
        kind: DataKind::Enum(vec![
            FieldDecl {
                name: "Response".into(),
                ty: Type::Named(net_response),
            },
            FieldDecl {
                name: "Error".into(),
                ty: Type::Text,
            },
        ]),
    };

    for (file_idx, file) in project.files.iter().enumerate() {
        let nav_mod = &parsed_files[file_idx];
        let map = &file_type_maps[file_idx];
        for d in &nav_mod.types {
            let key = qualify(&file.module, &d.ident.name);
            let Some(&(id, _, _, _)) = type_registry.get(&key) else {
                continue;
            };
            let kind = match &d.kind {
                NavDataKind::Channel(ty) => DataKind::Channel(vec![FieldDecl {
                    name: "element".into(),
                    ty: remap_opt(ty.ty, map).unwrap_or(Type::Unit),
                }]),
                NavDataKind::Arena(ty) => DataKind::Arena(vec![FieldDecl {
                    name: "element".into(),
                    ty: remap_opt(ty.ty, map).unwrap_or(Type::Unit),
                }]),
                NavDataKind::Vector(ty) => DataKind::Vector(vec![FieldDecl {
                    name: "element".into(),
                    ty: remap_opt(ty.ty, map).unwrap_or(Type::Unit),
                }]),
                NavDataKind::SoaVector(ty) => DataKind::SoaVector(vec![FieldDecl {
                    name: "element".into(),
                    ty: remap_opt(ty.ty, map).unwrap_or(Type::Unit),
                }]),
                NavDataKind::Map(key, value) => DataKind::Map(vec![
                    FieldDecl {
                        name: "key".into(),
                        ty: remap_opt(key.ty, map).unwrap_or(Type::Unit),
                    },
                    FieldDecl {
                        name: "value".into(),
                        ty: remap_opt(value.ty, map).unwrap_or(Type::Unit),
                    },
                ]),
                NavDataKind::Record(fields) => DataKind::Record(
                    fields
                        .iter()
                        .map(|f| FieldDecl {
                            name: f.ident.name.clone(),
                            ty: remap_opt(f.ty.ty, map).unwrap_or(Type::Unit),
                        })
                        .collect(),
                ),
                NavDataKind::Enum(variants) => DataKind::Enum(
                    variants
                        .iter()
                        .map(|v| FieldDecl {
                            name: v.ident.name.clone(),
                            ty: v
                                .payload
                                .as_ref()
                                .and_then(|p| remap_opt(p.ty, map))
                                .unwrap_or(Type::Unit),
                        })
                        .collect(),
                ),
            };
            definitions[id] = DataDecl {
                name: key,
                public: d.public,
                line: d.ident.range.start.line,
                kind,
            };
        }
    }

    let types = TypeTable {
        definitions,
        layouts: Vec::new(),
        order: Vec::new(),
    };

    // 2. Register module files and functions across the project.
    let mut module_symbols = BTreeMap::<String, String>::new();
    let mut symbols_by_id = BTreeMap::<String, SymbolInfo>::new();
    let mut decl_keys_by_id = BTreeMap::<String, DeclKey>::new();
    for (ordinal, (name, detail)) in CHANNEL_BUILTIN_DETAILS.iter().copied()
        .chain(PROCESS_BUILTIN_DETAILS.iter().copied())
        .chain(INTEGER_BUILTIN_DETAILS.iter().copied())
        .chain(std::iter::once(("try", "try expression; unwraps the success payload or returns a compatible error after lexical owner/loan cleanup and postcondition checks")))
        .chain(FRONTIER3_BUILTIN_DETAILS.iter().copied())
        .chain(V130_BUILTIN_DETAILS.iter().copied())
        .enumerate()
    {
        let kind = if name == "try" { "keyword" } else { "builtin" };
        let id = format!("{kind}:{name}");
        let position = NavPosition {line:1,column:1,byte:0};
        let range = NavRange {start:position,end:position};
        symbols_by_id.insert(id.clone(),SymbolInfo {id:id.clone(),name:name.into(),qualified_name:name.into(),kind,detail:detail.into(),public:false,file:String::new(),path:String::new(),range,selection_range:range,children:Vec::new()});
        decl_keys_by_id.insert(id,DeclKey {file_idx:usize::MAX,decl_ordinal:ordinal,kind});
    }
    for (ordinal, (id, name, detail)) in [
        (
            "field:ProcessOutput::status",
            "status",
            "ProcessOutput.status: int; exact child exit status",
        ),
        (
            "field:ProcessOutput::stdout",
            "stdout",
            "ProcessOutput.stdout: text; owned captured standard output",
        ),
        (
            "field:ProcessOutput::stderr",
            "stderr",
            "ProcessOutput.stderr: text; owned captured standard error",
        ),
        (
            "variant:ProcessRun::Output",
            "Output",
            "ProcessRun::Output(ProcessOutput); completed child, including nonzero exit status",
        ),
        (
            "variant:ProcessRun::Error",
            "Error",
            "ProcessRun::Error(text); recoverable launch or capture error",
        ),
        (
            "field:NetResponse::status",
            "status",
            "NetResponse.status: int; HTTP status (100-599, or the loopback status/NNN route); 0 for raw tcp://",
        ),
        (
            "field:NetResponse::body",
            "body",
            "NetResponse.body: text; owned response body (valid UTF-8, at most 16 MiB)",
        ),
        (
            "variant:NetResult::Response",
            "Response",
            "NetResult::Response(NetResponse); completed request, including non-2xx statuses",
        ),
        (
            "variant:NetResult::Error",
            "Error",
            "NetResult::Error(text); recoverable request error (rejected endpoint, connection, timeout or invalid response)",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let position = NavPosition {
            line: 1,
            column: 1,
            byte: 0,
        };
        let range = NavRange {
            start: position,
            end: position,
        };
        symbols_by_id.insert(
            id.into(),
            SymbolInfo {
                id: id.into(),
                name: name.into(),
                qualified_name: id.split_once(':').unwrap().1.into(),
                kind: "builtin",
                detail: detail.into(),
                public: true,
                file: String::new(),
                path: String::new(),
                range,
                selection_range: range,
                children: Vec::new(),
            },
        );
        decl_keys_by_id.insert(
            id.into(),
            DeclKey {
                file_idx: usize::MAX,
                decl_ordinal: 100 + ordinal,
                kind: "builtin",
            },
        );
    }
    let mut overlapping_local_collision: Option<(usize, usize, String)> = None;

    for (file_idx, file) in project.files.iter().enumerate() {
        if !file.module.is_empty() {
            let rel = relative_file_path(project, file);
            let full = file.path.to_string_lossy().into_owned();
            let mod_id = format!("module:{}", file.module);
            let short_name = file
                .module
                .rsplit_once("::")
                .map_or(file.module.as_str(), |(_, n)| n)
                .to_owned();
            let pos = NavPosition {
                line: 1,
                column: 1,
                byte: 0,
            };
            let range = NavRange {
                start: pos,
                end: pos,
            };
            let info = SymbolInfo {
                id: mod_id.clone(),
                name: short_name,
                qualified_name: file.module.clone(),
                kind: "module",
                detail: format!("mod {}", file.module),
                public: false,
                file: rel,
                path: full,
                range,
                selection_range: range,
                children: Vec::new(),
            };
            module_symbols.insert(file.module.clone(), mod_id.clone());
            decl_keys_by_id.insert(
                mod_id.clone(),
                DeclKey {
                    file_idx,
                    decl_ordinal: 0,
                    kind: "module",
                },
            );
            symbols_by_id.insert(mod_id, info);
        }
    }

    // Prefix spans retain the user's spelling while targets use package
    // namespaces. Register the loader's alias mapping in each lexical module.
    let aliases = project
        .files
        .iter()
        .flat_map(|f| f.module.split("::"))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>();
    let canonical_modules = module_symbols.clone();
    for file in &project.files {
        module_symbols.insert(format!("@crate:{}", file.module), file.package.clone());
        for alias in &aliases {
            let probe = project.qualified_for(file, &format!("{alias}::__navigation"));
            let namespace = probe.strip_suffix("::__navigation").unwrap();
            for (target, symbol) in &canonical_modules {
                if let Some(suffix) = target
                    .strip_prefix(namespace)
                    .filter(|s| s.is_empty() || s.starts_with("::"))
                {
                    module_symbols.insert(
                        qualify(&file.module, &format!("{alias}{suffix}")),
                        symbol.clone(),
                    );
                }
            }
        }
    }
    let mut fn_registry = BTreeMap::<String, FnSig>::new();
    for (file_idx, file) in project.files.iter().enumerate() {
        let nav_mod = &parsed_files[file_idx];
        let map = &file_type_maps[file_idx];
        for f in &nav_mod.functions {
            let qname = function_name(f, &file.module, map, &types);
            let sym_id = format!("{}:{qname}", if f.is_test { "test" } else { "fn" });
            let ret_ty = f
                .ret
                .as_ref()
                .and_then(|r| remap_opt(r.ty, map))
                .unwrap_or(Type::Unit);
            fn_registry.insert(
                qname,
                FnSig {
                    method_owner: f
                        .method_owner
                        .as_ref()
                        .and_then(|o| remap_opt(o.ty, map))
                        .and_then(|t| {
                            if let Type::Named(id) = t {
                                Some(id)
                            } else {
                                None
                            }
                        }),
                    id: sym_id,
                    owner: file.module.clone(),
                    public: f.public,
                    ret: ret_ty,
                    is_test: f.is_test,
                },
            );
        }
    }

    // 3. Build symbol declarations and resolve references per file.
    let mut nav_files = Vec::with_capacity(project.files.len());
    let mut fixed_receivers = BTreeSet::new();

    for (file_idx, file) in project.files.iter().enumerate() {
        let nav_mod = &parsed_files[file_idx];
        let map = &file_type_maps[file_idx];
        let file_rel = relative_file_path(project, file);
        let file_path = file.path.to_string_lossy().into_owned();
        let mut outline = Vec::new();
        let mut references = Vec::new();
        let mut decl_ordinal = 0usize;

        // Helper to record a reference from a TypeSyntax if it refers to a user-defined type.
        let record_type_ref = |ts: &TypeSyntax, references: &mut Vec<ReferenceSpan>| {
            if let Some(path) = &ts.path {
                let raw_name = path.joined();
                if let Some((_, _, sym_id)) = resolve_type_key(&raw_name, &file.module) {
                    if path.segments.len() > 1 {
                        record_module_prefix_refs(
                            &path.segments[..path.segments.len() - 1],
                            &file.module,
                            &module_symbols,
                            references,
                        );
                    }
                    if !sym_id.is_empty() {
                        if let Some(last_seg) = path.segments.last() {
                            references.push(ReferenceSpan {
                                range: last_seg.range,
                                symbol_id: sym_id,
                                is_declaration: false,
                            });
                        }
                    }
                }
            }
        };

        // 3a. Module declarations (`mod child;`)
        for m in &nav_mod.modules {
            let child_mod = qualify(&file.module, &m.ident.name);
            let mod_id = format!("module:{child_mod}");
            decl_ordinal += 1;
            let decl_info = SymbolInfo {
                id: mod_id.clone(),
                name: m.ident.name.clone(),
                qualified_name: child_mod.clone(),
                kind: "module",
                detail: format!("mod {}", m.ident.name),
                public: false,
                file: file_rel.clone(),
                path: file_path.clone(),
                range: m.range,
                selection_range: m.ident.range,
                children: Vec::new(),
            };
            outline.push(decl_info.clone());
            decl_keys_by_id.entry(mod_id.clone()).or_insert(DeclKey {
                file_idx,
                decl_ordinal,
                kind: "module",
            });
            symbols_by_id.entry(mod_id.clone()).or_insert(decl_info);
            references.push(ReferenceSpan {
                range: m.ident.range,
                symbol_id: mod_id,
                is_declaration: true,
            });
        }

        // 3b. Nominal data declarations (map metadata is not a source field).
        for d in &nav_mod.types {
            let qname = qualify(&file.module, &d.ident.name);
            match &d.kind {
                NavDataKind::Vector(_)
                | NavDataKind::SoaVector(_)
                | NavDataKind::Map(_, _)
                | NavDataKind::Channel(_)
                | NavDataKind::Arena(_) => {
                    let (keyword, args) = match &d.kind {
                        NavDataKind::Vector(ty) => ("vector", vec![ty]),
                        NavDataKind::SoaVector(ty) => ("soa vector", vec![ty]),
                        NavDataKind::Arena(ty) => ("arena", vec![ty]),
                        NavDataKind::Channel(ty) => ("channel", vec![ty]),
                        NavDataKind::Map(key, value) => ("map", vec![key, value]),
                        _ => unreachable!(),
                    };
                    for ty in &args {
                        record_type_ref(ty, &mut references);
                    }
                    let labels = args
                        .iter()
                        .map(|ty| {
                            format_type_label(remap_opt(ty.ty, map).unwrap_or(Type::Unit), &types)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let id = format!("struct:{qname}");
                    decl_ordinal += 1;
                    let sym = SymbolInfo {
                        id: id.clone(),
                        name: d.ident.name.clone(),
                        qualified_name: qname,
                        kind: "struct",
                        detail: format!(
                            "{}{keyword} {}({labels})",
                            if d.public { "pub " } else { "" },
                            d.ident.name,
                        ),
                        public: d.public,
                        file: file_rel.clone(),
                        path: file_path.clone(),
                        range: d.range,
                        selection_range: d.ident.range,
                        children: Vec::new(),
                    };
                    decl_keys_by_id.insert(
                        id.clone(),
                        DeclKey {
                            file_idx,
                            decl_ordinal,
                            kind: "struct",
                        },
                    );
                    symbols_by_id.insert(id.clone(), sym.clone());
                    references.push(ReferenceSpan {
                        range: d.ident.range,
                        symbol_id: id,
                        is_declaration: true,
                    });
                    outline.push(sym);
                }
                NavDataKind::Record(fields) => {
                    let struct_id = format!("struct:{qname}");
                    let mut children = Vec::with_capacity(fields.len());
                    for f in fields {
                        record_type_ref(&f.ty, &mut references);
                        let resolved_ty = remap_opt(f.ty.ty, map).unwrap_or(Type::Unit);
                        let field_qname = format!("{qname}::{}", f.ident.name);
                        let field_id = format!("field:{field_qname}");
                        decl_ordinal += 1;
                        let field_sym = SymbolInfo {
                            id: field_id.clone(),
                            name: f.ident.name.clone(),
                            qualified_name: field_qname,
                            kind: "field",
                            detail: format_type_label(resolved_ty, &types),
                            public: d.public,
                            file: file_rel.clone(),
                            path: file_path.clone(),
                            range: f.range,
                            selection_range: f.ident.range,
                            children: Vec::new(),
                        };
                        decl_keys_by_id.insert(
                            field_id.clone(),
                            DeclKey {
                                file_idx,
                                decl_ordinal,
                                kind: "field",
                            },
                        );
                        symbols_by_id.insert(field_id.clone(), field_sym.clone());
                        references.push(ReferenceSpan {
                            range: f.ident.range,
                            symbol_id: field_id,
                            is_declaration: true,
                        });
                        children.push(field_sym);
                    }
                    decl_ordinal += 1;
                    let struct_sym = SymbolInfo {
                        id: struct_id.clone(),
                        name: d.ident.name.clone(),
                        qualified_name: qname,
                        kind: "struct",
                        detail: format!(
                            "{}struct {}",
                            if d.public { "pub " } else { "" },
                            d.ident.name
                        ),
                        public: d.public,
                        file: file_rel.clone(),
                        path: file_path.clone(),
                        range: d.range,
                        selection_range: d.ident.range,
                        children,
                    };
                    decl_keys_by_id.insert(
                        struct_id.clone(),
                        DeclKey {
                            file_idx,
                            decl_ordinal,
                            kind: "struct",
                        },
                    );
                    symbols_by_id.insert(struct_id.clone(), struct_sym.clone());
                    references.push(ReferenceSpan {
                        range: d.ident.range,
                        symbol_id: struct_id,
                        is_declaration: true,
                    });
                    outline.push(struct_sym);
                }
                NavDataKind::Enum(variants) => {
                    let enum_id = format!("enum:{qname}");
                    let mut children = Vec::with_capacity(variants.len());
                    for v in variants {
                        if let Some(payload) = &v.payload {
                            record_type_ref(payload, &mut references);
                        }
                        let payload_ty = v
                            .payload
                            .as_ref()
                            .and_then(|p| remap_opt(p.ty, map))
                            .unwrap_or(Type::Unit);
                        let detail = if payload_ty == Type::Unit {
                            "()".to_owned()
                        } else {
                            format!("({})", format_type_label(payload_ty, &types))
                        };
                        let var_qname = format!("{qname}::{}", v.ident.name);
                        let var_id = format!("variant:{var_qname}");
                        decl_ordinal += 1;
                        let var_sym = SymbolInfo {
                            id: var_id.clone(),
                            name: v.ident.name.clone(),
                            qualified_name: var_qname,
                            kind: "variant",
                            detail,
                            public: d.public,
                            file: file_rel.clone(),
                            path: file_path.clone(),
                            range: v.range,
                            selection_range: v.ident.range,
                            children: Vec::new(),
                        };
                        decl_keys_by_id.insert(
                            var_id.clone(),
                            DeclKey {
                                file_idx,
                                decl_ordinal,
                                kind: "variant",
                            },
                        );
                        symbols_by_id.insert(var_id.clone(), var_sym.clone());
                        references.push(ReferenceSpan {
                            range: v.ident.range,
                            symbol_id: var_id,
                            is_declaration: true,
                        });
                        children.push(var_sym);
                    }
                    decl_ordinal += 1;
                    let enum_sym = SymbolInfo {
                        id: enum_id.clone(),
                        name: d.ident.name.clone(),
                        qualified_name: qname,
                        kind: "enum",
                        detail: format!(
                            "{}enum {}",
                            if d.public { "pub " } else { "" },
                            d.ident.name
                        ),
                        public: d.public,
                        file: file_rel.clone(),
                        path: file_path.clone(),
                        range: d.range,
                        selection_range: d.ident.range,
                        children,
                    };
                    decl_keys_by_id.insert(
                        enum_id.clone(),
                        DeclKey {
                            file_idx,
                            decl_ordinal,
                            kind: "enum",
                        },
                    );
                    symbols_by_id.insert(enum_id.clone(), enum_sym.clone());
                    references.push(ReferenceSpan {
                        range: d.ident.range,
                        symbol_id: enum_id,
                        is_declaration: true,
                    });
                    outline.push(enum_sym);
                }
            }
        }

        for owner in &nav_mod.implementations {
            record_type_ref(owner, &mut references);
        }
        // 3c. Function and Test declarations, parameters, view contracts, and bodies
        for f in &nav_mod.functions {
            let fn_qname = function_name(f, &file.module, map, &types);
            let fn_id = format!("{}:{fn_qname}", if f.is_test { "test" } else { "fn" });
            let mut fn_children = Vec::new();
            let mut scopes = ScopeStack::new();
            scopes.self_type = f.method_owner.as_ref().and_then(|o| remap_opt(o.ty, map));
            let mut param_sig_parts = Vec::with_capacity(f.params.len());

            for p in &f.params {
                record_type_ref(&p.ty, &mut references);
                let param_ty = remap_opt(p.ty.ty, map);
                let param_detail = format_param_detail(p, param_ty.unwrap_or(Type::Unit), &types);
                param_sig_parts.push(format!("{}: {param_detail}", p.ident.name));

                // Resolve parameter view contract field paths `.{mut f1, f2.sub}`
                if let (Some(view_fields), Some(Type::Named(root_type_id))) = (&p.view, param_ty) {
                    for vf in view_fields {
                        let mut cur_type_id = Some(root_type_id);
                        for seg in &vf.segments {
                            let Some(tid) = cur_type_id else {
                                break;
                            };
                            let Some(def) = types.definitions.get(tid) else {
                                break;
                            };
                            let DataKind::Record(fields) = &def.kind else {
                                break;
                            };
                            let Some(field_decl) = fields.iter().find(|fd| fd.name == seg.name)
                            else {
                                break;
                            };
                            let target_field_id = format!("field:{}::{}", def.name, seg.name);
                            references.push(ReferenceSpan {
                                range: seg.range,
                                symbol_id: target_field_id,
                                is_declaration: false,
                            });
                            cur_type_id = match field_decl.ty {
                                Type::Named(next_id) => Some(next_id),
                                _ => None,
                            };
                        }
                    }
                }

                let param_qname = format!("{fn_qname}::{}", p.ident.name);
                let param_id = format!("param:{param_qname}");
                decl_ordinal += 1;
                if overlapping_local_collision.is_none() && scopes.lookup(&p.ident.name).is_some() {
                    overlapping_local_collision =
                        Some((file_idx, p.ident.range.start.line, p.ident.name.clone()));
                }
                let param_sym = SymbolInfo {
                    id: param_id.clone(),
                    name: p.ident.name.clone(),
                    qualified_name: param_qname,
                    kind: "parameter",
                    detail: param_detail,
                    public: false,
                    file: file_rel.clone(),
                    path: file_path.clone(),
                    range: p.range,
                    selection_range: p.ident.range,
                    children: Vec::new(),
                };
                decl_keys_by_id.insert(
                    param_id.clone(),
                    DeclKey {
                        file_idx,
                        decl_ordinal,
                        kind: "parameter",
                    },
                );
                if f.method_owner.is_some() && p.ident.name == "self" {
                    fixed_receivers.insert(param_id.clone());
                }
                symbols_by_id.insert(param_id.clone(), param_sym.clone());
                references.push(ReferenceSpan {
                    range: p.ident.range,
                    symbol_id: param_id.clone(),
                    is_declaration: true,
                });
                fn_children.push(param_sym);
                scopes.insert(
                    p.ident.name.clone(),
                    LocalBinding {
                        id: param_id,
                        ty: param_ty,
                    },
                );
            }

            if let Some(ret_ts) = &f.ret {
                record_type_ref(ret_ts, &mut references);
            }
            let ret_ty = f
                .ret
                .as_ref()
                .and_then(|r| remap_opt(r.ty, map))
                .unwrap_or(Type::Unit);

            for req in &f.requires {
                resolve_expr(
                    req,
                    &file.module,
                    map,
                    &types,
                    &type_registry,
                    &fn_registry,
                    &module_symbols,
                    &scopes,
                    &mut references,
                );
            }
            for ens in &f.ensures {
                resolve_expr(
                    ens,
                    &file.module,
                    map,
                    &types,
                    &type_registry,
                    &fn_registry,
                    &module_symbols,
                    &scopes,
                    &mut references,
                );
            }

            let mut local_counter = 0usize;
            scopes.push();
            resolve_block(
                &f.body,
                file_idx,
                &file.module,
                &fn_qname,
                &file_rel,
                &file_path,
                map,
                &types,
                &type_registry,
                &fn_registry,
                &module_symbols,
                &mut scopes,
                &mut local_counter,
                &mut decl_ordinal,
                &mut overlapping_local_collision,
                &mut fn_children,
                &mut symbols_by_id,
                &mut decl_keys_by_id,
                &mut references,
            );
            scopes.pop();

            let fn_detail = if f.is_test {
                "test()".to_owned()
            } else {
                let pub_prefix = if f.public { "pub " } else { "" };
                let ret_suffix = if ret_ty == Type::Unit {
                    String::new()
                } else {
                    format!(" -> {}", format_type_label(ret_ty, &types))
                };
                format!("{pub_prefix}fn({}){ret_suffix}", param_sig_parts.join(", "))
            };

            decl_ordinal += 1;
            let fn_kind = if f.is_test { "test" } else { "function" };
            let fn_sym = SymbolInfo {
                id: fn_id.clone(),
                name: f.ident.name.clone(),
                qualified_name: fn_qname,
                kind: fn_kind,
                detail: fn_detail,
                public: f.public,
                file: file_rel.clone(),
                path: file_path.clone(),
                range: f.range,
                selection_range: f.ident.range,
                children: fn_children,
            };
            decl_keys_by_id.insert(
                fn_id.clone(),
                DeclKey {
                    file_idx,
                    decl_ordinal,
                    kind: fn_kind,
                },
            );
            symbols_by_id.insert(fn_id.clone(), fn_sym.clone());
            references.push(ReferenceSpan {
                range: f.ident.range,
                symbol_id: fn_id,
                is_declaration: true,
            });
            outline.push(fn_sym);
        }

        nav_files.push(FileNavigation {
            file_rel,
            file_path,
            outline,
            references,
        });
    }

    for (ordinal, hole) in hole_syntax(project)?.into_iter().enumerate() {
        let file = &mut nav_files[hole.file_index];
        let selection = NavRange {
            start: hole.range.start,
            end: NavPosition {
                byte: hole.range.start.byte + 2,
                column: hole.range.start.column + 2,
                ..hole.range.start
            },
        };
        let id = format!("hole:{}#{}", hole.function, hole.ordinal);
        let symbol = SymbolInfo {
            id: id.clone(),
            name: format!("?? #{}", hole.ordinal),
            qualified_name: format!("{}#{}", hole.function, hole.ordinal),
            kind: "hole",
            detail: format!("{}: {}", hole.expected_type, hole.intent),
            public: false,
            file: file.file_rel.clone(),
            path: file.file_path.clone(),
            range: hole.range,
            selection_range: selection,
            children: Vec::new(),
        };
        if let Some(parent) = file.outline.iter_mut().find(|s| {
            matches!(s.kind, "function" | "test")
                && s.range.start.byte <= hole.range.start.byte
                && hole.range.end.byte <= s.range.end.byte
        }) {
            parent.children.push(symbol.clone());
            if let Some(info) = symbols_by_id.get_mut(&parent.id) {
                info.children.push(symbol.clone());
            }
        }
        symbols_by_id.insert(id.clone(), symbol);
        decl_keys_by_id.insert(
            id.clone(),
            DeclKey {
                file_idx: hole.file_index,
                decl_ordinal: usize::MAX - ordinal,
                kind: "hole",
            },
        );
        file.references.push(ReferenceSpan {
            range: selection,
            symbol_id: id,
            is_declaration: true,
        });
    }
    Ok(ProjectNavigation {
        fixed_receivers,
        files: nav_files,
        symbols_by_id,
        decl_keys_by_id,
        overlapping_local_collision,
    })
}

fn record_module_prefix_refs(
    segments: &[IdentSpan],
    current_module: &str,
    module_symbols: &BTreeMap<String, String>,
    references: &mut Vec<ReferenceSpan>,
) {
    if segments.is_empty() {
        return;
    }
    let (mut acc, slice) = if segments[0].name == "crate" {
        (
            module_symbols
                .get(&format!("@crate:{current_module}"))
                .cloned()
                .unwrap_or_default(),
            &segments[1..],
        )
    } else {
        (current_module.to_owned(), segments)
    };
    for seg in slice {
        acc = qualify(&acc, &seg.name);
        if let Some(mod_id) = module_symbols.get(&acc) {
            references.push(ReferenceSpan {
                range: seg.range,
                symbol_id: mod_id.clone(),
                is_declaration: false,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_block(
    stmts: &[NavStmt],
    file_idx: usize,
    current_module: &str,
    fn_qname: &str,
    file_rel: &str,
    file_path: &str,
    map: &[Option<Type>],
    types: &TypeTable,
    type_registry: &BTreeMap<String, (usize, String, bool, String)>,
    fn_registry: &BTreeMap<String, FnSig>,
    module_symbols: &BTreeMap<String, String>,
    scopes: &mut ScopeStack,
    local_counter: &mut usize,
    decl_ordinal: &mut usize,
    overlapping_local_collision: &mut Option<(usize, usize, String)>,
    fn_children: &mut Vec<SymbolInfo>,
    symbols_by_id: &mut BTreeMap<String, SymbolInfo>,
    decl_keys_by_id: &mut BTreeMap<String, DeclKey>,
    references: &mut Vec<ReferenceSpan>,
) {
    for stmt in stmts {
        match &stmt.kind {
            NavStmtKind::Let {
                ident,
                mutable,
                value,
            } => {
                let val_ty = resolve_expr(
                    value,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                let idx = *local_counter;
                *local_counter += 1;
                *decl_ordinal += 1;
                if overlapping_local_collision.is_none() && scopes.lookup(&ident.name).is_some() {
                    *overlapping_local_collision =
                        Some((file_idx, ident.range.start.line, ident.name.clone()));
                }
                let local_id = format!("local:{fn_qname}#{idx}::{}", ident.name);
                let local_qname = format!("{fn_qname}::{}", ident.name);
                let ty_str = val_ty
                    .map(|t| format_type_label(t, types))
                    .unwrap_or_else(|| "unknown".into());
                let detail = if *mutable {
                    format!("let mut {ty_str}")
                } else {
                    format!("let {ty_str}")
                };
                let sym = SymbolInfo {
                    id: local_id.clone(),
                    name: ident.name.clone(),
                    qualified_name: local_qname,
                    kind: "local",
                    detail,
                    public: false,
                    file: file_rel.to_owned(),
                    path: file_path.to_owned(),
                    range: stmt.range,
                    selection_range: ident.range,
                    children: Vec::new(),
                };
                decl_keys_by_id.insert(
                    local_id.clone(),
                    DeclKey {
                        file_idx,
                        decl_ordinal: *decl_ordinal,
                        kind: "local",
                    },
                );
                symbols_by_id.insert(local_id.clone(), sym.clone());
                references.push(ReferenceSpan {
                    range: ident.range,
                    symbol_id: local_id.clone(),
                    is_declaration: true,
                });
                fn_children.push(sym);
                scopes.insert(
                    ident.name.clone(),
                    LocalBinding {
                        id: local_id,
                        ty: val_ty,
                    },
                );
            }
            NavStmtKind::ScopedBinding {
                ident,
                mutable,
                owner,
                selection,
                body,
            } => {
                let owner_ty = resolve_expr(
                    owner,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                let indices = match selection {
                    NavSelection::Region { start, end } => vec![start, end],
                    NavSelection::Element { index, .. } => vec![index],
                    NavSelection::Each => Vec::new(),
                    NavSelection::IntegerRange(end) => vec![end],
                };
                for index in indices {
                    resolve_expr(
                        index,
                        current_module,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        references,
                    );
                }
                let slice_ty = match selection {
                    NavSelection::Element {
                        access: ElementAccess::Index,
                        ..
                    }
                    | NavSelection::Each => owner_ty.and_then(|t| types.vector_element(t)),
                    NavSelection::Element {
                        access: ElementAccess::Key,
                        ..
                    } => owner_ty.and_then(|t| {
                        types
                            .arena_element(t)
                            .or_else(|| types.map_types(t).map(|(_, value)| value))
                    }),
                    NavSelection::IntegerRange(_) => Some(Type::Int),
                    NavSelection::Region { .. } => match owner_ty {
                        Some(Type::Text | Type::TextView) => Some(Type::TextView),
                        Some(Type::Ints) => Some(Type::Slice(Integer::I64)),
                        Some(Type::Fixed(n, _)) | Some(Type::Slice(n)) => Some(Type::Slice(n)),
                        Some(Type::Named(id))
                            if matches!(
                                types.definitions.get(id).map(|d| &d.kind),
                                Some(DataKind::Vector(_))
                            ) =>
                        {
                            Some(Type::VectorView(id))
                        }
                        Some(Type::VectorView(id)) => Some(Type::VectorView(id)),
                        _ => None,
                    },
                };
                let idx = *local_counter;
                *local_counter += 1;
                *decl_ordinal += 1;
                if overlapping_local_collision.is_none() && scopes.lookup(&ident.name).is_some() {
                    *overlapping_local_collision =
                        Some((file_idx, ident.range.start.line, ident.name.clone()));
                }
                let local_id = format!("local:{fn_qname}#{idx}::{}", ident.name);
                let local_qname = format!("{fn_qname}::{}", ident.name);
                let ty_str = slice_ty
                    .map(|t| format_type_label(t, types))
                    .unwrap_or_else(|| "[_]".into());
                let detail = if matches!(selection, NavSelection::IntegerRange(_)) {
                    format!("for {ty_str}")
                } else if *mutable {
                    format!("&mut {ty_str}")
                } else {
                    format!("&{ty_str}")
                };
                let sym = SymbolInfo {
                    id: local_id.clone(),
                    name: ident.name.clone(),
                    qualified_name: local_qname,
                    kind: "local",
                    detail,
                    public: false,
                    file: file_rel.to_owned(),
                    path: file_path.to_owned(),
                    range: stmt.range,
                    selection_range: ident.range,
                    children: Vec::new(),
                };
                decl_keys_by_id.insert(
                    local_id.clone(),
                    DeclKey {
                        file_idx,
                        decl_ordinal: *decl_ordinal,
                        kind: "local",
                    },
                );
                symbols_by_id.insert(local_id.clone(), sym.clone());
                references.push(ReferenceSpan {
                    range: ident.range,
                    symbol_id: local_id.clone(),
                    is_declaration: true,
                });
                fn_children.push(sym);

                scopes.push();
                scopes.insert(
                    ident.name.clone(),
                    LocalBinding {
                        id: local_id,
                        ty: slice_ty,
                    },
                );
                resolve_block(
                    body,
                    file_idx,
                    current_module,
                    fn_qname,
                    file_rel,
                    file_path,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    local_counter,
                    decl_ordinal,
                    overlapping_local_collision,
                    fn_children,
                    symbols_by_id,
                    decl_keys_by_id,
                    references,
                );
                scopes.pop();
            }
            NavStmtKind::Assign { target, value } => {
                if let Some(binding) = scopes.lookup(&target.name) {
                    references.push(ReferenceSpan {
                        range: target.range,
                        symbol_id: binding.id.clone(),
                        is_declaration: false,
                    });
                }
                resolve_expr(
                    value,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            NavStmtKind::AssignField { target, value } => {
                resolve_expr(
                    target,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                resolve_expr(
                    value,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            NavStmtKind::Expr(e) | NavStmtKind::Print(e) | NavStmtKind::Assert(e) => {
                resolve_expr(
                    e,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            NavStmtKind::Return(Some(e)) => {
                resolve_expr(
                    e,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            NavStmtKind::Return(None) | NavStmtKind::LoopJump => {}
            NavStmtKind::If(cond, yes, no) => {
                let (mut cond, mut yes, mut no) = (cond, yes, no);
                let mut alternative_scopes = 0;
                loop {
                    resolve_expr(
                        cond,
                        current_module,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        references,
                    );
                    scopes.push();
                    resolve_block(
                        yes,
                        file_idx,
                        current_module,
                        fn_qname,
                        file_rel,
                        file_path,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        local_counter,
                        decl_ordinal,
                        overlapping_local_collision,
                        fn_children,
                        symbols_by_id,
                        decl_keys_by_id,
                        references,
                    );
                    scopes.pop();
                    if no.is_empty() {
                        break;
                    }
                    // An else-if is represented as a single nested If. Walk
                    // that spine iteratively, preserving its lexical scopes.
                    if let [NavStmt {
                        kind: NavStmtKind::If(next_cond, next_yes, next_no),
                        ..
                    }] = no.as_slice()
                    {
                        scopes.push();
                        alternative_scopes += 1;
                        (cond, yes, no) = (next_cond, next_yes, next_no);
                        continue;
                    }
                    scopes.push();
                    resolve_block(
                        no,
                        file_idx,
                        current_module,
                        fn_qname,
                        file_rel,
                        file_path,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        local_counter,
                        decl_ordinal,
                        overlapping_local_collision,
                        fn_children,
                        symbols_by_id,
                        decl_keys_by_id,
                        references,
                    );
                    scopes.pop();
                    break;
                }
                for _ in 0..alternative_scopes {
                    scopes.pop();
                }
            }
            NavStmtKind::While(cond, body) => {
                resolve_expr(
                    cond,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                scopes.push();
                resolve_block(
                    body,
                    file_idx,
                    current_module,
                    fn_qname,
                    file_rel,
                    file_path,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    local_counter,
                    decl_ordinal,
                    overlapping_local_collision,
                    fn_children,
                    symbols_by_id,
                    decl_keys_by_id,
                    references,
                );
                scopes.pop();
            }
            NavStmtKind::Match(scrutinee, arms) => {
                let scrutinee_ty = resolve_expr(
                    scrutinee,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                for arm in arms {
                    let payload_ty = resolve_variant_path(
                        &arm.path,
                        scrutinee_ty,
                        scopes.self_type,
                        current_module,
                        types,
                        type_registry,
                        module_symbols,
                        references,
                    );
                    scopes.push();
                    if let Some(binding_ident) = &arm.binding {
                        let idx = *local_counter;
                        *local_counter += 1;
                        *decl_ordinal += 1;
                        if overlapping_local_collision.is_none()
                            && scopes.lookup(&binding_ident.name).is_some()
                        {
                            *overlapping_local_collision = Some((
                                file_idx,
                                binding_ident.range.start.line,
                                binding_ident.name.clone(),
                            ));
                        }
                        let local_id = format!("local:{fn_qname}#{idx}::{}", binding_ident.name);
                        let local_qname = format!("{fn_qname}::{}", binding_ident.name);
                        let payload_detail = payload_ty
                            .map(|t| format_type_label(t, types))
                            .unwrap_or_else(|| "unknown".into());
                        let detail = match &scrutinee.kind {
                            NavExprKind::Borrow(_, true) => format!("&mut {payload_detail}"),
                            NavExprKind::Borrow(_, false) => format!("&{payload_detail}"),
                            _ => payload_detail,
                        };
                        let sym = SymbolInfo {
                            id: local_id.clone(),
                            name: binding_ident.name.clone(),
                            qualified_name: local_qname,
                            kind: "local",
                            detail,
                            public: false,
                            file: file_rel.to_owned(),
                            path: file_path.to_owned(),
                            range: binding_ident.range,
                            selection_range: binding_ident.range,
                            children: Vec::new(),
                        };
                        decl_keys_by_id.insert(
                            local_id.clone(),
                            DeclKey {
                                file_idx,
                                decl_ordinal: *decl_ordinal,
                                kind: "local",
                            },
                        );
                        symbols_by_id.insert(local_id.clone(), sym.clone());
                        references.push(ReferenceSpan {
                            range: binding_ident.range,
                            symbol_id: local_id.clone(),
                            is_declaration: true,
                        });
                        fn_children.push(sym);
                        scopes.insert(
                            binding_ident.name.clone(),
                            LocalBinding {
                                id: local_id,
                                ty: payload_ty,
                            },
                        );
                    }
                    resolve_block(
                        &arm.body,
                        file_idx,
                        current_module,
                        fn_qname,
                        file_rel,
                        file_path,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        local_counter,
                        decl_ordinal,
                        overlapping_local_collision,
                        fn_children,
                        symbols_by_id,
                        decl_keys_by_id,
                        references,
                    );
                    scopes.pop();
                }
            }
        }
    }
}

// Mirrors the expression resolver's explicit snapshot/type/reference context.
#[allow(clippy::too_many_arguments)]
fn resolve_variant_path(
    path: &PathSpan,
    expected_enum_ty: Option<Type>,
    self_type: Option<Type>,
    current_module: &str,
    types: &TypeTable,
    type_registry: &BTreeMap<String, (usize, String, bool, String)>,
    module_symbols: &BTreeMap<String, String>,
    references: &mut Vec<ReferenceSpan>,
) -> Option<Type> {
    let joined = path.joined();
    let (type_part, variant_name) = joined.rsplit_once("::")?;
    if ["FileRead", "ReadResult", "WriteResult"].contains(&type_part) {
        let canonical = if type_part == "FileRead" {
            format!("ReadResult::{variant_name}")
        } else {
            joined.clone()
        };
        let (_, _, ty) = types.variant(&canonical)?;
        return Some(ty);
    }
    let enum_key = canonical_name(current_module, type_part, self_type, types);
    let &(type_id, ref owner, public, ref enum_sym_id) = type_registry.get(&enum_key)?;
    if owner != current_module && !public {
        return None;
    }
    if let Some(Type::Named(expected_id)) = expected_enum_ty {
        if expected_id != type_id {
            return None;
        }
    }
    let def = types.definitions.get(type_id)?;
    let DataKind::Enum(variants) = &def.kind else {
        return None;
    };
    let variant_decl = variants.iter().find(|v| v.name == variant_name)?;
    let variant_sym_id = format!("variant:{enum_key}::{variant_name}");

    if path.segments.len() > 2 {
        record_module_prefix_refs(
            &path.segments[..path.segments.len() - 2],
            current_module,
            module_symbols,
            references,
        );
    }
    if !enum_sym_id.is_empty()
        && path.segments.len() >= 2
        && (path.segments[path.segments.len() - 2].name != "Self" || self_type.is_none())
    {
        let enum_seg = &path.segments[path.segments.len() - 2];
        references.push(ReferenceSpan {
            range: enum_seg.range,
            symbol_id: enum_sym_id.clone(),
            is_declaration: false,
        });
    }
    if let Some(last_seg) = path.segments.last() {
        references.push(ReferenceSpan {
            range: last_seg.range,
            symbol_id: variant_sym_id,
            is_declaration: false,
        });
    }
    Some(variant_decl.ty)
}

#[allow(clippy::too_many_arguments)]
fn resolve_expr(
    expr: &NavExpr,
    current_module: &str,
    map: &[Option<Type>],
    types: &TypeTable,
    type_registry: &BTreeMap<String, (usize, String, bool, String)>,
    fn_registry: &BTreeMap<String, FnSig>,
    module_symbols: &BTreeMap<String, String>,
    scopes: &ScopeStack,
    references: &mut Vec<ReferenceSpan>,
) -> Option<Type> {
    match &expr.kind {
        NavExprKind::Int => Some(Type::Int),
        NavExprKind::Float => Some(Type::Float(Float::F64)),
        NavExprKind::Bool => Some(Type::Bool),
        NavExprKind::Text => Some(Type::Text),
        NavExprKind::Hole(ts, _) => {
            if let Some(path) = &ts.path {
                let key = canonical_name(current_module, &path.joined(), scopes.self_type, types);
                if let Some((_, owner, public, sym_id)) = type_registry.get(&key) {
                    if owner == current_module || *public {
                        if path.segments.len() > 1 {
                            record_module_prefix_refs(
                                &path.segments[..path.segments.len() - 1],
                                current_module,
                                module_symbols,
                                references,
                            );
                        }
                        if !sym_id.is_empty() {
                            if let Some(last_seg) = path.segments.last() {
                                references.push(ReferenceSpan {
                                    range: last_seg.range,
                                    symbol_id: sym_id.clone(),
                                    is_declaration: false,
                                });
                            }
                        }
                    }
                }
            }
            match ts.ty {
                Type::Named(local_idx) => map.get(local_idx).copied().flatten(),
                Type::Id(local_idx) => match map.get(local_idx).copied().flatten()? {
                    Type::Named(id) => Some(Type::Id(id)),
                    _ => None,
                },
                Type::VectorView(local_idx) => match map.get(local_idx).copied().flatten()? {
                    Type::Named(id) => Some(Type::VectorView(id)),
                    _ => None,
                },
                other => Some(other),
            }
        }
        NavExprKind::Var(ident) => {
            let binding = scopes.lookup(&ident.name)?;
            references.push(ReferenceSpan {
                range: ident.range,
                symbol_id: binding.id.clone(),
                is_declaration: false,
            });
            binding.ty
        }
        NavExprKind::Borrow(segments, _) => {
            let first = segments.first()?;
            let binding = scopes.lookup(&first.name)?;
            references.push(ReferenceSpan {
                range: first.range,
                symbol_id: binding.id.clone(),
                is_declaration: false,
            });
            let mut cur_ty = binding.ty;
            for seg in &segments[1..] {
                let Some(Type::Named(type_id)) = cur_ty else {
                    cur_ty = None;
                    break;
                };
                let Some(def) = types.definitions.get(type_id) else {
                    cur_ty = None;
                    break;
                };
                let DataKind::Record(fields) = &def.kind else {
                    cur_ty = None;
                    break;
                };
                let Some(f_decl) = fields.iter().find(|f| f.name == seg.name) else {
                    cur_ty = None;
                    break;
                };
                references.push(ReferenceSpan {
                    range: seg.range,
                    symbol_id: format!("field:{}::{}", def.name, seg.name),
                    is_declaration: false,
                });
                cur_ty = Some(f_decl.ty);
            }
            cur_ty
        }
        NavExprKind::Array(items) => {
            let mut elem_ty = None;
            for item in items {
                let t = resolve_expr(
                    item,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
                if elem_ty.is_none() {
                    elem_ty = t;
                }
            }
            match elem_ty {
                Some(Type::Int) | None => Some(Type::Ints),
                Some(Type::Text) => Some(Type::Texts),
                Some(Type::Number(n)) => Some(Type::Fixed(n, items.len())),
                _ => None,
            }
        }
        NavExprKind::Repeat(item, count) => {
            let elem_ty = resolve_expr(
                item,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            )?;
            match elem_ty {
                Type::Int => Some(Type::Fixed(Integer::I64, *count)),
                Type::Number(n) => Some(Type::Fixed(n, *count)),
                _ => None,
            }
        }
        NavExprKind::Field(base, field_ident) => {
            let base_ty = resolve_expr(
                base,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            )?;
            let Type::Named(type_id) = base_ty else {
                return None;
            };
            let def = types.definitions.get(type_id)?;
            let DataKind::Record(fields) = &def.kind else {
                return None;
            };
            let field_decl = fields.iter().find(|f| f.name == field_ident.name)?;
            references.push(ReferenceSpan {
                range: field_ident.range,
                symbol_id: format!("field:{}::{}", def.name, field_ident.name),
                is_declaration: false,
            });
            Some(field_decl.ty)
        }
        NavExprKind::Record(path, fields) => {
            let key = canonical_name(current_module, &path.joined(), scopes.self_type, types);
            let mut record_ty = None;
            let mut valid_fields: Option<&Vec<FieldDecl>> = None;
            if let Some(&(type_id, ref owner, public, ref sym_id)) = type_registry.get(&key) {
                if owner == current_module || public {
                    if let Some(def) = types.definitions.get(type_id) {
                        if let DataKind::Record(decl_fields) = &def.kind {
                            if path.segments.len() > 1 {
                                record_module_prefix_refs(
                                    &path.segments[..path.segments.len() - 1],
                                    current_module,
                                    module_symbols,
                                    references,
                                );
                            }
                            if !sym_id.is_empty()
                                && path
                                    .segments
                                    .last()
                                    .is_some_and(|s| s.name != "Self" || scopes.self_type.is_none())
                            {
                                if let Some(last_seg) = path.segments.last() {
                                    references.push(ReferenceSpan {
                                        range: last_seg.range,
                                        symbol_id: sym_id.clone(),
                                        is_declaration: false,
                                    });
                                }
                            }
                            record_ty = Some(Type::Named(type_id));
                            valid_fields = Some(decl_fields);
                        }
                    }
                }
            }
            for (f_ident, val_expr) in fields {
                if let Some(decl_fields) = valid_fields {
                    if decl_fields.iter().any(|df| df.name == f_ident.name) {
                        references.push(ReferenceSpan {
                            range: f_ident.range,
                            symbol_id: format!("field:{key}::{}", f_ident.name),
                            is_declaration: false,
                        });
                    }
                }
                resolve_expr(
                    val_expr,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            record_ty
        }
        NavExprKind::MethodCall(receiver, method, args) => {
            let receiver_ty = resolve_expr(
                receiver,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            );
            for arg in args {
                resolve_expr(
                    arg,
                    current_module,
                    map,
                    types,
                    type_registry,
                    fn_registry,
                    module_symbols,
                    scopes,
                    references,
                );
            }
            let Type::Named(id) = receiver_ty? else {
                return None;
            };
            let key = format!("{}::{}", types.definitions[id].name, method.name);
            let sig = fn_registry.get(&key).filter(|s| {
                s.method_owner == Some(id) && !s.is_test && (s.owner == current_module || s.public)
            })?;
            references.push(ReferenceSpan {
                range: method.range,
                symbol_id: sig.id.clone(),
                is_declaration: false,
            });
            Some(sig.ret)
        }
        NavExprKind::Call(path, args) | NavExprKind::Spawn(path, args) => {
            let is_spawn = matches!(expr.kind, NavExprKind::Spawn(_, _));
            let arg_tys = args
                .iter()
                .map(|a| {
                    resolve_expr(
                        a,
                        current_module,
                        map,
                        types,
                        type_registry,
                        fn_registry,
                        module_symbols,
                        scopes,
                        references,
                    )
                })
                .collect::<Vec<_>>();

            let raw_name = path.joined();
            if checker::BUILTINS.contains(&raw_name.as_str())
                && !fn_registry.contains_key(&canonical_name(
                    current_module,
                    &raw_name,
                    scopes.self_type,
                    types,
                ))
            {
                if raw_name == "process_run"
                    || INTEGER_BUILTIN_DETAILS
                        .iter()
                        .any(|(name, _)| *name == raw_name)
                    || FRONTIER3_BUILTIN_DETAILS
                        .iter()
                        .chain(V130_BUILTIN_DETAILS)
                        .any(|(name, _)| *name == raw_name)
                    || (CHANNEL_BUILTIN_DETAILS
                        .iter()
                        .any(|(name, _)| *name == raw_name)
                        && arg_tys
                            .first()
                            .copied()
                            .flatten()
                            .and_then(|ty| types.channel_element(ty))
                            .is_some())
                {
                    if let Some(last) = path.segments.last() {
                        references.push(ReferenceSpan {
                            range: last.range,
                            symbol_id: format!("builtin:{raw_name}"),
                            is_declaration: false,
                        });
                    }
                }
                return builtin_return_type(&raw_name, &arg_tys, types);
            }

            if !is_spawn {
                let key = canonical_name(current_module, &raw_name, scopes.self_type, types);
                if let Some((id, owner, public, sym)) = type_registry.get(&key) {
                    if (owner == current_module || *public)
                        && (types.vector_element(Type::Named(*id)).is_some()
                            || types.map_types(Type::Named(*id)).is_some()
                            || types.arena_element(Type::Named(*id)).is_some()
                            || matches!(types.definitions[*id].kind, DataKind::Channel(_)))
                    {
                        if path.segments.len() > 1 {
                            record_module_prefix_refs(
                                &path.segments[..path.segments.len() - 1],
                                current_module,
                                module_symbols,
                                references,
                            );
                        }
                        if let Some(last) = path
                            .segments
                            .last()
                            .filter(|s| s.name != "Self" || scopes.self_type.is_none())
                        {
                            references.push(ReferenceSpan {
                                range: last.range,
                                symbol_id: sym.clone(),
                                is_declaration: false,
                            });
                        }
                        return Some(Type::Named(*id));
                    }
                }
            }
            // Check if this is an enum variant constructor `[crate::][mod::]Enum::Variant(arg)`
            if !is_spawn && raw_name.contains("::") {
                if let Some((type_part, variant_name)) = raw_name.rsplit_once("::") {
                    if ["FileRead", "ReadResult", "WriteResult"].contains(&type_part) {
                        let canonical = if type_part == "FileRead" {
                            format!("ReadResult::{variant_name}")
                        } else {
                            raw_name.clone()
                        };
                        if let Some((owner_id, _, _)) = types.variant(&canonical) {
                            return Some(Type::Named(owner_id));
                        }
                    } else {
                        let enum_key =
                            canonical_name(current_module, type_part, scopes.self_type, types);
                        if let Some(&(type_id, ref owner, public, ref enum_sym_id)) =
                            type_registry.get(&enum_key)
                        {
                            if owner == current_module || public {
                                if let Some(def) = types.definitions.get(type_id) {
                                    if let DataKind::Enum(variants) = &def.kind {
                                        if variants.iter().any(|v| v.name == variant_name) {
                                            let variant_sym_id =
                                                format!("variant:{enum_key}::{variant_name}");
                                            if path.segments.len() > 2 {
                                                record_module_prefix_refs(
                                                    &path.segments[..path.segments.len() - 2],
                                                    current_module,
                                                    module_symbols,
                                                    references,
                                                );
                                            }
                                            if !enum_sym_id.is_empty() && path.segments.len() >= 2 {
                                                let enum_seg =
                                                    &path.segments[path.segments.len() - 2];
                                                if enum_seg.name != "Self"
                                                    || scopes.self_type.is_none()
                                                {
                                                    references.push(ReferenceSpan {
                                                        range: enum_seg.range,
                                                        symbol_id: enum_sym_id.clone(),
                                                        is_declaration: false,
                                                    });
                                                }
                                            }
                                            if let Some(last_seg) = path.segments.last() {
                                                references.push(ReferenceSpan {
                                                    range: last_seg.range,
                                                    symbol_id: variant_sym_id,
                                                    is_declaration: false,
                                                });
                                            }
                                            return Some(Type::Named(type_id));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let target = canonical_name(current_module, &raw_name, scopes.self_type, types);
            let sig = fn_registry
                .get(&target)
                .filter(|s| !s.is_test && (s.owner == current_module || s.public))?;
            if let Some(id) = sig.method_owner {
                if path.segments.len() >= 2 {
                    let owner = &path.segments[path.segments.len() - 2];
                    if owner.name != "Self" || scopes.self_type.is_none() {
                        references.push(ReferenceSpan {
                            range: owner.range,
                            symbol_id: format!(
                                "{}:{}",
                                if matches!(types.definitions[id].kind, DataKind::Enum(_)) {
                                    "enum"
                                } else {
                                    "struct"
                                },
                                types.definitions[id].name
                            ),
                            is_declaration: false,
                        });
                    }
                }
            }
            if path.segments.len() > 1 {
                record_module_prefix_refs(
                    &path.segments[..path.segments.len() - 1],
                    current_module,
                    module_symbols,
                    references,
                );
            }
            if let Some(last_seg) = path.segments.last() {
                references.push(ReferenceSpan {
                    range: last_seg.range,
                    symbol_id: sig.id.clone(),
                    is_declaration: false,
                });
            }
            if is_spawn {
                Type::task_of(sig.ret)
            } else {
                Some(sig.ret)
            }
        }
        NavExprKind::Try(keyword, inner) => {
            references.push(ReferenceSpan {
                range: keyword.range,
                symbol_id: "keyword:try".into(),
                is_declaration: false,
            });
            let Type::Named(id) = resolve_expr(
                inner,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            )?
            else {
                return None;
            };
            let DataKind::Enum(variants) = &types.definitions[id].kind else {
                return None;
            };
            variants.first().map(|variant| variant.ty)
        }
        NavExprKind::Unary(op, inner) => {
            let inner_ty = resolve_expr(
                inner,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            )?;
            if op == "!" {
                Some(Type::Bool)
            } else {
                Some(inner_ty)
            }
        }
        NavExprKind::Binary(op, left, right) => {
            let l_ty = resolve_expr(
                left,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            );
            let r_ty = resolve_expr(
                right,
                current_module,
                map,
                types,
                type_registry,
                fn_registry,
                module_symbols,
                scopes,
                references,
            );
            match op.as_str() {
                "||" | "&&" | "==" | "!=" | "<" | ">" | "<=" | ">=" => Some(Type::Bool),
                _ => l_ty.or(r_ty),
            }
        }
    }
}

fn builtin_return_type(name: &str, args: &[Option<Type>], types: &TypeTable) -> Option<Type> {
    crate::types::builtin_return_type(name, args, types)
}

pub fn symbols(project: &Project, requested_file: Option<&str>) -> Result<Value> {
    let nav = build_project_navigation(project)?;
    if let Some(req) = requested_file {
        let idx = match_file_index(project, req)
            .ok_or_else(|| Error::new(0, format!("unknown project file {req}")))?;
        let f = &nav.files[idx];
        Ok(json!({
            "coordinate_encoding": COORDINATE_ENCODING,
            "file": f.file_rel,
            "path": f.file_path,
            "symbols": f.outline.iter().map(SymbolInfo::to_outline_json).collect::<Vec<_>>(),
        }))
    } else if nav.files.len() == 1 {
        let f = &nav.files[0];
        Ok(json!({
            "coordinate_encoding": COORDINATE_ENCODING,
            "file": f.file_rel,
            "path": f.file_path,
            "symbols": f.outline.iter().map(SymbolInfo::to_outline_json).collect::<Vec<_>>(),
        }))
    } else {
        let all_symbols = nav
            .files
            .iter()
            .flat_map(|f| f.outline.iter().map(SymbolInfo::to_outline_json))
            .collect::<Vec<_>>();
        Ok(json!({
            "coordinate_encoding": COORDINATE_ENCODING,
            "symbols": all_symbols,
        }))
    }
}

pub fn symbol_at(
    project: &Project,
    requested_file: &str,
    line: usize,
    column: usize,
) -> Result<Value> {
    if line == 0 || column == 0 {
        return Err(Error::new(
            0,
            "symbol_at line and column are 1-based and must be >= 1",
        ));
    }
    let nav = build_project_navigation(project)?;
    let idx = match_file_index(project, requested_file)
        .ok_or_else(|| Error::new(0, format!("unknown project file {requested_file}")))?;
    let f = &nav.files[idx];

    // Pick the narrowest (innermost) token reference span containing `(line, column)`.
    let best = f
        .references
        .iter()
        .filter(|r| r.range.contains(line, column))
        .min_by_key(|r| r.range.byte_len());

    let symbol_json = if let Some(ref_span) = best {
        nav.symbols_by_id
            .get(&ref_span.symbol_id)
            .map(|sym| sym.to_target_json(ref_span.range))
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    Ok(json!({
        "coordinate_encoding": COORDINATE_ENCODING,
        "symbol": symbol_json,
    }))
}

#[derive(Clone, Debug)]
struct ResolvedOccurrence {
    file_idx: usize,
    file_rel: String,
    file_path: String,
    range: NavRange,
    is_declaration: bool,
}

fn collect_symbol_occurrences(
    nav: &ProjectNavigation,
    symbol_id: &str,
    include_declaration: bool,
) -> Vec<ResolvedOccurrence> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::<(usize, usize, usize)>::new();
    for (file_idx, f) in nav.files.iter().enumerate() {
        let mut file_refs = f
            .references
            .iter()
            .filter(|r| r.symbol_id == symbol_id && (include_declaration || !r.is_declaration))
            .collect::<Vec<_>>();
        file_refs.sort_by_key(|r| (r.range.start.byte, r.range.end.byte, !r.is_declaration));
        for r in file_refs {
            if seen.insert((file_idx, r.range.start.byte, r.range.end.byte)) {
                out.push(ResolvedOccurrence {
                    file_idx,
                    file_rel: f.file_rel.clone(),
                    file_path: f.file_path.clone(),
                    range: r.range,
                    is_declaration: r.is_declaration,
                });
            }
        }
    }
    out
}

pub fn references(
    project: &Project,
    requested_file: &str,
    line: usize,
    column: usize,
    include_declaration: bool,
) -> Result<Value> {
    if line == 0 || column == 0 {
        return Err(Error::new(
            0,
            "references line and column are 1-based and must be >= 1",
        ));
    }
    let nav = build_project_navigation(project)?;
    let idx = match_file_index(project, requested_file)
        .ok_or_else(|| Error::new(0, format!("unknown project file {requested_file}")))?;
    let f = &nav.files[idx];
    let snapshot_fp = project_fingerprint(project);

    let best = f
        .references
        .iter()
        .filter(|r| r.range.contains(line, column))
        .min_by_key(|r| r.range.byte_len());

    let Some(ref_span) = best else {
        return Ok(json!({
            "coordinate_encoding": COORDINATE_ENCODING,
            "include_declaration": include_declaration,
            "snapshot_fingerprint": snapshot_fp,
            "symbol": Value::Null,
            "references": [],
        }));
    };

    let Some(sym) = nav.symbols_by_id.get(&ref_span.symbol_id) else {
        return Ok(json!({
            "coordinate_encoding": COORDINATE_ENCODING,
            "include_declaration": include_declaration,
            "snapshot_fingerprint": snapshot_fp,
            "symbol": Value::Null,
            "references": [],
        }));
    };

    let occurrences = collect_symbol_occurrences(&nav, &sym.id, include_declaration);
    let refs_json = occurrences
        .iter()
        .map(|occ| {
            let src = &project.files[occ.file_idx].source;
            let text = src
                .get(occ.range.start.byte..occ.range.end.byte)
                .unwrap_or(&sym.name);
            json!({
                "symbol_id": sym.id,
                "name": sym.name,
                "qualified_name": sym.qualified_name,
                "kind": sym.kind,
                "file": occ.file_rel,
                "path": occ.file_path,
                "range": occ.range.to_json(),
                "text": text,
                "is_declaration": occ.is_declaration,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "coordinate_encoding": COORDINATE_ENCODING,
        "include_declaration": include_declaration,
        "snapshot_fingerprint": snapshot_fp,
        "symbol": sym.to_target_json(ref_span.range),
        "references": refs_json,
    }))
}

fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_reserved_keyword(name: &str) -> bool {
    [
        "fn", "let", "mut", "return", "break", "continue", "if", "else", "while", "true", "false",
        "spawn", "requires", "ensures", "assert", "print", "result", "mod", "pub", "test", "crate",
        "struct", "enum", "vector", "soa", "arena", "channel", "try", "extern", "map", "impl",
        "match", "borrow", "region", "get", "for", "in", "_",
    ]
    .contains(&name)
}

fn is_reserved_type_or_builtin(name: &str) -> bool {
    [
        "int",
        "bool",
        "text",
        "str",
        "ints",
        "texts",
        "task",
        "id",
        "u8",
        "u16",
        "u32",
        "u64",
        "usize",
        "i8",
        "i16",
        "i32",
        "i64",
        "isize",
        "f32",
        "f64",
        "ReadResult",
        "WriteResult",
        "ProcessRun",
        "ProcessOutput",
        "NetResponse",
        "NetResult",
    ]
    .contains(&name)
        || checker::BUILTINS.contains(&name)
}

fn validate_new_identifier(new_name: &str, sym: &SymbolInfo) -> Result<()> {
    if !is_valid_identifier(new_name) {
        return Err(Error::new(
            sym.selection_range.start.line,
            format!(
                "invalid rename target '{new_name}': identifiers must start with an ASCII letter or '_' and contain only ASCII letters, digits, or '_'"
            ),
        ));
    }
    if (sym.kind == "struct" || sym.kind == "enum")
        && !new_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
    {
        return Err(Error::new(
            sym.selection_range.start.line,
            format!("invalid rename target '{new_name}': data type names must start with an uppercase letter"),
        ));
    }
    if is_reserved_keyword(new_name) {
        return Err(Error::new(
            sym.selection_range.start.line,
            format!("invalid rename target '{new_name}': reserved language keyword"),
        ));
    }
    if is_reserved_type_or_builtin(new_name) {
        return Err(Error::new(
            sym.selection_range.start.line,
            format!("invalid rename target '{new_name}': reserved builtin type or function name"),
        ));
    }
    if sym.kind == "function" && !sym.qualified_name.contains("::") && new_name == "main" {
        return Err(Error::new(
            sym.selection_range.start.line,
            "cannot rename function to reserved entry point 'main'",
        ));
    }
    Ok(())
}

pub(crate) fn verify_snapshot_fingerprints(
    project: &Project,
    expected_snapshot_fp: Option<&str>,
    expected_file_fps: Option<&BTreeMap<String, String>>,
) -> Result<()> {
    let actual_snapshot_fp = project_fingerprint(project);
    if let Some(expected) = expected_snapshot_fp {
        if expected != actual_snapshot_fp {
            return Err(Error::new(
                0,
                format!(
                    "stale project snapshot: expected fingerprint {expected}, found {actual_snapshot_fp}"
                ),
            ));
        }
    }
    if let Some(file_fps) = expected_file_fps {
        for (req_path, expected_fp) in file_fps {
            let idx = match_file_index(project, req_path).ok_or_else(|| {
                Error::new(
                    0,
                    format!("unknown project file in file_fingerprints: {req_path}"),
                )
            })?;
            let actual_fp = source_fingerprint(&project.files[idx].source);
            if &actual_fp != expected_fp {
                return Err(Error::new(
                    0,
                    format!(
                        "stale file snapshot for {}: expected fingerprint {expected_fp}, found {actual_fp}",
                        relative_file_path(project, &project.files[idx])
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn check_project_complete(project: &Project) -> Result<()> {
    let (_, test_count) = project.checked(false)?;
    if test_count > 0 {
        project.checked(true)?;
    }
    Ok(())
}

fn classify_non_symbol_position(source: &str, line: usize, column: usize) -> Error {
    if let Ok(tokens) = lex_spanned(source) {
        for tok in &tokens {
            if tok.kind == TokKind::End {
                continue;
            }
            if tok.range.contains(line, column) {
                return match &tok.kind {
                    TokKind::Text(_) => Error::new(
                        line,
                        "cannot rename string literal content; select a symbol identifier",
                    ),
                    TokKind::Number(_) | TokKind::Float(_) => Error::new(
                        line,
                        "cannot rename numeric literal; select a symbol identifier",
                    ),
                    TokKind::Word(w) if is_reserved_keyword(w) => {
                        Error::new(line, format!("cannot rename reserved keyword '{w}'"))
                    }
                    TokKind::Word(w) if is_reserved_type_or_builtin(w) => {
                        Error::new(line, format!("cannot rename builtin '{w}'"))
                    }
                    TokKind::Symbol(s) => {
                        Error::new(line, format!("cannot rename operator or punctuation '{s}'"))
                    }
                    TokKind::Word(w) => {
                        Error::new(line, format!("cannot rename unresolved identifier '{w}'"))
                    }
                    TokKind::End => Error::new(
                        line,
                        "cannot rename comment, whitespace, or non-symbol position",
                    ),
                };
            }
        }
    }
    Error::new(
        line,
        "cannot rename comment, whitespace, or non-symbol position",
    )
}

fn ensure_symbol_is_renameable(
    sym: &SymbolInfo,
    line: usize,
    nav: &ProjectNavigation,
    project: &Project,
) -> Result<()> {
    if matches!(sym.kind, "builtin" | "keyword") {
        return Err(Error::new(
            line,
            "built-in operations and keywords cannot be renamed",
        ));
    }
    if sym.kind == "hole" {
        return Err(Error::new(
            line,
            "typed holes are replaced with verify_holes, not renamed",
        ));
    }
    if let Some((index, _)) = nav.files.iter().enumerate().find(|(index, file)| {
        !project.files[*index].package.is_empty()
            && file.references.iter().any(|r| r.symbol_id == sym.id)
    }) {
        return Err(Error::new(
            line,
            format!(
                "dependency sources are read-only; rename would change {}",
                project.files[index].source_key
            ),
        ));
    }
    if nav.fixed_receivers.contains(&sym.id) {
        return Err(Error::new(line, "method receiver self cannot be renamed"));
    }
    if sym.kind == "module" {
        return Err(Error::new(
            line,
            format!(
                "module '{}' cannot be renamed in-place because module declarations map to source file paths",
                sym.qualified_name
            ),
        ));
    }
    if sym.id == "fn:main" {
        return Err(Error::new(line, "cannot rename program entry point 'main'"));
    }
    Ok(())
}

fn symbol_has_external_visibility(sym: &SymbolInfo, nav: &ProjectNavigation) -> bool {
    if sym.public {
        return true;
    }
    if sym.kind == "field" || sym.kind == "variant" {
        if let Some((parent_qname, _)) = sym.qualified_name.rsplit_once("::") {
            let parent_struct = format!("struct:{parent_qname}");
            let parent_enum = format!("enum:{parent_qname}");
            if nav
                .symbols_by_id
                .get(&parent_struct)
                .is_some_and(|p| p.public)
                || nav
                    .symbols_by_id
                    .get(&parent_enum)
                    .is_some_and(|p| p.public)
            {
                return true;
            }
        }
    }
    false
}

pub fn prepare_rename(
    project: &Project,
    requested_file: &str,
    line: usize,
    column: usize,
    expected_snapshot_fp: Option<&str>,
    expected_file_fps: Option<&BTreeMap<String, String>>,
) -> Result<Value> {
    if line == 0 || column == 0 {
        return Err(Error::new(
            0,
            "prepare_rename line and column are 1-based and must be >= 1",
        ));
    }
    verify_snapshot_fingerprints(project, expected_snapshot_fp, expected_file_fps)?;
    check_project_complete(project)?;

    let nav = build_project_navigation(project)?;
    let idx = match_file_index(project, requested_file)
        .ok_or_else(|| Error::new(0, format!("unknown project file {requested_file}")))?;
    let f = &nav.files[idx];

    let best = f
        .references
        .iter()
        .filter(|r| r.range.contains(line, column))
        .min_by_key(|r| r.range.byte_len());

    let Some(ref_span) = best else {
        return Err(
            classify_non_symbol_position(&project.files[idx].source, line, column)
                .in_file(&project.files[idx].path),
        );
    };

    let Some(sym) = nav.symbols_by_id.get(&ref_span.symbol_id) else {
        return Err(
            classify_non_symbol_position(&project.files[idx].source, line, column)
                .in_file(&project.files[idx].path),
        );
    };

    ensure_symbol_is_renameable(sym, line, &nav, project)
        .map_err(|e| e.in_file(&project.files[idx].path))?;

    let occurrences = collect_symbol_occurrences(&nav, &sym.id, true);
    let public_symbol = symbol_has_external_visibility(sym, &nav);
    let snapshot_fp = project_fingerprint(project);
    let file_fp = source_fingerprint(&project.files[idx].source);

    Ok(json!({
        "coordinate_encoding": COORDINATE_ENCODING,
        "can_rename": true,
        "file": f.file_rel,
        "path": f.file_path,
        "range": ref_span.range.to_json(),
        "placeholder": sym.name,
        "symbol": sym.to_target_json(ref_span.range),
        "reference_count": occurrences.len(),
        "snapshot_fingerprint": snapshot_fp,
        "file_fingerprint": file_fp,
        "scope": "loaded_project",
        "public_symbol": public_symbol,
        "scope_warning": if public_symbol {
            Value::String(format!(
                "symbol '{}' is public; rename updates declarations and references within the loaded project ({}) only, and external consumers outside the loaded project are not updated",
                sym.qualified_name, project.name
            ))
        } else {
            Value::Null
        },
    }))
}

fn validate_rename_semantics(
    project: &Project,
    before_nav: &ProjectNavigation,
    after_nav: &ProjectNavigation,
    target_sym_id: &str,
    new_name: &str,
) -> Result<()> {
    if let Some((file_idx, line, collided_name)) = &after_nav.overlapping_local_collision {
        return Err(Error::new(
            *line,
            format!(
                "declaration collision: '{collided_name}' is already bound in the same or an enclosing scope"
            ),
        )
        .in_file(&project.files[*file_idx].path));
    }

    if after_nav.symbols_by_id.len() != before_nav.symbols_by_id.len()
        || after_nav.decl_keys_by_id.len() != before_nav.decl_keys_by_id.len()
    {
        return Err(Error::new(
            0,
            format!("declaration collision: '{new_name}' collides with an existing declaration"),
        ));
    }

    let Some(&target_decl_key) = before_nav.decl_keys_by_id.get(target_sym_id) else {
        return Err(Error::new(
            0,
            "internal error: missing declaration key for rename target",
        ));
    };

    for (file_idx, (before_file, after_file)) in before_nav
        .files
        .iter()
        .zip(after_nav.files.iter())
        .enumerate()
    {
        if before_file.references.len() != after_file.references.len() {
            let line = after_file
                .references
                .first()
                .map_or(1, |r| r.range.start.line);
            return Err(Error::new(
                line,
                format!(
                    "rename to '{new_name}' changes symbol resolution in {}",
                    before_file.file_rel
                ),
            )
            .in_file(&project.files[file_idx].path));
        }

        for (before_ref, after_ref) in before_file
            .references
            .iter()
            .zip(after_file.references.iter())
        {
            let before_key = before_nav.decl_keys_by_id.get(&before_ref.symbol_id);
            let after_key = after_nav.decl_keys_by_id.get(&after_ref.symbol_id);

            if before_key.is_none() || before_key != after_key {
                return Err(Error::new(
                    after_ref.range.start.line,
                    format!(
                        "rename to '{new_name}' would capture or rebind an existing reference (originally resolved to '{}', would resolve to '{}')",
                        before_ref.symbol_id, after_ref.symbol_id
                    ),
                )
                .in_file(&project.files[file_idx].path));
            }

            if before_ref.is_declaration != after_ref.is_declaration {
                return Err(Error::new(
                    after_ref.range.start.line,
                    format!(
                        "rename to '{new_name}' alters declaration/reference role at line {}",
                        after_ref.range.start.line
                    ),
                )
                .in_file(&project.files[file_idx].path));
            }

            let was_target = before_ref.symbol_id == target_sym_id;
            let is_target_now = after_key == Some(&target_decl_key);
            if was_target != is_target_now {
                return Err(Error::new(
                    after_ref.range.start.line,
                    format!(
                        "rename to '{new_name}' would capture or rebind reference at line {}",
                        after_ref.range.start.line
                    ),
                )
                .in_file(&project.files[file_idx].path));
            }
        }
    }

    Ok(())
}

pub fn rename(
    project: &Project,
    requested_file: &str,
    line: usize,
    column: usize,
    new_name: &str,
    expected_snapshot_fp: Option<&str>,
    expected_file_fps: Option<&BTreeMap<String, String>>,
) -> Result<Value> {
    if line == 0 || column == 0 {
        return Err(Error::new(
            0,
            "rename line and column are 1-based and must be >= 1",
        ));
    }
    verify_snapshot_fingerprints(project, expected_snapshot_fp, expected_file_fps)?;
    check_project_complete(project)?;

    let before_nav = build_project_navigation(project)?;
    let idx = match_file_index(project, requested_file)
        .ok_or_else(|| Error::new(0, format!("unknown project file {requested_file}")))?;
    let f = &before_nav.files[idx];

    let best = f
        .references
        .iter()
        .filter(|r| r.range.contains(line, column))
        .min_by_key(|r| r.range.byte_len());

    let Some(ref_span) = best else {
        return Err(
            classify_non_symbol_position(&project.files[idx].source, line, column)
                .in_file(&project.files[idx].path),
        );
    };

    let Some(sym) = before_nav.symbols_by_id.get(&ref_span.symbol_id) else {
        return Err(
            classify_non_symbol_position(&project.files[idx].source, line, column)
                .in_file(&project.files[idx].path),
        );
    };

    ensure_symbol_is_renameable(sym, line, &before_nav, project)
        .map_err(|e| e.in_file(&project.files[idx].path))?;
    validate_new_identifier(new_name, sym).map_err(|e| e.in_file(&project.files[idx].path))?;

    let occurrences = collect_symbol_occurrences(&before_nav, &sym.id, true);
    if !occurrences.iter().any(|o| o.is_declaration) {
        return Err(Error::new(
            line,
            format!(
                "cannot rename '{}': declaration is not part of the loaded project",
                sym.qualified_name
            ),
        )
        .in_file(&project.files[idx].path));
    }

    // Group occurrences per file, validate exact slice text and non-overlapping order,
    // and build in-memory overlays for post-rename verification.
    let mut edits_by_file = BTreeMap::<usize, Vec<&ResolvedOccurrence>>::new();
    for occ in &occurrences {
        edits_by_file.entry(occ.file_idx).or_default().push(occ);
    }

    let mut overlays = BTreeMap::<String, String>::new();
    let mut flat_edits_json = Vec::with_capacity(occurrences.len());
    let mut document_edits_json = Vec::with_capacity(edits_by_file.len());
    let mut file_fingerprints_json = serde_json::Map::new();

    for file in &project.files {
        let rel = relative_file_path(project, file);
        file_fingerprints_json.insert(rel, Value::String(source_fingerprint(&file.source)));
    }

    for (&file_idx, file_occs) in &edits_by_file {
        let src_file = &project.files[file_idx];
        let src = &src_file.source;
        let file_fp = source_fingerprint(src);
        let mut prev_end = 0usize;
        let mut per_file_edits = Vec::with_capacity(file_occs.len());

        for occ in file_occs {
            if occ.range.start.byte < prev_end || occ.range.end.byte < occ.range.start.byte {
                return Err(Error::new(
                    occ.range.start.line,
                    "overlapping edit ranges encountered during rename planning",
                )
                .in_file(&src_file.path));
            }
            let slice = src
                .get(occ.range.start.byte..occ.range.end.byte)
                .ok_or_else(|| {
                    Error::new(
                        occ.range.start.line,
                        "invalid UTF-8 byte range in rename edit",
                    )
                    .in_file(&src_file.path)
                })?;
            if slice != sym.name {
                return Err(Error::new(
                    occ.range.start.line,
                    format!(
                        "unexpected token text '{slice}' for symbol '{}' at line {}",
                        sym.name, occ.range.start.line
                    ),
                )
                .in_file(&src_file.path));
            }
            prev_end = occ.range.end.byte;

            let edit_obj = json!({
                "file": occ.file_rel,
                "path": occ.file_path,
                "range": occ.range.to_json(),
                "original_text": slice,
                "new_text": new_name,
                "is_declaration": occ.is_declaration,
                "file_fingerprint": file_fp,
            });
            flat_edits_json.push(edit_obj.clone());
            per_file_edits.push(edit_obj);
        }

        let mut updated = src.clone();
        for occ in file_occs.iter().rev() {
            updated.replace_range(occ.range.start.byte..occ.range.end.byte, new_name);
        }
        overlays.insert(file_occs[0].file_rel.clone(), updated);

        document_edits_json.push(json!({
            "file": file_occs[0].file_rel,
            "path": file_occs[0].file_path,
            "file_fingerprint": file_fp,
            "edits": per_file_edits,
        }));
    }

    // Apply proposed edits in memory (zero disk writes) and verify:
    // 1. Parsing and module graph integrity (`with_overlays`)
    // 2. Binding-relationship preservation and collision freedom (`validate_rename_semantics`)
    // 3. Full-project type, ownership, borrow, view, contract, and effect checks (`check_project_complete`)
    let renamed_project = project.with_overlays(&overlays)?;
    let after_nav = build_project_navigation(&renamed_project)?;
    validate_rename_semantics(project, &before_nav, &after_nav, &sym.id, new_name)?;
    check_project_complete(&renamed_project)?;

    let snapshot_fp = project_fingerprint(project);
    let post_snapshot_fp = project_fingerprint(&renamed_project);
    let public_symbol = symbol_has_external_visibility(sym, &before_nav);

    Ok(json!({
        "coordinate_encoding": COORDINATE_ENCODING,
        "writes": 0,
        "checked_files": renamed_project.files.len(),
        "symbol": sym.to_target_json(ref_span.range),
        "new_name": new_name,
        "snapshot_fingerprint": snapshot_fp,
        "post_rename_fingerprint": post_snapshot_fp,
        "file_fingerprints": Value::Object(file_fingerprints_json),
        "scope": "loaded_project",
        "public_symbol": public_symbol,
        "scope_warning": if public_symbol {
            Value::String(format!(
                "symbol '{}' is public; rename updates declarations and references within the loaded project ({}) only, and external consumers outside the loaded project are not updated",
                sym.qualified_name, project.name
            ))
        } else {
            Value::Null
        },
        "edits": flat_edits_json,
        "document_edits": document_edits_json,
    }))
}

#[cfg(test)]
mod v028_tests {
    use super::*;

    #[test]
    fn intrinsic_and_wire_alias_hovers_resolve_actual_builtin_names() {
        for (source, name) in [
            ("fn main(){let value=rotl(u32(1),-1);print value;}", "rotl"),
            (
                "fn main(){let bytes=[u8(1);8];let value=wire_read_u32_be(bytes,0);print value;}",
                "wire_read_u32_be",
            ),
        ] {
            let project = Project::from_source(source).unwrap();
            project.checked(false).unwrap();
            let column = source.find(name).unwrap() + 1;
            let result = symbol_at(&project, "", 1, column).unwrap();
            assert_eq!(result["symbol"]["name"], name, "{result}");
            assert_eq!(result["symbol"]["kind"], "builtin");
            assert!(result["symbol"]["detail"]
                .as_str()
                .unwrap()
                .contains("pure"));
            let result = symbol_at(&project, "", 1, source.rfind("value").unwrap() + 1).unwrap();
            assert!(
                result["symbol"]["detail"].as_str().unwrap().contains("u32"),
                "{result}"
            );
        }
    }

    #[test]
    fn new_builtin_hover_does_not_create_legacy_generic_definitions() {
        let source = "fn main(){let items=[1,2];print len(items);}";
        let result = symbol_at(
            &Project::from_source(source).unwrap(),
            "",
            1,
            source.find("len").unwrap() + 1,
        )
        .unwrap();
        assert_eq!(result["symbol"], Value::Null, "{result}");
    }
}
