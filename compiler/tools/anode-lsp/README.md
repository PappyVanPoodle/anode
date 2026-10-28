# `anode-lsp`: Language Server for Anode

Standalone Rust Language Server Protocol (LSP) implementation for Anode (`.an` / `.zn`), using `bin/anode.exe` as the single source of truth via its JSON Lines `agent` and `context` interfaces.

Anode 0.26 adds pinned worker campaigns, dependency URI navigation and read-only rename protection, vector-region inference, and subprocess result hover/highlighting. See [the campaign and navigation protocol](V026-CAMPAIGNS.md). Channels, `try`, bounded verification and witness proposals retain [the 0.25 protocol](V025-VERIFY.md); pinned batches and recovery retain [the 0.24 protocol](V024-BATCH.md). The LSP is version 0.26.0.

## Build, Test & Install

`tools/anode-lsp` is an isolated Cargo workspace with its own `Cargo.toml`, `Cargo.lock`, and `.cargo/config.toml` (`target-dir = "target"`, `rustc-wrapper = ""`).

```powershell
$env:RUSTC_WRAPPER = ''
Push-Location tools/anode-lsp

# Build release binary (tools/anode-lsp/target/release/anode-lsp.exe)
cargo build --release --locked --offline

# Run unit + protocol + real-compiler smoke tests
cargo test --locked --offline

# Run Clippy with warnings denied
cargo clippy --all-targets --locked --offline -- -D warnings

Pop-Location
```

### CLI Usage

```powershell
.\tools\anode-lsp\target\release\anode-lsp.exe [--compiler PATH] [--debounce-ms MS] [--stdio]
```

- `--compiler PATH`: Path to `bin/anode.exe` (also configurable via `ANODE_COMPILER` or LSP `initializationOptions.compilerPath`; auto-discovers `<workspace>/bin/anode.exe` by default).
- `--debounce-ms MS`: Debounce delay in milliseconds for coalescing rapid `textDocument/didChange` edits before querying the compiler (default: `120`).
- `--stdio`: Communicate over standard input/output using `Content-Length` framed JSON-RPC 2.0 (stdout is strictly protocol frames; all logs go to stderr).

---

## Implemented Capabilities

1. **Stdio LSP Server (`src/lsp.rs`, `src/server.rs`)**:
   - Handles `initialize`, `initialized`, `shutdown`, `exit`, `$/cancelRequest`, `textDocument/didOpen`, `textDocument/didChange` (both full and UTF-16 incremental range sync), `textDocument/didSave`, `textDocument/didClose`, `workspace/didChangeWatchedFiles`, `workspace/didChangeConfiguration`, `textDocument/hover`, `textDocument/definition`, `textDocument/documentSymbol`, `textDocument/formatting`, `textDocument/references`, `textDocument/prepareRename`, `textDocument/rename`, `workspace/executeCommand`, and custom `anode/context` / `anode/projectStatus`.
2. **In-Memory Unsaved Diagnostics (`src/compiler.rs`, `src/diagnostics.rs`)**:
   - Maintains one persistent `anode.exe agent <project> --json` child process per active project (`Anode.toml`, `Zenith.toml`, or standalone `.an`/`.zn` file).
   - Sends unsaved buffer contents via `"overlays"` without writing unsaved editor text to disk.
   - Normalizes Windows `\\?\` verbatim paths and `file://` URIs so diagnostics map cleanly back to open documents and clear when fixed.
3. **Debouncing, Cancellation & Stale Response Discarding (`src/server.rs`)**:
   - Coalesces rapid `didChange` bursts per project over a configurable debounce window.
   - Tracks an atomic monotonic `generation` counter per project shared between the coordinator and compiler worker thread; queued checks and completed compiler responses from older generations are deterministically discarded.
   - Tracks in-flight request IDs for `$/cancelRequest` (returning JSON-RPC `-32800` `request cancelled`) and verifies open document `version`s before and after worker execution so stale `textDocument/definition`, `textDocument/documentSymbol`, `textDocument/references`, `textDocument/prepareRename`, or `textDocument/rename` responses never navigate to outdated locations or apply stale edits (`-32801` `ContentModified`).
