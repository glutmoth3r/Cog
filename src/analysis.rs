//! Semantic checking is independent of instruction emission. It produces the
//! type, effect, flow, and call-site facts consumed by specialization and codegen.
//! Poison represents invalid source, not a runtime Error value, and suppresses
//! derivative diagnostics during recovery.
use crate::ast::*;
use crate::builtins::Builtin;
use crate::diagnostic::{Diagnostic, DiagnosticSuggestion, SourceSpan};
use std::collections::{BTreeMap, HashMap, HashSet};

mod writes;
use writes::Writes;

const CLONE_TRAIT: &str = "$core::clone::Clone";
const MAX_TRAIT_COHERENCE_WORK: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Type {
    Primitive(VarType, bool),
    /// A non-primitive type that also admits nil. Primitives keep their own
    /// flag; build values with [`Type::nullable`] to stay canonical.
    Nullable(Box<Type>),
    Nil,
    Never,
    EmptyArray,
    Array(Box<Type>),
    Task(Box<Type>),
    Tuple(Vec<Type>),
    Function(Vec<Type>, Box<Type>),
    Bytes,
    StrBuf,
    Handle,
    Record,
    Struct(usize, String, Vec<Type>),
    Enum(usize, String),
    Named(String),
    Generic(String),
    Dynamic,
    ErrorValue,
    Poison,
}

/// Marks the bound symbols printed by `Type::display` for `name_nominal_types`.
const NOMINAL_MARK: char = '\u{e000}';

const NILABLE_MATCH: &str =
    "non-exhaustive match: the scrutinee may be nil; add a `nil` or `_` arm";

pub(crate) fn type_compatible(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Dynamic, _) | (_, Type::Poison | Type::ErrorValue | Type::Never) => true,
        (Type::Primitive(a, nullable), Type::Primitive(b, actual_nullable)) => {
            a == b && (!*actual_nullable || *nullable)
        }
        (Type::Primitive(_, true) | Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nil) => true,
        (Type::Nullable(expected), Type::Nullable(actual)) => type_compatible(expected, actual),
        (Type::Nullable(expected), actual) => type_compatible(expected, actual),
        (Type::Named(expected), Type::Struct(_, actual, _) | Type::Enum(_, actual)) => {
            expected == actual || actual.ends_with(&format!("::{expected}"))
        }
        (
            Type::Struct(expected_id, expected_name, expected_args),
            Type::Struct(actual_id, actual_name, actual_args),
        ) => {
            expected_id == actual_id
                && expected_name == actual_name
                && expected_args.len() == actual_args.len()
                && expected_args
                    .iter()
                    .zip(actual_args)
                    // Instances are distinct runtime types, so type arguments
                    // cannot differ in nullability.
                    .all(|(expected, actual)| {
                        type_compatible(expected, actual) && same_nullability(expected, actual)
                    })
        }
        (Type::Enum(expected, _), Type::Enum(actual, _)) => expected == actual,
        (Type::Array(_), Type::EmptyArray) => true,
        (Type::Array(expected), Type::Array(actual)) => array_element_compatible(expected, actual),
        (Type::Task(expected), Type::Task(actual)) => type_compatible(expected, actual),
        (Type::Tuple(a), Type::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| type_compatible(x, y))
        }
        (Type::Function(a, ar), Type::Function(b, br)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(x, y)| type_compatible(x, y) && type_compatible(y, x))
                && type_compatible(ar, br)
        }
        (a, b) => a == b,
    }
}

fn same_nullability(left: &Type, right: &Type) -> bool {
    if matches!(
        (left, right),
        (Type::Dynamic | Type::Generic(_) | Type::Poison, _)
            | (_, Type::Dynamic | Type::Generic(_) | Type::Poison)
    ) {
        return true;
    }
    if left.is_nullable() != right.is_nullable() {
        return false;
    }
    match (left.non_nil(), right.non_nil()) {
        (Type::Array(left), Type::Array(right)) | (Type::Task(left), Type::Task(right)) => {
            same_nullability(&left, &right)
        }
        (Type::Tuple(left), Type::Tuple(right))
        | (Type::Struct(_, _, left), Type::Struct(_, _, right)) => left
            .iter()
            .zip(&right)
            .all(|(left, right)| same_nullability(left, right)),
        (Type::Function(left, left_result), Type::Function(right, right_result)) => {
            same_nullability(&left_result, &right_result)
                && left
                    .iter()
                    .zip(&right)
                    .all(|(left, right)| same_nullability(left, right))
        }
        _ => true,
    }
}

/// Whether joining two types would let an existing mutable container be used
/// with a different element type or type argument. Containers are invariant:
/// a write through the joined type could not be stored.
fn container_conflict(left: &Type, right: &Type) -> bool {
    match (left.non_nil(), right.non_nil()) {
        (Type::Array(left), Type::Array(right)) => {
            let joined = (*left).clone().join((*right).clone());
            container_conflict(&left, &right)
                || (joined != Type::Dynamic
                    && !(array_element_compatible(&joined, &left)
                        && array_element_compatible(&joined, &right)))
        }
        (Type::Task(left), Type::Task(right)) => container_conflict(&left, &right),
        (Type::Tuple(left), Type::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(&right)
                    .any(|(left, right)| container_conflict(left, right))
        }
        (Type::Struct(left_id, _, left), Type::Struct(right_id, _, right)) => {
            left_id == right_id
                && !left
                    .iter()
                    .zip(&right)
                    .all(|(left, right)| same_nullability(left, right))
        }
        _ => false,
    }
}

/// Natives that declare an unconstrained parameter but cannot convert or hash
/// `nil`.
fn native_rejects_nil(name: &str, parameter: usize) -> bool {
    parameter == 0
        && matches!(
            name,
            "hash::of"
                | "str::to_int"
                | "str::to_uint"
                | "str::to_bigint"
                | "str::to_float"
                | "str::to_bool"
                | "str::to_char"
        )
}

/// An array or tuple literal takes a nullable element type from its context:
/// the emitter builds it with the declared element type.
fn literal_widens(expr: &Expr, actual: &Type, expected: &Type) -> bool {
    if actual == expected {
        return true;
    }
    if expected.is_nullable() {
        return *actual == Type::Nil
            || literal_widens(expr, &actual.non_nil(), &expected.non_nil());
    }
    match (expr.node(), actual, expected) {
        (Expr::Array(_), Type::EmptyArray, Type::Array(_)) => true,
        (Expr::Array(items), Type::Array(actual), Type::Array(expected)) => items
            .iter()
            .all(|item| literal_widens(item, actual, expected)),
        (Expr::Tuple(items), Type::Tuple(actual), Type::Tuple(expected)) => {
            items.len() == actual.len()
                && actual.len() == expected.len()
                && items
                    .iter()
                    .zip(actual)
                    .zip(expected)
                    .all(|((item, actual), expected)| literal_widens(item, actual, expected))
        }
        _ => false,
    }
}

/// The type of an argument or result after its context widens a literal.
fn widened(expr: &Expr, actual: Type, expected: &Type) -> Type {
    if matches!(expr.node(), Expr::Array(_) | Expr::Tuple(_))
        && literal_widens(expr, &actual, expected)
    {
        expected.clone()
    } else {
        actual
    }
}

fn contains_nullable(ty: &Type) -> bool {
    match ty {
        Type::Primitive(..) => ty.is_nullable(),
        Type::Nullable(_) => true,
        Type::Array(element) | Type::Task(element) => contains_nullable(element),
        Type::Tuple(items) | Type::Struct(_, _, items) => items.iter().any(contains_nullable),
        Type::Function(parameters, result) => {
            parameters.iter().any(contains_nullable) || contains_nullable(result)
        }
        _ => false,
    }
}

fn contains_generic(ty: &Type) -> bool {
    match ty {
        Type::Generic(_) => true,
        Type::Nullable(inner) | Type::Array(inner) | Type::Task(inner) => contains_generic(inner),
        Type::Tuple(items) | Type::Struct(_, _, items) => items.iter().any(contains_generic),
        Type::Function(parameters, result) => {
            parameters.iter().any(contains_generic) || contains_generic(result)
        }
        _ => false,
    }
}

/// Generic inference visits `nil` arguments last, so that `T?` takes `T` from
/// the other arguments when they provide it.
fn inference_order(actual: &[Type]) -> impl Iterator<Item = (usize, &Type)> {
    let indexed = actual.iter().enumerate();
    indexed
        .clone()
        .filter(|(_, ty)| **ty != Type::Nil)
        .chain(indexed.filter(|(_, ty)| **ty == Type::Nil))
}

fn integer_literal_expression(expr: &Expr) -> bool {
    match expr.node() {
        Expr::Int(value) => *value >= 0,
        Expr::UInt(_) => true,
        Expr::Binary(
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
            left,
            right,
        ) => integer_literal_expression(left) && integer_literal_expression(right),
        _ => false,
    }
}

fn bigint_literal_expression(expr: &Expr) -> bool {
    integer_literal_tree(expr).is_some()
}

/// Whether an integer-literal expression contains a BigInt literal.
fn integer_literal_tree(expr: &Expr) -> Option<bool> {
    match expr.node() {
        Expr::Int(_) | Expr::UInt(_) => Some(false),
        Expr::BigInt(_) => Some(true),
        Expr::Unary(UnOp::Neg, value) => integer_literal_tree(value),
        Expr::Binary(
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
            left,
            right,
        ) => Some(integer_literal_tree(left)? | integer_literal_tree(right)?),
        _ => None,
    }
}

fn source_replacement(
    span: &SourceSpan,
    message: impl Into<String>,
    target: VarType,
) -> Option<DiagnosticSuggestion> {
    let text = span.source.text.get(span.range.clone())?.trim();
    if text.is_empty() || text.len() > 64 || text.contains('\n') || text.contains('\r') {
        return None;
    }
    Some(DiagnosticSuggestion {
        message: message.into(),
        span: span.clone(),
        replacement: format!("({text}) as {}", target.name()),
    })
}

fn expression_conversion_suggestion(
    expression: &Expr,
    from: VarType,
    to: VarType,
    condition: Option<&str>,
) -> Option<DiagnosticSuggestion> {
    let span = expression.span()?;
    let message = if let Some(condition) = condition {
        format!(
            "{condition}; convert this `{}` operand to `{}`",
            from.name(),
            to.name()
        )
    } else {
        format!("convert this `{}` operand to `{}`", from.name(), to.name())
    };
    source_replacement(span, message, to)
}

fn mixed_numeric_suggestions(
    left_type: VarType,
    right_type: VarType,
    left: &Expr,
    right: &Expr,
) -> Vec<DiagnosticSuggestion> {
    let mut suggestions = Vec::new();
    match (left_type, right_type) {
        (VarType::BigInt, VarType::Int | VarType::UInt) => {
            if let Some(suggestion) =
                expression_conversion_suggestion(right, right_type, VarType::BigInt, None)
            {
                suggestions.push(suggestion);
            }
        }
        (VarType::Int | VarType::UInt, VarType::BigInt) => {
            if let Some(suggestion) =
                expression_conversion_suggestion(left, left_type, VarType::BigInt, None)
            {
                suggestions.push(suggestion);
            }
        }
        (VarType::Int, VarType::UInt) => {
            if let Some(suggestion) = expression_conversion_suggestion(
                left,
                VarType::Int,
                VarType::UInt,
                Some("if the signed value is non-negative"),
            ) {
                suggestions.push(suggestion);
            }
            if let Some(suggestion) = expression_conversion_suggestion(
                right,
                VarType::UInt,
                VarType::Int,
                Some("if the unsigned value fits in `int`"),
            ) {
                suggestions.push(suggestion);
            }
        }
        (VarType::UInt, VarType::Int) => {
            if let Some(suggestion) = expression_conversion_suggestion(
                left,
                VarType::UInt,
                VarType::Int,
                Some("if the unsigned value fits in `int`"),
            ) {
                suggestions.push(suggestion);
            }
            if let Some(suggestion) = expression_conversion_suggestion(
                right,
                VarType::Int,
                VarType::UInt,
                Some("if the signed value is non-negative"),
            ) {
                suggestions.push(suggestion);
            }
        }
        (VarType::BigInt, VarType::Float) => {
            if let Some(suggestion) = expression_conversion_suggestion(
                left,
                VarType::BigInt,
                VarType::Float,
                Some("if the value fits in `float` and rounding large integers is acceptable"),
            ) {
                suggestions.push(suggestion);
            }
        }
        (VarType::Float, VarType::BigInt) => {
            if let Some(suggestion) = expression_conversion_suggestion(
                right,
                VarType::BigInt,
                VarType::Float,
                Some("if the value fits in `float` and rounding large integers is acceptable"),
            ) {
                suggestions.push(suggestion);
            }
        }
        _ => {}
    }
    suggestions
}

fn nullable_type_mismatch_help(expected: &Type, actual: &Type) -> Option<String> {
    if expected.is_nullable() || matches!(expected, Type::Nil | Type::Dynamic) {
        return None;
    }
    let may_be_nil = *actual == Type::Nil
        || (actual.is_nullable() && type_compatible(expected, &actual.non_nil()));
    if may_be_nil {
        return Some(format!(
            "this value may be `nil`; check it with `!= nil`, supply a fallback with `??`, or make the destination `{}`",
            expected.clone().nullable().display()
        ));
    }
    (matches!(expected, Type::Primitive(_, false)) && matches!(actual, Type::Primitive(_, true)))
        .then(|| "handle the possible `nil` value before converting".into())
}

fn primitive_type_mismatch_suggestion(
    expected: &Type,
    actual: &Type,
    span: Option<&SourceSpan>,
) -> Option<DiagnosticSuggestion> {
    let Type::Primitive(to, _) = expected else {
        return None;
    };
    if *to == VarType::Nil {
        return None;
    }
    let from = match actual {
        Type::Primitive(from, false) if *from != *to && supports_primitive_cast(*from, *to) => {
            Some(*from)
        }
        Type::Dynamic => None,
        _ => return None,
    };
    let message = match from {
        Some(from) => {
            let condition = match (from, *to) {
                (VarType::Int, VarType::UInt) => Some("if the signed value is non-negative"),
                (VarType::UInt, VarType::Int) => Some("if the unsigned value fits in `int`"),
                (VarType::Float, VarType::Int | VarType::UInt) => {
                    Some("if truncating the fractional part is intended and the value is in range")
                }
                (VarType::BigInt, VarType::Int | VarType::UInt) => {
                    Some("if the value fits in the target integer type")
                }
                (VarType::BigInt, VarType::Float) => {
                    Some("if rounding large integers is acceptable")
                }
                (VarType::Str, VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float) => {
                    Some("if the string contains a valid number")
                }
                _ => None,
            };
            if let Some(condition) = condition {
                format!(
                    "{condition}; convert this `{}` value to `{}`",
                    from.name(),
                    to.name()
                )
            } else {
                format!("convert this `{}` value to `{}`", from.name(), to.name())
            }
        }
        None => format!("convert this dynamic value to `{}`", to.name()),
    };
    source_replacement(span?, message, *to)
}

fn supports_primitive_cast(from: VarType, to: VarType) -> bool {
    match to {
        VarType::Int | VarType::UInt => matches!(
            from,
            VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float | VarType::Str
        ),
        VarType::BigInt => matches!(
            from,
            VarType::Int | VarType::UInt | VarType::BigInt | VarType::Str
        ),
        VarType::Float => matches!(
            from,
            VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float | VarType::Str
        ),
        VarType::Bool => matches!(from, VarType::Bool | VarType::Str),
        VarType::Char => matches!(from, VarType::Char | VarType::Str),
        VarType::Str => true,
        VarType::Nil => false,
    }
}

fn array_element_compatible(expected: &Type, actual: &Type) -> bool {
    match (expected, actual) {
        (Type::Dynamic, _) | (_, Type::Dynamic) => true,
        (_, Type::Poison | Type::ErrorValue | Type::Never) => true,
        (Type::Enum(expected, _), Type::Enum(actual, _)) => expected == actual,
        (Type::Nullable(expected), Type::Nullable(actual)) => {
            array_element_compatible(expected, actual)
        }
        (Type::Array(_), Type::EmptyArray) => true,
        (Type::Array(expected), Type::Array(actual)) => array_element_compatible(expected, actual),
        (Type::Tuple(expected), Type::Tuple(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| array_element_compatible(expected, actual))
        }
        (expected, actual) => expected == actual,
    }
}

fn generic_extension_matches(extension: &ExtendDecl, receiver: &Type) -> bool {
    let parameters: HashSet<_> = extension
        .type_params
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    let mut substitutions = HashMap::new();
    if !match_extension_type(&extension.target, receiver, &parameters, &mut substitutions) {
        return false;
    }
    extension
        .type_params
        .iter()
        .all(|parameter| substitutions.contains_key(&parameter.name))
}

fn associated_type_name(ty: &TypeAnnotation) -> Option<&str> {
    match &ty.kind {
        TypeKind::Named(name) => Some(name),
        TypeKind::ResolvedName { symbol, .. } => Some(symbol),
        TypeKind::Applied { source, .. } => Some(source),
        TypeKind::ResolvedApplied { symbol, .. } => Some(symbol),
        _ => None,
    }
}

fn extension_target_uses_parameter(ty: &TypeAnnotation, name: &str) -> bool {
    match &ty.kind {
        TypeKind::TypeParameter(parameter) => parameter == name,
        TypeKind::Array(element) => extension_target_uses_parameter(element, name),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            arguments
                .iter()
                .any(|argument| extension_target_uses_parameter(argument, name))
        }
        TypeKind::Tuple(elements) => elements
            .iter()
            .any(|element| extension_target_uses_parameter(element, name)),
        TypeKind::Function { parameters, result } => {
            parameters
                .iter()
                .any(|ty| extension_target_uses_parameter(ty, name))
                || extension_target_uses_parameter(result, name)
        }
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::SelfType => false,
    }
}

fn match_extension_type(
    pattern: &TypeAnnotation,
    actual: &Type,
    parameters: &HashSet<&str>,
    substitutions: &mut HashMap<String, Type>,
) -> bool {
    // A nullable pattern matches on the underlying type. Any other pattern
    // except a bare type parameter rejects a nullable receiver below.
    let stripped;
    let actual = if pattern.nilable {
        stripped = actual.non_nil();
        &stripped
    } else {
        actual
    };
    match &pattern.kind {
        TypeKind::TypeParameter(name) if parameters.contains(name.as_str()) => {
            if matches!(
                actual,
                Type::Dynamic | Type::Generic(_) | Type::Poison | Type::ErrorValue
            ) {
                return false;
            }
            match substitutions.get(name) {
                Some(previous) => type_compatible(previous, actual),
                None => {
                    substitutions.insert(name.clone(), actual.clone());
                    true
                }
            }
        }
        TypeKind::Applied { source, arguments } => {
            if source == "Task" {
                let Type::Task(actual) = actual else {
                    return false;
                };
                return arguments.len() == 1
                    && match_extension_type(&arguments[0], actual, parameters, substitutions);
            }
            let Type::Struct(_, actual_name, actual_arguments) = actual else {
                return false;
            };
            (source == actual_name || actual_name.ends_with(&format!("::{source}")))
                && arguments.len() == actual_arguments.len()
                && arguments
                    .iter()
                    .zip(actual_arguments)
                    .all(|(pattern, actual)| {
                        match_extension_type(pattern, actual, parameters, substitutions)
                    })
        }
        TypeKind::ResolvedApplied {
            source,
            symbol,
            arguments,
        } => {
            if source == "Task" || symbol == "Task" {
                let Type::Task(actual) = actual else {
                    return false;
                };
                return arguments.len() == 1
                    && match_extension_type(&arguments[0], actual, parameters, substitutions);
            }
            let Type::Struct(_, actual_name, actual_arguments) = actual else {
                return false;
            };
            (symbol == actual_name || actual_name.ends_with(&format!("::{source}")))
                && arguments.len() == actual_arguments.len()
                && arguments
                    .iter()
                    .zip(actual_arguments)
                    .all(|(pattern, actual)| {
                        match_extension_type(pattern, actual, parameters, substitutions)
                    })
        }
        TypeKind::Array(element) => match actual {
            Type::Array(actual) => match_extension_type(element, actual, parameters, substitutions),
            _ => false,
        },
        TypeKind::Tuple(elements) => match actual {
            Type::Tuple(actual) if elements.len() == actual.len() => {
                elements.iter().zip(actual).all(|(pattern, actual)| {
                    match_extension_type(pattern, actual, parameters, substitutions)
                })
            }
            _ => false,
        },
        TypeKind::Primitive(base) => match actual {
            Type::Primitive(actual, nullable) => base == actual && (!*nullable || pattern.nilable),
            Type::Nil => *base == VarType::Nil,
            _ => false,
        },
        TypeKind::Bytes => matches!(actual, Type::Bytes),
        TypeKind::ResolvedName { symbol, .. } | TypeKind::Named(symbol) => match actual {
            Type::Struct(_, name, arguments) => {
                arguments.is_empty() && (symbol == name || name.ends_with(&format!("::{symbol}")))
            }
            Type::Enum(_, name) => symbol == name || name.ends_with(&format!("::{symbol}")),
            Type::Primitive(base, nullable) => {
                let named = match symbol.as_str() {
                    "int" => VarType::Int,
                    "uint" => VarType::UInt,
                    "bigint" => VarType::BigInt,
                    "bool" => VarType::Bool,
                    "char" => VarType::Char,
                    "str" => VarType::Str,
                    "float" => VarType::Float,
                    _ => return false,
                };
                named == *base && (!*nullable || pattern.nilable)
            }
            _ => false,
        },
        TypeKind::Function {
            parameters: pattern_parameters,
            result,
        } => match actual {
            Type::Function(actual_parameters, actual_result)
                if pattern_parameters.len() == actual_parameters.len() =>
            {
                pattern_parameters
                    .iter()
                    .zip(actual_parameters)
                    .all(|(pattern, actual)| {
                        match_extension_type(pattern, actual, parameters, substitutions)
                    })
                    && match_extension_type(result, actual_result, parameters, substitutions)
            }
            _ => false,
        },
        TypeKind::SelfType => false,
        TypeKind::TypeParameter(_) => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TraitTypePattern {
    Variable(u32),
    Primitive(VarType),
    Nil,
    Named(String, Vec<Self>),
    Array(Box<Self>),
    Tuple(Vec<Self>),
    Function(Vec<Self>, Box<Self>),
    Nullable(Box<Self>),
    Opaque(String),
}

fn trait_type_pattern(
    ty: &TypeAnnotation,
    generic_parameters: &HashMap<&str, u32>,
) -> TraitTypePattern {
    let pattern = match &ty.kind {
        TypeKind::Primitive(base) if *base == VarType::Nil && !ty.nilable => TraitTypePattern::Nil,
        TypeKind::Primitive(base) => TraitTypePattern::Primitive(*base),
        TypeKind::Bytes => TraitTypePattern::Opaque("bytes".to_owned()),
        TypeKind::TypeParameter(name) => generic_parameters
            .get(name.as_str())
            .copied()
            .map(TraitTypePattern::Variable)
            .unwrap_or_else(|| TraitTypePattern::Opaque(format!("type parameter `{name}`"))),
        TypeKind::Named(name) => TraitTypePattern::Named(name.clone(), Vec::new()),
        TypeKind::ResolvedName { symbol, .. } => {
            TraitTypePattern::Named(symbol.clone(), Vec::new())
        }
        TypeKind::Applied { source, arguments } => TraitTypePattern::Named(
            source.clone(),
            arguments
                .iter()
                .map(|argument| trait_type_pattern(argument, generic_parameters))
                .collect(),
        ),
        TypeKind::ResolvedApplied {
            symbol, arguments, ..
        } => TraitTypePattern::Named(
            symbol.clone(),
            arguments
                .iter()
                .map(|argument| trait_type_pattern(argument, generic_parameters))
                .collect(),
        ),
        TypeKind::Array(element) => {
            TraitTypePattern::Array(Box::new(trait_type_pattern(element, generic_parameters)))
        }
        TypeKind::Tuple(elements) => TraitTypePattern::Tuple(
            elements
                .iter()
                .map(|element| trait_type_pattern(element, generic_parameters))
                .collect(),
        ),
        TypeKind::Function { parameters, result } => TraitTypePattern::Function(
            parameters
                .iter()
                .map(|parameter| trait_type_pattern(parameter, generic_parameters))
                .collect(),
            Box::new(trait_type_pattern(result, generic_parameters)),
        ),
        TypeKind::SelfType => TraitTypePattern::Opaque("Self".to_owned()),
    };
    if ty.nilable {
        TraitTypePattern::Nullable(Box::new(pattern))
    } else {
        pattern
    }
}

fn concrete_trait_type_pattern(ty: &Type) -> TraitTypePattern {
    match ty {
        Type::Primitive(base, true) => {
            TraitTypePattern::Nullable(Box::new(TraitTypePattern::Primitive(*base)))
        }
        Type::Primitive(base, false) => TraitTypePattern::Primitive(*base),
        Type::Nullable(inner) => {
            TraitTypePattern::Nullable(Box::new(concrete_trait_type_pattern(inner)))
        }
        Type::Nil => TraitTypePattern::Nil,
        Type::Array(element) => {
            TraitTypePattern::Array(Box::new(concrete_trait_type_pattern(element)))
        }
        Type::Task(result) => {
            TraitTypePattern::Named("Task".to_owned(), vec![concrete_trait_type_pattern(result)])
        }
        Type::Tuple(elements) => {
            TraitTypePattern::Tuple(elements.iter().map(concrete_trait_type_pattern).collect())
        }
        Type::Function(parameters, result) => TraitTypePattern::Function(
            parameters.iter().map(concrete_trait_type_pattern).collect(),
            Box::new(concrete_trait_type_pattern(result)),
        ),
        Type::Struct(_, name, arguments) => TraitTypePattern::Named(
            name.clone(),
            arguments.iter().map(concrete_trait_type_pattern).collect(),
        ),
        Type::Enum(_, name) | Type::Named(name) => {
            TraitTypePattern::Named(name.clone(), Vec::new())
        }
        Type::Generic(name) => TraitTypePattern::Opaque(format!("generic `{name}`")),
        Type::Dynamic => TraitTypePattern::Opaque("dynamic".to_owned()),
        Type::EmptyArray => TraitTypePattern::Opaque("empty array".to_owned()),
        Type::Never => TraitTypePattern::Opaque("never".to_owned()),
        Type::Bytes => TraitTypePattern::Opaque("bytes".to_owned()),
        Type::StrBuf => TraitTypePattern::Opaque("strbuf".to_owned()),
        Type::Handle => TraitTypePattern::Opaque("handle".to_owned()),
        Type::Record => TraitTypePattern::Opaque("record".to_owned()),
        Type::ErrorValue => TraitTypePattern::Opaque("Error".to_owned()),
        Type::Poison => TraitTypePattern::Opaque("poison".to_owned()),
    }
}

fn unify_trait_patterns(
    left: &TraitTypePattern,
    right: &TraitTypePattern,
    bindings: &mut HashMap<u32, TraitTypePattern>,
) -> bool {
    fn dereference(
        pattern: &TraitTypePattern,
        bindings: &HashMap<u32, TraitTypePattern>,
    ) -> TraitTypePattern {
        let mut current = pattern.clone();
        while let TraitTypePattern::Variable(variable) = current {
            let Some(next) = bindings.get(&variable) else {
                return TraitTypePattern::Variable(variable);
            };
            current = next.clone();
        }
        current
    }

    fn occurs(
        variable: u32,
        pattern: &TraitTypePattern,
        bindings: &HashMap<u32, TraitTypePattern>,
    ) -> bool {
        match dereference(pattern, bindings) {
            TraitTypePattern::Variable(other) => variable == other,
            TraitTypePattern::Named(_, arguments) | TraitTypePattern::Tuple(arguments) => arguments
                .iter()
                .any(|argument| occurs(variable, argument, bindings)),
            TraitTypePattern::Array(element) | TraitTypePattern::Nullable(element) => {
                occurs(variable, &element, bindings)
            }
            TraitTypePattern::Function(parameters, result) => {
                parameters
                    .iter()
                    .any(|parameter| occurs(variable, parameter, bindings))
                    || occurs(variable, &result, bindings)
            }
            TraitTypePattern::Primitive(_)
            | TraitTypePattern::Nil
            | TraitTypePattern::Opaque(_) => false,
        }
    }

    let left = dereference(left, bindings);
    let right = dereference(right, bindings);
    if left == right {
        return true;
    }
    match (&left, &right) {
        (TraitTypePattern::Variable(variable), _) if !occurs(*variable, &right, bindings) => {
            bindings.insert(*variable, right);
            true
        }
        (TraitTypePattern::Variable(_), _) => false,
        (_, TraitTypePattern::Variable(variable)) if !occurs(*variable, &left, bindings) => {
            bindings.insert(*variable, left);
            true
        }
        (_, TraitTypePattern::Variable(_)) => false,
        (TraitTypePattern::Primitive(left), TraitTypePattern::Primitive(right)) => left == right,
        (TraitTypePattern::Nil, TraitTypePattern::Nil) => true,
        (
            TraitTypePattern::Named(left_name, left_args),
            TraitTypePattern::Named(right_name, right_args),
        ) => {
            left_name == right_name
                && left_args.len() == right_args.len()
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| unify_trait_patterns(left, right, bindings))
        }
        (TraitTypePattern::Array(left), TraitTypePattern::Array(right))
        | (TraitTypePattern::Nullable(left), TraitTypePattern::Nullable(right)) => {
            unify_trait_patterns(left, right, bindings)
        }
        (TraitTypePattern::Tuple(left), TraitTypePattern::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| unify_trait_patterns(left, right, bindings))
        }
        (
            TraitTypePattern::Function(left_params, left_result),
            TraitTypePattern::Function(right_params, right_result),
        ) => {
            left_params.len() == right_params.len()
                && left_params
                    .iter()
                    .zip(right_params)
                    .all(|(left, right)| unify_trait_patterns(left, right, bindings))
                && unify_trait_patterns(left_result, right_result, bindings)
        }
        (TraitTypePattern::Opaque(left), TraitTypePattern::Opaque(right)) => left == right,
        _ => false,
    }
}

fn generic_trait_implementations_overlap(left: &ExtendDecl, right: &ExtendDecl) -> bool {
    if left.trait_name != right.trait_name
        || left.trait_arguments.len() != right.trait_arguments.len()
    {
        return false;
    }
    let left_parameters: HashMap<_, _> = left
        .type_params
        .iter()
        .enumerate()
        .map(|(index, parameter)| (parameter.name.as_str(), index as u32))
        .collect();
    let right_parameters: HashMap<_, _> = right
        .type_params
        .iter()
        .enumerate()
        .map(|(index, parameter)| (parameter.name.as_str(), (index as u32) | (1 << 31)))
        .collect();
    let left_target = trait_type_pattern(&left.target, &left_parameters);
    let right_target = trait_type_pattern(&right.target, &right_parameters);
    let mut bindings = HashMap::new();
    unify_trait_patterns(&left_target, &right_target, &mut bindings)
        && left
            .trait_arguments
            .iter()
            .zip(&right.trait_arguments)
            .all(|(left, right)| {
                unify_trait_patterns(
                    &trait_type_pattern(left, &left_parameters),
                    &trait_type_pattern(right, &right_parameters),
                    &mut bindings,
                )
            })
}

fn generic_trait_implementation_overlaps_exact(
    generic: &ExtendDecl,
    target: &Type,
    trait_name: &str,
    arguments: &[Type],
) -> bool {
    if generic.trait_name.as_deref() != Some(trait_name)
        || generic.trait_arguments.len() != arguments.len()
    {
        return false;
    }
    let parameters: HashMap<_, _> = generic
        .type_params
        .iter()
        .enumerate()
        .map(|(index, parameter)| (parameter.name.as_str(), index as u32))
        .collect();
    let mut bindings = HashMap::new();
    if !unify_trait_patterns(
        &trait_type_pattern(&generic.target, &parameters),
        &concrete_trait_type_pattern(target),
        &mut bindings,
    ) {
        return false;
    }
    generic
        .trait_arguments
        .iter()
        .zip(arguments)
        .all(|(pattern, actual)| {
            unify_trait_patterns(
                &trait_type_pattern(pattern, &parameters),
                &concrete_trait_type_pattern(actual),
                &mut bindings,
            )
        })
}

