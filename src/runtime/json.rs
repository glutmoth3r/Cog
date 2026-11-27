//! Bounded JSON conversion between JSON values and Cog's dynamic runtime values.
use super::RuntimeContext;
use crate::error::RuntimeError;
use crate::heap::HeapRef;
use crate::value::{ArrayValue, RecordValue, Value};
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::ser::{Error as _, SerializeMap, SerializeSeq};
use serde::{Deserializer, Serialize, Serializer};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

const MAX_DEPTH: usize = 64;
const RESOURCE: &str = "$cog:resource";
const HEAP_RESOURCE: &str = "$cog:heap-resource";
const HEAP_FAILURE: &str = "$cog:heap-failure";
const DEPTH: &str = "$cog:depth";
const OVERFLOW: &str = "$cog:overflow";
const DUPLICATE: &str = "$cog:duplicate";
const CYCLE: &str = "$cog:cycle";
const BORROW: &str = "$cog:borrow";
const TYPE: &str = "$cog:type";
const INVALID: &str = "$cog:invalid";
const NUMERIC: &str = "$cog:numeric";

pub(super) fn parse(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let Value::Str(text) = &args[0] else {
        return Err(RuntimeError::InvalidNativeCall);
    };
    if text.len() > context.limits.buffer_limit() {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    let (prepared, number_marker) = prepare_json_numbers(text, context.limits.buffer_limit())?;

    let mut budget = Budget {
        used: 0,
        limit: context.limits.buffer_limit(),
        heap_limit: context.limits.max_heap_objects,
        numeric_limit: context.limits.max_numeric_digits,
        number_marker,
        heap_collection_attempted: false,
        reserved_heap_objects: 0,
    };
    let mut deserializer = serde_json::Deserializer::from_str(&prepared);
    let value = ValueSeed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(parse_error)?;
    deserializer.end().map_err(parse_error)?;
    Ok(value)
}

pub(super) fn stringify(
    context: &mut RuntimeContext<'_>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if let Value::Error(_) = args[0] {
        return Ok(args[0].clone());
    }
    let mut writer = BoundedWriter::new(context.limits.buffer_limit());
    let state = Rc::new(SerializeState {
        depth: Cell::new(0),
        active: RefCell::new(HashSet::new()),
        error: RefCell::new(None),
        allocation_limit: context.limits.buffer_limit(),
        numeric_limit: context.limits.max_numeric_digits,
    });
    let result = {
        let mut serializer = serde_json::Serializer::new(&mut writer);
        JsonValue {
            value: &args[0],
            state: state.clone(),
        }
        .serialize(&mut serializer)
    };
    if result.is_err() {
        if let Some(error) = state.error.borrow_mut().take() {
            return Ok(Value::Error(Box::new(error)));
        }
        return Err(writer.error().unwrap_or_else(|| {
            result.err().map_or(
                RuntimeError::InvalidArgument {
                    op: "json::stringify",
                },
                |error| serialize_error(&error.to_string()),
            )
        }));
    }
    let text =
        String::from_utf8(writer.into_inner()).map_err(|_| RuntimeError::InvalidNativeCall)?;
    Ok(Value::Str(Rc::from(text)))
}

struct Budget {
    used: usize,
    limit: usize,
    heap_limit: Option<usize>,
    numeric_limit: Option<usize>,
    number_marker: String,
    heap_collection_attempted: bool,
    reserved_heap_objects: usize,
}

impl Budget {
    fn numeric_digits<E: serde::de::Error>(&self, digits: usize) -> Result<(), E> {
        if self.numeric_limit.is_some_and(|limit| digits > limit) {
            return Err(E::custom(NUMERIC));
        }
        Ok(())
    }

    fn charge<E: serde::de::Error>(&mut self, amount: usize) -> Result<(), E> {
        self.used = self
            .used
            .checked_add(amount)
            .filter(|used| *used <= self.limit)
            .ok_or_else(|| E::custom(RESOURCE))?;
        Ok(())
    }

