use cog::sdk::{Engine, Program, RunContext};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Case {
    name: &'static str,
    source: fn(u64) -> String,
}

fn int_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn work(n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
        total += index;
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work({iterations}))!!;
}}
"#
    )
}

fn slot_pressure_source(iterations: u64) -> String {
    let mut scratch = String::with_capacity(8_000);
    for slot in 0..200 {
        writeln!(scratch, "        const scratch_{slot} = index;").unwrap();
    }
    format!(
        r#"
@!include("std")
use io;

fn work(n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
{scratch}        total += index;
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work({iterations}))!!;
}}
"#
    )
}

fn uint_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn work(n: uint) -> uint {{
    const zero: uint = 0;
    const one: uint = 1;
    var index: uint = zero;
    var total: uint = zero;
    while index < n {{
        total += index;
        index += one;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work({iterations}))!!;
}}
"#
    )
}

fn float_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn work(n: int) -> float {{
    const half: float = 0.5;
    var index = 0;
    var total: float = 0.0;
    while index < n {{
        total += half;
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work({iterations}))!!;
}}
"#
    )
}

fn enum_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

enum Signal {{ Hit(value: int), Miss }}

fn work(signal: Signal, n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
        when signal is Signal {{ total += 1; }}
        when signal is Signal::Hit {{ total += 2; }}
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work(Signal::Hit(7), {iterations}))!!;
}}
"#
    )
}

fn mixed_integer_float_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn signed_work(value: int, probe: float, n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
        if value < probe {{ total += 1; }}
        if value >= probe {{ total += 2; }}
        if probe < value {{ total += 4; }}
        if probe >= value {{ total += 8; }}
        index += 1;
    }}
    return total;
}}

fn unsigned_work(value: uint, probe: float, n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
        if value < probe {{ total += 1; }}
        if value >= probe {{ total += 2; }}
        if probe < value {{ total += 4; }}
        if probe >= value {{ total += 8; }}
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(signed_work(9007199254740993, 9007199254740992.0, {iterations}))!!;
    io::println(unsigned_work(9007199254740993, 9007199254740992.0, {iterations}))!!;
}}
"#
    )
}

fn bigint_float_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn bigint_work(value: bigint, probe: float, n: int) -> int {{
    var index = 0;
    var total = 0;
    while index < n {{
        if value < probe {{ total += 1; }}
        if value >= probe {{ total += 2; }}
        if probe < value {{ total += 4; }}
        if probe >= value {{ total += 8; }}
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(bigint_work(9007199254740993, 9007199254740992.0, {iterations}))!!;
}}
"#
    )
}

fn bigint_arithmetic_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn bigint_work(n: int, high: bigint, high_plus_one: bigint) -> bigint {{
    const one: bigint = 1 as bigint;
    const signed_limit: bigint = 9223372036854775807 as bigint;
    var index = 0;
    var total: bigint = 0 as bigint;
    var high_total: bigint = high;
    var negative: bigint = 0 as bigint;
    while index < n {{
        total = total + one;
        total = total * one;
        high_total = high_total - high;
        high_total = high_total + high;
        negative = high - high_plus_one;
        negative = negative + one;
        index += 1;
    }}
    return total + signed_limit + negative + high_total;
}}

fn main() -> nil {{
    io::println(bigint_work(
        {iterations},
        9223372036854775808 as bigint,
        9223372036854775809 as bigint
    ))!!;
}}
"#
    )
}

fn bigint_helper_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

@inline
fn bigint_add(left: bigint, right: bigint) -> bigint {{
    return left + right;
}}

@inline
fn bigint_multiply(left: bigint, right: bigint) -> bigint {{
    return left * right;
}}

fn work(n: int) -> bigint {{
    const one: bigint = 1 as bigint;
    var index = 0;
    var total: bigint = 0 as bigint;
    while index < n {{
        total = bigint_add(total, one);
        total = bigint_multiply(total, one);
        index += 1;
    }}
    return total;
}}

fn main() -> nil {{
    io::println(work({iterations}))!!;
}}
"#
    )
}

fn bigint_divmod_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

@inline
fn bigint_divide(left: bigint, right: bigint) -> bigint {{
    return left / right;
}}

@inline
fn bigint_remainder(left: bigint, right: bigint) -> bigint {{
    return left % right;
}}

@inline
fn bigint_multiply(left: bigint, right: bigint) -> bigint {{
    return left * right;
}}

fn signed_work(n: int, initial: int) -> bigint {{
    const one: bigint = 1 as bigint;
    const two: bigint = 2 as bigint;
    const three: bigint = 3 as bigint;
    var index = 0;
    var value: bigint = initial as bigint;
    while index < n {{
        value = value / two;
        value = value % three;
        value = value - one;
        index += 1;
    }}
    return value;
}}

