// Builds a 200,000-line "label|amount" document (with blank, comment and
// malformed lines), then counts rows, the amount total and malformed lines.
struct Stats { rows: i64, total: i64, errors: i64 }
fn document(count: i64) -> String {
    let mut lines: Vec<String> = Vec::new();
    for i in 0..count {
        if i % 17 == 0 { lines.push("# comment".to_string()); }
        else if i % 13 == 0 { lines.push(String::new()); }
        else if i % 10 == 0 { lines.push("bad line".to_string()); }
        else { lines.push(format!("item{}|{}", i, i * 7 % 1000)); }
    }
    lines.join("\n")
}
fn scan(src: &str) -> Stats {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut s = Stats { rows: 0, total: 0, errors: 0 };
    let mut start = 0;
    for i in 0..=n {
        if i == n || bytes[i] == b'\n' {
            if i > start && bytes[start] != b'#' {
                match bytes[start..i].iter().position(|&b| b == b'|') {
                    Some(k) if k > 0 => match src[start + k + 1..i].parse::<i64>() {
                        Ok(v) => { s.rows += 1; s.total += v; }
                        Err(_) => s.errors += 1,
                    },
                    _ => s.errors += 1,
                }
            }
            start = i + 1;
        }
    }
    s
}
fn main() {
    let src = document(200000);
    let stats = scan(&src);
    println!("{}\n{}\n{}", stats.rows, stats.total, stats.errors);
}
