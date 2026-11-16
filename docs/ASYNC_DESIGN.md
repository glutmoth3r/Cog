# Async execution and I/O

Status: the task foundation, asynchronous timer/filesystem/process/network APIs,
statically dispatched async methods, and native plugin async operations are
implemented. This document records their current behavior and remaining
cancellation, concurrency, and ownership decisions.

## Source contract

An `@async fn name(args...) -> T` declaration keeps `T` as the function body's
result type. Calling it creates a cold `Task[T]`; its body does not run until the
task is started or awaited.

```cog
@async fn load() -> Data {
    // The body starts when this task is awaited or spawned.
    fetch_data()
}

@async fn main() -> nil {
    const data = await load();
    use_data(data);
}
```

`await expression` requires a `Task[T]` and produces `T`. It is valid only inside
an async function. A synchronous function may construct and pass a task, but cannot
await it. `Task[T]` is a reserved generic runtime type; users cannot construct one
with a record literal.

`spawn(task)` queues a task on the current executor and returns the same task handle.
It is the explicit way to run independent tasks concurrently. A queued task starts
when the executor reaches an `await` or joins the spawning scope. Discarding a cold
`Task` does not run its body. A diagnostic for discarded task results remains open.

```cog
@async fn main() -> nil {
    const data_task = spawn(load_data());
    const cookie_task = spawn(load_cookie());
    const data = await data_task;
    const cookie = await cookie_task;
    use_both(data, cookie);
}
```

An async `main` is awaited by the host. A synchronous `main` keeps its current
behavior. Async inherent receiver methods and associated functions follow the same
contract: calls create cold `Task[T]` values and run only when awaited or spawned.
Trait methods may also be declared `@async`; their implementations must match the
async marker and calls use the same cold-task contract. Trait dispatch remains static
through the existing generic specialization model. Async closures and dynamic trait
objects remain unsupported.

## Task lifetime and scheduling

Tasks are single-execution, shared handles scoped to one program invocation. Their
completed value or Error is memoized, and later awaits observe the same outcome
without repeating side effects. Cold tasks do not run unless awaited or queued with
`spawn`. When the final handle to a cold task is dropped, the scheduler releases its
captured arguments at the next VM instruction boundary. Started tasks remain owned
by their awaiters or structured parent until completion and joining.
The heap collector treats cold scheduler rows and captured Cog heap objects as one
reachability graph. It reclaims cycles without an external task handle or heap
reference at collection points, and before managed allocation under heap quota
pressure. Queued, running, and completed tasks are not candidates for this pass.

The Cog executor is single-threaded and cooperative. It keeps ready tasks in FIFO
order and switches tasks at `await` or while joining spawned children. Ordinary Cog
code is not preempted, so CPU-bound code does not run in parallel or yield
automatically. Async filesystem and process operations run on a bounded worker pool;
timer operations use executor deadlines. When no Cog task is ready, the executor
waits for the next timer or worker completion. Completion order for worker operations
depends on the host OS and operation durations.

An incomplete `await` saves the current VM continuation (instruction pointer,
operand stack, locals, call frames, and captures) and returns control to the FIFO
scheduler. Completion queues waiting tasks for resumption. The wait graph detects
cycles through direct awaits and structured child joins. Async task nesting therefore
does not grow the Rust call stack; ordinary synchronous calls and native/plugin calls
remain on the current VM turn.

The worker pool starts on first use, has four threads, and accepts up to 128 queued
jobs. The task table is capped at one million handles. These limits bound scheduler
and host-thread use, but do not bound whole-process memory. Existing synchronous I/O
and plugin calls may still block the executor. Async operations do not make CPU-bound
Cog code parallel.

## Async standard-library operations

These calls create cold tasks. They start when awaited or spawned, and their
capability checks and I/O happen when the task starts.
Asynchronous standard-library operations carry `async` in the module path, not
as a function-name suffix. Importing a submodule gives it a short local name,
for example `use async::net;` followed by `net::connect(host, port, timeout_ms)`.
`process::run` is async by default; its optional stdin and deadline share that
entry point. `process::run_sync` is the blocking process-capture operation.

