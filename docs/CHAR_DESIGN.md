# Character type

## Contract

- `char` stores exactly one Unicode scalar value. It is not a UTF-8 byte or a
  grapheme cluster, and no normalization is performed.
- A character literal uses single quotes and must decode to exactly one scalar,
  for example `'A'`, `'λ'`, or `'\n'`.
- Character literals support `\'`, `\\`, `\n`, `\t`, `\r`, `\0`, and the
  existing ASCII-only `\xNN` escape. Unicode scalars can otherwise appear
  directly in UTF-8 source. A `\u{...}` escape is not part of this first slice.
- `char` and `str` are distinct and do not coerce implicitly. `char as str`
  creates a one-scalar string; `str as char` succeeds only for a string with
  exactly one scalar and otherwise produces the language's recoverable Error.
- JSON stringification encodes `char` as a one-scalar JSON string; parsing that
  JSON string produces `str`, because JSON has no character type.
- Character equality and ordering compare Unicode scalar values. Arithmetic on
  `char` is not defined. String iteration yields one `char` per Unicode scalar.
- Character literals work in compile-time values and character match patterns.
  Interpolation and ordinary string formatting render the scalar as text.
- Bytecode encodes a character as a validated `u32` scalar. The C plugin ABI
  uses a dedicated type tag and the existing integer field for its scalar value;
  `CogValueView` layout and ABI version remain unchanged.

## Implementation order

1. Add lexer, AST, parser, type-checker, and compile-time support for character
   expressions, annotations, type tests, and match patterns.
2. Add the runtime `char` value, string iteration, comparisons, conversions, and
   array/SDK type contracts.
3. Add checked bytecode serialization and verifier flow facts, then update the
   language, runtime, and SDK references.

The contract leaves integer/codepoint conversion out until the later `uint`
design, so characters do not depend on the unresolved integer-width decision.
