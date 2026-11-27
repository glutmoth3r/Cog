use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{interpreter, project};
use std::path::{Path, PathBuf};

const ASCENDING_SOURCE: &str = r#"
@!include("std")
use io;

fn sum(n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        total += index;
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(sum(6000))!!;
}
"#;

const VIRTUAL_STACK_ASSEMBLY: &str = r#"
call main 0
call emit_value 1
pop
halt

.function main 0 0
main:
push 0
store 0
loop:
load 0
push 1
add
dup
swap
push 999
pop
store 0
stack_truncate 0
load 0
push 6000
lt
jnz loop
load 0
ret

.function emit_value 1 0
emit_value:
store 0
call_native io::stdout
load 0
call_native bytes::render
store 1
load 1
push 0
load 1
call_native bytes::len
call_native io::write_all
dup
is_type error
jnz emit_done
pop
call_native io::stdout
call_native io::flush
emit_done:
ret
"#;

const CAPTURED_CLOSURE_SOURCE: &str = r#"
@!include("std")
use io;

fn make_adder(bias: int) -> fn(int) -> int {
    const add = fn[bias](count: int) -> int {
        var index = 0;
        var total = 0;
        while index < count {
            total += bias;
            index += 1;
        }
        total
    };
    add
}

fn main() {
    const add_seven = make_adder(7);
    const add_eleven = make_adder(11);
    io::println((add_seven)(6000))!!;
    io::println((add_eleven)(6000))!!;
}
"#;

const REFERENCE_CAPTURE_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;

fn make_sum(values: int[]) -> fn(int) -> int {
    const sum = fn[values](count: int) -> int {
        var index = 0;
        var total = 0;
        while index < count {
            total += values[0];
            index += 1;
        }
        total
    };
    sum
}

fn main() {
    const sum = make_sum([7]);
    io::println((sum)(6000))!!;
}
"#;

const USER_ITERATOR_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;
use iter;

struct Source { limit: int }
struct Cursor { limit: int, value: int }

extend Source with iter::IntoIterator[Cursor, int] {
    fn into_iter(self) -> Cursor { Cursor(self.limit, 0) }
}

extend Cursor with iter::Iterator[int] {
    fn next(mut self) -> bool {
        if self.value >= self.limit { return false; }
        self.value += 1;
        true
    }
    fn current(self) -> int { self.value }
}

fn main() {
    var total = 0;
    for value in Source(6000) { total += value; }
    io::println(total)!!;
}
"#;

const DESCENDING_SOURCE: &str = r#"
@!include("std")
use io;

fn sum(n: int) -> int {
    var total = 0;
    for index in n..0 step -1 {
        total += index;
    }
    return total;
}

fn main() -> nil {
    io::println(sum(6000))!!;
}
"#;

const UINT_SOURCE: &str = r#"
@!include("std")
use io;

fn score(base: uint, n: uint) -> uint {
    const zero: uint = 0;
    const one: uint = 1;
    const three: uint = 3;
    const five: uint = 5;
    const six: uint = 6;
    const threshold: uint = 9223372036854775807;
    var remaining: uint = n;
    var index: uint = zero;
    var total: uint = zero;
    while remaining > zero {
        if base > threshold {
            total += (((index + one) * six) / three) % five;
        }
        index += one;
        remaining -= one;
    }
    return total;
}

fn main() -> nil {
    const base: uint = 9223372036854775808;
    const n: uint = 6000;
    io::println(score(base, n))!!;
}
"#;

const UINT_OVERFLOW_SOURCE: &str = r#"
@!include("std")
use io;

fn trigger(n: uint) -> uint {
    const zero: uint = 0;
    const one: uint = 1;
    const maximum: uint = 18446744073709551615;
    var index: uint = zero;
    var value: uint = zero;
    while index < n {
        if index + one == n {
            value = maximum + index;
        }
        index += one;
    }
    return value;
}

fn main() -> nil {
    const n: uint = 1024;
    io::println(trigger(n))!!;
}
"#;

const SHIFT_SOURCE: &str = r#"
@!include("std")
use io;

fn mix(n: int) -> int {
    var total = 0;
    var index = 0;
    while index < n {
        const amount = index % 64;
        total = (total + ((index % 1024) << (amount % 53)) % 1000003) % 1000003;
        total = (total + ((index * 7919 - 20000) >> amount) % 1000003) % 1000003;
        index += 1;
    }
    return total;
}

fn unsigned_mix(n: uint) -> uint {
    const zero: uint = 0;
    const one: uint = 1;
    const width: uint = 64;
    const modulus: uint = 1000003;
    const maximum: uint = 18446744073709551615;
    var total: uint = zero;
    var index: uint = zero;
    while index < n {
        const amount = index % width;
        total = (total + (maximum >> amount) % modulus) % modulus;
        total = (total + (one << amount) % modulus) % modulus;
        index += one;
    }
    return total;
}

fn main() -> nil {
    const n: uint = 6000;
    io::println(mix(6000))!!;
    io::println(unsigned_mix(n))!!;
}
"#;

const SHIFT_ERROR_SOURCE: &str = r#"
@!include("std")
use io;
use err::Error;

fn classify(n: int) -> int {
    var total = 0;
    var index = 0;
    while index < n {
        const amount = index % 70 - 3;
        const value = index * 7919 - 20000;
        const left = value << amount;
        const right = value >> amount;
        when left is Error::Overflow { total += 1000000; }
        when left is Error::InvalidArgument { total += 1000; }
        when right is Error::InvalidArgument { total += 1; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(classify(6000))!!;
}
"#;

const FLOAT_SOURCE: &str = r#"
@!include("std")
use io;

fn score(probe: float, n: int) -> float {
    const half: float = 0.5;
    const quarter: float = 0.25;
    const two: float = 2.0;
    var total: float = 0.0;
    var index = 0;
    while index < n {
        total = (((total + half) * two) / two) - quarter + quarter;
        if probe != probe {
            total += half;
        }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    const probe: float = "NaN" as float;
    io::println(score(probe, 6000))!!;
}
"#;

const FLOAT_DIVISION_BY_ZERO_SOURCE: &str = r#"
@!include("std")
use io;

fn trigger(divisor: float, n: int) -> float {
    const one: float = 1.0;
    var index = 0;
    var result: float = 0.0;
    while index < n {
        if index + 1 == n {
            result = one / divisor;
        }
        index += 1;
    }
    return result;
}

fn main() -> nil {
    io::println(trigger(0.0, 1024))!!;
}
"#;

const FLOAT_MOD_SOURCE: &str = r#"
@!include("std")
use io;

fn score(divisor: float, n: int) -> float {
    var index = 0;
    var total: float = 0.0;
    while index < n {
        total += 5.5 % divisor;
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(2.0, 6000))!!;
}
"#;

const JIT_DIRTY_LOCAL_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;

fn identity(value: int) -> int {
    return value;
}

fn sum(n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        index += 1;
        total += index;
        total += identity(0);
    }
    return total;
}

fn main() -> nil {
    io::println(sum(6000))!!;
}
"#;

const CHAR_BOOL_COMPARE_SOURCE: &str = r#"
@!include("std")
use io;

fn score(character: char, enabled: bool, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if character > 'a' {
            total += 1;
        }
        if character == '🦀' {
            total += 2;
        }
        if enabled == false {
            total += 10;
        }
        if enabled != false {
            total += 4;
        }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score('🦀', true, 6000))!!;
}
"#;

const MIXED_INT_UINT_COMPARE_SOURCE: &str = r#"
@!include("std")
use io;

fn score(signed: int, unsigned: uint, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if signed < unsigned { total += 1; }
        if signed <= unsigned { total += 2; }
        if signed > unsigned { total += 4; }
        if signed >= unsigned { total += 8; }
        if signed == unsigned { total += 16; }
        if signed != unsigned { total += 32; }
        if unsigned < signed { total += 64; }
        if unsigned <= signed { total += 128; }
        if unsigned > signed { total += 256; }
        if unsigned >= signed { total += 512; }
        if unsigned == signed { total += 1024; }
        if unsigned != signed { total += 2048; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(-1, 9223372036854775808, 6000))!!;
    io::println(score(3, 3, 6000))!!;
}
"#;

const MIXED_INTEGER_FLOAT_COMPARE_SOURCE: &str = r#"
@!include("std")
use io;

fn signed_score(signed: int, probe: float, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if signed < probe { total += 1; }
        if signed <= probe { total += 2; }
        if signed > probe { total += 4; }
        if signed >= probe { total += 8; }
        if signed == probe { total += 16; }
        if signed != probe { total += 32; }
        if probe < signed { total += 64; }
        if probe <= signed { total += 128; }
        if probe > signed { total += 256; }
        if probe >= signed { total += 512; }
        if probe == signed { total += 1024; }
        if probe != signed { total += 2048; }
        index += 1;
    }
    return total;
}

