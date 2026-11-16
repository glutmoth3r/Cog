# Cog language design DNA

- Keep source syntax compact. Put declaration attributes before the declaration and allow several attributes to compose.
- Reserve `@name(...)` for compiler-checked declaration properties. Unknown, duplicate, malformed, or inapplicable attributes are errors; attributes never silently do nothing.
- Reserve `@!name(...)` for deterministic compiler and project directives that run before module resolution. They are not runtime expressions or user-defined macros.
- `@!include("alias")` names a package alias. `std` is built in; other aliases
  and their paths come from `dependencies.kdl`.
- Use `@comptime const` for immutable compile-time values. Do not accept
  `@comptime var` until compile-time reassignment has explicit semantics and
  evaluator support.
- Comma-separated clauses in a `for` loop zip their sources in order and stop
  when the first source is exhausted. Evaluate each source once, left to right.
- `for key, value in map` iterates the result of `map.entries()` and binds each
  pair's two tuple elements; `Map::entries()` preserves insertion order in its
  snapshot. Zip clauses continue to require a separate `in` source per variable.
- Make new syntax preserve an existing semantic contract where one exists. Do not add placeholder behavior for attributes whose runtime or ABI semantics are not implemented.
- Keep computational Error values inspectable. Postfix `!!` opts into propagation
  through synchronous callers until a caller's lexical handler or the host boundary;
  an async task reports its propagated Error through `await`.
- Async is an implemented cooperative runtime foundation, not a future milestone.
  It defines task scheduling, structured joining, cancellation, I/O, resource
  limits, and shutdown; it does not promise CPU parallelism. Keep supported
  operations and remaining gaps aligned with `ROADMAP.md` and
  `docs/ASYNC_DESIGN.md`.
- Keep native JIT opt-in and profile-guided. The interpreter remains the
  semantic authority: unsupported code and executions with runtime metering
  stay in the VM, and disabled JIT must not collect profiles.
- Define `char` as one Unicode scalar value. Keep it distinct from `str`, and make string iteration yield `char` values.
