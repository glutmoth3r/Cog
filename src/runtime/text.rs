//! Checked conversions and bounded formatting, independent of VM instructions.
use super::RuntimeContext;
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::Value;
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

pub(super) fn to_int(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Int(_) => args[0].clone(),
        Value::BigInt(value) => {
            Value::Int(i64::try_from(value).map_err(|_| RuntimeError::Overflow)?)
        }
        Value::UInt(value) => {
            Value::Int(i64::try_from(*value).map_err(|_| RuntimeError::Overflow)?)
        }
        Value::Float(x) if x.is_finite() && *x >= i64::MIN as f64 && *x < -(i64::MIN as f64) => {
            Value::Int(*x as i64)
        }
        Value::Float(_) => return Err(RuntimeError::Overflow),
        Value::Str(text) => Value::Int(text.parse().map_err(|_| RuntimeError::ParseFailure {
            from: "str",
            to: "int",
        })?),
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_int" }),
    })
}

pub(super) fn to_uint(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::UInt(_) => args[0].clone(),
        Value::BigInt(value) => {
            Value::UInt(u64::try_from(value).map_err(|_| RuntimeError::Overflow)?)
        }
        Value::Int(value) => {
            Value::UInt(u64::try_from(*value).map_err(|_| RuntimeError::Overflow)?)
        }
        Value::Float(value)
            if value.is_finite() && *value >= 0.0 && *value < 18446744073709551616.0 =>
        {
            Value::UInt(*value as u64)
        }
        Value::Float(_) => return Err(RuntimeError::Overflow),
        Value::Str(text) => Value::UInt(text.parse().map_err(|_| RuntimeError::ParseFailure {
            from: "str",
            to: "uint",
        })?),
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_uint" }),
    })
}

pub(super) fn to_float(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Float(_) => args[0].clone(),
        Value::Int(x) => Value::Float(*x as f64),
        Value::UInt(x) => Value::Float(*x as f64),
        Value::BigInt(x) => {
            let value = x.to_f64().ok_or(RuntimeError::Overflow)?;
            if !value.is_finite() {
                return Err(RuntimeError::Overflow);
            }
            Value::Float(value)
        }
        Value::Str(text) => Value::Float(text.parse().map_err(|_| RuntimeError::ParseFailure {
            from: "str",
            to: "float",
        })?),
        _ => {
            return Err(RuntimeError::TypeMismatch {
                op: "str::to_float",
            });
        }
    })
}

pub(super) fn to_bigint(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = match &args[0] {
        Value::Error(_) | Value::BigInt(_) => args[0].clone(),
        Value::Int(value) => Value::BigInt(BigInt::from(*value)),
        Value::UInt(value) => Value::BigInt(BigInt::from(*value)),
        Value::Str(text) => {
            if let Some(limit) = context.limits.max_numeric_digits {
                let digits = text
                    .strip_prefix('-')
                    .or_else(|| text.strip_prefix('+'))
                    .unwrap_or(text)
                    .trim_start_matches('0')
                    .len()
                    .max(1);
                if digits > limit {
                    return Err(RuntimeError::ResourceLimit {
                        resource: "numeric digits",
                    });
                }
            }
            Value::BigInt(text.parse().map_err(|_| RuntimeError::ParseFailure {
                from: "str",
                to: "bigint",
            })?)
        }
        _ => {
            return Err(RuntimeError::TypeMismatch {
                op: "str::to_bigint",
            });
        }
    };
    if let (Value::BigInt(value), Some(limit)) = (&value, context.limits.max_numeric_digits)
        && crate::value::bigint_exceeds_decimal_digits(value, limit)
    {
        return Err(RuntimeError::ResourceLimit {
            resource: "numeric digits",
        });
    }
    Ok(value)
}

pub(super) fn to_bool(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Bool(_) => args[0].clone(),
        Value::Str(text) => match text.as_ref() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => {
                return Err(RuntimeError::ParseFailure {
                    from: "str",
                    to: "bool",
                });
            }
        },
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_bool" }),
    })
}

