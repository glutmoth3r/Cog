# C interoperation

The native build interface supports scalar C calls in both directions on
Linux x86_64 GNU. See [native compilation](NATIVE_COMPILATION.md) for the
toolchain and runtime archive setup.

## Cog calls C

Declare an import at build time with `--c-import module::name=symbol:i64` or
`--c-import module::name=symbol:f64`. An `i64` declaration maps Cog `int`
to and from C `int64_t`. An `f64` declaration maps Cog `float` to and from
C `double`. Each supported signature takes one argument and returns the
same scalar type. Match the exact C prototype when compiling the object.

```sh
cc -O2 -c examples/native/foreign.c -o target/foreign.o
cog build --native examples/native/imports.cog \
    --c-import foreign_math::process=process_value:i64 \
    --c-import foreign_math::scale=scale_value:f64 \
    --c-link target/foreign.o -o target/imports
target/imports
```

Inside Cog, the declarations are ordinary named native imports. The
compiler uses the SDK registration metadata. At runtime the executable
installs the linked C adapters and resolves the verified bytecode imports
against that registry. Calls from machine code use the same runtime bridge
as VM and JIT native calls. Adapters declare all host effects. C code
runs with process authority and must honor the declared prototype. The
linker catches unresolved symbols. A symbol name alone carries no
prototype information.

## C calls Cog

```sh
cog build --lib examples/native/library.cog --export add --export divide \
    -o target/libarithmetic.so
cc -pthread -I target examples/native/client.c -L target \
    -Wl,-rpath,"$PWD/target" -larithmetic -o target/client
target/client
```

Each `--export` selects a callable by its qualified Cog name or an
unambiguous short name. The header defines the exact C names and
prototypes. The library currently permits synchronous programs without
globals, exports up to 16 typed `int` arguments per function, and requires
an `int` result. The C argument and result storage use `int64_t`.
`cog_native_open()` returns an opaque runtime context or null on failure.
The context retains the verified program, callable identity and native
execution plan across invocations. Each invocation creates its own VM
frames and finishes its runtime resources before returning.

| Status | Interpretation |
| --- | --- |
| `COG_NATIVE_OK` | The function completed and wrote the result. |
| `COG_NATIVE_ERROR` | A Cog Error reached the export boundary. The caller's result slot stays intact. |
| `COG_NATIVE_FAILURE` | The invocation failed or the context or result pointer was rejected. The caller's result slot stays intact. |

Call `cog_native_close(context)` on the thread that opened it. A wrong
thread, mismatched library instance, or reentrant call is rejected.
Thread identity checks assume the passed context is live throughout the
operation. See [foreign memory](FOREIGN_MEMORY.md) for pointer contracts.

This interface is opt-in. The existing dynamic C plugin ABI v3 remains
available through the SDK and interpreter. Source annotations for C
imports, C exports, and C struct layout remain future work.
