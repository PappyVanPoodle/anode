# AGENTS.md — Anode 1.3.0 Language, Stdlib & Parallel Multi-Agent Guide

> **Self-Contained Agent Playbook:** This file contains everything an AI coding agent needs to write, typecheck, test, package, and parallelize `.an` programs using **Anode 1.3.0**—with zero prior context of the compiler's internal development.

---

## 1. Toolchain & Essential Commands

The SDK's `bin/` folder holds `anode.exe` (compiler, runner, test engine, formatter), `zn.exe` (the same compiler under its legacy name) and `anode-lsp.exe` (language server). The SDK's `install.ps1` puts `bin/` on your user `PATH` and sets `ANODE_HOME` to the SDK folder. Run these from the project folder (the one containing `Anode.toml`):

```powershell
anode check . --json        # typecheck, borrow-check, view-check and effect-check in milliseconds (structured JSON)
anode test . --jobs 4       # unit tests, 4 in parallel
anode run .                 # build with the native backend and run; program arguments go after `--`
anode build . -o app.exe    # native executable (the default backend); add --release for optimized C/MSVC
anode fmt .                 # canonical formatting; `anode fmt . --check` only reports
anode verify . --json       # bounded contract verification (requires/ensures sampling) in RAM
anode doctor                # toolchain report (MSVC location, backends)
anode new my_app            # new project: Anode.toml, src/, .gitignore, AGENTS.md, CLAUDE.md
```

Source limits: 16 MiB per file and 256 MiB per project.

**Standard library.** The 8 pure-`.an` packages (`cli`, `encoding`, `csv_jsonl`, `markdown`, `glob_regex`, `json`, `docgen`, `bench`) live in `$ANODE_HOME/packages/` (`packages/` in the SDK bundle). Import them as path dependencies in `Anode.toml`; the path is relative to that `Anode.toml` (absolute paths also work). There is no `packages = [...]` key:
```toml
[dependencies]
json = { path = "../packages/json" }
cli = { path = "../packages/cli" }
```
Then call them by alias, e.g. `json::parse_json(&view)`. §8 summarizes each package.

---

## 2. Critical Syntax Differences vs. Rust (Do NOT Write Rust-isms!)

Anode (`.an`) looks similar to Rust, **but has zero lifetime annotations (`'a`), zero generic trait bounds, and nominal collection declarations**. Follow these exact rules:

