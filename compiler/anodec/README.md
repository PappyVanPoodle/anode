# anodec

`anodec` is a second Anode compiler, written in Anode. It checks a project, generates C and compiles it with MSVC (`cl.exe`). The `anodec.exe` in the release zip was built by `anodec` from the sources in this folder.

It covers a subset of the language (see [What it does not support](#what-it-does-not-support)). For everything else, use `anode`.

## Commands

| Command | What it does |
| :--- | :--- |
| `anodec check [DIR\|FILE.an] [--json]` | Checks the project. Prints `ok`, or each diagnostic as `file:line: message` (`--json` for JSON) |
| `anodec build [DIR\|FILE.an] [-o OUT.exe] [--debug]` | Generates C and compiles it with `/O2`. The default output is `.zenith\build\<name>.exe` |
| `anodec run [DIR\|FILE.an] [--debug] [-- ARGS...]` | Builds, runs the program with `ARGS`, prints its output and exits with its exit code |
| `anodec test [DIR\|FILE.an]` | Runs each `test` block in its own process. Prints `test <name>` for each pass and `<N> tests passed` |
| `anodec new DIR` | Creates `Anode.toml`, `src/main.an`, `AGENTS.md` and `CLAUDE.md` |
| `anodec add PKG...` | Adds `PKG = { std = true }` to `[dependencies]` in `.\Anode.toml` |

`--debug` compiles with `/Od /Zi`, puts `#line` directives in the C so a debugger shows the `.an` source, and writes `OUT.pdb`. A failed `assert` in a debug or test build reports the `.an` file and line.

`--toolchain RECORD` and `--runtime-dir DIR` override the MSVC and runtime lookup described below. The older flag interface (`--emit-c`, `--build-exe`, `--summary`, `--project`, `--package`) still works.

## Projects and dependencies

`anodec` reads `[package] name`, `[build] entry` (default `src/main.an`) and `[dependencies]` from `Anode.toml`. `mod name;` loads `name.an` from the same folder as the file that declares it.

```toml
[dependencies]
json = { std = true }               # %ANODE_HOME%\packages\json
local = { path = "../local" }       # relative to this Anode.toml, or absolute
```

`{ std = true }` works for the eight standard packages, which are read from `%ANODE_HOME%\packages` (or from `..\..\packages` next to the project). Only `anodec` understands it; `anode` 1.3.0 needs `{ path = "..." }`.

With `anodec`, `bench`, `cli`, `csv_jsonl`, `docgen`, `glob_regex` and `json` check, build and pass their own tests. `encoding` needs `&[u8]` slices and `markdown` hits a limit of the C generator (overlapping call arguments); use `anode` for projects that need them.

## MSVC and the runtime

`anodec` needs Visual Studio 2022 Build Tools with the C++ workload, and `ANODE_HOME` set to the SDK folder (`install.ps1` sets it). It looks for the compiler in this order:

1. `%ANODE_HOME%\bin\toolchain.record`
2. `%LOCALAPPDATA%\ZenithLanguage\native-cache\toolchain.txt`, which `anode` writes
3. vswhere and the newest Windows 10/11 SDK; the result is saved as `%ANODE_HOME%\bin\toolchain.record` (or to the cache in step 2 when `ANODE_HOME` is not set)

The C runtime (`bvalue.h`, `runtime.c`, `bprocess.h`) is read from `%ANODE_HOME%\bin\codegen-runtime`, or from `codegen-runtime\` in the current folder.

When a command needs the SDK and `ANODE_HOME` is not set, `anodec` stops and says how to set it. That covers `anodec new` (it copies `%ANODE_HOME%\AGENTS.md`), a `{ std = true }` dependency, and building without a local `codegen-runtime\`. `anodec check` on a project without std dependencies works without it.

## What it does not support

Compared with `anode` 1.3.0, `anodec` rejects field views (`&mut T.{ ... }`), `requires` / `ensures`, `spawn` / `join`, `http_request` and the other network builtins, `try_read_bytes` / `try_write_bytes`, `&[u8]` slices and `??` holes. It has no Cranelift backend, formatter, `verify` or language server.

`anodec run` captures the program's output and prints it when the program exits, up to 16 MiB each of stdout and stderr, with a 10-minute limit.

## Language additions

These work in `anodec` but not yet in `anode` 1.3.0:

- `for x in &c` where `c` is `ints`, `texts`, a fixed array `[T; N]`, or a map with `text` or integer keys (the loop visits the keys). The collection is borrowed, not changed, during the loop.
- `concat` with 2 to 16 arguments.

Both compilers have `keys(m)`, `env_var`, `file_exists`, `create_dir`, `remove_file`, `list_dir`, `exit_code`, `text_find`, `text_replace` and `codepoint_text`.

## Building anodec

You need `anode` 1.3.0 for the first stage, and MSVC. `ANODE_HOME` is optional here: the stages use the `codegen-runtime\` in this folder.

```powershell
cd compiler\anodec
anode test . --jobs 4                        # 50 tests, run by anode
anode build . -o stage1.exe                  # Stage 1: built by anode
.\stage1.exe build . --debug -o stage2.exe   # Stage 2: built by anodec (Stage 1)
.\stage2.exe build . -o anodec.exe           # Stage 3: built by anodec (Stage 2)
.\anodec.exe test .                          # the same 50 tests, run by anodec
```

Stage 2 is a debug build because Stage 1 runs on `anode`'s runtime, which stops a child process after 30 seconds, and `cl.exe /O2` takes longer than that on this compiler's C (about 1 MB). Stage 3 is optimized.

Every stage emits the same C. To compare: `.\stageN.exe --emit-c stageN.c --project .`, then compare the files. For the sources in this folder, all stages produce 988,327 bytes with SHA-256 `47fb60e1dbfa9972a93f6bf27575d7b4a1b3536bc95fe5abc5bf2ca3f5d21a88`.

The compiler's own source avoids the language additions above, because `anode` has to compile it for Stage 1.

## Layout

| Path | Contents |
| :--- | :--- |
| `src/lexer.an`, `src/parser.an` | Tokens and syntax tree |
| `src/ast.an` | Tree types, names, module resolution |
| `src/checker.an` | Types, moves, borrows, builtins, `@effect` |
| `src/cgen.an` | C generation, `#line`, test runner |
| `src/ir_dump.an` | Text dump of the checked tree (`--summary`) |
| `src/main.an` | `Anode.toml` loader and the command line |
| `codegen-runtime/` | C runtime linked into every `anodec` program |
| `fixtures/` | Inputs for the tests |
| `frontend.sources` | Source list for the older `--project` flag |

Licensed under the MIT License (see [LICENSE](../LICENSE)).
