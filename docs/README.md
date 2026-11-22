# Cog documentation

Cog is experimental. These pages describe the current compiler and VM; status
labels distinguish implemented behavior from limitations, planned work, and open
questions. Standalone fenced `cog` examples in the README and documentation are
compiled and round-tripped by `tests/documentation.rs`. Fragments that need an external
package are syntax-checked there and exercised by project tests. Proposed syntax
in design contracts is shown as text.

Start with the [repository README](../README.md), the
[getting-started page](../web/get_started.html), or the [runnable examples](../examples/README.md).

| Reference | Contents |
| --- | --- |
| [Language](LANGUAGE.md) | Bindings, types, functions, control flow, patterns, strings, and collections |
| [Errors and cleanup](ERRORS.md) | Error values, lexical handlers, propagation, fallbacks, and defer |
| [Standard library](STANDARD_LIBRARY.md) | Generic array search and sorting, cloning, text, ASCII, hex, Base64, bytes, streams, time, and processes |
| [Projects and configuration](PROJECT_FORMATS.md) | Modules, KDL manifests, dependencies, lockfiles, targets, and caching |
| [Runtime and bytecode](RUNTIME.md) | Compilation pipeline, artifacts, native ABI, and embedding boundaries |
| [Native compilation](NATIVE_COMPILATION.md) | Linux AOT artifacts and shared execution with the JIT |
| [C interoperation](FFI.md) | Scalar imports, generated headers, and shared library calls |
| [Foreign memory](FOREIGN_MEMORY.md) | Context lifetime and managed value boundaries |
| [Numeric types](NUMERIC_TYPES.md) | Implemented `int`, `uint`, and `bigint` behavior; proposed decimal contract |
| [Rust SDK and plugins](SDK.md) | Owned embedding API, native registration, C ABI v3 and plugin lifecycle |
| [Semantic contracts](SEMANTIC_CONTRACTS.md) | Implemented invariants, current limitations, planned work, and open decisions |
| [Async design](ASYNC_DESIGN.md) | Task scheduling, async host/plugin APIs, limits, cancellation, and open ownership contracts |
| [Contributing](CONTRIBUTING.md) | Source layout, validation, diagnostics, and performance tools |

For implementation work, start with [contributing](CONTRIBUTING.md), then use
[runtime and bytecode](RUNTIME.md) for artifact and execution boundaries. The
[JIT design](JIT_DESIGN.md) and [async design](ASYNC_DESIGN.md) document their
separate implementation contracts.

The [roadmap](../ROADMAP.md) separates completed work, near-term priorities, and
longer-term goals. Design sections identify which parts are implemented and which
remain proposed.