| Call | Task result | Behavior |
| --- | --- | --- |
| `yield_now()` | `Task[nil]` | Completes on the scheduler queue, letting tasks already ready run before its awaiter resumes |
| `async::time::sleep(seconds)` | `Task[nil]` | Completes after the validated timer duration without blocking the executor |
| `async::time::timeout(task, seconds)` | `Task[T]` | Returns the source result or recoverable `Error::Timeout`; timeout does not cancel the source task |
| `task::select(tasks)` | `Task[int]` | Returns the index of the first completed task from a nonempty array of spawned siblings |
| `task::select2(first, second)` | `Task[int]` | Two-task convenience form; returns `0` or `1` for the first completed spawned sibling |
| `async::fs::metadata(path)` | `Task[record]` | Reads metadata on the worker pool, subject to `allow_fs_read` |
| `async::fs::read_dir(path)` | `Task[str[]]` | Lists sorted UTF-8 entry names, subject to `allow_fs_read` and the runtime buffer limit |
| `async::fs::read_bytes(path)` | `Task[bytes]` | Reads the file on the worker pool, subject to `allow_fs_read` and the runtime buffer limit |
| `async::fs::write_bytes(path, bytes)` | `Task[nil]` | Creates or truncates a file and writes the bytes, subject to `allow_fs_write` and the runtime buffer limit |
| `async::fs::read_text(path)` | `Task[str]` | Reads UTF-8 text on the worker pool; invalid UTF-8 is `ParseFailure` |
| `async::fs::write_text(path, text)` | `Task[nil]` | Snapshots and writes UTF-8 bytes, subject to `allow_fs_write` and the runtime buffer limit |
| `async::fs::read_lines(path)` | `Task[str[]]` | Reads UTF-8 lines without terminators, subject to `allow_fs_read` and the runtime buffer limit |
| `async::fs::append_bytes(path, bytes)` | `Task[nil]` | Appends a snapshot of bytes, subject to `allow_fs_write` and the runtime buffer limit |
| `async::fs::append_text(path, text)` | `Task[nil]` | Appends a snapshot of UTF-8 text, subject to `allow_fs_write` and the runtime buffer limit |
| `async::fs::copy(source, destination)` | `Task[uint]` | Copies a file on the worker pool and returns bytes copied, subject to `allow_fs_write` |
| `async::fs::rename(source, destination)` | `Task[nil]` | Renames a file or directory on the worker pool, subject to `allow_fs_write` |
| `async::fs::create_dir(path)` / `async::fs::create_dir_all(path)` | `Task[nil]` | Creates a directory or directory tree on the worker pool |
| `async::fs::remove_file(path)` / `async::fs::remove_dir(path)` | `Task[nil]` | Removes a file or empty directory on the worker pool |
| `async::io::open(path, mode, buffer_size)` | `Task[handle]` | Opens a managed file on the worker pool and reserves a handle slot before opening |
| `async::io::read(handle, max_bytes)` | `Task[bytes]` | Reads from a managed file or `io::stdin()`, bounded by `max_buffer_bytes` |
| `async::io::read_exact(handle, size)` | `Task[bytes]` | Reads exactly `size` bytes from a file or stdin, or returns `Error::IoFailure` on early EOF |
| `async::io::read_all(handle)` | `Task[bytes]` | Reads from the current position through EOF, bounded by `max_buffer_bytes`; stdin completes at EOF |
| `async::io::write(handle, bytes)` | `Task[uint]` | Makes one write, flushes accepted bytes, and returns the accepted count; supports `io::stdout()` |
| `async::io::write_all(handle, bytes)` | `Task[nil]` | Writes a snapshot to a managed file or `io::stdout()` and flushes before completion |
| `async::io::flush(handle)` | `Task[nil]` | Flushes a managed file or `io::stdout()` |
| `async::io::seek(handle, offset, origin)` | `Task[int]` | Seeks an existing managed file handle; origin is `"start"`, `"current"`, or `"end"` |
| `process::run(program, arguments)` | `Task[record]` | Captures a child process on the worker pool, subject to `allow_exec` and the per-stream runtime output limit, if configured |
| `process::run(program, arguments, timeout_ms)` | `Task[record]` | An int or uint third argument sets a deadline; timeout stops the Unix process group and reaps the direct child (direct child only on other platforms) |
| `process::run(program, arguments, stdin)` | `Task[record]` | A bytes third argument is sent to child stdin while stdout/stderr are captured |
| `process::run(program, arguments, stdin, timeout_ms)` | `Task[record]` | Sends a byte snapshot to stdin and applies a process-specific deadline, or no deadline when `nil` |
| `process::run(program, arguments, timeout_ms, max_output_bytes)` | `Task[record]` | Applies a deadline and limits combined stdout/stderr capture; pass `nil` for no deadline |
| `process::run(program, arguments, stdin, timeout_ms, max_output_bytes)` | `Task[record]` | Sends stdin, optionally applies a deadline, and limits combined stdout/stderr capture |
| `async::net::exchange(host, port, payload, timeout_ms)` | `Task[bytes]` | Resolves and connects on the worker pool, sends bytes, half-closes the write side, then reads to EOF; requires `allow_network` |
| `async::net::listen(host, port)` | `Task[handle]` | Binds a nonblocking TCP listener on a worker; requires `allow_network` |
| `async::net::accept(listener, accept_timeout_ms, stream_timeout_ms)` | `Task[handle]` | Accepts one connection; the resulting stream uses the supplied I/O timeout |
| `async::net::connect(host, port, timeout_ms)` | `Task[handle]` | Opens a context-scoped TCP stream; the timeout also bounds each later socket read or write |
| `async::net::read(handle, max_bytes)` | `Task[bytes]` | Reads up to the requested size; an empty result means peer EOF |
| `async::net::read_exact(handle, size)` | `Task[bytes]` | Reads exactly `size` bytes or returns `Error::IoFailure` on early EOF |
| `async::net::read_all(handle)` | `Task[bytes]` | Reads through peer EOF, bounded by `max_buffer_bytes` and the stream timeout |
| `async::net::write(handle, bytes)` | `Task[uint]` | Makes one write and returns the accepted byte count |
| `async::net::write_all(handle, bytes)` | `Task[nil]` | Snapshots and writes the complete byte buffer |
| `async::net::shutdown_write(handle)` | `Task[nil]` | Half-closes the local write side while keeping reads available |

