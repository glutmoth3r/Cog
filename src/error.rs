//! Runtime failure categories shared by the VM and native host layer.
//!
//! Recoverable program failures travel as `Value::Error`; invalid VM state and
//! explicit `crash` terminate execution. Source/compiler diagnostics live in
//! [`crate::diagnostic`] and do not become catchable Cog errors.
use crate::diagnostic::SourceLocation;
use std::rc::Rc;

pub(crate) const ERROR_TYPE: &str = "$core::err::Error";

pub(crate) const MAX_ERROR_TRACE_FRAMES: usize = 24;

/// One Cog instruction in a diagnostic trace. Unmapped instructions keep their
/// bytecode address without borrowing a coordinate from a different operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ErrorTraceFrame {
    pub instruction: usize,
    pub function: Option<String>,
    pub location: Option<SourceLocation>,
    pub native_operation: Option<String>,
    /// The compiler inlined this function into the next frame; both frames
    /// share the bytecode instruction.
    pub inlined: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ErrorProvenance {
    pub origin: ErrorTraceFrame,
    /// Innermost caller first.
    pub callers: Vec<ErrorTraceFrame>,
    pub propagation: Vec<ErrorTraceFrame>,
    pub observation: Option<ErrorTraceFrame>,
    pub truncated: bool,
}

/// A host-facing snapshot that owns no Cog heap values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeDiagnostic {
    pub kind: String,
    pub message: String,
    pub provenance: Option<ErrorProvenance>,
    pub suppressed: Vec<RuntimeDiagnostic>,
    pub suppressed_truncated: bool,
    pub unhandled_error: bool,
}

impl RuntimeDiagnostic {
    pub(crate) fn new(error: &RuntimeError) -> Self {
        let mut remaining = MAX_ERROR_TRACE_FRAMES;
        Self::snapshot(error, &mut remaining, 0)
    }

    fn snapshot(error: &RuntimeError, remaining: &mut usize, depth: usize) -> Self {
        let mut suppressed = Vec::new();
        for secondary in error.suppressed() {
            if *remaining == 0 || depth >= 4 {
                break;
            }
            *remaining -= 1;
            suppressed.push(Self::snapshot(secondary, remaining, depth + 1));
        }
        Self {
            kind: error.kind().into(),
            message: error.to_string(),
            provenance: error.provenance().map(|trace| (**trace).clone()),
            suppressed_truncated: suppressed.len() < error.suppressed().len(),
            suppressed,
            unhandled_error: error.is_recoverable()
                && error
                    .provenance()
                    .is_some_and(|trace| trace.observation.is_some()),
        }
    }

    /// Render the same structured records exposed to embedders.
    pub fn render(&self, fallback: Option<&SourceLocation>) -> String {
        use std::fmt::Write as _;
        let location = self
            .provenance
            .as_ref()
            .and_then(|trace| trace.origin.location.as_ref())
            .or(fallback);
        let mut output = if let Some(location) = location {
            format!(
                "runtime error at {}:{}:{}: {}",
                location.file.escape_debug(),
                location.line,
                location.column,
                self.message
            )
        } else {
            format!("runtime error: {}", self.message)
        };
        self.write_trace(&mut output, 0);
        if self.unhandled_error {
            let _ = write!(output, "\nUnhandled Error reached the program entry point.");
        }
        output
    }

    fn write_trace(&self, output: &mut String, depth: usize) {
        use std::fmt::Write as _;
        let indent = "  ".repeat(depth);
        if let Some(trace) = &self.provenance {
            let mut frame = |label, frame: &ErrorTraceFrame| {
                let _ = write!(output, "\n{indent}  {label} {frame}");
            };
            frame("origin:", &trace.origin);
            for caller in &trace.callers {
                frame("called from", caller);
            }
            for site in &trace.propagation {
                frame("propagated through", site);
            }
            if let Some(site) = &trace.observation {
                frame("observed at", site);
            }
            if trace.truncated {
                let _ = write!(output, "\n{indent}  … trace truncated");
            }
        }
        for secondary in &self.suppressed {
            let _ = write!(
                output,
                "\n{indent}  suppressed {}: {}",
                secondary.kind.escape_debug(),
                secondary.message,
            );
            secondary.write_trace(output, depth + 1);
        }
        if self.suppressed_truncated {
            let _ = write!(output, "\n{indent}  … suppressed diagnostics truncated");
        }
    }
}