fn unsigned_score(unsigned: uint, probe: float, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if unsigned < probe { total += 4096; }
        if unsigned <= probe { total += 8192; }
        if unsigned > probe { total += 16384; }
        if unsigned >= probe { total += 32768; }
        if unsigned == probe { total += 65536; }
        if unsigned != probe { total += 131072; }
        if probe < unsigned { total += 262144; }
        if probe <= unsigned { total += 524288; }
        if probe > unsigned { total += 1048576; }
        if probe >= unsigned { total += 2097152; }
        if probe == unsigned { total += 4194304; }
        if probe != unsigned { total += 8388608; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    const nan: float = "NaN" as float;
    const infinity: float = "inf" as float;
    io::println(signed_score(9007199254740993, 9007199254740992.0, 6000))!!;
    io::println(signed_score(9007199254740992, 9007199254740992.0, 6000))!!;
    io::println(signed_score(-9223372036854775808, -9223372036854775808.0, 6000))!!;
    io::println(signed_score(9223372036854775807, 9223372036854775808.0, 6000))!!;
    io::println(signed_score(0, nan, 6000))!!;
    io::println(signed_score(0, infinity, 6000))!!;
    io::println(unsigned_score(9007199254740993, 9007199254740992.0, 6000))!!;
    io::println(unsigned_score(9007199254740992, 9007199254740992.0, 6000))!!;
    io::println(unsigned_score(0, -9223372036854775808.0, 6000))!!;
    io::println(unsigned_score(18446744073709551615, 18446744073709551616.0, 6000))!!;
    io::println(unsigned_score(0, nan, 6000))!!;
    io::println(unsigned_score(0, infinity, 6000))!!;
}
"#;

const SMALL_BIGINT_COMPARE_SOURCE: &str = r#"
@!include("std")
use io;

fn signed_score(value: bigint, probe: int, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if value < probe { total += 1; }
        if value <= probe { total += 2; }
        if value > probe { total += 4; }
        if value >= probe { total += 8; }
        if value == probe { total += 16; }
        if value != probe { total += 32; }
        if probe < value { total += 64; }
        if probe <= value { total += 128; }
        if probe > value { total += 256; }
        if probe >= value { total += 512; }
        if probe == value { total += 1024; }
        if probe != value { total += 2048; }
        when value is bigint { total += 4096; }
        index += 1;
    }
    return total;
}

fn unsigned_score(value: bigint, probe: uint, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if value < probe { total += 1; }
        if value <= probe { total += 2; }
        if value > probe { total += 4; }
        if value >= probe { total += 8; }
        if value == probe { total += 16; }
        if value != probe { total += 32; }
        if probe < value { total += 64; }
        if probe <= value { total += 128; }
        if probe > value { total += 256; }
        if probe >= value { total += 512; }
        if probe == value { total += 1024; }
        if probe != value { total += 2048; }
        when value is bigint { total += 4096; }
        index += 1;
    }
    return total;
}

fn float_score(value: bigint, probe: float, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if value < probe { total += 1; }
        if value <= probe { total += 2; }
        if value > probe { total += 4; }
        if value >= probe { total += 8; }
        if value == probe { total += 16; }
        if value != probe { total += 32; }
        if probe < value { total += 64; }
        if probe <= value { total += 128; }
        if probe > value { total += 256; }
        if probe >= value { total += 512; }
        if probe == value { total += 1024; }
        if probe != value { total += 2048; }
        when value is bigint { total += 4096; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(signed_score(9007199254740993, 9007199254740992, 6000))!!;
    io::println(signed_score(-9223372036854775808, -9223372036854775808, 6000))!!;
    io::println(unsigned_score(9223372036854775808, 9223372036854775807, 6000))!!;
    io::println(unsigned_score(18446744073709551615, 18446744073709551615, 6000))!!;
    io::println(float_score(9007199254740993, 9007199254740992.0, 6000))!!;
    io::println(float_score(9223372036854775808, 9223372036854775808.0, 6000))!!;
    io::println(float_score(-9223372036854775808, -9223372036854775808.0, 6000))!!;
}
"#;

const LARGE_BIGINT_COMPARE_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;

fn score(value: bigint, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if value > 9223372036854775807 { total += 1; }
        when value is bigint { total += 1; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(18446744073709551616, 6000))!!;
}
"#;

const ROOTED_AGGREGATE_LOCALS_SOURCE: &str = r#"
@!include("std")
use io;

fn score(label: str, items: int[], magnitude: bigint, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        total += index;
        when label is str { total += 1; }
        when magnitude is bigint { total += 1; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score("kept alive across OSR", [11, 12, 13], 18446744073709551616, 6000))!!;
}
"#;

const JIT_TYPE_AND_BOOLEAN_SOURCE: &str = r#"
@!include("std")
use io;

fn score(value: int?, flag: bool, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        if flag ^ false {
            total += 1;
        }
        if !flag {
            total += 2;
        }
        if value == nil {
            total += 4;
        }
        if nil != value {
            total += 8;
        }
        when value is int {
            total += value;
        }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(2, true, 6000))!!;
    io::println(score(nil, false, 6000))!!;
}
"#;

const JIT_ENUM_TYPE_SOURCE: &str = r#"
@!include("std")
use io;

enum Signal { Hit(value: int), Miss }

fn score(signal: Signal, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        when signal is Signal { total += 1; }
        when signal is Signal::Hit { total += 2; }
        when signal is Signal::Miss { total += 4; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(Signal::Hit(7), 6000))!!;
    io::println(score(Signal::Miss(), 6000))!!;
}
"#;

const JIT_ERROR_VARIANT_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;
use err::Error;

fn main() -> nil {
    var index = 0;
    var total = 0;
    while index < 6000 {
        when (1 / 0) is Error::DivisionByZero { total += 1; }
        index += 1;
    }
    io::println(total)!!;
}
"#;

const JIT_STRUCT_TYPE_FALLBACK_SOURCE: &str = r#"
@!include("std")
use io;

struct Point { x: int }
struct Other { x: int }

fn score(point: Point?, other: Other, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        when point is Point { total += 1; }
        when other is Point { total += 2; }
        when (1 / 0) is Point { total += 4; }
        when index is Point { total += 8; }
        index += 1;
    }
    return total;
}

fn main() -> nil {
    io::println(score(Point(x: 1), Other(x: 2), 6000))!!;
    io::println(score(nil, Other(x: 2), 6000))!!;
}
"#;

const JIT_STRUCT_EQUALITY_SOURCE: &str = r#"
@!include("std")
use io;
use eq;
use err::Error;

