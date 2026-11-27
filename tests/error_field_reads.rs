//! Reading a field of an Error value keeps the original Error: the first cause
//! survives, as in the other strict operations.
use cog::error::RuntimeError;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::value::Value;
use cog::{bytecode, interpreter, project};

/// Runs through bytecode encode/decode on the VM and with the JIT enabled, and
/// requires both backends to agree.
fn observe(body: &str) -> (String, Result<(), RuntimeError>) {
    let source = format!("@!include(\"std\") use io; use err::Error;\n{body}");
    let program = project::compile_source("error_field_reads.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let execute = |jit: bool| {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
        context.configure_jit(jit, None, false);
        let result = interpreter::run_with_context(&program, &mut context).map(|_| ());
        drop(context);
        (String::from_utf8(output).unwrap(), result)
    };
    let vm = execute(false);
    let jit = execute(true);
    assert_eq!(jit.0, vm.0, "JIT and VM output differ");
    let kind = |result: &Result<(), RuntimeError>| {
        result
            .as_ref()
            .err()
            .map(RuntimeError::kind)
            .map(str::to_owned)
    };
    assert_eq!(kind(&jit.1), kind(&vm.1));
    vm
}

#[test]
fn an_error_in_place_of_a_struct_survives_a_field_read() {
    let (_, result) = observe(
        r#"
        struct Node { value: int }
        fn bad() -> Node { Error::InvalidArgument("next") }
        fn read() -> int { const node = bad(); node.value }
        fn main() { read()!!; }
        "#,
    );
    let error = result.unwrap_err();
    assert_eq!(error.kind(), "InvalidArgument");
    assert!(matches!(error.field("op"), Some(Value::Str(op)) if &*op == "next"));
}

#[test]
fn a_fallback_error_keeps_its_cause_through_chained_and_positional_reads() {
    let (output, result) = observe(
        r#"
        struct Node { value: int, next: Node?, pair: (int, int) }
        var calls = 0;
        fn next(node: Node) -> Node {
            calls += 1;
            node.next ?? Error::InvalidArgument("next")
        }
        fn main() {
            const root = Node(1, nil, (2, 3));
            const broken = next(root);
            when broken.value is Error::InvalidArgument(op) { io::println(op)!!; }
            when broken.pair.1 is Error::InvalidArgument(op) { io::println(op)!!; }
            when next(root).next is Error::InvalidArgument(op) { io::println(op)!!; }
            try { const first = next(root).pair.0 + 1; }
            catch(e) { io::println(e.error_kind())!!; }
            io::println(calls)!!;
        }
        "#,
    );
    result.unwrap();
    assert_eq!(output, "next\nnext\nnext\nInvalidArgument\n3\n");
}

#[test]
fn error_payload_fields_stay_readable_and_other_names_return_the_error() {
    let (output, result) = observe(
        r#"
        fn main() {
            try { const number = "invalid" as int; }
            catch(e) {
                io::println(e.from)!!;
                when e.absent is Error::ParseFailure(from, to) { io::println(to)!!; }
            }
        }
        "#,
    );
    result.unwrap();
    assert_eq!(output, "str\nint\n");
}

#[test]
fn hot_field_reads_of_an_error_keep_the_cause() {
    let (output, result) = observe(
        r#"
        struct Node { value: int }
        fn pick(index: int, node: Node) -> Node {
            if index % 1000 == 999 { return Error::Timeout(); }
            node
        }
        fn main() {
            const node = Node(1);
            var total = 0;
            var timeouts = 0;
            for index in 0..20000 {
                const read = pick(index, node).value;
                when read is Error::Timeout { timeouts += 1; } else { total += read; }
            }
            io::println(total)!!;
            io::println(timeouts)!!;
        }
        "#,
    );
    result.unwrap();
    assert_eq!(output, "19980\n20\n");
}

#[test]
fn a_missing_record_field_is_still_reported_as_missing() {
    let (_, result) = observe(
        r#"
        fn main() { const record = { present: 1 }; record.absent!!; }
        "#,
    );
    assert_eq!(result.unwrap_err().kind(), "MissingField");
}
