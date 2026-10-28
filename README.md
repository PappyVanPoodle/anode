<p align="center">
  <img src="assets/anode-logo-full.png" alt="anode.an — a systems language built for speed" width="560">
</p>

# anode.an

Anode is a compiled systems language for Windows x64. Source files use the `.an` extension.

- The syntax is close to Rust, with ownership and moves, but there are no lifetime annotations. A view into a string or buffer exists only inside the `borrow` block that created it.
- A function can list the struct fields it may use (`&mut Counter.{ mut value, step }`), declare effects it must not have (`@effect(no_alloc, no_io, bounded)`), and state `requires` / `ensures` conditions. The compiler checks all three.
- Arithmetic, array bounds and number conversions are checked in every build mode.
- `anode run` builds with Cranelift. `anode build --release` generates C and compiles it with MSVC.
- Programs can call `http://` and `https://` endpoints with `http_request(url, body)`, or `http_request(url, body, headers)` to send headers such as `Authorization`, when `net = true` is set in `Anode.toml`. `keys(m)` lists a map's keys. A source file may be up to 16 MiB and a project up to 256 MiB.

Version **1.3.0** · MIT License · [Download](https://github.com/PappyVanPoodle/anode/releases/latest) · [Language guide](AGENTS.md) · [Standard library API](docs/API-DOCS.md) · [Compiler source](https://github.com/PappyVanPoodle/anode/tree/main/compiler)

## Example

```anode
pub struct Counter {
    value: int,
    step: int,
    label: text
}
// May read `step` and write `value`; nothing else in Counter is reachable.
@effect(no_alloc, no_io, bounded)
pub fn advance(c: &mut Counter.{ mut value, step }) -> int requires c.step > 0 && c.value >= 0;
ensures result > 0;
{
    c.value = c.value + c.step;
    return c.value;
}
fn main() {
    let mut c = Counter { value: 0, step: 3, label: "ticks" };
    let v = advance(&mut c);
    print concat(concat(c.label, " = "), int_text(v));
}
```

If `advance` also assigned `c.label`, `anode check` would reject it:

```text
view contract for m0_advance.c does not permit mutation at field path [2]; narrow the helper call or declare that field
```

## Install

You need Windows 10 or 11 (x64) and Visual Studio 2022 Build Tools with the C++ workload. Anode links executables with MSVC's `link.exe`, and uses `cl.exe` for `--release`, `--sanitize` and the C backend.

1. Download [`anode-1.3.0-windows-x64.zip`](https://github.com/PappyVanPoodle/anode/releases/latest/download/anode-1.3.0-windows-x64.zip) from [Releases](https://github.com/PappyVanPoodle/anode/releases/latest) and extract it, for example to `C:\Tools\anode-1.3.0-windows-x64`.
2. Run the installer from that folder. It adds `bin\` to your user `PATH` and sets `ANODE_HOME`. It does not copy files or need administrator rights. `-WhatIf` shows what it would change; `-Uninstall` removes the changes.
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```
3. Open a new terminal:
   ```powershell
   anode new hello_anode
   cd hello_anode
   anode run .
   ```

## Commands

| Command | What it does |
| :--- | :--- |
| `anode new NAME` | Creates a project: `Anode.toml`, `src/`, `.gitignore`, `AGENTS.md`, `CLAUDE.md` |
| `anode check . --json` | Type, ownership, borrow, field-view and effect checks; diagnostics as JSON |
| `anode test . --jobs 4` | Runs the `test` blocks, 4 at a time |
| `anode run . -- ARGS` | Builds and runs the program |
| `anode build . -o app.exe` | Builds an executable; add `--release` for the MSVC build |
| `anode fmt .` | Formats the sources |
| `anode verify . --json` | Evaluates `requires` / `ensures` on sampled inputs without building |
| `anode doctor` | Shows which MSVC installation is used |

On the 9-file standard library showcase, `anode check` takes about 23 ms, and running its 35 tests after an edit takes about 0.27 s.

## Standard library

`packages/` has eight libraries written in Anode. None of them may read files, start processes or use the network. Add them to a project as path dependencies; the path is relative to your `Anode.toml`, or absolute:

```toml
[dependencies]
json = { path = "C:/Tools/anode-1.3.0-windows-x64/packages/json" }
```

| Package | Contents |
| :--- | :--- |
| [`cli`](packages/cli/src/lib.an) | Command-line parsing: subcommands, `--key=value`, `--flag`, `-abc`, `--`; help text |
| [`encoding`](packages/encoding/src/lib.an) | CRC-32, hex and base64 (RFC 4648); the decoders write into a buffer you pass in |
| [`csv_jsonl`](packages/csv_jsonl/src/lib.an) | CSV rows with quoted fields; string and integer fields from JSON Lines |
| [`markdown`](packages/markdown/src/lib.an) | Splits a document into headings, paragraphs, code blocks and list items, with word and sentence counts |
| [`glob_regex`](packages/glob_regex/src/lib.an) | Glob matching (`*`, `?`, `[a-z]`) and a small pattern search (`^`, `$`, `.`); neither allocates |
| [`json`](packages/json/src/lib.an) | JSON parser into an arena-backed tree, path queries such as `limits.depth`, compact output |
| [`docgen`](packages/docgen/src/lib.an) | Markdown API documentation from `.an` source files |
| [`bench`](packages/bench/src/lib.an) | Min, median, 95th percentile and max of timing samples; Markdown and JSON reports |

[docs/API-DOCS.md](docs/API-DOCS.md) lists all 72 public items; it was generated with `docgen`. [docs/BENCHMARK.md](docs/BENCHMARK.md) has timings from the development machine: about 62 µs for a CRC-32 of 3.5 KB, 0.5 µs to glob-match 8 paths and 3.8 µs to parse a 261-byte JSON document. The clock there moves in 15 ms steps, so these are rough.

[examples/stdlib-showcase](examples/stdlib-showcase) uses all eight packages, plus binary file I/O and the in-memory loopback server:

```powershell
cd examples\stdlib-showcase
anode test . --jobs 4
anode run .
anode run . -- doc      # writes .zenith\API-DOCS.md
anode run . -- bench    # writes .zenith\BENCHMARK.md and .json
```

## Compared with C and Rust

The same line scanner written three ways. It reads `label|amount` lines, skips blank and `#` lines, and counts rows, the total and malformed lines. All three were compiled (MSVC `/W4`, `rustc` 2021 edition, `anode`) and print `2 10 1` for `"# c\nalpha|12\nbad\nb|-2\n"`.

| | C | Rust | Anode |
| :--- | :--- | :--- | :--- |
| Buffer bounds | Checked by hand (`digits[32]`) | Checked | Checked (`region` views) |
| Lifetime annotations | — | `Entry<'a>`, `parse<'a>` | None |
| No allocation / no I/O / terminates, stated in the signature | No | No | `@effect(no_alloc, no_io, bounded)` |
| Pre- and postconditions | Comments | Comments or `debug_assert!` | `requires` / `ensures`, checked on every call |
| Signed integer overflow | Undefined behavior | Panics in debug builds, wraps in release builds | Stops the program in every build |

<details open>
<summary>Anode</summary>

```anode
pub struct Stats {
    rows: int,
    total: int,
    errors: int
}
// Folds one "label|amount" line into `s`: no heap, no I/O, always finishes.
@effect(no_alloc, no_io, bounded)
fn add_line(line: &str, s: &mut Stats.{ mut rows, mut total, mut errors }) {
    for i in 0..len(line) {
        if byte_at(line, i) == 124 {
            borrow digits = region(line, i + 1, len(line)) {
                if i > 0 && is_int(digits) {
                    s.rows = s.rows + 1;
                    s.total = s.total + parse_int(digits);
                    return;
                }
            }
            break;
        }
    }
    s.errors = s.errors + 1;
}
@effect(no_alloc, no_io, bounded)
pub fn scan(src: &str) -> Stats requires len(src) <= 16777216;
ensures result.rows + result.errors <= len(src);
{
    let mut s = Stats { rows: 0, total: 0, errors: 0 };
    let mut start = 0;
    for i in 0..len(src) + 1 {
        if i == len(src) || byte_at(src, i) == 10 {
            if i > start && byte_at(src, start) != 35 {
                borrow line = region(src, start, i) {
                    add_line(&line, &mut s);
                }
            }
            start = i + 1;
        }
    }
    return s;
}
```
</details>

<details>
<summary>Rust</summary>

```rust
#[derive(Debug, Default)]
pub struct Stats {
    pub rows: u64,
    pub total: i64,
    pub errors: u64,
}

/// A parsed row borrows its label from the line, so the struct
/// and the function both carry the lifetime `'a`.
pub struct Entry<'a> {
    pub label: &'a str,
    pub amount: i64,
}

pub fn parse<'a>(line: &'a str) -> Option<Entry<'a>> {
    let (label, digits) = line.split_once('|')?;
    if label.is_empty() {
        return None;
    }
    let amount = digits.parse::<i64>().ok()?;
    Some(Entry { label, amount })
}