struct Point { x: int, scale: int }
struct Node { value: int, next: Node? = nil }

extend Point with eq::Eq[Point] {
    fn equals(self, other: Point) -> bool {
        (self.x / self.scale)!! == (other.x / other.scale)!!
    }
}
extend Node with eq::Eq[Node] {
    fn equals(self, other: Node) -> bool { self.value == other.value }
}

fn matches[T: type + eq::Eq[T]](left: T, right: T) -> bool { left == right }

fn score(origin: Point, leaf: Node, chain: Node, n: int) -> int {
    var index = 0;
    var total = 0;
    while index < n {
        const point = Point(index % 3, 1);
        if point == origin { total += 1; }
        if point != origin { total += 10; }
        if matches(point, point) { total += 100; }
        if chain.next != leaf.next { total += 1000; }
        if leaf.next != leaf.next { total += 10000; }
        index += 1;
    }
    return total;
}

fn failing(origin: Point, n: int) -> bool {
    var index = 0;
    var same = false;
    while index < n {
        same = (Point(0, n - 1 - index) == origin)!!;
        index += 1;
    }
    same
}

fn main() -> nil {
    const leaf = Node(1);
    const chain = Node(2, leaf);
    io::println(score(Point(0, 1), leaf, chain, 6000))!!;
    when failing(Point(0, 1), 6000) is Error::DivisionByZero { io::println("DivisionByZero"); }
}
"#;

fn execute_result(source: &str, jit: bool, profile_path: Option<&Path>) -> Result<String, String> {
    let artifact =
        project::compile_source("jit-smoke.cog", source).unwrap_or_else(|error| panic!("{error}"));
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.configure_jit(jit, profile_path, false);
    interpreter::run_with_context(&artifact, &mut context).map_err(|error| error.to_string())?;
    drop(context);
    Ok(String::from_utf8(output).expect("Cog output should be UTF-8"))
}

fn execute_assembly_result(
    source: &str,
    jit: bool,
    profile_path: Option<&Path>,
) -> Result<String, String> {
    let artifact = cog::parser::assemble(source).map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.configure_jit(jit, profile_path, false);
    interpreter::run_with_context(&artifact, &mut context).map_err(|error| error.to_string())?;
    drop(context);
    String::from_utf8(output).map_err(|error| error.to_string())
}

fn execute(source: &str, jit: bool, profile_path: Option<&Path>) -> String {
    execute_result(source, jit, profile_path)
        .unwrap_or_else(|error| panic!("branch smoke program should complete: {error}"))
}

fn assert_jit_matches_vm(source: &str, suffix: &str, expected: &str) {
    let profile_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "jit-branch-profile-{}-{suffix}.json",
            std::process::id()
        ));
    let _ = std::fs::remove_file(&profile_path);
    let disabled_profile_path = profile_path.with_extension("disabled.json");
    let _ = std::fs::remove_file(&disabled_profile_path);

    let vm_output = execute(source, false, Some(&disabled_profile_path));
    assert!(
        !disabled_profile_path.exists(),
        "disabled JIT must not profile"
    );
    let jit_output = execute(source, true, Some(&profile_path));

    assert_eq!(vm_output, expected);
    assert_eq!(jit_output, vm_output);

    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist a profile after compilation"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"]
            .as_object()
            .expect("profile should include function records")
            .values()
            .any(|function| {
                function["compile_ns"].as_u64().unwrap_or(0) > 0
                    && function["calibrated"].as_bool() == Some(true)
            })
    );

    let _ = std::fs::remove_file(profile_path);
}

fn assert_jit_matches_vm_computed(source: &str, suffix: &str) {
    let expected = execute(source, false, None);
    assert_jit_matches_vm(source, suffix, &expected);
}

fn assert_jit_fallback_matches_vm(source: &str, suffix: &str, expected: &str) {
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "jit-fallback-profile-{}-{suffix}.json",
            std::process::id()
        ));
    let _ = std::fs::remove_file(&profile_path);

    let vm_output = execute(source, false, None);
    let jit_output = execute(source, true, Some(&profile_path));
    assert_eq!(vm_output, expected);
    assert_eq!(jit_output, vm_output);

    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist its compiled fallback plan"),
    )
    .expect("APE profile should be valid JSON");
    assert!(profile["functions"].as_object().is_some_and(|functions| {
        functions
            .values()
            .any(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
    }));
    let _ = std::fs::remove_file(profile_path);
}

fn assert_jit_fallback_matches_vm_computed(source: &str, suffix: &str) {
    let expected = execute(source, false, None);
    assert_jit_fallback_matches_vm(source, suffix, &expected);
}

#[test]
fn jit_conditional_jumps_match_vm_for_true_and_false_paths() {
    assert_jit_matches_vm(ASCENDING_SOURCE, "ascending", "17997000\n");
    assert_jit_matches_vm(DESCENDING_SOURCE, "descending", "18003000\n");
}

#[test]
fn jit_method_defaults_preserve_evaluation_order() {
    assert_jit_fallback_matches_vm(
        r#"
        @!include("std") use io;
        var trace = 0;
        fn mark(value: int) -> int { trace += value; value }
        struct Counter { value: int }
        extend Counter {
            fn add(self, amount: int = mark(1)) -> int { self.value + amount }
        }
        fn main() {
            const counter = Counter(1);
            var index = 0;
            var sum = 0;
            while index < 6000 {
                sum += counter.add();
                index += 1;
            }
            io::println(sum);
            io::println(trace);
        }
        "#,
        "method-defaults",
        "12000\n6000\n",
    );
}

