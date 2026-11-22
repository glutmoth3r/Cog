//! Bounded worker pool for asynchronous standard-library host operations.
use super::{
    FileOpenMode, FileWriteBuffer, RuntimeContext, RuntimeLimits, SharedFileStream,
    SharedInputStream, SharedOutputStream, VmCapabilities, host, open_file_stream,
};
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{ArrayValue, RuntimeType, Value};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Instant,
};

const WORKERS: usize = 4;
const QUEUE_CAPACITY: usize = 128;
const IO_CANCELLATION_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy)]
pub(crate) enum AsyncFsOperation {
    Metadata,
    ReadDir,
    ReadBytes,
    WriteBytes,
    CreateDir,
    CreateDirAll,
    RemoveFile,
    RemoveDir,
    ReadText,
    WriteText,
    ReadLines,
    AppendBytes,
    AppendText,
    Copy,
    Rename,
}

impl AsyncFsOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Metadata => "async::fs::metadata",
            Self::ReadDir => "async::fs::read_dir",
            Self::ReadBytes => "async::fs::read_bytes",
            Self::WriteBytes => "async::fs::write_bytes",
            Self::CreateDir => "async::fs::create_dir",
            Self::CreateDirAll => "async::fs::create_dir_all",
            Self::RemoveFile => "async::fs::remove_file",
            Self::RemoveDir => "async::fs::remove_dir",
            Self::ReadText => "async::fs::read_text",
            Self::WriteText => "async::fs::write_text",
            Self::ReadLines => "async::fs::read_lines",
            Self::AppendBytes => "async::fs::append_bytes",
            Self::AppendText => "async::fs::append_text",
            Self::Copy => "async::fs::copy",
            Self::Rename => "async::fs::rename",
        }
    }

    pub(crate) fn arity(self) -> usize {
        match self {
            Self::WriteBytes
            | Self::WriteText
            | Self::AppendBytes
            | Self::AppendText
            | Self::Copy
            | Self::Rename => 2,
            _ => 1,
        }
    }

    pub(crate) fn writes(self) -> bool {
        matches!(
            self,
            Self::WriteBytes
                | Self::CreateDir
                | Self::CreateDirAll
                | Self::RemoveFile
                | Self::RemoveDir
                | Self::WriteText
                | Self::AppendBytes
                | Self::AppendText
                | Self::Copy
                | Self::Rename
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AsyncTcpOperation {
    Listen,
    Accept,
    Connect,
    Read,
    ReadExact,
    ReadAll,
    Write,
    WriteAll,
    ShutdownWrite,
}

#[derive(Clone, Copy)]
pub(crate) enum AsyncFileOperation {
    Open,
    Read,
    ReadExact,
    ReadAll,
    Write,
    WriteAll,
    Flush,
    Seek,
}

impl AsyncFileOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Open => "async::io::open",
            Self::Read => "async::io::read",
            Self::ReadExact => "async::io::read_exact",
            Self::ReadAll => "async::io::read_all",
            Self::Write => "async::io::write",
            Self::WriteAll => "async::io::write_all",
            Self::Flush => "async::io::flush",
            Self::Seek => "async::io::seek",
        }
    }

    pub(crate) fn arity(self) -> usize {
        match self {
            Self::Open => 3,
            Self::Read | Self::ReadExact | Self::Write | Self::WriteAll => 2,
            Self::ReadAll => 1,
            Self::Flush => 1,
            Self::Seek => 3,
        }
    }
}

impl AsyncTcpOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Listen => "async::net::listen",
            Self::Accept => "async::net::accept",
            Self::Connect => "async::net::connect",
            Self::Read => "async::net::read",
            Self::ReadExact => "async::net::read_exact",
            Self::ReadAll => "async::net::read_all",
            Self::Write => "async::net::write",
            Self::WriteAll => "async::net::write_all",
            Self::ShutdownWrite => "async::net::shutdown_write",
        }
    }

    pub(crate) fn arity(self) -> usize {
        match self {
            Self::Listen => 2,
            Self::Accept => 3,
            Self::Connect => 3,
            Self::Read | Self::ReadExact | Self::Write | Self::WriteAll => 2,
            Self::ReadAll | Self::ShutdownWrite => 1,
        }
    }
}

pub(crate) enum AsyncFsJob {
    Metadata {
        task: usize,
        path: String,
        limit: usize,
    },
    ReadDir {
        task: usize,
        path: String,
        limit: usize,
    },
    ReadBytes {
        task: usize,
        path: String,
        limit: usize,
    },
    WriteBytes {
        task: usize,
        path: String,
        bytes: Vec<u8>,
    },
    ReadText {
        task: usize,
        path: String,
        limit: usize,
    },
    WriteText {
        task: usize,
        path: String,
        bytes: Vec<u8>,
    },
    ReadLines {
        task: usize,
        path: String,
        limit: usize,
    },
    AppendBytes {
        task: usize,
        path: String,
        bytes: Vec<u8>,
    },
    AppendText {
        task: usize,
        path: String,
        bytes: Vec<u8>,
    },
    Copy {
        task: usize,
        source: String,
        destination: String,
    },
    Rename {
        task: usize,
        source: String,
        destination: String,
    },
    CreateDir {
        task: usize,
        path: String,
    },
    CreateDirAll {
        task: usize,
        path: String,
    },
    RemoveFile {
        task: usize,
        path: String,
    },
    RemoveDir {
        task: usize,
        path: String,
    },
}

impl AsyncFsJob {
    fn task_id(&self) -> usize {
        match self {
            Self::Metadata { task, .. }
            | Self::ReadDir { task, .. }
            | Self::ReadBytes { task, .. }
            | Self::WriteBytes { task, .. }
            | Self::ReadText { task, .. }
            | Self::WriteText { task, .. }
            | Self::ReadLines { task, .. }
            | Self::AppendBytes { task, .. }
            | Self::AppendText { task, .. }
            | Self::Copy { task, .. }
            | Self::Rename { task, .. }
            | Self::CreateDir { task, .. }
            | Self::CreateDirAll { task, .. }
            | Self::RemoveFile { task, .. }
            | Self::RemoveDir { task, .. } => *task,
        }
    }
}

