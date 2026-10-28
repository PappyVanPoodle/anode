//! Read-only conversion of checked verifier evidence into versioned editor edits.
use crate::{
    navigation::compiler_range_to_lsp_range,
    project::{normalize_path, path_to_uri, ProjectLayout},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf};

fn fingerprint(source: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}:len={}", source.len())
}

pub fn decorate_verification(
    result: &mut Value,
    layout: &ProjectLayout,
    sources: &BTreeMap<PathBuf, String>,
    versions: &BTreeMap<PathBuf, i64>,
) {
    let snapshot = result.get("snapshot_fingerprint").cloned();
    if let Some(candidates) = result
        .get_mut("test_candidates")
        .and_then(Value::as_array_mut)
    {
        for candidate in candidates {
            // The compiler proposes only EOF insertions. Enforce that contract
            // again before exposing an editor WorkspaceEdit.
            if candidate.get("checked") != Some(&json!(true))
                || candidate.get("snapshot_fingerprint") != snapshot.as_ref()
                || candidate.get("writes") != Some(&json!(0))
            {
                continue;
            }
            let converted = (|| {
                let file = candidate.get("file")?.as_str()?;
                let (path, source) = sources
                    .iter()
                    .find(|(path, _)| layout.overlay_keys(path).iter().any(|key| key == file))?;
                if candidate.get("file_fingerprint")?.as_str()? != fingerprint(source) {
                    return None;
                }
                let edits = candidate.get("edits")?.as_array()?;
                if edits.len() != 1 {
                    return None;
                }
                let edit = &edits[0];
                let eof = json!({"byte":source.len(),"line":source.bytes().filter(|b|*b==b'\n').count()+1,"column":source.len()-source.rfind('\n').map_or(0,|p|p+1)+1});
                if edit.get("file")?.as_str()? != file
                    || edit.get("file_fingerprint") != candidate.get("file_fingerprint")
                    || edit.pointer("/range/start/byte")?.as_u64()? != source.len() as u64
                    || edit.pointer("/range/end/byte")?.as_u64()? != source.len() as u64
                    || edit.get("replacement") != candidate.get("source")
                    || edit.pointer("/range/start") != Some(&eof)
                    || edit.pointer("/range/end") != Some(&eof)
                {
                    return None;
                }
                let range = compiler_range_to_lsp_range(source, edit.get("range")?)?;
                Some((path.clone(), range, edit.get("replacement")?.clone()))
            })();
            if let Some((path, range, replacement)) = converted {
                candidate["uri"] = json!(path_to_uri(&path));
                candidate["document_version"] = json!(versions.get(&path));
                candidate["workspace_edit"] = json!({"documentChanges":[{"textDocument":{"uri":path_to_uri(&path),"version":versions.get(&path)},"edits":[{"range":range.to_json(),"newText":replacement}]}]});
            } else {
                candidate["editor_edit_rejected"] =
                    json!("candidate does not match the captured source and EOF insertion receipt");
            }
        }
    }
    let diagnostics = result.get("functions").and_then(Value::as_array).into_iter().flatten()
        .flat_map(|f| f.get("cases").and_then(Value::as_array).into_iter().flatten().filter_map(move |case| {
            let diagnostic = case.get("diagnostic")?;
            let file = diagnostic.get("file")?.as_str()?;
            let normalized = normalize_path(std::path::Path::new(file));
            let (path, source) = sources.iter().find(|(path,_)| **path == normalized)?;
            let line = diagnostic.get("line")?.as_u64()?.saturating_sub(1) as usize;
            let text = source.lines().nth(line).unwrap_or("");
            Some(json!({"uri":path_to_uri(path),"version":versions.get(path),"function":f.get("function"),"status":case.get("status"),"inputs":case.get("inputs"),"fault":case.get("fault"),"range":{"start":{"line":line,"character":0},"end":{"line":line,"character":text.encode_utf16().count()}},"message":diagnostic.get("message"),"severity":if case["status"]=="precondition_rejected"{3}else{1}}))
        })).collect::<Vec<_>>();
    result["sample_diagnostics"] = json!(diagnostics);
    result["coordinate_encoding"] = json!("utf-16");
}
