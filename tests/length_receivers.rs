//! `x.len()` and `len(x)` are rejected for statically known receivers that have
//! no built-in length.
use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> Result<cog::backend::CompiledProgram, String> {
    project::compile_source(
        "length.cog",
        &format!("@!include(\"std\")\nuse io;\n{source}"),
    )
}

fn run(source: &str) -> String {
    let program = compile(source).unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn both_length_forms_reject_types_without_a_built_in_length() {
    let declarations = "
        use str::builder;
        struct Point { x: int }
        enum Mode { On, Level(value: int) }
        fn number() -> int { 1 }
        @async fn later() -> int { 1 }
    ";
    for (value, ty) in [
        ("1", "int"),
        ("1.5", "float"),
        ("true", "bool"),
        ("'a'", "char"),
        ("Point(1)", "Point"),
        ("Mode::Level(1)", "Mode"),
        ("{ a: 1 }", "record"),
        ("fn() -> int { 1 }", "fn() -> int"),
        ("later()", "Task[int]"),
        ("builder()!!", "string builder"),
    ] {
        for (call, operation) in [("value.len()", "len()"), ("len(value)", "len")] {
            let source = format!("{declarations} fn main() {{ const value = {value}; {call}; }}");
            let error = compile(&source).expect_err(&source);
            assert!(
                error.contains(&format!("`{operation}` does not accept `{ty}`")),
                "{source}: {error}"
            );
            assert!(
                error.contains("only arrays, tuples, and strings have a built-in length"),
                "{source}: {error}"
            );
        }
    }
}

#[test]
fn length_of_a_known_type_is_rejected_in_fields_and_generic_instances() {
    for source in [
        "struct Holder { count: int } fn size(h: Holder) -> int { h.count.len() } fn main() {}",
        "fn size[T: type](value: T) -> int { len(value) } fn main() { size(1); }",
        "fn size[T: type](value: T) -> int { value.len() } fn main() { size(true); }",
        "fn size(value: int?) -> int { if value != nil { return len(value); } 0 } fn main() {}",
    ] {
        let error = compile(source).expect_err(source);
        assert!(error.contains("does not accept `"), "{source}: {error}");
    }
}

#[test]
fn the_built_in_length_of_a_type_with_its_own_method_suggests_the_method() {
    let error = compile(
        "use collections::Map;
         fn main() { const map: Map[str, int] = Map::new(); len(map); }",
    )
    .unwrap_err();
    assert!(error.contains("`len` does not accept `Map"), "{error}");
    assert!(
        error.contains("call the type's own method as `value.len()`"),
        "{error}"
    );
    assert!(!error.contains("$symbol"), "{error}");
}

#[test]
fn supported_user_defined_and_dynamic_lengths_still_work() {
    let output = run(r#"
        use collections::Map;
        use err::Error;
        struct Ring { size: int }
        extend Ring { fn len(self) -> int { self.size } }
        fn dynamic(value) { const size = len(value); when size is Error { -1 } else { size } }
        fn method(value) { const size = value.len(); when size is Error { -1 } else { size } }
        fn main() {
            const map: Map[str, int] = Map::new();
            const empty: int[] = [];
            io::println([1, 2].len() + len(empty))!!;
            io::println(len("héllo") + "ab".len())!!;
            io::println(len((1, "two", 3)) + (1, 2).len())!!;
            io::println(map.len())!!;
            io::println(Ring(9).len())!!;
            io::println(dynamic(1) + method(true) + dynamic([1]) + method("abc"))!!;
        }
        "#);
    assert_eq!(output, "2\n7\n5\n0\n9\n2\n");
}