| Concept | ❌ NEVER Write (Rust) | ✅ ALWAYS Write (Anode `.an`) |
| :--- | :--- | :--- |
| **Primitive types** | `i64`, `usize`, `isize`, `String`, `&str` | `int` (signed 64-bit), `i8`, `i16`, `i32`, `u8`, `u16`, `u32`, `u64`, `f32`, `f64`, `bool`, `text` (owned), `str` (borrowed view: `&str`) |
| **Printing** | `println!("x = {}", x);` | `print concat("x = ", int_text(x));` (`print` is a statement keyword, not a macro) |
| **Assertions** | `assert_eq!(foo(x), b);` | `let v = foo(x); assert v == b;` (`assert` is a pure contract statement—always bind user function calls to a `let` variable before `assert`). Text literals allocate, so `assert text_eq(s, "lit");` is rejected: write `let ok = text_eq(s, "lit"); assert ok;` |
| **Contracts (`requires` / `ensures`)** | No trailing semicolon | Always end each clause with `;` before `{`: `requires x > 0; ensures result >= 0; { ... }` |
| **String comparison** | `a == b` (on `text` or `str`) | `text_eq(a, b)` (always use `text_eq` to compare two `text`/`str` values) |
| **String concatenation** | `format!("{a}{b}")` or `a + b` | `concat(a, b)` |
| **Passing `&str` / `&[u8]`** | `f("lit")` or `f(&owned_text)` for `fn f(s: &str)` | `borrow v = region(t, 0, len(t)) { f(&v); }` — neither a literal nor `&text` converts to `&str`, and `&buf` on a `[u8; N]` does not convert to `&[u8]`. See §2.1. |
| **Reserved names** | `let result = parse(x);` | `result` is reserved (it names the return value in `ensures`): use `parsed`, `out`, etc. |
| **Return statements** | Implicit tail expression `x + 1` | Explicit `return x + 1;` (every value-returning path MUST use `return`) |
| **Ownership & Moves** | Implicit copy of strings/structs | Passing an owned `text`, `struct`, `enum`, `vector`, `map`, or `arena` by value **moves** it. Pass by borrow (`&x`) or call `clone(x)` if needed again. |
| **Collections** | `Vec<Item>`, `HashMap<K, V>` | Declare nominal types at module top level: `pub vector Items(Item);`, `pub soa vector Bodies(Body);`, `pub map Table(text, int);`, `pub arena Graph(Node);` |
| **Dynamic int/text arrays** | `vec![1, 2, 3]` | Built-in `ints` (`let mut v = []; push(v, 1); at(v, 0)` — the empty literal `[]` is an `ints`; there is no `ints()` constructor) and `texts` (`let mut t = texts(); push(t, "hi"); text_at(t, 0)`) |
| **Fixed buffers & slices** | `[0u8; 32]` | Value literal: `[u8(0); 32]`; type signature: `[u8; 32]` (owned) or `&[u8]` / `&mut [u8]` (borrowed slice view via `borrow s = region(buf, start, end) { ... }`) |
| **Type conversions** | `x as u32`, `x as usize` | Checked constructor syntax: `u32(x)`, `u8(x)`, `int(x)`, `f64(x)` |
| **Unit tests** | `#[test] fn my_test() {}` | `test my_test_name() { ... }` (identifier name, private, no parameters, no return type) |
| **`Anode.toml` manifest** | `edition = "2024"` | Only `[package] name/version`, `[build] entry/cache_mb/exports`, `[dependencies]`, and `[capabilities]` (never add `edition`) |

### 2.1 Calling Functions That Take `&str` or `&[u8]`
`str` and `[u8]` views exist only inside a scoped `borrow ... = region(...)` block (or as a parameter you already hold). Create the view at the call site and copy results out through a `let mut` declared before the block:
```anode
@effect(no_alloc, no_io, bounded)
pub fn count_eq(s: &str) -> int {
    let mut n = 0;
    for i in 0..len(s) {
        if byte_at(s, i) == 61 { n = n + 1; } // 61 = '='; no text literal under no_alloc
    }
    return n;
}

@effect(no_alloc, no_io, bounded)
pub fn sum_bytes(data: &[u8]) -> int {
    let mut total = 0;
    for i in 0..len(data) { total = total + int(at(data, i)); }
    return total;
}

pub fn twice(s: &str) -> int {
    return count_eq(&s) * 2; // a `&str` parameter is re-borrowed with an explicit `&`
}

test views_at_call_sites() {
    let line = "a=b=c";                      // bind the literal first
    let mut n = 0;
    borrow v = region(line, 0, len(line)) {  // text -> &str view
        n = twice(&v);
    }
    assert n == 4;

    let mut buf = [u8(0); 4];
    set(buf, 0, u8(7));
    let mut total = 0;
    borrow b = region(buf, 0, len(buf)) {    // [u8; N] -> &[u8] view
        total = sum_bytes(&b);
    }
    assert total == 7;
}
```
- Builtins (`len`, `byte_at`, `slice`, `text_eq`, `try_write_bytes`, ...) accept `text`, `str`, `[u8; N]` and slices directly; only **user functions** with `&str` / `&[u8]` parameters need the `region` view.
- Calling a user function always needs an explicit `&` / `&mut`, even when the argument is already a borrowed parameter: `helper(&s)`, `update(&mut p)`.

