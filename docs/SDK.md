# Rust embedding and native plugins

Cog 0.1.0 is experimental. `cog::sdk` is the owned embedding API; other compiler
and VM modules remain implementation interfaces. The dynamic plugin boundary is
**C ABI v3**, defined by [cog_plugin.h](../include/cog_plugin.h) and
`cog::sdk::abi`. No Rust ABI, allocator, `String`, `Value`, trait object or panic
crosses that boundary. Plugins must match the host OS, architecture and C calling
convention. Loading a library executes trusted code with process authority.

## Embedding and registration

```rust
use cog::sdk::{Engine, Signature, Type, Value};

let mut engine = Engine::new();
engine.register("host::double", Signature::new(vec![Type::Int], Type::Int),
    |_, arguments| {
        arguments[0].as_int()?.checked_mul(2).map(Value::int)
            .ok_or_else(|| cog::sdk::Error::new("integer overflow"))
    })?;
let program = engine.compile("embedded.cog",
    "@!include(\"std\") use host; use io; fn main() { io::println(host::double(21)); }")?;
let program = engine.decode(&program.encode()?)?;
drop(engine);
program.run(&mut std::io::stdout())?;
# Ok::<(), cog::sdk::Error>(())
```

Run the complete example with `cargo run --locked --offline --example embedding`.
`compile_project` resolves filesystem projects; `build_project` also writes the
artifact and resolution lock and uses the normal build cache. `read` bounds and
validates a bytecode reader. `Program::write` writes an encoded artifact atomically.
These methods produce executable programs. `check_library_project` and
`check_library_prepared` validate a `kind="library"` project and return its
warnings without requiring `main` or writing bytecode.
The Rust crate also exports `cog::package::build_archive` for library and
application archives, `cog::package::install_path` for building and installing a
local project into the user store, and `cog::package::open_application` for
extracting and validating application archives. Installed dependency resolution
is handled by the CLI.
Source check/build supports metadata-only registrations and never needs a loaded
library. Registrations are engine scoped; compiled/decoded programs own their
resolved imports and may outlive their engine. Cache fingerprints include native
metadata. A decoded extension import must match its exact canonical metadata
fingerprint (name, ABI version, parameter/result types, effects and mutation
indices). This identifies a contract; it provides no publisher authentication.
Locally built unsigned artifacts remain supported.

Running a program never exits the embedding process. After a successful run,
`RunContext::exit_code()` reports the low eight bits of an integer `main` result
(or 0 for a non-integer result). A failed run returns its usual SDK error and
resets this metadata to 0. The CLI and executable launchers use the status only
after cleanup succeeds.

An executable may declare `fn main(args: str[])`. The CLI passes arguments after
`--`; an embedder can set the same list on `RunContext` before execution. Supply
only program arguments, without the executable name:

```rust
use cog::sdk::RunContext;
use std::ffi::OsString;

let mut output = Vec::new();
let mut context = RunContext::new(&mut output);
context.set_process_arguments(vec![OsString::from("input.txt")]);
program.run_in(&mut context)?;
```

Cog strings require UTF-8. A non-UTF-8 OS argument is returned through the normal
recoverable Error path when `process::args()` is evaluated.

Register names as `module::function` with ASCII identifiers. Standard-library
modules are reserved. Each function accepts at most 64 positional parameters.
`Signature::with_mutating_parameters` takes strictly increasing argument indices;
the compiler rejects calls that expose const aliases to these mutations. Trusted
callbacks must honor the declared mutations and effects, including changes through
captured aliases. Parameter tags are the coarse native types in the C header,
not nominal Cog struct identities or the complete source annotation grammar.
Reference-like results from registered native functions are conservatively
read-only because a callback may return an alias of an existing value. Use
`.clone()` when an independent writable copy is needed and supported.
All typed boundaries retain runtime checks. A typed Error argument bypasses the
callback and propagates; `Any` can inspect/return it. A wrong successful return
type, callback error or caught panic becomes the catchable `err::Error::NativeFailure`.

`Type::Char` is tag 13, `Type::UInt` is tag 14, and tag 12 remains the Error
sentinel. In C `CogValueView`, `integer` is a `uint64_t`: it carries unsigned
values directly, while signed `int` values use the same 64-bit two's-complement
bits and should be cast to `int64_t` by C plugins. Rust embedders can use
`Value::uint` and `Value::as_uint`.

## Ownership, contexts and limits

Engine, Program and Value are thread confined (`!Send`/`!Sync`). Callbacks are
synchronous and owned (`'static` captures, including `Rc` shared state). Cloning
a Value preserves aliases and roots its graph. The SDK provides primitive,
character, unsigned integer and string constructors and checked array
creation/access/mutation. Other VM categories can be retained and passed back
unchanged; the SDK does not expose raw resource pointers, arbitrary record
construction or custom host-pointer values. Release retained graphs when no
longer needed. Unreachable Cog graph cycles are traced by the VM.

