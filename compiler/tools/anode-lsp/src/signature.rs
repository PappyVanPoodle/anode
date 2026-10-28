//! Signature help and prelude completion (1.2, contract N9).
//!
//! Signature help is compiler-backed: the server finds the innermost open
//! call around the cursor here, asks the compiler's `symbol_at` for the
//! callee, and turns the returned symbol detail (`name(params) -> T; docs`)
//! into an LSP `SignatureHelp`. Completion offers only the language prelude:
//! builtins, standard result types and their variants. Visible bindings and
//! fields still need a compiler `complete` method (not in agent protocol v1).
use serde_json::{json, Value};

/// One prelude entry: name, LSP `CompletionItemKind`, and the detail the
/// compiler's navigation reports for it.
struct Prelude {
    name: &'static str,
    kind: u32,
    detail: &'static str,
}
const FUNCTION: u32 = 3;
const ENUM: u32 = 13;
const STRUCT: u32 = 22;
const ENUM_MEMBER: u32 = 20;

/// The 1.2 Frontier 3 prelude (contracts N2-N5), the 1.3 OS/runtime builtins
/// and the 1.0 process prelude.
const PRELUDE: &[Prelude] = &[
    Prelude {
        name: "try_write_u16_be",
        kind: FUNCTION,
        detail: "try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool",
    },
    Prelude {
        name: "try_write_u32_be",
        kind: FUNCTION,
        detail: "try_write_u32_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u32) -> bool",
    },
    Prelude {
        name: "try_write_u64_be",
        kind: FUNCTION,
        detail: "try_write_u64_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u64) -> bool",
    },
    Prelude {
        name: "wire_try_write_u16_be",
        kind: FUNCTION,
        detail:
            "wire_try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool",
    },
    Prelude {
        name: "wire_try_write_u32_be",
        kind: FUNCTION,
        detail:
            "wire_try_write_u32_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u32) -> bool",
    },
    Prelude {
        name: "wire_try_write_u64_be",
        kind: FUNCTION,
        detail:
            "wire_try_write_u64_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u64) -> bool",
    },
    Prelude {
        name: "fnv1a_64",
        kind: FUNCTION,
        detail: "fnv1a_64(input: text/str/&[u8]/[u8; N]) -> u64",
    },
    Prelude {
        name: "sha256_text",
        kind: FUNCTION,
        detail: "sha256_text(input: text/str/&[u8]/[u8; N]) -> text",
    },
    Prelude {
        name: "net_request",
        kind: FUNCTION,
        detail: "net_request(endpoint: text/str, payload: text/str) -> NetResult",
    },
    Prelude {
        name: "try_read_bytes",
        kind: FUNCTION,
        detail: "try_read_bytes(path: text/str, dst: &mut [u8; N] or &mut [u8]) -> int",
    },
    Prelude {
        name: "try_write_bytes",
        kind: FUNCTION,
        detail: "try_write_bytes(path: text/str, src: [u8; N] or &[u8]) -> bool",
    },
    Prelude {
        name: "env_var",
        kind: FUNCTION,
        detail: "env_var(name: text/str) -> text",
    },
    Prelude {
        name: "create_dir",
        kind: FUNCTION,
        detail: "create_dir(path: text/str) -> bool",
    },
    Prelude {
        name: "remove_file",
        kind: FUNCTION,
        detail: "remove_file(path: text/str) -> bool",
    },
    Prelude {
        name: "net_serve_once",
        kind: FUNCTION,
        detail:
            "net_serve_once(endpoint: text/str, status: int, reply_body: text/str) -> NetResult",
    },
    Prelude {
        name: "process_run",
        kind: FUNCTION,
        detail: "process_run(program: text/str, args: &texts or &VectorOfText) -> ProcessRun",
    },
    Prelude {
        name: "NetResponse",
        kind: STRUCT,
        detail: "NetResponse { status: int, body: text }",
    },
    Prelude {
        name: "NetResult",
        kind: ENUM,
        detail: "enum NetResult { Response(NetResponse), Error(text) }",
    },
    Prelude {
        name: "ProcessOutput",
        kind: STRUCT,
        detail: "ProcessOutput { status: int, stdout: text, stderr: text }",
    },
    Prelude {
        name: "ProcessRun",
        kind: ENUM,
        detail: "enum ProcessRun { Output(ProcessOutput), Error(text) }",
    },
];
/// Variants offered after `Enum::`.
const VARIANTS: &[(&str, &str, &str)] = &[
    ("NetResult", "Response", "NetResult::Response(NetResponse)"),
    ("NetResult", "Error", "NetResult::Error(text)"),
    ("ProcessRun", "Output", "ProcessRun::Output(ProcessOutput)"),
    ("ProcessRun", "Error", "ProcessRun::Error(text)"),
];