pub(crate) enum AsyncIoJob {
    Filesystem {
        job: AsyncFsJob,
        cancelled: Arc<AtomicBool>,
    },
    ProcessRun {
        task: usize,
        program: String,
        arguments: Vec<String>,
        input: Option<Vec<u8>>,
        limit: usize,
        output_limit: Option<usize>,
        timeout: Option<Duration>,
        cancelled: Arc<AtomicBool>,
    },
    NetworkExchange {
        task: usize,
        host: String,
        port: u16,
        payload: Vec<u8>,
        timeout: Duration,
        limit: usize,
        cancelled: Arc<AtomicBool>,
    },
    TcpConnect {
        task: usize,
        host: String,
        port: u16,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    },
    TcpListen {
        task: usize,
        host: String,
        port: u16,
        cancelled: Arc<AtomicBool>,
    },
    TcpAccept {
        task: usize,
        listener: Arc<Mutex<TcpListener>>,
        accept_timeout: Duration,
        stream_timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpRead {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        max_bytes: usize,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpReadExact {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        size: usize,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpReadAll {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        limit: usize,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpWriteAll {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        bytes: Vec<u8>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpWrite {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        bytes: Vec<u8>,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    TcpShutdownWrite {
        task: usize,
        stream: Arc<Mutex<TcpStream>>,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileRead {
        task: usize,
        file: SharedFileStream,
        max_bytes: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileReadExact {
        task: usize,
        file: SharedFileStream,
        size: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileReadAll {
        task: usize,
        file: SharedFileStream,
        max_bytes: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileOpen {
        task: usize,
        path: String,
        mode: FileOpenMode,
        buffer: Option<FileWriteBuffer>,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileWriteAll {
        task: usize,
        file: SharedFileStream,
        bytes: Vec<u8>,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileWrite {
        task: usize,
        file: SharedFileStream,
        bytes: Vec<u8>,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileFlush {
        task: usize,
        file: SharedFileStream,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    FileSeek {
        task: usize,
        file: SharedFileStream,
        position: SeekFrom,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    Shutdown,
}

pub(crate) struct AsyncIoCompletion {
    pub task: usize,
    pub result: Result<AsyncIoResult, AsyncIoFailure>,
}

pub(crate) enum AsyncStdioInputJob {
    Read {
        task: usize,
        max_bytes: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    ReadExact {
        task: usize,
        size: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    ReadAll {
        task: usize,
        limit: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
}

pub(crate) enum AsyncStdioOutputJob {
    Write {
        task: usize,
        bytes: Vec<u8>,
        all: bool,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
    Flush {
        task: usize,
        cancelled: Arc<AtomicBool>,
        op: &'static str,
    },
}

pub(crate) enum AsyncIoResult {
    Cancelled,
    Nil,
    Bytes(Vec<u8>),
    Text(String),
    UInt(u64),
    Int(i64),
    Directory(Vec<String>),
    Lines(Vec<String>),
    Metadata {
        is_file: bool,
        is_dir: bool,
        is_readonly: bool,
        len: i64,
    },
    Process {
        exit_code: Option<i32>,
        success: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TcpStream {
        stream: TcpStream,
        timeout: Duration,
    },
    TcpListener(TcpListener),
    FileOpened {
        file: SharedFileStream,
        mode: FileOpenMode,
    },
}

pub(crate) struct AsyncStdioPool {
    input: Option<SyncSender<AsyncStdioInputJob>>,
    input_error: Option<String>,
    output: Option<SyncSender<AsyncStdioOutputJob>>,
    output_error: Option<String>,
    operation_cancellations: Arc<Mutex<Vec<Weak<AtomicBool>>>>,
}

impl AsyncStdioPool {
    pub(crate) fn new<'scope, 'env: 'scope, 'streams: 'scope>(
        scope: &'scope thread::Scope<'scope, 'env>,
        input: SharedInputStream<'streams>,
        output: SharedOutputStream<'streams>,
        completions: mpsc::Sender<AsyncIoCompletion>,
        needs_input: bool,
        needs_output: bool,
    ) -> Self {
        let (input, input_error) = if needs_input {
            let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
            let input_completions = completions.clone();
            let result = thread::Builder::new()
                .name("cog-async-stdin".into())
                .spawn_scoped(scope, move || {
                    stdin_worker(input, receiver, input_completions)
                });
            match result {
                Ok(worker) => {
                    drop(worker);
                    (Some(sender), None)
                }
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let (output, output_error) = if needs_output {
            let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
            let result = thread::Builder::new()
                .name("cog-async-stdout".into())
                .spawn_scoped(scope, move || stdout_worker(output, receiver, completions));
            match result {
                Ok(worker) => {
                    drop(worker);
                    (Some(sender), None)
                }
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        Self {
            input,
            input_error,
            output,
            output_error,
            operation_cancellations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn register_cancellation(&self, cancelled: &Arc<AtomicBool>) -> Result<(), AsyncIoFailure> {
        let mut cancellations = self
            .operation_cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cancellations.retain(|token| token.strong_count() > 0);
        cancellations
            .try_reserve(1)
            .map_err(|_| AsyncIoFailure::ResourceLimit {
                resource: "async operation cancellations",
            })?;
        cancellations.push(Arc::downgrade(cancelled));
        Ok(())
    }

    pub(crate) fn submit_input(&self, job: AsyncStdioInputJob) -> Result<(), AsyncIoFailure> {
        let op = match &job {
            AsyncStdioInputJob::Read { op, .. }
            | AsyncStdioInputJob::ReadExact { op, .. }
            | AsyncStdioInputJob::ReadAll { op, .. } => *op,
        };
        let sender = self.input.as_ref().ok_or_else(|| AsyncIoFailure::Io {
            op,
            message: self
                .input_error
                .clone()
                .unwrap_or_else(|| "async stdin worker is unavailable".into()),
        })?;
        let cancelled = match &job {
            AsyncStdioInputJob::Read { cancelled, .. }
            | AsyncStdioInputJob::ReadExact { cancelled, .. }
            | AsyncStdioInputJob::ReadAll { cancelled, .. } => cancelled,
        };
        self.register_cancellation(cancelled)?;
        sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => AsyncIoFailure::ResourceLimit {
                resource: "async stdio queue",
            },
            TrySendError::Disconnected(_) => AsyncIoFailure::Io {
                op,
                message: "async stdin worker has stopped".into(),
            },
        })
    }

    pub(crate) fn submit_output(&self, job: AsyncStdioOutputJob) -> Result<(), AsyncIoFailure> {
        let op = match &job {
            AsyncStdioOutputJob::Write { op, .. } | AsyncStdioOutputJob::Flush { op, .. } => *op,
        };
        let sender = self.output.as_ref().ok_or_else(|| AsyncIoFailure::Io {
            op,
            message: self
                .output_error
                .clone()
                .unwrap_or_else(|| "async stdout worker is unavailable".into()),
        })?;
        let cancelled = match &job {
            AsyncStdioOutputJob::Write { cancelled, .. }
            | AsyncStdioOutputJob::Flush { cancelled, .. } => cancelled,
        };
        self.register_cancellation(cancelled)?;
        sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => AsyncIoFailure::ResourceLimit {
                resource: "async stdio queue",
            },
            TrySendError::Disconnected(_) => AsyncIoFailure::Io {
                op,
                message: "async stdout worker has stopped".into(),
            },
        })
    }
}

impl Drop for AsyncStdioPool {
    fn drop(&mut self) {
        let cancellations = self
            .operation_cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for cancellation in cancellations.iter().filter_map(Weak::upgrade) {
            cancellation.store(true, Ordering::Release);
        }
    }
}

fn stdin_worker<'a>(
    input: SharedInputStream<'a>,
    jobs: Receiver<AsyncStdioInputJob>,
    completions: mpsc::Sender<AsyncIoCompletion>,
) {
    while let Ok(job) = jobs.recv() {
        let (task, result) = match job {
            AsyncStdioInputJob::Read {
                task,
                max_bytes,
                cancelled,
                op,
            } => (task, run_stdin_read(&input, max_bytes, &cancelled, op)),
            AsyncStdioInputJob::ReadExact {
                task,
                size,
                cancelled,
                op,
            } => (task, run_stdin_read_exact(&input, size, &cancelled, op)),
            AsyncStdioInputJob::ReadAll {
                task,
                limit,
                cancelled,
                op,
            } => (task, run_stdin_read_all(&input, limit, &cancelled, op)),
        };
        if completions
            .send(AsyncIoCompletion { task, result })
            .is_err()
        {
            return;
        }
    }
}

fn stdout_worker<'a>(
    output: SharedOutputStream<'a>,
    jobs: Receiver<AsyncStdioOutputJob>,
    completions: mpsc::Sender<AsyncIoCompletion>,
) {
    while let Ok(job) = jobs.recv() {
        let (task, result) = match job {
            AsyncStdioOutputJob::Write {
                task,
                bytes,
                all,
                cancelled,
                op,
            } => (task, run_stdout_write(&output, &bytes, all, &cancelled, op)),
            AsyncStdioOutputJob::Flush {
                task,
                cancelled,
                op,
            } => (task, run_stdout_flush(&output, &cancelled, op)),
        };
        if completions
            .send(AsyncIoCompletion { task, result })
            .is_err()
        {
            return;
        }
    }
}

pub(crate) enum AsyncIoFailure {
    Cancelled,
    Io {
        op: &'static str,
        message: String,
    },
    ResourceLimit {
        resource: &'static str,
    },
    ParseFailure {
        from: &'static str,
        to: &'static str,
    },
    Overflow,
    Timeout,
}

impl AsyncIoFailure {
    pub(crate) fn into_runtime_error(self) -> RuntimeError {
        match self {
            Self::Cancelled => RuntimeError::Cancelled,
            Self::Io { op, message } => RuntimeError::IoFailure { op, message },
            Self::ResourceLimit { resource } => RuntimeError::ResourceLimit { resource },
            Self::ParseFailure { from, to } => RuntimeError::ParseFailure { from, to },
            Self::Overflow => RuntimeError::Overflow,
            Self::Timeout => RuntimeError::Timeout,
        }
    }
}

pub(crate) struct AsyncIoPool {
    jobs: SyncSender<AsyncIoJob>,
    workers: Vec<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    operation_cancellations: Arc<Mutex<Vec<Weak<AtomicBool>>>>,
}

impl AsyncIoPool {
    pub(crate) fn new(sender: mpsc::Sender<AsyncIoCompletion>) -> io::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let stopping = Arc::new(AtomicBool::new(false));
        let operation_cancellations = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(WORKERS)
            .map_err(|_| io::Error::other("cannot allocate async I/O workers"))?;

        for index in 0..WORKERS {
            let receiver = Arc::clone(&receiver);
            let sender = sender.clone();
            let stopping = Arc::clone(&stopping);
            match thread::Builder::new()
                .name(format!("cog-async-io-{index}"))
                .spawn(move || worker(receiver, sender, stopping))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    for _ in 0..workers.len() {
                        let _ = jobs.send(AsyncIoJob::Shutdown);
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }

        Ok(Self {
            jobs,
            workers,
            stopping,
            operation_cancellations,
        })
    }

    pub(crate) fn submit(&self, job: AsyncIoJob) -> Result<(), AsyncIoFailure> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(AsyncIoFailure::Io {
                op: "async I/O",
                message: "worker pool has stopped".into(),
            });
        }
        let cancellation = match &job {
            AsyncIoJob::Filesystem { cancelled, .. }
            | AsyncIoJob::ProcessRun { cancelled, .. }
            | AsyncIoJob::NetworkExchange { cancelled, .. }
            | AsyncIoJob::TcpConnect { cancelled, .. }
            | AsyncIoJob::TcpListen { cancelled, .. }
            | AsyncIoJob::TcpAccept { cancelled, .. }
            | AsyncIoJob::TcpRead { cancelled, .. }
            | AsyncIoJob::TcpReadExact { cancelled, .. }
            | AsyncIoJob::TcpReadAll { cancelled, .. }
            | AsyncIoJob::TcpWriteAll { cancelled, .. }
            | AsyncIoJob::TcpWrite { cancelled, .. }
            | AsyncIoJob::TcpShutdownWrite { cancelled, .. }
            | AsyncIoJob::FileOpen { cancelled, .. }
            | AsyncIoJob::FileRead { cancelled, .. }
            | AsyncIoJob::FileReadExact { cancelled, .. }
            | AsyncIoJob::FileReadAll { cancelled, .. }
            | AsyncIoJob::FileWriteAll { cancelled, .. }
            | AsyncIoJob::FileWrite { cancelled, .. }
            | AsyncIoJob::FileFlush { cancelled, .. }
            | AsyncIoJob::FileSeek { cancelled, .. } => Some(cancelled),
            _ => None,
        };
        if let Some(cancelled) = cancellation {
            let mut cancellations = self
                .operation_cancellations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cancellations.retain(|token| token.strong_count() > 0);
            cancellations
                .try_reserve(1)
                .map_err(|_| AsyncIoFailure::ResourceLimit {
                    resource: "async operation cancellations",
                })?;
            cancellations.push(Arc::downgrade(cancelled));
        }
        self.jobs.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => AsyncIoFailure::ResourceLimit {
                resource: "async I/O queue",
            },
            TrySendError::Disconnected(_) => AsyncIoFailure::Io {
                op: "async I/O",
                message: "worker pool has stopped".into(),
            },
        })
    }
}

impl Drop for AsyncIoPool {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let cancellations = self
            .operation_cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for cancellation in cancellations.iter().filter_map(Weak::upgrade) {
            cancellation.store(true, Ordering::Release);
        }
        drop(cancellations);
        for _ in 0..self.workers.len() {
            let _ = self.jobs.send(AsyncIoJob::Shutdown);
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker(
    receiver: Arc<Mutex<Receiver<AsyncIoJob>>>,
    sender: mpsc::Sender<AsyncIoCompletion>,
    stopping: Arc<AtomicBool>,
) {
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        let Ok(job) = job else {
            return;
        };
        if stopping.load(Ordering::Acquire) {
            return;
        }
        let completion = match job {
            AsyncIoJob::Filesystem { job, cancelled } => run_filesystem(job, &cancelled),
            AsyncIoJob::ProcessRun {
                task,
                program,
                arguments,
                input,
                limit,
                output_limit,
                timeout,
                cancelled,
            } => AsyncIoCompletion {
                task,
                result: run_process(
                    program,
                    arguments,
                    input,
                    limit,
                    output_limit,
                    timeout,
                    cancelled,
                ),
            },
            AsyncIoJob::NetworkExchange {
                task,
                host,
                port,
                payload,
                timeout,
                limit,
                cancelled,
            } => AsyncIoCompletion {
                task,
                result: run_network_exchange(host, port, payload, timeout, limit, &cancelled),
            },
            AsyncIoJob::TcpConnect {
                task,
                host,
                port,
                timeout,
                cancelled,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_connect(host, port, timeout, &cancelled),
            },
            AsyncIoJob::TcpListen {
                task,
                host,
                port,
                cancelled,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_listen(host, port, &cancelled),
            },
            AsyncIoJob::TcpAccept {
                task,
                listener,
                accept_timeout,
                stream_timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_accept(listener, accept_timeout, stream_timeout, &cancelled, op),
            },
            AsyncIoJob::TcpRead {
                task,
                stream,
                max_bytes,
                timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_read(stream, max_bytes, timeout, &cancelled, op),
            },
            AsyncIoJob::TcpReadExact {
                task,
                stream,
                size,
                timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_read_exact(stream, size, timeout, &cancelled, op),
            },
            AsyncIoJob::TcpReadAll {
                task,
                stream,
                limit,
                timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_read_all(stream, limit, timeout, &cancelled, op),
            },
            AsyncIoJob::TcpWriteAll {
                task,
                stream,
                bytes,
                timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_write_all(stream, &bytes, timeout, &cancelled, op),
            },
            AsyncIoJob::TcpWrite {
                task,
                stream,
                bytes,
                timeout,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_write(stream, &bytes, timeout, &cancelled, op),
            },
            AsyncIoJob::TcpShutdownWrite {
                task,
                stream,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_tcp_shutdown_write(stream, &cancelled, op),
            },
            AsyncIoJob::FileOpen {
                task,
                path,
                mode,
                buffer,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_open(path, mode, buffer, &cancelled, op),
            },
            AsyncIoJob::FileRead {
                task,
                file,
                max_bytes,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_read(file, max_bytes, &cancelled, op),
            },
            AsyncIoJob::FileReadExact {
                task,
                file,
                size,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_read_exact(file, size, &cancelled, op),
            },
            AsyncIoJob::FileReadAll {
                task,
                file,
                max_bytes,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_read_all(file, max_bytes, &cancelled, op),
            },
            AsyncIoJob::FileWriteAll {
                task,
                file,
                bytes,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_write_all(file, &bytes, &cancelled, op),
            },
            AsyncIoJob::FileWrite {
                task,
                file,
                bytes,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_write(file, &bytes, &cancelled, op),
            },
            AsyncIoJob::FileFlush {
                task,
                file,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_flush(file, &cancelled, op),
            },
            AsyncIoJob::FileSeek {
                task,
                file,
                position,
                cancelled,
                op,
            } => AsyncIoCompletion {
                task,
                result: run_file_seek(file, position, &cancelled, op),
            },
            AsyncIoJob::Shutdown => return,
        };
        if sender.send(completion).is_err() {
            return;
        }
    }
}

fn run_network_exchange(
    host: String,
    port: u16,
    payload: Vec<u8>,
    timeout: Duration,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "async::net::exchange";
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| io_failure(OP, error))?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut last_error = None;
    let mut stream = None;
    for address in addresses.take(256) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(AsyncIoFailure::Timeout);
        }
        match connect_tcp_address(&address, deadline, cancelled) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_error) if cancelled.load(Ordering::Acquire) => {
                return Ok(AsyncIoResult::Cancelled);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(AsyncIoFailure::Timeout);
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        last_error.map_or_else(
            || AsyncIoFailure::Io {
                op: OP,
                message: "host resolved to no addresses".into(),
            },
            |error| io_failure(OP, error),
        )
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(AsyncIoFailure::Timeout);
    }
    let mut written = 0;
    while written < payload.len() {
        if cancelled.load(Ordering::Acquire) {
            return Ok(AsyncIoResult::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(AsyncIoFailure::Timeout);
        }
        stream
            .set_write_timeout(Some(remaining.min(IO_CANCELLATION_POLL)))
            .map_err(|error| io_failure(OP, error))?;
        match stream.write(&payload[written..]) {
            Ok(0) => {
                return Err(io_failure(
                    OP,
                    io::Error::new(io::ErrorKind::WriteZero, "failed to write TCP stream"),
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    return Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    return Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => return Err(network_failure(OP, error)),
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| io_failure(OP, error))?;

    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(AsyncIoResult::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(AsyncIoFailure::Timeout);
        }
        stream
            .set_read_timeout(Some(remaining.min(IO_CANCELLATION_POLL)))
            .map_err(|error| io_failure(OP, error))?;
        let count = match stream.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    return Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    return Err(AsyncIoFailure::Timeout);
                }
                continue;
            }
            Err(error) => return Err(network_failure(OP, error)),
        };
        if count == 0 {
            break;
        }
        if count > limit.saturating_sub(bytes.len()) {
            return Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
        }
        bytes
            .try_reserve(count)
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(AsyncIoResult::Bytes(bytes))
}

fn network_failure(op: &'static str, error: io::Error) -> AsyncIoFailure {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        AsyncIoFailure::Timeout
    } else {
        io_failure(op, error)
    }
}

#[cfg(unix)]
fn connect_tcp_address(
    address: &SocketAddr,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<TcpStream> {
    let family = match address {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    let socket_type = libc::SOCK_STREAM;
    // SAFETY: the address family comes from a valid `SocketAddr`; the socket type
    // and protocol are fixed libc constants, and this call passes no pointers.
    let raw_fd = unsafe { libc::socket(family, socket_type, libc::IPPROTO_TCP) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a fresh descriptor, and this function transfers
    // its sole ownership into `OwnedFd` before any fallible setup can return.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let raw_fd = fd.as_raw_fd();
    // SAFETY: `fd` keeps `raw_fd` open through this setup; these calls use only
    // scalar flags and pass no pointers to libc.
    let descriptor_flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe { libc::fcntl(raw_fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let status_flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
    if status_flags < 0
        || unsafe { libc::fcntl(raw_fd, libc::F_SETFL, status_flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }

    let connect_result = match address {
        SocketAddr::V4(address) => {
            let sockaddr = libc::sockaddr_in {
                #[cfg(any(
                    target_vendor = "apple",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                ))]
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: this C-layout address is initialized for IPv4 and remains
            // live for the call; the pointer and length describe that exact value.
            unsafe {
                libc::connect(
                    raw_fd,
                    (&sockaddr as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(address) => {
            let sockaddr = libc::sockaddr_in6 {
                #[cfg(any(
                    target_vendor = "apple",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                ))]
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
            };
            // SAFETY: this C-layout address is initialized for IPv6 and remains
            // live for the call; the pointer and length describe that exact value.
            unsafe {
                libc::connect(
                    raw_fd,
                    (&sockaddr as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };

    if connect_result < 0 {
        let error = io::Error::last_os_error();
        let pending = matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS)
                | Some(libc::EALREADY)
                | Some(libc::EINTR)
                | Some(libc::EWOULDBLOCK)
        );
        if !pending {
            return Err(error);
        }

        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TCP connection cancelled",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "TCP connection timed out",
                ));
            }
            let poll_millis = remaining
                .min(IO_CANCELLATION_POLL)
                .as_millis()
                .max(1)
                .min(i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: `descriptor` is one initialized `pollfd`, and the count is
            // exactly one for the pointer passed to libc.
            let ready = unsafe { libc::poll(&mut descriptor, 1, poll_millis) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            let mut socket_error: libc::c_int = 0;
            let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
            // SAFETY: both output pointers refer to live local storage and `length`
            // advertises the exact size of `socket_error`.
            let result = unsafe {
                libc::getsockopt(
                    raw_fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut length,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if socket_error != 0 {
                return Err(io::Error::from_raw_os_error(socket_error));
            }
            break;
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "TCP connection cancelled",
        ));
    }
    // SAFETY: `fd` still owns the descriptor, and restoring flags uses no pointers.
    if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, status_flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TcpStream::from(fd))
}

#[cfg(not(unix))]
fn connect_tcp_address(
    address: &SocketAddr,
    deadline: std::time::Instant,
    cancelled: &AtomicBool,
) -> io::Result<TcpStream> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "TCP connection cancelled",
        ));
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "TCP connection timed out",
        ));
    }
    TcpStream::connect_timeout(address, remaining)
}

fn run_tcp_connect(
    host: String,
    port: u16,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "async::net::connect";
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| io_failure(OP, error))?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut last_error = None;
    let mut connected = None;
    for address in addresses.take(256) {
        if cancelled.load(Ordering::Acquire) {
            return Ok(AsyncIoResult::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(AsyncIoFailure::Timeout);
        }
        match connect_tcp_address(&address, deadline, cancelled) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(_error) if cancelled.load(Ordering::Acquire) => {
                return Ok(AsyncIoResult::Cancelled);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(AsyncIoFailure::Timeout);
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = connected.ok_or_else(|| {
        last_error.map_or_else(
            || AsyncIoFailure::Io {
                op: OP,
                message: "host resolved to no addresses".into(),
            },
            |error| io_failure(OP, error),
        )
    })?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| io_failure(OP, error))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| io_failure(OP, error))?;
    Ok(AsyncIoResult::TcpStream { stream, timeout })
}

fn run_tcp_listen(
    host: String,
    port: u16,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "async::net::listen";
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| io_failure(OP, error))?;
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut last_error = None;
    for address in addresses.take(256) {
        if cancelled.load(Ordering::Acquire) {
            return Ok(AsyncIoResult::Cancelled);
        }
        match TcpListener::bind(address) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .map_err(|error| io_failure(OP, error))?;
                if cancelled.load(Ordering::Acquire) {
                    return Ok(AsyncIoResult::Cancelled);
                }
                return Ok(AsyncIoResult::TcpListener(listener));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.map_or_else(
        || AsyncIoFailure::Io {
            op: OP,
            message: "host resolved to no addresses".into(),
        },
        |error| io_failure(OP, error),
    ))
}

fn run_tcp_accept(
    listener: Arc<Mutex<TcpListener>>,
    accept_timeout: Duration,
    stream_timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    let listener = match listener.try_lock() {
        Ok(listener) => listener,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another accept is already in progress on this listener".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(accept_timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(AsyncIoResult::Cancelled);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(stream_timeout))
                    .map_err(|error| io_failure(op, error))?;
                stream
                    .set_write_timeout(Some(stream_timeout))
                    .map_err(|error| io_failure(op, error))?;
                if cancelled.load(Ordering::Acquire) {
                    return Ok(AsyncIoResult::Cancelled);
                }
                return Ok(AsyncIoResult::TcpStream {
                    stream,
                    timeout: stream_timeout,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(AsyncIoFailure::Timeout);
                }
                std::thread::sleep(remaining.min(IO_CANCELLATION_POLL));
            }
            Err(error) => return Err(io_failure(op, error)),
        }
    }
}

fn run_tcp_read(
    stream: Arc<Mutex<TcpStream>>,
    max_bytes: usize,
    timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(max_bytes)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    bytes.resize(max_bytes, 0);
    let mut stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another read is already in progress on this stream".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let result: Result<AsyncIoResult, AsyncIoFailure> = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(AsyncIoResult::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(AsyncIoFailure::Timeout);
        }
        if let Err(error) = stream.set_read_timeout(Some(remaining.min(IO_CANCELLATION_POLL))) {
            break Err(io_failure(op, error));
        }
        match stream.read(&mut bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    break Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => break Err(network_failure(op, error)),
            Ok(count) => {
                bytes.truncate(count);
                break Ok(AsyncIoResult::Bytes(bytes));
            }
        }
    };
    let restore = stream.set_read_timeout(Some(timeout));
    match result {
        Err(error) => Err(error),
        Ok(result) => restore
            .map(|()| result)
            .map_err(|error| io_failure(op, error)),
    }
}

fn run_tcp_read_exact(
    stream: Arc<Mutex<TcpStream>>,
    size: usize,
    timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    bytes.resize(size, 0);
    if size == 0 {
        return Ok(AsyncIoResult::Bytes(bytes));
    }
    let mut stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another read is already in progress on this stream".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let mut filled = 0;
    let result: Result<AsyncIoResult, AsyncIoFailure> = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(AsyncIoResult::Cancelled);
        }
        if filled == size {
            break Ok(AsyncIoResult::Bytes(bytes));
        }
        let remaining_time = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining_time.is_zero() {
            break Err(AsyncIoFailure::Timeout);
        }
        if let Err(error) = stream.set_read_timeout(Some(remaining_time.min(IO_CANCELLATION_POLL)))
        {
            break Err(io_failure(op, error));
        }
        let requested = (size - filled).min(8 * 1024);
        match stream.read(&mut bytes[filled..filled + requested]) {
            Ok(0) => {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                break Err(AsyncIoFailure::Io {
                    op,
                    message: "unexpected EOF before the requested byte count".into(),
                });
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    break Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => break Err(network_failure(op, error)),
        }
    };
    let restore = stream.set_read_timeout(Some(timeout));
    match result {
        Err(error) => Err(error),
        Ok(result) => restore
            .map(|()| result)
            .map_err(|error| io_failure(op, error)),
    }
}

fn run_tcp_read_all(
    stream: Arc<Mutex<TcpStream>>,
    limit: usize,
    timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another read is already in progress on this stream".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let result: Result<AsyncIoResult, AsyncIoFailure> = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(AsyncIoResult::Cancelled);
        }
        let remaining = limit.saturating_sub(bytes.len());
        let read_len = if remaining == 0 {
            1
        } else {
            remaining.min(chunk.len())
        };
        let remaining_time = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining_time.is_zero() {
            break Err(AsyncIoFailure::Timeout);
        }
        if let Err(error) = stream.set_read_timeout(Some(remaining_time.min(IO_CANCELLATION_POLL)))
        {
            break Err(io_failure(op, error));
        }
        let read_result = if remaining == 0 {
            stream.peek(&mut chunk[..read_len])
        } else {
            stream.read(&mut chunk[..read_len])
        };
        match read_result {
            Ok(0) => break Ok(AsyncIoResult::Bytes(bytes)),
            Ok(_) if remaining == 0 => {
                break Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
            }
            Ok(count) => {
                if bytes.try_reserve(count).is_err() {
                    break Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    break Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => break Err(network_failure(op, error)),
        }
    };
    let restore = stream.set_read_timeout(Some(timeout));
    match result {
        Err(error) => Err(error),
        Ok(result) => restore
            .map(|()| result)
            .map_err(|error| io_failure(op, error)),
    }
}

fn run_tcp_write(
    stream: Arc<Mutex<TcpStream>>,
    bytes: &[u8],
    timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    if bytes.is_empty() {
        return Ok(AsyncIoResult::UInt(0));
    }
    let mut stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another write is already in progress on this stream".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let result: Result<AsyncIoResult, AsyncIoFailure> = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(AsyncIoResult::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(AsyncIoFailure::Timeout);
        }
        if let Err(error) = stream.set_write_timeout(Some(remaining.min(IO_CANCELLATION_POLL))) {
            break Err(io_failure(op, error));
        }
        match stream.write(bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    break Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => break Err(network_failure(op, error)),
            Ok(count) => {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                break match u64::try_from(count) {
                    Ok(count) => Ok(AsyncIoResult::UInt(count)),
                    Err(_) => Err(AsyncIoFailure::Overflow),
                };
            }
        }
    };
    let restore = stream.set_write_timeout(Some(timeout));
    match result {
        Err(error) => Err(error),
        Ok(result) => restore
            .map(|()| result)
            .map_err(|error| io_failure(op, error)),
    }
}

fn run_tcp_write_all(
    stream: Arc<Mutex<TcpStream>>,
    bytes: &[u8],
    timeout: Duration,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "another write is already in progress on this stream".into(),
            });
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(AsyncIoFailure::Overflow)?;
    let mut written = 0;
    let result: Result<AsyncIoResult, AsyncIoFailure> = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(AsyncIoResult::Cancelled);
        }
        if written == bytes.len() {
            break Ok(AsyncIoResult::Nil);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(AsyncIoFailure::Timeout);
        }
        if let Err(error) = stream.set_write_timeout(Some(remaining.min(IO_CANCELLATION_POLL))) {
            break Err(io_failure(op, error));
        }
        match stream.write(&bytes[written..]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    break Ok(AsyncIoResult::Cancelled);
                }
                if deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .is_zero()
                {
                    break Err(AsyncIoFailure::Timeout);
                }
            }
            Err(error) => break Err(network_failure(op, error)),
            Ok(0) => {
                break Err(io_failure(
                    op,
                    io::Error::new(io::ErrorKind::WriteZero, "failed to write TCP stream"),
                ));
            }
            Ok(count) => written += count,
        }
    };
    let restore = stream.set_write_timeout(Some(timeout));
    match result {
        Err(error) => Err(error),
        Ok(result) => restore
            .map(|()| result)
            .map_err(|error| io_failure(op, error)),
    }
}

