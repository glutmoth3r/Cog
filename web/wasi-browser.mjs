export function createWasiRunner(url) {
    let compiled;
    return async args => {
        compiled ??= WebAssembly.compileStreaming(
            fetch(url, { signal: AbortSignal.timeout(5000) }).then(response => {
                if (!response.ok) {
                    throw new Error("Could not load the tutorial's WASM checker.");
                }
                return response;
            }),
        ).catch(error => {
            compiled = undefined;
            throw error;
        });
        const module = await compiled;
        return new Promise((resolve, reject) => {
            const worker = new Worker(new URL("./wasi-worker.mjs", import.meta.url), { type: "module" });
            const stop = () => {
                clearTimeout(timer);
                worker.terminate();
            };
            const timer = setTimeout(() => {
                stop();
                reject(new Error("WASI execution time limit reached."));
            }, 5000);
            worker.onmessage = ({ data: { result, error } }) => {
                stop();
                if (error !== undefined) reject(new Error(error));
                else resolve(result);
            };
            worker.onerror = event => {
                stop();
                reject(new Error(event.message || "Could not start the tutorial's WASM worker."));
            };
            try {
                worker.postMessage({ module, args });
            } catch (error) {
                stop();
                reject(error);
            }
        });
    };
}

const MAX_CHECK_BYTES = 96 * 1024;

// Validates the request, runs the WASI checker, and translates its protocol into a verdict.
// The comparison itself happens inside the Cog guest; this code only moves data.
export async function checkOutput(run, expected, actual) {
    const encoder = new TextEncoder();
    for (const [label, text] of [["Expected", expected], ["Actual", actual]]) {
        if (text.includes("\0")) {
            throw new Error(`${label} output contains a NUL character, which the WASM checker does not support.`);
        }
        if (encoder.encode(text).length > MAX_CHECK_BYTES) {
            throw new Error(`${label} output is larger than ${MAX_CHECK_BYTES / 1024} KiB, which is too large for the WASM checker.`);
        }
    }
    const { stdout, stderr, exitCode } = await run(["check", expected, actual]);
    if (exitCode !== 0) {
        throw new Error(`The WASM checker exited with status ${exitCode}.${stderr ? " " + stderr.trim() : ""}`);
    }
    switch (stdout) {
        case "match\n": return true;
        case "mismatch\n": return false;
        default: throw new Error("The WASM checker returned an unexpected response.");
    }
}
