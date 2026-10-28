# Anode 1.3 language reference

## 1.3: OS/runtime primitives and block campaigns

1.3 adds eight builtins and no types: the six OS/runtime builtins in the table, plus `keys` and `http_request` (described after it). They use `int`, `bool`, `text`, `texts`, `ints`, byte buffers and the existing `NetResult`, so every program keeps its type ids. A program that calls none of them compiles to the same native code and the same C as under 1.2. The six table names and `http_request` are reserved; `keys` is contextual. All eight allocate or perform I/O, so `requires`/`ensures`/`assert` reject them.

| Builtin | Capability | Effects |
| --- | --- | --- |
| `try_read_bytes(path, dst) -> int` | `file_read` | I/O, no allocation, bounded |
| `try_write_bytes(path, src) -> bool` | `file_write` | I/O, no allocation, bounded |
| `env_var(name) -> text` | `process_args` | I/O, allocating, bounded |
| `create_dir(path) -> bool` | `file_write` | I/O, no allocation, bounded |
| `remove_file(path) -> bool` | `file_write` | I/O, no allocation, bounded |
| `net_serve_once(endpoint, status, reply_body) -> NetResult` | `net` | I/O, allocating, never bounded |

Paths, names, endpoints and reply bodies are borrowed `text` or `str`. A path or name that is empty, contains a NUL byte or is not valid UTF-8 fails without any system call.

**Binary files.** These two builtins work with raw bytes, `0x00` and `0xFF` included.
- **`try_read_bytes(path, dst)`** reads the whole file into `dst[0..n]` and returns `n`.
  - `dst` must be a live, unborrowed, mutable `[u8; N]` or `&mut [u8]`.
  - If the file cannot be opened or read, or is larger than `dst` or 16 MiB, the result is `-1` and `dst` is unchanged.
  - The file is opened without write or delete sharing, so its size cannot change during the read. Only a device error inside the read itself can leave `dst` partly written; the result is still `-1`.
- **`try_write_bytes(path, src)`** writes every byte of a borrowed `[u8; N]` or `&[u8]`, creating or replacing the file. It returns `false` for more than 16 MiB, or when the file cannot be opened or fully written.

**Environment and directories.**
- **`env_var(name)`** returns the variable as new owned UTF-8 text. It returns `""` when the name contains `=`, or when the variable is unset, empty, not convertible to UTF-8 or over 16 MiB.
- **`create_dir(path)`** creates one directory. It returns `true` when the directory was created or already exists as a directory.
- **`remove_file(path)`** deletes one file and returns `true` when it was deleted.

**A one-shot local server.** `net_serve_once(endpoint, status, reply_body)` returns `NetResult`.
- **`loopback://serve/<payload>`** is served in memory, with no socket. `status` must be 100–599; the result is `Response(NetResponse { status, body: "<payload>" })`.
- **`http://127.0.0.1:<port>/<path>`** binds a listener and waits for one connection, with a 5,000 ms budget for the whole exchange.
  - It reads one HTTP/1.x request whose target must be exactly `/<path>`. The body is limited by `Content-Length` (empty without one), at most 16 MiB, and must be strict UTF-8.
  - It replies `HTTP/1.0 <status> OK` with `Content-Length`, `Connection: close` and `reply_body`, then returns `Response(NetResponse { status, body: <request body> })`.
  - A wrong path gets a `404` reply and `Error`; a malformed request gets `400` and `Error`.
- **`tcp://127.0.0.1:<port>`** requires `status == 0`. It reads until the peer half-closes, sends `reply_body`, and returns `Response(NetResponse { status: 0, body: <request bytes> })`.
- **Anything else**, including `localhost`, returns `Error("net_serve_once allows only loopback:// or 127.0.0.1 endpoints")` without binding.
- **Ports:** the listener uses `SO_REUSEADDR`. On Windows this also lets another socket bind the same port while it is in use, so choose a port no other program is serving.
- **Failures** are always `Error` values, never faults. They cover invalid status, timeout, bind failure and invalid request. The texts start with `net_serve_once`.

A spawned server and an in-program client work together: `let s = spawn serve(...)` in a worker that returns `net_serve_once(...)`, then `net_request` from the caller, then `join(s)`. The client should retry, because a connection refused before the server binds is an ordinary `Error`.

**Map keys.** `keys(m)` returns a new collection of the keys of a map whose key type is `text` (as `texts`) or `int` (as `ints`). Text keys are copied. The map is only borrowed and stays usable. The order is the map's internal slot order: unspecified, though it does not change while the map is unchanged. Other key types report "keys requires a map with text or int keys". `keys` allocates, so contracts and `@effect(no_alloc)` reject it; it is `no_io` and `bounded`. Like `contains_key`, it is a contextual name: a user function called `keys` keeps its meaning.

**External HTTP.** `http_request(url, body) -> NetResult` sends one request to an `http://` or `https://` URL through Windows WinHTTP, using the system proxy settings. `net_request` is unchanged and still reaches only loopback endpoints.
- It sends `GET` when `body` is empty and `POST` otherwise. A non-empty body goes out as `application/json` when its first non-space character is `{` or `[`, else as `text/plain; charset=utf-8`. No other headers can be set.
- `loopback://` URLs are passed to `net_request`.
- The URL is checked before any network activity. The host must be ASCII (use punycode for international names; IPv6 goes in brackets), the port 1–65535, and the path and query printable ASCII without spaces (percent-encode anything else). User info (`user@host`) is rejected; a `#fragment` is dropped.
- The request and the response are each limited to 16 MiB, and the response must be strict UTF-8 without NUL. Timeouts are 10 s to resolve, 10 s to connect, and 30 s each to send and receive.
- It needs `[capabilities] net = true`, like `net_request`. It performs I/O, allocates and is never `bounded`. Every failure is an `Error` value whose text starts with `http_request`.
- `winhttp.dll` is loaded on the first request, so a program gains no new DLL import.

**Source limits.** A source file may be up to 16 MiB and a whole project up to 256 MiB (previously 1 MiB and 8 MiB).

**Agent campaigns with block candidates.** `verify_batch`, `commit_holes` and `verify_campaign` now accept `"kind": "block"` candidates, as `verify_holes` already did.
- **How a block is applied:** each block is a braced body with an explicit `"captures"` array (possibly empty) of `{name, type, borrow}`. It becomes a signature-locked private helper `fn anode_synthesis_<index>_<nonce>(...)`, appended at the end of the file and called at the hole with the captured values.
- **What is rejected as a request error:** a block without a `captures` array, `captures` on an expression, and any other kind.
- **Receipts and briefs:** `exact_hole_only` is `false` whenever helpers were appended. An applied batch writes exactly the composed file text, helpers included. Worker briefs read: "expression or captured-block replacements at assigned exact hole spans; signatures, imports, views, effects and contracts are locked".

The rest of this reference describes the language as of 1.2 and earlier, all of which still applies.

## 1.2: bytes, hashing and loopback networking

1.2 adds six builtins, three aliases, two standard types and one capability. Programs that use none of them compile to the same native code and, apart from the runtime header text, the same C as in 1.1. The new names are reserved: a function cannot be named `fnv1a_64`, `sha256_text`, `net_request` or `try_write_u{16,32,64}_be` (or the `wire_` aliases), and a type cannot be named `NetResponse` or `NetResult`.

**Big-endian wire writes.** `try_write_u16_be(buf, offset, value)`, `try_write_u32_be` and `try_write_u64_be`, with the aliases `wire_try_write_u16_be`, `wire_try_write_u32_be` and `wire_try_write_u64_be`, mirror `try_write_u*_le`:
- `buf` is a live, unborrowed, mutable `[u8; N]` or `&mut [u8]`, and `value` has the matching unsigned width.
- When `offset < 0` or `offset + width > len(buf)`, the call returns `false` and leaves the buffer unchanged.
- Otherwise it stores the value most-significant byte first and returns `true`.

They are not allowed in contracts ("contracts cannot mutate buffers"). They do not allocate, perform no I/O, and are `bounded`. When the offset is proven in range on a fixed buffer, both backends store inline without a test.

**Hashing.** Both builtins take one borrowed `text`, `str`, `[u8; N]` or `&[u8]`; any other input reports "`<name>` requires text, str, [u8; N] or &[u8]".
- **`fnv1a_64(input) -> u64`** is 64-bit FNV-1a over the bytes: offset basis `0xcbf29ce484222325`, prime `0x100000001b3`. It does not allocate, performs no I/O, and is `bounded`. It is allowed in `requires`, `ensures` and `assert`, and the verifier evaluates it.
- **`sha256_text(input) -> text`** is FIPS 180-4 SHA-256 as 64 lowercase hex characters.
  - It allocates, so it is rejected under `@effect(no_alloc)`, and in `requires`/`ensures` with "contracts cannot allocate or perform I/O", the existing rule for allocating text builtins.
  - It performs no I/O and is `bounded`.

**Networking.** `net_request(endpoint, payload) -> NetResult` takes `endpoint` and `payload` as borrowed `text` or `str`. The two standard types are:

```
struct NetResponse { status: int, body: text }
enum NetResult { Response(NetResponse), Error(text) }
```

`try` unwraps `Response` or propagates `Error`.

- **Capability.** It requires `[capabilities] net = true`, which `anode new` writes as `net = false`. Like `process` and `ffi`, `net` is never granted implicitly, and a dependency's grant is limited to its parent's.
  - Single files and roots without a `[capabilities]` section report "net_request requires the net capability (default false)".
  - A manifest without `net = true` reports "project does not grant capability net".
  - A function that reaches it through a package that does not grant `net` reports "package <pkg> does not grant capability net required transitively by <fn>".
- **Effects.** It is rejected in contracts. It is not allowed under `@effect(no_io)`, `@effect(no_alloc)` or `@effect(bounded)` (it waits on a peer, like `process_run`), including transitively. The verifier never executes it.
- **Endpoints.**
  - `loopback://echo`: status 200, body = payload. Served in memory, without sockets.
  - `loopback://sha256`: status 200, body = `sha256_text(payload)`.
  - `loopback://head-fnv1a`: status 200, body = the `int` text of `fnv1a_64(payload)` read as a signed value; for `"a"` it is `-5808556873153909620`.
  - `loopback://status/NNN`: status `NNN` (three digits, 100–599), body = payload.
  - `http://127.0.0.1:<port>/<path>`: one HTTP/1.0 `POST` with `Content-Length` and `Connection: close`. Status = the response status; body = the response body, limited by `Content-Length` when present.
  - `tcp://127.0.0.1:<port>`: sends the payload, half-closes, and reads until the peer closes. Status = 0.
- **Rejected endpoints.** Every other scheme or host, including `localhost`, `127.0.0.2` and `[::1]`, returns `Error("net_request allows only loopback:// or 127.0.0.1 endpoints")` without connecting.
- **Limits.**
  - Endpoint, payload and response are each at most 16 MiB with no NUL byte.
  - A response must be strict UTF-8.
  - The whole exchange (connect, send and receive) has a 5,000 ms budget.
- **Failures are values, never faults.** Timeouts, refused connections, malformed responses and limit violations all return `Error(text)`. The texts are listed in measurements/v1200-feature-evidence/frontier3-runtime/HANDBACK.md. Examples:
  - "net_request timed out after 5000 ms";
  - "net_request connection failed (Winsock error 10061)";
  - "net_request unknown loopback route".
- **Layout.** `NetResult` uses the ordinary enum encoding: tag 1 = `Response`, tag 2 = `Error`, the same as `ProcessRun`. Its C mirror is `zn_net_result` in runtime.h. Programs load the Windows socket library only on their first socket request, so no program, including one that calls `net_request`, gains a DLL import.

**C backend (performance, no visible change).** A function whose view parameters were checked on entry now calls itself directly into its checked-once copy, without repeating the entry test. A local vector view created from a region is checked once, and its accesses later in the same block use one compare, or none where the index is proven below the region's count.

The rest of this reference describes the language as of 1.1 and earlier, all of which still applies.

## 1.1: same language, one visible runtime change

1.1 adds no syntax and changes no ownership rule, fault message or fault order. One value a program can observe changes:

- **`capacity(v)` after the first pushes.** A plain (non-SoA) vector that grows from empty now allocates 16 slots on its first push instead of 4, then doubles as before. Only a vector that never held more than 8 elements since it was empty reports a different capacity: 16 instead of 4 or 8. `capacity` still means "allocated slots"; the language has never promised a growth policy. SoA vectors, maps, arenas and channels grow exactly as in 1.0, and `clone` of any vector still allocates exactly its length.