fn run_tcp_shutdown_write(
    stream: Arc<Mutex<TcpStream>>,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let stream = match stream.try_lock() {
        Ok(stream) => stream,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(AsyncIoFailure::Io {
                op,
                message: "a write is already in progress on this stream".into(),
            });
        }
    };
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| io_failure(op, error))?;
    if cancelled.load(Ordering::Acquire) {
        Ok(AsyncIoResult::Cancelled)
    } else {
        Ok(AsyncIoResult::Nil)
    }
}

fn run_filesystem(job: AsyncFsJob, cancelled: &AtomicBool) -> AsyncIoCompletion {
    let task = job.task_id();
    if cancelled.load(Ordering::Acquire) {
        return AsyncIoCompletion {
            task,
            result: Ok(AsyncIoResult::Cancelled),
        };
    }
    let (task, result) = match job {
        AsyncFsJob::Metadata { task, path, limit } => {
            (task, filesystem_metadata(&path, limit, cancelled))
        }
        AsyncFsJob::ReadDir { task, path, limit } => {
            (task, filesystem_read_dir(&path, limit, cancelled))
        }
        AsyncFsJob::ReadBytes { task, path, limit } => (
            task,
            read_bytes(&path, limit, "async::fs::read_bytes", cancelled).map(AsyncIoResult::Bytes),
        ),
        AsyncFsJob::WriteBytes { task, path, bytes } => (
            task,
            write_bytes(&path, &bytes, "async::fs::write_bytes", false, cancelled),
        ),
        AsyncFsJob::ReadText { task, path, limit } => (
            task,
            read_text(&path, limit, cancelled).map(AsyncIoResult::Text),
        ),
        AsyncFsJob::WriteText { task, path, bytes } => (
            task,
            write_bytes(&path, &bytes, "async::fs::write_text", false, cancelled),
        ),
        AsyncFsJob::ReadLines { task, path, limit } => (
            task,
            read_lines(&path, limit, cancelled).map(AsyncIoResult::Lines),
        ),
        AsyncFsJob::AppendBytes { task, path, bytes } => (
            task,
            write_bytes(&path, &bytes, "async::fs::append_bytes", true, cancelled),
        ),
        AsyncFsJob::AppendText { task, path, bytes } => (
            task,
            write_bytes(&path, &bytes, "async::fs::append_text", true, cancelled),
        ),
        AsyncFsJob::Copy {
            task,
            source,
            destination,
        } => (
            task,
            fs_call(cancelled, "async::fs::copy", || {
                std::fs::copy(source, destination)
            })
            .map(AsyncIoResult::UInt),
        ),
        AsyncFsJob::Rename {
            task,
            source,
            destination,
        } => (
            task,
            fs_call(cancelled, "async::fs::rename", || {
                std::fs::rename(source, destination)
            })
            .map(|_| AsyncIoResult::Nil),
        ),
        AsyncFsJob::CreateDir { task, path } => (
            task,
            fs_call(cancelled, "async::fs::create_dir", || {
                std::fs::create_dir(path)
            })
            .map(|_| AsyncIoResult::Nil),
        ),
        AsyncFsJob::CreateDirAll { task, path } => (
            task,
            fs_call(cancelled, "async::fs::create_dir_all", || {
                std::fs::create_dir_all(path)
            })
            .map(|_| AsyncIoResult::Nil),
        ),
        AsyncFsJob::RemoveFile { task, path } => (
            task,
            fs_call(cancelled, "async::fs::remove_file", || {
                std::fs::remove_file(path)
            })
            .map(|_| AsyncIoResult::Nil),
        ),
        AsyncFsJob::RemoveDir { task, path } => (
            task,
            fs_call(cancelled, "async::fs::remove_dir", || {
                std::fs::remove_dir(path)
            })
            .map(|_| AsyncIoResult::Nil),
        ),
    };
    let result = if cancelled.load(Ordering::Acquire) && result.is_ok() {
        Err(AsyncIoFailure::Cancelled)
    } else {
        result
    };
    AsyncIoCompletion { task, result }
}