Task handles belong to the individual run that created them. Returning a retained
task to a later run does not give access to that run's tasks; attempting to use
the foreign handle produces a catchable invalid-argument Error.

Callable values belong to the exact `Program` instance that produced them.
They can be retained and returned on later runs of that instance, including
inside shared arrays or other value graphs. Invoking them in another `Program`
returns a catchable `Error::InvalidArgument` with `op: "foreign callable"`
before entering its code. Decoding the same bytecode again creates a different
instance. Retaining a callable does not provide a way to execute code after its
owning `Program` has been dropped, and runtime callable references cannot be
serialized as bytecode constants. These rules also apply to captured closures.

`RunContext` borrows its output writer, owns its input/resources, and is intended
for one run. The writer must implement `Write + Send`; a custom input reader passed
to `RunContext::with_input` must implement `Read + Send`, so scoped async standard-
stream workers can safely borrow the host streams during execution. The VM finishes
resources on all exits; resource values retained by
callbacks cannot revive a closed slot or transfer it into another context.
Dropping callback captures releases ordinary Rust resources. `NativeContext`
borrows one call's I/O, with checked output/read helpers; it cannot escape a safe
Rust callback. SDK v1 rejects compiler/VM reentry from native callbacks.
`Engine::register` accepts synchronous callbacks only; async functions require
the C ABI callbacks described below. Cross-thread calls into a borrowed context
are unsupported.
This reentry guard covers SDK v1 entry points; direct calls to the public
low-level `cog::interpreter` API are outside that guard.

Default permissions provide normal process authority. Optional `Permissions` and
`Limits` configure host policy and VM counters. All `Limits` fields default to
`None`, including `max_buffer_bytes` and `max_resources`; set `Some(n)` to opt into
those VM quotas. The resource count includes standard stream slots. Declared
plugin effects are checked before invocation; checked host I/O also checks its
permissions. Native code can
make OS calls independently: these flags are not an OS sandbox. Limits do not
bound all process memory, plugin allocations or blocking native/system calls.
Rust `Rc`/string payload allocation can abort on system OOM. Catching unwind does
not catch `panic=abort`, signals, foreign exceptions or native memory corruption.
Rust's panic hook may print even when a panic is caught. A panic payload whose
destructor panics is also contained; the second payload is leaked. User capture
destructors run under normal Rust Drop rules, outside the callback panic guard,
when the last `Engine` or `Program` holding the callback is released.

Apply explicit limits to one run with `RunContext::with_limits`:

```rust
use cog::sdk::{Limits, RunContext};

let limits = Limits {
    max_buffer_bytes: Some(8 * 1024 * 1024),
    ..Limits::default()
};
let mut output = Vec::new();
let mut context = RunContext::new(&mut output).with_limits(limits);
program.run_in(&mut context)?;
```

The experimental Cranelift JIT is opt-in with `RunContext::with_jit(true)`.
`with_jit_profile_path` selects its advisory APE profile, and
`with_jit_debug(true)` prints selection and fallback diagnostics. Configured
runtime limits keep execution in the interpreter. A synchronous import called
from compiled code uses the usual linked dispatch, including permission
checks, native Errors, and callback-scoped handles. Programs retain their
linked native library for the lifetime of the import.

