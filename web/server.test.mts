import assert from "node:assert/strict";
import { before, after, test } from "node:test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { readFile, access, mkdtemp, rm, readdir, mkdir, writeFile } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import { connect } from "node:net";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { runWasi } from "./wasi.mjs";
import { checkOutput } from "./wasi-browser.mjs";
import { buildCompiler, freePort, installLibrary, startTutorial } from "./server.mts";

let tutorial: Awaited<ReturnType<typeof startTutorial>>;
let origin: string;
// The tutorial server is the Cog program in web/tutorial, started through the same launcher
// users run. The first start builds the cogweb package and the server, which can take a while.
before(async () => {
    const compiler = await buildCompiler();
    await installLibrary(compiler);
    // Directories from a crashed earlier run: one matching the run pattern, one decoy.
    const runs = join(dirname(fileURLToPath(import.meta.url)), "..", "target", "tutorial-runs");
    await mkdir(join(runs, "cog-" + "0123456789abcdef".repeat(2)), { recursive: true });
    await writeFile(join(runs, "cog-" + "0123456789abcdef".repeat(2), "main.cog"), "fn main() {}");
    await mkdir(join(runs, "cog-keep-me"), { recursive: true });
    tutorial = await startTutorial(compiler, await freePort());
    origin = tutorial.origin;
}, { timeout: 900000 });
after(async () => {
    await rm(join(dirname(fileURLToPath(import.meta.url)), "..", "target", "tutorial-runs", "cog-keep-me"), { recursive: true, force: true });
    if (!tutorial) return;
    tutorial.child.removeAllListeners("close");
    if (tutorial.child.exitCode === null && tutorial.child.signalCode === null) {
        tutorial.child.kill("SIGTERM");
        await once(tutorial.child, "close");
    }
});

async function run(source: string, input = "") {
    const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: JSON.stringify({ source, input }) });
    assert.equal(response.status, 200);
    return response.json();
}

test("all thirteen tutorial programs compile and execute through real bytecode", async () => {
    const html = await readFile(new URL("./get_started.html", import.meta.url), "utf8");
    const sources = [...html.matchAll(/<textarea class="source"[^>]*>([\s\S]*?)<\/textarea>/g)].map(match => match[1].replaceAll("&lt;", "<").replaceAll("&gt;", ">").replaceAll("&quot;", '"').replaceAll("&#x27;", "'").replaceAll("&amp;", "&"));
    const outputs = ["Hello, Cog!\n", "Cog: 3\n", "42\n", "Cog: 1\nCog: 3\n", "2\n1\nsum=10, large\n", "[0, 9, 2, 3]\n4\n", "Plain: {name}\nHello, Ada! 2 + 3 = 5\n{Cog}\n", "Move to 2\nmoving\n", "3\n7\nParseFailure\nDone.\n", "42\nCog\n3\n", "right operand ran\nthe left Error survived the operation\n7\nthe selected array element is an Error\n", "Cog tools\nred / green / blue\n3\n", "42\n"];
    assert.equal(sources.length, outputs.length);
    for (let i = 0; i < sources.length; ++i) {
        const result = await run(sources[i], " Ada \n");
        assert.equal(result.ok, true, result.diagnostics);
        assert.equal(result.stdout, outputs[i]);
        assert.equal(result.diagnostics, "");
    }
});

test("syntax and type errors return compiler diagnostics without executing", async () => {
    for (const source of ['fn main() { const x = ; }', '@!include("std")\nuse io;\nfn main() { io::println("should not run"); const n: int = "wrong"; }']) {
        const result = await run(source);
        assert.equal(result.ok, false, source);
        assert.equal(result.phase, "compile");
        assert.equal(result.stdout, "");
        assert.match(result.diagnostics, /error\[E\d+\]/);
        assert.match(result.diagnostics, /main\.cog:/);
    }
});

