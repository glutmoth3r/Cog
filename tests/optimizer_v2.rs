use cog::instruction::Instruction;
use cog::{bytecode, project, sdk};

fn compile(body: &str) -> cog::backend::CompiledProgram {
    project::compile_source(
        "optimizer.cog",
        &format!("@!include(\"std\")\nuse io;\n{body}"),
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn execute(program: &cog::backend::CompiledProgram, jit: bool) -> Vec<u8> {
    let bytes = bytecode::encode(program).unwrap();
    let program = sdk::Engine::new().decode(&bytes).unwrap();
    let mut output = Vec::new();
    let mut context = sdk::RunContext::new(&mut output).with_jit(jit);
    program.run_in(&mut context).unwrap();
    drop(context);
    output
}

fn function<'a>(
    program: &'a cog::backend::CompiledProgram,
    name: &str,
) -> (usize, &'a [Instruction]) {
    let (&start, _) = program.functions.iter().find(|(_, n)| *n == name).unwrap();
    let end = program
        .callables
        .range(start + 1..)
        .next()
        .map_or(program.instructions.len(), |(&entry, _)| entry);
    (start, &program.instructions[start..end])
}

#[test]
fn inlined_branch_keeps_error_values_distinct_from_propagation() {
    let source = r#"
        fn step(x: int) -> int {
            if x >= 0 {
                const shifted = x + 2;
                shifted * 3
            } else {
                const shifted = x - 2;
                shifted * 5
            }
        }
        @inline("never") fn input(i: int) -> int {
            if i == 3 { 9223372036854775807 } else { i - 2 }
        }
        fn work() {
            defer { io::println(99); }
            for i in 0..4 {
                try { io::println(step(input(i)!!)); }
                catch (error) { io::println(error.error_kind()); }
            }
        }
        fn main() { work(); }
    "#;
    let optimized = compile(source);
    let reference = compile(&source.replace("fn step", "@inline(\"never\") fn step"));
    let (entry, _) = function(&optimized, "step");
    let (_, code) = function(&optimized, "work");
    assert!(
        !code
            .iter()
            .any(|op| matches!(op, Instruction::Call(target, _) if *target == entry))
    );
    let expected = execute(&reference, false);
    assert_eq!(expected, b"-20\n-15\n6\nOverflow\n99\n");
    for jit in [false, true] {
        assert_eq!(execute(&optimized, jit), expected);
        assert_eq!(execute(&reference, jit), expected);
    }
}

#[test]
fn names_specialize_only_when_all_paths_agree() {
    let program = compile(
        r#"
        var seen = 0;
        @inline("never") fn add(x: int) -> int {
            seen += 1;
            x + 1
        }
        @inline("never") fn sub(x: int) -> int {
            seen += 10;
            x - 1
        }
        fn stable(n: int) -> int {
            const target = add;
            var total = 0;
            for i in 0..n { total += (target)(i)!!; }
            total
        }
        fn changing(n: int) -> int {
            var target = add;
            var total = 0;
            for i in 0..n {
                total += (target)(i)!!;
                target = sub;
            }
            total
        }
        fn main() {
            io::println(stable(5)!!)!!;
            io::println(changing(5)!!)!!;
            io::println(seen)!!;
        }
        "#,
    );
    let (entry, _) = function(&program, "add");
    let (_, stable) = function(&program, "stable");
    let (_, changing) = function(&program, "changing");
    assert!(
        stable
            .iter()
            .any(|op| matches!(op, Instruction::Call(target, 1) if *target == entry))
    );
    assert!(
        !stable
            .iter()
            .any(|op| matches!(op, Instruction::CallValue(_)))
    );
    assert!(
        changing
            .iter()
            .any(|op| matches!(op, Instruction::CallValue(1)))
    );
    for jit in [false, true] {
        assert_eq!(execute(&program, jit), b"15\n7\n46\n");
    }
}

#[test]
fn a_specialized_recursive_call_keeps_the_frame_limit() {
    let body = r#"
        @inline("never") fn descend(n: int) -> int {
            if n == 0 { 0 } else { RECURSE }
        }
        fn main() { io::println(descend(30)!!)!!; }
    "#;
    let reference = body.replace("RECURSE", "descend(n - 1)!!");
    // The block introduces a stable named value without changing recursion.
    let specialized = body.replace("RECURSE", "const next = descend; (next)(n - 1)!!");
    let limits = sdk::Limits {
        max_call_frames: Some(8),
        ..sdk::Limits::default()
    };
    let mut errors = Vec::new();
    for source in [reference, specialized] {
        let artifact = compile(&source);
        let (_, code) = function(&artifact, "descend");
        assert!(
            !code
                .iter()
                .any(|op| matches!(op, Instruction::CallValue(_)))
        );
        for jit in [false, true] {
            let engine = sdk::Engine::new();
            let program = engine
                .decode(&bytecode::encode(&artifact).unwrap())
                .unwrap();
            let mut output = Vec::new();
            let error = program
                .run_in(
                    &mut sdk::RunContext::new(&mut output)
                        .with_jit(jit)
                        .with_limits(limits),
                )
                .unwrap_err();
            assert!(error.message().contains("call stack"), "{error}");
            assert!(output.is_empty());
            errors.push(error.kind());
        }
    }
    assert!(errors.windows(2).all(|pair| pair[0] == pair[1]));
}