    fn node<E: serde::de::Error>(&mut self) -> Result<(), E> {
        // Account for the Cog Value plus conservative per-entry/container
        // bookkeeping. The input byte limit alone would allow tiny JSON tokens
        // to expand into a much larger object graph.
        self.charge(2 * std::mem::size_of::<Value>() + 2 * std::mem::size_of::<usize>())
    }

    fn reserve_heap_object<E: serde::de::Error>(&mut self) -> Result<(), E> {
        if self.heap_limit.is_some_and(|limit| {
            !self.heap_collection_attempted
                && crate::heap::live_objects().saturating_add(self.reserved_heap_objects) >= limit
        }) {
            crate::heap::collect(&[]).map_err(|_| E::custom(HEAP_FAILURE))?;
            self.heap_collection_attempted = true;
        }
        if self.heap_limit.is_some_and(|limit| {
            crate::heap::live_objects().saturating_add(self.reserved_heap_objects) >= limit
        }) {
            return Err(E::custom(HEAP_RESOURCE));
        }
        self.reserved_heap_objects = self
            .reserved_heap_objects
            .checked_add(1)
            .ok_or_else(|| E::custom(HEAP_RESOURCE))?;
        Ok(())
    }

    fn release_heap_object(&mut self) {
        self.reserved_heap_objects = self.reserved_heap_objects.saturating_sub(1);
    }
}

struct ValueSeed<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_DEPTH {
            return Err(serde::de::Error::custom(DEPTH));
        }
        deserializer.deserialize_any(ValueVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

struct ValueVisitor<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl ValueVisitor<'_> {
    fn charge_node<E: serde::de::Error>(&mut self) -> Result<(), E> {
        self.budget.node()
    }
}