test("runtime failure preserves earlier output and stdin reaches read_line", async () => {
    const failed = await run('@!include("std")\nuse io;\nfn main() { io::println("before"); (1 / 0)!!; }');
    assert.equal(failed.ok, false);
    assert.equal(failed.phase, "run");
    assert.equal(failed.stdout, "before\n");
    assert.match(failed.diagnostics, /division by zero/);
    const read = await run('@!include("std")\nuse io;\nfn main() { io::println(io::read_line()!!)!!; }', "café λ\n");
    assert.equal(read.ok, true, read.diagnostics);
    assert.equal(read.stdout, "café λ\n");
    const eof = await run('@!include("std")\nuse io;\nfn main() { io::read_line()!!; }');
    assert.equal(eof.ok, false);
});

test("runtime host effects are denied before touching files or starting processes", async () => {
    const path = new URL("./tutorial-must-not-create.txt", import.meta.url).pathname;
    for (const source of [
        `@!include("std")\nuse io;\nfn main() { io::open(${JSON.stringify(path)}, "w", 0)!!; }`,
        '@!include("std")\nuse process;\nfn main() { const args: str[] = []; process::run_sync("unused", args)!!; }',
        '@!include("std")\nuse time;\nfn main() { time::now()!!; }',
        '@!include("std")\nuse entropy;\nfn main() { entropy::bytes(1)!!; }',
    ]) {
        const result = await run(source);
        assert.equal(result.ok, false, source);
        assert.match(result.diagnostics, /permission denied/i);
    }
    await assert.rejects(access(path));
});

test("infinite loops and excess output stop and subsequent runs still work", async () => {
    const timeout = await run("fn main() { while true {} }");
    assert.equal(timeout.ok, false);
    assert.match(timeout.diagnostics, /time limit/);
    const output = await run('@!include("std")\nuse io;\nfn main() { while true { io::println("too much output"); } }');
    assert.equal(output.ok, false);
    assert.match(output.diagnostics, /Output limit/);
    const next = await run('@!include("std")\nuse io;\nfn main() { io::println("ready"); }');
    assert.equal(next.stdout, "ready\n");
    assert.equal(next.ok, true);
});

test("HTTP serves the tutorial, rejects foreign origins, and confines static files", async () => {
    const page = await fetch(origin + "/web/get_started.html");
    assert.equal(page.status, 200);
    assert.match(await page.text(), /Get started with Cog/);
    assert.equal((await fetch(origin + "/api/status")).status, 200);
    assert.equal((await fetch(origin + "/src/main.rs")).status, 404);
    assert.equal((await fetch(origin + "/web/%2e%2e%2fsrc/main.rs")).status, 404);
    const foreign = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: "https://example.invalid", "Content-Type": "application/json" }, body: '{}' });
    assert.equal(foreign.status, 403);
    const invalid = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: 'invalid' });
    assert.equal(invalid.status, 400);
    const empty = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: '{}' });
    assert.equal(empty.status, 400);
});

test("invalid payloads and oversized requests return useful HTTP errors", async () => {
    for (const body of ['null', '[]', 'true', '{"source":4}', '{"source":"fn main() {}","input":{}}']) {
        const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body });
        assert.equal(response.status, 400);
    }
    const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: JSON.stringify({source:"x".repeat(100 * 1024)}) });
    assert.equal(response.status, 413);
});

test("a stale configured executable is rejected before the server starts", async () => {
    const child = spawn(process.execPath, [new URL("./server.mts", import.meta.url).pathname], {env:{...process.env,COG_BIN:process.execPath,PORT:"0"}});
    let stderr = "";
    child.stderr.on("data", chunk => {stderr += chunk;});
    const [code] = await once(child,"close");
    assert.equal(code,1);
    assert.match(stderr,/does not support --restricted/);
});

