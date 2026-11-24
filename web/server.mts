import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:net";
import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const library = join(root, "packages", "cogweb");

// Cargo reports the executable path, including any configured target triple.
export async function buildCompiler(): Promise<string> {
    const child = spawn("cargo", ["build", "--locked", "--offline", "--bin", "cog", "--target-dir", join(root, "target", "tutorial"), "--message-format=json"], { cwd: root });
    let output = "";
    child.stdout.setEncoding("utf8").on("data", text => { output += text; });
    child.stderr.pipe(process.stderr);
    const [code] = await once(child, "close");
    if (code !== 0) throw new Error("Cog build failed. Run cargo build --locked to fetch missing dependencies, then retry.");
    for (const line of output.trim().split("\n")) {
        const message = JSON.parse(line);
        if (message.reason === "compiler-artifact" && message.target.name === "cog" && message.executable) {
            await validateCompiler(message.executable);
            return message.executable;
        }
    }
    throw new Error("Cargo did not report the Cog executable.");
}

export async function validateCompiler(compiler: string): Promise<void> {
    const child = spawn(compiler, ["--help"]);
    let help = "";
    child.stdout.on("data", chunk => { help += chunk; });
    child.stderr.on("data", chunk => { help += chunk; });
    const [code] = await once(child, "close");
    if (code !== 0 || !help.includes("--restricted")) {
        throw new Error("This Cog executable does not support --restricted. Unset COG_BIN to build the current compiler, then restart the tutorial.");
    }
}

async function sourceFiles(path: string): Promise<string[]> {
    const info = await stat(path);
    if (!info.isDirectory()) return [path];
    const files: string[] = [];
    for (const entry of (await readdir(path)).sort()) files.push(...await sourceFiles(join(path, entry)));
    return files;
}

async function sourceDigest(...paths: string[]): Promise<string> {
    const hash = createHash("sha256");
    for (const path of paths) {
        for (const file of await sourceFiles(path)) hash.update(file).update(await readFile(file));
    }
    return hash.digest("hex");
}

const stamp = join(root, "target", "tutorial", "cogweb-install.stamp");

// The library is installed when a stamp records the digest of the current sources, the compiler,
// and the store, and a release of that version still exists there.
async function libraryStamp(compiler: string): Promise<string> {
    const sources = await sourceDigest(join(library, "src"), join(library, "package.kdl"));
    return [sources, compiler, storeDirectory()].join("\n");
}

function storeDirectory(): string {
    return join(process.env.COG_HOME ?? join(homedir(), ".cog"), "libraries");
}

async function libraryIsInstalled(compiler: string): Promise<boolean> {
    try {
        if ((await readFile(stamp, "utf8")) !== await libraryStamp(compiler)) return false;
        const version = /version="([^"]+)"/.exec(await readFile(join(library, "package.kdl"), "utf8"))?.[1];
        return (await readdir(storeDirectory())).some(name => name.startsWith("cogweb-"));
    } catch { return false; }
}

// Builds packages/cogweb into a .cogpkg and installs it in $COG_HOME/libraries (default ~/.cog/libraries).
export async function installLibrary(compiler: string): Promise<void> {
    if (await libraryIsInstalled(compiler)) return;
    console.log("Installing the cogweb library into the local Cog store…");
    const child = spawn(compiler, ["install", "--path", library, "--color", "never"], { cwd: root, stdio: ["ignore", "inherit", "inherit"] });
    const [code] = await once(child, "close");
    if (code !== 0) throw new Error("Could not install packages/cogweb. Run cog install --path packages/cogweb for details.");
    await mkdir(dirname(stamp), { recursive: true });
    await writeFile(stamp, await libraryStamp(compiler));
}

export async function freePort(): Promise<number> {
    const probe = createServer();
    probe.listen(0, "127.0.0.1");
    await once(probe, "listening");
    const { port } = probe.address() as { port: number };
    probe.close();
    await once(probe, "close");
    return port;
}

// Starts the Cog tutorial server (web/tutorial) and resolves once it announces its URL.
export async function startTutorial(compiler: string, port: number): Promise<{ child: ChildProcess; origin: string }> {
    const child = spawn(compiler, ["run", join("web", "tutorial"), "--color", "never"], {
        cwd: root,
        env: { ...process.env, COG_BIN: compiler, PORT: String(port) },
        stdio: ["ignore", "pipe", "inherit"],
    });
    // Register before the server is ready so a signal during startup cannot orphan the child.
    for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => { child.kill(signal); process.exitCode = 130; });
    process.once("exit", () => { child.kill("SIGTERM"); });
    let buffered = "";
    return await new Promise((resolveStart, reject) => {
        child.stdout!.setEncoding("utf8").on("data", text => {
            buffered += text;
            if (buffered.includes("Cog tutorial: ")) {
                child.stdout!.removeAllListeners("data");
                process.stdout.write(buffered);
                child.stdout!.pipe(process.stdout);
                resolveStart({ child, origin: `http://127.0.0.1:${port}` });
            }
        });
        child.on("error", reject);
        child.on("close", code => reject(new Error(`The Cog tutorial server exited with status ${code} before it was ready.\n${buffered}`)));
    });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    try {
        const compiler = process.env.COG_BIN ? resolve(process.env.COG_BIN) : await buildCompiler();
        await validateCompiler(compiler);
        await installLibrary(compiler);
        const port = Number(process.env.PORT ?? 4173);
        const { child } = await startTutorial(compiler, port);
        child.removeAllListeners("close");
        child.on("close", code => { process.exitCode = code ?? 1; });
    } catch (error) { console.error(error instanceof Error ? error.message : error); process.exitCode = 1; }
}