fn filesystem_context(output: &mut Vec<u8>, limit: usize) -> RuntimeContext<'_> {
    let mut context = RuntimeContext::new(output, VmCapabilities::relaxed());
    context.limits = RuntimeLimits {
        max_buffer_bytes: Some(limit),
        ..RuntimeLimits::default()
    };
    context
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), AsyncIoFailure> {
    if cancelled.load(Ordering::Acquire) {
        Err(AsyncIoFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn run_stdin_read(
    input: &SharedInputStream<'_>,
    max_bytes: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(max_bytes)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    if max_bytes == 0 {
        return Ok(AsyncIoResult::Bytes(output));
    }
    let mut input = input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while output.len() < max_bytes {
        check_cancelled(cancelled)?;
        let requested = (max_bytes - output.len()).min(8 * 1024);
        let mut chunk = [0; 8 * 1024];
        let count = loop {
            match input.read(&mut chunk[..requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_failure(op, error)),
                Ok(count) => break count,
            }
        };
        if count == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..count]);
        if count < requested {
            break;
        }
    }
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(output))
}

fn run_stdin_read_exact(
    input: &SharedInputStream<'_>,
    size: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    if size == 0 {
        return Ok(AsyncIoResult::Bytes(Vec::new()));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    bytes.resize(size, 0);
    let mut input = input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut filled = 0;
    while filled < size {
        check_cancelled(cancelled)?;
        let requested = (size - filled).min(8 * 1024);
        let count = loop {
            match input.read(&mut bytes[filled..filled + requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_failure(op, error)),
                Ok(count) => break count,
            }
        };
        if count == 0 {
            check_cancelled(cancelled)?;
            return Err(AsyncIoFailure::Io {
                op,
                message: "unexpected EOF before the requested byte count".into(),
            });
        }
        filled += count;
    }
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(bytes))
}