fn trait_annotation_cost(ty: &TypeAnnotation) -> usize {
    1usize.saturating_add(match &ty.kind {
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            arguments.iter().map(trait_annotation_cost).sum()
        }
        TypeKind::Array(element) => trait_annotation_cost(element),
        TypeKind::Tuple(elements) => elements.iter().map(trait_annotation_cost).sum(),
        TypeKind::Function { parameters, result } => parameters
            .iter()
            .map(trait_annotation_cost)
            .sum::<usize>()
            .saturating_add(trait_annotation_cost(result)),
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => 0,
    })
}

fn concrete_trait_type_cost(ty: &Type) -> usize {
    1usize.saturating_add(match ty {
        Type::Array(element) | Type::Task(element) | Type::Nullable(element) => {
            concrete_trait_type_cost(element)
        }
        Type::Tuple(elements) => elements.iter().map(concrete_trait_type_cost).sum(),
        Type::Function(parameters, result) => parameters
            .iter()
            .map(concrete_trait_type_cost)
            .sum::<usize>()
            .saturating_add(concrete_trait_type_cost(result)),
        Type::Struct(_, _, arguments) => arguments.iter().map(concrete_trait_type_cost).sum(),
        Type::Primitive(_, _)
        | Type::Nil
        | Type::Never
        | Type::EmptyArray
        | Type::Bytes
        | Type::StrBuf
        | Type::Handle
        | Type::Record
        | Type::Enum(_, _)
        | Type::Named(_)
        | Type::Generic(_)
        | Type::Dynamic
        | Type::ErrorValue
        | Type::Poison => 0,
    })
}

fn trait_implementation_cost(extension: &ExtendDecl) -> usize {
    extension
        .trait_arguments
        .iter()
        .map(trait_annotation_cost)
        .fold(
            trait_annotation_cost(&extension.target),
            usize::saturating_add,
        )
}

fn contains_unresolved_empty_array(ty: &Type) -> bool {
    match ty {
        Type::EmptyArray => true,
        Type::Array(element) | Type::Nullable(element) => contains_unresolved_empty_array(element),
        Type::Tuple(items) => items.iter().any(contains_unresolved_empty_array),
        Type::Function(parameters, result) => {
            parameters.iter().any(contains_unresolved_empty_array)
                || contains_unresolved_empty_array(result)
        }
        _ => false,
    }
}

fn replace_self_type(ty: &mut TypeAnnotation, target: &TypeAnnotation) {
    match &mut ty.kind {
        TypeKind::SelfType => {
            let nilable = ty.nilable || target.nilable;
            *ty = target.clone();
            ty.nilable = nilable;
        }
        TypeKind::Array(element) => replace_self_type(element, target),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            for argument in arguments {
                replace_self_type(argument, target);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                replace_self_type(element, target);
            }
        }
        TypeKind::Function { parameters, result } => {
            for parameter in parameters {
                replace_self_type(parameter, target);
            }
            replace_self_type(result, target);
        }
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_) => {}
    }
}

fn substitute_trait_type(ty: &mut TypeAnnotation, substitutions: &HashMap<String, TypeAnnotation>) {
    if let TypeKind::TypeParameter(name) = &ty.kind {
        if let Some(replacement) = substitutions.get(name) {
            let nilable = ty.nilable || replacement.nilable;
            *ty = replacement.clone();
            ty.nilable = nilable;
        }
        return;
    }
    match &mut ty.kind {
        TypeKind::Array(element) => substitute_trait_type(element, substitutions),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            for argument in arguments {
                substitute_trait_type(argument, substitutions);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                substitute_trait_type(element, substitutions);
            }
        }
        TypeKind::Function { parameters, result } => {
            for parameter in parameters {
                substitute_trait_type(parameter, substitutions);
            }
            substitute_trait_type(result, substitutions);
        }
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => {}
    }
}

fn is_concrete_trait_argument(ty: &Type) -> bool {
    match ty {
        Type::Dynamic | Type::Generic(_) | Type::EmptyArray | Type::Never | Type::Poison => false,
        Type::Array(element) | Type::Nullable(element) => is_concrete_trait_argument(element),
        Type::Tuple(elements) => elements.iter().all(is_concrete_trait_argument),
        Type::Function(parameters, result) => {
            parameters.iter().all(is_concrete_trait_argument) && is_concrete_trait_argument(result)
        }
        _ => true,
    }
}

impl Type {
    fn dynamic_array() -> Self {
        Self::Array(Box::new(Self::Dynamic))
    }
    fn concrete(base: VarType) -> Self {
        Self::Primitive(base, false)
    }

    /// The canonical type admitting the values of `self` and nil.
    fn nullable(self) -> Self {
        match self {
            Self::Primitive(base, _) => Self::Primitive(base, true),
            Self::Nil | Self::Dynamic | Self::Poison | Self::Nullable(_) => self,
            Self::Never | Self::ErrorValue => Self::Nil,
            other => Self::Nullable(Box::new(other)),
        }
    }

    /// Whether the type admits both nil and another normal value.
    fn is_nullable(&self) -> bool {
        match self {
            Self::Primitive(base, nilable) => *nilable && *base != VarType::Nil,
            Self::Nullable(_) => true,
            _ => false,
        }
    }

    /// Whether a member of this value cannot be used without a nil check.
    fn may_be_nil(&self) -> bool {
        *self == Self::Nil || self.is_nullable()
    }

    /// The type of a value of this type that is known not to be nil.
    fn non_nil(&self) -> Self {
        match self {
            Self::Primitive(base, _) if *base != VarType::Nil => Self::Primitive(*base, false),
            Self::Nullable(inner) => (**inner).clone(),
            other => other.clone(),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Primitive(base, nilable) => TypeAnnotation {
                kind: TypeKind::Primitive(*base),
                nilable: *nilable,
            }
            .display(),
            Self::Nullable(inner) if matches!(**inner, Self::Function(..)) => {
                format!("({})?", inner.display())
            }
            Self::Nullable(inner) => format!("{}?", inner.display()),
            Self::Nil => "nil".into(),
            Self::Never => "<no normal exit>".into(),
            Self::EmptyArray => "empty array".into(),
            Self::Array(element) if matches!(**element, Self::Function(..)) => {
                format!("({})[]", element.display())
            }
            Self::Array(element) => format!("{}[]", element.display()),
            Self::Task(result) => format!("Task[{}]", result.display()),
            Self::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(Type::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Function(parameters, result) => format!(
                "fn({}) -> {}",
                parameters
                    .iter()
                    .map(Type::display)
                    .collect::<Vec<_>>()
                    .join(", "),
                result.display()
            ),
            Self::Bytes => "bytes".into(),
            Self::StrBuf => "string builder".into(),
            Self::Handle => "resource".into(),
            Self::Record => "record".into(),
            Self::Struct(_, name, arguments) if !arguments.is_empty() => format!(
                "{NOMINAL_MARK}{}[{}]",
                name,
                arguments
                    .iter()
                    .map(Type::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Struct(_, name, _) => format!("{NOMINAL_MARK}{name}"),
            // A value typed by one variant is still reported as its enum.
            Self::Enum(_, name) => {
                let name = match name.rsplit_once("::") {
                    Some((parent, _))
                        if parent == crate::error::ERROR_TYPE
                            || (parent.starts_with('$') && !parent.contains("::")) =>
                    {
                        parent
                    }
                    _ => name,
                };
                format!("{NOMINAL_MARK}{name}")
            }
            Self::Named(name) => name.clone(),
            Self::Generic(name) => name.clone(),
            Self::Dynamic => "dynamic".into(),
            Self::ErrorValue => "error".into(),
            Self::Poison => "<invalid>".into(),
        }
    }

    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Poison, _) | (_, Self::Poison) => Self::Poison,
            (Self::Never, t) | (t, Self::Never) => t,
            (Self::ErrorValue, t) | (t, Self::ErrorValue) => t,
            (Self::Dynamic, _) | (_, Self::Dynamic) => Self::Dynamic,
            (Self::Nil, Self::Nil) => Self::Nil,
            (Self::Nil, t) | (t, Self::Nil) => t.nullable(),
            (a, b) if a.is_nullable() || b.is_nullable() => {
                a.non_nil().join(b.non_nil()).nullable()
            }
            (Self::Primitive(a, an), Self::Primitive(b, bn)) if a == b => {
                Self::Primitive(a, an || bn)
            }
            (Self::EmptyArray, Self::Array(element)) | (Self::Array(element), Self::EmptyArray) => {
                Self::Array(element)
            }
            (Self::Array(a), Self::Array(b)) if a == b => Self::Array(a),
            (Self::Task(a), Self::Task(b)) => Self::Task(Box::new((*a).join(*b))),
            (a @ Self::Function(_, _), b @ Self::Function(_, _)) if a == b => a,
            (Self::Array(a), Self::Array(b)) => {
                let element = (*a).join(*b);
                if element == Self::Dynamic {
                    Self::Dynamic
                } else {
                    Self::Array(Box::new(element))
                }
            }
            (Self::Enum(a, left), Self::Enum(b, right)) if a == b => {
                let parent = left
                    .rsplit_once("::")
                    .map_or(left.as_str(), |(parent, _)| parent);
                let right_parent = right
                    .rsplit_once("::")
                    .map_or(right.as_str(), |(parent, _)| parent);
                if parent == right_parent {
                    Self::Enum(a, parent.to_owned())
                } else {
                    Self::Dynamic
                }
            }
            (Self::Tuple(a), Self::Tuple(b)) if a == b => Self::Tuple(a),
            // Positions that differ only in nullability keep the tuple shape.
            (Self::Tuple(a), Self::Tuple(b))
                if a.len() == b.len()
                    && a.iter().zip(&b).all(|(a, b)| {
                        a.non_nil() == b.non_nil() || *a == Self::Nil || *b == Self::Nil
                    }) =>
            {
                Self::Tuple(a.into_iter().zip(b).map(|(a, b)| a.join(b)).collect())
            }
            (a, b) if a == b => a,
            _ => Self::Dynamic,
        }
    }
}

fn structurally_cloneable(ty: &Type) -> bool {
    match ty {
        Type::Primitive(_, _) | Type::Nil => true,
        Type::Array(element) | Type::Nullable(element) => structurally_cloneable(element),
        Type::Tuple(items) => items.iter().all(structurally_cloneable),
        _ => false,
    }
}

fn compatible_eq_hash_traits(checker: &Checker<'_>, left: &str, right: &str) -> bool {
    let Some(left) = checker.traits.get(left) else {
        return false;
    };
    let Some(right) = checker.traits.get(right) else {
        return false;
    };
    let is_equality = |declaration: &TraitDecl| {
        declaration.type_params.len() == 1
            && declaration.methods.len() == 1
            && declaration.methods[0].name == "equals"
    };
    let is_hashable = |declaration: &TraitDecl| {
        declaration.type_params.is_empty()
            && declaration.methods.len() == 2
            && declaration
                .methods
                .iter()
                .any(|method| method.name == "hash_code")
            && declaration
                .methods
                .iter()
                .any(|method| method.name == "equals")
    };
    (is_equality(left) && is_hashable(right)) || (is_hashable(left) && is_equality(right))
}

fn equivalent_equals_body(left: &FunctionDecl, right: &FunctionDecl) -> bool {
    left.body.stmts.is_empty()
        && right.body.stmts.is_empty()
        && left
            .body
            .tail
            .as_deref()
            .zip(right.body.tail.as_deref())
            .is_some_and(|(left, right)| equivalent_expression(left, right))
}

fn equivalent_expression(left: &Expr, right: &Expr) -> bool {
    match (left.node(), right.node()) {
        (Expr::Int(left), Expr::Int(right)) => left == right,
        (Expr::Float(left), Expr::Float(right)) => left.to_bits() == right.to_bits(),
        (Expr::Char(left), Expr::Char(right)) => left == right,
        (Expr::Str(left), Expr::Str(right)) => left == right,
        (Expr::Bytes(left), Expr::Bytes(right)) => left == right,
        (Expr::Bool(left), Expr::Bool(right)) => left == right,
        (Expr::Nil, Expr::Nil) => true,
        (Expr::TypeLiteral(left), Expr::TypeLiteral(right)) => left == right,
        (Expr::Ident(left), Expr::Ident(right)) => left == right,
        (Expr::Index(left_a, left_b), Expr::Index(right_a, right_b))
        | (Expr::Binary(_, left_a, left_b), Expr::Binary(_, right_a, right_b)) => {
            let same_operator = match (left.node(), right.node()) {
                (Expr::Binary(left, ..), Expr::Binary(right, ..)) => left == right,
                (Expr::Index(..), Expr::Index(..)) => true,
                _ => false,
            };
            same_operator
                && equivalent_expression(left_a, right_a)
                && equivalent_expression(left_b, right_b)
        }
        (Expr::Unary(left_op, left), Expr::Unary(right_op, right)) => {
            left_op == right_op && equivalent_expression(left, right)
        }
        (Expr::Propagate(left), Expr::Propagate(right)) => equivalent_expression(left, right),
        (Expr::Array(left), Expr::Array(right)) | (Expr::Tuple(left), Expr::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| equivalent_expression(left, right))
        }
        (Expr::Call(left_name, left_args), Expr::Call(right_name, right_args)) => {
            left_name == right_name && equivalent_arguments(left_args, right_args)
        }
        (
            Expr::SpecializedCall(left_name, left_types, left_args),
            Expr::SpecializedCall(right_name, right_types, right_args),
        ) => {
            left_name == right_name
                && left_types == right_types
                && equivalent_arguments(left_args, right_args)
        }
        (
            Expr::AssociatedCall {
                target: left_target,
                method: left_method,
                type_arguments: left_types,
                args: left_args,
            },
            Expr::AssociatedCall {
                target: right_target,
                method: right_method,
                type_arguments: right_types,
                args: right_args,
            },
        ) => {
            left_target == right_target
                && left_method == right_method
                && left_types == right_types
                && equivalent_arguments(left_args, right_args)
        }
        (
            Expr::MethodCall(left_recv, left_name, left_args),
            Expr::MethodCall(right_recv, right_name, right_args),
        ) => {
            left_name == right_name
                && equivalent_expression(left_recv, right_recv)
                && left_args.len() == right_args.len()
                && left_args
                    .iter()
                    .zip(right_args)
                    .all(|(left, right)| equivalent_expression(left, right))
        }
        (Expr::Record(left), Expr::Record(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((left_name, left), (right_name, right))| {
                        left_name == right_name && equivalent_expression(left, right)
                    })
        }
        _ => false,
    }
}

fn equivalent_arguments(left: &[CallArgument], right: &[CallArgument]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.name == right.name && equivalent_expression(&left.value, &right.value)
        })
}

fn c_record_kind(facts: &mut crate::mutability::Facts, id: usize, ty: Type) {
    use crate::mutability::ValueKind;
    facts.values.insert(
        id,
        match ty {
            Type::Poison => ValueKind::Invalid,
            Type::Never => ValueKind::Never,
            Type::Primitive(_, _) | Type::Nil | Type::Handle | Type::ErrorValue => {
                ValueKind::Scalar
            }
            Type::Nullable(inner) if *inner == Type::Handle => ValueKind::Scalar,
            _ => ValueKind::Reference,
        },
    );
}

#[derive(Clone)]
struct Binding {
    ty: Type,
    mutable: bool,
    // Unannotated var constraints: unknown writes require a conservative dynamic
    // result, while contradictory known writes remain type errors.
    inferred: Option<(usize, Type)>,
    id: usize,
    error_only: bool,
}

/// Facts known on one normal control-flow path. Normal-value refinement and
/// Error exclusion are independent because a value can carry either, both, or
/// neither state at runtime.
#[derive(Clone, Default)]
struct FlowFacts {
    normal_types: HashMap<usize, Type>,
    error_excluded: HashSet<usize>,
}

impl FlowFacts {
    fn join(states: impl IntoIterator<Item = Self>) -> Self {
        let mut states = states.into_iter();
        let Some(mut joined) = states.next() else {
            return Self::default();
        };
        for state in states {
            joined
                .normal_types
                .retain(|id, ty| state.normal_types.get(id).is_some_and(|other| other == ty));
            joined
                .error_excluded
                .retain(|id| state.error_excluded.contains(id));
        }
        joined
    }

    fn forget(&mut self, id: usize) {
        self.normal_types.remove(&id);
        self.error_excluded.remove(&id);
    }
}

#[derive(Default)]
struct Scope {
    bindings: HashMap<String, Binding>,
    imports: HashMap<String, String>,
    modules: HashMap<String, String>,
}

