use cog::{interpreter, project, wasm};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cog-wasi-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_wasi(artifact: &Path, args: &[&str]) -> Output {
    Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/wasm/run.mjs"
        ))
        .arg(artifact)
        .args(args)
        .output()
        .expect("Node is required to run the WASI end-to-end tests")
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_executes_loops_calls_recursion_and_unicode_stdout() {
    let source = r#"@!include("std")
use io;
@inline("never")
fn advance(i: int) -> int {
    if i == 0 { return 0; }
    advance(i - 1) + i
}
fn main() -> nil {
    var i = 0;
    var sum = 0;
    while i < 8 {
        sum += advance(i);
        i += 1;
    }
    io::println("Καλημέρα")!!;
    io::println(sum)!!;
}
"#;
    let (program, _) = project::compile_source_with_warnings("scalar.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("scalar.wasm");
    wasm::build_executable(&program, &artifact).unwrap();
    let mut vm = Vec::new();
    let mut context = cog::runtime::RuntimeContext::new(&mut vm, Default::default());
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    assert_eq!(vm, "Καλημέρα\n84\n".as_bytes());
    for _ in 0..2 {
        let result = run_wasi(&artifact, &[]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, vm);
    }
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_receives_arguments_and_reports_the_original_overflow() {
    let source = r#"@!include("std")
use io;
@inline("never")
fn add(a: int, b: int) -> int { a + b }
fn main(args: str[]) -> nil {
    io::println(args[0])!!;
    io::println(add(9223372036854775807, args.len())!!)!!;
}
"#;
    let (program, _) = project::compile_source_with_warnings("overflow.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("overflow.wasm");
    wasm::build_executable(&program, &artifact).unwrap();
    let mut vm = Vec::new();
    let mut context = cog::runtime::RuntimeContext::new(&mut vm, Default::default());
    context.set_process_arguments(vec!["水".into()]);
    let vm_error = interpreter::run_with_context(&program, &mut context).unwrap_err();
    assert_eq!(vm_error.kind(), "Overflow");
    drop(context);

    let result = run_wasi(&artifact, &["水"]);
    let diagnostics = String::from_utf8(result.stderr).unwrap();
    assert_eq!(result.status.code(), Some(1), "{diagnostics}");
    assert_eq!(vm, "水\n".as_bytes());
    assert_eq!(result.stdout, vm);
    assert!(diagnostics.contains("Error::Overflow"), "{diagnostics}");
    assert!(diagnostics.contains("overflow.cog:4:"), "{diagnostics}");
    assert!(diagnostics.contains("in add"), "{diagnostics}");
}

#[test]
fn wasi_rejects_unsupported_operations_before_writing_output() {
    let source = r#"@!include("std")
use io;
fn main() -> nil { io::read_line()!!; }
"#;
    let (program, _) = project::compile_source_with_warnings("unsupported.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("unsupported.wasm");
    let error = wasm::build_executable(&program, &artifact).unwrap_err();
    assert!(error.contains("io::read_line"), "{error}");
    assert!(!artifact.exists());
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_uses_verified_error_observation_control_flow() {
    let source = r#"@!include("std")
use io;
use err::Error;
@inline("never")
fn divide(x: int, y: int) -> int { x / y }
@propagate
fn operation() -> int {
    const failed = divide(1, 0);
    io::println("unreachable");
    failed
}
@propagate
fn main() -> nil {
    const value = capture { divide(1, 0) };
    when value is Error::DivisionByZero { io::println("saved"); };
    const result = try { operation() } catch(e) { 17 };
    io::println(result);
}
"#;
    let (program, _) = project::compile_source_with_warnings("observation.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("observation.wasm");
    wasm::build_executable(&program, &artifact).unwrap();
    let mut vm = Vec::new();
    interpreter::run_with(&program, &mut vm).unwrap();
    assert_eq!(vm, b"saved\n17\n");
    let result = run_wasi(&artifact, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, vm);
}

/// Build `source`, run it on the VM and as a WASI command with the same
/// arguments, and require identical stdout and failure kind. Returns stdout.
fn assert_vm_parity(name: &str, source: &str, args: &[&str]) -> String {
    let (program, _) =
        project::compile_source_with_warnings(&format!("{name}.cog"), source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join(format!("{name}.wasm"));
    wasm::build_executable(&program, &artifact).unwrap();
    let mut vm = Vec::new();
    let mut context = cog::runtime::RuntimeContext::new(&mut vm, Default::default());
    context.set_process_arguments(args.iter().map(Into::into).collect());
    let outcome = interpreter::run_with_context(&program, &mut context);
    drop(context);
    let result = run_wasi(&artifact, args);
    let diagnostics = String::from_utf8_lossy(&result.stderr).into_owned();
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&vm),
        "{diagnostics}"
    );
    match outcome {
        Ok(_) => assert!(result.status.success(), "{diagnostics}"),
        Err(error) => {
            assert_eq!(result.status.code(), Some(1), "{diagnostics}");
            assert!(
                diagnostics.contains(&format!("Error::{}", error.kind())),
                "{diagnostics}"
            );
        }
    }
    String::from_utf8(vm).unwrap()
}

const PARSE_PROGRAM: &str = r#"@!include("std")
use fmt;
use io;
use str;
use err::Error;
fn describe(value) -> str {
    when value is Error::ParseFailure(from, to) { return "parse failure " + from + " -> " + to; };
    when value is Error::Overflow { return "overflow"; };
    when value is Error { return "other error"; };
    var text = str::builder()!!;
    fmt::write(text, "{}", value)!!;
    str::build(text)!!
}
@propagate
fn main(args: str[]) -> nil {
    var i = 0;
    while i < args.len() {
        const text = args[i];
        const signed = capture { str::to_int(text) };
        const unsigned = capture { str::to_uint(text) };
        const boolean = capture { str::to_bool(text) };
        const character = capture { str::to_char(text) };
        io::println(describe(signed) + " | " + describe(unsigned) + " | "
            + describe(boolean) + " | " + describe(character))!!;
        i += 1;
    }
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_parses_text_like_the_vm() {
    let inputs = [
        "5",
        "+5",
        "-0",
        "-5",
        " 5",
        "5 ",
        "",
        "+",
        "-",
        "007",
        "1_0",
        "0x10",
        "12a",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "-9223372036854775809",
        "18446744073709551615",
        "18446744073709551616",
        "\u{663}",
        "\u{ff15}",
        "true",
        "false",
        "True",
        "x",
        "\u{e9}",
        "\u{1f600}",
        "e\u{301}",
    ];
    let output = assert_vm_parity("parse", PARSE_PROGRAM, &inputs);
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), inputs.len());
    let failure = |to: &str| format!("parse failure str -> {to}");
    // Whitespace, an empty string, a bare sign and Unicode digits are rejected.
    for (index, input) in inputs.iter().enumerate() {
        if [
            "", "+", "-", " 5", "5 ", "\u{663}", "\u{ff15}", "12a", "1_0",
        ]
        .contains(input)
        {
            let expected = format!("{} |", failure("int"));
            assert!(
                lines[index].starts_with(&expected),
                "{input:?}: {}",
                lines[index]
            );
        }
    }
    assert!(lines[1].starts_with("5 | 5 |"), "{}", lines[1]);
    assert!(lines[2].starts_with("0 | "), "{}", lines[2]);
    assert!(
        lines[14].starts_with(&format!("{} |", failure("int"))),
        "{}",
        lines[14]
    );
    assert!(
        lines[15].starts_with("-9223372036854775808 |"),
        "{}",
        lines[15]
    );
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_converts_an_argument_with_the_original_parse_failure() {
    let source = r#"@!include("std")
use io;
fn main(args: str[]) -> nil {
    io::println((args[0] as int)!! + 1)!!;
}
"#;
    assert_eq!(assert_vm_parity("cast", source, &["41"]), "42\n");
    assert_eq!(assert_vm_parity("cast", source, &["4 1"]), "");
}

const BUILDER_PROGRAM: &str = r#"@!include("std")
use fmt;
use io;
use str;
fn main(args: str[]) -> nil {
    var builder = str::builder()!!;
    fmt::write(builder, "[{}]", args[0])!!;
    fmt::write(builder, "{}-{}", 42)!!;
    fmt::write(builder, "<{}>", 'é')!!;
    fmt::write(builder, "{}", true)!!;
    fmt::write(builder, "{}", nil)!!;
    fmt::write(builder, "no placeholder", 7)!!;
    fmt::write(builder, "{}", args)!!;
    const first = str::build(builder)!!;
    fmt::write(builder, "|{}", builder)!!;
    const second = str::build(builder)!!;
    io::println(first)!!;
    io::println(second)!!;
    io::println(first.len())!!;
    io::println(str::build(str::builder()!!)!! == "")!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_string_builders_append_and_snapshot_like_the_vm() {
    let text = "Καλημέρα 水 \u{1f600}";
    let output = assert_vm_parity("builder", BUILDER_PROGRAM, &[text, "второй"]);
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(
        lines[0],
        format!("[{text}]42-{{}}<é>truenilno placeholder[{text}, второй]")
    );
    assert!(lines[1].starts_with(lines[0]) && lines[1].len() > lines[0].len());
    assert_eq!(lines[3], "true");
}

const SHIFT_PROGRAM: &str = r#"@!include("std")
use io;
use str;
use err::Error;
fn show(value) -> nil {
    when value is Error { io::println(value.error_kind())!!; } else { io::println(value)!!; }
}
@propagate
fn main(args: str[]) -> nil {
    const value = str::to_int(args[0])!!;
    const amount = str::to_int(args[1])!!;
    const unsigned = str::to_uint(args[2])!!;
    const unsigned_amount = str::to_uint(args[3])!!;
    show(value << amount);
    show(value >> amount);
    show(value << unsigned_amount);
    show(unsigned << amount);
    show(unsigned >> unsigned_amount);
    var compound = value;
    compound <<= amount;
    show(compound);
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_integer_shifts_match_the_vm() {
    for args in [
        ["1", "62", "1", "63"],
        ["1", "63", "3", "63"],
        ["-5", "1", "18446744073709551615", "1"],
        ["-1", "63", "9223372036854775808", "64"],
        ["7", "64", "7", "0"],
        ["7", "-1", "7", "18446744073709551615"],
    ] {
        assert_vm_parity("shift", SHIFT_PROGRAM, &args);
    }
    let output = assert_vm_parity("shift", SHIFT_PROGRAM, &["-5", "1", "8", "2"]);
    assert_eq!(output, "-10\n-3\n-20\n16\n2\n-10\n");
}

const REPLACE_PROGRAM: &str = r#"@!include("std")
use io;
use str;
use err::Error;
@propagate
fn main(args: str[]) -> nil {
    var i = 0;
    while i + 2 < args.len() {
        const result = capture { str::replace(args[i], args[i + 1], args[i + 2]) };
        when result is Error::InvalidArgument(op) {
            io::println("invalid argument " + op)!!;
        } else {
            io::println("<" + result!! + ">")!!;
        }
        i += 3;
    }
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_replace_matches_the_vm_for_overlap_empty_absent_and_unicode_cases() {
    let cases: [[&str; 3]; 14] = [
        ["banana", "ana", "X"],
        ["aaa", "aa", "b"],
        ["aaaa", "aa", "b"],
        ["abc", "", "x"],
        ["", "", "x"],
        ["abc", "z", "y"],
        ["", "a", "b"],
        ["abc", "abcd", "x"],
        ["a-b-c", "-", "--"],
        ["aaaa", "a", ""],
        ["aa", "a", "aa"],
        ["日本語日本", "日本", "é"],
        ["\u{1f600}x\u{1f600}", "\u{1f600}", "e\u{301}"],
        ["e\u{301}e\u{301}", "e", "E"],
    ];
    let arguments: Vec<&str> = cases.iter().flatten().copied().collect();
    let output = assert_vm_parity("replace", REPLACE_PROGRAM, &arguments);
    assert_eq!(
        output,
        "<bXna>\n<ba>\n<bb>\ninvalid argument str::replace\ninvalid argument str::replace\n<abc>\n<>\n<abc>\n<a--b--c>\n<>\n<aaaa>\n<é語é>\n<e\u{301}xe\u{301}>\n<E\u{301}E\u{301}>\n"
    );
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_html_escape_from_cogweb_runs_identically() {
    let source = r#"@!include("std")
use io;
use str::replace;
@public fn html_escape(value: str) -> str {
    var escaped = replace(value, "&", "&amp;")!!;
    escaped = replace(escaped, "<", "&lt;")!!;
    escaped = replace(escaped, ">", "&gt;")!!;
    escaped = replace(escaped, "\"", "&quot;")!!;
    replace(escaped, "'", "&#39;")!!
}
fn main(args: str[]) -> nil {
    io::println(html_escape(args[0])!!)!!;
}
"#;
    let input = "<a href=\"x\">Tom & 'Jerry' \u{2014} \u{65e5}\u{672c}</a>";
    assert_eq!(
        assert_vm_parity("html_escape", source, &[input]),
        "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39; \u{2014} \u{65e5}\u{672c}&lt;/a&gt;\n"
    );
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_reports_an_unhandled_replace_argument_error_with_its_origin() {
    let source = r#"@!include("std")
use io;
use str;
fn main(args: str[]) -> nil {
    io::println(str::replace(args[0], "", "x")!!)!!;
}
"#;
    let (program, _) = project::compile_source_with_warnings("replace_empty.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("replace_empty.wasm");
    wasm::build_executable(&program, &artifact).unwrap();
    let result = run_wasi(&artifact, &["abc"]);
    let diagnostics = String::from_utf8(result.stderr).unwrap();
    assert_eq!(result.status.code(), Some(1), "{diagnostics}");
    assert!(result.stdout.is_empty());
    assert!(
        diagnostics.contains("Error::InvalidArgument"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("str::replace"), "{diagnostics}");
}

const ERROR_RENDER_PROGRAM: &str = r#"@!include("std")
use fmt;
use io;
use str;
use err::Error;
@propagate
fn main(args: str[]) -> nil {
    var out = str::builder()!!;
    fmt::write(out, "{}", Error::MissingKey())!!;
    fmt::write(out, "|{}", Error::Timeout())!!;
    fmt::write(out, "|{}", Error::MissingField(field: "x"))!!;
    fmt::write(out, "|{}", Error::IndexOutOfBounds(index: 3, len: 2))!!;
    fmt::write(out, "|{}", Error::ParseFailure(from: "a", to: "b"))!!;
    fmt::write(out, "|{}", Error::NativeFailure(message: Error::MissingKey()))!!;
    fmt::write(out, "|{}", capture { str::to_int(args[0]) })!!;
    fmt::write(out, "|{}", capture { str::to_uint(args[0]) })!!;
    io::println(str::build(out)!!)!!;
    const missing = Error::MissingField(field: "y");
    io::println("interpolated {missing}")!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_renders_error_values_like_the_vm() {
    assert_vm_parity("error_render", ERROR_RENDER_PROGRAM, &["abc"]);
}

fn assert_build_rejected(name: &str, source: &str) {
    let (program, _) =
        project::compile_source_with_warnings(&format!("{name}.cog"), source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join(format!("{name}.wasm"));
    let error =
        wasm::build_executable(&program, &artifact).expect_err(&format!("{name} must be rejected"));
    assert!(
        error.contains("WASI backend does not support"),
        "{name}: {error}"
    );
    assert!(!artifact.exists());
}

#[test]
fn wasi_rejects_numeric_conversion_of_floats_and_bigints_at_build_time() {
    for (name, value) in [("float", "1.5"), ("bigint", "100000000000000000000")] {
        for convert in ["to_int", "to_uint"] {
            let source = format!(
                "@!include(\"std\")\nuse io;\nuse str;\nfn main() -> nil {{ io::println(str::{convert}({value})!!)!!; }}\n"
            );
            assert_build_rejected(&format!("{name}_{convert}"), &source);
        }
    }
}

#[test]
fn wasi_rejects_float_sources_for_numeric_conversion_at_build_time() {
    let source = r#"@!include("std")
use io;
use str;
fn main(args: str[]) -> nil { io::println(str::to_int(str::to_float(args[0])!!)!!)!!; }
"#;
    assert_build_rejected("runtime_float", source);
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_keeps_user_types_named_like_core_errors_distinct() {
    let source = r#"@!include("std")
use io;
struct Error { x: int }
fn main() -> nil {
    const value = Error(x: 1);
    when value is Error { io::println("struct")!!; };
    io::println(value)!!;
}
"#;
    assert_eq!(
        assert_vm_parity("user_struct_error", source, &[]),
        "struct\n{x: 1}\n"
    );
}

#[test]
fn wasi_rejects_user_enums_at_build_time() {
    let enum_source = r#"@!include("std")
use io;
enum Mine { MissingKey, Other(x: int) }
fn main() -> nil {
    const value = Mine::Other(x: 1);
    when value is Mine::Other(x) { io::println(x)!!; };
}
"#;
    assert_build_rejected("user_enum_error", enum_source);
}

const ALIAS_PROGRAM: &str = r#"@!include("std")
use io;
struct Point { x: int, y: int = 0 }
struct Bag { items: int[], origin: Point }
struct Pair(int, str);
var shared: int[] = [1];
fn grow(values: int[], value: int) -> nil { values.append(value); }
fn same(values: int[]) -> int[] { values }
fn shift(point: Point) -> nil { point.x += 10; }
fn plus(left, right) { left + right }
fn main() -> nil {
    var values: int[] = [1, 2];
    var alias = values;
    grow(alias, 3);
    alias.prepend(0);
    same(values)[1] = 9;
    values[2] += 40;
    io::println(values)!!;
    io::println(alias)!!;
    var chain: int[] = [];
    chain.append(1).append(2);
    chain.prepend(5).append(6);
    chain.prepend(7);
    io::println(chain)!!;

    var global_alias = shared;
    grow(global_alias, 2);
    shared.append(3);
    io::println(global_alias)!!;
    shared = [8];
    shared.append(9);
    io::println(shared)!!;
    io::println(global_alias)!!;

    var point = Point(1, y: 2);
    var bag = Bag(items: values, origin: point);
    shift(bag.origin);
    bag.items.append(100);
    bag.origin.y = 7;
    io::println(point)!!;
    io::println(values)!!;
    io::println(bag)!!;
    bag.items = [];
    bag.items.append(4);
    bag.origin = Point(0);
    io::println(bag)!!;
    io::println(point)!!;
    io::println(values)!!;

    var nested: int[][] = [values, [1]];
    nested.append(values);
    nested[0][0] = -1;
    nested[1].append(2);
    nested.append([]);
    nested[3].append(5);
    io::println(nested)!!;
    io::println(values)!!;
    io::println(nested.len() + nested[0].len() + len(nested[1]))!!;

    var points: Point[] = [point, Point(3)];
    points.append(points[0]);
    points[2].x = 500;
    io::println(points)!!;

    var pair = Pair(1, "a");
    var other = pair;
    other.0 = 2;
    pair.1 = "b";
    io::println(pair)!!;
    io::println(pair.0 + other.0)!!;
    io::println(pair["0"])!!;

    const copy = values + [1000];
    const front = plus(-7, values);
    values[0] = 11;
    io::println(copy)!!;
    io::println(front)!!;
    io::println(plus(values, 5))!!;
    var empty = values.empty_like();
    empty.append(1);
    io::println(empty)!!;
    io::println(values)!!;

    var untyped = [];
    untyped.append("first");
    untyped.prepend("zeroth");
    io::println(untyped)!!;
    var chars = [['a'], []];
    chars[1].append('é');
    io::println(chars)!!;
    var nilable: int?[] = [1, nil];
    nilable.append(nil);
    nilable[0] = nil;
    io::println(nilable)!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_shares_arrays_and_structs_like_the_vm() {
    let output = assert_vm_parity("alias", ALIAS_PROGRAM, &[]);
    assert!(output.starts_with("[0, 9, 42, 3]\n[0, 9, 42, 3]\n[7, 5, 1, 2, 6]\n"));
    assert!(output.contains("{x: 11, y: 7}\n"), "{output}");
    assert!(output.ends_with("[nil, nil, nil]\n"), "{output}");
}

const CONTRACT_PROGRAM: &str = r#"@!include("std")
use io;
use err::Error;
struct Point { x: int, y: int = 0 }
struct Other { x: int }
struct Holder { values: int[], nested: str[][], point: Point? }
fn describe(value) -> nil {
    when value is Error::TypeMismatch(op) { io::println("mismatch " + op)!!; }
    else {
        when value is Error::IndexOutOfBounds(index, len) {
            io::println($"bounds {index} {len}")!!;
        } else {
            when value is Error::MissingField(field) { io::println("missing " + field)!!; }
            else {
                when value is Error::InvalidArgument(op) { io::println("invalid " + op)!!; }
                else { io::println(value)!!; };
            };
        };
    };
}
fn push(target, value) { target.append(value) }
fn prepend(value, target) { target.prepend(value) }
fn read(target, key) { target[key] }
fn write(target, key, value) {
    try { target[key] = value; "written" } catch(error) { error }
}
fn plus(left, right) { left + right }
fn equal(left, right) { left == right }
fn less(left, right) { left < right }
fn size(value) { len(value) }
fn like(value) { value.empty_like() }
fn field(value) { value.x }
fn ints(values: int[]) -> int { values.len() }
fn matrix(values: int[][]) -> int { values.len() }
fn pointed(point: Point) -> int { point.x }
fn maybe(point: Point?) -> str { "ok" }
fn assign(point, y) -> nil { point.y = y; }
@propagate
fn main() -> nil {
    var numbers: int[] = [1, 2];
    const words: str[] = ["a"];
    var empty: int[] = [];
    describe(capture { push(numbers, "x") });
    describe(capture { prepend('c', numbers) });
    describe(capture { push(5, 1) });
    describe(capture { prepend(1, "no") });
    describe(capture { push(empty, empty) });
    describe(capture { write(numbers, 0, "x") });
    describe(capture { write(numbers, 2, 3) });
    describe(capture { write(numbers, -1, 3) });
    describe(capture { write(numbers, 5, "x") });
    describe(capture { write(numbers, "0", 3) });
    describe(capture { write(5, 0, 3) });
    describe(capture { read(numbers, 2) });
    describe(capture { read(numbers, -9223372036854775807) });
    describe(capture { read(numbers, "x") });
    describe(capture { read("text", 0) });
    describe(capture { read(nil, 0) });
    describe(capture { plus(numbers, words) });
    describe(capture { plus(numbers, "x") });
    describe(capture { plus('x', numbers) });
    describe(capture { plus(numbers, numbers) });
    describe(capture { plus(empty, empty) });
    describe(capture { plus(empty, 1) });
    describe(capture { equal(numbers, numbers) });
    describe(capture { equal(numbers, nil) });
    describe(capture { equal(numbers, 1) });
    describe(capture { less(numbers, numbers) });
    describe(capture { equal(Point(1), Point(1)) });
    describe(capture { equal(Point(1), numbers) });
    describe(capture { size(Point(1)) });
    describe(capture { size(3) });
    describe(capture { like(empty) });
    describe(capture { like(3) });
    describe(capture { like(numbers) });
    describe(capture { ints(empty) });
    describe(capture { matrix([[1]]) });
    describe(capture { pointed(Point(4)) });
    describe(capture { maybe(nil) });
    var assigned = Point(1);
    describe(capture { assign(assigned, "x") });
    describe(capture { assign(assigned, 2) });
    describe(assigned);
    var holder = Holder(values: [], nested: [[]], point: nil);
    const replacement = [["z"]];
    holder.values.append(1);
    holder.nested[0].append("n");
    describe(holder);
    describe(capture { push(holder.nested[0], 1) });
    describe(capture { write(holder, "values", words) });
    describe(capture { write(holder, "values", empty) });
    describe(capture { write(holder, "values", 1) });
    describe(capture { write(holder, "point", Other(x: 1)) });
    describe(capture { write(holder, "point", Point(9)) });
    describe(capture { write(holder, "point", nil) });
    describe(capture { write(holder, "nested", replacement) });
    describe(capture { write(holder, 3, nil) });
    describe(capture { read(holder, "absent") });
    describe(capture { read(holder, 3) });
    describe(capture { field(numbers) });
    describe(holder);
    var rows = [[1], [2]];
    describe(capture { push(rows, ["x"]) });
    describe(capture { push(rows, empty) });
    describe(rows);
    describe(numbers);
    describe(words);
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_enforces_array_and_field_contracts_like_the_vm() {
    let output = assert_vm_parity("contracts", CONTRACT_PROGRAM, &[]);
    for expected in [
        "mismatch arrpush",
        "mismatch arrprepend",
        "mismatch indexset",
        "mismatch index",
        "mismatch array_concat",
        "mismatch eq",
        "mismatch lt",
        "mismatch len",
        "mismatch array_empty_like",
        "bounds 2 2",
        "bounds -1 2",
        "missing absent",
    ] {
        assert!(output.contains(expected), "{expected}: {output}");
    }
}

const STORED_ERROR_PROGRAM: &str = r#"@!include("std")
use io;
use err::Error;
struct Slot { value: int, history: int[] }
fn half(value: int) -> int { value / 0 }
fn show(value) -> nil {
    when value is Error::DivisionByZero { io::println("division")!!; }
    else {
        when value is Error::Overflow { io::println("overflow")!!; }
        else { io::println(value)!!; };
    };
}
fn push(target, value) { target.append(value) }
fn plus(left, right) { left + right }
fn read(target, key) { target[key] }
fn write(target, key, value) {
    try { target[key] = value; "written" } catch(error) { error }
}
@propagate
fn main() -> nil {
    const failed = capture { half(1) };
    const big = capture { Error::Overflow() };
    var values: int[] = [failed, 2, big];
    show(values[0]);
    show(values[1]);
    show(values[2]);
    show(values.len());
    values[1] = failed;
    show(values);
    var only = [failed];
    show(capture { push(only, 1) });
    show(only.len());
    var slot = Slot(value: failed, history: [failed]);
    show(slot.value);
    show(slot.history[0]);
    slot.value = 3;
    show(slot);
    slot.value = big;
    slot.history.append(5);
    show(slot);
    const ignored = capture { push(values, failed) };
    show(ignored);
    show(values.len());
    show(capture { push(failed, 1) });
    show(capture { write(failed, 0, 1) });
    show(capture { write(values, big, 1) });
    show(capture { plus(failed, values) });
    show(capture { plus(values, big) });
    show(capture { failed[0] });
    show(capture { len(failed) });
    show(capture { failed.value });
    for item in values {
        const seen = capture { item + 0 };
        show(seen);
    }
    const detailed = [capture { values[9] }, capture { read(slot, "absent") }];
    show(detailed);
    when detailed[0] is Error::IndexOutOfBounds(index, len) {
        io::println($"{index}/{len}")!!;
    };
    var text = "message";
    const wrapped = Error::NativeFailure(message: text);
    text = "changed";
    show(wrapped);
    show(wrapped.message);
    show(text);
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_preserves_errors_stored_in_arrays_and_fields() {
    let output = assert_vm_parity("stored", STORED_ERROR_PROGRAM, &[]);
    assert!(output.starts_with("division\n2\noverflow\n3\n"), "{output}");
    assert!(output.contains("9/3\n"), "{output}");
}

const LOOP_PROGRAM: &str = r#"@!include("std")
use io;
use iter;
use err::Error;
struct Countdown { start: int }
struct Cursor { next_value: int, value: int = 0 }
extend Countdown with iter::IntoIterator[Cursor, int] {
    fn into_iter(self) -> Cursor { Cursor(next_value: self.start) }
}
extend Cursor with iter::Iterator[int] {
    fn next(mut self) -> bool {
        if self.next_value == 0 { return false; }
        self.value = self.next_value;
        self.next_value -= 1;
        true
    }
    fn current(self) -> int { self.value }
}
fn walk(values, extra) {
    var sum = 0;
    for value in values {
        if value == 2 { values.append(extra); }
        sum += value;
    }
    sum
}
fn front(values) {
    var sum = 0;
    for value in values { values.prepend(0); sum += value; }
    sum
}
fn each(value) -> nil { for item in value { io::println(item)!!; } }
@propagate
fn main(args: str[]) -> nil {
    var values: int[] = [1, 2, 3];
    var seen = 0;
    for value in values {
        if value == 1 { values[1] = 20; values[2] = 30; }
        if value == 20 { continue; }
        seen += value;
        if value == 30 { break; }
    }
    io::println(seen)!!;
    each("héllo 水");
    each("");
    const none: str[] = [];
    each(none);
    for argument in args { io::println(argument + "!")!!; }
    for left in values, right in args, index in 10..20 {
        io::println($"{left} {right} {index}")!!;
    }
    for item in Countdown(start: 3) { io::println(item)!!; }
    for item in iter::range(2, 5) { io::println(item)!!; }
    for row in [[1], [2, 3]] { for cell in row { io::println(cell)!!; } }
    for index in 0..values.len() { values[index] *= 2; }
    io::println(values)!!;
    const invalidated = capture { walk([1, 2, 3], 4) };
    when invalidated is Error::InvalidArgument(op) { io::println(op)!!; };
    const prepended = capture { front([1]) };
    when prepended is Error::InvalidArgument(op) { io::println(op)!!; };
    const invalid = capture { each(42) };
    when invalid is Error::TypeMismatch(op) { io::println(op)!!; };
    each(args);
    walk([2], 1)!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_iterates_arrays_strings_and_custom_iterators_like_the_vm() {
    let output = assert_vm_parity("loops", LOOP_PROGRAM, &["a", "水"]);
    assert!(
        output
            .contains("array iteration invalidated\narray iteration invalidated\nfor iteration\n"),
        "{output}"
    );
}

const STRUCT_PROGRAM: &str = r#"@!include("std")
use io;
struct Marker;
struct Empty {}
struct Id(int);
struct Config {
    name: str = default_name(),
    count: int = default_count(),
    tags: str[] = [],
}
struct Node { value: int, next: Node? = nil }
struct Box[T: type] { value: T }
extend Node {
    fn sum(self) -> int {
        var total = self.value;
        const next = self.next;
        when next is Node { total += next.sum(); };
        total
    }
}
fn default_name() -> str { io::println("default name")!!; "cog" }
fn default_count() -> int { io::println("default count")!!; 3 }
fn argument(text: str, value: int) -> int { io::println(text)!!; value }
fn kind(value) -> str {
    match value {
        Marker => "marker",
        Empty => "empty",
        Id => "id",
        Config => "config",
        Node => "node",
        int => "int",
        _ => "other",
    }
}
fn pick(value) -> str {
    when value is Node if value.value > 1 { return "large node"; } else { return "no"; };
}
fn main() -> nil {
    var first = Config();
    const second = Config(count: argument("count", 7), name: "explicit");
    first.tags.append("one");
    io::println(first)!!;
    io::println(second)!!;
    const list = Node(1, next: Node(value: 2, next: Node(3)));
    io::println(list.sum())!!;
    io::println(list)!!;
    io::println(kind(Marker()))!!;
    io::println(kind(Empty()))!!;
    io::println(kind(Id(1)))!!;
    io::println(kind(first))!!;
    io::println(kind(list))!!;
    io::println(kind(1))!!;
    io::println(kind([1]))!!;
    io::println(kind(nil))!!;
    io::println(pick(list))!!;
    io::println(pick(list.next))!!;
    const number = Box[int](value: 4);
    var words = Box[str[]](value: ["w"]);
    words.value.append("x");
    io::println(number.value + 1)!!;
    io::println(words)!!;
    io::println([Marker(), Marker()])!!;
    const tail = list.next;
    if tail != nil { io::println(tail.next)!!; };
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_constructs_structs_with_defaults_and_type_tests_like_the_vm() {
    let output = assert_vm_parity("structs", STRUCT_PROGRAM, &[]);
    assert!(
        output.starts_with("default name\ndefault count\ncount\n"),
        "{output}"
    );
    assert!(output.contains("marker\nempty\nid\nconfig\nnode\nint\nother\n"));
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_reports_unhandled_aggregate_errors_with_the_vm_kind() {
    for (name, body) in [
        ("read_bounds", "const v = [1]; io::println(v[1]!!)!!;"),
        (
            "write_bounds",
            "var v = [1]; io::println(\"before\")!!; v[3] = 2; io::println(\"after\")!!;",
        ),
        (
            "empty_read",
            "const v: int[] = []; io::println(v[0] + 1)!!;",
        ),
        (
            "propagated",
            "const v = [1]; const x = v[-1]!!; io::println(x)!!;",
        ),
        (
            "condition",
            "const v: bool[] = []; if v[0] { io::println(1)!!; }",
        ),
    ] {
        let source = format!("@!include(\"std\")\nuse io;\nfn main() -> nil {{ {body} }}\n");
        assert_vm_parity(name, &source, &[]);
    }
}

const SCALE_PROGRAM: &str = r#"@!include("std")
use io;
struct Node { value: int, next: Node? = nil }
struct Counter { total: int = 0 }
fn add(counter: Counter, values: int[], index: int) -> nil { counter.total += values[index]; }
fn main() -> nil {
    var values: int[] = [];
    for index in 0..30000 { values.append(index); }
    var counter = Counter();
    for index in 0..values.len() { add(counter, values, index); }
    for value in values { counter.total += value; }
    io::println(counter.total)!!;
    var rows: int[][] = [];
    for index in 0..3000 { rows.append([index, index + 1]); }
    var cells = 0;
    for row in rows { for cell in row { cells += cell; } }
    io::println(cells)!!;
    var list = Node(0);
    for index in 1..20000 { list = Node(index, next: list); }
    io::println(list.value)!!;
    // Releasing the long list must not recurse through every node.
    list = Node(-1);
    io::println(list)!!;
    for index in 0..1000 {
        var a = Node(index);
        const b = Node(index, next: a);
        a.next = b;
    }
    var kept = Node(0);
    for index in 1..1000 { kept = Node(index, next: kept); }
    io::println($"done {kept.value}")!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_runs_large_loops_and_releases_deep_structures() {
    let output = assert_vm_parity("scale", SCALE_PROGRAM, &[]);
    assert!(output.ends_with("done 999\n"), "{output}");
}

const CYCLE_PROGRAM: &str = r#"@!include("std")
use fmt;
use io;
use str;
use err::Error;
struct Node { value: int, next: Node? = nil, children: Node[] = [] }
@propagate
fn main() -> nil {
    var node = Node(1);
    node.next = node;
    node.children.append(node);
    const first = node.next;
    if first != nil {
        const second = first.next;
        if second != nil { io::println(second.value)!!; };
    };
    var out = str::builder()!!;
    const rendered = capture { fmt::write(out, "{}", node) };
    when rendered is Error::ResourceLimit(resource) { io::println(resource)!!; };
    var deep = Node(0);
    for index in 0..40 { deep = Node(index, next: deep); }
    io::println(deep)!!;
    for index in 0..40 { deep = Node(index, next: deep); }
    io::println(capture { fmt::write(out, "{}", deep) })!!;
    io::println(str::build(out)!!.len())!!;
    io::println(node.children)!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_bounds_cyclic_and_deep_rendering_like_the_vm() {
    let output = assert_vm_parity("cycle", CYCLE_PROGRAM, &[]);
    assert!(
        output.starts_with("1\nrender depth\n{children: [], next: {"),
        "{output}"
    );
    // The final unhandled rendering of a cycle ends the program with an Error.
    assert!(output.contains("Error: runtime resource limit: render depth\n"));
}

const DEPTH_PROGRAM: &str = r#"@!include("std")
use io;
use err::Error;
fn wrap(value) { [value] }
@propagate
fn main() -> nil {
    var value = wrap(1)!!;
    var depth = 0;
    while depth < 100 {
        const next = capture { wrap(value) };
        when next is Error::ResourceLimit(resource) {
            io::println($"{resource} at {depth}")!!;
            depth = 100;
        } else {
            value = next!!;
            depth += 1;
        };
    }
    var holder = [];
    const pushed = capture { holder.append(value) };
    when pushed is Error::ResourceLimit(resource) { io::println(resource)!!; }
    else { io::println(holder.len())!!; };
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_limits_array_type_depth_like_the_vm() {
    let output = assert_vm_parity("depth", DEPTH_PROGRAM, &[]);
    assert!(output.starts_with("array type depth at "), "{output}");
}

const BYTES_PROGRAM: &str = r#"@!include("std")
use bytes;
use io;
struct Packet { body: bytes, parts: bytes[] = [] }
fn describe(value) -> str {
    when value is bytes { return "bytes"; };
    "other"
}
fn size(value: bytes) -> int { bytes::len(value) }
@propagate
fn main() -> nil {
    var buffer = bytes::from_str("abc");
    var packet = Packet(body: buffer);
    packet.parts.append(buffer);
    bytes::set(packet.parts[0], 0, 122);
    io::println(describe(packet.body))!!;
    io::println(describe(packet))!!;
    io::println(describe("abc"))!!;
    io::println(size(packet.body))!!;
    io::println(bytes::to_str(buffer, 0, 3))!!;
    io::println(packet)!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_stores_and_tests_byte_buffers_in_aggregates_like_the_vm() {
    let output = assert_vm_parity("aggregate_bytes", BYTES_PROGRAM, &[]);
    assert!(
        output.starts_with("bytes\nother\nother\n3\nzbc\n"),
        "{output}"
    );
}

const ORDER_PROGRAM: &str = r#"@!include("std")
use eq;
use io;
use err::Error;
struct Point { x: int, label: str }
extend Point with eq::Eq[Point] {
    fn equals(self, other: Point) -> bool {
        io::println("equals " + self.label + " " + other.label);
        self.x == other.x
    }
}
fn items(text: str, values: int[]) -> int[] { io::println(text)!!; values }
fn number(text: str, value: int) -> int { io::println(text)!!; value }
fn point(label: str) -> Point { io::println("make " + label)!!; Point(1, label) }
fn same[T: type + eq::Eq[T]](left: T, right: T) -> bool { left == right }
fn missing(values: Point[], index: int) -> Point { values[index] }
@propagate
fn main() -> nil {
    var values: int[] = [number("first", 1), number("second", 2)];
    items("append receiver", values).append(number("append value", 3));
    items("prepend receiver", values).prepend(number("prepend value", 0));
    items("set receiver", values)[number("set index", 0)] = number("set value", 9);
    io::println(values)!!;
    io::println(point("a") == point("b"))!!;
    io::println(point("c") != point("d"))!!;
    io::println(same(point("e"), Point(2, "f")))!!;
    const points = [Point(1, "g")];
    const failed = capture { missing(points, 4) == point("h") };
    when failed is Error::IndexOutOfBounds(index, len) {
        io::println($"{index} {len}")!!;
    };
    items("bad receiver", values)[number("bad index", 10)] = number("bad value", 9);
    io::println("unreachable")!!;
}
"#;

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_evaluates_aggregate_operands_and_struct_equality_like_the_vm() {
    let output = assert_vm_parity("order", ORDER_PROGRAM, &[]);
    assert!(
        output.contains(
            "prepend receiver\nprepend value\nset receiver\nset index\nset value\n[9, 1, 2, 3]\n"
        ),
        "{output}"
    );
    assert!(output.contains("equals a b\ntrue\n"), "{output}");
    assert!(
        output.ends_with("make h\n4 1\nbad receiver\nbad index\nbad value\n"),
        "{output}"
    );
}

#[test]
fn wasi_rejects_unsupported_aggregates_at_build_time() {
    for (name, body) in [
        ("tuple", "const v = (1, 2); io::println(v.0)!!;"),
        ("record", "const v = { a: 1 }; io::println(v.a)!!;"),
        (
            "float_array",
            "const v: float[] = []; io::println(v.len())!!;",
        ),
        (
            "closure",
            "const k = 1; const f = fn[k]() -> int { k }; io::println((f)())!!;",
        ),
        ("clone", "const v = [1]; io::println(v.clone())!!;"),
    ] {
        let source = format!("@!include(\"std\")\nuse io;\nfn main() -> nil {{ {body} }}\n");
        assert_build_rejected(name, &source);
    }
    assert_build_rejected(
        "float_field",
        "@!include(\"std\")\nuse io;\nstruct P { x: float? = nil }\nfn main() -> nil { io::println(P())!!; }\n",
    );
    assert_build_rejected(
        "map",
        "@!include(\"std\")\nuse io;\nuse collections::Map;\nfn main() -> nil { var m = Map::new[str, int](); m.set(\"a\", 1); }\n",
    );
}