fn run_stdin_read_all(
    input: &SharedInputStream<'_>,
    limit: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut output = Vec::new();
    let mut input = input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        check_cancelled(cancelled)?;
        let count = {
            let available = loop {
                match input.fill_buf() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(io_failure(op, error)),
                    Ok(available) => break available,
                }
            };
            if available.is_empty() {
                break;
            }
            if output.len() == limit {
                return Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
            }
            let count = available.len().min(limit - output.len());
            output
                .try_reserve(count)
                .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
            output.extend_from_slice(&available[..count]);
            count
        };
        input.consume(count);
    }
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(output))
}

fn run_stdout_write(
    output: &SharedOutputStream<'_>,
    bytes: &[u8],
    all: bool,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    if !all && bytes.is_empty() {
        return Ok(AsyncIoResult::UInt(0));
    }
    let mut output = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if all {
        output
            .write_all(bytes)
            .map_err(|error| io_failure(op, error))?;
        output.flush().map_err(|error| io_failure(op, error))?;
        check_cancelled(cancelled)?;
        Ok(AsyncIoResult::Nil)
    } else {
        let count = output.write(bytes).map_err(|error| io_failure(op, error))?;
        if count > 0 {
            output.flush().map_err(|error| io_failure(op, error))?;
        }
        check_cancelled(cancelled)?;
        Ok(AsyncIoResult::UInt(
            u64::try_from(count).map_err(|_| AsyncIoFailure::Overflow)?,
        ))
    }
}