#[test]
fn jit_virtual_operand_stack_matches_vm_for_stack_manipulation_and_truncation() {
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-virtual-stack-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);

    let vm_output = execute_assembly_result(VIRTUAL_STACK_ASSEMBLY, false, None)
        .expect("interpreter should execute the stack-manipulation fixture");
    let jit_output = execute_assembly_result(VIRTUAL_STACK_ASSEMBLY, true, Some(&profile_path))
        .expect("JIT should execute the stack-manipulation fixture");

    assert_eq!(vm_output, "6000");
    assert_eq!(jit_output, vm_output);
    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist its compiled profile"),
    )
    .expect("APE profile should be valid JSON");
    assert!(profile["functions"].as_object().is_some_and(|functions| {
        functions
            .values()
            .any(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
    }));

    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_compiles_hot_direct_calls_to_functions_without_loops() {
    let source = r#"
call main 0
call emit_value 1
pop
halt

.function main 0 0
main:
push 0
store 0
push 0
store 1
loop:
load 0
call add_one 1
load 1
add
store 1
load 0
push 1
add
store 0
load 0
push 6000
lt
jnz loop
load 1
ret

.function add_one 1 0
add_one:
store 0
load 0
push 1
add
ret

.function emit_value 1 0
emit_value:
store 0
call_native io::stdout
load 0
call_native bytes::render
store 1
load 1
push 0
load 1
call_native bytes::len
call_native io::write_all
dup
is_type error
jnz emit_done
pop
call_native io::stdout
call_native io::flush
emit_done:
ret
"#;
    let artifact = cog::parser::assemble(source).expect("fixture assembly should be valid");
    let main_entry = *artifact
        .callables
        .keys()
        .next()
        .expect("assembly should declare main");
    let main_end = artifact
        .callables
        .range((
            std::ops::Bound::Excluded(main_entry),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .map_or(artifact.instructions.len(), |(entry, _)| *entry);
    let helper_entry = artifact.instructions[main_entry..main_end]
        .iter()
        .find_map(|instruction| match instruction {
            cog::instruction::Instruction::Call(target, 1) => Some(*target),
            _ => None,
        })
        .expect("main should call the loop-free helper");
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-hot-call-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);

    let vm_output = execute_assembly_result(source, false, None)
        .expect("interpreter should execute the hot direct-call fixture");
    let jit_output = execute_assembly_result(source, true, Some(&profile_path))
        .expect("JIT should execute the hot direct-call fixture");

    assert_eq!(vm_output, "18003000");
    assert_eq!(jit_output, vm_output);
    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist compiled call profiles"),
    )
    .expect("APE profile should be valid JSON");
    let helper_profile = &profile["functions"][helper_entry.to_string()];
    assert!(
        helper_profile["compile_ns"].as_u64().unwrap_or(0) > 0,
        "expected loop-free helper at {helper_entry} to compile, got {profile}"
    );
    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_typed_parameter_guard_resumes_vm_for_error_values() {
    let source = r#"
call main 0
call emit_value 1
pop
halt

.function main 0 0
main:
push 0
store 0
loop:
load 0
call increment 1
pop
load 0
push 1
add
dup
store 0
push 160
lt
jnz loop
push 1
push 0
div
call increment 1
ret

.function increment 1 0
increment:
store 0
load 0
push 1
add
ret

.function emit_value 1 0
emit_value:
store 0
call_native io::stdout
load 0
call_native bytes::render
store 1
load 1
push 0
load 1
call_native bytes::len
call_native io::write_all
dup
is_type error
jnz emit_done
pop
call_native io::stdout
call_native io::flush
emit_done:
ret
"#;
    let mut artifact = cog::parser::assemble(source).expect("typed guard fixture should assemble");
    let main_entry = *artifact
        .callables
        .keys()
        .next()
        .expect("fixture should declare main");
    let main_end = artifact
        .callables
        .range((
            std::ops::Bound::Excluded(main_entry),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .map_or(artifact.instructions.len(), |(entry, _)| *entry);
    let increment_entry = artifact.instructions[main_entry..main_end]
        .iter()
        .find_map(|instruction| match instruction {
            cog::instruction::Instruction::Call(target, 1) => Some(*target),
            _ => None,
        })
        .expect("main should call increment");
    artifact
        .callables
        .get_mut(&increment_entry)
        .expect("increment should have a call contract")
        .parameter_types = Some(vec![Some(cog::value::RuntimeType::Int)]);
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-typed-guard-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, jit.then_some(profile_path.as_path()), false);
        interpreter::run_with_context(&artifact, &mut context)
            .map_err(|error| error.to_string())?;
        drop(context);
        String::from_utf8(output).map_err(|error| error.to_string())
    };
    let vm_output = run(false).expect("interpreter fixture should complete");
    let jit_output = run(true).expect("typed JIT guard should resume in the interpreter");
    assert_eq!(jit_output, vm_output);

    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("hot increment should compile"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"][increment_entry.to_string()]["compile_ns"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_osr_reads_distinct_scalar_captures_from_closure_instances() {
    assert_jit_matches_vm(
        CAPTURED_CLOSURE_SOURCE,
        "captured-closure",
        "42000\n66000\n",
    );
}

#[test]
fn jit_reference_capture_fallback_matches_vm() {
    assert_jit_fallback_matches_vm(
        REFERENCE_CAPTURE_FALLBACK_SOURCE,
        "reference-capture",
        "42000\n",
    );
}

#[test]
fn jit_higher_order_array_callback_fallback_matches_vm() {
    assert_jit_fallback_matches_vm(
        r#"
        @!include("std")
        use io;
        use array;

        fn main() {
            var values: int[] = [];
            var index = 0;
            while index < 6000 {
                values.append(index);
                index += 1;
            }
            const bias = 7;
            const add_bias = fn[bias](value: int) -> int { value + bias };
            const mapped = array::map(values, add_bias);
            io::println(mapped.len())!!;
            io::println(mapped[5999]!!)!!;
        }
        "#,
        "array-callback",
        "6000\n6006\n",
    );
}

#[test]
fn jit_bytes_annotated_parameters_and_results_match_vm() {
    assert_jit_fallback_matches_vm(
        r#"
        @!include("std")
        use io;
        use bytes;

        struct Buffer { data: bytes }

        fn bump(target: bytes, index: int) -> bytes {
            bytes::set(target, index, (bytes::get(target, index)!! + 1) % 256)!!;
            target
        }

        fn main() {
            var buffer = Buffer(bytes::new(4));
            var index = 0;
            while index < 6000 {
                buffer.data = bump(buffer.data, index % 4);
                index += 1;
            }
            io::println(bytes::get(buffer.data, 0)!!)!!;
            io::println(bytes::get(buffer.data, 3)!!)!!;
        }
        "#,
        "bytes-annotations",
        "220\n220\n",
    );
}

#[test]
fn jit_bytes_type_tests_fall_back_to_vm_matching() {
    assert_jit_fallback_matches_vm(
        r#"
        @!include("std")
        use io;
        use bytes;

        fn count(value, rounds: int) -> int {
            var matched = 0;
            var index = 0;
            while index < rounds {
                when value is bytes { matched += bytes::len(value)!!; }
                index += 1;
            }
            matched
        }

        fn main() {
            io::println(count(bytes::new(1), 6000))!!;
            io::println(count(io::stdout(), 6000))!!;
        }
        "#,
        "bytes-types",
        "6000\n0\n",
    );
}

#[test]
fn jit_user_iterator_fallback_matches_vm() {
    assert_jit_fallback_matches_vm(USER_ITERATOR_FALLBACK_SOURCE, "user-iterator", "18003000\n");
}

#[test]
fn jit_uint_arithmetic_and_unsigned_comparisons_match_vm() {
    assert_jit_matches_vm(UINT_SOURCE, "uint", "12000\n");
}

#[test]
fn jit_uint_overflow_fault_resumes_in_vm_with_the_same_error() {
    let profile_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "jit-branch-profile-{}-uint-overflow.json",
            std::process::id()
        ));
    let _ = std::fs::remove_file(&profile_path);

    let vm_result = execute_result(UINT_OVERFLOW_SOURCE, false, None);
    let jit_result = execute_result(UINT_OVERFLOW_SOURCE, true, Some(&profile_path));
    assert_eq!(vm_result, Ok("Error: integer arithmetic overflow\n".into()));
    assert_eq!(jit_result, vm_result);

    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist its successful compilation"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"]
            .as_object()
            .expect("profile should include function records")
            .values()
            .any(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
    );

    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_integer_shifts_match_vm() {
    assert_jit_matches_vm_computed(SHIFT_SOURCE, "shift");
}

#[test]
fn jit_shift_errors_resume_in_vm_with_the_same_results() {
    assert_jit_fallback_matches_vm_computed(SHIFT_ERROR_SOURCE, "shift-error");
}

#[test]
fn jit_float_arithmetic_and_nan_comparison_match_vm() {
    assert_jit_matches_vm(FLOAT_SOURCE, "float", "6000\n");
}

#[test]
fn jit_float_division_by_zero_fault_resumes_in_vm() {
    let profile_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "jit-float-divzero-profile-{}.json",
            std::process::id()
        ));
    let _ = std::fs::remove_file(&profile_path);
    let vm_result = execute_result(FLOAT_DIVISION_BY_ZERO_SOURCE, false, None);
    let jit_result = execute_result(FLOAT_DIVISION_BY_ZERO_SOURCE, true, Some(&profile_path));
    assert_eq!(vm_result, Ok("Error: division by zero\n".into()));
    assert_eq!(jit_result, vm_result);
    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist its successful compilation"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"]
            .as_object()
            .expect("profile should include function records")
            .values()
            .any(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
    );
    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_float_remainder_resumes_in_vm() {
    let profile_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-float-mod-profile-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);
    assert_eq!(execute(FLOAT_MOD_SOURCE, false, None), "9000\n");
    assert_eq!(
        execute(FLOAT_MOD_SOURCE, true, Some(&profile_path)),
        "9000\n"
    );
    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("JIT should persist its successful compilation"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"]
            .as_object()
            .expect("profile should include function records")
            .values()
            .any(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
    );
    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_spills_ssa_locals_before_falling_back_to_a_call() {
    assert_jit_fallback_matches_vm(
        JIT_DIRTY_LOCAL_FALLBACK_SOURCE,
        "dirty-locals",
        "18003000\n",
    );
}

#[test]
fn jit_char_ordering_and_bool_equality_match_vm() {
    assert_jit_matches_vm(CHAR_BOOL_COMPARE_SOURCE, "char-bool", "42000\n");
}

#[test]
fn jit_mixed_signed_unsigned_comparisons_match_vm_at_boundaries() {
    assert_jit_matches_vm(
        MIXED_INT_UINT_COMPARE_SOURCE,
        "mixed-int-uint",
        "17106000\n10140000\n",
    );
}

#[test]
fn jit_mixed_integer_float_comparisons_match_vm_at_exact_boundaries() {
    let artifact = project::compile_source(
        "jit-mixed-int-float.cog",
        MIXED_INTEGER_FLOAT_COMPARE_SOURCE,
    )
    .expect("mixed comparison fixture should compile");
    let expected_types = [
        Some(cog::value::RuntimeType::Int),
        Some(cog::value::RuntimeType::Float),
        Some(cog::value::RuntimeType::Int),
    ];
    let signed_score_entry = artifact
        .callables
        .iter()
        .find_map(|(&entry, signature)| {
            signature
                .parameter_types
                .as_ref()
                .is_some_and(|types| types.as_slice() == expected_types.as_slice())
                .then_some(entry)
        })
        .expect("signed_score should have int/float/int parameter types");
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-mixed-int-float-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);

    let vm_output = execute(MIXED_INTEGER_FLOAT_COMPARE_SOURCE, false, None);
    let jit_output = execute(
        MIXED_INTEGER_FLOAT_COMPARE_SOURCE,
        true,
        Some(&profile_path),
    );
    assert_eq!(jit_output, vm_output);

    let profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile_path).expect("signed_score should persist its JIT profile"),
    )
    .expect("APE profile should be valid JSON");
    assert!(
        profile["functions"][signed_score_entry.to_string()]["jit_ns_per_backedge"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "signed_score must execute in native code for this regression"
    );
    let _ = std::fs::remove_file(profile_path);
}

