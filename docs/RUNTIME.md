# Runtime and bytecode

The compiler emits instructions directly; textual assembly is a separate diagnostic
tool. Cog executes on an interpreter with a stack VM by default. An experimental,
opt-in Cranelift JIT can compile a limited set of synchronous loops.

The frontend applies finite input budgets: 16 MiB per source, 64 MiB for project
source snapshots, 262,144 tokens per source (including interpolation), 100 frontend
diagnostics, 64 recursive parser levels and 128 expression-tree levels. Package
discovery limits packages to 256, modules to 4096 and namespace nesting to 64.
The Rust API exposes `limits::CompilerLimits` and `frontend::parse_with_limits`;
recursive depth settings can be lowered but cannot exceed the safety ceilings.
These compiler limits are independent of optional runtime execution settings.
Loaded/generated syntax and comptime allocation have conservative 64 MiB accounting
budgets. KDL configuration is limited to 256 KiB and 64 nesting levels, including
nested comments and slashdash prefixes, before entering the KDL parser. These
budgets do not establish a whole-process memory guarantee.

```text
UTF-8 source → recovering lexer/parser → module/name resolution → semantic checking
             → direct instruction emission → .cogb → validated loader/linker → VM
```

For a contributor view of these phases, including where compile-time evaluation,
specialization, optimization, and verification fit, see the
[compilation path](CONTRIBUTING.md#compilation-and-execution-path).

## Executable artifacts

One `.cogb` links the chosen entry point and imported module/package code. It runs
without sources, manifests, or recompilation. Artifacts are experimental: rebuild
after incompatible format changes; older versions are rejected.

The current format is v38: magic `COG\0`, little-endian `u16` version and flags,
`u32` import count and imports, a `u32` callable count and callable descriptors,
then `u32` instruction count and explicit instruction encodings, followed by a
source-map table. Each map entry stores a half-open instruction range and a
length-prefixed UTF-8 file name with one-based line and column coordinates. Each
callable descriptor stores its entry, parameter count, capture count, async flag,
an optional parameter contract table, and an optional result contract. V15 added
async-call, await, and spawn instructions for task execution; V16 adds async timer,
file-read, and process-run instructions; V17 adds asynchronous filesystem operation
instructions; V18 adds asynchronous UTF-8 text file instructions; V19 adds
asynchronous file copy and rename; V20 adds task cancellation/check instructions;
V21 adds the cooperative `yield_now` task instruction; V22 adds timed task waits;
V23 adds two-task selection; V24 adds selection over a task array; V25 adds
asynchronous line reads and file append instructions; V26 adds bounded TCP exchange;
V27 adds persistent TCP connect, read, write, and half-close instructions; V28 adds
TCP listener and accept instructions; V29 adds bounded process-run instructions;
V30 stores an async marker on native imports so a call can create a plugin `Task[T]`;
V31–V34 extend process and TCP operations with stdin transfer, persistent stream
management, listeners, and bounded read-to-EOF; V35 adds exact-length reads for
managed files and TCP streams; V36 adds partial writes for managed files and TCP
streams.
When present, the parameter table contains one
optional runtime type per parameter and is omitted when every parameter is dynamic.
Entries are unique and sorted; they partition startup and function regions.
V13 stores declared tuple-slot and nominal record-field contracts in typed aggregate
instructions, character constants as validated `u32` Unicode scalars, and checked
unsigned 64-bit constants and runtime types. V14 adds canonical signed little-endian
BigInt constants, a BigInt runtime type, and a verifier value-type tag. Rebuild all
older artifacts.
Native imports store a length-prefixed UTF-8 symbolic name,
ABI version, parameter count/types, successful-result type, and async marker. Imports are sorted
by identity and linked once. Host pointers and Rust enum layout are not serialized.

ABI v3 native tags are any=0, int=1, str=2, bytes=3, handle=4, nil=5, number=6,
array=7, record=8, float=9, bool=10, strbuf=11, Error=12, char=13, and uint=14.
ABI v3 exposes BigInt values as opaque handles through `Any`. A plugin can
forward the handle within its call. The ABI has no BigInt payload for inspection
or construction.
These are runtime descriptors, not the available source annotation grammar. Array values
carry recursive element type metadata, and runtime mutations enforce that
invariant. The format limits
imports to 4096 and parameters per import to 64. Files are limited to 128 MiB,
aggregate static strings to 64 MiB, and constant/type nodes to 1,000,000;
instruction/constant/slot bounds are also enforced by the codec.
Runtime array and tuple type descriptors are limited to a nesting depth of 64,
1,000,000 aggregate type nodes, and 64 MiB of aggregate type names, matching the
bytecode verifier's descriptor budgets. VM construction or mutation that would
exceed a budget produces a catchable `Error::ResourceLimit`. This bounds
type-descriptor growth; it is not a general limit on the size or depth of values
inside explicitly dynamic arrays.

`CallNative(index)` consumes its arguments and produces one result. Synchronous
imports dispatch through a linked descriptor/function pointer; async imports create
a cold `Task[T]` consumed by the same cooperative scheduler as Cog async functions.
SDK callbacks retain owned argument roots, and dynamic plugins use a per-call
handle arena and ABI v3 wakeup contract through [C ABI v3](SDK.md).
Recoverable native failure becomes Error; invalid VM state and crash reach the host
as terminal failures. The compiler-private cleanup merge uses this same ABI.

The loader checks tags, sizes, UTF-8, constant nesting, native indices, import
signatures, callable entries, direct-call/capture arities, function-region branches,
operand heights at control-flow joins, definite local initialization, and descriptor
shape. Verification has bounded analysis work. The VM checks annotated arguments
and results at direct and indirect call boundaries; mismatches produce the language's
recoverable `Error` value, and existing `Error` values continue to propagate through
typed boundaries. Full static value-type verification remains planned. Source
maps carry file/line/column metadata without embedding source text. The verifier
also tracks bounded value, callable, capture, aggregate-field, and exact tagged
variant facts. Element facts of a freshly constructed aggregate follow it into
frame locals until an alias of it may be written or escape the frame; this
partial analysis does not prove general type safety. The CLI adds
the mapped location to terminal runtime errors, and embedders can inspect
`RuntimeContext::last_source_location`; catchable Errors handled by Cog are not
annotated. This does not provide OS isolation or a total memory guarantee. Runtime buffers, handles,
reference graphs, cells, Error values, and open builders cannot be serialized as
constants. Artifacts are unsigned; no publisher authenticity is implied by loading.

## Experimental Cranelift JIT

The CLI enables JIT with `cog run --jit <file.cog>`; the Rust SDK exposes
`RunContext::with_jit(true)`, `with_jit_profile_path`, and `with_jit_debug`.
JIT is disabled by default. Functions must be synchronous and pass bytecode,
type, stack, and OSR checks. The current lowering covers checked `int` and
`uint` arithmetic, `float` addition/subtraction/multiplication/division,
same-type comparisons for these numeric types, exact mixed `int`/`uint`
comparisons, `char` ordering, boolean equality, and boolean branches. Other
mixed numeric operations, float remainder, and unsupported instructions return
to the interpreter at their bytecode position. Primitive type tests are lowered
when their runtime tag is represented directly; struct type tests resume in the
interpreter. Strings, BigInts, Errors,
aggregates, and other opaque values can remain rooted across OSR. The call bridge
runs synchronous Cog and native calls through the current execution context and
can resume machine execution with the returned result. It also covers indexing,
indexed writes, and length using shared VM checks. Returned Errors resume at
the following instruction so that VM observation and propagation run once.
Runtime metering limits also force interpretation so native code cannot bypass
step, stack, or object quotas.

APE starts with up to three probe compilations, measures compile and native
execution time, and then uses a conservative shared speedup estimate to consider
new functions. Compile tokens are a CPU-share budget, not a hard compile-latency
limit: Cranelift compilation is synchronous, and an oversized compile is recorded
as budget debt that later refills repay. Heat and active-window counters belong to
the current run and reset after an idle gap; they are not loaded from the profile.
The profile is advisory and keyed by bytecode, Cog ABI and format, Cranelift
version, target triple, and CPU features. Invalid measurements, malformed JSON,
mismatched profiles and profiles larger than 4 MiB are ignored. The CLI stores it under the project's
`target/.cog-ape-v2.json`.
The machine-code cache is limited to 512 functions and 64 MiB of executable
memory per execution plan. This is an experimental optimization, not a sandbox
or a guarantee that every eligible function will be compiled.

`cargo run --locked --offline --release --bin bench_jit -- [iterations] [samples]`
compares the same scalar, call, callback, collection, and Mandelbrot workloads
through the VM and JIT. It separates the first JIT-enabled invocation from
retained-plan runs, checks output equality, and stores temporary profiles only under
`target/jit-bench/`. Retained-plan timings measure adaptive JIT-enabled execution;
the APE may still choose the interpreter for some or all of a sample. See
[`JIT_DESIGN.md`](JIT_DESIGN.md) for execution contracts. The repository
keeps host-specific measurements in its development evidence.

## Embedding

Use the owned [Rust SDK](SDK.md) for embedding and native registration. The
lower-level modules below remain compiler/VM implementation interfaces.

The Rust library exposes `project::compile_source`, `bytecode::encode/decode/read/validate`,
`RuntimeContext`, `VmCapabilities`, `RuntimeLimits`, and
`interpreter::run_with_context`. See [tests/runtime.rs](../tests/runtime.rs) for
custom inputs/writers, failure injection, and restricted execution examples.

`project::compile_source` compiles an in-memory source and supports the built-in
`@!include("std")` directive. Other dependency aliases require filesystem
project loading through `project::load` or `project::prepare`, which reads
`dependencies.kdl`; unsupported declarations produce located diagnostics.

Use `Loaded::compile()` after project loading to retain package/file-mode
build-input bases. In-memory embedders can supply `build_inputs::BuildInputs` to
`project::compile_source_with_inputs`; the ordinary source API captures process CWD.

RuntimeContext owns managed resources and input buffering and borrows the output
writer. The writer must implement `Write + Send`, and a supplied input reader must
implement `Read + Send`; async standard-stream operations borrow them in scoped
workers. A blocking stdin read cannot be interrupted and can delay task cancellation
or execution shutdown until the host read returns. RuntimeContext is intended for
one execution. Handles contain context identity, a slot,
and generation; they are not OS descriptors or pointers. Slot reuse does not revive
stale handles. Runtime quotas default to `None`, including `max_buffer_bytes`,
`max_resources`, and `max_numeric_digits`. When configured, the buffer limit caps
buffer-related operations, the resource limit caps live resource slots, and the
numeric digit limit caps BigInt values and JSON integer digits. These are VM
settings, not a total VM memory quota.

Embedders can optionally set `max_buffer_bytes`, `max_resources`,
`max_numeric_digits`, `max_steps`, `max_operand_values`, `max_call_frames`, combined
local/global `max_slots`, and `max_heap_objects` on `RuntimeLimits`; all default to
`None`. BigInt pushes, arithmetic, conversions, and JSON conversion enforce the
numeric digit limit. The object limit
applies to live managed objects in the current thread's shared Cog heap; `Value`
handles retained by an embedder are roots.
The VM traces cycles at allocation checkpoints and execution exit, and releases
unreachable graph edges iteratively. If the top-level result contains managed
values, the runner also collects cycles held only by that discarded result.
Collection snapshots enumerate registered
objects rather than scanning retired slots left by an earlier heap peak; slot
and free-list capacity can still be retained for reuse. Bulk array constants are
checked against the remaining object quota before construction, so one instruction cannot exceed that
configured count. `json::parse` reserves object slots for arrays and records
before descending into their values and may collect cycles once when the quota is
reached. The count is not a byte quota and does not
bound all host allocations or blocking calls. Exhaustion terminates execution and
finishes host resources; it does not run Cog defer blocks or interrupt blocking
native calls. In-memory programs with calls must supply `CompiledProgram::callables`;
execution rechecks call signatures and frame/branch boundaries even when decoding
is bypassed.

Every managed-object registration through `HeapRef::try_new` while
`interpreter::run_with_context` or `NativeImport::call` is active checks the
quota before adding a registry slot. This includes direct native API calls by
an embedder. At capacity the allocator first collects unreachable cycles; if
the heap is still full, allocation fails without increasing the live-object
count. Nested executions share the thread's heap and retain the stricter active
quota. Core VM and builtin paths use the fallible constructor. Embedding code
that constructs values directly should use `try_new` to receive quota failures;
the convenience `HeapRef::new` panics when its allocation fails. This limit
still does not bound payload bytes or other host allocations.

Rust code constructing graph values uses `cog::heap::HeapRef::new` for managed
storage, for example `Value::Array(HeapRef::new(ArrayValue::new(values)?))`.
Keeping a cloned `HeapRef` or `Value` keeps that object graph alive. Embedders
that create/drop cycles outside `run_with_context` can call
`cog::heap::collect_cycles()` to reclaim unreachable cycles on that thread.
`ArrayValue::borrow` and `try_borrow` expose read-only element borrows. Use its
checked mutation methods (`set`, `push`, and `append_all`) to preserve element
contracts, bounded type metadata, and iterator invalidation; the backing `RefCell`
is private. A shared array borrow prevents mutation until the borrow is released.
Public mutable values use checked interior borrows at VM and native API boundaries.
If a Rust borrow conflicts with a Cog read or mutation, the operation
returns a catchable `InvalidArgument` Error instead of panicking the host process;
release host borrows before calling back into Cog when the operation must succeed.

Executable function references and closure captures carry a private program
identity. The VM binds bytecode function literals during execution, including
inside array templates; native-returned values retain their original identity.
Calling an unbound or foreign reference yields a catchable `InvalidArgument`
with `op: "foreign callable"`. Runtime function references are not portable
bytecode constants. Low-level `run_with_context` calls each have a fresh identity
because `CompiledProgram` can be edited between executions. The immutable SDK
`Program` retains a stable identity across its runs.

| Permission | Host operation |
| --- | --- |
| `allow_print` / `allow_read` | Stdout / stdin |
| `allow_fs_read` / `allow_fs_write` | `io::open`, `fs::metadata`/`read_dir`, and FS mutations; read-write open requires both |
| `allow_sleep` / `allow_time` | Sleeping / clocks |
| `allow_random` / `allow_exec` | Entropy / process execution |
| `allow_network` | Async TCP exchanges, listeners, and persistent TCP streams |

Permissions are checked for each effect. The CLI uses the default permissive host
policy. `cog run --restricted` allows stdin/stdout and denies other runtime host
effects; restricted embedding can disable all host effects. Imports do not grant authority.
This is a host boundary, not a completed untrusted-package delegation model.

`--restricted` (for both `cog run` and `cog eval`) also applies fixed default runtime
limits, with no flags or environment variables to change them:

| Limit | Value |
|-------|-------|
| `max_steps` | 200,000,000 |
| `max_heap_objects` | 1,000,000 |
| `max_call_frames` | 10,000 |
| `max_buffer_bytes` | 16 MiB |
| `max_resources` | 16 |
| `max_slots` / `max_operand_values` | 1,000,000 each |
| `max_numeric_digits` | 10,000 |
| standard output | 8 MiB |

The values live in `RuntimeLimits::restricted_defaults()`. Exceeding one ends the run
with a recoverable runtime resource-limit error (exit code 1); exceeding the output cap
fails the write with `restricted output limit of 8 MiB exceeded`. Active limits disable
the JIT, so restricted runs always use the VM. Unrestricted runs are unchanged and set
no limits. Embedders keep full control through `RunContext::with_limits`; `sdk::Limits`
still defaults to `None` for every field.

`run_with_context` calls `finish` on every VM exit, including terminal failure.
Finish closes resources, flushes eligible output, invalidates handles, and is
idempotent. A finish failure becomes primary after successful execution or secondary
when a primary failure is active. Drop releases resources without a hidden retry.
Embedding code making native calls directly must call finish itself.

## Assembly and diagnostics

`cog run file.asm --allow-asm` enables the diagnostic assembler. Its calls include
`call target argc`, `callv argc`, and symbolic `call_native io::write_all`.
Direct calls supply callable arities automatically. Indirect functions and closures
require `.function label argc captures` declarations before bytecode encoding.
`is_type error` consumes a value and produces bool; use `dup` to retain it.
`is_type enum Name`, `is_type variant Name::Item`, and `is_type struct Name` test
nominal identities. The
`record n` primitive consumes key/value pairs. Typed-array construction and runtime
type constraints have dedicated instructions. Printing, conversions, formatting,
time, entropy, and processes use native calls rather than dedicated opcodes.

Source diagnostics retain file/line/column origins through frontend linking and
recover independent errors. The current artifact format carries validated
serialized source maps and inline frame records; see
[diagnostics](DIAGNOSTICS.md). Public runtime APIs are listed in
[standard library](STANDARD_LIBRARY.md);
cleanup and error boundaries are defined in [errors](ERRORS.md).
