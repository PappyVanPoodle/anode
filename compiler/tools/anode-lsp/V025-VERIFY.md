# Bounded live verification and witness tests (0.25)

`anode/verify` runs the compiler's finite, pure checked-IR evaluator against the
active source overlays. It uses no native execution, console, files, FFI or
threads. Unsupported operations and IO (including channel operations) are
reported as skipped/incomplete. It never claims a universal proof or backend
equivalence. `anode/verifyContracts` retains its existing static planning API.

```json
{"jsonrpc":"2.0","id":25,"method":"anode/verify","params":{"textDocument":{"uri":"file:///C:/app/src/main.an","version":7},"function":"crate::identity","emit_test":true,"max_cases":128,"max_steps":100000,"timeout_ms":1000}}
```

Use either `function`, `functions`, or a UTF-16 `position` inside a function.
Omitting all three selects contracted functions. `execute` defaults to true for
this request; false only plans and emits no test. Limits are checked by the
compiler: cases 1..4096, steps per case 1..1000000 and timeout 1..10000 ms.
The main protocol thread remains responsive during evaluation. Cancellation
suppresses the result; the worker finishes within its evaluator budget. A
document/source/manifest change while the request is queued or executing returns
ContentModified (`-32801`); cancellation returns `-32800`. Source patches and
`apply:true` are rejected. The compiler's request pins cover all package sources
and dependency manifests.

The usual response envelope contains `result.status`, finite sample `functions`,
`totals`, `sample_diagnostics` in UTF-16, `writes:0`, `universal_proof:false`,
source/snapshot fingerprints and optional `test_candidates`. A candidate is one
deterministic ordinary-checked declaration per selected function: its first
actual counterexample/runtime fault, or its last accepted boundary sample only
after that function completes its grid. Rejected preconditions, incomplete
successful sampling and unsupported code never invent a witness.

Generated source uses the supported syntax `test anode_witness_HASH() { ... }`.
It constructs its actual sampled inputs and invokes the original function in
its declaring module. Scalar successful results also receive an equality
assertion. Inputs include exact integer widths, finite floats and supported
owned records/enums/collections. Generated source is checked in memory through
the normal complete checker, including test bodies. Failed validation returns
diagnostics and no insertion edits. It does not execute the generated test.

Each candidate includes `witness_status`, `expected_outcome`, exact `source`,
`inputs`, `observed_result` or `fault`, `case_index`, `file_fingerprint`,
`snapshot_fingerprint`, cryptographic `snapshot_sha256`, and an EOF `edits`
receipt. Counterexample tests have `expected_outcome:"fails_contract"`; runtime
fault tests have `"runtime_fault"`. These tests are expected to fail when replayed
against the unchanged program. They are not passing regression tests.
`"passes_for_sample"` describes finite successful evaluator evidence only.

The LSP validates captured source fingerprints and EOF ranges again, then adds
a versioned `workspace_edit` to each checked candidate. It never applies it.
Dependency source keys are `@dependencies/alias[/child]/src/file.an`; their edits
use the actual file URI and its open-document version. The agent accepts the
same `emit_test:true` on `verify_contracts` (or `verify`) and defaults to no test
emission. `VerificationOptions.emit_test` defaults to false. CLI
`anode verify --emit-test` consumes this same API and remains zero-write.

Channels and `try` now participate in compiler-backed hover, definitions,
references and safe rename. `try` and contextual builtins show semantic hover
details but have no source definition and cannot be renamed. A user function
shadowing a contextual builtin retains ordinary function navigation. The VS
Code grammar highlights `channel`, `try`, receive/send/close operations and
the `FileRead` alias. The **Verify Function and Propose Witness Test** command
shows structured evidence/proposals without applying changes.