4. **Typed-Hole Diagnostics & Hover (`src/diagnostics.rs`, `src/hover.rs`)**:
   - Extracts `details.kind == "synthesis_hole"` from compiler diagnostics, highlights the exact UTF-16 span of `??(type, "intent")`, sets `code: "synthesis_hole"` and `data: details`, and formats the expected declared type (`declared_type_is_not_inferred`), intent, available scope bindings (`name`, `type`, `borrow`, `mutable`, `moved`, `moved_fields`, `borrowed_in_expression`, `exclusive_in_expression`), `@effect` contracts, and project capabilities.
5. **Function Context Hover (`src/hover.rs`, `src/unicode.rs`)**:
   - Resolves the cursor's identifier, `::`-qualified call path, or `@intent`/`@effect`/`pub fn`/`test` header to a module-qualified function name (`MODULE::FUNCTION`) and queries `{"method": "context", "function": "...", "overlays": ...}`.
   - Renders reconstructed Anode signatures (including `&`, `&mut`, and explicit `.{...}` field views), source file/line, `@intent` guidance, `@effect` contracts, inferred `may_allocate`/`may_do_io`, inferred parameter field-index views, relevant record/enum definitions, callers, and callees.
6. **Compiler-Backed Code Navigation, Find References & Safe Rename (`src/navigation.rs`, compiler `src/navigation.rs`)**:
   - **Go to Definition (`textDocument/definition`)**: Converts the editor's 0-based UTF-16 cursor position to the compiler's 1-based UTF-8 `(line, column)` and queries `{"method": "symbol_at", "file": "...", "line": L, "column": C, "overlays": {...}}`. Navigates to functions, `test`s, parameters, local bindings (`let`, `borrow [mut] name = region(...)`, `match` arm bindings), `struct`/`enum` types, record fields (declarations, literals, `.field` access, `AssignField`, `&var.field` borrows, and `.{...}` parameter view contracts), and enum variants (declarations, constructors, and `match` patterns), including cross-module (`mod::...`, `crate::...`) definitions and distinct same-name bindings across scopes. Returns `null` when no symbol is resolved — never guesses from identifier text.
   - **Document Symbols / Outline (`textDocument/documentSymbol`)**: Queries `{"method": "symbols", "file": "...", "overlays": {...}}` and converts compiler symbols into nested LSP `DocumentSymbol` trees (`Struct` -> `Field`, `Enum` -> `EnumMember`, `Function` -> `Variable` parameters and locals) with exact UTF-16 `range` and `selectionRange`.
   - **Find References (`textDocument/references`)**: Queries `{"method": "references", "file": "...", "line": L, "column": C, "include_declaration": true|false, "overlays": {...}}` and resolves all occurrences by symbol identity across the loaded project, excluding comments and string contents, distinguishing same-name bindings across scopes, and returning deterministically ordered LSP `Location`s.
   - **Safe Project-Wide Rename (`textDocument/prepareRename` & `textDocument/rename`)**: Queries `{"method": "prepare_rename", ...}` and `{"method": "rename", "new_name": "...", ...}` without touching disk (`writes: 0`). Returns a versioned `WorkspaceEdit` (`documentChanges` with open document `version`s plus `changes`). Rejects invalid identifiers, keywords/builtins, `fn main`, declaration collisions, stale/incomplete snapshots, full-project check failures (`Project::checked`), and renames that capture or rebind any reference (`DeclKey` binding-preservation check in memory). Reports `"scope": "loaded_project"` and a `scope_warning` when renaming public symbols that may have external consumers.
