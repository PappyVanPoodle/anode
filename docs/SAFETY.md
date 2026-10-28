# Anode 1.2 safety boundary

## 1.2: new authority, proof-gated check removal

The 1.1 rules (V1100-PARALLEL.md M1–M9) still hold, and V1200-PARALLEL.md N1–N12 adds to them. Every unproven case keeps the 1.1 check, message and fault order. Benchmark workloads and drivers are byte-identical to 1.1. Programs that use no 1.2 feature produce byte-identical native code (18 of 18 CLIF and object hashes) and the same C apart from the runtime header text.

**Network authority.**
- `net_request` is behind the opt-in `net` capability, which is never granted by default: not by `anode new`, not to a single file, not to a root without `[capabilities]`, and not to the dependencies of such a root.
- A dependency's grant is limited to its parent's, and the transitive check keeps the existing "required transitively" message.
- The runtime accepts only `loopback://` routes, which are served in memory, and the literal host `127.0.0.1`. Every other host, `localhost` included, is rejected by the endpoint parser before any socket exists.
- HTTP paths must be printable ASCII without spaces, so a path cannot inject headers.
- Responses are bounded (16 MiB, UTF-8, no NUL) and time-limited (5,000 ms for the whole exchange). Every failure is an `Error` value, never a fault or a hang.
- The Windows socket library is loaded only on the first socket request, from System32, and is never a static import.

**Effects.**
- `net_request` is I/O, allocating and never `bounded`.
- `sha256_text` allocates.
- The big-endian writes and `fnv1a_64` allocate nothing and perform no I/O.
- These classifications propagate through `@effect` like every other builtin's.

**Borrowing.** The big-endian writes, under both spellings, follow exactly the `_le` rules: the same code path for ownership, liveness, mutability and loans, and the same view-summary entry, so a callee's write is seen as a write. A dedicated test compares 18 rule templates × 3 widths with `_le`.

**Removed checks.**
- **Proven big-endian store (native and C).** The bounds test of `try_write_u*_be` on a `[u8; N]` is dropped only when a dominating int-range or `Flow` fact proves `0 <= offset` and `offset + width <= N`. This is the proof that already gates the inline `_le` and `read_u*_be` paths. Unproven offsets and every slice keep the checked helper.
- **Self-calls in a checked-once function (C).** A function whose vector-view parameters passed M5's entry test calls itself with the cached data and length, skipping the callee's entry test. The views are pinned (no checked function can replace a borrowed view parameter), so the callee would receive the same descriptor and pass the same test. The checked fallback stays byte-identical to 1.1.0's emission.
- **Local region views (C).** A local `VectorView` made by `region(...)` and never replaced in the rest of its block is tested once with M5's invariant test. Accesses later in the block use M5's single compare. When `Flow` proves the index below the region count's lower bound, there is no compare at all. That relies on runtime postcondition R-N8: `zn_vector_view_region` either faults or sets `len == count`. Root checked this in src/runtime.c.
- **Runtime faults reachable only from hand-written C:**
  - "invalid packet width" and "packet storage overflow" (`zn_wire_write_be`);
  - "invalid hash input storage" (a NULL pointer with a length, or an unmasked static-text length, passed to a hash).

**Tests.**
- Every new builtin runs with identical output on native, C/LTCG and C-ASan.
- SHA-256 is checked against the NIST vectors and every input length from 0 to 130, compared with two independent implementations.
- `zn_wire_write_be` is checked over 296 stores and 811 rejections; every rejection leaves the buffer untouched.
- 63 endpoint rejections connect to nothing; a live listener confirms it.
- The 49 malformed-view cases still pass.

## 1.1 proof-gated check removal

The rules are the same as 1.0's (V1100-PARALLEL.md, M1–M9). Every removed check is proved by one of four things:
- a compiler invariant;
- a dominating guard;
- an executed `requires`/`assert`;
- a runtime invariant documented in the contract.

Every unproven case keeps the 1.0 check, message and fault order. Benchmark workloads and drivers are byte-identical to 1.0, and no code is keyed to benchmark names or constants.

**Constant-mask ranges (M6).** Only `bit_and` with a compile-time constant on an unsigned width gains a range, `[0, K]`. A conversion to `int` is discharged only when `K <= i64::MAX`. On native code the bound is flow-insensitive: it holds for every definition of the value.

**Native SoA rows and drops (M3).** A push writes row `len-1` of each column at the documented layout offset. This relies on runtime invariant R1: SoA storage is allocated only by `zn_soa_push_index`/`zn_soa_clone_storage`, after the validated layout computation, for 1..=256 columns of nonzero width. Storage is released directly only when the element type owns nothing; owning elements keep the per-element drop.

**Inline float conversions (M3).** A C harness checked inline `int`/`u64`→float and `f32`↔`f64` bit-for-bit against `zn_fp_convert`: 15,860 conversions, 654 of them NaN, with no mismatch. `float`→`int` stays on the runtime with its exact range and NaN faults.

**Native overflow versioning (M4).** The rule B shipped for C in 1.0 now applies to native code too, with two extra restrictions that only keep more checks:
- the loop body may make no calls;
- the bounded interval must stay within ±2^62.

The single pre-loop guard proves every guarded result fits in i64. When it fails, the original checked loop runs and faults at the same iteration with the same message.

**C vector-view versioning (M5).** The entry test is exactly the runtime invariant `len <= INT64_MAX/width && (len == 0 || data != NULL)`. It applies only to view parameters the function cannot replace. Any view that fails it runs the unchanged 1.0 checked body, which the dedicated test compares byte for byte with the 1.0.0 compiler's emission. The 49 malformed-view cases and 20 pair and boundary cases pass on native, C and C-ASan, both unversioned and versioned. Inline pair checks keep the replaced runtime call's messages and order.

**Runtime (M7).** runtime.h is byte-identical to 1.0, and the SoA layout is unchanged byte for byte. The one-pass column computation rejects exactly what 1.0 rejected, with the same first fault: 48 malformed layouts were compared with the 1.0 helpers on normal and ASan builds.

One stricter behavior: cloning an `int` or text array whose descriptor is corrupted to an impossible length now fails with "array too large" before reading. 1.0 would have read past the end. Only C code that forges descriptors can reach this; no Anode program can.

**Soundness fixes carried from 1.0** remain in force: `zn_mod`'s bias/mask form for power-of-two divisors (an MSVC 14.44 `/O2` miscompile) and native loops that rewrite their own bound keep their checks.


## 1.0 proof-gated check removal and two soundness fixes

Every check 1.0 removes needs a proof from one of four sources: a compiler invariant, a dominating guard, an executed `requires`/`assert`, or a runtime-maintained invariant documented in V1000-PARALLEL.md (L1–L8). Every unproven case keeps the 0.29 check, message and fault order. The benchmark workloads and drivers are byte-identical to 0.29, and no code is keyed to benchmark names or constants.

**Native pinned descriptors.** A descriptor may survive `clear_metadata()` only in three cases:
- `Borrow::Shared` parameters, and projections reached only through shared borrows (the checker freezes their owners for the call);
- `Borrow::Exclusive` parameters, under the existing whole-function `stable` proof;
- the existing pinned views.

An IR pass refuses any place the function writes, moves, swaps, writes back, drops, borrows exclusively or passes mutably. The non-negative fact for text length is keyed to the exact SSA value of `raw & INT64_MAX`.

**VectorView versioning.** The fast body is chosen by one entry test, `limit == len`. That test is exactly the runtime invariant `len <= INT64_MAX/width && (len == 0 || data != NULL)`; any view outside it runs the unchanged 0.29 checked body. 42 foreign-view cases pass in native, C and C-ASan, and the v0.27 foreign-view regression passes unchanged.

**C-backend flow facts.** These cover only owned `int`/`bool` locals whose address never escapes. Writes kill facts, a loop head forgets every local written anywhere in the loop, and facts hold on entry, on back edges and at every exit. Plain C `+ - *` is emitted only when every mathematical result for the proven operand ranges fits in i64, so there is no C signed-overflow undefined behavior.

Loop versioning: one guard, evaluated before the loop, proves every result in the fast copy fits. Otherwise the original checked loop runs. SoA rows are addressed directly, relying on the runtime invariant that SoA storage is allocated only by `zn_soa_push_index`/`zn_soa_clone_storage` after the checked offset computation; the index check and message stay. `restrict` still requires the 0.28 proof, and MSVC precise floating point is unchanged (no FMA contraction, no reassociation).

