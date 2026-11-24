use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> Result<cog::backend::CompiledProgram, String> {
    let source = format!("@!include(\"std\")\nuse io;\n{source}");
    project::compile_source("function_values.cog", &source)
}

fn run(source: &str) -> Result<String, String> {
    let program = compile(source)?;
    let program = bytecode::decode(&bytecode::encode(&program).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).map_err(|error| error.to_string())?;
    String::from_utf8(output).map_err(|error| error.to_string())
}

#[test]
fn struct_function_field_uses_default_named_function_when_omitted() {
    let output = run(r#"
        fn double(x: int) -> int { x * 2 }
        struct Handler { name: str, run: fn(int) -> int = double }
        fn main() {
            const h = Handler(name: "d");
            io::println((h.run)(5))!!;
            io::println(h.name)!!;
        }
    "#)
    .unwrap();
    assert_eq!(output, "10\nd\n");
}

#[test]
fn struct_function_field_accepts_closure_with_explicit_capture() {
    let output = run(r#"
        fn double(x: int) -> int { x * 2 }
        struct Handler { name: str, run: fn(int) -> int = double }
        fn main() {
            const base = 100;
            const h = Handler(name: "c", run: fn[base](x: int) -> int { x + base });
            io::println((h.run)(5))!!;
        }
    "#)
    .unwrap();
    assert_eq!(output, "105\n");
}

#[test]
fn calling_function_field_without_parentheses_is_unknown_method() {
    let error = compile(
        r#"
        fn double(x: int) -> int { x * 2 }
        struct Handler { name: str, run: fn(int) -> int = double }
        fn main() {
            const h = Handler(name: "d");
            io::println(h.run(5))!!;
        }
    "#,
    )
    .map(|_| ())
    .expect_err("method-style call of a function field must not compile");
    assert!(error.contains("E1002"), "unexpected diagnostic: {error}");
}

#[test]
fn json_stringify_rejects_function_values_with_type_mismatch() {
    let output = run(r#"
        use json;
        use err::Error;
        fn double(x: int) -> int { x * 2 }
        struct Holder { run: fn(int) -> int }
        fn report(result) {
            when result is Error::TypeMismatch(_) { io::println("mismatch")!!; }
            else { crash("function value was serialized"); }
        }
        fn main() {
            const f: fn(int) -> int = double;
            report(json::stringify(f));
            report(json::stringify([f]));
            report(json::stringify({ run: f }));
            report(json::stringify(Holder(run: f)));
        }
    "#)
    .unwrap();
    assert_eq!(output, "mismatch\nmismatch\nmismatch\nmismatch\n");
}

#[test]
fn mutable_capture_shares_state_across_calls_through_struct_field() {
    let output = run(r#"
        struct Counter { next: fn() -> int }
        fn main() {
            var total = 0;
            const c = Counter(next: fn[var total]() -> int {
                total += 1;
                total
            });
            io::println((c.next)())!!;
            io::println((c.next)())!!;
            io::println((c.next)())!!;
            io::println(total)!!;
        }
    "#)
    .unwrap();
    assert_eq!(output, "1\n2\n3\n3\n");
}