Everything else is check removal under proof, plus faster runtime helpers:

- **Constant masks.** `bit_and(x, K)` on `u64` or a narrower unsigned type, with `K` a compile-time constant, is known to lie in `[0, K]`. When `K <= 9223372036854775807`, `int(bit_and(x, K))` needs no range check on either backend. Other masks keep the check and its fault.
- **Native SoA and conversions.** After a `push`, each column is written directly. Dropping or cloning a vector whose elements own nothing (scalars, fixed buffers, and records/enums of those) releases or copies storage without a per-element loop. `int` and `u64` to `f32`/`f64`, and `f32`↔`f64`, compile inline, bit-identical to the runtime conversion for every input, NaN included. `float` to `int` keeps its runtime checks and messages.
- **Overflow guards before loops (native).** In an innermost loop with no calls, a checked `+`/`-` of loop-invariant values plus a bounded loop value can be covered by one guard before the loop. When the guard holds, the loop runs without those per-iteration overflow branches. Otherwise the original checked loop runs and faults with the same message at the same iteration. The C backend has done this since 1.0.
- **Vector-view parameters (C backend).** A short function that reads a vector-view parameter in a loop tests the view once on entry. A well-formed view runs a copy with one compare per access, or none where the index is proven. A malformed foreign view runs the unchanged checked copy, so its results and faults are exactly 1.0's. Native code has done this since 1.0. Plain-vector `push`/`pop` also run inline in C when the vector neither grows nor faults.
- **Runtime.** SoA helpers locate columns in one pass, linear in the column count (1.0 re-summed earlier columns for each one). `int` and text arrays clone with one exact allocation.

The rest of this reference describes the language as of 1.0 and earlier, all of which still applies.


## 1.0: same language, more proofs, two bugs fixed

1.0 adds no syntax and changes no ownership rule or fault message. Two results change, both from wrong to correct:

- **C/LTCG builds:** `x % 2^k` and `x % -2^k`, for a negative `x` that is a multiple of the divisor, now give 0. In 0.29 some inlined shapes gave -2^k (for example, `-256 % 256` gave -256). This was an MSVC 14.44 optimizer miscompile; the runtime's `zn_mod` now avoids it.
- **Native builds:** a loop now keeps its bounds check when it reassigns its own bound. Take `let n=len(a); while i<n { ... n=b; ... at(a,i) }`: 0.29 kept treating `n` as `len(a)` after the reassignment and could read out of bounds. Such a program now reports `array index out of bounds`, as the C backend always did.

Everything else is check removal under proof. The compiler keeps work out of hot loops only when it can show the result is unchanged:

- **Shared borrows.** A `&T` parameter, and fields reached only through shared borrows, keep their length and data pointer across loops that call `push`, `slice`, `concat` or helpers. Nothing in the call can replace them while the borrow holds. Exclusive (`&mut`) parameters keep them only when the function never replaces them.
- **Text lengths** are known to be non-negative, so `len(text)` no longer re-tests that.
- **Vector `push`/`pop`** run inline when the vector neither grows nor faults. Every other case calls the runtime, with the same messages.
- **Short functions with vector views** may be compiled twice. One entry test selects either the copy with proven accesses unchecked or the unchanged checked copy, so a malformed foreign view still faults exactly as before.
- **The C backend** learns ranges for local `int`/`bool` values from `if`, executed `assert` and loop bounds. It uses them for power-of-two `/`/`%`, conversions, fixed buffers, wire access and `+ - *`, which become plain C only when the ranges prove no overflow. A branch-free innermost loop may run an unchecked copy behind one up-front guard; if the guard fails, the original checked loop runs with the same fault at the same point.

Runtime behavior visible to measurement tools:
- Maps keep a private 7-bit tag per slot in the same allocation, so a map's control block is twice its capacity.
- Text keys of 1–7 bytes hash differently from 0.29. Maps still expose no iteration order.
- An idle task worker may poll the queue briefly (one worker at a time) before sleeping.
- `int_text` output is byte-identical to 0.29.

## 0.29: faster checked code, same semantics

0.29 adds no syntax and changes no source-visible result, fault message or ownership rule. The compiler removes work only when it can prove the outcome is unchanged:

- `a / 2^k` and `a % 2^k` on `int` become a shift or mask when `a` is proven non-negative and the divisor is exactly that power of two. Negative or unproven dividends keep truncating signed division, so `-7 / 2 == -3` and `-7 % 2 == -1` as before, and division by zero still faults.
- Integer conversions such as `u8(x)` keep their `integer conversion out of range` check unless the proven range of `x` already fits the target type.
- `fixed[T; N]` indexing and wire reads/writes skip their bounds check only for indices proven inside `0..N` (or `0..=N-width`). Unproven accesses still report `array index out of bounds` or `packet read out of bounds`; `try_*` operations keep their exact true/false result.
- Vector and region indexing keep the same messages and fault order; a malformed foreign `VectorView` still reports the index fault before the storage fault.

Runtime behavior visible to measurement tools: `spawn`/`join` reuse up to 64 small task blocks (results of 64 bytes or less) until `zn_task_pool_shutdown` or process exit, so an allocation tracker that expects zero live blocks mid-program must shut the pool down first. Text map keys use a new internal hash; maps still expose no iteration order. A joiner may briefly spin and run queued tasks inline before sleeping; `exit_code` is still rejected inside every task.

## Exact-width modular integers

For T = int/i64, i8, i16, i32, u8, u16, u32 or u64: wrapping_add(T,T), wrapping_sub(T,T) and wrapping_mul(T,T) return T modulo 2^width. bit_and, bit_or and bit_xor take two values of the same T; bit_not takes one. shl, shr, rotl and rotr take (T,int) and return T. Counts are converted to unsigned and masked by width-1, including negative or oversized counts. Right shift is arithmetic for signed T and logical for unsigned T; rotations use exactly that type's width. These operations are pure, no_alloc, no_io and bounded, and are allowed in contracts. They do not change ordinary checked arithmetic or checked numeric conversions.

```text
fn next(value:u8)->u8 { return wrapping_add(value,u8(1)); }
fn mix(value:u32)->u32 { return bit_xor(rotl(value,-1),u32(7)); }
```

Big-endian byte reads are read_u16_be/read_u32_be/read_u64_be; wire_read_u16_be/wire_read_u32_be/wire_read_u64_be are equivalent source aliases. They use the existing checked packet-buffer/offset rules and return the matching unsigned type. Existing little-endian reads keep their behavior. Invalid offsets still produce packet read out of bounds; there is no implicit unchecked pointer operation.

## Runtime representation and execution

Text stays a 16-byte descriptor with a valid NUL-terminated UTF-8 data pointer. Compiler-generated private static descriptors tag their length; compiled readers mask it and borrowed views store a plain length. Source ownership/move rules do not change. Dynamic constructors still own copied storage. Direct C clients explicitly using the new zn_text_literal API must use zn_text_size; exported supported POD ABI still rejects text/other heap owners, so the private tag cannot silently enter that boundary.

spawn/join source signatures and typed result layouts stay unchanged. The runtime reuses a bounded worker pool, combines task metadata/results, validates opaque move-only handles and helps execute nested work before waiting. Argument packets remain separately owned. Join/implicit drop destroys results once. Pool shutdown/restart and process-exit cleanup are checked; reload module ownership includes pool teardown before final unload.

Native internal f32/f64 calls use XMM registers; public C signatures and packed memory remain unchanged. Detected POPCNT/LZCNT/BMI1 features are bound into native cache identity. A generated binary may require the features selected on its compilation host; unsupported hosts compile using the safe baseline fallback. Ordered reductions and unproven aliases retain conservative paths.

## Earlier language features (through 0.27)

## 0.27 execution and tooling compatibility

The public language surface and checked program/type/IR contracts remain compatible with 0.26. No new unchecked form or weaker annotation is introduced. Vector windows still use start/count while legacy text/integer regions retain start/end. Generational id bits do not prove arena liveness; channel state is not inferred from a scalar descriptor. Unknown or mutating calls conservatively invalidate optimization facts.

Native code generation builds one shared immutable function lookup/range-analysis context and one fallible reference-counted target ISA per analyzed program. Parameter-sensitive helper summaries retain their original inputs. Only channel payload types required by a module's functions/locals and transitive owned layouts receive module cleanup thunks.

Frontend ownership-taking paths move already-owned ASTs through remapping, method elaboration and checking. Public borrowed APIs remain available. Bounded request-local caches reuse syntax and raw navigation spans only for exact source bytes and matching path/module/package identities. Altered source text is reparsed. Every candidate, worker and composed proposal still receives ordinary semantic checking.

The CLI reload-generation default now uses the same validated compiler digest as build receipts, under a versioned hash domain. Explicit generation names, source/layout/provider pins, fresh-input checks, migration rules and publication guards retain their existing behavior.

The interactive website's feature snippets are bound to executable examples and labelled context wrappers. Its campaign server medians refer to a small persistent-agent fixture; full roundtrip, startup, model generation and linking are separate quantities.

## Earlier language features (through 0.26)

## Scoped contiguous vector regions (0.26)

For a nominal contiguous declaration `vector Items(Item);`, `borrow sub = region(items,start,count) { ... }` creates a shared nonowning window. `borrow mut sub = region(items,start,count) { ... }` permits element mutation. Nested regions use coordinates relative to the supplied window. Vector regions use **start/count**; legacy text/str and integer-array/slice regions retain their existing half-open **start/end** semantics. `soa vector` storage is columnar and cannot be used as a contiguous record window.

Borrowed parameters use `&[Items]` and `&mut [Items]`; this is a view of Items' element storage, not an owned vector of vectors. Views cannot be owned, stored, returned or sent to a worker. Supported window operations include `len`, scoped `at`/`at_pair`, `clone_at`, mutable `replace`/`swap`, and shared/exclusive `for` traversal. A view cannot resize or move its owner. The entire underlying vector stays loan-protected for the lexical window; conflicting owner access rejects. Empty windows at the end are valid. Negative bounds/counts, oversized windows and overflowing element offsets fail through checked runtime guards. Entry checks use count <= length-start before address multiplication.

## Direct subprocess capture (0.26)

`process_run(program,&args) -> ProcessRun` takes text/str plus a borrowed texts collection or supported nominal text vector. `ProcessRun::Output` owns `ProcessOutput {status:int,stdout:text,stderr:text}`; `ProcessRun::Error` owns an error text. `try process_run(...)` works in an enclosing compatible two-variant result. Nonzero child exits are successful captures carrying the exact unsigned32-bit Windows exit status as an int. Launch/capture/timeout/invalid-text errors return Error instead of publishing partial success.

The root manifest, source package and each ancestor dependency must explicitly declare `[capabilities] process=true`; absent/legacy declarations default to false. Permission is checked on ordinary builds, hole probes, cache reuse, libraries and reloads. `process_args`, threads and host-run tests do not confer it. Calls allocate and perform IO and reject `no_alloc`, `no_io` and `bounded` contracts.

The Windows runtime calls CreateProcessW directly, with explicit UTF-16 argument quoting and a restricted inherited-handle list; it adds no implicit shell. Stdout and stderr drain concurrently, each capped at 16 MiB. Launch, process completion and pipe capture share a 30,000 ms deadline. Timeout or capture failure terminates the owned job. Text with embedded NUL or invalid UTF-8, and malformed captured UTF-8/NUL, return errors. Empty arguments, Unicode, quotes, spaces and trailing backslashes retain their argument identity.

## Multi-worker campaigns and package navigation (0.26)

`anode verify PROJECT --campaign --max-workers N --json` plans whole-function assignments for 1–16 workers (default 4, maximum 4096 holes). `--campaign REQUEST.json` verifies pinned worker responses; place PROJECT before the optional request flag. CLI and JSON actions must agree; `--batch` is exclusive. `--function` cannot restrict campaign evidence. `--emit-test`, explicit budgets and `--plan` configure response verification; planning alone never executes.