In this example, both tasks start before either result is awaited, so the file read
can overlap the timer:

```cog
use async::fs;
use async::time;

@async fn load_after_minimum(path: str) -> bytes {
    const timer = spawn(time::sleep(0.05));
    const file = spawn(fs::read_bytes(path));
    const data = await file;
    await timer;
    data
}
```

`async::time::sleep` and `async::time::timeout` use the same duration, negative-value,
precision, and representability rules as `time::sleep`. `async::time::timeout` starts its
source task when the timeout task starts. If the deadline wins, the wrapper completes
with `Error::Timeout`; the source continues unless its owner separately cancels it.
If the source is a structured child, its parent still joins it and reports a later
unobserved failure. Keep the source handle when the caller needs to inspect its result
or cancel it explicitly.
`task::select` requires a nonempty array of tasks already started with `spawn` under
the same structured parent. `task::select2` has the same contract for its two inputs.
Both return the input index of whichever task completes first; if several are complete
when selection starts, scheduler completion order wins, with the lowest input index
breaking ties for duplicate handles. Selection does not observe any outcome, so await
the selected task to receive its value or Error. Other tasks continue, and their parent
still joins them and reports unobserved failures. Cancelling the selection task removes
its waiter registrations without cancelling any input task.
The synchronous `time::sleep` remains blocking. Async filesystem paths resolve from
the process working directory. Metadata
retains the synchronous API's symlink behavior; directory reads return UTF-8 names in
byte order and fail without a partial array. `async::fs::write_bytes` snapshots its input
when the task starts, then creates or truncates the target file. Filesystem reads and
listings use `allow_fs_read`; writes and mutations use `allow_fs_write`.
`async::fs::read_lines` requires valid UTF-8 and follows `str::lines` terminator behavior.
Append operations create a missing target and do not truncate existing content; they
snapshot input before submitting the worker job. Concurrent appends have no ordering
or atomic-record guarantee, and a host write failure may leave a partial append. The
process result has the same
`exit_code`, `success`, `stdout`, and `stderr` fields as `process::run`; a nonzero
exit status is still a successful result record.