impl<'de> Visitor<'de> for ValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Nil)
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        let mut this = self;
        let magnitude = value.unsigned_abs();
        this.budget.numeric_digits(if magnitude == 0 {
            1
        } else {
            magnitude.ilog10() as usize + 1
        })?;
        this.charge_node()?;
        Ok(Value::Int(value))
    }

    fn visit_i128<E: serde::de::Error>(self, value: i128) -> Result<Value, E> {
        let mut this = self;
        let magnitude = value.unsigned_abs();
        this.budget.numeric_digits(if magnitude == 0 {
            1
        } else {
            magnitude.ilog10() as usize + 1
        })?;
        this.charge_node()?;
        if let Ok(value) = i64::try_from(value) {
            Ok(Value::Int(value))
        } else {
            Ok(Value::BigInt(BigInt::from(value)))
        }
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        let mut this = self;
        this.budget.numeric_digits(if value == 0 {
            1
        } else {
            value.ilog10() as usize + 1
        })?;
        this.charge_node()?;
        if value <= i64::MAX as u64 {
            Ok(Value::Int(value as i64))
        } else {
            Ok(Value::UInt(value))
        }
    }

    fn visit_u128<E: serde::de::Error>(self, value: u128) -> Result<Value, E> {
        let mut this = self;
        this.budget.numeric_digits(if value == 0 {
            1
        } else {
            value.ilog10() as usize + 1
        })?;
        this.charge_node()?;
        if let Ok(value) = u64::try_from(value) {
            if value <= i64::MAX as u64 {
                Ok(Value::Int(value as i64))
            } else {
                Ok(Value::UInt(value))
            }
        } else {
            Ok(Value::BigInt(BigInt::from(value)))
        }
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        if !value.is_finite() {
            return Err(E::custom(OVERFLOW));
        }
        let mut this = self;
        this.charge_node()?;
        Ok(Value::Float(value))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        if let Some(raw) = value.strip_prefix(&this.budget.number_marker) {
            return parse_marked_number(raw, this.budget);
        }
        this.budget.charge(value.len())?;
        Ok(Value::Str(Rc::from(value)))
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        let mut this = self;
        this.charge_node()?;
        if let Some(raw) = value.strip_prefix(&this.budget.number_marker) {
            return parse_marked_number(raw, this.budget);
        }
        this.budget.charge(value.len())?;
        Ok(Value::Str(Rc::from(value)))
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut sequence: A) -> Result<Value, A::Error> {
        self.charge_node()?;
        self.budget.reserve_heap_object::<A::Error>()?;
        let result = (|| {
            self.budget.charge(std::mem::size_of::<ArrayValue>())?;
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element_seed(ValueSeed {
                budget: &mut *self.budget,
                depth: self.depth + 1,
            })? {
                values
                    .try_reserve(1)
                    .map_err(|_| A::Error::custom(RESOURCE))?;
                values.push(value);
            }
            let array =
                HeapRef::try_new(ArrayValue::dynamic(values)).map_err(|error| match error {
                    RuntimeError::ResourceLimit { .. } => A::Error::custom(RESOURCE),
                    RuntimeError::InvalidArgument { .. } => A::Error::custom(BORROW),
                    _ => A::Error::custom(INVALID),
                })?;
            Ok(Value::Array(array))
        })();
        self.budget.release_heap_object();
        result
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut object: A) -> Result<Value, A::Error> {
        self.charge_node()?;
        let mut first_key = object.next_key::<String>()?;
        self.budget.reserve_heap_object::<A::Error>()?;
        let result = (|| {
            self.budget.charge(std::mem::size_of::<RecordValue>())?;
            let mut fields = HashMap::new();
            let mut next_key = first_key.take();
            while let Some(key) = next_key {
                self.budget
                    .charge(key.len().saturating_add(4 * std::mem::size_of::<usize>()))?;
                if fields.contains_key(&key) {
                    return Err(A::Error::custom(DUPLICATE));
                }
                fields
                    .try_reserve(1)
                    .map_err(|_| A::Error::custom(RESOURCE))?;
                let value = object.next_value_seed(ValueSeed {
                    budget: &mut *self.budget,
                    depth: self.depth + 1,
                })?;
                fields.insert(key, value);
                next_key = object.next_key::<String>()?;
            }
            let record =
                HeapRef::try_new(RecordValue::new(fields)).map_err(|error| match error {
                    RuntimeError::ResourceLimit { .. } => A::Error::custom(RESOURCE),
                    RuntimeError::InvalidArgument { .. } => A::Error::custom(BORROW),
                    _ => A::Error::custom(INVALID),
                })?;
            Ok(Value::Record(record))
        })();
        self.budget.release_heap_object();
        result
    }
}

fn parse_marked_number<E: serde::de::Error>(raw: &str, budget: &mut Budget) -> Result<Value, E> {
    let digits = raw
        .strip_prefix('-')
        .unwrap_or(raw)
        .trim_start_matches('0')
        .len()
        .max(1);
    budget.numeric_digits(digits)?;
    budget.charge(raw.len())?;
    parse_json_number(raw)
}