Every worker response carries the campaign/skeleton/snapshot pins and assigned hole fingerprints. Ordinary checking covers each worker, each candidate and the full composition. Default bounded pure evaluation checks newly completed functions, including those without written contracts; it never launches processes/IO/FFI/native code. Witness tests stay checked proposals. `apply:true` explicitly requests the existing guarded source transaction; source overlays use read-only agent/LSP endpoints. Exit0 means successful/planned/eligible/applied,1 invalid/failed/stale/malformed,2 incomplete/conditional/exhausted evidence. Exact schemas, constraints and transaction limits are in tools/anode-lsp/V026-CAMPAIGNS.md.

Cross-package definitions/references resolve actual dependency paths and source identities. Rename refuses dependency declarations or changes involving dependency occurrences. Vector-view and process builtin/type hovers use the same checked namespaces.

## Selected public ABI roots (0.26)

Optional `[build] exports = "function, module::function"` selects public POD-compatible static/reload entry points. All source bodies still receive ordinary checking. Unknown/private/duplicate/empty selections reject. With no setting, the prior behavior remains: all public functions, including dependency functions, are export roots. This permits internal owned text/collection helpers while keeping selected C/Rust interfaces POD/fixed-buffer only. The text-scoring-engine README documents real host calls and state migration.

## Earlier language features (through 0.25)

## Recoverable error propagation (0.25)

`try expression` has unary precedence and consumes an owned two-variant result enum. Its first variant supplies the expression's value; its second variant propagates to the enclosing function's second result variant when the error payload types agree. Success and error payloads may be unit or supported owned values. `FileRead` aliases the existing `ReadResult` type; both spellings remain accepted.

Propagation follows an ordinary checked early return: pending call loans close in reverse order, scoped aliases write back, earlier expression temporaries and lexical owners are cleaned up, and return postconditions run against the returned enum. Invalid result shapes, incompatible errors, escaping views and invalid postconditions receive normal diagnostics or checked failures. Runtime bounds/arithmetic faults retain their existing process-fatal behavior.

## Typed bounded channels (0.25)

`pub channel Jobs(Job);` declares a nominal owning endpoint type. `Jobs(capacity)` requires a positive checked capacity. `clone(endpoint)` shares the synchronized, reference-counted bounded FIFO. Supported worker-transferable scalars, IDs and finite owned aggregates/collections can be payloads; borrowed views, recursive owned layouts and nested tasks reject. Endpoint values can themselves move into and return from workers.

`send(ch, value) -> bool` consumes an owned payload and waits for space or closure. A closed send returns false and destroys that payload. `recv(ch, &mut slot) -> bool` waits for data or closed-and-empty; `try_recv` does not wait for data. Successful receive drops/replaces the old initialized slot; unsuccessful receive leaves it unchanged. `close(ch)` is idempotent and wakes waiters; buffered values remain drainable. `is_closed`, `len` and `capacity` return synchronized snapshots. The last endpoint destroys unread payloads through the generated typed destructor.

All channel synchronization has an I/O effect. Creation allocates ring storage. Blocking send/receive and task joins cannot publish `bounded`; nonwaiting queries/polling may have finite source control flow without a scheduling deadline. Contract queries are snapshots at evaluation time; cross-thread stability is never inferred. Owning channels remain excluded recursively from public static-library and hot-reload state boundaries.

## Column-loop optimization (0.25)

Eligible field-only SoA loops hoist stable column bases/strides outside the body. Independent single-column f32/f64 updates can use SSE lanes with scalar tails and the original operation order. Integer overflow, reductions, unknown calls, checked narrowing conversions and ambiguous expanded source mappings keep checked scalar paths. Bounds, ownership and assertions remain active. The channel-pipeline evidence includes actual native `mulps` and `addpd` disassembly plus differential native/C/C-ASan tests.

## Local packages and test execution (0.25)

`[dependencies]` in `Anode.toml` accepts `dep = "../path"` or `dep = { path = "../path" }`. Dependencies require manifests; missing paths, cycles, invalid aliases and conflicting declarations reject deterministically. Names resolve as `dep::Symbol` or `dep::module::Symbol`, including transitive package routes. Nominal types remain distinct across aliases. Package identities and element layouts participate in native and C cache receipts.

Dependencies receive only capabilities declared in their own manifest and permitted by every ancestor/root route. File access, process arguments, thread creation and FFI requirements propagate through checked calls. Historical root capability behavior remains, including explicit FFI opt-in. Source keys use `@dependencies/alias[/child]/src/file.an`; root keys stay unchanged. Project formatting writes root-package sources; dependency files remain inputs unless explicitly selected as their own formatting target. Compiler outputs cannot overwrite dependency manifests or sources.

`anode test PROJECT --jobs N` accepts 1–16 and defaults to 1. One checked dispatcher executable runs enumerated cases in independent bounded child processes; every case runs, and captured output/status is reported in source order. User-visible arguments remain empty and invocation working directory is preserved. Process memory is isolated; file fixtures still need to tolerate the chosen concurrency. The dispatcher grants no process-argument capability to source functions. Ordinary aggregate test compilation remains available through the existing library API.

## Verification witness proposals (0.25)

`anode verify PROJECT --emit-test [--function NAME] [--json]` returns deterministic source tests from real counterexamples/faults or completed successful boundary sampling. Proposals are checked in memory and carry exact insertion ranges, source/snapshot fingerprints, SHA pins and expected outcomes. They write zero files and do not execute generated tests. Counterexample/fault replays remain expected failures; successful samples remain finite evidence. `--plan` invents no witness; `--batch` uses its separate JSON options and cannot mix these CLI flags.

Read-only LSP `anode/verify` evaluates supported pure IR over captured versioned overlays, with budgets, cancellation and stale source/manifest checks. It can supply checked UTF-16 workspace edits for review. Unsupported I/O/FFI/channel operations report incomplete evidence. See tools/anode-lsp/V025-VERIFY.md for exact fields and limits.

This is the implemented subset, not a specification of future Rust feature parity.

## Generational arenas and handles (0.24)

`pub arena Entities(Entity);` declares an owning nominal arena. `id(Entities)` is a Copy 64-bit value containing a 32-bit slot and nonzero 32-bit generation. It can appear in records, enums, vectors, SoA columns, map keys/values and worker inputs/results. Only handles of the declared arena type are accepted; handle equality and inequality compare their bits. Handle arithmetic and implicit integer conversion are unavailable.

Construct with `Entities()`. `alloc(owner, value)` moves an element into a free/new slot and returns its handle. `free(owner, handle)` destroys a live element and returns true, or returns false for a stale/invalid handle. Freed slots increment their generation; maximum-generation slots retire permanently. `contains_id` and `len` query membership/live count without allocating. `clone_get` deep-clones a live element; stale access fails before memory access. Cloning an arena deep-clones occupied payloads and preserves its handle mappings.

Use `borrow item = get(owner, handle) { ... }` or its mutable form for scoped zero-copy access. `borrow mut (a, b) = get_pair(owner, first, second) { ... }` validates both generations and different slots before yielding two mutable aliases. The backing arena remains protected from relocation, free, movement and conflicting access while a loan exists. Views/ownership/cleanup rules remain active. Arena destruction drops only occupied payloads.

Handles are instance-relative: independent arenas of one nominal type, and snapshot clones, can contain the same slot/generation. Retain the owning scene alongside its IDs. Every access checks the supplied arena; a Copy ID alone is not a reference or a certificate of liveness.

## Bounded effects (0.24)

`@effect(no_alloc, no_io, bounded)` combines the existing allocation/I/O restrictions with finite source control flow. Bounded functions reject every `while`, direct/mutual recursion and calls to user functions that lack a proven, explicitly published `bounded` contract. Unknown/foreign callees reject. Finite range and borrowed collection loops retain checked bounds, single evaluation and ordinary ownership rules. This contract provides no scheduling or wall-clock deadline. See the four-module ECS example.

## Pinned batch synthesis and recovery (0.24)

`skeleton_receipt` and `list_holes` return canonical `skeleton_sha256` plus exact `snapshot_sha256`. Submit pinned expression candidates to `verify_batch` for a zero-write plan, or to `commit_holes` with explicit `apply:true` for guarded disk application. Each expression stays inside its assigned half-open UTF-8 hole range. Full composition must pass normal checking; remaining holes remain conditional. Optional contract execution is explicit finite evidence. The CLI is `anode verify PROJECT --batch REQUEST.json [--json]`; its JSON options govern execution and application, so ordinary verifier flags cannot be mixed with `--batch`. Exit 0 means eligible/applied, 1 invalid/stale/failure, and 2 conditional/incomplete.

Commits serialize cooperating writers using hard-link source locks, preflight all source/manifest pins, and use backed-up per-file atomic replacement. Multi-file OS-wide visibility and crash atomicity are not promised. `inspect_hole_transaction` and explicit `recover_holes` validate abandoned transactions and refuse live owners, corrupt backups or externally changed files. Read [the exact batch/transaction contract](tools/anode-lsp/V024-BATCH.md). Editor queries remain read-only, including `anode/holes` over versioned unsaved documents.

## Explicit reload migration and handle exports (0.24)

`anode hot-reload BASELINE UPDATED --migrate-fn NAME` accepts a checked public `@effect(no_io)` function taking owned old POD/fixed state and returning new POD/fixed state. Its layout digests bind the active input and new output. Dynamic `ReloadSlot::new_with_state` owns aligned bytes; `replace_state` holds quiescence across validation, compiled migration and atomic code/state/receipt replacement. `with_state_entry` and `state_snapshot` expose the active layout with its bytes; raw C function casts remain host ABI obligations. Nested bool values are validated and invalid callback edits roll back. Existing typed `replace` keeps layout-strict behavior.

Migration metadata is bound into actual DLL bytes and checked alongside source/artifact receipts. Public ABI, views and contracts beyond the selected state transition remain protected. `no_io` permits allocation and does not guarantee termination; runtime contract/arithmetic failures remain process-fatal. Owning arenas, enums, views and tasks are excluded from public migration state.

Static libraries can expose `id(Arena)` through nominal opaque 8-byte C/Rust structs using the aggregate pointer/final-result-pointer ABI. Rust wrappers pass Copy values; `from_bits` supplies bits without establishing liveness. Owning arenas remain internal, and checked arena operations validate handles. Cross-module remapping and native object dependencies include arena element layouts, ID types and bounded contracts.

## Fixed-point C bootstrap (0.24)

The seven-module Anode frontend emits C for all its own ordinary functions. Actual Stage 1 and Stage 2 compilers produce byte-identical C, with native/ASan IR, fixture and fault comparisons. The boxed subset binaries explicitly reserve 64 MiB stack and keep parser/depth guards. Its [README and receipts](examples/bootstrap-compiler/README.md) define the supported subset and host flags. The main Rust/Cranelift compiler and its application measurement profile remain separate.

## Columnar vectors, pairs and swap (0.23)

`pub soa vector Particles(Particle);` declares a nominal vector of records with one contiguous column per field. The 24-byte owner holds an allocation, length and capacity; column starts are padded to eight-byte boundaries, with adjacent elements at their field's natural width. Scalars, fixed buffers and finite owned fields retain ordinary type, clone and destruction rules. Recursive owned types and stored views remain rejected. Existing vector constructors, `len`, `capacity`, `push`, `pop`, `replace`, `clone_at`, scoped `at` borrows and shared/mutable iteration work with this storage kind.

Field-only aliases access columns directly. Whole-record aliases use nonowning stack projections, with mutable writeback on normal exit, return, break and continue. Projection, pair selection and swap do not allocate. There is no automatic SIMD-vectorization guarantee; the storage and field access are columnar.

`borrow mut (left, right) = at_pair(items, i, j) { ... }` creates two exclusive element aliases after checking both bounds and `i != j`. The backing vector remains protected for the whole scope. This works with ordinary and SoA vectors. Borrowed owners/fields cannot escape or move out. `swap(a, b)` exchanges distinct mutable bindings/fields of exactly the same type without allocation, cloning or destruction; normal field-view, borrow and partial-move checks still apply.

Like the contextual collection helpers, a declared user function named `swap` takes precedence in its scope. Existing programs keep their checked helper calls; the builtin applies where no such function resolves.

## Outbound C calls (0.23)

`extern fn symbol(parameters...) -> Return;` declares a raw C symbol, preserved separately from module-internal names. Enable `[capabilities] ffi = true` in the project manifest; declarations default to denial, including standalone source and legacy unrestricted manifests. New manifests set it false. Repeated `--link-lib FILE.lib` arguments supply explicit x64 COFF providers to `build`, `run`, `test` and static-library export. Provider bytes and canonical paths participate in output identities. Baked-output APIs reject extern programs rather than reuse an artifact without provider identity.

