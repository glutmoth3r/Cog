# Numeric types

## Status

`int` is a checked signed 64-bit integer, `uint` is a checked unsigned 64-bit
integer, `bigint` is an exact signed arbitrary-precision integer, and `float` is
IEEE 754 binary64. `decimal` remains deferred.

## Integer model

`int` and `uint` are fixed-width 64-bit types, so their behavior does not vary by
target. `int` remains the default within its signed range and the usual type for
indexes and signed arithmetic. `uint` is the default for larger positive values
through `u64::MAX`. Neither fixed-width type silently grows when an operation
overflows. `bigint` handles exact integer values outside both ranges and grows
subject to the optional runtime digit limit. Defer narrower `i8`–`i32` and
`u8`–`u32` types until storage, ABI, or protocol needs justify them.

## Integer literals

Integer literals retain their unsigned magnitude through parsing. Values
through `i64::MAX` default to `int`; larger positive values through `u64::MAX`
infer `uint`; larger values infer `bigint`. The negative value
`-9223372036854775808` remains `int`; smaller values infer `bigint`. An expected
`uint` or `bigint` type also gives integer literals their type in supported typed
contexts.

```text
const ordinary: int = 42;
const contextual: uint = 42;
const limit: uint = 18446744073709551615;
const beyond_u64: bigint = 18446744073709551616;
```

An expected `int` does not convert a `uint` or `bigint` literal outside the
signed range. Use an explicit checked conversion when changing value types.
Integer literal suffixes are not part of the syntax. Decimal and hexadecimal
integer literals support digit separators.

An expected `uint` type selects `uint` for small nonnegative literals in typed
contexts:

```text
fn repeat(count: uint) -> uint { return count + 1; }
const next = repeat(4);
```

An expected `bigint` type similarly selects `bigint` for integer-literal
expressions. Non-literal `int` and `uint` values require an explicit cast.

## Operations and conversions

- Integer arithmetic requires operands of the same integer type. There is no implicit
  signed/unsigned conversion. `uint` addition, subtraction, multiplication,
  division, and remainder are checked; overflow, underflow, and division by zero
  produce the existing recoverable `Error` values rather than wrapping.
- Unary negation is defined for `int`, not `uint`. Convert explicitly when a
  signed result is intended.
- `bigint` supports unary negation and `+`, `-`, `*`, `/`, and `%` when both
  operands are `bigint`. Division truncates toward zero; remainder follows the
  dividend's sign. Division by zero produces the ordinary recoverable Error.
  BigInt arithmetic does not mix implicitly with `int`, `uint`, or `float`.