test("the fixed WASM checker is served with the WASM MIME type and nothing else from target is", async () => {
    const response = await fetch(origin + "/api/tutorial-checker.wasm");
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "application/wasm");
    assert.equal(response.headers.get("x-content-type-options"), "nosniff");
    const bytes = new Uint8Array(await response.arrayBuffer());
    assert.deepEqual([...bytes.subarray(0, 4)], [0, 0x61, 0x73, 0x6d]);
    assert.equal(bytes.length, Number(response.headers.get("content-length")));
    for (const path of ["/target/tutorial/checker.wasm", "/api/tutorial-checker", "/api/other.wasm", "/web/server.mts", "/web/server.test.mts"]) {
        assert.equal((await fetch(origin + path)).status, 404, path);
    }
    assert.equal((await fetch(origin + "/api/tutorial-checker.wasm", { method: "POST" })).status, 405);
});

test("the checker endpoint rejects cross-site fetches", async () => {
    const response = await fetch(origin + "/api/tutorial-checker.wasm", { headers: { "Sec-Fetch-Site": "cross-site" } });
    assert.equal(response.status, 403);
});

test("browser modules are served as JavaScript and wasi files stay inside web", async () => {
    for (const name of ["wasi.mjs", "wasi-worker.mjs", "wasi-browser.mjs"]) {
        const response = await fetch(`${origin}/web/${name}`);
        assert.equal(response.status, 200, name);
        assert.equal(response.headers.get("content-type"), "text/javascript; charset=utf-8");
    }
    assert.equal((await fetch(origin + "/web/get_started.html")).headers.get("content-type"), "text/html; charset=utf-8");
    assert.equal((await fetch(origin + "/examples/wasm/run.mjs")).status, 404);
});

test("the WASM checker compares complete output exactly, including Unicode and whitespace", async () => {
    const module = await WebAssembly.compile(await (await fetch(origin + "/api/tutorial-checker.wasm")).arrayBuffer());
    const run = (args: string[]) => runWasi(module, args);
    const cases: [string, string, boolean][] = [
        ["", "", true],
        ["hello\n", "hello\n", true],
        ["hello\n", "hello", false],
        ["Καλημέρα 🦀\n", "Καλημέρα 🦀\n", true],
        ["é", "e\u0301", false],
        [" hi", "hi", false],
        ["a\r\nb", "a\nb", false],
    ];
    for (const [expected, actual, same] of cases) assert.equal(await checkOutput(run, expected, actual), same, JSON.stringify([expected, actual]));
    const invalid = await run(["check", "only-one"]);
    assert.equal(invalid.stdout, "invalid-request\n");
    await assert.rejects(checkOutput(run, "a\0", "a"), /NUL/);
    await assert.rejects(checkOutput(run, "x".repeat(97 * 1024), "x"), /too large/);
    await assert.rejects(checkOutput(async () => ({ stdout: "", stderr: "boom", exitCode: 1 }), "a", "a"), /status 1.*boom/);
    await assert.rejects(checkOutput(async () => ({ stdout: "??", stderr: "", exitCode: 0 }), "a", "a"), /unexpected/);
});

test("a tutorial page links the WASM checker only through the fixed endpoint", async () => {
    const html = await readFile(new URL("./get_started.html", import.meta.url), "utf8");
    assert.match(html, /wasi-browser\.mjs/);
    assert.match(html, /\/api\/tutorial-checker\.wasm/);
});

