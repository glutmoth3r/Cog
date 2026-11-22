#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."
profile=target/x86_64-unknown-linux-gnu/release
cli=${COG_NATIVE_CLI:-$profile/cog}
cc=${CC:-cc}
mkdir -p target
out=$(mktemp -d "$PWD/target/native-check.XXXXXXXX")
trap 'rm -rf -- "$out"' EXIT

cp examples/native/{pipeline,error,cleanup}.cog "$out"
"$cli" build --native "$out/pipeline.cog" -o "$out/pipeline"
"$cli" run "$out/pipeline.cog" -- Ω > "$out/vm"
env -u COG_NATIVE_RUNTIME PATH=/nonexistent COG_AOT_TRACE=1 "$out/pipeline" Ω > "$out/aot" 2> "$out/trace"
cmp "$out/vm" "$out/aot"
printf '497969313\n[Ω]\n' | cmp - "$out/aot"
grep -Eq '\[aot\] function=.*status=0 backedges=999999' "$out/trace"
grep -Eq 'compiled=999999 .*aggregate=1999998' "$out/trace"

# The ELF AOT table is capped at 512 entries. Keep a hot direct callee beyond
# that table so the opt-in runtime JIT must compile and use its VM fallback.
{
    printf '@!include("std")\nuse io;\n'
    for index in $(seq 0 512); do
        printf '@inline("never")\nfn filler_%03d(value: int) -> int { value + 1 }\n' "$index"
    done
    cat <<'COG'
@inline("never")
fn vm_fallback(value: int) -> int { value + 2 }

fn main() -> nil {
    var index = 0;
    var total = 0;
    while index < 6000 {
        total = total + (vm_fallback(index)!!);
        index += 1;
    }
    io::println(total)!!;
}
COG
} > "$out/hybrid-jit.cog"
"$cli" build --native "$out/hybrid-jit.cog" -o "$out/hybrid-jit"
"$cli" run "$out/hybrid-jit.cog" > "$out/vm"
COG_AOT_JIT=1 COG_AOT_TRACE=1 "$out/hybrid-jit" > "$out/aot-jit" 2> "$out/trace"
cmp "$out/vm" "$out/aot-jit"
printf '18009000\n' | cmp - "$out/aot-jit"
grep -Eq '\[jit\] call-compile function=.* machine_code_bytes=' "$out/trace"
grep -Eq '\[jit\] bridge function=.*compiled=[1-9][0-9]* cog=' "$out/trace"

for workload in blocks callable; do
    cp "benchmarks/optimizer/$workload.cog" "$out"
    "$cli" build --native "$out/$workload.cog" -o "$out/$workload"
    "$cli" run "$out/$workload.cog" > "$out/vm"
    "$cli" run --jit "$out/$workload.cog" > "$out/jit"
    COG_AOT_TRACE=1 "$out/$workload" > "$out/aot" 2> "$out/trace"
    cmp "$out/vm" "$out/jit"
    cmp "$out/vm" "$out/aot"
    grep -Eq '\[aot\] function=.*status=0 backedges=199999' "$out/trace"
    if [[ "$workload" == callable ]]; then
        grep -Eq '\[jit\] bridge .*compiled=199999 cog=0 ' "$out/trace"
    fi
done

for stdlib_example in examples/breadth_first.cog tests/fixtures/stdlib_v2_binary.cog benchmarks/stdlib/set.cog; do
    name=$(basename "$stdlib_example" .cog)
    cp "$stdlib_example" "$out/$name.cog"
    "$cli" build --native "$out/$name.cog" -o "$out/$name"
    "$cli" run "$out/$name.cog" > "$out/vm"
    "$cli" run --jit "$out/$name.cog" > "$out/jit"
    env -u COG_NATIVE_RUNTIME PATH=/nonexistent "$out/$name" > "$out/aot"
    cmp "$out/vm" "$out/jit"
    cmp "$out/vm" "$out/aot"
done

cp examples/native/optimizer-error.cog "$out"
"$cli" build --native "$out/optimizer-error.cog" -o "$out/optimizer-error"
"$cli" run "$out/optimizer-error.cog" > "$out/vm"
"$cli" run --jit "$out/optimizer-error.cog" > "$out/jit"
env -u COG_NATIVE_RUNTIME PATH=/nonexistent "$out/optimizer-error" > "$out/aot"
cmp "$out/vm" "$out/jit"
cmp "$out/vm" "$out/aot"
printf 'Overflow\n150014997\n' | cmp - "$out/aot"

"$cli" build --native "$out/error.cog" -o "$out/error"
if "$cli" run --color never "$out/error.cog" -- > "$out/vm" 2> "$out/vm-error"; then
    exit 1
fi
if "$out/error" > "$out/aot" 2> "$out/aot-error"; then
    exit 1
fi
cmp "$out/vm" "$out/aot"
diff -u <(grep -Ei 'runtime error.*division by zero' "$out/vm-error" | sed "s/^error: //") "$out/aot-error"

"$cli" build --native "$out/cleanup.cog" -o "$out/cleanup"
"$cli" run "$out/cleanup.cog" > "$out/vm"
COG_AOT_TRACE=1 "$out/cleanup" > "$out/aot" 2> "$out/trace"
cmp "$out/vm" "$out/aot"
printf '70093\n140000\n' | cmp - "$out/aot"
grep -Eq '\[aot\] function=.*backedges=[1-9]' "$out/trace"

"$cc" -std=c11 -Wall -Wextra -Werror -O2 -c examples/native/foreign.c -o "$out/foreign.o"
imports=(--c-import foreign_math::process=process_value:i64
         --c-import foreign_math::scale=scale_value:f64)
"$cli" build --native examples/native/imports.cog "${imports[@]}" --c-link "$out/foreign.o" -o "$out/imports"
env -u COG_NATIVE_RUNTIME COG_AOT_TRACE=1 "$out/imports" > "$out/aot" 2> "$out/trace"
printf '4799685\n1.75\n' | cmp - "$out/aot"
grep -Eq '\[aot\] function=.*status=0 backedges=99999' "$out/trace"
grep -Eq 'native=99999 ' "$out/trace"
cp "$out/imports" "$out/previous"
if "$cli" build --native examples/native/imports.cog "${imports[@]}" -o "$out/imports" > "$out/failed" 2>&1; then
    exit 1
fi
cmp "$out/previous" "$out/imports"
grep -Eq 'native link failed:.*|undefined reference' "$out/failed"

mkdir -p "$out/library/src"
printf 'package "arithmetic" version="0.1.0" kind="library"\n' > "$out/library/package.kdl"
cp examples/native/library.cog "$out/library/src/lib.cog"
"$cli" build --lib "$out/library" --export add --export divide -o "$out/libarithmetic.so"
"$cc" -std=c11 -Wall -Wextra -Werror -pthread -I "$out" examples/native/client.c \
    -L "$out" -Wl,-rpath,"$out" -larithmetic -ldl -o "$out/client"
cp "$out/libarithmetic.so" "$out/second.so"
env -u COG_NATIVE_RUNTIME COG_AOT_TRACE=1 "$out/client" "$out/second.so" 2> "$out/trace"
grep -Eq '\[aot\] call function=.*status=0' "$out/trace"
grep -Eq '\[aot\] call function=.*status=2' "$out/trace"
# Unchanged source, runtime and toolchain produce the same executable bits.
"$cli" build --native "$out/pipeline.cog" -o "$out/rebuilt"
cmp "$out/pipeline" "$out/rebuilt"
printf 'Native checks passed\n'
