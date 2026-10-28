use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            line: value.get("line")?.as_u64()? as u32,
            character: value.get("character")?.as_u64()? as u32,
        })
    }

    pub fn to_json(self) -> Value {
        json!({
            "line": self.line,
            "character": self.character,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            start: Position::from_json(value.get("start")?)?,
            end: Position::from_json(value.get("end")?)?,
        })
    }

    pub fn to_json(self) -> Value {
        json!({
            "start": self.start.to_json(),
            "end": self.end.to_json(),
        })
    }
}

/// Split document text into lines without trailing `\r` or `\n`.
pub fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Convert a UTF-8 byte offset within a single line to an LSP UTF-16 column index.
pub fn utf8_byte_to_utf16_col(line: &str, byte_offset: usize) -> u32 {
    let clamped = byte_offset.min(line.len());
    let mut utf16_col = 0u32;
    for (idx, ch) in line.char_indices() {
        if idx >= clamped {
            break;
        }
        utf16_col += ch.len_utf16() as u32;
    }
    utf16_col
}

/// Convert an LSP UTF-16 column index within a single line to a valid UTF-8 byte offset.
/// If `utf16_col` points into the middle of a surrogate pair, clamps to the start of that character.
pub fn utf16_col_to_utf8_byte(line: &str, utf16_col: u32) -> usize {
    let mut current_utf16 = 0u32;
    for (idx, ch) in line.char_indices() {
        if current_utf16 >= utf16_col {
            return idx;
        }
        let width = ch.len_utf16() as u32;
        if current_utf16 + width > utf16_col {
            return idx;
        }
        current_utf16 += width;
    }
    line.len()
}

/// Convert an LSP UTF-16 `Position` in a full document to a UTF-8 byte offset.
/// Correctly handles both LF (`\n`) and CRLF (`\r\n`) line endings.
pub fn position_to_byte_offset(text: &str, position: Position) -> usize {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    let bytes = text.as_bytes();
    let len = bytes.len();

    while line_start <= len {
        let mut line_end = line_start;
        while line_end < len && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        let content_end = if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };

        if current_line == position.line {
            let line_slice = &text[line_start..content_end];
            return line_start + utf16_col_to_utf8_byte(line_slice, position.character);
        }

        if line_end >= len {
            break;
        }
        line_start = line_end + 1;
        current_line += 1;
    }

    text.len()
}

/// Convert a UTF-8 byte offset in a full document to an LSP UTF-16 `Position`.
pub fn byte_offset_to_position(text: &str, byte_offset: usize) -> Position {
    let clamped = byte_offset.min(text.len());
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    let bytes = text.as_bytes();
    let len = bytes.len();

    while line_start <= len {
        let mut line_end = line_start;
        while line_end < len && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        let content_end = if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };

        if clamped <= line_end || line_end >= len {
            let in_line = clamped.min(content_end).saturating_sub(line_start);
            let line_slice = &text[line_start..content_end];
            let col = utf8_byte_to_utf16_col(line_slice, in_line);
            return Position::new(current_line, col);
        }

        line_start = line_end + 1;
        current_line += 1;
    }

    Position::new(current_line, 0)
}

/// Apply LSP `contentChanges` (supports both full-document sync and UTF-16 ranged incremental edits).
pub fn apply_content_changes(text: &mut String, changes: &[Value]) {
    for change in changes {
        let Some(replacement) = change.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some(range_val) = change.get("range").filter(|v| !v.is_null()) {
            if let Some(range) = Range::from_json(range_val) {
                let start_byte = position_to_byte_offset(text, range.start);
                let end_byte = position_to_byte_offset(text, range.end).max(start_byte);
                text.replace_range(start_byte..end_byte, replacement);
                continue;
            }
        }
        *text = replacement.to_owned();
    }
}

/// Find the byte span `(start_byte, end_byte)` of a typed hole `??(...)` on a single line,
/// ignoring `??` inside string literals or after `//` line comments.
pub fn find_hole_byte_span_in_line(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_string = false;

    while i < bytes.len() {
        if in_string {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        if bytes[i] == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'?' && i + 1 < bytes.len() && bytes[i + 1] == b'?' {
            let hole_start = i;
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                let mut depth = 0usize;
                let mut hole_in_string = false;
                while j < bytes.len() {
                    if hole_in_string {
                        if bytes[j] == b'\\' {
                            j = (j + 2).min(bytes.len());
                            continue;
                        }
                        if bytes[j] == b'"' {
                            hole_in_string = false;
                        }
                        j += 1;
                        continue;
                    }
                    if bytes[j] == b'"' {
                        hole_in_string = true;
                    } else if bytes[j] == b'(' {
                        depth += 1;
                    } else if bytes[j] == b')' {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some((hole_start, j + 1));
                        }
                    }
                    j += 1;
                }
                return Some((hole_start, bytes.len()));
            }
            return Some((hole_start, i + 2));
        }
        i += 1;
    }
    None
}