### 2.2 Critical Rules Discovered in Practice
These are the mistakes the compiler rejected most often while the standard library was written. Check them before every `anode check`:
1. **`&str` and `&[u8]` arguments need a `region` view.** A string literal, an `&text` and an `&[u8; N]` never convert to `&str` / `&[u8]`: write `borrow v = region(t, 0, len(t)) { f(&v); }` (§2.1).
2. **Text literals allocate.** They are rejected inside `@effect(no_alloc)` functions and directly inside `assert`: compare bytes (`byte_at(s, i) == 61`) or bind first (`let ok = text_eq(s, "lit"); assert ok;`).
3. **`text_join` takes the collection itself:** `text_join(parts, ",")`, never `text_join(&parts, ",")`.
4. **`result` is reserved** (it names the return value in `ensures`); pick another local name.
5. **A library needs an entry point for `anode check`.** End every `lib.an` with a private `fn main() {}` (§4.2); it does not clash with the root's `main`.
6. **The empty `ints` value is `[]`.** There is no `ints()` constructor (`texts()` does exist).

---

## 3. Complete Collection & Memory Operations

### 3.1 Built-in `ints` and `texts`
```anode
let mut nums = [];   // empty `ints` literal; `ints()` does not exist (`[1, 2, 3]` also works)
push(nums, 10);
set(nums, 0, 20);
let x = at(nums, 0); // 20
let n = len(nums);   // 1

let parts = split("a,b,c", ","); // returns `texts`
let first = text_at(parts, 0);   // returns owned `text` ("a")
let joined = text_join(parts, "-");  // "a-b-c" (pass the collection itself, not &parts)
```

### 3.2 Nominal `vector` & Columnar `soa vector`
```anode
pub struct Item { id: int, label: text }
pub vector Items(Item);
pub soa vector FastStats(StatRecord); // Record fields must be finite owned types

fn use_vectors() {
    let mut items = Items();
    push(items, Item { id: 1, label: "alpha" });
    push(items, Item { id: 2, label: "beta" });

    // 1. Read/write element in place via scoped borrow (freezes `items` during block):
    borrow row = at(items, 0) {
        print row.label;
    }
    borrow mut row = at(items, 1) {
        row.id = 99;
    }

    // 2. Borrow two distinct mutable elements simultaneously:
    borrow mut (left, right) = at_pair(items, 0, 1) {
        let tmp = left.id;
        left.id = right.id;
        right.id = tmp;
    }

    // 3. Copy an element out as a new owned value, replace, swap, or pop:
    let owned_copy = clone_at(items, 0);
    swap(&mut items, 0, 1);
    let last = pop(items);

    // 4. Contiguous zero-copy window (`region(vec, start, count)` — works on `vector`, not `soa vector`):
    borrow window = region(items, 0, len(items)) {
        for elem in &window {
            print elem.label;
        }
    }
}
```
> **Note on `region(...)`:**
> - On a nominal `vector Name(T);`, `borrow sub = region(v, start, count) { ... }` uses **`(start, count)`** and has parameter type `&[Name]` or `&mut [Name]`.
> - On `text`/`str`, `ints`, or fixed `[u8; N]` arrays, `borrow sub = region(x, start, end) { ... }` uses half-open **`(start, end)`** bounds and has parameter type `&str` or `&[u8]` / `&mut [u8]`.

### 3.3 Nominal `map Name(KeyType, ValueType);`
Keys can be `int` (or fixed-width integers), `text`, or `id(ArenaName)`.
```anode
pub map Scores(text, int);

fn use_map() -> int {
    let mut m = Scores();
    insert(m, "alice", 100); // consumes key and value
    if contains_key(m, "alice") {
        // Scoped zero-copy borrow of value:
        borrow mut val = get(m, "alice") {
            val = val + 25;
        }
        // Or copy value out:
        let copied = clone_get(m, "alice");
        remove(m, "alice");
        return copied;
    }
    return 0;
}
```
- **Listing keys:** `keys(m)` returns the keys of a `text`-keyed map as `texts`, or of an `int`-keyed map as `ints`. Text keys are copied, the order is unspecified, and the map stays usable. Other key types are rejected. It allocates, so it is not allowed under `@effect(no_alloc)` or in contracts.
  ```anode
  let names = keys(m);
  for i in 0..len(names) {
      let name = text_at(names, i);
      let score = clone_get(m, name);
  }
  ```