**Runtime.** Map control bytes keep states 0/1/2. Each slot also has a private 7-bit tag in the same allocation, which only ever filters candidates; key equality decides every match. Capacity math checks both `cap*width <= PTRDIFF_MAX` and `2*cap <= SIZE_MAX`. Idle, spinning and signalled pool counts change only under the pool lock, and a no-lost-wakeup stress test passes with the main thread never helping. Task handles are still never reused, and `exit_code` is still main-thread-only. Exported layouts and signatures in runtime.h are unchanged.

**Soundness fixes.**
- *Native (0.29 bug, found by Worker A).* A loop could inherit the length snapshot of a local the loop later rewrites, so one bounds check was dropped and the native build read out of bounds where C faulted. Snapshots now survive only for locals the loop never rewrites. Regression: `rewritten_length_snapshots_do_not_reach_the_loop_head`.
- *C/LTCG (latent since at least 0.29; found during integration).* MSVC 14.44.35207 at `/O2` lowers an inlined signed `x % 2^k` (and `x % -2^k`) as `(x & mask) - 2^k` for every negative x. The C source contains no undefined behavior; this is a compiler bug, reproduced in pure C. `zn_mod` now routes power-of-two magnitudes through `((a + bias) & mask) - bias` with no signed `%`, keeping the exact remainder-by-zero and `INT64_MIN % -1` faults.
  - Evidence: 2,879,998 pure-C checks with 0 mismatches under the backend's flags, plus regression `signed_power_of_two_remainder_survives_msvc_inlining`; the pre-fix build printed 12 wrong lines.
  - The bootstrap example's frozen 0.22 `codegen-runtime` still uses plain `%`, pinned by its provenance check. Its outputs are checked against the seed and against independent golden models.

## 0.29 proof-gated check removal

Every check that 0.29 removes is removed only under a proof, and every unproven case keeps the 0.28 check and exact message. Native power-of-two division/remainder requires a proven non-negative dividend and an exact `2^k` divisor; conversion checks are dropped only when proven bounds fit the target; fixed-buffer and wire checks are dropped only for `Type::Fixed(_, N)` indices proven in range. Range facts are sound supersets on every path (including failed convergence and loop back-edges); post-fixpoint narrowing intersects with every definition and leaves calls and moves unconstrained. Guard facts survive a loop only for locals that are never rewritten. The C backend uses only whole-function range facts, never native guard facts.

Nominal vector slots rely on runtime-maintained `len <= INT64_MAX / width` and `(len == 0 || data != NULL)` and use one unsigned compare. A VectorView access compares against a validity limit computed at a dominating point; when that compare fails, a cold path replays the 0.28 index-first then storage-overflow checks, including the explicit negative-index guard, so malformed foreign views (for example `len = UINT64_MAX` with a negative index) fault exactly as before. The v0.27 foreign-view regression passes unchanged. Descriptors stay cached across calls only for borrowed view parameters that no function in the program writes.

Generated C helpers (`zn_v029_*`) compute in unsigned types with explicit sign restoration and masked shift counts, so there is no C signed-overflow, overshift or out-of-range narrowing UB; they were checked against the 0.28 helpers (exhaustive at 8 bits) under C and C-ASan. The runtime ABI in runtime.h is byte-identical. Task handles stay unique and are never reused even though up to 64 small task blocks are recycled; recycled blocks are re-zeroed, validated under the pool lock and released by `zn_task_pool_shutdown`. Completion wakeups are counted under the pool lock, so no waiter can miss its wakeup. Inline helping runs through the same execution path that enforces the main-thread-only `exit_code` rule.

## Inline code and defined arithmetic

Inline native/C indexing retains bounds, address-overflow and generation/state checks. Invalid enum tags are rejected before jump-table index narrowing. Id bits never establish liveness; mutable owners and unknown calls invalidate metadata. Descriptor SSA is restricted to nonescaping views/scalars, and address-observable or owned values retain storage. Dense dispatch and shared cold trap blocks preserve exact faults. The new foreign-span case includes negative indices with UINT64_MAX length and confirms the index check occurs before storage arithmetic; the failing old fast-path proof is retained.

Explicit modular intrinsics operate on unsigned bit patterns and canonical signed/unsigned results. Masked counts prevent negative/oversized shifts and shift-by-width UB. Ordinary checked operators still trap on overflow. SIMD requires independent homogeneous float columns and retains update order, no FMA/reassociation; checked integer/IEEE reductions are ordered. Restrict pointers require whole-owner/disjoint-column proof and correct lexical lifetime; public/shared/refined/scoped-view cases remain conservative. Host feature detection and artifact identity use the same canonical mask.

## Static text, tasks and external boundaries

Static literal storage is immutable and permanent for its linked module; tagged clones allocate nothing and destruction skips free. Every text-length use masks the tag, including views, hashes, IO/process helpers and metadata caches. Ordinary C constructors keep untagged owned lengths. New direct literal callers guarantee valid permanent NUL-terminated UTF-8 and use zn_text_size. Supported public library/native-provider ABI recursively rejects text, heap owners, tasks and views as before.

The persistent pool validates tokens under a lock before touching task metadata, never reuses consumed handles, and preserves result publication synchronization. Nested joiners help queued work; saturation can execute work inline outside the lock. Owned results are destroyed once; shutdown waits for/drains work and closes workers. Task metadata/results share an allocation; moved packets stay separate. Worker exit_code is forbidden even when main-thread helping executes the callback; queue saturation has a deterministic negative regression. Explicit shutdown requires quiescence and cannot run from an executing worker. Raw C clients must respect ownership and callback contracts.

Reload runtime cleanup is proof-bound and invoked before final FreeLibrary; independently loaded wrappers share guarded module ownership. Raw hosts embedding static libraries in unloadable DLLs must quiesce all calls/callbacks and call the declared shutdown helper before unloading, outside the loader lock. Runtime/provider/OS/compiler boundaries remain trusted as previously documented. ASan instruments the selected C code/runtime, not arbitrary foreign providers or direct native instructions.

Optimized C/LTCG, ordinary native and ASan objects have separated provenance/cache paths. Ordinary COFF validation remains strict; MSVC LTCG payload acceptance is explicit to that path and still followed by the actual toolchain link. Corruption checks, lowered caps, fresh source verification and atomic hardlink publication remain tested. Finite pure verification now evaluates the new integer/packet primitives without IO, threads or native execution; it remains bounded evidence, never universal proof.

The standalone bootstrap keeps frozen runtime provenance, stack/depth guards and all real safety negatives. Its documented intrinsic/type subset and versioned feature eligibility are explicit; its C fixed point does not replace the full Rust compiler.

## Earlier safety boundaries (through 0.27)

## 0.27 optimization boundaries

Ownership, moves, borrows, field views, effects, capabilities, bounds, overflow, UTF-8 and contracts remain checked. Shared analysis does not share mutable program state or assume id liveness, unchanged channel state, or immutable storage across unknown calls. Bounds/trap paths remain observable across native, C and C-ASan cases.

Syntax reuse is request-local and bounded, keyed by exact source content plus source/module/package identity. It cannot reuse a semantic verdict. Every candidate and combined campaign still undergoes semantic verification; source pinning, dependency authority and fresh-disk commit checks remain required. Publicly replaced SourceFile text invalidates its parsed value. Unsupported or exhausted finite verification remains incomplete rather than being promoted to proof.

Runtime identity stores exact SHA-256 source digests plus the compiler/toolchain/profile recipe instead of embedding source files. One-byte source/recipe changes invalidate it. The process-wide executable digest validates canonical path, length, modification time and Windows file identity before reuse; failed reads are not permanently cached. SHA-256 uses Windows CNG where available with an exact software fallback. These are integrity checks, not a substitute for an authenticated toolchain.

Cache opening can enforce a byte cap without rereading each payload; actual retrieval still validates content. Lowered caps, corrupt entries, LRU reopening and rebuilds remain tested. Native artifact publication uses the existing safe replacement rules, preserving hardlink targets and unchanged running executables. Cold runtime compaction and linker COMDAT/string/identical-code folding retain the checked hot and error paths; runtime behavior and real subprocess timeout/cleanup were exercised.

