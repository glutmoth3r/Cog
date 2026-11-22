# Opt-in Error propagation

`@propagate` lets a runtime function return an unhandled Error at a
statement boundary. Cog still represents Error independently of the normal
type. A normal value, including `nil`, passes through the new checks.

```cog
@!include("std")
use err::Error;

@propagate
fn double(text: str) -> int {
    const number = text as int;
    number * 2
}

@propagate
fn double_or_zero(text: str) -> int {
    const result = capture { double(text) };
    when result is Error::ParseFailure { return 0; };
    result
}

fn main() -> nil {}
```

An implicit exit runs the same cleanup as an ordinary return. An ordinary
caller can store, inspect or recover the returned Error. A call under its
own lexical `try` remains an observation. Postfix `!!` still bypasses
catches in its own function and relays an explicit propagation marker to
callers. Either kind of unhandled Error reaching the program entry point
is reported to the host.

| Boundary | Behavior in an opted-in function |
| --- | --- |
| `const` and `var` | Observe the completed initializer before binding, including a typed initializer. Destructuring adds no recursive checks to aggregate leaves. |
| Discarded expression and statement block tail | Observe the completed value before discarding it. A branch executes only on its existing control flow path. |
| Assignment | Observe an ordinary local or global replacement before storing it. Cell and indexed mutations report the result of the existing mutation operation. Direct indexed writes can store Error leaves. A compound indexed assignment observes the completed arithmetic before writing. |
| Explicit return and function tail | Return the value after cleanup. A local `try` retains its existing observation rules before return. |
| Conditions, ranges and iteration | Use the existing control flow checks. An Error condition cannot choose a branch. |
| Call arguments, callbacks and defaults | Use their existing observation and evaluation rules. The caller's attribute applies at its own statement boundaries. An ordinary callee, closure or declaration default does not inherit it. |
| `await` | Preserve the existing task and cancellation rules. An Error from `await` is observed at the containing statement boundary. |
| Binary operators, aggregates and fallback | Preserve existing operand evaluation, top-level Error selection and fallback rules. Implicit propagation adds no check between strict operands or recursive check of aggregate leaves. |
| `defer` | Observe statements in cleanup. Unhandled cleanup errors use the existing primary and suppressed Error ordering. |

`capture { ... }` is contextual syntax inside an opted-in function. It
preserves the result when the capture expression itself sits at an implicit
observation boundary. It permits `const saved = capture { risky() };` and
`return capture { risky() };`. Nested statements still have their own
observation boundaries. Returning an Error from a capture does not mark it
as a normal value. Reusing that result in another expression can observe it
at a later boundary.

An enclosing lexical `try` still observes errors evaluated in the capture
block. Nested `try/catch` and `?!` can recover before the capture produces
its result. `??` still only selects a fallback for `nil`. Postfix `!!`
inside a capture exits the callable. A closure is a separate callable and
does not inherit the parser or propagation mode of an opted-in function.

The compiler lowers the attribute and capture syntax to existing bytecode
instructions and keeps opted-in functions out of source inlining. The
bytecode format and function signature metadata are unchanged. The existing
execution backends can use their normal VM fallback for instructions they
do not lower. The attribute applies to runtime definitions, including
`@async` definitions. The compile time evaluator and the `-> !T` return
contract are outside this implementation.