The Windows x64 ABI supports integer widths, `_Bool`, float/double and fixed integer buffers. Fixed parameters are explicit pointers to elements: owned/shared arguments use `const T*`, exclusive arguments use `T*`. Fixed results use a leading `T* result` argument and void C return. Native code must honor lengths, output initialization, constness, aliasing and pointer lifetimes. Record/enum/heap/view/task/varargs signatures and source contracts on extern declarations reject. All extern calls have conservative allocation/I/O effects. This is a trusted native boundary, not a proof of the foreign implementation.

## Parallel typed holes and contract evidence (0.23)

The JSONL methods `list_holes` and `verify_holes` enumerate and check `??(Type, "intent")` slots. IDs are qualified-function/lexical-index names within a fingerprinted snapshot; ranges use UTF-8 bytes. Reports include checked scope where available, expected types, intents, effect/view contracts and requires/ensures text. Proposals are expressions or checked helper blocks with explicit captures. Independent proposals and their composition are checked without writing files. Remaining holes produce conditional results with `validated:false`; only an ordinary all-filled check can validate the composed source. Contract-expression holes are listed but cannot be replaced through body synthesis. Stale identities, overlapping edits and boundary changes reject. The LSP/VS Code integration exposes symbols/actions with UTF-16 conversion and document-version guards.

`anode verify PROJECT` runs finite checked-IR boundary samples by default; `--plan` returns the static plan, and `--json` returns the full report. Select functions with repeated `--function NAME`; bound work with `--max-cases`, `--max-steps` and `--timeout-ms`. The evaluator has no native process, file, console, thread or FFI operations. Unsupported constructs and exclusive-parameter cases are reported separately. Preconditions can reject sampled inputs; postcondition failures, runtime faults and budget exhaustion remain distinct. CLI status 1 means invalid source/counterexample/fault; 2 means incomplete or no executed evidence; 0 means a completed finite sample or requested plan. Agent execution requires explicit `execute:true`; editor actions request static plans. This is finite evidence, not universal proof.

## Native reload (0.23)

`anode hot-reload BASELINE UPDATED [-o PATCH.dll]` strictly checks both complete source snapshots and compares canonical named layouts and public ABI/view/effect/contract boundaries. Record offsets/types, enum variants, map entry geometry and vector storage kinds participate, even where descriptor sizes match. Compatible changes produce a DLL, object, C header and JSON integrity/source proof. The default filename includes the source/options/provider generation so changed revisions can coexist with loaded code. Optional flags are `--release`, `--rebuild`, `--cache-mb`, `--link-lib` and `--json`.

`LoadedPatch` owns the code; `ReloadSlot` guards entry-point calls and waits for quiescence before replacement. A retained patch keeps old code alive. Hosts must use the documented ABI and synchronize state access. The supported public state is scalar/POD/fixed-buffer data; opaque owners, views and tasks reject at this boundary. This does not rewrite direct calls in an arbitrary running executable. Explicit generations are retained outside the object-cache budget; `storage` counts them and `clean` preserves them for host lifetime.

## Bootstrap C emitter (0.23)

The seven-module Anode frontend adds `cgen.an` and `--emit-c OUT.c`. It lowers supported AST bodies into actual C functions and control flow, using checked tagged-value/ownership support and a frozen C runtime. The verification script invokes installed MSVC, runs the generated executables and compares them with seed-compiled programs. Source emission is deterministic across native and ASan builds of the frontend. Unsupported or unsafe-to-elaborate constructs fail before output. The complete own-source frontend check succeeds; complete own-source native generation is still conservatively rejected. The example README specifies the Stage 1 subset. The main compiler and workbench continue to use the Rust-written seed, not this experimental tagged-value backend.

## Typed worker results and systems I/O (0.22)

`spawn function(arguments)` infers a linear `task<Result>` handle. `join(handle)` consumes the handle and transfers the function's result into the caller. Results may be scalars, fixed buffers, `text`, `ints`, `texts`, finite records, enums, vectors or maps, including nested owners. Unit-returning workers are also supported. Task type spelling is diagnostic metadata, not a source annotation. Worker parameters and results cannot contain borrowed views or task handles. Arguments still transfer ownership; no caller-local borrow crosses a thread boundary.

An unjoined handle is not detached: scope cleanup waits for it and destroys its result exactly once. A joined result is owned by its receiving scope. Moving a handle invalidates its previous binding, and cloning or joining it twice is rejected. Worker callbacks write into separate heap result storage; no worker returns a reference to its stack.

| Operation | Result and behavior |
| --- | --- |
| `monotonic_ms()` | `int` milliseconds since system boot; nondecreasing, I/O effect. |
| `file_exists(path)` | `bool`; true for an existing file or directory, false for a missing path. Invalid UTF-8/NUL paths and other filesystem errors fail. Requires `file_read`; I/O effect. |
| `list_dir(path)` | Owned `texts` of immediate UTF-8 basenames, including files and directories, excluding `.` and `..`, sorted by unsigned UTF-8 bytes. Invalid paths, non-directories, invalid Unicode names and enumeration errors fail. Requires `file_read`; allocation and I/O effects. |
| `exit_code(code)` | Set final process status, unit result. Integer 0–255 only, main thread only, including through nested helpers. Execution and ordinary cleanup continue; latest setter wins. I/O effect. `anode run` preserves this status. |

Directory enumeration is a snapshot, not a lock against filesystem changes. The multi-module `examples/parallel-directory-scanner` demonstrates owned worker reports and recoverable file reads.

## Windows static libraries and bindings (0.22)

`anode build PROJECT --lib [-o NAME.lib] [--release] [--rebuild] [--cache-mb N]` creates a Windows x64 MSVC static archive plus same-basename `.h` and `.rs` bindings. A library may omit `main`; tests are omitted, while normal name, type, privacy, ownership, view, effect and capability checking remains active. `--lib` uses the C ABI backend; `--sanitize`, `--baked` and `--backend fast` are rejected for this command. Ordinary executable builds are unchanged.

Eligible `pub fn` APIs use scalars, finite public POD records and fixed integer buffers. Owning text/collections, enums, borrowed views and task handles are rejected at the ABI boundary. Aggregate values use explicit input/output pointers in C; scalar results return directly. Generated wrappers convert checked values field by field. The C header states alignment, aliasing, lifetime, CRT and platform requirements; raw C callers must satisfy them. The Rust module exposes value and `&`/`&mut` wrappers around its private `extern "C"` declarations. No reference is retained after a call. Contract, bounds, arithmetic or runtime failures still terminate the host process; this is not a recoverable-error ABI.

Library bodies may use owned internal data, file I/O and workers under ordinary checks. `arg`, `args_count` and `exit_code` are executable-only and produce source diagnostics in library mode; a library does not initialize a host process entry/argument context.

Default managed artifacts are `.zenith/build/NAME.lib`, `.h` and `.rs` (or `NAME-release.*`). `storage` counts these artifacts; `clean` recognizes their named outputs. The export receipt validates the archive and both bindings, and every build checks current source before reuse. See `examples/embedded-math-lib` for actual C and Rust consumers.

## Formatting and editor diagnostics (0.22)

`anode fmt PROJECT` formats the project's reachable `.an`/`.zn` source files; a standalone source path also works. `--check` makes no writes and returns status 1 if any file needs formatting, status 0 if all are canonical. `--json` reports the checked and changed paths. The manifest is not formatted. All results are prepared before publication; the CLI rechecks the source and replaces each changed file from a sibling temporary file, preserving other hardlinks to the original data.

Formatting reparses input and output and preserves the ordered language, literal and comment tokens. It is deterministic and idempotent, changes whitespace, and requires valid syntax but no `main`. Semantically invalid but parse-valid source can be formatted; it remains invalid when checked. Raw location-bearing AST/IR hashes may change with line positions. The tests compare normalized semantic IR and the lossless token stream rather than claiming source metadata is unchanged.

The JSONL agent accepts `method: "format"` and returns prepared, fingerprinted files with `writes: 0`; it does not change source on disk. LSP `textDocument/formatting` uses this path, UTF-16 document ranges and in-flight source-version checks. The editor decides whether to apply its edits.

The agent's diagnostic recovery retains the strict failure and probes independent declarations/functions for additional errors. It has a finite 32-diagnostic/32-probe bound, does not mask invalid bodies into successful programs, and does not replace normal compilation checks. It may report fewer cascading errors inside one function. Snapshot and source fingerprints remain the basis for navigation and edits.

## Anode-written frontend (0.22)

`examples/bootstrap-compiler` is a real six-module lexer/parser/basic type-and-move checker. It emits deterministic textual AST/type IR and processes all six of its own sources, including test bodies, through that same path. It reads an explicit source list; the Rust compiler still builds the frontend executable and provides native code generation. The example README lists supported syntax, diagnostics, bounds and incomplete semantic proofs. A passing frontend check is not a substitute for the full seed checker.

## Owned hash maps (0.21)

Declare a nominal map with `map Name(Key,Value);`, optionally `pub`. `Name()` creates an empty owner without allocating. Keys are `text`, `int`, or any of `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`. Text keys compare exact UTF-8 bytes; integer keys preserve their declared width, including all bits of `u64`. Bool, float, record, enum and collection keys are rejected. Values support the same finite owned/copy categories as typed vector elements, including nested records, enums, vectors and maps. Unit, tasks and borrowed views cannot be stored. Recursive type cycles remain rejected, including cycles through maps or vectors. Separately named maps are distinct types even with identical key/value types.

```text
struct Symbol { declaration:int, uses:ints }
map Symbols(text,Symbol);
fn main() {
    let mut symbols=Symbols();
    let key="entry";
    insert(symbols,clone(key),Symbol{declaration:3,uses:[]});
    let present=contains_key(symbols,key);
    assert present;
    borrow symbol=get(symbols,key){print symbol.declaration;}
    borrow mut symbol=get(symbols,key){push(symbol.uses,9);}
    let independent=clone_get(symbols,key);
    let removed=remove(symbols,key);
    assert removed;
    print len(independent.uses);
    print key;
}
```

| Operation | Ownership and behavior |
| --- | --- |
| `len(m)` | Number of live entries as `int`; no allocation or I/O. |
| `contains_key(m,key)` | Borrow the lookup key and return whether it is present; no allocation or I/O. |
| `insert(m,key,value)` | Require mutable access; move owned key/value arguments and copy scalars. Insert or replace, destroying the old stored key and value exactly once. Return unit. Conservatively may allocate. |
| `remove(m,key)` | Require mutable access; borrow the lookup key, destroy an existing stored key/value, and return true. Return false if absent. Does not allocate or perform I/O. |
| `clone_get(m,key)` | Borrow the lookup key; return an independent deep copy of an owned value, or copy a scalar. A missing key terminates with `map key not found`. Owned results conservatively may allocate; scalar copies do not. No I/O. |
| `clone(m)` | Deep-copy the map and all owned keys and values into independent storage; conservatively may allocate. No I/O. |
| `borrow value=get(m,key){...}` | Shared scoped alias to an existing value; a missing key fails before access. No allocation or copy. |
| `borrow mut value=get(m,key){...}` | Exclusive scoped alias; permits field edits, nested collection operations and whole-value replacement. Replacement destroys the old value. No allocation for selecting the alias. |

`get` is available only in the scoped borrow form; it is not an escaping reference expression. The receiver must be a named map, record field, borrowed parameter or existing scoped alias. Mutation needs a mutable owner or exclusive access. Stored keys are immutable. Lookup/removal/clone/get read their key argument, so a named text key remains reusable. Insertion consumes an owned key: explicitly `clone(key)` when retaining a separate copy. Use the declared key type; numeric conversion is explicit.

The complete backing map is protected while a value alias is live. A shared alias blocks map insertion, removal, replacement and movement; an exclusive alias also blocks direct reads or other aliases through that map. Different keys do not prove disjoint mutable entries. Disjoint sibling record fields remain available. The receiver is protected before the key and later arguments are evaluated, so nested calls cannot resize or consume it during selection. The lookup key's loan ends after selection; an independent key variable can then be changed without changing the stored key. Scalar copies and explicit clones can escape; aliases and owned fields reached through them cannot be moved out, stored, returned or sent to a worker.