7. **UTF-8 <-> UTF-16 Position Conversion (`src/unicode.rs`, `src/navigation.rs`)**:
   - Converts accurately between the compiler's `"utf8-1based"` coordinate convention (1-based `line`, 1-based UTF-8 byte `column`, 0-based `byte` offset, half-open `[start, end)`) and LSP 0-based UTF-16 code units (handling BMP multi-byte characters, non-BMP surrogate pairs like emoji, and LF/CRLF line endings).
8. **Canonical Document Formatting (`src/formatting.rs`)**:
   - Sends `{"method":"format","file":"src/main.an","overlays":{...}}` to the compiler and returns a whole-document `TextEdit`, or an empty array when already canonical. Formatting uses four spaces and LF outside literal contents; editor indentation options do not alter the canonical style. Comments, literal spelling, annotations and language tokens are retained. Syntax errors produce an explicit request failure. Formatting does not assert that ownership, effects or types are valid.
   - Computes the replacement range in UTF-16 from the original document, verifies the compiler's source/output fingerprints, checks all open project document versions, supports cancellation, and rejects a changed snapshot with `ContentModified` (`-32801`). It does not write files. Selected-range formatting remains unsupported.

## Formatting and diagnostic recovery (Anode 0.22)

Compiler agent `format` responses include `writes: 0`, `snapshot_fingerprint` and `files`, with each file carrying its project-relative `file`, `path`, `changed`, formatted `text`, original `file_fingerprint` and `formatted_fingerprint`. The request optionally accepts the same project/file fingerprint guards as rename. Omit `file` to prepare all loaded source files; manifests are not reformatted. Libraries do not require `main` for formatting.

A failed agent `check` now returns up to 32 distinct diagnostics (`max_diagnostics` can lower the cap), `diagnostics_limit`, `diagnostics_truncated`, and `recovery` metadata. Syntax recovery checks independent original declaration slices. Semantic probes reorder the visitation of unchanged declarations in detached complete snapshots; they do not mask statements, delete invalid code or validate a modified program. The original strict failure remains `ok: false`, `recovery.validated` is false, and compilation still rejects the invalid candidate. Recovery is bounded to 32 probes and can miss additional errors within a single function or behind a project-wide failure.

Protocol tests use the freshly built LSP binary by default. Set `ANODE_TEST_LSP` or `ANODE_TEST_COMPILER` to test explicit binaries; no historical staged binary is selected implicitly.

---

## Compiler Navigation & Refactoring Protocol (`symbol_at`, `symbols`, `references`, `prepare_rename`, `rename`)

Implemented in `src/navigation.rs` and wired in `src/agent.rs`:

- **Coordinate convention (`coordinate_encoding = "utf8-1based"`)**:
  - `line`: 1-based line number.
  - `column`: 1-based UTF-8 byte offset within the line.
  - `byte`: 0-based UTF-8 byte offset within the file.
  - Ranges (`range`, `selection_range`, `reference_range`) are half-open `[start, end)`.
- **`{"id": 1, "method": "symbols", "file": "src/reader.an", "overlays": {...}}`**:
  - Returns hierarchical module symbols (`id`, `name`, `qualified_name`, `kind`, `detail`, `file`, `path`, `range`, `selection_range`, `children`).
- **`{"id": 2, "method": "symbol_at", "file": "src/reader.an", "line": 20, "column": 42, "overlays": {...}}`**:
  - Returns `symbol: null` if the position does not reference a symbol, or a resolved symbol object (`id`, `name`, `qualified_name`, `kind`, `detail`, `type`, `file`, `path`, `range`, `selection_range`, `reference_range`, `is_definition`).
- **`{"id": 3, "method": "references", "file": "src/reader.an", "line": 24, "column": 42, "include_declaration": true, "overlays": {...}}`**:
  - Returns `symbol`, `include_declaration`, `snapshot_fingerprint`, and deterministically ordered `references` (`symbol_id`, `name`, `qualified_name`, `kind`, `file`, `path`, `range`, `text`, `is_declaration`).
