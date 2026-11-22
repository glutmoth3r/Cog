use cog::error::RuntimeError;
use cog::{bytecode, frontend, interpreter, project};

fn execute(source: &str) -> (String, Result<(), RuntimeError>) {
    let source = format!("@!include(\"std\") use io; use err::Error;\n{source}");
    let artifact = project::compile_source("error_semantics_v2.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let artifact = bytecode::decode(&bytecode::encode(&artifact).unwrap()).unwrap();
    let mut output = Vec::new();
    let result = interpreter::run_with(&artifact, &mut output);
    (String::from_utf8(output).unwrap(), result)
}

#[test]
fn automatic_propagation_observes_completed_expressions_and_runs_cleanup() {
    let (output, result) = execute(
        r#"
fn fail() -> int { 10 / 0 }
fn side() -> int { io::println("right"); 2 }
@propagate
fn work() -> int {
    defer { io::println("older"); }
    defer { io::println("newer"); "bad" as int; }
    const result = fail() + side();
    io::println("unreachable");
    result
}
fn main() -> int {
    const error = work();
    io::println("ordinary caller");
    error
}
"#,
    );
    assert_eq!(output, "right\nnewer\nolder\nordinary caller\n");
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    assert_eq!(error.suppressed().len(), 1);
    assert_eq!(error.suppressed()[0].kind(), "ParseFailure");
}

#[test]
fn capture_preserves_only_its_result_and_explicit_propagation_bypasses_try() {
    let (output, result) = execute(
        r#"
fn fail() -> int { 10 / 0 }
@propagate
fn forward() -> int { return capture { capture { fail() } }; }
@propagate
fn statement_in_capture() -> int {
    const result = capture {
        fail();
        crash("capture swallowed a statement error");
    };
    result
}
@propagate
fn try_capture() -> int {
    try { capture { fail() } } catch(e) { 13 }
}
@propagate
fn explicit() -> int {
    try { capture { fail()!! } }
    catch(e) { crash("explicit propagation entered its local catch"); }
}
@propagate
fn main() {
    const result = capture { forward() };
    when result is Error::DivisionByZero { io::println("captured"); };
    io::println(result ?! 7);
    const recovered = capture { try { fail() } catch(e) { 11 } };
    io::println(recovered);
    io::println(try_capture());
    try { const _value = statement_in_capture(); }
    catch(e) { io::println("statement"); }
    try { explicit(); } catch(e) { io::println("explicit"); }
}
"#,
    );
    result.unwrap();
    assert_eq!(output, "captured\n7\n11\n13\nstatement\nexplicit\n");
}

#[test]
fn aggregate_leaves_and_closure_bodies_keep_their_existing_boundaries() {
    let (output, result) = execute(
        r#"
fn fail() -> int { 10 / 0 }
@propagate
fn main() {
    var array: int[] = [fail()];
    const tuple = (fail(), 2);
    const record = { leaf: fail() };
    const callback = fn() -> int {
        const ignored = fail();
        io::println("closure");
        3
    };
    array[0] = "bad" as int;
    io::println(array[0].error_kind());
    io::println(tuple[0].error_kind());
    io::println(record.leaf.error_kind());
    io::println((callback)());
}
"#,
    );
    result.unwrap();
    assert_eq!(
        output,
        "ParseFailure\nDivisionByZero\nDivisionByZero\nclosure\n3\n"
    );
}

#[test]
fn automatic_propagation_observes_await_at_the_statement_boundary() {
    let (output, result) = execute(
        r#"
@async @propagate
fn child() -> int {
    defer { io::println("child cleanup"); }
    const failed = 1 / 0;
    io::println("unreachable");
    failed
}
@async @propagate
fn main() -> nil {
    defer { io::println("main cleanup"); }
    const failed = await child();
    io::println("unreachable");
}
"#,
    );
    assert_eq!(output, "child cleanup\nmain cleanup\n");
    assert_eq!(result.unwrap_err().kind(), "DivisionByZero");
}

#[test]
fn capture_keyword_does_not_change_parsing_in_ordinary_functions() {
    let (output, result) = execute(
        r#"
fn main() {
    const capture = true;
    if capture { io::println("ordinary identifier"); }
}
"#,
    );
    result.unwrap();
    assert_eq!(output, "ordinary identifier\n");
    for source in [
        "@propagate const x = 1;",
        "@propagate @propagate fn main() {}",
        "@comptime @propagate fn main() {}",
    ] {
        assert!(
            !frontend::parse("attribute.cog", source)
                .diagnostics
                .is_empty(),
            "{source}"
        );
    }
}
