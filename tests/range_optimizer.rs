use cog::{bytecode, instruction::Instruction, project, sdk};

fn compile(body: &str) -> cog::backend::CompiledProgram {
    project::compile_source(
        "range_optimizer.cog",
        &format!("@!include(\"std\")\nuse io;\n{body}"),
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn execute(program: &cog::backend::CompiledProgram, jit: bool) -> String {
    let bytes = bytecode::encode(program).unwrap();
    let program = sdk::Engine::new().decode(&bytes).unwrap();
    let mut output = Vec::new();
    let mut context = sdk::RunContext::new(&mut output).with_jit(jit);
    program.run_in(&mut context).unwrap();
    drop(context);
    String::from_utf8(output).unwrap()
}

fn compare_steps(
    source: &str,
    expected: &str,
) -> (cog::backend::CompiledProgram, cog::backend::CompiledProgram) {
    let constant = compile(&source.replace("STEP", "2").replace("BACK", "-2"));
    let dynamic = compile(
        &source
            .replace("STEP", "opaque(2)")
            .replace("BACK", "opaque(-2)"),
    );
    for jit in [false, true] {
        assert_eq!(execute(&constant, jit), expected, "constant jit={jit}");
        assert_eq!(execute(&dynamic, jit), expected, "dynamic jit={jit}");
    }
    (constant, dynamic)
}

#[test]
fn constant_steps_keep_break_continue_and_defer_behavior() {
    let (constant, dynamic) = compare_steps(
        r#"
        @inline("never") fn opaque(value: int) -> int { value }
        fn work(limit: int) {
            var total = 0;
            for i in 0..limit step STEP {
                defer { io::println(i)!!; }
                if i == 2 { continue; }
                total += i;
                if i == 4 { break; }
            }
            io::println(total)!!;
            for i in limit..0 step BACK { io::println(i)!!; }
            for i in 0..limit step STEP, j in limit..0 step BACK {
                io::println($"{i}:{j}")!!;
            }
        }
        fn main() { work(6); }
        "#,
        "0\n2\n4\n4\n6\n4\n2\n0:6\n2:4\n4:2\n",
    );
    let direction_comparisons = |program: &cog::backend::CompiledProgram| {
        program
            .instructions
            .iter()
            .filter(|op| matches!(op, Instruction::Lt | Instruction::Gt))
            .count()
    };
    assert!(direction_comparisons(&constant) < direction_comparisons(&dynamic));
}

#[test]
fn constant_steps_preserve_header_errors_overflow_and_cleanup() {
    compare_steps(
        r#"
        @inline("never") fn opaque(value: int) -> int { value }
        fn bound(label: str, value: int) -> int { io::println(label)!!; value }
        fn failing() -> int { 1 / 0 }
        fn run(limit: int) {
            defer { io::println("cleanup")!!; }
            try {
                for i in bound("start", 9223372036854775806)..bound("end", limit) step STEP {
                    io::println("once")!!;
                }
            } catch(error) { io::println(error.error_kind())!!; }
            try { for i in failing()..limit step STEP { io::println("wrong")!!; } }
            catch(error) { io::println(error.error_kind())!!; }
            try {
                for i in 0..limit step 1 { i = limit; }
            } catch(error) { io::println(error.error_kind())!!; }
            try { for i in 0..limit step 0 { io::println("wrong")!!; } }
            catch(error) { io::println(error.error_kind())!!; }
        }
        fn main() { run(9223372036854775807); }
        "#,
        "start\nend\nonce\nOverflow\nDivisionByZero\nOverflow\nInvalidRangeStep\ncleanup\n",
    );
}

#[test]
fn constant_steps_cover_default_unsigned_and_bigint_ranges() {
    let program = compile(
        r#"
        fn work(limit: uint, big: bigint) {
            for i in 0..limit { io::println(i)!!; }
            for i in 0..limit step 2 { io::println(i)!!; }
            for i in big..big - 5 step -2 { io::println(i)!!; }
            const jump = 2;
            for i in 0..5 step jump { io::println(i)!!; }
        }
        fn main() { work(3, 18446744073709551618); }
        "#,
    );
    for jit in [false, true] {
        assert_eq!(
            execute(&program, jit),
            "0\n1\n2\n0\n2\n18446744073709551618\n18446744073709551616\n18446744073709551614\n0\n2\n4\n"
        );
    }
}