pub(super) fn to_char(_: &mut RuntimeContext<'_>, args: &[Value]) -> Result<Value, RuntimeError> {
    Ok(match &args[0] {
        Value::Error(_) | Value::Char(_) => args[0].clone(),
        Value::Str(text) => {
            let mut chars = text.chars();
            let Some(character) = chars.next() else {
                return Err(RuntimeError::ParseFailure {
                    from: "str",
                    to: "char",
                });
            };
            if chars.next().is_some() {
                return Err(RuntimeError::ParseFailure {
                    from: "str",
                    to: "char",
                });
            }
            Value::Char(character)
        }
        _ => return Err(RuntimeError::TypeMismatch { op: "str::to_char" }),
    })
}

fn limit(resource: &'static str) -> RuntimeError {
    RuntimeError::ResourceLimit { resource }
}

pub(super) fn to_str(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if let Value::Error(_) = &args[0] {
        return Ok(args[0].clone());
    }
    if let Value::Str(text) = &args[0] {
        if context
            .limits
            .max_buffer_bytes
            .is_some_and(|limit| text.len() > limit)
        {
            return Err(limit("str"));
        }
        return Ok(args[0].clone());
    }
    Ok(Value::Str(
        render(&args[0], context.limits.buffer_limit(), "str")?.into(),
    ))
}

pub(super) fn builder(_: &mut RuntimeContext<'_>, _: &[Value]) -> Result<Value, RuntimeError> {
    Ok(Value::StrBuf(HeapRef::try_new(
        RefCell::new(String::new()),
    )?))
}

pub(super) fn build(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::StrBuf(buffer) = &args[0] else {
        unreachable!("checked native signature")
    };
    let text = buffer
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op: "str::build" })?;
    if context
        .limits
        .max_buffer_bytes
        .is_some_and(|limit| text.len() > limit)
    {
        return Err(limit("str"));
    }
    // Snapshot: later writes through any builder alias cannot change this str.
    Ok(Value::Str(Rc::from(text.as_str())))
}

pub(super) fn write(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::StrBuf(buffer) = &args[0] else {
        unreachable!("checked native signature")
    };
    let Value::Str(template) = &args[1] else {
        unreachable!("checked native signature")
    };
    if let Value::Error(_) = &args[2] {
        return Ok(args[2].clone());
    }
    let length = buffer
        .try_borrow()
        .map_err(|_| RuntimeError::InvalidArgument { op: "fmt::write" })?
        .len();
    let remaining = context
        .limits
        .buffer_limit()
        .checked_sub(length)
        .ok_or_else(|| limit("str"))?;
    let Some(position) = template.find("{}") else {
        if template.len() > remaining {
            return Err(limit("str"));
        }
        let mut text = buffer
            .try_borrow_mut()
            .map_err(|_| RuntimeError::InvalidArgument { op: "fmt::write" })?;
        text.try_reserve(template.len()).map_err(|_| limit("str"))?;
        text.push_str(template);
        return Ok(args[0].clone());
    };
    let prefix = &template[..position];
    let suffix = &template[position + 2..];
    let literal_length = template.len() - 2;
    let budget = remaining
        .checked_sub(literal_length)
        .ok_or_else(|| limit("str"))?;
    // Simple scalars cannot alias the target builder, so render them into a
    // fixed stack buffer and append only after all size checks succeed.
    let mut inline = InlineText::default();
    let rendered = match render_small_scalar(&args[2], &mut inline) {
        Some(text) => {
            if text.len() > budget {
                return Err(limit("str"));
            }
            Cow::Borrowed(text)
        }
        None => Cow::Owned(render(&args[2], budget, "str")?),
    };
    let mut text = buffer
        .try_borrow_mut()
        .map_err(|_| RuntimeError::InvalidArgument { op: "fmt::write" })?;
    text.try_reserve(prefix.len() + rendered.len() + suffix.len())
        .map_err(|_| limit("str"))?;
    text.push_str(prefix);
    text.push_str(&rendered);
    text.push_str(suffix);
    Ok(args[0].clone())
}

#[derive(Default)]
struct InlineText {
    bytes: [u8; 32],
    length: usize,
}

impl InlineText {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).expect("formatting writes valid UTF-8")
    }
}

impl Write for InlineText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if text.len() > self.bytes.len().saturating_sub(self.length) {
            return Err(std::fmt::Error);
        }
        let end = self.length + text.len();
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