fn run_stdout_flush(
    output: &SharedOutputStream<'_>,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
        .map_err(|error| io_failure(op, error))?;
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Nil)
}

fn run_file_open(
    path: String,
    mode: FileOpenMode,
    buffer: Option<FileWriteBuffer>,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let file = open_file_stream(&path, mode, buffer).map_err(|error| io_failure(op, error))?;
    if cancelled.load(Ordering::Acquire) {
        drop(file);
        return Err(AsyncIoFailure::Cancelled);
    }
    Ok(AsyncIoResult::FileOpened {
        file: Arc::new(Mutex::new(file)),
        mode,
    })
}

fn run_file_read(
    file: SharedFileStream,
    max_bytes: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(max_bytes)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    if max_bytes == 0 {
        return Ok(AsyncIoResult::Bytes(output));
    }

    let mut file = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    file.flush().map_err(|error| io_failure(op, error))?;
    while output.len() < max_bytes {
        check_cancelled(cancelled)?;
        let requested = (max_bytes - output.len()).min(8 * 1024);
        let mut chunk = [0; 8 * 1024];
        let count = loop {
            match file.read(&mut chunk[..requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_failure(op, error)),
                Ok(count) => break count,
            }
        };
        if count == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..count]);
        if count < requested {
            break;
        }
    }
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(output))
}

