//! Error traces must show the same logical frames in the plain VM, the VM with
//! `--jit`, and native executables, including frames of inlined callees.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const SOURCE: &str = r#"@!include("std")
@inline
fn ratio(a: int, b: int) -> int {
    a / b
}
@inline("never")
fn scale(n: int) -> int {
    const doubled = n * 2;
    ratio(10, n) + doubled
}
fn main() -> nil { scale(0)!!; }
"#;

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cog-trace-parity-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inline.cog"), SOURCE).unwrap();
    dir
}

/// Keeps the trace from the runtime error line on and hides the scratch path,
/// so modes that print different banners (`error:` prefix, build notices)
/// compare equal.
fn normalized_trace(stderr: &[u8], dir: &Path) -> String {
    let text = String::from_utf8_lossy(stderr).replace(dir.to_str().unwrap(), "<dir>");
    let start = text.find("runtime error at").expect(&text);
    text[start..].to_string()
}

fn assert_expected_trace(trace: &str) {
    let lines: Vec<&str> = trace.lines().collect();
    assert_eq!(
        lines[0], "runtime error at <dir>/inline.cog:4:5: division by zero",
        "{trace}"
    );
    assert_eq!(
        lines[1], "  origin: inline::ratio at <dir>/inline.cog:4:5 (inlined)",
        "{trace}"
    );
    assert_eq!(
        lines[2], "  called from inline::scale at <dir>/inline.cog:9:5",
        "{trace}"
    );
    assert!(
        lines[3].starts_with("  called from inline::main at <dir>/inline.cog:11:"),
        "{trace}"
    );
    // The propagation chain must not repeat a hop.
    let propagated: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.trim_start().starts_with("propagated through"))
        .collect();
    let mut unique = propagated.clone();
    unique.dedup();
    assert_eq!(propagated, unique, "{trace}");
    assert_eq!(trace.matches("origin:").count(), 1, "{trace}");
}

fn run_trace(extra: &[&str]) -> String {
    let dir = scratch_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .args(extra)
        .arg(dir.join("inline.cog"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let trace = normalized_trace(&output.stderr, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    trace
}

#[test]
fn vm_trace_marks_inlined_helper_and_keeps_caller_call_site() {
    assert_expected_trace(&run_trace(&[]));
}

#[test]
fn jit_trace_matches_plain_vm_trace() {
    let vm = run_trace(&[]);
    let jit = run_trace(&["--jit"]);
    assert_expected_trace(&jit);
    assert_eq!(vm, jit);
}

/// Needs a C toolchain and the AOT runtime archive. Build it with
/// `cargo rustc --release --locked --target x86_64-unknown-linux-gnu --lib --crate-type staticlib`
/// and run with `COG_NATIVE_RUNTIME=<path to libcog.a>`.
#[test]
#[ignore = "needs a C compiler and COG_NATIVE_RUNTIME (AOT runtime archive)"]
fn native_executable_trace_matches_plain_vm_trace() {
    let vm = run_trace(&[]);
    let dir = scratch_dir();
    let exe = dir.join("inline-native");
    let build = Command::new(env!("CARGO_BIN_EXE_cog"))
        .args(["build", "--native"])
        .arg(dir.join("inline.cog"))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let output = Command::new(&exe).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let native = normalized_trace(&output.stderr, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_expected_trace(&native);
    assert_eq!(vm, native);
}
