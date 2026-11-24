# cogweb

A small HTTP/1.1 framework in Cog, with a local Cog tutorial-server example.
This package is developed separately from the Cog interpreter checkout; it
imports that checkout's standard library and does not copy or modify its code.

## Package layout

```text
src/http.cog                     bounded HTTP parser, request helpers, cookies, responses
src/router.cog                   method router with exact, parameter, wildcard, and prefix paths
src/cors.cog                     explicit-origin CORS and preflight policy
src/security.cog                 response security-header defaults and CSP helper
src/static_files.cog             root- and extension-allowlisted text files
ffi/gzip/                         optional async native gzip response compressor
examples/tutorial_server/        Cog rewrite of the local tutorial server
examples/tutorial_server/web/    browser page with Cog-only launch guidance
examples/tutorial_server/cog.lock pinned local package graph
DESIGN_DNA.md                    design constraints
AUDIT.md                         backend audit and remaining gaps
IMPLEMENTATION_PLAN.md           completed scope and validation
```

## HTTP API

Each connection carries one HTTP/1.1 request and one response, then closes.
The parser accepts `Content-Length`, normalizes header names to lowercase,
rejects duplicate headers and `Transfer-Encoding`, and enforces a 16 KiB header
limit plus the caller's request-body limit. `Request` exposes case-insensitive
headers, normalized media type, decoded query/form fields, and the standard JSON
parse result. It does not implement TLS, chunked encoding, keep-alive, or binary
request bodies.

```cog
const request = await http::read_request(socket, 96 * 1024)!!;
const route_id = routes.resolve(request.method, request.path);
const response = http::json_response(200, { ready: true });
await http::write_response(socket, response)!!;
```

`Request.query_param(name)` and `Request.form_param(name)` return the first
matching `application/x-www-form-urlencoded` field, decoding `%HH` and `+`.
`query_params` and `form_params` return all repeated values in wire order.
`request.is_json()` accepts `application/json` and `+json` media types;
`is_form_urlencoded()` checks form content. `request.json()` returns the standard
JSON parse result, so callers can handle malformed input explicitly.
`request.accepts(media_type)` understands exact Accept types, `type/*`, `*/*`,
and quality values, with more-specific ranges taking precedence. `cookie(name)`
returns the first cookie value without URL decoding (balanced outer quotes are
removed), and `bearer_token()` extracts a well-formed Bearer credential.
`html_escape` is for untrusted text/quoted attributes; `html_document` and
`html_page_response` accept trusted markup for their content argument.
Use `write_response_for(socket, request, response)` or
`write_bytes_response_for` when the request is available: they suppress the body
for HEAD while retaining the representation's Content-Length. The writer also
suppresses bodies for 204, 205, and 304; 204 omits Content-Length and 205 writes
Content-Length: 0.

```cog
routes.get("/users/:id", "user");
const matched = routes.resolve_match(request.method, request.path);
if matched != nil {
    const user_id = matched.param("id") ?? "";
}
```

Route parameters match one encoded path segment and are percent-decoded when
read through `RouteMatch.param`. Patterns may end in `*name` to capture the
remaining path, including slashes; the wildcard must be the final segment.
Exact paths win first; patterns use the greatest number of static segments and
then the greatest fixed length; prefixes use the longest registered prefix.
Prefix registrations must end in `/` (except `/`). `allowed_methods`
returns matching methods in registration order, and `method_not_allowed_response`
sets the `Allow` header. `resolve` remains available for ID-only dispatch.
The tutorial example exposes parameters with `GET /api/greet/:name`; repeated
`tag` query fields are returned in order. `GET /api/path/:root/*tail` shows a
terminal wildcard whose decoded value may contain slashes.

`http::Cookie::new(name, value)` creates a cookie with `Path=/`, `HttpOnly`,
and `SameSite=Lax`. Configure it with `set_path`, `set_domain`, `set_max_age`,
`set_secure`, `set_http_only`, and `set_same_site`, then add it to a response.
Responses retain multiple `Set-Cookie` headers. Cookie values use strict ASCII
cookie-octets; SameSite=None requires Secure. This does not sign, encrypt, or
persist sessions.

```cog
var session = http::Cookie::new("session", opaque_id);
session.set_path("/app");
session.set_max_age(3600);
session.set_secure(true); // use on HTTPS deployments
var response = http::json_response(200, { ready: true });
response.add_cookie(session);
```

## CORS and response security

`cors::Policy` takes explicit origin, method, and request-header allowlists.
`cors::apply(response, request, policy)` adds CORS headers to allowed origins;
`cors::preflight_response(request, policy)` returns a 204 or 403 for preflight
requests and `nil` for ordinary requests. Explicit-origin policies vary on
Origin, and wildcard origins cannot be combined with credentials. CORS controls
browser access; it does not authenticate users.

`security::apply_defaults(response)` adds nosniff, referrer, frame, and
permissions-policy headers without replacing values the application already
set. `security::with_content_security_policy` adds an explicit CSP; there is no default
CSP because applications have different script and style requirements. Add
these helpers in the response path so error and static responses receive the
same policy.

```cog
const policy = cors::Policy(
    origins: ["https://app.example"],
    methods: ["GET", "POST"],
    headers: ["content-type"],
);
const cors_response = cors::apply(response, request, policy)!!;
const hardened = security::apply_defaults(cors_response)!!;
```