Task failures follow the existing Cog Error rules: awaiting a task returns its
value, produces a recoverable `Error` value for recoverable failures, or propagates
a fatal runtime error. Invalid arguments, denied capabilities, queue exhaustion,
OS errors, and configured size limits use those same runtime error categories.

`async::io::open` uses the same modes and buffer-size rules as `io::open`. It reserves
a resource slot before submitting the host open, so concurrent opens cannot exceed
`max_resources` or truncate a file before discovering that no handle slot is
available. Capability checks happen before the worker call. A cancellation that races
with a completed create/truncate can leave that filesystem side effect, but the new
handle is dropped and its reserved slot released. Open calls already in the host
filesystem cannot be interrupted.

Async and synchronous operations share the same buffered stdin and stdout writer.
Their operations are serialized on each standard stream, so `io::read_line` and
`async::io::read` do not lose bytes to separate buffers. Standard-stream operations
use the same `allow_read` / `allow_print` capabilities as their synchronous forms.
Managed-file operations share a lock, so their effects are serialized. File reads
and writes use the same `allow_fs_read` / `allow_fs_write` and handle-mode checks as
`io::read_into`, `io::write_all`, and `io::flush`. Read sizes and write snapshots obey
`max_buffer_bytes`; a zero-byte `read` or `read_exact` returns an empty buffer.
`read_exact` returns `Error::IoFailure` if EOF arrives before the requested size;
timeout is not applicable to managed-file reads, and cancellation may leave the file
position advanced through a partial prefix. `read_all` fails with
`Error::ResourceLimit` if the remaining content exceeds that cap; the stream may have
consumed the byte that confirms the excess. Cancellation is checked between 8 KiB
chunks, though an active host call or waiting on another operation for the same
stream can delay it. Blocking stdin reads cannot be interrupted while the host reader
is blocked; cancellation is observed after that read returns. An empty `write`
returns zero without I/O. A nonempty
`write` makes one write and flushes accepted bytes before returning their count;
cancellation can be reported after the write side effect. `write_all` flushes even
after a partial or cancelled write so closing the Cog handle cannot discard buffered
output still owned by a worker. `seek` flushes pending buffered output before moving
the file position.

## Structured lifetime and errors

Tasks started with `spawn` are structured children of the current async function.
That function waits for its spawned children before completing. There is no detached
task API. Cancellation is cooperative and exposed as `cancel(task) -> bool`. It
requests cancellation for that task and its spawned descendants. Cold, queued, and
timer-waiting tasks complete immediately with `Error::Cancelled`. Running Cog code
checks the request at statement and `await` boundaries; cancellation follows normal
Error propagation, so active `defer` blocks run and a `try/catch` may handle
`Error::Cancelled`. Handling it consumes that request. Cancellation does not detach
work from its structured parent.

An unhandled fatal runtime error in an async function requests cancellation of its
spawned children before the function joins them. Children still run their cleanup and
the parent waits for them to finish cancellation; recoverable Error values do not
trigger this fail-fast path.

Filesystem reads and writes check cancellation between 8 KiB chunks, and directory
enumeration checks between entries. A blocked OS open/read/write call cannot be
interrupted; `async::fs::copy` and single-call metadata/mutation operations also run to
their host-call boundary. Cancellation may therefore leave a truncated/partial file,
append prefix, or a completed one-call mutation. If a host operation fails, that
failure stays the primary Error and cancellation is attached as suppressed. Async
process operations check a cancellation token while waiting for output and process
completion. On Unix, the child starts in a new process group; cancellation or timeout
sends `SIGKILL` to that group and reaps the direct child. This stops ordinary
descendants that remain in the group. A descendant that creates another session or
process group can outlive the request. On other platforms, cancellation and timeout
stop only the direct child. When cancellation and a deadline race, cancellation wins
if the worker observes it first. Dropping the invocation requests cancellation of
active process groups and interruptible I/O, discards queued worker jobs, and joins the
worker pool. An already executing filesystem syscall, copy, DNS lookup, or connect
attempt must return before its worker can join.