#[test]
fn jit_small_bigint_comparisons_match_vm_at_signed_and_float_boundaries() {
    assert_jit_matches_vm_computed(SMALL_BIGINT_COMPARE_SOURCE, "small-bigint");
}

#[test]
fn jit_large_bigint_comparisons_resume_in_the_vm() {
    let expected = execute(LARGE_BIGINT_COMPARE_FALLBACK_SOURCE, false, None);
    assert_jit_fallback_matches_vm(
        LARGE_BIGINT_COMPARE_FALLBACK_SOURCE,
        "large-bigint",
        &expected,
    );
}

#[test]
fn jit_roots_and_restores_string_and_aggregate_locals_across_osr() {
    assert_jit_matches_vm(
        ROOTED_AGGREGATE_LOCALS_SOURCE,
        "rooted-aggregate",
        "18009000\n",
    );
}

#[test]
fn jit_nullable_type_test_and_boolean_ops_match_vm() {
    assert_jit_matches_vm(JIT_TYPE_AND_BOOLEAN_SOURCE, "type-bool", "66000\n36000\n");
}

#[test]
fn jit_nil_equality_preserves_errors_in_both_operands() {
    let template = r#"
        @!include("std")
        use io;
        fn probe(count: int) {
            const failure: int? = "bad" as int;
            var index = 0;
            var swallowed = 0;
            while index < count {
                when COMPARISON is bool { swallowed += 1; }
                index += 1;
            }
            swallowed
        }
        fn main() { io::println(probe(6000)); }
    "#;
    for (suffix, comparison) in [
        ("eq-left", "failure == nil"),
        ("eq-right", "nil == failure"),
        ("ne-left", "failure != nil"),
        ("ne-right", "nil != failure"),
    ] {
        assert_jit_fallback_matches_vm(&template.replace("COMPARISON", comparison), suffix, "0\n");
    }
}

#[test]
fn jit_enum_and_variant_type_tests_match_vm() {
    assert_jit_matches_vm(JIT_ENUM_TYPE_SOURCE, "enum-types", "18000\n30000\n");
}

#[test]
fn jit_struct_type_tests_fall_back_to_vm_matching() {
    assert_jit_fallback_matches_vm(JIT_STRUCT_TYPE_FALLBACK_SOURCE, "struct-types", "6000\n0\n");
}

#[test]
fn jit_hot_struct_equality_matches_vm() {
    assert_jit_fallback_matches_vm(
        JIT_STRUCT_EQUALITY_SOURCE,
        "struct-equality",
        "6642000\nDivisionByZero\n",
    );
}

#[test]
fn jit_error_variant_type_test_falls_back_to_vm_matching() {
    assert_jit_fallback_matches_vm(JIT_ERROR_VARIANT_FALLBACK_SOURCE, "error-variant", "6000\n");
}

#[test]
fn jit_bridges_recursive_calls_and_lexical_cleanup() {
    assert_jit_matches_vm(
        r#"
        @!include("std") use io; use err::Error;
        var trace = 0;

        @inline("never")
        fn descend(depth: int) -> int {
            trace += 1;
            if depth == 0 { return 7; }
            descend(depth - 1)
        }

        @inline("never")
        fn step(index: int) -> int {
            defer { trace += 10; }
            const value = descend(3);
            if index == 9999 { return (value / (index - 9999))!!; }
            value
        }

        fn work(n: int) -> int {
            var i = 0;
            var total = 0;
            while i < n {
                try {
                    total += step(i);
                } catch(error) {
                    when error is Error::DivisionByZero { total += 100; }
                    else { error!!; }
                }
                i += 1;
            }
            total
        }

        fn main() {
            io::println(work(10000));
            io::println(trace);
        }
        "#,
        "bridge-recursive-cleanup",
        "70093\n140000\n",
    );
}

#[test]
fn jit_bridges_named_targets_and_nested_mutable_captures() {
    assert_jit_matches_vm(
        r#"
        @!include("std") use io;
        fn plain(x: int) -> int { x * 2 }

        fn make() -> fn(int) -> int {
            var total = 0;
            const inner = fn[var total](x: int) -> int { total += 1; x + total };
            const outer = fn[inner](x: int) -> int { (inner)(x) };
            outer
        }

        fn work(n: int, first: fn(int) -> int, second: fn(int) -> int) -> int {
            var callback = first;
            var i = 0;
            var sum = 0;
            while i < n {
                if i == 5000 { callback = second; }
                sum += (callback)(i);
                i += 1;
            }
            sum
        }

        fn main() { io::println(work(10000, make(), plain)); }
        "#,
        "bridge-switch-target",
        "99995000\n",
    );
}