fn unsigned_work(n: int) -> bigint {{
    const high: bigint = 9223372036854775808 as bigint;
    var index = 0;
    var value: bigint = high;
    while index < n {{
        value = bigint_divide(value, high);
        value = bigint_remainder(value, high);
        value = bigint_multiply(value, high);
        index += 1;
    }}
    return value;
}}

fn boundary_work(n: int, minimum: int, negative_one: int) -> bigint {{
    const minimum_big: bigint = minimum as bigint;
    const negative_one_big: bigint = negative_one as bigint;
    var index = 0;
    var value: bigint = 0 as bigint;
    while index < n {{
        value = minimum_big / negative_one_big;
        index += 1;
    }}
    return value;
}}

fn main() -> nil {{
    io::println(signed_work({iterations}, -17))!!;
    io::println(unsigned_work({iterations}))!!;
    io::println(boundary_work({iterations}, -9223372036854775807 - 1, -1))!!;
}}
"#
    )
}

fn bigint_compare_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn compare(negative: bigint, positive: bigint, large: bigint, n: int) -> int {{
    var index = 0;
    var result = 0;
    while index < n {{
        if negative < positive {{ result += 1; }}
        if positive > negative {{ result += 2; }}
        if negative <= large {{ result += 4; }}
        if large >= negative {{ result += 8; }}
        if positive < large {{ result += 16; }}
        if large > positive {{ result += 32; }}
        if negative == positive {{ result += 64; }}
        if negative != positive {{ result += 128; }}
        index += 1;
    }}
    return result;
}}

fn main() -> nil {{
    io::println(compare(-1 as bigint, 7 as bigint, 9223372036854775808 as bigint, {iterations}))!!;
}}
"#
    )
}

fn bigint_mixed_arithmetic_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn mixed_work(negative: bigint, large: bigint, n: int) -> int {{
    var index = 0;
    var result = 0;
    while index < n {{
        const sum = negative + large;
        const difference = large - negative;
        const product = negative * large;
        const reverse_product = large * negative;
        if sum == 9223372036854775807 {{ result += 1; }}
        if difference > 9223372036854775807 {{ result += 2; }}
        if product == (-9223372036854775807 - 1) {{ result += 4; }}
        if reverse_product == (-9223372036854775807 - 1) {{ result += 8; }}
        index += 1;
    }}
    return result;
}}

fn main() -> nil {{
    io::println(mixed_work(-1 as bigint, 9223372036854775808 as bigint, {iterations}))!!;
}}
"#
    )
}

fn bigint_mixed_overflow_source(iterations: u64) -> String {
    format!(
        r#"
@!include("std")
use io;

fn mixed_overflow(signed: bigint, unsigned: bigint, n: int) -> bigint {{
    var index = 0;
    var result: bigint = 0 as bigint;
    while index < n {{
        result = signed - unsigned;
        index += 1;
    }}
    return result;
}}

fn main() -> nil {{
    io::println(mixed_overflow(-1 as bigint, 9223372036854775808 as bigint, {iterations}))!!;
}}
"#
    )
}

fn run(program: &Program, jit: bool, profile: Option<&Path>) -> Result<(Duration, String), String> {
    let mut output = Vec::new();
    let mut context = RunContext::new(&mut output).with_jit(jit);
    if std::env::var_os("COG_JIT_BENCH_DEBUG").is_some() {
        context = context.with_jit_debug(true);
    }
    if let Some(profile) = profile {
        context = context.with_jit_profile_path(profile);
    }
    let started = Instant::now();
    program
        .run_in(&mut context)
        .map_err(|error| error.to_string())?;
    let elapsed = started.elapsed();
    drop(context);
    let output = String::from_utf8(output).map_err(|error| error.to_string())?;
    Ok((elapsed, output))
}

fn compile(engine: &Engine, name: &str, source: &str) -> Result<Program, String> {
    engine
        .compile(name, source)
        .map_err(|error| error.to_string())
}

fn median(values: &[Duration]) -> Duration {
    let mut values = values.to_vec();
    values.sort_unstable();
    values[values.len() / 2]
}

