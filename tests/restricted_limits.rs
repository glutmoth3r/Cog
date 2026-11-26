//! Fixed default runtime limits applied by `cog run --restricted` and
//! `cog eval --restricted`. Unrestricted runs keep their unlimited defaults.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn run_source(source: &str, restricted: bool) -> Output {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "cog-restricted-limits-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.cog");
    std::fs::write(&path, format!("@!include(\"std\") use io;\n{source}")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cog"));
    command.arg("run");
    if restricted {
        command.arg("--restricted");
    }
    let output = command.arg(&path).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    output
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A resource-limit failure is a recoverable runtime error: exit code 1, not a
/// signal, abort, or panic.
fn assert_resource_limit(output: &Output, expected: &str) {
    let text = stderr(output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains(expected), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

const DEEP_RECURSION: &str = "fn depth(n: int) -> int { if n == 0 { return 0; } return depth(n - 1) + 1; }\n\
     fn main() { io::println(depth(20000))!!; }";

#[test]
fn restricted_run_leaves_normal_programs_unaffected() {
    let output = run_source(
        "fn fib(n: int) -> int { if n < 2 { return n; } return fib(n - 1) + fib(n - 2); }\n\
         fn main() { io::println(fib(15))!!; }",
        true,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, b"610\n");
}

#[test]
fn restricted_run_stops_unbounded_recursion() {
    let output = run_source(
        "fn f(n: int) -> int { return f(n + 1) + 1; }\nfn main() { f(0); }",
        true,
    );
    assert_resource_limit(&output, "runtime resource limit: call stack");
}

#[test]
fn restricted_run_stops_endless_println_at_the_output_cap() {
    let output = run_source(
        "use str;\nfn main() { const line = str::repeat(\"x\", 4095)!!; while true { io::println(line)!!; } }",
        true,
    );
    assert_resource_limit(&output, "restricted output limit of 8 MiB exceeded");
    // Each println writes a 4 KiB line, so the cap is reached in ~2k calls.
    assert!(output.stdout.len() <= 8 * 1024 * 1024);
    assert!(output.stdout.len() > 8 * 1024 * 1024 - 4096);
}

#[test]
fn restricted_run_reports_string_doubling_as_a_resource_limit_error() {
    let output = run_source(
        "fn main() { var s = \"ab\"; var i = 0; while i < 40 { s = (s + s)!!; i += 1; } }",
        true,
    );
    assert_resource_limit(&output, "runtime resource limit: str");
}

#[test]
fn restricted_eval_applies_the_buffer_limit() {
    let output = Command::new(env!("CARGO_BIN_EXE_cog"))
        .args([
            "eval",
            "--restricted",
            "--use",
            "str",
            "str::repeat(\"ab\", 20000000).len()",
        ])
        .output()
        .unwrap();
    assert_resource_limit(&output, "runtime resource limit: bytes");
}

#[test]
fn restricted_eval_still_evaluates_normal_expressions() {
    let output = Command::new(env!("CARGO_BIN_EXE_cog"))
        .args(["eval", "--restricted", "2 + 3 * 4"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, b"14\n");
}

#[test]
fn restricted_call_frame_limit_does_not_apply_to_unrestricted_runs() {
    let restricted = run_source(DEEP_RECURSION, true);
    assert_resource_limit(&restricted, "runtime resource limit: call stack");
    let unrestricted = run_source(DEEP_RECURSION, false);
    assert!(
        !stderr(&unrestricted).contains("runtime resource limit: call stack"),
        "{}",
        stderr(&unrestricted)
    );
}

// The step limit is 200,000,000 instructions, about 30 seconds in debug builds.
#[test]
#[ignore = "runs the 200M-step limit to exhaustion; slow in debug builds"]
fn restricted_run_stops_an_infinite_loop_at_the_step_limit() {
    let output = run_source("fn main() { while true {} }", true);
    assert_resource_limit(&output, "runtime resource limit: instruction steps");
}

#[test]
#[ignore = "runs the 200M-step limit to exhaustion; slow in debug builds"]
fn restricted_run_stops_endless_string_doubling_and_array_growth() {
    let output = run_source(
        "fn main() { var s = \"ab\"; while true { s = s + s; } }",
        true,
    );
    assert_resource_limit(&output, "runtime resource limit: instruction steps");
    let output = run_source(
        "fn main() { var a: int[] = []; while true { a.append(1); } }",
        true,
    );
    assert_resource_limit(&output, "runtime resource limit: instruction steps");
}
