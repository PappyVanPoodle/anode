use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const MAX_LSP_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Read a single `Content-Length` framed JSON-RPC message from `reader`.
/// Returns `Ok(None)` on clean EOF before headers.
pub fn read_lsp_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF while reading LSP headers",
            ));
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            }
            // Ignore leading blank lines between frames if any.
            continue;
        }

        if let Some((name, val)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let parsed = val.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
                })?;
                if parsed > MAX_LSP_MESSAGE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "LSP message exceeds 32 MiB limit",
                    ));
                }
                content_length = Some(parsed);
            }
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;

    let parsed = serde_json::from_slice::<Value>(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    Ok(Some(parsed))
}

/// Write a single `Content-Length` framed JSON-RPC message to `writer` and flush.
pub fn write_lsp_message<W: Write>(writer: &mut W, message: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", payload.len())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn jsonrpc_response(id: &Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub fn jsonrpc_error(
    id: &Value,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Value {
    let mut err = json!({
        "code": code,
        "message": message.into(),
    });
    if let Some(d) = data {
        err["data"] = d;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": err,
    })
}

pub fn jsonrpc_notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_lsp_framing_roundtrip() {
        let mut buf = Vec::new();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        write_lsp_message(&mut buf, &msg).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_lsp_message(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(read_lsp_message(&mut cursor).unwrap().is_none());
    }
}
