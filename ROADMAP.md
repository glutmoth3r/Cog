# Cog roadmap

Cog is in early Beta. This roadmap summarizes implemented capabilities, the
remaining release acceptance gates, and the next language goals. See the [language reference](docs/LANGUAGE.md),
[standard library reference](docs/STANDARD_LIBRARY.md), and
[semantic contracts](docs/SEMANTIC_CONTRACTS.md) for current behavior.

## Implemented

- A recovering lexer/parser, separate semantic analysis, located diagnostics,
  module resolution, and direct stack-bytecode generation.
- A stack VM with checked frames, control flow, locals, callable boundaries,
  source maps, a generational graph heap, iterative cycle collection, and
  optional execution limits. The optional object-count limit is not a byte or
  whole-process memory limit.
- Portable `.cogb` v43 artifacts (v38 to v42 files are still accepted) with bounded decoding and structural
  verification. The VM checks declared callable argument/result types at call
  boundaries; static value-flow verification is still in progress.
- Packages, KDL configuration, path/archive/user-store dependencies, `cog.lock`,
  explicit imports, build targets, cache invalidation, and contained
  compile-time text/binary inputs. `cog package build` and local
  `cog install --path` write deterministic TAR+Zstandard archives; COGPKG 2
  carries library packages and COGPKG 3 carries executable application bytecode.
  Libraries and applications install into separate directories under `~/.cog`
  (or `COG_HOME`), and `cog run app.cogpkg -- args` runs a relocated application.
- Package plugins share C ABI v3 signatures/effects and declare exact target
  libraries for Linux x86_64, Windows x86_64, and macOS arm64. Packages may
  carry a selected target subset; `cog package build --target <triple>` selects
  one native variant, while runtime loading has no fallback.
- Checked `<<`, `>>`, `<<=`, and `>>=` bit shifts for `int`, `uint`, and
  `bigint` in the VM, compile-time evaluation, the JIT (`int`/`uint` lowered,
  other cases resumed in the VM), and, for `int`/`uint`, the WASI backend.
  Arrays grow with `.append` and `.prepend`.
- Structs, enums, typed arrays/tuples, generic functions and structs, trait
  bounds, generic receiver methods, nullable/Error flow analysis, and compile-
  time functions with error handling.
- Synchronous source closures with explicit value and cell captures, including
  reference-valued and nested captures, first-class function values, and positional
  indirect calls. Captured closure bodies can enter JIT OSR. Eligible synchronous
  callback dispatch uses the VM call bridge and can return to machine code.
- Statically dispatched user-defined `for-in` iteration through concrete
  `iter::IntoIterator` and `iter::Iterator` implementations. Generic target
  implementations and iteration through a generic trait bound are not supported.
- Arrays with generic higher-order `map`, `filter`, and `fold`, Unicode-scalar
  string traversal, insertion-ordered hashed `Map`,
  bounded JSON, process arguments/environment, path and filesystem
  APIs, byte streams, and Cog implementations of common search, sort, string,
  and encoding utilities.
- A tutorial and CI/fuzz infrastructure. Short fuzz smoke runs and local Linux
  validation have been recorded; sustained fuzz campaigns remain outstanding.
  The CI workflow is configured for Linux x86_64, Windows x86_64, and macOS
  arm64 on Rust 1.95 and stable; all six platform/toolchain jobs and the
  tutorial passed for the early Beta candidate. Sustained campaigns remain.
- A Rust SDK for embedding and native registration, plus CLI plugins across a
  versioned C ABI v3 with call-scoped handles, async task callbacks and retained
  library lifetimes.
- Compact compiler syntax: `@!include("std")` and aliases declared in
  `dependencies.kdl`, plus composable `@comptime`, `@public`, `@async`, and
  `@inline` attributes. `@inline` is a best-effort hint and
  `@inline("never")` disables inlining for a function. `@comptime const` works
  at module and function scope. Unknown attributes fail during parsing;
  `@repr` and `@derive` have no semantics yet. Bare legacy declaration
  spellings are rejected.
- Inline-frame error provenance (bytecode v40, `ErrorTraceFrame.inlined`), with
  each propagation step recorded once.
- Default resource limits and an 8 MiB output cap for `--restricted` runs, without
  extra flags.
- Local aggregate fact tracking in the bytecode verifier (BETA-08 partly done:
  increments 2 and 3 of the differential-test plan), checked by a verifier-versus-VM
  differential test.
