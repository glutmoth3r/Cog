# Errors and cleanup

Cog separates a value's normal type from computational Error. `nil` is a normal
singleton value, not an Error. Compiler type/phase/name errors cannot be caught by
source code. Recoverable runtime failures produce Error; `crash` and invalid VM
state are terminal outcomes.

`nil` fills a normal absence role similar to Lua's `nil` or JavaScript's `null`,
but it is not Cog's failure signal. Unlike Rust's `Result<T, E>`, Error is not part
of a function's declared normal type. Unlike Python exceptions, a runtime failure
does not always unwind immediately: it can remain a value until an observing
operation or `!!` applies the documented handling rule.

## Runtime locations, warnings, and type hints

The CLI reports the source file, line, and column for terminal runtime errors
when the bytecode contains source maps. This applies to source projects,
`.cogb` files, and installed `.cogpkg` applications. Embedders can read the same
coordinate with `RunContext::last_source_location`. See
[Diagnostics](DIAGNOSTICS.md) for origins, call paths, propagation, and
structured access to suppressed failures.

Compiler warnings are non-fatal. For example, `while false` and literal
ascending ranges with no iterations report that their loop bodies never run.
Type mismatch diagnostics may suggest a checked `as` conversion, a nullable
destination such as `str?`, or the element positions that need attention in an
array or tuple. When a fix is unambiguous, a `help:` entry shows the exact
source span replaced by the suggested conversion. For `int`/`uint` arithmetic,
the compiler shows conditional alternatives because the safe conversion depends
on the value range.

For `a: int` and `b: bigint`, the replacement appears in context:

```text
help: convert this `int` operand to `bigint`
   |
 5 |     const c = (a) as bigint + b;
   |               ^^^^^^^^^^^^^
```

## Error values and inspection

```cog
@!include("std")
use io;
use err::Error;

fn main() -> nil {
    const result: int = "invalid" as int;
    when result is Error::ParseFailure(from, to) {
        io::println($"Cannot convert {from} to {to}")!!;
    } else {
        io::println(result)!!;
    }
}
```


An `int` annotation describes the normal result; it does not turn every runtime
failure into a compile error or excuse a normal type mismatch. Error patterns use
the standard `err::Error` enum. Import it explicitly. Whole-type checks, specific
variants, payloads, guards, and ordinary match arms work inside or outside catch.
`Error::Cancelled` is returned when an async task accepts a cancellation request;
it follows the same inspection, catch, and propagation rules as other recoverable
errors. `Error::Timeout` is returned by `time::timeout` when its deadline
expires; it does not cancel the source task. A structured parent still joins that
source task and reports its later Error unless the source is observed separately.
`task::select` and `task::select2` report the winning task index without observing its
outcome; awaiting the selected handle is still required to observe its value or Error.
Other spawned tasks remain joined by their parent, including when they later fail.

Outside an observing boundary, Error can be stored, returned, or discarded. Strict
binary operations evaluate both operands left to right once and preserve the first
Error; an Error in the left operand does not by itself skip the right operand.
Function-call arguments are observed in written order, so an Error argument stops
later argument evaluation. Boolean short-circuiting and fallback operators retain
their own lazy rules.

An `if` or `while` condition observes Error as it evaluates: neither branch/body
runs, later condition work after the Error is skipped, and a surrounding lexical
handler receives it. Earlier side effects remain. An Error used as an array index
prevents the read; a selected Error element remains inspectable with `when` and is
observed when used by an ordinary operation. Strict binary operators still evaluate
both operands before returning their first Error. Use `!!` for explicit propagation
when a value is otherwise stored, returned, or discarded without observation.

A normal aggregate may contain an Error component: `[1 / 0, 7]` outside try is a
normal array. Reading/using the first component observes Error; the whole array
does not become recursively poisoned. Writing an Error into an array element,
tuple position, record field, or collection stores it at that leaf. An Error in the
assignment receiver or index still propagates without mutation. JSON serialization
has no Error representation and returns an encountered Error unchanged.

## Lexical try/catch

```cog
@!include("std")
use io;
use err::Error;

fn main() -> nil {
    try {
        const value = "invalid" as int;
        io::println(value);
    } catch(error) {
        when error is Error::ParseFailure {
            io::println("Please enter an integer")!!;
        } else {
            error!!;
        }
    }
}
```


The first unhandled protected Error stops the try body, restores the operand-stack
boundary, runs cleanup for exited scopes, and enters catch with the original Error.
Earlier side effects are not rolled back. Reading an existing Error binding inside
try also observes it; arguments can fail before the callee is entered.

Handlers are lexical. A callee does not inherit its caller's handler for its own
operations. An unmarked Error returned from the callee remains a value and is
observed at the caller's call site. A marked Error from `!!` reaches the caller's
call site as propagation control: a lexical handler covering that call catches it;
without one, the caller relays it through cleanup. Explicit inspection with
`when`, `match`, `err::kind()`, or `.error_kind()` receives an Error value before
the surrounding handler; inspection does not clear an Error stored in a binding.