fn prepare_json_numbers(text: &str, limit: usize) -> Result<(String, String), RuntimeError> {
    let marker = unique_json_number_marker(text);
    let mut output_len = text.len();
    let mut index = 0usize;
    while index < text.len() {
        let byte = text.as_bytes()[index];
        if byte == b'"' {
            index = json_string_end(text, index);
        } else if byte == b'-' || byte.is_ascii_digit() {
            if let Some(end) = json_number_end(text.as_bytes(), index) {
                let raw = &text[index..end];
                if marked_json_integer(raw) {
                    let replacement_len = marker.len().saturating_add(2);
                    output_len = if replacement_len >= raw.len() {
                        output_len.checked_add(replacement_len - raw.len())
                    } else {
                        output_len.checked_sub(raw.len() - replacement_len)
                    }
                    .ok_or(RuntimeError::ResourceLimit { resource: "bytes" })?;
                }
                index = end;
            } else {
                index += text[index..].chars().next().unwrap().len_utf8();
            }
        } else {
            index += text[index..].chars().next().unwrap().len_utf8();
        }
    }
    if output_len > limit {
        return Err(RuntimeError::ResourceLimit { resource: "bytes" });
    }
    let mut prepared = String::new();
    prepared
        .try_reserve_exact(output_len)
        .map_err(|_| RuntimeError::ResourceLimit { resource: "bytes" })?;

    index = 0;
    while index < text.len() {
        let byte = text.as_bytes()[index];
        if byte == b'"' {
            let end = json_string_end(text, index);
            prepared.push_str(&text[index..end]);
            index = end;
        } else if byte == b'-' || byte.is_ascii_digit() {
            if let Some(end) = json_number_end(text.as_bytes(), index) {
                let raw = &text[index..end];
                if marked_json_integer(raw) {
                    prepared.push('"');
                    prepared.push_str(&marker);
                    prepared.push_str(raw);
                    prepared.push('"');
                } else {
                    prepared.push_str(raw);
                }
                index = end;
            } else {
                let next = index + text[index..].chars().next().unwrap().len_utf8();
                prepared.push_str(&text[index..next]);
                index = next;
            }
        } else {
            let next = index + text[index..].chars().next().unwrap().len_utf8();
            prepared.push_str(&text[index..next]);
            index = next;
        }
    }
    Ok((prepared, marker))
}

fn unique_json_number_marker(text: &str) -> String {
    let seed = (text.as_ptr() as usize) ^ text.len();
    let mut attempt = 0usize;
    loop {
        let marker = format!("__COG_BIGINT_{seed:x}_{attempt:x}__");
        if !json_strings_contain(text, &marker) {
            return marker;
        }
        attempt = attempt.wrapping_add(1);
    }
}

fn json_strings_contain(text: &str, needle: &str) -> bool {
    let mut index = 0usize;
    while index < text.len() {
        if text.as_bytes()[index] != b'"' {
            index += text[index..].chars().next().unwrap().len_utf8();
            continue;
        }
        let end = json_string_end(text, index);
        if let Ok(value) = serde_json::from_str::<String>(&text[index..end])
            && value.contains(needle)
        {
            return true;
        }
        index = end;
    }
    false
}

fn json_string_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn json_number_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index).copied()? {
        b'0' => index += 1,
        b'1'..=b'9' => {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return None,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return None;
        }
    }
    Some(index)
}

fn marked_json_integer(raw: &str) -> bool {
    if raw == "-0" {
        return true;
    }
    if raw.contains(['.', 'e', 'E']) {
        return false;
    }
    let negative = raw.starts_with('-');
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let bound = if negative {
        "9223372036854775808"
    } else {
        "18446744073709551615"
    };
    digits.len() > bound.len() || (digits.len() == bound.len() && digits > bound)
}

fn parse_json_number<E: serde::de::Error>(raw: &str) -> Result<Value, E> {
    if raw == "-0" {
        return Ok(Value::Float(-0.0));
    }
    if raw.contains(['.', 'e', 'E']) {
        let value = raw.parse::<f64>().map_err(|_| E::custom(OVERFLOW))?;
        return value
            .is_finite()
            .then_some(Value::Float(value))
            .ok_or_else(|| E::custom(OVERFLOW));
    }
    let number = raw.parse::<BigInt>().map_err(|_| E::custom(OVERFLOW))?;
    if let Some(value) = number.to_i64() {
        Ok(Value::Int(value))
    } else if let Some(value) = number.to_u64() {
        Ok(Value::UInt(value))
    } else {
        Ok(Value::BigInt(number))
    }
}