The optimized Anode-written frontend retains its documented subset, frozen runtime provenance, 64 MiB stage stack reservation and explicit depth guards. Its C fixed point is not a proof of equivalence to the complete Rust/Cranelift compiler. The default main CLI stack remains unchanged. Raw failed attempts and corrections remain in the release evidence.

## Earlier safety boundaries (through 0.26)

## 0.26 vector windows and process authority

Nominal vector windows are nonowning pointer/count descriptors introduced only by lexical borrowed forms or borrowed parameters. The checker reserves the full ultimate owner before evaluating bounds; nested aliases retain the owner loan. Shared access cannot create exclusive element loans. Moves, resize, conflicting reads/mutations, escaping windows and worker transfer reject. Dynamic start/count and element-pair distinctness guards remain in both native and C lowering; integer/text legacy endpoint semantics are unchanged. Owned element replacement destroys the prior element once; no-alloc window traversal adds no implicit clone. SoA is never reinterpreted as contiguous records.

Process permission is explicit and intersects every package route; legacy unrestricted IO does not imply subprocess authority. Normal source checks precede cache reuse and published library/reload artifacts. The runtime uses direct CreateProcessW, explicit UTF-16 quoting, restricted inherited handles, concurrent pipe drains,16MiB per-stream caps and a 30-second total deadline with owned-job termination on failure. Nonzero exits remain data; invalid input/capture yields recoverable Error. Dedicated tests cover native/C/C-ASan results, deadline/output failures, malformed text, unrelated inheritable handles and handle/allocation cleanup. The OS, Windows job/process/pipe implementation, CRT and code generators remain trusted. Capability permission does not make an explicitly launched program safe or promise system-wide resource isolation.

## 0.26 campaigns and ABI integration

Campaign verification reconstructs assignments from current pinned sources instead of trusting submitted worker ownership. Unassigned, duplicate, overlapping, stale and foreign edits reject. Every composition still passes ordinary ownership, types, views, effects and capabilities. Pure bounded verification cannot execute process/IO/FFI/thread/native effects; unsupported cases remain incomplete. Finite grids and witnesses are not universal proofs. Default writes are zero. Explicit source transactions retain cooperative-lock, rollback and crash-recovery limits; multi-file visibility is not OS-wide atomic.

Selected ABI exports control signature validation at the boundary, not source checking or capability checks. Owning collections and borrowed windows cannot cross the POD boundary. Stateful hosts switch code/state under guarded callbacks, verify generation/layout receipts and retain old code while needed. Explicit reload generations are published artifacts outside the object cache's LRU; no live generation is automatically deleted. The five-module example and7-module bootstrap stay separate from Ghosthand and the complete Rust compiler.

The0.25 iterative C block emitter/default Windows CLI stack and legacy generic builtin navigation behavior remain regression requirements. No safety check or earlier negative fixture is disabled by this release.

## Earlier safety boundaries (through 0.25)

## 0.25 propagation, channels and vectorized loops

`try` uses the normal result construction, return postconditions and lexical cleanup path. Pending call loans are aborted in reverse order before an error return; earlier argument/aggregate temporaries and scoped writebacks remain accounted for. It does not turn runtime bounds/overflow faults into recoverable results. Differential native/C/C-ASan tests and exact thread-safe allocation ledgers cover these paths.

Channel endpoints share synchronized reference-counted ring storage. Checked capacities and payload layouts precede allocation; FIFO operations transfer owned values with typed destruction for replaced slots, failed sends and unread messages. Borrowed views/nested tasks cannot enter the queue. All endpoint operations carry I/O effects; wait operations and implicit task joins cannot claim bounded control flow. Closing wakes waiters and allows draining, but programs must still arrange progress: waiting on an endpoint that nobody can service can deadlock. Finite source-control contracts do not promise deadlines or scheduling fairness.

SoA optimization hoists only stable metadata and uses packed floating operations for independent supported updates. It retains scalar tails and rejects transformations that would drop assertions, change reduction order, skip checked conversions or relax signed-overflow/bounds behavior. Native instruction evidence and adversarial differential cases are retained with the channel example.

## 0.25 packages, witness edits and test processes

Local dependencies have route-qualified identities and capability intersections checked through transitive calls. A root's permission cannot grant an undeclared capability to a dependency. Package manifests/layouts participate in cache identities and source pins. Project formatting excludes dependency write targets, and output protection covers all loaded sources and package manifests. Existing pinned hole commit/recovery refuses dependency-source replacement through a root batch.

Witness synthesis reconstructs actual sampled inputs, checks the proposed source normally and returns zero-write edits with freshness pins. Expected counterexamples/faults are explicitly failing replays. Finite samples and checked test syntax do not establish universal correctness. Live editor verification executes only supported pure checked IR, stays budgeted, and rejects stale/cancelled results without applying edits.

Parallel test execution checks source capabilities before building its private selector, compiles one dispatcher and launches only enumerated cases with 1–16 child slots. It hides the selector from user arguments, preserves invocation cwd and reports all statuses in deterministic order. Child process isolation does not isolate filesystem fixtures; users control concurrency through `--jobs`.

The expanded bootstrap remains a separately documented C-backed subset. Its byte-identical Stage 1/2 output, real native/ASan executions and retained failure fixtures provide evidence for that subset. The full Rust/Cranelift compiler supplies the current complete ownership/view/effect and packaging implementation. JSON stores UTF-16 code units to preserve valid escaped NUL/surrogates while retaining the runtime's non-NUL text invariant; its README documents resource limits and the parsed-document writer precondition.

## 0.24 arenas and bounded control flow

Arenas own their payloads and metadata; IDs own no memory. Access checks nominal type, slot bounds, occupancy and nonzero generation before yielding a payload. Free increments generation and never wraps; exhausted slots retire. Copies/clones of an instance-relative handle can remain valid in corresponding same-type snapshots, so handles do not establish globally unique provenance. Scoped get/get_pair loans reserve the backing owner; mutation/free/reallocation conflicts reject. Deep clone/drop and worker transfer preserve the existing owner rules. Private seeded-runtime tests exercise generation exhaustion without exposing a source-level unsafe constructor.

Bounded effects reject while and cyclic/unpublished call paths while retaining checked finite loops. They express finite source control flow, not an elapsed-time/resource/scheduling guarantee. Existing bounds, arithmetic, views, capabilities and no_alloc/no_io checks remain authoritative.

## 0.24 source transactions

Skeleton and source SHA-256 pins, exact assigned hole ranges and independent/composed checks protect batch boundaries. Disk application and finite contract execution require explicit options. Incomplete diagnostic checking supplies no executable proof. Cooperative source locks, synced journals/backups and per-file rename support rollback and guarded recovery. Independent files do not acquire simultaneous OS-wide atomic visibility; a process/OS crash can expose a partial set. Recovery refuses active owners, corrupt/outside data and external edits. Other writers must cooperate or remain quiescent. Hard-link support is a precondition before source writes; directory-metadata durability depends on the OS. See tools/anode-lsp/V024-BATCH.md for the exact operational contract.

## 0.24 state migration and self-compilation

Dynamic reload slots own aligned POD/fixed bytes and replace code/layout/state receipts under a single quiescence lock. Checked no_io migration hooks must match the actual input/output schema digests and compiled DLL metadata. Nested bool representations are checked before internal native calls and after mutation; invalid safe byte edits restore prior state. Existing typed references retain layout-strict replacement. Raw host ABI casts, correct function signatures and pointer lifetime discipline remain trusted host responsibilities. A no_io migration may allocate or fail to terminate; ordinary runtime failures still end the process. Owning arenas/enums/views/tasks do not cross this state boundary.

The Anode-written subset reaches byte-identical Stage 1/Stage 2 C self-compilation and runs real native/ASan fixtures. Its boxed value backend and basic checker implement a smaller language than the Rust seed. It uses a documented 64 MiB stack reservation with active parser/call-depth checks. Fixed-point agreement and these tests are evidence about that subset; the full seed supplies the current native ownership/view/effect/arena rules.

## 0.23 columns, disjoint aliases and transfers