- **`{"id": 4, "method": "prepare_rename", "file": "src/reader.an", "line": 24, "column": 42, "overlays": {...}}`**:
  - Validates that the project snapshot is clean (`Project::checked`) and the target symbol supports renaming; returns `can_rename: true`, `placeholder`, `range`, `symbol`, `scope: "loaded_project"`, `public_symbol`, `scope_warning`, and `snapshot_fingerprint`.
- **`{"id": 5, "method": "rename", "file": "src/reader.an", "line": 24, "column": 42, "new_name": "decode_packet", "snapshot_fingerprint": "...", "overlays": {...}}`**:
  - Validates the new identifier, checks for collisions, applies edits in memory, verifies `Project::checked(false)` (and `Project::checked(true)` when tests exist), and verifies `DeclKey` binding preservation across all project references before returning `writes: 0`, `snapshot_fingerprint`, `file_fingerprints`, and non-overlapping `edits` (`file`, `path`, `range`, `original_text`, `new_text`, `is_declaration`, `file_fingerprint`).

## Signature help and prelude completion (Anode 1.2)

- `textDocument/signatureHelp` (triggers `(` and `,`, `src/signature.rs`): the server finds the innermost open call at the cursor, skipping string literals, `//` comments and nested brackets. It resolves the callee with compiler `symbol_at` and splits the returned detail (`name(params) -> T; documentation`) into label, parameters, documentation and the active parameter (top-level commas so far). Builtin details come from compiler navigation, which documents every 1.2 Frontier 3 builtin: `try_write_u{16,32,64}_be` and the `wire_` aliases, `fnv1a_64`, `sha256_text`, `net_request`, and the `NetResponse`/`NetResult` prelude with its fields and variants.
- `textDocument/completion` (trigger `:`) offers the language prelude only: the 1.2 builtins and aliases, `process_run`, the `NetResponse`/`NetResult`/`ProcessOutput`/`ProcessRun` types, and their variants after `NetResult::` or `ProcessRun::`. Visible bindings, record fields and user functions still need an agent-protocol `complete` method, which protocol v1 does not have; no such items are guessed.

## Remaining Compiler Interface Follow-Ups

1. **Multi-Diagnostic Accumulation**:
   - Extend the bounded independent-declaration recovery to multiple hard errors within one function without changing program validity or losing ownership context. Typed holes are enumerated separately by `list_holes` below.
2. **Column / Byte Span Information in `check` Diagnostics**:
   - Include `column` / `end_line` / `end_column` on `Error` diagnostics returned by `check`.
3. **Partial `context` Queries When Project Has Unresolved Holes or Errors**:
   - Allow `{"method": "context", "function": "..."}` to inspect valid functions even when another function has an unresolved hole or type error.
4. **In-Session Agent Reload & Tolerant Startup**:
   - Support `{"method": "reload"}` inside `agent::serve` and allow `overlays` to introduce newly added module files without restarting the agent subprocess.

## Multi-hole proposals and finite contract evidence (Anode 0.23)

The compiler agent adds three methods. Each accepts the existing project-relative `overlays`; none writes source files. `list_holes` and `verify_holes` never execute code.

`{"method":"list_holes"}` returns `snapshot_fingerprint`, `file_fingerprints`, `holes`, and ordinary checker diagnostics. Slots use lexical order within a qualified function: `crate::name#0`, `module::name#0`, or `module::Type::method#0`. IDs stay stable when unrelated lines move; fingerprints change with the source. Every slot includes its exact half-open UTF-8 byte `range`, `expected_type`, hole/function intents, effects, declared field views, requires/ensures source, available functions, and `checked_context.scope` with types, moves, mutability and loans. Context is unavailable after a hard error prevents reaching that slot, or where contract/body occurrences cannot be distinguished. Listing still reports every syntactic slot. Intent prose is advisory.

```json
{"method":"verify_holes","snapshot_fingerprint":"from listing","candidates":[
  {"id":"crate::sum#0","hole_fingerprint":"from slot","replacement":"x + 1"},
  {"id":"crate::sum#1","hole_fingerprint":"from slot","kind":"block",
   "replacement":"{ let adjusted = x + 2; return adjusted; }",
   "captures":[{"name":"x","type":"int","borrow":"owned"}]}
]}
```

