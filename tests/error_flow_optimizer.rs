use cog::{
    bytecode,
    instruction::{Instruction, ValueType},
    project, sdk,
};

fn compile(source: &str) -> cog::backend::CompiledProgram {
    project::compile_source(
        "error_flow.cog",
        &format!("@!include(\"std\")\nuse io;\n{source}"),
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn execute(program: &cog::backend::CompiledProgram, jit: bool) -> String {
    let program = sdk::Engine::new()
        .decode(&bytecode::encode(program).unwrap())
        .unwrap();
    let mut output = Vec::new();
    let mut context = sdk::RunContext::new(&mut output).with_jit(jit);
    program.run_in(&mut context).unwrap();
    drop(context);
    String::from_utf8(output).unwrap()
}

#[test]
fn successful_error_checks_prove_later_reads_without_hiding_overflow() {
    let source = r#"
    @inline("never") fn opaque(value: int) -> int { value }
    fn work(value: int) {
        defer { io::println("cleanup")!!; }
        try {
            const first = INPUT;
            io::println(first)!!;
            const second = INPUT;
            io::println(second)!!;
            const incremented = second + 1;
            io::println(incremented)!!;
        } catch(error) { io::println(error.error_kind())!!; }
    }
    fn main() { work(7); work(9223372036854775807); work(1 / 0); }
    "#;
    let optimized = compile(&source.replace("INPUT", "value"));
    let reference = compile(&source.replace("INPUT", "opaque(value)"));
    for jit in [false, true] {
        assert_eq!(
            execute(&optimized, jit),
            "7\n7\n8\ncleanup\n9223372036854775807\n9223372036854775807\nOverflow\ncleanup\nDivisionByZero\ncleanup\n"
        );
        assert_eq!(execute(&optimized, jit), execute(&reference, jit));
    }
    let checks = |program: &cog::backend::CompiledProgram| {
        program
            .instructions
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::IsType(ValueType::Error)))
            .count()
    };
    assert!(
        checks(&optimized) < checks(&reference),
        "{}",
        optimized.disassemble()
    );
}

#[test]
fn non_error_facts_join_without_becoming_equal_constants_or_erasing_effects() {
    let program = compile(
        r#"
    var visits: int = 0;
    fn effect() -> int { visits += 1; visits }
    fn work(flag: bool, input: int) {
        try {
            var value = if flag { 3 } else { 8 };
            value = if flag { 4 } else { 9 };
            io::println(value)!!;
            io::println(input)!!;
            const saved = input;
            io::println(effect())!!;
            io::println(saved)!!;
        } catch(error) { io::println(error.error_kind())!!; }
    }
    fn main() { work(true, 5); work(false, 6); work(true, 1 / 0); }
    "#,
    );
    for jit in [false, true] {
        assert_eq!(
            execute(&program, jit),
            "4\n5\n1\n5\n9\n6\n2\n6\n4\nError: division by zero\nDivisionByZero\n"
        );
    }
}

#[test]
fn checked_loop_locals_keep_overflow_checks_and_result_values() {
    let program = compile(
        r#"
    fn sum(n: int) -> int {
        try {
            var total = 0;
            var i = 0;
            while i < n { total += i; i += 1; }
            total
        } catch(error) { -1 }
    }
    fn main() { io::println(sum(10000))!!; io::println(sum(1 / 0))!!; }
    "#,
    );
    for jit in [false, true] {
        assert_eq!(execute(&program, jit), "49995000\n-1\n");
    }
}

/// Losing track of stack facts must only keep the affected checks, not make the
/// emitted program fail verification and fall back to unoptimized code.
#[test]
fn error_checks_survive_the_tracked_stack_value_limit() {
    let checks = |literals: usize| {
        let items = vec!["\"x\""; literals].join(", ");
        let program = compile(&format!(
            r#"@inline("never") fn work(s: str) {{
                try {{
                    const values = [s, {items}, s];
                    io::println(values.len())!!;
                }} catch(error) {{ io::println("caught")!!; }}
            }}
            fn untouched(n: int) -> int {{ const copy = n + 1; copy + copy }}
            fn main() {{ work("abc"); io::println(untouched(1))!!; }}"#
        ));
        assert_eq!(execute(&program, false), format!("{}\n4\n", literals + 2));
        program
            .instructions
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::IsType(ValueType::Error)))
            .count()
    };
    let below = checks(200);
    for literals in 250..260 {
        assert!(
            checks(literals) <= below + 2,
            "{literals} literals disabled optimization"
        );
    }
}
