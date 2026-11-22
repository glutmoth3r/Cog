import { readFile } from "node:fs/promises";
import { WASI } from "node:wasi";

const [artifact, ...args] = process.argv.slice(2);
const module = await WebAssembly.compile(await readFile(artifact));
const wasi = new WASI({ version: "preview1", args: [artifact, ...args], returnOnExit: true });
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
process.exitCode = wasi.start(instance);