SoA columns preserve finite typed layouts, checked capacity/size arithmetic, ownership and deep cloning/destruction. Field-only aliases access columns; whole-record aliases borrow stack projections rather than minting duplicate owners. Mutable projections write back before loans end on every ordinary control exit. The whole vector remains reserved while aliases exist. Pair selection establishes bounds and unequal indices before two exclusive aliases are available. `swap` requires distinct mutable typed places and does not allocate, clone or destroy owners. Range/descriptor/predicate analyses discard affected facts rather than reusing pre-swap or pre-writeback values. Allocation-instrumented native/C/ASan tests check balanced owners and physical field strides.

## 0.23 foreign native boundary

FFI is denied by default and requires explicit project capability and declared scalar/fixed-buffer ABI. Symbol spelling survives module remapping; incompatible declarations of the same foreign symbol reject. Externs cannot claim checked source effect/view/pre/postcondition contracts. Their effects remain conservatively unknown allocation/I/O, and direct spawning rejects. Explicit provider libraries are validated/staged and hashed into output/cache identities. Baked outputs reject extern programs where provider identity is unavailable.

Anode cannot prove a C implementation's memory behavior, termination, effects or thread safety. Enabled FFI trusts the native provider to honor the declared ABI, buffer lengths, output initialization, mutability, aliasing, lifetime and concurrency requirements. This is an opt-in native trust boundary, comparable to the promises required by a Rust unsafe FFI wrapper. The ordinary source checker and runtime guards are still active; they do not turn arbitrary foreign code into checked code.

## 0.23 synthesis and finite verification

Hole contexts are diagnostic-only typed assumptions, never emittable IR. A partial proposal remains conditional; a complete result requires the ordinary strict checker on actual all-filled source, including tests. Candidate expressions are type-constrained, blocks become checked helpers with explicit captures, and locked function types/views/effects/contracts remain unchanged. Fingerprints bind slots and edits to a source/manifest snapshot; byte and document-version guards prevent stale publication. No source file is written by the agent methods.

Contract execution uses a bounded checked-IR evaluator with explicit steps/cases/deadline and no foreign, I/O, process or thread operations. Unsupported constructs, no accepted precondition inputs and exhausted budgets do not become successful proofs. Passing finite samples do not establish a universal property or native-code equivalence. Representative verdicts are compared with actual native/C behavior in tests. Normal compilation checks remain authoritative.

## 0.23 reload and bootstrap code generation

Reload rejects changed live layouts/storage kinds or incompatible public type/borrow/view/effect/contracts, with structured differences. Artifact/source hashes and manifest self-digests detect stale/mismatched artifacts; they are integrity data, not authentication of untrusted plugins. Hosts must load their trusted compiler-produced artifacts. Replacement compares the actual live slot's compatibility, waits for guarded calls, and retains loaded code through shared ownership. Foreign entry-point calling conventions and host state synchronization remain the host's responsibility. Public opaque owners/views/tasks cannot cross this ABI, and direct executable call sites are not rewritten.

The Anode-written C emitter is a separate Stage 1 subset using checked value primitives, explicit ownership cleanup and conservative eligibility guards in addition to its basic frontend. It rejects cases its analysis cannot safely elaborate, including its complete own native build today. This is not a proof-equivalent replacement for the Rust seed's full checker or backend. Generated native/ASan programs are tested against seed outputs and guarded failure behavior; the main compiler's static and runtime checks remain unchanged.

## 0.22 typed task ownership and systems I/O

Each generated worker has a heap packet and separately allocated, correctly aligned result slot with a type-specific execute/destroy callback. Joining waits before moving that result into caller storage, clears its owner slot and consumes the task. Implicit task destruction waits before recursively destroying an unconsumed result. Workers do not expose stack storage, share a caller-local borrow or detach an owner. The checker rejects moved/cloned/rejoined handles and scoped views across the boundary. Existing record, enum, collection and field-loan checks apply to worker parameters and results. The native and C backends use the same runtime transfer contract.

The project checker grants `file_exists` and `list_dir` only with `file_read`; missing access cannot be bypassed by a helper or library build. Paths and directory entries use checked UTF-8/UTF-16 conversion and owned storage. Errors fail rather than return corrupt names or partial enumerations. Sorting makes one unchanged directory snapshot deterministic; concurrent filesystem mutation is outside that guarantee. Monotonic time is a system observation, not a pure function. The exit setter validates 0–255 and enforces its main-thread restriction in the runtime, including indirect calls from a worker. It does not bypass normal task/scope cleanup.

## 0.22 static library trust boundary

Library compilation has the same project/type/ownership/view/effect/capability validation as executable compilation, with no mandatory or synthetic `main` and no test bodies. The exported ABI admits only validated scalar/POD/fixed-buffer layouts. Heap owners, enums, tasks and escaping views do not cross it. Public record closure, sizes, alignments and field offsets are checked; raw aggregate buffers are converted field by field rather than reinterpreted as internal nominal types. All library source bodies remain checked even when a prior archive receipt matches.

Executable-only `arg`, `args_count` and `exit_code` reject in library source, including inside private helpers. The library does not guess or initialize the embedding process's argument/main-thread context.

Generated Rust wrappers keep raw externs private, express shared/exclusive borrows with Rust references, and return owned POD values. Raw C callers are outside the Anode checker and must provide valid aligned pointers, nonoverlapping mutable/result storage and the declared Windows x64 ABI/CRT. These requirements are written in the generated header. This feature does not make arbitrary C callers safe. Internal/runtime symbols are isolated from independently built libraries. Runtime assertion, overflow, bounds and contract failures remain process-fatal and are documented in the binding contract.

## 0.22 formatting and diagnostic recovery

Formatting changes trivia only. It parses before and after formatting and verifies that literal, comment and language tokens remain in exactly the same order. It performs no name/ownership repair, code generation or execution. The CLI validates all proposed text before writing, checks for intervening source edits and publishes files by temporary-file replacement rather than truncating source data shared by hardlinks. Project-wide publication is per-file, not a multi-file filesystem transaction; an I/O error can leave earlier files formatted, with unchanged semantics.

Agent/editor formatting returns edits and fingerprints without writing files. LSP ranges count UTF-16 code units and stale/cancelled in-flight requests do not publish edits. Formatting semantically invalid source does not make it safe or valid to compile.

Multi-error recovery preserves the original strict failure and source snapshot. It gathers independent syntax/declaration/function errors in bounded probes, without hiding invalid function bodies to report a successful compilation. Invalid recovered input remains invalid. Recovery is diagnostic assistance rather than an alternate safety checker.

The Anode-written bootstrap frontend has basic moves, types and scoped-loan checks only. Its documented unsupported syntax and incomplete interprocedural/alias analysis remain real limits. The Rust seed's full checks apply when building this frontend, and remain authoritative for Anode executable safety.

## 0.21 maps and scoped value access

Maps are nominal owned containers with validated integer/text keys and finite copy/owned value layouts. Key/value types are checked before either backend sees IR; unit, tasks, borrowed views, unsupported keys and recursive cycles reject. A map cannot expose its layout metadata as source record fields. Integer keys retain their declared width and all unsigned high bits; text keys compare byte length and content. References are never stored in table entries.

`insert` consumes owning key/value operands, copies scalars, and destroys a replaced stored key/value exactly once. `remove` borrows its lookup key, destroys a present entry, and returns false when absent. `clone_get` and whole-map cloning recursively duplicate owning data instead of aliasing it. Typed clone/drop code traverses live entries only. Internal raw storage copies are completed with deep copies before an owning clone becomes observable; rehashing relocates existing owners without creating a second owner. Maps can move through records, enums, vectors, returns and worker packets under the existing transfer rules. Owned worker input is independent of its caller; borrowed input is rejected.

Scoped `get` selects the value pointer only after a presence check. Missing `get`/`clone_get` fails with `map key not found`; there is no unchecked pointer or escaping reference result. The checker reserves the entire backing map before evaluating the lookup key. Shared value loans forbid table mutation, replacement and movement; exclusive value loans additionally forbid backing reads and overlapping aliases. A different key does not establish disjointness. Disjoint sibling record fields remain usable. The lookup key itself is only borrowed for selection, and a separate named key remains reusable. Mutable aliases may edit fields or replace a whole value, with destruction of its previous owner. References and owned fields cannot move out through an alias. Nested loans and return/break/continue use existing lexical cleanup.