Maps move, return, deep-clone and drop under the normal owner rules, including ownership transfer to a worker. Scope exit, return, break, continue, replacement and consuming matches retain typed cleanup. `len`, `contains_key`, scoped `get` and scalar `clone_get` can be used by `no_alloc,no_io` functions when their argument expressions and body also satisfy those effects. `remove` is nonallocating but mutating, so assertions/contracts reject it. Assertions/contracts also reject insertion and owned-value `clone_get`; bind effectful results before asserting. An allocating text literal or key conversion still counts as allocation even when the lookup itself does not allocate.

Storage uses checked open addressing with hash lookup, growth and tombstone reuse. Capacity, allocation-size and probe bounds are checked in native and C builds; failure remains controlled in optimized builds. Hashing is deterministic and not randomized against adversarial inputs. Since 1.3, `keys(m)` lists the keys of `text`- and `int`-keyed maps (see the 1.3 section); otherwise the language exposes neither bucket order nor map iteration, capacity/reserve, custom hash/equality, generic type parameters or reference-returning entry APIs. `map` is reserved; `contains_key`, `insert`, `remove` and `clone_get` are contextual builtins, with existing user functions of the same name retaining precedence.

The [four-module symbol resolver](examples/symbol-resolver/README.md) reads a small source file, resolves lexical scopes and shadowing, and reports unknown/duplicate declarations. Integer scope maps own text symbol maps containing records, enum metadata and typed reference vectors. Its queries have checked effect/view contracts; stress tests exercise map growth, removal/reinsertion, deep copies and worker ownership. This is a compiler building block written in Anode; the compiler remains the Rust seed.

## Else-if and wildcard enum arms (0.21)

`else if` accepts a normal condition and block after an `else`, and can chain before a final `else`. Conditions run in order until one succeeds. It elaborates to nested checked `if` statements, retaining branch scopes, ownership merging and cleanup. No `if let`, match guards or new conditional binding form is introduced.

```text
enum Message { Ready(text), Pending, Closed }
let message=Message::Pending();
match &message {
    Message::Ready(payload)=>{print payload;}
    _=>{}
}
```

An enum match may end with one `_=>{...}` arm to cover every remaining variant. The wildcard must be last, has no payload binding, and is rejected when earlier explicit arms already cover every variant. Explicit duplicate arms and mismatched payload bindings remain errors. Omitting both a variant and a covering wildcard is still non-exhaustive. A wildcard is an enum arm, not a pattern for scalar values or structural destructuring.

The same syntax works in consuming, shared and mutable matches. A consuming wildcard destroys the complete unbound owned payload exactly once. Shared/mutable wildcards retain the whole-enum loan until the arm exits, even without a payload binding; they never take ownership or permit changing the source variant. Normal return/break/continue cleanup remains active. Wildcards do not permit payload access, references to escape, guards, or nested patterns.

## Native build reuse (0.20)

Native source semantics are unchanged. With `cache_mb=0` the compiler emits one application object and discards it after linking. An explicit positive cache budget enables native module objects in the existing shared LRU store, alongside C objects and baked executables. Missing modules compile in parallel under `ZN_JOBS` (1–16, default at most four).

Every invocation validates all source before accepting an output receipt or cached object. Keys cover checked bodies and signatures, conservative reachable helper bodies, used nominal layouts, generated worker adapters, native hotness, compiler identity, runtime and toolchain. Source-only coordinates are omitted unless they affect emitted code. Changes that influence inlining or layout invalidate their dependent units; unrelated existing layouts can remain cached. Recursion in the call graph is visited finitely. `--rebuild` bypasses both EXE and module reuse.

The shared object/baked-artifact budget excludes final EXEs and the fixed runtime/toolchain cache. Reducing the cap enforces eviction even on an unchanged build. Eviction/corruption cause regeneration; verified memory snapshots isolate concurrent native builders. There is no persistent semantic-proof cache, incremental type checker or remote-cache trust protocol in this release.

## Checked for loops (0.18)

`for index in start..end { ... }` visits an exclusive integer range. Both bounds must have type `int`; they are evaluated once, from left to right, before iteration. The loop binding is an immutable scalar copy, local to the body. Equal or descending bounds produce zero iterations. The last iteration below `int`'s maximum does not increment past that maximum. Range syntax is currently a loop header, not a first-class range value.

```text
pub struct Row { value:int }
pub vector Rows(Row);
fn main() {
    let mut rows=Rows();
    for i in 0..4 { push(rows,Row{value:i}); }
    for row in &mut rows {
        if row.value==1 { continue; }
        row.value=row.value+10;
    }
    let mut total=0;
    for row in &rows { total=total+row.value; }
    assert total==36;
}
```

`for item in &vector` borrows each element shared; `for item in &mut vector` borrows each element exclusively. The source must be a named nominal vector, record field, borrowed parameter or scoped vector alias. A mutable loop needs mutable/exclusive source access even when the vector is empty. The vector length is captured once. Each body uses the existing checked element-loan machinery, protecting the entire backing vector against movement, replacement, growth or removal; an exclusive iteration also blocks direct backing-vector reads. Disjoint sibling record fields remain available. Shared nested iteration is permitted; concurrent mutable element loans are conservatively rejected.

The item is an alias, not a cloned owner. Shared items permit scalar reads, shared method calls and explicit cloning. Mutable items also permit field edits, mutable method calls and whole-slot replacement; replacing an owned element destroys its prior contents. References cannot escape the iteration, be stored or returned, or cross a worker boundary. The vector becomes available again after the loop. Body-local owned values and worker handles retain normal cleanup on return, break and continue.

`break` and `continue` target the nearest enclosing `for` or `while`. The hidden counter advances before the body, so continue cannot skip advancement. Neither counter nor captured bounds appear in user navigation or typed-hole scope. Loop bindings cannot shadow an active local/parameter, following the language's existing rule; a binding can be reused after its loop ends. An outer owned value still cannot be consumed inside a repeatable loop, even when a branch breaks immediately.

Iteration itself allocates nothing. Bounds/body calls retain their ordinary transitive effects; explicit receiver views remain enforced. Both backends receive existing checked loop, loan, element and cleanup IR. Navigation, references and safe rename recognize range and element bindings, including their method and field accesses.

There is no implicit consuming iteration, iterator protocol/adapters, `for mut`, inclusive `..=`, custom step or reverse traversal in this release. Existing `ints`, `texts`, fixed arrays and text views can be indexed with an integer-range loop; direct borrowed element iteration currently requires a named typed vector. The words `for` and `in` are reserved.

## Inherent methods (0.17)

Methods belong to a named record, enum or vector declared in the same module. The first parameter is explicitly named and typed `self`; `Self` denotes that owner type within its `impl`. Methods are ordinary statically dispatched calls after elaboration, with the same ownership, field-view, effect, contract and worker checks as free functions.

```text
pub struct Counter { value:int, label:text }
impl Counter {
    @effect(no_alloc,no_io)
    pub fn read(self:&Self)->int { return self.value; }

    @effect(no_alloc,no_io)
    pub fn add(self:&mut Self,n:int) { self.value=self.value+n; }

    pub fn finish(self:Self)->text { return self.label; }
    pub fn incremented(mut self:Self)->Self {
        self.value=self.value+1;
        return self;
    }
}
fn main() {
    let mut counter=Counter{value:3,label:"counter"};
    counter.add(2);
    let n=counter.read();
    assert n==5;
    let same=Counter::read(&counter);
    assert same==5;
    print counter.finish();
}
```

- `self:&Self` borrows shared access; `self:&mut Self` requires a mutable place and borrows exclusive access. Dot calls supply that receiver borrow automatically. The receiver must be a named owner, record field, borrowed parameter or scoped alias; bind a returned temporary before borrowing it.
- `self:Self` consumes the receiver; `mut self:Self` additionally permits mutation of the owned parameter. Owned results support chains such as `make().incremented().finish()`. An owned receiver transferred to a worker uses the explicit form `spawn Counter::worker(counter)`.
- Explicit calls such as `Counter::read(&counter)` use the same checked implementation. `Self` also works in return types, record constructors, enum variants and explicit calls inside an implementation. Its keyword occurrences are not renamed with the owner type.
- A receiver can declare a field view, for example `self:&mut Self.{mut left}`. A disjoint sibling argument can then be borrowed in the same call. Whole-receiver loans still reject overlapping later arguments; no implicit two-phase borrowing is added.
- Private methods stay within their declaring module; `pub fn` exports a method under the existing public-signature rules. Multiple `impl` blocks are permitted; duplicate methods and method/enum-variant name collisions are rejected. Field access `value.name` and method call `value.name()` are distinct. Methods can share a short name with a builtin.
- Navigation, references and validated rename resolve both dot calls and explicit calls by nominal owner identity. Different modules can each define an `Item::read`. The required receiver name `self` cannot be renamed. `impl` is now reserved syntax.

This release requires typed `self`, rather than Rust's shorthand `&self` syntax. There are no receiver-less associated functions, foreign-type extensions, primitive-type methods, traits, generics, dynamic dispatch or borrowed return values. User-function calls in pre/postconditions retain the existing restrictions. No backend-specific method object or vtable is introduced.

## Borrowed enum matching (0.16)

`match &value { ... }` inspects an enum without consuming it or copying its payload. `match &mut value { ... }` permits in-place payload edits. Arm syntax is unchanged: a payload binding inherits shared or exclusive access from the match. The source must be a named enum, a record field, a borrowed parameter, or an existing scoped element/payload alias. Bind a returned enum before borrowing it.

```text
enum Outcome { Ready(text), Empty }
let mut outcome=Outcome::Ready("first");
match &outcome {
    Outcome::Ready(message)=>{print message;}
    Outcome::Empty=>{}
}
match &mut outcome {
    Outcome::Ready(message)=>{message="updated";}
    Outcome::Empty=>{}
}
```

The original 0.16 form covers every variant explicitly, with the correct payload binding; 0.21 also permits the final wildcard described above. The compiler checks the tag before forming a reference to its active payload. Shared payloads can be read, passed to shared parameters, or explicitly cloned. Mutable payloads also permit field edits, nested collection operations, and whole-payload assignment. Replacing a payload destroys the previous payload while preserving the enclosing variant.

The entire source enum is protected for the lexical match scope. Shared matching prevents its movement, replacement, or mutation; exclusive matching also blocks direct reads through the original enum. Mutating an unrelated sibling record field is allowed. Even a variant with no payload retains this protection. The source variant cannot be changed from inside one of its borrowed arms. Payload references cannot escape the arm, move out of an owning field, or cross a worker boundary. Scalar copies and explicitly cloned owners can escape. Returning, breaking, or continuing releases lexical access without destroying the borrowed payload; ordinary owner cleanup still runs.

Borrowed matches compose with vector element loans, nested enums, text regions and integer slices. Pure inspection and scalar edits can satisfy `no_alloc,no_io`; allocation in an arm still violates `no_alloc`. Field-view contracts account for the entire selected enum path. The native and C backends use typed internal pointers; aliases never become independent owners. `match value` retains its existing consuming behavior. There are no borrowed return types, partial-reference patterns, guards, implicit matching modes, or escaping lifetimes in this milestone.

The [four-module expression runner](examples/expression-runner/README.md) parses integer arithmetic into a typed vector of enum nodes, validates child indices, and evaluates them using borrowed matches. Navigation, references and safe rename infer payload binding types and show `&T` or `&mut T` appropriately.

## Scoped vector element borrows (0.15)

`borrow item = at(vector,index) { ... }` gives a shared view of an existing element for that block. `borrow mut item = at(vector,index) { ... }` gives exclusive access. Selecting an element bounds-checks once, creates an internal pointer, and does not allocate or copy the element. Its type is inferred from the nominal vector declaration. The receiver must be a named typed vector, record field, or borrowed parameter. Existing integer arrays still use `region` for scoped views.

```text
struct Entry { title:text, words:int }
vector Entries(Entry);
let mut entries=Entries();
push(entries,Entry{title:"Opening",words:120});
borrow entry=at(entries,0){print entry.title;}
borrow mut entry=at(entries,0){entry.words=150;}
```

While a shared element is borrowed, the entire backing vector is protected against mutation, removal, replacement, or movement. Multiple shared element borrows are allowed. An exclusive borrow also prevents direct reads through the backing vector. Work on disjoint sibling record fields remains allowed. The index expression is evaluated while the vector is protected, so it cannot grow or consume the vector while selecting an element. Different indices do not establish disjointness: two simultaneous mutable element loans of the same vector are rejected.

