use cog::{bytecode, project, sdk};

#[test]
fn string_key_maps_serialize_as_json_objects_and_other_maps_fail() {
    let source = r#"
        @!include("std")
        use io;
        use json;
        use collections;
        struct Map { value: int }
        fn main() {
            io::println(json::stringify(Map(value: 5))!!)!!;
            var values = collections::Map::new[str, int]();
            values.set("first", 1)!!;
            values.set("second", 2)!!;
            values.set("first", 3)!!;
            io::println(json::stringify(values)!!)!!;
            io::println(json::stringify([values])!!)!!;
            values.remove("first")!!;
            io::println(json::stringify(values)!!)!!;
            values.set("bad", 1 / 0)!!;
            io::println(json::stringify(values).error_kind())!!;
            var numbers = collections::Map::new[int, str]();
            io::println(json::stringify(numbers).error_kind())!!;
            numbers.set(1, "one")!!;
            io::println(json::stringify(numbers).error_kind())!!;
        }
    "#;
    let program = project::compile_source("json-map.cog", source).unwrap();
    let artifact = bytecode::encode(&program).unwrap();
    for jit in [false, true] {
        let program = sdk::Engine::new().decode(&artifact).unwrap();
        let mut out = Vec::new();
        let mut context = sdk::RunContext::new(&mut out).with_jit(jit);
        program.run_in(&mut context).unwrap();
        drop(context);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"value\":5}\n{\"first\":3,\"second\":2}\n[{\"first\":3,\"second\":2}]\n{\"second\":2}\nDivisionByZero\nTypeMismatch\nTypeMismatch\n"
        );
    }
}
