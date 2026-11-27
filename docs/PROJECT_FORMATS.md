# Projects and configuration

Cog uses KDL 2 for user configuration and a separate generated `cog.lock` format.
Dependencies can use local paths, local archives, or exact versions from the
user-local package store. A standalone program needs no
configuration; create configuration files only when their settings are needed.

## Start a project

```sh
cog init my-app
cog check my-app
cog run my-app
cog build my-app
cog init --lib integer-math
cog check integer-math
cog package build integer-math
cog install --path integer-math
```

Init requires an explicit directory. By default it creates `src/main.cog` plus
commented `package.kdl`, `dependencies.kdl`, and `build.kdl` templates. The
generated `app` target works immediately: `cog run my-app --target app`. Use
`cog init --lib <directory>` to create a library manifest, `src/lib.cog`, and a
small public `src/api.cog` module ready for `cog package build` or local install.
Dependency examples stay commented until you create their source packages.
Use `cog init .` to initialize the current directory. The package name is derived
from the selected directory. Once created, the manifest name
survives directory renames. Package names must be nonempty and cannot be `.`/`..`
or contain path separators/control characters.

| File | Purpose |
| --- | --- |
| `package.kdl` | Package name, version, kind and optional embedded native plugin declarations |
| `dependencies.kdl` | Dependency aliases with local paths, `.cogpkg` archives, or installed package identities |
| `build.kdl` | Optional named targets with nonstandard entry points |
| `cog.lock` | Generated dependency resolution; keep it with the project |
| `target/` | Disposable artifacts and build cache |

```kdl
// package.kdl
package "my-app" version="0.1.0"
```

`kind` defaults to `application`, preserving existing projects. A library
package sets `kind="library"`; `src/lib.cog` (or `lib.cog` when there is no
`src/` tree) is its check entry point and does not itself define an importable
module name. Public modules such as `src/integer_math.cog` are imported by
module name after the package is included. `cog check` validates a library
without requiring `main` or writing bytecode. `cog package build` emits a
library dependency archive; executable `build` or `run` support for library
roots is not implemented.

## KDL syntax and supported settings