impl std::fmt::Display for ErrorTraceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(function) = &self.function {
            write!(f, "{} at ", function.escape_debug())?;
        }
        if let Some(location) = &self.location {
            write!(
                f,
                "{}:{}:{}",
                location.file.escape_debug(),
                location.line,
                location.column
            )?;
        } else {
            write!(f, "instruction {}", self.instruction)?;
        }
        if let Some(operation) = &self.native_operation {
            write!(f, " (native {})", operation.escape_debug())?;
        }
        if self.inlined {
            write!(f, " (inlined)")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum RuntimeError {
    StackUnderflow,
    CallStackUnderflow,
    TypeMismatch {
        op: &'static str,
    },
    DivisionByZero,
    Overflow,
    InvalidLocal(usize),
    NegativeSleep(f64),
    Timeout,
    MissingHalt,
    InvalidJump(usize),
    InvalidCall(usize),
    ParseFailure {
        from: &'static str,
        to: &'static str,
    },
    IndexOutOfBounds {
        index: i64,
        len: usize,
    },
    MissingField {
        field: String,
    },
    InvalidCapture(usize),
    InvalidNativeCall,
    NativeFailure {
        message: String,
    },
    InvalidResource,
    InvalidArgument {
        op: &'static str,
    },
    Cancelled,
    ResourceLimit {
        resource: &'static str,
    },
    IoFailure {
        op: &'static str,
        message: String,
    },
    PermissionDenied {
        op: &'static str,
    },
    ProcessFailed {
        command: String,
        code: Option<i32>,
    },
    IoReadFailed,
    IoWriteFailed {
        message: String,
    },
    // Distinct from `IoReadFailed` (whose Display text is specifically about
    // stdin/EOF) — this is `entropy::bytes` failing to reach the OS entropy
    // source (e.g. `/dev/urandom` missing or unreadable), a different
    // failure a caller of `random::*` needs to be able to tell apart.
    EntropyUnavailable,
    InvalidRangeStep,
    Unordered,
    /// An explicitly constructed variant of the standard-library Error enum.
    LanguageError {
        tag: std::rc::Rc<str>,
        fields: crate::heap::HeapRef<crate::value::ValueFields>,
    },
    /// Cleanup diagnostics attached without changing the primary language Error.
    WithSuppressed {
        primary: Box<RuntimeError>,
        suppressed: Vec<RuntimeError>,
    },
    WithProvenance {
        error: Box<RuntimeError>,
        provenance: Rc<ErrorProvenance>,
    },
    Crashed {
        context: Option<std::rc::Rc<str>>,
        value: crate::value::Value,
    },
}

impl RuntimeError {
    pub fn primary(&self) -> &Self {
        match self {
            Self::WithSuppressed { primary, .. } => primary.primary(),
            Self::WithProvenance { error, .. } => error.primary(),
            error => error,
        }
    }

    pub fn suppressed(&self) -> &[RuntimeError] {
        match self {
            Self::WithSuppressed { suppressed, .. } => suppressed,
            Self::WithProvenance { error, .. } => error.suppressed(),
            _ => &[],
        }
    }

    pub fn with_suppressed(self, secondary: RuntimeError) -> Self {
        match self {
            Self::WithProvenance { error, provenance } => Self::WithProvenance {
                error: Box::new(error.with_suppressed(secondary)),
                provenance,
            },
            Self::WithSuppressed {
                primary,
                mut suppressed,
            } => {
                suppressed.push(secondary);
                Self::WithSuppressed {
                    primary,
                    suppressed,
                }
            }
            primary => Self::WithSuppressed {
                primary: Box::new(primary),
                suppressed: vec![secondary],
            },
        }
    }

    pub fn provenance(&self) -> Option<&Rc<ErrorProvenance>> {
        match self {
            Self::WithProvenance { provenance, .. } => Some(provenance),
            Self::WithSuppressed { primary, .. } => primary.provenance(),
            _ => None,
        }
    }

    pub(crate) fn attach_provenance(&mut self, provenance: ErrorProvenance) {
        let error = std::mem::replace(self, Self::MissingHalt);
        *self = Self::WithProvenance {
            error: Box::new(error),
            provenance: Rc::new(provenance),
        };
    }

    fn provenance_mut(&mut self) -> Option<&mut ErrorProvenance> {
        match self {
            Self::WithProvenance { provenance, .. } => Some(Rc::make_mut(provenance)),
            Self::WithSuppressed { primary, .. } => primary.provenance_mut(),
            _ => None,
        }
    }

    pub(crate) fn record_propagation(&mut self, frame: ErrorTraceFrame) {
        if let Some(provenance) = self.provenance_mut() {
            if provenance.propagation.len() < MAX_ERROR_TRACE_FRAMES {
                provenance.propagation.push(frame);
            } else {
                provenance.truncated = true;
            }
        }
    }

    pub(crate) fn record_observation(&mut self, frame: ErrorTraceFrame) {
        if let Some(provenance) = self.provenance_mut() {
            provenance.observation = Some(frame);
        }
    }

    pub(crate) fn without_provenance(self) -> Self {
        match self {
            Self::WithProvenance { error, .. } => error.without_provenance(),
            Self::WithSuppressed {
                primary,
                suppressed,
            } => Self::WithSuppressed {
                primary: Box::new(primary.without_provenance()),
                suppressed,
            },
            error => error,
        }
    }

    pub fn is_recoverable(&self) -> bool {
        matches!(
            self.primary(),
            RuntimeError::DivisionByZero
                | RuntimeError::Overflow
                | RuntimeError::TypeMismatch { .. }
                | RuntimeError::NegativeSleep(_)
                | RuntimeError::Timeout
                | RuntimeError::ParseFailure { .. }
                | RuntimeError::IndexOutOfBounds { .. }
                | RuntimeError::MissingField { .. }
                | RuntimeError::ProcessFailed { .. }
                | RuntimeError::IoReadFailed
                | RuntimeError::IoWriteFailed { .. }
                | RuntimeError::IoFailure { .. }
                | RuntimeError::InvalidResource
                | RuntimeError::InvalidArgument { .. }
                | RuntimeError::Cancelled
                | RuntimeError::NativeFailure { .. }
                | RuntimeError::ResourceLimit { .. }
                | RuntimeError::EntropyUnavailable
                | RuntimeError::PermissionDenied { .. }
                | RuntimeError::InvalidRangeStep
                | RuntimeError::Unordered
                | RuntimeError::LanguageError { .. }
        )
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::StackUnderflow => "StackUnderflow",
            Self::CallStackUnderflow => "CallStackUnderflow",
            Self::TypeMismatch { .. } => "TypeMismatch",
            Self::DivisionByZero => "DivisionByZero",
            Self::Overflow => "Overflow",
            Self::InvalidLocal(_) => "InvalidLocal",
            Self::NegativeSleep(_) => "NegativeSleep",
            Self::Timeout => "Timeout",
            Self::MissingHalt => "MissingHalt",
            Self::InvalidJump(_) => "InvalidJump",
            Self::InvalidCall(_) => "InvalidCall",
            Self::ParseFailure { .. } => "ParseFailure",
            Self::IndexOutOfBounds { .. } => "IndexOutOfBounds",
            Self::MissingField { .. } => "MissingField",
            Self::InvalidCapture(_) => "InvalidCapture",
            Self::InvalidNativeCall => "InvalidNativeCall",
            Self::NativeFailure { .. } => "NativeFailure",
            Self::InvalidResource => "InvalidResource",
            Self::InvalidArgument { .. } => "InvalidArgument",
            Self::Cancelled => "Cancelled",
            Self::ResourceLimit { .. } => "ResourceLimit",
            Self::IoFailure { .. } => "IoFailure",
            Self::PermissionDenied { .. } => "PermissionDenied",
            Self::ProcessFailed { .. } => "ProcessFailed",
            Self::IoReadFailed => "IoReadFailed",
            Self::IoWriteFailed { .. } => "IoWriteFailed",
            Self::EntropyUnavailable => "EntropyUnavailable",
            Self::InvalidRangeStep => "InvalidRangeStep",
            Self::Unordered => "Unordered",
            Self::LanguageError { tag, .. } => tag.rsplit("::").next().unwrap_or(tag),
            Self::WithSuppressed { primary, .. } => primary.kind(),
            Self::WithProvenance { error, .. } => error.kind(),
            Self::Crashed { .. } => "Crashed",
        }
    }

    pub fn tag(&self) -> std::rc::Rc<str> {
        match self {
            Self::WithSuppressed { primary, .. } => primary.tag(),
            Self::WithProvenance { error, .. } => error.tag(),
            Self::LanguageError { tag, .. } => tag.clone(),
            _ => format!("{ERROR_TYPE}::{}", self.kind()).into(),
        }
    }

    pub fn field(&self, name: &str) -> Option<crate::value::Value> {
        use crate::value::Value;
        let text = |s: &str| Value::Str(std::rc::Rc::from(s));
        Some(match (self, name) {
            (Self::WithSuppressed { primary, .. }, name) => return primary.field(name),
            (Self::WithProvenance { error, .. }, name) => return error.field(name),
            (Self::TypeMismatch { op } | Self::PermissionDenied { op }, "op") => text(op),
            (Self::InvalidArgument { op } | Self::IoFailure { op, .. }, "op") => text(op),
            (Self::IoFailure { message, .. }, "message") => text(message),
            (Self::ResourceLimit { resource }, "resource") => text(resource),
            (Self::NegativeSleep(seconds), "seconds") => Value::Float(*seconds),
            (Self::ParseFailure { from, .. }, "from") => text(from),
            (Self::ParseFailure { to, .. }, "to") => text(to),
            (Self::IndexOutOfBounds { index, .. }, "index") => Value::Int(*index),
            (Self::IndexOutOfBounds { len, .. }, "len") => Value::Int(*len as i64),
            (Self::MissingField { field }, "field") => text(field),
            (Self::ProcessFailed { command, .. }, "command") => text(command),
            (Self::ProcessFailed { code, .. }, "code") => {
                code.map_or(Value::Nil, |n| Value::Int(n as i64))
            }
            (Self::IoWriteFailed { message }, "message") => text(message),
            (Self::NativeFailure { message }, "message") => text(message),
            (Self::LanguageError { fields, .. }, name) => {
                return fields.get(name);
            }
            _ => return None,
        })
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::WithSuppressed { primary, .. } => primary.fmt(f),
            RuntimeError::WithProvenance { error, .. } => error.fmt(f),
            RuntimeError::StackUnderflow => write!(f, "stack underflow"),
            RuntimeError::CallStackUnderflow => {
                write!(f, "call stack underflow (return without call)")
            }
            RuntimeError::TypeMismatch { op } => write!(f, "type mismatch in `{op}`"),
            RuntimeError::DivisionByZero => write!(f, "division by zero"),
            RuntimeError::Overflow => write!(f, "integer arithmetic overflow"),
            RuntimeError::InvalidLocal(i) => write!(f, "invalid local `{i}`"),
            RuntimeError::NegativeSleep(s) => write!(f, "negative sleep duration `{s}`"),
            RuntimeError::Timeout => write!(f, "async task timed out"),
            RuntimeError::MissingHalt => write!(f, "missing halt"),
            RuntimeError::InvalidJump(i) => write!(f, "invalid jump target `{i}`"),
            RuntimeError::InvalidCall(i) => write!(f, "invalid callable target or signature `{i}`"),
            RuntimeError::ParseFailure { from, to } => {
                write!(f, "cannot parse {from} as {to}")
            }
            RuntimeError::IndexOutOfBounds { index, len } => {
                write!(f, "index {index} out of bounds (length {len})")
            }
            RuntimeError::MissingField { field } => write!(f, "missing field `{field}`"),
            RuntimeError::InvalidCapture(i) => write!(f, "invalid capture `{i}`"),
            RuntimeError::InvalidNativeCall => write!(f, "invalid native call"),
            RuntimeError::NativeFailure { message } => {
                write!(f, "native extension failed: {message}")
            }
            RuntimeError::InvalidResource => write!(f, "closed, stale or foreign resource handle"),
            RuntimeError::InvalidArgument { op } => write!(f, "invalid argument in `{op}`"),
            RuntimeError::Cancelled => write!(f, "async task was cancelled"),
            RuntimeError::ResourceLimit { resource } => {
                write!(f, "runtime resource limit: {resource}")
            }
            RuntimeError::IoFailure { op, message } => write!(f, "{op}: {message}"),
            RuntimeError::PermissionDenied { op } => write!(f, "permission denied for `{op}`"),
            RuntimeError::ProcessFailed { command, code } => match code {
                Some(code) => write!(f, "process `{command}` failed with exit code {code}"),
                None => write!(f, "process `{command}` could not be started"),
            },
            RuntimeError::IoReadFailed => {
                write!(f, "failed to read from stdin (EOF or invalid input)")
            }
            RuntimeError::IoWriteFailed { message } => {
                write!(f, "failed to write output: {message}")
            }
            RuntimeError::EntropyUnavailable => {
                write!(f, "failed to read system entropy source")
            }
            RuntimeError::InvalidRangeStep => write!(f, "range step must be nonzero"),
            RuntimeError::Unordered => write!(f, "values are not ordered"),
            RuntimeError::LanguageError { tag, fields } => {
                let tag = tag.strip_prefix(ERROR_TYPE).map_or_else(
                    || tag.clone(),
                    |tail| std::rc::Rc::from(format!("Error{tail}")),
                );
                write!(f, "{}", crate::value::Value::Variant(tag, fields.clone()))
            }
            RuntimeError::Crashed { context, value } => match context {
                Some(ctx) => write!(f, "crash: {ctx}: {value}"),
                None => write!(f, "crash: {value}"),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub enum ParseError {
    UnknownInstruction { line: usize, text: String },
    MissingOperand { line: usize, instr: String },
    UnexpectedOperand { line: usize, instr: String },
    InvalidOperand { line: usize, token: String },
    ResourceLimit { resource: &'static str },
    UnknownLabel { line: usize, name: String },
    DuplicateLabel { line: usize, name: String },
    EmptyLabel { line: usize },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::UnknownInstruction { line, text } => {
                write!(f, "line {line}: unknown instruction `{text}`")
            }
            ParseError::MissingOperand { line, instr } => {
                write!(f, "line {line}: `{instr}` requires an operand")
            }
            ParseError::UnexpectedOperand { line, instr } => {
                write!(f, "line {line}: `{instr}` takes no operand")
            }
            ParseError::InvalidOperand { line, token } => {
                write!(f, "line {line}: invalid operand `{token}`")
            }
            ParseError::ResourceLimit { resource } => {
                write!(f, "{resource} limit exceeded")
            }
            ParseError::UnknownLabel { line, name } => {
                write!(f, "line {line}: unknown label `{name}`")
            }
            ParseError::DuplicateLabel { line, name } => {
                write!(f, "line {line}: label `{name}` defined more than once")
            }
            ParseError::EmptyLabel { line } => {
                write!(f, "line {line}: empty label name")
            }
        }
    }
}