// Opt-in: runs the checker in a real Worker. Set COG_CHROMIUM to a Chromium/Chrome executable.
test("a real Chromium Worker runs the WASM checker", { skip: process.env.COG_CHROMIUM ? false : "set COG_CHROMIUM to enable" }, async () => {
    const profile = await mkdtemp(join(tmpdir(), "cog-chromium-"));
    const chromium = spawn(process.env.COG_CHROMIUM!, ["--headless=new", "--no-sandbox", "--disable-gpu", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"]);
    try {
        let log = "";
        const endpoint = await new Promise<string>((resolve, reject) => {
            chromium.stderr.setEncoding("utf8").on("data", text => {
                log += text;
                const found = /DevTools listening on (ws:\/\/\S+)/.exec(log);
                if (found) resolve(found[1]);
            });
            chromium.on("close", () => reject(new Error("Chromium exited early:\n" + log)));
        });
        const http = `http://${new URL(endpoint).host}`;
        const target = await (await fetch(`${http}/json/new?${encodeURIComponent(origin + "/web/wasi-browser-check.html")}`, { method: "PUT" })).json();
        const socket = new WebSocket(target.webSocketDebuggerUrl);
        await once(socket, "open");
        let id = 0;
        const pending = new Map<number, (value: any) => void>();
        socket.onmessage = event => { const message = JSON.parse(String(event.data)); pending.get(message.id)?.(message); };
        const evaluate = (expression: string) => new Promise<any>(resolve => { pending.set(++id, resolve); socket.send(JSON.stringify({ id, method: "Runtime.evaluate", params: { expression, returnByValue: true } })); });
        let text = "running";
        for (let attempt = 0; attempt < 100 && text === "running"; ++attempt) {
            await new Promise(done => setTimeout(done, 100));
            text = (await evaluate('document.getElementById("result")?.textContent ?? "running"')).result?.result?.value ?? "running";
        }
        socket.close();
        assert.deepEqual(JSON.parse(text), { verdicts: [true, false, true, false, true], error: null });
    } finally {
        chromium.kill("SIGKILL");
        await rm(profile, { recursive: true, force: true });
    }
});

test("the WASI host links guests that import clocks or unknown WASI functions", async () => {
    // Hand-assembled module: _start calls clock_time_get and an unimplemented import.
    const text = (value: string) => [value.length, ...new TextEncoder().encode(value)];
    const wasi = text("wasi_snapshot_preview1");
    const section = (id: number, body: number[]) => [id, body.length, ...body];
    const bytes = new Uint8Array([
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
        ...section(1, [3, 0x60, 3, 0x7f, 0x7e, 0x7f, 1, 0x7f, 0x60, 0, 1, 0x7f, 0x60, 0, 0]),
        ...section(2, [2, ...wasi, ...text("clock_time_get"), 0, 0, ...wasi, ...text("fd_unknown"), 0, 1]),
        ...section(3, [1, 2]),
        ...section(5, [1, 0, 1]),
        ...section(7, [2, ...text("memory"), 2, 0, ...text("_start"), 0, 2]),
        ...section(10, [1, 14, 0, 0x41, 0, 0x42, 0, 0x41, 8, 0x10, 0, 0x1a, 0x10, 1, 0x1a, 0x0b]),
    ]);
    const result = await runWasi(await WebAssembly.compile(bytes), []);
    assert.equal(result.exitCode, 0);
});

const repositoryRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

// Sends a raw request so tests can control the Host header, which fetch forbids.
function rawRequest(options: { method?: string; path: string; headers?: Record<string, string>; body?: string }): Promise<{ status: number; headers: Record<string, string | string[] | undefined>; text: string }> {
    return new Promise((resolve, reject) => {
        const url = new URL(origin);
        const request = httpRequest({ host: url.hostname, port: url.port, method: options.method ?? "GET", path: options.path, headers: options.headers, setHost: false }, response => {
            let text = "";
            response.setEncoding("utf8").on("data", chunk => { text += chunk; });
            response.on("end", () => resolve({ status: response.statusCode!, headers: response.headers, text }));
        });
        request.on("error", reject);
        request.end(options.body);
    });
}

test("a wrong Host header is rejected with security headers", async () => {
    const response = await rawRequest({ path: "/api/status", headers: { Host: "example.invalid" } });
    assert.equal(response.status, 403);
    assert.equal(response.headers["x-content-type-options"], "nosniff");
    const body = JSON.stringify({ source: "fn main() {}", input: "" });
    const rebound = await rawRequest({ method: "POST", path: "/api/run", headers: { Host: "evil.example:80", Origin: "http://evil.example:80", "Content-Type": "application/json", "Content-Length": String(body.length) }, body });
    assert.equal(rebound.status, 403);
    const localhost = await rawRequest({ path: "/api/status", headers: { Host: `localhost:${new URL(origin).port}` } });
    assert.equal(localhost.status, 200);
});

test("run requests need the local Origin and a JSON content type", async () => {
    const body = JSON.stringify({ source: "fn main() {}", input: "" });
    const post = (headers: Record<string, string>) => fetch(origin + "/api/run", { method: "POST", headers, body });
    assert.equal((await post({ "Content-Type": "application/json" })).status, 403, "missing Origin");
    assert.equal((await post({ Origin: origin, "Content-Type": "text/plain" })).status, 403, "wrong content type");
    assert.equal((await post({ Origin: origin })).status, 403, "default content type");
    assert.equal((await post({ Origin: origin, "Content-Type": "application/json; charset=utf-8" })).status, 200);
});

test("concurrent runs are queued and all complete", async () => {
    const results = await Promise.all([1, 2, 3].map(n => run(`@!include("std")\nuse io;\nfn main() { io::println("run ${n}"); }`)));
    results.forEach((result, index) => {
        assert.equal(result.ok, true, result.diagnostics);
        assert.equal(result.stdout, `run ${index + 1}\n`);
    });
});

test("silent clients do not block other requests", async () => {
    const url = new URL(origin);
    const idle = [connect({ host: url.hostname, port: Number(url.port) }), connect({ host: url.hostname, port: Number(url.port) })];
    try {
        await Promise.all(idle.map(socket => once(socket, "connect")));
        const started = Date.now();
        const response = await fetch(origin + "/api/status", { signal: AbortSignal.timeout(15000) });
        assert.equal(response.status, 200);
        assert.ok(Date.now() - started < 15000);
    } finally {
        idle.forEach(socket => socket.destroy());
    }
});

test("early errors still carry security headers and reveal no filesystem paths", async () => {
    const tooLarge = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: JSON.stringify({ source: "x".repeat(100 * 1024) }) });
    assert.equal(tooLarge.status, 413);
    assert.equal(tooLarge.headers.get("x-content-type-options"), "nosniff");
    assert.doesNotMatch(await tooLarge.text(), /\/home\/|target\/tutorial/);
    const url = new URL(origin);
    const socket = connect({ host: url.hostname, port: Number(url.port) });
    await once(socket, "connect");
    let raw = "";
    socket.setEncoding("utf8").on("data", chunk => { raw += chunk; });
    socket.on("error", () => {});
    socket.write("NOT A REQUEST\r\n\r\n");
    await once(socket, "close");
    assert.match(raw, /^HTTP\/1\.1 400/);
    assert.match(raw.toLowerCase(), /x-content-type-options: nosniff/);
    assert.doesNotMatch(raw, /\/home\/|target\/tutorial/);
});

