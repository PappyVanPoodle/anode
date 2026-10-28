use crate::{
    project::normalize_path,
    unicode::{split_lines, utf16_col_to_utf8_byte, utf8_byte_to_utf16_col, Position, Range},
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Convert an LSP 0-based UTF-16 `Position` to compiler 1-based `(line, utf8_byte_column)`.
pub fn lsp_pos_to_compiler_coords(text: &str, pos: Position) -> Option<(usize, usize)> {
    let lines = split_lines(text);
    let line_idx = pos.line as usize;
    let line_text = if line_idx < lines.len() {
        lines[line_idx]
    } else if line_idx == 0 && lines.is_empty() {
        ""
    } else {
        return None;
    };
    let utf8_byte = utf16_col_to_utf8_byte(line_text, pos.character);
    Some((line_idx + 1, utf8_byte + 1))
}

/// Convert a compiler 1-based `{"line": L, "column": C}` position to an LSP 0-based UTF-16 `Position`.
pub fn compiler_pos_to_lsp_pos(text: &str, nav_pos: &Value) -> Option<Position> {
    let line_1 = nav_pos.get("line").and_then(Value::as_u64)? as usize;
    let col_1 = nav_pos.get("column").and_then(Value::as_u64)? as usize;
    if line_1 == 0 || col_1 == 0 {
        return None;
    }
    let lines = split_lines(text);
    let line_0 = line_1 - 1;
    let utf8_byte = col_1 - 1;
    let character = if let Some(line_text) = lines.get(line_0) {
        utf8_byte_to_utf16_col(line_text, utf8_byte)
    } else {
        0
    };
    Some(Position {
        line: line_0 as u32,
        character,
    })
}

/// Convert a compiler `{"start": ..., "end": ...}` range to an LSP 0-based UTF-16 `Range`.
pub fn compiler_range_to_lsp_range(text: &str, nav_range: &Value) -> Option<Range> {
    let start = compiler_pos_to_lsp_pos(text, nav_range.get("start")?)?;
    let end = compiler_pos_to_lsp_pos(text, nav_range.get("end")?)?;
    Some(Range { start, end })
}

/// Map compiler symbol kind strings to LSP `SymbolKind` numbers.
pub fn symbol_kind_to_lsp_number(kind: &str) -> u32 {
    match kind {
        "module" => 2,     // SymbolKind.Module
        "struct" => 23,    // SymbolKind.Struct
        "enum" => 10,      // SymbolKind.Enum
        "field" => 8,      // SymbolKind.Field
        "variant" => 22,   // SymbolKind.EnumMember
        "function" => 12,  // SymbolKind.Function
        "test" => 12,      // SymbolKind.Function
        "parameter" => 13, // SymbolKind.Variable
        "local" => 13,     // SymbolKind.Variable
        "hole" => 13,      // A typed synthesis slot, with an exact selection span
        _ => 13,
    }
}

fn pos_le(a: Position, b: Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}

/// Convert a compiler outline symbol object into a hierarchical LSP `DocumentSymbol`.
pub fn convert_document_symbol(sym: &Value, text: &str) -> Option<Value> {
    let name = sym.get("name").and_then(Value::as_str)?;
    let kind_str = sym.get("kind").and_then(Value::as_str).unwrap_or("local");
    let detail = sym
        .get("detail")
        .and_then(Value::as_str)
        .or_else(|| sym.get("qualified_name").and_then(Value::as_str))
        .unwrap_or("");

    let sel_range = sym
        .get("selection_range")
        .and_then(|r| compiler_range_to_lsp_range(text, r))?;
    let mut full_range = sym
        .get("range")
        .and_then(|r| compiler_range_to_lsp_range(text, r))
        .unwrap_or(sel_range);

    // Ensure `range` strictly encloses `selectionRange` per LSP specification.
    if !pos_le(full_range.start, sel_range.start) {
        full_range.start = sel_range.start;
    }
    if !pos_le(sel_range.end, full_range.end) {
        full_range.end = sel_range.end;
    }

    let children = sym
        .get("children")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| convert_document_symbol(c, text))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(json!({
        "name": name,
        "detail": detail,
        "kind": symbol_kind_to_lsp_number(kind_str),
        "range": full_range.to_json(),
        "selectionRange": sel_range.to_json(),
        "children": children,
    }))
}

