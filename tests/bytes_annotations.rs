//! `bytes` as a first-class annotation type for parameters, results, locals,
//! struct fields, arrays, nullable values, and generic arguments.
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

fn source(body: &str) -> String {
    format!("@!include(\"std\") use io; use bytes; use err::Error;\n{body}")
}

/// Source that includes the standard library without importing `bytes`.
fn unimported(body: &str) -> String {
    format!("@!include(\"std\") use io;\n{body}")
}

fn round_trip(program: cog::backend::CompiledProgram) -> cog::backend::CompiledProgram {
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn compile(text: &str) -> cog::backend::CompiledProgram {
    let program = project::compile_source("bytes_annotations.cog", text)
        .unwrap_or_else(|error| panic!("{error}"));
    round_trip(program)
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
    run_source(&source(body))
}

fn run_source(text: &str) -> String {
    run_program(compile(text))
}

fn run_program(program: cog::backend::CompiledProgram) -> String {
    let vm = execute(&program, false);
    let jit = execute(&program, true);
    assert_eq!(jit, vm, "JIT and VM output differ");
    vm
}

fn rejects(body: &str, expected: &str) {
    rejects_source(&source(body), expected);
}

fn rejects_source(text: &str, expected: &str) {
    let error = project::compile_source("bytes_annotations.cog", text)
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

#[test]
fn user_struct_named_bytes_takes_precedence_over_the_builtin_type() {
    assert_eq!(
        run_source(&unimported(
            r#"
        struct bytes { x: int }
        struct Holder { item: bytes, rest: bytes[] }
        struct Box[T: type] { value: T }

        fn f(b: bytes) -> int { b.x }
        fn pass(b: bytes?) -> bytes? { b }
        fn id[T: type](v: T) -> T { v }

        fn main() {
            io::println(f(bytes(x: 3)))!!;
            io::println(pass(nil) == nil)!!;
            const held = Holder(bytes(x: 4), [bytes(x: 5)]);
            io::println(f(held.item) + f(held.rest[0]))!!;
            io::println(f(id[bytes](bytes(x: 6))))!!;
            const boxed: Box[bytes] = Box[bytes](bytes(x: 7));
            io::println(f(boxed.value))!!;
            io::println(match held.item { bytes => "struct", _ => "other" })!!;
        }
        "#
        )),
        "3\ntrue\n9\n6\n7\nstruct\n"
    );
    // The module import does not hide the declaration from annotations.
    rejects(
        "struct bytes { x: int } fn f(b: bytes) -> int { b.x } \
         fn main() { f(bytes::new(1)); }",
        "declared as bytes but given bytes",
    );
}

#[test]
fn user_enum_named_bytes_takes_precedence_over_the_builtin_type() {
    assert_eq!(
        run_source(&unimported(
            r#"
        enum bytes { Small(size: int), Large }

        fn size(b: bytes) -> int {
            match b { bytes::Small(n) => n, bytes::Large => 100 }
        }

        fn main() {
            io::println(size(bytes::Small(2)))!!;
            io::println(size(bytes::Large()))!!;
        }
        "#
        )),
        "2\n100\n"
    );
}

#[test]
fn comptime_type_alias_named_bytes_takes_precedence_over_the_builtin_type() {
    assert_eq!(
        run("@comptime const bytes: type = int; \
             fn next(b: bytes) -> bytes { b + 1 } \
             fn main() { io::println(next(1))!!; }"),
        "2\n"
    );
}

#[test]
fn generic_type_parameter_named_bytes_takes_precedence_over_the_builtin_type() {
    for imports in ["", "use bytes;"] {
        assert_eq!(
            run_source(&format!(
                "@!include(\"std\") use io; {imports}\n{}",
                r#"
        struct Box[bytes: type] { value: bytes }

        fn id[bytes: type](v: bytes) -> bytes {
            const copy: bytes = v;
            copy
        }

        fn forward[bytes: type](v: bytes) -> bytes { id[bytes](v) }

        fn main() {
            io::println(id(3))!!;
            io::println(forward("text"))!!;
            io::println(Box(true).value)!!;
        }
        "#
            )),
            "3\ntext\ntrue\n"
        );
    }
}

#[test]
fn builtin_bytes_type_works_with_and_without_the_module_import() {
    let body = r#"
        fn same(b: bytes) -> bytes { b }
        fn id[T: type](v: T) -> T { v }

        fn main() {
            var all: bytes[] = [];
            all << id[bytes](same(buffers::from_str("abc")));
            io::println(buffers::len(all[0])!!)!!;
        }
        "#;
    for (imports, module) in [("use bytes;", "bytes"), ("use bytes as raw;", "raw")] {
        assert_eq!(
            run_source(&format!(
                "@!include(\"std\") use io; {imports}\n{}",
                body.replace("buffers::", &format!("{module}::"))
            )),
            "3\n"
        );
    }
    assert_eq!(
        run_source(&unimported(
            "fn size(b: bytes?) -> int { if b == nil { return 0; } 1 } \
             fn id[T: type](v: T) -> T { v } \
             fn main() { const none: bytes? = nil; io::println(size(none))!!; }"
        )),
        "0\n"
    );
    // A function named `bytes` is not a type and leaves annotations alone.
    assert_eq!(
        run_source(&unimported(
            "use entropy::bytes; fn size(b: bytes?) -> int { 4 } \
             fn main() { io::println(size(nil))!!; }"
        )),
        "4\n"
    );
}

#[test]
fn imported_user_type_named_bytes_takes_precedence_over_the_builtin_type() {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("bytes-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("shapes.cog"),
        "public struct bytes { x: int } public struct Frame { y: int }",
    )
    .unwrap();
    for (import, name) in [
        ("use shapes::bytes;", "bytes"),
        ("use shapes::*;", "bytes"),
        ("use shapes::Frame as bytes;", "Frame"),
    ] {
        let field = if name == "Frame" { "y" } else { "x" };
        std::fs::write(
            root.join("main.cog"),
            format!(
                "@!include(\"std\")\nuse io; {import}\n\
                 fn f(b: bytes) -> int {{ b.{field} }} \
                 fn id[T: type](v: T) -> T {{ v }} \
                 fn main() {{ io::println(f(id[bytes](bytes({field}: 8))))!!; }}"
            ),
        )
        .unwrap();
        let loaded = project::load(&root.join("main.cog"), None, false)
            .unwrap_or_else(|errors| panic!("{}", cog::frontend::render(&errors)));
        let program = project::compile(loaded.program, loaded.diagnostics, &loaded.names)
            .unwrap_or_else(|error| panic!("{import}: {error}"));
        assert_eq!(run_program(round_trip(program)), "8\n", "{import}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn bytes_type_tests_narrow_dynamic_values() {
    assert_eq!(
        run(r#"
        struct Point { x: int }

        fn size(data: bytes) -> int { bytes::len(data)!! }

        fn measure(value) -> int {
            when value is bytes { size(value) } else { -1 }
        }

        fn describe(value) -> str {
            match value {
                bytes if size(value) > 2 => "long",
                bytes => "short",
                str => "str",
                _ => "other",
            }
        }

        fn fill(value) -> nil {
            when value is bytes { bytes::set(value, 0, 65)!!; }
        }

        fn choose(value) -> bytes {
            when value is str { bytes::from_str(value) }
            else if true { when value is bytes { value } else { bytes::new(0) } }
            else { bytes::new(0) }
        }

        fn is_bytes(value) -> bool { when value is bytes { true } else { false } }

        fn main() {
            var data = bytes::new(3);
            fill(data);
            fill("ignored");
            io::println(bytes::get(data, 0)!!)!!;
            io::println(measure(data))!!;
            io::println(measure("abc"))!!;
            io::println(describe(data))!!;
            io::println(describe(bytes::new(1)))!!;
            io::println(describe("abc"))!!;
            io::println(describe(1))!!;
            io::println(size(choose("four")))!!;
            io::println(size(choose(data)))!!;
            io::println(is_bytes(nil))!!;
            io::println(is_bytes(1 / 0))!!;
            io::println(is_bytes(io::stdout()))!!;
            io::println(is_bytes([data]))!!;
            io::println(is_bytes(Point(x: 1)))!!;
            io::println(is_bytes(bytes::copy(data)))!!;
        }
        "#),
        "65\n3\n-1\nlong\nshort\nstr\nother\n4\n3\nfalse\nfalse\nfalse\nfalse\nfalse\ntrue\n"
    );
}

#[test]
fn bytes_type_tests_require_writable_arguments_for_narrowed_mutation() {
    rejects(
        "fn fill(value) -> nil { when value is bytes { bytes::set(value, 0, 1)!!; } } \
         fn main() { const buffer = bytes::new(1); fill(buffer); }",
        "cannot mutate",
    );
}

#[test]
fn bytes_type_tests_evaluate_the_scrutinee_once_and_ignore_value_names() {
    assert_eq!(
        run(r#"
        var evaluations = 0;
        fn buffer() -> bytes { evaluations += 1; bytes::new(1) }
        fn main() {
            const bytes = 7;
            when buffer() is bytes { io::println(bytes)!!; }
            io::println(evaluations)!!;
        }
        "#),
        "7\n1\n"
    );
    // A function named `bytes` is not a type, so the pattern stays built in.
    assert_eq!(
        run_source(&unimported(
            "use entropy::bytes; \
             fn kind(value) -> str { when value is bytes { \"bytes\" } else { \"other\" } } \
             fn main() { io::println(kind(bytes(2)!!))!!; io::println(kind(2))!!; }"
        )),
        "bytes\nother\n"
    );
}

#[test]
fn user_types_named_bytes_take_precedence_in_type_tests() {
    assert_eq!(
        run_source(&unimported(
            r#"
        struct bytes { x: int }
        fn kind(value) -> str { when value is bytes { "struct" } else { "other" } }
        fn covered(value: bytes) -> int { match value { bytes => value.x } }
        fn main() {
            io::println(kind(bytes(x: 1)))!!;
            io::println(kind(1))!!;
            io::println(covered(bytes(x: 2)))!!;
        }
        "#
        )),
        "struct\nother\n2\n"
    );
}

#[test]
fn compile_time_bytes_type_tests_match_runtime_identity() {
    assert_eq!(
        run(r#"
        @comptime fn is_buffer(value: str) -> bool { when value is bytes { true } else { false } }
        @comptime const NO: bool = is_buffer("text");
        fn main() { io::println(NO)!!; }
        "#),
        "false\n"
    );
}

#[test]
fn array_contracts_keep_byte_buffers_and_handles_distinct() {
    rejects(
        "fn main() { var handles = [io::stdout()]; handles << bytes::new(1); }",
        "type mismatch",
    );
    rejects(
        "fn main() { var buffers = [bytes::new(1)]; buffers << io::stdout(); }",
        "type mismatch",
    );
    assert_eq!(
        run(r#"
        fn append(items, value) {
            try { items << value; }
            catch(error) { io::println(error.error_kind())!!; }
        }

        fn replace(items, value) {
            try { items[0] = value; }
            catch(error) { io::println(error.error_kind())!!; }
        }

        fn main() {
            var handles = [io::stdout()];
            append(handles, bytes::new(1));
            replace(handles, bytes::new(1));
            append(handles, io::stdout());
            io::println(len(handles))!!;
            var buffers = [bytes::new(1)];
            append(buffers, io::stdout());
            replace(buffers, io::stdout());
            append(buffers, bytes::new(2));
            io::println(len(buffers))!!;
        }
        "#),
        "TypeMismatch\nTypeMismatch\n2\nTypeMismatch\nTypeMismatch\n2\n"
    );
}
