# Native compilation

Cog can compile executables and shared libraries on Linux x86_64 GNU. Native
builds need a C compiler driver and a Cog runtime archive built from the same
source revision. The CLI locates the archive beside itself or in the current
project's Cargo target directory. Build both with the same Cargo toolchain and
target:

```sh
cargo build --locked --release --target x86_64-unknown-linux-gnu --bin cog
cargo rustc --locked --release --target x86_64-unknown-linux-gnu --lib --crate-type staticlib
native_profile=target/x86_64-unknown-linux-gnu/release
"$native_profile/cog" build --native examples/native/pipeline.cog -o target/pipeline
target/pipeline Ω
```

The executable links Cog runtime code statically. It needs the target's
system libraries at launch. The archive and compiler are build inputs. Set
`CC` to a compatible driver executable or leave it unset to use `cc`.
Set `COG_NATIVE_RUNTIME` when the archive lives elsewhere. An explicit value
must point to an existing archive.

## Compilation and execution

The result of a synchronous or asynchronous `main` declared `-> int` becomes the
process exit status after cleanup, using its low eight bits. Every other `main`
uses status 0 after a successful run, and unhandled runtime failures use status
1.

Native builds use standard project discovery, lock resolution, build inputs,
the frontend, optimizer, and bytecode verifier. For a project with multiple
applications, `--target` selects the application from `build.kdl`. Native
builds emit an object file and link it in a temporary directory adjacent to
the destination. A successful link replaces the executable in one rename.
Builds currently relink every time. The CLI does not read or write an APE
profile when producing or running a native artifact.

The JIT and AOT backends use the same Cranelift lowering and helper
signatures. JIT defines the functions in an executable memory module.
AOT defines the functions and verified bytecode in an ELF object for the
target baseline ISA. The linker resolves helper relocations and a table of
native entries. At runtime that table belongs to the artifact's execution
plan. The VM enters compiled loops at the first eligible backedge and can
call eligible scalar functions from their entry. Ineligible instructions
and failed guards resume at the bytecode position defined by the common
native exit protocol. Runtime helpers use the common tagged frame and
GC roots, so array aliases and closures remain ordinary Cog values.

Set `COG_AOT_TRACE=1` when running a linked executable or C client to see
native entry outcomes and completed backedges. The build prints machine
code bytes and code generation time. Use the scripts in
[`examples/native`](../examples/native) to reproduce execution and timings.

The current subset uses existing JIT lowering and its function size
bounds. Async scheduling and operations outside the lowering run through
the VM. Cog rejects a native build with zero eligible machine functions.
Windows and macOS artifact production is pending. The private machine
frame and table ABI can change between Cog builds.

### Optional runtime JIT for AOT fallbacks

By default, an AOT executable uses its embedded machine code and resumes in
the embedded VM when it reaches bytecode without an AOT entry. To let the
runtime JIT compile hot eligible functions that were not embedded, set
`COG_AOT_JIT=1` before starting the executable:

```sh
COG_AOT_JIT=1 ./target/my-app arg1 arg2
```

The adaptive compiler is initialized at startup. Existing AOT entries remain
in use; the JIT profiles bridged direct Cog calls and compiles eligible missing
callees for later native slices. AOT loops yield to the VM periodically while
this option is enabled so the runtime can collect those call profiles. The
JIT uses the existing code size and function budgets. Its profiles are local
to that process and are not persisted.

This does not make bytecode outside the shared JIT/AOT lowering eligible for
native compilation. Async functions, oversized functions and unsupported
operations still use the VM. A command-line `--jit` after the executable name
is a Cog program argument, not a runtime option. For a shared library, set
`COG_AOT_JIT=1` before calling `cog_native_open()`; the setting is captured
when that runtime context is created. `COG_AOT_TRACE=1` shows JIT compilation
and bridge decisions.

An executable with no eligible native functions still runs its bytecode in the
VM. Shared libraries require their exported functions to be eligible.

See [C interoperation](FFI.md) for shared libraries and C imports.