### 3.4 Generational `arena Name(NodeType);` and `id(Name)`
Use `arena` instead of recursive pointers/lifetimes for ASTs, JSON trees, DOMs, and graphs:
```anode
pub struct AstNode { kind: int, parent: int, text_val: text }
pub arena AstArena(AstNode);

fn use_arena() -> int {
    let mut nodes = AstArena();
    let root_id = alloc(nodes, AstNode { kind: 1, parent: -1, text_val: "root" }); // type: `id(AstArena)`
    assert contains_id(nodes, root_id);

    borrow mut n = get(nodes, root_id) {
        n.kind = 2;
    }
    let snapshot = clone_get(nodes, root_id);
    free(nodes, root_id);
    return snapshot.kind;
}
```

---

## 4. Multi-File Modules & Multi-Package `[dependencies]`

### 4.1 Modules Inside a Package
- An executable package defaults to `src/main.an`; a library package defaults to `src/lib.an`.
- Declare child modules with `mod helper;` (loads `src/helper.an`) and call public items as `helper::my_fn(...)` or `crate::helper::my_fn(...)`.

### 4.2 Local Reusable Packages (`packages/<name>`)
Root `Anode.toml`:
```toml
[package]
name = "my-app"
version = "0.1.0"

[build]
entry = "src/main.an"
cache_mb = 1

[dependencies]
json = { path = "packages/json" }
cli = { path = "packages/cli" }

[capabilities]
file_read = true
file_write = true
process_args = true
process = false
threads = true
ffi = false
net = true
```
Library `packages/json/Anode.toml`:
```toml
[package]
name = "json"
version = "0.1.0"

[build]
entry = "src/lib.an"

[capabilities]
file_read = false
file_write = false
process_args = false
process = false
threads = false
ffi = false
net = false
```
- **Library entry point:** `anode check <package_dir>` requires a `fn main()` even when `entry = "src/lib.an"` (only `anode build --lib` skips the check). End each library's `lib.an` with a private empty entry point; it does not clash with the root's `main` when the package is used as a dependency:
  ```anode
  // Library packages carry an empty entry point so `anode check <package>` accepts them.
  fn main() {}
  ```
- Inside `src/main.an`, call `pub` functions and types from `packages/json/src/lib.an` directly via the dependency alias (`json::parse(...)`, `json::JsonTree`, or `json::submodule::helper(...)`) without writing `mod json;`.
- **Capability inheritance rule:** A dependency's effective capabilities are the intersection of its own `[capabilities]` and its parent package's `[capabilities]`. Pure library packages should leave all capabilities `false`.

---

## 5. Field-View Contracts, `@effect` Verification & `try`

### 5.1 Field Views (`&mut Struct.{mut a, b}`)
Lock which fields a function may read or mutate:
```anode
pub fn update_cadence(s: &mut DocState.{mut cadence, tokens, sentences}) -> int {
    s.cadence = (s.tokens * 1000) / s.sentences;
    return s.cadence;
}
```

