//! Convert compiler formatting output into a UTF-16 whole-document edit.
use crate::unicode::{split_lines, Position, Range};
use serde_json::{json, Value};

fn fingerprint(source: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}:len={}", source.len())
}

pub fn formatting_edits(result: &Value, file: &str, source: &str) -> Result<Value, String> {
    let files = result
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "compiler format result is missing files".to_owned())?;
    if files.len() != 1 || files[0].get("file").and_then(Value::as_str) != Some(file) {
        return Err("compiler format result does not match the requested file".into());
    }
    let entry = &files[0];
    if entry.get("file_fingerprint").and_then(Value::as_str) != Some(fingerprint(source).as_str()) {
        return Err("compiler formatting snapshot is stale".into());
    }
    let text = entry
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "compiler format result is missing text".to_owned())?;
    if entry.get("formatted_fingerprint").and_then(Value::as_str)
        != Some(fingerprint(text).as_str())
    {
        return Err("compiler formatting output fingerprint is invalid".into());
    }
    if source == text {
        return Ok(json!([]));
    }
    let lines = split_lines(source);
    let end = Position::new(
        (lines.len() - 1) as u32,
        lines.last().unwrap().encode_utf16().count() as u32,
    );
    Ok(json!([{"range":Range::new(Position::new(0, 0), end).to_json(),"newText":text}]))
}