pub fn scan(src: &str) -> Stats {
    let mut s = Stats::default();
    for line in src.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse(line) {
            Some(entry) => {
                s.rows += 1;
                s.total += entry.amount;
            }
            None => s.errors += 1,
        }
    }
    s
}
```
</details>

<details>
<summary>C</summary>

```c
#include <stdlib.h>
#include <string.h>

typedef struct { long rows, total, errors; } Stats;

/* Caller must pass the true length of `line`; nothing checks it. */
static int parse_amount(const char *line, size_t len, long *amount) {
    const char *bar = memchr(line, '|', len);
    if (!bar || bar == line) return 0;
    char digits[32];                          /* fixed buffer: size checked by hand */
    size_t n = len - (size_t)(bar + 1 - line);
    if (n == 0 || n >= sizeof digits) return 0;
    memcpy(digits, bar + 1, n);
    digits[n] = '\0';
    char *end;
    *amount = strtol(digits, &end, 10);       /* out-of-range input silently clamps */
    return *end == '\0';
}

Stats scan(const char *src, size_t len) {
    Stats s = {0, 0, 0};
    size_t start = 0;
    for (size_t i = 0; i <= len; i++) {
        if (i == len || src[i] == '\n') {
            if (i > start && src[start] != '#') {
                long amount;
                if (parse_amount(src + start, i - start, &amount)) {
                    s.rows++;
                    s.total += amount;        /* signed overflow is undefined */
                } else {
                    s.errors++;
                }
            }
            start = i + 1;
        }
    }
    return s;
}
```
</details>

## AGENTS.md and CLAUDE.md

`anode new` writes `AGENTS.md` and `CLAUDE.md` into every new project, for code assistants working in it. [AGENTS.md](AGENTS.md) covers how the syntax differs from Rust, collections and borrows, field views and effects, every builtin, and the standard library; section 2.2 lists the mistakes the compiler rejects most often. [CLAUDE.md](CLAUDE.md) is a short checklist that points to it.

## Tests

- 617 compiler tests passed in the 1.3.0 release run (62 test targets). Several features were added after that run: the `AGENTS.md` / `CLAUDE.md` files written by `anode new`, `keys`, `http_request` and the larger source limits. Their tests are in the 1.3 runtime test target, which passes on the current build (10 tests, including `keys` on the native, C and AddressSanitizer builds and `http_request` round trips through WinHTTP). The full suite has not been rerun since these additions.
- The compiler frontend written in Anode was built in three stages; Stage 1, 2 and 3 produce identical C (796,798 bytes).
- The standard library and the showcase have 35 tests: 31 in the packages and 4 that combine packages.

## License

Anode, its standard library packages and the compiler source are released under the [MIT License](LICENSE).

The Windows SDK includes third-party components, including Cranelift and the Rust standard library. Their license terms and notices are in [THIRD-PARTY/README.txt](THIRD-PARTY/README.txt).

## Repository layout

```text
README.md               this page
LICENSE                 MIT License
AGENTS.md  CLAUDE.md    guides for code assistants (also written by `anode new`)
install.ps1             per-user installer (ships inside the release zip next to bin\)
assets/                 logo and icon
THIRD-PARTY/            notices for third-party components in the SDK
docs/                   API-DOCS.md, BENCHMARK.md, LANGUAGE.md, SAFETY.md
packages/               the eight standard library packages
examples/               stdlib-showcase
compiler/               Rust source of the compiler and language server (repository only)
site/                   GitHub Pages landing page (repository only)
```

The compiler binaries (`anode.exe`, `zn.exe`, `anode-lsp.exe`) ship in the release zip; [compiler/](https://github.com/PappyVanPoodle/anode/tree/main/compiler) has their source and build steps. Further reading: [docs/LANGUAGE.md](docs/LANGUAGE.md) (language reference by release) and [docs/SAFETY.md](docs/SAFETY.md) (safety boundary and capability model).
