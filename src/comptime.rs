//! Compile-time evaluation for constant values and compile-time-only functions.
//!
//! This phase runs after name resolution and before runtime semantic checking and
//! codegen. It may materialize only the implemented compile-time value forms;
//! forbidden runtime effects and evaluator budget exhaustion become diagnostics.
use crate::ast::{
    BinOp, BindingPattern, Block, CallArgument, Expr, FallbackKind, ForClause, FunctionDecl, Item,
    MatchPattern, Program, Stmt, StructField, TypeAnnotation, TypeKind, UnOp, Visibility,
};
use crate::diagnostic::{Diagnostic, SourceSpan};
use crate::error::RuntimeError;
use num_bigint::BigInt;
use std::cmp::Ordering;
use std::collections::HashMap;

const MAX_EVALUATION_STEPS: usize = 100_000;
const MAX_EVALUATION_DEPTH: usize = 256;
const MAX_FUNCTION_CALL_DEPTH: usize = 128;
const MAX_STRING_BYTES: usize = 1_000_000;
const MAX_SCALAR_FOLD_NODES: usize = 64;

#[derive(Clone, Debug)]
struct PropagatedError {
    error: RuntimeError,
    origin_call_depth: usize,
}

#[derive(Clone, Debug)]
enum ComptimeValue {
    Int(i64),
    UInt(u64),
    BigInt(BigInt),
    Float(f64),
    Char(char),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Nil,
    Error(RuntimeError),
    /// Internal control flow for `!!`, carried through calls until a caller
    /// handler catches it or the compile-time evaluation root reports it.
    Propagate(PropagatedError),
    /// Compiler-only type identity. It can be used in annotations but has no
    /// runtime representation and therefore cannot be materialized as Expr.
    Type(TypeAnnotation),
    Struct {
        name: String,
        fields: Vec<(String, ComptimeValue)>,
    },
}

impl ComptimeValue {
    fn to_expr_with_budget(
        &self,
        span: Option<SourceSpan>,
        budget: &mut Budget,
    ) -> Result<Expr, String> {
        match self {
            Self::Error(error) => return Err(error.to_string()),
            Self::Propagate(_) => {
                return Err(RuntimeError::TypeMismatch {
                    op: "comptime propagation",
                }
                .to_string());
            }
            Self::Type(_) => {
                return Err(RuntimeError::TypeMismatch {
                    op: "comptime type",
                }
                .to_string());
            }
            _ => {}
        }
        budget.allocate(self.clone_cost()?)?;
        self.to_expr(span).map_err(|error| error.to_string())
    }

    fn to_expr(&self, span: Option<SourceSpan>) -> Result<Expr, RuntimeError> {
        let value = match self {
            Self::Int(value) => Expr::Int(*value),
            Self::UInt(value) => Expr::UInt(*value),
            Self::BigInt(value) => Expr::BigInt(value.clone()),
            Self::Float(value) => Expr::Float(*value),
            Self::Char(value) => Expr::Char(*value),
            Self::Str(value) => Expr::Str(value.clone()),
            Self::Bytes(value) => Expr::Bytes(value.clone()),
            Self::Bool(value) => Expr::Bool(*value),
            Self::Nil => Expr::Nil,
            Self::Error(error) => return Err(error.clone()),
            Self::Propagate(_) => {
                return Err(RuntimeError::TypeMismatch {
                    op: "comptime propagation",
                });
            }
            Self::Type(_) => {
                return Err(RuntimeError::TypeMismatch {
                    op: "comptime type",
                });
            }
            Self::Struct { name, fields } => Expr::Call(
                name.clone(),
                fields
                    .iter()
                    .map(|(field, value)| Ok(CallArgument::named(field, value.to_expr(None)?)))
                    .collect::<Result<_, RuntimeError>>()?,
            ),
        };
        Ok(match span {
            Some(span) => Expr::Located(span, Box::new(value)),
            None => value,
        })
    }
}

fn clone_values_for_rewrite(
    values: &Values,
    budget: &mut Budget,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Values> {
    match budget.clone_values(values) {
        Ok(values) => Some(values),
        Err(message) => {
            diagnostics.push(comptime_error(message, None));
            None
        }
    }
}

#[derive(Default)]
struct Budget {
    steps: usize,
    allocated: usize,
    inputs: Option<crate::build_inputs::BuildInputs>,
}

impl Budget {
    fn allocate(&mut self, bytes: usize) -> Result<(), String> {
        self.allocated = self.allocated.saturating_add(bytes);
        if self.allocated > crate::ast_budget::MAX_COST {
            return Err("comptime allocation budget exceeded".into());
        }
        Ok(())
    }
    fn step(&mut self, depth: usize) -> Result<(), String> {
        self.allocate(0)?;
        if depth > MAX_EVALUATION_DEPTH {
            return Err("comptime evaluation depth limit exceeded".into());
        }
        self.steps = self
            .steps
            .checked_add(1)
            .ok_or_else(|| "comptime evaluator step limit exceeded".to_owned())?;
        if self.steps > MAX_EVALUATION_STEPS {
            return Err("comptime evaluator step limit exceeded".into());
        }
        Ok(())
    }

    fn clone_value(&mut self, value: &ComptimeValue) -> Result<ComptimeValue, String> {
        self.allocate(value.clone_cost()?)?;
        Ok(value.clone())
    }

    fn clone_values(&mut self, values: &Values) -> Result<Values, String> {
        let mut cost = values
            .len()
            .saturating_mul(std::mem::size_of::<(String, Option<ComptimeValue>)>());
        for (name, value) in values {
            cost = cost.saturating_add(name.len());
            if let Some(value) = value {
                cost = cost.saturating_add(value.clone_cost()?);
            }
        }
        self.allocate(cost)?;
        Ok(values.clone())
    }

    fn clone_error(&mut self, error: &RuntimeError) -> Result<RuntimeError, String> {
        self.allocate(ComptimeValue::error_clone_cost(error, 0))?;
        Ok(error.clone())
    }

    fn copy_string(&mut self, value: &str) -> Result<String, String> {
        self.allocate(value.len())?;
        let mut copy = String::new();
        copy.try_reserve_exact(value.len())
            .map_err(|_| "comptime string allocation failed".to_owned())?;
        copy.push_str(value);
        Ok(copy)
    }

    fn reserve_vec<T>(&mut self, values: &mut Vec<T>, count: usize) -> Result<(), String> {
        let bytes = count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or("comptime allocation size overflow")?;
        self.allocate(bytes)?;
        values
            .try_reserve_exact(count)
            .map_err(|_| "comptime vector allocation failed".to_owned())
    }

    fn insert_value(
        &mut self,
        values: &mut Values,
        name: &str,
        value: Option<ComptimeValue>,
    ) -> Result<(), String> {
        let entry_cost = std::mem::size_of::<(String, Option<ComptimeValue>)>().saturating_mul(2);
        self.allocate(entry_cost)?;
        values
            .try_reserve(1)
            .map_err(|_| "comptime environment allocation failed".to_owned())?;
        let key = self.copy_string(name)?;
        values.insert(key, value);
        Ok(())
    }
}

type Values = HashMap<String, Option<ComptimeValue>>;

/// Fold a closed scalar expression during runtime lowering. Runtime errors and
/// values without a JIT immediate representation stay in the ordinary VM path.
pub(crate) fn fold_scalar_expression(expr: &Expr) -> Option<crate::value::Value> {
    fold_constant_expression_inner(expr, false)
}

/// Fold a closed scalar or BigInt expression for runtime bytecode generation.
/// BigInt work stays bounded by the comptime evaluator's allocation budget.
pub(crate) fn fold_constant_expression(expr: &Expr) -> Option<crate::value::Value> {
    fold_constant_expression_inner(expr, true)
}

fn fold_constant_expression_inner(expr: &Expr, allow_bigint: bool) -> Option<crate::value::Value> {
    let mut nodes = 0;
    if !constant_foldable(expr, &mut nodes, allow_bigint) || has_float_negation(expr) {
        return None;
    }

    let globals = Values::new();
    let functions = HashMap::new();
    let constructors = HashMap::new();
    let types = HashMap::new();
    let mut budget = Budget::default();
    let mut context = EvalContext {
        globals: &globals,
        functions: &functions,
        structs: StructScope {
            constructors: &constructors,
            owner: None,
        },
        types: &types,
        budget: &mut budget,
    };
    match eval_expr(expr, &Values::new(), &mut context, 0, 0).ok()? {
        ComptimeValue::Int(value) => Some(crate::value::Value::Int(value)),
        ComptimeValue::UInt(value) => Some(crate::value::Value::UInt(value)),
        ComptimeValue::BigInt(value) if allow_bigint => Some(crate::value::Value::BigInt(value)),
        ComptimeValue::Float(value) => Some(crate::value::Value::Float(value)),
        ComptimeValue::Bool(value) => Some(crate::value::Value::Bool(value)),
        ComptimeValue::Char(value) => Some(crate::value::Value::Char(value)),
        ComptimeValue::Nil => Some(crate::value::Value::Nil),
        _ => None,
    }
}

fn constant_foldable(expr: &Expr, nodes: &mut usize, allow_bigint: bool) -> bool {
    *nodes += 1;
    if *nodes > MAX_SCALAR_FOLD_NODES {
        return false;
    }
    match expr.node() {
        Expr::Int(_)
        | Expr::UInt(_)
        | Expr::Float(_)
        | Expr::Char(_)
        | Expr::Bool(_)
        | Expr::Nil => true,
        Expr::BigInt(_) => allow_bigint,
        Expr::Unary(UnOp::Neg | UnOp::Not, value) => constant_foldable(value, nodes, allow_bigint),
        // A `bigint` left shift can exceed the runtime digit limit from small
        // operands, so the VM performs it and reports a recoverable Error.
        Expr::Binary(BinOp::Shl, left, right) => {
            constant_foldable(left, nodes, false) && constant_foldable(right, nodes, false)
        }
        Expr::Binary(
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Mod
            | BinOp::Eq
            | BinOp::NotEq
            | BinOp::Lt
            | BinOp::LtEq
            | BinOp::Gt
            | BinOp::GtEq
            | BinOp::And
            | BinOp::Or
            | BinOp::Xor
            | BinOp::Shr,
            left,
            right,
        ) => {
            constant_foldable(left, nodes, allow_bigint)
                && constant_foldable(right, nodes, allow_bigint)
        }
        _ => false,
    }
}

fn has_float_negation(expr: &Expr) -> bool {
    fn contains_float(expr: &Expr) -> bool {
        match expr.node() {
            Expr::Float(_) => true,
            Expr::Unary(_, value) | Expr::Propagate(value) | Expr::Cast(value, _) => {
                contains_float(value)
            }
            Expr::Binary(_, left, right) => contains_float(left) || contains_float(right),
            _ => false,
        }
    }

    match expr.node() {
        Expr::Unary(UnOp::Neg, value) => contains_float(value) || has_float_negation(value),
        Expr::Unary(_, value) | Expr::Propagate(value) | Expr::Cast(value, _) => {
            has_float_negation(value)
        }
        Expr::Binary(_, left, right) => has_float_negation(left) || has_float_negation(right),
        _ => false,
    }
}

impl ComptimeValue {
    fn error_clone_cost(error: &RuntimeError, depth: usize) -> usize {
        if depth > MAX_EVALUATION_DEPTH {
            return crate::ast_budget::MAX_COST;
        }
        let nested = match error {
            RuntimeError::MissingField { field } => field.len(),
            RuntimeError::NativeFailure { message } | RuntimeError::IoWriteFailed { message } => {
                message.len()
            }
            RuntimeError::IoFailure { message, .. } => message.len(),
            RuntimeError::ProcessFailed { command, .. } => command.len(),
            RuntimeError::WithProvenance { error, .. } => Self::error_clone_cost(error, depth + 1),
            RuntimeError::WithSuppressed {
                primary,
                suppressed,
            } => suppressed
                .iter()
                .fold(Self::error_clone_cost(primary, depth + 1), |cost, error| {
                    cost.saturating_add(Self::error_clone_cost(error, depth + 1))
                })
                .saturating_add(
                    suppressed
                        .len()
                        .saturating_mul(std::mem::size_of::<RuntimeError>()),
                ),
            RuntimeError::Crashed {
                value: crate::value::Value::Error(value),
                ..
            } => Self::error_clone_cost(value, depth + 1),
            _ => 0,
        };
        std::mem::size_of::<RuntimeError>().saturating_add(nested)
    }

    fn clone_cost(&self) -> Result<usize, String> {
        fn value_cost(value: &ComptimeValue, depth: usize) -> Result<usize, String> {
            if depth > MAX_EVALUATION_DEPTH {
                return Ok(crate::ast_budget::MAX_COST);
            }
            let mut cost = 256usize;
            match value {
                ComptimeValue::Str(text) => cost = cost.saturating_add(text.len()),
                ComptimeValue::Bytes(bytes) => cost = cost.saturating_add(bytes.len()),
                ComptimeValue::BigInt(value) => {
                    cost = cost.saturating_add(bigint_storage_cost(value));
                }
                ComptimeValue::Type(ty) => {
                    cost = cost.saturating_add(crate::ast_budget::annotation(ty)?);
                }
                ComptimeValue::Struct { name, fields } => {
                    cost = cost.saturating_add(name.len()).saturating_add(
                        fields
                            .len()
                            .saturating_mul(std::mem::size_of::<(String, ComptimeValue)>()),
                    );
                    for (field, value) in fields {
                        cost = cost
                            .saturating_add(field.len())
                            .saturating_add(value_cost(value, depth + 1)?);
                    }
                }
                ComptimeValue::Error(error) => {
                    cost = cost.saturating_add(ComptimeValue::error_clone_cost(error, depth));
                }
                ComptimeValue::Propagate(propagation) => {
                    cost = cost
                        .saturating_add(ComptimeValue::error_clone_cost(&propagation.error, depth));
                }
                ComptimeValue::Int(_)
                | ComptimeValue::UInt(_)
                | ComptimeValue::Float(_)
                | ComptimeValue::Char(_)
                | ComptimeValue::Bool(_)
                | ComptimeValue::Nil => {}
            }
            Ok(cost)
        }

        value_cost(self, 0)
    }
}

struct Constructor {
    fields: Vec<StructField>,
    owner: Option<String>,
}

#[derive(Clone, Copy)]
struct StructScope<'a> {
    constructors: &'a HashMap<String, Constructor>,
    owner: Option<&'a str>,
}