Catch is not protected by its own try. Errors in catch or a selected fallback can
reach an outer handler. An Error in a pattern guard is not a pattern mismatch.
Ordinary `return risky();` evaluates under local lexical observation and can enter
a local catch before returning.

## Explicit propagation with !!

```cog
@!include("std")
use io;

fn risky() -> int { 1 / 0 }

fn parse() -> int {
    defer { io::println("Leaving parse"); }
    try {
        risky()!!
    } catch(error) {
        crash("This local catch is bypassed by !!");
    }
}

fn middle() -> int {
    defer { io::println("Leaving middle"); }
    parse()
}

fn work() -> int {
    defer { io::println("Leaving work"); }
    middle()
}

fn main() -> nil {
    try { io::println(work()); }
    catch(error) { io::println(error.error_kind())!!; }
}
```


Postfix `!!` evaluates its operand once. A normal value, including nil, passes
unchanged. Error exits the current function through LIFO cleanup, bypassing all
of that function's local catches. Synchronous callers relay the marked Error
through their own cleanup until a caller's lexical `try` handles it or it reaches
the VM/host. An async task is a propagation boundary: its Error becomes the result
observed by `await`. `!!` requires an enclosing function and is invalid at a
global initializer root. An explicit nested try inside the operand remains its
own boundary. `!!` checks the expression result, not Error values stored inside
aggregate leaves.

`!!` preserves the normal type: `int?` remains `int?`. It is neither nil unwrapping
nor a panic. In main, the same cleanup/propagation path returns Error to the VM/host,
causing unsuccessful program completion. It is not a direct process-exit primitive.
Prefix `!!x` remains ordinary boolean double-negation, `!(!x)`.

## Lazy fallbacks

```cog
@!include("std")
use io;

fn choose(value: int?) -> int { value ?? 10 }

fn main() -> nil {
    const absent = choose(nil);
    const recovered = ("invalid" as int) ?! 7;
    io::println($"{absent}, {recovered}")!!;
}
```


| Operator | Selects the right operand when | Other outcomes |
| --- | --- | --- |
| `value ?? fallback` | Left is normal nil | Normal non-nil and Error pass through |
| `value ?! fallback` | Left is Error | All normal values, including nil, pass through |
| `value!!` | No fallback | Error propagates through synchronous callers; normal values pass through |

The left operand evaluates once; the right evaluates only when selected, at most
once per expression evaluation. Both fallbacks are right-associative with equal
precedence below `||`. Parenthesize mixed chains when that improves readability.

`??` does not handle Error: inside try, a left-side Error enters catch. `?!`
explicitly handles its left operand before surrounding observation. The selected
right operand uses the surrounding lexical context, so its Error can enter catch.
`!!` inside either operand still exits the function. Unselected source is still
checked for semantic errors.

For `T? ?? T`, the normal result is T. `T? ?! T` still permits nil. This changes
the expression result, not the original binding's declared type. Control-flow
checks [narrow nullable bindings](LANGUAGE.md#narrowing) on paths that prove nil
is absent. General union inference remains pending.

## Defer and cleanup failures

`defer { ... }` registers cleanup in its declaration's lexical environment.
Exited defers run once in reverse registration order, including on return, break,
propagation, and lexical try exit. Normal results of defer bodies are discarded.
Return/tail values are saved before cleanup under existing shared/value semantics;
an array snapshot retains its aliases. Explicit `.clone()` is a separate operation
for statically cloneable arrays and tuples; see the [language reference](LANGUAGE.md).

With normal completion, an unhandled cleanup Error becomes the primary failure.
With an active Error, cleanup failures are retained as secondary/suppressed causes
and remaining cleanup continues. The Rust runtime exposes this through
`RuntimeError::WithSuppressed`; source patterns, kind, and payload inspect the
primary. A public Cog API for suppressed errors and complete provenance chains
is not implemented.

Managed streams can be closed with `defer { io::close(file); }`. Host context
finalization also closes resources and preserves primary/secondary failures.
Terminal `crash` does not follow the ordinary recoverable source cleanup path.
The embedding runtime still finalizes managed resources on terminal VM failure.

## Strings and diagnostic rendering

An interpolation hole producing Error keeps the string computation in Error state.
A lexical try observes the failure before the next hole; outside an observing
boundary, later holes remain eager and the first Error is preserved. `?!` inside
a hole handles that expression; around the whole string it handles the whole result.

Diagnostic rendering deliberately shows Error text: `io::println(error)` and
`bytes::render(error)` can render an inspected Error. This is distinct from a
conversion such as `error as str`, which propagates Error. Printing inside try
can itself observe an Error argument before the print call; inspect or handle the
value explicitly when choosing a diagnostic path.