test("diagnostics never reveal run directories or absolute paths", async () => {
    const forbidden = [/target\/tutorial-runs/, /\/home\//, new RegExp(repositoryRoot.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))];
    const compile = await run('fn main() { const x = ; }');
    const failure = await run('@!include("std")\nuse io;\nfn main() { io::println("before"); (1 / 0)!!; }');
    assert.equal(compile.phase, "compile");
    assert.equal(failure.phase, "run");
    assert.match(compile.diagnostics, /main\.cog:1:/);
    assert.match(failure.diagnostics, /division by zero/);
    assert.match(failure.diagnostics, /main\.cog:3:/);
    for (const text of [compile.diagnostics, failure.diagnostics]) {
        for (const pattern of forbidden) assert.doesNotMatch(text, pattern);
    }
});

test("limit messages are stable and run directories are removed", async () => {
    const timeout = await run("fn main() { while true {} }");
    assert.equal(timeout.diagnostics, "Execution time limit reached. Check for an infinite loop.");
    const output = await run('@!include("std")\nuse io;\nfn main() { while true { io::println("too much output"); } }');
    assert.equal(output.diagnostics, "Output limit reached. Print fewer values.");
    const leftovers = (await readdir(join(repositoryRoot, "target", "tutorial-runs")).catch(() => [] as string[])).filter(name => name.startsWith("cog-"));
    // The startup sweep removed the stale run directory but left the decoy alone.
    assert.deepEqual(leftovers, ["cog-keep-me"]);
});
