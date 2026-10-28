# Contract-pinned hole batches (Anode 0.24)

The public Rust operations are `holes::skeleton_receipt(&Project)`, `holes::verify_batch(&Project, &Value)` and `holes::commit_holes(&Project, &Value)`, all returning `Result<Value>`. Their agent method names are identical. `verify_batch` always writes zero source files. `commit_holes` defaults to a zero-write dry-run and requires the literal JSON boolean `apply:true` to write sources. The CLI integration calls `commit_holes` for `anode verify PROJECT --batch REQUEST.json --json`.

Start with `{"method":"list_holes"}` or `{"method":"skeleton_receipt"}`. Both supply `skeleton_sha256` and `snapshot_sha256`; `list_holes` also supplies the slots and their fingerprints. The skeleton's `canonical` object uses schema `anode.contract-skeleton.v1`: project-relative module names and imports; all public and private declaration names, visibility and field layouts; function and method signatures including borrow modes and field views; sorted effect attributes including `bounded`; advisory intent attributes; location-free requires/ensures expression trees; and unresolved hole IDs, declared types, intents and contract/body placement. Function body tokens, comments, and source locations are excluded except hole signatures. Manifest bytes are pinned exactly, including entry and capability configuration. JSON object keys and module paths are deterministic. This contract identity is never a proof of unresolved code.

`snapshot_sha256` hashes an unambiguous JSON encoding of the exact manifest and every project-relative source path and source byte string. Receipts also expose SHA-256 values for the manifest and individual files. Existing FNV `snapshot_fingerprint`, `file_fingerprints` and `hole_fingerprint` fields remain compatible with the older hole API; new batches additionally require the SHA-256 pins.

```json
{
  "method": "commit_holes",
  "skeleton_sha256": "64 lowercase hexadecimal characters from the receipt",
  "snapshot_sha256": "64 lowercase hexadecimal characters from the receipt",
  "apply": false,
  "candidates": [
    {
      "id": "crate::sum#0",
      "hole_fingerprint": "from this slot's listing",
      "replacement": "left + right",
      "kind": "expression"
    }
  ]
}
```

Submit 1–32 distinct slots. Optional legacy snapshot/file fingerprints and exact compiler-coordinate `range` guards must match when supplied. A new batch accepts only expression replacements: it wraps each expression in parentheses within the assigned hole's exact half-open UTF-8 span. It never appends helper declarations or modifies other source bytes. Block/helper candidates remain available through the older read-only `verify_holes` API. Contract-expression holes are inspectable but locked against replacement. Stale pins, duplicate/unknown IDs, nested holes and syntax escaping the assigned expression are rejected.

The report contains per-candidate `independent` results, the `composed` result, descending-byte-order `edits`, complete proposed `files`, `exact_hole_only`, `commit_eligible`, `writes`, and `applied`. Independent proposals are checked against the same original snapshot; the composition receives ordinary full-project ownership, privacy, capability, view, effect and contract type checks. Results with remaining holes stay `conditional`, `validated:false`. The diagnostic checker returns no executable proof or backend IR. Only a normally checked, fully filled composition can become eligible for application. User tests are checked without being executed.

An optional `verify_contracts` options object invokes the existing finite checked-IR verifier after normal checking succeeds. Execution still defaults to false; `execute:true` must be explicit inside that object. Empty `functions` selects every function completed by this batch. Budgets remain `max_cases` 1–4096, `max_steps` 1–1000000, and `timeout_ms` 1–10000. The report always states `universal_proof:false`; successful samples provide finite evidence. Counterexamples, runtime faults, unsupported execution or exhausted evidence budgets prevent an explicitly requested execution from becoming commit eligible. Incomplete compositions defer verification; diagnostic hole assumptions are never executed. IO, foreign calls, threads and unsupported aliases/arena values are never executed by this evaluator.

Application accepts file-backed projects with unchanged disk snapshots. Agent applications reject overlays and patches. Managed projects reload their root and standalone sources reload their original entry file, preserving manifest/capability checks; ambiguous dual-manifest roots require resolution before application. All loaded source and manifest bytes are checked before locks, after acquiring cooperative project/per-source locks, and after staging. Lock metadata is fully written and synced before its atomic publication through non-overwriting hard links; an interrupted publisher cannot leave an empty blocking lock. Filesystems without hard-link support fail before source writes. A second cooperating writer cannot apply the old receipt. Other editors must honor these locks or keep the files quiescent.

Every changed source has a synced original backup and proposed replacement in a confined transaction directory. The journal records root, owner PID, source/manifest receipt and original/proposed hashes, then is atomically published before replacement begins. Each same-filesystem file rename is atomic. Multiple files **do not have simultaneous OS-wide atomic visibility**, and a process or OS crash can leave a partial set. Ordinary replacement errors restore earlier files; failed rollback preserves the journal and locks. After all replacements succeed, source locks are released before deleting rollback data, so a cleanup crash can leave private artifacts without stranding a lock whose backups are gone. File contents are synced; directory metadata durability depends on the operating system. A read-only Windows destination is rejected during preflight; a destination held without delete-sharing exercises rollback after an earlier file changed.

The public recovery operations are `holes::inspect_hole_transaction(&Project, &Value)` and `holes::recover_holes(&Project, &Value)`; agent names match. They inspect the pending project's lock, and an optional `journal` must be its exact `.anode-hole-batch-...` basename. Recovery defaults to a read-only inspection. `apply:true` requests rollback to the original snapshot, serializes recovery with a kernel file lock, and refuses a live owner PID. Its zero-byte `.anode-recovery.lock` gate remains in the project to prevent unlink/reopen races; process death automatically releases the kernel lock. OS process-query failures and PID reuse are conservatively treated as a live owner. It checks confined canonical paths, bounded artifact sizes/counts, original/proposed source hashes, unchanged untouched modules and manifest, and the SHA-256 of the entire reconstructed original snapshot before restoring bytes. A changed external file, corrupt backup or foreign lock causes refusal. Successful recovery removes only the transaction's known artifacts and matching abandoned locks. A crash before the journal is prepared can also release its abandoned preparing locks after validating the unchanged source receipt.

```json
{"method":"inspect_hole_transaction"}
{"method":"recover_holes","journal":".anode-hole-batch-1234-...","apply":true}
```

The editor query `anode/holes` is the read-only alias for `anode/listHoles`. It returns typed holes and the complete skeleton receipt over unsaved project overlays, retaining UTF-16 `lsp_range`, per-document versions, cancellation and stale-response guards. `anode/verifyHoles` uses pinned `verify_batch` when `skeleton_sha256` is provided and retains the older read-only behavior otherwise. Successful plans include a versioned workspace edit for review. All editor synthesis requests reject `apply:true`; direct or nested contract execution requests are rejected. `arena`, `id(Arena)`, arena constructors, `alloc`, `free`, `contains_id`, `get`/`get_pair` aliases and `bounded` are recognized by compiler navigation and VS Code syntax highlighting.

Dedicated evidence is in `tests/milestone_v024_agent_lsp.rs` and the `v024_` protocol tests. Worker commands use process-local `RUSTC_WRAPPER=''`, `CARGO_TARGET_DIR=%LOCALAPPDATA%/ZenithLanguage/target-v024-agent-lsp`, `--locked --offline --jobs 2`; the protocol tests point `ANODE_TEST_COMPILER` at that target's `debug/zn.exe`. Earlier release logs and targets are retained.