`process::run` snapshots its `bytes` input when the task starts, starts a
concurrent stdin writer, and captures stdout/stderr under their independent
`max_buffer_bytes` limits. An explicit per-run output limit is shared across both
streams; exceeding it terminates the child and returns `Error::ResourceLimit`. The
per-stream runtime limit is unset by default. The child stdin closes after the
snapshot is written; an early child close is treated as a normal end to input. On Unix
the pipe writer is nonblocking and checks cancellation at roughly 10 ms intervals.
Other platforms rely on child termination to
release a blocked pipe write. The per-run output limit must be a nonnegative int or uint
and counts stdout plus stderr together. The four-argument form accepts
`timeout_ms, max_output_bytes`; use `nil` for no deadline. The five-argument form
accepts `stdin, timeout_ms, max_output_bytes`, with an optional nil timeout.

Task handles are cloneable leases. Dropping the final handle to a cold task queues
its target for release; the scheduler removes it at the next VM instruction boundary
and drops its captured arguments. A reachability pass also reclaims cold tasks whose
handles are reachable only through cycles spanning task captures and Cog-managed
aggregates. The pass runs with heap collection and before managed allocations under
heap quota pressure. Completed task outcomes remain available while a handle can
observe them, and unobserved spawned children remain retained through their parent's
structured join.

`cancel(task)` returns whether it accepted a new request. It cascades through
structured spawned children, but not through tasks merely awaited by the target.
Cold, ready-queue, and timer tasks are completed immediately with `Error::Cancelled`.
Running Cog tasks receive the recoverable Error at generated statement and `await`
checkpoints; normal cleanup and catch rules apply. A catch consumes the request, so
the task may continue. Filesystem operations check cancellation between chunks where
possible; a blocked syscall or `async::fs::copy` must finish before it can return. An
interrupted write may leave a partial result. Host failures remain the primary Error
with cancellation suppressed. Async process calls terminate the Unix process group
(or only the direct child on other platforms) and reap the direct child after
cancellation. Descendants that create another process group are outside this
cancellation scope. Completed tasks and tasks with an outstanding request return
`false`.

Awaiting a task observes its result under the existing Cog Error rules. The task's
`defer` blocks run before its result becomes complete; cleanup failures follow the
existing primary and suppressed Error rules. An unawaited spawned child's Error is
reported when its owning async scope joins that child. Ordering follows executor
completion order.

`async::net::exchange` is a bounded one-shot request/response operation. Its
deadline starts before name resolution and bounds connect and data transfer after
resolution. The operating system's synchronous DNS resolver cannot be interrupted
and may delay return beyond the deadline. The response obeys the runtime buffer cap.

Persistent streams and listeners use the same bounded worker pool and require
`allow_network`. `async::net::listen(host, port)` binds on a worker and returns a
context-scoped handle counted by `max_resources`. `async::net::accept` performs a
nonblocking accept loop on a worker with a `1..=300000` ms accept timeout; its
`stream_timeout_ms` uses the same range and configures the accepted socket. Only one
accept can be active per listener; overlapping accepts return `Error::IoFailure`.
Listener and stream handles can both be released with `io::close(handle)`.

`async::net::connect` validates the port and a `1..=300000` ms timeout, resolves and
connects on a worker, and returns a resource handle owned by the current
`RuntimeContext`; it counts against `max_resources` and can be released with
`io::close(handle)`. A `read` size must be positive and no larger than the active
`max_buffer_bytes` limit. It returns at most that many bytes, and an empty buffer
means the peer has closed its write side. `read_exact` accepts a nonnegative size up
to that limit, and returns `Error::IoFailure` if EOF arrives before all requested
bytes. Both `read_exact` and `read_all` use one operation deadline; timeout or
cancellation may consume a prefix without returning it. `read_all` accumulates until
EOF and returns `Error::ResourceLimit` if the peer sends more than `max_buffer_bytes`.
Writes snapshot their input when started and are subject to the same byte limit.
`write` returns zero for an empty buffer; otherwise it makes one write and returns the
accepted count, which can be smaller than the input. Its deadline bounds the
operation; cancellation can be reported after bytes have been sent and may hide that
count. `write_all` retries until the complete buffer is sent. At the limit, read-all
peeks for EOF and leaves an over-limit byte in the
stream. The connection timeout is also installed as the socket read/write timeout
for each later operation.

