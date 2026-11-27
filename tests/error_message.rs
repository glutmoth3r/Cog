//! `err::message` returns an Error's text without the `Error: ` label and
//! without propagating it.
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> Result<cog::backend::CompiledProgram, String> {
    let source = format!("@!include(\"std\") use io; use err; use err::Error;\n{source}");
    project::compile_source("error_message.cog", &source).map_err(|error| error.to_string())
}

/// Runs through bytecode encode/decode on the VM and with the JIT enabled,
/// requires both to agree, and returns the output.
fn run_limited(source: &str, buffer_limit: Option<usize>) -> String {
    let program = compile(source).unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let execute = |jit: bool| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, None, false);
        context.limits.max_buffer_bytes = buffer_limit;
        interpreter::run_with_context(&program, &mut context).unwrap();
        drop(context);
        String::from_utf8(output).unwrap()
    };
    let vm = execute(false);
    assert_eq!(execute(true), vm, "JIT and VM output differ");
    vm
}

fn run(source: &str) -> String {
    run_limited(source, None)
}

#[test]
fn message_is_the_rendered_text_without_the_error_label() {
    let output = run(r#"
        use bytes;
        fn show(error) {
            const text = err::message(error);
            const rendered = bytes::render(error);
            if "Error: " + text != bytes::to_str(rendered, 0, bytes::len(rendered)) {
                crash("text differs");
            }
            io::println(text)!!;
            io::println(error)!!;
        }
        fn main() {
            show(1 / 0);
            show(Error::InvalidArgument("next"));
        }
        "#);
    assert_eq!(
        output,
        "division by zero\nError: division by zero\n\
         Error::InvalidArgument(op: next)\nError: Error::InvalidArgument(op: next)\n"
    );
}

#[test]
fn only_the_leading_label_is_removed_from_payload_text() {
    let output = run(r#"
        
        fn risky() -> int { 1 / 0 }
        fn main() {
            io::println(err::message(Error::InvalidArgument("Error: inner")))!!;
            io::println(err::message(Error::IoFailure("Error: read", "Error: gone")))!!;
            // An Error stored in a payload is nested text and keeps its label.
            try { risky(); }
            catch(cause) { io::println(err::message(Error::InvalidArgument(cause)))!!; }
        }
        "#);
    assert_eq!(
        output,
        "Error::InvalidArgument(op: Error: inner)\n\
         Error::IoFailure(message: Error: gone, op: Error: read)\n\
         Error::InvalidArgument(op: Error: division by zero)\n"
    );
}

#[test]
fn message_inspects_without_triggering_catch_and_keeps_the_binding() {
    let output = run(r#"
        use err::message as describe;
        var calls = 0;
        fn fail() -> int { calls += 1; 1 / 0 }
        fn main() {
            try {
                io::println(err::message(fail()))!!;
                io::println(describe(fail()))!!;
            } catch(error) { crash("inspection triggered catch"); }
            try { fail(); }
            catch(error) {
                io::println($"caught: {err::message(error)}")!!;
                io::println(error.error_kind())!!;
            }
            const stored = fail();
            const text: str = err::message(stored);
            io::println(text)!!;
            io::println(err::kind(stored))!!;
            io::println(calls)!!;
        }
        "#);
    assert_eq!(
        output,
        "division by zero\ndivision by zero\ncaught: division by zero\n\
         DivisionByZero\ndivision by zero\nDivisionByZero\n4\n"
    );
}

#[test]
fn message_of_a_normal_value_is_empty() {
    let output = run(r#"
        fn main() {
            io::println(err::message(7))!!;
            io::println(err::message("text"))!!;
            io::println(err::message(nil))!!;
            io::println(err::message([1 / 0]))!!;
        }
        "#);
    assert_eq!(output, "\n\n\n\n");
}

#[test]
fn message_stays_stable_in_a_hot_loop() {
    let output = run(r#"
        fn main() {
            var same = 0;
            for _i in 0..5000 {
                try {
                    if err::message(1 / 0) == "division by zero" { same += 1; }
                } catch(error) { crash("inspection triggered catch"); }
            }
            io::println(same)!!;
        }
        "#);
    assert_eq!(output, "5000\n");
}

#[test]
fn oversized_message_is_a_recoverable_resource_limit() {
    let output = run_limited(
        r#"
        fn main() {
            const error = 1 / 0;
            try { const text = err::message(error); crash("missed limit"); }
            catch(limit) { if limit.error_kind() != "ResourceLimit" { crash("kind"); } }
            if err::kind(error) != "DivisionByZero" { crash("binding changed"); }
        }
        "#,
        Some(8),
    );
    assert_eq!(output, "");
}

#[test]
fn the_buffer_limit_counts_only_the_message() {
    let output = run_limited(
        r#"
        fn main() { io::print(err::message(1 / 0))!!; }
        "#,
        Some("division by zero".len()),
    );
    assert_eq!(output, "division by zero");
}

#[test]
fn a_user_function_named_message_keeps_working_beside_the_module() {
    let output = run(r#"
        fn message() -> str { "mine" }
        fn main() {
            io::println(message())!!;
            io::println(err::message(1 / 0))!!;
        }
        "#);
    assert_eq!(output, "mine\ndivision by zero\n");
}

#[test]
fn message_has_checked_arity_and_is_runtime_only() {
    let error = compile("fn main() { err::message()!!; }").unwrap_err();
    assert!(error.contains("err::message"), "{error}");
    let error = compile("fn main() { err::message(1, 2)!!; }").unwrap_err();
    assert!(error.contains("err::message"), "{error}");
    let error = compile(
        "comptime fn text() -> str { err::message(1) }\n@comptime const TEXT = text();\nfn main() {}",
    )
    .unwrap_err();
    assert!(error.contains("comptime"), "{error}");
}
