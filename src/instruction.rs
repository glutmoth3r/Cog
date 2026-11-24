//! Portable stack-machine instruction definitions.
//!
//! The backend emits these instructions; the bytecode codec and verifier inspect
//! them, and both the interpreter and JIT consume them. Runtime semantics belong
//! to the interpreter, so a new lowering must preserve its observable behavior.
use crate::template::ArrayTemplate;
use crate::value::{RuntimeType, Value};
use std::rc::Rc;

/// Runtime value classification. Nominal enum and struct identities are resolved
/// by the compiler; predicates never cast, narrow a binding, or propagate an Error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Int,
    UInt,
    BigInt,
    Float,
    Bool,
    Char,
    Str,
    Nil,
    Error,
    Enum(Rc<str>),
    Variant(Rc<str>),
    /// A declared, non-generic struct identified by its resolved name. It
    /// matches only records constructed by that struct, never nil or Error.
    Struct(Rc<str>),
}

impl ValueType {
    pub(crate) fn primitive(name: &str) -> Option<Self> {
        Some(match name {
            "int" => Self::Int,
            "uint" => Self::UInt,
            "bigint" => Self::BigInt,
            "float" => Self::Float,
            "bool" => Self::Bool,
            "char" => Self::Char,
            "str" => Self::Str,
            "nil" => Self::Nil,
            _ => return None,
        })
    }

    pub(crate) fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Int, Value::Int(_))
            | (Self::UInt, Value::UInt(_))
            | (Self::BigInt, Value::BigInt(_))
            | (Self::Float, Value::Float(_))
            | (Self::Bool, Value::Bool(_))
            | (Self::Char, Value::Char(_))
            | (Self::Str, Value::Str(_))
            | (Self::Nil, Value::Nil)
            | (Self::Error, Value::Error(_)) => true,
            (Self::Enum(name), Value::Variant(tag, _)) => tag
                .rsplit_once("::")
                .is_some_and(|(parent, _)| parent == name.as_ref()),
            (Self::Variant(name), Value::Variant(tag, _)) => name == tag,
            (Self::Struct(name), Value::Record(record)) => {
                record.nominal_name() == Some(name.as_ref())
            }
            (Self::Variant(name), Value::Error(error)) => match error.primary() {
                crate::error::RuntimeError::LanguageError { tag, .. } => name == tag,
                error => {
                    name.strip_prefix(crate::error::ERROR_TYPE)
                        .and_then(|tail| tail.strip_prefix("::"))
                        == Some(error.kind())
                }
            },
            _ => false,
        }
    }
}

impl std::fmt::Display for ValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enum(name) => write!(f, "enum {name}"),
            Self::Variant(name) => write!(f, "variant {name}"),
            Self::Struct(name) => write!(f, "struct {name}"),
            Self::Int => f.write_str("int"),
            Self::UInt => f.write_str("uint"),
            Self::BigInt => f.write_str("bigint"),
            Self::Float => f.write_str("float"),
            Self::Bool => f.write_str("bool"),
            Self::Char => f.write_str("char"),
            Self::Str => f.write_str("str"),
            Self::Nil => f.write_str("nil"),
            Self::Error => f.write_str("error"),
        }
    }
}
#[derive(Debug, Clone)]
pub enum Instruction {
    /// Index into the program's checked and linked native import table.
    CallNative(usize),
    Add,
    Mul,
    Div,
    Mod,
    Sub,
    Push(Value),
    PushArray(ArrayTemplate),
    Load(usize),
    Store(usize),
    LoadGlobal(usize),
    StoreGlobal(usize),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    Halt,
    Call(usize, usize),      // target, argument count
    CallAsync(usize, usize), // async target, argument count
    Await,
    Spawn,
    Cancel,
    CheckCancel,
    YieldNow,
    TimeoutAsync,
    Select,
    Select2,
    SleepAsync,
    ReadBytesAsync,
    ProcessRunAsync,
    ProcessRunTimeoutAsync,
    ProcessRunInputAsync,
    ProcessRunInputTimeoutAsync,
    ProcessRunInputTimeoutOutputLimitAsync,
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
    Return,
    /// Return an Error and mark it for propagation through the caller.
    ReturnPropagate,
    /// Consume the propagated-return marker and jump when it was not set.
    JumpIfNoPropagation(usize),
    IsType(ValueType),
    ErrorKind,
    And,
    Or,
    Xor,
    Not,
    Len,
    Exclusive,
    Dup,
    Pop,
    Swap,
    /// Restore the operand stack to a depth relative to the current call frame.
    StackTruncate(usize),
    CallValue(usize),
    Index,
    IndexSet,
    MakeCell,
    CellGet,
    CellSet,
    MakeRecord(usize),
    MakeArray(usize),
    MakeTuple(usize),
    MakeTypedTuple(usize, Vec<RuntimeType>),
    MakeTypedArray(usize, RuntimeType),
    ConstrainRecordFields(Vec<(String, RuntimeType)>),
    ConstrainArrayType(RuntimeType),
    ConstrainArrayFrom,
    ArrayEmptyLike,
    MakeClosure(usize, usize),
    LoadCapture(usize),
    Crash(Option<Rc<str>>),
    // `<<` — mutate the array in place (push to the end), unlike Add/Sub/etc this
    // does NOT allocate a new array: it must preserve Rc<RefCell<..>> aliasing
    // (see docs/LANGUAGE.md on shared-mutation semantics). Returns the same array
    // value so `arr << x` can be chained/used as an expression.
    ArrayPush,
    // `>>` — mutate the array in place (insert at the front). Same aliasing
    // requirement as ArrayPush.
    ArrayPrepend,
}

