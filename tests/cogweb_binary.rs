//! End-to-end regression for the cogweb binary-serving API
//! (`http::binary_response` and `static_files::serve_file`).
//!
//! The `binary_assets` example is copied into a scratch directory, patched to a
//! free port and a small size cap, started with the compiler under test, and
//! queried over plain TCP. Building the example takes tens of seconds.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const SIZE_CAP: usize = 1024;
const WASM_BYTES: &[u8] = b"\0asm\x01\0\0\0\xff\x80";

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn request(port: u16, method: &str, path: &str) -> Response {
    request_with_body(port, method, path, &[])
}

fn request_with_body(port: u16, method: &str, path: &str, body: &[u8]) -> Response {
    request_with_content_type(port, method, path, "", body)
}

fn request_with_content_type(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    if !content_type.is_empty() {
        write!(stream, "Content-Type: {content_type}\r\n").unwrap();
    }
    write!(stream, "\r\n").unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    // The server closes the connection after the response; a reset after the
    // data has arrived is tolerated.
    let _ = stream.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {raw:?}"));
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.trim().to_string(), value.trim().to_string())
        })
        .collect();
    Response {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

fn prepare_example(scratch: &Path, port: u16) {
    prepare_example_with_cap(scratch, port, &SIZE_CAP.to_string());
}

fn prepare_example_with_cap(scratch: &Path, port: u16, cap: &str) {
    let cogweb = Path::new(env!("CARGO_MANIFEST_DIR")).join("packages/cogweb");
    let source = cogweb.join("examples/binary_assets");
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch.join("src")).unwrap();
    fs::create_dir_all(scratch.join("assets")).unwrap();
    fs::copy(source.join("package.kdl"), scratch.join("package.kdl")).unwrap();
    fs::write(
        scratch.join("dependencies.kdl"),
        format!(
            "cogweb path=\"{}\"\n",
            cogweb.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let main = fs::read_to_string(source.join("src/main.cog")).unwrap();
    let patched = main
        .replace("const PORT = 4176;", &format!("const PORT = {port};"))
        .replace("4 * 1024 * 1024", cap);
    assert!(patched.contains(&format!("const PORT = {port};")));
    assert!(patched.contains(&format!(", {cap})")));
    assert!(!patched.contains("4 * 1024 * 1024"));
    fs::write(scratch.join("src/main.cog"), patched).unwrap();

    let all_bytes: Vec<u8> = (0..=255u8).collect();
    fs::write(scratch.join("assets/all.bin"), all_bytes).unwrap();
    fs::write(scratch.join("assets/module.wasm"), WASM_BYTES).unwrap();
    fs::write(scratch.join("assets/big.bin"), vec![7u8; SIZE_CAP + 1]).unwrap();
    fs::write(scratch.join("assets/notes.txt"), b"not allowed").unwrap();
}

fn start_server(scratch: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .current_dir(scratch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let diagnostics = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let collected = diagnostics.clone();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        *collected.lock().unwrap() = text;
    });
    let server = Server { child };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("cogweb binary assets") {
                let _ = sender.send(());
            }
        }
    });
    if receiver.recv_timeout(Duration::from_secs(240)).is_err() {
        panic!(
            "example server did not report that it is listening; stderr: {}",
            diagnostics.lock().unwrap()
        );
    }
    server
}