fn parse_error(error: serde_json::Error) -> RuntimeError {
    let message = error.to_string();
    if message.contains(HEAP_FAILURE) {
        RuntimeError::ResourceLimit {
            resource: "VM heap",
        }
    } else if message.contains(HEAP_RESOURCE) {
        RuntimeError::ResourceLimit {
            resource: "VM heap objects",
        }
    } else if message.contains(RESOURCE) {
        RuntimeError::ResourceLimit { resource: "bytes" }
    } else if message.contains(NUMERIC) {
        RuntimeError::ResourceLimit {
            resource: "numeric digits",
        }
    } else if message.contains(DEPTH) {
        RuntimeError::ResourceLimit {
            resource: "JSON nesting",
        }
    } else if message.contains(OVERFLOW) || message.contains("number out of range") {
        RuntimeError::Overflow
    } else if message.contains(BORROW) {
        RuntimeError::InvalidArgument {
            op: "json::parse allocation",
        }
    } else {
        // Duplicate names are rejected to avoid ambiguous object semantics.
        RuntimeError::ParseFailure {
            from: "JSON",
            to: "Cog value",
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    failure: Option<RuntimeError>,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            failure: None,
        }
    }

    fn error(&self) -> Option<RuntimeError> {
        self.failure.clone()
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(bytes.len()) else {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::new(io::ErrorKind::WriteZero, RESOURCE));
        };
        if length > self.limit {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::new(io::ErrorKind::WriteZero, RESOURCE));
        }
        if self.bytes.try_reserve(bytes.len()).is_err() {
            self.failure = Some(RuntimeError::ResourceLimit { resource: "bytes" });
            return Err(io::Error::other(RESOURCE));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SerializeState {
    depth: Cell<usize>,
    active: RefCell<HashSet<(u8, usize)>>,
    error: RefCell<Option<RuntimeError>>,
    allocation_limit: usize,
    numeric_limit: Option<usize>,
}

struct ValueGuard {
    state: Rc<SerializeState>,
    identity: Option<(u8, usize)>,
}

impl Drop for ValueGuard {
    fn drop(&mut self) {
        self.state
            .depth
            .set(self.state.depth.get().saturating_sub(1));
        if let Some(identity) = self.identity
            && let Ok(mut active) = self.state.active.try_borrow_mut()
        {
            active.remove(&identity);
        }
    }
}

fn enter_value<E: serde::ser::Error>(
    state: &Rc<SerializeState>,
    identity: Option<(u8, usize)>,
) -> Result<ValueGuard, E> {
    let depth = state.depth.get();
    // Match the parser's boundary: a value at depth MAX_DEPTH is allowed,
    // while its child at MAX_DEPTH + 1 is rejected.
    if depth > MAX_DEPTH {
        return Err(E::custom(DEPTH));
    }
    if let Some(identity) = identity {
        let mut active = state
            .active
            .try_borrow_mut()
            .map_err(|_| E::custom(BORROW))?;
        if active.contains(&identity) {
            return Err(E::custom(CYCLE));
        }
        active.try_reserve(1).map_err(|_| E::custom(RESOURCE))?;
        active.insert(identity);
    }
    state.depth.set(depth + 1);
    Ok(ValueGuard {
        state: state.clone(),
        identity,
    })
}

struct JsonValue<'a> {
    value: &'a Value,
    state: Rc<SerializeState>,
}