fn run_file_read_exact(
    file: SharedFileStream,
    size: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    if size == 0 {
        return Ok(AsyncIoResult::Bytes(Vec::new()));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
    bytes.resize(size, 0);
    let mut file = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    file.flush().map_err(|error| io_failure(op, error))?;

    let mut filled = 0;
    while filled < size {
        check_cancelled(cancelled)?;
        let requested = (size - filled).min(8 * 1024);
        let count = loop {
            match file.read(&mut bytes[filled..filled + requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_failure(op, error)),
                Ok(count) => break count,
            }
        };
        if count == 0 {
            check_cancelled(cancelled)?;
            return Err(AsyncIoFailure::Io {
                op,
                message: "unexpected EOF before the requested byte count".into(),
            });
        }
        filled += count;
    }

    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(bytes))
}

fn run_file_read_all(
    file: SharedFileStream,
    max_bytes: usize,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut output = Vec::new();
    let mut file = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    file.flush().map_err(|error| io_failure(op, error))?;

    loop {
        check_cancelled(cancelled)?;
        if output.len() == max_bytes {
            let mut probe = [0; 1];
            let count = loop {
                match file.read(&mut probe) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(io_failure(op, error)),
                    Ok(count) => break count,
                }
            };
            if count != 0 {
                return Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
            }
            break;
        }

        let requested = (max_bytes - output.len()).min(8 * 1024);
        let mut chunk = [0; 8 * 1024];
        let count = loop {
            match file.read(&mut chunk[..requested]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_failure(op, error)),
                Ok(count) => break count,
            }
        };
        if count == 0 {
            break;
        }
        output
            .try_reserve(count)
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        output.extend_from_slice(&chunk[..count]);
    }

    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Bytes(output))
}

fn run_file_write(
    file: SharedFileStream,
    bytes: &[u8],
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    if bytes.is_empty() {
        return Ok(AsyncIoResult::UInt(0));
    }
    let mut file = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let count = file.write(bytes).map_err(|error| io_failure(op, error))?;
    // The job may outlive io::close; do not leave accepted bytes in its final Arc.
    file.flush().map_err(|error| io_failure(op, error))?;
    check_cancelled(cancelled)?;
    let count = u64::try_from(count).map_err(|_| AsyncIoFailure::Overflow)?;
    Ok(AsyncIoResult::UInt(count))
}

fn run_file_write_all(
    file: SharedFileStream,
    bytes: &[u8],
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut file = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let write_result = (|| {
        for chunk in bytes.chunks(8 * 1024) {
            check_cancelled(cancelled)?;
            file.write_all(chunk)
                .map_err(|error| io_failure(op, error))?;
        }
        Ok::<(), AsyncIoFailure>(())
    })();
    // A queued write can outlive io::close. Flush even on a partial/cancelled
    // write so the final Arc drop cannot discard a BufferedFile's pending bytes.
    let flush_result = file.flush().map_err(|error| io_failure(op, error));
    write_result?;
    flush_result?;
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Nil)
}

fn run_file_flush(
    file: SharedFileStream,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    file.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
        .map_err(|error| io_failure(op, error))?;
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Nil)
}

fn run_file_seek(
    file: SharedFileStream,
    position: SeekFrom,
    cancelled: &AtomicBool,
    op: &'static str,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let position = file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .seek(position)
        .map_err(|error| io_failure(op, error))?;
    check_cancelled(cancelled)?;
    let position = i64::try_from(position).map_err(|_| AsyncIoFailure::Overflow)?;
    Ok(AsyncIoResult::Int(position))
}

fn fs_call<T>(
    cancelled: &AtomicBool,
    op: &'static str,
    call: impl FnOnce() -> io::Result<T>,
) -> Result<T, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let value = call().map_err(|error| io_failure(op, error))?;
    check_cancelled(cancelled)?;
    Ok(value)
}

fn filesystem_metadata(
    path: &str,
    _limit: usize,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "async::fs::metadata";
    let metadata = fs_call(cancelled, OP, || std::fs::metadata(path))?;
    let len = i64::try_from(metadata.len()).map_err(|_| AsyncIoFailure::Overflow)?;
    Ok(AsyncIoResult::Metadata {
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_readonly: metadata.permissions().readonly(),
        len,
    })
}