/// Compute an accurate UTF-16 `Range` for a 1-based compiler line number.
/// If `is_hole` is true and the line contains `??(...)`, highlights the exact `??(...)` expression.
/// Otherwise highlights the non-whitespace code span on that line.
pub fn line_diagnostic_range(text: &str, one_based_line: usize, is_hole: bool) -> Range {
    let lines = split_lines(text);
    if lines.is_empty() {
        return Range::new(Position::new(0, 0), Position::new(0, 0));
    }
    let line_idx = one_based_line.saturating_sub(1).min(lines.len() - 1);
    let line = lines[line_idx];
    let lsp_line = line_idx as u32;

    if is_hole {
        if let Some((start_b, end_b)) = find_hole_byte_span_in_line(line) {
            return Range::new(
                Position::new(lsp_line, utf8_byte_to_utf16_col(line, start_b)),
                Position::new(lsp_line, utf8_byte_to_utf16_col(line, end_b)),
            );
        }
    }

    let code_end_limit = find_comment_start_byte(line).unwrap_or(line.len());
    let code_slice = &line[..code_end_limit];
    let (start_b, end_b) = if !code_slice.trim().is_empty() {
        let leading = code_slice.len() - code_slice.trim_start().len();
        let trailing = code_slice.trim_end().len();
        (leading, trailing)
    } else if !line.trim().is_empty() {
        let leading = line.len() - line.trim_start().len();
        let trailing = line.trim_end().len();
        (leading, trailing)
    } else {
        (0, line.len())
    };

    Range::new(
        Position::new(lsp_line, utf8_byte_to_utf16_col(line, start_b)),
        Position::new(lsp_line, utf8_byte_to_utf16_col(line, end_b)),
    )
}

