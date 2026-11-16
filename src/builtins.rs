//! Native builtin metadata shared by checking and lowering.
pub(crate) fn native_module(name: &str) -> bool {
    matches!(
        name,
        "err"
            | "build"
            | "task"
            | "async"
            | "async::time"
            | "async::fs"
            | "async::io"
            | "async::net"
    ) || crate::runtime::names().any(|function| {
        function
            .split_once("::")
            .is_some_and(|(module, _)| module == name)
    })
}

pub(crate) fn native_modules() -> std::collections::BTreeSet<String> {
    [
        "err",
        "build",
        "task",
        "async",
        "async::time",
        "async::fs",
        "async::io",
        "async::net",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain(
        crate::runtime::names()
            .filter_map(|name| name.split_once("::").map(|(module, _)| module.to_owned())),
    )
    .collect()
}

#[derive(Clone)]
pub(crate) enum Builtin {
    Native(crate::runtime::NativeImport),
    Len,
    Exclusive,
    ErrorKind,
    BuildInput,
    Spawn,
    Cancel,
    YieldNow,
    TimeoutAsync,
    Select,
    Select2,
    SleepAsync,
    ReadBytesAsync,
    ProcessRunAsync,
    NetworkExchangeAsync,
    TcpListenAsync,
    TcpAcceptAsync,
    TcpConnectAsync,
    TcpReadAsync,
    TcpReadExactAsync,
    TcpReadAllAsync,
    TcpWriteAsync,
    TcpWriteAllAsync,
    TcpShutdownWriteAsync,
    FsMetadataAsync,
    FsReadDirAsync,
    FsWriteBytesAsync,
    FsCreateDirAsync,
    FsCreateDirAllAsync,
    FsRemoveFileAsync,
    FsRemoveDirAsync,
    FsReadTextAsync,
    FsWriteTextAsync,
    FsReadLinesAsync,
    FsAppendBytesAsync,
    FsAppendTextAsync,
    FsCopyAsync,
    FsRenameAsync,
    AsyncIoOpen,
    AsyncIoRead,
    AsyncIoReadExact,
    AsyncIoReadAll,
    AsyncIoWrite,
    AsyncIoWriteAll,
    AsyncIoFlush,
    AsyncIoSeek,
}

impl Builtin {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "len" => Some(Builtin::Len),
            "exclusive" => Some(Builtin::Exclusive),
            "err::kind" => Some(Builtin::ErrorKind),
            "spawn" => Some(Builtin::Spawn),
            "cancel" => Some(Builtin::Cancel),
            "yield_now" => Some(Builtin::YieldNow),
            "async::time::timeout" => Some(Builtin::TimeoutAsync),
            "task::select" => Some(Builtin::Select),
            "task::select2" => Some(Builtin::Select2),
            "async::time::sleep" => Some(Builtin::SleepAsync),
            "async::fs::read_bytes" => Some(Builtin::ReadBytesAsync),
            "async::fs::metadata" => Some(Builtin::FsMetadataAsync),
            "async::fs::read_dir" => Some(Builtin::FsReadDirAsync),
            "async::fs::write_bytes" => Some(Builtin::FsWriteBytesAsync),
            "async::fs::create_dir" => Some(Builtin::FsCreateDirAsync),
            "async::fs::create_dir_all" => Some(Builtin::FsCreateDirAllAsync),
            "async::fs::remove_file" => Some(Builtin::FsRemoveFileAsync),
            "async::fs::remove_dir" => Some(Builtin::FsRemoveDirAsync),
            "async::fs::read_text" => Some(Builtin::FsReadTextAsync),
            "async::fs::write_text" => Some(Builtin::FsWriteTextAsync),
            "async::fs::read_lines" => Some(Builtin::FsReadLinesAsync),
            "async::fs::append_bytes" => Some(Builtin::FsAppendBytesAsync),
            "async::fs::append_text" => Some(Builtin::FsAppendTextAsync),
            "async::fs::copy" => Some(Builtin::FsCopyAsync),
            "async::fs::rename" => Some(Builtin::FsRenameAsync),
            "async::io::open" => Some(Builtin::AsyncIoOpen),
            "async::io::read" => Some(Builtin::AsyncIoRead),
            "async::io::read_exact" => Some(Builtin::AsyncIoReadExact),
            "async::io::read_all" => Some(Builtin::AsyncIoReadAll),
            "async::io::write" => Some(Builtin::AsyncIoWrite),
            "async::io::write_all" => Some(Builtin::AsyncIoWriteAll),
            "async::io::flush" => Some(Builtin::AsyncIoFlush),
            "async::io::seek" => Some(Builtin::AsyncIoSeek),
            "process::run" => Some(Builtin::ProcessRunAsync),
            "async::net::exchange" => Some(Builtin::NetworkExchangeAsync),
            "async::net::listen" => Some(Builtin::TcpListenAsync),
            "async::net::accept" => Some(Builtin::TcpAcceptAsync),
            "async::net::connect" => Some(Builtin::TcpConnectAsync),
            "async::net::read" => Some(Builtin::TcpReadAsync),
            "async::net::read_exact" => Some(Builtin::TcpReadExactAsync),
            "async::net::read_all" => Some(Builtin::TcpReadAllAsync),
            "async::net::write" => Some(Builtin::TcpWriteAsync),
            "async::net::write_all" => Some(Builtin::TcpWriteAllAsync),
            "async::net::shutdown_write" => Some(Builtin::TcpShutdownWriteAsync),
            "build::include_str"
            | "build::include_bytes"
            | "build::try_include_str"
            | "build::try_include_bytes" => Some(Builtin::BuildInput),
            _ => crate::runtime::NativeImport::named(name)
                .ok()
                .map(Self::Native),
        }
    }

    // VM primitives retain explicit arities; runtime calls use linked signatures.
    pub(crate) fn arity(&self) -> usize {
        match self {
            Builtin::Native(import) => import.parameters().len(),
            Builtin::YieldNow => 0,
            Builtin::ProcessRunAsync
            | Builtin::TimeoutAsync
            | Builtin::Select2
            | Builtin::FsWriteBytesAsync
            | Builtin::FsWriteTextAsync
            | Builtin::FsAppendBytesAsync
            | Builtin::FsAppendTextAsync
            | Builtin::FsCopyAsync
            | Builtin::FsRenameAsync
            | Builtin::TcpListenAsync
            | Builtin::TcpReadAsync
            | Builtin::TcpReadExactAsync
            | Builtin::TcpWriteAsync
            | Builtin::TcpWriteAllAsync => 2,
            Builtin::AsyncIoRead
            | Builtin::AsyncIoReadExact
            | Builtin::AsyncIoWrite
            | Builtin::AsyncIoWriteAll => 2,
            Builtin::AsyncIoOpen => 3,
            Builtin::AsyncIoSeek => 3,
            Builtin::TcpAcceptAsync => 3,
            Builtin::NetworkExchangeAsync => 4,
            Builtin::TcpConnectAsync => 3,
            _ => 1,
        }
    }

    pub(crate) fn accepts_arity(&self, arity: usize) -> bool {
        match self {
            Builtin::ProcessRunAsync => (2..=5).contains(&arity),
            _ => self.arity() == arity,
        }
    }

    pub(crate) fn instruction_for_arity(
        &self,
        arity: usize,
    ) -> Option<crate::instruction::Instruction> {
        if !self.accepts_arity(arity) {
            return None;
        }
        match (self, arity) {
            (Builtin::ProcessRunAsync, 2) => Some(crate::instruction::Instruction::ProcessRunAsync),
            (Builtin::ProcessRunAsync, 3) => {
                Some(crate::instruction::Instruction::ProcessRunTimeoutAsync)
            }
            (Builtin::ProcessRunAsync, 4) => {
                Some(crate::instruction::Instruction::ProcessRunInputTimeoutAsync)
            }
            (Builtin::ProcessRunAsync, 5) => {
                Some(crate::instruction::Instruction::ProcessRunInputTimeoutOutputLimitAsync)
            }
            _ => self.instruction(),
        }
    }

    pub(crate) fn returns_task(&self) -> bool {
        match self {
            Builtin::Native(import) => import.is_async(),
            _ => matches!(
                self,
                Builtin::SleepAsync
                    | Builtin::YieldNow
                    | Builtin::TimeoutAsync
                    | Builtin::Select
                    | Builtin::Select2
                    | Builtin::ReadBytesAsync
                    | Builtin::ProcessRunAsync
                    | Builtin::NetworkExchangeAsync
                    | Builtin::TcpListenAsync
                    | Builtin::TcpAcceptAsync
                    | Builtin::TcpConnectAsync
                    | Builtin::TcpReadAsync
                    | Builtin::TcpReadExactAsync
                    | Builtin::TcpReadAllAsync
                    | Builtin::TcpWriteAsync
                    | Builtin::TcpWriteAllAsync
                    | Builtin::TcpShutdownWriteAsync
                    | Builtin::FsMetadataAsync
                    | Builtin::FsReadDirAsync
                    | Builtin::FsWriteBytesAsync
                    | Builtin::FsCreateDirAsync
                    | Builtin::FsCreateDirAllAsync
                    | Builtin::FsRemoveFileAsync
                    | Builtin::FsRemoveDirAsync
                    | Builtin::FsReadTextAsync
                    | Builtin::FsWriteTextAsync
                    | Builtin::FsReadLinesAsync
                    | Builtin::FsAppendBytesAsync
                    | Builtin::FsAppendTextAsync
                    | Builtin::FsCopyAsync
                    | Builtin::FsRenameAsync
                    | Builtin::AsyncIoOpen
                    | Builtin::AsyncIoRead
                    | Builtin::AsyncIoReadExact
                    | Builtin::AsyncIoReadAll
                    | Builtin::AsyncIoWrite
                    | Builtin::AsyncIoWriteAll
                    | Builtin::AsyncIoFlush
                    | Builtin::AsyncIoSeek
            ),
        }
    }

    pub(crate) fn instruction(&self) -> Option<crate::instruction::Instruction> {
        Some(match self {
            Builtin::Native(_) | Builtin::BuildInput => return None,
            Builtin::Len => crate::instruction::Instruction::Len,
            Builtin::Exclusive => crate::instruction::Instruction::Exclusive,
            Builtin::ErrorKind => crate::instruction::Instruction::ErrorKind,
            Builtin::Spawn => crate::instruction::Instruction::Spawn,
            Builtin::Cancel => crate::instruction::Instruction::Cancel,
            Builtin::YieldNow => crate::instruction::Instruction::YieldNow,
            Builtin::TimeoutAsync => crate::instruction::Instruction::TimeoutAsync,
            Builtin::Select => crate::instruction::Instruction::Select,
            Builtin::Select2 => crate::instruction::Instruction::Select2,
            Builtin::SleepAsync => crate::instruction::Instruction::SleepAsync,
            Builtin::ReadBytesAsync => crate::instruction::Instruction::ReadBytesAsync,
            Builtin::ProcessRunAsync => crate::instruction::Instruction::ProcessRunAsync,
            Builtin::NetworkExchangeAsync => crate::instruction::Instruction::NetworkExchangeAsync,
            Builtin::TcpListenAsync => crate::instruction::Instruction::TcpListenAsync,
            Builtin::TcpAcceptAsync => crate::instruction::Instruction::TcpAcceptAsync,
            Builtin::TcpConnectAsync => crate::instruction::Instruction::TcpConnectAsync,
            Builtin::TcpReadAsync => crate::instruction::Instruction::TcpReadAsync,
            Builtin::TcpReadExactAsync => crate::instruction::Instruction::TcpReadExactAsync,
            Builtin::TcpReadAllAsync => crate::instruction::Instruction::TcpReadAllAsync,
            Builtin::TcpWriteAsync => crate::instruction::Instruction::TcpWriteAsync,
            Builtin::TcpWriteAllAsync => crate::instruction::Instruction::TcpWriteAllAsync,
            Builtin::TcpShutdownWriteAsync => {
                crate::instruction::Instruction::TcpShutdownWriteAsync
            }
            Builtin::FsMetadataAsync => crate::instruction::Instruction::FsMetadataAsync,
            Builtin::FsReadDirAsync => crate::instruction::Instruction::FsReadDirAsync,
            Builtin::FsWriteBytesAsync => crate::instruction::Instruction::FsWriteBytesAsync,
            Builtin::FsCreateDirAsync => crate::instruction::Instruction::FsCreateDirAsync,
            Builtin::FsCreateDirAllAsync => crate::instruction::Instruction::FsCreateDirAllAsync,
            Builtin::FsRemoveFileAsync => crate::instruction::Instruction::FsRemoveFileAsync,
            Builtin::FsRemoveDirAsync => crate::instruction::Instruction::FsRemoveDirAsync,
            Builtin::FsReadTextAsync => crate::instruction::Instruction::FsReadTextAsync,
            Builtin::FsWriteTextAsync => crate::instruction::Instruction::FsWriteTextAsync,
            Builtin::FsReadLinesAsync => crate::instruction::Instruction::FsReadLinesAsync,
            Builtin::FsAppendBytesAsync => crate::instruction::Instruction::FsAppendBytesAsync,
            Builtin::FsAppendTextAsync => crate::instruction::Instruction::FsAppendTextAsync,
            Builtin::FsCopyAsync => crate::instruction::Instruction::FsCopyAsync,
            Builtin::FsRenameAsync => crate::instruction::Instruction::FsRenameAsync,
            Builtin::AsyncIoOpen => crate::instruction::Instruction::AsyncIoOpen,
            Builtin::AsyncIoRead => crate::instruction::Instruction::AsyncIoRead,
            Builtin::AsyncIoReadExact => crate::instruction::Instruction::AsyncIoReadExact,
            Builtin::AsyncIoReadAll => crate::instruction::Instruction::AsyncIoReadAll,
            Builtin::AsyncIoWrite => crate::instruction::Instruction::AsyncIoWrite,
            Builtin::AsyncIoWriteAll => crate::instruction::Instruction::AsyncIoWriteAll,
            Builtin::AsyncIoFlush => crate::instruction::Instruction::AsyncIoFlush,
            Builtin::AsyncIoSeek => crate::instruction::Instruction::AsyncIoSeek,
        })
    }
}