fn percentile(values: &[Duration], numerator: usize) -> Duration {
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = (values.len() * numerator).div_ceil(100).saturating_sub(1);
    values[index]
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let iterations = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(100_000);
    let samples = args
        .next()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(7);
    if args.next().is_some() || iterations == 0 || !(1..=1_000).contains(&samples) {
        return Err("usage: bench_jit [iterations>0] [samples=1..1000]".into());
    }

    let output_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("jit-bench");
    fs::create_dir_all(&output_dir)?;
    let engine = Engine::new();
    let cases = [
        Case {
            name: "int",
            source: int_source,
        },
        Case {
            name: "slot-pressure",
            source: slot_pressure_source,
        },
        Case {
            name: "uint",
            source: uint_source,
        },
        Case {
            name: "float",
            source: float_source,
        },
        Case {
            name: "enum",
            source: enum_source,
        },
        Case {
            name: "mixed-int-float",
            source: mixed_integer_float_source,
        },
        Case {
            name: "bigint-float",
            source: bigint_float_source,
        },
        Case {
            name: "bigint-arithmetic",
            source: bigint_arithmetic_source,
        },
        Case {
            name: "bigint-helper",
            source: bigint_helper_source,
        },
        Case {
            name: "bigint-divmod",
            source: bigint_divmod_source,
        },
        Case {
            name: "bigint-compare",
            source: bigint_compare_source,
        },
        Case {
            name: "bigint-mixed-arithmetic",
            source: bigint_mixed_arithmetic_source,
        },
        Case {
            name: "bigint-mixed-overflow",
            source: bigint_mixed_overflow_source,
        },
    ];

    println!(
        "Cog JIT benchmark | mode={} target={}-{} iterations={iterations} samples={samples}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    println!(
        "case,vm_median_us,jit_first_us,jit_enabled_retained_median_us,jit_p10_us,jit_p90_us,compile_us,vm_ns_per_backedge,jit_ns_per_backedge,calibrated"
    );

    for case in cases {
        let source = (case.source)(iterations);
        let vm_program = compile(&engine, case.name, &source)?;
        let jit_program = compile(&engine, case.name, &source)?;
        let profile = output_dir.join(format!("{}-{}.json", case.name, std::process::id()));
        let _ = fs::remove_file(&profile);

        let (vm_warmup, expected_output) =
            run(&vm_program, false, None).map_err(io::Error::other)?;
        let mut vm_times = Vec::with_capacity(samples);
        vm_times.push(vm_warmup);
        for _ in 1..samples {
            let (elapsed, output) = run(&vm_program, false, None).map_err(io::Error::other)?;
            if output != expected_output {
                return Err(format!("VM output changed for {}", case.name).into());
            }
            vm_times.push(elapsed);
        }

        let (jit_first, jit_output) =
            run(&jit_program, true, Some(&profile)).map_err(io::Error::other)?;
        if jit_output != expected_output {
            return Err(format!("VM/JIT output mismatch for {}", case.name).into());
        }

        let mut jit_times = Vec::with_capacity(samples);
        for _ in 0..samples {
            let (elapsed, output) =
                run(&jit_program, true, Some(&profile)).map_err(io::Error::other)?;
            if output != expected_output {
                return Err(format!("JIT output changed for {}", case.name).into());
            }
            jit_times.push(elapsed);
        }

        let artifact_bytes = jit_program.encode()?.len();
        let mut compile_ns = 0;
        let mut vm_ns_per_backedge = 0;
        let mut jit_ns_per_backedge = 0;
        let mut calibrated = false;
        if let Ok(bytes) = fs::read(&profile) {
            let data: serde_json::Value = serde_json::from_slice(&bytes)?;
            if let Some(functions) = data["functions"].as_object() {
                for function in functions.values() {
                    let compile = function["compile_ns"].as_u64().unwrap_or(0);
                    if compile > 0 {
                        compile_ns = compile_ns.max(compile);
                        vm_ns_per_backedge = vm_ns_per_backedge
                            .max(function["vm_ns_per_backedge"].as_u64().unwrap_or(0));
                        jit_ns_per_backedge = jit_ns_per_backedge
                            .max(function["jit_ns_per_backedge"].as_u64().unwrap_or(0));
                        calibrated |= function["calibrated"].as_bool() == Some(true);
                    }
                }
            }
        }

        println!(
            "{},{},{},{},{},{},{},{},{},{}",
            case.name,
            micros(median(&vm_times)),
            micros(jit_first),
            micros(median(&jit_times)),
            micros(percentile(&jit_times, 10)),
            micros(percentile(&jit_times, 90)),
            compile_ns / 1_000,
            vm_ns_per_backedge,
            jit_ns_per_backedge,
            calibrated
        );
        let slot_report = (case.name == "slot-pressure")
            .then(|| local_slot_count(&jit_program, "work"))
            .flatten()
            .map_or_else(String::new, |slots| format!("; work_local_slots={slots}"));
        eprintln!(
            "{}: bytecode={} bytes{}; matching output {:?}",
            case.name,
            artifact_bytes,
            slot_report,
            expected_output.trim_end()
        );

        let _ = fs::remove_file(profile);
    }

    Ok(())
}

fn local_slot_count(program: &Program, function_name: &str) -> Option<usize> {
    let mut in_function = false;
    let mut max_slot = None;
    let disassembly = program.disassemble();
    for line in disassembly.lines() {
        if line.starts_with("function ") && in_function {
            break;
        }
        if let Some(label) = line.strip_suffix(':') {
            if in_function {
                break;
            }
            in_function = label == function_name;
            continue;
        }
        if in_function {
            let mut fields = line.split_whitespace();
            let operation = fields.next()?;
            if !matches!(operation, "load" | "store") {
                continue;
            }
            let slot = fields.next()?.parse::<usize>().ok()?.checked_add(1)?;
            max_slot = Some(max_slot.map_or(slot, |current: usize| current.max(slot)));
        }
    }
    max_slot
}