The array accepts 1–32 distinct slots. Snapshot and slot fingerprints are mandatory; optional `file_fingerprints` and compiler-coordinate `range` guards must match exactly. Captures are explicit identifiers with source types and `owned` (default), `shared`, or `exclusive` borrowing. A block becomes a normally checked private helper; `edits` contains its EOF insertion, and `files` contains complete composed sources. Helpers preserve the original function signatures, effects, views and contracts. Expressions are separately constrained to the hole's declared type before checking the actual replacement snapshot. Imports, declarations and nested unresolved holes cannot escape the slot. Contract-expression slots are listed with `in_contract:true` but replacement returns an `unsupported_boundary` diagnostic because contracts are locked.

The response contains `independent` per-slot results, `composed`, descending-byte-order `edits`, complete `files`, optional `helpers`, and `writes:0`. A result with other unresolved holes is explicitly `conditional`, `validated:false`. Typed assumptions exist only inside a diagnostic checker API that returns no emittable IR. Only the ordinary checker accepting the actual all-filled snapshot yields `composed.validated:true`. Every source body and test body is checked; tests are never run. Contract clauses remain enforced and type checked by the compiler; hole verification does not execute or prove them.

`{"method":"verify_contracts"}` checks unchanged source and plans deterministic domains. `execute:true` explicitly opts into a bounded evaluator of checked IR. Options are `functions` (qualified names), `max_cases` (1–4096, default 128 total), `max_steps` (1–1000000 per case, default 100000), and `timeout_ms` (1–10000 total, default 1000). CLI `anode verify` opts into execution by default; `--plan` requests the static plan. The CLI also supports `--execute`, repeated `--function`, `--max-cases`, `--max-steps`, `--timeout-ms`, and `--json`.

Integer domains include 0/1/-1 where representable and declared-width extrema; floats include 0/1/-1 and finite extrema, booleans include both values, text/ints/texts include empty values and small representatives. Fixed buffers and nominal records/enums/containers have deterministic zero/empty representatives. Domains form a deterministic Cartesian grid bounded by the total case budget. Reports distinguish `passed`, `precondition_rejected`, postcondition `counterexample`, `runtime_fault`, `skipped`, and `budget_exhausted`; rejecting every input is incomplete evidence. `sampled` is finite evidence, with `universal_proof:false`, not a proof or a backend equivalence guarantee. Runtime uses neither native execution nor user test runners. IO, threads, FFI, unknown builtins, exclusive parameters and scoped alias operations are skipped before execution; supported pure calls, arithmetic, checked conversions, owned empties, conditionals and bounded loops are interpreted. Values also have fixed memory budgets, and large report values are explicitly truncated.

LSP capabilities advertise `experimental.anodeSynthesis` and `quickfix.anode.hole`. Requests `anode/listHoles`, `anode/verifyHoles`, and `anode/verifyContracts` take `textDocument:{uri,version}` plus compiler method fields. Results retain the compiler report under `{ok,result,diagnostics}`; listings add `lsp_range`, URI and document version per slot. Verified proposals include a versioned UTF-16 `workspace_edit`. Synthesis results are rejected on captured-source/version changes or cancellation. `anode/verifyContracts` remains static and rejects `execute:true`; the separate `anode/verify` request opts into bounded pure execution. Document symbols include typed holes, and `textDocument/codeAction` returns the `anode.proposeHole` command with fingerprinted slot metadata. VS Code's command prepares and displays a checked proposal for review without applying it.
## Live verification (0.25)

The read-only `anode/verify` request evaluates bounded pure checked IR over
versioned source overlays and can return checked witness-test insertion edits.
See [V025-VERIFY.md](V025-VERIFY.md) for request limits, cancellation, dependency
source keys and the expected-failure semantics of counterexample tests.