- Mutability analysis cost: only functions whose read summaries changed are
  re-run, and scalar-typed constructor fields no longer carry references. `cog check`
  of `packages/cogweb` dropped from 14 s to under 2 s, and of `web/tutorial` from
  over 10 minutes to about 5 s.
- `cogweb` 0.5.0 in `packages/cogweb` with `static_files::serve_file` and
  `http::binary_response`. The tutorial server is a Cog program (`web/tutorial`)
  on it, launched by `web/server.mts`, with 22 tests in `web/server.test.mts`.

## Beta release acceptance

- Close known gaps in already implemented language features, or make their Beta
  exclusions explicit:
  - Error leaves are retained by aggregate constructors and writes to array/tuple
    positions, record fields, and collections. Reads preserve the leaf until a
    consuming operation observes it. `json::stringify` propagates an encountered
    Error unchanged and returns no partial JSON string. Direct coverage for array,
    tuple, record, and Map writes plus nested JSON serialization lives in
    `tests/semantic_contracts.rs`; run it as part of Beta validation.
  - Beta excludes struct patterns with field bindings. Struct values carry
    their nominal identity at run time, and `when value is Struct` tests it for
    non-generic structs.
  - Beta's comptime contract is the implemented subset in [compile-time
    evaluation](docs/LANGUAGE.md#compile-time-evaluation) and [its semantic
    contracts](docs/SEMANTIC_CONTRACTS.md#implemented-comptime-evaluation-and-functions):
    scalar and struct evaluation, comptime functions, limited type-valued bindings,
    and documented phase/Error behavior. `@comptime var`, reassignment, loops,
    array/tuple/enum values, field inspection, and native calls remain outside it.
  - Beta excludes composing user-defined `Clone` implementations through arrays
    and tuples. Structural cloning remains available for supported primitive leaves
    and nested arrays/tuples; custom implementations remain usable in generic calls.
- Evolve scalar types in stages. `char` is a Unicode scalar with single-quoted
  literals, explicit string conversion, scalar-order comparisons, string
  iteration, and bytecode/SDK representations. `uint` is a checked unsigned
  64-bit type with explicit conversions, exact mixed comparisons, JSON, range,
  trait, bytecode, and SDK support. `bigint` now provides exact signed
  arbitrary-precision integers, contextual literals, checked conversions,
  arithmetic/comparisons, JSON, generic traits, `Map` keys, and compile-time
  scalar evaluation; the C ABI v3 still has no BigInt payload. The implemented
  numeric behavior is recorded in [numeric type design](docs/NUMERIC_TYPES.md).
  Arbitrary-precision `decimal` remains proposed. Defer `i8`–`i64` and
  `u8`–`u64` until fixed-width types show how they relate to the base names and
  whether they add useful distinctions.
- Verify bounded bytecode value-flow summaries against the VM for declared
  aggregate/call/native contracts, global writes, unknown effects, and
  unreachable instruction structure. Preserve dynamic type failures as
  recoverable `Error` values; Beta does not claim a complete static type proof.
- Finish the contract review of the Rust native SDK and CLI plugin interface,
  including ownership, error paths, panic containment, and library lifetimes.
- Complete the broader compiler/runtime resource audit. Keep ordinary host
  authority and runtime limits optional; do not claim whole-process memory
  isolation.
- Run the four 24 CPU-hour fuzz campaigns after explicit owner authorization
  and resource allocation. Validate the final revision on Linux x86_64,
  Windows x86_64, and macOS arm64 with Rust 1.95 and stable, and verify clean
  packaging and installation.
- Close release-blocking defects and document remaining limitations before
  preparing 0.1.0 release artifacts.

## Async foundation (started early)

Async work has started ahead of the remaining Beta acceptance gates. The current
foundation supports `@async fn`, `Task[T]`, `await`, `spawn`, `cancel`, `yield_now`,
async `main`, bytecode verification, FIFO cooperative scheduling, memoized outcomes,
structured joining, timers and timeouts, asynchronous filesystem reads, writes,
append and line operations, asynchronous managed-file open, read, exact/read-to-EOF,
partial/full write, flush, and seek, async stdin/stdout handles sharing the sync
stream buffers, process capture with async child stdin and Unix
process-group cancellation, bounded TCP request/response and persistent streams with
exact/read-to-EOF and partial/full writes, listeners, and async inherent, associated,
and statically dispatched trait methods. Async process cancellation and
timeouts terminate the Unix process group and reap the direct child; other platforms
currently stop only the direct child. Fatal async scope errors cancel spawned children
before joining their cleanup. Unix TCP connect attempts poll cancellation while the
connection is pending; synchronous DNS resolution remains noninterruptible.
Timer, filesystem, and TCP operations are grouped under `async::`; `process::run`
returns a task by default, accepts optional stdin bytes and/or a process deadline,
and `process::run_sync` names the blocking capture path.
It also reclaims unreachable cold-task cycles, resumes saved VM continuations through
FIFO waiter queues, detects wait cycles, performs cooperative task cancellation, and
warns about discarded tasks. `task::select` and `task::select2` select among spawned
sibling tasks while preserving structured ownership and leaving result observation
explicit. The implementation, limits, and unsupported cases are recorded in
[async design](docs/ASYNC_DESIGN.md).
Native plugin async operations use C ABI v3 start/poll/cancel/destroy callbacks and
thread-safe scheduler wakeups. Detach and async closures remain future work.

## After Beta: language completeness

1. Extend compile-time programming with value parameters, canonical
   specialization, and materialization of arrays, tuples, and enum values.
2. Resolve trait implementation coherence/visibility rules and decide whether
   generic target implementations or iteration through generic trait bounds are
   needed before expanding generic abstractions further.
3. Continue measured compiler/runtime optimization. The opt-in Cranelift JIT
   and APE bootstrap/adaptive selector remain experimental. The JIT validates
   native exit states, charges oversized compilations as refillable budget debt,
   caches locals in Cranelift SSA across OSR blocks and writes them back at
   native exits, and includes matched VM/JIT benchmarks for supported scalar
   loops and compact BigInt arithmetic with overflow fallback. The V2 bridge runs
   synchronous Cog and native calls and selected aggregate operations from
   compiled code. Broaden lowering
   only with VM-equivalence checks and retain
   interpretation for unsupported operations and metered executions. See
   [JIT design](docs/JIT_DESIGN.md). Compiler-generated bytecode also receives
   bounded CFG-based scalar propagation, branch pruning, unreachable-block
   removal, and redundant local-store elimination before final verification.
   Runtime lowering folds bounded BigInt constants and eligible constant calls;
   small statically typed helpers with pure expressions and local `const`
   bindings can be inlined to expose more work to constant propagation and JIT
   lowering.
4. Implement Windows process-tree termination to match Unix process-group
   cancellation. Buffered TCP framing helpers and detached tasks need separate memory,
   ownership, cancellation, and shutdown contracts.
5. Struct type tests (`when value is Struct`) can guard an optional struct
   field, so a user-written `Clone` can copy recursive fields. The checker still
   treats `Node?` like `Node` and joins `nil` with a struct to `dynamic`, so
   `if field == nil { nil } else { field.clone() }` is rejected; decide whether
   to track struct nullability. For the same reason a `match` with only a struct
   arm is exhaustive just for the few proven non-nil scrutinees listed in
   [LANGUAGE.md](docs/LANGUAGE.md#patterns-when-and-match). Struct `==` and `!=`
   call an explicit `eq::Eq[Self]` implementation, and `cog check` rejects structs
   without one. Compile-time evaluation still compares structs without that
   implementation (equal records compare as `false`). Decide whether a built-in
   struct `.clone()` should exist.
6. Extend the WASI backend only by owner decision. `str::to_int`, `to_uint`,
   `to_bool`, `to_char`, `str::builder`, `build`, `fmt::write`, `replace`, arrays
   and structs have VM parity (tests/wasm.rs); the guest has no cycle collector.
   `Map`, enums, tuples, floats, closures and async remain unsupported; extending
   them changes the scope documented in [WebAssembly](docs/WEBASSEMBLY.md). A
   browser-hosted compiler and VM are not implemented.

## Readiness

Version `0.1.0-beta.1` marks **early Beta**. The local audit verified the
implemented Beta contracts and Linux x86_64 packaging and installation on
Rust 1.95 and stable. CI run
[45030017021](https://github.com/glutmoth3r/cog-lang/actions/runs/45030017021)
then passed Linux x86_64, Windows x86_64 and macOS arm64 on both Rust 1.95 and
stable, along with the tutorial. Full release acceptance still requires the
four sustained fuzz campaigns. Detailed audit evidence lives in the
repository's development records.
