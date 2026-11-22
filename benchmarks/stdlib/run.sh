#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."
baseline=$(realpath "${1:?Provide the baseline profile directory}")
current=$(realpath "${2:?Provide the current profile directory}")
out=${3:-target/stdlib-v2/evidence}
mkdir -p "$out" target
scratch=$(mktemp -d "$PWD/target/stdlib-measure.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/before" "$scratch/after"
cp benchmarks/stdlib/lookup-before.cog "$scratch/before/lookup.cog"
cp benchmarks/stdlib/lookup-after.cog "$scratch/after/lookup.cog"

harness=benchmarks/optimizer/measure.py
legacy=(--source benchmarks/stdlib/map.cog --source benchmarks/stdlib/arrays.cog
        --source benchmarks/stdlib/sort_text.cog)
python3 "$harness" "$baseline" "${legacy[@]}" --source "$scratch/before/lookup.cog" > "$out/before.txt"
python3 "$harness" "$current" "${legacy[@]}" --source "$scratch/after/lookup.cog" \
    --reference "$out/before.txt" > "$out/after.txt"

added=(--source benchmarks/stdlib/queue.cog --source benchmarks/stdlib/set.cog
       --source benchmarks/stdlib/heap.cog --source benchmarks/stdlib/binary.cog)
for size in 500 2000 8000; do
    python3 "$harness" "$current" "${added[@]}" --arg "$size" > "$out/size-$size.txt"
done

for name in map queue set heap binary; do
    dir="$scratch/$name"
    mkdir "$dir"
    cp "benchmarks/stdlib/$name.cog" "$dir/$name.cog"
    COG_NATIVE_RUNTIME="$current/libcog.a" "$current/cog" build --native \
        "$dir/$name.cog" -o "$dir/program" 2> "$out/trace-$name.txt"
    COG_AOT_TRACE=1 "$dir/program" >> "$out/trace-$name.txt" 2>&1
    if [[ -x /usr/bin/time ]]; then
        /usr/bin/time -f '%M' -o "$out/peak-kib-$name.txt" "$dir/program" > /dev/null
    fi
done
