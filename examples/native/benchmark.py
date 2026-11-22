"""Measure fresh processes and retained Cog plans on the same workloads."""

import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
os.chdir(ROOT)
PROFILE = ROOT / "target/x86_64-unknown-linux-gnu/release"
CLI = os.environ.get("COG_NATIVE_CLI", PROFILE / "cog")
os.environ.setdefault("COG_NATIVE_RUNTIME", str(PROFILE / "libcog.a"))
os.environ.pop("COG_AOT_TRACE", None)
SAMPLES = 9


def checked(*args, text=False):
    result = subprocess.run(args, capture_output=True, text=text)
    if result.returncode:
        raise RuntimeError(f"{args!r} failed ({result.returncode}): {result.stderr!r}")
    return result


def run(*args):
    return checked(*args).stdout




with tempfile.TemporaryDirectory(prefix="cog-native-bench-", dir="target") as directory:
    out = Path(directory).resolve()
    c = out / "c"
    rust = out / "rust"
    run(os.environ.get("CC", "cc"), "-O3", "examples/native/baseline.c", "-o", c)
    run("rustc", "-O", "examples/native/baseline.rs", "-o", rust)
    for workload in ("numeric", "pipeline", "closures", "bytes", "bigint"):
        source = out / f"{workload}.cog"
        source.write_bytes(Path(f"examples/native/{workload}.cog").read_bytes())
        executable = out / workload
        command = [CLI, "build", "--native", source, "-o", executable]
        start = time.perf_counter_ns()
        build = checked(*command, text=True)
        build_ms = (time.perf_counter_ns() - start) / 1e6
        print(workload, build.stderr.strip().replace("\n", " | "))
        expected = run(executable).splitlines()[0]
        assert run(CLI, "run", source).splitlines()[0] == expected
        assert run(CLI, "run", "--jit", source).splitlines()[0] == expected
        commands = [
            ("VM", [CLI, "run", source.with_suffix(".cogb")]),
            ("JIT", [CLI, "run", "--jit", source.with_suffix(".cogb")]),
            ("AOT", [executable]),
        ]
        if workload in ("numeric", "pipeline"):
            assert run(c, workload).strip() == expected == run(rust, workload).strip()
            commands += [("C", [c, workload]), ("Rust", [rust, workload])]
        print(f"  build_ms={build_ms:.3f} exe_bytes={executable.stat().st_size}", flush=True)
        for label, command in commands:
            # Keep child processes isolated from saved APE profiles. The
            # retained-plan example below explicitly measures reuse.
            timings = []
            for _ in range(SAMPLES):
                for stale in out.glob("target/.cog-ape-*.json"):
                    stale.unlink()
                start = time.perf_counter_ns()
                run(*command)
                timings.append((time.perf_counter_ns() - start) / 1e6)
            print(f"  {label:4s} fresh_median_ms={statistics.median(timings):.3f}")
        print(run(PROFILE / "examples/bench_native", source).decode().strip())


    library = out / "libarithmetic.so"
    run(CLI, "build", "--lib", "examples/native/library.cog", "--export", "add", "-o", library)
    client = out / "bench-api"
    run(os.environ.get("CC", "cc"), "-O3", "-I", out, "examples/native/bench_api.c",
        "-L", out, f"-Wl,-rpath,{out}", "-larithmetic", "-o", client)
    samples = [list(map(float, run(client).split())) for _ in range(SAMPLES)]
    print(f"C -> Cog init_median_us={statistics.median(x[0] for x in samples) / 1000:.3f}"
          f" call_median_ns={statistics.median(x[1] for x in samples):.3f}"
          f" library_bytes={library.stat().st_size}")

    obj = out / "foreign.o"
    exe = out / "native-imports"
    run(os.environ.get("CC", "cc"), "-O3", "-c", "examples/native/foreign.c", "-o", obj)
    run(CLI, "build", "--native", "examples/native/imports.cog",
        "--c-import", "foreign_math::process=process_value:i64",
        "--c-import", "foreign_math::scale=scale_value:f64", "--c-link", obj, "-o", exe)
    assert run(exe) == b"4799685\n1.75\n"
    samples = []
    for _ in range(SAMPLES):
        start = time.perf_counter_ns()
        run(exe)
        samples.append((time.perf_counter_ns() - start) / 1e6)
    print(f"Cog -> C 100000 calls fresh_median_ms={statistics.median(samples):.3f}")
