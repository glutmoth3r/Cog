# Diagnostics

The compiler returns source spans and warning or error severity. Runtime
failures are separate. A Cog `Error` can be stored and inspected before a
later operation handles it or it reaches the host.

For source artifacts, the VM records provenance when an Error first appears
as an instruction result. The record includes the bytecode instruction, the
source file and coordinates when present, the executing Cog function, and the
import identity when a native call produced the Error. It also saves the
active synchronous callers, innermost first. Returning an Error records the
call site, and a propagating return records the propagation site. When the
caller propagates automatically at the call expression that returned the
Error, both records name the same step, which is recorded once. An Error
that reaches the program entry boundary also gets an observation record.

```text
runtime error at example.cog:4:8: division by zero
  origin: example::divide at example.cog:4:8
  called from example::main at example.cog:9:18
  propagated through example::main at example.cog:9:18
  observed at instruction 5
Unhandled Error reached the program entry point.
```

Storage, aggregate reads, copying, and ordinary operations preserve the
original trace. Each propagated copy can extend its own trace independently.
The caller and propagation lists each store up to 24 frames and indicate
truncation. Cleanup failures are shown separately in execution order, with
their own origins where available. Host reports cap the number and depth of
nested suppressed diagnostics.

The CLI and Linux AOT executables share the renderer. Rust embedding can read
`RuntimeContext::last_runtime_diagnostic()` or
`sdk::RunContext::last_runtime_diagnostic()` after a failure. The record
implements `serde::Serialize` and contains the Error kind, message, trace,
and suppressed diagnostics. The context clears the previous diagnostic before
each run. The runtime Error's display text, patterns, and payload access keep
their existing behavior.

When the compiler inlines a function, its body keeps a logical frame. The
trace names the inlined function at the failing coordinate and marks it
`(inlined)`. The next frame is its caller at the call expression. Both frames
refer to the same bytecode instruction, and inlining does not otherwise
change the function and location sequence. Function names are module
qualified, as in the example below, when the CLI builds a file or project.
`ErrorTraceFrame` has an `inlined` boolean, and the serialized record always
includes it (`false` for ordinary frames).

```text
runtime error at example.cog:2:35: division by zero
  origin: example::ratio at example.cog:2:35 (inlined)
  called from example::work at example.cog:7:26
```

Bytecode versions 40 and later store a table of inline frames. Each record contains the
inlined function name, the optional call-site coordinate, and an optional
enclosing inline frame with a smaller index. Each source map range can name
its innermost inline frame. The loader rejects tables that are larger than
the instruction table, are nested more than 64 deep, contain forward parent
references, or contain frames that no source map range reaches. Version 39
artifacts have function names and no inline frames. Version 38 artifacts
also lack function names and report instruction coordinates only. Both remain
readable. Version 41 adds struct type tests; older versions reject that
type-test tag. Instructions without a source span retain their bytecode address in
the trace.

VM fallback and synchronous runtime calls from compiled code preserve
provenance. For errors reproduced by the VM after a native fault, the trace
records the instruction where the VM observed that fault, including its
inline frames. Ordinary Error values from async Cog bodies retain their
origin. Task creation, observation through `await`, and structured joins do
not yet add async causal frames.
