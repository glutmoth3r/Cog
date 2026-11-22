# Native compilation

Cog can compile executables and shared libraries on Linux x86_64 GNU. Native
builds need a C compiler driver and a Cog runtime archive built from the same
source revision. Build both with the same Cargo toolchain and target:

```sh
cargo build --locked --release --target x86_64-unknown-linux-gnu --bin cog
cargo rustc --locked --release --target x86_64-unknown-linux-gnu --lib --crate-type staticlib
native_profile=target/x86_64-unknown-linux-gnu/release
export COG_NATIVE_RUNTIME="$PWD/$native_profile/libcog.a"
"$native_profile/cog" build --native examples/native/pipeline.cog -o target/pipeline
target/pipeline Ω
```

The executable links Cog runtime code statically. It needs the target's
system libraries at launch. The archive and compiler are build inputs. Set
`CC` to a compatible driver executable or leave it unset to use `cc`.
When `COG_NATIVE_RUNTIME` is unset, Cog checks for `libcog_native.a` next to
the CLI.

## Compilation and execution

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

See [C interoperation](FFI.md) for shared libraries and C imports.