fn filesystem_read_dir(
    path: &str,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "async::fs::read_dir";
    let entries = fs_call(cancelled, OP, || std::fs::read_dir(path))?;
    let mut names = Vec::new();
    let mut accounted = 0usize;
    for entry in entries {
        check_cancelled(cancelled)?;
        let name = entry
            .map_err(|error| io_failure(OP, error))?
            .file_name()
            .into_string()
            .map_err(|_| AsyncIoFailure::ParseFailure {
                from: "path",
                to: "str",
            })?;
        let cost = name
            .len()
            .checked_add(std::mem::size_of::<Value>())
            .and_then(|size| size.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        accounted = accounted
            .checked_add(cost)
            .filter(|total| *total <= limit)
            .ok_or(AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        names
            .try_reserve(1)
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        names.push(name);
    }
    names.sort_unstable();
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Directory(names))
}

fn read_text(path: &str, limit: usize, cancelled: &AtomicBool) -> Result<String, AsyncIoFailure> {
    let bytes = read_bytes(path, limit, "async::fs::read_text", cancelled)?;
    let text = String::from_utf8(bytes).map_err(|_| AsyncIoFailure::ParseFailure {
        from: "bytes",
        to: "str",
    })?;
    check_cancelled(cancelled)?;
    Ok(text)
}

fn read_lines(
    path: &str,
    limit: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<String>, AsyncIoFailure> {
    const OP: &str = "async::fs::read_lines";
    let bytes = read_bytes(path, limit, OP, cancelled)?;
    let text = String::from_utf8(bytes).map_err(|_| AsyncIoFailure::ParseFailure {
        from: "bytes",
        to: "str",
    })?;
    let mut lines = Vec::new();
    let mut accounted = 0usize;
    for line in text.lines() {
        check_cancelled(cancelled)?;
        let cost = line
            .len()
            .checked_add(std::mem::size_of::<Value>())
            .and_then(|size| size.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        accounted = accounted
            .checked_add(cost)
            .filter(|total| *total <= limit)
            .ok_or(AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        lines
            .try_reserve(1)
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(line.len())
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        owned.push_str(line);
        lines.push(owned);
    }
    Ok(lines)
}

fn write_bytes(
    path: &str,
    bytes: &[u8],
    op: &'static str,
    append: bool,
    cancelled: &AtomicBool,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options.open(path).map_err(|error| io_failure(op, error))?;
    for chunk in bytes.chunks(8192) {
        check_cancelled(cancelled)?;
        file.write_all(chunk)
            .map_err(|error| io_failure(op, error))?;
    }
    check_cancelled(cancelled)?;
    Ok(AsyncIoResult::Nil)
}

fn read_bytes(
    path: &str,
    limit: usize,
    op: &'static str,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, AsyncIoFailure> {
    check_cancelled(cancelled)?;
    let mut file = std::fs::File::open(path).map_err(|error| io_failure(op, error))?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        check_cancelled(cancelled)?;
        let count = match file.read(&mut chunk) {
            Ok(0) => {
                check_cancelled(cancelled)?;
                return Ok(bytes);
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_failure(op, error)),
        };
        if count > limit.saturating_sub(bytes.len()) {
            return Err(AsyncIoFailure::ResourceLimit { resource: "bytes" });
        }
        check_cancelled(cancelled)?;
        bytes
            .try_reserve(count)
            .map_err(|_| AsyncIoFailure::ResourceLimit { resource: "bytes" })?;
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn run_process(
    program: String,
    arguments: Vec<String>,
    input: Option<Vec<u8>>,
    limit: usize,
    output_limit: Option<usize>,
    timeout: Option<Duration>,
    cancelled: Arc<AtomicBool>,
) -> Result<AsyncIoResult, AsyncIoFailure> {
    const OP: &str = "process::run";
    if cancelled.load(Ordering::Acquire) {
        return Ok(AsyncIoResult::Cancelled);
    }
    let mut output = Vec::new();
    let mut context = filesystem_context(&mut output, limit);
    let argument_values = arguments
        .into_iter()
        .map(|argument| Value::Str(argument.into()))
        .collect();
    let arguments =
        ArrayValue::typed(argument_values, RuntimeType::Str).map_err(|_| AsyncIoFailure::Io {
            op: OP,
            message: "cannot allocate process argument array".into(),
        })?;
    let arguments =
        Value::Array(HeapRef::try_new(arguments).map_err(|error| runtime_failure(OP, error))?);
    let program = Value::Str(program.into());
    let process_arguments = [program, arguments];
    let result = match host::run_cancellable(
        &mut context,
        &process_arguments,
        input,
        timeout,
        output_limit,
        cancelled,
    ) {
        Err(RuntimeError::Cancelled) => return Ok(AsyncIoResult::Cancelled),
        result => result.map_err(|error| runtime_failure(OP, error))?,
    };
    let Value::Record(result) = result else {
        return Err(AsyncIoFailure::Io {
            op: OP,
            message: "process returned an invalid result".into(),
        });
    };
    let exit_code = match result
        .try_get_field("exit_code")
        .map_err(|error| runtime_failure(OP, error))?
    {
        Some(Value::Int(code)) => i32::try_from(code).ok(),
        Some(Value::Nil) => None,
        _ => {
            return Err(AsyncIoFailure::Io {
                op: OP,
                message: "process returned an invalid exit code".into(),
            });
        }
    };
    let success = match result
        .try_get_field("success")
        .map_err(|error| runtime_failure(OP, error))?
    {
        Some(Value::Bool(success)) => success,
        _ => {
            return Err(AsyncIoFailure::Io {
                op: OP,
                message: "process returned an invalid success flag".into(),
            });
        }
    };
    let stdout = bytes_field(&result, "stdout", OP)?;
    let stderr = bytes_field(&result, "stderr", OP)?;
    Ok(AsyncIoResult::Process {
        exit_code,
        success,
        stdout,
        stderr,
    })
}

fn bytes_field(
    record: &crate::value::RecordValue,
    name: &str,
    op: &'static str,
) -> Result<Vec<u8>, AsyncIoFailure> {
    match record
        .try_get_field(name)
        .map_err(|error| runtime_failure(op, error))?
    {
        Some(Value::Bytes(bytes)) => {
            bytes
                .try_borrow()
                .map(|bytes| bytes.clone())
                .map_err(|_| AsyncIoFailure::Io {
                    op,
                    message: "cannot read process output".into(),
                })
        }
        _ => Err(AsyncIoFailure::Io {
            op,
            message: "process returned invalid output".into(),
        }),
    }
}

fn runtime_failure(op: &'static str, error: RuntimeError) -> AsyncIoFailure {
    match error {
        RuntimeError::ResourceLimit { resource } => AsyncIoFailure::ResourceLimit { resource },
        RuntimeError::ParseFailure { from, to } => AsyncIoFailure::ParseFailure { from, to },
        RuntimeError::Overflow => AsyncIoFailure::Overflow,
        RuntimeError::Timeout => AsyncIoFailure::Timeout,
        RuntimeError::IoFailure { message, .. } => AsyncIoFailure::Io { op, message },
        error => AsyncIoFailure::Io {
            op,
            message: error.to_string(),
        },
    }
}

fn io_failure(op: &'static str, error: io::Error) -> AsyncIoFailure {
    AsyncIoFailure::Io {
        op,
        message: error.to_string(),
    }
}
