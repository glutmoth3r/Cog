use cog::{bytecode, interpreter, project};

fn run(body: &str) -> String {
    let source = format!("@!include(\"std\")\nuse io;\n{body}");
    let program = project::compile_source("range_shadowing.cog", &source).unwrap();
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn bigint_range_bound_uses_the_narrowed_outer_binding() {
    assert_eq!(
        run(r#"
        fn main() {
            const limit: bigint? = 3 as bigint;
            if limit != nil {
                for limit in 0..limit { io::println(limit); }
            }
        }
        "#),
        "0\n1\n2\n"
    );
}

#[test]
fn range_header_uses_outer_step_and_evaluates_operands_once_in_order() {
    assert_eq!(
        run(r#"
        fn trace(label: str, value: int) -> int {
            io::println(label);
            value
        }
        fn main() {
            const i = 2;
            for i in trace("start", 1)..trace("end", 6) step trace("step", i) {
                io::println(i);
            }
        }
        "#),
        "start\nend\nstep\n1\n3\n5\n"
    );
}