/// Resolve the target file path from a compiler `symbol` JSON object.
pub fn resolve_symbol_target_path(
    symbol: &Value,
    fallback_path: &Path,
    project_root: &Path,
) -> PathBuf {
    if let Some(p_str) = symbol.get("path").and_then(Value::as_str) {
        if !p_str.is_empty() && p_str != "<source>" {
            let p = Path::new(p_str);
            if p.is_absolute() {
                return normalize_path(p);
            }
            return normalize_path(&project_root.join(p));
        }
    }
    if let Some(f_str) = symbol.get("file").and_then(Value::as_str) {
        if !f_str.is_empty() && f_str != "<source>" {
            let p = Path::new(f_str);
            if p.is_absolute() {
                return normalize_path(p);
            }
            return normalize_path(&project_root.join(p));
        }
    }
    normalize_path(fallback_path)
}

/// Convert a compiler `symbol_at` result payload into an LSP `Location` (`{"uri": ..., "range": ...}`).
pub fn convert_symbol_at_to_location<F>(
    result: &Value,
    fallback_path: &Path,
    project_root: &Path,
    resolve_file: F,
) -> Option<Value>
where
    F: Fn(&Path) -> (String, String),
{
    let symbol = result.get("symbol")?;
    if matches!(
        symbol.get("kind").and_then(Value::as_str),
        Some("builtin" | "keyword")
    ) {
        return None;
    }
    if symbol.is_null() {
        return None;
    }
    let target_path = resolve_symbol_target_path(symbol, fallback_path, project_root);
    let (target_uri, target_text) = resolve_file(&target_path);

    let nav_range = symbol
        .get("selection_range")
        .or_else(|| symbol.get("range"))?;
    let lsp_range = compiler_range_to_lsp_range(&target_text, nav_range)?;

    Some(json!({
        "uri": target_uri,
        "range": lsp_range.to_json(),
    }))
}

/// Convert a compiler `references` result payload into an array of LSP `Location` objects.
pub fn convert_references_to_locations<F>(
    result: &Value,
    fallback_path: &Path,
    project_root: &Path,
    resolve_file: F,
) -> Vec<Value>
where
    F: Fn(&Path) -> (String, String),
{
    let Some(refs) = result.get("references").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut locations = Vec::with_capacity(refs.len());
    for item in refs {
        let target_path = resolve_symbol_target_path(item, fallback_path, project_root);
        let (target_uri, target_text) = resolve_file(&target_path);
        let Some(nav_range) = item.get("range") else {
            continue;
        };
        let Some(lsp_range) = compiler_range_to_lsp_range(&target_text, nav_range) else {
            continue;
        };
        locations.push(json!({
            "uri": target_uri,
            "range": lsp_range.to_json(),
        }));
    }
    locations
}

/// Convert a compiler `prepare_rename` result payload into an LSP `PrepareRenameResult`
/// (`{"range": ..., "placeholder": ...}`).
pub fn convert_prepare_rename_result(result: &Value, doc_text: &str) -> Option<Value> {
    if !result
        .get("can_rename")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let nav_range = result.get("range")?;
    let lsp_range = compiler_range_to_lsp_range(doc_text, nav_range)?;
    let placeholder = result
        .get("placeholder")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(json!({
        "range": lsp_range.to_json(),
        "placeholder": placeholder,
    }))
}

