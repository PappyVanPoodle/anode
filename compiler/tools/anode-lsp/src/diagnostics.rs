use crate::unicode::{line_diagnostic_range, Range};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct ConvertedDiagnostic {
    pub file: Option<String>,
    pub line: usize,
    pub is_hole: bool,
    pub message: String,
    pub details: Option<Value>,
}

/// Parse a single compiler diagnostic JSON object and format its message (including full typed-hole synthesis details).
pub fn parse_compiler_diagnostic(raw: &Value) -> ConvertedDiagnostic {
    let line = raw.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
    let file = raw
        .get("file")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let base_message = raw
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("compiler check failed");
    let details = raw.get("details").filter(|v| !v.is_null()).cloned();

    let is_hole = details
        .as_ref()
        .and_then(|d| d.get("kind"))
        .and_then(Value::as_str)
        == Some("synthesis_hole");

    let message = if is_hole {
        format_hole_diagnostic_message(base_message, details.as_ref().unwrap())
    } else {
        base_message.to_owned()
    };

    ConvertedDiagnostic {
        file,
        line,
        is_hole,
        message,
        details,
    }
}

/// Format a rich human-and-agent-readable message for a `synthesis_hole` diagnostic.
pub fn format_hole_diagnostic_message(base_message: &str, details: &Value) -> String {
    let declared_type = details
        .get("declared_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let not_inferred = details
        .get("declared_type_is_not_inferred")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let intent = details.get("intent").and_then(Value::as_str).unwrap_or("");

    let type_note = if not_inferred {
        format!("{declared_type} (explicitly declared, not inferred)")
    } else {
        declared_type.to_owned()
    };

    let scope_summary = format_hole_scope(details.get("scope"));
    let effects_summary = format_hole_effects(details.get("effect_contracts"));
    let capabilities_summary = format_hole_capabilities(details);

    let mut lines = Vec::new();
    if intent.is_empty() {
        lines.push(format!("Typed hole ??({declared_type}): {base_message}"));
    } else {
        lines.push(format!(
            "Typed hole ??({declared_type}, {:?}): {base_message}",
            intent
        ));
    }
    lines.push(format!("Expected declared type: {type_note}"));
    if !intent.is_empty() {
        lines.push(format!("Intent: {intent:?}"));
    }
    lines.push(format!("Available variables: {scope_summary}"));
    lines.push(format!("Effect constraints: {effects_summary}"));
    lines.push(format!("Project capabilities: {capabilities_summary}"));

    lines.join("\n")
}

pub fn format_hole_scope(scope_val: Option<&Value>) -> String {
    let Some(items) = scope_val.and_then(Value::as_array) else {
        return "none".to_owned();
    };
    if items.is_empty() {
        return "none".to_owned();
    }

    let mut formatted = Vec::with_capacity(items.len());
    for item in items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("_");
        let ty = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let borrow = item
            .get("borrow")
            .and_then(Value::as_str)
            .unwrap_or("Owned");
        let mutable = item
            .get("mutable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let moved = item.get("moved").and_then(Value::as_bool).unwrap_or(false);
        let moved_fields = item
            .get("moved_fields")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|path| {
                        path.as_array()
                            .map(|p| {
                                p.iter()
                                    .filter_map(Value::as_u64)
                                    .map(|idx| idx.to_string())
                                    .collect::<Vec<_>>()
                                    .join(".")
                            })
                            .unwrap_or_default()
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let borrowed_in_expr = item
            .get("borrowed_in_expression")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let exclusive_in_expr = item
            .get("exclusive_in_expression")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let type_display = match borrow {
            "Shared" => format!("&{ty}"),
            "Exclusive" => format!("&mut {ty}"),
            _ if mutable => format!("mut {ty}"),
            _ => ty.to_owned(),
        };

        let mut ownership_parts = Vec::new();
        ownership_parts.push(format!("borrow: {borrow}"));
        ownership_parts.push(format!("mutable: {mutable}"));
        if moved {
            ownership_parts.push("ownership: moved".to_owned());
        } else if !moved_fields.is_empty() {
            ownership_parts.push(format!(
                "ownership: partially moved (moved_fields: [{}])",
                moved_fields.join(", ")
            ));
        } else {
            ownership_parts.push("ownership: live".to_owned());
        }

        if exclusive_in_expr {
            ownership_parts.push("loan: exclusive_in_expression".to_owned());
        } else if borrowed_in_expr {
            ownership_parts.push("loan: borrowed_in_expression".to_owned());
        }

        formatted.push(format!(
            "{name}: {type_display} ({})",
            ownership_parts.join(", ")
        ));
    }

    formatted.join("; ")
}

pub fn format_hole_effects(effects_val: Option<&Value>) -> String {
    let Some(arr) = effects_val.and_then(Value::as_array) else {
        return "none".to_owned();
    };
    let effects: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
    if effects.is_empty() {
        "none".to_owned()
    } else {
        format!("@effect({})", effects.join(", "))
    }
}

pub fn format_hole_capabilities(details: &Value) -> String {
    if details
        .get("capabilities_unrestricted")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return "unrestricted".to_owned();
    }
    if let Some(caps) = details
        .get("project_capabilities")
        .and_then(Value::as_array)
    {
        let names: Vec<&str> = caps.iter().filter_map(Value::as_str).collect();
        if names.is_empty() {
            "none (all capabilities disabled)".to_owned()
        } else {
            names.join(", ")
        }
    } else {
        "none".to_owned()
    }
}

/// Build an LSP `Diagnostic` JSON object from a `ConvertedDiagnostic` and the target file's current source text.
pub fn to_lsp_diagnostic(diag: &ConvertedDiagnostic, source_text: &str) -> Value {
    let range: Range = line_diagnostic_range(source_text, diag.line, diag.is_hole);
    let mut obj = json!({
        "range": range.to_json(),
        "severity": 1, // DiagnosticSeverity::Error
        "source": "anode",
        "message": diag.message,
    });

    if diag.is_hole {
        obj["code"] = json!("synthesis_hole");
    }
    if let Some(details) = &diag.details {
        obj["data"] = details.clone();
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typed_hole_diagnostic_formatting() {
        let compiler_diag = json!({
            "severity": "error",
            "line": 6,
            "file": r"\\?\C:\proj\examples\agent-sketch.an",
            "message": "unresolved typed hole; implement it before checking or building",
            "details": {
                "kind": "synthesis_hole",
                "declared_type": "int",
                "declared_type_is_not_inferred": true,
                "intent": "count values greater than zero",
                "effect_contracts": ["no_alloc", "no_io"],
                "capabilities_unrestricted": true,
                "project_capabilities": null,
                "scope": [
                    {
                        "name": "values",
                        "type": "ints",
                        "borrow": "Shared",
                        "mutable": false,
                        "moved": false,
                        "moved_fields": [],
                        "borrowed_in_expression": false,
                        "exclusive_in_expression": false
                    }
                ]
            }
        });

        let parsed = parse_compiler_diagnostic(&compiler_diag);
        assert!(parsed.is_hole);
        assert!(parsed
            .message
            .contains("Expected declared type: int (explicitly declared, not inferred)"));
        assert!(parsed
            .message
            .contains("Intent: \"count values greater than zero\""));
        assert!(parsed
            .message
            .contains("values: &ints (borrow: Shared, mutable: false, ownership: live)"));
        assert!(parsed
            .message
            .contains("Effect constraints: @effect(no_alloc, no_io)"));
        assert!(parsed
            .message
            .contains("Project capabilities: unrestricted"));

        let source = "// comment\n// comment\n@intent(\"...\")\n@effect(no_alloc, no_io)\nfn positive_count(values: &ints) -> int {\n    return ??(int, \"count values greater than zero\");\n}\n";
        let lsp_diag = to_lsp_diagnostic(&parsed, source);
        assert_eq!(lsp_diag["code"], "synthesis_hole");
        assert_eq!(lsp_diag["range"]["start"]["line"], 5);
        assert_eq!(lsp_diag["range"]["start"]["character"], 11);
        assert_eq!(lsp_diag["range"]["end"]["character"], 52);
        assert_eq!(lsp_diag["data"]["declared_type"], "int");
    }
}
