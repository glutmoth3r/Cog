//! Measure retained VM and JIT execution of the native examples.

use cog::sdk::{Engine, RunContext};
use std::hint::black_box;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source_path = args.next().ok_or("expected a Cog source path")?;
    let arguments: Vec<_> = args.collect();
    let source = std::fs::read_to_string(&source_path)?;
    let name = source_path.to_string_lossy();
    let mut expected = None;
    for jit in [false, true] {
        let mut compilation = Instant::now();
        let program = Engine::new().compile(&name, &source)?;
        let frontend = compilation.elapsed();
        let mut times = Vec::with_capacity(8);
        for _ in 0..8 {
            let mut output = Vec::new();
            let mut context = RunContext::new(&mut output).with_jit(jit);
            context.set_process_arguments(arguments.clone());
            compilation = Instant::now();
            program.run_in(&mut context)?;
            times.push(compilation.elapsed().as_secs_f64() * 1e3);
            drop(context);
            assert_eq!(&output, expected.get_or_insert_with(|| output.clone()));
            black_box(output);
        }
        let first = times.remove(0);
        times.sort_by(f64::total_cmp);
        println!(
            "{} frontend_ms={:.3} first_ms={first:.3} retained_median_ms={:.3}",
            if jit { "JIT" } else { "VM" },
            frontend.as_secs_f64() * 1e3,
            times[times.len() / 2],
        );
    }
    Ok(())
}