impl Serialize for JsonValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let identity = match self.value {
            Value::Array(array) => Some((0, array.identity())),
            Value::Record(record) => Some((1, record.identity())),
            _ => None,
        };
        let _guard = enter_value::<S::Error>(&self.state, identity)?;
        match self.value {
            Value::Nil => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Int(value) => serializer.serialize_i64(*value),
            Value::UInt(value) => serializer.serialize_u64(*value),
            Value::BigInt(value) => {
                if self
                    .state
                    .numeric_limit
                    .is_some_and(|limit| crate::value::bigint_exceeds_decimal_digits(value, limit))
                {
                    *self.state.error.borrow_mut() = Some(RuntimeError::ResourceLimit {
                        resource: "numeric digits",
                    });
                    return Err(S::Error::custom(NUMERIC));
                }
                let digits = value.bits().saturating_mul(30_103).saturating_add(99_999) / 100_000;
                let bytes = usize::try_from(digits)
                    .unwrap_or(usize::MAX)
                    .max(1)
                    .saturating_add(usize::from(value.sign() == num_bigint::Sign::Minus));
                if bytes > self.state.allocation_limit {
                    *self.state.error.borrow_mut() =
                        Some(RuntimeError::ResourceLimit { resource: "bytes" });
                    return Err(S::Error::custom(RESOURCE));
                }
                let text = value.to_str_radix(10);
                // SAFETY: base-10 BigInt formatting emits only an optional minus
                // sign and decimal digits, with no leading plus or invalid zeros,
                // so the string is a valid JSON integer token.
                let number = unsafe { serde_json::value::RawValue::from_string_unchecked(text) };
                number.serialize(serializer)
            }
            Value::Float(value) if value.is_finite() => serializer.serialize_f64(*value),
            Value::Float(_) => Err(S::Error::custom(INVALID)),
            Value::Char(value) => serializer.serialize_char(*value),
            Value::Str(value) => serializer.serialize_str(value),
            Value::Array(array) => {
                let values = array.try_borrow().map_err(|_| S::Error::custom(BORROW))?;
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values.iter() {
                    sequence.serialize_element(&JsonValue {
                        value,
                        state: self.state.clone(),
                    })?;
                }
                sequence.end()
            }
            Value::Record(record) => {
                let fields = record
                    .try_borrow_fields()
                    .map_err(|_| S::Error::custom(BORROW))?;
                if let Some(arguments) = record
                    .nominal_name()
                    .and_then(|name| name.strip_prefix("$core_collections_Map["))
                {
                    if !arguments.starts_with("str,") {
                        return Err(S::Error::custom(TYPE));
                    }
                    let Some(Value::Array(slots)) = fields.get("slots") else {
                        return Err(S::Error::custom(TYPE));
                    };
                    let slots = slots.try_borrow().map_err(|_| S::Error::custom(BORROW))?;
                    let mut object = serializer.serialize_map(None)?;
                    for slot in slots.iter() {
                        let Value::Record(slot) = slot else {
                            return Err(S::Error::custom(TYPE));
                        };
                        let slot = slot
                            .try_borrow_fields()
                            .map_err(|_| S::Error::custom(BORROW))?;
                        match slot.get("live") {
                            Some(Value::Bool(false)) => continue,
                            Some(Value::Bool(true)) => {}
                            _ => return Err(S::Error::custom(TYPE)),
                        }
                        let Some(Value::Str(key)) = slot.get("key") else {
                            return Err(S::Error::custom(TYPE));
                        };
                        let value = slot.get("value").ok_or_else(|| S::Error::custom(TYPE))?;
                        object.serialize_entry(
                            key.as_ref(),
                            &JsonValue {
                                value,
                                state: self.state.clone(),
                            },
                        )?;
                    }
                    return object.end();
                }
                let key_bytes = fields
                    .len()
                    .checked_mul(std::mem::size_of::<&str>())
                    .filter(|bytes| *bytes <= self.state.allocation_limit)
                    .ok_or_else(|| S::Error::custom(RESOURCE))?;
                let mut keys = Vec::new();
                keys.try_reserve_exact(key_bytes / std::mem::size_of::<&str>())
                    .map_err(|_| S::Error::custom(RESOURCE))?;
                keys.extend(fields.keys().map(String::as_str));
                keys.sort_unstable();
                let mut object = serializer.serialize_map(Some(keys.len()))?;
                for key in keys {
                    let value = fields.get(key).ok_or_else(|| S::Error::custom(INVALID))?;
                    object.serialize_entry(
                        key,
                        &JsonValue {
                            value,
                            state: self.state.clone(),
                        },
                    )?;
                }
                object.end()
            }
            Value::Error(error) => {
                let mut first_error = self
                    .state
                    .error
                    .try_borrow_mut()
                    .map_err(|_| S::Error::custom(BORROW))?;
                if first_error.is_none() {
                    *first_error = Some((**error).clone());
                }
                Err(S::Error::custom(TYPE))
            }
            _ => Err(S::Error::custom(TYPE)),
        }
    }
}

