use cog::sdk::{Engine, Program, RunContext};
use std::time::{Duration, Instant};

fn source(attribute: &str, iterations: u32) -> String {
    format!(
        r#"
@!include("std")
use io;

{attribute}
@inline("never")
fn step(value: int) -> int {{
    const adjusted = value + 1;
    adjusted * 2
}}

fn main() -> nil {{
    var index = 0;
    var checksum = 0;
    while index < {iterations} {{
        checksum += step(index);
        index += 1;
    }}
    io::println(checksum)!!;
}}
"#
    )
}

fn execute(program: &Program, jit: bool, expected: &str) -> Duration {
    let mut output = Vec::new();
    let mut context = RunContext::new(&mut output).with_jit(jit);
    let start = Instant::now();
    program.run_in(&mut context).unwrap();
    let elapsed = start.elapsed();
    drop(context);
    assert_eq!(output, expected.as_bytes());
    elapsed
}

fn median(samples: &mut [Duration]) -> f64 {
    samples.sort_unstable();
    samples[samples.len() / 2].as_secs_f64() * 1_000.0
}

fn main() {
    let iterations = 500_000;
    let expected = format!("{}\n", u64::from(iterations) * u64::from(iterations + 1));
    let engine = Engine::new();
    let cases = [("ordinary", ""), ("propagate", "@propagate")];
    let sources = cases.map(|(_, attribute)| source(attribute, iterations));
    let mut compile_times = [Vec::new(), Vec::new()];
    let mut vm_times = [Vec::new(), Vec::new()];
    let mut jit_times = [Vec::new(), Vec::new()];
    let programs = sources
        .each_ref()
        .map(|source| engine.compile("bench.cog", source).unwrap());
    let first_jit = programs
        .each_ref()
        .map(|program| execute(program, true, &expected));
    for program in &programs {
        execute(program, false, &expected);
        execute(program, true, &expected);
    }
    for _ in 0..7 {
        for (index, source) in sources.iter().enumerate() {
            let start = Instant::now();
            let program = engine.compile("bench.cog", source).unwrap();
            compile_times[index].push(start.elapsed());
            assert_eq!(program.encode().unwrap(), programs[index].encode().unwrap());
            vm_times[index].push(execute(&programs[index], false, &expected));
            jit_times[index].push(execute(&programs[index], true, &expected));
        }
    }
    println!("mode,compile_ms,bytecode_bytes,vm_ms,jit_first_ms,jit_enabled_retained_ms");
    for (index, (mode, _)) in cases.iter().enumerate() {
        println!(
            "{mode},{:.3},{},{:.3},{:.3},{:.3}",
            median(&mut compile_times[index]),
            programs[index].encode().unwrap().len(),
            median(&mut vm_times[index]),
            first_jit[index].as_secs_f64() * 1_000.0,
            median(&mut jit_times[index]),
        );
    }
}