### 5.2 Transitive `@effect(no_alloc, no_io, bounded)`
- `no_alloc`: Forbids heap allocations (`concat`, `slice`, `sha256_text`, `push`, `insert`, `alloc`, `spawn`). **Text literals (`"abc"`) also count as allocations**, so they are rejected inside `no_alloc` functions: compare byte values instead (`byte_at(s, i) == 61` for `'='`). Allowed in `no_alloc`: scalar/bitwise math, `fnv1a_64`, `byte_at`, `len`, `at`, `set`, `region`, `read_u*_be/le`, `try_write_u*_be/le`, fixed arrays `[T; N]`, and in-place `borrow`/`borrow mut` accesses.
- `no_io`: Forbids `print`, file I/O, `env_var`, `process_run`, `net_request`, `net_serve_once`, and channel operations.
- `bounded`: Requires provably finite control flow (forbids `while` loops, recursion, `process_run`, `net_request`, `net_serve_once`; `for i in a..b` and `for x in &vec` are `bounded`!).

### 5.3 Two-Variant Enum Error Propagation (`try`)
Any 2-variant enum where the first variant is the success payload and the second variant is the error payload (with matching error payload type) supports `try`:
```anode
pub enum ParseOutcome { Ok(int), Err(text) }

fn parse_pair(a: &text, b: &text) -> ParseOutcome {
    let x = try parse_one(a);
    let y = try parse_one(b);
    return ParseOutcome::Ok(x + y);
}
```

---

## 6. Complete Built-in Standard Library (Anode 1.3.0)

### 6.1 Text & UTF-8 Primitives
- `len(s) -> int` (byte length), `byte_at(s, i) -> int` (`0..=255`), `char_count(s) -> int`, `is_char_boundary(s, byte_idx) -> bool`, `codepoint_at(s, byte_idx) -> int`, `next_char_boundary(s, byte_idx) -> int`, `codepoint_text(cp) -> text`.
- `concat(a, b) -> text`, `slice(s, start_byte, end_byte) -> text`, `text_eq(a, b) -> bool`, `contains(s, needle) -> bool`, `text_find(s, needle) -> int` (byte offset or `-1`), `text_starts_with(s, prefix) -> bool`, `text_ends_with(s, suffix) -> bool`, `text_trim(s) -> text`, `text_replace(s, from, to) -> text`, `split(s, delim) -> texts`, `text_lines(s) -> texts`, `text_join(parts, sep) -> text` (pass the `texts` value itself; `&parts` is rejected).
- Conversions: `int_text(n) -> text`, `is_int(s) -> bool`, `parse_int(s) -> int`, `float_text(f) -> text`, `is_f64(s) -> bool`, `parse_f64(s) -> f64`, `to_text(x) -> text`.

### 6.2 Math, Bitwise & Modular Intrinsics (`no_alloc, no_io, bounded`)
- Checked arithmetic: `+`, `-`, `*`, `/`, `%`, `abs(x)`, `sqrt(f64_val)`, `is_finite(f)`, `is_nan(f)`, `saturating_add(a, b)`, `saturating_sub(a, b)`.
- Modular / bitwise over `int`, `i8`, `i16`, `i32`, `u8`, `u16`, `u32`, `u64`:
  - `wrapping_add(a, b)`, `wrapping_sub(a, b)`, `wrapping_mul(a, b)`
  - `bit_and(a, b)`, `bit_or(a, b)`, `bit_xor(a, b)`, `bit_not(x)`
  - `shl(x, shift_int)`, `shr(x, shift_int)`, `rotl(x, shift_int)`, `rotr(x, shift_int)`
  - Tip: `int(bit_and(u64_val, u64(9223372036854775807)))` converts masked `u64 -> int` with zero runtime range check.

### 6.3 Wire Framing & Binary Buffers (`no_alloc, no_io, bounded`)
- Fixed buffer indexing: `at(buf, i)`, `set(buf, i, val)`, `try_set(buf, i, val) -> bool`, `len(buf)`.
- Endian reads (`[u8; N]` or `&[u8]`): `read_u16_le(buf, off)`, `read_u32_le(buf, off)`, `read_u64_le(buf, off)`, `read_u16_be(buf, off)`, `read_u32_be(buf, off)`, `read_u64_be(buf, off)`.
- Endian checked writes (`[u8; N]` or `&mut [u8]`, returns `false` and leaves buffer unchanged if out of bounds):
  - `try_write_u16_le(buf, off, val)`, `try_write_u32_le(buf, off, val)`, `try_write_u64_le(buf, off, val)`
  - `try_write_u16_be(buf, off, val)`, `try_write_u32_be(buf, off, val)`, `try_write_u64_be(buf, off, val)`

