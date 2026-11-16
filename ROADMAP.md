# Cog roadmap

Cog is preparing its first Beta release. This roadmap summarizes implemented
capabilities, release work still in progress, and the next language goals. It is
not a release schedule. See the [language reference](docs/LANGUAGE.md),
[standard library reference](docs/STANDARD_LIBRARY.md), and
[semantic contracts](docs/SEMANTIC_CONTRACTS.md) for current behavior.

## Implemented

- A recovering lexer/parser, separate semantic analysis, located diagnostics,
  module resolution, and direct stack-bytecode generation.
- A stack VM with checked frames, control flow, locals, callable boundaries,
  source maps, a generational graph heap, iterative cycle collection, and
  optional execution limits. The optional object-count limit is not a byte or
  whole-process memory limit.
- Portable `.cogb` v25 artifacts with bounded decoding and structural
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
- Structs, enums, typed arrays/tuples, generic functions and structs, trait
  bounds, generic receiver methods, nullable/Error flow analysis, and compile-
  time functions with error handling.
- Arrays, Unicode-scalar string traversal, insertion-ordered hashed `Map`,
  bounded JSON, process arguments/environment, path and filesystem
  APIs, byte streams, and Cog implementations of common search, sort, string,
  and encoding utilities.
- A tutorial and CI/fuzz infrastructure. Short fuzz smoke runs and local Linux
  validation have been recorded; sustained fuzz campaigns remain outstanding.
  The CI workflow is configured for Linux x86_64, Windows x86_64, and macOS
  arm64 on Rust 1.95 and stable; current-revision results are still required.
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

## Before the first Beta release

- Close known gaps in already implemented language features, or make their Beta
  exclusions explicit:
  - Error leaves are retained by aggregate constructors and writes to array/tuple
    positions, record fields, and collections. Reads preserve the leaf until a
    consuming operation observes it. `json::stringify` propagates an encountered
    Error unchanged and returns no partial JSON string. Direct coverage for array,
    tuple, record, and Map writes plus nested JSON serialization lives in
    `tests/semantic_contracts.rs`; run it as part of Beta validation.
  - Beta excludes struct patterns and runtime nominal struct-type tests. Structs
    keep static type identities but use runtime record storage; Beta does not add
    runtime nominal struct identity.
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

1. Add first-class function values and source-level closures, including capture,
   aliasing, and error semantics. The runtime has closure-related support, but
   the source grammar does not yet expose closures or indirect calls.
2. Extend compile-time programming with value parameters, canonical
   specialization, and materialization of arrays, tuples, and enum values.
3. Resolve trait implementation coherence/visibility rules and user-defined
   iteration before expanding generic abstractions further.
4. Continue measured compiler/runtime optimization. The opt-in Cranelift JIT
   and APE bootstrap/adaptive selector remain experimental. JIT V1 now validates
   native exit states, charges oversized compilations as refillable budget debt,
   caches locals in Cranelift SSA across OSR blocks and writes them back at
   native exits, and includes matched VM/JIT benchmarks for supported scalar
   loops and compact BigInt arithmetic with overflow fallback. Broaden lowering
   only with VM-equivalence checks and retain
   interpretation for unsupported operations and metered executions. See
   [JIT design](docs/JIT_DESIGN.md). Compiler-generated bytecode also receives
   bounded CFG-based scalar propagation, branch pruning, unreachable-block
   removal, and redundant local-store elimination before final verification.
   Runtime lowering folds bounded BigInt constants and eligible constant calls;
   simple statically typed BigInt helpers can be inlined to expose their loops
   to existing JIT arithmetic lowering.
5. Implement Windows process-tree termination to match Unix process-group
   cancellation. Buffered TCP framing helpers and detached tasks need separate memory,
   ownership, cancellation, and shutdown contracts.

## Readiness

The current assessment is **Alpha**. Beta requires the release work above and
evidence for each acceptance gate. A passing local test suite does not establish
cross-platform readiness, complete bytecode type safety, or production
suitability. Detailed status and historical audit evidence are kept in ignored
`dev_docs/` files and are not included in Cargo packages.
