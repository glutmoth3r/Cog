use cog::{bytecode, project, sdk};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "exit-status-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn source(value: i64, asynchronous: bool) -> String {
    format!(
        r#"@!include("std")
use io;
{}fn main() -> int {{
    defer {{ io::println("cleanup")!!; }}
    {}
    {value}
}}
"#,
        if asynchronous { "@async " } else { "" },
        if asynchronous {
            "(await yield_now())!!;"
        } else {
            ""
        }
    )
}
fn run(arguments: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cog"))
        .args(arguments)
        .output()
        .unwrap()
}
#[test]
fn source_cached_bytecode_and_jit_use_low_eight_bits_after_cleanup() {
    let directory = Directory::new();
    let file = directory.0.join("app.cog");
    let artifact = directory.0.join("app.cogb");
    for asynchronous in [false, true] {
        for value in [0, 7, 255, 256, -1, i64::MIN, i64::MAX] {
            std::fs::write(&file, source(value, asynchronous)).unwrap();
            for options in [
                vec!["run".as_ref(), file.as_os_str()],
                vec!["run".as_ref(), file.as_os_str(), "--jit".as_ref()],
                vec![artifact.as_os_str()],
                vec!["run".as_ref(), file.as_os_str(), "--restricted".as_ref()],
            ] {
                let output = run(&options);
                assert_eq!(
                    output.status.code(),
                    Some((value & 255) as i32),
                    "{value}, async={asynchronous}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(output.stdout, b"cleanup\n");
            }
        }
    }
}
#[test]
fn archived_application_uses_status_and_non_integer_main_defaults_to_zero() {
    let directory = Directory::new();
    std::fs::create_dir_all(directory.0.join("src")).unwrap();
    std::fs::write(
        directory.0.join("package.kdl"),
        "package \"exit-app\" version=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(directory.0.join("dependencies.kdl"), "").unwrap();
    std::fs::write(directory.0.join("src/main.cog"), source(7, false)).unwrap();
    let archive = directory.0.join("app.cogpkg");
    let built = run(&[
        "package".as_ref(),
        "build".as_ref(),
        directory.0.as_os_str(),
        "-o".as_ref(),
        archive.as_os_str(),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    for restricted in [false, true] {
        let mut options = vec![archive.as_os_str()];
        if restricted {
            options.push("--restricted".as_ref());
        }
        let output = run(&options);
        assert_eq!(
            output.status.code(),
            Some(7),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"cleanup\n");
    }
    let file = directory.0.join("text.cog");
    std::fs::write(&file, "fn main() -> str { \"normal\" }").unwrap();
    let output = run(&[file.as_os_str()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `main` sources whose result must not select the exit status.
const UNDECLARED_STATUS: &[&str] = &[
    "fn main() { 7 }",
    "fn main() { return 7; }",
    "@async fn main() { (await yield_now())!!; 7 }",
    "fn main(args: str[]) { 7 }",
    "fn main() -> uint { 18446744073709551615 }",
    "@async fn main() -> uint { 18446744073709551615 }",
    "fn main() -> int? { 7 }",
    "fn main() -> str { \"normal\" }",
];

/// `main` sources that declare `int`, with their expected status.
const DECLARED_STATUS: &[&str] = &[
    "fn main() -> int { 7 }",
    "fn main() -> int { return 7; }",
    "@async fn main() -> int { (await yield_now())!!; 7 }",
    "fn main(args: str[]) -> int { 7 }",
    "@comptime const Status: type = int; fn main() -> Status { 7 }",
];

/// `main` sources that end with an unhandled failure.
const FAILING_MAIN: &[&str] = &[
    "fn main() { (1 / 0)!!; }",
    "fn main() { 1 / 0 }",
    "@async fn main() { (await yield_now())!!; 1 / 0 }",
    "@async fn main() { (await yield_now())!!; (1 / 0)!!; }",
];

fn status_source(body: &str) -> String {
    format!("@!include(\"std\") {body}")
}

#[test]
fn only_a_declared_int_main_selects_the_status() {
    let engine = sdk::Engine::new();
    let run_main = |body: &str| {
        let artifact = project::compile_source("status.cog", &status_source(body))
            .unwrap_or_else(|error| panic!("{body}: {error}"));
        let program = engine
            .decode(&bytecode::encode(&artifact).unwrap())
            .unwrap();
        let mut output = Vec::new();
        let mut context = sdk::RunContext::new(&mut output);
        let result = program.run_in(&mut context);
        (result.is_ok(), context.exit_code())
    };
    for body in UNDECLARED_STATUS {
        assert_eq!(run_main(body), (true, 0), "{body}");
    }
    for body in DECLARED_STATUS {
        assert_eq!(run_main(body), (true, 7), "{body}");
    }
    for body in FAILING_MAIN {
        assert_eq!(run_main(body), (false, 0), "{body}");
    }
    let directory = Directory::new();
    let file = directory.0.join("status.cog");
    for (bodies, expected) in [
        (UNDECLARED_STATUS, 0),
        (DECLARED_STATUS, 7),
        (FAILING_MAIN, 1),
    ] {
        for body in bodies {
            std::fs::write(&file, status_source(body)).unwrap();
            for jit in [false, true] {
                let mut options = vec!["run".as_ref(), file.as_os_str()];
                if jit {
                    options.push("--jit".as_ref());
                }
                let output = run(&options);
                assert_eq!(
                    output.status.code(),
                    Some(expected),
                    "{body}, jit={jit}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_ignores_results_of_undeclared_main() {
    let directory = Directory::new();
    for (body, expected) in [
        ("fn main() { 7 }", 0),
        ("fn main() -> uint { 18446744073709551615 }", 0),
        ("fn main() -> int { 7 }", 7),
        ("fn main() { 1 / 0 }", 1),
    ] {
        let artifact = project::compile_source("status.cog", &status_source(body)).unwrap();
        let wasm = directory.0.join("status.wasm");
        cog::wasm::build_executable(&artifact, &wasm).unwrap();
        let output = Command::new("node")
            .arg("--disable-warning=ExperimentalWarning")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/wasm/run.mjs"
            ))
            .arg(&wasm)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected), "{body}");
    }
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[ignore = "requires a C compiler and the Cog AOT runtime archive"]
fn aot_ignores_results_of_undeclared_main() {
    let directory = Directory::new();
    for (body, expected) in [
        ("fn main() { 7 }", 0),
        ("@async fn main() { (await yield_now())!!; 7 }", 0),
        ("fn main() -> int { 7 }", 7),
        ("fn main() { 1 / 0 }", 1),
    ] {
        let artifact = project::compile_source("status.cog", &status_source(body)).unwrap();
        let executable = directory.0.join("status");
        cog::native::build_executable(&artifact, &executable).unwrap();
        let output = Command::new(&executable).output().unwrap();
        assert_eq!(output.status.code(), Some(expected), "{body}");
    }
}

#[test]
fn embedding_reports_status_without_exiting_and_resets_it_between_runs() {
    let engine = sdk::Engine::new();
    let compile = |body: &str| {
        let artifact = project::compile_source("embedding.cog", body).unwrap();
        engine
            .decode(&bytecode::encode(&artifact).unwrap())
            .unwrap()
    };
    let code = compile("@!include(\"std\") @async fn main() -> int { (await yield_now())!!; -1 }");
    let nil = compile("fn main() {}");
    let failed = compile("fn main() -> int { (1 / 0)!! }");
    let mut output = Vec::new();
    let mut context = sdk::RunContext::new(&mut output);
    code.run_in(&mut context).unwrap();
    assert_eq!(context.exit_code(), 255);
    nil.run_in(&mut context).unwrap();
    assert_eq!(context.exit_code(), 0);
    code.run_in(&mut context).unwrap();
    assert!(failed.run_in(&mut context).is_err());
    assert_eq!(context.exit_code(), 0);
}
#[test]
fn output_finish_failure_overrides_the_returned_status() {
    struct FailingFlush;
    impl std::io::Write for FailingFlush {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("finish failed"))
        }
    }
    let artifact = project::compile_source(
        "finish.cog",
        "@!include(\"std\") use io; fn main() -> int { io::print(\"pending\")!!; 7 }",
    )
    .unwrap();
    let program = sdk::Engine::new()
        .decode(&bytecode::encode(&artifact).unwrap())
        .unwrap();
    let mut output = FailingFlush;
    let mut context = sdk::RunContext::new(&mut output);
    assert!(program.run_in(&mut context).is_err());
    assert_eq!(context.exit_code(), 0);
}

#[test]
fn runtime_failure_overrides_status_and_cleanup_runs_before_failure() {
    let directory = Directory::new();
    let file = directory.0.join("failed.cog");
    std::fs::write(
        &file,
        r#"@!include("std") use io;
fn main() -> int { defer { io::println("cleanup")!!; } (1 / 0)!! }
"#,
    )
    .unwrap();
    let output = run(&["run".as_ref(), file.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"cleanup\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("division by zero"));
}
#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_uses_integer_status_after_output() {
    let directory = Directory::new();
    for value in [7, 255, 256, -1, i64::MIN, i64::MAX] {
        let artifact = project::compile_source(
            "status.cog",
            &source(value, false).replace(
                "defer { io::println(\"cleanup\")!!; }",
                "io::println(\"cleanup\")!!;",
            ),
        )
        .unwrap();
        let wasm = directory.0.join("status.wasm");
        cog::wasm::build_executable(&artifact, &wasm).unwrap();
        let output = Command::new("node")
            .arg("--disable-warning=ExperimentalWarning")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/wasm/run.mjs"
            ))
            .arg(&wasm)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some((value & 255) as i32),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"cleanup\n");
    }
}
#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[ignore = "requires a C compiler and the Cog AOT runtime archive"]
fn aot_uses_integer_status_after_sync_and_async_cleanup() {
    let directory = Directory::new();
    for asynchronous in [false, true] {
        for value in [7, 255, 256, -1, i64::MIN, i64::MAX] {
            let artifact =
                project::compile_source("status.cog", &source(value, asynchronous)).unwrap();
            let executable = directory.0.join("status");
            cog::native::build_executable(&artifact, &executable).unwrap();
            let output = Command::new(&executable).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some((value & 255) as i32),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"cleanup\n");
        }
    }
}
