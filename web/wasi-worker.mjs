import { runWasi } from "./wasi.mjs";

globalThis.onmessage = async ({ data: { module, args } }) => {
    try {
        globalThis.postMessage({ result: await runWasi(module, args) });
    } catch (error) {
        globalThis.postMessage({ error: error instanceof Error ? error.message : String(error) });
    }
};
