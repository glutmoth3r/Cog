"""Compare compiler revisions using matching release artifacts, one revision at a time."""
import argparse
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
os.chdir(ROOT)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("profile", type=Path)
parser.add_argument("--only", nargs="+", metavar="WORKLOAD")
parser.add_argument("--reference", type=Path, metavar="RESULTS",
                    help="require each checksum to match a previous run")
args = parser.parse_args()
profile = args.profile.resolve()
reference = {}
if args.reference:
    for line in args.reference.read_text().splitlines():
        fields, separator, checksum = line.partition(" checksum=")
        if separator:
            reference[fields.split()[0]] = checksum
env = dict(os.environ, COG_NATIVE_RUNTIME=str(profile / "libcog.a"))
env.pop("COG_AOT_TRACE", None)


def run(*command):
    result = subprocess.run(command, capture_output=True, env=env)
    if result.returncode:
        raise RuntimeError(f"{command!r} exited {result.returncode}: {result.stderr.decode(errors='replace')}")
    return result.stdout, result.stderr


def measure(*command, samples=5, expected=None):
    elapsed = []
    for _ in range(samples):
        start = time.perf_counter()
        output, diagnostics = run(*command)
        elapsed.append((time.perf_counter() - start) * 1000)
        if expected is not None:
            assert output == expected, command
    return statistics.median(elapsed), output, diagnostics


def compile_bytecode(cli, source, bytecode):
    elapsed = []
    for _ in range(5):
        bytecode.unlink(missing_ok=True)
        start = time.perf_counter()
        run(cli, "build", source, "-o", bytecode)
        elapsed.append((time.perf_counter() - start) * 1000)
    return statistics.median(elapsed)


cli = profile / "cog"
workloads = [Path(f"benchmarks/optimizer/{name}.cog") for name in ("blocks", "callable", "nested", "temporary")]
workloads += [Path(f"examples/native/{name}.cog") for name in
              ("numeric", "pipeline", "closures", "bytes", "bigint")]
with tempfile.TemporaryDirectory(prefix="optimizer-", dir="target") as tmp:
    out = Path(tmp).resolve()
    mandelbrot = (ROOT / "src/bin/bench_jit.rs").read_text().split("fn mandelbrot_render_source", 1)[1]
    mandelbrot = mandelbrot.split('r#"', 1)[1].split('"#', 1)[0]
    mandelbrot_path = out / "mandelbrot.cog"
    mandelbrot_path.write_text(mandelbrot)
    large = out / "large.cog"
    declarations = "\n".join(f'fn f{i}(x: int) -> int {{ (x + {i}) % 997 }}' for i in range(1000))
    large.write_text('@!include("std")\nuse io;\n' + declarations
                     + '\nfn main() { io::println(f999(7)!!)!!; }\n')
    for workload in workloads + [mandelbrot_path, large]:
        if args.only and workload.stem not in args.only:
            continue
        workspace = out / workload.stem
        workspace.mkdir()
        source = workspace / workload.name
        source.write_bytes(workload.read_bytes())
        bytecode = source.with_suffix(".cogb")
        native = workspace / workload.stem
        compile_ms = compile_bytecode(cli, source, bytecode)
        build_ms, _, build_log = measure(cli, "build", "--native", source, "-o", native, samples=3)
        expected = run(cli, "run", bytecode)[0]
        assert run(cli, "run", "--jit", bytecode)[0] == expected
        if args.reference:
            assert reference[workload.stem] == repr(expected), workload.stem
        aot_ms, _, _ = measure(native, expected=expected)
        print(f"{workload.stem} compile_ms={compile_ms:.3f} bytecode_bytes={bytecode.stat().st_size} "
              f"aot_build_ms={build_ms:.3f} aot_run_ms={aot_ms:.3f} "
              f"exe_bytes={native.stat().st_size} checksum={expected!r}", flush=True)
        print(build_log.decode().strip(), flush=True)
        print(run(profile / "examples/bench_native", source)[0].decode().strip(), flush=True)
