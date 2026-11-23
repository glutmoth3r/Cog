const MAX_ARGUMENT_BYTES = 256 * 1024;
const MAX_OUTPUT_BYTES = 128 * 1024;
const EBADF = 8;
const EINVAL = 28;
const ENOSYS = 52;

class WasiExit {
    constructor(code) {
        this.code = code;
    }
}

// The browser host provides the WASI calls used by Cog's tutorial guest.
export async function runWasi(module, args) {
    const encoder = new TextEncoder();
    let argumentBytes = 0;
    const argv = ["cog", ...args].map(argument => {
        if (typeof argument !== "string" || argument.includes("\0")) {
            throw new Error("WASI arguments must be text without NUL characters.");
        }
        if (argument.length > MAX_ARGUMENT_BYTES - argumentBytes) {
            throw new Error("WASI argument limit reached.");
        }
        const bytes = encoder.encode(argument);
        argumentBytes += bytes.length + 1;
        if (argumentBytes > MAX_ARGUMENT_BYTES) {
            throw new Error("WASI argument limit reached.");
        }
        return bytes;
    });

    const stdout = { text: "", decoder: new TextDecoder("utf-8", { ignoreBOM: true }) };
    const stderr = { text: "", decoder: new TextDecoder("utf-8", { ignoreBOM: true }) };
    let outputBytes = 0;
    let memory;
    const preview1 = {
        __proto__: null,
        args_sizes_get(count, size) {
            const view = new DataView(memory.buffer);
            view.setUint32(count >>> 0, argv.length, true);
            view.setUint32(size >>> 0, argumentBytes, true);
            return 0;
        },
        args_get(pointers, buffer) {
            const data = new Uint8Array(memory.buffer);
            const view = new DataView(data.buffer);
            let offset = buffer >>> 0;
            pointers >>>= 0;
            for (const bytes of argv) {
                view.setUint32(pointers, offset, true);
                data.set(bytes, offset);
                view.setUint8(offset + bytes.length, 0);
                offset += bytes.length + 1;
                pointers += 4;
            }
            return 0;
        },
        environ_sizes_get(count, size) {
            const view = new DataView(memory.buffer);
            view.setUint32(count >>> 0, 0, true);
            view.setUint32(size >>> 0, 0, true);
            return 0;
        },
        environ_get() {
            return 0;
        },
        fd_write(fd, iov, count, written) {
            const stream = fd === 1 ? stdout : fd === 2 ? stderr : null;
            if (stream === null) return EBADF;
            const view = new DataView(memory.buffer);
            const start = iov >>> 0;
            const length = count >>> 0;
            let size = 0;
            for (let index = 0; index < length; index++) {
                const entry = start + index * 8;
                const offset = view.getUint32(entry, true);
                const bytes = view.getUint32(entry + 4, true);
                if (bytes > MAX_OUTPUT_BYTES - outputBytes) {
                    throw new Error("WASI output limit reached.");
                }
                const data = new Uint8Array(view.buffer, offset, bytes);
                stream.text += stream.decoder.decode(data, { stream: true });
                outputBytes += bytes;
                size += bytes;
            }
            view.setUint32(written >>> 0, size, true);
            return 0;
        },
        proc_exit(code) {
            throw new WasiExit(code >>> 0);
        },
        // Newer Rust toolchains may import these even when the program does not use them.
        clock_time_get(id, _precision, result) {
            if (id > 1) return EINVAL;
            const nanoseconds = id === 0
                ? BigInt(Date.now()) * 1000000n
                : BigInt(Math.round(performance.now() * 1000000));
            new DataView(memory.buffer).setBigUint64(result >>> 0, nanoseconds, true);
            return 0;
        },
        random_get(buffer, size) {
            const bytes = new Uint8Array(memory.buffer, buffer >>> 0, size >>> 0);
            for (let offset = 0; offset < bytes.length; offset += 65536) {
                crypto.getRandomValues(bytes.subarray(offset, offset + 65536));
            }
            return 0;
        },
        sched_yield() {
            return 0;
        },
    };

    // Any other import the guest declares reports "not implemented" instead of failing to link.
    for (const { module: namespace, name, kind } of WebAssembly.Module.imports(module)) {
        if (namespace === "wasi_snapshot_preview1" && kind === "function" && !(name in preview1)) {
            preview1[name] = () => ENOSYS;
        }
    }
    const instance = await WebAssembly.instantiate(module, { wasi_snapshot_preview1: preview1 });
    memory = instance.exports.memory;
    const start = instance.exports._start;
    if (!(memory instanceof WebAssembly.Memory) || typeof start !== "function") {
        throw new Error("The tutorial artifact is not a WASI command.");
    }
    let exitCode = 0;
    try {
        start();
    } catch (error) {
        if (!(error instanceof WasiExit)) throw error;
        exitCode = error.code;
    }
    return {
        stdout: stdout.text + stdout.decoder.decode(),
        stderr: stderr.text + stderr.decoder.decode(),
        exitCode,
    };
}
