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
