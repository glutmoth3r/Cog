//! Table-driven regressions for the CLI `--restricted` policy. Every denied
//! operation must surface a recoverable `Error::PermissionDenied` value and
//! leave no host side effect behind.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Scratch area under the Cargo target directory: `src` holds the program and
/// `work` is the (initially empty) working directory of the child process.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "restricted-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(self.work())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn run(&self, source: &str, arguments: &[&str]) -> Output {
        let program = self.root.join("src/main.cog");
        std::fs::write(&program, source).unwrap();
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .args(arguments)
            .arg(&program)
            .current_dir(self.work())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn report(output: &Output) -> String {
    format!(
        "status={:?} stdout={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const CLASSIFY: &str = r#"
    when r is Error::PermissionDenied(op) { io::println("denied")!!; }
    else {
        when r is Error { io::println("other-error")!!; }
        else { io::println("allowed")!!; }
    }
"#;

fn sync_program(imports: &str, expression: &str) -> String {
    format!(
        "@!include(\"std\") use io; use err::Error; {imports}\nfn main() {{ const r = {expression};{CLASSIFY} }}\n"
    )
}

fn async_program(imports: &str, expression: &str) -> String {
    format!(
        "@!include(\"std\") use io; use err::Error; {imports}\n@async fn main() {{ const r = await {expression};{CLASSIFY} }}\n"
    )
}

fn assert_denied_rows(rows: &[(&str, String)]) {
    for (name, source) in rows {
        let scratch = Scratch::new();
        let output = scratch.run(source, &["--restricted"]);
        assert!(output.status.success(), "{name}: {}", report(&output));
        assert_eq!(output.stdout, b"denied\n", "{name}: {}", report(&output));
        assert!(
            scratch.entries().is_empty(),
            "{name}: side effect left in the working directory: {:?}",
            scratch.entries()
        );
    }
}

#[test]
fn restricted_sync_host_operations_return_permission_denied_without_side_effects() {
    let sync = sync_program;
    let rows = [
        ("fs::metadata", sync("use fs;", "fs::metadata(\"x\")")),
        ("fs::read_dir", sync("use fs;", "fs::read_dir(\".\")")),
        (
            "fs::create_dir",
            sync("use fs;", "fs::create_dir(\"made\")"),
        ),
        (
            "fs::create_dir_all",
            sync("use fs;", "fs::create_dir_all(\"made/deep\")"),
        ),
        (
            "fs::remove_dir",
            sync("use fs;", "fs::remove_dir(\"made\")"),
        ),
        ("fs::remove_file", sync("use fs;", "fs::remove_file(\"x\")")),
        ("io::open read", sync("", "io::open(\"x\", \"r\", 0)")),
        (
            "io::open write",
            sync("", "io::open(\"created\", \"w\", 0)"),
        ),
        ("process::args", sync("use process;", "process::args()")),
        (
            "process::env",
            sync("use process;", "process::env(\"PATH\")"),
        ),
        (
            "process::shell",
            sync("use process;", "process::shell(\"echo hi > shell-made\")"),
        ),
        (
            "process::run_sync",
            sync(
                "use process;",
                "process::run_sync(\"touch\", [\"run-sync-made\"])",
            ),
        ),
        ("time::now", sync("use time;", "time::now()")),
        ("time::now_nanos", sync("use time;", "time::now_nanos()")),
        ("time::now_millis", sync("use time;", "time::now_millis()")),
        (
            "time::monotonic_nanos",
            sync("use time;", "time::monotonic_nanos()"),
        ),
        ("time::sleep", sync("use time;", "time::sleep(0.001)")),
        ("entropy::bytes", sync("use entropy;", "entropy::bytes(1)")),
    ];
    assert_denied_rows(&rows);
}

#[test]
fn restricted_async_host_operations_return_permission_denied_without_side_effects() {
    let asy = async_program;
    let rows = [
        (
            "async fs::write_text",
            asy("use async::fs;", "fs::write_text(\"made\", \"x\")"),
        ),
        (
            "async fs::append_text",
            asy("use async::fs;", "fs::append_text(\"made\", \"x\")"),
        ),
        (
            "async fs::metadata",
            asy("use async::fs;", "fs::metadata(\"x\")"),
        ),
        (
            "async fs::read_dir",
            asy("use async::fs;", "fs::read_dir(\".\")"),
        ),
        (
            "async fs::copy",
            asy("use async::fs;", "fs::copy(\"x\", \"copied\")"),
        ),
        (
            "async fs::rename",
            asy("use async::fs;", "fs::rename(\"x\", \"renamed\")"),
        ),
        (
            "async fs::remove_file",
            asy("use async::fs;", "fs::remove_file(\"x\")"),
        ),
        (
            "async fs::create_dir",
            asy("use async::fs;", "fs::create_dir(\"made\")"),
        ),
        (
            "async io::open read",
            asy("use async::io as aio;", "aio::open(\"x\", \"r\", 0)"),
        ),
        (
            "async io::open write",
            asy("use async::io as aio;", "aio::open(\"made\", \"w\", 0)"),
        ),
        (
            "async time::sleep",
            asy("use async::time;", "time::sleep(0.001)"),
        ),
        (
            "async time::timeout",
            asy("use async::time;", "time::timeout(time::sleep(0.001), 1.0)"),
        ),
        (
            "async process::run",
            asy(
                "use process;",
                "process::run(\"touch\", [\"async-run-made\"])",
            ),
        ),
        (
            "async net::connect",
            asy("use async::net;", "net::connect(\"127.0.0.1\", 9, 100)"),
        ),
        (
            "async net::listen",
            asy("use async::net;", "net::listen(\"127.0.0.1\", 0)"),
        ),
    ];
    assert_denied_rows(&rows);
}

#[test]
fn restricted_mode_keeps_stdout_and_unrestricted_runs_report_allowed() {
    let scratch = Scratch::new();
    let output = scratch.run(
        "@!include(\"std\") use io; fn main() { io::println(\"hello\")!!; io::print(\"tail\")!!; }",
        &["--restricted"],
    );
    assert!(output.status.success(), "{}", report(&output));
    assert_eq!(output.stdout, b"hello\ntail");

    // Without --restricted the classifier prints "allowed", so the "denied"
    // rows above cannot be false positives of the harness.
    let source = sync_program("use time;", "time::now()");
    let output = scratch.run(&source, &[]);
    assert!(output.status.success(), "{}", report(&output));
    assert_eq!(output.stdout, b"allowed\n");
}

#[test]
fn restricted_eval_denies_filesystem_process_and_time_access() {
    let scratch = Scratch::new();
    for (name, expression) in [
        ("fs", "fs::create_dir(\"eval-made\")"),
        ("process", "process::shell(\"echo hi > eval-shell\")"),
        ("time", "time::now()"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(["eval", "--restricted", "--use", name, expression])
            .current_dir(scratch.work())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        // `eval` prints an Error result as a value rather than failing.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with("Error: permission denied for `"),
            "{name}: {}",
            report(&output)
        );
        assert!(
            scratch.entries().is_empty(),
            "{name}: {:?}",
            scratch.entries()
        );
    }
}

#[test]
fn restricted_mode_applies_to_application_archives() {
    let scratch = Scratch::new();
    std::fs::write(
        scratch.root.join("package.kdl"),
        "package \"restricted-archive\" version=\"1\"\n",
    )
    .unwrap();
    std::fs::write(
        scratch.root.join("src/main.cog"),
        sync_program("use fs;", "fs::create_dir(\"made\")"),
    )
    .unwrap();
    let archive = scratch.root.join("application.cogpkg");
    let build = Command::new(env!("CARGO_BIN_EXE_cog"))
        .args(["package", "build"])
        .arg(&scratch.root)
        .arg("-o")
        .arg(&archive)
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", report(&build));
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .args(arguments)
            .arg(&archive)
            .current_dir(scratch.work())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    let output = run(&["--restricted"]);
    assert!(output.status.success(), "{}", report(&output));
    assert_eq!(output.stdout, b"denied\n", "{}", report(&output));
    assert!(scratch.entries().is_empty(), "{:?}", scratch.entries());

    let output = run(&[]);
    assert!(output.status.success(), "{}", report(&output));
    assert_eq!(output.stdout, b"allowed\n", "{}", report(&output));
    assert_eq!(scratch.entries(), ["made"]);
}

#[test]
fn restricted_mode_is_rejected_together_with_native_plugins() {
    let scratch = Scratch::new();
    let output = scratch.run(
        "@!include(\"std\") use io; fn main() { io::println(\"x\")!!; }",
        &["--restricted", "--plugin", "missing-plugin"],
    );
    assert!(!output.status.success(), "{}", report(&output));
    assert!(output.stdout.is_empty(), "{}", report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--restricted cannot be combined with native plugins"),
        "{stderr}"
    );
}
