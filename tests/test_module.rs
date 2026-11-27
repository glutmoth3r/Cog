use cog::{bytecode, project, sdk};

fn compile(source: &str) -> sdk::Program {
    let source = format!("@!include(\"std\")\nuse test; use eq;\n{source}");
    let program = project::compile_source("test_module.cog", &source)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    sdk::Engine::new()
        .decode(&bytecode::encode(&program).unwrap())
        .unwrap()
}

fn run(source: &str, jit: bool) -> Result<(), String> {
    let program = compile(source);
    let mut output = Vec::new();
    program
        .run_in(&mut sdk::RunContext::new(&mut output).with_jit(jit))
        .map_err(|error| error.message().to_owned())
}

#[test]
fn assertions_accept_successful_checks_in_vm_and_jit() {
    let source = r#"
        struct Point { x: int }
        extend Point with eq::Eq[Point] {
            fn equals(self, other: Point) -> bool { self.x == other.x }
        }
        fn main() {
            test::assert(true);
            test::assert_eq(4, 4);
            test::assert_eq("same", "same");
            test::assert_eq(Point(x: 7), Point(x: 7));
        }
    "#;
    assert_eq!(run(source, false), Ok(()));
    assert_eq!(run(source, true), Ok(()));
}

#[test]
fn failed_assertions_crash_with_a_clear_message() {
    for (source, message) in [
        ("fn main() { test::assert(false); }", "assertion failed"),
        (
            "fn main() { test::assert_eq(1, 2); }",
            "assertion failed: values are not equal",
        ),
    ] {
        for jit in [false, true] {
            let error = run(source, jit).unwrap_err();
            assert!(error.contains(message), "JIT={jit}: {error}");
        }
    }
}

#[test]
fn assert_eq_requires_the_equality_trait() {
    let source = "@!include(\"std\") use test; struct Point { x: int } fn main() { test::assert_eq(Point(x: 1), Point(x: 1)); }";
    let error = project::compile_source("test_module.cog", source).unwrap_err();
    assert!(error.to_string().contains("eq::Eq"), "{error}");
}
