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

#[test]
fn wasi_still_rejects_structs_in_string_programs() {
    let source = r#"@!include("std")
use io;
struct Point { x: int }
fn main() -> nil { io::println(Point(x: 1).x)!!; }
"#;
    let (program, _) = project::compile_source_with_warnings("point.cog", source).unwrap();
    let directory = Directory::new();
    let artifact = directory.0.join("point.wasm");
    let error = wasm::build_executable(&program, &artifact).unwrap_err();
    assert!(error.contains("WASI backend does not support"), "{error}");
    assert!(!artifact.exists());
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
    let error = wasm::build_executable(&program, &artifact).unwrap_err();
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
fn wasi_rejects_user_types_named_like_core_errors_at_build_time() {
    let struct_source = r#"@!include("std")
use io;
struct Error { x: int }
fn main() -> nil { io::println(Error(x: 1).x)!!; }
"#;
    assert_build_rejected("user_struct_error", struct_source);
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