impl Instruction {
    /// Required operands and resulting values, relative to the active frame.
    /// StackTruncate sets an absolute frame-relative height instead of applying
    /// the ordinary pop/push rule. Return and Crash have no normal successor.
    #[inline]
    pub(crate) fn stack_effect(
        &self,
        imports: &[crate::runtime::NativeImport],
    ) -> Result<(usize, usize), crate::error::RuntimeError> {
        use crate::error::RuntimeError;
        Ok(match self {
            Self::Push(_)
            | Self::PushArray(_)
            | Self::Load(_)
            | Self::LoadGlobal(_)
            | Self::LoadCapture(_) => (0, 1),
            Self::Store(_)
            | Self::StoreGlobal(_)
            | Self::Pop
            | Self::JumpIfFalse(_)
            | Self::JumpIfTrue(_)
            | Self::Return
            | Self::ReturnPropagate
            | Self::Crash(_) => (1, 0),
            Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Mod
            | Self::Eq
            | Self::Ne
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::And
            | Self::Or
            | Self::Xor
            | Self::Index
            | Self::CellSet
            | Self::ArrayPush
            | Self::ArrayPrepend
            | Self::ConstrainArrayFrom => (2, 1),
            Self::IndexSet => (3, 1),
            Self::IsType(_)
            | Self::ErrorKind
            | Self::Not
            | Self::Len
            | Self::Exclusive
            | Self::Cancel
            | Self::MakeCell
            | Self::CellGet
            | Self::ConstrainArrayType(_)
            | Self::ConstrainRecordFields(_)
            | Self::ArrayEmptyLike => (1, 1),
            Self::Dup => (1, 2),
            Self::Swap => (2, 2),
            Self::Call(_, n)
            | Self::MakeArray(n)
            | Self::MakeTuple(n)
            | Self::MakeTypedTuple(n, _)
            | Self::MakeTypedArray(n, _)
            | Self::MakeClosure(_, n) => (*n, 1),
            Self::CallAsync(_, n) => (*n, 1),
            Self::CheckCancel => (0, 1),
            Self::YieldNow => (0, 1),
            Self::TimeoutAsync => (2, 1),
            Self::Select2 => (2, 1),
            Self::Select => (1, 1),
            Self::Await
            | Self::Spawn
            | Self::SleepAsync
            | Self::ReadBytesAsync
            | Self::FsMetadataAsync
            | Self::FsReadDirAsync
            | Self::FsCreateDirAsync
            | Self::FsCreateDirAllAsync
            | Self::FsRemoveFileAsync
            | Self::FsRemoveDirAsync
            | Self::FsReadTextAsync
            | Self::FsReadLinesAsync => (1, 1),
            Self::TcpReadAllAsync => (1, 1),
            Self::ProcessRunAsync
            | Self::FsWriteBytesAsync
            | Self::FsWriteTextAsync
            | Self::FsAppendBytesAsync
            | Self::FsAppendTextAsync
            | Self::FsCopyAsync
            | Self::FsRenameAsync
            | Self::TcpReadAsync
            | Self::TcpReadExactAsync
            | Self::TcpWriteAsync
            | Self::TcpWriteAllAsync => (2, 1),
            Self::TcpListenAsync => (2, 1),
            Self::TcpAcceptAsync => (3, 1),
            Self::ProcessRunTimeoutAsync => (3, 1),
            Self::ProcessRunInputAsync => (3, 1),
            Self::ProcessRunInputTimeoutAsync => (4, 1),
            Self::ProcessRunInputTimeoutOutputLimitAsync => (5, 1),
            Self::AsyncIoSeek => (3, 1),
            Self::AsyncIoOpen => (3, 1),
            Self::AsyncIoFlush => (1, 1),
            Self::AsyncIoRead
            | Self::AsyncIoReadExact
            | Self::AsyncIoWrite
            | Self::AsyncIoWriteAll => (2, 1),
            Self::AsyncIoReadAll => (1, 1),
            Self::NetworkExchangeAsync => (4, 1),
            Self::TcpConnectAsync => (3, 1),
            Self::TcpShutdownWriteAsync => (1, 1),
            Self::CallValue(n) => (n.checked_add(1).ok_or(RuntimeError::StackUnderflow)?, 1),
            Self::MakeRecord(n) => (n.checked_mul(2).ok_or(RuntimeError::StackUnderflow)?, 1),
            Self::CallNative(index) => (
                imports
                    .get(*index)
                    .ok_or(RuntimeError::InvalidNativeCall)?
                    .parameters()
                    .len(),
                1,
            ),
            Self::StackTruncate(depth) => (*depth, 0),
            Self::Jump(_) | Self::JumpIfNoPropagation(_) | Self::Halt => (0, 0),
        })
    }
}
