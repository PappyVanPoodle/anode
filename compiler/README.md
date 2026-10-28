# Anode compiler source

This folder is the Rust source of the Anode 1.3.0 compiler (`zn` / `anode`) and of its language server (`tools/anode-lsp`). The prebuilt SDK in [Releases](https://github.com/PappyVanPoodle/anode/releases/latest) contains the same binaries.

## Build

You need Rust (stable, 2021 edition) and Visual Studio 2022 Build Tools with the C++ workload. The compiler's runtime (`src/runtime.c`) is compiled with MSVC when a program is built.

```powershell
cd compiler
cargo build --release
# target\release\zn.exe is the compiler; the SDK ships the same file as anode.exe too.

cd tools\anode-lsp
cargo build --release
# target\release\anode-lsp.exe
```

## Layout

| Path | Contents |
| :--- | :--- |
| `src/syntax.rs` | Lexer and parser |
| `src/checker.rs` | Types, ownership, borrows and builtins; produces checked IR |
| `src/views.rs`, `src/effects.rs` | Field-view contracts and `@effect` checking |
| `src/fast.rs` | Native backend (Cranelift) |
| `src/cgen.rs`, `src/cgen_helpers.rs` | C backend (MSVC) |
| `src/runtime.c`, `src/runtime.h` | Runtime linked into every program |
| `src/project.rs` | `Anode.toml`, modules, packages, `anode new` |
| `src/verify.rs`, `src/holes.rs` | Contract sampling and `??` hole campaigns |
| `src/main.rs` | The `anode` command line |
| `tools/anode-lsp/` | Language server |

The compiler's test suite and release measurements are not included here; they depend on files outside this folder.

Licensed under the MIT License (see [LICENSE](LICENSE)). Third-party crates and the Rust standard library keep their own licenses; see [THIRD-PARTY/README.txt](../THIRD-PARTY/README.txt).
