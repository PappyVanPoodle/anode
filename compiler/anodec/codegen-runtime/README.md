# Bootstrap C support

`runtime.h` and `runtime.c` are frozen verbatim from `baselines/0.22.0.zip`:

- runtime.h SHA-256: 4D7D102DA346A0F79DC2F16853BEE43F9C666EC9757521B331769FC7CA2DCABD
- runtime.c SHA-256: 7E1F63F703EB0BC41DF0474DB5338CA0AAA46E3F4180D29B318D7ED51C1466C6

They preserve the packaged 0.22 seed's checked text/UTF-8, arithmetic and system ABI while the current compiler runtime changes independently.

`bvalue.h` is this example's trusted generic value support. It contains no source AST, program interpreter, cached generated C or fixture-specific program/output. `src/cgen.an` generates every ordinary source function, statement and control-flow path, including the complete seven-module compiler. The header supplies finite tagged owners, checked indexing, deep cloning, clearing moves, recursive destruction and selected builtin operations. Maps use owned key/value pairs and linear lookup. Scoped aliases point into live owners; shared read-only arguments and scalar snapshots may overlap, while mutable-reference conflicts and overlapping owner moves remain rejected. Read-only builtin places use scoped pointers only while sibling evaluation preserves their owners. Text regions use independent checked slices.

The frozen runtime's recoverable IO tags are 1 (Ok) and 2 (Err). The value support validates those tags and translates them to the bootstrap's zero-based enum tags. Both branches are exercised by the generated shared-reference/IO fixture and by actual source loading in both compiler generations. MSVC links reserve a 64 MiB stack for the compiler's boxed temporary frames, while source nesting and generated call depth remain checked.

All successful generated applications check that their live owner and call-frame counters return to zero. MSVC ASan builds instrument both emitted functions and this header/runtime. These checks are useful evidence, not a replacement for the complete seed checker or a full formal safety audit.

## v0.25 generic support provenance

The current bvalue.h adds generational arenas and scalar ids, authored locally for this example. BO owns optional generation/live arrays and a live-element count; ids contain only 32-bit slot and generation values. Same-type ids are relative to the supplied owner: distinct same-type instances can have equal live ids. Deep arena clones preserve slot/generation values with independent storage, as required by the 0.24 snapshot behavior. Free destroys one live payload once and advances its generation; exhausted generations retire slots and reusable slots follow the seed's LIFO free list; stale/invalid checks do not touch a payload. Arena destruction drops every remaining live owner. Existing checked value operations dispatch arena get and clone_get through slot/generation validation. Id map keys and equality compare both components.

The try helpers only validate zero-based result tags and transfer payload ownership. cgen.an emits the actual success/failure branch and all lexical/temporary cleanup; there is no source interpreter in the support layer. Original runtime.c and runtime.h bytes are unchanged, and prior evidence snapshots preserve the older bvalue.h. Verification binds the current header and executes real arena/try native and ASan fixtures.

## v0.26 column and span support provenance

The new BSOA owner stores exactly one independent BSEQ column per record field, with a separate row count. Rows are never stored in an AoS record array. Push moves record fields into columns; clones own independent column arrays; pop reconstructs an owned record; replace and whole-row assignment destroy and replace each column cell. BROW is a nonowning column owner/index descriptor; BREF is a nonowning selected-element pointer. Record field access resolves these proxies while preserving scalar and owning-field behavior. Scoped references never decrement the backing owner's accounting.

BVIEW holds a nonowning contiguous B pointer and element count. Nominal regions validate signed start/count and count <= length-start before pointer arithmetic. Nested regions reuse the same backing allocation. Legacy integer region bounds remain start/end. Text regions keep the previous frozen-runtime checked-copy implementation. Pair and swap helpers check bounds/distinctness before creating aliases. The frontend rejects SoA regions, raw/stored/escaping/cloned/resized vector spans and conflicting backing-owner access. The representation probe counts header allocation calls and verifies actual separate columns, independent clones, exact nested pointer offsets, empty views, and zero allocation during span selection/swap/replacement under native and ASan builds.

All additions live in bvalue.h and real src/cgen.an output; runtime.c and runtime.h remain byte-exact frozen0.22 files. There is no generic AST executor, cached result, target-source shortcut or seed subprocess. The preserved0.25 seed compiles the compiler sources; explicit behavior-equivalent0.25 fixtures provide oracles for target syntax that the seed cannot parse.

## v0.28 integer support

BI.slot carries the canonical numeric type code (width, plus100 for unsigned). b_intrinsic uses uint64_t modular arithmetic and count masking, restores signed narrow values with unsigned extension and memcpy through zn_num_signed, and preserves this code across values/clones/storage. Existing frozen zn_num_cast/zn_num_binary provide checked conversions/ordinary arithmetic; unsigned comparisons/printing use raw unsigned bits. Runtime files stay byte-identical. No host0.28 call is present in frontend implementation.

## v1.2 process support

`bprocess.h` is new, locally authored trusted support for `process_run`. It is included only by generated C that calls `process_run`: `src/cgen.an` emits `#include "bprocess.h"` as a first-use helper, so every other program's C and headers are unchanged. It launches with `CreateProcessW` directly, with no shell. MSVC argument quoting is explicit, and the inherited-handle list is restricted to NUL stdin and the two capture pipes. The child is assigned to a kill-on-close job object, and both pipes drain concurrently, each capped at 16 MiB. Captured text must be valid UTF-8 without NUL. Nonzero exits are returned as `ProcessRun::Output` with the exact unsigned 32-bit status; launch, capture and timeout failures return `ProcessRun::Error`. The deadline is 600,000 ms, because the Stage-3 `--build-exe` compiles the whole compiler with MSVC `/O2`. The fixed-array and big-endian wire helpers (`zn_v120_*`) are emitted into generated C by `src/cgen.an`, like the v0.29 helpers. `bvalue.h`, `runtime.h` and `runtime.c` are byte-unchanged.