fn render_small_scalar<'a>(value: &'a Value, inline: &'a mut InlineText) -> Option<&'a str> {
    match value {
        Value::Int(_) | Value::UInt(_) | Value::Bool(_) | Value::Char(_) => {
            write!(inline, "{value}").ok()?;
            Some(inline.as_str())
        }
        Value::Nil => Some("nil"),
        Value::Str(text) => Some(text),
        _ => None,
    }
}

/// One bounded renderer serves string conversion, builders and byte rendering.
/// Traversal depth is bounded independently of output size, including cycles.
pub(crate) fn render(
    value: &Value,
    limit: usize,
    resource: &'static str,
) -> Result<String, RuntimeError> {
    let mut output = RenderBuffer {
        text: String::new(),
        limit,
        failure: None,
    };
    if output.value(value, 0).is_err() {
        return Err(output.failure.unwrap_or_else(|| self::limit(resource)));
    }
    Ok(output.text)
}

struct RenderBuffer {
    text: String,
    limit: usize,
    failure: Option<RuntimeError>,
}

impl Write for RenderBuffer {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if text.len() > self.limit.saturating_sub(self.text.len()) {
            return Err(std::fmt::Error);
        }
        self.text
            .try_reserve(text.len())
            .map_err(|_| std::fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

impl RenderBuffer {
    fn borrow_error(&mut self) -> std::fmt::Error {
        self.failure = Some(RuntimeError::InvalidArgument { op: "render" });
        std::fmt::Error
    }

    fn fields(
        &mut self,
        fields: &std::collections::HashMap<String, Value>,
        depth: usize,
    ) -> std::fmt::Result {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(fields.len())
            .map_err(|_| std::fmt::Error)?;
        entries.extend(fields);
        entries.sort_unstable_by_key(|(name, _)| *name);
        for (index, (name, value)) in entries.into_iter().enumerate() {
            if index > 0 {
                self.write_str(", ")?;
            }
            write!(self, "{name}: ")?;
            self.value(value, depth + 1)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> std::fmt::Result {
        if depth >= 64 {
            self.failure = Some(limit("render depth"));
            return Err(std::fmt::Error);
        }
        match value {
            Value::Array(values) => {
                let values = values.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.write_str(", ")?;
                    }
                    self.value(value, depth + 1)?;
                }
                self.write_str("]")
            }
            Value::Record(fields) => {
                let fields = fields
                    .try_borrow_fields()
                    .map_err(|_| self.borrow_error())?;
                self.write_str("{")?;
                self.fields(&fields, depth)?;
                self.write_str("}")
            }
            Value::Variant(tag, fields) => {
                write!(self, "{tag}(")?;
                let fields = fields.try_borrow().map_err(|_| self.borrow_error())?;
                self.fields(&fields, depth)?;
                self.write_str(")")
            }
            Value::Cell(cell) => {
                let value = cell.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str("cell(")?;
                self.value(&value, depth + 1)?;
                self.write_str(")")
            }
            Value::StrBuf(buffer) => {
                let text = buffer.try_borrow().map_err(|_| self.borrow_error())?;
                self.write_str(&text)
            }
            Value::Bytes(buffer) => {
                let bytes = buffer.try_borrow().map_err(|_| self.borrow_error())?;
                write!(self, "<bytes:{}>", bytes.len())
            }
            Value::Error(error) => {
                self.write_str("Error: ")?;
                match error.primary() {
                    RuntimeError::LanguageError { tag, fields } => {
                        if let Some(tail) = tag.strip_prefix(crate::error::ERROR_TYPE) {
                            write!(self, "Error{tail}(")?;
                        } else {
                            write!(self, "{tag}(")?;
                        }
                        let fields = fields.try_borrow().map_err(|_| self.borrow_error())?;
                        self.fields(&fields, depth)?;
                        self.write_str(")")
                    }
                    RuntimeError::Crashed { context, value } => {
                        self.write_str("crash: ")?;
                        if let Some(context) = context {
                            write!(self, "{context}: ")?;
                        }
                        self.value(value, depth + 1)
                    }
                    _ => write!(self, "{error}"),
                }
            }
            // These variants cannot recursively visit a user-controlled graph.
            _ => write!(self, "{value}"),
        }
    }
}
