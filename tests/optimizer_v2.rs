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
    let (&start, _) = program
        .functions
        .iter()
        .find(|(_, n)| *n == name || n.ends_with(&format!("::{name}")))
        .unwrap();
    let end = program
        .callables
        .range(start + 1..)
        .next()
        .map_or(program.instructions.len(), |(&entry, _)| entry);
    (start, &program.instructions[start..end])
}

#[test]
fn scalar_assignment_preserves_constants_through_array_context() {
    let program = compile(
        r#"
        fn work() {
            var value = 2;
            value = 3;
            if value == 3 { io::println("correct")!!; }
            else { io::println("unreachable")!!; }
        }
        fn main() { work(); }
        "#,
    );
    let (_, code) = function(&program, "work");
    assert!(!code.iter().any(|op| matches!(op, Instruction::Push(cog::value::Value::Str(s)) if s.to_string() == "unreachable")));
    for jit in [false, true] {
        assert_eq!(execute(&program, jit), b"correct\n");
    }
}

#[test]
fn discarded_scalar_literals_do_not_leave_stack_operations() {
    let program = compile(
        r#"
        @inline("never") fn work(flag: bool) {
            if flag { 123456; } else { 654321; }
            io::println("done")!!;
        }
        fn main() { work(true); work(false); }
        "#,
    );
    let (_, code) = function(&program, "work");
    assert!(!code.iter().any(|op| matches!(
        op,
        Instruction::Push(cog::value::Value::Int(123456 | 654321))
    )));
    for jit in [false, true] {
        assert_eq!(execute(&program, jit), b"done\ndone\n");
    }
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

fn calls_helper(program: &cog::backend::CompiledProgram, code: &[Instruction], name: &str) -> bool {
    let (entry, _) = function(program, name);
    code.iter()
        .any(|op| matches!(op, Instruction::Call(target, _) if *target == entry))
}

/// Compiles a source twice, with every `HELPER` marker allowing and then
/// forbidding inlining, and checks that both artifacts print the same output.
fn inline_differential(
    body: &str,
    expected: &str,
) -> (cog::backend::CompiledProgram, cog::backend::CompiledProgram) {
    let optimized = compile(&body.replace("HELPER", "@inline"));
    let reference = compile(&body.replace("HELPER", "@inline(\"never\")"));
    for program in [&optimized, &reference] {
        for jit in [false, true] {
            assert_eq!(
                String::from_utf8(execute(program, jit)).unwrap(),
                expected,
                "jit={jit}\n{}",
                program.disassemble()
            );
        }
    }
    (optimized, reference)
}

#[test]
fn inlined_callees_read_global_constants_of_every_scalar_kind() {
    let (optimized, reference) = inline_differential(
        r#"
        const INT = 100;
        const UNSIGNED: uint = 3;
        const BIG: bigint = 123456789012345678901234567890;
        const RATIO = 1.5;
        const FLAG = true;
        const MARK = 'x';
        HELPER fn add(x: int) -> int { x + INT }
        HELPER fn unsigned(x: uint) -> uint { x + UNSIGNED }
        HELPER fn big(x: bigint) -> bigint { x + BIG }
        HELPER fn ratio(x: float) -> float { x * RATIO }
        HELPER fn flag(x: bool) -> bool { if FLAG { x } else { !x } }
        HELPER fn mark(x: char) -> bool { x == MARK }
        fn work(n: int, u: uint, b: bigint, f: float, t: bool, c: char) {
            io::println(add(n))!!;
            io::println(unsigned(u))!!;
            io::println(big(b))!!;
            io::println(ratio(f))!!;
            io::println(flag(t))!!;
            io::println(mark(c))!!;
        }
        fn main() { work(1, 2, 3, 2.0, false, 'x'); }
        "#,
        "101\n5\n123456789012345678901234567893\n3\nfalse\ntrue\n",
    );
    let work = function(&optimized, "work").1;
    for name in ["add", "unsigned", "big", "ratio", "flag", "mark"] {
        assert!(calls_helper(
            &reference,
            function(&reference, "work").1,
            name
        ));
        assert!(!calls_helper(&optimized, work, name), "{name}: {work:?}");
    }
    assert!(
        !work
            .iter()
            .any(|op| matches!(op, Instruction::LoadGlobal(_)))
    );
}

#[test]
fn inlined_global_constants_ignore_caller_locals_and_respect_callee_shadowing() {
    let (optimized, _) = inline_differential(
        r#"
        const K = 5;
        const OTHER = 7;
        HELPER fn plain(x: int) -> int { x + K }
        HELPER fn parameter(K: int) -> int { K + OTHER }
        HELPER fn before_binding(x: int) -> int {
            const seen = K;
            const K = x + 1000;
            seen * 10000 + K
        }
        HELPER fn nested(x: int) -> int {
            const inner = if x > 0 {
                const K = x * 2;
                K
            } else {
                0
            };
            inner * 100 + K
        }
        fn work(n: int) {
            const K = 900;
            const OTHER = 800;
            io::println(plain(n) + K)!!;
            io::println(parameter(OTHER))!!;
            io::println(before_binding(n))!!;
            io::println(nested(n))!!;
            io::println(plain(plain(n)))!!;
        }
        fn main() { work(2); }
        "#,
        "907\n807\n51002\n405\n12\n",
    );
    for name in ["plain", "parameter", "before_binding", "nested"] {
        assert!(
            !calls_helper(&optimized, function(&optimized, "work").1, name),
            "{name}"
        );
    }
}

#[test]
fn callees_reading_mutable_or_unfolded_globals_stay_direct_calls() {
    let (optimized, _) = inline_differential(
        r#"
        var counter: int = 1;
        const TEXT = "t";
        const FAILED = 1 / 0;
        HELPER fn mutable(x: int) -> int { x + counter }
        HELPER fn text(x: int) -> bool { TEXT == "t" }
        HELPER fn failed(x: int) -> int { x + FAILED }
        fn work(n: int) {
            io::println(mutable(n))!!;
            counter = 10;
            io::println(mutable(n))!!;
            io::println(text(n))!!;
            io::println(failed(n).error_kind())!!;
        }
        fn main() { work(2); }
        "#,
        "3\n12\ntrue\nDivisionByZero\n",
    );
    let (_, work) = function(&optimized, "work");
    for name in ["mutable", "text", "failed"] {
        let (entry, _) = function(&optimized, name);
        assert!(
            work.iter()
                .any(|op| matches!(op, Instruction::Call(target, 1) if *target == entry)),
            "{name}"
        );
    }
}

#[test]
fn global_initializers_and_overflow_keep_their_behavior_when_constants_inline() {
    let source = r#"
        const LIMIT = 9223372036854775807;
        HELPER fn above(x: int) -> int { x + LIMIT }
        const START = above(0);
        fn work(n: int) {
            defer { io::println("cleanup")!!; }
            io::println(START == LIMIT)!!;
            HANDLE
        }
        fn main() { work(1); }
    "#;
    let handled = "try { io::println(above(n)); }\n\
                   catch (error) { io::println(error.error_kind())!!; }";
    let (optimized, _) = inline_differential(
        &source.replace("HANDLE", handled),
        "true\nOverflow\ncleanup\n",
    );
    assert!(!calls_helper(
        &optimized,
        function(&optimized, "work").1,
        "above"
    ));
    // The Error origin keeps naming the callee as an inline frame.
    let failing = source.replace("HANDLE", "io::println(above(n)!!)!!;");
    let mut traces = Vec::new();
    for marker in ["@inline", "@inline(\"never\")"] {
        let program = compile(&failing.replace("HELPER", marker));
        let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        let mut output = Vec::new();
        let mut context =
            cog::runtime::RuntimeContext::new(&mut output, cog::runtime::VmCapabilities::default());
        let error = cog::interpreter::run_with_context(&program, &mut context).unwrap_err();
        assert_eq!(error.kind(), "Overflow");
        let trace = context
            .last_runtime_diagnostic()
            .unwrap()
            .provenance
            .as_ref()
            .unwrap();
        traces.push(
            std::iter::once(&trace.origin)
                .chain(&trace.callers)
                .map(|frame| frame.function.clone())
                .collect::<Vec<_>>(),
        );
        drop(context);
        assert_eq!(output, b"true\ncleanup\n");
    }
    assert_eq!(traces[0], traces[1]);
    assert!(
        traces[0]
            .iter()
            .any(|name| name.as_deref() == Some("above")),
        "{:?}",
        traces[0]
    );
}

#[test]
fn inlined_constants_resolve_in_the_callee_module() {
    let directory =
        std::env::temp_dir().join(format!("cog-optimizer-modules-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("config.cog"),
        "const SECRET = 40;\n\
         public const SHARED = 2;\n\
         @inline public fn answer(x: int) -> int { x + SECRET + SHARED }\n",
    )
    .unwrap();
    std::fs::write(
        directory.join("main.cog"),
        "@!include(\"std\")\n\
         use io;\n\
         use config;\n\
         const SECRET = 1000;\n\
         const SHARED = 2000;\n\
         fn work(n: int) { io::println(config::answer(n))!!; \
         io::println(SECRET + SHARED + n)!!; }\n\
         fn main() { work(0); }\n",
    )
    .unwrap();
    let loaded = project::load(&directory, None, false)
        .unwrap_or_else(|errors| panic!("{}", cog::frontend::render(&errors)));
    let program = project::compile(loaded.program, loaded.diagnostics, &loaded.names).unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    let (_, work) = function(&program, "work");
    assert!(
        !calls_helper(&program, work, "answer"),
        "{}",
        program.disassemble()
    );
    for jit in [false, true] {
        assert_eq!(execute(&program, jit), b"42\n3000\n");
    }
}
