# Standard library

## Declared build inputs

`@!include("std") use build;` exposes comptime-only `build::include_str(path)` and
`build::include_bytes(path)`. The `try_include_str` and `try_include_bytes` variants
return nil only when the file is absent. Read/UTF-8 failures are catchable Error
values; unhandled Errors at mandatory comptime roots stop compilation.

Paths are relative to the project root in project mode, invocation CWD in explicit
file mode, or the dependency's root in dependency code. Absolute paths and escapes
through `..` or symlinks are compilation errors. This policy applies to compiler
inputs; runtime filesystem APIs retain normal process permissions. Embedded inputs
remain usable after their source files are removed. See the
[build-input contracts](SEMANTIC_CONTRACTS.md#implemented-declared-build-inputs).

The project entry point must include the standard package with
`@!include("std")` before any source imports standard-library modules. Then
import modules explicitly: `use io;`, `use str;`, or `use math::PI;`.
The `clone`, `eq`, and `ord` modules define traits for generic code. The `iter`
module defines the protocol for statically dispatched user-defined `for` loops.
Selective and wildcard imports work for public members. The ordinary prelude
contains `len(value)` and `exclusive(value)`. Imports do not grant host permissions.
`exclusive` inspects runtime reference counts for supported shared values; it is
not an ownership or mutation-capability guarantee.
All functions can fail computationally; see [errors](ERRORS.md).
Runtime byte and resource quotas are optional embedder settings on `RuntimeLimits`;
`max_buffer_bytes`, `max_resources`, and `max_numeric_digits` default to `None`.
Their documented limits
apply when an embedder explicitly sets `Some(n)`. This does not cap total process
memory or make OS allocation failures recoverable.

## User-defined iteration

The `iter::IntoIterator[Iter, Item]` trait converts a collection into an iterator;
`iter::Iterator[Item]` supplies `next(mut self) -> bool` and `current(self) -> Item`.
`for` calls `current()` only after `next()` returns true. These methods are
statically dispatched, and Errors they return use ordinary propagation and
`try`/`catch` rules when explicitly propagated with `!!`. Implementations must
target concrete types; generic target
implementations and iteration through a generic trait bound are not supported.
See [user-defined collection iteration](LANGUAGE.md#user-defined-collection-iteration)
for a complete example. `iter::range(start, end)` provides a concrete
`Range` and `RangeCursor` for half-open ascending integer ranges.
An inverted range is empty. The cursor has no current element before the
first successful `next()` or after exhaustion.

Source modules in `core/` and native functions share the resolver. The tables below
describe successful results, not additional source annotation syntax. Native calls
take positional arguments only. Source functions support named/default arguments.

## Text and output

| Call | Result / behavior |
| --- | --- |
| `io::print(value)` | Render, write to stdout, and flush; normal nil |
| `io::println(value)` | As print, followed by a newline; normal nil |
| `io::read_line()` | Read stdin, removing trailing CR/LF; EOF is IoReadFailed |
| `io::write_text(handle, text)` | UTF-8 encode and write all text to the selected stream; does not flush |
| `str::trim(text)` | Remove Unicode whitespace from both edges |
| `str::trim_start(text)` / `str::trim_end(text)` | Remove leading / trailing whitespace |
| `str::starts_with(text, prefix)` / `str::ends_with(text, suffix)` | Exact prefix / suffix test; an empty pattern matches |
| `str::contains(text, part)` | Exact substring test; an empty pattern matches |
| `str::replace(text, pattern, replacement)` | Replace all exact non-overlapping matches |
| `str::split(text, separator)` | New array of strings; separator must be nonempty |
| `str::join(parts, separator = "")` | Join an array of strings; empty array produces `""` |
| `str::to_int(value)` / `str::to_uint(value)` / `str::to_bigint(value)` / `str::to_float(value)` | Checked numeric conversion, also available through `as` and receiver methods |
| `str::to_bool(value)` / `str::to_str(value)` | Boolean / string conversion |
| `str::compare(left, right)` | Lexicographic comparison; negative, zero, or positive int |
| `str::builder()` | A new shared mutable string builder |
| `fmt::write(builder, template, value)` | Append formatted text; return the same builder |
| `str::build(builder)` | Immutable string snapshot of a builder |

```cog
@!include("std")
use io;
use str;
use fmt;

fn main() -> nil {
    var text = str::builder();
    fmt::write(text, "count={}; ", 3)!!;
    const snapshot = str::build(text)!!;
    fmt::write(text, "{}", "done")!!;
    const completed = str::build(text)!!;
    io::println(snapshot)!!;
    io::println(completed)!!;
}
```


`fmt::write` replaces the first `{}` in its template; later placeholders remain
literal. Without a placeholder, it appends the template without rendering an
unused value, but an Error value still propagates. This runtime formatting API is
separate from language interpolation. Failure preserves the builder's existing
prefix through all aliases, including self-formatting. Buffer limits count UTF-8
bytes; rendering has a traversal-depth limit of 64.

`str::to_bigint` accepts `int`, `uint`, `bigint`, or a signed decimal integer
string. It observes `max_numeric_digits`; BigInt-to-int/uint conversions check
the destination range. Conversions propagate Error. `bytes::render` and library printing deliberately
render errors for diagnostics. Float-to-int and float-to-uint check finite and
range bounds; integer conversions between `int` and `uint` are checked. Integer-
to-float can lose precision. Numeric string parsing uses the current Rust parser;
it is distinct from Cog's source literal grammar. Nonfinite float strings can be
parsed, but converting their result to int produces Overflow.

Trim is implemented in Cog and recognizes the 25 Unicode White_Space codepoints:
U+0009..U+000D, U+0020, U+0085, U+00A0, U+1680, U+2000..U+200A, U+2028, U+2029,
U+202F, U+205F, U+3000. Interior text, NUL, U+FEFF BOM, and U+200B ZERO WIDTH SPACE
are preserved. It copies input into a temporary byte buffer, so buffer limits apply
even when no trimming is needed. Runtime is linear in input size.

Search, replace, and split compare complete UTF-8 sequences without case folding or
Unicode normalization. Replace and split use non-overlapping matches from left to
right. Replace rejects an empty pattern with InvalidArgument and returns the original
text when there are no matches. Split preserves empty fields:
`str::split(",a,,", ",")` is `["", "a", "", ""]`.
An empty separator produces InvalidArgument. Join requires string elements and
propagates an Error element; it does not implicitly render other values.

These helpers copy their input strings into byte buffers; join uses a string builder.
Existing buffer limits apply to these allocations and the joined/replaced result.
Prefix and suffix tests are linear in the pattern size after copying. Contains,
replace, and split use a simple scan, with worst-case time proportional to text size
times pattern size.

```cog
@!include("std")
use io;
use str;

fn main() -> nil {
    const fields = str::split("red,green,blue", ",")!!;
    io::println(str::join(fields, " / ")!!)!!;
}
```


A prompt printed to stdout is separate from the input returned by `io::read_line`. Use
`str::trim(io::read_line()) as int` to accept whitespace around a number.

### Text search and slices

`str::find(text, pattern)` and `str::rfind(text, pattern)` return a
zero-based Unicode scalar index or nil. The empty pattern matches at zero
for find and at the scalar length for rfind. Reverse search includes
overlapping matches. `count` counts nonoverlapping matches.
`split_once` and `rsplit_once` return the portions on either side of
the first or last separator, or MissingKey when absent. Count and split
reject empty patterns. Matching compares exact UTF-8 sequences without
normalization and uses linear-time search.

`lines` splits on LF, dropping CR only when directly before LF. It keeps
interior blank lines and omits the empty field after a final newline.
`repeat` accepts a nonnegative count, checks output size, and builds one
output buffer. `remove_prefix` and `remove_suffix` remove a single exact
affix when present. An empty or absent affix leaves the text unchanged.

### Text utilities

| Call | Result / behavior |
| --- | --- |
| `str::byte_len(text)` | Number of UTF-8 bytes; `len(text)` counts Unicode scalars |
| `str::from_bytes(buffer)` | Strict UTF-8 decoding of a whole byte buffer |
| `str::pad_start(text, width, fill = ' ')` / `str::pad_end(text, width, fill = ' ')` | Add the `char` fill before / after text until it has `width` Unicode scalars |
| `str::is_blank(text)` | Whether text is empty or only Unicode whitespace, as recognized by `trim` |
| `str::to_ascii_lowercase(text)` / `str::to_ascii_uppercase(text)` | Change ASCII letters and keep every other scalar |
| `str::to_int_or(text, fallback)` | Parse a string like `str::to_int`; return `fallback` on ParseFailure |

Padding never truncates: text already at least `width` scalars long is
returned unchanged. A negative width produces InvalidArgument naming the called
function. Width counts scalars, not terminal columns or grapheme clusters.
Padding obeys the runtime buffer limit. `to_int_or` also uses the fallback for
empty, space-surrounded, and out-of-range numbers. An Error passed as an argument
still propagates. Case conversion and `byte_len` copy the input into a byte
buffer, so buffer limits apply.

```cog
@!include("std")
use io;
use str;

fn main() -> nil {
    const port = str::to_int_or("not a number", 8080)!!;
    io::println(str::pad_start(str::to_str(port)!!, 6, '0')!!)!!; // 008080
}
```

## Arrays

| Call | Successful result / behavior |
| --- | --- |
| `array::is_empty(items)` | bool; inspect length only |
| `array::first(items)` / `array::last(items)` | First / last element, or nil for an empty array |
| `array::index_of(items, value)` | First matching zero-based index, or nil when absent |
| `array::contains(items, value)` | bool; stop at the first match |
| `array::map(items, transform)` | Fresh array of callback results, in input order |
| `array::filter(items, predicate)` | Fresh array containing elements whose predicate returns true |
| `array::fold(items, initial, combine)` | Left fold in input order; returns `initial` for an empty input |
| `array::position(items, value)` | Typed equality search; inferred element type `T` must implement `Eq[T]` |
| `items.swap(left, right)` | Mutates one array, or returns IndexOutOfBounds without changing it |
| `array::min(items)` / `array::max(items)` | Select the least / greatest element using `Ord`; empty input returns InvalidArgument |
| `array::sum(items)` / `array::sum_float(items)` | Add an `int[]` / `float[]` from left to right; empty input gives `0` / `0.0` |
| `array::repeat(value, count)` | Fresh array holding `value` in each of `count` slots |
| `array::min_by_key(items, key)` / `array::max_by_key(items, key)` | Element with the least / greatest `Ord` key; empty input returns InvalidArgument |
| `array::frequencies(items)` | `Map[T, int]` of occurrence counts for `Hashable` elements, in first-occurrence order |
| `array::slice(items, offset, count)` | Fresh outer array for `[offset, offset + count)` |
| `array::reversed(items)` | Fresh outer array in reverse order |
| `array::concat(left, right)` | Fresh outer array containing both inputs in order |

`sum` returns Overflow when a running total leaves the int range, even if
later elements would bring it back; `sum_float` follows ordinary float rounding,
infinity, and NaN behavior. `repeat` rejects a negative count with
InvalidArgument and stores the same value in every slot, so repeated arrays or
records are aliases, not clones. `min_by_key` and `max_by_key` call the
synchronous key function once per element, return the first of equal keys, and
propagate key and comparison errors. Key
functions share the reference access rules of map, so keyed selection over
reference-valued elements needs a writable source.

All operations except `items.swap` leave their outer input arrays unchanged. A
nil element is indistinguishable from an empty array through first/last alone; use
is_empty when that distinction matters. Slice accepts an empty range at the end;
negative or out-of-bounds ranges produce InvalidArgument rather than clamping.

`map`, `filter`, and `fold` accept synchronous function values, including named
functions and explicit-capture closures. They invoke callbacks from left to right,
once per input element; `filter` appends an element only after its predicate
returns true. Callback errors propagate immediately, and no partial result is
returned. `map` and `filter` return fresh outer arrays while preserving shared
element references. Indirect calls conservatively treat reference arguments as
potentially mutable and reference-valued results as read-only. Callbacks over
reference-valued elements may therefore require a writable source path, and
reference results should be cloned before mutation when `.clone()` supports them.

`index_of` and `contains` use Cog's `==`: mixed int/float comparisons compare
the exact integer with the floating value without rounding the integer first,
different normal types compare unequal, and equality between arrays or records is
not implemented and can produce TypeMismatch. Search propagates an Error argument
or an Error encountered before a match. First/last return the selected element,
including Error. Slice,
reversed, and concat propagate Error as they consume elements; an unselected Error
does not affect slice, and is_empty inspects no elements.

Fresh outer arrays retain the element type of their inputs, including when the result
is empty. They retain shared references to nested arrays, records, and other shared
values; they do not perform a deep copy or grant writable access to elements obtained
through a read-only path. The existing conservative alias analysis applies.
`.empty_like()` returns a fresh empty array with the receiver's recursive element
type; it is used by the array utilities to preserve type information for empty results.
`position` instead requires a statically known element type that implements `Eq[T]`;
the built-in implementations cover int, uint, bigint, float, bool, char, nil, and str. Programs
can add `Eq[T]` implementations for their own types. `min` and `max` propagate
errors found while reading the array and return the original selected element,
preserving aliases for shared values. Structural `.clone()` creates an
independent array container and recursively copies nested arrays and tuples when all
elements are supported primitive or nested container values. It does not share the
mutability of the source container or its nested containers, while repeated nested
references within the source remain shared within the copy. Clone recursion is limited
to 64 nested arrays/tuples; deeper values return ResourceLimit.

```cog
@!include("std")
use io;
use array;

fn main() -> nil {
    const original = [1, 2, 3];
    var result = array::reversed(original)!!;
    result.append(4);
    io::println(original)!!; // [1, 2, 3]
    io::println(result)!!;   // [3, 2, 1, 4]
}
```

### Typed array algorithms

| Call | Successful result |
| --- | --- |
| `any(items, pred)`, `all(items, pred)` | Short-circuit boolean tests. On an empty input, any is false and all is true |
| `find(items, pred)`, `find_index(items, pred)` | First element or zero-based index. A missing element returns `Error::MissingKey` from find and nil from find_index |
| `count_if(items, pred)` | Number of true predicate results |
| `take(items, n)`, `drop(items, n)` | Fresh prefix or suffix. Counts greater than length are clamped. Negative counts return InvalidArgument |
| `partition(items, pred)` | Tuple of two fresh arrays, accepted items followed by rejected items |
| `flatten(items)`, `flat_map(items, transform)` | Concatenate nested arrays or callback results in order |
| `zip(left, right)`, `enumerate(items)` | Fresh arrays of pairs. Zip stops at the shorter input, enumerate starts at zero |
| `chunks(items, size)` | Consecutive fresh arrays including a shorter last chunk when needed |
| `windows(items, size)` | Overlapping complete windows with stride one, each with its own outer storage |
| `dedup(items)`, `unique(items)` | Keep the first of adjacent equal elements using `Eq[T]`, or the first of every equal key using `Hashable` |
| `group_by(items, key)` | `Map[K, T[]]` with keys ordered by first occurrence and stable item order within each group |
| `scan(items, seed, combine)` | Each post-step accumulator in left-to-right order. The seed itself is omitted |

These functions are in `array`. Chunk and window sizes must be positive.
An oversized window produces an empty array. All returned outer arrays
are independent and retain their element types, including empty results.
Shared elements and accumulators keep their ordinary aliases. Callback
calls run once per visited element and stop at the first propagated Error.
A successful find may return a stored nil. These calls share the
reference access rules for map, filter, and fold above.

Related helpers: `clone::value(item)` returns `item.clone()` for any type implementing `Clone`. `text_order::compare(left, right)` compares two strings by UTF-8 bytes and returns -1, 0, or 1. `hash::of(value)` returns an int hash for int, uint, bigint, bool, char, or str values (other types produce TypeMismatch); the hash is seeded per process, so do not persist it. `str::to_char(text)` converts a one-scalar string to char, returns a char unchanged, and produces ParseFailure for empty or longer strings.

## Collections

`collections::Map[K, V]` is an insertion-ordered hash table. Its
empty constructor is `Map::new[K, V]()`. `Map::from(entries)` builds one from an
array of `(key, value)` tuples and infers both types from that array. Duplicate keys
follow `set` semantics: the last value wins and the first insertion position stays.
An expected result type can also infer the arguments to `new`:
`const scores: collections::Map[str, int] = collections::Map::new();`. `Map`
supports `int`, `uint`, `bigint`, `str`, `bool`, and `char` keys. Custom key types implement
`collections::Hashable`:

```cog
@!include("std")
use collections;

struct UserId { value: int }

extend UserId with collections::Hashable {
    fn hash_code(self) -> int { self.value }
    fn equals(self, other: UserId) -> bool {
        self.value == other.value
    }
}

fn main() {
    var users = collections::Map::new[UserId, str]();
    users.set(UserId(value: 7), "Ada");
}
```

Equal keys must return the same `hash_code`, and a key's hash and equality
behavior must remain stable while it is stored. Negative hash codes are valid.
Built-in keys use process-keyed hashes, so their numeric hash codes are not
stable across processes and must not be persisted.

| API | Behavior |
| --- | --- |
| `.len()`, `.is_empty()`, `.capacity()` | Number of live entries, emptiness, and bucket count |
| `.contains(key)` | Tests key presence, including when the stored value is nil |
| `.get(key)` | Returns the value or `Error::MissingKey` |
| `.get_or(key, default)` | Returns the value or the default after one lookup |
| `.get_or_insert(key, default)` | Returns the existing value, including nil, or inserts and returns the eager default. One hash lookup |
| `.merge(other)` | Applies the source snapshot in source insertion order. Existing destination positions stay fixed |
| `.set(key, value)` | Inserts or replaces; returns true only for a new key |
| `.remove(key)` | Removes and returns the value, or `Error::MissingKey` |
| `.clear()` | Removes all entries while retaining bucket capacity |
| `.entries()`, `.keys()`, `.values()` | Insertion-ordered snapshots |

For a nilable value type, `contains` reports presence independently of the value:
declare a comptime alias such as `@comptime const MaybeInt: type = int?;`, then use
`Map::new[str, MaybeInt]()` and store `nil` under a key. `get` returns nil for that
present key and `Error::MissingKey` for an absent key.

`set`, `remove`, `clear`, `get_or_insert`, and `merge` require a
writable receiver. If a merge encounters an Error, entries it applied
before that Error stay applied. Updating a key preserves its original position; removing a key
and inserting it again places it at the end. Removal unlinks a bucket entry and
uses tombstones in the ordered slot array. The array is compacted during resize
or after tombstones exceed half its slots, preserving the order of live entries.
Lookup and removal are expected O(1) with well-distributed hashes; collisions
can make a bucket chain linear. Removal is amortized O(1), including occasional
compaction.

`Map` implements `Clone` when its key and value types implement `Clone`. It
creates independent map storage, copies keys through `Clone`, clones each live
value, and preserves insertion order. Snapshot arrays have independent outer
storage; values otherwise follow normal Cog aliasing rules.

```cog
@!include("std")
use collections;
use io;

fn main() -> nil {
    var scores = collections::Map::new[str, int]();
    scores.set("Ada", 10);
    scores.set("Lin", 8);
    scores.set("Ada", 12);
    io::println(scores.get("Ada"))!!;
}
```

### Set

Use `Set::new[T]()` or `Set::from(values)`. Elements implement
`Hashable`. The set uses the same hashing and equality contract as Map.
Insertion returns true only for a new key and keeps the first representative
of an equal key. Removal returns nil on success or `Error::MissingKey`.
Use `contains`, `len`, `is_empty`, `clear`, and `to_array` for queries,
reset, and an insertion-ordered snapshot.

`union`, `intersection`, `difference`, and `symmetric_difference`
return independent sets. Union takes the receiver's order and appends
new keys from the argument. Intersection and difference keep receiver
order. Symmetric difference lists receiver-only entries, then
argument-only entries. `subset` and `disjoint` return booleans.
Algebra reuses the stored hashes and propagates equality errors.
Basic operations are expected amortized O(1) and algebra is expected
linear in the number of visited slots with well-distributed hashes.

### Queues

| Type | Operations |
| --- | --- |
| `Queue[T]` | `new`, `from`, `push` at the back, `peek` and `pop` at the front, `len`, `is_empty`, `clear` |
| `Deque[T]` | `new`, `from`, `push_front`, `push_back`, `pop_front`, `pop_back`, `peek_front`, `peek_back`, `len`, `is_empty`, `clear` |
| `PriorityQueue[T]` | `new`, `from`, `push`, `pop`, `peek`, `len`, `is_empty` |

Queue and Deque use a geometrically growing ring. Inserts are amortized
O(1), reads and removal are O(1), and removal clears the extracted slot
immediately. Capacity stays allocated until clear. Popping or peeking an
empty structure returns InvalidArgument. A queue with nilable elements
can return nil as a successful element.

PriorityQueue removes the least element under `Ord`. Inserts and removals
take O(log n) comparisons, peek is O(1), and from uses n inserts.
Equal elements have unspecified ordering. Comparisons finish before the
heap is changed, so a comparison Error leaves its contents intact.
A custom comparator must keep stored priorities stable and avoid mutating
the same heap during comparison.

These types share storage through assignment. See the
[breadth-first traversal](../examples/breadth_first.cog) and
[word inventory](../examples/word_inventory.cog) for complete examples.

## JSON

The native `json` module provides `json::parse(text)` and
`json::stringify(value)`. Parsing maps JSON `null` to `nil`, booleans and strings
to their Cog counterparts, signed 64-bit integer tokens to `int`, positive integer
tokens through `u64::MAX` to `uint`, and finite fractional/exponent tokens to
`float`. Integer tokens outside the `int` and `uint` ranges parse as exact
`bigint` values, subject to `max_numeric_digits`. The JSON token
`-0` is kept as floating negative zero to preserve its sign. JSON arrays
become mutable heterogeneous Cog arrays; objects become records with string
field names. Duplicate object names are rejected as `Error::ParseFailure`. Decimal
numbers are represented by IEEE 754 `float` values. BigInt serialization uses
exact decimal digits and observes `max_numeric_digits`.

Stringification supports `nil`, bool, int, uint, bigint, finite float, char, string, arrays,
and records. A `char` serializes as a one-scalar JSON string; parsing it back
produces `str`, since JSON has no character type. Record fields are emitted in
lexicographic key order, so equal records produce deterministic text. Strings
use JSON escaping and preserve Unicode.
Top-level Cog `Error` values and Error leaves encountered inside arrays or records
propagate unchanged; no partial JSON string is returned. Unsupported values
(including bytes, handles, functions, variants, and iterators) produce
`Error::TypeMismatch`. A non-finite float, cyclic array/record graph, or conflicting
host borrow produces a catchable `Error` instead of panicking.

Records serialize as JSON objects using their stored field names, and parsing
does not preserve a nominal Cog struct type. JSON text is a data representation,
not a lossless encoding of Cog type identity or array type constraints.

Both directions use the runtime `max_buffer_bytes` limit. Decoding also charges a
conservative allocation budget for the created Cog value graph and limits nested
values to 64 levels. Encoding is streamed through a bounded writer and uses the
same depth limit. These limits bound this API's input/output and temporary JSON
work; they are not a process-wide memory quota. When `max_heap_objects` is set,
decoding reserves one managed-object slot for each pending array or record before
visiting its children. Reaching the quota produces a catchable
`Error::ResourceLimit` with the `"VM heap objects"` resource payload; the parser
may collect unreachable cycles once when the quota is reached.

```cog
@!include("std")
use io;
use json;

fn main() -> nil {
    const value = json::parse("{\"name\":\"Ada\",\"active\":true}")!!;
    io::println(json::stringify(value)!!)!!;
}
```


## Math, sorting, and random values

| Module | Public API |
| --- | --- |
| `math` | `abs`, `min`, `max`, `floor`, `ceil`, `round`, `pow`, `sqrt`, `ln`, `log2`, `log10`, `clamp`, `gcd`, `lcm`, `sign`, `div_floor`, `mod_floor`, `isqrt`, `is_nan`, `is_finite` |
| `math` constants | `PI`, `E`, `TAU`, `SQRT_2`, `SQRT_3`, `LN_2`, `LN_10`, `LOG2_E`, `LOG10_E`, `DEG_TO_RAD`, `RAD_TO_DEG` |
| `sort` | `quick`, `stable`, `sorted`, `is_sorted`; `insertion`, `selection`, and `bubble` remain educational algorithms |
| `search` | `binary`, `lower_bound`, `upper_bound`, `equal_range`; inferred comptime type `T` must implement `Ord` |
| `random` | `next()`, `range(lo, hi)`, `range_uint(lo, hi)`, `boolean()`, `Seeded`, `choose`, `shuffle`, `sample` |
| `entropy` | `bytes(count)`, bytes from OS entropy |

All sorting functions require elements implementing `Ord { compare(self, other: Self) -> int }`.
`quick`, `stable`, `insertion`, `selection`, and `bubble` mutate their input and
return `nil`; these in-place calls require a mutable binding. `quick` and
`selection` are unstable, while `stable`, `insertion`, and `bubble` preserve the
order of equal elements. `bubble` and `selection` are
quadratic teaching examples. `quick` uses median-of-three, three-way partitioning,
an insertion-sort cutoff, smaller-side recursion, and a heapsort fallback to bound
worst-case time and stack depth. `sorted(items)` returns an independently cloned,
sorted array and therefore requires both `Ord` and `Clone`; it leaves the input
unchanged. `is_sorted(items)` checks ascending order without mutation.
`quick` and `stable` use Rust's native sort for int, uint, bigint, float, bool, char, and str
arrays; other `Ord` implementations use the Cog algorithms.

`Ord.compare` uses a negative int for less-than, zero for equality, and a positive
int for greater-than. It may return an Error; sorting, searching, `min`, and `max`
propagate that Error and do not lose elements. A failure can leave an in-place
array partially reordered, but it remains a permutation of the original array.
Floating-point comparisons involving NaN return `Error::Unordered`; NaN is not
assigned an arbitrary place in the ordering.

The `search` functions require an array sorted in ascending order according to
`Ord`. `binary` returns an index of a matching element or `-1`; when matches
are duplicated, it may return any matching index. `lower_bound` returns the first
index whose element is greater than or equal to the target, and `upper_bound`
returns the first index whose element is greater than the target. Both bounds can
equal the array length when no element meets the condition. `equal_range` returns both bounds as a half-open interval. Empty arrays return
`-1` from `binary` and `0` from both bound functions. These functions do not mutate
the input.

`Ord` is implemented for int, uint, bigint, float, bool, char, and str. String comparison is
lexicographic by UTF-8 bytes; valid UTF-8 preserves Unicode scalar ordering. `char`
comparison uses Unicode scalar values. User types can implement `Ord` to use
sorting and selection functions. See the [language
reference](LANGUAGE.md) for compile-time type parameters and trait syntax.
`clone::Clone`, `eq::Eq[T]`, and `ord::Ord` are traits written in Cog; their
implementations are checked statically.

Math accepts int/uint/bigint/float arguments; other normal values produce TypeMismatch. Existing
Error values propagate unchanged. Primitive sorting uses Rust's standard slice
sorts; custom `Ord` implementations use the Cog fallback algorithms.

| Call | Result and limits |
| --- | --- |
| `math::abs(x)`, `math::min(a, b)`, `math::max(a, b)` | int, uint, bigint, or float; min/max retain the selected argument's type |
| `math::floor(x)`, `math::ceil(x)` | Integer inputs retain int/uint/bigint; floats produce int rounded toward negative/positive infinity |
| `math::round(x)` | int/uint/bigint inputs are unchanged; floats round to int, with ties toward positive infinity |
| `math::pow(base, exponent)` | Checked int, uint, bigint, or floating power; exponent must be a nonnegative int |
| `math::sign(x)` | int -1, 0, or 1 for any numeric type; NaN produces Unordered |
| `math::div_floor(a, b)`, `math::mod_floor(a, b)` | int quotient rounded toward negative infinity, and the matching remainder with the divisor's sign |
| `math::isqrt(x)` | int square root rounded down; negative arguments produce InvalidArgument |
| `math::is_nan(x)`, `math::is_finite(x)` | bool classification of a float |
| `math::sqrt(x)` | float square root; negative arguments produce InvalidArgument |
| `math::ln(x)`, `math::log2(x)`, `math::log10(x)` | float logarithm; zero/negative arguments produce InvalidArgument |

Integer arithmetic overflow and nonfinite/out-of-range integer rounding produce
Overflow. Integer rounding preserves int, uint, and bigint inputs without converting them to
float. BigInt arithmetic and `pow` observe `max_numeric_digits`. `pow` uses exponentiation by squaring; a negative exponent produces InvalidArgument,
and a non-int exponent produces TypeMismatch. Its result has the base's numeric type,
including exponent zero; `pow(0, 0)` is 1. Square root and logarithms preserve NaN and
positive infinity. Their finite calculations cover very small and very large inputs,
including subnormal floats. Logarithm domain errors identify the `math::ln` operation. Logarithms are
approximations; even exact powers can differ from an integer by floating-point
rounding. `abs` normalizes floating negative zero to positive zero.

`div_floor` and `mod_floor` produce DivisionByZero for a zero divisor.
Dividing the minimum int by -1 overflows in `div_floor`, while `mod_floor`
returns 0. Use `mod_floor` to wrap negative coordinates:
`math::mod_floor(-1, 64)` is 63, whereas `-1 % 64` is -1.

Random uses a 31-bit pseudorandom generator seeded once from OS entropy. `next()`
returns an int in `[0, 2147483648)`; `boolean()` returns bool. `range(lo, hi)` takes
int bounds and samples `[lo, hi)`. `range_uint(lo, hi)` accepts uint bounds and
returns uint. Both ranges reject empty intervals and widths above `2147483648`;
`range` also returns Overflow if its signed width calculation overflows. Rejection
sampling discards the incomplete remainder interval to avoid modulo bias. Bounds
are checked before requesting entropy. Entropy failures remain recoverable Error.
This generator is not cryptographic. Seed/partition/recursive helpers are private.

### Numeric utilities and seeded random streams

`math::clamp(value, low, high)` keeps the type of the selected numeric
operand and uses the language's exact mixed-type comparison. Reversed
bounds return InvalidArgument and NaN returns Unordered.
`math::gcd(a, b)` and `math::lcm(a, b)` operate on signed int and
return nonnegative results, or Overflow when the answer cannot fit an
int. GCD of two zeros and LCM with either operand zero return zero.

`random::Seeded::new(seed)` creates an independent deterministic stream,
normalizing a signed int seed modulo 2³¹. `next()` and
`range(low, high)` mutate the stream. Range uses rejection sampling and
accepts widths up to 2³¹. With a writable stream, `random::choose`
selects an element, `shuffle` permutes a writable array in place, and
`sample` returns a fresh array drawn by index without replacement.
Equal values at different indices may appear more than once in a sample.
Choose rejects empty arrays and sample rejects counts outside
`0..=items.len()`. This generator is intended for reproducibility.
Use `entropy::bytes` for OS entropy.

## Byte buffers

| Call | Successful result |
| --- | --- |
| `bytes::new(size)` | Zero-filled, fixed-length mutable buffer |
| `bytes::copy(buffer)` | Independent copy of the full buffer |
| `bytes::slice(buffer, offset, count)` | Independent copy of a checked byte range |
| `bytes::concat(left, right)` | New buffer containing both inputs |
| `bytes::from_str(text)` | New buffer containing UTF-8 bytes |
| `bytes::render(value)` | New buffer containing diagnostic display text |
| `bytes::len(buffer)` | Byte length |
| `bytes::get(buffer, index)` | Integer in 0..255 |
| `bytes::set(buffer, index, value)` | Checked mutation; nil |
| `bytes::to_str(buffer, offset, count)` | Strict UTF-8 decoding of a byte range |

The buffer type is spelled `bytes` in annotations, including struct fields and
`bytes[]` arrays. Buffers share storage through assignment. `bytes::set` requires writable access.
Narrow a dynamic value with `when value is bytes { ... }` before passing it to a `bytes` parameter.
Copy, slice, and concat allocate independent storage and obey the runtime buffer
limit; concat accepts aliases, including the same buffer for both arguments.
Invalid byte values, negative/out-of-bounds ranges, and invalid UTF-8 produce Error.
`len(str)` counts Unicode scalars; use `bytes::len(bytes::from_str(text))` for bytes.
The built-in `len(value)` and `value.len()` do not accept a `bytes` buffer; a
statically known buffer is a compile error, so call `bytes::len(buffer)`.

```cog
@!include("std")
use io;
use bytes;

struct Frame { payload: bytes, tag: str }

fn fill(target: bytes, value: int) -> nil {
    for index in 0..bytes::len(target)!! {
        bytes::set(target, index, value)!!;
    }
}

fn main() -> nil {
    var frame = Frame(bytes::new(3), "ascii");
    fill(frame.payload, 67);
    io::println(bytes::to_str(frame.payload, 0, 3)!!)!!;
}
```

### Binary integers

`binary::read_u64_le`, `read_u64_be`, `write_u64_le`, and
`write_u64_be` work on eight bytes at an explicit zero-based byte offset.
They read or write a `uint`. Writes require a mutable buffer and validate
the entire range before any byte changes. Invalid ranges return
InvalidArgument.

`binary::encode_varuint(value)` allocates a byte buffer containing canonical
unsigned LEB128. `read_varuint(buffer, offset)` returns a
`(value, next_offset)` pair without copying the buffer. It reports
ParseFailure for truncated or overlong encodings and Overflow if the
encoding would exceed 64 bits. Existing runtime buffer limits apply.

## ASCII and hexadecimal text

| Call | Successful result / behavior |
| --- | --- |
| `ascii::is_ascii(value)` | Whether a byte is in 0..127 |
| `ascii::is_alpha(value)` / `ascii::is_digit(value)` | ASCII letter / decimal digit test |
| `ascii::is_alphanumeric(value)` | ASCII letter or decimal digit test |
| `ascii::is_lowercase(value)` / `ascii::is_uppercase(value)` | ASCII lowercase / uppercase letter test |
| `ascii::is_whitespace(value)` | Space or a byte in 9..13, including vertical tab |
| `ascii::to_lowercase(value)` / `ascii::to_uppercase(value)` | Convert ASCII letters; leave other bytes unchanged |
| `hex::encode(data, uppercase = false)` | Encode a byte buffer as hexadecimal str, two digits per byte |
| `hex::decode(text)` | New writable byte buffer decoded from hexadecimal str |
| `base64::encode(data)` | Encode arbitrary bytes as padded Base64 str |
| `base64::decode(text)` | Decode strict padded Base64 into a new writable byte buffer |

ASCII functions take int byte values in 0..255; values outside that range produce
InvalidArgument. Bytes 128..255 return false from all classifiers and are unchanged
by case conversion. These functions do not perform Unicode character conversion.

Hex encoding uses lowercase by default; decoding accepts either case. Empty input
is valid in both directions. Odd digit counts, non-hexadecimal characters, prefixes
such as `0x`, and whitespace produce ParseFailure with `from: "hex", to: "bytes"`.
Decoded bytes need not be valid UTF-8; use bytes::to_str explicitly when text is
expected. Encoding and decoding are linear in input size. Existing buffer limits
apply to copied input and output buffers, including the doubled size when encoding.

Base64 uses the standard alphabet (A-Z, a-z, 0-9, +, /) and = padding. The empty
string is valid. Decoding requires a length divisible by four, and rejects
whitespace, URL-safe substitutions, misplaced padding, and nonzero unused bits in
the final padded sextet. Failures produce ParseFailure with from: "base64" and
to: "bytes". Decoding returns arbitrary bytes; valid UTF-8 is not implied.
Encoding and decoding are linear in input size. Decoding copies its text into a
byte buffer; both that copy and the resulting buffer use the existing byte limit.
Encoding allocates output text using the same limit.

```cog
@!include("std")
use io;
use bytes;
use hex;

fn main() -> nil {
    const encoded = hex::encode(bytes::from_str("Cog")!!)!!;
    const decoded = hex::decode(encoded)!!;
    io::println(encoded)!!; // 436f67
    io::println(bytes::to_str(decoded, 0, bytes::len(decoded)!!)!!)!!;
}
```


## Streams and files

| Call | Successful result / behavior |
| --- | --- |
| `io::stdout()` / `io::stdin()` | Managed standard stream handles |
| `io::open(path, mode, buffer_size)` | File handle; modes `r`, `r+`, `w`, `w+`, `a`, `a+` |
| `io::read_into(handle, buffer, offset, count)` | Number of bytes read; 0 at EOF or for an empty range |
| `io::write(handle, buffer, offset, count)` | Number of bytes accepted; partial writes and 0 are allowed |
| `io::write_all(handle, buffer, offset, count)` | Retry partial/interrupted writes; nil, or Error on WriteZero |
| `io::seek(handle, offset, origin)` | New absolute position; origin is `"start"`, `"current"`, or `"end"` |
| `io::flush(handle)` / `io::close(handle)` | nil |

Stream transfer calls take the handle first, then the byte buffer, offset, and count.
Ranges are half-open `[offset, offset + count)`, including length/zero. Bounds and
permissions are checked before I/O. `read_into` mutates its second argument; immutable
handle tokens can be used without making their bindings var. Closing a handle
invalidates all aliases, even if flushing fails. Stale/double-close/foreign-context
handles produce InvalidResource; closed standard streams are not reopened.
TCP stream handles also work with the generic synchronous read/write calls. Those
calls can block the async executor; import `async::net` and use `net::read` and
`net::write_all` in async code.

File handles can be opened with `io::open` or `async::io::open`. Import `async::io`
as `use async::io as aio;` to call `aio::open`, `aio::read`, `aio::write_all`,
`aio::write`, `aio::read_exact`, `aio::read_all`, `aio::flush`, and `aio::seek`.
Async tasks share each managed file handle with
synchronous I/O and serialize operations on that file. Reads and writes
obey the existing filesystem capabilities, handle mode, and runtime byte limit.
`aio::read` accepts zero and returns an empty buffer; `aio::write` accepts an empty
buffer and returns zero without I/O. `aio::write_all` snapshots and
flushes its bytes before completing; `aio::write` makes one write and flushes the
accepted bytes, returning the accepted count as `uint`; cancellation can be reported
after a write side effect. `aio::read_all` reads from the current position through
EOF and fails if the remaining content exceeds `max_buffer_bytes`;
`aio::read_exact` returns exactly the requested byte count or `Error::IoFailure` on
early EOF. `aio::seek` flushes buffered output first.
`aio::open` creates a file handle on the worker pool using the same modes and buffer
size rules as `io::open`. It checks capabilities and reserves a handle slot before
opening, so concurrent opens obey `max_resources`. A host open cannot be interrupted;
cancellation may leave a created or truncated file, but does not publish its handle.
`aio::read`, `aio::read_exact`, and `aio::read_all` also accept `io::stdin()`;
`aio::write`, `aio::write_all`, and `aio::flush` accept `io::stdout()`. Sync and
async calls share the same buffered input and output writer, so input is not split
between separate buffers. Standard-stream operations use `allow_read` and
`allow_print`; managed files use the filesystem capabilities and handle modes.
Blocking stdin reads cannot be interrupted while the host reader is waiting for
input. Cancelling such a task takes effect after the read returns, and runtime
shutdown waits for an in-flight blocking read to finish.

Zero buffer size disables file output buffering. A positive size enables buffering
for output, not read-ahead. Read/seek on a read-write file first flushes pending
output. Flush is not fsync or a durability guarantee. `read_line` and `read_into` on
stdin share buffered input; a failed read can have consumed a prefix already.

Use explicit/deferred close for deterministic lifetime. The VM also finalizes
managed resources at the end of execution. An automatically finalized stream does
not silently retry a previously reported write/flush failure; explicit operations
may retry. See [runtime](RUNTIME.md) for embedding limits and permissions.

`io::open` interprets relative paths from the process working directory, with the
same operating-system path rules used by Rust. Runtime file APIs are not rooted at
the package directory. Compile-time `build::include_*` paths use their separately
documented project/dependency bases.

## Filesystem operations

Asynchronous file operations live under `async::fs`. Import that namespace with
`use async::fs;` to call them as `fs::read_text(path)`.

| Call | Result / behavior |
| --- | --- |
| `fs::metadata(path)` | Record with `is_file`, `is_dir`, `is_readonly`, and OS-reported `len` fields |
| `fs::metadata(path)` | Cold `Task[record]`; reads metadata on the async I/O worker pool |
| `fs::read_dir(path)` | Array of immediate entry names, sorted lexically as UTF-8 strings |
| `fs::read_dir(path)` | Cold `Task[str[]]`; returns sorted UTF-8 entry names |
| `fs::read_bytes(path)` | Cold `Task[bytes]`; reads the file on the async I/O worker pool |
| `fs::write_bytes(path, bytes)` | Cold `Task[nil]`; creates or truncates a file and writes the bytes |
| `fs::read_text(path)` | Cold `Task[str]`; reads UTF-8 text on the async I/O worker pool |
| `fs::write_text(path, text)` | Cold `Task[nil]`; creates or truncates a file and writes UTF-8 text |
| `fs::read_lines(path)` | Cold `Task[str[]]`; reads UTF-8 lines, omitting line terminators |
| `fs::append_bytes(path, bytes)` | Cold `Task[nil]`; creates a file if needed and appends bytes |
| `fs::append_text(path, text)` | Cold `Task[nil]`; creates a file if needed and appends UTF-8 text |
| `fs::copy(source, destination)` | Cold `Task[uint]`; copies a file and returns its byte count |
| `fs::rename(source, destination)` | Cold `Task[nil]`; renames a file or directory |
| `fs::create_dir(path)` / `fs::create_dir_all(path)` | Create a directory, or the directory tree |
| `fs::create_dir(path)` / `fs::create_dir_all(path)` | Cold `Task[nil]`; create a directory or directory tree on the worker pool |
| `fs::remove_file(path)` / `fs::remove_dir(path)` | Remove one file, or an empty directory |
| `fs::remove_file(path)` / `fs::remove_dir(path)` | Cold `Task[nil]`; remove a file or empty directory on the worker pool |
| `aio::open(path, mode, buffer_size)` | Cold `Task[handle]`; opens a managed file on the worker pool using `io::open` modes |
| `aio::read(handle, max_bytes)` | Cold `Task[bytes]`; reads an existing file handle, bounded by `max_buffer_bytes` |
| `aio::read_exact(handle, size)` | Cold `Task[bytes]`; reads exactly `size` bytes or returns `Error::IoFailure` on early EOF |
| `aio::read_all(handle)` | Cold `Task[bytes]`; reads from the current position through EOF, bounded by `max_buffer_bytes` |
| `aio::write(handle, bytes)` | Cold `Task[uint]`; makes one write, flushes accepted bytes, and returns the accepted count |
| `aio::write_all(handle, bytes)` | Cold `Task[nil]`; writes a byte snapshot and flushes before completion |
| `aio::flush(handle)` | Cold `Task[nil]`; flushes an existing file handle |
| `aio::seek(handle, offset, origin)` | Cold `Task[int]`; seeks an existing file handle using `"start"`, `"current"`, or `"end"` |

Paths are interpreted from the process working directory. Metadata follows
symbolic links; `is_readonly` reports the OS readonly attribute and is not a
portable access check. Async filesystem calls run on the bounded worker pool and
check `allow_fs_read` or `allow_fs_write` when the task starts. File reads and writes
obey the runtime buffer limit; async writes snapshot their bytes when the task
starts, then create or truncate the target file. Append operations snapshot their
input when the task starts and write at the end of the target without truncating it.
Concurrent appends have no ordering or atomic-record guarantee; await them in sequence
when order matters. A host write error can leave a partial append.
Task cancellation is checked between 8 KiB read/write chunks and directory entries.
A cancelled write or append can leave a partial file. An individual blocked filesystem
call and `async::fs::copy` cannot be interrupted; they check cancellation before and after
the host call, so a mutation may already have completed when `Error::Cancelled` is
observed.
`async::fs::read_lines` uses UTF-8 `str::lines` semantics: `\n` and `\r\n` terminators
are removed, interior empty lines are kept, and a final terminator does not add an
extra empty line. It returns `ParseFailure` for invalid UTF-8. Copy and rename operate on host
paths without materializing file contents as Cog byte values, so the VM buffer
limit does not apply to them. `async::fs::read_text` returns
`ParseFailure` for invalid UTF-8. Directory listing fails with
`ParseFailure` if an entry
name cannot be represented as UTF-8, and does not return a partial result after an
OS or resource error. Entries are sorted by UTF-8 byte order for deterministic
results. The runtime buffer limit accounts for encoded entry-name or line bytes plus
a fixed allowance per Cog value/string reference; it does not bound all allocator or
process overhead. Filesystem failures produce `IoFailure` with the operation name and
OS message. `allow_fs_read` gates metadata/listing and file reads, and
`allow_fs_write` gates file writes, directory creation, and removal. The CLI's
ordinary runtime policy grants both; the restricted policy denies both.

## Paths

| Call | Result / behavior |
| --- | --- |
| `path::join(base, child)` | Join using the host platform's path rules |
| `path::is_absolute(path)` | Whether the path is absolute on the host platform |
| `path::parent(path)` | Parent path as str, or nil when there is no parent |
| `path::file_name(path)` | Final normal component as str, or nil when absent |
| `path::extension(path)` | Extension without the leading dot, or nil when absent |
| `path::with_extension(path, extension)` | Return a path with its extension replaced |

These are lexical path operations: they do not access the filesystem, resolve
symbolic links, normalize `.`/`..`, or guarantee that a path exists. Their separator,
root, prefix, and extension behavior follows the host OS. `join` and
`with_extension` return Error if the resulting path cannot be represented as Cog
UTF-8 text, and return ResourceLimit when the result exceeds the runtime buffer
limit. File opening still uses the process working directory and OS permissions.
`with_extension` returns `InvalidArgument` when the extension contains a path
separator according to the host OS.

## Async task selection

| Call | Successful result / behavior |
| --- | --- |
| `task::select(tasks)` | Cold `Task[int]`; returns the index of the first completed task in a nonempty array of spawned siblings |
| `task::select2(first, second)` | Cold `Task[int]`; two-task form of `task::select` |

`spawn(task)` queues a task as a structured child of the current async function, and `yield_now()` returns a cold `Task[nil]` that gives other ready tasks a turn when awaited.

Every input must already be a spawned child of the same async scope. Await the
selected handle to observe its result or Error; the structured parent still joins
all remaining tasks. Cancelling the selector removes its waiters without cancelling
the inputs.

## Time and processes

Asynchronous timers live under `async::time`; `use async::time;` gives the short
local calls `time::sleep` and `time::timeout`. `process::run` returns cold tasks by
default; use `process::run_sync` for blocking capture. Alongside the stdin and timeout
forms below, `run` accepts a timeout and per-run output limit, or stdin, an optional
timeout, and an output limit. Pass `nil` when setting a limit without a timeout.

| Call | Successful result / behavior |
| --- | --- |
| `time::sleep(seconds)` | nil; int/uint/float duration with subsecond precision |
| `time::sleep(seconds)` | Cold `Task[nil]`; timer completion does not block the executor |
| `time::timeout(task, seconds)` | Cold `Task[T]`; returns the source result or `Error::Timeout`, without cancelling the source |
| `time::now()` | Unix epoch seconds as int |
| `time::now_nanos()` | Unix epoch nanoseconds as int |
| `time::now_millis()` | Unix epoch milliseconds as int |
| `time::monotonic_nanos()` | Monotonic nanoseconds with an unspecified origin; compare differences only |
| `process::args()` | `str[]` containing program arguments, excluding the Cog executable |
| `process::env(name)` | Environment value as str, or nil when the name is absent |
| `process::run_sync(program, arguments)` | Captured process result; argument array of strings, no shell expansion |
| `process::run(program, arguments)` | Cold `Task[record]`; capture runs on the async I/O worker pool; cancellation kills the Unix process group and reaps the direct child (direct child only on other platforms) |
| `process::run(program, arguments, timeout_ms)` | Cold `Task[record]`; an int or uint third argument sets the process deadline |
| `process::run(program, arguments, stdin)` | Cold `Task[record]`; a bytes third argument is written to child stdin while both output streams are captured |
| `process::run(program, arguments, stdin, timeout_ms)` | Cold `Task[record]`; writes stdin and applies a process-specific deadline, or no deadline when `nil` |
| `process::run(program, arguments, timeout_ms, max_output_bytes)` | Cold `Task[record]`; applies a deadline and a total stdout-plus-stderr capture limit |
| `process::run(program, arguments, stdin, timeout_ms, max_output_bytes)` | Cold `Task[record]`; writes stdin, optionally applies a deadline, and limits total captured output |
| `process::shell(command)` | Explicit `/bin/sh -c` on Unix; unsupported on other targets |

Wall-clock results are not monotonic; use `monotonic_nanos` for elapsed-time
measurements. `async::time::sleep` and `async::time::timeout` validate durations like `sleep`;
an expired timeout produces `Error::Timeout` while its source task continues. If it is
a structured child, its parent still joins it and reports a later unobserved failure.
Negative sleep yields NegativeSleep; nonfinite or unrepresentable
durations and pre-epoch clocks fail, and i64 overflow is reported. On Windows,
durations that round to the OS `INFINITE` wait sentinel are rejected instead of
blocking forever. OS scheduling does not guarantee precise wakeups.

`process::args()` and `process::env(name)` read the current process state. Cog
arguments and environment values must be valid UTF-8; otherwise the call returns
`Error::ParseFailure`. The argument array and returned environment value obey the
runtime buffer limit. An empty environment name, or one containing `=` or NUL,
returns `Error::InvalidArgument`. A missing environment variable returns nil.
Both calls require the `allow_process_info` embedding capability; the CLI's
`--restricted` mode denies them. The ordinary CLI inherits the environment of
the process that launched it.

Process results contain `exit_code` (int, or nil after signal termination), `success`
(bool), `stdout`, and `stderr` (byte buffers). A nonzero exit is a normal result;
spawn/capture/wait failure is `Error::IoFailure(op, message)` with the OS cause.
Output exceeding the configured cap produces `Error::ResourceLimit`. Shell
script text is not included in host-generated launch errors. Decode output
explicitly with `bytes::to_str`.

`process::run_sync` is synchronous, with null stdin and inherited cwd/environment. The argument
array is validated before spawn. Output streams are captured concurrently, with
a separate buffer limit for each. On capture failure/limit, the direct child is
killed and reaped without joining the other reader. A descendant retaining that
pipe can keep the detached reader thread and pipe open until the descendant exits.
On successful capture, descendants retaining pipes may delay EOF indefinitely.
`process::run` uses the same launch, capture, per-stream `max_buffer_bytes` limit, and
`allow_exec` capability rules, and checks permission when its task starts. An explicit
`max_output_bytes` adds a total cap shared by stdout and stderr; exceeding it terminates
the child and returns `Error::ResourceLimit`. The value must be a nonnegative int or uint;
zero permits no output. If omitted, the existing per-stream runtime limit applies; that
runtime limit is unset by default, so pass this argument when output must be bounded.
For example, `process::run(program, arguments, nil, 128 * 1024)` caps combined output at
128 KiB without setting a deadline.
The blocking process operation runs on the bounded async I/O worker pool. On Unix it
starts the child in a new process group. Cancelling the task sends `SIGKILL` to that
group and reaps the direct child; other platforms stop only the direct child.
When the third `process::run` argument is an int or uint, it is a timeout in
`1..=300000` milliseconds.
If the direct child and both output streams finish before the deadline, it returns
the same result record as `process::run`; otherwise it kills and reaps the direct child
and returns `Error::Timeout`. On Unix timeout also kills members of the process group;
other platforms stop only the direct child. The deadline starts when the worker begins
the launch; time waiting in the worker queue is excluded. Descendants that create a
separate session or process group can outlive cancellation or timeout. The operation
still obeys `allow_exec` and the per-stream output limit. Streaming capture and launch
options remain unavailable.

When the third `process::run` argument is `bytes`, it is snapshotted when the task
starts and obeys `max_buffer_bytes`; the child receives EOF after that snapshot is
written. A child that closes stdin early may still return a normal result. Input is
written concurrently with stdout/stderr capture, so a child can safely produce output
before it finishes reading stdin. Output retains the existing independent per-stream
limits. A per-run total output limit follows the timeout: use `nil` for no deadline,
including in the five-argument stdin form. The timeout keeps its `1..=300000` ms range.
Cancellation stops the Unix stdin writer cooperatively and terminates the process group;
on other platforms, child termination is what releases a blocked stdin write.

## Networking

TCP tasks live under `async::net`. Import it with `use async::net;` to use short
local names such as `net::connect`.

| Call | Successful result / behavior |
| --- | --- |
| `net::exchange(host, port, payload, timeout_ms)` | Cold `Task[bytes]`; sends one request and reads the peer's response through EOF |
| `net::listen(host, port)` | Cold `Task[handle]`; binds a nonblocking listener on a worker |
| `net::accept(listener, accept_timeout_ms, stream_timeout_ms)` | Cold `Task[handle]`; accepts one connection and configures its socket timeout |
| `net::connect(host, port, timeout_ms)` | Cold `Task[handle]`; opens a context-scoped TCP stream and applies the timeout to later socket operations |
| `net::read(handle, max_bytes)` | Cold `Task[bytes]`; reads up to the requested byte count, with an empty buffer meaning peer EOF |
| `net::read_exact(handle, size)` | Cold `Task[bytes]`; reads exactly `size` bytes or returns `Error::IoFailure` on early EOF |
| `net::read_all(handle)` | Cold `Task[bytes]`; reads through peer EOF, bounded by `max_buffer_bytes` and the stream timeout |
| `net::write(handle, bytes)` | Cold `Task[uint]`; makes one write and returns the accepted byte count |
| `net::write_all(handle, bytes)` | Cold `Task[nil]`; snapshots and writes the entire byte buffer |
| `net::shutdown_write(handle)` | Cold `Task[nil]`; half-closes the write side and leaves the read side open |

`port` accepts an int or uint in `1..=65535`. `timeout_ms` accepts an int or uint in
`1..=300000`. For `async::net::exchange`, the deadline starts before DNS lookup and
bounds connect and transfer after resolution; DNS itself is synchronous and may
outlast the deadline. The exchange sends the payload, half-closes its write side, and
reads until peer EOF, subject to the runtime byte limit.

`async::net::listen` binds the requested nonzero port on the host worker pool. It
returns a listener handle scoped to the current runtime context, counts against
`max_resources`, and must be released with `io::close(handle)`. `async::net::accept`
accepts one connection with an `accept_timeout_ms` in `1..=300000` and sets its
read/write timeout from `stream_timeout_ms` in the same range. Only one accept may be
active per listener; an overlapping accept returns `Error::IoFailure`.

`async::net::connect` uses the same port and timeout ranges and returns a resource
handle scoped to the current runtime context. It counts against `max_resources` and
must be released with `io::close(handle)`. The timeout becomes the socket timeout for
each later read or write. `async::net::read` requires a positive `max_bytes` no larger
than the configured runtime byte limit; it returns up to that many bytes, with an
empty buffer representing EOF. `async::net::read_exact` accepts a nonnegative size up
to `max_buffer_bytes` and either returns exactly that many bytes or `Error::IoFailure`
if EOF arrives first. `async::net::read_all` holds the read side for the
whole operation, accumulates until EOF, and returns `Error::ResourceLimit` if the
stream exceeds `max_buffer_bytes`; its stream timeout bounds the entire operation.
Timeout or cancellation during exact or read-all may consume a prefix without
returning it. The over-limit read-all probe does not consume the next byte.
`async::net::write` snapshots the bytes and attempts one write, returning zero for an
empty buffer or the accepted count as `uint`; a short write is valid.
`async::net::write_all` snapshots the bytes
and retries until the complete buffer is sent. Both obey the byte limit. One read and
one write may run concurrently on a handle; overlapping reads or writes fail with
`Error::IoFailure`. The stream timeout bounds each write. Cancellation may happen
after a partial transfer and hide the accepted count.
`async::net::shutdown_write` half-closes only the local write direction.

These operations run on the bounded async worker pool and require the host's
`allow_network` capability; the CLI's `--restricted` mode denies them. Cancellation
polls accept, read, read-all, write, and exchange data-transfer operations at short intervals;
a cancelled write may already have sent a prefix. Queued connect, listen, and
shutdown-write jobs check cancellation before starting. Running connect/listen jobs
check again after DNS or the host call returns, so a cancelled task will not publish a
new stream or listener handle. Submitted work retains a cloned socket or listener
until it finishes or is cancelled, even if `io::close` has invalidated the Cog handle.
DNS resolution remains synchronous inside workers and cannot be interrupted. On Unix,
TCP connect attempts use nonblocking sockets and poll cancellation about every 10 ms;
on other platforms the system connect call can delay cancellation until it returns or
reaches the operation timeout. `async::net::exchange` includes DNS time in its deadline,
but a resolver call can outlast that deadline. Listener name resolution uses the same
synchronous host resolver and has no DNS deadline.
Socket timeouts become `Error::Timeout`; other network failures become
`Error::IoFailure`; invalid ranges become `Error::InvalidArgument`.

## Error names

`err::Error` provides TypeMismatch, DivisionByZero, Overflow, NegativeSleep, Timeout,
ParseFailure, IndexOutOfBounds, MissingField, MissingKey, Unordered, PermissionDenied,
ProcessFailed, IoReadFailed, IoWriteFailed, EntropyUnavailable, InvalidRangeStep,
InvalidResource, InvalidArgument, Cancelled, ResourceLimit, and IoFailure. Their
payload declarations are in
[core/err.cog](../core/err.cog). `err::kind(value)` and `value.error_kind()` return
the primary error kind as str, or `"ok"` for a normal value.
`err::message(value)` returns the diagnostic text of an Error as str without the
`Error: ` label that `bytes::render` shows, for example `"division by zero"`; it
returns `""` for a normal value. Both calls inspect an Error argument without
propagating it. Neither is available at comptime. `.unwrap()`
and `.expect(message)` crash on Error. Prefer patterns and explicit recovery or
propagation when failure is recoverable.