### 6.4 Hashing & Crypto (`v1.2.0`)
- `fnv1a_64(input) -> u64`: Pure (`no_alloc, no_io, bounded`, usable in `requires`/`ensures`/`assert`) 64-bit FNV-1a over borrowed `text`, `str`, `[u8; N]`, or `&[u8]`.
- `sha256_text(input) -> text`: Zero-dependency FIPS 180-4 SHA-256 returning 64 lowercase hex characters (`no_io, bounded`, allocates `text`).

### 6.5 Filesystem, Binary I/O, Environment & Subprocesses (`v1.3.0`)
- **Text File I/O:**
  - `file_exists(path) -> bool`, `list_dir(path) -> texts` (requires `file_read = true`).
  - `read_text(path) -> text`, `try_read_text(path) -> ReadResult` (`ReadResult::Ok(text)` | `ReadResult::Err(text)`, requires `file_read = true`).
  - `write_text(path, content)`, `try_write_text(path, content) -> WriteResult` (`WriteResult::Ok` | `WriteResult::Err(text)`, requires `file_write = true`).
- **Binary File I/O (`v1.3.0`, `no_alloc, bounded`):**
  - `try_read_bytes(path, dst_buf) -> int`: Reads raw binary file (`0x00..=0xFF` allowed) into mutable `[u8; N]` or `&mut [u8]`. Returns byte count read (`0..=len(dst_buf)`), or `-1` on error/oversized file with `dst_buf` unchanged (requires `file_read = true`).
  - `try_write_bytes(path, src_buf) -> bool`: Writes all bytes of borrowed `[u8; N]` or `&[u8]` to `path` (requires `file_write = true`).
- **Directories & Files (`v1.3.0`, `no_alloc, bounded`, requires `file_write = true`):**
  - `create_dir(path) -> bool` (true if created or already exists as a directory).
  - `remove_file(path) -> bool` (true if deleted).
- **CLI Args, Environment & Time:**
  - `args_count() -> int`, `arg(i) -> text`, `env_var(name) -> text` (returns `""` if unset; requires `process_args = true`).
  - `monotonic_ms() -> int`, `exit_code(status_0_to_255)`.
- **Subprocess Execution (requires `process = true`):**
  - `process_run(program, &args_texts) -> ProcessRun` (`ProcessRun::Output(ProcessOutput { status: int, stdout: text, stderr: text })` | `ProcessRun::Error(text)`).

### 6.6 Loopback Networking Client & Server (`v1.2.0` / `v1.3.0`, requires `net = true`)
Standard prelude types:
```anode
struct NetResponse { status: int, body: text }
enum NetResult { Response(NetResponse), Error(text) }
```
- **Client (`net_request(endpoint, payload) -> NetResult`):**
  - In-memory routes (no socket): `loopback://echo`, `loopback://sha256`, `loopback://head-fnv1a`, `loopback://status/NNN`.
  - Socket routes: `http://127.0.0.1:<port>/<path>` (HTTP/1.0 POST) and `tcp://127.0.0.1:<port>`.
- **Server (`net_serve_once(endpoint, status, reply_body) -> NetResult`, `v1.3.0`):**
  - In-memory route: `loopback://serve/<request_payload>` -> returns `Response(NetResponse { status, body: "<request_payload>" })`.
  - Socket routes: `http://127.0.0.1:<port>/<path>` (binds, accepts 1 HTTP request, replies with `status` + `reply_body`, returns request body in `Response.body`) and `tcp://127.0.0.1:<port>` (`status == 0`).
