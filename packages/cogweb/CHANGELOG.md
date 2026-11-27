# Changelog

## 0.6.0

- Requires a Cog compiler that checks nullable types.
- `Request.body_bytes` exposes the bounded raw request body. Existing UTF-8
  callers can keep using `Request.body`; `body_text()` reports invalid UTF-8 as
  `nil` instead of rejecting the request. Form helpers raise
  `Error::ParseFailure("http", "request body UTF-8")` for invalid text, and
  `Request.json()` returns that Error value rather than parsing an empty string.

## 0.5.0

- Requires a Cog compiler with integer shift operators. Library sources add array
  elements with `.append(value)` instead of the removed `<<` array operator. The
  public API is unchanged.

## 0.4.0

- Added `static_files::serve_file` for bounded binary assets and
  `http::binary_response` for response heads with separately sent bytes. Added
  MIME types for `wasm`, `mjs`, images, and `woff2`. Added the `binary_assets`
  example.
- Added an optional async C ABI v3 gzip extension using the system's
  zlib-compatible library. It is packaged separately and does not add a native
  dependency to the base `cogweb` framework.

## 0.3.0

- Added repeated query/form values, JSON/form media checks, Accept quality
  negotiation, request-cookie lookup, and Bearer token extraction.
- Added cookie builders and support for multiple Set-Cookie response headers.
- Added explicit CORS origin/method/header policies, preflight responses, and
  Vary handling.
- Added opt-in response security defaults and a header-safe CSP helper; the
  tutorial server now applies these defaults and handles local preflight.
- Added terminal catch-all route parameters and expanded common HTTP reason
  phrases, error responses, and redirect status selection.
- Corrected 205 responses to suppress content and advertise a zero-length body.

## 0.2.0

- Added method-specific route registration for PUT, PATCH, DELETE, HEAD, and
  OPTIONS, plus exact and trailing-slash prefix routes.
- Added whole-segment `:name` path parameters, percent-decoded access through
  `RouteMatch.param`, specificity-based matching, and `allowed_methods`.
- Added normalized media-type lookup, JSON and URL-encoded form helpers, HTML
  escaping/document helpers, and 204/404/405 response helpers.
- Added case-sensitive method matching, HEAD-to-GET route fallback, and
  request-aware response writers that omit bodies for HEAD/204/304.
- Added root- and extension-allowlisted UTF-8 static-file responses with a
  metadata size check, exact-length read, one-byte growth probe, and MIME
  selection.
- Updated the tutorial server to use `Allow`-bearing 405 responses and accept
  JSON media types with optional parameters.

## 0.1.0

- Added a bounded HTTP/1.1 request parser, text and JSON response helpers, a raw
  byte response writer, and a method/path router.
- Added a local tutorial-server application with the existing route checks,
  source/input/process limits, restricted bytecode execution, and a two-handler
  cooperative accept loop.
- Added a staged tutorial-page copy with Cog launch instructions; the browser
  uses native JavaScript and the replacement server has no Node/TypeScript
  runtime dependency.
- Documented Cog backend constraints, launch requirements, validation, and
  rollback.