One read and one write may overlap on the same stream. A second read while a read is
in flight, or a second write while a write is in flight, completes with
`Error::IoFailure` instead of occupying another worker waiting for the same stream.
Exact and read-all operations hold the read lock for their full lifetime.
`async::net::shutdown_write`
closes only the local write direction, so the peer can observe EOF while the Cog task
continues reading. Started accept, read, exact-read, read-all, and write operations check task
cancellation at roughly 10 ms intervals; a cancelled write may already have sent a
prefix of its input. Queued connect, listen, and shutdown-write
jobs check cancellation before starting. Running connect/listen jobs check again after
DNS or the host call returns, and the VM drops a completed socket/listener instead of
publishing a handle if cancellation won the completion race. Cancellation releases
the worker's cloned socket or listener when the operation returns, even if `io::close`
already invalidated the Cog handle. DNS lookup for connect, exchange, and listen
remains synchronous inside the worker and cannot be interrupted; a resolver call may
outlast its operation deadline. On Unix, connect attempts for `async::net::connect` and
`async::net::exchange` use nonblocking sockets and poll task cancellation at roughly
10 ms intervals. On other platforms, the system connect call may delay cancellation
until it returns or reaches the operation deadline. Listener binding has no DNS
deadline.

Native plugins can declare async functions that return cold `Task[T]` values. ABI
v3 runs `start`, `poll`, `cancel`, and operation destruction on the VM thread.
Worker threads may only signal the scheduler through a thread-safe `wake` callback;
they cannot access Cog values or the borrowed host context. Cancellation calls the
plugin's cancel callback and waits for its workers to stop before operation state is
destroyed. See the [SDK ABI contract](SDK.md#c-abi-lifetime-and-failure-contract)
and [concurrent plugin example](../examples/async_plugin.cog).

## Implementation status and next work

Implemented:

1. `@async fn`, `Task[T]`, `await`, and `spawn` parsing and type checks, including
   rejection of `await` and `spawn` in synchronous functions.
2. Serializable task creation, explicit sync/async direct-call contracts, and
   bytecode versioning/verifier checks.
3. A per-invocation FIFO executor, async `main`, memoized outcomes, shared globals,
   structured child joining, Error propagation, and a shared instruction budget.
4. Deadline-based async sleep, source-task timeouts, and worker-pool operations for
   filesystem metadata, listing, byte/text/line reads, writes and appends, copy/rename,
   directory creation/removal, process capture, bounded TCP exchanges, and persistent
   TCP streams, with
   bounded queues and `Task[T]` contracts.
5. Async inherent receiver methods, associated functions, and statically dispatched
   trait methods, lowered through the same `CallAsync` contract as direct functions.
6. Warning `W0005` for discarded async-function, method, and built-in calls, plus
   local task bindings and alias chains that leave scope unstarted.
7. Reclamation of unreachable cold-task cycles that cross scheduler captures and
   Cog-managed heap objects, while preserving tasks reachable from external aliases.
8. Cooperative cancellation of task trees, including cleanup execution and bounded
   worker-operation behavior.
9. Resumable VM continuations, FIFO waiter wakeups, cycle detection across awaited
   tasks and structured joins, and non-nested structured joining.
10. `yield_now()` as an explicit cooperative queue handoff for CPU work.
11. `task::select` and `task::select2` for selecting the first of one or more spawned
   sibling tasks without consuming results or detaching the remaining tasks.
12. C ABI v3 async native functions with cold task creation, scheduler wakeups,
   VM-thread polling, cancellation callbacks and operation-state cleanup.

Next:

1. Extend `W0005` to indirect calls if source-level function values are added;
   aliases of tracked tasks are covered.
2. Define a detach contract before exposing detached tasks.
3. Implement Windows process-tree termination to match the Unix process-group
   contract. Add buffered TCP framing helpers only after their memory and
   cancellation behavior is specified.

The executor is cooperative rather than parallel for Cog code. Incomplete awaits
pause and resume VM continuations through the scheduler, while ordinary synchronous
native/plugin calls still run to completion and can block it. TCP, filesystem, and
process operations use the bounded host worker pool. Async native plugins manage
their own worker resources and enter the scheduler through queued wakeups.
