use cog::sdk::{Engine, Limits, RunContext};

#[test]
fn cog_fixtures_match_across_vm_and_retained_jit() {
    for (name, source, expected) in [
        (
            "collections.cog",
            include_str!("fixtures/stdlib_v2_collections.cog"),
            "[4, 2, 1, 3, 6]\n[4, 2]\n[1]\n[1, 3, 6]\n[4, 1, 2]\ncollections ok\n",
        ),
        (
            "algorithms.cog",
            include_str!("fixtures/stdlib_v2_algorithms.cog"),
            "algorithms ok\n",
        ),
        (
            "binary.cog",
            include_str!("fixtures/stdlib_v2_binary.cog"),
            "binary ok\n",
        ),
        (
            "breadth_first.cog",
            include_str!("../examples/breadth_first.cog"),
            "[0, 1, 2, 3, 4]\n",
        ),
        (
            "set-benchmark.cog",
            include_str!("../benchmarks/stdlib/set.cog"),
            "112000\n",
        ),
        (
            "word_inventory.cog",
            include_str!("../examples/word_inventory.cog"),
            "apple: 3\npear: 2\nplum: 1\n",
        ),
    ] {
        let engine = Engine::new();
        let source = cog::project::compile_source(name, source).unwrap_or_else(|e| panic!("{e}"));
        let program = engine
            .decode(&cog::bytecode::encode(&source).unwrap())
            .unwrap();
        for jit in [false, true, true] {
            let mut output = Vec::new();
            {
                let mut context = RunContext::new(&mut output).with_jit(jit);
                context.set_process_arguments(Vec::new());
                program
                    .run_in(&mut context)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            }
            assert_eq!(output, expected.as_bytes(), "{name}, JIT={jit}");
        }
    }
}

#[test]
fn repeating_text_obeys_buffer_limits_with_jit_requested() {
    let program = Engine::new()
        .compile(
            "limited-repeat.cog",
            r#"@!include("std") use str; use err::Error;
            fn main() {
                when str::repeat("é", 6) is Error::ResourceLimit {} else {
                    crash("repeat exceeded the buffer quota");
                }
            }"#,
        )
        .unwrap();
    let mut output = Vec::new();
    program
        .run_in(
            &mut RunContext::new(&mut output)
                .with_jit(true)
                .with_limits(Limits {
                    max_buffer_bytes: Some(8),
                    ..Limits::default()
                }),
        )
        .unwrap();
    assert!(output.is_empty());
}

#[test]
fn queue_drains_reference_values_under_a_small_live_heap_limit() {
    let program = Engine::new()
        .compile(
            "limited-queue.cog",
            r#"@!include("std") use collections;
            fn main() {
                var queue = collections::Queue::new[int[]]();
                for i in 0..256 {
                    queue.push([i])!!;
                    if queue.pop()!![0]!! != i { crash("queue lost a payload"); }
                }
            }"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let mut output = Vec::new();
    let limits = Limits {
        max_heap_objects: Some(48),
        ..Limits::default()
    };
    program
        .run_in(&mut RunContext::new(&mut output).with_limits(limits))
        .unwrap();
    assert!(output.is_empty());
}