`run_in` executes, and JIT-compiles, on the calling thread. Give a thread that
enables the JIT at least 1 MiB of stack; the 2 MiB default of Rust spawned
threads satisfies this. Stack exhaustion terminates the process. See
[thread stack requirements](JIT_DESIGN.md#thread-stack-requirements) for the
measurements and their limits.

## Independently built CLI plugins

For source projects, package-declared plugins are discovered from the root and
transitive dependency `package.kdl` files. `check` and `build` register only the
declared signatures; `run` loads the matching C ABI v3 libraries relative to
their owning package and checks each loaded descriptor against its metadata.
This removes the need to repeat `--plugin` for source projects. `cog package
build` writes deterministic TAR+Zstandard archives. Applications use COGPKG 3
and include executable bytecode compiled from the archived source snapshot;
libraries use COGPKG 2 and need no `main`. `cog run app.cogpkg -- args` runs a
relocated application package. The CLI selects an exact target-specific plugin
library from the package metadata, with no fallback. A package may carry any
nonempty subset of Linux x86_64, Windows x86_64, and macOS arm64 variants;
`cog package build` selects the host by default, while `--target <triple>` builds
an archive for one other supported target. The Rust API exposes archive build,
targeted archive build, local installation, and application-open/validation
helpers through `cog::package`. Opening a package validates plugin
metadata without loading native code. A standalone `.cogb` still does not embed
native libraries or inspect an adjacent package manifest. A `.cogb` that imports
a plugin still needs an explicit `--plugin` argument.

Standalone projects and detached bytecode can still use repeatable
`--plugin path/to/plugin.kdl` arguments:

```kdl
plugin version=1 library="./libnative_plugin.so"
function "host::double" result="int" { param "int"; }
function "host::fail" result="nil"
function "host::delayed" result="int" { param "int"; effect "output"; async; }
```

Library paths are relative to the manifest file; parent components such as `../`
are allowed. The manifest is not a filesystem sandbox. Use `.dylib` on macOS and
`.dll` on Windows. Function children accept `param "type"`, `effect "name"`, and
`mutates 0` nodes. Effects are `output`, `input`, `fs_read`, `fs_write`, `process`,
`process_info`, `clock`, `sleep`, `entropy`, and `network`. Add an `async` child
to declare a function returning `Task[result]`; its ABI descriptor must provide
async start, poll, cancel and destroy callbacks. Unknown/duplicate metadata, unsupported
versions, excessive arity, oversized/deep manifests and descriptor mismatches are
rejected. A library's descriptor must match the manifest's entire function set. The OS loads
the library and runs its initializers before this descriptor can be inspected;
only load native code you trust.
Only `run` loads the library; `check` and `build` use its metadata without loading
it, including when the library is missing. Bytecode does not bundle native code;
deployment must provide matching manifests/libraries explicitly when running a
`.cogb` that imports native functions.
`--restricted` is optional host policy and cannot contain a trusted plugin: the
CLI rejects it together with explicit or package plugins, and applies it to
application archives without plugins.
`cog build` and `cog package build` refuse an output path that names a plugin
library declared by any package, for any target. `cog build` also protects each
`--plugin` manifest and its library.

```sh
cog check main.cog --plugin plugin.kdl
cog build main.cog --plugin plugin.kdl -o program.cogb
cog run program.cogb --plugin plugin.kdl
```

The Rust [cdylib example](../examples/native_plugin.rs) uses C descriptors and
`abi::dispatch`, which catches unwinding callback panics before returning through C.
Build it against a separately compiled Cog library (Linux example):

```sh
cargo build --locked --offline --lib
rustc --edition=2024 --crate-type=cdylib --crate-name=native_plugin \
  examples/native_plugin.rs --extern cog=target/debug/libcog.rlib \
  -L dependency=target/debug/deps -o target/libnative_plugin.so
```

If Cargo uses an explicit `--target`, take the library and its dependencies from
`target/<triple>/debug`, pass the same `--target` to `rustc`, and keep
`-L dependency=target/debug/deps` for host proc macros. Rust dependencies must
use the same toolchain; this is a build-time requirement for the wrapper, not
the dynamic ABI. C/C++ plugins need only the
header and their own compiler. The test fixture in
[tests/fixtures/native_plugin.c](../tests/fixtures/native_plugin.c) builds and loads
independently on Unix.

The [async C plugin example](../examples/native_async_plugin.c) runs two worker
threads. Its output shows both `start` lines before either `finish` line, and the
companion program cancels a long operation and joins its worker:

```sh
cc -std=c11 -O2 -fPIC -shared -pthread -Iinclude \
  examples/native_async_plugin.c -o examples/libnative_async_plugin.so
cog run examples/async_plugin.cog --plugin examples/async_plugin.kdl
cog run examples/async_plugin_cancel.cog --plugin examples/async_plugin.kdl
```

## C ABI lifetime and failure contract

- Export `cog_plugin_v3` with the C calling convention. Its immutable descriptor
  and function metadata remain valid until unload. `size` must equal `sizeof`
  the v3 structure and `version` must equal 3. Reserved numeric tags are rejected;
  incompatible layout changes require another ABI version.
- Views are length-delimited, without a NUL requirement. Empty views may be null;
  nonempty views and typed arrays must be valid/aligned for their complete length.
  Names and strings are UTF-8. The host copies descriptor metadata before use.
  Checks detect compatibility mistakes; they cannot make malicious pointers safe.
- `create`/`destroy` are either both absent or both present. `create` writes a
  plugin-owned instance pointer (initially null); the host never dereferences or
  frees it. On any subsequent registration failure or final release, `destroy`
  runs once, including for null/partially initialized state after create failure.
  Initialize that state so destruction is safe. No call overlaps destruction.
  A rejected descriptor runs neither callback. Rust authors may use
  `abi::create_state`/`destroy_state` in C wrappers to contain unwinding
  initialization/destruction panics. Never exchange Rust allocations.
- Every registered function owns a shared reference to the instance and library.
  Engines and compiled/decoded programs retain callbacks; the last release calls
  `destroy` before unloading.
- A synchronous function has `asynchronous=0` and `invoke` set. The host table,
  call context, argument handles and borrowed string views are valid only until
  `invoke` returns, on the calling thread. Retain copies in plugin-owned memory
  if needed.
- An async function has `asynchronous=1`, `invoke=NULL`, and all four operation
  callbacks set. A Cog call creates a cold `Task[result]`; a task that is never
  started calls none of them. On the VM thread, `start` receives the call-scoped
  argument handles and creates an opaque operation state. `poll` runs on the VM
  thread with a fresh call-scoped frame; return `COG_PENDING` while incomplete,
  or `COG_OK` with a result handle when complete. Do not retain handles, views or
  frame contexts from `start` or `poll`.
- Returning `COG_OK` from `start` transfers the operation to the host, which
  releases it exactly once. Any other status leaves nothing for the host to
  release: `start` must clean up its partial operation, including workers, and
  the host calls neither `cancel` nor `destroy_operation`.
- The host polls once after a successful `start`, and again only after a wake.
  Any `poll` status other than `COG_PENDING` completes the operation, whether it
  succeeds or fails. A completed operation receives `destroy_operation` without
  `cancel`.
- An incomplete operation receives `cancel` and then `destroy_operation`. This
  covers task cancellation, a run that ends or fails while the operation is
  pending, `set_error` during a successful `start` or a pending `poll`, and host
  failures that abandon the operation. Both run on the VM thread, receive no host
  table or frame, and cannot report an error.
- `start` receives an opaque `wake_context`. It and the host table passed to
  `start` stay valid until that operation's `cancel` or `destroy_operation`
  returns. A worker thread may call only `host->wake(wake_context)`; it must not
  call `inspect`, `make`, `write_output`, `set_error`, or any VM/SDK entry point.
  Both callbacks must stop and join every worker before returning; there must be
  no wake call after either one returns. `destroy_operation` must do this even
  after completion. These rules keep the wake context and loaded library alive
  until worker shutdown.
- Wake notifications are coalesced per operation, and the poll that follows sees
  memory published before a successful `wake`. The scheduler queue holds 4096
  notifications. If `wake` returns `COG_LIMIT`, retry it later from the worker,
  and stop retrying on cancellation. Never wait for room inside `start` or
  `poll`: only the VM thread empties the queue. While `cancel` or
  `destroy_operation` runs, `wake` returns `COG_INVALID`.
- Async callbacks never run on host worker threads. The plugin owns its worker
  threads or event source; the host owns only scheduler wakeup and task lifetime.
  Opaque handles have per-call generations and are rejected in other calls;
  they are not addresses. In the current host's `uint64_t` representation, bits
  63..12 hold a 52-bit generation and bits 11..0 hold a zero-based slot index.
  This layout is opaque: plugins must not decode, construct, persist, or reuse
  tokens. At most 4096 values are rooted per call; new string/array data consumes
  the configured buffer budget. Generations use 52 bits per thread; exhaustion
  after `2^52 - 1` calls fails without wrapping so expired handles cannot revive.
  Arbitrary nested/opaque Cog values can be forwarded unchanged.
- `inspect` supports primitives, characters, strings and array length; other values
  are opaque (Errors have a separate kind). `make` supports nil/int/uint/float/bool,
  char, string and array; array access/mutation is checked. No VM internals or
  Rust layouts are exposed.
  Safe Rust callbacks using `abi::PluginCall::inspect` receive an
  `InspectedValue`; its raw pointers are private, and returned byte/string slices
  are tied to the view's borrow. The raw C `CogValueView` pointers and handles
  remain callback-scoped, so copy data that must survive the callback.
  Inspected string storage stays rooted throughout the callback.
- A successful `invoke` or ready `poll` writes a valid result handle whose value
  matches the declared result type. A failure status, `set_error`, an invalid
  handle or a mismatched result becomes NativeFailure.
- `set_error` copies at most 64 KiB of UTF-8. It rejects longer text with
  `COG_LIMIT` and invalid UTF-8 with `COG_INVALID`, recording nothing for that
  call; `abi::dispatch` truncates longer Rust error text at a character boundary.
  `write_output` returns `COG_LIMIT` for a view longer than the configured buffer
  limit, or longer than any possible allocation when no limit is set. A failed
  host operation returns a status and must be handled by the plugin.
- Never unwind through C or throw C++ exceptions into the host. Host functions
  contain their own panics. `abi::dispatch` covers synchronous Rust callbacks
  only: Rust async callbacks, and Rust `create`/`destroy` functions that bypass
  the state helpers, must catch panics themselves. A panic that reaches an
  `extern "C"` boundary aborts the process.
- The host passes mutable arrays by reference and does not enforce a plugin's
  declared mutations at run time. A plugin must call `array_set` only on arrays
  reached through parameters declared with `mutates`.

Cross-platform support and release readiness require actual target CI results;
successful local Linux tests do not establish Windows/macOS/MSRV support.