The shared runtime uses deterministic hashing and bounded open addressing with explicit empty/live/tombstone states. Allocation-size multiplication, capacity growth, descriptor invariants, slot ranges and probing are checked before memory access or allocation. Deletion clears dead storage; reinsertion can reuse tombstones, and checked rehashing preserves the live entries. Integer-key reads use width-aware byte copies to avoid alignment and strict-aliasing violations. Both native and C code generation use the same runtime storage ABI with type-specific key/value cleanup. Hashes are not randomized: bounded probing is not a constant-time guarantee or protection against adversarial collision workloads. Capacity checks do not prove freedom from resource exhaustion.

Map queries and scoped selection do not allocate or perform I/O. Scalar `clone_get` is also nonallocating; insertion and owned deep copies conservatively allocate in effect analysis. Removal is nonallocating but mutating, so it remains forbidden in assertions/contracts. Effectful argument expressions still count: constructing a text key does not become nonallocating because lookup is read-only. Pure contracts can inspect existing keys; they cannot mutate tables or allocate owning results. Effects and field views remain checked transitively.

The four-module `examples/symbol-resolver` exercises lexical shadowing in its source language, independent of Anode's restriction on shadowing live local bindings. Its 12 tests pass in direct native and optimized C with AddressSanitizer. They include 1,536 text-key declarations and lookups, 512 sibling scopes in an integer-key map, 768-key deletion/replacement/reinsertion, nested map/record/enum/vector clones, shared/mutable value access and worker transfer. These are executable regressions for the supported subset, not a proof of complete Rust-equivalent soundness or a self-hosted compiler.

## 0.21 conditional and wildcard cleanup

Else-if syntax elaborates to existing nested checked `if` statements. Later conditions run only when earlier conditions are false; continuing branch states merge under the existing ownership rules. No additional reference or conditional-binding mechanism is introduced.

A final unbound wildcard expands into the remaining enum variants before the existing exhaustive switch IR is emitted. Explicit duplicates, wrong payload bindings, a non-final wildcard, a wildcard payload binding, redundant wildcards and missing coverage reject. Only the active variant is inspected or destroyed. In an owning wildcard arm, the unbound owned payload is destroyed exactly once; in shared/mutable wildcard arms the complete source enum stays loaned for the arm even though there is no payload binding. Wildcards neither change the source tag through a borrow nor permit a reference to escape. Early exits retain the same reverse-order owner cleanup and loan release as explicit arms. Checked enum matching adds no scalar/structural patterns, guards, arbitrary lifetime inference or unsafe escape.

## 0.20 runtime loop proofs

Shared borrowed record fields now reuse descriptor metadata, keyed by the complete local/field path. The compiler gathers only fields actually accessed by the checked body, following records only; it never hoists inactive enum payloads, vector-element aliases, mutable record projections or unmentioned siblings of a field view. Inlined calls remap only independently proven stable projections. Element contents remain mutable loads, and distinct fields retain distinct bounds identities.

Read-only user calls preserve transient scalar and stable-descriptor facts because the checker prohibits mutation through shared parameters. Any exclusive reference argument still clears those facts. Successful length conversion establishes a fact about that exact immutable descriptor value. Repeated conversions may omit the redundant sign test only when that successful check dominates the use. Branch merges intersect facts; loop boundaries and mutable calls clear them as before.

Small loop conditions may now include a stable descriptor length. The initial condition and zero-iteration path remain; the bottom copy tests the updated index, while continue paths still execute the header. Bounds, arithmetic, first length checks, mutable-descriptor reads and failure ordering remain checked. Differential native/C/C-ASan regressions exercise nested records, unequal arrays, empty inputs, disjoint field views, replacement, inactive enums, element aliases, negative/out-of-range indices, shared/mutable calls and loop exits.

Native function entries have a minimum 16-byte alignment in both emission modes. Cranelift/object/linker padding and relocations provide alignment; no source instructions, checks, stack probes or unwind records are skipped. Compiler identity invalidates prior cached objects after this setting changes.

## 0.20 native module cache

The native backend accepts an immutable `CheckedProgram`; CLI source checking still precedes both EXE receipts and object-cache lookup. Partitioning does not change ownership acceptance. Cache-off retains the whole-program native path. Positive budgets reuse per-module COFF objects; all units link into one checked application.

- Keys hash the unit's checked bodies/locals/signatures and conservative transitive call dependencies, including private helper bodies. Used nominal layouts are closed through their record/enum/vector fields. An unrelated existing type's layout is omitted. Function/type-ID changes can conservatively invalidate more units; emitted assertion lines remain part of checked IR.
- Incoming hotness and newly required worker adapters are explicit key inputs. Otherwise an unchanged worker body could incorrectly reuse an object lacking the callback requested by another module. Compiler binary, runtime/header, ABI version and installed toolchain identity separate incompatible objects.
- Per-unit generation retains whole-program optimizer summaries and the same 0.19 inlining/range/guard rules. Cross-unit COFF functions/adapters have link-visible symbols; source visibility is still enforced by the checker. User code gains no FFI or unchecked-link escape.
- Optimizer expression graphs are immutable `Arc` data. Each bounded worker creates its own Cranelift context, symbols, locals, guard facts and output. Workers share no mutable code-generation state. All join before linking; failure leaves the old EXE/receipt untouched.
- The existing cache checksum detects accidental local corruption, not hostile forged objects. Cached bytes become verified private memory snapshots under the cache lock, then code generation runs without holding that lock. Concurrent eviction cannot change those snapshots. Runtime linking and Windows unwind generation remain checked. The cap is enforced before reusable EXE receipts as well as writes; objects too large to retain still build normally.
- A cache handle builds one mutex-protected metadata index while holding the process lock. Hits still checksum the complete payload; index entries are not substitutes for validation. Recency persists through the existing header touch/file modification time, oversized puts preserve useful entries, and interrupted writes remain accounted for. No new persistent database or sidecar files are introduced. Checked user calls/adapters are marked colocated because all their definitions link into the same native image; external runtime declarations retain their normal linkage rules.
- Cached native payloads become private, checksum-verified byte snapshots. Missing units generate into private byte buffers. The linker receives one direct entry object and, when needed, a Rust-written COFF archive for the other units, avoiding a scratch-file write per module. The archive has both standard linker indexes, checked 32-bit offsets, short member names, aligned members and unique exports; the system linker validates/resolves it. Function bodies, ownership and unwind data are unchanged. See Microsoft's [archive specification](https://learn.microsoft.com/en-us/windows/win32/debug/pe-format#archive-library-file-format). Archive-format tests verify endian order, symbol sorting, offsets, padding and invalid exports; integration tests exercise the actual linker.

Validation covers cross-module owning records/vectors, borrowed payloads, methods, worker ABI transitions, changed layouts, transitive helper/hotness keys, unchanged-code coordinates, bypass attempts through stale EXEs, cached overflow failures, corruption, eviction, force rebuilds and concurrent native builders. Existing C/ASan reference paths remain available. This is native object reuse rather than a persistent store of ownership judgments.

## 0.19 runtime optimization

These optimizations run only after typed ownership, views, effects and capability validation. They do not change which source programs are accepted.

- Literal interval comparisons of the same checked int place use an equivalent unsigned subtraction/comparison. Calls, indexing, arithmetic and distinct places do not qualify for that lowering. Other numeric widths and floats retain the original path.
- Integer ranges union every definition, assignment, branch and loop write. The analysis widens expanding cycles, preserves stable endpoints and falls back to full unknown ranges if it does not converge in 32 passes. Address-exposed or borrowed scalar storage stays unknown. Helper-return analysis is limited to two call levels and 64 uncached queries; incomplete evidence stays unknown. Contextual inputs cover every possible successful argument value, not one sampled execution.
- Checked add/subtract/multiply may become ordinary instructions only when the complete mathematical range fits i64. Division/remainder can become unsigned only when the dividend is proven nonnegative and the divisor strictly positive; this also excludes zero and MIN/-1 faults. Unknown cases retain the original checks.
- Dominating guard facts follow scalar value versions. Assignments create a new identity; copying preserves a snapshot. Branch joins keep common facts, returning branches do not pollute reachable siblings, switch arms start from independent snapshots and invalidate facts on exit, and loop entry/exit clears transient evidence. Calls with reference arguments invalidate facts. Bounds-check elimination additionally requires a nonnegative index and the same stable cached descriptor length. Memory/projected reads do not acquire scalar aliases.
- Total predicate flattening admits only bounded register comparisons, constants, boolean logic, copies and calls to similarly total helpers. It excludes checked arithmetic, floats, memory, contracts, allocation, recursion, loops and reference arguments. Thus eager boolean evaluation cannot trigger a source-visible effect or failure. Expansion is capped at 256 checked operations.
- For a positive constant modulus 2^k-1 and a proven nonnegative dividend, a single Mersenne fold is permitted only if (dividend_max >> k) < modulus and the folded sum fits i64. The compiler computes (x & modulus) + (x >> k), then subtracts the modulus if needed. This is exact because the folded value is below twice the modulus. Sparse corrections can use a cold branch; other cases use selection or the original remainder operation. The source hash algorithm is unchanged.
- Small scalar/borrowed helpers may now inline structured loops within the existing 80-operation helper / 256-operation caller budgets and two nested helper levels. Function-local loop target maps are saved/restored across inline calls, so a callee's break/continue cannot capture the caller's same-numbered target. Assertions, conditions, overflow/bounds checks and early returns remain in the checked body. Owned temporaries, allocations and recursive expansion are still excluded.
- No failure path is replaced with unchecked memory access merely because an index is usually valid. C and C-ASan remain differential validation paths; native application code itself is not ASan-instrumented. Cache receipts still include compiler identity, so old objects cannot bypass the changed backend.