A declaration has a node name, positional string arguments, and `key="value"`
properties. In the example above, `package` is the node, `"my-app"` is its name
argument, and `version` is a property of that same node. A newline ends a node.
`//` introduces a comment. These follow [KDL 2 syntax](https://kdl.dev/spec/).

Cog supports the following complete configuration schema:

| File | Declaration | Effect |
| --- | --- | --- |
| `package.kdl` | `package "name" version="value"`; optional kind is `application` or `library`; child `plugin` and `asset` declarations | Package identity, kind, native signatures, declared assets and default artifact name; version recorded in dependency resolution and lockfiles |
| `dependencies.kdl` | `alias path="directory"`, `alias archive="file.cogpkg"`, or `alias package="name" version="exact-version"` | Connects a local or user-installed source package for imports; paths are relative to this file |
| `build.kdl` | `target "name" entry="source.cog"` | Selects a named executable with `--target name` |

Manifest values are strings, except the plugin ABI version integer. A package
plugin block may contain shared function declarations and target library
declarations; other configuration declarations remain flat. Unknown child nodes, additional properties and duplicate
declarations are rejected with diagnostics. Version is metadata, without semver solving or version constraints. Compiler
optimization profiles, runtime permissions, registry dependencies and arbitrary
build scripts are not currently KDL settings. Configuration is read and checked
during source project preparation, including cache hits; saved bytecode runs
without it.

```kdl
// dependencies.kdl
geometry path="./packages/geometry"
regex archive="./vendor/cog-regex.cogpkg"
arithmetic package="integer-math" version="0.1.0"
```

The node name is the dependency alias in the consuming package. Exactly one
dependency source is required: `path`, `archive`, or the `package` and `version`
pair. A path is relative to the directory containing `dependencies.kdl`; a
directory dependency needs its own `package.kdl`, while a `.cogpkg` archive must
contain one at its root. Installed dependencies select an exact package name and
version from the user-local store. Aliases can differ from actual package names.
Transitive packages declare their own dependencies; consumers do not repeat them.
Package metadata, including native plugin signatures and libraries, travels with
the package archive, so a consuming project only adds its dependency declaration.
It does not need a separate `plugin.kdl` or a repeated function list.

Local and installed dependency archives are hashed with BLAKE3 and extracted into
the consuming project's `target/.cog-package-cache/<digest>` directory.
Dependencies accept only COGPKG 2 archives whose root manifest declares
`kind="library"`. The lockfile records the content-addressed package path, so
replacing archive bytes makes `--locked` resolution fail until the owner
deliberately updates the lock.
Application archives use COGPKG 3 and are extracted into
`target/.cog-application-cache/<digest>` when run; COGPKG 2 applications are not
runnable packages. Both versions contain a regular-file TAR stream inside a
Zstandard frame, with a root `COGPKG` marker and `package.kdl`. Unsupported
markers and versions are rejected. Cog long-name headers are supported with a
4096-byte path limit; PAX and other metadata extensions are rejected.
Extraction accepts only regular files and directories, rejects traversal,
duplicate and case-colliding path components, and caps compressed and expanded
archive bytes at 512 MiB, entries at 65,536, and aggregate path bytes at 64 MiB.
The archive is copied to a temporary snapshot while its digest is checked, then
that snapshot is extracted, avoiding a source-file change race. This digest
detects changed/corrupt local inputs; it does not authenticate the publisher or
establish that native code is safe. `cog check` can statically validate archive
dependencies without loading native plugins. `cog install --path <directory>`
builds a complete COGPKG archive for the current host and installs it locally.
Libraries are stored under `$COG_HOME/libraries/` and applications under
`$COG_HOME/programs/`; when
`COG_HOME` is unset, the store is `~/.cog`. Store directories are keyed by the
package name and exact version. Reinstalling identical contents is idempotent;
changing archive contents without changing the version is rejected. An installed
application can be run by passing its printed `.cogpkg` path to `cog run`.
Installed libraries can be shared by any project owned by the same user. There
is no registry, network resolver, version solver, or cache garbage collector.

`cog install --git owner/repository [--rev revision] [--locked]` clones a GitHub
repository over HTTPS, checks out the requested Git tag, commit, or explicit Git
revision (such as `origin/main`), and
installs its root Cog project in the same local store. The full HTTPS GitHub URL
is also accepted. Git must be installed and available on `PATH`. The command
prints the resolved commit and package identity. When `--rev` is omitted, it
uses the repository's default HEAD, which can change; specify a commit for a
reproducible source selection. The resulting `cog.lock` and installed package
identity do not record the Git commit. Existing package names and versions
remain immutable, so changed contents require a new package version.

The command installs one repository; it does not fetch transitive Git
dependencies. Dependencies must already resolve through local paths or
installed packages supported by `dependencies.kdl`. The checkout
is temporary and is removed after installation. Authentication uses the local
Git configuration and available credentials, with interactive terminal prompts
disabled. Installing packages can include native plugins; trust the source
before using them.

For GitHub installs, local path dependencies must resolve inside the checkout,
including transitive dependencies. Symbolic links anywhere in the checkout
and local `archive=` dependencies are rejected. Use an already installed
`package=` dependency when an archive is needed. These restrictions do not
change `cog install --path`.
Package archive creation and application extraction/validation are also available
from the Rust crate through `cog::package::build_archive`,
`cog::package::build_archive_for_target`, `cog::package::install_path`, and
`cog::package::open_application`. The crate
does not expose installed dependency resolution; the CLI handles dependency
resolution and application execution. Before a COGPKG 3 application runs, Cog checks that
each `package.kdl` identity and `dependencies.kdl` edge matches `cog.lock`; it
also requires the cached lockfile and vendored package directory components to
be free of symbolic links. The opener pins directory handles while validating
manifests, dependencies, the lockfile, bytecode, and declared plugin files, so
cache path replacement cannot redirect those reads outside the opened package
tree. The platform loader later receives a path for each native plugin; anyone
able to modify the local application cache must therefore be treated as trusted
to replace native code. A package-declared plugin runs with the host process'
authority, and the archive digest does not authenticate its publisher or safety.

## Package-declared native plugins

A package can declare C ABI v3 signatures and the corresponding library in the
same `package.kdl`. The function metadata is the static compiler contract and is
checked against the library's complete runtime descriptor before callbacks are
registered:

```kdl
package "cog-regex" version="0.1.0" kind="library" {
    plugin version=1 {
        library target="x86_64-unknown-linux-gnu" path="native/linux-x86_64/libcog_regex.so"
        library target="x86_64-pc-windows-msvc" path="native/windows-x86_64/cog_regex.dll"
        library target="aarch64-apple-darwin" path="native/macos-arm64/libcog_regex.dylib"
        function "regex::is_match" result="bool" {
            param "str"
            param "str"
            effect "fs_read"
        }
    }
}
```

The same package block can list distributable data or bytecode paths with
`asset "data/stopwords.txt"`. Asset paths use the same normalized relative-path
rules and are collected from transitive local packages. They are manifest
inputs to `cog package build`, which copies each declared regular file into the
package archive.

The library path must be a normalized relative path with forward slashes; it may
not be absolute or contain `..`, `.` components, backslashes or drive prefixes.
Control characters are also rejected.
This is lexical path validation, not a filesystem sandbox; local symlinks remain
trusted inputs during normal local builds. The package builder rejects symlink
inputs and opens package files beneath their owning package root. A plugin
block accepts one `function` node per native import;
Function children are the same `param`, `effect`, and `mutates` declarations as
the standalone plugin manifest. A package plugin shares this metadata across its
target libraries. It may declare any nonempty subset of the supported targets:
`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, and
`aarch64-apple-darwin`. A library package does not need binaries for platforms
its maintainer cannot build. `cog package build` packages the current host's
variant by default; `--target <triple>` selects another supported variant and
keeps only that library declaration and file in the archive. The same option
works for COGPKG 3 applications. Cog bytecode remains portable, but a packaged
native plugin is available only for the targets included in its archive. At run
time Cog chooses only the exact host target and reports an error if it is
missing. It never falls back to another platform's library. `cog check`,
`cog build`, and `cog package build` use signatures without loading native code.
`cog run <project>` and `cog run <application.cogpkg>` load the matching library
from its owning package and check its descriptor against the shared metadata.
Application bytecode is decoded and validated before native libraries are
loaded. This executes trusted native code with the Cog host process's
operating-system authority; the ABI descriptor check is compatibility
validation, not a security boundary.

## Building source package archives

`cog package build [project-directory] [--target <triple>] [-o output.cogpkg] [--locked]` validates
the source project and writes a deterministic TAR archive compressed with
Zstandard. The default output is `target/package-<name>.cogpkg`. Application
packages use COGPKG 3 and include validated executable bytecode at
`.cog/app.cogb`; the bytecode is compiled from the prepared Cog source snapshot
used during validation. Cog source files are preserved unchanged. If `--target`
is explicit, the default output name includes the target triple. Library
packages use COGPKG 2, contain no executable, and do not require `main`. `--locked`
requires an existing lockfile to match whenever the resolved dependency graph
requires one. Without it, the archive still receives a canonical lockfile but the
source project is not modified.

`cog install --path <project-directory> [--locked]` builds for the current host
and installs the package into the local user store. COGPKG 2 libraries go under
`libraries/`; COGPKG 3 applications go under `programs/`. The default store is
`~/.cog`, and an absolute `COG_HOME` path overrides it. The CLI prints the
installed archive path; `cog run <path>` runs an installed application.

The builder snapshots discovered Cog source bytes before compiler validation and
compiles from that snapshot. Total Cog source snapshots are limited to 64 MiB in
memory. Manifests are re-read and checked against the prepared package metadata;
build configuration, declared assets, and plugin libraries are captured into
archive entries before writing.
The builder includes each package's discovered Cog source files and
`package.kdl`, the root `build.kdl` when present, and explicitly declared assets
and plugin libraries. It generates `dependencies.kdl` and `cog.lock` for the
archive. Every
dependency package is placed under `.cog/packages/<id>`, so dependencies that
were originally outside the root directory work after extraction. The generated
`dependencies.kdl` points aliases at those archive-local paths; source directives
such as `@!include("game")` remain unchanged. A COGPKG 2 library archive can be
used by declaring it with `archive="relative/path.cogpkg"` in another project's
`dependencies.kdl`. A COGPKG 2 archive can also be installed with
`cog install --path <library-directory>` and selected by exact package identity.

Only those allowlisted inputs enter the archive. Unlisted files, `.git`, `target`,
tests, and examples are omitted. Asset and plugin inputs must be regular files;
symbolic links, unsafe archive paths, case-colliding paths, and output paths that
would replace an input are rejected. The TAR and compressed archive are each
limited to 512 MiB. Package creation does not sign, encrypt, authenticate, or
load native code. Local COGPKG 2 library dependencies are extracted automatically
during project preparation. A COGPKG 3 application is run with
`cog run app.cogpkg -- <program arguments>` and works after moving the archive
without its source project. A COGPKG 3 application is not accepted as a
dependency. Package registries, version solving, and cache garbage collection
are not implemented yet.

Bytecode execution does not inspect an adjacent package manifest, so any `.cogb`
that imports a plugin still needs explicit `--plugin` manifests at run time.
`--plugin` remains available for standalone projects and bytecode. An app
package embeds its executable `.cogb` and plugin target variants in the archive.

## Compile-time source packages

The package entry point can include a source dependency during compilation. Its
`.cog` files become modules in that package; executable module code still runs in
the resulting program. Directives in other modules are rejected. A directive
declares a package connection; its alias does not need a prior `use`. It does not
import module names into source scope: that is the role of `use`.

```cog
@!include("game")
use random from game as game_random;
```

The `game` alias must be declared in `dependencies.kdl`, for example:

```kdl
game path="vendor/game"
math package="integer-math" version="0.1.0"
```

The local dependency directory must have its own `package.kdl`. An installed
dependency selects an exact package identity from `~/.cog/libraries/`. Package
source files are read from `src/` when present, otherwise from the package root.

The `std` alias is built in. Include it before importing standard-library
modules; it participates in package-qualified imports and ambiguity checks:

```cog
@!include("std")
@!include("game")

use random from game as game_random;
use random from std as std_random;
```

Only the built-in `std` alias and aliases present in `dependencies.kdl` resolve
through `@!include`. Declare each dependency there; local paths are resolved
relative to that manifest. Resolved package paths and identities are recorded in
`cog.lock`.

`use module from package as alias;` selects one package explicitly. A plain
`use module;` works when the module has a single visible provider; if multiple
included packages provide that namespace, compilation reports an ambiguity and
requires `from`. Declare source dependencies in `dependencies.kdl` and include
them in the project entry point by alias:

```cog
@!include("game")
use random from game as rand;
```

The version is nonempty metadata, not a semver solver. Registry/git sources and
version constraints are not supported yet. KDL syntax parsing is separate from
strict schema validation. Unknown nodes/properties, wrong types, duplicate
singleton nodes/properties, and conflicting aliases produce located diagnostics;
independent schema errors accumulate. Cog rejects duplicate properties even though
KDL itself permits last-value semantics. There is no alternate-format fallback.

## Modules and imports

Sources are discovered under `src/` when it exists, otherwise under the package
root. Root `lib.cog` is the package API; `a/b.cog` defines `a::b`. Directories form
namespaces; nested `lib.cog` is an ordinary module named `lib`.

```text
package.kdl
src/
  main.cog
  greeting.cog
  utils/
    math.cog
```

A public function in `greeting.cog` can be imported with `use greeting;` and called
as `greeting::greet(...)`. See the runnable [modules project](../examples/modules/README.md).

Supported imports include `use a::b;`, `use a::b::item as alias;`,
`use a::b::{x, y as z};`, and `use a::b::*;`, globally or inside blocks.
`self`/`super` refer to the current/parent module. From main, use `super` or
`super::item` to reach the root `lib.cog` explicitly.

Explicit `self`/`super` and dependency aliases select a module or package directly.
Unqualified paths collect visible providers from the current package, its
dependencies, and the included standard library. Multiple providers are ambiguous,
including for nested paths. A dependency alias cannot
collide with a local/standard module or a reserved name. Declaring a dependency
does not replace `use`; imports are required for standard modules too.

Declarations are private unless marked `public`. Private helpers can call each
other directly or through their module name within that module, including
recursion. Defaults/globals resolve in the declaration environment. Wildcards
import only public declarations. Package/module identity distinguishes equal
names in different files/packages. Import cycles are allowed; package dependency
cycles and statically detected global initialization cycles are rejected.

Global initializers currently execute before main and can call functions/native
operations. They are ordered by references to globals, including references through
functions/defaults; independent initializations preserve declaration order. Cycle
analysis is conservative and may reject dependencies through an unexecuted branch.

Nested directories containing `package.kdl` are separate package boundaries and
need dependency aliases. Symlinks, hidden directories, `target/`, `tests/`, and
`examples/` are excluded from ordinary discovery. Module filenames/directories
must be Cog identifiers; `a.cog` together with `a/` is ambiguous. Package paths
must be UTF-8. Only entry/import-reached modules are parsed/checked/emitted; all
of their declarations are retained, without per-function stripping.

## Targets and artifacts

Default entry selection tries `src/main.cog`, then `main.cog`, then the single
configured target. Multiple targets without a conventional main require selection.

```kdl
// build.kdl
target "client" entry="src/client.cog"
target "server" entry="src/server.cog"
```

```sh
cog build . --target server
cog run . --target server
```

The artifact is `target/server.cogb` for an explicitly selected target, otherwise
`target/<package-name>.cogb` (directory name without a package manifest). A single
nonstandard target selected implicitly still uses the package artifact name.
Entry paths must be discovered sources inside the package source tree; absolute
paths and `..` are forbidden in `entry`. See [multiple targets](../examples/targets/README.md).

One artifact includes the entry and linked modules/dependencies; there are no
separate library `.cogb` artifacts. A source passed explicitly produces a `.cogb`
beside it. `cog build ... -o output` overrides the destination with any filename.
Source/project builds reject destinations that resolve to a discovered source,
configuration manifest or the root `cog.lock`. Keep outputs in a dedicated
directory such as `target/` or `bin/`.
Configuration filenames are reserved even when missing, including ASCII case
variants, to keep output paths safe across common target filesystems.

The CLI recognizes bytecode by its four-byte `COG\0` signature regardless of
name, suffix or directory; a `.cogb` suffix also selects bytecode decoding so
corrupt artifacts retain bytecode diagnostics. The bounded decoder validates
the complete file in either case. An explicit `cog run file` uses recognized
bytecode without source freshness checks, artifact rewriting or cache creation.

`cog <bytecode-file> <arguments...>` runs a saved artifact and forwards every
argument after its path verbatim, including an initial `--` and Cog option
names. This also supports the argument vector from Linux `binfmt_misc` with
ordinary interpreter flags. Use the explicit `cog run <file> [options] --
<arguments...>` form for Cog's own flags and separator behavior. Source and
project invocation retain their existing argument-separator rules. Native
detached bytecode still requires explicit `--plugin` options; recognizing a
header never locates package manifests or loads plugin code automatically.

## Dependency lockfile

Version 1 is a small UTF-8, line-oriented format:

```text
COGLOCK 1
root 0
pkg 0 "my-app" "0.1.0" path "."
pkg 1 "geometry" "0.2.0" path "packages/geometry"
dep 0 "geometry" 1
```

The header is exactly `COGLOCK 1`. A single root references an existing package.
A pkg record contains ID, quoted name, quoted version, `path`, and quoted path;
a dep record contains owner ID, quoted alias, and target ID. IDs are decimal u32
without leading zeros except `0`. Identity includes name, version, and source;
matching names from different sources are not merged.

Paths are normalized relative to the lockfile, using `/` and removing redundant
`./`. Relative `..` is allowed for explicitly declared external local dependencies;
absolute host paths are not written. Fields are space/tab separated; blank lines
are allowed, records cannot span lines, and strings are always double-quoted.
String escapes are `\"`, `\\`, `\n`, `\r`, `\t`; unknown escapes, unescaped control
characters, and unterminated strings are invalid. There are no comments or
executable expressions. The reader accepts LF/CRLF; the writer uses LF and a final newline.

Canonical writing assigns root ID 0, sorts remaining packages by name/version/source
kind/normalized source in UTF-8 order, and writes pkg records before dep records.
Dependencies sort by owner/alias, with one ASCII space between fields. Only reachable
packages are emitted; identical resolution gives identical bytes, without timestamps.
The reader validates versions, fields, duplicate IDs/identities/aliases, and references;
the resolver separately checks the package graph and agreement with manifests.

Build/run validate resolution even on a cache hit. Without `--locked`, a missing or
stale valid lock is prepared for update and written after artifact preparation,
before execution. Runtime failure does not undo it. `--locked` rejects missing/stale
locks. Corrupt locks are diagnosed and preserved regardless of that flag. Check
writes no lock. No dependency graph requires no lock, but an existing lock is still
validated when dependencies are removed.

Mutable path sources remain live directories, not frozen snapshots. Source changes
invalidate builds even with `--locked`. Reproducibility requires the same available
source contents. Immutable sources and revision/content hashes need future work.

## Build cache and CLI output

Build/run share a whole-program cache. BLAKE3 fingerprints cover compiler binary,
configuration/resolution, entry and package/module identities, discovered catalog
and source contents (including unused modules and embedded core), and artifact bytes.
The source snapshot passed to compilation matches the hashed bytes; mtime/size alone
are insufficient. Preparation parses the entry point to resolve package inclusion
before checking the cache. Cache hits skip further module parsing, semantic checking,
and emission, while still validating configuration, resolution, artifact hash, and
bytecode.

Cache records live in `<package-root>/target/.cog-cache/`, keyed by output path.
The current internal 74-byte record contains `COGCACHE`, little-endian u16 version 2,
32-byte input hash, and 32-byte artifact hash. A companion `.deps` KDL record stores
build-input bases, normalized paths, BLAKE3 content hashes or absence states. Its
contents participate in the input hash; current dependencies are reread before
reuse. This includes creation/deletion of optional inputs. Both files are disposable and not a substitute
for `cog.lock`. Missing/corrupt cache or artifacts cause rebuilds. Compile errors stop
execution instead of running a stale artifact. Artifact replacement uses a temporary
file and rename. Deleting target does not lose dependency resolution.

Check always analyzes source and writes no artifact/lock/cache. Explicit bytecode
execution needs no source/configuration/cache. Builds report `built` or `finished`;
check reports `checked`; a cached run is silent. Preparation timing excludes VM
execution. Status/diagnostics go to stderr, program output and requested `--debug`
disassembly to stdout. `--color auto|always|never` controls color, and `NO_COLOR`
disables automatic coloring. Failures distinguish preparation from runtime errors.

Caching is conservative and still reads/discovers inputs. This is not file-level
incremental compilation. Compiler-aware build inputs and their materialization
contracts are in [semantic contracts](SEMANTIC_CONTRACTS.md). Project mode locates
the nearest enclosing package.kdl when invoked from a subdirectory. Runtime CWD
is not changed.
