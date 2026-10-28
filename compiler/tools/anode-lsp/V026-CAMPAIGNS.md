# Pinned campaigns and package navigation (0.26)

The Rust entry points are `holes::plan_campaign(&Project, &Value) -> Result<Value>`
and `holes::verify_campaign(&Project, &Value) -> Result<Value>`. They operate on
the complete loaded project, including dependency sources and manifests.

An agent request can use `method:"plan_campaign"` or `method:"verify_campaign"`.
The equivalent user envelope is `command:"campaign", action:"plan"|"verify"`.
An omitted action selects verification when `responses` is present, otherwise
planning. `method:"campaign"` is also an alias. A conflicting method, command,
or action is rejected. Existing method requests retain their meanings.

```json
{"command":"campaign","action":"plan","max_workers":4}
```

Planning accepts integer `max_workers` from 1 through 16, default 4. All holes
in one function remain together. Groups are sorted by descending hole count
then function name and assigned to the least-loaded worker, with worker order
breaking ties. The planner emits at most the requested number of nonempty
workers and supports up to 4096 holes. Every hole belongs to exactly one worker.
Distinct workers have disjoint expression spans and function bodies; they may
edit distinct functions in the same file. Full composition is still required.

The `anode.campaign.v1` receipt includes a deterministic `campaign_id`, both
`skeleton_sha256` and `snapshot_sha256`, `max_workers`, `total_holes`, source
fingerprints and `workers`. Each worker has `worker_id`, both SHA pins,
`hole_ids`, `files`, `root_writable`, and a JSON `brief`. The brief includes
exact hole IDs and UTF-8 ranges, hole fingerprints, expected types, required
field views, effect bounds, requires/ensures, intent and checked context when
available, plus signatures callable from the assigned scope. These signatures
are context; ordinary checking determines the validity of actual helper calls.
Dependency assignments are read-only proposals. Contract holes are visible but
cannot be replaced, because contracts are locked boundaries.

Return the full unchanged plan as `campaign`. Worker responses must carry their
own identity and SHA pins; each candidate uses the fingerprint from its brief.

```json
{
  "command":"campaign",
  "action":"verify",
  "campaign":{"schema":"anode.campaign.v1","campaign_id":"...","max_workers":4,"skeleton_sha256":"...","snapshot_sha256":"...","workers":[]},
  "responses":[{
    "worker_id":"worker-1",
    "campaign_id":"...",
    "skeleton_sha256":"...",
    "snapshot_sha256":"...",
    "candidates":[{"id":"crate::answer#0","hole_fingerprint":"...","replacement":"42"}]
  }],
  "verify_contracts":{"emit_test":true,"max_cases":128,"max_steps":100000,"timeout_ms":1000},
  "apply":false
}
```

The abbreviated example above is schematic: use the complete returned plan,
including its actual worker briefs. Verification reconstructs the plan from the
current project and compares its identity and assignments. It rejects stale
sources/manifests, modified assignments, foreign or duplicate workers, foreign
campaigns, wrong-worker holes, duplicate/overlapping holes, stale fingerprints,
and source ranges differing from the pinned exact span. Workers submit one
response each; a response may contain a nonempty subset of its assignments.
Missing responses/holes remain explicitly incomplete. Expression candidates
only; body/helper declarations outside assigned spans are rejected.

Every candidate is independently checked. Each worker batch and the full
composition are also checked against ordinary type, ownership, loan, view,
effect and capability rules. An `anode.campaign-receipt.v1` result contains
`workers`, `independent`, `composed`, `missing_holes`, proposed `files`/`edits`,
`contract_evidence`, `commit_eligible`, `applied`, `writes`, and `status`.
Statuses are `sampled`, `planned`, `incomplete`, or `failed`. Unresolved holes
defer execution; typed diagnostic assumptions are never evaluated.

Bounded pure verification defaults to `execute:true` for campaigns and selects
every newly completed function, including functions without written contracts.
Additional requested functions may extend this set. Budgets are global cases
1..4096, steps per case 1..1000000 and elapsed timeout 1..10000 ms. Unsupported
IR, scoped aliases, IO, FFI, channels, threads and subprocesses are skipped;
`process_run` can never execute through this evaluator. Incomplete grids and
unsupported functions prevent an execution-requested commit. `execute:false`
explicitly requests static planning. Evidence is finite and always reports
`universal_proof:false`. Witness tests use the same ordinary-checked proposals
as 0.25: successful witnesses require a completed accepted grid; actual failures
describe expected failing tests. Unsupported/incomplete samples invent none.
Witnesses describe the proposed composed snapshot and are never inserted by a
campaign commit.

Default `writes:0` prepares a reviewable result. `apply:true` applies only an
eligible complete batch using the existing fresh-disk preflight, cooperative
project/source locks, synced backups/journal, ordinary-failure rollback and
explicit abandoned-transaction recovery. Root source files only; dependency
edits refuse commit. Source overlays and patches cannot be applied. Multi-file
visibility is not OS-wide atomic and crash recovery can require the retained
journal; the full `transaction_guarantees` receipt remains authoritative.

The coordinator CLI is `anode verify [PROJECT] --campaign [REQUEST.json]`.
Without a request file it plans with four workers. A response request verifies;
an explicit action must agree. `--batch` and `--campaign` are exclusive.
`--emit-test` and explicit evaluator budgets merge into `verify_contracts`;
`--plan` selects `execute:false` for verification. `--function` is rejected for
campaigns so all completed functions stay selected. JSON status conventions:
exit 0 success/planned/eligible/applied, exit 1 invalid/failed/malformed/stale or
other errors, exit 2 conditional/incomplete/exhausted evidence.

The LSP advertises `anode/planCampaign` and `anode/verifyCampaign`. Parameters
are the same plus `textDocument:{uri,version}`. Captured package overlays and
document versions are checked before and after the asynchronous query;
cancellation returns -32800 and source/manifest changes return -32801.
`apply:true` is rejected with -32602. Planner `editor_holes` is a separate UTF-16
sidecar, preserving exact worker briefs. Verified root-source alternatives get
a versioned `workspace_edit`; dependency alternatives receive none. Witnesses
retain their composed-snapshot source receipt and must be reviewed against that
snapshot. Neither request applies a workspace edit. The VS Code commands open
the plan as JSON and review a chosen campaign-response JSON file in the output.

Cross-package definitions/references use actual dependency file URIs while
snapshot keys remain `@dependencies/alias[/child]/src/file.an`. Package-local
`crate::`, dependency aliases, nested modules, data/field/method references and
vector regions share the loader's namespaces. Rename rejects a dependency
declaration or any rename whose occurrences include dependency sources, even
when initiated from a root reference. Root-only renames retain the complete
recheck and capture/rebinding checks. Vector `region(owner,start,count)` aliases
and `&[VectorName]` parameters infer their element and field symbols, including
nested regions and CRLF/UTF-16 protocol coordinates. Existing integer/text
regions retain start/end semantics. `process_run`, `ProcessRun`, `ProcessOutput`,
their variants and fields have semantic hover, reference support and highlighting;
builtins have no source definition and cannot be renamed. Generic collection
and text builtins keep their legacy null navigation symbols; channel builtin
references remain receiver-gated.