- **External HTTP(S) (`http_request(url, body[, headers]) -> NetResult`, also requires `net = true`):**
  - Any `http://` or `https://` host, through Windows WinHTTP with the system proxy settings. `net_request` above still reaches only loopback endpoints.
  - `GET` when `body` is empty, `POST` otherwise. A body whose first non-space character is `{` or `[` is sent as `application/json`, anything else as `text/plain; charset=utf-8`.
  - Optional `headers: texts`, one `Name: value` per entry, borrowed. A `Content-Type` entry replaces the automatic one. Names must be tokens, values printable ASCII without line breaks; `Host`, `Content-Length`, `Transfer-Encoding` and `Connection` are rejected; at most 64 headers and 64 KiB. Errors give the header's position, never its value.
    ```anode
    let mut headers = texts();
    push(headers, concat("Authorization: Bearer ", env_var("API_KEY"))); // env_var needs process_args = true
    match http_request(url, body, headers) {
        NetResult::Response(r) => { print r.status; }
        NetResult::Error(e) => { print e; }
    }
    ```
  - The URL is checked before any I/O: ASCII host (punycode for international names, IPv6 in brackets), port 1–65535, printable-ASCII path and query (percent-encode anything else), no `user@host`. `loopback://` URLs are passed to `net_request`.
  - Request and response are limited to 16 MiB each; the response must be UTF-8 without NUL. Timeouts: 10 s resolve, 10 s connect, 30 s send, 30 s receive. Failures are `Error` values starting with `http_request`.

---

## 7. Parallel 4-Worker Same-File Synthesis (`??` Holes + `--campaign`)

In **Anode 1.3.0**, up to 16 parallel workers can fill `??(type, "intent")` holes inside the **same `.an` file** using either **single expressions** (`"kind": "expression"`) or **full multi-statement braced function bodies** (`"kind": "block"` with explicit `"captures"`):

1. **Plan Campaign (`~15 ms`, 0 writes):**
   ```powershell
   anode verify . --campaign --max-workers 4 --json > plan.json
   ```
2. **Worker Candidate Formats:**
   - **Expression candidate:**
     ```json
     {
       "id": "crate::my_fn#0",
       "hole_fingerprint": "<from brief>",
       "kind": "expression",
       "replacement": "a + b"
     }
     ```
   - **Multi-statement block candidate (`v1.3.0`):**
     ```json
     {
       "id": "crate::my_fn#0",
       "hole_fingerprint": "<from brief>",
       "kind": "block",
       "captures": [
         { "name": "s", "type": "DocState.{mut cadence, tokens, sentences}", "borrow": "exclusive" }
       ],
       "replacement": "{ let scaled = s.tokens * 1000; s.cadence = scaled / s.sentences; return s.cadence; }"
     }
     ```
3. **Verify in RAM (`"apply": false`) or Atomically Commit (`"apply": true`):**
   ```powershell
   anode verify . --campaign responses.json --json
   ```

---

## 8. Standard Library Packages (`$ANODE_HOME/packages/`)

All 8 packages are pure `.an` libraries: every `[capabilities]` entry is `false`, so they perform no file, process or network I/O. Read files, arguments and the clock in your own `main.an` and pass the data in. Parameters typed `&str` / `&[u8]` need `region` views at the call site (§2.1). The SDK's `docs/API-DOCS.md` is the generated reference for every public item.