- `<<` and `>>` shift an `int`, `uint`, or `bigint` by an `int` or `uint` amount.
  They are checked like other integer operations; see
  [bit shifts](LANGUAGE.md#bit-shifts).
- Equality and ordering compare `int`, `uint`, and `bigint` exactly, without
  converting integer operands through `float`. Integer/float comparisons also
  compare the exact integer against the binary floating value.
- Conversions are explicit. `int as uint` fails for negative values;
  `uint as int` fails above `i64::MAX`. `float as uint` requires a finite,
  nonnegative input below `2^64` and truncates toward zero. `uint as float` may
  lose precision, as `int as float` can today. Failed conversions produce
  `Error::Overflow`.
- String conversion accepts decimal integer spelling only. A negative spelling
  cannot convert to `uint`. `str::to_bigint` accepts signed decimal spelling;
  converting a BigInt to `int` or `uint` checks the destination range.
- Ranges require bounds of one integer type. Nonnegative `int` literals can take
  the type of a `uint` bound. A `bigint` bound gives integer-literal expressions
  in the bounds or step `bigint` type, and dynamic operands in that range must
  evaluate to `bigint`. BigInt ranges support negative steps; `uint` ranges
  only ascend. Collection indexes and lengths remain `int`.

## Language and runtime integration

The implementation includes:

- static and runtime type checks, nullable types, `is` patterns, interpolation,
  array/tuple contracts, and compile-time scalar evaluation including BigInt;
- checked arithmetic, comparisons, casts, range bounds, and standard-library
  conversions;
- `Eq`, `Ord`, and `Hashable` implementations for `int`, `uint`, and `bigint`,
  including BigInt use as a `Map` key;
- JSON parsing/stringification, where integer tokens map by range to `int`,
  `uint`, or `bigint` and serialize as exact JSON integers;
- bytecode value/type tags in `.cogb`, with canonical signed little-endian
  BigInt constants and a distinct runtime type tag;
- the native ABI, with an explicit unsigned 64-bit payload and type tag. C ABI v3
  uses a `uint64_t` integer field while preserving signed `int` through its bits.
  C ABI v3 does not expose BigInt values.

The VM and bytecode use a fixed little-endian 64-bit payload for `uint`. The
runtime resource limits remain applicable to parsing and serialization; `uint`
does not introduce arbitrary-precision allocation.

## Implemented and proposed next numeric types

### `bigint` (implemented)

- `bigint` is an exact, signed, arbitrary-precision integer. Unsuffixed integer
  literals keep the current `int` and `uint` ranges; larger positive literals and
  negative values below `i64::MIN` infer `bigint`. A `bigint` annotation also
  contextualizes an in-range integer literal. No suffix is needed to write a large
  integer.
- `+`, `-`, `*`, `/`, and `%` require integer operands of one type. Division
  truncates toward zero, and the remainder has the sign of the dividend. Division
  by zero remains a recoverable Error. Unary negation is supported. The only
  arithmetic failure specific to `bigint` is reaching a configured resource limit.
- Comparisons between `int`, `uint`, and `bigint` are exact. Arithmetic does not
  implicitly convert between them; literals can take an expected type, and values
  use explicit casts. Converting `bigint` to `int` or `uint` checks the destination
  range. Comparisons with `float` use exact integer-versus-binary-float comparison.
  Converting it to `float` is explicit, may lose precision, and returns Overflow
  when the result would be non-finite.
- `bigint` implements `Eq`, `Ord`, `Clone`, and `Hashable`, can be a `Map` key, and
  uses exact decimal text for interpolation and JSON serialization. JSON integer
  tokens outside the `int` and `uint` ranges parse as `bigint`. `str::to_bigint`
  parses decimal integer spelling and propagates Overflow, ParseFailure, or the
  configured numeric digit limit. Compile-time scalar evaluation supports BigInt.
  BigInt range iteration is supported in both single and parallel `for` clauses.
  WASI and native C ABI v3 payloads do not support BigInt.

### `decimal`

- `decimal` represents an exact finite base-10 value as a signed coefficient times
  a power of ten. Canonical values have no trailing zeroes in the coefficient and
  zero has one representation. It is a distinct type from binary `float`.
- A `d` suffix marks exact decimal literals, including exponent notation:
  `12d`, `1.250d`, and `1e-3d`. Unsuffixed fractional literals keep their current
  `float` type.
- Addition, subtraction, multiplication, and comparisons are exact. Remainder is
  `a - trunc(a / b) * b`, so it has the dividend's sign. Integer operands can be
  promoted losslessly to `decimal`; `float` does not mix with `decimal` without an
  explicit conversion. Division returns an exact result when its decimal expansion
  terminates; otherwise it returns
  `Error::InexactResult`. `decimal::divide(left, right, scale, mode)` provides
  explicit rounding for repeating results. `scale` is a nonnegative number of
  fractional digits. `TowardZero`, `HalfEven`, and `AwayFromZero` specify how
  discarded digits affect the result.
- Integer-to-decimal casts are exact. Decimal-to-integer casts truncate toward
  zero and check the destination range. Decimal-to-float is explicit, may lose
  precision, and returns Overflow outside the finite `float` range. Converting
  `float` to `decimal` parses its displayed decimal spelling. `str::to_decimal`
  parses exact decimal spelling and propagates ParseFailure or ResourceLimit.
- JSON fractional and exponent tokens parse as `decimal`; integer tokens use
  `int`, `uint`, or `bigint` by range. The existing `-0` token remains negative
  `float` zero. JSON serialization writes decimals as exact JSON numbers.

### Decimal resource and ABI proposal

`RuntimeLimits::max_numeric_digits` optionally caps BigInt values and integer
tokens parsed from JSON. BigInt operands and results are checked by runtime
arithmetic and conversions; BigInt JSON serialization is also capped. The default
remains unlimited, matching the other optional runtime limits; `max_buffer_bytes`
independently bounds JSON and formatted text. Compile-time BigInt evaluation uses
its separate compiler allocation budget.

Decimal will require bytecode tags and a format-version change. C ABI v3 remains
unchanged; exposing BigInt or decimal values to native plugins requires a later ABI
contract with owned canonical representations. BigInt is the foundation for future
decimal coefficient arithmetic. Bitwise and/or/xor/not on integers remain deferred
until their negative-value semantics are specified.

Fixed-width `iN`/`uN` types remain deferred. If they are later justified, their
relationship to checked arithmetic, casts, generic traits, ABI payloads, and
`int`/`uint` aliases must be specified before adding them.