/// Convert a compiler `rename` result payload into a versioned LSP `WorkspaceEdit`.
pub fn convert_rename_to_workspace_edit<F>(
    result: &Value,
    fallback_path: &Path,
    project_root: &Path,
    resolve_file: F,
) -> Option<Value>
where
    F: Fn(&Path) -> (String, String, Option<i64>),
{
    let edits = result.get("edits").and_then(Value::as_array)?;
    let mut grouped: std::collections::BTreeMap<PathBuf, (String, Option<i64>, Vec<Value>)> =
        std::collections::BTreeMap::new();

    for edit in edits {
        let target_path = resolve_symbol_target_path(edit, fallback_path, project_root);
        let (target_uri, target_text, doc_version) = resolve_file(&target_path);
        let nav_range = edit.get("range")?;
        let lsp_range = compiler_range_to_lsp_range(&target_text, nav_range)?;
        let new_text = edit.get("new_text").and_then(Value::as_str)?;
        let text_edit = json!({
            "range": lsp_range.to_json(),
            "newText": new_text,
        });
        let entry = grouped
            .entry(target_path)
            .or_insert_with(|| (target_uri, doc_version, Vec::new()));
        entry.2.push(text_edit);
    }

    let mut changes = serde_json::Map::new();
    let mut document_changes = Vec::with_capacity(grouped.len());
    for (_, (uri, version, file_edits)) in grouped {
        changes.insert(uri.clone(), json!(file_edits));
        document_changes.push(json!({
            "textDocument": {
                "uri": uri,
                "version": version,
            },
            "edits": file_edits,
        }));
    }

    Some(json!({
        "changes": Value::Object(changes),
        "documentChanges": document_changes,
        "_anodeMetadata": {
            "scope": result.get("scope").cloned().unwrap_or(json!("loaded_project")),
            "public_symbol": result.get("public_symbol").cloned().unwrap_or(json!(false)),
            "scope_warning": result.get("scope_warning").cloned().unwrap_or(Value::Null),
            "snapshot_fingerprint": result.get("snapshot_fingerprint").cloned().unwrap_or(Value::Null),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_navigation_coordinate_conversion_with_emoji_and_crlf() {
        let source = "fn demo() {\r\n    let msg = \"🦀🚀\"; let val = 1;\r\n}\r\n";
        // On line 1 (`    let msg = "🦀🚀"; let val = 1;`):
        // "    let msg = \"" is 15 ASCII chars (15 bytes, 15 UTF-16 units)
        // "🦀🚀" is 8 UTF-8 bytes, 4 UTF-16 units
        // "\"; let " is 7 ASCII chars (7 bytes, 7 UTF-16 units)
        // So `val` starts at UTF-16 col = 15 + 4 + 7 = 26, and UTF-8 byte = 15 + 8 + 7 = 30 (1-based col = 31).
        let (line_1, col_1) = lsp_pos_to_compiler_coords(
            source,
            Position {
                line: 1,
                character: 26,
            },
        )
        .unwrap();
        assert_eq!((line_1, col_1), (2, 31));

        let nav_range = json!({
            "start": { "line": 2, "column": 31, "byte": 44 },
            "end": { "line": 2, "column": 34, "byte": 47 }
        });
        let lsp_range = compiler_range_to_lsp_range(source, &nav_range).unwrap();
        assert_eq!(
            lsp_range.start,
            Position {
                line: 1,
                character: 26
            }
        );
        assert_eq!(
            lsp_range.end,
            Position {
                line: 1,
                character: 29
            }
        );
    }

    #[test]
    fn test_convert_document_symbol_encloses_selection_range() {
        let source = "struct Boxed {\n    item: int,\n}\n";
        let sym = json!({
            "id": "struct:main::Boxed@src/main.an:0..31",
            "name": "Boxed",
            "qualified_name": "Boxed",
            "kind": "struct",
            "detail": "struct Boxed",
            "range": {
                "start": { "line": 1, "column": 1, "byte": 0 },
                "end": { "line": 3, "column": 2, "byte": 31 }
            },
            "selection_range": {
                "start": { "line": 1, "column": 8, "byte": 7 },
                "end": { "line": 1, "column": 13, "byte": 12 }
            },
            "children": [
                {
                    "id": "field:main::Boxed.item@src/main.an:19..28",
                    "name": "item",
                    "qualified_name": "Boxed.item",
                    "kind": "field",
                    "detail": "item: int",
                    "range": {
                        "start": { "line": 2, "column": 5, "byte": 19 },
                        "end": { "line": 2, "column": 14, "byte": 28 }
                    },
                    "selection_range": {
                        "start": { "line": 2, "column": 5, "byte": 19 },
                        "end": { "line": 2, "column": 9, "byte": 23 }
                    },
                    "children": []
                }
            ]
        });

        let doc_sym = convert_document_symbol(&sym, source).unwrap();
        assert_eq!(doc_sym["name"], "Boxed");
        assert_eq!(doc_sym["kind"], 23); // Struct
        assert_eq!(doc_sym["selectionRange"]["start"]["line"], 0);
        assert_eq!(doc_sym["selectionRange"]["start"]["character"], 7);
        assert_eq!(doc_sym["children"][0]["name"], "item");
        assert_eq!(doc_sym["children"][0]["kind"], 8); // Field
    }
}
