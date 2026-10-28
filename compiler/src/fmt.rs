//! Canonical, lossless formatting. Only trivia is rewritten: literal spellings,
//! comment contents and all language tokens survive in the same order. Comment
//! line endings and other trivia use LF; string literals remain byte-for-byte.
//! Parsing checks syntax, not executable validity; a library needs no `main`.
use crate::{project::Project, syntax::parse_module, Error, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct FormattedFile {
    pub path: PathBuf,
    pub source: String,
    pub changed: bool,
}

/// Format a standalone module without performing name, ownership or effect checks.
/// Malformed syntax is rejected and the caller's source is never modified.
pub fn format_source(source: &str) -> Result<String> {
    parse_module(source)?;
    let tokens = tokenize(source);
    let formatted = render(&tokens);
    parse_module(&formatted)?;
    let after = tokenize(&formatted);
    if tokens
        .iter()
        .map(|t| t.text)
        .ne(after.iter().map(|t| t.text))
    {
        return Err(Error::new(0, "formatter changed the lossless token stream"));
    }
    Ok(formatted)
}

/// Prepare root-package sources before the caller decides to write.
/// Dependency packages are inputs, not implicit formatting write targets.
/// This function performs no file writes and does not format the manifest.
pub fn format_project(project: &Project) -> Result<Vec<FormattedFile>> {
    project
        .files
        .iter()
        .filter(|file| file.package.is_empty())
        .map(|file| {
            let source = format_source(&file.source).map_err(|e| e.in_file(&file.path))?;
            Ok(FormattedFile {
                path: file.path.clone(),
                changed: source != file.source,
                source,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Token<'a> {
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub comment: bool,
}

/// Lossless lexemes for valid syntax; on invalid input this still makes forward
/// progress, so diagnostics can find independent declaration boundaries.
pub(crate) fn tokenize(source: &str) -> Vec<Token<'_>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut line = 1;
    while pos < bytes.len() {
        let ch = source[pos..].chars().next().unwrap();
        if ch.is_whitespace() {
            line += usize::from(ch == '\n');
            pos += ch.len_utf8();
            continue;
        }
        let start = pos;
        let start_line = line;
        let comment = bytes[pos..].starts_with(b"//");
        if comment {
            // The language ends comments at LF, not a bare CR. A CR inside a
            // comment must never expose the following text as executable code.
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
        } else if ch == '"' {
            pos += 1;
            while pos < bytes.len() {
                let c = bytes[pos];
                pos += 1;
                line += usize::from(c == b'\n');
                if c == b'\\' && pos < bytes.len() {
                    line += usize::from(bytes[pos] == b'\n');
                    pos += 1;
                } else if c == b'"' {
                    break;
                }
            }
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            pos += 1;
            while pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
        } else if ch.is_ascii_digit() {
            pos += 1;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos + 1 < bytes.len() && bytes[pos] == b'.' && bytes[pos + 1].is_ascii_digit() {
                pos += 1;
                while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                    pos += 1;
                }
            }
            if pos < bytes.len() && matches!(bytes[pos], b'e' | b'E') {
                pos += 1;
                if pos < bytes.len() && matches!(bytes[pos], b'+' | b'-') {
                    pos += 1;
                }
                while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                    pos += 1;
                }
            }
        } else {
            pos += ch.len_utf8();
            if pos < bytes.len()
                && [
                    "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "::", "??", "..",
                ]
                .iter()
                .any(|pair| source[start..].starts_with(pair))
            {
                pos += 1;
            }
        }
        let text_end = if comment && bytes.get(pos.wrapping_sub(1)) == Some(&b'\r') {
            pos - 1
        } else {
            pos
        };
        out.push(Token {
            text: &source[start..text_end],
            start,
            end: pos,
            line: start_line,
            comment,
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    Block,
    Inline,
    Paren { annotation: bool },
    Bracket,
}

#[derive(Default)]
struct Writer {
    text: String,
    indent: usize,
}
impl Writer {
    fn space(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with([' ', '\n']) {
            self.text.push(' ');
        }
    }
    fn trim_space(&mut self) {
        while self.text.ends_with(' ') {
            self.text.pop();
        }
    }
    fn newline(&mut self) {
        self.trim_space();
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }
    fn token(&mut self, text: &str) {
        if self.text.is_empty() || self.text.ends_with('\n') {
            self.text.push_str(&"    ".repeat(self.indent));
        }
        self.text.push_str(text);
    }
}

fn render(tokens: &[Token<'_>]) -> String {
    let mut out = Writer::default();
    let mut stack = Vec::new();
    let mut previous = "";
    let mut previous_line = 0;
    let mut annotation = false;
    let mut declaration = false;
    let mut prefix = false;
    for (index, token) in tokens.iter().enumerate() {
        let text = token.text;
        if token.comment {
            if token.line == previous_line {
                // A statement normally ends a line; retain a trailing comment on it.
                if out.text.ends_with('\n') {
                    out.text.pop();
                }
                out.space();
            } else {
                out.newline();
            }
            out.token(text);
            out.newline();
            continue;
        }
        let next = tokens[index + 1..]
            .iter()
            .find(|t| !t.comment)
            .map(|t| t.text);
        let next_is_trailing_comment = tokens
            .get(index + 1)
            .is_some_and(|t| t.comment && t.line == token.line);
        match text {
            "@" => {
                out.newline();
                out.token(text);
                annotation = true;
            }
            "(" => {
                if [
                    "if", "while", "match", "return", "print", "assert", "requires", "ensures",
                    "borrow", "mut", "try",
                ]
                .contains(&previous)
                {
                    out.space();
                } else {
                    out.trim_space();
                }
                out.token(text);
                stack.push(Delimiter::Paren { annotation });
                annotation = false;
            }
            ")" => {
                out.trim_space();
                out.token(text);
                if let Some(Delimiter::Paren { annotation: true }) = stack.pop() {
                    if !next_is_trailing_comment {
                        out.newline();
                    }
                }
            }
            "[" => {
                if !["", "(", "[", ".", "::", "@"].contains(&previous) && !prefix {
                    out.space();
                }
                out.token(text);
                stack.push(Delimiter::Bracket);
            }
            "]" => {
                out.trim_space();
                out.token(text);
                stack.pop();
            }
            "{" => {
                let after = tokens[index + 1..]
                    .iter()
                    .filter(|t| !t.comment)
                    .take(2)
                    .collect::<Vec<_>>();
                let record = after.len() == 2 && after[1].text == ":";
                let inline = previous == "." || (record && !declaration);
                if previous != "." {
                    out.space();
                }
                out.token(text);
                if inline {
                    stack.push(Delimiter::Inline);
                    out.space();
                } else {
                    stack.push(Delimiter::Block);
                    out.indent += 1;
                    if next != Some("}") && !next_is_trailing_comment {
                        out.newline();
                    }
                }
                declaration = false;
            }
            "}" => {
                if stack.pop() == Some(Delimiter::Inline) {
                    out.trim_space();
                    out.space();
                    out.token(text);
                } else {
                    out.indent = out.indent.saturating_sub(1);
                    if previous != "{" {
                        out.newline();
                    }
                    out.token(text);
                    if next == Some("else") {
                        out.space();
                    } else if !matches!(next, Some("," | ";" | ")")) && !next_is_trailing_comment {
                        out.newline();
                    }
                }
            }
            ";" => {
                out.trim_space();
                out.token(text);
                if matches!(
                    stack.last(),
                    Some(Delimiter::Paren { .. } | Delimiter::Bracket)
                ) {
                    out.space();
                } else if !next_is_trailing_comment {
                    out.newline();
                }
            }
            "," => {
                out.trim_space();
                out.token(text);
                if stack.last() == Some(&Delimiter::Block) && !next_is_trailing_comment {
                    out.newline();
                } else {
                    out.space();
                }
            }
            ":" => {
                out.trim_space();
                out.token(text);
                out.space();
            }
            "." | "::" | ".." => {
                out.trim_space();
                out.token(text);
            }
            "!" | "&" | "-" if text != "-" || expression_start(previous) => {
                if !["", "(", "[", ".", "::", ".."].contains(&previous) && !prefix {
                    out.space();
                }
                out.token(text);
                prefix = true;
                previous = text;
                previous_line = token.line;
                continue;
            }
            "+" | "-" | "*" | "/" | "%" | "=" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&"
            | "||" | "->" | "=>" => {
                out.space();
                out.token(text);
                out.space();
            }
            _ => {
                if !["", "(", "[", ".", "::", "..", "@"].contains(&previous) && !prefix {
                    out.space();
                }
                out.token(text);
                if ["struct", "enum"].contains(&text) {
                    declaration = true;
                }
            }
        }
        prefix = false;
        previous = text;
        previous_line = token.line;
    }
    out.newline();
    out.text
}

fn expression_start(previous: &str) -> bool {
    [
        "", "=", "(", "[", "{", ",", ":", "return", "print", "assert", "requires", "ensures", "in",
        "try", "..", "+", "-", "*", "/", "%", "==", "!=", "<", ">", "<=", ">=", "&&", "||", "!",
    ]
    .contains(&previous)
}

#[cfg(test)]
mod v023_tests {
    use super::*;

    #[test]
    fn columnar_pair_borrows_externs_and_swap_format_losslessly() {
        let source = "// column storage\npub struct Item{x:int,label:text}\npub soa vector Items(Item);\nextern fn native(value:i32)->i32;\n@effect(no_alloc,no_io)\nfn exchange(items:&mut Items){borrow mut (left,right)=at_pair(items,0,1){swap(left.x,right.x);}}\nfn main(){}";
        let output = format_source(source).unwrap();
        assert!(output.contains("pub soa vector Items(Item);"));
        assert!(output.contains("extern fn native(value: i32) -> i32;"));
        assert!(output.contains("borrow mut (left, right) = at_pair(items, 0, 1)"));
        assert!(output.contains("swap(left.x, right.x);"));
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source).iter().map(|t| t.text).collect::<Vec<_>>(),
            tokenize(&output).iter().map(|t| t.text).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod v024_tests {
    use super::*;

    #[test]
    fn arenas_ids_pair_borrows_and_bounded_effects_format_without_token_changes() {
        let source = "// scene ✓\npub struct Entity{parent:id(Entities),x:int}\npub arena Entities(Entity);\npub soa vector Motion(Entity);\n@effect(no_alloc,no_io,bounded)fn update(world:&mut Entities,a:id(Entities),b:id(Entities)){borrow mut (left,right)=get_pair(world,a,b){swap(left.x,right.x);}for i in 0..3{let copy=i;}}fn main(){}";
        let output = format_source(source).unwrap();
        assert!(output.contains("pub arena Entities(Entity);"));
        assert!(output.contains("parent: id(Entities)"));
        assert!(output.contains("@effect(no_alloc, no_io, bounded)"));
        assert!(output.contains("borrow mut (left, right) = get_pair(world, a, b)"));
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source).iter().map(|t| t.text).collect::<Vec<_>>(),
            tokenize(&output).iter().map(|t| t.text).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod v025_tests {
    use super::*;

    #[test]
    fn channels_and_try_preserve_tokens_comments_and_unary_boundaries() {
        let source = "// pipeline ✓\npub channel Jobs(int);\nenum Outcome{Ok(int),Err(text)}\nfn ok()->Outcome{return Outcome::Ok(-2);}\nfn relay()->Outcome{let value=try (ok());return Outcome::Ok(value);}\nfn main(){let queue=Jobs(2);let mut out=0;send(queue,1);try_recv(queue,&mut out);close(queue);}";
        let output = format_source(source).unwrap();
        assert!(output.contains("pub channel Jobs(int);"));
        assert!(output.contains("try (ok())"));
        assert!(output.contains("try_recv(queue, &mut out)"));
        assert!(output.contains("// pipeline ✓"));
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>(),
            tokenize(&output)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod v026_tests {
    use super::*;

    #[test]
    fn nominal_vector_windows_preserve_borrowed_type_tokens_and_comments() {
        let source = "// window ✓\r\nvector Values(int);\r\nfn read(values:&[Values])->int{let mut total=0;for value in &values{total=total+value;}return total;}\r\nfn write(values:&mut [Values]){replace(values,0,5);}\r\nfn main(){let mut values=Values();push(values,4);borrow mut outer=region(values,0,1){borrow mut inner=region(outer,0,1){write(&mut inner);}}borrow view=region(values,0,1){print read(&view);}}";
        let output = format_source(source).unwrap();
        assert!(output.contains("&[Values]"));
        assert!(output.contains("&mut [Values]"));
        assert!(output.contains("borrow mut inner = region(outer, 0, 1)"));
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>(),
            tokenize(&output)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn process_try_and_result_names_format_without_inserting_or_dropping_tokens() {
        let source = "fn capture(program:&str,args:&texts)->ProcessRun{let output=try process_run(program,&args);return ProcessRun::Output(output);}fn main(){let args=texts();let captured=process_run(\"example.exe\",&args);match captured{ProcessRun::Output(output)=>{print output.status;print output.stdout;print output.stderr;},ProcessRun::Error(message)=>{print message;}}}";
        let output = format_source(source).unwrap();
        assert!(output.contains("try process_run(program, &args)"));
        assert!(output.contains("ProcessRun::Output(output)"));
        assert!(output.contains("ProcessRun::Error(message)"));
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>(),
            tokenize(&output)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod v028_tests {
    use super::*;

    #[test]
    fn modular_and_big_endian_calls_format_losslessly_and_remain_checked() {
        let source="// exact-width ✓\nfn hash(v:u64)->u64{return bit_xor(rotl(v,-1),u64(1));}fn main(){let data=[u8(1);8];let value=wire_read_u32_be(data,0);print value;print hash(u64(7));}";
        let output = format_source(source).unwrap();
        assert_eq!(output, format_source(&output).unwrap());
        assert_eq!(
            tokenize(source)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>(),
            tokenize(&output)
                .iter()
                .map(|token| token.text)
                .collect::<Vec<_>>()
        );
        crate::project::Project::from_source(source)
            .unwrap()
            .checked(false)
            .unwrap();
        crate::project::Project::from_source(&output)
            .unwrap()
            .checked(false)
            .unwrap();
    }
}
