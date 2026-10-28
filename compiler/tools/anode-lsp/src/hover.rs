use crate::{
    diagnostics::{format_hole_capabilities, format_hole_effects, format_hole_scope},
    project::strip_verbatim_and_normalize_drive,
    unicode::Range,
};
use serde_json::{json, Value};
use std::path::Path;

/// Format a successful `anode context` JSON result into an LSP `Hover` object.
pub fn format_context_hover(result: &Value, range: Option<Range>) -> Value {
    let function = result
        .get("function")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let line = result.get("line").and_then(Value::as_u64).unwrap_or(0);
    let file_display = result
        .get("file")
        .and_then(Value::as_str)
        .map(|f| {
            strip_verbatim_and_normalize_drive(Path::new(f))
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "<unknown>".to_owned());

    let returns = result
        .get("returns")
        .and_then(Value::as_str)
        .unwrap_or("unit");

    let intents: Vec<&str> = result
        .get("intents")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let intents_advisory = result
        .get("intents_are_advisory")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let effects_obj = result.get("effects");
    let effect_contracts: Vec<&str> = effects_obj
        .and_then(|e| e.get("contracts"))
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let may_allocate = effects_obj
        .and_then(|e| e.get("may_allocate"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let may_do_io = effects_obj
        .and_then(|e| e.get("may_do_io"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let params = result
        .get("parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut sig_lines = Vec::new();
    for intent in &intents {
        sig_lines.push(format!("@intent({intent:?})"));
    }
    if !effect_contracts.is_empty() {
        sig_lines.push(format!("@effect({})", effect_contracts.join(", ")));
    }

    let formatted_params: Vec<String> = params.iter().map(format_parameter_signature).collect();
    let ret_suffix = if returns == "unit" || returns.is_empty() {
        String::new()
    } else {
        format!(" -> {returns}")
    };
    sig_lines.push(format!(
        "fn {function}({}){ret_suffix}",
        formatted_params.join(", ")
    ));

    let mut md = Vec::new();
    md.push(format!("```anode\n{}\n```", sig_lines.join("\n")));
    md.push(format!("**Defined at:** `{file_display}:{line}`"));

    if !intents.is_empty() {
        let advisory_note = if intents_advisory {
            " *(advisory; not compiler-enforced)*"
        } else {
            ""
        };
        let list = intents
            .iter()
            .map(|i| format!("\"{i}\""))
            .collect::<Vec<_>>()
            .join("; ");
        md.push(format!("**Intent{advisory_note}:** {list}"));
    }

    let contracts_display = if effect_contracts.is_empty() {
        "none".to_owned()
    } else {
        format!("`@effect({})`", effect_contracts.join(", "))
    };
    md.push(format!(
        "**Effects:** contracts: {contracts_display} | inferred: `may_allocate = {may_allocate}`, `may_do_io = {may_do_io}`"
    ));

    // Inferred & declared parameter views
    if let Some(views) = result.get("views").and_then(Value::as_array) {
        if !params.is_empty() && !views.is_empty() {
            let mut view_items = Vec::new();
            for (idx, param) in params.iter().enumerate() {
                let p_name = param.get("name").and_then(Value::as_str).unwrap_or("_");
                let borrow = param
                    .get("borrow")
                    .and_then(Value::as_str)
                    .unwrap_or("Owned");
                if borrow == "Owned" {
                    view_items.push(format!("`{p_name}`: owned"));
                    continue;
                }
                let inferred = views
                    .get(idx)
                    .and_then(Value::as_array)
                    .map(|accesses| format_inferred_view_accesses(accesses))
                    .unwrap_or_else(|| "none".to_owned());
                view_items.push(format!("`{p_name}` ({borrow}): {inferred}"));
            }
            md.push(format!("**Inferred Views:** {}", view_items.join(", ")));
        }
    }

    // Relevant signature types
    if let Some(types) = result.get("types").and_then(Value::as_array) {
        if !types.is_empty() {
            let mut type_lines = Vec::new();
            for t in types {
                let t_name = t.get("name").and_then(Value::as_str).unwrap_or("?");
                let t_def = t.get("definition").and_then(Value::as_str).unwrap_or("?");
                type_lines.push(format!("`{t_name}`: `{t_def}`"));
            }
            md.push(format!("**Signature Types:** {}", type_lines.join("; ")));
        }
    }

    let callers: Vec<&str> = result
        .get("callers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let callees: Vec<&str> = result
        .get("callees")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let callers_str = if callers.is_empty() {
        "none".to_owned()
    } else {
        callers
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let callees_str = if callees.is_empty() {
        "none".to_owned()
    } else {
        callees
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    md.push(format!(
        "**Callers:** {callers_str} | **Callees:** {callees_str}"
    ));

    build_hover_json(md.join("\n\n"), range)
}

fn format_parameter_signature(param: &Value) -> String {
    let name = param.get("name").and_then(Value::as_str).unwrap_or("_");
    let ty = param
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let borrow = param
        .get("borrow")
        .and_then(Value::as_str)
        .unwrap_or("Owned");

    let prefix = match borrow {
        "Shared" => "&",
        "Exclusive" => "&mut ",
        _ => "",
    };

    let view_suffix = if let Some(view_arr) = param
        .get("declared_view")
        .filter(|v| !v.is_null())
        .and_then(Value::as_array)
    {
        let fields: Vec<String> = view_arr
            .iter()
            .map(|vf| {
                let mutable = vf.get("mutable").and_then(Value::as_bool).unwrap_or(false);
                let path = vf
                    .get("path")
                    .and_then(Value::as_array)
                    .map(|p| {
                        p.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                if mutable {
                    format!("mut {path}")
                } else {
                    path
                }
            })
            .collect();
        format!(".{{{}}}", fields.join(", "))
    } else {
        String::new()
    };

    format!("{name}: {prefix}{ty}{view_suffix}")
}

fn format_inferred_view_accesses(accesses: &[Value]) -> String {
    if accesses.is_empty() {
        return "unused/read-only".to_owned();
    }
    let parts: Vec<String> = accesses
        .iter()
        .map(|acc| {
            let mutable = acc.get("mutable").and_then(Value::as_bool).unwrap_or(false);
            let mode = if mutable { "mut" } else { "read" };
            let indices: Vec<String> = acc
                .get("field_indices")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_u64)
                        .map(|n| n.to_string())
                        .collect()
                })
                .unwrap_or_default();
            if indices.is_empty() {
                format!("whole-owner ({mode})")
            } else {
                format!("field_indices [{}] ({mode})", indices.join("."))
            }
        })
        .collect();
    parts.join(", ")
}

/// Format a Markdown `Hover` for a `synthesis_hole` (`??(type, "intent")`) diagnostic.
pub fn format_hole_hover(details: &Value, range: Option<Range>) -> Value {
    let declared_type = details
        .get("declared_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let intent = details.get("intent").and_then(Value::as_str).unwrap_or("");
    let scope = format_hole_scope(details.get("scope"));
    let effects = format_hole_effects(details.get("effect_contracts"));
    let capabilities = format_hole_capabilities(details);

    let mut md = Vec::new();
    md.push(format!("```anode\n??({declared_type}, {intent:?})\n```"));
    md.push(format!(
        "**Expected Declared Type:** `{declared_type}` *(explicitly declared, not inferred)*"
    ));
    if !intent.is_empty() {
        md.push(format!("**Hole Intent:** {intent:?}"));
    }
    md.push(format!(
        "**Available Variables & Ownership State:**\n- {}",
        scope.replace("; ", "\n- ")
    ));
    md.push(format!("**Effect Constraints:** `{effects}`"));
    md.push(format!("**Project Capabilities:** `{capabilities}`"));

    build_hover_json(md.join("\n\n"), range)
}

/// Format a hover explaining why `anode context` cannot currently inspect `function_name`
/// when the compiler rejects `context` due to an active compile error or unresolved hole.
pub fn format_context_unavailable_hover(
    function_name: &str,
    compiler_diag: &Value,
    range: Option<Range>,
) -> Value {
    let line = compiler_diag
        .get("line")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let msg = compiler_diag
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("project failed compiler check");

    let mut md = Vec::new();
    md.push(format!("**Function Context:** `{function_name}`"));
    md.push(format!(
        "Compiler `context` query is currently unavailable because `anode context` requires a checkable project (`Project::checked(false)`).\n\n**Blocking compiler diagnostic (line {line}):** {msg}"
    ));

    if let Some(details) = compiler_diag.get("details").filter(|d| !d.is_null()) {
        if details.get("kind").and_then(Value::as_str) == Some("synthesis_hole") {
            let declared_type = details
                .get("declared_type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let effects = format_hole_effects(details.get("effect_contracts"));
            let scope = format_hole_scope(details.get("scope"));
            md.push(format!(
                "**Active Typed Hole in Project:** expected `{declared_type}`, effects `{effects}`, scope: {scope}"
            ));
        }
    }

    build_hover_json(md.join("\n\n"), range)
}

fn build_hover_json(markdown: String, range: Option<Range>) -> Value {
    let mut hover = json!({
        "contents": {
            "kind": "markdown",
            "value": markdown,
        }
    });
    if let Some(r) = range {
        hover["range"] = r.to_json();
    }
    hover
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_context_hover_with_views_and_effects() {
        let ctx = json!({
            "function": "buffer::append",
            "file": r"\\?\C:\proj\examples\views\src\buffer.zn",
            "line": 5,
            "parameters": [
                {
                    "name": "b",
                    "type": "buffer::Buffer",
                    "borrow": "Exclusive",
                    "declared_view": [
                        {"mutable": true, "path": ["samples"]},
                        {"mutable": false, "path": ["calibration"]}
                    ]
                },
                {
                    "name": "input",
                    "type": "ints",
                    "borrow": "Shared",
                    "declared_view": null
                }
            ],
            "returns": "unit",
            "effects": {
                "contracts": ["no_io"],
                "may_allocate": true,
                "may_do_io": false
            },
            "intents": ["Append calibrated samples"],
            "intents_are_advisory": true,
            "views": [
                [{"field_indices": [0], "mutable": true}, {"field_indices": [1], "mutable": false}],
                [{"field_indices": [], "mutable": false}]
            ],
            "types": [
                {"name": "buffer::Buffer", "definition": "Record([FieldDecl { name: \"samples\", ty: Ints }])"}
            ],
            "callees": [],
            "callers": ["main"]
        });

        let hover = format_context_hover(&ctx, None);
        let value = hover["contents"]["value"].as_str().unwrap();
        assert!(value.contains(
            "fn buffer::append(b: &mut buffer::Buffer.{mut samples, calibration}, input: &ints)"
        ));
        assert!(value.contains("@effect(no_io)"));
        assert!(value.contains("Append calibrated samples"));
        assert!(value.contains("field_indices [0] (mut)"));
        assert!(value.contains("`main`"));
    }
}