fn serialize_error(message: &str) -> RuntimeError {
    if message.contains(RESOURCE) {
        RuntimeError::ResourceLimit { resource: "bytes" }
    } else if message.contains(DEPTH) {
        RuntimeError::ResourceLimit {
            resource: "JSON nesting",
        }
    } else if message.contains(CYCLE) {
        RuntimeError::InvalidArgument {
            op: "json::stringify cycle",
        }
    } else if message.contains(BORROW) {
        RuntimeError::InvalidArgument {
            op: "json::stringify borrow",
        }
    } else if message.contains(TYPE) {
        RuntimeError::TypeMismatch {
            op: "json::stringify",
        }
    } else {
        RuntimeError::InvalidArgument {
            op: "json::stringify",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::VmCapabilities;

    #[test]
    fn parser_rejects_ambiguous_and_over_budget_inputs() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from(r#"{"a":1,"a":2}"#))]),
            Err(RuntimeError::ParseFailure { .. })
        ));

        context.limits.max_buffer_bytes = Some(8);
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from("1"))]),
            Err(RuntimeError::ResourceLimit { .. })
        ));

        context.limits.max_buffer_bytes = Some(1024);
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from("9223372036854775808"))]),
            Ok(Value::UInt(9_223_372_036_854_775_808))
        ));
    }

    #[test]
    fn parser_respects_the_live_heap_object_limit_and_cleans_up_partial_values() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let baseline = crate::heap::live_objects();

        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from("[{},]"))]),
            Err(RuntimeError::ParseFailure { .. })
        ));
        assert_eq!(crate::heap::live_objects(), baseline);

        context.limits.max_heap_objects = Some(baseline);
        let error = parse(&mut context, &[Value::Str(Rc::from("[]"))]).unwrap_err();
        assert!(error.is_recoverable());
        assert!(matches!(
            error,
            RuntimeError::ResourceLimit {
                resource: "VM heap objects"
            }
        ));
        assert_eq!(crate::heap::live_objects(), baseline);

        // The enclosing array reserves its object slot before decoding the
        // child, so the nested array must be rejected before either is made.
        context.limits.max_heap_objects = Some(baseline + 1);
        let error = parse(&mut context, &[Value::Str(Rc::from("[[]]"))]).unwrap_err();
        assert!(error.is_recoverable());
        assert!(matches!(
            error,
            RuntimeError::ResourceLimit {
                resource: "VM heap objects"
            }
        ));
        assert_eq!(crate::heap::live_objects(), baseline);

        let error = parse(&mut context, &[Value::Str(Rc::from(r#"{"a":[]}"#))]).unwrap_err();
        assert!(error.is_recoverable());
        assert!(matches!(
            error,
            RuntimeError::ResourceLimit {
                resource: "VM heap objects"
            }
        ));
        assert_eq!(crate::heap::live_objects(), baseline);

        context.limits.max_heap_objects = Some(baseline + 2);
        let value = parse(&mut context, &[Value::Str(Rc::from("[[]]"))]).unwrap();
        assert_eq!(crate::heap::live_objects(), baseline + 2);
        drop(value);
        assert_eq!(crate::heap::live_objects(), baseline);
    }

    #[test]
    fn parser_collects_unreachable_cycles_before_using_the_heap_quota() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let cycle = HeapRef::try_new(RecordValue::new(HashMap::new())).unwrap();
        cycle
            .try_set_field("self", Value::Record(cycle.clone()))
            .unwrap();
        drop(cycle);
        assert_eq!(crate::heap::live_objects(), 1);

        context.limits.max_heap_objects = Some(1);
        let value = parse(&mut context, &[Value::Str(Rc::from("[]"))]).unwrap();
        assert_eq!(crate::heap::live_objects(), 1);
        drop(value);
        assert_eq!(crate::heap::live_objects(), 0);
    }

    #[test]
    fn heap_quota_error_from_parse_stays_catchable_in_the_vm() {
        let mut program = crate::backend::CompiledProgram::new(vec![
            crate::instruction::Instruction::Push(Value::Str(Rc::from("[[]]"))),
            crate::instruction::Instruction::CallNative(0),
            crate::instruction::Instruction::IsType(crate::instruction::ValueType::Error),
            crate::instruction::Instruction::JumpIfFalse(5),
            crate::instruction::Instruction::Halt,
            crate::instruction::Instruction::Push(Value::Str(Rc::from("not an Error"))),
            crate::instruction::Instruction::Crash(None),
        ]);
        program
            .imports
            .push(crate::runtime::NativeImport::named("json::parse").unwrap());

        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        context.limits.max_heap_objects = Some(1);
        crate::interpreter::run_with_context(&program, &mut context).unwrap();
        context.finish().unwrap();
    }

    #[test]
    fn parser_limits_nesting_and_builds_heterogeneous_mutable_arrays() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        assert!(matches!(
            parse(&mut context, &[Value::Str(Rc::from(nested))]),
            Err(RuntimeError::ResourceLimit {
                resource: "JSON nesting"
            })
        ));

        let Value::Array(array) =
            parse(&mut context, &[Value::Str(Rc::from(r#"[1,"two",null]"#))]).unwrap()
        else {
            panic!("JSON array did not produce a Cog array");
        };
        array.try_push(Value::Bool(false), false).unwrap();
        array.set(0, Value::Str(Rc::from("one"))).unwrap();
        let result = stringify(&mut context, &[Value::Array(array)]).unwrap();
        assert!(
            matches!(result, Value::Str(text) if text.as_ref() == r#"["one","two",null,false]"#)
        );
    }

    #[test]
    fn parser_maximum_nesting_round_trips_through_stringifier() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        let nested = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        let value = parse(&mut context, &[Value::Str(Rc::from(nested.as_str()))]).unwrap();
        let result = stringify(&mut context, &[value]).unwrap();
        assert!(matches!(result, Value::Str(text) if text.as_ref() == nested));
    }

    #[test]
    fn serializer_bounds_output_and_rejects_cycles_and_unsupported_values() {
        let mut output = Vec::new();
        let mut context = RuntimeContext::new(&mut output, VmCapabilities::restricted());
        context.limits.max_buffer_bytes = Some(2);
        assert!(matches!(
            stringify(&mut context, &[Value::Str(Rc::from("long"))]),
            Err(RuntimeError::ResourceLimit { .. })
        ));

        context.limits.max_buffer_bytes = Some(1024);
        assert!(matches!(
            stringify(
                &mut context,
                &[Value::Bytes(
                    HeapRef::try_new(RefCell::new(Vec::new())).unwrap()
                )]
            ),
            Err(RuntimeError::TypeMismatch { .. })
        ));

        assert!(matches!(
            stringify(&mut context, &[Value::Float(f64::INFINITY)]),
            Err(RuntimeError::InvalidArgument { .. })
        ));

        let mut nested = Value::Nil;
        for _ in 0..=MAX_DEPTH {
            nested = Value::Array(HeapRef::try_new(ArrayValue::dynamic(vec![nested])).unwrap());
        }
        assert!(matches!(
            stringify(&mut context, &[nested]),
            Err(RuntimeError::ResourceLimit {
                resource: "JSON nesting"
            })
        ));

        let array = HeapRef::try_new(ArrayValue::dynamic(Vec::new())).unwrap();
        array.try_push(Value::Array(array.clone()), false).unwrap();
        assert!(matches!(
            stringify(&mut context, &[Value::Array(array.clone())]),
            Err(RuntimeError::InvalidArgument { .. })
        ));
        drop(array);
    }
}