## Preserved 0.12 boundary

The compiler is Rust. Both native backends consume the same checked typed IR. Generated programs use a small trusted C runtime and MSVC linking. This is a tested, deliberately restricted language, not a claim that a new checker has Rust's maturity or soundness guarantees.

0.12 loop jumps carry a checker-assigned enclosing-loop ID in IR. The checker emits reverse-order drops for owned bindings introduced in the exited loop, including nested scopes and match payloads. Outer bindings and borrowed slice descriptors are not destroyed by that cleanup. Moved slots are already zeroed; their cleanup is harmless. Native jumps use explicit loop blocks; C uses explicit labels so a source `break` inside a match cannot accidentally target C's switch. Continue returns to the condition, including its side effects and checks. Unknown targets and out-of-loop statements reject.

The checker keeps the conservative prohibition on moving an outer owner inside a repeatable loop. It now also merges the pre-loop moved state into the post-loop state: assignment inside a loop cannot establish restoration if the loop might never execute. Terminal branches do not make values available on reachable sibling branches. This is not a general dataflow/lifetime proof or an infinite-loop return analysis. Bounds, overflow, effect, capability, scoped-loan and worker ownership rules remain unchanged.

0.11's text operations preserve the existing owned, NUL-free UTF-8 invariant. The decoder checks remaining length before continuation reads and rejects invalid/overlong encodings, surrogates and out-of-range scalars. Byte offsets are checked before dereference. End-of-text is a valid query/next boundary but not a readable scalar. Replacement and joining check output-size arithmetic before allocating and copy into independent output storage. No borrowed text pointer escapes a runtime call. New builtins use the same checked argument loans and field-view analysis as existing text operations; both backends use matching address/value ABI wrappers.

The seven read-only queries are classified no-allocation/no-I/O; five constructors/transformations may allocate and cannot run under `no_alloc` or in contracts. Allocation failure still terminates. Unicode scalars are not grapheme clusters. Search is exact and has no normalization/case folding; the trimming whitespace set is explicit in LANGUAGE.md. A fresh buffer is returned even for a no-op replacement/trim. Existing bytes/index, integer overflow, borrowing and worker ownership restrictions are unchanged.

0.10 adds copyable f32/f64 scalars with 4/8-byte aggregate storage. Native code carries bits in the private integer scalar ABI, uses actual floating registers for arithmetic/comparisons, and preserves width when packing worker arguments and fields. C code uses float/double with `/fp:precise`. Bit transfer uses memcpy or native bitcast, never pointer-punning. Floating operations retain infinity, NaN, signed zero and rounding; these are numeric results, not memory-safety failures. They cannot silently become indexes: explicit float-to-integer conversion truncates and validates finite, exact power-of-two bounds before the cast. No integer safety checks were relaxed.

The language exposes no floating-environment or rounding-mode mutation. No fast-math or implicit contraction is requested. NaN payload propagation is unspecified. `sqrt` and remainder use the trusted CRT math routines. Decimal parsing validates the entire input and rejects non-finite values; because CRT parsing may use internal locale/conversion machinery, it is conservatively excluded from no-allocation contracts. Floating buffers/slices, SIMD, decimal arithmetic and formal numerical-stability guarantees remain outside this release.

0.9's scoped slice descriptors never own or free their pointer. The checker holds a shared/exclusive loan of the entire backing place for the lexical block, reserves it before evaluating bounds, and propagates field access through declared views. A nested slice holds its parent loan; returning, moving, cloning, storing or sending a slice to a worker is rejected. Disjoint record fields can still be used. Two mutable ranges from the same owner are conservatively rejected even when their numeric ranges are disjoint. Runtime range and element checks precede memory access, including empty slices and little-endian writes. No heap allocation is introduced by slice creation.

Both code generators consume the same checked slice operation. Native heap-descriptor reads use the same alias classification as append/length operations; a regression test exercises allocation, append, slicing and early-return cleanup without an intervening external call. Width/layout, nested offset and failed-write cases also run under C AddressSanitizer. Broader stored/returned anchored lifetimes are not implemented.

0.8 stores integer fields/elements at their declared width (1/2/4/8 bytes). Native registers use canonical 64-bit values, sign/zero-extending loads and narrowing stores after type-checked operations. Function returns, enum payloads and thread packets follow the same boundary. Casts, arithmetic, division/remainder and negation check range; only explicitly named saturating operations clamp. Fixed integer arrays own initialized inline storage, cannot grow, and obey existing move/loan/view rules. Failed try-writes check the entire span before touching any byte. Other reads/writes remain fail-fast. There are no unchecked indexing or pointer-casting APIs.

Arrays and final padded record/enum layouts respect a 64 KiB size limit. This does not bound total stack use across a function or recursive calls. Windows stack probes remain enabled. Buffer operations are local memory operations, not networking capabilities or an OS sandbox.

0.7 extends the existing owner rules to `texts` and built-in read/write result enums. A text collection owns each element, drops it exactly once, and deep-clones on explicit `clone`/`text_at`. Recoverable I/O never converts invalid text into a valid `text`. Native and C backends share the same checked layouts and runtime behavior.

Agent verification uses an immutable project snapshot. Each request gets independent in-memory overlays; source/module size limits and all normal checks apply. Patches require one exact match. The module graph and capability manifest cannot be changed inside a request. Verification neither executes the candidate nor writes files. A successful check is not a test result or a logic-correctness proof. Reload the agent after changing the on-disk project or manifest.

Effect contracts are conservative over checked IR and transitive calls. Free-form intent and model advice cannot override them. Typed holes report a partial checking context, not a verified program, and block all code generation. Current project capabilities govern this language's known builtins; they are not an OS sandbox or the per-value capability-token design proposed for a future networking API. Legacy single-file projects without a capabilities table retain their prior unrestricted builtin policy.

## Static rules

1. **Owners.** Each owning text, collection (including a nominal map/vector), record, enum or task binding has one usable owner. Passing/returning/binding by value moves ownership and clears source storage. A moved binding cannot be read. Copy scalar values do not allocate or alias owned storage.
2. **Shared loans.** `&ints` and `&text` permit inspection, repeated shared calls and explicit cloning. They do not permit mutation, moving the owner or replacing its storage.
3. **Exclusive loans.** `&mut ints` requires a mutable owner or an existing exclusive parameter. No competing read/shared loan/exclusive loan/move is allowed for overlapping field paths while the call is being evaluated or executed. Nested arguments are checked too.
4. **Loan scope.** Calls use explicit, direct borrow arguments, with dot-method receiver borrows supplied by elaboration. Scoped `borrow` blocks, borrowed enum matches and borrowed vector loops create checked lexical aliases. References cannot become ordinary local values, be returned, stored in collections, captured, or sent to a worker. Call loans last for the entire call; lexical loans end with their checked scope. Nested reborrows end when their call or scope completes.
5. **Alias boundary.** Because reference values cannot be stored, a caller cannot manufacture hidden aliases to an owned value. Multiple shared parameters can alias; exclusive access cannot overlap another access. Distinct field paths may coexist, and an explicit view may share its read-only fields with another parameter. Functions are checked under these parameter guarantees.
6. **Control flow.** Branches conservatively merge moved-state information. Repeatable loops cannot repeatedly move an owner declared outside the loop. This currently rejects some programs that Rust accepts. Shadowing is also rejected.
7. **Destruction.** Successful returns and ordinary scope exits drop owners in reverse declaration order. Borrowed parameters do not drop their caller's storage. Returning an owned value transfers it before other owners are cleaned up. Moved storage is empty, so cleanup cannot free the transferred allocation twice.
8. **Threads.** Only owned/copy parameters may enter a worker. References and shared mutable global state are absent. Each task has one owner; join consumes it, and dropping an unjoined task joins automatically. A worker can borrow its own moved-in data locally.