#[test]
fn jit_bridges_native_mutations_errors_and_retained_reference_results() {
    use cog::sdk::{Engine, Limits, RunContext, Signature, Type, Value};
    use std::{cell::Cell, rc::Rc};

    let calls = Rc::new(Cell::new(0));
    let native_calls = Rc::clone(&calls);
    let mut engine = Engine::new();
    engine
        .register(
            "host::step",
            Signature::new(vec![Type::Array, Type::Int], Type::Array)
                .with_mutating_parameters(vec![0]),
            move |_, args| {
                native_calls.set(native_calls.get() + 1);
                let i = args[1].as_int()?;
                args[0].array_set(0, Value::int(i))?;
                if i == 9999 {
                    args[0].array_get(10)
                } else {
                    Ok(args[0].clone())
                }
            },
        )
        .unwrap();
    let program = engine
        .compile(
            "native-bridge.cog",
            r#"
            @!include("std") use io; use host; use err::Error;
            fn work(n: int) -> int {
                var array: int[] = [0];
                const alias = array;
                var i = 0;
                var total = 0;
                while i < n {
                    try {
                        const same = host::step(array, i);
                        total += alias[0];
                    } catch (error) {
                        when error is Error { total += alias[0]; }
                    }
                    i += 1;
                }
                total
            }
            fn main() { io::println(work(10000)); }
            "#,
        )
        .unwrap();
    let program = engine.decode(&program.encode().unwrap()).unwrap();
    let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-native-bridge-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile);
    for jit in [false, true, true] {
        calls.set(0);
        let mut output = Vec::new();
        program
            .run_in(
                &mut RunContext::new(&mut output)
                    .with_jit(jit)
                    .with_jit_profile_path(&profile),
            )
            .unwrap();
        assert_eq!(calls.get(), 10000);
        assert_eq!(output, b"49995000\n");
    }
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile).unwrap()).unwrap();
    assert!(
        data["functions"]
            .as_object()
            .unwrap()
            .values()
            .any(|function| {
                function["compile_ns"].as_u64().unwrap_or(0) > 0 && function["calibrated"] == true
            })
    );
    std::fs::remove_file(&profile).unwrap();

    for jit in [false, true] {
        calls.set(0);
        let mut output = Vec::new();
        let result = program.run_in(
            &mut RunContext::new(&mut output)
                .with_jit(jit)
                .with_jit_profile_path(&profile)
                .with_limits(Limits {
                    max_steps: Some(100),
                    ..Limits::default()
                }),
        );
        assert!(result.is_err());
        assert!(!profile.exists());
        assert!(calls.get() < 100);
    }
}

#[test]
fn jit_roots_bridged_aggregate_results_and_reference_captures_during_gc() {
    assert_jit_matches_vm(
        r#"
        @!include("std") use io;

        @inline("never")
        fn pair(read: fn() -> int, i: int) -> int[] { [(read)(), i] }

        fn work(n: int, read: fn() -> int) -> int {
            var sum = 0;
            var i = 0;
            while i < n {
                const values = pair(read, i);
                sum += values[0] + values[1];
                i += 1;
            }
            sum
        }

        fn main() {
            var state: int[] = [7];
            const read = fn[state]() -> int { state[0] };
            io::println(work(10000, read));
            state[0] = 11;
            io::println(work(10000, read));
        }
        "#,
        "bridge-gc",
        "50065000\n50105000\n",
    );
}

#[test]
fn jit_bridge_does_not_replay_effects_on_terminal_failure() {
    let artifact = project::compile_source(
        "bridge-terminal.cog",
        r#"
        @!include("std") use io;
        @inline("never")
        fn step(i: int) -> int {
            if i == 9999 { io::println(i); crash("terminal"); }
            i
        }
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += step(i); i += 1; }
            sum
        }
        fn main() { io::println(work(10000)); }
        "#,
    )
    .unwrap();
    let artifact = cog::bytecode::decode(&cog::bytecode::encode(&artifact).unwrap()).unwrap();
    let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-terminal-bridge-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile);
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, Some(&profile), false);
        let error = interpreter::run_with_context(&artifact, &mut context).unwrap_err();
        let location = context.last_source_location().cloned();
        let diagnostic = context.last_runtime_diagnostic().cloned().unwrap();
        assert_eq!(diagnostic.kind, "Crashed");
        let origin = &diagnostic.provenance.as_ref().unwrap().origin;
        assert!(origin.function.as_ref().unwrap().ends_with("step"));
        drop(context);
        (error.to_string(), location, output, diagnostic)
    };
    let vm = run(false);
    assert_eq!(vm.2, b"9999\n");
    assert_eq!(run(true), vm);
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile).unwrap()).unwrap();
    assert!(
        data["functions"]
            .as_object()
            .unwrap()
            .values()
            .any(|function| {
                function["compile_ns"].as_u64().unwrap_or(0) > 0 && function["calibrated"] == true
            })
    );
    std::fs::remove_file(&profile).unwrap();
}

#[test]
fn jit_and_vm_report_the_same_inlined_error_frames() {
    let artifact = project::compile_source(
        "jit-inline.cog",
        r#"
        @!include("std") use io;
        fn ratio(a: int, b: int) -> int { a / b }
        @inline("never")
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += ratio(100, n - 1 - i); i += 1; }
            sum
        }
        fn main() -> nil {
            const total = work(10000)!!;
            io::println(total);
        }
        "#,
    )
    .unwrap();
    let artifact = cog::bytecode::decode(&cog::bytecode::encode(&artifact).unwrap()).unwrap();
    assert!(!artifact.inline_frames.is_empty());
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, None, false);
        let error = interpreter::run_with_context(&artifact, &mut context).unwrap_err();
        let diagnostic = context.last_runtime_diagnostic().cloned().unwrap();
        drop(context);
        (error.to_string(), output, diagnostic)
    };
    let vm = run(false);
    let trace = vm.2.provenance.as_ref().unwrap();
    assert_eq!(trace.origin.function.as_deref(), Some("ratio"));
    assert!(trace.origin.inlined);
    assert_eq!(trace.callers[0].function.as_deref(), Some("work"));
    assert_eq!(run(true), vm);
}

/// Runs `source` in the VM and with JIT enabled after a bytecode round trip.
/// Both runs must produce the same output and the same error, including the
/// diagnostic kind and logical trace. Returns the JIT profile for coverage
/// assertions.
fn assert_jit_matches_vm_with_diagnostics(source: &str, suffix: &str) -> serde_json::Value {
    let artifact = project::compile_source(&format!("{suffix}.cog"), source)
        .unwrap_or_else(|error| panic!("{error}"));
    let artifact = cog::bytecode::decode(&cog::bytecode::encode(&artifact).unwrap()).unwrap();
    let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-{suffix}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile);
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, jit.then_some(profile.as_path()), false);
        let result = interpreter::run_with_context(&artifact, &mut context)
            .map(|_| ())
            .map_err(|error| error.to_string());
        let diagnostic = context.last_runtime_diagnostic().cloned();
        let location = context.last_source_location().cloned();
        drop(context);
        (
            result,
            String::from_utf8(output).unwrap(),
            diagnostic,
            location,
        )
    };
    let vm = run(false);
    let jit = run(true);
    assert_eq!(jit, vm);
    let data: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&profile).expect("JIT should persist a profile after compilation"),
    )
    .expect("APE profile should be valid JSON");
    std::fs::remove_file(&profile).unwrap();
    data
}

fn compiled_function_count(profile: &serde_json::Value) -> usize {
    profile["functions"].as_object().map_or(0, |functions| {
        functions
            .values()
            .filter(|function| function["compile_ns"].as_u64().unwrap_or(0) > 0)
            .count()
    })
}

#[test]
fn jit_nested_compiled_calls_match_vm() {
    let profile = assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        @inline("never")
        fn step(x: int) -> int { x * 3 + 1 }
        @inline("never")
        fn stage(x: int) -> int { step(x) + step(x + 1) }
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += stage(i); i += 1; }
            sum
        }
        fn main() { io::println(work(6000)); }
        "#,
        "nested-calls",
    );
    // Debug builds may exhaust the compile budget after the first function;
    // `src/jit.rs` unit tests force the nested compiled-callee path directly.
    assert!(compiled_function_count(&profile) >= 1, "{profile}");
}