Common 4xx/5xx text responses and `redirect_status_response` are available for
route dispatch. The redirect helper only accepts 301, 302, 303, 307, and 308;
`unauthorized_response(realm)` includes a Bearer challenge, and
`not_acceptable_response(message)` pairs with Accept negotiation.

`Request.body` is UTF-8 text. Cog currently cannot use its runtime `bytes`
value type in a field or parameter annotation, so the request type cannot expose
an arbitrary binary body. Responses can be text or JSON; callers with a byte
buffer can use `http::write_bytes_response(socket, response, body)`.

The optional [`ffi/gzip`](ffi/gzip/README.md) package uses Cog's C ABI v3 and a
system zlib-compatible library to compress response bytes asynchronously. It is
separate from the base package: applications opt in, negotiate
`Accept-Encoding`, and send the returned buffer with the HTTP byte writer.

`static_files::serve_text(root, url_path, extensions, max_bytes)` provides a
bounded UTF-8 text response with a MIME type inferred from the extension. It
rejects encoded paths, backslashes, dot segments, and extensions outside the
caller-provided allowlist (extension names are case-insensitive and have no
leading dot). Pass a trusted root and a short allowlist; the helper
checks the metadata length before opening, reads exactly that many bytes, and
probes at most one extra byte if the file may have grown. It does not prevent
symlink escapes, so it is not a filesystem sandbox. It does not serve binary
assets.

`static_files::serve_file(socket, request, root, url_path, extensions, max_bytes)`
serves binary assets. It applies the same path rules, allowlist, and size cap as
`serve_text`, but sends the raw bytes with `http::write_bytes_response_for`
and infers the MIME type from the extension, including `application/wasm`,
images, `woff2`, and `mjs`. It answers 404 for rejected or missing files and 413
above the cap. Unlike `serve_text` it writes to the socket itself, so the
caller cannot add headers afterwards. For other byte buffers, build the head with
`http::binary_response(status, content_type)` and send it with
`write_bytes_response(_for)`. Cog cannot yet store `bytes` in a struct field, so
the body is not part of `Response`. See `examples/binary_assets`.

The router maps method/path pairs to string route IDs. Cog does not have
callable types for general handler callbacks, so the application dispatches
those IDs itself. It supports exact routes, `:name` parameters, terminal
`*name` path wildcards, and method-specific prefixes, plus
GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS helpers.
Method matching is case-sensitive. HEAD resolves an explicit HEAD route first,
then falls back to the matching GET route; `allowed_methods` includes the
implicit HEAD capability for GET paths.

## Tutorial server

The example keeps the tutorial's `/api/status`, `/api/run`, redirect, and
allowlisted static-file routes. `/api/greet/:name` demonstrates repeated query
fields and `/api/path/:root/*tail` demonstrates a wildcard route.
It checks Host and Origin, limits JSON requests
to 96 KiB, source to 64 KiB, stdin to 16 KiB, and captured stdout plus stderr
to 128 KiB. Compilation and execution share a 5 second deadline. User bytecode
runs with `--restricted`. Up to two connection handlers run concurrently on
Cog's cooperative single-thread executor.
Documentation and example text files are served through the framework's
extension-allowlisted helper with a 1 MiB per-file limit; the tutorial page is
read from the configured cogweb root. Responses use the framework security
defaults, and allowed local origins can complete CORS preflight requests.

Run it from the Cog source checkout root so relative paths resolve to `docs/`,
`examples/`, and `target/`. Set `COGWEB_ROOT` to this workspace so the server
serves its updated tutorial page instead of the checkout's Node-based setup page:

```sh
cd ~/RustroverProjects/cog
COGWEB_ROOT="$HOME/cogweb" cog run "$HOME/cogweb/examples/tutorial_server"
```

The server builds the compiler with Cargo on first start. Set `COG_BIN` to an
absolute path to a compatible Cog executable to skip that build. Set `PORT` to
choose a nonzero port; the default is `4173`. For example:

```sh
COGWEB_ROOT="$HOME/cogweb" COG_BIN=/path/to/cog PORT=4173 \
  cog run "$HOME/cogweb/examples/tutorial_server"
```

The server process itself must run without `--restricted`, because it needs
network, filesystem, process, and environment access. User programs are run
with `--restricted`. That option limits Cog host effects; it is not an OS memory
sandbox.

The sample currently handles at most two connections at a time, which also caps
active tutorial runs at two. It does not yet cancel a running child immediately
when its client disconnects. Static files are text and are read relative to the
server's working directory.

## Dependencies and validation

- Framework: Cog's standard library only.
- Optional gzip extension: C11 compiler, pthreads, Cog C ABI v3 header, and a
  zlib-compatible development library; currently built for Linux x86_64.
- Tutorial server: the local `cogweb` package, Cargo for the optional compiler
  build, and the Cog CLI to launch the app.
- No Node.js, TypeScript, npm, or frontend build step is used. The page's small
  interactive script is browser-native JavaScript.
- `cog check /home/yan/cogweb` and `cog check
  /home/yan/cogweb/examples/tutorial_server` passed using the current compiler
  in `/tmp/cogweb-cargo-target`.
- `cog build /home/yan/cogweb/examples/tutorial_server -o
  /tmp/cogweb-tutorial-0.3-final.cogb` passed.
- No tests or live server session were run; runtime HTTP behavior remains
  unverified.

## Rollback

The work is isolated in `/home/yan/cogweb`. Remove that directory to roll it
back; the Cog source checkout remains unchanged until this staged replacement is
integrated.