Examples rejected at compile time:

```text
inspect_and_change(&a, &mut a); // shared + exclusive alias
change_twice(&mut a, &mut a);   // competing writers
consume_and_inspect(a, &a);    // borrow after move
let reference = &a;           // escaping/stored loan
spawn borrowed_worker(&a);    // thread cannot borrow caller storage
```

Parameter borrows support whole ints/text values and whole records/enums. The text `slice` built-in returns a new owned allocation, not a borrowed slice. Field views and 0.9's scoped integer slices are supported. There are no lifetime parameters, stored/returned references, interior mutability, reference-counting, trait objects, closures, unsafe blocks or foreign calls. Those features would require a substantially richer analysis.

## Runtime rules

Every integer addition/subtraction/multiplication/negation checks overflow. Division and remainder check zero and the signed minimum / -1 overflow. Indexed collection reads/writes check signed indices; map `get`/`clone_get` check presence before value access. Allocation and capacity arithmetic are checked. Failed contracts, allocation failures and invalid arithmetic/access terminate the process. Safety checks stay enabled in optimized builds. Preconditions/postconditions are runtime checks, not proofs of logical correctness.

The native runtime's allocation, UTF-8 buffers, thread startup/join and destruction are trusted. Exporting C and modifying it bypasses the language guarantees. MSVC, the OS and CRT are also trusted dependencies.

Version 0.3 preserves those rules while changing their implementation:

- Addition/subtraction use unsigned intermediate arithmetic plus explicit signed-overflow detection. `memcpy` interprets valid signed result bits without executing overflowing signed C arithmetic.
- Dynamic multiplication uses MSVC's full signed 128-bit product and checks that the high half is the sign extension of the low half. Literal multiplication keeps checked thresholds so the backend can fold constant divisions.
- An unsigned index comparison rejects both negative and too-large indices. This relies on the enforced array-capacity bound `len <= SIZE_MAX / sizeof(int64_t) < INT64_MAX` on this x64 backend. No source operation can forge a descriptor or exceed that bound.
- Appends inline the spare-capacity path; growth keeps its overflow and allocation checks in the cached runtime. Array reads borrow the descriptor instead of copying it.
- Hot/cold code placement changes whether helpers inline or share a function body, not which checks they perform. Type/ownership/borrow checking is unchanged. Only actually spawned functions get worker wrappers; unused functions still undergo static checking.

## Historical 0.4 validation (retained below; 0.5 additions follow)

- 33 Rust test functions pass: 3 optional-model transport tests, 20 language/native tests and 10 project/cache/I/O integration tests. Several tests contain many distinct accepted/rejected programs, including 17 explicit borrow rejection cases and 600 malformed-source inputs. The five-module example also executes its six declared native tests.
- Shared/reborrowed reads, exclusive updates, repeated use after a loan ends, cloning from a loan and ownership transfer into native workers execute correctly.
- Type errors, use after move, branch/loop moves, alias conflicts, moved/immutable/shared borrows, moving through a borrowed parameter, reference escape and borrowed worker arguments are rejected.
- Native bounds/overflow/contract failures produce controlled failures; tasks join on scope exit.
- The 196,608-reading analytics workload passes with AddressSanitizer on both generated C and the cached runtime.
- Two intentional C-only positive controls independently trigger `heap-use-after-free` in generated code and `heap-buffer-overflow` inside the runtime. This checks that instrumentation is active; these defects are not expressible through supported Zenith source.
- `cargo clippy --all-targets -- -D warnings` passes.
- Inline and shared arithmetic are compared with Rust `checked_*` operations across a 15-by-15 boundary-value matrix, in fast and optimized builds. Overflow/zero-division/invalid-index/length/growth/contract cases deliberately fail with the controlled runtime error, including `INT64_MIN` indices. Appending and cloning 10,000 elements verifies growth and copying.
- Nested worker spawns in calls/branches/loops still compile and execute after wrapper elimination.
- Project tests reject borrowing/thread-rule bypasses through module calls, hidden I/O in imported functions, escaping module paths and malformed manifests. Assertions preserve original module line numbers.
- Native Unicode-path I/O, UTF-8 slicing, signed parsing boundaries and invalid-input failures run under AddressSanitizer. The five-module readings application's tests also run with AddressSanitizer.
- The CLI reuses unchanged output, recompiles one of two modules after a body edit, rejects unsafe edits before using a receipt, rebuilds a corrupted EXE and preserves unrelated files during managed cleanup. Cache tests cover LRU eviction, corrupt objects and objects larger than the budget.

## Project, I/O and cache boundaries

Module visibility is checked during name resolution; the entire resolved program then passes through the same ownership/alias checker. Module boundaries cannot authorize stored references or borrowed worker arguments. Files resolve within the project root and have bounded sizes/count/depth. All loaded functions are checked, even if unused.

An explicit capabilities section enforces filesystem reads/writes, argument access and thread creation throughout supported source. Omitted permissions are denied; new projects explicitly deny all four. Legacy sources/manifests without this section preserve previous access. This is not a native-code sandbox, per-path permission system or proof of dependency intent.

Text file input rejects invalid UTF-8 and NUL, checks allocation/length and limits input to 64 MiB. Arguments and paths convert between Windows Unicode and UTF-8. Byte access checks indices; slicing checks both range and UTF-8 code-point boundaries. Integer parsing checks every step for signed overflow. Failures terminate rather than return a recoverable error; writes can partially modify a file on I/O failure.

Unchanged-output reuse still performs source checking. Artifact fingerprints cover compiler/toolchain identity and build inputs; stored bytes are checked before reuse. Module-cache mutations and output builds are locked. Objects are copied to private scratch before linking, so eviction cannot remove an object being linked. Content fingerprints detect ordinary local corruption/freshness changes, not malicious tampering; they are not cryptographic authentication for a remote cache. Remote caches are not implemented.

Managed cleanup enumerates known build outputs and refuses redirected managed directories. It preserves source and unrelated filenames. The module cache evicts only recognized owned entries; a small marker and lock remain after emptying it. Budget eviction never changes compiler acceptance rules.

AddressSanitizer is a dynamic memory checker, not a proof of soundness or a concurrency/logic verifier. These checks do not establish termination, freedom from deadlocks/resource exhaustion, correct business logic, or coverage of every code-generation path. No model was used to approve compiler safety.