#[test]
fn jit_nested_compiled_call_errors_unwind_to_the_vm_with_the_same_trace() {
    let profile = assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        @inline("never")
        fn step(x: int) -> int { 600 / x }
        @inline("never")
        fn stage(x: int) -> int { step(x + 1) + step(x) }
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += stage(n - 2 - i); i += 1; }
            sum
        }
        fn main() -> nil {
            const total = work(6000)!!;
            io::println(total);
        }
        "#,
        "nested-call-error",
    );
    assert!(compiled_function_count(&profile) >= 1, "{profile}");
}

#[test]
fn jit_recursion_beyond_the_native_call_depth_matches_vm() {
    // Depths up to 49 exceed the native nesting cap, so the outermost
    // compiled call is discarded and repeated by the VM.
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        @inline("never")
        fn dive(n: int, divisor: int) -> int {
            if n == 0 { 100 / divisor } else { dive(n - 1, divisor) + 1 }
        }
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += dive(i % 50, n - 1 - i); i += 1; }
            sum
        }
        fn main() -> nil {
            io::println(work(5999))!!;
            io::println(work(6000))!!;
        }
        "#,
        "deep-recursion",
    );
}

#[test]
fn jit_compiled_closure_calls_match_vm_for_captures_cells_and_bound_functions() {
    let profile = assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        fn make_adder(bias: int) -> fn(int) -> int {
            const add = fn[bias](x: int) -> int { x + bias };
            add
        }
        @inline("never")
        fn twice(x: int) -> int { x * 2 }
        fn work(
            n: int,
            a: fn(int) -> int,
            b: fn(int) -> int,
            c: fn(int) -> int,
            d: fn(int) -> int,
            e: fn(int) -> int,
        ) -> int {
            var i = 0;
            var sum = 0;
            while i < n {
                const slot = i % 5;
                var callback = a;
                if slot == 1 { callback = b; }
                else if slot == 2 { callback = c; }
                else if slot == 3 { callback = d; }
                else if slot == 4 { callback = e; }
                sum += (callback)(i);
                i += 1;
            }
            sum
        }
        fn main() -> nil {
            var count = 0;
            const counter = fn[var count](x: int) -> int { count += 1; x + count };
            const offset = 3;
            const scaled = fn[offset](x: int) -> int { twice(x) + offset };
            io::println(work(6000, make_adder(7), make_adder(11), twice, counter, scaled))!!;
            io::println(count)!!;
        }
        "#,
        "closure-calls",
    );
    // Debug builds exhaust the compile budget after the first few functions;
    // `src/jit.rs` unit tests force the compiled-closure path directly.
    assert!(compiled_function_count(&profile) >= 1, "{profile}");
}

#[test]
fn jit_compiled_closure_call_errors_match_vm() {
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        fn work(n: int, callback: fn(int) -> int) -> int {
            var i = 0;
            var sum = 0;
            while i < n { sum += (callback)(i); i += 1; }
            sum
        }
        fn main() -> nil {
            const limit = 5999;
            const ratio = fn[limit](x: int) -> int { 600 / (limit - x) };
            io::println(work(6000, ratio))!!;
        }
        "#,
        "closure-call-error",
    );
}

#[test]
fn jit_array_push_and_prepend_match_vm_through_aliases() {
    let profile = assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        fn fill(values: int[], rows: int[][], labels: str[], n: int) -> int {
            var i = 0;
            while i < n {
                values.append(i);
                values.prepend(i * 2);
                rows.append([i]);
                labels.append("x");
                i += 1;
            }
            values.len() + rows.len() + labels.len()
        }
        fn main() -> nil {
            var values: int[] = [];
            const alias = values;
            var rows: int[][] = [];
            var labels: str[] = [];
            io::println(fill(values, rows, labels, 6000))!!;
            io::println(alias.len())!!;
            io::println(alias[0])!!;
            io::println(alias[11999])!!;
            io::println(rows[5999][0])!!;
        }
        "#,
        "array-push",
    );
    assert!(compiled_function_count(&profile) >= 1, "{profile}");
}

#[test]
fn jit_array_push_type_error_matches_vm() {
    // A scalar of the wrong type takes the native fast path first; `try_push`
    // rejects it without mutation and the VM bridge reports the error.
    let source = r#"
call main 0
halt

.function main 0 0
main:
push -1
array 1
store 1
push 0
store 0
loop:
load 1
load 0
arrpush
pop
load 0
push 1
add
store 0
load 0
push 6000
lt
jnz loop
load 1
push 1.5
arrpush
ret
"#;
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-array-push-error-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile_path);
    let vm = execute_assembly_result(source, false, None);
    let jit = execute_assembly_result(source, true, Some(&profile_path));
    assert!(vm.is_err(), "{vm:?}");
    assert_eq!(jit, vm);
    let profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile_path).expect("loop should compile"))
            .unwrap();
    assert!(compiled_function_count(&profile) >= 1, "{profile}");
    let _ = std::fs::remove_file(profile_path);
}

/// Differential check for assembled programs: output and error text must match
/// after a bytecode round trip. Returns the VM result for further assertions.
fn assert_assembly_jit_matches_vm(source: &str, suffix: &str) -> Result<String, String> {
    let artifact = cog::parser::assemble(source).unwrap_or_else(|error| panic!("{error}"));
    let artifact = cog::bytecode::decode(&cog::bytecode::encode(&artifact).unwrap()).unwrap();
    let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-asm-{suffix}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile);
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, jit.then_some(profile.as_path()), false);
        let result = interpreter::run_with_context(&artifact, &mut context)
            .map(|value| format!("{value:?}"))
            .map_err(|error| error.to_string());
        drop(context);
        result.map(|value| format!("{value} {}", String::from_utf8(output).unwrap()))
    };
    let vm = run(false);
    assert_eq!(run(true), vm, "{suffix}");
    assert!(profile.exists(), "{suffix}: the loop should compile");
    std::fs::remove_file(profile).unwrap();
    vm
}

/// A loop that applies one array operation 6000 times to an initially empty
/// array, then runs `tail` and returns the array.
fn array_loop_assembly(operation: &str, tail: &str) -> String {
    format!(
        "
call main 0
halt

.function main 0 0
main:
array 0
store 1
push 0
store 0
loop:
{operation}
pop
load 0
push 1
add
store 0
load 0
push 6000
lt
jnz loop
{tail}
load 1
ret
"
    )
}

#[test]
fn jit_native_pushes_establish_array_types_and_preserve_error_operands() {
    let append = "load 1\nload 0\narrpush";
    let prepend = "load 0\nload 1\narrprepend";
    for (name, operation) in [("append", append), ("prepend", prepend)] {
        let result = assert_assembly_jit_matches_vm(&array_loop_assembly(operation, ""), name);
        assert!(result.is_ok(), "{result:?}");
        // The first native push fixes the element type; later values of another
        // type are rejected and leave the array unchanged.
        for (value, kind) in [
            ("push 1.5", "float"),
            ("push true", "bool"),
            ("push nil", "nil"),
        ] {
            let bad = if name == "append" {
                format!("load 1\n{value}\narrpush\npop")
            } else {
                format!("{value}\nload 1\narrprepend\npop")
            };
            let _ = assert_assembly_jit_matches_vm(
                &array_loop_assembly(operation, &bad),
                &format!("{name}-bad-{kind}"),
            );
        }
    }
    // An Error operand on either side is returned unchanged, mid-loop.
    for (name, operation) in [
        (
            "error-value",
            "load 1\npush 600\npush 3000\nload 0\nsub\ndiv\narrpush",
        ),
        (
            "error-value-front",
            "push 600\npush 3000\nload 0\nsub\ndiv\nload 1\narrprepend",
        ),
        (
            "error-array",
            "push 600\npush 3000\nload 0\nsub\ndiv\nload 0\narrpush",
        ),
        ("scalar-array", "load 0\nload 0\narrpush"),
        ("self-push", "load 1\nload 1\narrpush"),
        ("self-prepend", "load 1\nload 1\narrprepend"),
    ] {
        let _ = assert_assembly_jit_matches_vm(&array_loop_assembly(operation, ""), name);
    }
}