The local name behaves as a borrowed value. Scalar reads copy scalars; owned fields cannot move out. Use `clone(item)` or `clone(item.field)` when an independent owner is needed. Whole-element assignment inside a mutable borrow destroys the old value and installs the replacement in its existing slot; it does not rebind the pointer. Mutations of fields, nested vectors, and nested slice/text regions obey the same shared/exclusive rules as borrowed function parameters. A shared element cannot grant a mutable reborrow.

Borrow names are lexical and cannot be used after their block, stored as references, returned as references, or sent to workers. Returning a scalar copy or an explicitly cloned owner is valid. Early return, `break`, and `continue` preserve owner cleanup; aliases never destroy their backing element. Borrowed enums can be inspected with `match &item` or edited with `match &mut item` starting in 0.16. No escaping lifetimes, iterators, or independent mutable references to different indices are introduced.

Selecting and reading elements can satisfy `@effect(no_alloc,no_io)` and explicit record-field view contracts. Allocation in a nested operation still violates `no_alloc`. Bounds, arithmetic overflow, UTF-8 region boundaries, and worker ownership checks stay active on both backends. References, navigation, and safe rename include element bindings and fields inferred from them.

The [typed catalog](examples/typed-catalog/README.md) now totals word counts and edits titles without copying records. The [Anode-written lexer](examples/bootstrap-lexer/README.md) stores complete typed token records and inspects them with scoped borrows.

## Typed growable vectors (0.14)

Declare a nominal collection with `vector Name(Element);`, optionally `pub`. `Name()` creates an empty owned vector without allocating. Element types include integers, floats, bools, text, existing arrays, fixed integer buffers, records, enums, other vectors, and (since 0.21) maps. The finite type graph must be acyclic; even recursion through a vector is rejected in this version. Unit, tasks, and scoped borrowed views cannot be elements. Two differently named vector declarations remain distinct types even when they have the same element type.

```text
struct Entry { title:text, words:int }
vector Entries(Entry);
let mut entries=Entries();
push(entries,Entry{title:"Opening",words:120});
let copy=clone_at(entries,0);
replace(entries,0,Entry{title:"Revised",words:150});
let moved=pop(entries);
print copy.title;
print moved.words;
```

| Operation | Ownership and behavior |
| --- | --- |
| `len(v)`, `capacity(v)` | Number of initialized elements and allocated slots as `int`; neither allocates. |
| `push(v,value)` | Requires mutable, unborrowed access; consumes an owning value, copies a scalar, and grows storage when needed. |
| `at(v,index)` | Checked scalar access only. An owning element requires explicit `clone_at` or `pop`. |
| `clone_at(v,index)` | Checked independent deep copy; leaves the source element intact. Scalar copies allocate nothing. |
| `pop(v)` | Requires mutable access; transfers the final element without cloning. An empty vector fails safely. |
| `replace(v,index,value)` | Requires mutable access; bounds-checks, destroys the old element, and takes ownership of the replacement. Length is unchanged. |
| `clone(v)` | Independent deep copy of the entire vector and every owning element. |

Except for `len` and `clone`, vector operations require a named owner, a record field, or a borrowed parameter as their first argument. The receiver is protected while remaining arguments are evaluated: `push(v,clone_at(v,0))` is valid, but `push(v,pop(v))` is rejected as overlapping mutation. Shared borrows permit reads/copies; exclusive borrows permit mutation. Owned vectors can move to workers; borrowed arguments still cannot cross thread boundaries. Drops on normal exits, returns, loop exits, and match paths destroy all remaining owning elements and release storage. No element pointers escape.

Growth checks capacity multiplication and byte-size overflow before allocation. Negative and out-of-range indexing fails before memory access. The native and C backends share these checks. All operations are `no_io`; effect analysis conservatively treats vector mutation builtins and aggregate copying as potentially allocating. Contracts permit only non-allocating read operations. No vector reserve, insert/remove-at, generic type parameters or recursive containers are included. Scoped element borrowing was added in 0.15, iteration in 0.18, and separate nominal maps in 0.21 above.

The new names `capacity`, `replace`, `pop`, and `clone_at` are contextual builtins: an existing user function with that name in the current module takes precedence. Calls elsewhere can use that function's qualified name. This preserves older programs with helpers named `capacity` or `replace`. Rename still refuses creating a new builtin-name collision. `vector` itself is a reserved keyword.

The [three-module catalog](examples/typed-catalog/README.md) uses a vector of records containing owned text and tag collections. Editor references and safe rename cover vector declarations, element types, type annotations, constructors, and fields reached through `clone_at` or `pop`.

## Structured loop exits (0.12)

`break;` exits the nearest enclosing `while`. `continue;` ends the current iteration and evaluates that loop's condition again. Both work inside `if`, exhaustive `match`, and scoped `borrow` blocks. A `break` inside a match targets the loop, not the match. Nested loops have independent targets; labels, break values, `for`, and unconditional `loop` syntax are not implemented.

```text
let mut i=0;
while i<10 {
    i=i+1;
    if i%2==0 {continue;}
    if i>7 {break;}
    print i;
}
```

Each exit destroys owned locals from the scopes it leaves, in reverse binding order. This includes match payloads and unjoined worker handles; task destruction waits for completion. Owners outside the loop survive. Scoped slice loans end when their block is exited and cannot escape through either statement. Existing restrictions on moving an outer owner inside a repeatable loop still apply, even immediately before `break`; clone explicitly when needed.

Both keywords are reserved identifiers. They take no expression and require a semicolon. Use outside a loop is an error, including inside a called function that has no loop of its own. Statements after an unconditional exit are rejected as unreachable; a loop remains conservatively capable of executing zero times, even when its condition is literally `true`. A non-void function therefore still needs a return after a loop unless another enclosing construct guarantees return. Restoring a moved owner only inside a loop does not make it available after the loop; restore it unconditionally outside the loop.

Loop exits allocate nothing and introduce no I/O themselves. Cleanup can have effects (such as joining a task), and existing transitive effect/capability checks still inspect the checked body. Conditions, bounds, overflow and pre/postconditions retain their ordinary behavior. See `examples/config-reader` for cross-module parsing with match exits and nested search loops.

## Unicode text toolkit (0.11)

Text is valid UTF-8 without NUL. `len(text)` still counts **bytes**. Text positions below are byte offsets, so they compose with `slice(text,start,end)`. A Unicode scalar is a code point excluding surrogate values; a visible grapheme can contain multiple scalars. There is no implicit normalization, case folding or locale-sensitive matching.

| Operation | Result and boundary behavior |
| --- | --- |
| `char_count(text)` | `int` count of Unicode scalars, not bytes or graphemes. |
| `is_char_boundary(text,offset)` | `bool`; false for negatives, beyond the end or continuation bytes. The end is a valid boundary, including empty text. |
| `codepoint_at(text,offset)` | Scalar as `int`; offset must identify the start of a scalar **before** the end. Invalid offsets fail safely. |
| `next_char_boundary(text,offset)` | Byte offset after this scalar. Starting at the end returns the end. Other invalid offsets fail. |
| `codepoint_text(value)` | New owned text containing one scalar; rejects NUL, negatives, surrogate values and values above 0x10FFFF. |
| `text_find(text,needle,start)` | Absolute byte offset of first exact match at/after start, or -1. Start must be a valid boundary in range. Empty needle matches start. |
| `text_starts_with(text,prefix)` / `text_ends_with(text,suffix)` | Exact `bool` comparison; an empty prefix/suffix matches. |
| `text_trim(text)` | Owned copy with the whitespace set below removed from both ends. Interior text is unchanged. |
| `text_replace(text,needle,replacement)` | Owned copy replacing non-overlapping exact matches left-to-right. An empty needle inserts the replacement at every scalar boundary, including both ends. |
| `text_lines(text)` | Owned `texts`, splitting on LF and stripping a CR immediately before LF. No extra terminal empty line; empty input yields no lines. Interior empty lines and an unpaired final CR are preserved. |
| `text_join(parts,separator)` | Owned text joining `texts`. Empty collection yields empty text. Computes checked output size and allocates one output buffer. |

`text_trim` recognizes U+0009–000D, U+0020, U+0085, U+00A0, U+1680, U+2000–200A, U+2028–2029, U+202F, U+205F and U+3000. It preserves U+200B and U+FEFF. Line splitting deliberately uses LF/CRLF, not every whitespace character.

Traversal/search/prefix/suffix/boundary queries allocate nothing and are allowed in contracts and `@effect(no_alloc,no_io)`. Construction/trim/replace/lines/join allocate and are excluded from contracts and `no_alloc`, including when a particular input could reuse text. All new operations are `no_io`. Arguments are borrowed for the call; transformations never change or consume their input owners. New text owns independent storage and can move into a worker. No character pointers or escaping references are exposed.

```text
let text = "café 🦜";
let mut offset = 0;
while offset < len(text) {
    print codepoint_at(text, offset);
    offset = next_char_boundary(text, offset);
}
let parts = text_lines("first\nsecond\n");
print text_join(parts, " / ");
```

Traversal/count/trim/lines/join are linear in their input/output size. Exact search uses a byte scan plus candidate comparison; worst-case search/replacement can be quadratic in repetitive text and needle length. Replacement precomputes checked output size and fills one buffer. There is no regex engine, grapheme segmentation, Unicode identifier expansion or general text-view type in this release.

## Floating point (0.10)

`f32` and `f64` are distinct copyable scalar types, stored in 4 and 8 bytes respectively. Records, enums, function arguments/returns and owned worker packets support both. Typed vectors can hold either float type starting in 0.14; fixed buffers and slices still require integer elements.

Decimal literals such as `1.5` and scientific literals such as `2e3` or `1.25e-2` have type `f64`. A decimal point in source must have digits on both sides. Non-finite literals are rejected; use computed infinity/NaN if required. Unsuffixed integer literals remain `int`. `f32(value)` / `f64(value)` explicitly convert any integer or float; arithmetic does not silently mix widths or integer/float types.

```text
let x=f32(0.1);
let y=1.5e2;
print f64(x)*y;
print sqrt(2.5);
let infinity=1.0/0.0;
assert !is_finite(infinity);
```

`+ - * / %`, unary minus, and comparisons operate at the declared floating width. `%` is truncating remainder, with the dividend's sign, rather than Euclidean modulo. Operations use ordinary IEEE binary rounding without fast-math reassociation or implicit fused multiply-add. Overflow may yield infinity; invalid arithmetic may yield NaN; underflow may yield a subnormal or zero. Floating division by zero does not use the integer fail-fast rule. Signed zero is preserved. NaN is unequal to itself, and ordered comparisons involving NaN are false. No stable NaN payload is promised.

`sqrt(x)` and `abs(x)` preserve width. `is_finite(x)` and `is_nan(x)` return bool. Explicit integer casts, including `int(x)`, truncate toward zero and then check range. NaN, infinity and out-of-range results fail before any C integer conversion; no saturation or wrapping occurs. Thus `u8(-0.5)` is zero, but `u8(-1.0)` fails. Integer-to-float conversions and f64-to-f32 narrowing may round; narrowing overflow may produce infinity.

`print value` and `float_text(value)` use up to 9 significant digits for f32 or 17 for f64, sufficient for round-trip conversion; infinity/NaN display as `inf`, `-inf`, `nan`. This is not decimal-place formatting. `float_text` returns owned allocated text.

`parse_f64(text)` accepts a finite decimal value with optional sign, decimal point and exponent; surrounding whitespace, hex floats, NaN/infinity spellings and trailing junk are rejected. Text forms `.5` and `1.` are accepted. Overflow fails; underflow to zero is accepted. `is_f64(text)` checks the same grammar and finite range without failing. Parsing uses the runtime's C-locale decimal converter. Neither parsing builtin is promised allocation-free, so both are excluded from `no_alloc` and assertion/contract expressions; compute a local result first. They satisfy `no_io`. Arithmetic, casts and the four numeric helpers above satisfy `no_alloc,no_io` and can appear directly in assertions/contracts.

## Scoped integer slices (0.9)

```text
@effect(no_alloc,no_io)
fn first(bytes:&[u8])->u8 { return at(bytes,0); }
fn main() {
    let mut packet=[u8(0);32];
    borrow mut payload=region(packet,4,28) {
        set(payload,0,u8(7));
        borrow header=region(payload,0,4) { print first(&header); }
    }
    // The slice is gone; the owner is available again.
    print at(packet,4);
}
```