fn identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The innermost call whose `(` is still open at byte `cursor`: the byte
/// offset where its callee identifier starts, the callee text and the
/// zero-based active parameter (top-level commas so far). String literals
/// and `//` comments are skipped; brackets nest.
pub fn call_context(text: &str, cursor: usize) -> Option<(usize, String, usize)> {
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());
    // (open byte, is paren, commas at this depth)
    let mut stack: Vec<(usize, bool, usize)> = Vec::new();
    let mut i = 0;
    while i < cursor {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < cursor && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < cursor && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => stack.push((i, true, 0)),
            b'[' | b'{' => stack.push((i, false, 0)),
            b')' | b']' | b'}' => {
                stack.pop();
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let &(open, paren, commas) = stack.last()?;
    if !paren {
        return None;
    }
    let mut end = open;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (identifier_byte(bytes[start - 1]) || bytes[start - 1] == b':') {
        start -= 1;
    }
    while start < end && bytes[start] == b':' {
        start += 1;
    }
    let callee = &text[start..end];
    if callee.is_empty() || callee.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    Some((start, callee.to_owned(), commas))
}

/// Split `params` at commas outside brackets.
fn split_parameters(params: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in params.chars() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

/// LSP `SignatureHelp` from a compiler `symbol_at` symbol whose detail is
/// `name(params) -> T; documentation` (builtins) or a function signature.
pub fn signature_help(symbol: &Value, active: usize) -> Option<Value> {
    let detail = symbol.get("detail").and_then(Value::as_str)?;
    let detail = detail.strip_prefix("fn ").unwrap_or(detail);
    let open = detail.find('(')?;
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in detail.char_indices().skip(open) {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth == 0 && c == ')' {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    // The label runs to the first top-level `;` after the parameter list.
    let tail = &detail[close + 1..];
    let label_end = tail.find(';').map_or(detail.len(), |i| close + 1 + i);
    let label = detail[..label_end].trim().to_owned();
    let documentation = detail[label_end..]
        .trim_start_matches(';')
        .trim()
        .to_owned();
    let parameters = split_parameters(&detail[open + 1..close]);
    let mut signature = json!({
        "label": label,
        "parameters": parameters.iter().map(|p| json!({"label": p})).collect::<Vec<_>>(),
    });
    if !documentation.is_empty() {
        signature["documentation"] = json!({"kind": "markdown", "value": documentation});
    }
    Some(json!({
        "signatures": [signature],
        "activeSignature": 0,
        "activeParameter": active.min(parameters.len().saturating_sub(1)),
    }))
}

/// Prelude completion at byte `cursor`: variants after `Enum::`, otherwise
/// prelude builtins and types whose names start with the typed prefix.
pub fn prelude_completions(text: &str, cursor: usize) -> Value {
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());
    let mut start = cursor;
    while start > 0 && identifier_byte(bytes[start - 1]) {
        start -= 1;
    }
    let prefix = &text[start..cursor];
    let mut items = Vec::new();
    if start >= 2 && &bytes[start - 2..start] == b"::" {
        let mut owner_start = start - 2;
        while owner_start > 0 && identifier_byte(bytes[owner_start - 1]) {
            owner_start -= 1;
        }
        let owner = &text[owner_start..start - 2];
        for (enum_name, variant, detail) in VARIANTS {
            if *enum_name == owner && variant.starts_with(prefix) {
                items.push(json!({"label": variant, "kind": ENUM_MEMBER, "detail": detail}));
            }
        }
    } else {
        for entry in PRELUDE {
            if entry.name.starts_with(prefix) {
                items
                    .push(json!({"label": entry.name, "kind": entry.kind, "detail": entry.detail}));
            }
        }
    }
    json!({"isIncomplete": false, "items": items})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_context_tracks_nesting_strings_and_commas() {
        let text = "fn f(){let x=net_request(\"a,(b\", g(1,2), ";
        let (start, callee, active) = call_context(text, text.len()).unwrap();
        assert_eq!(callee, "net_request");
        assert_eq!(&text[start..start + 11], "net_request");
        assert_eq!(active, 2);
        let text = "let y=NetResult::Response(";
        assert_eq!(
            call_context(text, text.len()).unwrap().1,
            "NetResult::Response"
        );
        assert!(call_context("let a=[1,", 9).is_none());
    }

    #[test]
    fn signature_help_splits_parameters_outside_brackets() {
        let symbol = json!({"detail": "try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool; checked big-endian write"});
        let help = signature_help(&symbol, 2).unwrap();
        let signature = &help["signatures"][0];
        assert_eq!(
            signature["label"],
            "try_write_u16_be(buf: &mut [u8; N] or &mut [u8], offset: int, value: u16) -> bool"
        );
        assert_eq!(signature["parameters"].as_array().unwrap().len(), 3);
        assert_eq!(signature["parameters"][2]["label"], "value: u16");
        assert_eq!(help["activeParameter"], 2);
        assert_eq!(
            signature["documentation"]["value"],
            "checked big-endian write"
        );
    }

    #[test]
    fn prelude_completion_offers_builtins_types_and_variants() {
        let items = prelude_completions("let h=fnv", 9);
        assert_eq!(items["items"][0]["label"], "fnv1a_64");
        let text = "match r{NetResult::R";
        let items = prelude_completions(text, text.len());
        assert_eq!(items["items"].as_array().unwrap().len(), 1);
        assert_eq!(items["items"][0]["label"], "Response");
        let types = prelude_completions("Net", 3);
        let names: Vec<&str> = types["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["NetResponse", "NetResult"]);
    }
}
