//! Behavior regressions for standard-library functions that previously had no direct callers.

use cog::{bytecode, interpreter, project, runtime::VmCapabilities};

fn run(source: &str) -> String {
    let source = format!(
        "@!include(\"std\") use io; use math; use random; use sort; use search; use ascii; \
         use base64; use binary; use bytes; use hex; use text_order; use clone; use err::Error;\n{source}"
    );
    let program = project::compile_source("stdlib-coverage.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with_capabilities(&program, &mut output, VmCapabilities::default()).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn ascii_classification_matches_documented_byte_ranges() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            for byte in 0..128 {
                const lower = byte >= 97 && byte <= 122;
                const upper = byte >= 65 && byte <= 90;
                const numeric = byte >= 48 && byte <= 57;
                const space = byte == 32 || (byte >= 9 && byte <= 13);
                require(ascii::is_ascii(byte)!!, "ascii");
                require(ascii::is_lowercase(byte)!! == lower, "lowercase");
                require(ascii::is_uppercase(byte)!! == upper, "uppercase");
                require(ascii::is_whitespace(byte)!! == space, "whitespace");
                require(ascii::is_alphanumeric(byte)!! == (lower || upper || numeric), "alnum");
            }
            require(!ascii::is_ascii(128)!!, "128 is not ascii");
            require(!ascii::is_ascii(255)!!, "255 is not ascii");
            require(ascii::is_whitespace(32)!! && ascii::is_whitespace(10)!!, "space and newline");
            require(!ascii::is_whitespace(95)!!, "underscore");
            require(ascii::to_lowercase(65)!! == 97 && ascii::to_uppercase(122)!! == 90, "case");
            when ascii::is_lowercase(-1) is Error::InvalidArgument {} else { crash("negative"); }
            when ascii::is_whitespace(256) is Error::InvalidArgument {} else { crash("256"); }
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn math_min_and_max_pick_operands_and_preserve_types() {
    let output = run(r#"
        fn main() {
            io::println(math::max(3, 7)!!)!!;
            io::println(math::max(-3, -7)!!)!!;
            io::println(math::max(4, 4)!!)!!;
            io::println(math::min(3, 7)!!)!!;
            io::println(math::min(-3, -7)!!)!!;
            io::println(math::max(1.5, 1.25)!!)!!;
            io::println(math::min(1.5, 1.25)!!)!!;
            io::println(math::max(2, 2.5)!!)!!;
            io::println(math::min(2, 2.5)!!)!!;
            when math::max("a", 1) is Error::TypeMismatch {} else { crash("max type"); }
            when math::min(1, true) is Error::TypeMismatch {} else { crash("min type"); }
        }
    "#);
    assert_eq!(output, "7\n-3\n4\n3\n-7\n1.5\n1.25\n2.5\n2\n");
}

#[test]
fn random_range_uint_is_bounded_and_rejects_invalid_ranges() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            var seen = [0, 0, 0, 0, 0];
            for draw in 0..2000 {
                const value = random::range_uint(10, 15)!!;
                require(value >= 10 && value < 15, "range_uint bounds");
                const slot = (value - 10)!!;
                seen[slot as int] += 1;
            }
            for count in seen { require(count > 0, "every value drawn"); }
            require(random::range_uint(7, 8)!! == 7, "single-value range");
            for draw in 0..200 {
                const wide = random::range_uint(18446744073709551000, 18446744073709551615)!!;
                require(wide >= 18446744073709551000, "high bounds");
            }
            when random::range_uint(5, 5) is Error::InvalidArgument {} else { crash("empty"); }
            when random::range_uint(9, 3) is Error::InvalidArgument {} else { crash("reversed"); }
            when random::range_uint(0, 4294967296) is Error::InvalidArgument {}
            else { crash("too wide"); }
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn seeded_random_streams_are_reproducible_and_bounded() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            var first = random::Seeded::new(42)!!;
            var second = random::Seeded::new(42)!!;
            var other = random::Seeded::new(43)!!;
            var same = true;
            var differs = false;
            for step in 0..50 {
                const a = first.next()!!;
                const b = second.next()!!;
                if a != b { same = false; }
                if a != other.next()!! { differs = true; }
                require(a >= 0 && a < 2147483648, "next bounds");
            }
            require(same, "same seed reproduces");
            require(differs, "different seeds diverge");
            var negative = random::Seeded::new(-1)!!;
            var wrapped = random::Seeded::new(2147483647)!!;
            require(negative.next()!! == wrapped.next()!!, "seed normalized modulo 2^31");
            var ranged = random::Seeded::new(7)!!;
            for step in 0..500 {
                const value = ranged.range(-3, 4)!!;
                require(value >= -3 && value < 4, "range bounds");
            }
            var left = random::Seeded::new(5)!!;
            var right = random::Seeded::new(5)!!;
            var a = [1, 2, 3, 4, 5, 6, 7, 8];
            var b = [1, 2, 3, 4, 5, 6, 7, 8];
            random::shuffle(left, a)!!;
            random::shuffle(right, b)!!;
            require(a == b && left.next()!! == right.next()!!, "shuffle reproducible");
            require(sort::sorted(a)!! == [1, 2, 3, 4, 5, 6, 7, 8], "shuffle is a permutation");
            const picked = random::sample(random::Seeded::new(9)!!, [10, 20, 30, 40], 3)!!;
            const again = random::sample(random::Seeded::new(9)!!, [10, 20, 30, 40], 3)!!;
            require(picked.len() == 3 && picked == again, "sample reproducible");
            require(random::choose(random::Seeded::new(1)!!, [5])!! == 5, "choose single");
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn quadratic_and_stable_sorts_agree_with_sorted_on_edge_inputs() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn check(input: int[]) {
            const expected = sort::sorted(input)!!;
            var quick = input.clone();
            sort::quick(quick)!!;
            require(quick == expected, "quick");
            var insertion = input.clone();
            sort::insertion(insertion)!!;
            require(insertion == expected, "insertion");
            var selection = input.clone();
            sort::selection(selection)!!;
            require(selection == expected, "selection");
            var bubble = input.clone();
            sort::bubble(bubble)!!;
            require(bubble == expected, "bubble");
            var stable = input.clone();
            sort::stable(stable)!!;
            require(stable == expected, "stable");
            require(sort::is_sorted(stable)!!, "is_sorted");
        }
        fn main() {
            const empty: int[] = [];
            check(empty);
            check([1]);
            check([2, 1]);
            check([1, 2, 3, 4, 5]);
            check([5, 4, 3, 2, 1]);
            check([3, 1, 3, 1, 2, 2, 3]);
            check([7, 7, 7, 7]);
            check([-5, 0, 5, -5, 0, 5, -9223372036854775807]);
            var long: int[] = [];
            for index in 0..100 { long << (index * 37) % 101 - 50; }
            check(long);
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn stable_insertion_and_bubble_sorts_keep_equal_key_order() {
    let output = run(r#"
        use ord::Ord;
        struct Item { key: int, tag: int }
        extend Item with Ord {
            fn compare(self, other: Self) -> int {
                if self.key < other.key { return -1; }
                if self.key > other.key { return 1; }
                0
            }
        }
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn make() -> Item[] {
            var items: Item[] = [];
            for index in 0..40 { items << Item(key: (index * 7) % 4, tag: index); }
            items
        }
        fn verify(items: Item[], label: str) {
            require(items.len() == 40, label);
            for index in 1..40 {
                const before = items[index - 1];
                const after = items[index];
                require(before.key <= after.key, label);
                if before.key == after.key { require(before.tag < after.tag, label); }
            }
        }
        fn main() {
            var a = make(); sort::insertion(a)!!; verify(a, "insertion");
            var b = make(); sort::bubble(b)!!; verify(b, "bubble");
            var c = make(); sort::stable(c)!!; verify(c, "stable");
            var d = make(); sort::selection(d)!!;
            for index in 1..40 { require(d[index - 1].key <= d[index].key, "selection order"); }
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn search_functions_report_documented_positions_with_duplicates() {
    let output = run(r#"
        fn main() {
            const items = [1, 2, 2, 2, 5, 9];
            io::println(search::lower_bound(items, 2)!!)!!;
            io::println(search::upper_bound(items, 2)!!)!!;
            io::println(search::lower_bound(items, 0)!!)!!;
            io::println(search::upper_bound(items, 10)!!)!!;
            const range = search::equal_range(items, 2)!!;
            io::println(range.0)!!;
            io::println(range.1)!!;
            const missing = search::equal_range(items, 4)!!;
            io::println(missing.0 == missing.1)!!;
            const none: int[] = [];
            io::println(search::lower_bound(none, 1)!!)!!;
            io::println(search::binary(items, 9)!!)!!;
            io::println(search::binary(items, 5)!!)!!;
        }
    "#);
    assert_eq!(output, "1\n4\n0\n6\n1\n4\ntrue\n0\n5\n4\n");
}

#[test]
fn encoding_helpers_round_trip_and_match_known_vectors() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            const raw = bytes::from_str("Man")!!;
            require(base64::encode(raw)!! == "TWFu", "base64 vector");
            require(base64::encode(bytes::from_str("Ma")!!)!! == "TWE=", "base64 pad one");
            require(base64::encode(bytes::from_str("M")!!)!! == "TQ==", "base64 pad two");
            require(base64::encode(bytes::new(0)!!)!! == "", "base64 empty");
            require(bytes::len(base64::decode("TWFu")!!)!! == 3, "base64 decode length");
            require(hex::encode(base64::decode("TWFu")!!)!! == "4d616e", "base64 decode");
            require(hex::encode(raw, true)!! == "4D616E", "hex uppercase");
            require(bytes::len(hex::decode("4d616e")!!)!! == 3, "hex decode");
            when hex::decode("zz") is Error::ParseFailure {} else { crash("hex invalid"); }
            when hex::decode("abc") is Error::ParseFailure {} else { crash("hex odd"); }
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn text_order_and_clone_follow_documented_contracts() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            require(text_order::compare("a", "b")!! == -1, "less");
            require(text_order::compare("b", "a")!! == 1, "greater");
            require(text_order::compare("same", "same")!! == 0, "equal");
            require(text_order::compare("ab", "abc")!! == -1, "prefix shorter first");
            require(text_order::compare("", "a")!! == -1, "empty first");
            require(text_order::compare("Z", "a")!! == -1, "uppercase before lowercase");
            require(text_order::compare("z", "é")!! == -1, "byte order for non-ascii");
            var original = [1, 2, 3];
            const duplicate = clone::value(original)!!;
            original << 4;
            require(original.len() == 4 && duplicate.len() == 3, "clone independent");
            require(clone::value(5)!! == 5, "clone scalar");
            io::println("ok")!!;
        }
    "#);
    assert_eq!(output, "ok\n");
}

#[test]
fn math_integer_helpers_cover_signs_boundaries_and_errors() {
    let output = run(r#"
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            const largest = "9223372036854775807" as int;
            const smallest = "-9223372036854775808" as int;
            for left in -9..10 {
                for right in -4..5 {
                    if right != 0 {
                        const q = math::div_floor(left, right)!!;
                        const r = math::mod_floor(left, right)!!;
                        require(q * right + r == left, "identity");
                        if right > 0 { require(r >= 0 && r < right, "positive range"); }
                        else { require(r <= 0 && r > right, "negative range"); }
                    }
                }
            }
            io::println(math::div_floor(-7, 2)!!)!!;
            io::println(math::mod_floor(-1, 64)!!)!!;
            require(math::mod_floor(smallest, -1)!! == 0, "minimum remainder");
            require(math::mod_floor(-1, largest)!! == largest - 1, "wide remainder");
            require(math::mod_floor(1, smallest)!! == smallest + 1, "minimum divisor");
            require(math::div_floor(smallest, 1)!! == smallest, "minimum quotient");
            when math::div_floor(smallest, -1) is Error::Overflow {} else { crash("overflow"); }
            when math::div_floor(1, 0) is Error::DivisionByZero {} else { crash("div zero"); }
            when math::mod_floor(1, 0) is Error::DivisionByZero {} else { crash("mod zero"); }

            for value in 0..200 {
                const root = math::isqrt(value)!!;
                require(root * root <= value && (root + 1) * (root + 1) > value, "isqrt");
            }
            io::println(math::isqrt(largest)!!)!!;
            when math::isqrt(-1) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("negative root"); }

            require(math::sign(smallest)!! == -1 && math::sign(7)!! == 1, "int sign");
            require(math::sign(-0.0)!! == 0 && math::sign(-0.5)!! == -1, "float sign");
            require(math::sign("5" as uint)!! == 1, "uint sign");
            require(math::sign("-99999999999999999999" as bigint)!! == -1, "bigint sign");
            const infinity = "inf" as float;
            const nan = "NaN" as float;
            when math::sign(nan) is Error::Unordered {} else { crash("nan sign"); }
            when math::sign("1") is Error::TypeMismatch(op) { io::println(op)!!; }
            else { crash("text sign"); }
            require(math::is_nan(nan)!! && !math::is_nan(infinity)!!, "is_nan");
            require(math::is_finite(1.5)!!, "finite");
            require(!math::is_finite(infinity)!! && !math::is_finite(-infinity)!!, "infinite");
            require(!math::is_finite(nan)!!, "nan is not finite");
        }
    "#);
    assert_eq!(output, "-4\n63\n3037000499\nmath::isqrt\nmath::sign\n");
}

#[test]
fn array_sums_repeats_and_keyed_selection_handle_edges() {
    let output = run(r#"
        use array;
        struct Item { name: str, rank: int }
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn rank(item: Item) -> int { item.rank }
        fn inverse(value: int) -> int { 10 / value }
        fn main() {
            const largest = "9223372036854775807" as int;
            const no_ints: int[] = [];
            const no_floats: float[] = [];
            io::println(array::sum([1, -2, 40])!!)!!;
            io::println(array::sum(no_ints)!!)!!;
            io::println(array::sum_float([1.5, 2.25])!!)!!;
            io::println(array::sum_float(no_floats)!!)!!;
            when array::sum([largest, 1, -1]) is Error::Overflow { io::println("overflow")!!; }
            else { crash("sum overflow"); }

            io::println(array::repeat("é", 3)!!)!!;
            io::println(array::repeat(0, 0)!!.len())!!;
            when array::repeat(0, -1) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("negative repeat"); }
            var shared = [0];
            const rows = array::repeat(shared, 2)!!;
            shared << 1;
            require(rows[0]!!.len() == 2 && rows[1]!!.len() == 2, "repeat aliases");

            var calls = 0;
            const key = fn[var calls](item: Item) -> int { calls += 1; item.rank };
            var items = [
                Item(name: "b", rank: 2), Item(name: "a", rank: 1),
                Item(name: "c", rank: 2), Item(name: "d", rank: 1),
            ];
            io::println(array::min_by_key(items, key)!!.name)!!;
            io::println(calls)!!;
            io::println(array::max_by_key(items, rank)!!.name)!!;
            var nobody: Item[] = [];
            when array::min_by_key(nobody, rank) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("empty min_by_key"); }
            when array::max_by_key(nobody, rank) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("empty max_by_key"); }
            when array::max_by_key([2, 0, 5], inverse) is Error::DivisionByZero {
                io::println("key error")!!;
            } else { crash("key error lost"); }

            const counts = array::frequencies(["z", "é", "z", "z"])!!;
            io::println(counts.keys())!!;
            io::println(counts.values())!!;
            io::println(array::frequencies(no_ints)!!.len())!!;
        }
    "#);
    assert_eq!(
        output,
        "39\n0\n3.75\n0\noverflow\n[é, é, é]\n0\narray::repeat\na\n4\nb\n\
         array::min_by_key\narray::max_by_key\nkey error\n[z, é]\n[3, 1]\n0\n"
    );
}

#[test]
fn text_utilities_count_scalars_and_preserve_unicode() {
    let output = run(r#"
        use str;
        fn require(ok: bool, label: str) { if !ok { crash(label); } }
        fn main() {
            require(str::byte_len("")!! == 0 && str::byte_len("aé€")!! == 6, "byte_len");
            const data = bytes::from_str("né")!!;
            require(str::from_bytes(data)!! == "né", "from_bytes");
            require(str::from_bytes(bytes::new(0)!!)!! == "", "from empty");
            var invalid = bytes::new(1)!!;
            bytes::set(invalid, 0, 255)!!;
            when str::from_bytes(invalid) is Error {} else { crash("invalid utf-8"); }

            io::println(str::pad_start("7", 3, '0')!!)!!;
            io::println(str::pad_end("é", 3, '·')!! + "|")!!;
            io::println(str::pad_start("ab", 4)!! + "|")!!;
            io::println(str::pad_end("", 2)!! + "|")!!;
            require(str::pad_start("long", 2)!! == "long", "no truncation");
            require(str::pad_end("éé", 2)!! == "éé", "exact width");
            require(str::pad_end("x", 0)!! == "x", "zero width");
            when str::pad_start("x", -1) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("negative start"); }
            when str::pad_end("x", -1) is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("negative end"); }

            require(str::is_blank("")!! && str::is_blank(" \t\r\n")!!, "blank");
            require(!str::is_blank(" é ")!!, "not blank");
            io::println(str::to_ascii_lowercase("Hello ÉCOLE 42")!!)!!;
            io::println(str::to_ascii_uppercase("straße")!!)!!;
            require(str::to_ascii_lowercase("")!! == "", "empty case");
            const original = "MiXed";
            require(str::to_ascii_uppercase(original)!! == "MIXED", "upper");
            require(original == "MiXed", "input unchanged");

            require(str::to_int_or("-42", 7)!! == -42, "parsed");
            require(str::to_int_or("", 7)!! == 7, "empty");
            require(str::to_int_or(" 1", 7)!! == 7, "spaces");
            require(str::to_int_or("9223372036854775808", 7)!! == 7, "out of range");
            require(str::to_int_or("٣", 7)!! == 7, "non-ascii digit");
            when str::to_int_or(Error::MissingKey(), 7) is Error::MissingKey {
                io::println("kept")!!;
            } else { crash("error argument replaced"); }
        }
    "#);
    assert_eq!(
        output,
        "007\né··|\n  ab|\n  |\nstr::pad_start\nstr::pad_end\nhello École 42\nSTRAßE\nkept\n"
    );
}
