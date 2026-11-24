# Cog server that builds Cog to WebAssembly

A teaching server written in Cog with `cogweb`. It serves a page and compiles
`programs/NAME.cog` to a `wasm32-wasip1` module on request; the page then runs
the module in the browser using a small WASI host (`web/wasi.mjs`).

Requirements: a Cog compiler with the WASI backend and the Rust target
(`rustup target add wasm32-wasip1`). From this directory:

```sh
COG_BIN=/path/to/cog cog run .
```

Open `http://127.0.0.1:4177/`, choose a module (`pyramid` or `greet`), and Run.

Routes: `/` (page), `/web/wasi.mjs`, and `GET /wasm/NAME.wasm`, which builds
`programs/NAME.cog` and serves the result as `application/wasm` via
`static_files::serve_file`. Add a module by dropping a `.cog` file into
`programs/`. A failed build returns 422 with the compiler's diagnostics.

Limits: run it without `--restricted` because it starts the compiler. It
handles one connection at a time and rebuilds on every request. Names are
restricted to `a-z`, `0-9`, and `_` inside `programs/`. This is a local demo, not
a hosting service. Modules are limited by the WASI backend (no async, floats,
collections, `str::builder`, or `str::to_int`).