`region(owner,start,end)` is allowed only in a `borrow [mut] name = region(...) { ... }` statement. Its owner must be a live named fixed integer array, dynamic `ints`, or another slice, including a record field. Bounds are `int`, half-open, and checked before pointer arithmetic: `0 <= start <= end <= len(owner)`. Empty slices are valid. The owner is reserved while the bounds expressions run, so those expressions cannot move or mutate it.

`[T]` is a nonowning integer slice. It can appear in borrowed parameters (`&[T]`, `&mut [T]`) and is inferred for a scoped binding. It cannot be returned, stored in a record/enum/collection, copied, cloned, reassigned or sent to a worker. It lasts through its lexical block, including nested calls; no borrowed value escapes. Move the owned buffer into a worker and create its slice there instead.

A shared slice permits other reads, but prohibits moves, replacement, growth and writes of overlapping backing storage. An exclusive slice prohibits direct reads too. Reborrowing through the slice is allowed; its parent is similarly restricted until the child block ends. Disjoint record fields remain usable and field-view contracts include slice creation/access. Two mutable regions of the same buffer are rejected even if their numeric ranges do not overlap: this release locks the backing place, not individual element intervals.

`len`, `at`, `set`, and `try_set` operate relative to the slice. `read_u16/u32/u64_le` and `try_write_u16/u32/u64_le` also accept u8 slices. Writes require an exclusive slice; failed `try_*` operations leave storage unchanged. Slices cannot grow. Creating and using one allocates no heap memory: the descriptor is one pointer and one length (16 bytes on x64). `@effect(no_alloc,no_io)` remains checked. General anchored/stored lifetimes, text slices and collections of references are later work.

## Fixed-width integers and packet buffers (0.8)

Signed `i8`, `i16`, `i32`, `i64` and unsigned `u8`, `u16`, `u32`, `u64` are distinct integer types. `int` is an alias for `i64`. Unsuffixed decimal literals remain `int`; use explicit checked conversions such as `u8(255)`, `i16(-123)`, or `int(value)`. There are no implicit narrowing conversions or mixed-width arithmetic. Conversion outside the destination range fails at runtime. Unsuffixed literals retain their signed 64-bit limit; larger u64 values can be read from a buffer or constructed with checked u64 arithmetic.

`+ - * / %` preserve operand type and check overflow and division by zero. Unary negation requires a signed type and checks its minimum value. Signed minimum divided or reduced modulo -1 fails. Comparisons use signed or unsigned ordering according to the operand type. `saturating_add(a,b)` and `saturating_sub(a,b)` deliberately clamp to the type's range. Printing preserves the full signed/unsigned range.

```text
let mut bytes = [u8(0); 32];
let written = try_write_u32_le(bytes, 3, u32(4294967295));
assert written;
assert read_u32_le(bytes, 3) == u32(4294967295);
let rejected = try_write_u64_le(bytes, 29, u64(7));
assert !rejected; // buffer unchanged
```

`[T; N]` is an inline fixed-capacity array of an integer type. `N` is a literal from 1 to 65536, with total array storage capped at 64 KiB. `[value; N]` evaluates the value once and fills all elements, without heap allocation. These arrays are affine owners: assignment/arguments/returns move, `clone` explicitly copies, and whole-array `&`/`&mut` parameters follow existing loan rules. Arrays may be record fields or enum payloads and can move into workers. Scoped slices are described above. Element references, nested fixed arrays and zero-length fixed arrays are not implemented.

- `len(array)` returns its capacity as `int`.
- `at(array,index)` returns an element; `set(array,index,value)` changes it. Negative/out-of-range indexes fail. Values must exactly match the element type.
- `try_set(array,index,value)` returns false on an invalid index without modifying storage. `push` is unavailable for fixed arrays.
- `read_u16_le`, `read_u32_le`, `read_u64_le` accept a fixed **u8** array and an `int` byte offset, returning the indicated unsigned type. They fail if the complete value does not fit.
- `try_write_u16_le`, `try_write_u32_le`, `try_write_u64_le` require a mutable fixed u8 array, an int byte offset, and the corresponding unsigned value. They validate the entire span before writing and return false on failure. Encoding is explicitly little endian; it is independent of record padding.

These operations, conversions, saturation, and fixed-array cloning are permitted inside `@effect(no_alloc,no_io)`. Thread startup and printing are not. Named-record cloning remains conservatively classified as potentially allocating. Unchecked integer arithmetic is not part of this release; floating arithmetic follows the rules above.

## Programs

A program is one or more UTF-8 `.an` modules containing named functions. A project uses `Anode.toml`; `.zn` and `Zenith.toml` remain supported. Children of a module use the same source extension as their parent. If both manifests exist, name the manifest explicitly. Exactly one root `fn main()` must have no arguments and no return value. Comments begin with `//`. Statements end with semicolons. Names use ASCII letters, digits and underscores; a name cannot start with a digit. Shadowing a live local or parameter is rejected.

## Additions in 0.7

### Owned text collections

`texts` holds owned UTF-8 strings. `["one", "two"]` infers `texts`; `texts()` creates an empty collection. `[]` still means `ints`. `push(collection, text)` requires a mutable collection and **moves** its text argument. `text_at(collection, index)` returns an **allocated copy**; it does not return a stored reference. `len`, deep `clone`, `&texts`, `&mut texts`, record fields, enums and transfer to workers are supported. `split(text, separator)` returns a text collection, preserving empty fields. An empty separator is a checked runtime error. Indexing, growth overflow and ownership checks remain active.

### Recoverable file results

`try_read_text(path)` returns the built-in `ReadResult` enum: `ReadResult::Ok(text)` or `ReadResult::Err(text)`. `try_write_text(path, content)` returns `WriteResult::Ok` or `WriteResult::Err(text)`. Match all variants to handle failures. Both still require the matching project capability. Existing `read_text`/`write_text` retain their fail-fast behavior. Results report path/open/read/write/UTF-8/size errors; out-of-memory and violated language safety checks still terminate. File writes are not atomic transactions.

### Function annotations and typed holes

`@effect(no_alloc, no_io)` before `fn`/`pub fn` is a checked, transitive contract. `no_alloc` disallows operations that may allocate, even if a particular execution would not (e.g. `push` with spare capacity). `no_io` disallows file, console, argument and thread operations. Both analyze all reachable syntactic branches and recursive calls conservatively. They describe ordinary function execution, excluding process startup and fatal runtime error reporting. `no_alloc` does not mean no deallocation. `requires`, `ensures` and `assert` remain checked runtime conditions, not general proofs.

`@intent("description")` preserves human/agent guidance for context queries. Natural-language intent has no compiler-enforced meaning unless separately expressed through an effect or executable contract/test.

`??(int, "count positive values")` parses as a typed hole. Its type is **explicitly declared**, not inferred from downstream use. Checking emits structured scope/type/move/borrow/effect information at the first hole and fails; compilation cannot proceed. Filling it still requires passing normal checks. Bare `??("description")` and automatic synthesis are not implemented.

```text
fn function_name(parameter: int, mut collection: ints) -> int {
    let fixed = 12;
    let mut changing = 0;
    changing = changing + 1;
    return fixed + changing;
}
```

Omitting `-> TYPE` means a function returns no value. Non-void functions must return on every statically checked path. Functions may call later functions; recursion is supported but resource usage and termination are not proven.

## Modules, projects and tests

```text
// src/main.zn
mod stats;
fn main() {
    let values = [12, -5, 9];
    print stats::minimum(&values);
}

// src/stats.zn
pub fn minimum(values: &ints) -> int {
    let mut smallest = at(values, 0);
    let mut i = 1;
    while i < len(values) {
        if at(values, i) < smallest { smallest = at(values, i); }
        i = i + 1;
    }
    return smallest;
}

test finds_negative_minimum() {
    let values = [12, -5, 9];
    let answer = minimum(&values);
    assert answer == -5;
}
```

`mod stats;` in the root loads `src/stats.zn`. `mod order;` in that file loads `src/stats/order.zn`. Cross-module calls require a `pub fn`; functions are private otherwise. Qualified names are relative to the current module; `crate::stats::minimum` starts from the root. Modules are resolved before ownership checking, including calls and worker arguments across files. Diagnostics retain original source paths and line numbers.

`test name() { ... }` declares a private, no-argument, no-result test. `zn test PROJECT` compiles a native runner and executes all declared tests instead of the user's main function. Tests cannot be called or spawned by ordinary code. A failed assertion terminates the runner with an error. Native tests are not sandboxed.

`zn new DIRECTORY` creates a new project and refuses to overwrite an existing directory. The manifest supports `[package]` name/version, `[build]` entry/cache_mb and `[capabilities]` file_read/file_write/process_args/threads. It is a strict small TOML subset, not Cargo: no dependencies, build scripts or downloads. Entry paths must remain inside the project and use forward slashes. Cache budgets are integers from 0 to 4096 MiB; zero disables retained application objects and baked executables. --cache-mb overrides the manifest for that command. C modules, native program objects and optimized executables share one global LRU cap.

An explicit `[capabilities]` section denies omitted permissions and checks all loaded function bodies, including unused/imported functions. New projects explicitly disable all four permissions. Legacy manifests and standalone sources with no capabilities section keep their existing access. Console output is always available. These permissions govern supported language operations; they are not path-scoped or an OS sandbox.

## Values

| Type | Literal | Ownership |
| --- | --- | --- |
| `int` | `42`, `-3` | Copy |
| `bool` | `true`, `false` | Copy |
| `text` | `"Hello"` | Move |
| `ints` | `[1, 2, 3]`, `[]` | Move |
| `texts` | `["one", "two"]`, `texts()` | Move; owns its elements |
| task | `spawn work(argument)` | Move; inferred, not a user annotation |

Text stores UTF-8; `len(text)` counts bytes. Text escapes are `\n`, `\r`, `\t`, `\"`, `\\`. Embedded NUL is rejected. Integer literals must fit positive signed 64-bit range before unary negation; obtain the minimum integer with `-9223372036854775807 - 1`.

## Expressions and control flow

Integer operators: `+ - * / %`, comparisons `< <= > >= == !=`. Boolean operators: `! && || == !=`. `&&` and `||` short-circuit and their conditional operand cannot consume an owned value. Integer division truncates toward zero. Operands and call arguments evaluate left to right. Use `text_eq(a, b)` for text equality; the `==` operator does not accept text.

```text
if condition {
    print "yes";
} else if another_condition {
    print "another";
} else {
    print "no";
}

while condition {
    // break; and continue; target this loop
}

assert condition;
return value;
```

`print` takes an int, bool or text and emits a newline. `assert` accepts the same pure expressions as contracts. Parentheses group expressions.

## Built-ins

| Call | Behavior |
| --- | --- |
| `len(text_or_collection)` | Borrow and return byte/element count |
| `at(ints, index)` | Borrow and return an element with bounds check |
| `push(mutable_collection, value)` | Append integer, or move text into texts |
| `set(mutable_ints, index, value)` | Replace an element with bounds check |
| `clone(owner)` | Deep-copy text, ints, texts, or a record/enum |
| `text_at(texts, index)` | Bounds-check and allocate an independent text copy |
| `split(text, separator)` | Allocate texts, including empty fields; nonempty separator required |
| `concat(text, text)` | Borrow both and allocate combined text |
| `text_eq(text, text)` | Borrow and compare exact UTF-8 bytes |
| `contains(text, needle)` | Borrow and test for a substring |
| `slice(text, start, end)` | Return owned copy of half-open byte range; bounds and UTF-8 boundaries checked |
| `byte_at(text, index)` | Return unsigned byte as int; index checked |
| `is_int(text)` | True for a representable signed 64-bit decimal; optional sign, no surrounding whitespace |
| `parse_int(text)` | Parse that decimal or terminate with an error |
| `int_text(int)` | Allocate signed decimal text |
| `args_count()` | Number of user arguments, excluding the executable; requires process_args |
| `arg(index)` | Owned UTF-8 argument; index zero is the first user argument; requires process_args |
| `read_text(path)` | Read an owned UTF-8 file up to 64 MiB; reject NUL/invalid UTF-8; requires file_read |
| `write_text(path, text)` | Write/overwrite a UTF-8 file; requires file_write |
| `spawn function(arguments)` | Transfer arguments and execute a typed-result function on a native thread |
| `join(task)` | Consume handle, wait, and transfer its scalar or owned result |

