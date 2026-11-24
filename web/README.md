# Interactive Cog tutorial

Requires Rust 1.95 or newer, Cargo, and Node.js 24.12 or newer. From the repository root:

```sh
node web/server.mts
```

Open **http://127.0.0.1:4173/web/get_started.html**. Edit an example, optionally add
stdin input, and choose Run. Output and compiler/runtime diagnostics appear below
the editor. Reset restores the original example; Ctrl+Enter or Command+Enter runs it.

The tutorial server is a Cog program (`web/tutorial`) built on the `cogweb` library
(`packages/cogweb`). `node web/server.mts` is only a launcher: it builds the Cog
compiler with locked, offline Cargo, packs `packages/cogweb` into a `.cogpkg` and
installs it in `$COG_HOME/libraries` (default `~/.cog/libraries`), then runs
`cog run web/tutorial`. The install is skipped when a stamp under `target/tutorial/`
matches the digest of the library sources, the compiler path and the store. After
editing `packages/cogweb`, bump its `version` in `package.kdl` and the dependency in
`web/tutorial/dependencies.kdl`: an installed release is immutable and the installer
refuses different contents under the same version.
That server invokes the real compiler and runs each example's `.cogb` artifact.
The first start compiles the server and can take minutes. If dependencies are not
cached, first run `cargo build --locked --bin cog` with network access. There are no
npm dependencies or JavaScript implementations of Cog semantics.

To run the server without the launcher, install the library and start it from the
repository root: `cog install --path packages/cogweb`, then
`COG_BIN=$(which cog) PORT=4173 cog run web/tutorial`.

`PORT` changes the local port; `COG_BIN` selects an already-built Cog executable
that supports `--restricted`; `COG_HOME` selects the library store. The default binds only to loopback. Runs allow stdin
and stdout, deny other runtime host effects, and have time/output limits. This is
a local learning tool, not a service for hosting untrusted code. CLI runtime limits
do not impose a total VM memory quota.

## WASM output checker

After a successful run, each lesson offers "Check output with the WASM checker".
The example still runs through the VM path above. The page then passes the expected
and actual output to `web/tutorial_logic.cog`, a Cog program compiled to a
`wasm32-wasip1` command. The comparison (exact, including Unicode, whitespace, and
the final newline) executes in WebAssembly inside a module Worker; JavaScript only
handles the DOM, the Worker, and moving strings.

`GET /api/tutorial-checker.wasm` serves that one artifact (`application/wasm`) with
`static_files::serve_file`. The server builds it at startup with
`cog build --target wasm32-wasip1`, which needs `rustup target add wasm32-wasip1`;
without it the endpoint returns 503 until the server is restarted, and the page
shows an error. Cross-site fetches are refused. The server also serves `web/*.mjs`
as JavaScript. It exposes no other build output. Limits: 96 KiB per text, no NUL characters, 5 s per fetch and per
Worker run; violations produce an error message instead of a verdict.

This does **not** compile arbitrary Cog source in the browser. The browser runs a
precompiled checker; compiling and running lesson code still needs the local server.

Optional real-browser check: `COG_CHROMIUM=/usr/bin/chromium node --test web/server.test.mts`
drives Chromium over the DevTools protocol and runs the checker in a Worker. It is skipped
by default and not required in CI.

The HTML remains readable when opened directly or without JavaScript; execution
requires the local server. Language documentation is in [docs/](../docs/README.md).

```sh
node --test web/server.test.mts
cargo test --locked --offline --test documentation --test projects
```
