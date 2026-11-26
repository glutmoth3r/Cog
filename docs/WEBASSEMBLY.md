# WASI command backend

Cog can build stand-alone `wasm32-wasip1` commands for a subset of the language.
Install the Rust target on the machine that builds the artifact:

```sh
rustup target add wasm32-wasip1
cog build --target wasm32-wasip1 examples/wasm/scalar.cog -o target/scalar.wasm
node --disable-warning=ExperimentalWarning examples/wasm/run.mjs target/scalar.wasm "Hello, WASI"
```

The host needs a WASI Preview 1 runtime. The example runner uses Node 24 or
later. The guest receives arguments after its executable name, including Unicode
text. It exits with status 1 when an Error reaches the entry point, and reports
its kind and bytecode origin on stderr. A source coordinate appears when the
bytecode contains one. Optimizer inlining can remove logical frames. WASI reports
the source and function information available from the compiled artifact.

The compiler validates Cog bytecode, finds reachable functions and basic
blocks, and maps each operand stack position to a named local in generated
Rust. Blocks contain compiled operations and call other compiled functions
directly. The installed Rust compiler and its WASI standard library produce the
command with `-O`. The artifact contains Wasm code for the Cog program and a
small guest runtime for the supported values and operations. Set `RUSTC` to
select a different Rust compiler executable when building.

Supported values include `int`, `uint`, `bool`, `char`, `str`, and `nil`,
arrays and structs of supported values, and shared mutable byte buffers and
string builders. Integer overflow, division by zero, and invalid shift amounts
produce Error values.
Branches, loops, recursion, direct calls, assignments, Error tests, and `!!` use
the existing verified control flow. `main(args: str[])` and `process::args()`
provide the host arguments as an ordinary `str[]`.

## Arrays and structs

Arrays and structs follow the VM's semantics, including Error kinds and payload
fields:

- Assignment, calls, fields, and elements share one mutable object.
- Array literals, empty and annotated arrays, `.append`, `.prepend`, indexing,
  index assignment, `len`, `+`, and `.empty_like()` are supported.
- Arrays keep their element type at run time. Writes, insertions,
  concatenation, typed assignment, parameters, results, and struct fields check
  it and report `TypeMismatch` without changing the array. Array types deeper
  than the VM allows report `ResourceLimit`.
- `for` iterates arrays, strings, ranges, parallel clauses, and user types that
  implement `iter::IntoIterator`. Changing the length of an array during its
  iteration reports `InvalidArgument`.
- Named, empty, tuple-like, and generic structs support construction, field
  defaults, field reads and writes, methods, `when value is Struct`, and `==`
  through an `eq::Eq` implementation.
- Errors stored in elements and fields stay values at that position.
- Text conversion matches the VM, including `ResourceLimit` (`render depth`) for
  cyclic or very deeply nested values.

The guest uses reference counting and has no cycle collector. A cyclic structure
(for example, a struct field that refers back to the struct) behaves like it does
on the VM, but its memory is never reclaimed before the command exits. A
long-running loop that keeps creating cycles can therefore exhaust Wasm memory.
Acyclic structures are released when their last reference goes away, including
long linked lists. Running out of memory while adding to an array or rendering
text reports `ResourceLimit`; other allocation failures stop the command with a
Wasm trap. The guest does not apply the VM's optional heap and buffer limits.

## Host operations

The guest supports these host operations, which follow
the VM's semantics, including Error kinds and payload fields:

- `io::stdout`, `io::write_all`, and `io::flush`, which `io::print` and
  `io::println` use.
- `bytes::from_str`, `bytes::render`, `bytes::len`, `bytes::new`, `bytes::get`,
  `bytes::set`, and `bytes::to_str`. Ranges are checked like the VM
  (`InvalidArgument` for `bytes range`), and `bytes::to_str` reports a
  `ParseFailure` for invalid UTF-8.
- `str::to_int`, `str::to_uint`, `str::to_bool`, and `str::to_char`, so
  `text as int` works. Parsing uses Rust's rules: an optional `+` (or `-` for
  `int`), ASCII digits only, no surrounding whitespace, and a `ParseFailure`
  from `str` to the target type on empty text, overflow, or any other
  character. Converting between `int` and `uint` reports `Overflow` when the
  value does not fit.
- `str::builder`, `str::build`, and `fmt::write`. Only the first `{}` in a
  template is replaced. `str::build` returns a snapshot.

Standard-library functions written in Cog compile when they use only these
operations. This includes `str::replace`, whose empty pattern returns
`Error::InvalidArgument("str::replace")`, and functions such as
`http::html_escape`. The guest builds `Error::Variant(...)` values from the core
`Error` enum. User enums and anonymous records are rejected.

## Limits

A call made at call depth 64 raises a runtime `CallDepthLimit` Error. This
applies to every nested call, not only direct recursion. The compiler rejects
more than 20,000 reachable instructions, more than 256 slots in a function frame
(which also bounds the number of elements in one array literal), and generated
source above 16 MiB at build time.

Support is based on the instructions reachable from the entry point.
Unsupported instructions and native imports cause build errors. Current limits
include floating point (so `str::to_float`, `str::to_bigint`, and array or field
types that mention `float` or `bigint`), BigInt, tuples (including tuple patterns),
array templates from the assembler, `collections::Map` and `Set` operations (they
need native hashing), `.clone()` on arrays, function values and closures, async,
crash, filesystem access, user native imports, and other host operations such as
`str::to_str`, `bytes::copy`, `bytes::slice`, and `bytes::concat`; any other
reachable host operation fails the build. The guest has no VM resource limits:
allocation failure for `bytes::new` reports `ResourceLimit`, but string and
builder sizes are bounded only by Wasm memory. This guest reports a source origin
with the final Error. It does not carry the VM's full diagnostics, and its
compact values do not use Cog's tracing GC. The WASI command target is
separate from running the Cog compiler and VM in browser WebAssembly.

## Tutorial checker in the browser

`web/tutorial_logic.cog` is built as a WASI command and served by the local tutorial
server as `/api/tutorial-checker.wasm`. A small Preview 1 host in `web/wasi.mjs`
runs it in a browser Worker and compares two output strings. This runs a
precompiled program; the browser does not compile Cog source, and the lessons
themselves still run on the VM through the local server. See `web/README.md`.
Verify it with `cargo test --locked --test tutorial_logic -- --include-ignored`
and `node --test web/server.test.mts`.

Run the end-to-end suite with `cargo test --locked --test wasm -- --include-ignored`.
The suite requires the WASI Rust target and Node. It executes the produced
artifacts and compares output against the VM.