Relevant primary references: Rust's [borrowing model](https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html), Microsoft's [AddressSanitizer support](https://learn.microsoft.com/en-us/cpp/sanitizers/asan?view=msvc-170), and the documented [`_mul128` intrinsic](https://learn.microsoft.com/en-us/cpp/intrinsics/mul128?view=msvc-170). Zenith implements the restricted rules above rather than inheriting Rust's borrow checker.

## 0.5 typed core and native backend

Both backends now take CheckedProgram, whose construction and fields are restricted to the compiler crate. The checker produces typed local/field places, initialization, explicit moves and source clearing, call-loan markers, drops and structured control flow. The C/native writers do not independently infer ownership. Capabilities are still checked over resolved source, not inferred as a complete effect system in the IR.

- Every record/enum is affine. Partial-move tracking rejects overlapping field uses, whole-value reads/borrows after partial moves, and transfers through borrowed parameters.
- Mutating a field requires a mutable owner or exclusive whole-owner parameter. Nested initializer/call expressions cannot move an owner while another argument borrows it.
- Reinitialization restores the moved field. Branches merge only continuing paths, including match arms. The checker remains conservative about repeatable-loop moves and disjoint-field borrows.
- Exhaustive matches validate variant identity, duplicates, payload binding and coverage. Consumed payloads transfer into the selected arm; nonselected payload storage is never read or dropped as that variant.
- Drop/clone operations follow finite validated type layouts. The C backend has compile-time size assertions. Deep cloning duplicates owned heap fields; task cloning remains forbidden.
- Records/enums cannot contain stored references or recursive by-value layouts. Owned aggregates may cross worker boundaries; references cannot. Worker results were limited to int in 0.5; the typed owned-result contract above applies from 0.22.
- Fresh storage is initialized at the checked IR's defining operation. Source clearing on moves and all required drops remain. Unused slots are not redundantly zeroed at function entry.
- Direct native hot arithmetic uses signed overflow-producing instructions plus explicit failure branches. Division/remainder reject zero and MIN/-1 before the machine operation. Cold arithmetic uses the same checked runtime helpers as C. Array addressing follows the checked index/length comparison.
- The runtime remains responsible for allocation/capacity checks, UTF-8, I/O, threads and heap destruction. Cranelift, its object writer, MSVC linker, CRT and OS join the trusted boundary.
- Windows stack probing is enabled. Generated functions receive native .pdata/.xdata unwind records; no Rust-style panic unwinding or exception recovery is introduced.
- AddressSanitizer remains a C-backend check of application code and runtime. Direct-native application code is not ASan-instrumented; an explicit fast+sanitize request is rejected.
- The source checker always runs before an output receipt can be reused. Native receipts hash typed IR directly, compiler/toolchain identity and output bytes. The existing local-corruption, not hostile-remote-cache, boundary is unchanged.

0.5 validation adds 6 data-type test functions, 6 direct-backend test functions and a cross-module type/privacy test to the original 33, for 46 total. Tests execute nested owned records, partial reinitialization, enum payload returns, worker transfer and deep clone; C versions run under ASan. Native/C differential tests cover output and controlled failure, including both inline/hot and shared/cold arithmetic. Native valid integer-edge results are checked against Rust checked arithmetic. Larger stack frames and Windows unwind sections are exercised.

These tests do not prove that every possible source program is sound. Stored references, recursive heap data, generic ownership, shared mutation and FFI need new analyses before introduction. Resource exhaustion, recursion termination, deadlock freedom and application logic remain separate from memory/alias checking.

## 0.5.1 optimization invariants

- Inlining consumes checked IR, retaining all assertions, overflow branches and index checks. It accepts only scalar/unit results, borrowed or scalar inputs, and no owned temporaries or loops. Expansion is capped at 80 operations per helper, 256 operations per caller and two nested helper levels; recursive calls remain calls. Early returns branch to a private inline exit.
- An inline push tests capacity and invokes the existing checked grow routine when full. It reloads the descriptor after reallocating. The runtime's allocation-size bound makes the subsequent element offset and length increment representable. The source cannot forge descriptors.
- Array descriptors and separately allocated integer buffers have distinct alias regions. Untagged accesses and calls remain conservative. Shared borrowed-array metadata is immutable for the call lifetime. Exclusive metadata is reused only after a transitive checked-IR proof that the function and all its callees cannot resize or replace a descriptor. Unknown/recursive effect cycles stay conservative. Array element contents are never cached as immutable.
- Loop rotation duplicates only a small condition block, keeps the initial zero-iteration test, and evaluates the condition exactly once per attempted iteration. Arithmetic failures in conditions and early-return behavior remain checked.
- Four additional native differential tests cover aliasing, transitive growth, owned replacement, cloned buffers, nested helper returns, evaluation order, contracts, bounds and loop-condition failures. Total: 50 Rust test functions. ASan coverage remains on the C backend, not direct-native code.

## 0.6 field-contract and cache invariants

- Loans are keyed by local owner plus field-index path. Prefix overlap catches parent/child aliases; disjoint paths can coexist.
- Private access analysis refines a conservative whole-owner summary. Calls compose callee footprints through actual field projections; no stored reference can hide an alias. Explicit view contracts are verified against transitive body effects, including requires/ensures.
- Public functions without views retain whole-owner borrowing. A changed public implementation cannot silently narrow its callers' access contract.
- The ABI still passes the enclosing place pointer. The body checker, rather than a runtime view wrapper, enforces the declared rights. Backends do not add blanket noalias assumptions to an enclosing pointer whose read-only field can be shared.
- Both code generators execute disjoint field borrowing and partial-owner destruction consistently. A cross-module explicit view also executes with C AddressSanitizer. Negative tests reject overlapping loans, outside-view access, mutable access through a read-only view, moved fields, reference escape and borrowed worker arguments.
- Background baking accepts only checked IR and writes to shared artifact storage. Retrieval rechecks current source and matches the compiler/toolchain/profile identity before copying bytes. Unsafe source cannot reach a baked executable.
- Native objects and baked executables share the existing corruption-checked, locked LRU envelope with C objects. The cache remains local trusted storage, not an authenticated remote artifact service.
- Cache limits bound retained entries. Active compiler scratch, named output executables, runtime/toolchain files, and compiler-development Cargo storage are separate. Abrupt process termination can leave scratch; this release does not silently purge arbitrary temporary directories.

Validation: 59 Rust test functions, including the original safety/runtime suites, plus the packaged multi-module field-view example. This extends the supported language subset without claiming a proof of Rust-equivalent soundness.


## 0.13 text-view safety

The checker treats `str` as non-copyable, borrowed-only data. Parser/type validation forbids owned parameters, stored views and view returns. Shared loans reserve the backing place before evaluating bounds. Both backends produce a two-word descriptor only after bounds and UTF-8 boundary checks. No unsafe conversion or mutation API is exposed. Views and slice loans use the same lexical cleanup and worker-transfer rules.

Length-bounded text builtins accept views through an explicit whitelist; C-string-based routines do not. `contains` and `print` have bounded view-specific runtime paths. `to_text` is the only new operation producing an independent owner, and cannot satisfy `no_alloc`. Runtime faults are exercised in native, optimized C and ASan builds. A scoped view is not proof of application-level logic correctness, and Anode still supports a much smaller safe-language surface than Rust.

## 0.17 inherent method safety

Method resolution runs before the ordinary view, ownership and effect passes. It selects a nominal target using type hints, then elaborates the receiver into an existing owned argument or shared/exclusive place borrow. Hints do not establish acceptance: the ordinary checker rechecks the complete call, receiver permissions, evaluation order, argument overlap, moves, effects, contracts and thread-transfer constraints. A method cannot create an escaping reference or bypass a declared receiver view. Both backends receive the same existing checked-call IR, with no new method-dispatch runtime.

Implementations are restricted to the type's declaring module; private methods remain private. Shared/exclusive receivers must refer to named places, and exclusive loans reserve their footprint before later arguments are evaluated. A mutable receiver cannot be called through shared access. Owned receivers consume their argument once. New native/C/ASan tests exercise these rules and preserve overflow and precondition failures. The focused dot/explicit call audit produces identical checked IR. This section extends the historical safety notes above; the 0.17 milestone had 142 compiler tests and 22 LSP tests.

## 0.18 iteration safety

Integer for-loop bounds are evaluated once in source order and checked as int before the existing checked while IR is emitted. The immutable user binding copies the current index; the checked index increment occurs before the user body, preserving continue semantics. An exclusive upper bound permits the final MAX-1 to MAX increment without overflow. Equal or descending ranges do not execute the body. Hidden compiler names contain a character unavailable to source identifiers and are omitted from typed-hole scopes.

Borrowed vector traversal validates its named source, nominal element type, liveness and requested mutability before iteration, including empty loops. Each body uses the existing Element loan: the entire backing vector is protected while its alias is live, and exclusive element access also blocks backing reads. No source expression with arbitrary side effects is accepted as an iterator. The captured length cannot become stale through body-driven vector growth because mutation/removal/movement of the backing vector is forbidden. Disjoint fields remain available. References cannot escape, and existing outer-owner loop transfer restrictions remain in force.

The native and C backends receive existing loops, typed element addresses, loans and cleanup. Return/break/continue retain owner destruction and worker joining. Method/view/effect analyses visit loop bounds, owners and bodies; range bindings and vector aliases have the appropriate types during method resolution. Ten new tests exercise both successful and rejected programs, native/C/ASan behavior, maximum-boundary arithmetic, scoped navigation/rename, runtime failures and cross-module capability checking. The prior out-of-loop jump rejection tests are preserved, with their diagnostic expectation updated from enclosing while to enclosing loop.

