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

Supported values include `int`, `uint`, `bool`, `char`, `str`, and `nil`.
Integer overflow and division by zero produce Error values. Branches, loops,
recursion, direct calls, scalar assignments, Error tests, and `!!` use the
existing verified control flow. `main(args: str[])` and `process::args()` can
read host arguments using length and index operations. The guest supports the
subset of `io` and `bytes` operations used by `io::print` and `io::println`.
The compiler limits direct recursion to 64 nested calls and rejects excessive
instruction, generated source, and frame sizes at build time.

Support is based on the instructions reachable from the entry point.
Unsupported instructions and native imports cause build errors. Current limits
include floating point, BigInt, general collection construction and mutation,
closures, async, crash, filesystem access, user native imports, and other host
operations. This guest reports a source origin with the final Error. It does
not carry the VM's full diagnostics and its own compact values do not require
Cog's tracing GC. The WASI command target is separate from running the Cog
compiler and VM in browser WebAssembly.

Run the end-to-end suite with `cargo test --locked --test wasm -- --include-ignored`.
The suite requires the WASI Rust target and Node. It executes the produced
artifacts and compares output against the VM.