impl StructScope<'_> {
    fn owns(self, constructor: &Constructor) -> bool {
        self.owner.is_some() && self.owner == constructor.owner.as_deref()
    }
}

struct EvalContext<'a> {
    globals: &'a Values,
    functions: &'a HashMap<String, FunctionDecl>,
    structs: StructScope<'a>,
    types: &'a HashMap<String, TypeAnnotation>,
    budget: &'a mut Budget,
}

impl<'a> EvalContext<'a> {
    fn with_owner<T>(&mut self, owner: Option<&'a str>, eval: impl FnOnce(&mut Self) -> T) -> T {
        let previous = std::mem::replace(&mut self.structs.owner, owner);
        let result = eval(self);
        self.structs.owner = previous;
        result
    }
}

enum BlockExit {
    Return(ComptimeValue),
    Tail(ComptimeValue),
    Propagate(PropagatedError),
    Continue,
}

/// Evaluate declarations after name resolution and before semantic checking or
/// runtime lowering. Successful values become ordinary immutable bindings
/// initialized from materialized runtime values.
pub(crate) fn evaluate_program_with_inputs(
    program: &mut Program,
    inputs: &crate::build_inputs::BuildInputs,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut budget = Budget {
        inputs: Some(inputs.clone()),
        ..Budget::default()
    };
    let mut globals = Values::new();
    let functions: HashMap<_, _> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Function(function) if function.comptime => {
                Some((function.name.clone(), function.clone()))
            }
            _ => None,
        })
        .collect();
    let constructors: HashMap<_, _> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::StructDecl {
                name,
                fields,
                owner,
                ..
            } => Some((
                name.clone(),
                Constructor {
                    fields: fields.clone(),
                    owner: owner.clone(),
                },
            )),
            _ => None,
        })
        .collect();
    let mut types = HashMap::<String, TypeAnnotation>::new();
    let mut type_bindings = std::collections::HashSet::new();

    for item in &mut program.items {
        let Item::Comptime {
            name,
            visibility,
            ty,
            value,
        } = item
        else {
            continue;
        };
        let name = name.clone();
        let visibility = *visibility;
        let ty = ty.clone();
        let span = value.span().cloned();
        let is_type_binding = ty.as_ref().is_some_and(is_type_annotation);
        match evaluate(
            value,
            &globals,
            &globals,
            &functions,
            StructScope {
                constructors: &constructors,
                owner: span
                    .as_ref()
                    .and_then(|span| span.source.module_owner.get().map(String::as_str)),
            },
            &types,
            &mut budget,
        ) {
            Ok(ComptimeValue::Type(result)) if is_type_binding => {
                if let Err(message) = crate::ast_budget::annotation(&result) {
                    diagnostics.push(comptime_error(message, span));
                    globals.insert(name.clone(), None);
                    *item = Item::Invalid {
                        binding: Some(name),
                    };
                    continue;
                }
                globals.insert(name.clone(), Some(ComptimeValue::Type(result.clone())));
                types.insert(name.clone(), result);
                type_bindings.insert(name);
                *item = Item::Invalid { binding: None };
            }
            Ok(result) if is_type_binding => {
                diagnostics.push(comptime_error(
                    format!(
                        "type binding expects a type value, got {}",
                        comptime_value_type_name(&result)
                    ),
                    span,
                ));
                globals.insert(name.clone(), None);
                *item = Item::Invalid {
                    binding: Some(name),
                };
            }
            Ok(result) => match result.to_expr_with_budget(span.clone(), &mut budget) {
                Ok(value) => {
                    globals.insert(name.clone(), Some(result));
                    *item = Item::GlobalConst {
                        name,
                        visibility,
                        ty,
                        value,
                    };
                }
                Err(error) => {
                    diagnostics.push(comptime_error(error, span));
                    globals.insert(name.clone(), None);
                    *item = Item::Invalid {
                        binding: Some(name),
                    };
                }
            },
            Err(message) => {
                diagnostics.push(comptime_error(message, span));
                globals.insert(name.clone(), None);
                *item = Item::Invalid {
                    binding: Some(name),
                };
            }
        }
    }

    substitute_program_types(program, &types);
    program.items.retain(|item| match item {
        Item::Invalid { binding: None } => false,
        Item::Comptime { name, .. } => !type_bindings.contains(name),
        _ => true,
    });

    for item in &mut program.items {
        match item {
            Item::Function(function) if !function.comptime => evaluate_function(
                function,
                &globals,
                &functions,
                &constructors,
                &mut budget,
                &mut diagnostics,
            ),
            Item::Extend(extension) => {
                for method in &mut extension.methods {
                    if !method.function.comptime {
                        evaluate_function(
                            &mut method.function,
                            &globals,
                            &functions,
                            &constructors,
                            &mut budget,
                            &mut diagnostics,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    diagnostics
}

fn evaluate_function(
    function: &mut FunctionDecl,
    globals: &Values,
    functions: &HashMap<String, FunctionDecl>,
    constructors: &HashMap<String, Constructor>,
    budget: &mut Budget,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut values = Values::new();
    for parameter in &function.params {
        values.insert(parameter.name.clone(), None);
    }
    rewrite_block(
        &mut function.body,
        &mut values,
        globals,
        functions,
        StructScope {
            constructors,
            owner: function.owner.as_deref(),
        },
        budget,
        diagnostics,
    );
}

fn rewrite_block(
    block: &mut Block,
    outer: &mut Values,
    globals: &Values,
    functions: &HashMap<String, FunctionDecl>,
    structs: StructScope<'_>,
    budget: &mut Budget,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(mut values) = clone_values_for_rewrite(outer, budget, diagnostics) else {
        return;
    };
    for statement in &mut block.stmts {
        rewrite_stmt(
            statement,
            &mut values,
            globals,
            functions,
            structs,
            budget,
            diagnostics,
        );
    }
    if let Some(tail) = &mut block.tail {
        rewrite_expr(
            tail,
            &mut values,
            globals,
            functions,
            structs,
            budget,
            diagnostics,
        );
    }
}

fn rewrite_stmt(
    statement: &mut Stmt,
    values: &mut Values,
    globals: &Values,
    functions: &HashMap<String, FunctionDecl>,
    structs: StructScope<'_>,
    budget: &mut Budget,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match statement.node().clone() {
        Stmt::Comptime { name, ty, value } => {
            let span = value.span().cloned().or_else(|| statement.span().cloned());
            let mut types = HashMap::new();
            for (binding, value) in globals.iter().chain(values.iter()) {
                if let Some(ComptimeValue::Type(ty)) = value {
                    types.insert(binding.clone(), ty.clone());
                }
            }
            match evaluate(&value, values, globals, functions, structs, &types, budget) {
                Ok(result) => match result.to_expr_with_budget(span.clone(), budget) {
                    Ok(value) => {
                        values.insert(name.clone(), Some(result));
                        replace_stmt(
                            statement,
                            Stmt::Const {
                                pattern: BindingPattern::Name(name),
                                ty,
                                value,
                            },
                        );
                    }
                    Err(error) => {
                        diagnostics.push(comptime_error(error, span));
                        values.insert(name.clone(), None);
                        replace_stmt(
                            statement,
                            Stmt::Invalid {
                                binding: Some(name),
                            },
                        );
                    }
                },
                Err(message) => {
                    diagnostics.push(comptime_error(message, span));
                    values.insert(name.clone(), None);
                    replace_stmt(
                        statement,
                        Stmt::Invalid {
                            binding: Some(name),
                        },
                    );
                }
            }
        }
        Stmt::Const {
            pattern, mut value, ..
        }
        | Stmt::VarDecl {
            pattern, mut value, ..
        } => {
            rewrite_expr(
                &mut value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            pattern.visit_names(&mut |name| {
                values.insert(name.to_owned(), None);
            });
            replace_binding_value(statement, pattern, value);
        }
        Stmt::Assign {
            mut target,
            mut value,
        }
        | Stmt::CompoundAssign {
            mut target,
            mut value,
            ..
        } => {
            rewrite_expr(
                &mut target,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_expr(
                &mut value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_assignment(statement, target, value);
        }
        Stmt::Return(Some(mut value)) | Stmt::Expr(mut value) | Stmt::Crash { mut value, .. } => {
            rewrite_expr(
                &mut value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_value_statement(statement, value);
        }
        Stmt::Defer { mut body } => {
            rewrite_block(
                &mut body,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(statement, Stmt::Defer { body });
        }
        Stmt::Try {
            mut body,
            catch_name,
            mut catch_block,
        } => {
            rewrite_block(
                &mut body,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            let Some(mut catch_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            catch_values.insert(catch_name.clone(), None);
            rewrite_block(
                &mut catch_block,
                &mut catch_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(
                statement,
                Stmt::Try {
                    body,
                    catch_name,
                    catch_block,
                },
            );
        }
        Stmt::If {
            mut cond,
            mut then_block,
            mut else_block,
        } => {
            rewrite_expr(
                &mut cond,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_block(
                &mut then_block,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            if let Some(block) = &mut else_block {
                rewrite_block(
                    block,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
            replace_stmt(
                statement,
                Stmt::If {
                    cond,
                    then_block,
                    else_block,
                },
            );
        }
        Stmt::When {
            name,
            mut value,
            mut then_block,
            mut else_block,
        } => {
            rewrite_expr(
                &mut value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            let Some(mut then_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            then_values.insert(name.clone(), None);
            rewrite_block(
                &mut then_block,
                &mut then_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            if let Some(block) = &mut else_block {
                rewrite_block(
                    block,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
            replace_stmt(
                statement,
                Stmt::When {
                    name,
                    value,
                    then_block,
                    else_block,
                },
            );
        }
        Stmt::While { mut cond, mut body } => {
            rewrite_expr(
                &mut cond,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_block(
                &mut body,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(statement, Stmt::While { cond, body });
        }
        Stmt::For {
            var,
            mut start,
            mut end,
            mut step,
            mut body,
        } => {
            rewrite_expr(
                &mut start,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_expr(
                &mut end,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            if let Some(step) = &mut step {
                rewrite_expr(
                    step,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
            let Some(mut loop_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            loop_values.insert(var.clone(), None);
            rewrite_block(
                &mut body,
                &mut loop_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(
                statement,
                Stmt::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                },
            );
        }
        Stmt::ForEach {
            pattern,
            mut iterable,
            mut body,
        } => {
            rewrite_expr(
                &mut iterable,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            let Some(mut loop_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            let mut names = Vec::new();
            pattern.visit_names(&mut |name| names.push(name.to_owned()));
            for name in names {
                loop_values.insert(name, None);
            }
            rewrite_block(
                &mut body,
                &mut loop_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(
                statement,
                Stmt::ForEach {
                    pattern,
                    iterable,
                    body,
                },
            );
        }
        Stmt::ForZip {
            mut clauses,
            mut body,
        } => {
            for clause in &mut clauses {
                match clause {
                    ForClause::Range {
                        start, end, step, ..
                    } => {
                        rewrite_expr(
                            start,
                            values,
                            globals,
                            functions,
                            structs,
                            budget,
                            diagnostics,
                        );
                        rewrite_expr(
                            end,
                            values,
                            globals,
                            functions,
                            structs,
                            budget,
                            diagnostics,
                        );
                        if let Some(step) = step {
                            rewrite_expr(
                                step,
                                values,
                                globals,
                                functions,
                                structs,
                                budget,
                                diagnostics,
                            );
                        }
                    }
                    ForClause::Each { iterable, .. } => rewrite_expr(
                        iterable,
                        values,
                        globals,
                        functions,
                        structs,
                        budget,
                        diagnostics,
                    ),
                }
            }
            let Some(mut loop_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            for clause in &clauses {
                match clause {
                    ForClause::Range { var, .. } => {
                        loop_values.insert(var.clone(), None);
                    }
                    ForClause::Each { pattern, .. } => {
                        let mut names = Vec::new();
                        pattern.visit_names(&mut |name| names.push(name.to_owned()));
                        for name in names {
                            loop_values.insert(name, None);
                        }
                    }
                }
            }
            rewrite_block(
                &mut body,
                &mut loop_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            replace_stmt(statement, Stmt::ForZip { clauses, body });
        }
        Stmt::Use { .. } | Stmt::Break | Stmt::Continue | Stmt::Invalid { .. } => {}
        Stmt::Located(_, _) => unreachable!("node removes locations"),
        Stmt::Return(None) => {}
    }
}

fn rewrite_expr(
    expr: &mut Expr,
    values: &mut Values,
    globals: &Values,
    functions: &HashMap<String, FunctionDecl>,
    structs: StructScope<'_>,
    budget: &mut Budget,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match expr {
        Expr::Located(_, inner) => rewrite_expr(
            inner,
            values,
            globals,
            functions,
            structs,
            budget,
            diagnostics,
        ),
        Expr::Array(items) | Expr::Tuple(items) => {
            for item in items {
                rewrite_expr(
                    item,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::Interpolated(parts) => {
            for part in parts {
                if let crate::ast::StringPart::Expression(expr) = part {
                    rewrite_expr(
                        expr,
                        values,
                        globals,
                        functions,
                        structs,
                        budget,
                        diagnostics,
                    );
                }
            }
        }
        Expr::Record(fields) => {
            for (_, value) in fields {
                rewrite_expr(
                    value,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::SpecializedCall(_, types, _) => {
            let aliases = globals
                .iter()
                .chain(values.iter())
                .filter_map(|(name, value)| {
                    if let Some(ComptimeValue::Type(ty)) = value {
                        Some((name.clone(), ty.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            for ty in types {
                substitute_type(ty, &aliases);
            }
            if let Expr::SpecializedCall(_, _, arguments) = expr {
                for argument in arguments {
                    rewrite_expr(
                        &mut argument.value,
                        values,
                        globals,
                        functions,
                        structs,
                        budget,
                        diagnostics,
                    );
                }
            }
        }
        Expr::AssociatedCall {
            target,
            type_arguments,
            args,
            ..
        } => {
            let aliases = globals
                .iter()
                .chain(values.iter())
                .filter_map(|(name, value)| {
                    if let Some(ComptimeValue::Type(ty)) = value {
                        Some((name.clone(), ty.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            substitute_type(target, &aliases);
            for ty in type_arguments {
                substitute_type(ty, &aliases);
            }
            for argument in args {
                rewrite_expr(
                    &mut argument.value,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::BracketApply(_, _) | Expr::Apply(_, _) => {}
        Expr::Call(_, arguments) => {
            for argument in arguments {
                rewrite_expr(
                    &mut argument.value,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::MethodCall(receiver, _, arguments) => {
            rewrite_expr(
                receiver,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            for argument in arguments {
                rewrite_expr(
                    argument,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::Index(receiver, index) | Expr::Binary(_, receiver, index) => {
            rewrite_expr(
                receiver,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_expr(
                index,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
        }
        Expr::Unary(_, value) | Expr::Propagate(value) | Expr::Cast(value, _) => {
            rewrite_expr(
                value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
        }
        Expr::Fallback {
            value, fallback, ..
        } => {
            rewrite_expr(
                value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_expr(
                fallback,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
        }
        Expr::Block(block) | Expr::Capture(block) => rewrite_block(
            block,
            values,
            globals,
            functions,
            structs,
            budget,
            diagnostics,
        ),
        Expr::Match { value, arms, .. } => {
            rewrite_expr(
                value,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            for arm in arms {
                let Some(mut arm_values) = clone_values_for_rewrite(values, budget, diagnostics)
                else {
                    return;
                };
                if let crate::ast::MatchPattern::Variant { bindings, .. } = &arm.pattern {
                    for binding in bindings {
                        arm_values.insert(binding.clone(), None);
                    }
                }
                if let Some(guard) = &mut arm.guard {
                    rewrite_expr(
                        guard,
                        &mut arm_values,
                        globals,
                        functions,
                        structs,
                        budget,
                        diagnostics,
                    );
                }
                rewrite_block(
                    &mut arm.body,
                    &mut arm_values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::Try {
            body,
            catch_name,
            catch_block,
        } => {
            rewrite_block(
                body,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            let Some(mut catch_values) = clone_values_for_rewrite(values, budget, diagnostics)
            else {
                return;
            };
            catch_values.insert(catch_name.clone(), None);
            rewrite_block(
                catch_block,
                &mut catch_values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(
                cond,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            rewrite_expr(
                then_branch,
                values,
                globals,
                functions,
                structs,
                budget,
                diagnostics,
            );
            if let Some(branch) = else_branch {
                rewrite_expr(
                    branch,
                    values,
                    globals,
                    functions,
                    structs,
                    budget,
                    diagnostics,
                );
            }
        }
        Expr::Int(_)
        | Expr::UInt(_)
        | Expr::BigInt(_)
        | Expr::Float(_)
        | Expr::Char(_)
        | Expr::Str(_)
        | Expr::Bytes(_)
        | Expr::Bool(_)
        | Expr::Nil
        | Expr::TypeLiteral(_)
        | Expr::Closure { .. }
        | Expr::Ident(_) => {}
    }
}

fn evaluate(
    expr: &Expr,
    values: &Values,
    globals: &Values,
    functions: &HashMap<String, FunctionDecl>,
    structs: StructScope<'_>,
    types: &HashMap<String, TypeAnnotation>,
    budget: &mut Budget,
) -> Result<ComptimeValue, String> {
    let mut context = EvalContext {
        globals,
        functions,
        structs,
        types,
        budget,
    };
    validate_expr(expr, values, &mut context, 0)?;
    let value = match eval_expr(expr, values, &mut context, 0, 0)? {
        ComptimeValue::Propagate(propagation) => ComptimeValue::Error(propagation.error),
        value => value,
    };
    context.budget.allocate(value.clone_cost()?)?;
    Ok(value)
}

fn validate_expr(
    expr: &Expr,
    values: &Values,
    context: &mut EvalContext<'_>,
    depth: usize,
) -> Result<(), String> {
    context.budget.step(depth)?;
    match expr.node() {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Char(_)
        | Expr::Bool(_)
        | Expr::Nil
        | Expr::TypeLiteral(_)
        | Expr::Bytes(_) => Ok(()),
        Expr::Str(value) if value.len() <= MAX_STRING_BYTES => Ok(()),
        Expr::Str(_) => Err("comptime string size limit exceeded".into()),
        Expr::Ident(name) => match values.get(name).or_else(|| context.globals.get(name)) {
            Some(Some(_)) => Ok(()),
            Some(None) => Err(format!(
                "runtime value `{name}` is not available at comptime"
            )),
            None if context.types.contains_key(name) || primitive_type(name).is_some() => Ok(()),
            None => Err(format!("value `{name}` is not available at comptime")),
        },
        Expr::Propagate(_) => Err("`!!` requires an enclosing comptime callable".into()),
        Expr::Unary(_, value) => validate_expr(value, values, context, depth + 1),
        Expr::Fallback {
            value, fallback, ..
        } => {
            validate_expr(value, values, context, depth + 1)?;
            validate_expr(fallback, values, context, depth + 1)
        }
        Expr::Binary(_, left, right) => {
            validate_expr(left, values, context, depth + 1)?;
            validate_expr(right, values, context, depth + 1)
        }
        Expr::Index(receiver, index) => {
            if !matches!(index.node(), Expr::Str(_) | Expr::Int(0..)) {
                return Err(
                    "comptime field access requires a literal field name or position".into(),
                );
            }
            validate_expr(receiver, values, context, depth + 1)?;
            validate_expr(index, values, context, depth + 1)
        }
        Expr::AssociatedCall { .. } => {
            Err("type-associated extension calls are not available at comptime yet".into())
        }
        Expr::Call(name, arguments) | Expr::SpecializedCall(name, _, arguments) => {
            if matches!(
                crate::builtins::Builtin::from_name(name),
                Some(crate::builtins::Builtin::BuildInput)
            ) {
                if arguments.len() != 1
                    || arguments[0]
                        .name
                        .as_deref()
                        .is_some_and(|name| name != "path")
                {
                    return Err("build input calls require exactly one `path` argument".into());
                }
                return validate_expr(&arguments[0].value, values, context, depth + 1);
            }
            let parameters = if let Some(constructor) = context.structs.constructors.get(name) {
                constructor.fields.as_slice()
            } else if let Some(function) = context.functions.get(name).filter(|f| f.comptime) {
                function.params.as_slice()
            } else if context.functions.contains_key(name) {
                return Err(format!(
                    "runtime function `{name}` cannot execute at comptime"
                ));
            } else {
                return Err(format!(
                    "call to `{name}` is runtime-only and cannot execute at comptime"
                ));
            };
            let bindings = crate::arguments::bind(name, parameters, arguments);
            if let Some(error) = bindings.errors.first() {
                return Err(error.message.clone());
            }
            for argument in arguments {
                validate_expr(&argument.value, values, context, depth + 1)?;
            }
            for (index, parameter) in parameters.iter().enumerate() {
                if bindings.by_parameter[index].is_none()
                    && let Some(default) = &parameter.default
                {
                    validate_expr(default, values, context, depth + 1)?;
                }
            }
            Ok(())
        }
        _ => Err("this expression is not supported at comptime yet".into()),
    }
}

fn eval_expr(
    expr: &Expr,
    values: &Values,
    context: &mut EvalContext<'_>,
    depth: usize,
    call_depth: usize,
) -> Result<ComptimeValue, String> {
    context.budget.step(depth)?;
    match expr.node() {
        Expr::Int(value) => Ok(ComptimeValue::Int(*value)),
        Expr::UInt(value) => Ok(ComptimeValue::UInt(*value)),
        Expr::BigInt(value) => {
            context.budget.allocate(bigint_storage_cost(value))?;
            Ok(ComptimeValue::BigInt(value.clone()))
        }
        Expr::Float(value) => Ok(ComptimeValue::Float(*value)),
        Expr::Char(value) => Ok(ComptimeValue::Char(*value)),
        Expr::Str(value) => {
            context.budget.allocate(value.len())?;
            Ok(ComptimeValue::Str(value.clone()))
        }
        Expr::Bytes(value) => {
            context.budget.allocate(value.len())?;
            Ok(ComptimeValue::Bytes(value.clone()))
        }
        Expr::Bool(value) => Ok(ComptimeValue::Bool(*value)),
        Expr::Nil => Ok(ComptimeValue::Nil),
        Expr::TypeLiteral(ty) => {
            context
                .budget
                .allocate(crate::ast_budget::annotation(ty)?)?;
            let mut ty = ty.clone();
            substitute_type(&mut ty, context.types);
            Ok(ComptimeValue::Type(ty))
        }
        Expr::Ident(name) => {
            let binding = values.get(name).or_else(|| context.globals.get(name));
            if let Some(Some(value)) = binding {
                context.budget.clone_value(value)
            } else if let Some(ty) = context.types.get(name) {
                context
                    .budget
                    .allocate(crate::ast_budget::annotation(ty)?)?;
                Ok(ComptimeValue::Type(ty.clone()))
            } else if let Some(ty) = primitive_type(name) {
                Ok(ComptimeValue::Type(ty))
            } else {
                Err(format!("value `{name}` is not available at comptime"))
            }
        }
        Expr::Propagate(value) => {
            let value = eval_expr(value, values, context, depth + 1, call_depth)?;
            match value {
                ComptimeValue::Error(error) if call_depth > 0 => {
                    Ok(ComptimeValue::Propagate(PropagatedError {
                        error,
                        origin_call_depth: call_depth,
                    }))
                }
                ComptimeValue::Error(_) => {
                    Err("`!!` requires an enclosing comptime callable".into())
                }
                other => Ok(other),
            }
        }
        Expr::Fallback {
            kind,
            value,
            fallback,
        } => {
            let value = eval_expr(value, values, context, depth + 1, call_depth)?;
            if matches!(&value, ComptimeValue::Propagate(_)) {
                return Ok(value);
            }
            let use_fallback = match kind {
                FallbackKind::Nil => matches!(&value, ComptimeValue::Nil),
                FallbackKind::Error => matches!(&value, ComptimeValue::Error(_)),
            };
            if use_fallback {
                eval_expr(fallback, values, context, depth + 1, call_depth)
            } else {
                Ok(value)
            }
        }
        Expr::Unary(op, value) => {
            let value = eval_expr(value, values, context, depth + 1, call_depth)?;
            if let ComptimeValue::Error(error) = value {
                return Ok(ComptimeValue::Error(error));
            }
            if let ComptimeValue::Propagate(error) = value {
                return Ok(ComptimeValue::Propagate(error));
            }
            match (op, value) {
                (UnOp::Neg, ComptimeValue::Int(value)) => Ok(value.checked_neg().map_or(
                    ComptimeValue::Error(RuntimeError::Overflow),
                    ComptimeValue::Int,
                )),
                (UnOp::Neg, ComptimeValue::BigInt(value)) => {
                    context.budget.allocate(bigint_storage_cost(&value))?;
                    Ok(ComptimeValue::BigInt(-value))
                }
                (UnOp::Neg, ComptimeValue::Float(value)) => Ok(ComptimeValue::Float(-value)),
                (UnOp::Not, ComptimeValue::Bool(value)) => Ok(ComptimeValue::Bool(!value)),
                _ => Err("invalid unary operation during comptime evaluation".into()),
            }
        }
        Expr::Binary(BinOp::And, left, right) => {
            match eval_expr(left, values, context, depth + 1, call_depth)? {
                ComptimeValue::Bool(false) => Ok(ComptimeValue::Bool(false)),
                ComptimeValue::Bool(true) => {
                    match eval_expr(right, values, context, depth + 1, call_depth)? {
                        ComptimeValue::Bool(value) => Ok(ComptimeValue::Bool(value)),
                        ComptimeValue::Error(error) => Ok(ComptimeValue::Error(error)),
                        ComptimeValue::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                        _ => Err("logical operator requires bool values".into()),
                    }
                }
                ComptimeValue::Error(error) => Ok(ComptimeValue::Error(error)),
                ComptimeValue::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                _ => Err("logical operator requires bool values".into()),
            }
        }
        Expr::Binary(BinOp::Or, left, right) => {
            match eval_expr(left, values, context, depth + 1, call_depth)? {
                ComptimeValue::Bool(true) => Ok(ComptimeValue::Bool(true)),
                ComptimeValue::Bool(false) => {
                    match eval_expr(right, values, context, depth + 1, call_depth)? {
                        ComptimeValue::Bool(value) => Ok(ComptimeValue::Bool(value)),
                        ComptimeValue::Error(error) => Ok(ComptimeValue::Error(error)),
                        ComptimeValue::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                        _ => Err("logical operator requires bool values".into()),
                    }
                }
                ComptimeValue::Error(error) => Ok(ComptimeValue::Error(error)),
                ComptimeValue::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                _ => Err("logical operator requires bool values".into()),
            }
        }
        Expr::Binary(op, left, right) => {
            let left = eval_expr(left, values, context, depth + 1, call_depth)?;
            let right = eval_expr(right, values, context, depth + 1, call_depth)?;
            eval_binary(*op, left, right, context.budget)
        }
        Expr::Index(receiver, index) => {
            if !matches!(index.node(), Expr::Str(_) | Expr::Int(0..)) {
                return Err(
                    "comptime field access requires a literal field name or position".into(),
                );
            }
            let receiver = eval_expr(receiver, values, context, depth + 1, call_depth)?;
            if matches!(
                receiver,
                ComptimeValue::Error(_) | ComptimeValue::Propagate(_)
            ) {
                return Ok(receiver);
            }
            let ComptimeValue::Struct { name, fields } = receiver else {
                return Err("comptime field access requires a struct".into());
            };
            let positional;
            let field = match index.node() {
                Expr::Str(field) if field.len() <= MAX_STRING_BYTES => field.as_str(),
                Expr::Str(_) => return Err("comptime string size limit exceeded".into()),
                Expr::Int(position) => {
                    context.budget.allocate(20)?;
                    positional = position.to_string();
                    positional.as_str()
                }
                _ => unreachable!("literal field key was checked"),
            };
            let constructor = context
                .structs
                .constructors
                .get(&name)
                .ok_or_else(|| format!("unknown comptime struct `{name}`"))?;
            for (declaration, (field_name, value)) in constructor.fields.iter().zip(fields) {
                if field_name == field {
                    if declaration.visibility == Visibility::Private
                        && !context.structs.owns(constructor)
                    {
                        return Err(format!("field `{field}` is private"));
                    }
                    return Ok(value);
                }
            }
            Err(format!("unknown field `{field}`"))
        }
        Expr::AssociatedCall { .. } => {
            Err("type-associated extension calls are not available at comptime yet".into())
        }
        Expr::Call(name, arguments) | Expr::SpecializedCall(name, _, arguments) => {
            if matches!(
                crate::builtins::Builtin::from_name(name),
                Some(crate::builtins::Builtin::BuildInput)
            ) {
                let path = eval_expr(&arguments[0].value, values, context, depth + 1, call_depth)?;
                let ComptimeValue::Str(path) = path else {
                    return Err("build input path must be a comptime-known string".into());
                };
                let inputs = context
                    .budget
                    .inputs
                    .as_ref()
                    .ok_or("missing compiler build input context")?;
                return match inputs.read(expr.span(), &path)? {
                    Err(error) => Ok(ComptimeValue::Error(error)),
                    Ok(None) if name.starts_with("build::try_") => Ok(ComptimeValue::Nil),
                    Ok(None) => Ok(ComptimeValue::Error(RuntimeError::IoFailure {
                        op: "build input",
                        message: format!("file not found: {path}"),
                    })),
                    Ok(Some(bytes)) if name.ends_with("include_str") => {
                        context.budget.allocate(bytes.len())?;
                        Ok(match String::from_utf8(bytes) {
                            Ok(text) => ComptimeValue::Str(text),
                            Err(_) => ComptimeValue::Error(RuntimeError::ParseFailure {
                                from: "bytes",
                                to: "str",
                            }),
                        })
                    }
                    Ok(Some(bytes)) => {
                        context.budget.allocate(bytes.len())?;
                        Ok(ComptimeValue::Bytes(bytes))
                    }
                };
            }
            if let Some(constructor) = context.structs.constructors.get(name) {
                if !context.structs.owns(constructor)
                    && constructor
                        .fields
                        .iter()
                        .any(|field| field.visibility == Visibility::Private)
                {
                    return Err(format!(
                        "cannot construct `{name}` outside its defining module because it has private fields"
                    ));
                }
                return eval_struct_constructor(
                    name,
                    constructor,
                    arguments,
                    values,
                    context,
                    depth + 1,
                    call_depth,
                );
            }
            let function = context
                .functions
                .get(name)
                .filter(|function| function.comptime)
                .ok_or_else(|| format!("runtime function `{name}` cannot execute at comptime"))?;
            let bindings = crate::arguments::bind(name, &function.params, arguments);
            if let Some(error) = bindings.errors.first() {
                return Err(error.message.clone());
            }
            let mut supplied = Vec::new();
            context.budget.reserve_vec(&mut supplied, arguments.len())?;
            for argument in arguments {
                let value = eval_expr(&argument.value, values, context, depth + 1, call_depth)?;
                if matches!(value, ComptimeValue::Propagate(_)) {
                    return Ok(value);
                }
                supplied.push(value);
            }
            context.with_owner(function.owner.as_deref(), |context| {
                eval_comptime_function(
                    function,
                    (&bindings.by_parameter, &supplied),
                    context,
                    depth + 1,
                    call_depth + 1,
                )
            })
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let condition = eval_expr(cond, values, context, depth + 1, call_depth)?;
            match condition {
                ComptimeValue::Bool(true) => {
                    eval_expr(then_branch, values, context, depth + 1, call_depth)
                }
                ComptimeValue::Bool(false) => match else_branch {
                    Some(branch) => eval_expr(branch, values, context, depth + 1, call_depth),
                    None => Ok(ComptimeValue::Nil),
                },
                ComptimeValue::Error(error) => Ok(ComptimeValue::Error(error)),
                ComptimeValue::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                _ => Err("comptime if condition must be bool".into()),
            }
        }
        Expr::Match { value, arms, .. } => {
            let value = eval_expr(value, values, context, depth + 1, call_depth)?;
            if let ComptimeValue::Propagate(error) = value {
                return Ok(ComptimeValue::Propagate(error));
            }
            for arm in arms {
                if !matches_pattern(&arm.pattern, &value) {
                    continue;
                }
                if let Some(guard) = &arm.guard {
                    match eval_expr(guard, values, context, depth + 1, call_depth)? {
                        ComptimeValue::Bool(true) => {}
                        ComptimeValue::Bool(false) => continue,
                        ComptimeValue::Error(error) => return Ok(ComptimeValue::Error(error)),
                        ComptimeValue::Propagate(error) => {
                            return Ok(ComptimeValue::Propagate(error));
                        }
                        _ => return Err("comptime match guard must be bool".into()),
                    }
                }
                let mut arm_values = context.budget.clone_values(values)?;
                return match eval_comptime_block(
                    &arm.body,
                    &mut arm_values,
                    context,
                    depth + 1,
                    call_depth,
                )? {
                    BlockExit::Return(value) | BlockExit::Tail(value) => Ok(value),
                    BlockExit::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                    BlockExit::Continue => Ok(ComptimeValue::Nil),
                };
            }
            Ok(ComptimeValue::Nil)
        }
        Expr::Block(block) | Expr::Capture(block) => {
            let mut values = context.budget.clone_values(values)?;
            match eval_comptime_block(block, &mut values, context, depth + 1, call_depth)? {
                BlockExit::Return(value) | BlockExit::Tail(value) => Ok(value),
                BlockExit::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                BlockExit::Continue => Ok(ComptimeValue::Nil),
            }
        }
        Expr::Try {
            body,
            catch_name,
            catch_block,
        } => {
            let mut values = context.budget.clone_values(values)?;
            match eval_comptime_try(
                body,
                catch_name,
                catch_block,
                &mut values,
                context,
                depth + 1,
                call_depth,
            )? {
                BlockExit::Return(value) | BlockExit::Tail(value) => Ok(value),
                BlockExit::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
                BlockExit::Continue => Ok(ComptimeValue::Nil),
            }
        }
        _ => Err("this expression is not supported at comptime yet".into()),
    }
}

fn eval_struct_constructor<'a>(
    name: &str,
    constructor: &'a Constructor,
    arguments: &[CallArgument],
    values: &Values,
    context: &mut EvalContext<'a>,
    depth: usize,
    call_depth: usize,
) -> Result<ComptimeValue, String> {
    let fields = &constructor.fields;
    let bindings = crate::arguments::bind(name, fields, arguments);
    if let Some(error) = bindings.errors.first() {
        return Err(error.message.clone());
    }

    let mut supplied = Vec::new();
    context.budget.reserve_vec(&mut supplied, arguments.len())?;
    for argument in arguments {
        let value = eval_expr(&argument.value, values, context, depth + 1, call_depth)?;
        if matches!(value, ComptimeValue::Propagate(_)) {
            return Ok(value);
        }
        supplied.push(value);
    }

    let mut result = Vec::new();
    context.budget.reserve_vec(&mut result, fields.len())?;
    for (index, field) in fields.iter().enumerate() {
        let mut value = if let Some(argument) = bindings.by_parameter[index] {
            context.budget.clone_value(&supplied[argument])?
        } else if let Some(default) = &field.default {
            context.with_owner(constructor.owner.as_deref(), |context| {
                eval_expr(default, values, context, depth + 1, call_depth)
            })?
        } else {
            return Err(format!("`{name}` expects argument `{}`", field.name));
        };
        if matches!(value, ComptimeValue::Propagate(_)) {
            return Ok(value);
        }
        if let Some(ty) = &field.ty {
            value = contextualize_comptime_value(value, ty);
            if !comptime_value_matches_annotation(&value, ty) {
                return Err(format!(
                    "type mismatch: field `{}` of `{name}` expects {}, got {}",
                    field.name,
                    ty.display(),
                    comptime_value_type_name(&value)
                ));
            }
        }
        let field_name = context.budget.copy_string(&field.name)?;
        result.push((field_name, value));
    }
    Ok(ComptimeValue::Struct {
        name: context.budget.copy_string(name)?,
        fields: result,
    })
}

fn eval_comptime_function(
    function: &FunctionDecl,
    arguments: (&[Option<usize>], &[ComptimeValue]),
    context: &mut EvalContext<'_>,
    depth: usize,
    call_depth: usize,
) -> Result<ComptimeValue, String> {
    let (by_parameter, supplied) = arguments;
    if call_depth > MAX_FUNCTION_CALL_DEPTH {
        return Err("comptime function call depth limit exceeded".into());
    }
    let mut values = Values::new();
    for (index, parameter) in function.params.iter().enumerate() {
        let mut value = if let Some(argument) = by_parameter[index] {
            context.budget.clone_value(&supplied[argument])?
        } else if let Some(default) = &parameter.default {
            eval_expr(default, &values, context, depth + 1, call_depth)?
        } else {
            return Err(format!(
                "`{}` expects argument `{}`",
                function.name, parameter.name
            ));
        };
        if matches!(value, ComptimeValue::Propagate(_)) {
            return Ok(value);
        }
        if let Some(ty) = &parameter.ty {
            value = contextualize_comptime_value(value, ty);
            if !comptime_value_matches_annotation(&value, ty) {
                return Err(format!(
                    "type mismatch: parameter `{}` of `{}` expects {}, got {}",
                    parameter.name,
                    function.name,
                    ty.display(),
                    comptime_value_type_name(&value)
                ));
            }
        }
        context
            .budget
            .insert_value(&mut values, &parameter.name, Some(value))?;
    }
    match eval_comptime_block(&function.body, &mut values, context, depth + 1, call_depth)? {
        BlockExit::Return(value) | BlockExit::Tail(value) => Ok(value),
        BlockExit::Propagate(error) => Ok(ComptimeValue::Propagate(error)),
        BlockExit::Continue => Ok(ComptimeValue::Nil),
    }
}

fn contextualize_comptime_value(value: ComptimeValue, ty: &TypeAnnotation) -> ComptimeValue {
    match (value, &ty.kind) {
        (ComptimeValue::Int(value), TypeKind::Primitive(crate::ast::VarType::UInt))
            if value >= 0 =>
        {
            ComptimeValue::UInt(value as u64)
        }
        (ComptimeValue::Int(value), TypeKind::Primitive(crate::ast::VarType::BigInt)) => {
            ComptimeValue::BigInt(BigInt::from(value))
        }
        (value, _) => value,
    }
}

fn comptime_value_matches_annotation(value: &ComptimeValue, ty: &TypeAnnotation) -> bool {
    use crate::ast::VarType;
    // Computational Error is orthogonal to a callable's normal value type.
    if matches!(value, ComptimeValue::Error(_)) {
        return true;
    }
    match (&ty.kind, value) {
        (TypeKind::Named(name), ComptimeValue::Type(_)) if name == "type" => true,
        (TypeKind::Primitive(VarType::Int), ComptimeValue::Int(_))
        | (TypeKind::Primitive(VarType::UInt), ComptimeValue::UInt(_))
        | (TypeKind::Primitive(VarType::BigInt), ComptimeValue::BigInt(_))
        | (TypeKind::Primitive(VarType::Float), ComptimeValue::Float(_))
        | (TypeKind::Primitive(VarType::Char), ComptimeValue::Char(_))
        | (TypeKind::Primitive(VarType::Str), ComptimeValue::Str(_))
        | (TypeKind::Primitive(VarType::Bool), ComptimeValue::Bool(_))
        | (TypeKind::Primitive(VarType::Nil), ComptimeValue::Nil) => true,
        (TypeKind::Bytes, ComptimeValue::Bytes(_)) => true,
        (TypeKind::Primitive(_) | TypeKind::Bytes, ComptimeValue::Nil) => ty.nilable,
        (TypeKind::Named(expected), ComptimeValue::Struct { name, .. }) => expected == name,
        (TypeKind::ResolvedName { symbol, .. }, ComptimeValue::Struct { name, .. }) => {
            symbol == name
        }
        (TypeKind::Named(_) | TypeKind::ResolvedName { .. }, ComptimeValue::Nil) => ty.nilable,
        _ => false,
    }
}

fn comptime_value_type_name(value: &ComptimeValue) -> String {
    match value {
        ComptimeValue::Int(_) => "int".into(),
        ComptimeValue::UInt(_) => "uint".into(),
        ComptimeValue::BigInt(_) => "bigint".into(),
        ComptimeValue::Float(_) => "float".into(),
        ComptimeValue::Char(_) => "char".into(),
        ComptimeValue::Str(_) => "str".into(),
        ComptimeValue::Bytes(_) => "bytes".into(),
        ComptimeValue::Bool(_) => "bool".into(),
        ComptimeValue::Nil => "nil".into(),
        ComptimeValue::Error(_) => "Error".into(),
        ComptimeValue::Propagate(_) => "propagated Error".into(),
        ComptimeValue::Struct { name, .. } => name.clone(),
        ComptimeValue::Type(_) => "type".into(),
    }
}

fn is_type_annotation(ty: &TypeAnnotation) -> bool {
    matches!(&ty.kind, TypeKind::Named(name) if name == "type") && !ty.nilable
}

fn primitive_type(name: &str) -> Option<TypeAnnotation> {
    use crate::ast::VarType;
    let primitive = match name {
        "int" => VarType::Int,
        "uint" => VarType::UInt,
        "bigint" => VarType::BigInt,
        "float" => VarType::Float,
        "bool" => VarType::Bool,
        "char" => VarType::Char,
        "str" => VarType::Str,
        "nil" => VarType::Nil,
        _ => return None,
    };
    Some(TypeAnnotation {
        kind: TypeKind::Primitive(primitive),
        nilable: false,
    })
}

fn substitute_type(ty: &mut TypeAnnotation, aliases: &HashMap<String, TypeAnnotation>) {
    match &mut ty.kind {
        TypeKind::Named(name) => {
            if let Some(replacement) = aliases.get(name) {
                let nilable = ty.nilable || replacement.nilable;
                *ty = replacement.clone();
                ty.nilable = nilable;
            }
        }
        TypeKind::ResolvedName { symbol, .. } => {
            if let Some(replacement) = aliases.get(symbol) {
                let nilable = ty.nilable || replacement.nilable;
                *ty = replacement.clone();
                ty.nilable = nilable;
            }
        }
        TypeKind::Array(element) => substitute_type(element, aliases),
        TypeKind::Applied { arguments, .. } | TypeKind::ResolvedApplied { arguments, .. } => {
            for argument in arguments {
                substitute_type(argument, aliases);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                substitute_type(element, aliases);
            }
        }
        TypeKind::Function { parameters, result } => {
            for parameter in parameters {
                substitute_type(parameter, aliases);
            }
            substitute_type(result, aliases);
        }
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => {}
    }
}

fn substitute_program_types(program: &mut Program, aliases: &HashMap<String, TypeAnnotation>) {
    fn block(block: &mut Block, aliases: &HashMap<String, TypeAnnotation>) {
        for statement in &mut block.stmts {
            stmt(statement, aliases);
        }
    }
    fn expr(expression: &mut Expr, aliases: &HashMap<String, TypeAnnotation>) {
        match expression {
            Expr::Located(_, inner)
            | Expr::Propagate(inner)
            | Expr::Unary(_, inner)
            | Expr::Cast(inner, _) => expr(inner, aliases),
            Expr::TypeLiteral(ty) => substitute_type(ty, aliases),
            Expr::Closure {
                params,
                result_type,
                body,
                ..
            } => {
                for parameter in params {
                    if let Some(ty) = &mut parameter.ty {
                        substitute_type(ty, aliases);
                    }
                }
                if let Some(ty) = result_type {
                    substitute_type(ty, aliases);
                }
                block(body, aliases);
            }
            Expr::Array(items) | Expr::Tuple(items) => {
                for item in items {
                    expr(item, aliases);
                }
            }
            Expr::Record(fields) => {
                for (_, value) in fields {
                    expr(value, aliases);
                }
            }
            Expr::Call(_, arguments) => {
                for argument in arguments {
                    expr(&mut argument.value, aliases);
                }
            }
            Expr::SpecializedCall(_, types, arguments) => {
                for ty in types {
                    substitute_type(ty, aliases);
                }
                for argument in arguments {
                    expr(&mut argument.value, aliases);
                }
            }
            Expr::AssociatedCall {
                target,
                type_arguments,
                args,
                ..
            } => {
                substitute_type(target, aliases);
                for ty in type_arguments {
                    substitute_type(ty, aliases);
                }
                for argument in args {
                    expr(&mut argument.value, aliases);
                }
            }
            Expr::BracketApply(receiver, arguments) => {
                expr(receiver, aliases);
                for argument in arguments {
                    expr(argument, aliases);
                }
            }
            Expr::Apply(receiver, arguments) => {
                expr(receiver, aliases);
                for argument in arguments {
                    expr(&mut argument.value, aliases);
                }
            }
            Expr::MethodCall(receiver, _, arguments) => {
                expr(receiver, aliases);
                for argument in arguments {
                    expr(argument, aliases);
                }
            }
            Expr::Index(receiver, index) | Expr::Binary(_, receiver, index) => {
                expr(receiver, aliases);
                expr(index, aliases);
            }
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let crate::ast::StringPart::Expression(value) = part {
                        expr(value, aliases);
                    }
                }
            }
            Expr::Fallback {
                value, fallback, ..
            } => {
                expr(value, aliases);
                expr(fallback, aliases);
            }
            Expr::Block(body) | Expr::Capture(body) => block(body, aliases),
            Expr::Match { value, arms, .. } => {
                expr(value, aliases);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        expr(guard, aliases);
                    }
                    block(&mut arm.body, aliases);
                }
            }
            Expr::Try {
                body, catch_block, ..
            } => {
                block(body, aliases);
                block(catch_block, aliases);
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                expr(cond, aliases);
                expr(then_branch, aliases);
                if let Some(else_branch) = else_branch {
                    expr(else_branch, aliases);
                }
            }
            Expr::Int(_)
            | Expr::UInt(_)
            | Expr::BigInt(_)
            | Expr::Float(_)
            | Expr::Char(_)
            | Expr::Str(_)
            | Expr::Bytes(_)
            | Expr::Bool(_)
            | Expr::Nil
            | Expr::Ident(_) => {}
        }
    }
    fn stmt(statement: &mut Stmt, aliases: &HashMap<String, TypeAnnotation>) {
        match statement {
            Stmt::Located(_, inner) => stmt(inner, aliases),
            Stmt::VarDecl { ty, .. } | Stmt::Const { ty, .. } | Stmt::Comptime { ty, .. } => {
                if let Some(ty) = ty {
                    substitute_type(ty, aliases);
                }
            }
            Stmt::Defer { body }
            | Stmt::While { body, .. }
            | Stmt::For { body, .. }
            | Stmt::ForEach { body, .. } => block(body, aliases),
            Stmt::ForZip { clauses, body } => {
                for clause in clauses {
                    match clause {
                        ForClause::Range {
                            start, end, step, ..
                        } => {
                            expr(start, aliases);
                            expr(end, aliases);
                            if let Some(step) = step {
                                expr(step, aliases);
                            }
                        }
                        ForClause::Each { iterable, .. } => expr(iterable, aliases),
                    }
                }
                block(body, aliases);
            }
            Stmt::Try {
                body, catch_block, ..
            } => {
                block(body, aliases);
                block(catch_block, aliases);
            }
            Stmt::If {
                then_block,
                else_block,
                ..
            }
            | Stmt::When {
                then_block,
                else_block,
                ..
            } => {
                block(then_block, aliases);
                if let Some(other_block) = else_block {
                    block(other_block, aliases);
                }
            }
            _ => {}
        }
    }
    for item in &mut program.items {
        match item {
            Item::Function(function) => {
                for parameter in &mut function.type_params {
                    for bound in &mut parameter.bounds {
                        for argument in &mut bound.arguments {
                            substitute_type(argument, aliases);
                        }
                    }
                }
                for parameter in &mut function.params {
                    if let Some(ty) = &mut parameter.ty {
                        substitute_type(ty, aliases);
                    }
                }
                if let Some(ty) = &mut function.result_type {
                    substitute_type(ty, aliases);
                }
                block(&mut function.body, aliases);
            }
            Item::Extend(extension) => {
                substitute_type(&mut extension.target, aliases);
                for argument in &mut extension.trait_arguments {
                    substitute_type(argument, aliases);
                }
                for method in &mut extension.methods {
                    let function = &mut method.function;
                    for parameter in &mut function.params {
                        if let Some(ty) = &mut parameter.ty {
                            substitute_type(ty, aliases);
                        }
                    }
                    if let Some(ty) = &mut function.result_type {
                        substitute_type(ty, aliases);
                    }
                    block(&mut function.body, aliases);
                }
            }
            Item::GlobalConst { ty, .. }
            | Item::GlobalVar { ty, .. }
            | Item::Comptime { ty, .. } => {
                if let Some(ty) = ty {
                    substitute_type(ty, aliases);
                }
            }
            Item::StructDecl { fields, .. } => {
                for field in fields {
                    if let Some(ty) = &mut field.ty {
                        substitute_type(ty, aliases);
                    }
                }
            }
            Item::EnumDecl { variants, .. } => {
                for variant in variants {
                    for field in &mut variant.payload {
                        if let Some(ty) = &mut field.ty {
                            substitute_type(ty, aliases);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn eval_comptime_block(
    block: &Block,
    outer: &mut Values,
    context: &mut EvalContext<'_>,
    depth: usize,
    call_depth: usize,
) -> Result<BlockExit, String> {
    let mut values = context.budget.clone_values(outer)?;
    for statement in &block.stmts {
        context.budget.step(depth)?;
        match statement.node() {
            Stmt::Const { pattern, value, .. } | Stmt::VarDecl { pattern, value, .. } => {
                if matches!(pattern, BindingPattern::Tuple(_)) {
                    return Err(
                        "tuple destructuring is not supported inside a comptime function yet"
                            .into(),
                    );
                }
                let value = eval_expr(value, &values, context, depth + 1, call_depth)?;
                if let ComptimeValue::Propagate(error) = value {
                    return Ok(BlockExit::Propagate(error));
                }
                let BindingPattern::Name(name) = pattern else {
                    unreachable!("tuple pattern was handled above")
                };
                context
                    .budget
                    .insert_value(&mut values, name, Some(value))?;
            }
            Stmt::Comptime { name, value, .. } => {
                let value = eval_expr(value, &values, context, depth + 1, call_depth)?;
                if let ComptimeValue::Propagate(error) = value {
                    return Ok(BlockExit::Propagate(error));
                }
                context
                    .budget
                    .insert_value(&mut values, name, Some(value))?;
            }
            Stmt::Return(value) => {
                let value = value.as_ref().map_or(Ok(ComptimeValue::Nil), |value| {
                    eval_expr(value, &values, context, depth + 1, call_depth)
                })?;
                if let ComptimeValue::Propagate(error) = value {
                    return Ok(BlockExit::Propagate(error));
                }
                return Ok(BlockExit::Return(value));
            }
            Stmt::Expr(value) => {
                if let Some(value) =
                    eval_comptime_statement_expr(value, &values, context, depth + 1, call_depth)?
                {
                    if let ComptimeValue::Propagate(error) = value {
                        return Ok(BlockExit::Propagate(error));
                    }
                    return Ok(BlockExit::Return(value));
                }
            }
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                match eval_comptime_try(
                    body,
                    catch_name,
                    catch_block,
                    &mut values,
                    context,
                    depth + 1,
                    call_depth,
                )? {
                    BlockExit::Return(value) => return Ok(BlockExit::Return(value)),
                    BlockExit::Propagate(error) => {
                        return Ok(BlockExit::Propagate(error));
                    }
                    BlockExit::Tail(ComptimeValue::Error(error)) => {
                        return Ok(BlockExit::Return(ComptimeValue::Error(error)));
                    }
                    BlockExit::Tail(_) | BlockExit::Continue => {}
                }
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                let condition = eval_expr(cond, &values, context, depth + 1, call_depth)?;
                let selected = match condition {
                    ComptimeValue::Bool(true) => Some(then_block),
                    ComptimeValue::Bool(false) => else_block.as_ref(),
                    ComptimeValue::Error(error) => {
                        return Ok(BlockExit::Return(ComptimeValue::Error(error)));
                    }
                    ComptimeValue::Propagate(error) => {
                        return Ok(BlockExit::Propagate(error));
                    }
                    _ => return Err("comptime if condition must be bool".into()),
                };
                if let Some(selected) = selected {
                    match eval_comptime_block(
                        selected,
                        &mut values,
                        context,
                        depth + 1,
                        call_depth,
                    )? {
                        BlockExit::Return(value) => return Ok(BlockExit::Return(value)),
                        BlockExit::Propagate(error) => {
                            return Ok(BlockExit::Propagate(error));
                        }
                        BlockExit::Tail(_) | BlockExit::Continue => {}
                    }
                }
            }
            Stmt::Use { .. } => {}
            _ => {
                return Err("this statement is not supported in a comptime function yet".into());
            }
        }
    }
    if let Some(tail) = &block.tail {
        let value = eval_expr(tail, &values, context, depth + 1, call_depth)?;
        return match value {
            ComptimeValue::Propagate(error) => Ok(BlockExit::Propagate(error)),
            value => Ok(BlockExit::Tail(value)),
        };
    }
    Ok(BlockExit::Continue)
}

fn eval_comptime_try(
    body: &Block,
    catch_name: &str,
    catch_block: &Block,
    values: &mut Values,
    context: &mut EvalContext<'_>,
    depth: usize,
    call_depth: usize,
) -> Result<BlockExit, String> {
    let mut body_values = context.budget.clone_values(values)?;
    let body_exit = eval_comptime_block(body, &mut body_values, context, depth + 1, call_depth)?;
    let error = match &body_exit {
        BlockExit::Return(ComptimeValue::Error(error))
        | BlockExit::Tail(ComptimeValue::Error(error)) => Some(context.budget.clone_error(error)?),
        BlockExit::Propagate(propagation) if propagation.origin_call_depth > call_depth => {
            Some(context.budget.clone_error(&propagation.error)?)
        }
        _ => None,
    };
    let Some(error) = error else {
        return Ok(body_exit);
    };
    let mut catch_values = context.budget.clone_values(values)?;
    context.budget.insert_value(
        &mut catch_values,
        catch_name,
        Some(ComptimeValue::Error(error)),
    )?;
    eval_comptime_block(
        catch_block,
        &mut catch_values,
        context,
        depth + 1,
        call_depth,
    )
}

fn eval_comptime_statement_expr(
    expr: &Expr,
    values: &Values,
    context: &mut EvalContext<'_>,
    depth: usize,
    call_depth: usize,
) -> Result<Option<ComptimeValue>, String> {
    match expr.node() {
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let condition = eval_expr(cond, values, context, depth + 1, call_depth)?;
            let selected = match condition {
                ComptimeValue::Bool(true) => Some(then_branch.as_ref()),
                ComptimeValue::Bool(false) => else_branch.as_deref(),
                _ => return Err("comptime if condition must be bool".into()),
            };
            selected.map_or(Ok(None), |selected| {
                eval_comptime_statement_expr(selected, values, context, depth + 1, call_depth)
            })
        }
        Expr::Block(block) | Expr::Capture(block) => {
            let mut values = context.budget.clone_values(values)?;
            match eval_comptime_block(block, &mut values, context, depth + 1, call_depth)? {
                BlockExit::Return(value) => Ok(Some(value)),
                BlockExit::Propagate(error) => Ok(Some(ComptimeValue::Propagate(error))),
                BlockExit::Tail(_) | BlockExit::Continue => Ok(None),
            }
        }
        Expr::Match { value, arms, .. } => {
            let value = eval_expr(value, values, context, depth + 1, call_depth)?;
            for arm in arms {
                if !matches_pattern(&arm.pattern, &value) {
                    continue;
                }
                if let Some(guard) = &arm.guard {
                    match eval_expr(guard, values, context, depth + 1, call_depth)? {
                        ComptimeValue::Bool(true) => {}
                        ComptimeValue::Bool(false) => continue,
                        ComptimeValue::Error(error) => {
                            return Ok(Some(ComptimeValue::Error(error)));
                        }
                        _ => return Err("comptime match guard must be bool".into()),
                    }
                }
                let mut arm_values = context.budget.clone_values(values)?;
                return match eval_comptime_block(
                    &arm.body,
                    &mut arm_values,
                    context,
                    depth + 1,
                    call_depth,
                )? {
                    BlockExit::Return(value) => Ok(Some(value)),
                    BlockExit::Propagate(error) => Ok(Some(ComptimeValue::Propagate(error))),
                    BlockExit::Tail(_) | BlockExit::Continue => Ok(None),
                };
            }
            Ok(None)
        }
        Expr::Try {
            body,
            catch_name,
            catch_block,
        } => {
            let mut values = context.budget.clone_values(values)?;
            match eval_comptime_try(
                body,
                catch_name,
                catch_block,
                &mut values,
                context,
                depth + 1,
                call_depth,
            )? {
                BlockExit::Return(value) => Ok(Some(value)),
                BlockExit::Propagate(error) => Ok(Some(ComptimeValue::Propagate(error))),
                BlockExit::Tail(ComptimeValue::Error(error)) => {
                    Ok(Some(ComptimeValue::Error(error)))
                }
                BlockExit::Tail(_) | BlockExit::Continue => Ok(None),
            }
        }
        _ => {
            let value = eval_expr(expr, values, context, depth + 1, call_depth)?;
            if matches!(value, ComptimeValue::Error(_) | ComptimeValue::Propagate(_)) {
                Ok(Some(value))
            } else {
                Ok(None)
            }
        }
    }
}

fn eval_binary(
    op: BinOp,
    left: ComptimeValue,
    right: ComptimeValue,
    budget: &mut Budget,
) -> Result<ComptimeValue, String> {
    use ComptimeValue::{BigInt as Big, Bool, Float, Int, Nil, Str, UInt};
    if let ComptimeValue::Error(error) = left {
        return Ok(ComptimeValue::Error(error));
    }
    if let ComptimeValue::Error(error) = right {
        return Ok(ComptimeValue::Error(error));
    }
    match (op, left, right) {
        (BinOp::Add, Int(a), Int(b)) => checked_int(a.checked_add(b)),
        (BinOp::Sub, Int(a), Int(b)) => checked_int(a.checked_sub(b)),
        (BinOp::Mul, Int(a), Int(b)) => checked_int(a.checked_mul(b)),
        (BinOp::Div, Int(_), Int(0)) | (BinOp::Mod, Int(_), Int(0)) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Div, Int(a), Int(b)) => checked_int(a.checked_div(b)),
        (BinOp::Mod, Int(a), Int(b)) => checked_int(a.checked_rem(b)),
        (BinOp::Add, UInt(a), UInt(b)) => checked_uint(a.checked_add(b)),
        (BinOp::Sub, UInt(a), UInt(b)) => checked_uint(a.checked_sub(b)),
        (BinOp::Mul, UInt(a), UInt(b)) => checked_uint(a.checked_mul(b)),
        (BinOp::Div, UInt(_), UInt(0)) | (BinOp::Mod, UInt(_), UInt(0)) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Div, UInt(a), UInt(b)) => checked_uint(a.checked_div(b)),
        (BinOp::Mod, UInt(a), UInt(b)) => checked_uint(a.checked_rem(b)),
        (BinOp::Add, Big(a), Big(b)) => {
            preflight_bigint_binary(&a, &b, op, budget)?;
            Ok(Big(a + b))
        }
        (BinOp::Sub, Big(a), Big(b)) => {
            preflight_bigint_binary(&a, &b, op, budget)?;
            Ok(Big(a - b))
        }
        (BinOp::Mul, Big(a), Big(b)) => {
            preflight_bigint_binary(&a, &b, op, budget)?;
            Ok(Big(a * b))
        }
        (BinOp::Div, Big(_), Big(b)) if b == BigInt::from(0) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Div, Big(a), Big(b)) => {
            preflight_bigint_binary(&a, &b, op, budget)?;
            Ok(Big(a / b))
        }
        (BinOp::Mod, Big(_), Big(b)) if b == BigInt::from(0) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Mod, Big(a), Big(b)) => {
            preflight_bigint_binary(&a, &b, op, budget)?;
            Ok(Big(a % b))
        }
        (BinOp::Add, Float(a), Float(b)) => Ok(Float(a + b)),
        (BinOp::Sub, Float(a), Float(b)) => Ok(Float(a - b)),
        (BinOp::Mul, Float(a), Float(b)) => Ok(Float(a * b)),
        (BinOp::Div, Float(_), Float(0.0)) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Div, Float(a), Float(b)) => Ok(Float(a / b)),
        (BinOp::Mod, Float(_), Float(0.0)) => {
            Ok(ComptimeValue::Error(RuntimeError::DivisionByZero))
        }
        (BinOp::Mod, Float(a), Float(b)) => Ok(Float(a % b)),
        (BinOp::Add, Int(a), Float(b)) => Ok(Float(a as f64 + b)),
        (BinOp::Add, Float(a), Int(b)) => Ok(Float(a + b as f64)),
        (BinOp::Add, UInt(a), Float(b)) => Ok(Float(a as f64 + b)),
        (BinOp::Add, Float(a), UInt(b)) => Ok(Float(a + b as f64)),
        (BinOp::Sub, Int(a), Float(b)) => Ok(Float(a as f64 - b)),
        (BinOp::Sub, Float(a), Int(b)) => Ok(Float(a - b as f64)),
        (BinOp::Sub, UInt(a), Float(b)) => Ok(Float(a as f64 - b)),
        (BinOp::Sub, Float(a), UInt(b)) => Ok(Float(a - b as f64)),
        (BinOp::Mul, Int(a), Float(b)) => Ok(Float(a as f64 * b)),
        (BinOp::Mul, Float(a), Int(b)) => Ok(Float(a * b as f64)),
        (BinOp::Mul, UInt(a), Float(b)) => Ok(Float(a as f64 * b)),
        (BinOp::Mul, Float(a), UInt(b)) => Ok(Float(a * b as f64)),
        (BinOp::Div, Int(a), Float(b)) if b != 0.0 => Ok(Float(a as f64 / b)),
        (BinOp::Div, Float(a), Int(b)) if b != 0 => Ok(Float(a / b as f64)),
        (BinOp::Div, UInt(a), Float(b)) if b != 0.0 => Ok(Float(a as f64 / b)),
        (BinOp::Div, Float(a), UInt(b)) if b != 0 => Ok(Float(a / b as f64)),
        (BinOp::Div, Int(_), Float(_))
        | (BinOp::Div, Float(_), Int(_))
        | (BinOp::Div, UInt(_), Float(_))
        | (BinOp::Div, Float(_), UInt(_)) => Ok(ComptimeValue::Error(RuntimeError::DivisionByZero)),
        (BinOp::Mod, Int(a), Float(b)) if b != 0.0 => Ok(Float(a as f64 % b)),
        (BinOp::Mod, Float(a), Int(b)) if b != 0 => Ok(Float(a % b as f64)),
        (BinOp::Mod, UInt(a), Float(b)) if b != 0.0 => Ok(Float(a as f64 % b)),
        (BinOp::Mod, Float(a), UInt(b)) if b != 0 => Ok(Float(a % b as f64)),
        (BinOp::Mod, Int(_), Float(_))
        | (BinOp::Mod, Float(_), Int(_))
        | (BinOp::Mod, UInt(_), Float(_))
        | (BinOp::Mod, Float(_), UInt(_)) => Ok(ComptimeValue::Error(RuntimeError::DivisionByZero)),
        (BinOp::Add, Str(a), Str(b)) => {
            let size = a
                .len()
                .checked_add(b.len())
                .ok_or("comptime string size overflow")?;
            if size > MAX_STRING_BYTES {
                return Err("comptime string size limit exceeded".into());
            }
            budget.allocate(size)?;
            let mut result = String::with_capacity(size);
            result.push_str(&a);
            result.push_str(&b);
            Ok(Str(result))
        }
        (BinOp::And, Bool(a), Bool(b)) => Ok(Bool(a && b)),
        (BinOp::Or, Bool(a), Bool(b)) => Ok(Bool(a || b)),
        (BinOp::Xor, Bool(a), Bool(b)) => Ok(Bool(a != b)),
        (BinOp::Eq, a, b) => Ok(Bool(equal(&a, &b))),
        (BinOp::NotEq, a, b) => Ok(Bool(!equal(&a, &b))),
        (BinOp::Lt, a, b) => compare(a, b, |order| order == Some(Ordering::Less)),
        (BinOp::LtEq, a, b) => compare(a, b, |order| {
            matches!(order, Some(Ordering::Less | Ordering::Equal))
        }),
        (BinOp::Gt, a, b) => compare(a, b, |order| order == Some(Ordering::Greater)),
        (BinOp::GtEq, a, b) => compare(a, b, |order| {
            matches!(order, Some(Ordering::Greater | Ordering::Equal))
        }),
        (BinOp::Shl | BinOp::Shr, value, amount) => {
            let left = op == BinOp::Shl;
            let amount = match amount {
                Int(amount) => i128::from(amount),
                UInt(amount) => i128::from(amount),
                _ => return Err("operator has incompatible comptime operand types".into()),
            };
            let shifted = match value {
                Int(value) => crate::value::shift_int(value, amount, left).map(Int),
                UInt(value) => crate::value::shift_uint(value, amount, left).map(UInt),
                Big(value) => {
                    let bits = if left && amount >= 0 {
                        crate::value::bigint_left_shift_bits(&value, amount)
                    } else {
                        value.bits()
                    };
                    budget.allocate(bigint_storage_cost_from_bits(bits))?;
                    crate::value::shift_bigint(&value, amount, left, None).map(Big)
                }
                _ => return Err("operator has incompatible comptime operand types".into()),
            };
            Ok(shifted.unwrap_or_else(ComptimeValue::Error))
        }
        (BinOp::Xor, _, _) => Err("operator is not supported for these comptime values".into()),
        (_, Nil, _) | (_, _, Nil) => Err("operator is not supported for nil at comptime".into()),
        _ => Err("operator has incompatible comptime operand types".into()),
    }
}

fn checked_int(value: Option<i64>) -> Result<ComptimeValue, String> {
    Ok(value.map_or(
        ComptimeValue::Error(RuntimeError::Overflow),
        ComptimeValue::Int,
    ))
}

fn checked_uint(value: Option<u64>) -> Result<ComptimeValue, String> {
    Ok(value.map_or(
        ComptimeValue::Error(RuntimeError::Overflow),
        ComptimeValue::UInt,
    ))
}

fn bigint_storage_cost(value: &BigInt) -> usize {
    bigint_storage_cost_from_bits(value.bits())
}

fn bigint_storage_cost_from_bits(bits: u64) -> usize {
    usize::try_from(bits.saturating_add(31) / 32)
        .unwrap_or(usize::MAX)
        .saturating_mul(std::mem::size_of::<u32>())
        .saturating_add(std::mem::size_of::<BigInt>())
}

fn preflight_bigint_binary(
    left: &BigInt,
    right: &BigInt,
    op: BinOp,
    budget: &mut Budget,
) -> Result<(), String> {
    let left_bits = left.bits();
    let right_bits = right.bits();
    let result_bits = match op {
        BinOp::Add | BinOp::Sub => left_bits.max(right_bits).saturating_add(1),
        BinOp::Mul => left_bits.saturating_add(right_bits),
        BinOp::Div => left_bits,
        BinOp::Mod => left_bits.min(right_bits),
        _ => 0,
    };
    budget.allocate(bigint_storage_cost_from_bits(result_bits))
}

fn matches_pattern(pattern: &MatchPattern, value: &ComptimeValue) -> bool {
    match pattern {
        MatchPattern::Wildcard => true,
        MatchPattern::StringLiteral(expected) => {
            matches!(value, ComptimeValue::Str(actual) if actual == expected)
        }
        MatchPattern::CharacterLiteral(expected) => {
            matches!(value, ComptimeValue::Char(actual) if actual == expected)
        }
        MatchPattern::Type(name) => match value {
            ComptimeValue::Int(_) => name == "int",
            ComptimeValue::UInt(_) => name == "uint",
            ComptimeValue::BigInt(_) => name == "bigint",
            ComptimeValue::Float(_) => name == "float",
            ComptimeValue::Bool(_) => name == "bool",
            ComptimeValue::Char(_) => name == "char",
            ComptimeValue::Str(_) => name == "str",
            ComptimeValue::Nil => name == "nil",
            ComptimeValue::Struct { name: actual, .. } => name == actual,
            ComptimeValue::Type(_) => name == "type",
            ComptimeValue::Bytes(_) => name == "bytes",
            ComptimeValue::Error(error) => {
                let name = name.rsplit("::").next().unwrap_or(name);
                name == "Error" || name == error.kind()
            }
            ComptimeValue::Propagate(_) => false,
        },
        MatchPattern::Variant { name, bindings } => {
            bindings.is_empty()
                && matches!(value, ComptimeValue::Error(error) if name.rsplit("::").next() == Some(error.kind()))
        }
    }
}

fn equal(left: &ComptimeValue, right: &ComptimeValue) -> bool {
    match (left, right) {
        (ComptimeValue::Int(a), ComptimeValue::Int(b)) => a == b,
        (ComptimeValue::UInt(a), ComptimeValue::UInt(b)) => a == b,
        (ComptimeValue::BigInt(a), ComptimeValue::BigInt(b)) => a == b,
        (ComptimeValue::BigInt(a), ComptimeValue::Int(b)) => *a == BigInt::from(*b),
        (ComptimeValue::Int(a), ComptimeValue::BigInt(b)) => BigInt::from(*a) == *b,
        (ComptimeValue::BigInt(a), ComptimeValue::UInt(b)) => *a == BigInt::from(*b),
        (ComptimeValue::UInt(a), ComptimeValue::BigInt(b)) => BigInt::from(*a) == *b,
        (ComptimeValue::Int(a), ComptimeValue::UInt(b)) => {
            crate::value::compare_int_uint(*a, *b) == Ordering::Equal
        }
        (ComptimeValue::UInt(a), ComptimeValue::Int(b)) => {
            crate::value::compare_int_uint(*b, *a) == Ordering::Equal
        }
        (ComptimeValue::Float(a), ComptimeValue::Float(b)) => a == b,
        (ComptimeValue::Int(a), ComptimeValue::Float(b)) => {
            crate::value::compare_int_float(*a, *b) == Some(Ordering::Equal)
        }
        (ComptimeValue::UInt(a), ComptimeValue::Float(b)) => {
            crate::value::compare_uint_float(*a, *b) == Some(Ordering::Equal)
        }
        (ComptimeValue::Float(a), ComptimeValue::UInt(b)) => {
            crate::value::compare_uint_float(*b, *a) == Some(Ordering::Equal)
        }
        (ComptimeValue::Float(a), ComptimeValue::Int(b)) => {
            crate::value::compare_int_float(*b, *a) == Some(Ordering::Equal)
        }
        (ComptimeValue::BigInt(a), ComptimeValue::Float(b)) => {
            crate::value::compare_bigint_float(a, *b) == Some(Ordering::Equal)
        }
        (ComptimeValue::Float(a), ComptimeValue::BigInt(b)) => {
            crate::value::compare_bigint_float(b, *a) == Some(Ordering::Equal)
        }
        (ComptimeValue::Str(a), ComptimeValue::Str(b)) => a == b,
        (ComptimeValue::Char(a), ComptimeValue::Char(b)) => a == b,
        (ComptimeValue::Bool(a), ComptimeValue::Bool(b)) => a == b,
        (ComptimeValue::Nil, ComptimeValue::Nil) => true,
        (ComptimeValue::Type(a), ComptimeValue::Type(b)) => a == b,
        _ => false,
    }
}

fn compare(
    left: ComptimeValue,
    right: ComptimeValue,
    compare: impl FnOnce(Option<Ordering>) -> bool,
) -> Result<ComptimeValue, String> {
    let order = match (left, right) {
        (ComptimeValue::Int(left), ComptimeValue::Int(right)) => Some(left.cmp(&right)),
        (ComptimeValue::UInt(left), ComptimeValue::UInt(right)) => Some(left.cmp(&right)),
        (ComptimeValue::BigInt(left), ComptimeValue::BigInt(right)) => Some(left.cmp(&right)),
        (ComptimeValue::BigInt(left), ComptimeValue::Int(right)) => {
            Some(left.cmp(&BigInt::from(right)))
        }
        (ComptimeValue::Int(left), ComptimeValue::BigInt(right)) => {
            Some(BigInt::from(left).cmp(&right))
        }
        (ComptimeValue::BigInt(left), ComptimeValue::UInt(right)) => {
            Some(left.cmp(&BigInt::from(right)))
        }
        (ComptimeValue::UInt(left), ComptimeValue::BigInt(right)) => {
            Some(BigInt::from(left).cmp(&right))
        }
        (ComptimeValue::Int(left), ComptimeValue::UInt(right)) => {
            Some(crate::value::compare_int_uint(left, right))
        }
        (ComptimeValue::UInt(left), ComptimeValue::Int(right)) => {
            Some(crate::value::compare_int_uint(right, left).reverse())
        }
        (ComptimeValue::Float(left), ComptimeValue::Float(right)) => left.partial_cmp(&right),
        (ComptimeValue::Char(left), ComptimeValue::Char(right)) => Some(left.cmp(&right)),
        (ComptimeValue::Int(left), ComptimeValue::Float(right)) => {
            crate::value::compare_int_float(left, right)
        }
        (ComptimeValue::Float(left), ComptimeValue::Int(right)) => {
            crate::value::compare_int_float(right, left).map(Ordering::reverse)
        }
        (ComptimeValue::UInt(left), ComptimeValue::Float(right)) => {
            crate::value::compare_uint_float(left, right)
        }
        (ComptimeValue::Float(left), ComptimeValue::UInt(right)) => {
            crate::value::compare_uint_float(right, left).map(Ordering::reverse)
        }
        (ComptimeValue::BigInt(left), ComptimeValue::Float(right)) => {
            crate::value::compare_bigint_float(&left, right)
        }
        (ComptimeValue::Float(left), ComptimeValue::BigInt(right)) => {
            crate::value::compare_bigint_float(&right, left).map(Ordering::reverse)
        }
        _ => return Err("comparison requires numeric comptime values".into()),
    };
    Ok(ComptimeValue::Bool(compare(order)))
}

fn comptime_error(message: String, span: Option<SourceSpan>) -> Diagnostic {
    Diagnostic::new(
        "E4001",
        format!("comptime evaluation failed: {message}"),
        span,
    )
}

fn replace_stmt(statement: &mut Stmt, replacement: Stmt) {
    if let Stmt::Located(_, inner) = statement {
        **inner = replacement;
    } else {
        *statement = replacement;
    }
}

fn replace_binding_value(statement: &mut Stmt, pattern: BindingPattern, value: Expr) {
    match statement.node().clone() {
        Stmt::Const { ty, .. } => replace_stmt(statement, Stmt::Const { pattern, ty, value }),
        Stmt::VarDecl { ty, .. } => replace_stmt(statement, Stmt::VarDecl { pattern, ty, value }),
        _ => unreachable!("binding node is unchanged"),
    }
}

fn replace_assignment(statement: &mut Stmt, target: Expr, value: Expr) {
    match statement.node().clone() {
        Stmt::Assign { .. } => replace_stmt(statement, Stmt::Assign { target, value }),
        Stmt::CompoundAssign { op, .. } => {
            replace_stmt(statement, Stmt::CompoundAssign { target, op, value })
        }
        _ => unreachable!("assignment node is unchanged"),
    }
}

fn replace_value_statement(statement: &mut Stmt, value: Expr) {
    match statement.node().clone() {
        Stmt::Return(Some(_)) => replace_stmt(statement, Stmt::Return(Some(value))),
        Stmt::Expr(_) => replace_stmt(statement, Stmt::Expr(value)),
        Stmt::Crash { context, .. } => replace_stmt(statement, Stmt::Crash { value, context }),
        _ => unreachable!("value statement node is unchanged"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate_for_test(
        expr: &Expr,
        values: &Values,
        globals: &Values,
        budget: &mut Budget,
    ) -> Result<ComptimeValue, String> {
        let functions = HashMap::new();
        let constructors = HashMap::new();
        let types = HashMap::new();
        let mut context = EvalContext {
            globals,
            functions: &functions,
            structs: StructScope {
                constructors: &constructors,
                owner: None,
            },
            types: &types,
            budget,
        };
        eval_expr(expr, values, &mut context, 0, 0)
    }

    #[test]
    fn comptime_shifts_match_runtime_shift_values() {
        use crate::value::Value;
        let values = [
            (ComptimeValue::Int(-5), Value::Int(-5)),
            (ComptimeValue::Int(3), Value::Int(3)),
            (ComptimeValue::Int(i64::MIN), Value::Int(i64::MIN)),
            (ComptimeValue::UInt(u64::MAX), Value::UInt(u64::MAX)),
            (ComptimeValue::UInt(3), Value::UInt(3)),
            (
                ComptimeValue::BigInt(BigInt::from(-5)),
                Value::BigInt(BigInt::from(-5)),
            ),
        ];
        let amounts = [
            (ComptimeValue::Int(-1), Value::Int(-1)),
            (ComptimeValue::Int(0), Value::Int(0)),
            (ComptimeValue::Int(1), Value::Int(1)),
            (ComptimeValue::Int(62), Value::Int(62)),
            (ComptimeValue::UInt(63), Value::UInt(63)),
            (ComptimeValue::Int(64), Value::Int(64)),
            (ComptimeValue::UInt(200), Value::UInt(200)),
        ];
        for (op, left) in [(BinOp::Shl, true), (BinOp::Shr, false)] {
            for (comptime_value, runtime_value) in &values {
                for (comptime_amount, runtime_amount) in &amounts {
                    let folded = eval_binary(
                        op,
                        comptime_value.clone(),
                        comptime_amount.clone(),
                        &mut Budget::default(),
                    )
                    .unwrap();
                    let runtime = crate::value::shift_values(
                        runtime_value.clone(),
                        runtime_amount.clone(),
                        left,
                        None,
                    );
                    let context = format!("{op:?} {runtime_value:?} {runtime_amount:?}");
                    match (folded, runtime) {
                        (ComptimeValue::Int(a), Ok(Value::Int(b))) => assert_eq!(a, b, "{context}"),
                        (ComptimeValue::UInt(a), Ok(Value::UInt(b))) => {
                            assert_eq!(a, b, "{context}")
                        }
                        (ComptimeValue::BigInt(a), Ok(Value::BigInt(b))) => {
                            assert_eq!(a, b, "{context}")
                        }
                        (ComptimeValue::Error(a), Err(b)) => {
                            assert_eq!(a.kind(), b.kind(), "{context}")
                        }
                        (folded, runtime) => panic!("{context}: {folded:?} != {runtime:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn comptime_mixed_numeric_comparisons_cover_float_boundaries() {
        use ComptimeValue::{Bool, Float, Int};
        let i64_max = Int(i64::MAX);
        let i64_min = Int(i64::MIN);
        let upper = Float(9223372036854775808.0);
        let lower = Float(-9223372036854775808.0);
        let above_53 = Int(9007199254740993);
        let exact_53 = Float(9007199254740992.0);

        assert!(!equal(&i64_max, &upper));
        assert!(equal(&i64_min, &lower));
        assert!(!equal(&above_53, &exact_53));
        assert!(equal(&Int(0), &Float(-0.0)));
        assert!(!equal(&Int(1), &Float(f64::NAN)));
        assert!(!equal(&Int(1), &Float(f64::INFINITY)));
        assert!(!equal(&Int(1), &Float(f64::NEG_INFINITY)));

        let evaluate =
            |op, left, right| match eval_binary(op, left, right, &mut Budget::default()).unwrap() {
                Bool(value) => value,
                _ => panic!("comptime comparison did not return bool"),
            };
        assert!(evaluate(BinOp::Lt, i64_max.clone(), upper.clone()));
        assert!(!evaluate(BinOp::Lt, upper, i64_max.clone()));
        assert!(!evaluate(BinOp::Lt, i64_min.clone(), lower.clone()));
        assert!(!evaluate(BinOp::Lt, above_53, exact_53));
        for op in [BinOp::Lt, BinOp::Gt, BinOp::LtEq, BinOp::GtEq] {
            assert!(!evaluate(op, Int(1), Float(f64::NAN)));
        }
        assert!(evaluate(BinOp::NotEq, Int(1), Float(f64::NAN)));
        assert!(!evaluate(BinOp::Eq, Int(1), Float(f64::INFINITY)));
        assert!(evaluate(BinOp::Lt, Int(1), Float(f64::INFINITY)));
        assert!(evaluate(BinOp::LtEq, Int(1), Float(f64::INFINITY)));
        assert!(!evaluate(BinOp::Gt, Int(1), Float(f64::INFINITY)));
        assert!(!evaluate(BinOp::GtEq, Int(1), Float(f64::INFINITY)));
        assert!(evaluate(BinOp::Gt, Int(1), Float(f64::NEG_INFINITY)));
        assert!(evaluate(BinOp::GtEq, Int(1), Float(f64::NEG_INFINITY)));
        assert!(!evaluate(BinOp::Lt, Int(1), Float(f64::NEG_INFINITY)));
        assert!(!evaluate(BinOp::LtEq, Int(1), Float(f64::NEG_INFINITY)));
    }

    #[test]
    fn comptime_identifier_clone_is_charged_before_copy() {
        let globals = HashMap::from([(
            "large".to_owned(),
            Some(ComptimeValue::Str("x".repeat(1024))),
        )]);
        let values = Values::new();
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - 512,
            ..Budget::default()
        };

        let result = evaluate_for_test(
            &Expr::Ident("large".to_owned()),
            &values,
            &globals,
            &mut budget,
        );

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
    }

    #[test]
    fn comptime_ast_materialization_clone_is_charged_before_copy() {
        let value = ComptimeValue::Str("x".repeat(1024));
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - 512,
            ..Budget::default()
        };

        let result = value.to_expr_with_budget(None, &mut budget);

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
    }

    #[test]
    fn comptime_nested_scope_copy_is_charged_before_copy() {
        let values = HashMap::from([(
            "large".to_owned(),
            Some(ComptimeValue::Str("x".repeat(1024))),
        )]);
        let globals = Values::new();
        let expression = Expr::Block(Block {
            stmts: Vec::new(),
            tail: Some(Box::new(Expr::Nil)),
        });
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - 512,
            ..Budget::default()
        };

        let result = evaluate_for_test(&expression, &values, &globals, &mut budget);

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
    }

    #[test]
    fn comptime_string_concatenation_is_charged_before_allocation() {
        let expression = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Str("x".repeat(1024))),
            Box::new(Expr::Str("y".repeat(1024))),
        );
        let values = Values::new();
        let globals = Values::new();
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - 3000,
            ..Budget::default()
        };

        let result = evaluate_for_test(&expression, &values, &globals, &mut budget);

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
    }

    #[test]
    fn comptime_binding_name_is_charged_before_copy() {
        let mut values = Values::new();
        let entry_cost = std::mem::size_of::<(String, Option<ComptimeValue>)>().saturating_mul(2);
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - entry_cost - 1,
            ..Budget::default()
        };

        let result = budget.insert_value(
            &mut values,
            &"parameter".repeat(128),
            Some(ComptimeValue::Int(1)),
        );

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
        assert!(values.is_empty());
    }

    #[test]
    fn comptime_constructor_field_name_is_charged_before_copy() {
        let fields = vec![StructField {
            name: "field".repeat(256),
            ty: None,
            default: Some(Expr::Int(1)),
            visibility: crate::ast::Visibility::Private,
        }];
        let vector_cost = std::mem::size_of::<(String, ComptimeValue)>();
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - vector_cost - 256,
            ..Budget::default()
        };
        let functions = HashMap::new();
        let constructors = HashMap::new();
        let types = HashMap::new();
        let globals = Values::new();
        let values = Values::new();
        let mut context = EvalContext {
            globals: &globals,
            functions: &functions,
            structs: StructScope {
                constructors: &constructors,
                owner: None,
            },
            types: &types,
            budget: &mut budget,
        };

        let constructor = Constructor {
            fields,
            owner: None,
        };
        let result =
            eval_struct_constructor("Wide", &constructor, &[], &values, &mut context, 0, 0);

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
    }

    #[test]
    fn comptime_vector_capacity_is_charged_before_reserve() {
        let mut values = Vec::<u64>::new();
        let mut budget = Budget {
            allocated: crate::ast_budget::MAX_COST - 1,
            ..Budget::default()
        };

        let result = budget.reserve_vec(&mut values, 4);

        assert!(result.unwrap_err().contains("allocation budget exceeded"));
        assert_eq!(values.capacity(), 0);
    }
}