fn scratch_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[test]
fn cogweb_binary_assets_example_serves_bytes_and_rejects_bad_paths() {
    let port = free_port();
    let scratch = scratch_dir("cogweb_binary_assets");
    prepare_example(&scratch, port);
    let _server = start_server(&scratch);

    // Raw handler bytes: non-UTF-8 content survives and content-length is exact.
    let blob = request(port, "GET", "/blob");
    assert_eq!(blob.status, 200);
    assert_eq!(blob.body, [0x00, 0xff, 0x80, 0x0a, 0x0d]);
    assert_eq!(blob.header("content-length"), Some("5"));
    assert_eq!(
        blob.header("content-type"),
        Some("application/octet-stream")
    );

    // Static files are byte-identical with the right MIME types.
    let all: Vec<u8> = (0..=255u8).collect();
    let bin = request(port, "GET", "/assets/all.bin");
    assert_eq!(bin.status, 200);
    assert_eq!(bin.body, all);
    assert_eq!(bin.header("content-length"), Some("256"));
    assert_eq!(bin.header("content-type"), Some("application/octet-stream"));

    let wasm = request(port, "GET", "/assets/module.wasm");
    assert_eq!(wasm.status, 200);
    assert_eq!(wasm.body, WASM_BYTES);
    assert_eq!(wasm.header("content-type"), Some("application/wasm"));

    // Rejections.
    assert_eq!(request(port, "GET", "/assets/missing.bin").status, 404);
    assert_eq!(request(port, "GET", "/assets/notes.txt").status, 404);
    assert_eq!(request(port, "GET", "/assets/../src/main.cog").status, 404);
    assert_eq!(request(port, "GET", "/assets/big.bin").status, 413);

    // HEAD carries headers only.
    let head = request(port, "HEAD", "/assets/all.bin");
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("256"));
    assert_eq!(
        head.header("content-type"),
        Some("application/octet-stream")
    );
    assert!(head.body.is_empty());
}

#[test]
fn cogweb_request_body_preserves_binary_bytes_and_utf8_text_api() {
    let port = free_port();
    let scratch = scratch_dir("cogweb_request_body");
    prepare_example(&scratch, port);
    let _server = start_server(&scratch);

    let binary = [0x00, 0xff, 0x80, 0x0a, 0x0d];
    let echoed = request_with_body(port, "POST", "/echo", &binary);
    assert_eq!(echoed.status, 200);
    assert_eq!(echoed.body, binary);
    assert_eq!(echoed.header("content-length"), Some("5"));

    let text = "Cog ☃ request".as_bytes();
    let echoed = request_with_body(port, "POST", "/text", text);
    assert_eq!(echoed.status, 200);
    assert_eq!(echoed.body, text);

    let not_text = request_with_body(port, "POST", "/text", &binary);
    assert_eq!(not_text.status, 415);
    assert_eq!(not_text.body, b"Body is not UTF-8");

    let valid_form = request_with_content_type(
        port,
        "POST",
        "/form",
        "application/x-www-form-urlencoded",
        b"value=hello",
    );
    assert_eq!(valid_form.status, 200);
    assert_eq!(valid_form.body, b"hello");

    let invalid_form = request_with_content_type(
        port,
        "POST",
        "/form",
        "application/x-www-form-urlencoded",
        &binary,
    );
    assert_eq!(invalid_form.status, 400);
    assert_eq!(invalid_form.body, b"Invalid form body");

    let valid_json = request_with_content_type(port, "POST", "/json", "application/json", b"{}");
    assert_eq!(valid_json.status, 200);
    assert_eq!(valid_json.body, b"Valid JSON");

    let invalid_json =
        request_with_content_type(port, "POST", "/json", "application/json", &binary);
    assert_eq!(invalid_json.status, 400);
    assert_eq!(invalid_json.body, b"Invalid JSON body");
}

#[test]
fn cogweb_serve_file_surfaces_a_negative_size_cap_instead_of_answering_not_found() {
    let port = free_port();
    let scratch = scratch_dir("cogweb_binary_assets_negative_cap");
    prepare_example_with_cap(&scratch, port, "-1");
    let mut server = start_server(&scratch);

    // The caller bug propagates out of the handler: the connection closes without
    // any response, in particular without a 404.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "GET /assets/all.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    assert!(
        !String::from_utf8_lossy(&raw).contains("404"),
        "negative cap must not map to 404: {raw:?}"
    );
    assert!(raw.is_empty(), "no response is expected: {raw:?}");
    server.child.kill().unwrap();
}
