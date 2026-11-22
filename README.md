# Cog

Cog is an experimental programming language implemented in Rust, with a compiler,
portable bytecode, and a stack-based virtual machine. It aims to combine a small,
expression-oriented language with explicit mutation and practical error handling.

The compiler and interpreter work today. Cog is being prepared for its first
public release; the language, standard library, and bytecode format may still
change. Compile-time bindings and functions evaluate scalar expressions and
construct structs; type-valued bindings, source-package inclusion, and basic
statically dispatched extensions are also available. Runtime generic functions
take inferred `[T: type]` parameters that are erased before runtime; trait
bounds and parameterized trait contracts are checked during specialization. Generic
structs and receiver methods are supported, as are synchronous closures with
explicit scalar captures and statically dispatched user-defined iteration over
concrete types. The optional Cranelift JIT remains experimental and falls back to
the VM for unsupported operations.

```cog
@!include("std")
use io;

fn greet(name: str, punctuation: str = "!") -> str {
    $"Hello, {name}{punctuation}"
}

fn main() -> nil {
    io::println(greet("Cog", punctuation: "!"))!!;
}
```


Save this as `hello.cog`, then run `cog run hello.cog`.

Use the [documentation index](docs/README.md) to find the [language reference](docs/LANGUAGE.md),
[runtime and bytecode guide](docs/RUNTIME.md), [project formats](docs/PROJECT_FORMATS.md),
[Rust SDK](docs/SDK.md), and [contributor guide](docs/CONTRIBUTING.md).

## Build and install

You need Rust 1.95 or newer and Cargo. From this directory:

```sh
cargo build --locked
cargo install --path . --locked --bin cog
cog run examples/hello.cog
```

Ensure Cargo's bin directory is on your `PATH`. With dependencies already cached,
add `--offline` to Cargo commands. Installation is optional: use
`cargo run --locked --bin cog -- run examples/hello.cog` directly from the checkout.

## Check, build, and run

```sh
cog check examples/hello.cog
cog build examples/hello.cog -o target/hello.cogb
cog run target/hello.cogb
cog eval --debug '2 + 3 * 4'
cog eval --use str 'str::trim("  Cog  ")'
cog --version

cog init my-app
cog run my-app
cog package build my-app -o my-app.cogpkg
cog run my-app.cogpkg -- "argument value"
cog install --path examples/library_package
```

Create a library project directly with `cog init --lib integer-math`. This
generates `kind="library"`, a `src/lib.cog` check entry point, and an example
public API module in `src/api.cog`.

`check` analyzes source without writing artifacts. `run` builds or reuses validated
bytecode and executes it. A saved `.cogb` runs without source files or recompilation.
Project builds default to `target/<package-name>.cogb`; a standalone source defaults
to a `.cogb` beside it. Build status and diagnostics go to stderr; program output
goes to stdout.
`package build` validates a project and writes a deterministic TAR+Zstandard
`.cogpkg`. Application archives use COGPKG 3 and contain executable bytecode;
they run after relocation with `cog run app.cogpkg -- <program arguments>`.
Library archives use COGPKG 2 and can be used as local dependencies by declaring
`alias archive="relative/path.cogpkg"` in `dependencies.kdl`.
Package builds select the current host's native plugins by default; pass
`--target x86_64-unknown-linux-gnu`, `--target x86_64-pc-windows-msvc`, or
`--target aarch64-apple-darwin` to build a package for one specific target.
`cog install --path <project-directory>` builds for the current host and stores
the package under `~/.cog/libraries/` or `~/.cog/programs/`; set an absolute
`COG_HOME` to choose
another store location. A consumer selects an installed library by exact identity
in one dependency line, for example
`arithmetic package="integer-math" version="0.1.0"`. Package plugin metadata travels in the archive, so consumers
do not create a separate plugin manifest. Plugin bundles select an exact Linux,
Windows, or macOS target without fallback. Archive digests detect changed bytes
but do not authenticate publishers or make native code safe.
See [project formats](docs/PROJECT_FORMATS.md) for archive and trust limits.

`run` passes arguments after `--` to an entry point declared as `fn main(args: str[])`.
The executable name is excluded. `fn main()` remains valid, and other entry-point
parameter signatures are rejected. `--restricted` disables `process::args()`, so
access to this argument list is denied there as well.

Bytecode is recognized by its `COG\0` header regardless of its filename or
directory. `.cogb` remains the default extension, but an explicit build output
can have any name:

```sh
cog build examples/command_line_args.cog -o target/args-tool
cog target/args-tool --help "two words"
```

With the shorthand `cog <bytecode-file> ...`, every argument after the file
belongs to the Cog program, including `--` and flags such as `--help` or `-V`.
Use `cog run <bytecode-file> [Cog options] -- <program arguments>` when passing
options to Cog itself. This shorthand also accepts the argument vector supplied
by a Linux `binfmt_misc` interpreter registration. Direct OS execution requires
the corresponding OS setup and executable file permissions.

`eval` compiles and runs one Cog expression without creating a source or bytecode
file. Repeat `--use <module>` to import standard-library modules. `--debug` prints
the generated bytecode before the result, which is useful when exploring how Cog
lowers operators.

`cog check` also reports non-fatal warnings for unread local bindings, statements
after unconditional exits, constant `if` conditions, and boolean conditionals that
return their condition unchanged. Warnings do not prevent compilation. Prefix an
intentionally unused local name with `_` to silence its warning; global bindings
are not included in the unused-local check.

## Language highlights

- `const` prevents reassignment and mutation through that access path. `var`
  requires reachable mutation; it does not imply dynamic typing. Reference-like
  assignment shares an object without an implicit move.
- A block's last expression without `;` is its result; no tail means `nil`.
  Functions may declare a normal result type with `-> T`.
- Errors are separate from normal types and from `nil`. Lexical `try/catch`
  handles failures; `!!` propagates an Error from the current function through
  cleanup. Lazy `??` selects a fallback for nil, and `?!` for Error.
- Plain strings preserve braces. `$"...{expression}..."` explicitly interpolates
  expressions. `when` and `match` inspect types and enum payloads.
- Calls support defaults and positional arguments before named arguments.
  Constructors use the same call syntax. Modules use explicit imports and
  `public` visibility; local dependencies use KDL manifests and `cog.lock`, with
  compile-time package inclusion and package-qualified imports.

Arrays are homogeneous and carry a recursive element-type invariant such as
`int[]` or `str[][]`. Known mismatches are rejected during compilation; mutations
through dynamic code are checked at runtime. Fixed-shape tuples and user-declared
extension methods are also supported. Trait implementations and bounded generic
functions use static dispatch. Arrays and tuples support structural `.clone()` for
supported primitive and nested container types. Generic structs support inferred
compile-time type parameters and applied type annotations. Nullable and Error flow
facts refine supported branches and are conservatively invalidated by writes and
calls.

## License

This project is dual-licensed under either:

- Mozilla Public License 2.0 (MPL-2.0)
- Apache License, Version 2.0 (Apache-2.0)

at your option.

You may choose the license that best suits your needs.

Contributions are accepted under both licenses.

Full texts: [MPL-2.0](LICENSE-MPL-2.0) and [Apache-2.0](LICENSE-APACHE-2.0).