struct Checker<'a> {
    functions: HashMap<String, &'a FunctionDecl>,
    traits: HashMap<String, &'a TraitDecl>,
    constructors: HashMap<String, (usize, &'a [StructField])>,
    struct_parameters: HashMap<String, Vec<TypeParameter>>,
    struct_fields: Vec<&'a [StructField]>,
    struct_owners: Vec<Option<String>>,
    enums: HashMap<String, usize>,
    variants: HashMap<String, (usize, &'a [StructField])>,
    enum_variants: Vec<Vec<String>>,
    globals: HashMap<String, Binding>,
    scopes: Vec<Scope>,
    module: Option<String>,
    owner: Option<String>,
    loops: usize,
    /// Facts at each reachable `break` of the enclosing loops.
    loop_breaks: Vec<Vec<FlowFacts>>,
    normal_reachable: bool,
    flow_facts: FlowFacts,
    /// Rebinding summary of the function being checked.
    writes: Writes,
    /// Facts of the last condition evaluated with distinct true and false exits.
    branch_flows: Option<(usize, FlowFacts, FlowFacts)>,
    /// The expected type of a constructor call, keyed by its expression.
    result_hint: Option<(usize, Type)>,
    /// A nullable expected type of a literal or branching expression, keyed by
    /// its expression, so that its branches build the declared type.
    value_hint: Option<(usize, Type)>,
    /// Branching expressions whose value is never used.
    discarded: HashSet<usize>,
    literal_types: HashMap<usize, TypeAnnotation>,
    in_cleanup: bool,
    in_function: bool,
    in_async: bool,
    in_comptime: bool,
    in_comptime_initializer: bool,
    result_type: Option<TypeAnnotation>,
    dynamic_bindings: std::collections::HashSet<usize>,
    unresolved_empty_arrays: HashMap<usize, (String, SourceSpan)>,
    next_binding: usize,
    diagnostics: Vec<Diagnostic>,
    location: Option<SourceSpan>,
    access_facts: crate::mutability::Facts,
    extension_methods: HashMap<(Type, String), &'a ExtensionMethod>,
    extension_method_traits: HashMap<(Type, String), Option<&'a str>>,
    generic_extension_methods: Vec<(&'a ExtendDecl, &'a ExtensionMethod)>,
    associated_extension_methods: Vec<(&'a ExtendDecl, &'a ExtensionMethod)>,
    trait_implementations: std::collections::HashSet<(Type, String, Vec<Type>)>,
    generic_trait_implementations: Vec<&'a ExtendDecl>,
    trait_coherence_work: usize,
    current_function: Option<String>,
    method_calls: HashMap<CallSite, String>,
    iteration_calls: HashMap<CallSite, Vec<Option<IterationMethods>>>,
    call_specializations: HashMap<CallSite, String>,
    struct_call_specializations: HashMap<CallSite, String>,
    specializations: Vec<Specialization>,
    uint_literals: HashSet<usize>,
    uint_expressions: HashSet<usize>,
    bigint_literals: HashSet<usize>,
    bigint_range_expressions: HashSet<usize>,
    specialization_names: HashMap<(String, Vec<Type>), String>,
    struct_specializations: Vec<StructSpecialization>,
    struct_specialization_names: HashMap<(String, Vec<Type>), String>,
}

pub(crate) type CallSite = (Option<String>, usize);

#[derive(Clone)]
pub(crate) struct Specialization {
    pub template: String,
    pub name: String,
    pub arguments: HashMap<String, TypeAnnotation>,
}

#[derive(Clone)]
pub(crate) struct StructSpecialization {
    pub template: String,
    pub name: String,
    pub arguments: HashMap<String, TypeAnnotation>,
}

pub(crate) struct CheckResult {
    pub diagnostics: Vec<Diagnostic>,
    pub method_calls: HashMap<CallSite, String>,
    pub iteration_calls: HashMap<CallSite, Vec<Option<IterationMethods>>>,
    pub call_specializations: HashMap<CallSite, String>,
    pub struct_call_specializations: HashMap<CallSite, String>,
    pub specializations: Vec<Specialization>,
    pub struct_specializations: Vec<StructSpecialization>,
    pub uint_literals: HashSet<usize>,
    pub uint_expressions: HashSet<usize>,
    pub bigint_literals: HashSet<usize>,
    pub bigint_range_expressions: HashSet<usize>,
    /// Literals that must be built with a nullable element type taken from
    /// their context.
    pub literal_types: HashMap<usize, TypeAnnotation>,
    /// Access facts kept by [`check_deferring_effects`] for a later
    /// [`check_deferred_effects`] call.
    deferred_effects: Option<crate::mutability::Facts>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Effects {
    Check,
    Skip,
    Defer,
}

#[derive(Clone, Debug)]
pub(crate) struct IterationMethods {
    pub into_iter: String,
    pub next: String,
    pub current: String,
    pub into_iter_template: String,
    pub next_template: String,
    pub current_template: String,
}

pub(crate) fn expression_site(function: Option<&str>, expr: &Expr) -> CallSite {
    (function.map(str::to_owned), expr as *const Expr as usize)
}

pub(crate) fn statement_site(function: Option<&str>, stmt: &Stmt) -> CallSite {
    (function.map(str::to_owned), stmt as *const Stmt as usize)
}

pub fn check(program: &Program, function_modules: &HashMap<String, String>) -> Vec<Diagnostic> {
    check_with_methods(program, function_modules).diagnostics
}

pub(crate) fn check_with_methods(
    program: &Program,
    function_modules: &HashMap<String, String>,
) -> CheckResult {
    check_with_methods_and_names(program, function_modules, &BTreeMap::new())
}

pub(crate) fn check_with_methods_and_names(
    program: &Program,
    function_modules: &HashMap<String, String>,
    names: &BTreeMap<String, String>,
) -> CheckResult {
    check_program(program, function_modules, names, Effects::Check)
}

/// Checks the program without the interprocedural mutability pass and keeps its
/// access facts. Generic specialization uses this for every expansion round and
/// runs the mutability pass with [`check_deferred_effects`] only on the final
/// program or on a round that already failed, so the whole-program fixed point
/// does not repeat for each round.
pub(crate) fn check_deferring_effects(
    program: &Program,
    function_modules: &HashMap<String, String>,
    names: &BTreeMap<String, String>,
) -> CheckResult {
    check_program(program, function_modules, names, Effects::Defer)
}

/// Runs the mutability pass deferred by [`check_deferring_effects`] for the
/// same program and appends its diagnostics.
pub(crate) fn check_deferred_effects(program: &Program, result: &mut CheckResult) {
    if let Some(facts) = result.deferred_effects.take() {
        result
            .diagnostics
            .extend(crate::mutability::check(program, &facts));
    }
}

/// Resolves calls and specializations for clients that discard diagnostics.
/// The interprocedural mutability pass only produces diagnostics, so it is
/// skipped here.
pub(crate) fn resolve_methods_and_names(
    program: &Program,
    function_modules: &HashMap<String, String>,
    names: &BTreeMap<String, String>,
) -> CheckResult {
    check_program(program, function_modules, names, Effects::Skip)
}

fn check_program(
    program: &Program,
    function_modules: &HashMap<String, String>,
    names: &BTreeMap<String, String>,
    effects: Effects,
) -> CheckResult {
    let mut c = Checker {
        functions: HashMap::new(),
        traits: HashMap::new(),
        constructors: HashMap::new(),
        struct_parameters: HashMap::new(),
        struct_fields: Vec::new(),
        struct_owners: Vec::new(),
        enums: HashMap::new(),
        variants: HashMap::new(),
        enum_variants: Vec::new(),
        globals: HashMap::new(),
        scopes: vec![Scope::default()],
        module: None,
        owner: None,
        loops: 0,
        loop_breaks: Vec::new(),
        normal_reachable: true,
        flow_facts: FlowFacts::default(),
        writes: Writes::default(),
        branch_flows: None,
        result_hint: None,
        value_hint: None,
        discarded: HashSet::new(),
        literal_types: HashMap::new(),
        in_cleanup: false,
        in_function: false,
        in_async: false,
        in_comptime: false,
        in_comptime_initializer: false,
        result_type: None,
        dynamic_bindings: Default::default(),
        unresolved_empty_arrays: HashMap::new(),
        next_binding: 1,
        diagnostics: Vec::new(),
        location: None,
        access_facts: Default::default(),
        extension_methods: HashMap::new(),
        extension_method_traits: HashMap::new(),
        generic_extension_methods: Vec::new(),
        associated_extension_methods: Vec::new(),
        trait_implementations: Default::default(),
        generic_trait_implementations: Vec::new(),
        trait_coherence_work: 0,
        current_function: None,
        method_calls: HashMap::new(),
        iteration_calls: HashMap::new(),
        call_specializations: HashMap::new(),
        struct_call_specializations: HashMap::new(),
        specializations: Vec::new(),
        uint_literals: HashSet::new(),
        uint_expressions: HashSet::new(),
        bigint_literals: HashSet::new(),
        bigint_range_expressions: HashSet::new(),
        specialization_names: HashMap::new(),
        struct_specializations: Vec::new(),
        struct_specialization_names: HashMap::new(),
    };
    c.access_facts.symbol_names = names
        .iter()
        .map(|(symbol, name)| (symbol.clone(), name.clone()))
        .collect();
    for item in &program.items {
        match item {
            Item::Invalid {
                binding: Some(name),
            } => {
                let id = c.next_binding;
                c.next_binding += 1;
                c.globals.insert(
                    name.clone(),
                    Binding {
                        id,
                        error_only: false,
                        inferred: None,
                        ty: Type::Poison,
                        mutable: true,
                    },
                );
            }
            Item::Function(f) if c.functions.insert(f.name.clone(), f).is_some() => {
                c.error(
                    "E1001",
                    format!("function `{}` defined more than once", f.name),
                    f.body
                        .stmts
                        .first()
                        .and_then(Stmt::span)
                        .or_else(|| f.body.tail.as_deref().and_then(Expr::span))
                        .or(f.result_span.as_ref()),
                );
            }
            Item::Function(_) => {}
            Item::Trait(declaration)
                if c.traits
                    .insert(declaration.name.clone(), declaration)
                    .is_some() =>
            {
                c.error(
                    "E1001",
                    format!("trait `{}` defined more than once", declaration.name),
                    None,
                );
            }
            Item::Trait(_) => {}
            Item::StructDecl {
                name,
                type_params,
                fields,
                owner,
                ..
            } => {
                let id = c.struct_fields.len();
                c.struct_fields.push(fields);
                c.struct_owners.push(owner.clone());
                c.constructors.insert(name.clone(), (id, fields));
                c.struct_parameters
                    .insert(name.clone(), type_params.clone());
            }
            Item::EnumDecl { name, variants, .. } => {
                let id = c.enum_variants.len();
                c.enums.insert(name.clone(), id);
                let names = variants
                    .iter()
                    .map(|v| format!("{name}::{}", v.name))
                    .collect();
                c.enum_variants.push(names);
                for v in variants {
                    c.variants
                        .insert(format!("{name}::{}", v.name), (id, &v.payload));
                }
            }
            _ => {}
        }
    }
    // Predeclare globals so forward references can be checked without execution.
    // This runs after every struct and enum is registered: an annotation resolved
    // earlier would stay an unresolved name, which does not match the nominal type.
    for item in &program.items {
        if let Item::GlobalConst { name, ty, .. }
        | Item::GlobalVar { name, ty, .. }
        | Item::Comptime { name, ty, .. } = item
        {
            let ty = ty.as_ref().map_or(Type::Dynamic, |t| c.annotation_type(t));
            let id = c.next_binding;
            c.next_binding += 1;
            c.globals.insert(
                name.clone(),
                Binding {
                    id,
                    error_only: false,
                    inferred: None,
                    ty,
                    mutable: matches!(item, Item::GlobalVar { .. }),
                },
            );
        }
    }
    for item in &program.items {
        let Item::Extend(extension) = item else {
            continue;
        };
        let target = c.annotation_type(&extension.target);
        if !extension.type_params.is_empty()
            && !matches!(target, Type::Struct(_, _, _) | Type::Array(_))
        {
            c.error(
                "E2001",
                "generic extensions currently require a generic struct or array target",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
        }
        for parameter in &extension.type_params {
            if !extension_target_uses_parameter(&extension.target, &parameter.name) {
                c.error(
                    "E2001",
                    format!(
                        "generic extension parameter `{}` must appear in its target type",
                        parameter.name
                    ),
                    extension
                        .methods
                        .first()
                        .and_then(|method| method.function.result_span.as_ref()),
                );
            }
        }
        if extension.trait_name.is_none()
            && !extension.comptime
            && !matches!(
                target,
                Type::Struct(_, _, _) | Type::Enum(_, _) | Type::Array(_)
            )
        {
            c.error(
                "E2001",
                format!(
                    "`extend` requires a named struct, enum, or generic array type; use `@comptime extend` for `{}`",
                    extension.target.display()
                ),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
        }
        if let Some(trait_name) = &extension.trait_name {
            c.register_trait_implementation(&target, trait_name, extension);
        }
        for method in &extension.methods {
            let function = &method.function;
            if let Some(receiver) = function.params.first().filter(|p| p.name == "self")
                && let Some(receiver_type) = &receiver.ty
                && !type_compatible(&target, &c.annotation_type(receiver_type))
            {
                c.error(
                    "E2001",
                    format!(
                        "extension method `{}()` receiver must have type {}, got {}",
                        method.name,
                        extension.target.display(),
                        receiver_type.display()
                    ),
                    function
                        .body
                        .stmts
                        .first()
                        .and_then(Stmt::span)
                        .or_else(|| function.body.tail.as_deref().and_then(Expr::span)),
                );
            }
            let has_receiver = function.params.first().is_some_and(|p| p.name == "self");
            let duplicate = if extension.type_params.is_empty() && has_receiver {
                let key = (target.clone(), method.name.clone());
                let previous = c.extension_methods.get(&key).copied();
                let previous_trait = c.extension_method_traits.get(&key).copied().flatten();
                let equivalent = previous.is_some_and(|previous| {
                    method.name == "equals"
                        && previous.function.params == function.params
                        && previous.function.result_type == function.result_type
                        && equivalent_equals_body(&previous.function, function)
                        && previous_trait
                            .zip(extension.trait_name.as_deref())
                            .is_some_and(|(left, right)| compatible_eq_hash_traits(&c, left, right))
                });
                if previous.is_none() {
                    c.extension_method_traits
                        .entry(key.clone())
                        .or_insert(extension.trait_name.as_deref());
                    c.extension_methods.entry(key).or_insert(method);
                    false
                } else {
                    !equivalent
                }
            } else if !extension.type_params.is_empty() && has_receiver {
                let duplicate = c
                    .generic_extension_methods
                    .iter()
                    .any(|(existing, previous)| {
                        previous.name == method.name
                            && existing.target.canonical() == extension.target.canonical()
                    });
                if !duplicate {
                    c.generic_extension_methods.push((extension, method));
                }
                duplicate
            } else {
                let duplicate =
                    c.associated_extension_methods
                        .iter()
                        .any(|(existing, previous)| {
                            previous.name == method.name
                                && existing.target.canonical() == extension.target.canonical()
                        });
                if !duplicate {
                    c.associated_extension_methods.push((extension, method));
                }
                duplicate
            };
            if duplicate {
                c.error(
                    "E1001",
                    format!(
                        "extension method `{}()` is defined more than once for `{}`",
                        method.name,
                        extension.target.display()
                    ),
                    function.result_span.as_ref(),
                );
            }
            c.functions.insert(function.name.clone(), function);
        }
    }
    for item in &program.items {
        if let Item::Use(decl) = item {
            c.import(decl);
        }
    }
    for item in &program.items {
        if let Item::GlobalConst {
            name, ty, value, ..
        }
        | Item::GlobalVar {
            name, ty, value, ..
        }
        | Item::Comptime {
            name, ty, value, ..
        } = item
        {
            c.module = name.rsplit_once("::").map(|(m, _)| m.to_owned());
            let expected_type = ty.as_ref().map(|annotation| c.annotation_type(annotation));
            c.writes = Writes::default();
            c.writes.expr(value);
            c.flow_facts = FlowFacts::default();
            let actual = c.expr_with_expected_result(value, expected_type.as_ref());
            if ty.is_none() && contains_unresolved_empty_array(&actual) {
                c.error(
                    "E2001",
                    format!("cannot infer array element type for `{name}`; add a `T[]` annotation"),
                    value.span(),
                );
            }
            let valid = ty.clone().is_none_or(|expected| {
                c.assignable(expected, actual.clone(), value.span(), &format!("`{name}`"))
            });
            let binding = c.globals.get_mut(name).expect("predeclared global");
            if !valid || actual == Type::Poison {
                binding.ty = Type::Poison;
            } else if !binding.mutable && ty.is_none() {
                binding.ty = actual;
            }
        }
    }
    for item in &program.items {
        match item {
            Item::Function(f) => c.check_function(f, function_modules),
            Item::Trait(_) => {}
            Item::Extend(extension) => {
                for method in &extension.methods {
                    c.check_function(&method.function, function_modules);
                }
            }
            Item::StructDecl {
                name,
                type_params,
                fields,
                ..
            } => {
                c.module = name.rsplit_once("::").map(|(m, _)| m.to_owned());
                if type_params.is_empty() {
                    c.defaults(fields);
                }
            }
            _ => {}
        }
    }
    if effects == Effects::Check {
        c.diagnostics
            .extend(crate::mutability::check(program, &c.access_facts));
    }
    name_nominal_types(program, names, &mut c.diagnostics);
    let deferred_effects = (effects == Effects::Defer).then_some(c.access_facts);
    CheckResult {
        diagnostics: c.diagnostics,
        method_calls: c.method_calls,
        iteration_calls: c.iteration_calls,
        call_specializations: c.call_specializations,
        struct_call_specializations: c.struct_call_specializations,
        specializations: c.specializations,
        struct_specializations: c.struct_specializations,
        uint_literals: c.uint_literals,
        uint_expressions: c.uint_expressions,
        bigint_literals: c.bigint_literals,
        bigint_range_expressions: c.bigint_range_expressions,
        literal_types: c.literal_types,
        deferred_effects,
    }
}

/// Spells the structs and enums printed by `Type::display` with their source
/// names, keeping the module path only where two types share a name.
fn name_nominal_types(
    program: &Program,
    names: &BTreeMap<String, String>,
    diagnostics: &mut [Diagnostic],
) {
    if diagnostics.is_empty() {
        return;
    }
    let short = |name: &str| name.rsplit("::").next().unwrap_or(name).to_owned();
    let types: Vec<_> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::StructDecl { name, .. } | Item::EnumDecl { name, .. } => {
                names.get_key_value(name)
            }
            _ => None,
        })
        .collect();
    let mut declared = HashMap::<String, usize>::new();
    for (_, source) in &types {
        *declared.entry(short(source)).or_default() += 1;
    }
    let mut replacements: Vec<_> = types
        .into_iter()
        .map(|(symbol, source)| {
            let name = short(source);
            let name = if declared[&name] == 1 {
                name
            } else {
                source.clone()
            };
            (format!("{NOMINAL_MARK}{symbol}"), name)
        })
        .collect();
    // Longest first, so that `$symbol1` never rewrites part of `$symbol10`.
    replacements.sort_by_key(|(symbol, _)| std::cmp::Reverse(symbol.len()));
    let rename = |text: &mut String| {
        if !text.contains(NOMINAL_MARK) {
            return;
        }
        for (symbol, name) in &replacements {
            *text = text.replace(symbol, name);
        }
        // Types without a resolved source name already read as written.
        text.retain(|character| character != NOMINAL_MARK);
    };
    for diagnostic in diagnostics {
        rename(&mut diagnostic.message);
        diagnostic
            .notes
            .iter_mut()
            .chain(&mut diagnostic.helps)
            .for_each(rename);
    }
}

impl<'a> Checker<'a> {
    fn owns_struct(&self, id: usize) -> bool {
        self.owner.as_ref().is_some_and(|owner| {
            self.struct_owners
                .get(id)
                .is_some_and(|struct_owner| struct_owner.as_ref() == Some(owner))
        })
    }

    fn implements_trait(&self, ty: &Type, trait_name: &str, arguments: &[Type]) -> bool {
        self.implements_trait_inner(ty, trait_name, arguments, &mut HashSet::new())
    }

    fn standard_trait_symbol(&self, source_name: &str) -> Option<String> {
        self.access_facts
            .symbol_names
            .iter()
            .find_map(|(symbol, source)| (source == source_name).then(|| symbol.clone()))
    }

    fn implemented_trait_argument_candidates(&self, ty: &Type, trait_name: &str) -> Vec<Vec<Type>> {
        let mut matches = self
            .trait_implementations
            .iter()
            .filter(|&(target, name, _)| target == ty && name == trait_name)
            .map(|(_, _, arguments)| arguments.clone())
            .collect::<Vec<_>>();
        for extension in &self.generic_trait_implementations {
            let mut visiting = HashSet::new();
            if let Some(arguments) = self.generic_trait_implementation_arguments(
                extension,
                ty,
                trait_name,
                None,
                &mut visiting,
            ) {
                matches.push(arguments);
            }
        }
        let mut seen = HashSet::new();
        matches.retain(|arguments| seen.insert(arguments.clone()));
        matches
    }

    fn generic_trait_implementation_arguments(
        &self,
        extension: &ExtendDecl,
        target: &Type,
        trait_name: &str,
        requested_arguments: Option<&[Type]>,
        visiting: &mut HashSet<(Type, String, Vec<Type>)>,
    ) -> Option<Vec<Type>> {
        if extension.trait_name.as_deref() != Some(trait_name)
            || requested_arguments
                .is_some_and(|arguments| arguments.len() != extension.trait_arguments.len())
        {
            return None;
        }
        let parameters: HashSet<_> = extension
            .type_params
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        let mut substitutions = HashMap::new();
        if !match_extension_type(&extension.target, target, &parameters, &mut substitutions) {
            return None;
        }
        if let Some(arguments) = requested_arguments
            && !extension
                .trait_arguments
                .iter()
                .zip(arguments)
                .all(|(pattern, actual)| {
                    match_extension_type(pattern, actual, &parameters, &mut substitutions)
                })
        {
            return None;
        }
        if extension
            .type_params
            .iter()
            .any(|parameter| !substitutions.contains_key(&parameter.name))
        {
            return None;
        }
        let arguments = extension
            .trait_arguments
            .iter()
            .map(|argument| self.concrete_annotation(argument, &substitutions, target))
            .collect::<Option<Vec<_>>>()?;
        let query = (target.clone(), trait_name.to_owned(), arguments.clone());
        if !visiting.insert(query.clone()) {
            return None;
        }
        let valid = extension.type_params.iter().all(|parameter| {
            let Some(actual) = substitutions.get(&parameter.name) else {
                return false;
            };
            parameter.bounds.iter().all(|bound| {
                let Some(bound_arguments) = bound
                    .arguments
                    .iter()
                    .map(|argument| self.concrete_annotation(argument, &substitutions, target))
                    .collect::<Option<Vec<_>>>()
                else {
                    return false;
                };
                self.implements_trait_inner(actual, &bound.name, &bound_arguments, visiting)
            })
        });
        visiting.remove(&query);
        valid.then_some(arguments)
    }

    fn check_iteration_method(
        &mut self,
        receiver_type: &Type,
        method: &str,
        span: Option<SourceSpan>,
    ) -> Option<(Type, String, String)> {
        let binding_name = format!("$iteration_receiver_{}", self.next_binding);
        let binding_id = self.next_binding;
        self.next_binding += 1;
        self.scopes.push(Scope::default());
        self.define(
            &binding_name,
            Binding {
                id: binding_id,
                error_only: false,
                inferred: None,
                ty: receiver_type.clone(),
                mutable: true,
            },
            span.as_ref(),
        );

        let call = Expr::MethodCall(
            Box::new(Expr::Ident(binding_name)),
            method.to_owned(),
            Vec::new(),
        );
        let previous_flow = self.flow_facts.clone();
        let result = self.expr(&call);
        self.flow_facts = previous_flow;
        let site = expression_site(self.current_function.as_deref(), &call);
        let resolved = self
            .call_specializations
            .get(&site)
            .or_else(|| self.method_calls.get(&site))
            .cloned();
        let template = self
            .access_facts
            .calls
            .remove(&crate::mutability::expr_id(&call));
        self.access_facts
            .specializations
            .remove(&crate::mutability::expr_id(&call));
        self.scopes.pop();

        resolved
            .filter(|_| result != Type::Poison)
            .map(|name| (result, template.unwrap_or_else(|| name.clone()), name))
    }

    fn custom_iteration(
        &mut self,
        collection: &Type,
        span: Option<SourceSpan>,
    ) -> Option<(Type, IterationMethods)> {
        let Some(into_iterator_trait) = self.standard_trait_symbol("iter::IntoIterator") else {
            self.error(
                "E2001",
                format!(
                    "for-in over {} requires the `iter` module and an `IntoIterator` implementation",
                    collection.display()
                ),
                span.as_ref(),
            );
            return None;
        };
        let implemented =
            self.implemented_trait_argument_candidates(collection, &into_iterator_trait);
        let arguments = match implemented.as_slice() {
            [arguments] => arguments,
            [] => {
                self.error(
                    "E2001",
                    format!(
                        "type {} does not implement `iter::IntoIterator[Iter, Item]`",
                        collection.display()
                    ),
                    span.as_ref(),
                );
                return None;
            }
            _ => {
                self.error(
                    "E2001",
                    format!(
                        "type {} has multiple applicable `iter::IntoIterator` implementations",
                        collection.display()
                    ),
                    span.as_ref(),
                );
                return None;
            }
        };
        let [iterator_type, item_type] = arguments.as_slice() else {
            self.error(
                "E2001",
                "`iter::IntoIterator` must have `Iter` and `Item` type arguments",
                span.as_ref(),
            );
            return None;
        };
        let Some(iterator_trait) = self.standard_trait_symbol("iter::Iterator") else {
            self.error(
                "E2001",
                "the `iter::Iterator` trait is unavailable; import the `iter` module",
                span.as_ref(),
            );
            return None;
        };
        if !self.implements_trait(
            iterator_type,
            &iterator_trait,
            std::slice::from_ref(item_type),
        ) {
            self.error(
                "E2001",
                format!(
                    "iterator type {} does not implement `iter::Iterator[{}]`",
                    iterator_type.display(),
                    item_type.display()
                ),
                span.as_ref(),
            );
            return None;
        }

        let (actual_iterator_type, into_iter_template, into_iter) =
            self.check_iteration_method(collection, "into_iter", span.clone())?;
        if !type_compatible(iterator_type, &actual_iterator_type) {
            self.error(
                "E2001",
                format!(
                    "`into_iter()` returns {}, expected {}",
                    actual_iterator_type.display(),
                    iterator_type.display()
                ),
                span.as_ref(),
            );
            return None;
        }

        let (next_type, next_template, next) =
            self.check_iteration_method(iterator_type, "next", span.clone())?;
        if !type_compatible(&Type::concrete(VarType::Bool), &next_type) {
            self.error(
                "E2001",
                format!("`next()` returns {}, expected bool", next_type.display()),
                span.as_ref(),
            );
            return None;
        }

        let (current_type, current_template, current) =
            self.check_iteration_method(iterator_type, "current", span.clone())?;
        if !type_compatible(item_type, &current_type) {
            self.error(
                "E2001",
                format!(
                    "`current()` returns {}, expected {}",
                    current_type.display(),
                    item_type.display()
                ),
                span.as_ref(),
            );
            return None;
        }

        Some((
            item_type.clone(),
            IterationMethods {
                into_iter,
                next,
                current,
                into_iter_template,
                next_template,
                current_template,
            },
        ))
    }

    fn implements_trait_inner(
        &self,
        ty: &Type,
        trait_name: &str,
        arguments: &[Type],
        visiting: &mut HashSet<(Type, String, Vec<Type>)>,
    ) -> bool {
        if self.trait_implementations.contains(&(
            ty.clone(),
            trait_name.to_owned(),
            arguments.to_vec(),
        )) || (trait_name == CLONE_TRAIT && arguments.is_empty() && structurally_cloneable(ty))
        {
            return true;
        }

        self.generic_trait_implementations.iter().any(|extension| {
            self.generic_trait_implementation_arguments(
                extension,
                ty,
                trait_name,
                Some(arguments),
                visiting,
            )
            .is_some()
        })
    }

    fn concrete_annotation(
        &self,
        annotation: &TypeAnnotation,
        substitutions: &HashMap<String, Type>,
        self_type: &Type,
    ) -> Option<Type> {
        let ty = match &annotation.kind {
            TypeKind::Primitive(base) if *base == VarType::Nil && !annotation.nilable => Type::Nil,
            TypeKind::Primitive(base) => Type::Primitive(*base, annotation.nilable),
            TypeKind::Bytes => Type::Bytes,
            TypeKind::TypeParameter(name) => substitutions.get(name)?.clone(),
            TypeKind::SelfType => self_type.clone(),
            TypeKind::Array(element) => Type::Array(Box::new(self.concrete_annotation(
                element,
                substitutions,
                self_type,
            )?)),
            TypeKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.concrete_annotation(element, substitutions, self_type))
                    .collect::<Option<Vec<_>>>()?,
            ),
            TypeKind::Function { parameters, result } => Type::Function(
                parameters
                    .iter()
                    .map(|parameter| self.concrete_annotation(parameter, substitutions, self_type))
                    .collect::<Option<Vec<_>>>()?,
                Box::new(self.concrete_annotation(result, substitutions, self_type)?),
            ),
            TypeKind::Applied { source, arguments } if source == "Task" => {
                if arguments.len() != 1 {
                    return None;
                }
                Type::Task(Box::new(self.concrete_annotation(
                    &arguments[0],
                    substitutions,
                    self_type,
                )?))
            }
            TypeKind::Applied { source, arguments } => {
                let name = self
                    .constructors
                    .keys()
                    .find(|name| *name == source || name.ends_with(&format!("::{source}")))?;
                let (id, _) = self.constructors.get(name)?;
                Type::Struct(
                    *id,
                    name.clone(),
                    arguments
                        .iter()
                        .map(|argument| {
                            self.concrete_annotation(argument, substitutions, self_type)
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } if source == "Task" || symbol == "Task" => {
                if arguments.len() != 1 {
                    return None;
                }
                Type::Task(Box::new(self.concrete_annotation(
                    &arguments[0],
                    substitutions,
                    self_type,
                )?))
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } => {
                let name = if self.constructors.contains_key(symbol) {
                    symbol
                } else {
                    self.constructors
                        .keys()
                        .find(|name| *name == source || name.ends_with(&format!("::{source}")))?
                };
                let (id, _) = self.constructors.get(name)?;
                Type::Struct(
                    *id,
                    name.clone(),
                    arguments
                        .iter()
                        .map(|argument| {
                            self.concrete_annotation(argument, substitutions, self_type)
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            TypeKind::Named(name) | TypeKind::ResolvedName { source: name, .. } => {
                if let Some((id, _)) = self.constructors.get(name) {
                    Type::Struct(*id, name.clone(), Vec::new())
                } else if let Some(id) = self.enums.get(name) {
                    Type::Enum(*id, name.clone())
                } else {
                    match name.as_str() {
                        "int" => Type::concrete(VarType::Int),
                        "uint" => Type::concrete(VarType::UInt),
                        "bigint" => Type::concrete(VarType::BigInt),
                        "float" => Type::concrete(VarType::Float),
                        "bool" => Type::concrete(VarType::Bool),
                        "char" => Type::concrete(VarType::Char),
                        "str" => Type::concrete(VarType::Str),
                        _ => Type::Named(name.clone()),
                    }
                }
            }
        };
        Some(if annotation.nilable {
            ty.nullable()
        } else {
            ty
        })
    }

    fn type_annotation_for(&self, ty: &Type) -> Option<TypeAnnotation> {
        let kind = match ty {
            Type::Nullable(inner) => {
                let mut annotation = self.type_annotation_for(inner)?;
                annotation.nilable = true;
                return Some(annotation);
            }
            Type::Primitive(base, _) => TypeKind::Primitive(*base),
            Type::Nil => TypeKind::Primitive(VarType::Nil),
            Type::Array(element) => TypeKind::Array(Box::new(self.type_annotation_for(element)?)),
            Type::Task(result) => TypeKind::ResolvedApplied {
                source: "Task".to_owned(),
                symbol: "Task".to_owned(),
                arguments: vec![self.type_annotation_for(result)?],
            },
            Type::Tuple(items) => TypeKind::Tuple(
                items
                    .iter()
                    .map(|item| self.type_annotation_for(item))
                    .collect::<Option<Vec<_>>>()?,
            ),
            Type::Function(parameters, result) => TypeKind::Function {
                parameters: parameters
                    .iter()
                    .map(|ty| self.type_annotation_for(ty))
                    .collect::<Option<Vec<_>>>()?,
                result: Box::new(self.type_annotation_for(result)?),
            },
            Type::Struct(_, name, arguments) if !arguments.is_empty() => {
                TypeKind::ResolvedApplied {
                    source: name.clone(),
                    symbol: name.clone(),
                    arguments: arguments
                        .iter()
                        .map(|argument| self.type_annotation_for(argument))
                        .collect::<Option<Vec<_>>>()?,
                }
            }
            Type::Struct(_, name, _) | Type::Enum(_, name) | Type::Named(name) => {
                TypeKind::ResolvedName {
                    source: name.clone(),
                    symbol: name.clone(),
                }
            }
            Type::Bytes => TypeKind::Bytes,
            Type::Dynamic
            | Type::Generic(_)
            | Type::Never
            | Type::EmptyArray
            | Type::StrBuf
            | Type::Handle
            | Type::Record
            | Type::ErrorValue
            | Type::Poison => return None,
        };
        Some(TypeAnnotation {
            kind,
            nilable: matches!(ty, Type::Primitive(_, true)),
        })
    }

    fn charge_trait_coherence(&mut self, amount: usize, span: Option<&SourceSpan>) -> bool {
        let Some(work) = self
            .trait_coherence_work
            .checked_add(amount)
            .filter(|work| *work <= MAX_TRAIT_COHERENCE_WORK)
        else {
            self.error(
                "E4003",
                "trait implementation coherence analysis limit exceeded",
                span,
            );
            return false;
        };
        self.trait_coherence_work = work;
        true
    }

    fn register_trait_implementation(
        &mut self,
        target: &Type,
        trait_name: &str,
        extension: &'a ExtendDecl,
    ) {
        let Some(declaration) = self
            .traits
            .get(trait_name)
            .map(|declaration| (*declaration).clone())
        else {
            // A trait path the resolver already rejected is bound to a poisoned
            // placeholder; its diagnostic was reported, so do not cascade.
            if self
                .globals
                .get(trait_name)
                .is_some_and(|binding| binding.ty == Type::Poison)
            {
                return;
            }
            self.error(
                "E1002",
                format!("unknown trait `{trait_name}`"),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        };
        if matches!(
            target,
            Type::Dynamic | Type::Generic(_) | Type::EmptyArray | Type::Poison
        ) {
            self.error(
                "E2001",
                "trait implementations require a concrete target type",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        if extension.trait_arguments.len() != declaration.type_params.len() {
            self.error(
                "E2001",
                format!(
                    "trait `{trait_name}` expects {} type argument(s), got {}",
                    declaration.type_params.len(),
                    extension.trait_arguments.len()
                ),
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        let generic = !extension.type_params.is_empty();
        let trait_arguments: Vec<_> = extension
            .trait_arguments
            .iter()
            .map(|argument| self.annotation_type(argument))
            .collect();
        if !generic
            && trait_arguments
                .iter()
                .any(|argument| !is_concrete_trait_argument(argument))
        {
            self.error(
                "E2001",
                "trait implementations require concrete trait type arguments",
                extension
                    .methods
                    .first()
                    .and_then(|method| method.function.result_span.as_ref()),
            );
            return;
        }
        let span = extension
            .methods
            .first()
            .and_then(|method| method.function.result_span.as_ref());
        let new_cost = trait_implementation_cost(extension);
        let mut overlaps = false;
        for existing in self.generic_trait_implementations.clone() {
            if existing.trait_name.as_deref() != Some(trait_name)
                || existing.trait_arguments.len() != extension.trait_arguments.len()
            {
                continue;
            }
            let cost = new_cost.saturating_add(trait_implementation_cost(existing));
            if !self.charge_trait_coherence(cost, span) {
                return;
            }
            let matches = if generic {
                generic_trait_implementations_overlap(extension, existing)
            } else {
                generic_trait_implementation_overlaps_exact(
                    existing,
                    target,
                    trait_name,
                    &trait_arguments,
                )
            };
            if matches {
                overlaps = true;
                break;
            }
        }
        if generic && !overlaps {
            let exact_implementations = self
                .trait_implementations
                .iter()
                .filter(|(_, name, _)| name == trait_name)
                .cloned()
                .collect::<Vec<_>>();
            for (existing_target, existing_trait, existing_arguments) in exact_implementations {
                if existing_trait != trait_name
                    || existing_arguments.len() != extension.trait_arguments.len()
                {
                    continue;
                }
                let cost = new_cost.saturating_add(
                    concrete_trait_type_cost(&existing_target).saturating_add(
                        existing_arguments
                            .iter()
                            .map(concrete_trait_type_cost)
                            .sum::<usize>(),
                    ),
                );
                if !self.charge_trait_coherence(cost, span) {
                    return;
                }
                if generic_trait_implementation_overlaps_exact(
                    extension,
                    &existing_target,
                    &existing_trait,
                    &existing_arguments,
                ) {
                    overlaps = true;
                    break;
                }
            }
        }
        if !generic && !overlaps {
            for existing in self.generic_trait_implementations.clone() {
                if existing.trait_name.as_deref() != Some(trait_name)
                    || existing.trait_arguments.len() != trait_arguments.len()
                {
                    continue;
                }
                let cost = trait_implementation_cost(existing).saturating_add(
                    concrete_trait_type_cost(target).saturating_add(
                        trait_arguments
                            .iter()
                            .map(concrete_trait_type_cost)
                            .sum::<usize>(),
                    ),
                );
                if !self.charge_trait_coherence(cost, span) {
                    return;
                }
                if generic_trait_implementation_overlaps_exact(
                    existing,
                    target,
                    trait_name,
                    &trait_arguments,
                ) {
                    overlaps = true;
                    break;
                }
            }
        }
        let duplicate_exact = self
            .trait_implementations
            .iter()
            .any(|(implemented, name, _)| implemented == target && name == trait_name);
        if overlaps || duplicate_exact {
            self.error(
                "E1001",
                format!(
                    "trait `{trait_name}` has overlapping implementations for `{}`; implementation bounds do not make overlaps distinct",
                    extension.target.display(),
                ),
                span,
            );
            return;
        }
        let key = (
            target.clone(),
            trait_name.to_owned(),
            trait_arguments.clone(),
        );
        if !generic {
            self.trait_implementations.insert(key.clone());
        }

        let trait_substitutions: HashMap<_, _> = declaration
            .type_params
            .iter()
            .cloned()
            .zip(extension.trait_arguments.iter().cloned())
            .collect();

        let target_annotation = self
            .type_annotation_for(target)
            .or_else(|| Some(extension.target.clone()));
        let mut implementation_valid = true;
        for required in &declaration.methods {
            let Some(implementation) = extension
                .methods
                .iter()
                .find(|method| method.name == required.name)
            else {
                self.error(
                    "E2001",
                    format!(
                        "implementation of `{trait_name}` for `{}` is missing method `{}()`",
                        target.display(),
                        required.name
                    ),
                    extension
                        .methods
                        .first()
                        .and_then(|method| method.function.result_span.as_ref()),
                );
                implementation_valid = false;
                continue;
            };
            let function = &implementation.function;
            let mut valid = function.params.len() == required.params.len();
            if implementation.mutating_receiver != required.mutating_receiver {
                self.error(
                    "E2001",
                    format!(
                        "mutability of the `self` receiver for `{}()` does not match trait `{trait_name}`",
                        required.name
                    ),
                    function.result_span.as_ref(),
                );
                valid = false;
            }
            if !valid {
                self.error(
                    "E2001",
                    format!(
                        "method `{}()` for trait `{trait_name}` has {} parameter(s), expected {}",
                        required.name,
                        function.params.len(),
                        required.params.len()
                    ),
                    function.result_span.as_ref(),
                );
            }
            if function.comptime {
                self.error(
                    "E4002",
                    "trait methods must be runtime functions",
                    function.result_span.as_ref(),
                );
                valid = false;
            }
            if function.asynchronous != required.asynchronous {
                self.error(
                    "E2001",
                    format!(
                        "async status of method `{}()` does not match trait `{trait_name}`",
                        required.name
                    ),
                    function.result_span.as_ref(),
                );
                valid = false;
            }
            for (index, (expected, actual)) in
                required.params.iter().zip(&function.params).enumerate()
            {
                if expected.name != actual.name {
                    self.error(
                        "E2001",
                        format!(
                            "parameter {} of `{}()` must be named `{}` to match trait `{trait_name}`",
                            index + 1,
                            required.name,
                            expected.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
                if index == 0 {
                    continue;
                }
                let Some(mut expected_type) = expected.ty.clone() else {
                    self.error(
                        "E2001",
                        format!(
                            "trait method `{}()` must type every non-self parameter",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                    continue;
                };
                let Some(actual_type) = actual.ty.as_ref() else {
                    self.error(
                        "E2001",
                        format!(
                            "implementation method `{}()` must type every parameter",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                    continue;
                };
                substitute_trait_type(&mut expected_type, &trait_substitutions);
                if let Some(target_annotation) = &target_annotation {
                    replace_self_type(&mut expected_type, target_annotation);
                }
                let expected_type = self.annotation_type(&expected_type);
                let actual_type = self.annotation_type(actual_type);
                if expected_type != actual_type {
                    self.error(
                        "E2001",
                        format!(
                            "parameter `{}` of `{}()` does not match trait `{trait_name}`",
                            actual.name, required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
            }
            match (&required.result_type, &function.result_type) {
                (Some(expected), Some(actual)) => {
                    let mut expected = expected.clone();
                    substitute_trait_type(&mut expected, &trait_substitutions);
                    if let Some(target_annotation) = &target_annotation {
                        replace_self_type(&mut expected, target_annotation);
                    }
                    let expected = self.annotation_type(&expected);
                    let actual = self.annotation_type(actual);
                    if expected != actual {
                        self.error(
                            "E2001",
                            format!(
                                "result of `{}()` does not match trait `{trait_name}`",
                                required.name
                            ),
                            function.result_span.as_ref(),
                        );
                        valid = false;
                    }
                }
                (None, None) => {}
                _ => {
                    self.error(
                        "E2001",
                        format!(
                            "result annotation of `{}()` does not match trait `{trait_name}`",
                            required.name
                        ),
                        function.result_span.as_ref(),
                    );
                    valid = false;
                }
            }
            implementation_valid &= valid;
        }
        if generic && implementation_valid {
            self.generic_trait_implementations.push(extension);
        } else if !generic && !implementation_valid {
            self.trait_implementations.remove(&key);
        }
    }

    fn explicit_substitutions(
        &mut self,
        parameters: &[TypeParameter],
        expr: &Expr,
    ) -> HashMap<String, Type> {
        let arguments = match expr.node() {
            Expr::SpecializedCall(_, arguments, _) => arguments.as_slice(),
            Expr::AssociatedCall { type_arguments, .. } if !type_arguments.is_empty() => {
                type_arguments.as_slice()
            }
            Expr::AssociatedCall { .. } => return HashMap::new(),
            _ => return HashMap::new(),
        };
        if arguments.is_empty() && parameters.is_empty() {
            return HashMap::new();
        }
        if arguments.len() != parameters.len() {
            self.error("E2001", format!("expected {} explicit generic arguments, got {}; partial arguments are not supported", parameters.len(), arguments.len()), expr.span());
            return HashMap::new();
        }
        parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), self.annotation_type(argument)))
            .collect()
    }

    fn generic_call(
        &mut self,
        name: &str,
        function: &FunctionDecl,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        self.generic_call_with_result_context(name, function, args, actual, expr, None)
    }

    fn generic_call_with_result_context(
        &mut self,
        name: &str,
        function: &FunctionDecl,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
        expected_result: Option<&Type>,
    ) -> Type {
        let bindings = crate::arguments::bind(name, &function.params, args);
        for error in bindings.errors {
            self.error(
                "E1005",
                error.message,
                error
                    .argument
                    .and_then(|index| args[index].value.span())
                    .or(expr.span()),
            );
        }

        let type_parameters: std::collections::HashSet<_> = function
            .type_params
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        let mut substitutions = self.explicit_substitutions(&function.type_params, expr);
        for (argument_index, actual_type) in inference_order(&actual) {
            let Some(parameter_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            let Some(expected) = function.params[parameter_index].ty.as_ref() else {
                continue;
            };
            self.infer_generic_type(
                expected,
                actual_type,
                &type_parameters,
                &mut substitutions,
                args[argument_index].value.span(),
            );
        }
        if let (Some(expected), Some(result)) = (expected_result, function.result_type.as_ref()) {
            self.infer_generic_type(
                result,
                expected,
                &type_parameters,
                &mut substitutions,
                expr.span(),
            );
        }

        for _ in 0..function.type_params.len() {
            let before = substitutions.len();
            for parameter in &function.type_params {
                let Some(actual_type) = substitutions.get(&parameter.name).cloned() else {
                    continue;
                };
                for bound in &parameter.bounds {
                    let candidates =
                        self.implemented_trait_argument_candidates(&actual_type, &bound.name);
                    let mut matching = Vec::new();
                    for candidate in candidates {
                        if candidate.len() != bound.arguments.len() {
                            continue;
                        }
                        let mut inferred = substitutions.clone();
                        if bound
                            .arguments
                            .iter()
                            .zip(&candidate)
                            .all(|(pattern, actual)| {
                                match_extension_type(
                                    pattern,
                                    actual,
                                    &type_parameters,
                                    &mut inferred,
                                )
                            })
                        {
                            matching.push(inferred);
                        }
                    }
                    if matching.len() == 1 {
                        substitutions = matching.pop().expect("one matching trait impl");
                    }
                }
            }
            if substitutions.len() == before {
                break;
            }
        }

        for parameter in &function.type_params {
            let Some(actual_type) = substitutions.get(&parameter.name).cloned() else {
                self.error(
                    "E2001",
                    format!(
                        "cannot infer generic type parameter `{}` for `{name}`",
                        parameter.name
                    ),
                    expr.span(),
                );
                continue;
            };
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(&actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{name}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        expr.span(),
                    );
                }
            }
        }

        let missing: Vec<_> = function
            .type_params
            .iter()
            .filter(|parameter| !substitutions.contains_key(&parameter.name))
            .map(|parameter| parameter.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Type::Poison;
        }

        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(parameter_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = function.params[parameter_index].ty.as_ref() {
                let expected_type = self.substituted_type(expected, &substitutions);
                let actual_type = &widened(
                    &args[argument_index].value,
                    actual_type.clone(),
                    &expected_type,
                );
                if !type_compatible(&expected_type, actual_type) {
                    let mut diagnostic = Diagnostic::new(
                        "E2001",
                        format!(
                            "type mismatch: parameter `{}` of `{name}` expects {}, got {}",
                            function.params[parameter_index].name,
                            expected_type.display(),
                            actual_type.display()
                        ),
                        args[argument_index].value.span().cloned(),
                    );
                    if let Some(suggestion) = primitive_type_mismatch_suggestion(
                        &expected_type,
                        actual_type,
                        args[argument_index].value.span(),
                    ) {
                        diagnostic = diagnostic.suggestion(suggestion);
                    } else if let Some(help) =
                        nullable_type_mismatch_help(&expected_type, actual_type)
                    {
                        diagnostic = diagnostic.help(help);
                    }
                    self.diagnostics.push(diagnostic);
                }
            }
        }

        let mut annotations = HashMap::new();
        for parameter in &function.type_params {
            let Some(actual_type) = substitutions.get(&parameter.name) else {
                return Type::Poison;
            };
            let Some(annotation) = self.type_annotation_for(actual_type) else {
                self.error(
                    "E2001",
                    format!(
                        "type {} cannot be used as a runtime generic argument",
                        actual_type.display()
                    ),
                    expr.span(),
                );
                return Type::Poison;
            };
            annotations.insert(parameter.name.clone(), annotation);
        }

        let type_arguments = function
            .type_params
            .iter()
            .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
            .collect::<Vec<_>>();
        let key = (function.name.clone(), type_arguments);
        let instance = if let Some(name) = self.specialization_names.get(&key) {
            name.clone()
        } else {
            let specialized_name = format!("{}$spec{}", function.name, self.specializations.len());
            self.specialization_names
                .insert(key, specialized_name.clone());
            self.specializations.push(Specialization {
                template: function.name.clone(),
                name: specialized_name.clone(),
                arguments: annotations,
            });
            specialized_name
        };
        self.call_specializations.insert(
            expression_site(self.current_function.as_deref(), expr),
            instance.clone(),
        );
        // Mutation analysis consumes concrete effects when a specialization is
        // available, while the ordinary call mapping keeps the generic template
        // as a fallback during specialization discovery.
        self.access_facts
            .specializations
            .insert(crate::mutability::expr_id(expr), instance);

        let result = function
            .result_type
            .as_ref()
            .map_or(Type::Dynamic, |result| {
                self.substituted_type(result, &substitutions)
            });
        if function.asynchronous {
            Type::Task(Box::new(result))
        } else {
            result
        }
    }

    fn generic_struct_call(
        &mut self,
        name: &str,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        let Some((id, fields)) = self.constructors.get(name).copied() else {
            return Type::Poison;
        };
        let parameters = self
            .struct_parameters
            .get(name)
            .cloned()
            .unwrap_or_default();
        let bindings = crate::arguments::bind(name, fields, args);
        for error in bindings.errors {
            self.error(
                "E1005",
                error.message,
                error
                    .argument
                    .and_then(|index| args[index].value.span())
                    .or(expr.span()),
            );
        }
        let type_parameters: std::collections::HashSet<_> = parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        let mut substitutions = self.explicit_substitutions(&parameters, expr);
        // A declared destination such as `Box[Node?]` selects the nullable
        // instance; argument types alone cannot.
        if let Some((_, expected)) = self
            .result_hint
            .take_if(|(hinted, _)| *hinted == crate::mutability::expr_id(expr))
            && let Type::Struct(expected_id, _, arguments) = expected.non_nil()
            && expected_id == id
        {
            for (parameter, argument) in parameters.iter().zip(arguments) {
                if contains_nullable(&argument) {
                    substitutions
                        .entry(parameter.name.clone())
                        .or_insert(argument);
                }
            }
        }
        for (argument_index, actual_type) in inference_order(&actual) {
            let Some(field_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = fields[field_index].ty.as_ref() {
                let known = self.substituted_type(expected, &substitutions);
                let actual_type =
                    &widened(&args[argument_index].value, actual_type.clone(), &known);
                self.infer_generic_type(
                    expected,
                    actual_type,
                    &type_parameters,
                    &mut substitutions,
                    args[argument_index].value.span(),
                );
            }
        }
        for parameter in &parameters {
            let Some(actual_type) = substitutions.get(&parameter.name).cloned() else {
                self.error(
                    "E2001",
                    format!(
                        "cannot infer generic type parameter `{}` for `{name}`",
                        parameter.name
                    ),
                    expr.span(),
                );
                continue;
            };
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(&actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{name}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        expr.span(),
                    );
                }
            }
        }
        if parameters
            .iter()
            .any(|parameter| !substitutions.contains_key(&parameter.name))
        {
            return Type::Poison;
        }
        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(field_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = fields[field_index].ty.as_ref() {
                let expected_type = self.substituted_type(expected, &substitutions);
                let actual_type = &widened(
                    &args[argument_index].value,
                    actual_type.clone(),
                    &expected_type,
                );
                if actual_type == &Type::EmptyArray
                    && let Type::Array(element) = expected_type.non_nil()
                {
                    self.refine_empty_array(&args[argument_index].value, &element);
                } else if !type_compatible(&expected_type, actual_type) {
                    let mut diagnostic = Diagnostic::new(
                        "E2001",
                        format!(
                            "type mismatch: field `{}` of `{name}` expects {}, got {}",
                            fields[field_index].name,
                            expected_type.display(),
                            actual_type.display()
                        ),
                        args[argument_index].value.span().cloned(),
                    );
                    if let Some(suggestion) = primitive_type_mismatch_suggestion(
                        &expected_type,
                        actual_type,
                        args[argument_index].value.span(),
                    ) {
                        diagnostic = diagnostic.suggestion(suggestion);
                    } else if let Some(help) =
                        nullable_type_mismatch_help(&expected_type, actual_type)
                    {
                        diagnostic = diagnostic.help(help);
                    }
                    self.diagnostics.push(diagnostic);
                }
            } else if contains_unresolved_empty_array(actual_type) {
                self.error(
                    "E2001",
                    "cannot infer array element type for an empty argument to a dynamic field; declare a typed array first",
                    args[argument_index].value.span(),
                );
            }
        }
        let Some(specialized_name) = self.request_struct_specialization(
            name,
            &parameters,
            &parameters
                .iter()
                .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
                .collect::<Vec<_>>(),
        ) else {
            return Type::Poison;
        };
        self.struct_call_specializations.insert(
            expression_site(self.current_function.as_deref(), expr),
            specialized_name,
        );
        Type::Struct(
            id,
            name.to_owned(),
            parameters
                .iter()
                .filter_map(|parameter| substitutions.get(&parameter.name).cloned())
                .collect(),
        )
    }

    fn infer_generic_type(
        &mut self,
        pattern: &TypeAnnotation,
        actual: &Type,
        parameters: &std::collections::HashSet<&str>,
        substitutions: &mut HashMap<String, Type>,
        span: Option<&SourceSpan>,
    ) {
        // A `P?` pattern infers `P` from the underlying type. Other patterns
        // also look through nil so the compatibility check reports it.
        let stripped;
        let actual = if pattern.nilable || !matches!(pattern.kind, TypeKind::TypeParameter(_)) {
            stripped = actual.non_nil();
            &stripped
        } else {
            actual
        };
        match &pattern.kind {
            TypeKind::TypeParameter(name) if parameters.contains(name.as_str()) => {
                if matches!(actual, Type::ErrorValue | Type::Poison) {
                    return;
                }
                if pattern.nilable && substitutions.contains_key(name) && *actual == Type::Nil {
                    return;
                }
                if substitutions.contains_key(name)
                    && matches!(actual, Type::Dynamic | Type::EmptyArray | Type::Generic(_))
                {
                    return;
                }
                if matches!(actual, Type::Dynamic | Type::EmptyArray | Type::Generic(_)) {
                    self.error(
                        "E2001",
                        format!("cannot infer `{name}` from {}", actual.display()),
                        span,
                    );
                    return;
                }
                match substitutions.get(name) {
                    Some(previous) if !type_compatible(previous, actual) => self.error(
                        "E2001",
                        format!(
                            "generic parameter `{name}` inferred as both {} and {}",
                            previous.display(),
                            actual.display()
                        ),
                        span,
                    ),
                    Some(_) => {}
                    None => {
                        substitutions.insert(name.clone(), actual.clone());
                    }
                }
            }
            TypeKind::Array(element) => match actual {
                Type::Array(actual_element) => self.infer_generic_type(
                    element,
                    actual_element,
                    parameters,
                    substitutions,
                    span,
                ),
                Type::EmptyArray => self.error(
                    "E2001",
                    "cannot infer a generic element type from an empty array; annotate the array first",
                    span,
                ),
                _ => {}
            },
            TypeKind::Tuple(elements) => {
                if let Type::Tuple(actual_elements) = actual
                    && elements.len() == actual_elements.len()
                {
                    for (expected, actual) in elements.iter().zip(actual_elements) {
                        self.infer_generic_type(
                            expected,
                            actual,
                            parameters,
                            substitutions,
                            span,
                        );
                    }
                }
            }
            TypeKind::Function {
                parameters: expected_parameters,
                result: expected_result,
            } => {
                if let Type::Function(actual_parameters, actual_result) = actual
                    && expected_parameters.len() == actual_parameters.len()
                {
                    for (expected, actual) in expected_parameters.iter().zip(actual_parameters) {
                        self.infer_generic_type(
                            expected,
                            actual,
                            parameters,
                            substitutions,
                            span,
                        );
                    }
                    self.infer_generic_type(
                        expected_result,
                        actual_result,
                        parameters,
                        substitutions,
                        span,
                    );
                }
            }
            TypeKind::Applied {
                source, arguments, ..
            }
            | TypeKind::ResolvedApplied {
                source,
                symbol: _,
                arguments,
            } if source == "Task" => {
                if let Type::Task(actual) = actual
                    && arguments.len() == 1
                {
                    self.infer_generic_type(
                        &arguments[0],
                        actual,
                        parameters,
                        substitutions,
                        span,
                    );
                }
            }
            TypeKind::Applied {
                source, arguments, ..
            }
            | TypeKind::ResolvedApplied {
                symbol: source,
                arguments,
                ..
            } => {
                if let Type::Struct(_, actual_name, actual_arguments) = actual
                    && (source == actual_name
                        || actual_name.ends_with(&format!("::{source}")))
                    && arguments.len() == actual_arguments.len()
                {
                    for (expected, actual) in arguments.iter().zip(actual_arguments) {
                        self.infer_generic_type(
                            expected,
                            actual,
                            parameters,
                            substitutions,
                            span,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn substituted_type(
        &mut self,
        annotation: &TypeAnnotation,
        substitutions: &HashMap<String, Type>,
    ) -> Type {
        let ty = match &annotation.kind {
            TypeKind::TypeParameter(name) => substitutions
                .get(name)
                .cloned()
                .unwrap_or(Type::Generic(name.clone())),
            TypeKind::Array(element) => {
                Type::Array(Box::new(self.substituted_type(element, substitutions)))
            }
            TypeKind::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.substituted_type(element, substitutions))
                    .collect(),
            ),
            TypeKind::Function { parameters, result } => Type::Function(
                parameters
                    .iter()
                    .map(|parameter| self.substituted_type(parameter, substitutions))
                    .collect(),
                Box::new(self.substituted_type(result, substitutions)),
            ),
            TypeKind::Applied { source, arguments } if source == "Task" => {
                if arguments.len() != 1 {
                    Type::Poison
                } else {
                    Type::Task(Box::new(
                        self.substituted_type(&arguments[0], substitutions),
                    ))
                }
            }
            TypeKind::Applied { source, arguments } => {
                let symbol = self.resolve(source, None).unwrap_or_else(|| source.clone());
                self.substituted_applied_type(&symbol, arguments, substitutions)
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } if source == "Task" || symbol == "Task" => {
                if arguments.len() != 1 {
                    Type::Poison
                } else {
                    Type::Task(Box::new(
                        self.substituted_type(&arguments[0], substitutions),
                    ))
                }
            }
            TypeKind::ResolvedApplied {
                symbol, arguments, ..
            } => self.substituted_applied_type(symbol, arguments, substitutions),
            _ => self.annotation_type(annotation),
        };
        if annotation.nilable {
            ty.nullable()
        } else {
            ty
        }
    }

    fn substituted_applied_type(
        &mut self,
        symbol: &str,
        arguments: &[TypeAnnotation],
        substitutions: &HashMap<String, Type>,
    ) -> Type {
        let Some((id, _)) = self.constructors.get(symbol).copied() else {
            return Type::Poison;
        };
        Type::Struct(
            id,
            symbol.to_owned(),
            arguments
                .iter()
                .map(|argument| self.substituted_type(argument, substitutions))
                .collect(),
        )
    }

    fn check_function(&mut self, f: &FunctionDecl, function_modules: &HashMap<String, String>) {
        if !f.type_params.is_empty() {
            return;
        }
        let previous_flow_facts = std::mem::take(&mut self.flow_facts);
        let previous_function = self.current_function.replace(f.name.clone());
        let previous_owner = std::mem::replace(&mut self.owner, f.owner.clone());
        let previous_async = std::mem::replace(&mut self.in_async, f.asynchronous);
        self.module = function_modules
            .get(&f.name)
            .cloned()
            .or_else(|| f.name.rsplit_once("::").map(|(m, _)| m.to_owned()));
        self.writes = Writes::default();
        self.writes.block(&f.body);
        loop {
            self.flow_facts = FlowFacts::default();
            self.unresolved_empty_arrays.clear();
            let diagnostics_start = self.diagnostics.len();
            let unresolved = self.dynamic_bindings.len();
            self.scopes.push(Scope::default());
            self.in_comptime = f.comptime;
            self.in_async = f.asynchronous;
            self.defaults(&f.params);
            for parameter in &f.params {
                let ty = parameter
                    .ty
                    .as_ref()
                    .map_or(Type::Dynamic, |ty| self.annotation_type(ty));
                self.define(
                    &parameter.name,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty,
                        mutable: true,
                    },
                    None,
                );
            }
            self.in_function = true;
            self.result_type = f.result_type.clone();
            self.location = f.result_span.clone();
            let result = self.block(&f.body);
            for (_, (name, span)) in std::mem::take(&mut self.unresolved_empty_arrays) {
                self.error(
                    "E2001",
                    format!("cannot infer array element type for `{name}`; add a `T[]` annotation"),
                    Some(&span),
                );
            }
            if let Some(expected) = f.result_type.clone() {
                self.assignable(
                    expected,
                    result,
                    f.body
                        .tail
                        .as_deref()
                        .and_then(Expr::span)
                        .or(f.result_span.as_ref()),
                    &format!("normal result of `{}`", f.name),
                );
            } else if contains_unresolved_empty_array(&result) {
                self.error(
                    "E2001",
                    format!("cannot infer array element type in the result of `{}`; add a `T[]` annotation", f.name),
                    f.body.tail.as_deref().and_then(Expr::span),
                );
            }
            self.in_function = false;
            self.in_comptime = false;
            self.in_async = false;
            self.result_type = None;
            self.location = None;
            self.scopes.pop();
            if unresolved == self.dynamic_bindings.len() {
                break;
            }
            self.diagnostics.truncate(diagnostics_start);
        }
        self.flow_facts = previous_flow_facts;
        self.current_function = previous_function;
        self.owner = previous_owner;
        self.in_async = previous_async;
    }

    fn annotation_type(&mut self, ty: &TypeAnnotation) -> Type {
        let resolved = self.annotation_base_type(ty);
        if ty.nilable {
            resolved.nullable()
        } else {
            resolved
        }
    }

    fn annotation_base_type(&mut self, ty: &TypeAnnotation) -> Type {
        match &ty.kind {
            TypeKind::Primitive(base) if *base == VarType::Nil && !ty.nilable => Type::Nil,
            TypeKind::Primitive(base) => Type::Primitive(*base, ty.nilable),
            TypeKind::Bytes => Type::Bytes,
            TypeKind::Named(name) => {
                if name == "Task" {
                    self.error("E2001", "`Task` requires one result type argument", None);
                    return Type::Poison;
                }
                let resolved = self.resolve(name, None).unwrap_or_else(|| name.clone());
                if let Some((id, _)) = self.constructors.get(&resolved) {
                    if self
                        .struct_parameters
                        .get(&resolved)
                        .is_some_and(|parameters| !parameters.is_empty())
                    {
                        self.error(
                            "E2001",
                            format!("generic struct `{name}` requires type arguments"),
                            None,
                        );
                        Type::Poison
                    } else {
                        Type::Struct(*id, resolved, Vec::new())
                    }
                } else if let Some(id) = self.enums.get(&resolved) {
                    Type::Enum(*id, resolved)
                } else {
                    Type::Named(resolved)
                }
            }
            TypeKind::ResolvedName { source, symbol } if source == "Task" || symbol == "Task" => {
                self.error("E2001", "`Task` requires one result type argument", None);
                Type::Poison
            }
            TypeKind::ResolvedName { source, symbol } => {
                if let Some((id, _)) = self.constructors.get(symbol) {
                    if self
                        .struct_parameters
                        .get(symbol)
                        .is_some_and(|parameters| !parameters.is_empty())
                    {
                        self.error(
                            "E2001",
                            format!("generic struct `{source}` requires type arguments"),
                            None,
                        );
                        Type::Poison
                    } else {
                        Type::Struct(*id, symbol.clone(), Vec::new())
                    }
                } else if let Some(id) = self.enums.get(symbol) {
                    Type::Enum(*id, symbol.clone())
                } else {
                    Type::Named(if symbol == source {
                        source.clone()
                    } else {
                        symbol.clone()
                    })
                }
            }
            TypeKind::Applied { source, arguments } => {
                if source == "Task" {
                    if arguments.len() != 1 {
                        self.error("E2001", "`Task` expects one result type argument", None);
                        return Type::Poison;
                    }
                    return Type::Task(Box::new(self.annotation_type(&arguments[0])));
                }
                let symbol = self.resolve(source, None).unwrap_or_else(|| source.clone());
                self.applied_struct_type(source, &symbol, arguments)
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } if source == "Task" || symbol == "Task" => {
                if arguments.len() != 1 {
                    self.error("E2001", "`Task` expects one result type argument", None);
                    Type::Poison
                } else {
                    Type::Task(Box::new(self.annotation_type(&arguments[0])))
                }
            }
            TypeKind::ResolvedApplied {
                source,
                symbol,
                arguments,
            } => self.applied_struct_type(source, symbol, arguments),
            TypeKind::TypeParameter(name) => Type::Generic(name.clone()),
            TypeKind::SelfType => Type::Named("Self".into()),
            TypeKind::Array(element) => Type::Array(Box::new(self.annotation_type(element))),
            TypeKind::Tuple(items) => Type::Tuple(
                items
                    .iter()
                    .map(|item| self.annotation_type(item))
                    .collect(),
            ),
            TypeKind::Function { parameters, result } => Type::Function(
                parameters
                    .iter()
                    .map(|item| self.annotation_type(item))
                    .collect(),
                Box::new(self.annotation_type(result)),
            ),
        }
    }

    fn applied_struct_type(
        &mut self,
        source: &str,
        symbol: &str,
        arguments: &[TypeAnnotation],
    ) -> Type {
        let Some((id, _)) = self.constructors.get(symbol).copied() else {
            self.error("E2001", format!("`{source}` is not a generic struct"), None);
            return Type::Poison;
        };
        let parameters = self
            .struct_parameters
            .get(symbol)
            .cloned()
            .unwrap_or_default();
        if parameters.is_empty() || parameters.len() != arguments.len() {
            self.error(
                "E2001",
                format!(
                    "generic struct `{source}` expects {} type argument(s), got {}",
                    parameters.len(),
                    arguments.len()
                ),
                None,
            );
            return Type::Poison;
        }
        let actual: Vec<_> = arguments
            .iter()
            .map(|argument| self.annotation_type(argument))
            .collect();
        let substitutions: HashMap<_, _> = parameters
            .iter()
            .zip(&actual)
            .map(|(parameter, actual)| (parameter.name.clone(), actual.clone()))
            .collect();
        for (parameter, actual_type) in parameters.iter().zip(&actual) {
            if !is_concrete_trait_argument(actual_type) {
                continue;
            }
            for bound in &parameter.bounds {
                let bound_arguments: Vec<_> = bound
                    .arguments
                    .iter()
                    .map(|argument| self.substituted_type(argument, &substitutions))
                    .collect();
                if !self.implements_trait(actual_type, &bound.name, &bound_arguments) {
                    self.error(
                        "E2001",
                        format!(
                            "type {} does not implement trait `{}` required by `{source}`",
                            actual_type.display(),
                            bound.display()
                        ),
                        None,
                    );
                }
            }
        }
        if actual.iter().all(is_concrete_trait_argument) {
            self.request_struct_specialization(symbol, &parameters, &actual);
        }
        Type::Struct(id, symbol.to_owned(), actual)
    }

    fn request_struct_specialization(
        &mut self,
        template: &str,
        parameters: &[TypeParameter],
        arguments: &[Type],
    ) -> Option<String> {
        if parameters.len() != arguments.len() || !arguments.iter().all(is_concrete_trait_argument)
        {
            return None;
        }
        let key = (template.to_owned(), arguments.to_vec());
        if let Some(name) = self.struct_specialization_names.get(&key) {
            return Some(name.clone());
        }
        let annotations: Vec<_> = arguments
            .iter()
            .map(|argument| self.type_annotation_for(argument))
            .collect::<Option<_>>()?;
        let name = format!(
            "{}[{}]",
            template,
            annotations
                .iter()
                .map(TypeAnnotation::canonical)
                .collect::<Vec<_>>()
                .join(",")
        );
        let substitutions: HashMap<_, _> = parameters
            .iter()
            .zip(&annotations)
            .map(|(parameter, annotation)| (parameter.name.clone(), annotation.clone()))
            .collect();
        self.struct_specialization_names.insert(key, name.clone());
        self.struct_specializations.push(StructSpecialization {
            template: template.to_owned(),
            name: name.clone(),
            arguments: substitutions,
        });
        Some(name)
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: Option<&SourceSpan>) {
        self.diagnostics.push(Diagnostic::new(
            code,
            message,
            span.cloned().or_else(|| self.location.clone()),
        ));
    }

    fn assignable(
        &mut self,
        expected: TypeAnnotation,
        actual: Type,
        span: Option<&SourceSpan>,
        binding: &str,
    ) -> bool {
        let expected_type = self.annotation_type(&expected);
        let valid = matches!(actual, Type::Poison | Type::ErrorValue | Type::Never)
            || type_compatible(&expected_type, &actual);
        if !valid {
            let mut diagnostic = Diagnostic::new(
                "E2001",
                format!(
                    "type mismatch: {binding} declared as {} but given {}",
                    expected.display(),
                    actual.display()
                ),
                span.cloned().or_else(|| self.location.clone()),
            );
            if let Some(suggestion) =
                primitive_type_mismatch_suggestion(&expected_type, &actual, span)
            {
                diagnostic = diagnostic.suggestion(suggestion);
            } else if let Some(help) = nullable_type_mismatch_help(&expected_type, &actual) {
                diagnostic = diagnostic.help(help);
            }
            self.diagnostics.push(diagnostic);
        }
        valid
    }

    fn assignable_type(
        &mut self,
        expected: Type,
        actual: Type,
        span: Option<&SourceSpan>,
        binding: &str,
    ) {
        match expected {
            Type::Primitive(base, nilable) => {
                self.assignable(
                    TypeAnnotation {
                        kind: TypeKind::Primitive(base),
                        nilable,
                    },
                    actual,
                    span,
                    binding,
                );
            }
            Type::Nil => {
                self.assignable(
                    TypeAnnotation {
                        kind: TypeKind::Primitive(VarType::Nil),
                        nilable: false,
                    },
                    actual,
                    span,
                    binding,
                );
            }
            Type::Dynamic | Type::Poison => {}
            expected if type_compatible(&expected, &actual) => {}
            expected => {
                let mut diagnostic = Diagnostic::new(
                    "E2001",
                    format!(
                        "type mismatch: {binding} declared as {} but given {}",
                        expected.display(),
                        actual.display()
                    ),
                    span.cloned().or_else(|| self.location.clone()),
                );
                if let Some(help) = nullable_type_mismatch_help(&expected, &actual) {
                    diagnostic = diagnostic.help(help);
                }
                match (&expected, &actual) {
                    (Type::Array(expected), Type::Array(_)) => {
                        diagnostic = diagnostic.help(format!(
                            "array element types are invariant; make the elements match `{}` or convert them before assignment",
                            expected.display()
                        ));
                    }
                    (Type::Tuple(expected), Type::Tuple(actual))
                        if expected.len() == actual.len() =>
                    {
                        diagnostic = diagnostic.help(
                            "tuple element types are positional; check each element against its declared position",
                        );
                    }
                    _ => {}
                }
                self.diagnostics.push(diagnostic);
            }
        }
    }

    fn define(&mut self, name: &str, mut binding: Binding, span: Option<&SourceSpan>) {
        binding.id = self.next_binding;
        self.next_binding += 1;
        let scope = self.scopes.last_mut().expect("root scope");
        if scope.bindings.insert(name.to_owned(), binding).is_some() {
            self.error(
                "E1001",
                format!("binding `{name}` defined more than once in this scope"),
                span,
            );
        }
    }

    fn collect_binding_types(
        &mut self,
        pattern: &BindingPattern,
        ty: Type,
        span: Option<&SourceSpan>,
        bindings: &mut Vec<(String, Type, Option<SourceSpan>)>,
    ) {
        match pattern {
            BindingPattern::Name(name) => bindings.push((name.clone(), ty, span.cloned())),
            BindingPattern::Tuple(patterns) => match ty {
                Type::Tuple(types) if types.len() == patterns.len() => {
                    for (pattern, ty) in patterns.iter().zip(types) {
                        self.collect_binding_types(pattern, ty, span, bindings);
                    }
                }
                Type::Tuple(types) => {
                    self.error(
                        "E2001",
                        format!(
                            "tuple binding pattern has {} elements but the value has {}",
                            patterns.len(),
                            types.len()
                        ),
                        span,
                    );
                    for pattern in patterns {
                        self.collect_binding_types(pattern, Type::Poison, span, bindings);
                    }
                }
                Type::Dynamic => {
                    for pattern in patterns {
                        self.collect_binding_types(pattern, Type::Dynamic, span, bindings);
                    }
                }
                Type::Poison => {
                    for pattern in patterns {
                        self.collect_binding_types(pattern, Type::Poison, span, bindings);
                    }
                }
                Type::ErrorValue | Type::Never => {
                    for pattern in patterns {
                        self.collect_binding_types(pattern, Type::Never, span, bindings);
                    }
                }
                actual => {
                    self.error(
                        "E2001",
                        format!(
                            "tuple binding pattern requires a tuple, got {}",
                            actual.display()
                        ),
                        span,
                    );
                    for pattern in patterns {
                        self.collect_binding_types(pattern, Type::Poison, span, bindings);
                    }
                }
            },
        }
    }

    fn local(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.bindings.get(name).cloned())
    }

    fn effective_type(&self, binding: &Binding) -> Type {
        if binding.error_only {
            Type::ErrorValue
        } else {
            self.flow_facts
                .normal_types
                .get(&binding.id)
                .cloned()
                .unwrap_or_else(|| binding.ty.clone())
        }
    }

    fn binding_for_value(&mut self, value: &Expr) -> Option<Binding> {
        let Expr::Ident(name) = value.node() else {
            return None;
        };
        if let Some(binding) = self.local(name) {
            return Some(binding);
        }
        let resolved = self.resolve(name, value.span())?;
        self.globals.get(&resolved).cloned()
    }

    fn apply_type_pattern(&mut self, value: &Expr, name: &str, positive: bool) {
        let Some(binding) = self.binding_for_value(value) else {
            return;
        };
        if binding.id == 0
            || matches!(value.node(), Expr::Ident(name) if self.writes.deferred.contains(name))
        {
            return;
        }
        let error_pattern = name == "Error"
            || name == crate::error::ERROR_TYPE
            || name.starts_with(&format!("{}::", crate::error::ERROR_TYPE));
        if error_pattern {
            if positive {
                self.flow_facts.error_excluded.remove(&binding.id);
            } else if name == "Error" || name == crate::error::ERROR_TYPE {
                self.flow_facts.error_excluded.insert(binding.id);
            }
            return;
        }

        let tested = if name == "nil" {
            Some(Type::Nil)
        } else if let Some(primitive) = crate::instruction::ValueType::primitive(name) {
            match primitive {
                crate::instruction::ValueType::Int => Some(Type::concrete(VarType::Int)),
                crate::instruction::ValueType::UInt => Some(Type::concrete(VarType::UInt)),
                crate::instruction::ValueType::BigInt => Some(Type::concrete(VarType::BigInt)),
                crate::instruction::ValueType::Float => Some(Type::concrete(VarType::Float)),
                crate::instruction::ValueType::Bool => Some(Type::concrete(VarType::Bool)),
                crate::instruction::ValueType::Char => Some(Type::concrete(VarType::Char)),
                crate::instruction::ValueType::Str => Some(Type::concrete(VarType::Str)),
                crate::instruction::ValueType::Nil | crate::instruction::ValueType::Error => None,
                crate::instruction::ValueType::Enum(_)
                | crate::instruction::ValueType::Variant(_)
                | crate::instruction::ValueType::Struct(_)
                | crate::instruction::ValueType::Bytes => None,
            }
        } else if let Some(id) = self.enums.get(name) {
            Some(Type::Enum(*id, name.to_owned()))
        } else if let Some((id, _)) = self.constructors.get(name) {
            Some(Type::Struct(*id, name.to_owned(), Vec::new()))
        } else {
            (name == "bytes").then_some(Type::Bytes)
        };

        if positive {
            // Any successful non-Error type test proves the value is a normal
            // value. A nil test retains a nil normal type; a concrete primitive
            // test removes nullable from the matching primitive.
            self.flow_facts.error_excluded.insert(binding.id);
            let current = self.effective_type(&binding);
            let refined = if name == "nil" {
                Some(Type::Nil)
            } else {
                match (&current, tested) {
                    (Type::Primitive(actual, true), Some(Type::Primitive(expected, false)))
                        if actual == &expected =>
                    {
                        Some(Type::Primitive(*actual, false))
                    }
                    // A successful enum test keeps the already known variant set.
                    (Type::Nullable(inner), Some(Type::Enum(id, _))) if matches!(**inner, Type::Enum(other, _) if other == id) => {
                        Some((**inner).clone())
                    }
                    (Type::Dynamic, Some(tested)) => Some(tested),
                    (_, Some(tested)) if type_compatible(&current, &tested) => Some(tested),
                    _ => None,
                }
            };
            if let Some(refined) = refined {
                self.flow_facts.normal_types.insert(binding.id, refined);
            }
        } else if name == "nil" {
            self.narrow_non_nil(value);
        } else if name != "Error" {
            // A failed Error test proves only that Error is absent. It does not
            // remove nil or otherwise change the declared normal type.
            self.flow_facts.error_excluded.insert(binding.id);
        }
    }

    /// Record that a nullable binding holds a value on the current path.
    /// Only bindings narrow: fields and elements can change through aliases.
    fn narrow_non_nil(&mut self, value: &Expr) {
        let Expr::Ident(name) = value.node() else {
            return;
        };
        if self.writes.deferred.contains(name) {
            return;
        }
        let Some(binding) = self.binding_for_value(value) else {
            return;
        };
        let current = self.effective_type(&binding);
        if binding.id != 0 && current.is_nullable() {
            self.flow_facts
                .normal_types
                .insert(binding.id, current.non_nil());
        }
    }

    /// Discard facts that a call, or a task running during `await`, can break:
    /// mutable globals and locals captured mutably by a closure.
    fn invalidate_call_flow_facts(&mut self) {
        self.invalidate_flow_facts(&HashSet::new());
    }

    /// Discard facts that do not hold on every entry of a region that runs the
    /// code summarized by `writes` an unknown number of times.
    fn invalidate_region_flow_facts(&mut self, writes: &Writes) {
        self.invalidate_flow_facts(&writes.assigned);
    }

    fn invalidate_flow_facts(&mut self, assigned: &HashSet<String>) {
        let locals = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.iter())
            .filter(|(_, binding)| binding.mutable);
        for (name, binding) in locals {
            self.flow_facts.error_excluded.remove(&binding.id);
            if assigned.contains(name) || self.writes.captured.contains(name) {
                self.flow_facts.normal_types.remove(&binding.id);
            }
        }
        for binding in self.globals.values().filter(|binding| binding.mutable) {
            self.flow_facts.forget(binding.id);
        }
    }

    fn join_flow_exits(&mut self, states: impl IntoIterator<Item = FlowFacts>) {
        self.flow_facts = FlowFacts::join(states);
    }

    fn refine_empty_array(&mut self, receiver: &Expr, element: &Type) {
        let Expr::Ident(name) = receiver.node() else {
            return;
        };
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get_mut(name) {
                if binding.ty == Type::EmptyArray {
                    let inferred = Type::Array(Box::new(element.clone()));
                    binding.ty = inferred.clone();
                    if let Some((id, _)) = binding.inferred {
                        binding.inferred = Some((id, inferred));
                        self.unresolved_empty_arrays.remove(&id);
                    }
                }
                return;
            }
        }
    }

    fn refine_error_binding(&mut self, value: &Expr) {
        let Expr::Ident(name) = value.node() else {
            return;
        };
        let binding = self
            .local(name)
            .or_else(|| self.globals.get(name).cloned().filter(|b| !b.mutable));
        if let Some(mut binding) = binding {
            binding.error_only = true;
            let overlay = self.scopes.len() - 2;
            self.scopes[overlay].bindings.insert(name.clone(), binding);
        }
    }

    fn type_test(&mut self, name: &str, span: Option<&SourceSpan>) -> Option<String> {
        if crate::instruction::ValueType::primitive(name).is_some() {
            return Some(name.into());
        }
        let resolved = self.resolve(name, span)?;
        if self.enums.contains_key(&resolved) || self.variants.contains_key(&resolved) {
            return Some(resolved);
        }
        if self
            .globals
            .get(&resolved)
            .is_some_and(|binding| binding.ty == Type::Poison)
        {
            return None;
        }
        let message = if resolved == "bytes" && !self.constructors.contains_key(&resolved) {
            return Some(resolved);
        } else if self.constructors.contains_key(&resolved) {
            if self
                .struct_parameters
                .get(&resolved)
                .is_none_or(|parameters| parameters.is_empty())
            {
                return Some(resolved);
            }
            format!(
                "type test on generic struct `{name}` is not supported; \
                 patterns cannot name its type arguments"
            )
        } else {
            format!("unknown type or enum variant `{name}` in type test")
        };
        self.error("E2001", message, span);
        None
    }

    fn known(&self, name: &str) -> bool {
        self.functions.contains_key(name)
            || self.globals.contains_key(name)
            || self.constructors.contains_key(name)
            || self.variants.contains_key(name)
            || self.enums.contains_key(name)
            || Builtin::from_name(name).is_some()
    }

    fn resolve(&mut self, name: &str, span: Option<&SourceSpan>) -> Option<String> {
        if let Some(target) = self.scopes.iter().rev().find_map(|s| s.imports.get(name)) {
            return Some(target.clone());
        }
        if self.enums.contains_key(name) || self.variants.contains_key(name) {
            return Some(name.to_owned());
        }
        if let Some((prefix, tail)) = name.split_once("::") {
            if self.module.as_deref() == Some(prefix) {
                return Some(name.to_owned());
            }
            if let Some(module) = self.scopes.iter().rev().find_map(|s| s.modules.get(prefix)) {
                return Some(format!("{module}::{tail}"));
            }
            // Enum qualification is a type path, not a module import.
            if self.variants.contains_key(name) {
                return Some(name.to_owned());
            }
            if let Some(module) = &self.module {
                let local = format!("{module}::{name}");
                if self.known(&local) {
                    return Some(local);
                }
            }
            self.error("E1002", format!("undeclared module `{prefix}`"), span);
            return None;
        }
        if let Some(module) = &self.module {
            let local = format!("{module}::{name}");
            if self.known(&local) {
                return Some(local);
            }
        }
        Some(name.to_owned())
    }

    fn import(&mut self, decl: &UseDecl) {
        if decl.namespace {
            let alias = decl
                .aliases
                .first()
                .and_then(|s| s.as_deref())
                .unwrap_or(&decl.module);
            self.scopes
                .last_mut()
                .unwrap()
                .modules
                .insert(alias.to_owned(), decl.module.clone());
        } else if decl.wildcard {
            let prefix = format!("{}::", decl.module);
            let names: Vec<_> = self
                .functions
                .keys()
                .chain(self.globals.keys())
                .chain(self.constructors.keys())
                .chain(self.variants.keys())
                .chain(self.enums.keys())
                .filter_map(|n| {
                    n.strip_prefix(&prefix)
                        .map(|short| (short.to_owned(), n.clone()))
                })
                .collect();
            self.scopes.last_mut().unwrap().imports.extend(names);
            // Builtin wildcard resolution uses the imported namespace as fallback.
            self.scopes
                .last_mut()
                .unwrap()
                .modules
                .insert(decl.module.clone(), decl.module.clone());
        } else {
            for (i, name) in decl.names.iter().enumerate() {
                let alias = decl
                    .aliases
                    .get(i)
                    .and_then(|a| a.as_deref())
                    .unwrap_or(name);
                self.scopes
                    .last_mut()
                    .unwrap()
                    .imports
                    .insert(alias.to_owned(), format!("{}::{name}", decl.module));
            }
        }
    }

    fn defaults(&mut self, fields: &[StructField]) {
        for field in fields {
            if let Some(value) = &field.default {
                let expected = field
                    .ty
                    .as_ref()
                    .map(|ty| self.annotation_type(ty))
                    .filter(contains_nullable);
                let actual = self.expr_with_expected_result(value, expected.as_ref());
                if let Some(ty) = field.ty.clone() {
                    self.assignable(
                        ty,
                        actual,
                        value.span(),
                        &format!("default for `{}`", field.name),
                    );
                } else if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        format!("cannot infer array element type for default `{}`; add a type annotation", field.name),
                        value.span(),
                    );
                }
            }
        }
    }

    fn block(&mut self, block: &Block) -> Type {
        self.block_expecting(block, None)
    }

    /// Check a block whose value is never used.
    fn discarded_block(&mut self, block: &Block) -> Type {
        self.discard_tail(block);
        self.block(block)
    }

    fn discard(&mut self, expr: &Expr) {
        self.discarded.insert(crate::mutability::expr_id(expr));
    }

    fn discard_tail(&mut self, block: &Block) {
        if let Some(tail) = &block.tail {
            self.discard(tail);
        }
    }

    /// Join the values of two branches, rejecting a join that would change the
    /// element type of a container that may already be shared.
    fn join_values(&mut self, left: Type, right: Type, used: bool) -> Type {
        if !used || !container_conflict(&left, &right) {
            return left.join(right);
        }
        let message = format!(
            "type mismatch: cannot join `{}` and `{}`; array element types and generic type arguments are invariant",
            left.display(),
            right.display()
        );
        self.diagnostics.push(
            Diagnostic::new("E2001", message, self.location.clone()).help(
                "annotate the destination so that every branch builds that type, or copy the elements into a new array",
            ),
        );
        Type::Poison
    }

    fn block_expecting(&mut self, block: &Block, expected: Option<&Type>) -> Type {
        self.scopes.push(Scope::default());
        let mut reaches_tail = true;
        let previous_reachability = self.normal_reachable;
        // Check all source, including unreachable statements. Only reachable
        // normal paths contribute to a block result; return/break/crash do not.
        for stmt in &block.stmts {
            self.normal_reachable = previous_reachability && reaches_tail;
            let result = self.stmt(stmt);
            reaches_tail &= result != Type::Never;
        }
        self.normal_reachable = previous_reachability && reaches_tail;
        let expected_result = expected.cloned().or_else(|| {
            block
                .tail
                .as_deref()
                .filter(|_| self.in_function && self.scopes.len() == 3)
                .and_then(|_| self.result_type.clone())
                .map(|annotation| self.annotation_type(&annotation))
        });
        let tail = block.tail.as_ref().map_or(Type::Nil, |tail| {
            self.expr_with_expected_result(tail, expected_result.as_ref())
        });
        self.normal_reachable = previous_reachability;
        // Leaving the block runs its cleanup, which may call anything.
        if block
            .stmts
            .iter()
            .any(|stmt| matches!(stmt.node(), Stmt::Defer { .. }))
        {
            self.invalidate_call_flow_facts();
        }
        self.scopes.pop();
        if reaches_tail { tail } else { Type::Never }
    }

    fn stmt(&mut self, stmt: &Stmt) -> Type {
        let previous = self.location.clone();
        if let Some(span) = stmt.span() {
            self.location = Some(span.clone());
        }
        let result = self.stmt_inner(stmt);
        self.location = previous;
        result
    }

    fn stmt_inner(&mut self, stmt: &Stmt) -> Type {
        match stmt.node() {
            Stmt::Located(_, _) => unreachable!("node removes locations"),
            Stmt::Invalid { binding } => {
                if let Some(name) = binding {
                    self.define(
                        name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty: Type::Poison,
                            mutable: true,
                        },
                        stmt.span(),
                    );
                }
            }
            Stmt::VarDecl { pattern, ty, value } | Stmt::Const { pattern, ty, value } => {
                let binding_label = match pattern {
                    BindingPattern::Name(name) => format!("`{name}`"),
                    BindingPattern::Tuple(_) => "tuple binding".to_owned(),
                };
                let expected_result = ty
                    .as_ref()
                    .map(|annotation| self.annotation_type(annotation));
                let actual = self.expr_with_expected_result(value, expected_result.as_ref());
                let valid = ty.clone().is_none_or(|annotation| {
                    self.assignable(annotation, actual.clone(), value.span(), &binding_label)
                });
                let mutable = matches!(stmt.node(), Stmt::VarDecl { .. });
                let access_id = stmt.node() as *const Stmt as usize;
                if !valid || actual == Type::Poison {
                    self.access_facts.invalid_bindings.insert(access_id);
                } else {
                    self.access_facts.invalid_bindings.remove(&access_id);
                }

                let pattern_type = if !valid || actual == Type::Poison {
                    Type::Poison
                } else if let Some(annotation) = ty {
                    self.annotation_type(annotation)
                } else {
                    actual.clone()
                };
                let mut binding_types = Vec::new();
                self.collect_binding_types(pattern, pattern_type, value.span(), &mut binding_types);
                let mut pattern_name_ids = Vec::new();
                pattern.visit_names(&mut |name| pattern_name_ids.push(name.as_ptr() as usize));

                for ((name, projected_type, span), binding_id) in
                    binding_types.into_iter().zip(pattern_name_ids)
                {
                    let inferred = mutable && ty.is_none() && projected_type != Type::Poison;
                    if ty.is_none()
                        && contains_unresolved_empty_array(&projected_type)
                        && let Some(span) = value.span().cloned()
                    {
                        self.unresolved_empty_arrays
                            .insert(binding_id, (name.clone(), span));
                    }
                    let binding_type = if !valid || actual == Type::Poison {
                        Type::Poison
                    } else if inferred
                        && (projected_type == Type::ErrorValue
                            || self.dynamic_bindings.contains(&binding_id))
                    {
                        Type::Dynamic
                    } else {
                        projected_type.clone()
                    };
                    self.define(
                        &name,
                        Binding {
                            id: 0,
                            error_only: false,
                            ty: binding_type,
                            mutable,
                            inferred: inferred.then_some((
                                binding_id,
                                if projected_type == Type::ErrorValue {
                                    Type::Dynamic
                                } else {
                                    projected_type
                                },
                            )),
                        },
                        span.as_ref(),
                    );
                }
                if actual == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::Comptime { name, ty, value } => {
                let is_comptime_initializer = matches!(stmt.node(), Stmt::Comptime { .. });
                let previous_comptime_initializer = self.in_comptime_initializer;
                self.in_comptime_initializer |= is_comptime_initializer;
                let expected_result = ty
                    .as_ref()
                    .map(|annotation| self.annotation_type(annotation));
                let actual = self.expr_with_expected_result(value, expected_result.as_ref());
                self.in_comptime_initializer = previous_comptime_initializer;
                let valid = ty.clone().is_none_or(|t| {
                    self.assignable(t, actual.clone(), value.span(), &format!("`{name}`"))
                });
                let mutable = false;
                let access_id = stmt.node() as *const Stmt as usize;
                if !valid || actual == Type::Poison {
                    self.access_facts.invalid_bindings.insert(access_id);
                } else {
                    self.access_facts.invalid_bindings.remove(&access_id);
                }
                let id = stmt as *const Stmt as usize;
                let inferred = mutable && ty.is_none() && actual != Type::Poison;
                if ty.is_none()
                    && contains_unresolved_empty_array(&actual)
                    && let Some(span) = value.span().cloned()
                {
                    self.unresolved_empty_arrays
                        .insert(id, (name.clone(), span));
                }
                let binding_type = if !valid || actual == Type::Poison {
                    Type::Poison
                } else if inferred
                    && (actual == Type::ErrorValue || self.dynamic_bindings.contains(&id))
                {
                    Type::Dynamic
                } else {
                    ty.as_ref()
                        .map_or(actual.clone(), |t| self.annotation_type(t))
                };
                self.define(
                    name,
                    Binding {
                        id: 0,
                        error_only: false,
                        ty: binding_type,
                        mutable,
                        inferred: inferred.then_some((
                            id,
                            if actual == Type::ErrorValue {
                                Type::Dynamic
                            } else {
                                actual.clone()
                            },
                        )),
                    },
                    stmt.span(),
                );
                if actual == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::CompoundAssign { target, op, value } => {
                let diagnostics = self.diagnostics.len();
                let element = self.element_target(target);
                let left = match target.node() {
                    Expr::Index(..) => element.clone().unwrap_or(Type::Poison),
                    _ => self.expr(target),
                };
                let invalid_target = self.diagnostics.len() > diagnostics;
                // A shift amount keeps its own `int`/`uint` type.
                if left == Type::concrete(VarType::UInt) && !matches!(op, BinOp::Shl | BinOp::Shr) {
                    self.contextualize_integer_literals(value, &left);
                }
                let right = self.expr(value);
                let actual = self.binary_types(*op, left, right, value.span(), None);
                // Writing through a rejected target would repeat its diagnostic.
                if invalid_target {
                    return Type::Nil;
                }
                return self.assignment(target, element, value, actual, value.span());
            }
            Stmt::Assign { target, value } => {
                // As in emission, an indexed target's receiver and index run
                // before the value and are checked with the facts known then.
                let element = self.element_target(target);
                let actual = if matches!(value.node(), Expr::Array(items) if items.is_empty()) {
                    let expected = match target.node() {
                        Expr::Index(..) => element.clone().unwrap_or(Type::Poison),
                        _ => self.expr(target),
                    };
                    if matches!(expected.non_nil(), Type::Array(_)) {
                        expected.non_nil()
                    } else {
                        self.expr(value)
                    }
                } else {
                    self.expr(value)
                };
                return self.assignment(target, element, value, actual, value.span());
            }
            Stmt::Use {
                module,
                from,
                names,
                aliases,
                namespace,
                wildcard,
            } => self.import(&UseDecl {
                span: self.location.clone(),
                module: module.clone(),
                from: from.clone(),
                names: names.clone(),
                aliases: aliases.clone(),
                namespace: *namespace,
                wildcard: *wildcard,
            }),
            Stmt::Return(value) => {
                if !self.in_function {
                    self.error(
                        "E1004",
                        "`return` requires an enclosing function body",
                        stmt.span(),
                    );
                }
                if self.in_cleanup {
                    self.error("E1004", "`return` cannot leave a defer block", stmt.span());
                }
                let expected_result = value
                    .as_ref()
                    .and_then(|_| self.result_type.clone())
                    .map(|annotation| self.annotation_type(&annotation));
                let actual = value.as_ref().map_or(Type::Nil, |expr| {
                    self.expr_with_expected_result(expr, expected_result.as_ref())
                });
                if let Some(expected) = self.result_type.clone() {
                    self.assignable(expected, actual, stmt.span(), "return value");
                } else if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        "cannot infer array element type for an empty return value; add a result type annotation",
                        stmt.span(),
                    );
                }
                return Type::Never;
            }
            Stmt::Expr(expr) => {
                self.discard(expr);
                let actual = self.expr(expr);
                if contains_unresolved_empty_array(&actual) {
                    self.error(
                        "E2001",
                        "cannot infer array element type for an empty expression; add a type context",
                        expr.span(),
                    );
                }
                if actual == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::Crash { value, .. } => {
                self.expr(value);
                return Type::Never;
            }
            Stmt::Defer { body } => {
                let loops = std::mem::replace(&mut self.loops, 0);
                let in_cleanup = std::mem::replace(&mut self.in_cleanup, true);
                // Cleanup runs later, after writes this point cannot see.
                let flow = self.flow_facts.clone();
                self.flow_facts.normal_types.retain(|id, _| {
                    self.scopes
                        .iter()
                        .flat_map(|scope| scope.bindings.values())
                        .chain(self.globals.values())
                        .any(|binding| binding.id == *id && !binding.mutable)
                });
                self.discarded_block(body);
                self.flow_facts = flow;
                self.loops = loops;
                self.in_cleanup = in_cleanup;
            }
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.discard_tail(body);
                self.discard_tail(catch_block);
                if self.try_blocks(body, catch_name, catch_block, None, false) == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                let (condition, true_flow, false_flow) = self.condition(cond);
                let reachable = self.normal_reachable;
                let base_flow = self.flow_facts.clone();
                let then_reachable = reachable && !matches!(cond.node(), Expr::Bool(false));
                self.normal_reachable = then_reachable;
                self.flow_facts = true_flow;
                let a = self.discarded_block(then_block);
                let then_flow = self.flow_facts.clone();
                self.flow_facts = false_flow;
                let else_reachable = reachable && !matches!(cond.node(), Expr::Bool(true));
                self.normal_reachable = else_reachable;
                let b = else_block
                    .as_ref()
                    .map_or(Type::Nil, |block| self.discarded_block(block));
                let else_flow = self.flow_facts.clone();
                self.normal_reachable = reachable;
                let mut exits = Vec::new();
                if then_reachable && a != Type::Never {
                    exits.push(then_flow);
                }
                if else_reachable && b != Type::Never {
                    exits.push(else_flow);
                }
                if !exits.is_empty() {
                    self.join_flow_exits(exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if condition == Type::Never || a.join(b) == Type::Never {
                    return Type::Never;
                }
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                let ty = self.expr(value);
                let base_flow = self.flow_facts.clone();
                self.scopes.push(Scope::default());
                self.define(
                    name,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty,
                        mutable: true,
                    },
                    value.span(),
                );
                let then_result = self.discarded_block(then_block);
                let then_flow = self.flow_facts.clone();
                self.scopes.pop();
                self.flow_facts = base_flow.clone();
                if let Some(block) = else_block {
                    let else_result = self.discarded_block(block);
                    let else_flow = self.flow_facts.clone();
                    let mut exits = Vec::new();
                    if then_result != Type::Never {
                        exits.push(then_flow);
                    }
                    if else_result != Type::Never {
                        exits.push(else_flow);
                    }
                    if !exits.is_empty() {
                        self.join_flow_exits(exits);
                    } else {
                        self.flow_facts = base_flow;
                    }
                } else if then_result != Type::Never {
                    self.join_flow_exits([then_flow, base_flow]);
                } else {
                    self.flow_facts = base_flow;
                }
            }
            Stmt::While { cond, body } => {
                let mut writes = Writes::default();
                writes.expr(cond);
                writes.block(body);
                self.invalidate_region_flow_facts(&writes);
                let (condition, true_flow, false_flow) = self.condition(cond);
                self.flow_facts = true_flow;
                self.loops += 1;
                self.loop_breaks.push(Vec::new());
                self.discarded_block(body);
                let mut exits = self.loop_breaks.pop().unwrap();
                self.loops -= 1;
                let forever = matches!(cond.node(), Expr::Bool(true));
                let has_break = !exits.is_empty();
                if !forever || !has_break {
                    exits.push(false_flow);
                }
                self.join_flow_exits(exits);
                if condition == Type::Never || (forever && !has_break) {
                    return Type::Never;
                }
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let (range_type, step_type) = self.range_types(start, end, step.as_deref());
                self.scopes.push(Scope::default());
                self.define(
                    var,
                    Binding {
                        id: 0,
                        error_only: false,
                        inferred: None,
                        ty: range_type.clone(),
                        mutable: true,
                    },
                    start.span(),
                );
                self.loop_body(body);
                self.scopes.pop();
                if range_type == Type::Never || step_type == Some(Type::Never) {
                    return Type::Never;
                }
            }
            Stmt::ForEach {
                pattern,
                iterable,
                body,
            } => {
                let collection = self.expr(iterable);
                let collection = self.require_value(collection, iterable, "iterate over");
                if self.in_comptime {
                    self.error(
                        "E4002",
                        "collection iteration is available only in runtime functions",
                        stmt.span(),
                    );
                }
                let (element, iteration) = match &collection {
                    Type::Array(element) => ((**element).clone(), None),
                    Type::EmptyArray | Type::Dynamic => (Type::Dynamic, None),
                    Type::Primitive(VarType::Str, _) => (Type::concrete(VarType::Char), None),
                    Type::ErrorValue | Type::Never => (Type::Never, None),
                    Type::Poison => (Type::Poison, None),
                    _ => self
                        .custom_iteration(&collection, iterable.span().cloned())
                        .map(|(element, methods)| (element, Some(methods)))
                        .unwrap_or((Type::Poison, None)),
                };
                self.iteration_calls.insert(
                    statement_site(self.current_function.as_deref(), stmt),
                    vec![iteration.clone()],
                );
                self.access_facts
                    .iterations
                    .insert(stmt.node() as *const Stmt as usize, vec![iteration]);
                let mut bindings = Vec::new();
                self.collect_binding_types(pattern, element, iterable.span(), &mut bindings);
                self.scopes.push(Scope::default());
                for (name, ty, span) in bindings {
                    self.define(
                        &name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty,
                            mutable: true,
                        },
                        span.as_ref(),
                    );
                }
                self.loop_body(body);
                self.scopes.pop();
                if collection == Type::Never || collection == Type::ErrorValue {
                    return Type::Never;
                }
            }
            Stmt::ForZip { clauses, body } => {
                let mut bindings = Vec::with_capacity(clauses.len());
                let mut iteration_methods = Vec::with_capacity(clauses.len());
                let mut never = false;
                for clause in clauses {
                    match clause {
                        ForClause::Range {
                            var,
                            start,
                            end,
                            step,
                        } => {
                            let (range_type, step_type) =
                                self.range_types(start, end, step.as_deref());
                            never |= range_type == Type::Never || step_type == Some(Type::Never);
                            bindings.push((var.clone(), range_type, start.span().cloned()));
                            iteration_methods.push(None);
                        }
                        ForClause::Each { pattern, iterable } => {
                            let collection = self.expr(iterable);
                            let collection =
                                self.require_value(collection, iterable, "iterate over");
                            if self.in_comptime {
                                self.error(
                                    "E4002",
                                    "collection iteration is available only in runtime functions",
                                    stmt.span(),
                                );
                            }
                            let (element, methods) = match &collection {
                                Type::Array(element) => ((**element).clone(), None),
                                Type::EmptyArray | Type::Dynamic => (Type::Dynamic, None),
                                Type::Primitive(VarType::Str, _) => {
                                    (Type::concrete(VarType::Char), None)
                                }
                                Type::ErrorValue | Type::Never => (Type::Never, None),
                                Type::Poison => (Type::Poison, None),
                                _ => self
                                    .custom_iteration(&collection, iterable.span().cloned())
                                    .map(|(element, methods)| (element, Some(methods)))
                                    .unwrap_or((Type::Poison, None)),
                            };
                            iteration_methods.push(methods);
                            never |= collection == Type::Never || collection == Type::ErrorValue;
                            self.collect_binding_types(
                                pattern,
                                element,
                                iterable.span(),
                                &mut bindings,
                            );
                        }
                    }
                }
                self.iteration_calls.insert(
                    statement_site(self.current_function.as_deref(), stmt),
                    iteration_methods.clone(),
                );
                self.access_facts
                    .iterations
                    .insert(stmt.node() as *const Stmt as usize, iteration_methods);
                self.scopes.push(Scope::default());
                for (name, ty, span) in bindings {
                    self.define(
                        &name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty,
                            mutable: true,
                        },
                        span.as_ref(),
                    );
                }
                self.loop_body(body);
                self.scopes.pop();
                if never {
                    return Type::Never;
                }
            }
            Stmt::Break if self.loops == 0 => {
                self.error("E1004", "`break` used outside of a loop", stmt.span())
            }
            Stmt::Break => {
                if self.normal_reachable {
                    let flow = self.flow_facts.clone();
                    // A `break` may run cleanup before reaching the loop exit.
                    if self.writes.has_defer {
                        self.invalidate_call_flow_facts();
                    }
                    let exit = std::mem::replace(&mut self.flow_facts, flow);
                    self.loop_breaks.last_mut().unwrap().push(exit);
                }
                return Type::Never;
            }
            Stmt::Continue if self.loops == 0 => {
                self.error("E1005", "`continue` used outside of a loop", stmt.span())
            }
            Stmt::Continue => return Type::Never,
        }
        Type::Nil
    }

    /// Check the body of a `for` loop; it may run any number of times.
    fn loop_body(&mut self, body: &Block) {
        let mut writes = Writes::default();
        writes.block(body);
        self.invalidate_region_flow_facts(&writes);
        let entry = self.flow_facts.clone();
        self.loops += 1;
        self.loop_breaks.push(Vec::new());
        self.discarded_block(body);
        self.loop_breaks.pop();
        self.loops -= 1;
        self.flow_facts = entry;
    }

    /// Report an operation that needs a value on an operand that may be nil.
    /// Returns the poisoned type after reporting, otherwise the operand type.
    fn require_value(&mut self, ty: Type, operand: &Expr, action: &str) -> Type {
        if !ty.may_be_nil() {
            return ty;
        }
        let name = ty.display();
        let message = if ty == Type::Nil {
            format!("cannot {action} a value that is `nil`")
        } else {
            format!("cannot {action} a value of type `{name}` that may be `nil`")
        };
        let help = match operand.node() {
            Expr::Ident(binding) => {
                let spelled = self.source_binding_name(binding, operand);
                if self.writes.deferred.contains(binding) {
                    format!(
                        "`{spelled}` is assigned in a `defer` block and cannot be narrowed; copy it to a `const` and check that"
                    )
                } else {
                    format!(
                        "check `{spelled} != nil` first, or supply a fallback with `{spelled} ?? value`"
                    )
                }
            }
            _ => "bind it to a `const` and check that with `!= nil`, or supply a fallback with `??`; fields and elements are not narrowed".to_owned(),
        };
        self.diagnostics.push(
            Diagnostic::new(
                "E2004",
                message,
                operand.span().cloned().or_else(|| self.location.clone()),
            )
            .help(help),
        );
        Type::Poison
    }

    /// Spells a referenced binding as the source does. Globals carry linked
    /// symbols, and a help message has to quote text that is valid in place:
    /// the short name, a module path, or an import alias.
    fn source_binding_name(&self, binding: &str, reference: &Expr) -> String {
        if !self.access_facts.symbol_names.contains_key(binding) {
            return binding.to_owned();
        }
        // The receiver of a member access has no span of its own; the
        // enclosing postfix expression starts with it.
        reference
            .span()
            .or(self.location.as_ref())
            .and_then(|span| span.source.text.get(span.range.clone()))
            .and_then(|text| {
                let end = text
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                    .unwrap_or(text.len());
                let (path, rest) = text.split_at(end);
                (!path.is_empty() && (rest.is_empty() || rest.starts_with(['.', '['])))
                    .then(|| path.to_owned())
            })
            // The linked symbol is displayed with its module path.
            .unwrap_or_else(|| binding.to_owned())
    }

    /// The built-in length operations count array elements and string scalars.
    /// A byte buffer's length is a `bytes` module operation.
    fn reject_bytes_length(&mut self, operation: &str, span: Option<&SourceSpan>) {
        self.diagnostics.push(
            Diagnostic::new(
                "E2001",
                format!("`{operation}` does not accept `bytes`"),
                span.cloned().or_else(|| self.location.clone()),
            )
            .help("import `bytes` and call `bytes::len(buffer)` for the byte length"),
        );
    }

    fn binary_types(
        &mut self,
        op: BinOp,
        a: Type,
        b: Type,
        span: Option<&SourceSpan>,
        operands: Option<(&Expr, &Expr)>,
    ) -> Type {
        if a == Type::Poison || b == Type::Poison {
            return Type::Poison;
        }
        if a == Type::Never || b == Type::Never {
            return Type::Never;
        }
        if a == Type::ErrorValue || b == Type::ErrorValue {
            return Type::ErrorValue;
        }
        if matches!(op, BinOp::Eq | BinOp::NotEq) {
            return Type::concrete(VarType::Bool);
        }
        let array = |ty: &Type| matches!(ty, Type::Array(_) | Type::EmptyArray);
        if matches!(op, BinOp::Shl | BinOp::Shr) && (array(&a) || array(&b)) {
            let (shift, method) = if op == BinOp::Shl {
                ("<<", "array.append(value)")
            } else {
                (">>", "array.prepend(value)")
            };
            self.error(
                "E2002",
                format!(
                    "`{shift}` is an integer bit shift, not array mutation; use `{method}` to add an element"
                ),
                span,
            );
            return Type::Poison;
        }
        if a == Type::Dynamic || b == Type::Dynamic {
            return Type::Dynamic;
        }
        match (op, a.clone(), b.clone()) {
            (BinOp::Add, Type::Array(a), Type::Array(b)) if a == b => Type::Array(a),
            (
                BinOp::Shl | BinOp::Shr,
                Type::Primitive(x @ (VarType::Int | VarType::UInt | VarType::BigInt), false),
                Type::Primitive(VarType::Int | VarType::UInt, false),
            ) => Type::concrete(x),
            (
                BinOp::Add,
                Type::Primitive(VarType::Str, false),
                Type::Primitive(VarType::Str, false),
            ) => Type::concrete(VarType::Str),
            (
                BinOp::And | BinOp::Or | BinOp::Xor,
                Type::Primitive(VarType::Bool, false),
                Type::Primitive(VarType::Bool, false),
            ) => Type::concrete(VarType::Bool),
            (
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
                Type::Primitive(
                    x @ (VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float),
                    false,
                ),
                Type::Primitive(
                    y @ (VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float),
                    false,
                ),
            ) => {
                if x == y {
                    Type::concrete(x)
                } else if x != VarType::BigInt
                    && y != VarType::BigInt
                    && (x == VarType::Float || y == VarType::Float)
                {
                    Type::concrete(VarType::Float)
                } else {
                    let mut diagnostic = Diagnostic::new(
                        "E2002",
                        "mixed integer arithmetic requires an explicit conversion",
                        span.cloned().or_else(|| self.location.clone()),
                    );
                    let suggestions = operands.map_or_else(Vec::new, |(left, right)| {
                        mixed_numeric_suggestions(x, y, left, right)
                    });
                    if suggestions.is_empty() {
                        diagnostic = diagnostic.help(
                            "make the operand types match with an explicit checked conversion",
                        );
                    } else {
                        for suggestion in suggestions {
                            diagnostic = diagnostic.suggestion(suggestion);
                        }
                    }
                    self.diagnostics.push(diagnostic);
                    Type::Poison
                }
            }
            (
                BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq,
                Type::Primitive(
                    x @ (VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float),
                    false,
                ),
                Type::Primitive(
                    y @ (VarType::Int | VarType::UInt | VarType::BigInt | VarType::Float),
                    false,
                ),
            ) if x == y
                || x == VarType::UInt
                || y == VarType::UInt
                || x == VarType::BigInt
                || y == VarType::BigInt
                || x == VarType::Float
                || y == VarType::Float =>
            {
                Type::concrete(VarType::Bool)
            }
            (
                BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq,
                Type::Primitive(VarType::Char, false),
                Type::Primitive(VarType::Char, false),
            ) => Type::concrete(VarType::Bool),
            _ => {
                let mut diagnostic = Diagnostic::new(
                    "E2002",
                    format!(
                        "invalid operands {} and {} for {op:?}",
                        a.display(),
                        b.display()
                    ),
                    span.cloned().or_else(|| self.location.clone()),
                );
                if a.may_be_nil() || b.may_be_nil() {
                    diagnostic = diagnostic.help(
                        "an operand may be `nil`; check it with `!= nil` or supply a fallback with `??`",
                    );
                }
                self.diagnostics.push(diagnostic);
                Type::Poison
            }
        }
    }

    /// A rechecked body can widen an operand after an earlier pass selected a
    /// struct equality implementation, so each visit starts without one.
    fn forget_equality_call(&mut self, expr: &Expr) {
        let site = expression_site(self.current_function.as_deref(), expr);
        self.method_calls.remove(&site);
        self.call_specializations.remove(&site);
        let id = crate::mutability::expr_id(expr);
        self.access_facts.calls.remove(&id);
        self.access_facts.specializations.remove(&id);
    }

    /// Resolves `==` and `!=` between two statically known structs to the
    /// `equals` method of their `eq::Eq[Self]` implementation.
    fn struct_equality(
        &mut self,
        op: BinOp,
        (left_expr, left): (&Expr, Type),
        (right_expr, right): (&Expr, Type),
        expr: &Expr,
    ) -> Type {
        let operator = if op == BinOp::Eq { "==" } else { "!=" };
        if left != right {
            // Unknown type arguments keep the dynamic runtime comparison.
            if let (Type::Struct(left_id, _, a), Type::Struct(right_id, _, b)) = (&left, &right)
                && left_id == right_id
                && !a.iter().chain(b).all(is_concrete_trait_argument)
            {
                return Type::concrete(VarType::Bool);
            }
            let message = format!(
                "cannot compare different struct types `{}` and `{}` with `{operator}`",
                self.source_type_name(&left),
                self.source_type_name(&right)
            );
            self.error("E2002", message, expr.span());
            return Type::Poison;
        }
        // Without resolved names (for example while reporting earlier front-end
        // errors) the standard trait cannot be identified.
        if self.access_facts.symbol_names.is_empty() {
            return Type::concrete(VarType::Bool);
        }
        let arguments = std::slice::from_ref(&left);
        let Some(equality) = self
            .standard_trait_symbol("eq::Eq")
            .filter(|name| self.implements_trait(&left, name, arguments))
        else {
            let name = self.source_type_name(&left);
            // An extension in the declaring module spells the type unqualified.
            let base = name.split('[').next().unwrap_or(&name);
            let local = &name[base.rfind("::").map_or(0, |index| index + 2)..];
            self.diagnostics.push(
                Diagnostic::new(
                    "E2001",
                    format!(
                        "type `{name}` does not implement `eq::Eq[{name}]` required by `{operator}`"
                    ),
                    expr.span().cloned().or_else(|| self.location.clone()),
                )
                .help(format!(
                    "import `eq` and add `extend {local} with eq::Eq[{local}]` defining \
                     `fn equals(self, other: {local}) -> bool`"
                )),
            );
            return Type::Poison;
        };
        // A concrete implementation registers its method under the target type.
        // Otherwise the matching generic implementation supplies it.
        let key = (left.clone(), equality.clone(), arguments.to_vec());
        let method = if self.trait_implementations.contains(&key) {
            self.extension_methods
                .get(&(left.clone(), "equals".to_owned()))
                .copied()
        } else {
            self.generic_trait_implementations
                .iter()
                .find(|extension| {
                    self.generic_trait_implementation_arguments(
                        extension,
                        &left,
                        &equality,
                        Some(arguments),
                        &mut HashSet::new(),
                    )
                    .is_some()
                })
                .and_then(|extension| {
                    extension
                        .methods
                        .iter()
                        .find(|method| method.name == "equals")
                })
        };
        let Some(method) = method else {
            // The implementation was rejected with its own diagnostic.
            return Type::Poison;
        };
        let function = &method.function;
        self.invalidate_call_flow_facts();
        self.access_facts
            .calls
            .insert(crate::mutability::expr_id(expr), function.name.clone());
        if function.type_params.is_empty() {
            self.method_calls.insert(
                expression_site(self.current_function.as_deref(), expr),
                function.name.clone(),
            );
        } else {
            let operands = [
                CallArgument::positional(left_expr.clone()),
                CallArgument::positional(right_expr.clone()),
            ];
            self.generic_call(&function.name, function, &operands, vec![left, right], expr);
        }
        Type::concrete(VarType::Bool)
    }

    /// Spells a type the way the source declares it, for diagnostics.
    fn source_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Struct(_, symbol, arguments) => {
                let name = self.access_facts.symbol_names.get(symbol).unwrap_or(symbol);
                if arguments.is_empty() {
                    name.clone()
                } else {
                    let arguments = arguments
                        .iter()
                        .map(|argument| self.source_type_name(argument))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{name}[{arguments}]")
                }
            }
            Type::Array(element) => format!("{}[]", self.source_type_name(element)),
            other => other.display(),
        }
    }

    /// Check the receiver and index of an element assignment and return the
    /// element type. Returns nothing for other targets and rejected receivers.
    fn element_target(&mut self, target: &Expr) -> Option<Type> {
        let Expr::Index(receiver, _) = target.node() else {
            return None;
        };
        let diagnostics = self.diagnostics.len();
        let entry = self.flow_facts.clone();
        if matches!(
            self.expr(receiver).non_nil(),
            Type::Enum(_, _) | Type::ErrorValue
        ) {
            self.error(
                "E1003",
                "enum payload bindings are immutable",
                target.span(),
            );
        }
        // A rejected receiver would be reported again by the target.
        if self.diagnostics.len() > diagnostics {
            return None;
        }
        self.flow_facts = entry;
        Some(self.expr(target))
    }

    fn assignment(
        &mut self,
        target: &Expr,
        element: Option<Type>,
        value: &Expr,
        mut actual: Type,
        span: Option<&SourceSpan>,
    ) -> Type {
        match target.node() {
            Expr::Ident(name) => {
                let binding = self.local(name).or_else(|| {
                    self.resolve(name, target.span()).and_then(|n| {
                        self.access_facts
                            .globals
                            .insert(crate::mutability::expr_id(target), n.clone());
                        self.globals.get(&n).cloned()
                    })
                });
                if let Some(binding) = binding {
                    let expected = binding
                        .inferred
                        .as_ref()
                        .map(|(_, initial)| initial)
                        .unwrap_or(&binding.ty)
                        .clone();
                    actual = self.contextualized_literal_type(value, actual, &expected);
                    // Writes invalidate pattern knowledge, including
                    // overlays in enclosing arms. Do not infer new facts
                    // from conditional writes without a CFG join.
                    if binding.mutable && self.normal_reachable {
                        self.flow_facts.forget(binding.id);
                        for scope in &mut self.scopes {
                            for entry in scope.bindings.values_mut() {
                                if entry.id == binding.id {
                                    entry.error_only = false;
                                }
                            }
                        }
                    }
                    if !binding.mutable {
                        self.error(
                            "E1003",
                            format!("cannot assign to const `{name}`"),
                            target.span(),
                        );
                    }
                    if let Some((id, initial)) = binding.inferred {
                        if actual == Type::Dynamic {
                            self.dynamic_bindings.insert(id);
                        } else if initial != Type::Dynamic {
                            self.assignable_type(
                                initial,
                                actual.clone(),
                                span,
                                &format!("`{name}`"),
                            );
                        }
                    } else {
                        self.assignable_type(
                            binding.ty,
                            actual.clone(),
                            span,
                            &format!("`{name}`"),
                        );
                    }
                } else {
                    self.error(
                        "E1002",
                        format!("cannot assign to undefined variable `{name}`"),
                        target.span(),
                    );
                }
            }
            Expr::Index(..) => {
                let Some(expected) = element else {
                    return Type::Nil;
                };
                self.assignable_type(expected, actual.clone(), span, "field");
            }
            _ => self.error(
                "E1003",
                "invalid assignment target: expected a variable or indexed expression",
                target.span(),
            ),
        }
        if actual == Type::Never {
            Type::Never
        } else {
            Type::Nil
        }
    }

    /// Check a condition and return the facts of its true and false exits. The
    /// current facts are left at the merge of both.
    fn condition(&mut self, expr: &Expr) -> (Type, FlowFacts, FlowFacts) {
        let (actual, true_flow, false_flow) = self.branch(expr);
        // Dynamic conditions remain runtime checked; a statically invalid condition is rejected.
        if !matches!(actual, Type::Dynamic | Type::ErrorValue | Type::Poison) {
            self.assignable(
                TypeAnnotation {
                    kind: TypeKind::Primitive(VarType::Bool),
                    nilable: false,
                },
                actual.clone(),
                expr.span(),
                "condition",
            );
        }
        (actual, true_flow, false_flow)
    }

    fn branch(&mut self, expr: &Expr) -> (Type, FlowFacts, FlowFacts) {
        self.branch_flows = None;
        let actual = self.expr(expr);
        match self.branch_flows.take() {
            Some((id, true_flow, false_flow)) if id == crate::mutability::expr_id(expr) => {
                (actual, true_flow, false_flow)
            }
            _ => (actual, self.flow_facts.clone(), self.flow_facts.clone()),
        }
    }

    /// `&&` and `||` evaluate the right operand only on one exit of the left.
    fn logical(&mut self, op: BinOp, left: &Expr, right: &Expr, expr: &Expr) -> Type {
        let (left_type, left_true, left_false) = self.branch(left);
        let (entry, skipped) = if op == BinOp::And {
            (left_true, left_false)
        } else {
            (left_false, left_true)
        };
        self.flow_facts = entry;
        let (right_type, right_true, right_false) = self.branch(right);
        let (true_flow, false_flow) = if op == BinOp::And {
            (right_true, FlowFacts::join([skipped, right_false]))
        } else {
            (FlowFacts::join([skipped, right_true]), right_false)
        };
        self.join_flow_exits([true_flow.clone(), false_flow.clone()]);
        self.branch_flows = Some((crate::mutability::expr_id(expr), true_flow, false_flow));
        self.binary_types(op, left_type, right_type, expr.span(), Some((left, right)))
    }

    /// Record the facts established by `binding == nil` and `binding != nil`.
    fn nil_comparison(&mut self, op: BinOp, left: &Expr, right: &Expr, expr: &Expr) {
        let value = match (left.node(), right.node()) {
            (value, Expr::Nil) if !matches!(value, Expr::Nil) => left,
            (Expr::Nil, _) => right,
            _ => return,
        };
        let unproven = self.flow_facts.clone();
        self.narrow_non_nil(value);
        let proven = std::mem::replace(&mut self.flow_facts, unproven.clone());
        let (true_flow, false_flow) = if op == BinOp::NotEq {
            (proven, unproven)
        } else {
            (unproven, proven)
        };
        self.branch_flows = Some((crate::mutability::expr_id(expr), true_flow, false_flow));
    }

    fn try_blocks(
        &mut self,
        body: &Block,
        name: &str,
        catch: &Block,
        expected: Option<&Type>,
        used: bool,
    ) -> Type {
        let base_flow = self.flow_facts.clone();
        let success = self.block_expecting(body, expected);
        let success_flow = self.flow_facts.clone();
        self.flow_facts = base_flow;
        // A failure may happen after any of the body's writes.
        let mut writes = Writes::default();
        writes.block(body);
        self.invalidate_region_flow_facts(&writes);
        self.scopes.push(Scope::default());
        self.define(
            name,
            Binding {
                id: 0,
                error_only: false,
                inferred: None,
                ty: Type::ErrorValue,
                mutable: false,
            },
            None,
        );
        let failure = self.block_expecting(catch, expected);
        let failure_flow = self.flow_facts.clone();
        self.scopes.pop();
        let exits = [
            (success != Type::Never).then_some(success_flow),
            (failure != Type::Never).then_some(failure_flow),
        ]
        .into_iter()
        .flatten();
        self.join_flow_exits(exits);
        self.join_values(success, failure, used)
    }

    fn expr_with_expected_result(&mut self, expr: &Expr, expected: Option<&Type>) -> Type {
        if let Some(expected) = expected {
            self.contextualize_integer_literals(expr, expected);
        }
        let Expr::AssociatedCall {
            target,
            method,
            type_arguments,
            args,
        } = expr.node()
        else {
            let Some(expected) = expected else {
                return self.expr(expr);
            };
            match expr.node() {
                Expr::Call(..) | Expr::SpecializedCall(..) => {
                    self.result_hint = Some((crate::mutability::expr_id(expr), expected.clone()));
                }
                Expr::Array(_)
                | Expr::Tuple(_)
                | Expr::Block(_)
                | Expr::Capture(_)
                | Expr::If { .. }
                | Expr::Match { .. }
                | Expr::Try { .. }
                | Expr::Fallback { .. }
                    if contains_nullable(expected) =>
                {
                    self.value_hint = Some((crate::mutability::expr_id(expr), expected.clone()));
                }
                _ => {}
            }
            let actual = self.expr(expr);
            return widened(expr, actual, expected);
        };
        let previous = self.location.clone();
        if let Some(span) = expr.span() {
            self.location = Some(span.clone());
        }
        let result = self.associated_call(target, method, type_arguments, args, expr, expected);
        c_record_kind(
            &mut self.access_facts,
            crate::mutability::expr_id(expr),
            result.clone(),
        );
        self.location = previous;
        result
    }

    fn associated_call(
        &mut self,
        target: &TypeAnnotation,
        method: &str,
        type_arguments: &[TypeAnnotation],
        args: &[CallArgument],
        expr: &Expr,
        expected_result: Option<&Type>,
    ) -> Type {
        let diagnostics_start = self.diagnostics.len();
        let result =
            self.associated_call_inner(target, method, type_arguments, args, expr, expected_result);
        if self.diagnostics.len() > diagnostics_start {
            Type::Poison
        } else {
            result
        }
    }

    fn associated_call_inner(
        &mut self,
        target: &TypeAnnotation,
        method: &str,
        type_arguments: &[TypeAnnotation],
        args: &[CallArgument],
        expr: &Expr,
        expected_result: Option<&Type>,
    ) -> Type {
        let Some(target_name) = associated_type_name(target) else {
            self.error(
                "E2001",
                "associated call target must be a nominal type",
                expr.span(),
            );
            return Type::Poison;
        };
        if !self.constructors.contains_key(target_name) && !self.enums.contains_key(target_name) {
            self.error(
                "E2001",
                format!("type `{target_name}` cannot define associated functions"),
                expr.span(),
            );
            return Type::Poison;
        }
        let candidates: Vec<_> = self
            .associated_extension_methods
            .iter()
            .filter_map(|(extension, candidate)| {
                (candidate.name == method
                    && associated_type_name(&extension.target) == Some(target_name))
                .then_some(*candidate)
            })
            .collect();
        let Some(extension_method) = candidates.first().copied() else {
            self.error(
                "E1002",
                format!("type `{target_name}` has no associated function `{method}()`"),
                expr.span(),
            );
            return Type::Poison;
        };
        if candidates.len() > 1 {
            self.error(
                "E2001",
                format!("associated function `{target_name}::{method}()` is ambiguous"),
                expr.span(),
            );
            return Type::Poison;
        }

        let function = extension_method.function.clone();
        if function.type_params.is_empty() {
            let argument_bindings = crate::arguments::bind(method, &function.params, args);
            for (argument_index, argument) in args.iter().enumerate() {
                if let Some(parameter_index) = argument_bindings.by_argument[argument_index]
                    && let Some(expected) = function.params[parameter_index].ty.as_ref()
                {
                    let expected_type = self.annotation_type(expected);
                    self.contextualize_integer_literals(&argument.value, &expected_type);
                }
            }
        }
        let mut hints = vec![None; args.len()];
        if function.type_params.is_empty() {
            let argument_bindings = crate::arguments::bind(method, &function.params, args);
            for (hint, parameter) in hints.iter_mut().zip(argument_bindings.by_argument) {
                if let Some(ty) = parameter.and_then(|index| function.params[index].ty.as_ref()) {
                    *hint = Some(self.annotation_type(ty)).filter(contains_nullable);
                }
            }
        }
        let mut actual = Vec::with_capacity(args.len());
        for (argument, hint) in args.iter().zip(&hints) {
            actual.push(self.expr_with_expected_result(&argument.value, hint.as_ref()));
        }
        self.invalidate_call_flow_facts();
        self.access_facts
            .calls
            .insert(crate::mutability::expr_id(expr), function.name.clone());
        if function.comptime != self.in_comptime {
            let (caller_phase, callee_phase) = if self.in_comptime {
                ("comptime", "runtime")
            } else {
                ("runtime", "comptime")
            };
            self.error(
                "E4002",
                format!(
                    "{caller_phase} code cannot call {callee_phase}-only associated function `{method}()`"
                ),
                expr.span(),
            );
            return Type::Poison;
        }
        if actual.contains(&Type::Poison) {
            return Type::Poison;
        }
        if actual.contains(&Type::Never) {
            return Type::Never;
        }
        let associated_name = format!("{target_name}::{method}");

        if !function.type_params.is_empty() {
            return self.generic_call_with_result_context(
                &associated_name,
                &function,
                args,
                actual,
                expr,
                expected_result,
            );
        }
        if !type_arguments.is_empty() {
            self.error(
                "E2001",
                format!("associated function `{method}()` does not accept generic arguments"),
                expr.span(),
            );
            return Type::Poison;
        }

        let bindings = crate::arguments::bind(&associated_name, &function.params, args);
        for error in bindings.errors {
            self.error(
                "E1005",
                error.message,
                error
                    .argument
                    .and_then(|index| args[index].value.span())
                    .or(expr.span()),
            );
        }
        for (argument_index, actual_type) in actual.iter().enumerate() {
            let Some(parameter_index) = bindings.by_argument[argument_index] else {
                continue;
            };
            if let Some(expected) = &function.params[parameter_index].ty {
                let expected_type = self.annotation_type(expected);
                self.assignable(
                    expected.clone(),
                    widened(
                        &args[argument_index].value,
                        actual_type.clone(),
                        &expected_type,
                    ),
                    args[argument_index].value.span(),
                    &format!(
                        "argument `{}` of `{method}()`",
                        function.params[parameter_index].name
                    ),
                );
            }
        }
        self.method_calls.insert(
            expression_site(self.current_function.as_deref(), expr),
            function.name.clone(),
        );
        let result = function
            .result_type
            .as_ref()
            .map_or(Type::Dynamic, |ty| self.annotation_type(ty));
        if function.asynchronous {
            Type::Task(Box::new(result))
        } else {
            result
        }
    }

    fn expr(&mut self, expr: &Expr) -> Type {
        let previous = self.location.clone();
        if let Some(span) = expr.span() {
            self.location = Some(span.clone());
        }
        let result = self.expr_inner(expr);
        if result == Type::concrete(VarType::UInt) {
            self.uint_expressions
                .insert(crate::mutability::expr_id(expr));
        }
        use crate::mutability::expr_id;
        c_record_kind(&mut self.access_facts, expr_id(expr), result.clone());
        self.location = previous;
        result
    }

    fn range_types(
        &mut self,
        start: &Expr,
        end: &Expr,
        step: Option<&Expr>,
    ) -> (Type, Option<Type>) {
        let uint = Type::concrete(VarType::UInt);
        let bigint = Type::concrete(VarType::BigInt);
        let start_literal = integer_literal_tree(start);
        let end_literal = integer_literal_tree(end);
        let (mut start_type, mut end_type) =
            if start_literal == Some(true) || end_literal == Some(true) {
                (
                    self.range_operand_type(start, start_literal.is_some(), &bigint),
                    self.range_operand_type(end, end_literal.is_some(), &bigint),
                )
            } else if start_literal.is_some() && end_literal.is_none() {
                // Literal-only arithmetic has no flow facts to change while we
                // check the other bound for context. Lowering still emits start first.
                let diagnostics_start = self.diagnostics.len();
                let end_type = self.expr(end);
                let end_diagnostics = self.diagnostics.len() - diagnostics_start;
                let start_type = self.range_operand_type(start, true, &end_type);
                self.diagnostics[diagnostics_start..].rotate_left(end_diagnostics);
                (start_type, end_type)
            } else {
                let start_type = self.expr(start);
                let end_type = self.range_operand_type(end, end_literal.is_some(), &start_type);
                (start_type, end_type)
            };

        if start_type.may_be_nil() {
            start_type = self.require_value(start_type, start, "start a range at");
        }
        if end_type.may_be_nil() {
            end_type = self.require_value(end_type, end, "end a range at");
        }

        if start_type != bigint && end_type != bigint {
            if end_type == uint {
                start_type = self.contextualized_literal_type(start, start_type, &uint);
            }
            if start_type == uint {
                end_type = self.contextualized_literal_type(end, end_type, &uint);
            }
        }

        let range_type = match (&start_type, &end_type) {
            (Type::Primitive(VarType::Int, false), Type::Primitive(VarType::Int, false))
            | (Type::Primitive(VarType::UInt, false), Type::Primitive(VarType::UInt, false)) => {
                start_type.clone()
            }
            (Type::Primitive(VarType::BigInt, false), Type::Primitive(VarType::BigInt, false))
            | (Type::Primitive(VarType::BigInt, false), Type::Dynamic)
            | (Type::Dynamic, Type::Primitive(VarType::BigInt, false)) => bigint.clone(),
            (Type::Dynamic, _) | (_, Type::Dynamic) => Type::Dynamic,
            (Type::Poison, _) | (_, Type::Poison) => Type::Poison,
            (Type::Never, _) | (_, Type::Never) => Type::Never,
            (Type::ErrorValue, _) | (_, Type::ErrorValue) => Type::ErrorValue,
            _ => {
                self.error(
                    "E2002",
                    format!(
                        "range bounds must have one integer type (`int`, `uint`, or `bigint`), got {} and {}",
                        start_type.display(),
                        end_type.display()
                    ),
                    end.span(),
                );
                Type::Poison
            }
        };

        let step_type = step.map(|step| {
            let mut actual =
                self.range_operand_type(step, bigint_literal_expression(step), &range_type);
            if range_type == uint {
                actual = self.contextualized_literal_type(step, actual, &uint);
            }
            if actual.may_be_nil() {
                actual = self.require_value(actual, step, "step a range by");
            }
            if matches!(
                range_type,
                Type::Primitive(VarType::Int | VarType::UInt | VarType::BigInt, false)
            ) && actual != range_type
                && !matches!(
                    actual,
                    Type::Dynamic | Type::Poison | Type::Never | Type::ErrorValue
                )
            {
                self.error(
                    "E2002",
                    format!(
                        "range step must be `{}`, got {}",
                        range_type.display(),
                        actual.display()
                    ),
                    step.span(),
                );
                actual = Type::Poison;
            }
            actual
        });

        if range_type == bigint {
            self.bigint_range_expressions.extend(
                [start, end]
                    .into_iter()
                    .chain(step)
                    .map(crate::mutability::expr_id),
            );
        } else {
            self.bigint_range_expressions.extend(
                [(start, start_type), (end, end_type)]
                    .into_iter()
                    .filter(|(_, ty)| *ty == bigint)
                    .map(|(bound, _)| crate::mutability::expr_id(bound)),
            );
        }
        (range_type, step_type)
    }

    fn range_operand_type(&mut self, expr: &Expr, literal: bool, expected: &Type) -> Type {
        if literal && *expected == Type::concrete(VarType::BigInt) {
            self.contextualize_integer_literals(expr, expected);
            c_record_kind(
                &mut self.access_facts,
                crate::mutability::expr_id(expr),
                expected.clone(),
            );
            expected.clone()
        } else {
            self.expr(expr)
        }
    }

    fn contextualize_integer_literals(&mut self, expr: &Expr, expected: &Type) {
        if let Type::Nullable(inner) = expected {
            return self.contextualize_integer_literals(expr, inner);
        }
        match (expected, expr.node()) {
            (Type::Primitive(VarType::UInt, _), Expr::Int(value)) if *value >= 0 => {
                self.uint_literals.insert(crate::mutability::expr_id(expr));
            }
            (
                Type::Primitive(VarType::UInt, _),
                Expr::Binary(
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
                    left,
                    right,
                ),
            ) => {
                self.contextualize_integer_literals(left, expected);
                self.contextualize_integer_literals(right, expected);
            }
            (Type::Primitive(VarType::BigInt, _), Expr::Int(_) | Expr::UInt(_)) => {
                self.bigint_literals
                    .insert(crate::mutability::expr_id(expr));
            }
            (Type::Primitive(VarType::BigInt, _), Expr::Unary(UnOp::Neg, value)) => {
                self.contextualize_integer_literals(value, expected);
            }
            (
                Type::Primitive(VarType::BigInt, _),
                Expr::Binary(
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
                    left,
                    right,
                ),
            ) => {
                self.contextualize_integer_literals(left, expected);
                self.contextualize_integer_literals(right, expected);
            }
            (Type::Array(element), Expr::Array(items)) => {
                for item in items {
                    self.contextualize_integer_literals(item, element);
                }
            }
            (Type::Tuple(expected_items), Expr::Tuple(items))
                if expected_items.len() == items.len() =>
            {
                for (item, expected) in items.iter().zip(expected_items) {
                    self.contextualize_integer_literals(item, expected);
                }
            }
            _ => {}
        }
    }

    fn contextualized_literal_type(&mut self, expr: &Expr, actual: Type, expected: &Type) -> Type {
        if let Type::Nullable(inner) = expected {
            return self.contextualized_literal_type(expr, actual, inner);
        }
        match (expected, expr.node(), actual) {
            (Type::Primitive(VarType::BigInt, _), expression, actual)
                if bigint_literal_expression(expression)
                    && matches!(
                        actual,
                        Type::Primitive(VarType::Int | VarType::UInt | VarType::BigInt, false)
                    ) =>
            {
                self.contextualize_integer_literals(expr, expected);
                Type::concrete(VarType::BigInt)
            }
            (
                Type::Primitive(VarType::UInt, _),
                Expr::Int(value),
                Type::Primitive(VarType::Int, false),
            ) if *value >= 0 => {
                self.uint_literals.insert(crate::mutability::expr_id(expr));
                self.uint_expressions
                    .insert(crate::mutability::expr_id(expr));
                Type::concrete(VarType::UInt)
            }
            (Type::Primitive(VarType::UInt, _), Expr::Binary(op, left, right), actual)
                if actual == Type::concrete(VarType::Int)
                    && matches!(
                        op,
                        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
                    )
                    && integer_literal_expression(left)
                    && integer_literal_expression(right) =>
            {
                self.contextualize_integer_literals(expr, expected);
                self.uint_expressions
                    .insert(crate::mutability::expr_id(expr));
                Type::concrete(VarType::UInt)
            }
            (Type::Array(expected_element), Expr::Array(items), Type::Array(actual_element))
                if items.iter().all(integer_literal_expression)
                    && *actual_element == Type::concrete(VarType::Int)
                    && **expected_element == Type::concrete(VarType::UInt) =>
            {
                self.contextualize_integer_literals(expr, expected);
                Type::Array(expected_element.clone())
            }
            (Type::Array(expected_element), Expr::Array(items), Type::Array(_))
                if **expected_element == Type::concrete(VarType::BigInt)
                    && items.iter().all(bigint_literal_expression) =>
            {
                self.contextualize_integer_literals(expr, expected);
                Type::Array(expected_element.clone())
            }
            (_, _, actual) => actual,
        }
    }

    fn expr_inner(&mut self, expr: &Expr) -> Type {
        let id = crate::mutability::expr_id(expr);
        let hint = self
            .value_hint
            .take_if(|(hinted, _)| *hinted == id)
            .map(|(_, expected)| expected);
        let used = !self.discarded.contains(&id);
        // Record only a type of the literal's own shape: a skipped branch can
        // carry a literal that its context would reject.
        if let Some(expected) = hint.as_ref().map(Type::non_nil)
            && match (expr.node(), &expected) {
                (Expr::Array(_), Type::Array(_)) => true,
                (Expr::Tuple(items), Type::Tuple(types)) => items.len() == types.len(),
                _ => false,
            }
            && let Some(annotation) = self.type_annotation_for(&expected)
        {
            self.literal_types.insert(id, annotation);
        }
        match expr.node() {
            Expr::BracketApply(_, _) => {
                self.error(
                    "E1003",
                    "resolve brackets and callable names before semantic analysis",
                    expr.span(),
                );
                Type::Poison
            }
            Expr::Apply(callee, arguments) => {
                let callable = self.expr(callee);
                let callable = self.require_value(callable, callee, "call");
                let Type::Function(parameters, result) = callable else {
                    if callable != Type::Poison {
                        self.error(
                            "E2001",
                            format!("value of type {} is not callable", callable.display()),
                            expr.span(),
                        );
                    }
                    return Type::Poison;
                };
                self.access_facts.indirect_signatures.insert(
                    crate::mutability::expr_id(expr),
                    Type::Function(parameters.clone(), result.clone()),
                );
                if arguments.len() != parameters.len() {
                    self.error(
                        "E1005",
                        format!(
                            "call expects {} argument(s), got {}",
                            parameters.len(),
                            arguments.len()
                        ),
                        expr.span(),
                    );
                }
                let mut valid = true;
                for (index, argument) in arguments.iter().enumerate() {
                    if argument.name.is_some() {
                        self.error(
                            "E1005",
                            "indirect calls accept positional arguments only",
                            argument.value.span(),
                        );
                        valid = false;
                    }
                    let actual =
                        self.expr_with_expected_result(&argument.value, parameters.get(index));
                    if let Some(expected) = parameters.get(index)
                        && !type_compatible(expected, &actual)
                    {
                        let mut diagnostic = Diagnostic::new(
                            "E2001",
                            format!(
                                "argument {} expects {}, got {}",
                                index + 1,
                                expected.display(),
                                actual.display()
                            ),
                            argument.value.span().cloned(),
                        );
                        if let Some(help) = nullable_type_mismatch_help(expected, &actual) {
                            diagnostic = diagnostic.help(help);
                        }
                        self.diagnostics.push(diagnostic);
                        valid = false;
                    }
                }
                self.invalidate_call_flow_facts();
                if valid { *result } else { Type::Poison }
            }
            Expr::Located(_, _) => unreachable!("node removes locations"),
            Expr::Int(_)
                if self
                    .uint_literals
                    .contains(&crate::mutability::expr_id(expr)) =>
            {
                Type::concrete(VarType::UInt)
            }
            Expr::Int(_) => Type::concrete(VarType::Int),
            Expr::UInt(_) => Type::concrete(VarType::UInt),
            Expr::BigInt(_) => Type::concrete(VarType::BigInt),
            Expr::Float(_) => Type::concrete(VarType::Float),
            Expr::Char(_) => Type::concrete(VarType::Char),
            Expr::Bool(_) => Type::concrete(VarType::Bool),
            Expr::Str(_) => Type::concrete(VarType::Str),
            Expr::Bytes(_) => Type::Bytes,
            Expr::Interpolated(parts) => {
                let mut invalid = false;
                let mut never = false;
                for part in parts {
                    if let StringPart::Expression(value) = part {
                        let ty = self.expr(value);
                        invalid |= ty == Type::Poison;
                        never |= ty == Type::Never;
                    }
                }
                if invalid {
                    Type::Poison
                } else if never {
                    Type::Never
                } else {
                    Type::concrete(VarType::Str)
                }
            }
            Expr::Nil => Type::Nil,
            Expr::TypeLiteral(_) => {
                self.error(
                    "E4002",
                    "type values are only valid in compile-time type bindings",
                    expr.span(),
                );
                Type::Poison
            }
            Expr::Ident(name) => {
                if let Some(binding) = self.local(name) {
                    return self.effective_type(&binding);
                }
                let Some(resolved) = self.resolve(name, expr.span()) else {
                    return Type::Poison;
                };
                if let Some(binding) = self.globals.get(&resolved) {
                    self.access_facts
                        .globals
                        .insert(crate::mutability::expr_id(expr), resolved);
                    return self.effective_type(binding);
                }
                if let Some(function) = self.functions.get(&resolved).cloned() {
                    let function_name = function.name.clone();
                    let comptime = function.comptime;
                    if comptime != self.in_comptime {
                        let (caller_phase, callee_phase) = if self.in_comptime {
                            ("comptime", "runtime")
                        } else {
                            ("runtime", "comptime")
                        };
                        self.error(
                            "E4002",
                            format!(
                                "{caller_phase} code cannot use {callee_phase}-only function `{function_name}` as a value"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if function.asynchronous
                        || !function.type_params.is_empty()
                        || function
                            .params
                            .iter()
                            .any(|parameter| parameter.default.is_some())
                        || function.result_type.is_none()
                        || function
                            .params
                            .iter()
                            .any(|parameter| parameter.ty.is_none())
                    {
                        self.error(
                            "E4002",
                            format!("`{function_name}` cannot be used as a function value; async, generic, or defaulted callables are not supported here"),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    let callable = Type::Function(
                        function
                            .params
                            .iter()
                            .map(|parameter| {
                                parameter
                                    .ty
                                    .as_ref()
                                    .map_or(Type::Dynamic, |ty| self.annotation_type(ty))
                            })
                            .collect(),
                        Box::new(
                            function
                                .result_type
                                .as_ref()
                                .map_or(Type::Dynamic, |ty| self.annotation_type(ty)),
                        ),
                    );
                    self.access_facts
                        .function_values
                        .insert(function_name.clone(), callable.clone());
                    self.access_facts
                        .function_value_sites
                        .insert(crate::mutability::expr_id(expr), function_name);
                    return callable;
                }
                if let Some((_, fields)) = self.variants.get(&resolved) {
                    let payload = if fields.is_empty() { "" } else { "..." };
                    self.error(
                        "E1002",
                        format!(
                            "variant `{name}` is not a value; construct it with `{name}({payload})`"
                        ),
                        expr.span(),
                    );
                } else {
                    self.error("E1002", format!("undefined variable `{name}`"), expr.span());
                }
                Type::Poison
            }
            Expr::Closure {
                captures,
                params,
                result_type,
                body,
            } => {
                if self.in_comptime {
                    self.error(
                        "E4002",
                        "closures cannot be created in compile-time code",
                        expr.span(),
                    );
                    return Type::Poison;
                }
                let mut capture_bindings = Vec::with_capacity(captures.len());
                let mut valid = true;
                for capture in captures {
                    let Some(binding) = self.local(&capture.name) else {
                        self.error(
                            "E1002",
                            format!("closure capture `{}` is not a local binding", capture.name),
                            expr.span(),
                        );
                        valid = false;
                        continue;
                    };
                    if capture.mutable && !binding.mutable {
                        self.error(
                            "E1003",
                            format!(
                                "mutable capture `{}` requires a `var` binding",
                                capture.name
                            ),
                            expr.span(),
                        );
                        valid = false;
                    }
                    let ty = if capture.mutable {
                        binding.ty.clone()
                    } else {
                        self.effective_type(&binding)
                    };
                    capture_bindings.push((capture.name.clone(), ty, capture.mutable));
                }
                let mut parameter_types = Vec::with_capacity(params.len());
                for parameter in params {
                    let Some(annotation) = &parameter.ty else {
                        self.error(
                            "E2001",
                            format!(
                                "closure parameter `{}` requires an explicit type",
                                parameter.name
                            ),
                            expr.span(),
                        );
                        valid = false;
                        parameter_types.push(Type::Poison);
                        continue;
                    };
                    parameter_types.push(self.annotation_type(annotation));
                }
                let Some(result_annotation) = result_type.as_ref() else {
                    self.error(
                        "E2001",
                        "closure requires an explicit result type",
                        expr.span(),
                    );
                    return Type::Poison;
                };
                let declared_result = self.annotation_type(result_annotation);
                let saved_bindings = self
                    .scopes
                    .iter()
                    .map(|scope| scope.bindings.clone())
                    .collect::<Vec<_>>();
                for scope in &mut self.scopes {
                    scope.bindings.clear();
                }
                self.scopes.push(Scope::default());
                for (name, ty, mutable) in capture_bindings {
                    self.define(
                        &name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty,
                            mutable,
                        },
                        expr.span(),
                    );
                }
                for (parameter, ty) in params.iter().zip(parameter_types.iter().cloned()) {
                    self.define(
                        &parameter.name,
                        Binding {
                            id: 0,
                            error_only: false,
                            inferred: None,
                            ty,
                            mutable: true,
                        },
                        expr.span(),
                    );
                }
                let saved_function = self.in_function;
                let saved_async = self.in_async;
                let saved_result = self.result_type.replace(result_annotation.clone());
                let saved_loops = std::mem::replace(&mut self.loops, 0);
                let saved_loop_breaks = std::mem::take(&mut self.loop_breaks);
                let saved_flow = std::mem::take(&mut self.flow_facts);
                let saved_reachable = self.normal_reachable;
                self.in_function = true;
                self.in_async = false;
                self.normal_reachable = true;
                let expected_body = Some(&declared_result).filter(|ty| contains_nullable(ty));
                let body_type = self.block_expecting(body, expected_body);
                if body_type != Type::Never {
                    let body_type = match &body.tail {
                        Some(tail) => widened(tail, body_type, &declared_result),
                        None => body_type,
                    };
                    valid &= self.assignable(
                        result_annotation.clone(),
                        body_type,
                        body.tail.as_deref().and_then(Expr::span),
                        "closure result",
                    );
                }
                self.scopes.pop();
                for (scope, bindings) in self.scopes.iter_mut().zip(saved_bindings) {
                    scope.bindings = bindings;
                }
                self.in_function = saved_function;
                self.in_async = saved_async;
                self.result_type = saved_result;
                self.loops = saved_loops;
                self.loop_breaks = saved_loop_breaks;
                self.flow_facts = saved_flow;
                self.normal_reachable = saved_reachable;
                if !valid {
                    Type::Poison
                } else {
                    Type::Function(parameter_types, Box::new(declared_result))
                }
            }
            Expr::Array(items) => {
                let mut never = false;
                let mut element: Option<Type> = None;
                let mut inconsistent = false;
                let expected_element = match hint.as_ref().map(Type::non_nil) {
                    Some(Type::Array(element)) => Some(*element),
                    _ => None,
                };
                let mut conflicting = false;
                for item in items {
                    let ty = self.expr_with_expected_result(item, expected_element.as_ref());
                    never |= ty == Type::Never;
                    if ty != Type::Never && ty != Type::ErrorValue {
                        if let Some(previous) = &element
                            && previous.clone().join(ty.clone()) == Type::Dynamic
                            && *previous != Type::Dynamic
                            && ty != Type::Dynamic
                        {
                            inconsistent = true;
                        }
                        element = Some(match element.take() {
                            None => ty,
                            Some(current) => {
                                let joined = self.join_values(current, ty, true);
                                conflicting |= joined == Type::Poison;
                                joined
                            }
                        });
                    }
                }
                if conflicting {
                    return Type::Poison;
                }
                if inconsistent {
                    self.diagnostics.push(
                        Diagnostic::new(
                            "E2001",
                            "type mismatch: array elements must have a consistent type",
                            expr.span().cloned().or_else(|| self.location.clone()),
                        )
                        .help("make the array elements the same type or explicitly convert the outliers with `as`")
                    );
                    return Type::Poison;
                }
                if never {
                    Type::Never
                } else if items.is_empty() {
                    Type::EmptyArray
                } else {
                    Type::Array(Box::new(element.unwrap_or(Type::Dynamic)))
                }
            }
            Expr::Tuple(items) => {
                let expected_items = match hint.as_ref().map(Type::non_nil) {
                    Some(Type::Tuple(expected)) if expected.len() == items.len() => expected,
                    _ => Vec::new(),
                };
                let types: Vec<_> = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.expr_with_expected_result(item, expected_items.get(index))
                    })
                    .collect();
                if types.contains(&Type::Never) {
                    Type::Never
                } else {
                    Type::Tuple(types)
                }
            }
            Expr::Record(fields) => {
                let mut never = false;
                for (_, value) in fields {
                    never |= self.expr(value) == Type::Never;
                }
                if never { Type::Never } else { Type::Record }
            }
            Expr::Cast(inner, target) => match self.expr(inner) {
                Type::Poison => Type::Poison,
                Type::Never => Type::Never,
                _ => Type::concrete(*target),
            },
            Expr::Unary(op, inner) => {
                let ty = if *op == UnOp::Not {
                    let (ty, true_flow, false_flow) = self.branch(inner);
                    self.branch_flows =
                        Some((crate::mutability::expr_id(expr), false_flow, true_flow));
                    ty
                } else {
                    self.expr(inner)
                };
                if *op == UnOp::Await {
                    if !self.in_async {
                        self.error(
                            "E2002",
                            "`await` is only valid inside an `@async fn`",
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if self.in_cleanup {
                        self.error(
                            "E2002",
                            "`await` is not allowed in a `defer` block",
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    self.invalidate_call_flow_facts();
                    let ty = self.require_value(ty, inner, "await");
                    return match ty {
                        Type::Task(result) => *result,
                        Type::Poison => Type::Poison,
                        Type::Never => Type::Never,
                        Type::ErrorValue => Type::ErrorValue,
                        other => {
                            self.error(
                                "E2002",
                                format!("`await` expects Task[T], got {}", other.display()),
                                expr.span(),
                            );
                            Type::Poison
                        }
                    };
                }
                match (op, ty.clone()) {
                    (_, Type::Poison | Type::ErrorValue | Type::Dynamic | Type::Never) => {
                        ty.clone()
                    }
                    (
                        UnOp::Neg,
                        Type::Primitive(VarType::Int | VarType::BigInt | VarType::Float, false),
                    ) => ty.clone(),
                    (UnOp::Not, Type::Primitive(VarType::Bool, false)) => ty.clone(),
                    _ => {
                        let mut diagnostic = Diagnostic::new(
                            "E2002",
                            format!("invalid operand {} for {op:?}", ty.display()),
                            expr.span().cloned().or_else(|| self.location.clone()),
                        );
                        if ty.may_be_nil() {
                            diagnostic = diagnostic.help(
                                "the operand may be `nil`; check it with `!= nil` or supply a fallback with `??`",
                            );
                        }
                        self.diagnostics.push(diagnostic);
                        Type::Poison
                    }
                }
            }
            Expr::Propagate(inner) => {
                if !self.in_function || (self.in_comptime_initializer && !self.in_comptime) {
                    self.error(
                        "E1004",
                        "`!!` requires an enclosing callable body; a runtime comptime initializer is not a callable",
                        expr.span(),
                    );
                }
                match self.expr(inner) {
                    Type::ErrorValue => Type::Never,
                    ty => ty,
                }
            }
            Expr::Fallback {
                kind,
                value,
                fallback,
            } => self.fallback(*kind, value, fallback, expr.span(), hint.as_ref(), used),
            Expr::Binary(op @ (BinOp::And | BinOp::Or), a, b) => self.logical(*op, a, b, expr),
            Expr::Binary(op, a, b) => {
                let mut left = self.expr(a);
                let mut right = self.expr(b);
                if matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
                ) {
                    if left == Type::concrete(VarType::BigInt) && bigint_literal_expression(b) {
                        self.contextualize_integer_literals(b, &left);
                        right = Type::concrete(VarType::BigInt);
                    } else if right == Type::concrete(VarType::BigInt)
                        && bigint_literal_expression(a)
                    {
                        self.contextualize_integer_literals(a, &right);
                        left = Type::concrete(VarType::BigInt);
                    } else if left == Type::concrete(VarType::UInt)
                        && matches!(b.node(), Expr::Int(value) if *value >= 0)
                    {
                        self.contextualize_integer_literals(b, &left);
                        right = Type::concrete(VarType::UInt);
                    } else if right == Type::concrete(VarType::UInt)
                        && matches!(a.node(), Expr::Int(value) if *value >= 0)
                    {
                        self.contextualize_integer_literals(a, &right);
                        left = Type::concrete(VarType::UInt);
                    }
                }
                if matches!(op, BinOp::Eq | BinOp::NotEq) {
                    self.forget_equality_call(expr);
                    // Compile-time evaluation cannot call runtime trait methods.
                    // A nil operand is compared without calling `equals`.
                    let (left_value, right_value) = (left.non_nil(), right.non_nil());
                    if !self.in_comptime
                        && matches!(
                            (&left_value, &right_value),
                            (Type::Struct(..), Type::Struct(..))
                        )
                    {
                        return self.struct_equality(*op, (a, left_value), (b, right_value), expr);
                    }
                    self.nil_comparison(*op, a, b, expr);
                }
                self.binary_types(*op, left, right, expr.span(), Some((a, b)))
            }
            Expr::Index(a, b) => {
                let receiver = self.expr(a);
                let index = self.expr(b);
                if receiver == Type::Poison || index == Type::Poison {
                    return Type::Poison;
                }
                if receiver == Type::Never || index == Type::Never {
                    return Type::Never;
                }
                if receiver.may_be_nil() {
                    let action = match (receiver.non_nil(), b.node()) {
                        (Type::Struct(..) | Type::Record, Expr::Str(field)) => {
                            format!("access field `{field}` of")
                        }
                        (Type::Struct(..) | Type::Tuple(_), Expr::Int(position)) => {
                            format!("access position {position} of")
                        }
                        _ => "index".to_owned(),
                    };
                    return self.require_value(receiver, a, &action);
                }
                if self.require_value(index, b, "index with") == Type::Poison {
                    return Type::Poison;
                }
                if let Type::Struct(id, struct_name, type_arguments) = &receiver {
                    let field_name = match b.node() {
                        Expr::Str(name) => Some(name.clone()),
                        Expr::Int(index) if *index >= 0 => Some(index.to_string()),
                        _ => None,
                    };
                    if let Some(name) = field_name {
                        if let Some(field) = self.struct_fields[*id]
                            .iter()
                            .find(|field| field.name == name)
                        {
                            if field.visibility == Visibility::Private && !self.owns_struct(*id) {
                                self.error("E1002", format!("field `{name}` is private"), b.span());
                                return Type::Poison;
                            }
                            return field.ty.as_ref().map_or(Type::Dynamic, |field_type| {
                                let parameters = self
                                    .struct_parameters
                                    .get(struct_name)
                                    .cloned()
                                    .unwrap_or_default();
                                let substitutions: HashMap<_, _> = parameters
                                    .iter()
                                    .zip(type_arguments)
                                    .map(|(parameter, argument)| {
                                        (parameter.name.clone(), argument.clone())
                                    })
                                    .collect();
                                self.substituted_type(field_type, &substitutions)
                            });
                        }
                        let fields = self.struct_fields[*id];
                        let positional = fields
                            .iter()
                            .enumerate()
                            .all(|(index, field)| field.name == index.to_string());
                        if positional && matches!(b.node(), Expr::Int(_)) {
                            self.error(
                                "E2001",
                                format!("struct field position `{name}` is out of range"),
                                b.span(),
                            );
                            return Type::Poison;
                        }
                        self.error("E1002", format!("unknown field `{name}`"), b.span());
                        return Type::Poison;
                    }
                    if let Expr::Int(index) = b.node() {
                        let fields = self.struct_fields[*id];
                        let positional = fields
                            .iter()
                            .enumerate()
                            .all(|(index, field)| field.name == index.to_string());
                        if positional {
                            self.error(
                                "E2001",
                                format!("invalid struct field position `{index}`"),
                                b.span(),
                            );
                            return Type::Poison;
                        }
                    }
                    self.error(
                        "E1002",
                        "struct field access requires a field name",
                        b.span(),
                    );
                    return Type::Poison;
                }
                match (&receiver, b.node()) {
                    (Type::Array(element), _) => return (**element).clone(),
                    (Type::EmptyArray, _) => return Type::Dynamic,
                    (Type::Tuple(items), Expr::Int(index)) if *index >= 0 => {
                        if let Some(item) = items.get(*index as usize) {
                            return item.clone();
                        }
                        self.error(
                            "E2001",
                            format!(
                                "tuple position {index} is out of range for {} elements",
                                items.len()
                            ),
                            b.span(),
                        );
                        return Type::Poison;
                    }
                    (Type::Tuple(_), _) => {
                        self.error(
                            "E2001",
                            "tuple access requires a numeric constant position",
                            b.span(),
                        );
                        return Type::Poison;
                    }
                    _ => {}
                }
                Type::Dynamic
            }
            Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                self.call(name, args, expr)
            }
            Expr::AssociatedCall {
                target,
                method,
                type_arguments,
                args,
            } => self.associated_call(target, method, type_arguments, args, expr, None),
            Expr::MethodCall(receiver_expr, method, args) => {
                let receiver = self.expr(receiver_expr);
                let mut hints = vec![None; args.len()];
                if let Some(function) = self
                    .extension_methods
                    .get(&(receiver.clone(), method.clone()))
                    .map(|extension| extension.function.clone())
                {
                    for ((argument, parameter), hint) in args
                        .iter()
                        .zip(function.params.iter().skip(1))
                        .zip(&mut hints)
                    {
                        if let Some(expected) = parameter.ty.as_ref() {
                            let expected_type = self.annotation_type(expected);
                            self.contextualize_integer_literals(argument, &expected_type);
                            *hint = Some(expected_type).filter(contains_nullable);
                        }
                    }
                } else if matches!(method.as_str(), "append" | "prepend")
                    && let Type::Array(element) = receiver.non_nil()
                    && contains_nullable(&element)
                    && let Some(hint) = hints.first_mut()
                {
                    *hint = Some(*element);
                }
                // Error inspection accepts any value, so nil passes through.
                let receiver = if receiver.may_be_nil()
                    && !matches!(method.as_str(), "unwrap" | "expect" | "error_kind")
                    && !self
                        .extension_methods
                        .contains_key(&(receiver.clone(), method.clone()))
                {
                    self.require_value(
                        receiver,
                        receiver_expr,
                        &format!("call method `{method}()` on"),
                    )
                } else {
                    receiver
                };
                let mut poisoned = receiver == Type::Poison;
                let mut never = receiver == Type::Never;
                let mut argument_types = Vec::with_capacity(args.len());
                for (arg, hint) in args.iter().zip(&hints) {
                    let ty = self.expr_with_expected_result(arg, hint.as_ref());
                    poisoned |= ty == Type::Poison;
                    never |= ty == Type::Never;
                    argument_types.push(ty);
                }
                self.invalidate_call_flow_facts();
                if receiver == Type::Poison {
                    return Type::Poison;
                }
                let exact_method = self
                    .extension_methods
                    .get(&(receiver.clone(), method.clone()))
                    .copied();
                let mut generic_candidates: Vec<_> = self
                    .generic_extension_methods
                    .iter()
                    .filter_map(|(extension, extension_method)| {
                        (extension_method.name == *method
                            && generic_extension_matches(extension, &receiver))
                        .then_some(*extension_method)
                    })
                    .collect();
                let ambiguous_generic_method = generic_candidates.len() > 1;
                let generic_method = generic_candidates.first().copied();
                generic_candidates.clear();
                if exact_method.is_none() && ambiguous_generic_method {
                    self.error(
                        "E2001",
                        format!("method `{method}()` has multiple applicable generic extensions"),
                        expr.span(),
                    );
                    return Type::Poison;
                }
                if let Some(extension_method) = exact_method.or(generic_method) {
                    let function = &extension_method.function;
                    if function.comptime && !self.in_comptime {
                        self.error(
                            "E4002",
                            format!(
                                "runtime code cannot call comptime-only extension method `{method}()`"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if self.in_comptime && !function.comptime {
                        self.error(
                            "E4002",
                            format!(
                                "comptime code cannot call runtime extension method `{method}()`"
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if poisoned {
                        return Type::Poison;
                    }
                    if never {
                        return Type::Never;
                    }
                    self.access_facts
                        .calls
                        .insert(crate::mutability::expr_id(expr), function.name.clone());
                    if extension_method.mutating_receiver {
                        self.access_facts
                            .mutating_calls
                            .insert(crate::mutability::expr_id(expr));
                    }
                    if !function.type_params.is_empty() {
                        let mut call_arguments = Vec::with_capacity(args.len() + 1);
                        call_arguments.push(CallArgument::positional((**receiver_expr).clone()));
                        call_arguments.extend(args.iter().cloned().map(CallArgument::positional));
                        let mut actual_types = Vec::with_capacity(argument_types.len() + 1);
                        actual_types.push(receiver.clone());
                        actual_types.extend(argument_types.iter().cloned());
                        return self.generic_call(
                            &function.name,
                            function,
                            &call_arguments,
                            actual_types,
                            expr,
                        );
                    }
                    let expected = function.params.len().saturating_sub(1);
                    let required = function
                        .params
                        .iter()
                        .skip(1)
                        .filter(|param| param.default.is_none())
                        .count();
                    if args.len() < required || args.len() > expected {
                        let expected = if required == expected {
                            expected.to_string()
                        } else {
                            format!("{required} to {expected}")
                        };
                        self.error(
                            "E1005",
                            format!(
                                "method `{method}()` expects {expected} argument(s), got {}",
                                args.len()
                            ),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if let Some(receiver_param) = function.params.first()
                        && let Some(ty) = &receiver_param.ty
                    {
                        self.assignable(
                            ty.clone(),
                            receiver.clone(),
                            receiver_expr.span(),
                            "method receiver",
                        );
                    }
                    for ((argument, actual), parameter) in args
                        .iter()
                        .zip(argument_types.iter())
                        .zip(function.params.iter().skip(1))
                    {
                        if let Some(expected) = &parameter.ty {
                            let expected_type = self.annotation_type(expected);
                            self.assignable(
                                expected.clone(),
                                widened(argument, actual.clone(), &expected_type),
                                argument.span(),
                                &format!("argument `{}` of `{method}()`", parameter.name),
                            );
                        }
                    }
                    self.method_calls.insert(
                        expression_site(self.current_function.as_deref(), expr),
                        function.name.clone(),
                    );
                    let result = function
                        .result_type
                        .as_ref()
                        .map_or(Type::Dynamic, |ty| self.annotation_type(ty));
                    return if function.asynchronous {
                        Type::Task(Box::new(result))
                    } else {
                        result
                    };
                }
                if method == "clone"
                    && matches!(receiver, Type::Array(_) | Type::Tuple(_))
                    && self.implements_trait(&receiver, CLONE_TRAIT, &[])
                {
                    if !args.is_empty() {
                        self.error(
                            "E1005",
                            format!("method `clone()` expects 0 argument(s), got {}", args.len()),
                            expr.span(),
                        );
                        return Type::Poison;
                    }
                    if poisoned {
                        return Type::Poison;
                    }
                    if never {
                        return Type::Never;
                    }
                    self.access_facts
                        .structural_clones
                        .insert(crate::mutability::expr_id(expr));
                    self.method_calls.insert(
                        expression_site(self.current_function.as_deref(), expr),
                        "$vm::structural_clone".to_owned(),
                    );
                    return receiver;
                }
                let expected = match method.as_str() {
                    "expect" | "append" | "prepend" => 1,
                    "len" | "to_int" | "to_uint" | "to_float" | "to_bool" | "to_char"
                    | "to_str" | "error_kind" | "unwrap" | "empty_like" => 0,
                    _ => {
                        self.error("E1002", format!("unknown method `{method}()`"), expr.span());
                        return Type::Poison;
                    }
                };
                if args.len() != expected {
                    self.error(
                        "E1005",
                        format!(
                            "method `{method}()` expects {expected} argument(s), got {}",
                            args.len()
                        ),
                        expr.span(),
                    );
                    return Type::Poison;
                }
                if poisoned {
                    return Type::Poison;
                }
                if never {
                    return Type::Never;
                }
                if method == "len" && receiver == Type::Bytes {
                    self.reject_bytes_length("len()", expr.span());
                    return Type::Poison;
                }
                match method.as_str() {
                    "empty_like" => match receiver {
                        Type::Array(_) | Type::Dynamic | Type::ErrorValue => receiver,
                        _ => {
                            self.error(
                                "E2001",
                                "empty_like() requires an array receiver",
                                receiver_expr.span(),
                            );
                            Type::Poison
                        }
                    },
                    "append" | "prepend" => {
                        if !matches!(
                            receiver,
                            Type::Array(_) | Type::EmptyArray | Type::Dynamic | Type::ErrorValue
                        ) {
                            self.error(
                                "E2001",
                                "array mutation requires an array receiver",
                                expr.span(),
                            );
                            Type::Poison
                        } else {
                            if let (Type::Array(element), Some(actual)) =
                                (&receiver, argument_types.first())
                            {
                                self.assignable_type(
                                    (**element).clone(),
                                    actual.clone(),
                                    args[0].span(),
                                    "array element",
                                );
                                if **element == Type::Dynamic {
                                    self.refine_empty_array(receiver_expr, actual);
                                }
                            }
                            if receiver == Type::EmptyArray
                                && let Some(actual) = argument_types.first()
                            {
                                self.refine_empty_array(receiver_expr, actual);
                                return Type::Array(Box::new(actual.clone()));
                            }
                            receiver
                        }
                    }
                    "to_int" | "len" => Type::concrete(VarType::Int),
                    "to_float" => Type::concrete(VarType::Float),
                    "to_bool" | "exclusive" => Type::concrete(VarType::Bool),
                    "to_char" => Type::concrete(VarType::Char),
                    "to_str" | "error_kind" => Type::concrete(VarType::Str),
                    "unwrap" | "expect" => receiver,
                    _ => Type::Dynamic,
                }
            }
            Expr::Block(block) | Expr::Capture(block) => {
                if !used {
                    self.discard_tail(block);
                }
                self.block_expecting(block, hint.as_ref())
            }
            Expr::Match {
                value,
                arms,
                exhaustive,
            } => {
                let actual = self.expr(value);
                let mut result = None;
                let mut covered = std::collections::HashSet::new();
                let mut wildcard = false;
                let base_flow = self.flow_facts.clone();
                let mut unmatched_flow = Some(base_flow.clone());
                let mut normal_exits = Vec::new();
                // The scrutinee is evaluated once. A guard that can rebind it
                // would separate the binding from the value being matched.
                let stable_scrutinee = match value.node() {
                    Expr::Ident(name) => {
                        let mut writes = Writes::default();
                        for guard in arms.iter().filter_map(|arm| arm.guard.as_ref()) {
                            writes.expr(guard);
                        }
                        !writes.assigned.contains(name)
                            && (arms.iter().all(|arm| arm.guard.is_none())
                                || !self.writes.captured.contains(name))
                    }
                    _ => true,
                };
                for arm in arms {
                    let arm_flow = unmatched_flow.clone().unwrap_or_else(|| base_flow.clone());
                    self.flow_facts = arm_flow.clone();
                    let previous_reachability = self.normal_reachable;
                    let arm_reachable = unmatched_flow.is_some();
                    self.normal_reachable &= arm_reachable;
                    // Separate pattern facts from payload bindings so a payload
                    // can shadow the scrutinee name without a duplicate binding.
                    self.scopes.push(Scope::default());
                    self.scopes.push(Scope::default());
                    let mut tested_type = None;
                    match &arm.pattern {
                        MatchPattern::Wildcard => wildcard |= arm.guard.is_none(),
                        MatchPattern::StringLiteral(_) | MatchPattern::CharacterLiteral(_) => {}
                        MatchPattern::Type(name) => {
                            let tested = self.type_test(name, expr.span());
                            tested_type.clone_from(&tested);
                            if tested.as_deref().is_some_and(|name| {
                                name == crate::error::ERROR_TYPE
                                    || name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                            }) {
                                self.refine_error_binding(value);
                            }
                            if let Some(name) = &tested
                                && stable_scrutinee
                            {
                                self.apply_type_pattern(value, name, true);
                            }
                            if let Some(name) = tested
                                && arm.guard.is_none()
                            {
                                if let Some(id) = self.enums.get(&name) {
                                    covered.extend(self.enum_variants[*id].iter().cloned());
                                } else {
                                    // Variants, `nil`, and struct names.
                                    covered.insert(name);
                                }
                            }
                        }
                        MatchPattern::Variant { name, bindings } => {
                            let resolved = self.resolve(name, expr.span());
                            let variant = resolved
                                .as_ref()
                                .and_then(|name| self.variants.get(name))
                                .copied();
                            if let Some((id, fields)) = variant {
                                if resolved.as_deref().is_some_and(|name| {
                                    name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                                }) {
                                    self.refine_error_binding(value);
                                }
                                let error_pattern = resolved.as_deref().is_some_and(|name| {
                                    name.starts_with(&format!("{}::", crate::error::ERROR_TYPE))
                                });
                                if error_pattern && let Some(name) = resolved.as_deref() {
                                    self.apply_type_pattern(value, name, true);
                                }
                                if !error_pattern
                                    && matches!(actual.non_nil(), Type::Enum(other, _) if other != id)
                                {
                                    self.error(
                                        "E2001",
                                        "pattern belongs to a different enum",
                                        expr.span(),
                                    );
                                }
                                if fields.len() != bindings.len() {
                                    self.error(
                                        "E1005",
                                        format!(
                                            "pattern `{name}` expects {} binding(s), got {}",
                                            fields.len(),
                                            bindings.len()
                                        ),
                                        expr.span(),
                                    );
                                }
                                if !error_pattern && stable_scrutinee {
                                    self.narrow_non_nil(value);
                                }
                                if arm.guard.is_none() {
                                    covered.insert(resolved.unwrap());
                                }
                                for (i, binding) in bindings.iter().enumerate() {
                                    if binding != "_" {
                                        let ty = fields
                                            .get(i)
                                            .and_then(|field| field.ty.as_ref())
                                            .map_or(Type::Dynamic, |t| self.annotation_type(t));
                                        self.define(
                                            binding,
                                            Binding {
                                                id: 0,
                                                error_only: false,
                                                inferred: None,
                                                ty,
                                                mutable: false,
                                            },
                                            expr.span(),
                                        );
                                    }
                                }
                            } else {
                                if !resolved.as_deref().is_some_and(|name| {
                                    self.globals.get(name).is_some_and(|b| b.ty == Type::Poison)
                                }) {
                                    self.error(
                                        "E1002",
                                        format!("unknown enum variant `{name}` in pattern"),
                                        expr.span(),
                                    );
                                }
                                for binding in bindings {
                                    if binding != "_" {
                                        self.define(
                                            binding,
                                            Binding {
                                                id: 0,
                                                error_only: false,
                                                inferred: None,
                                                ty: Type::Poison,
                                                mutable: false,
                                            },
                                            expr.span(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    let mut guard_rejected = None;
                    if let Some(guard) = &arm.guard {
                        let (_, true_flow, false_flow) = self.condition(guard);
                        self.flow_facts = true_flow;
                        guard_rejected = Some(false_flow);
                    }
                    if !used {
                        self.discard_tail(&arm.body);
                    }
                    let ty = self.block_expecting(&arm.body, hint.as_ref());
                    if arm_reachable && ty != Type::Never {
                        normal_exits.push(self.flow_facts.clone());
                    }
                    result = Some(match result.take() {
                        None => ty,
                        Some(current) => self.join_values(current, ty, used),
                    });
                    self.scopes.pop();
                    self.scopes.pop();
                    self.normal_reachable = previous_reachability;

                    if !arm_reachable {
                        unmatched_flow = None;
                    } else if let Some(rejected) = guard_rejected {
                        // A later arm follows either a failed pattern or a
                        // failed guard.
                        unmatched_flow = Some(if matches!(arm.pattern, MatchPattern::Wildcard) {
                            rejected
                        } else {
                            FlowFacts::join([arm_flow, rejected])
                        });
                    } else {
                        match &arm.pattern {
                            MatchPattern::Wildcard => unmatched_flow = None,
                            MatchPattern::StringLiteral(_) | MatchPattern::CharacterLiteral(_) => {}
                            MatchPattern::Type(_) => {
                                let mut next = arm_flow;
                                if let Some(name) = tested_type.as_deref()
                                    && stable_scrutinee
                                {
                                    self.flow_facts = next.clone();
                                    self.apply_type_pattern(value, name, false);
                                    next = self.flow_facts.clone();
                                }
                                unmatched_flow = Some(next);
                            }
                            MatchPattern::Variant { .. } => {
                                unmatched_flow = Some(arm_flow);
                            }
                        }
                    }
                }
                if *exhaustive && !wildcard && actual != Type::Poison {
                    let scrutinee = actual.non_nil();
                    let enum_id = match scrutinee {
                        Type::Enum(id, _) => Some(id),
                        Type::ErrorValue => self
                            .variants
                            .get(&format!("{}::DivisionByZero", crate::error::ERROR_TYPE))
                            .map(|(id, _)| *id),
                        _ => None,
                    };
                    // A pattern of the scrutinee's own struct covers every
                    // struct value.
                    let values_covered = match &scrutinee {
                        Type::Struct(_, name, _) => covered.contains(name),
                        _ => enum_id.is_some_and(|id| {
                            self.enum_variants[id]
                                .iter()
                                .all(|name| covered.contains(name))
                        }),
                    };
                    let nil_covered = !actual.is_nullable() || covered.contains("nil");
                    if !values_covered {
                        self.error(
                            "E2003",
                            "non-exhaustive match: add missing variants or a `_` arm",
                            expr.span(),
                        );
                    } else if !nil_covered {
                        self.error("E2003", NILABLE_MATCH, expr.span());
                    } else {
                        unmatched_flow = None;
                    }
                }
                if let Some(unmatched) = unmatched_flow {
                    normal_exits.push(unmatched);
                }
                if !normal_exits.is_empty() {
                    self.join_flow_exits(normal_exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if actual == Type::Never {
                    return Type::Never;
                }
                let result = result.unwrap_or(Type::Nil);
                if !*exhaustive && !wildcard {
                    result.join(Type::Nil)
                } else {
                    result
                }
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => {
                if !used {
                    self.discard_tail(body);
                    self.discard_tail(catch_block);
                }
                self.try_blocks(body, catch_name, catch_block, hint.as_ref(), used)
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let (condition, true_flow, false_flow) = self.condition(cond);
                let reachable = self.normal_reachable;
                let base_flow = self.flow_facts.clone();
                let then_reachable = reachable && !matches!(cond.node(), Expr::Bool(false));
                self.normal_reachable = then_reachable;
                self.flow_facts = true_flow;
                if !used {
                    self.discard(then_branch);
                    if let Some(else_branch) = else_branch {
                        self.discard(else_branch);
                    }
                }
                let a = self.expr_with_expected_result(then_branch, hint.as_ref());
                let then_flow = self.flow_facts.clone();
                self.flow_facts = false_flow;
                let else_reachable = reachable && !matches!(cond.node(), Expr::Bool(true));
                self.normal_reachable = else_reachable;
                let b = else_branch.as_ref().map_or(Type::Nil, |branch| {
                    self.expr_with_expected_result(branch, hint.as_ref())
                });
                let else_flow = self.flow_facts.clone();
                self.normal_reachable = reachable;
                let mut exits = Vec::new();
                if then_reachable && a != Type::Never {
                    exits.push(then_flow);
                }
                if else_reachable && b != Type::Never {
                    exits.push(else_flow);
                }
                if !exits.is_empty() {
                    self.join_flow_exits(exits);
                } else {
                    self.flow_facts = base_flow;
                }
                if condition == Type::Never
                    || (a == Type::Never && matches!(cond.node(), Expr::Bool(true)))
                    || (b == Type::Never && matches!(cond.node(), Expr::Bool(false)))
                {
                    Type::Never
                } else {
                    self.join_values(a, b, used)
                }
            }
        }
    }

    fn fallback(
        &mut self,
        kind: FallbackKind,
        value: &Expr,
        fallback: &Expr,
        span: Option<&SourceSpan>,
        expected: Option<&Type>,
        used: bool,
    ) -> Type {
        let lhs = self.expr(value);
        let alternative = match (kind, lhs.clone()) {
            (FallbackKind::Nil, Type::Nil) => Type::Never,
            (FallbackKind::Nil, Type::Primitive(VarType::Nil, _)) => Type::Never,
            (FallbackKind::Nil, Type::Primitive(t, _)) => Type::Primitive(t, false),
            (FallbackKind::Nil, Type::Nullable(inner)) => *inner,
            (FallbackKind::Error, Type::ErrorValue) => Type::Never,
            _ => lhs.clone(),
        };
        let can_fallback = match kind {
            FallbackKind::Nil => lhs == Type::Dynamic || alternative != lhs,
            FallbackKind::Error => {
                lhs != Type::Never
                    && !matches!(
                        value.node(),
                        Expr::Int(_)
                            | Expr::Float(_)
                            | Expr::Char(_)
                            | Expr::Bool(_)
                            | Expr::Str(_)
                            | Expr::Nil
                    )
            }
        };
        let reachable = self.normal_reachable;
        self.normal_reachable = reachable && can_fallback && lhs != Type::Never;
        self.access_facts
            .fallback_possible
            .insert(crate::mutability::expr_id(fallback), can_fallback);
        // A skipped RHS is still checked for static violations.
        let skipped = self.flow_facts.clone();
        let rhs = self.expr_with_expected_result(fallback, expected);
        self.normal_reachable = reachable;
        if rhs == Type::Never {
            self.flow_facts = skipped;
        } else {
            self.join_flow_exits([skipped, self.flow_facts.clone()]);
        }
        if lhs == Type::Poison || rhs == Type::Poison {
            return Type::Poison;
        }
        if !can_fallback {
            return lhs;
        }
        let result = self.join_values(alternative.clone(), rhs.clone(), used);
        if result == Type::Dynamic && alternative != Type::Dynamic && rhs != Type::Dynamic {
            self.error(
                "E2001",
                format!(
                    "type mismatch: incompatible normal fallback types {} and {}",
                    alternative.display(),
                    rhs.display()
                ),
                span,
            );
            Type::Poison
        } else {
            result
        }
    }

    fn call(&mut self, name: &str, args: &[CallArgument], expr: &Expr) -> Type {
        let before = self.diagnostics.len();
        let hints = self.argument_hints(name, args);
        let actual: Vec<_> = args
            .iter()
            .zip(&hints)
            .map(|(arg, hint)| self.expr_with_expected_result(&arg.value, hint.as_ref()))
            .collect();
        let poisoned = actual.contains(&Type::Poison);
        let never = actual.contains(&Type::Never);
        let result = self.call_inner(name, args, actual, expr);
        if poisoned || self.diagnostics.len() > before {
            Type::Poison
        } else if never {
            Type::Never
        } else {
            result
        }
    }

    /// Report a builtin or native argument of the wrong type.
    fn argument_mismatch(&mut self, name: &str, expected: &str, actual: &Type, value: &Expr) {
        let mut diagnostic = Diagnostic::new(
            "E2001",
            format!(
                "type mismatch: `{name}` expects {expected}, got {}",
                actual.display()
            ),
            value.span().cloned().or_else(|| self.location.clone()),
        );
        if actual.may_be_nil() {
            diagnostic = diagnostic.help(
                "this value may be `nil`; check it with `!= nil` or supply a fallback with `??`",
            );
        }
        self.diagnostics.push(diagnostic);
    }

    /// Nullable parameter types of a callee, which its arguments build.
    /// Parameters that depend on a type parameter are inferred instead.
    /// Diagnostics are left to the call check that follows.
    fn argument_hints(&mut self, name: &str, args: &[CallArgument]) -> Vec<Option<Type>> {
        let diagnostics = self.diagnostics.len();
        let mut hints = vec![None; args.len()];
        if let Some(resolved) = self.resolve(name, None)
            && let Some(fields) = self
                .functions
                .get(&resolved)
                .map(|function| function.params.as_slice())
                .or_else(|| self.constructors.get(&resolved).map(|(_, fields)| *fields))
                .or_else(|| self.variants.get(&resolved).map(|(_, fields)| *fields))
        {
            let bindings = crate::arguments::bind(name, fields, args);
            for (hint, parameter) in hints.iter_mut().zip(bindings.by_argument) {
                if let Some(ty) = parameter.and_then(|index| fields[index].ty.as_ref()) {
                    *hint = Some(self.annotation_type(ty))
                        .filter(|ty| contains_nullable(ty) && !contains_generic(ty));
                }
            }
        }
        self.diagnostics.truncate(diagnostics);
        hints
    }

    fn call_inner(
        &mut self,
        name: &str,
        args: &[CallArgument],
        actual: Vec<Type>,
        expr: &Expr,
    ) -> Type {
        // Without effect summaries, any call may rebind a captured local or a
        // global. Drop their refinements before checking what follows.
        self.invalidate_call_flow_facts();
        let span = expr.span();
        let Some(resolved) = self.resolve(name, span) else {
            return Type::Poison;
        };
        if matches!(Builtin::from_name(&resolved), Some(Builtin::Spawn)) {
            if !self.in_async {
                self.error("E2002", "`spawn` is only valid inside an `@async fn`", span);
                return Type::Poison;
            }
            if args.len() != 1 {
                self.error(
                    "E1005",
                    format!("`spawn` expects 1 argument, got {}", args.len()),
                    span,
                );
                return Type::Poison;
            }
            return match actual.first() {
                Some(Type::Task(_)) => actual[0].clone(),
                Some(Type::Poison) => Type::Poison,
                Some(other) => {
                    self.error(
                        "E2002",
                        format!("`spawn` expects Task[T], got {}", other.display()),
                        args[0].value.span(),
                    );
                    Type::Poison
                }
                None => Type::Poison,
            };
        }
        if matches!(Builtin::from_name(&resolved), Some(Builtin::Cancel)) {
            if args.len() != 1 {
                self.error(
                    "E1005",
                    format!("`cancel` expects 1 argument, got {}", args.len()),
                    span,
                );
                return Type::Poison;
            }
            return match actual.first() {
                Some(Type::Task(_) | Type::Dynamic | Type::ErrorValue) => {
                    Type::concrete(VarType::Bool)
                }
                Some(Type::Poison) => Type::Poison,
                Some(other) => {
                    self.error(
                        "E2002",
                        format!("`cancel` expects Task[T], got {}", other.display()),
                        args[0].value.span(),
                    );
                    Type::Poison
                }
                None => Type::Poison,
            };
        }
        if let Some((id, fields)) = self.constructors.get(&resolved).copied()
            && fields
                .iter()
                .any(|field| field.visibility == Visibility::Private)
            && !self.owns_struct(id)
        {
            self.error(
                "E1002",
                format!("cannot construct `{name}` outside its defining module because it has private fields"),
                span,
            );
            return Type::Poison;
        }
        self.access_facts
            .calls
            .insert(crate::mutability::expr_id(expr), resolved.clone());
        if self
            .globals
            .get(&resolved)
            .is_some_and(|b| b.ty == Type::Poison)
        {
            return Type::Poison;
        }
        if let Some((is_comptime, function_name)) = self
            .functions
            .get(&resolved)
            .map(|function| (function.comptime, function.name.clone()))
            && is_comptime != self.in_comptime
        {
            let (caller_phase, callee_phase) = if self.in_comptime {
                ("comptime", "runtime")
            } else {
                ("runtime", "comptime")
            };
            self.error(
                "E4002",
                format!(
                    "{caller_phase} code cannot call {callee_phase}-only function `{function_name}`"
                ),
                span,
            );
            return Type::Poison;
        }
        if let Some(function) = self
            .functions
            .get(&resolved)
            .filter(|function| !function.type_params.is_empty())
            .cloned()
        {
            return self.generic_call(&resolved, function, args, actual, expr);
        }
        if self
            .struct_parameters
            .get(&resolved)
            .is_some_and(|parameters| !parameters.is_empty())
        {
            return self.generic_struct_call(&resolved, args, actual, expr);
        }
        let fields = self
            .functions
            .get(&resolved)
            .map(|f| f.params.as_slice())
            .or_else(|| self.constructors.get(&resolved).map(|(_, fields)| *fields))
            .or_else(|| self.variants.get(&resolved).map(|(_, fields)| *fields));
        if let Some(fields) = fields {
            let bindings = crate::arguments::bind(name, fields, args);
            for error in bindings.errors {
                self.error(
                    "E1005",
                    error.message,
                    error.argument.and_then(|i| args[i].value.span()).or(span),
                );
            }
            let mut actual = actual;
            for i in 0..actual.len() {
                if let Some(index) = bindings.by_argument[i] {
                    if let Some(ty) = fields[index].ty.clone() {
                        let expected_type = self.annotation_type(&ty);
                        actual[i] = self.contextualized_literal_type(
                            &args[i].value,
                            actual[i].clone(),
                            &expected_type,
                        );
                        actual[i] = widened(&args[i].value, actual[i].clone(), &expected_type);
                        if actual[i] == Type::EmptyArray
                            && let Type::Array(element) = expected_type.non_nil()
                        {
                            self.refine_empty_array(&args[i].value, &element);
                        }
                        self.assignable(
                            ty,
                            actual[i].clone(),
                            args[i].value.span(),
                            &format!("parameter `{}` of `{name}`", fields[index].name),
                        );
                    } else if contains_unresolved_empty_array(&actual[i]) {
                        self.error(
                            "E2001",
                            "cannot infer array element type for an empty argument to a dynamic parameter; declare a typed array first",
                            args[i].value.span(),
                        );
                    }
                }
            }
            return if let Some((id, _)) = self.constructors.get(&resolved) {
                Type::Struct(*id, resolved.clone(), Vec::new())
            } else if let Some((id, _)) = self.variants.get(&resolved) {
                if resolved.starts_with(&format!("{}::", crate::error::ERROR_TYPE)) {
                    Type::ErrorValue
                } else {
                    Type::Enum(*id, resolved.clone())
                }
            } else {
                let result = self
                    .functions
                    .get(&resolved)
                    .and_then(|function| function.result_type.as_ref())
                    .map_or(Type::Dynamic, |t| self.annotation_type(t));
                if self
                    .functions
                    .get(&resolved)
                    .is_some_and(|function| function.asynchronous)
                {
                    Type::Task(Box::new(result))
                } else {
                    result
                }
            };
        }
        if let Some(builtin) = Builtin::from_name(&resolved) {
            if matches!(builtin, Builtin::Len)
                && let ([ty], [argument]) = (actual.as_slice(), args)
                && self.require_value(ty.clone(), &argument.value, "take the length of")
                    == Type::Poison
            {
                return Type::Poison;
            }
            if matches!(builtin, Builtin::Len) && actual.as_slice() == [Type::Bytes] {
                self.reject_bytes_length("len", span);
                return Type::Poison;
            }
            if matches!(builtin, Builtin::BuildInput) {
                if !self.in_comptime {
                    self.error("E4002", format!("`{resolved}` requires a comptime-known path and is only available at comptime"), span);
                    return Type::Poison;
                }
                if actual.len() != 1 || args[0].name.as_deref().is_some_and(|name| name != "path") {
                    self.error(
                        "E1005",
                        "build input calls require exactly one `path` argument",
                        span,
                    );
                    return Type::Poison;
                }
                if !matches!(actual[0], Type::Dynamic)
                    && !type_compatible(&Type::concrete(VarType::Str), &actual[0])
                {
                    self.error("E2001", "build input path must be a string", span);
                }
                return if resolved.ends_with("include_str") {
                    Type::Primitive(VarType::Str, resolved.starts_with("build::try_"))
                } else if resolved.starts_with("build::try_") {
                    Type::Dynamic
                } else {
                    Type::Bytes
                };
            }
            if self.in_comptime
                && matches!(
                    builtin,
                    Builtin::Native(_)
                        | Builtin::YieldNow
                        | Builtin::TimeoutAsync
                        | Builtin::Select
                        | Builtin::Select2
                        | Builtin::SleepAsync
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
                        | Builtin::AsyncIoRead
                        | Builtin::AsyncIoReadExact
                        | Builtin::AsyncIoReadAll
                        | Builtin::AsyncIoOpen
                        | Builtin::AsyncIoWrite
                        | Builtin::AsyncIoWriteAll
                        | Builtin::AsyncIoFlush
                        | Builtin::AsyncIoSeek
                )
            {
                self.error(
                    "E4002",
                    format!("runtime-only function `{resolved}` cannot execute at comptime"),
                    span,
                );
                return Type::Poison;
            }
            if let Some(index) = actual.iter().position(contains_unresolved_empty_array) {
                self.error(
                    "E2001",
                    "cannot infer array element type for an empty argument; declare a typed array first",
                    args.get(index).and_then(|arg| arg.value.span()),
                );
            }
            if !builtin.accepts_arity(args.len()) {
                let expected = if matches!(builtin, Builtin::ProcessRunAsync) {
                    "2, 3, 4, or 5".to_owned()
                } else {
                    builtin.arity().to_string()
                };
                self.error(
                    "E1005",
                    format!(
                        "`{name}` expects {expected} argument(s), got {}",
                        args.len()
                    ),
                    span,
                );
            }
            if args.iter().any(|arg| arg.name.is_some()) {
                let message = if matches!(builtin, Builtin::Native(_)) {
                    "native functions use positional arguments"
                } else {
                    "VM primitives use positional arguments"
                };
                self.error("E1005", message, span);
            }
            for (index, (actual, argument)) in actual.iter().zip(args).enumerate() {
                let (expected, valid) = match (&builtin, index) {
                    (Builtin::TimeoutAsync, 0) => (
                        "Task[T]",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || matches!(actual, Type::Task(_)),
                    ),
                    (Builtin::TimeoutAsync, 1) => (
                        "int, uint, or float",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || [VarType::Int, VarType::UInt, VarType::Float]
                                .into_iter()
                                .any(|kind| *actual == Type::concrete(kind)),
                    ),
                    (Builtin::Select2, _) => (
                        "Task[T]",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || matches!(actual, Type::Task(_)),
                    ),
                    (Builtin::Select, 0) => (
                        "Task[T][]",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || matches!(actual, Type::Array(element) if matches!(element.as_ref(), Type::Task(_) | Type::Dynamic)),
                    ),
                    (Builtin::SleepAsync, 0) => (
                        "int, uint, or float",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || [VarType::Int, VarType::UInt, VarType::Float]
                                .into_iter()
                                .any(|kind| *actual == Type::concrete(kind)),
                    ),
                    (Builtin::NetworkExchangeAsync, 0) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::NetworkExchangeAsync, 1 | 3) => (
                        "int or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (Builtin::NetworkExchangeAsync, 2) => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (Builtin::TcpConnectAsync | Builtin::TcpListenAsync, 0) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::TcpConnectAsync, 1 | 2)
                    | (Builtin::TcpListenAsync, 1)
                    | (Builtin::TcpReadAsync, 1)
                    | (Builtin::TcpReadExactAsync, 1)
                    | (Builtin::TcpAcceptAsync, 1 | 2) => (
                        "int or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (Builtin::AsyncIoReadExact, 1) => (
                        "int or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (
                        Builtin::TcpReadAsync
                        | Builtin::TcpReadExactAsync
                        | Builtin::TcpReadAllAsync
                        | Builtin::TcpAcceptAsync
                        | Builtin::TcpWriteAsync
                        | Builtin::TcpWriteAllAsync
                        | Builtin::TcpShutdownWriteAsync,
                        0,
                    ) => (
                        "handle",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::Handle,
                    ),
                    (Builtin::TcpWriteAsync | Builtin::TcpWriteAllAsync, 1) => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (
                        Builtin::AsyncIoRead
                        | Builtin::AsyncIoReadExact
                        | Builtin::AsyncIoReadAll
                        | Builtin::AsyncIoWrite
                        | Builtin::AsyncIoWriteAll
                        | Builtin::AsyncIoFlush
                        | Builtin::AsyncIoSeek,
                        0,
                    ) => (
                        "handle",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::Handle,
                    ),
                    (Builtin::AsyncIoOpen, 0 | 1) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::AsyncIoOpen, 2) => (
                        "int",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int),
                    ),
                    (Builtin::AsyncIoRead, 1) => (
                        "int or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (Builtin::AsyncIoWrite | Builtin::AsyncIoWriteAll, 1) => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (Builtin::AsyncIoSeek, 1) => (
                        "int",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int),
                    ),
                    (Builtin::AsyncIoSeek, 2) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::ReadBytesAsync, 0)
                    | (Builtin::ProcessRunAsync, 0)
                    | (Builtin::FsMetadataAsync, 0)
                    | (Builtin::FsReadDirAsync, 0)
                    | (Builtin::FsWriteBytesAsync, 0)
                    | (Builtin::FsCreateDirAsync, 0)
                    | (Builtin::FsCreateDirAllAsync, 0)
                    | (Builtin::FsRemoveFileAsync, 0)
                    | (Builtin::FsRemoveDirAsync, 0)
                    | (Builtin::FsReadTextAsync, 0)
                    | (Builtin::FsReadLinesAsync, 0)
                    | (Builtin::FsWriteTextAsync, 0)
                    | (Builtin::FsAppendBytesAsync, 0)
                    | (Builtin::FsAppendTextAsync, 0)
                    | (Builtin::FsCopyAsync, 0)
                    | (Builtin::FsRenameAsync, 0) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::ProcessRunAsync, 1) => (
                        "array",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::dynamic_array(), actual),
                    ),
                    (Builtin::ProcessRunAsync, 2) if args.len() == 5 => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (Builtin::ProcessRunAsync, 2) if args.len() == 4 => (
                        "bytes, int, uint, or nil",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt)
                            || *actual == Type::Nil,
                    ),
                    (Builtin::ProcessRunAsync, 2) => (
                        "bytes, int, or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (Builtin::ProcessRunAsync, 3) => (
                        "int, uint, or nil",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt)
                            || *actual == Type::Nil,
                    ),
                    (Builtin::ProcessRunAsync, 4) => (
                        "int or uint",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || *actual == Type::concrete(VarType::Int)
                            || *actual == Type::concrete(VarType::UInt),
                    ),
                    (Builtin::FsWriteBytesAsync, 1) => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (Builtin::FsWriteTextAsync, 1) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::FsAppendBytesAsync, 1) => (
                        "bytes",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::Bytes, actual),
                    ),
                    (Builtin::FsAppendTextAsync, 1) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    (Builtin::FsCopyAsync | Builtin::FsRenameAsync, 1) => (
                        "str",
                        matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            || type_compatible(&Type::concrete(VarType::Str), actual),
                    ),
                    _ => continue,
                };
                if !valid {
                    self.argument_mismatch(name, expected, actual, &argument.value);
                }
            }
            if let Builtin::Native(ref import) = builtin {
                for (index, ((parameter, actual), argument)) in import
                    .parameters()
                    .iter()
                    .zip(&actual)
                    .zip(args)
                    .enumerate()
                {
                    let value = &argument.value;
                    if native_rejects_nil(import.name(), index) {
                        self.require_value(actual.clone(), value, &format!("call `{name}` with"));
                    }
                    use crate::runtime::NativeType as N;
                    if *parameter == N::Number {
                        if !matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                            && *actual != Type::concrete(VarType::Int)
                            && *actual != Type::concrete(VarType::UInt)
                            && *actual != Type::concrete(VarType::Float)
                        {
                            self.error(
                                "E2001",
                                format!(
                                    "type mismatch: `{name}` expects int, uint, or float, got {}",
                                    actual.display()
                                ),
                                value.span(),
                            );
                        }
                        continue;
                    }
                    let expected = match parameter {
                        N::Any => continue,
                        N::Number => {
                            unreachable!("number arguments use the numeric-union check above")
                        }
                        N::Int => Type::concrete(VarType::Int),
                        N::UInt => Type::concrete(VarType::UInt),
                        N::Float => Type::concrete(VarType::Float),
                        N::Bool => Type::concrete(VarType::Bool),
                        N::Char => Type::concrete(VarType::Char),
                        N::StrBuf => Type::StrBuf,
                        N::Str => Type::concrete(VarType::Str),
                        N::Bytes => Type::Bytes,
                        N::Handle => Type::Handle,
                        N::Nil => Type::Nil,
                        N::Array => Type::dynamic_array(),
                        N::Record => Type::Record,
                    };
                    if !matches!(actual, Type::Dynamic | Type::Poison | Type::ErrorValue)
                        && !type_compatible(&expected, actual)
                    {
                        self.argument_mismatch(name, &expected.display(), actual, value);
                    }
                }
            }
            return match builtin {
                Builtin::Len => Type::concrete(VarType::Int),
                Builtin::ErrorKind => Type::concrete(VarType::Str),
                Builtin::Exclusive => Type::concrete(VarType::Bool),
                Builtin::BuildInput => Type::Poison,
                Builtin::Spawn => actual.first().cloned().unwrap_or(Type::Poison),
                Builtin::Cancel => Type::concrete(VarType::Bool),
                Builtin::YieldNow => Type::Task(Box::new(Type::Nil)),
                Builtin::TimeoutAsync => match actual.first() {
                    Some(Type::Task(result)) => Type::Task(result.clone()),
                    _ => Type::Task(Box::new(Type::Dynamic)),
                },
                Builtin::Select => Type::Task(Box::new(Type::concrete(VarType::Int))),
                Builtin::Select2 => Type::Task(Box::new(Type::concrete(VarType::Int))),
                Builtin::SleepAsync => Type::Task(Box::new(Type::Nil)),
                Builtin::ReadBytesAsync => Type::Task(Box::new(Type::Bytes)),
                Builtin::AsyncIoRead => Type::Task(Box::new(Type::Bytes)),
                Builtin::AsyncIoReadExact => Type::Task(Box::new(Type::Bytes)),
                Builtin::AsyncIoReadAll => Type::Task(Box::new(Type::Bytes)),
                Builtin::AsyncIoOpen => Type::Task(Box::new(Type::Handle)),
                Builtin::AsyncIoSeek => Type::Task(Box::new(Type::concrete(VarType::Int))),
                Builtin::AsyncIoWrite | Builtin::TcpWriteAsync => {
                    Type::Task(Box::new(Type::concrete(VarType::UInt)))
                }
                Builtin::AsyncIoWriteAll | Builtin::AsyncIoFlush => Type::Task(Box::new(Type::Nil)),
                Builtin::ProcessRunAsync => Type::Task(Box::new(Type::Record)),
                Builtin::NetworkExchangeAsync => Type::Task(Box::new(Type::Bytes)),
                Builtin::TcpListenAsync | Builtin::TcpAcceptAsync | Builtin::TcpConnectAsync => {
                    Type::Task(Box::new(Type::Handle))
                }
                Builtin::TcpReadAsync | Builtin::TcpReadExactAsync | Builtin::TcpReadAllAsync => {
                    Type::Task(Box::new(Type::Bytes))
                }
                Builtin::TcpWriteAllAsync | Builtin::TcpShutdownWriteAsync => {
                    Type::Task(Box::new(Type::Nil))
                }
                Builtin::FsMetadataAsync => Type::Task(Box::new(Type::Record)),
                Builtin::FsReadDirAsync => Type::Task(Box::new(Type::Array(Box::new(
                    Type::concrete(VarType::Str),
                )))),
                Builtin::FsReadLinesAsync => Type::Task(Box::new(Type::Array(Box::new(
                    Type::concrete(VarType::Str),
                )))),
                Builtin::FsReadTextAsync => Type::Task(Box::new(Type::concrete(VarType::Str))),
                Builtin::FsCopyAsync => Type::Task(Box::new(Type::concrete(VarType::UInt))),
                Builtin::FsWriteBytesAsync
                | Builtin::FsWriteTextAsync
                | Builtin::FsAppendBytesAsync
                | Builtin::FsAppendTextAsync
                | Builtin::FsRenameAsync
                | Builtin::FsCreateDirAsync
                | Builtin::FsCreateDirAllAsync
                | Builtin::FsRemoveFileAsync
                | Builtin::FsRemoveDirAsync => Type::Task(Box::new(Type::Nil)),
                Builtin::Native(import) => {
                    let result = match import.result() {
                        crate::runtime::NativeType::Any if import.name() == "str::to_bigint" => {
                            Type::concrete(VarType::BigInt)
                        }
                        crate::runtime::NativeType::Int => Type::concrete(VarType::Int),
                        crate::runtime::NativeType::UInt => Type::concrete(VarType::UInt),
                        crate::runtime::NativeType::Float => Type::concrete(VarType::Float),
                        crate::runtime::NativeType::Bool => Type::concrete(VarType::Bool),
                        crate::runtime::NativeType::Char => Type::concrete(VarType::Char),
                        crate::runtime::NativeType::StrBuf => Type::StrBuf,
                        crate::runtime::NativeType::Str => Type::concrete(VarType::Str),
                        crate::runtime::NativeType::Nil => Type::Nil,
                        crate::runtime::NativeType::Bytes => Type::Bytes,
                        crate::runtime::NativeType::Handle => Type::Handle,
                        crate::runtime::NativeType::Array => Type::dynamic_array(),
                        crate::runtime::NativeType::Record => Type::Record,
                        _ => Type::Dynamic,
                    };
                    if import.is_async() {
                        Type::Task(Box::new(result))
                    } else {
                        result
                    }
                }
            };
        }
        // Wildcard native imports have no source declarations to enumerate.
        for scope in self.scopes.iter().rev() {
            for module in scope.modules.values() {
                if let Some(builtin) = Builtin::from_name(&format!("{module}::{name}"))
                    && builtin.accepts_arity(args.len())
                {
                    if let Builtin::Native(ref import) = builtin {
                        self.access_facts
                            .calls
                            .insert(crate::mutability::expr_id(expr), import.name().into());
                    }
                    return Type::Dynamic;
                }
            }
        }
        self.error("E1002", format!("unknown function `{name}`"), span);
        Type::Poison
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;

    #[test]
    fn normal_type_and_error_exclusion_facts_join_independently() {
        let id = 7;
        let non_nil_with_error_possible = FlowFacts {
            normal_types: HashMap::from([(id, Type::concrete(VarType::Int))]),
            error_excluded: HashSet::new(),
        };
        let mut non_nil_without_error = non_nil_with_error_possible.clone();
        non_nil_without_error.error_excluded.insert(id);

        let joined = FlowFacts::join([non_nil_with_error_possible, non_nil_without_error.clone()]);
        assert_eq!(
            joined.normal_types.get(&id),
            Some(&Type::concrete(VarType::Int))
        );
        assert!(!joined.error_excluded.contains(&id));

        let nullable_without_error = FlowFacts {
            normal_types: HashMap::new(),
            error_excluded: HashSet::from([id]),
        };
        let joined = FlowFacts::join([non_nil_without_error, nullable_without_error]);
        assert!(!joined.normal_types.contains_key(&id));
        assert!(joined.error_excluded.contains(&id));
    }
}