#[test]
fn jit_array_pushes_stay_correct_past_the_native_root_capacity() {
    // Reference values go through the bridge and root their results; the loop
    // must survive exhausting the native root table without repeating a push.
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        fn work(n: int, x: str, y: str, row: int[], labels: str[], rows: int[][], values: int[]) -> int {
            var i = 0;
            var kept = x;
            while i < n {
                labels.prepend(x);
                labels.append(y);
                rows.prepend(row);
                values.append(i);
                kept = labels[0];
                kept = labels[1];
                kept = labels[0];
                kept = labels[1];
                i += 1;
            }
            io::println(kept);
            labels.len() + rows.len() + rows[0].len() + values.len()
        }
        fn main() -> nil {
            var labels: str[] = [];
            var rows: int[][] = [];
            var values: int[] = [];
            const row = [1, 2];
            io::println(work(24000, "x", "y", row, labels, rows, values))!!;
            io::println(values[23999])!!;
            if false { labels.append(""); rows.append([0]); values.append(0); }
        }
        "#,
        "array-push-roots",
    );
}

#[test]
fn jit_arrays_resized_between_native_reads_stay_visible_through_aliases() {
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        @inline("never")
        fn grow(values: int[], i: int) -> int { values.append(i); values.len() }
        fn work(n: int, values: int[], other: int[]) -> int {
            var i = 0;
            var sum = 0;
            while i < n {
                sum += grow(values, i);
                other.append(values[values.len() - 1]);
                values[i] = values[i] + other[0];
                other.prepend(i);
                sum += values[values.len() - 1] + other[other.len() - 1] + other.len();
                i += 1;
            }
            sum + values[n - 1]
        }
        fn main() -> nil {
            var values: int[] = [];
            const alias = values;
            var other: int[] = [];
            io::println(work(6000, values, other)!!)!!;
            io::println(alias.len())!!;
        }
        "#,
        "array-resize",
    );
}

#[test]
fn jit_compiled_callee_results_keep_identity_and_release_temporary_roots() {
    // More calls than the native root table holds, with rooted arguments,
    // captures, temporaries and results at every nesting level.
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        use str;
        @inline("never")
        fn same(values: int[]) -> int[] { values }
        @inline("never")
        fn pass(values: int[]) -> int[] { same(same(values)) }
        @inline("never")
        fn grow(value: bigint, x: int) -> bigint { value * 3 + 18446744073709551617 + str::to_bigint(x) }
        @inline("never")
        fn twice(value: bigint, x: int) -> bigint { grow(grow(value, x), x) }
        @inline("never")
        fn label(text: str, other: str, x: int) -> str { if x % 2 == 0 { text } else { other } }
        @inline("never")
        fn relabel(text: str, other: str, x: int) -> str { label(label(text, other, x), other, x + 1) }
        fn make(values: int[], seed: bigint) -> fn(int) -> bigint {
            const run = fn[values, seed](x: int) -> bigint {
                if x < 0 { same(values); }
                twice(seed, x)
            };
            run
        }
        fn work(n: int, values: int[], run: fn(int) -> bigint, first: str, second: str) -> bigint {
            var i = 0;
            var total: bigint = str::to_bigint(0);
            var text = first;
            while i < n {
                var shared = pass(values);
                shared[0] = shared[0] + 1;
                total += twice(18446744073709551616, i) + (run)(i);
                text = relabel(text, second, i);
                i += 1;
            }
            io::println(text);
            total
        }
        fn main() -> nil {
            var values: int[] = [0];
            const run = make(values, 36893488147419103232);
            io::println(work(20000, values, run, "even", "odd"))!!;
            io::println(values[0])!!;
        }
        "#,
        "callee-roots",
    );
}

#[test]
fn jit_mutual_recursion_and_late_errors_match_vm_traces() {
    assert_jit_matches_vm_with_diagnostics(
        r#"
        @!include("std") use io;
        @inline("never")
        fn even(n: int, d: int) -> int { if n <= 0 { (600 / d)!! } else { odd(n - 1, d) + 1 } }
        @inline("never")
        fn odd(n: int, d: int) -> int { if n <= 0 { (500 / d)!! } else { even(n - 1, d) + 2 } }
        @inline("never")
        fn tree(n: int) -> int { if n <= 1 { n } else { tree(n - 2) + tree(n - 1) } }
        fn inlined(n: int, d: int) -> int { even(n, d) }
        @inline("never")
        fn stage(n: int, d: int) -> int { inlined(n, d) + tree(n % 12) }
        fn work(n: int) -> int {
            var i = 0;
            var sum = 0;
            var errors = 0;
            while i < n {
                try { sum += stage(i % 41, i % 977); }
                catch(_error) { errors += 1; }
                i += 1;
            }
            while i < n * 2 {
                sum += stage(i % 41, n * 2 - 200 - i);
                i += 1;
            }
            sum + errors
        }
        fn main() -> nil { io::println(work(6000))!!; }
        "#,
        "mutual-recursion",
    );
}

#[test]
fn jit_compiled_closure_calls_reject_bad_arguments_like_the_vm() {
    // After a compiled loop of valid calls, the same closure receives a float
    // and an Error argument, and non-callable targets are invoked.
    let program = |tail: &str| {
        format!(
            "
call main 0
halt

.function main 0 0
main:
push 5
closure @add 1
store 1
push 0
store 0
loop:
load 0
load 1
callv 1
pop
load 0
push 1
add
store 0
load 0
push 6000
lt
jnz loop
{tail}
ret

.function add 1 1
add:
store 0
load 0
loadcap 0
add
ret
"
        )
    };
    for (name, tail) in [
        ("ok", "load 0\nload 1\ncallv 1"),
        ("float", "push 1.5\nload 1\ncallv 1"),
        ("error-argument", "push 1\npush 0\ndiv\nload 1\ncallv 1"),
        ("error-target", "load 0\npush 1\npush 0\ndiv\ncallv 1"),
        ("not-callable", "load 0\nload 0\ncallv 1"),
    ] {
        let _ = assert_assembly_jit_matches_vm(&program(tail), &format!("closure-args-{name}"));
    }
}

#[test]
fn resource_limits_keep_nested_and_closure_calls_in_the_vm() {
    let source = r#"
        @!include("std") use io;
        @inline("never")
        fn dive(n: int) -> int { if n <= 0 { 0 } else { dive(n - 1) + 1 } }
        fn work(n: int, run: fn(int) -> int, values: int[]) -> int {
            var i = 0;
            var sum = 0;
            while i < n { values.append(i); sum += dive(i % 20) + (run)(i); i += 1; }
            sum + dive(200)
        }
        fn main() -> nil {
            var values: int[] = [];
            io::println(work(3000, dive, values))!!;
            if false { values.append(0); }
        }
    "#;
    let artifact = project::compile_source("jit-limits.cog", source).unwrap();
    let profile = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("jit-call-limits-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&profile);
    let run = |jit| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.limits.max_call_frames = Some(100);
        context.configure_jit(jit, jit.then_some(profile.as_path()), false);
        let result = interpreter::run_with_context(&artifact, &mut context)
            .map(|_| ())
            .map_err(|error| error.to_string());
        drop(context);
        (result, output)
    };
    let vm = run(false);
    assert!(vm.0.is_err(), "{vm:?}");
    assert_eq!(run(true), vm);
    assert!(!profile.exists(), "limited runs must not enable the JIT");
}