| Package | Rust equivalent | Main API |
| :--- | :--- | :--- |
| `cli` | `clap` | `parse_args(&texts) -> CliArgs`, `has_flag`, `flag_count`, `flag_value_or`, `positional_or`, `render_help` |
| `encoding` | `crc32fast` / `hex` / `base64` | `crc32_bytes`, `crc32_text`, `hex_encode_bytes`, `hex_encode_u64`, `try_hex_decode_into`, `base64_encode_bytes`, `try_base64_decode_into` |
| `csv_jsonl` | `csv` / JSON Lines | `parse_csv_line`, `non_empty_lines`, `extract_json_string`, `extract_json_int` |
| `markdown` | `pulldown-cmark` (blocks) | `scan_markdown(&str) -> Blocks` (`soa vector` of `BlockSpan`), `count_words_in_range`, `count_sentences_in_range`, `count_kind` |
| `glob_regex` | `glob` / `regex` (subset) | `glob_match`, `find_subpattern`, `contains_subpattern` |
| `json` | `serde_json` (tree) | `parse_json(&str) -> JsonParse`, `object_get`, `array_len`, `array_get`, `query_path`, `node_kind`/`node_int`/`node_bool`/`node_text`, `render_json` |
| `docgen` | `rustdoc` (Markdown) | `extract_module_docs(&str, &str) -> ModuleDoc`, `render_module_markdown` |
| `bench` | `criterion` (reporting) | `compute_stats(&str, int, &ints, u64) -> BenchStat`, `render_bench_markdown`, `render_bench_json` |

- **`cli`** — `parse_args` turns `texts` into `CliArgs { subcommand, flags, positionals }`. Grammar: `--` makes every later token positional; `--key=value` and `--key`; `-abc` is three short flags and `-o=value` gives the last one a value; a lone `-` and negative integers are bare tokens; the first bare token is the subcommand. `flag_value_or` returns the last `key=value`; `render_help` prints an aligned flag table from `FlagSpecs`.
- **`encoding`** — IEEE 802.3 CRC-32 (`"123456789"` → `0xCBF43926`) and RFC 4648 base16/base64. Checksums and decoders are `@effect(no_alloc, no_io, bounded)`; decoders write into a caller-owned `&mut [u8]` and return the byte count, or `-1` with the buffer unchanged on invalid input or overflow. Helpers: `try_copy_text_into`, `bytes_eq_text`, `hex_encode_text`, `base64_encode_text`.
- **`csv_jsonl`** — RFC 4180-style rows (quoted fields, `""` escapes, one trailing CR ignored) and one-object-per-line JSON: `extract_json_*` read top-level keys only, decode every string escape including `\uXXXX` surrogate pairs, and return `""` / the fallback when a key is missing.
- **`markdown`** — one bounded pass producing `BlockSpan { kind, start, end, level, words, sentences }` with kinds 1 heading, 2 paragraph, 3 code fence, 4 list item. `start..end` are byte offsets, so `slice(doc, start, end)` gives the block text.
- **`glob_regex`** — allocation-free, bounded matchers. Globs: `*`, `?`, `[a-z]`, `[!..]`/`[^..]`, `\x`. Subpatterns: `^`, `$`, `.`, `\x`, literals; returns the first match offset or `-1`.
- **`json`** — strict parser (null, booleans, 64-bit integers, escaped strings, arrays, objects; no fractions or exponents; depth ≤ 128) into `JsonDoc { nodes: JsonArena, ids: JsonIds, root }`. Nodes are int indices in pre-order (root = 0); `ids[i]` holds the arena handle of node i. `query_path(&doc, "a.b.0")` walks members and array indices; `render_json` writes compact JSON in source member order.
- **`docgen`** — scans `.an` source line by line: the leading comment block becomes the module summary, and `//` comments, `@effect(...)` and `@intent("...")` directly above a `pub` declaration document it. It records signatures, field views and `requires`/`ensures` clauses, and `render_module_markdown` writes a GitHub Markdown API reference.
- **`bench`** — timing is I/O, so measure in `main.an` (e.g. `monotonic_ms()` around a batch of runs) and pass the samples in microseconds. `compute_stats` sorts a copy and reports exact min, median, nearest-rank P95, max and total; `render_bench_markdown` / `render_bench_json` produce deterministic reports.
