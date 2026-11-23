use cog::{interpreter, project, runtime::VmCapabilities};
use std::ffi::OsString;

const SOURCE: &str = include_str!("../web/tutorial_logic.cog");

fn cases() -> Vec<(Vec<&'static str>, &'static str)> {
    vec![
        (vec!["check", "", ""], "match\n"),
        (vec!["check", "hello\n", "hello\n"], "match\n"),
        (vec!["check", "hello\n", "hello"], "mismatch\n"),
        (vec!["check", "Καλημέρα 🦀\n", "Καλημέρα 🦀\n"], "match\n"),
        (vec!["check", "é", "e\u{301}"], "mismatch\n"),
        (vec!["check", " hi", "hi"], "mismatch\n"),
        (vec![], "invalid-request\n"),
        (vec!["other", "a", "a"], "invalid-request\n"),
        (vec!["check", "a"], "invalid-request\n"),
        (vec!["check", "a", "a", "extra"], "invalid-request\n"),
    ]
}

#[test]
fn tutorial_checker_compares_exact_output_in_vm() {
    let program = project::compile_source("web/tutorial_logic.cog", SOURCE).unwrap();
    for (args, expected) in cases() {
        let mut output = Vec::new();
        interpreter::run_with_process_args(
            &program,
            &mut output,
            VmCapabilities::default(),
            args.into_iter().map(OsString::from).collect(),
        )
        .unwrap();
        assert_eq!(output, expected.as_bytes());
    }
}

#[test]
#[ignore = "requires rustc wasm32-wasip1 and Node WASI"]
fn tutorial_checker_matches_vm_in_wasi() {
    let program = project::compile_source("web/tutorial_logic.cog", SOURCE).unwrap();
    let directory = std::env::temp_dir().join(format!("cog-tutorial-logic-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let artifact = directory.join("checker.wasm");
    cog::wasm::build_executable(&program, &artifact).unwrap();
    for (args, expected) in cases() {
        let output = std::process::Command::new("node")
            .arg("--disable-warning=ExperimentalWarning")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/wasm/run.mjs"
            ))
            .arg(&artifact)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, expected.as_bytes(), "arguments: {args:?}");
    }
    std::fs::remove_dir_all(directory).unwrap();
}