fn find_comment_start_byte(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        if in_string {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_byte_inside_string_or_comment(line: &str, target_byte: usize) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() && i <= target_byte {
        if in_string {
            if bytes[i] == b'\\' {
                if target_byte <= i + 1 {
                    return true;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == b'"' {
                if i == target_byte {
                    return true;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return target_byte >= i;
        }
        if bytes[i] == b'"' {
            in_string = true;
            if i == target_byte {
                return true;
            }
        }
        i += 1;
    }
    in_string
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoverTarget {
    /// Cursor is on a function name/call/declaration/annotation.
    Function {
        /// Extracted identifier or `::`-qualified path, e.g. `search::matching` or `positive_count`.
        symbol: String,
        /// UTF-16 range of the hovered token/path.
        range: Range,
        /// One-based line of the cursor.
        one_based_line: usize,
    },
    /// Cursor is directly on a `??(...)` typed hole expression.
    TypedHole {
        range: Range,
        one_based_line: usize,
        enclosing_function: Option<String>,
    },
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract the hover target at an LSP UTF-16 `Position` in `text`.
pub fn extract_hover_target(text: &str, position: Position) -> Option<HoverTarget> {
    let lines = split_lines(text);
    let line_idx = position.line as usize;
    if line_idx >= lines.len() {
        return None;
    }
    let line = lines[line_idx];
    let one_based_line = line_idx + 1;
    let byte_col = utf16_col_to_utf8_byte(line, position.character);

    // 1. Check if cursor is inside a typed hole `??(...)` on this line.
    if let Some((hole_start, hole_end)) = find_hole_byte_span_in_line(line) {
        if byte_col >= hole_start && byte_col <= hole_end {
            let range = Range::new(
                Position::new(position.line, utf8_byte_to_utf16_col(line, hole_start)),
                Position::new(position.line, utf8_byte_to_utf16_col(line, hole_end)),
            );
            let enclosing_function = find_enclosing_function_name(&lines, line_idx);
            return Some(HoverTarget::TypedHole {
                range,
                one_based_line,
                enclosing_function,
            });
        }
    }

    // 2. Check if cursor is on an annotation line `@effect(...)` or `@intent(...)` preceding a `fn`.
    let trimmed = line.trim_start();
    if trimmed.starts_with("@effect") || trimmed.starts_with("@intent") {
        if let Some(fn_name) = find_function_declared_at_or_below(&lines, line_idx) {
            let start_b = line.len() - trimmed.len();
            let end_b = line.trim_end().len();
            let range = Range::new(
                Position::new(position.line, utf8_byte_to_utf16_col(line, start_b)),
                Position::new(position.line, utf8_byte_to_utf16_col(line, end_b)),
            );
            return Some(HoverTarget::Function {
                symbol: fn_name,
                range,
                one_based_line,
            });
        }
    }

    if is_byte_inside_string_or_comment(line, byte_col) {
        return None;
    }

    // 3. Extract identifier or `::`-qualified path at `byte_col`.
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    let probe = if byte_col < bytes.len()
        && (is_ident_byte(bytes[byte_col]) || bytes[byte_col] == b':')
    {
        byte_col
    } else if byte_col > 0 && (is_ident_byte(bytes[byte_col - 1]) || bytes[byte_col - 1] == b':') {
        byte_col - 1
    } else {
        return None;
    };

    let mut start = probe;
    while start > 0 {
        if is_ident_byte(bytes[start - 1]) {
            start -= 1;
        } else if start >= 2 && bytes[start - 1] == b':' && bytes[start - 2] == b':' {
            start -= 2;
        } else {
            break;
        }
    }

    let mut end = probe;
    while end < bytes.len() {
        if is_ident_byte(bytes[end]) {
            end += 1;
        } else if end + 1 < bytes.len() && bytes[end] == b':' && bytes[end + 1] == b':' {
            end += 2;
        } else {
            break;
        }
    }

    // Trim any stray leading/trailing `:`
    while start < end && bytes[start] == b':' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b':' {
        end -= 1;
    }

    if start >= end {
        return None;
    }

    let raw_symbol = &line[start..end];
    if raw_symbol
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit())
    {
        return None;
    }

    // If user hovered on `pub`, `fn`, `test`, `requires`, or `ensures` on a function declaration line,
    // resolve to that declared function's name.
    if ["pub", "fn", "test", "requires", "ensures"].contains(&raw_symbol) {
        if let Some(fn_name) = find_function_declared_at_or_below(&lines, line_idx)
            .or_else(|| find_enclosing_function_name(&lines, line_idx))
        {
            let range = Range::new(
                Position::new(position.line, utf8_byte_to_utf16_col(line, start)),
                Position::new(position.line, utf8_byte_to_utf16_col(line, end)),
            );
            return Some(HoverTarget::Function {
                symbol: fn_name,
                range,
                one_based_line,
            });
        }
        return None;
    }

    // Ignore non-function keywords
    if [
        "let", "mut", "return", "if", "else", "while", "true", "false", "spawn", "assert", "print",
        "result", "mod", "crate", "struct", "enum", "match", "int", "i8", "i16", "i32", "i64",
        "u8", "u16", "u32", "u64", "bool", "text", "ints", "texts",
    ]
    .contains(&raw_symbol)
    {
        return None;
    }

    let range = Range::new(
        Position::new(position.line, utf8_byte_to_utf16_col(line, start)),
        Position::new(position.line, utf8_byte_to_utf16_col(line, end)),
    );

    Some(HoverTarget::Function {
        symbol: raw_symbol.to_owned(),
        range,
        one_based_line,
    })
}

/// Find the name of a function declared on `line_idx` or immediately following `@intent`/`@effect` lines.
pub fn find_function_declared_at_or_below(lines: &[&str], start_line_idx: usize) -> Option<String> {
    for line in lines.iter().skip(start_line_idx).take(6) {
        let code = match find_comment_start_byte(line) {
            Some(idx) => &line[..idx],
            None => *line,
        }
        .trim();
        if code.is_empty() || code.starts_with('@') {
            continue;
        }
        if let Some(name) = parse_fn_decl_name(code) {
            return Some(name);
        }
        break;
    }
    None
}

/// Find the function containing `line_idx` by scanning backwards for `fn <name>` or `test <name>`.
pub fn find_enclosing_function_name(lines: &[&str], line_idx: usize) -> Option<String> {
    for idx in (0..=line_idx.min(lines.len().saturating_sub(1))).rev() {
        let line = lines[idx];
        let code = match find_comment_start_byte(line) {
            Some(c) => &line[..c],
            None => line,
        }
        .trim();
        if let Some(name) = parse_fn_decl_name(code) {
            return Some(name);
        }
    }
    None
}

fn parse_fn_decl_name(trimmed_code: &str) -> Option<String> {
    let rest = if let Some(r) = trimmed_code.strip_prefix("pub ") {
        r.trim_start()
    } else {
        trimmed_code
    };
    let after_kw = if let Some(r) = rest.strip_prefix("fn ") {
        r.trim_start()
    } else {
        let r = rest.strip_prefix("test ")?;
        r.trim_start()
    };
    let name_len = after_kw.bytes().take_while(|b| is_ident_byte(*b)).count();
    if name_len == 0 {
        return None;
    }
    Some(after_kw[..name_len].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf8_utf16_conversion_with_bmp_and_astral_chars() {
        // 'Привет' = 6 chars, 12 UTF-8 bytes, 6 UTF-16 code units
        // ' ' = 1 byte, 1 UTF-16 unit
        // '🚀' = 1 char (U+1F680), 4 UTF-8 bytes, 2 UTF-16 code units
        // ' ' = 1 byte, 1 UTF-16 unit
        // 'fn' = 2 bytes, 2 UTF-16 units
        let line = "let s = \"Привет 🚀\"; positive_count(&v);";
        let fn_byte = line.find("positive_count").unwrap();
        let utf16_col = utf8_byte_to_utf16_col(line, fn_byte);
        // "let s = \"" (9) + "Привет" (6) + " " (1) + "🚀" (2) + "\"; " (3) = 21 UTF-16 units
        assert_eq!(utf16_col, 21);
        assert_eq!(utf16_col_to_utf8_byte(line, utf16_col), fn_byte);

        // Mid-surrogate pair clamps safely to start of '🚀'
        let rocket_byte = line.find('🚀').unwrap();
        let rocket_utf16 = utf8_byte_to_utf16_col(line, rocket_byte);
        assert_eq!(rocket_utf16, 16);
        assert_eq!(utf16_col_to_utf8_byte(line, 17), rocket_byte);
        assert_eq!(utf16_col_to_utf8_byte(line, 18), rocket_byte + 4);
    }

    #[test]
    fn test_hole_span_with_unicode_prefix() {
        let text =
            "fn f() -> int {\r\n    let crab = \"🦀\"; return ??(int, \"счёт 🚀\");\r\n}\r\n";
        let range = line_diagnostic_range(text, 2, true);
        assert_eq!(range.start.line, 1);
        assert_eq!(range.end.line, 1);
        // "    let crab = \"" (16) + "🦀" (2) + "\"; return " (10) = 28 UTF-16 units
        assert_eq!(range.start.character, 28);
        // "??(int, \"счёт 🚀\")" -> "??(int, \"" (9) + "счёт " (5) + "🚀" (2) + "\")" (2) = 18 UTF-16 units
        assert_eq!(range.end.character, 46);
    }

    #[test]
    fn test_incremental_content_changes_with_unicode_and_crlf() {
        let mut doc = "fn main() {\r\n    let a = \"🦀\";\r\n    print 1;\r\n}\r\n".to_owned();
        // Replace `1` on line 2 (0-based) with `42`
        let changes = vec![json!({
            "range": {
                "start": { "line": 2, "character": 10 },
                "end": { "line": 2, "character": 11 }
            },
            "text": "42"
        })];
        apply_content_changes(&mut doc, &changes);
        assert!(doc.contains("print 42;"));
    }

    #[test]
    fn test_extract_hover_target_qualified_and_unicode() {
        let src = "mod search;\nfn main() {\n    let prefix = \"🚀\"; let r = search::matching(&lines, &q);\n}\n";
        let line = src.lines().nth(2).unwrap();
        let byte_idx = line.find("matching").unwrap() + 2;
        let col = utf8_byte_to_utf16_col(line, byte_idx);
        let target = extract_hover_target(src, Position::new(2, col)).unwrap();
        assert_eq!(
            target,
            HoverTarget::Function {
                symbol: "search::matching".into(),
                range: Range::new(
                    Position::new(
                        2,
                        utf8_byte_to_utf16_col(line, line.find("search::matching").unwrap())
                    ),
                    Position::new(
                        2,
                        utf8_byte_to_utf16_col(
                            line,
                            line.find("search::matching").unwrap() + "search::matching".len()
                        )
                    ),
                ),
                one_based_line: 3,
            }
        );
    }
}
