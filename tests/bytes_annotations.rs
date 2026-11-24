//! `bytes` as a first-class annotation type for parameters, results, locals,
//! struct fields, arrays, nullable values, and generic arguments.
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

fn source(body: &str) -> String {
    format!("@!include(\"std\") use io; use bytes; use err::Error;\n{body}")
}

fn compile(body: &str) -> cog::backend::CompiledProgram {
    let program = project::compile_source("bytes_annotations.cog", &source(body))
        .unwrap_or_else(|error| panic!("{error}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn execute(program: &cog::backend::CompiledProgram, jit: bool) -> String {
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.configure_jit(jit, None, false);
    interpreter::run_with_context(program, &mut context).unwrap_or_else(|error| panic!("{error}"));
    drop(context);
    String::from_utf8(output).unwrap()
}

/// Runs the program through bytecode encode/decode on the VM and with the JIT
/// enabled, and requires both backends to agree.
fn run(body: &str) -> String {
    let program = compile(body);
    let vm = execute(&program, false);
    let jit = execute(&program, true);
    assert_eq!(jit, vm, "JIT and VM output differ");
    vm
}

fn rejects(body: &str, expected: &str) {
    let error = project::compile_source("bytes_annotations.cog", &source(body))
        .expect_err("the program must be rejected")
        .to_string();
    assert!(
        error.contains(expected),
        "expected `{expected}` in:\n{error}"
    );
}

#[test]
fn bytes_values_flow_through_annotated_struct_fields_parameters_and_results() {
    assert_eq!(
        run(r#"
        struct Buffer { data: bytes, label: str = "buffer" }

        fn make(text: str) -> Buffer {
            Buffer(data: bytes::from_str(text))
        }

        fn fill(target: bytes, value: int) -> nil {
            for index in 0..bytes::len(target)!! {
                bytes::set(target, index, value)!!;
            }
        }

        fn head(source: bytes) -> int {
            bytes::get(source, 0)!!
        }

        fn copy_of(source: bytes) -> bytes {
            bytes::copy(source)
        }

        fn main() {
            var buffer = make("abc");
            const alias: bytes = buffer.data;
            io::println(head(buffer.data))!!;
            fill(buffer.data, 90);
            io::println(bytes::to_str(alias, 0, bytes::len(alias)!!)!!)!!;
            var copied = copy_of(alias);
            bytes::set(copied, 0, 65)!!;
            io::println(bytes::to_str(copied, 0, 3)!!)!!;
            io::println(bytes::to_str(buffer.data, 0, 3)!!)!!;
            buffer.data = bytes::from_str("new");
            io::println(bytes::to_str(buffer.data, 0, 3)!!)!!;
            io::println(buffer.label)!!;
        }
        "#),
        "97\nZZZ\nAZZ\nZZZ\nnew\nbuffer\n"
    );
}

#[test]
fn bytes_annotations_compose_with_arrays_nullable_defaults_and_generics() {
    assert_eq!(
        run(r#"
        struct Packet { payload: bytes = bytes::new(2), trailer: bytes? = nil }
        struct Box[T: type] { value: T }

        fn total(buffers: bytes[]) -> int {
            var sum = 0;
            for buffer in buffers { sum += bytes::len(buffer)!!; }
            sum
        }

        fn size(buffer: bytes?) -> int {
            if buffer == nil { return -1; }
            bytes::len(buffer)!!
        }

        fn identity[T: type](value: T) -> T { value }

        fn main() {
            var buffers: bytes[] = [];
            buffers << bytes::new(3);
            buffers << bytes::from_str("hello");
            io::println(total(buffers))!!;
            io::println(size(nil))!!;
            io::println(size(bytes::new(4)))!!;
            var first = Packet();
            const second = Packet();
            bytes::set(first.payload, 0, 7)!!;
            io::println(bytes::get(second.payload, 0)!!)!!;
            io::println(first.trailer == nil)!!;
            const boxed = Box(bytes::from_str("xy"));
            io::println(bytes::len(boxed.value)!!)!!;
            const same: bytes = identity(boxed.value);
            io::println(bytes::to_str(same, 0, 2)!!)!!;
            const pair: (bytes, int) = (bytes::new(1), 1);
            io::println(bytes::len(pair.0)!!)!!;
        }
        "#),
        "8\n-1\n4\n0\ntrue\n2\nxy\n1\n"
    );
}

#[test]
fn bytes_annotations_reject_other_types_statically_and_at_runtime() {
    rejects(
        "fn head(source: bytes) -> int { bytes::get(source, 0)!! } fn main() { head(\"text\"); }",
        "declared as bytes but given str",
    );
    rejects(
        "struct Buffer { data: bytes } fn main() { const buffer = Buffer(data: [1, 2]); }",
        "declared as bytes but given int[]",
    );
    rejects(
        "fn text() -> bytes { \"text\" } fn main() { text(); }",
        "type mismatch",
    );
    rejects(
        "fn main() { var buffers: bytes[] = []; buffers << \"text\"; }",
        "type mismatch",
    );
    rejects(
        "fn kind(value) -> str { when value is bytes { return \"bytes\"; } \"other\" } \
         fn main() { kind(bytes::new(1)); }",
        "type patterns do not support `bytes`",
    );
    assert_eq!(
        run(r#"
        struct Buffer { data: bytes }

        fn overwrite(item, replacement) {
            try { item.data = replacement; }
            catch(error) { io::println(error.error_kind())!!; }
        }

        fn append(items, value) {
            try { items << value; }
            catch(error) { io::println(error.error_kind())!!; }
        }

        fn main() {
            var buffer = Buffer(data: bytes::from_str("ok"));
            overwrite(buffer, "wrong");
            overwrite(buffer, io::stdout());
            io::println(bytes::to_str(buffer.data, 0, 2)!!)!!;
            var buffers: bytes[] = [bytes::new(1)];
            append(buffers, "wrong");
            append(buffers, io::stdout());
            append(buffers, bytes::new(2));
            io::println(len(buffers))!!;
        }
        "#),
        "TypeMismatch\nTypeMismatch\nok\nTypeMismatch\nTypeMismatch\n2\n"
    );
}

#[test]
fn bytes_parameters_mutated_by_callees_require_writable_arguments() {
    rejects(
        "fn fill(target: bytes) -> nil { bytes::set(target, 0, 1)!!; } \
         fn main() { const buffer = bytes::new(1); fill(buffer); }",
        "cannot mutate",
    );
    rejects(
        "struct Buffer { data: bytes } \
         fn fill(target: bytes) -> nil { bytes::set(target, 0, 1)!!; } \
         fn main() { const buffer = Buffer(bytes::new(1)); fill(buffer.data); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn fill(target: bytes) -> nil { bytes::set(target, 0, 66)!!; } \
             fn main() { var buffer = bytes::new(1); fill(buffer); io::println(bytes::get(buffer, 0)!!)!!; }"
        ),
        "66\n"
    );
}

#[test]
fn bytes_type_name_coexists_with_the_bytes_module_and_local_names() {
    assert_eq!(
        run(r#"
        fn wrap(bytes: bytes) -> bytes { bytes }

        fn main() {
            const data: bytes = wrap(bytes::from_str("ok"));
            io::println(bytes::len(data)!!)!!;
        }
        "#),
        "2\n"
    );
}