User functions also support non-escaping borrowed parameters:

```text
fn count(values: &ints) -> int { return len(values); }
fn append(values: &mut ints) { push(values, 7); }
fn main() {
    let mut values = [1, 2];
    append(&mut values);
    print count(&values);
    print len(values); // ownership remains here
}
```

Borrowed parameter types include `&ints`, `&texts`, `&text`, `&mut ints`, `&mut texts`, and borrowed records/enums. Borrow arguments must be explicit and name a live binding or record-field projection. Shared references can coexist; an exclusive reference requires a mutable owner (or an exclusive parameter) and excludes other accesses to overlapping fields until the call completes. Calls evaluate left-to-right. A scalar value computed before taking an exclusive borrow can be used, but reading an overlapping owner in a later argument while exclusively borrowed is rejected.

References cannot be ordinary local values, function returns, array elements or thread arguments. Scoped `borrow` slice bindings are the explicit local exception described above. Reborrowing inside a function is allowed, but a shared parameter cannot become exclusive. Moving/replacing a borrowed owner is forbidden; `clone` creates an independent owned value, and is not available on slices. Mutation of an exclusive array parameter uses `push(values, ...)` / `set(values, ...)`; reborrowing for a user call uses `function(&mut values)`.

These restrictions avoid escaping lifetime inference. They are considerably narrower than Rust's reference system. All owned locals/parameters drop in reverse declaration order; borrowed parameters are never dropped by the borrower.

## Contracts

Put zero or more `requires EXPRESSION;` and `ensures EXPRESSION;` clauses between a function signature and its body. Each must evaluate to bool. Preconditions run once at entry. Postconditions run on every explicit/implicit return; `result` refers to the return value. Only parameters and result are in scope for postconditions; function-local declarations are rejected. Contracts are checked at runtime in all build modes.

Pure text inspection (`text_eq`, `contains`, `byte_at`, `is_int`, `parse_int`) is allowed in assertions/contracts along with scalar operations and `len`/`at`. Allocating text operations and I/O are not. Store a value in a local first if an assertion needs the result of an effectful operation.

Text built-ins borrow their arguments during evaluation; their owned results must follow ordinary move rules. Windows file paths and arguments support Unicode. I/O errors and invalid parsing/slicing terminate with a controlled runtime error; recoverable result types are not implemented yet. File writes are not an atomic transaction.

## Build output

`check` parses, checks types, ownership and borrow conflicts. `build` normally emits native COFF through Cranelift and links an executable, keeping intermediate files private. Explicit `--backend c` uses MSVC `/Od` and shared checked arithmetic. `--release` uses `/O2 /Ob1`: repeated code can inline checked operations, while cold code calls shared checked helpers to limit binary size and compilation work. The runtime object is optimized independently. No arithmetic or bounds-check-free mode exists. `--sanitize` instruments both program and runtime with MSVC AddressSanitizer; `run` locates the installed sanitizer DLL without copying it into application output.

`emit-c FILE -o FILE.c` exports checked program C for inspection. That C needs the matching runtime object supplied by the backend; it is not a standalone source export of the runtime. Emitted C may be manually modified, but such edits bypass Zenith's checks.

`run PROJECT -- arg1 arg2` passes arguments to the executable. `build`, `check`, `test`, `emit-c`, `review` and `review-prompt` accept project directories or `.zn` files. Omit the project inside a directory with `Zenith.toml`. Project output names are stable under `.zenith/build/`; `--rebuild` bypasses executable/module reuse. Even an unchanged-output build checks source ownership/types/capabilities first. `storage` reports artifacts; `clean` removes known managed outputs and `clean --cache` also evicts shared module objects.

Compiler input is limited to 16 MiB per source file and 256 MiB total (1 MiB and 8 MiB before 1.3), with bounded nesting/expression depth, module depth and module count. These are explicit limits rather than resource-safety guarantees for arbitrary compilation workloads.

## Owned records and tagged enums (0.5)

    struct Parcel { label:text, values:ints, priority:int }
    enum Delivery { Ready(Parcel), Failed(text), Empty }

Declarations may be public with pub struct or pub enum. Data-type names begin with an uppercase ASCII letter. Cross-module names use module::Type, including variant constructors such as model::Delivery::Empty().

Record construction supplies every field once, in any written order; initializer expressions evaluate in that written order. Fields are accessible through a named owner (bind a returned record before accessing its fields).

    let mut p=Parcel{label:"north",values:[4,8],priority:2};
    let label=p.label;       // move one field
    print len(p.values);    // its sibling remains usable
    p.label="renamed";       // restore the moved field
    let duplicate=clone(p); // explicit deep clone of owned fields

Records/enums always move as whole values, even when all their fields are copy scalars. Reading or borrowing an owner with any outstanding partial move is rejected. Mutating a field requires a mutable owned binding or an exclusive parameter. Nested field projections are supported. Field borrows use overlapping-path checks. Private borrowed parameter footprints are inferred transitively; public contracts require explicit views to narrow whole-owner access.

Enums have one optional payload per variant. Unit variants are constructed with empty parentheses. Exhaustive match consumes its enum; explicit payload arms require a binding and explicit unit arms do not. A final wildcard may cover unbound variants since 0.21.

    match answer {
        Delivery::Ready(parcel)=>{print parcel.label;}
        Delivery::Failed(message)=>{print message;}
        Delivery::Empty=>{}
    }

Explicit variants appear at most once; every variant must be covered explicitly or by a final wildcard. All continuing arms participate in moved-state merging; an arm that returns does not poison the continuing paths. A bound payload is cleaned up at the end of its arm, or transferred by an owned return; a consuming wildcard destroys an unbound payload. Nested matches and 0.21 wildcards are supported. There are no guards, destructuring patterns or match expressions yet.

Functions may accept &Parcel, &mut Parcel, &Delivery or &mut Delivery. References remain non-escaping whole-owner call arguments. Generic Option/Result types do not exist yet: use a named enum for recoverable domain results. Built-in read_text/write_text failures still terminate instead of returning an enum.

Finite layout limits: at most 256 data declarations and 256 fields/variants per declaration, no recursive by-value layouts, dependency depth at most 64, and a 64 KiB field/payload layout limit (an enum tag/alignment can add a few bytes). Stored references, generic type parameters and user-defined heap indirection are not implemented. Nominal vectors and maps can own records under these finite-layout rules.

## Backend selection and checked IR (0.5)

The default build/run backend is direct Cranelift native compilation. --release selects optimized C/MSVC; --backend fast or --backend c overrides selection. --sanitize selects C/MSVC unless an explicit incompatible fast request is supplied, in which case compilation reports an error.

Both backends consume the same ownership-checked typed IR. The emit-ir command prints it or saves it with -o. It is an inspection format, not a stable serialized compiler API. There is no retained IR cache.

The direct backend uses the same runtime ownership helpers and checks, plus inline checked hot arithmetic/indexing. It emits Windows stack probes and .pdata/.xdata unwind records. With cache_mb enabled it retains dependency-keyed native module objects in the same bounded store as C modules and baked optimized executables. Both backends can reuse validated unchanged EXEs after rechecking source.

## 0.6 field views

Borrow a nested place with &value.field or &mut value.nested.field. Two places overlap when they share a local owner and either field path is a prefix of the other. Disjoint sibling fields may be read, moved, or borrowed independently. A whole-owner move still conflicts with any live field loan.

Explicit parameter contracts use a borrowed record type followed by a field set:

    pub fn update(s:&mut State.{mut samples,config.limit}) { ... }
    pub fn inspect(s:&State.{config.limit,label}) { ... }

A plain field grants shared reads; mut grants reads and mutation. Only &mut parameters may grant mutation. Empty, duplicate, overlapping, nonexistent and non-record paths are rejected. Enum payload projections are not supported as view fields. Views are not values, new aggregate layouts, or stored references.

Unannotated private parameters infer their footprints transitively, starting from a conservative whole-owner footprint. Read-only use can refine an exclusive parameter's effective footprint, but the explicit &mut call still requires a mutable owner. Unused borrowed parameters stay conservative. Recursive call cycles may retain whole-owner effects. Public unannotated parameters keep whole-owner access, regardless of their present implementation. An explicit view, public or private, is a checked upper bound: the body, helper calls, preconditions and postconditions must stay inside it.

A projected view may borrow the live fields of a partially moved record; it may not access a moved field or a whole partially moved parent. Function return, stored-reference and thread restrictions remain unchanged. No view grants concurrent shared mutation.

## 0.6 bounded artifacts and background optimization

cache_mb (0..4096, default 0) and --cache-mb N select the maximum retained shared artifact bytes. Native program objects, C module objects and optimized baked EXEs share one store and LRU eviction policy. The most recent command using that store enforces its selected cap; there is no permanent allocation per project. Runtime/toolchain files and named final outputs are reported separately.

zn bake PROJECT --background launches a hidden, lower-priority, one-shot Rust worker; foreground zn bake FILE.zn is also available. Baking needs a nonzero cache budget. Each managed project has one bake lock and one overwritten bake-status.txt. A crash releases the process lock; a stale RUNNING line is not proof of a live worker, so baking can be requested again. There is no installed daemon or file watcher.

The worker checks an in-memory source snapshot and compiles it through the optimized C backend. Artifacts are keyed by checked IR (including field contracts), compiler identity, runtime, and native toolchain. It never overwrites a project executable. A source edit during baking may leave an older artifact in the bounded store; it cannot be selected for different checked inputs.

zn build/run PROJECT --baked rechecks source and retrieves the matching optimized artifact, or uses fast compilation on a miss. It does not wait for background optimization. --rebuild bypasses both artifact and output reuse. --baked cannot be combined with --backend, --release or --sanitize; those choose their own explicit paths. Baking never turns off bounds/overflow checks or language contracts. Baked EXEs are not ASan-instrumented.

Caching reuses dependency-keyed direct-native module objects, C modules when the C cache is selected, and one baked EXE per matching checked program. No incremental semantic checker, background PGO, or running-process code replacement is present.


## 0.13 borrowed UTF-8 text views

`text` owns its storage. `str` is an immutable, length-delimited view into a live text owner or another view:

```text
@effect(no_alloc,no_io)
fn number(value:&str)->int { return parse_int(value); }
fn main() {
    let source="123 trailing text";
    borrow digits=region(source,0,3) {
        print number(&digits);
        let independent=to_text(digits);
        print independent;
    }
}
```

`region` uses half-open UTF-8 byte offsets. Bounds and both character boundaries are checked in every backend, including optimized builds. It creates a pointer/length descriptor without copying bytes or allocating. Nested regions are relative to their immediate view. Empty and end-of-source views are allowed. A view has no implicit NUL terminator; all supported operations respect its length.

Views exist only in scoped `borrow` blocks and shared `&str` parameters. They cannot be moved, cloned, returned, stored in records/enums/collections, assigned or sent to a worker. `&mut str` and mutable text regions are rejected. The source cannot be moved/replaced while its view is live. Disjoint record fields remain usable. Scope exit, return, break and continue release the loan without freeing source storage. A worker may own a `text` and create views locally. These are lexical loans, not Rust's general lifetime inference or escaping references.

Explicit `to_text(view)` makes an independent, NUL-terminated owning copy and is an allocating operation. It also accepts `text`. There is no implicit conversion between user function parameters `&text` and `&str`: create a region or an owning copy explicitly.

Both `text` and `str` work with `len`, `print`, `byte_at`, `char_count`, `is_char_boundary`, `codepoint_at`, `next_char_boundary`, `text_eq`, `contains`, `text_find`, `text_starts_with`, `text_ends_with`, `is_int`, `parse_int`, `concat`, `slice`, `text_trim`, `text_replace`, `text_lines` and the separator argument of `text_join`. Constructors still allocate owned results; pure queries retain their no-allocation/no-I/O effects. Printing is I/O.

For this release, `clone`, `split`, `parse_f64`, `is_f64`, and file path/data APIs still require owned `text` because those runtime routes assume NUL termination. Use `to_text` explicitly first. Float parsing and typed growable collections are not changed by this milestone.

`examples/log-scanner` demonstrates three modules with Unicode labels and signed integer fields, recoverable malformed rows, CRLF/EOF handling, and compiler-checked `no_alloc` scanning over one file buffer. File reading and output still allocate/use I/O.
