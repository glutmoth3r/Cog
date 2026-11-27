//! Bounded monomorphization of generic functions after semantic inference.
//!
//! Each expansion round checks the current AST, uses call-site facts to discover
//! specializations, and rechecks the expanded program. The interprocedural
//! mutability pass runs only on the final round (or a failing one). The work and
//! instance caps bound compiler cost; the final check result is passed on to
//! codegen. The uses a template makes of its trait bounds are carried to each
//! instance, so the instance calls the implementation that proved the bound.
use crate::analysis::{self, BoundCall, BoundCalls, CheckResult, Specialization};
use crate::ast::{
    Block, Expr, ForClause, FunctionDecl, Item, Program, Stmt, StringPart, TypeAnnotation, TypeKind,
};
use crate::diagnostic::Diagnostic;
use std::collections::{HashMap, HashSet};

const MAX_SPECIALIZATIONS: usize = 256;
// Specialization discovery rechecks the expanded program once per round. Bound
// the aggregate estimated AST work across those full-program passes as well as
// bounding the AST clones themselves.
const MAX_SPECIALIZATION_ANALYSIS_COST: usize = 8 * crate::ast_budget::MAX_COST;

pub(crate) fn expand(
    source: &Program,
    function_modules: &std::collections::HashMap<String, String>,
) -> Result<(Program, CheckResult), Vec<Diagnostic>> {
    expand_with_names(source, function_modules, &std::collections::BTreeMap::new())
}

pub(crate) fn expand_with_names(
    source: &Program,
    function_modules: &std::collections::HashMap<String, String>,
    names: &std::collections::BTreeMap<String, String>,
) -> Result<(Program, CheckResult), Vec<Diagnostic>> {
    expand_with_analysis_limit_and_names(
        source,
        function_modules,
        names,
        MAX_SPECIALIZATION_ANALYSIS_COST,
    )
}

#[cfg(test)]
fn expand_with_analysis_limit(
    source: &Program,
    function_modules: &std::collections::HashMap<String, String>,
    analysis_limit: usize,
) -> Result<(Program, CheckResult), Vec<Diagnostic>> {
    expand_with_analysis_limit_and_names(
        source,
        function_modules,
        &std::collections::BTreeMap::new(),
        analysis_limit,
    )
}

fn expand_with_analysis_limit_and_names(
    source: &Program,
    function_modules: &std::collections::HashMap<String, String>,
    names: &std::collections::BTreeMap<String, String>,
    analysis_limit: usize,
) -> Result<(Program, CheckResult), Vec<Diagnostic>> {
    let mut allocation_cost = crate::ast_budget::program(source).map_err(budget_error)?;
    let mut analysis_cost = check_analysis_cost(0, allocation_cost, analysis_limit)?;
    let mut program = source.clone();
    let mut generated = HashSet::new();
    let mut bound_calls = BoundCalls::default();
    for round in 0..MAX_SPECIALIZATIONS {
        // Intermediate rounds only discover specializations. The mutability pass
        // runs once on the final program, whose call sites all resolve to
        // generated instances, or on a round that ends with an error, where its
        // diagnostics take precedence as they did when every round ran it.
        let mut checked =
            analysis::check_deferring_effects(&program, function_modules, names, &bound_calls);
        if !checked.diagnostics.is_empty() {
            analysis::check_deferred_effects(&program, &mut checked);
            return Err(checked.diagnostics);
        }
        let additions =
            specialization_additions(&program, &checked, &mut generated, &mut allocation_cost)
                .and_then(|additions| {
                    if additions.is_empty() {
                        return Ok(additions);
                    }
                    if round + 1 == MAX_SPECIALIZATIONS {
                        return Err(vec![Diagnostic::new(
                            "E4003",
                            format!(
                                "generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"
                            ),
                            None,
                        )]);
                    }
                    analysis_cost =
                        check_analysis_cost(analysis_cost, allocation_cost, analysis_limit)?;
                    Ok(additions)
                });
        let additions = match additions {
            Ok(additions) => additions,
            Err(diagnostics) => {
                analysis::check_deferred_effects(&program, &mut checked);
                return Err(if checked.diagnostics.is_empty() {
                    diagnostics
                } else {
                    checked.diagnostics
                });
            }
        };
        if additions.is_empty() {
            analysis::check_deferred_effects(&program, &mut checked);
            if !checked.diagnostics.is_empty() {
                return Err(checked.diagnostics);
            }
            return Ok((program, checked));
        }
        let instances: HashSet<_> = additions
            .iter()
            .filter_map(|item| match item {
                Item::Function(function) => Some(function.name.clone()),
                _ => None,
            })
            .collect();
        // Expression identities are stable once an instance is in the program.
        program.items.extend(additions);
        for specialization in &checked.specializations {
            if instances.contains(&specialization.name) {
                carry_bound_calls(
                    &mut program,
                    specialization,
                    &checked.probed,
                    &mut bound_calls,
                );
            }
        }
    }
    unreachable!("the last specialization round returns")
}

/// Builds the instances requested by one checked round that have not been
/// generated yet.
fn specialization_additions(
    program: &Program,
    checked: &CheckResult,
    generated: &mut HashSet<String>,
    allocation_cost: &mut usize,
) -> Result<Vec<Item>, Vec<Diagnostic>> {
    let mut additions = Vec::new();
    for specialization in &checked.specializations {
        if !generated.insert(specialization.name.clone()) {
            continue;
        }
        let Some(template) = function_template(program, &specialization.template) else {
            return Err(vec![Diagnostic::new(
                "E4003",
                format!(
                    "generic function template `{}` is unavailable",
                    specialization.template
                ),
                None,
            )]);
        };
        check_allocation(
            *allocation_cost,
            crate::ast_budget::function(template).map_err(budget_error)?,
        )?;
        let mut instance = template.clone();
        specialize_function(&mut instance, &specialization.arguments);
        instance.name.clone_from(&specialization.name);
        instance.type_params.clear();
        let instance = Item::Function(instance);
        *allocation_cost = check_allocation(
            *allocation_cost,
            crate::ast_budget::item(&instance).map_err(budget_error)?,
        )?;
        additions.push(instance);
        if generated.len() > MAX_SPECIALIZATIONS {
            return Err(vec![Diagnostic::new(
                "E4003",
                format!("generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"),
                None,
            )]);
        }
    }
    for specialization in &checked.struct_specializations {
        if !generated.insert(specialization.name.clone()) {
            continue;
        }
        let Some(Item::StructDecl {
            visibility,
            fields,
            owner,
            ..
        }) = program.items.iter().find(|item| {
            matches!(item, Item::StructDecl { name, type_params, .. }
                if name == &specialization.template && !type_params.is_empty())
        })
        else {
            return Err(vec![Diagnostic::new(
                "E4003",
                format!(
                    "generic struct template `{}` is unavailable",
                    specialization.template
                ),
                None,
            )]);
        };
        check_allocation(
            *allocation_cost,
            crate::ast_budget::fields(fields).map_err(budget_error)?,
        )?;
        let mut instance_fields = fields.clone();
        for field in &mut instance_fields {
            if let Some(ty) = &mut field.ty {
                substitute_type(ty, &specialization.arguments);
            }
            if let Some(default) = &mut field.default {
                substitute_expr(default, &specialization.arguments, &mut Vec::new());
            }
        }
        let instance = Item::StructDecl {
            name: specialization.name.clone(),
            visibility: *visibility,
            type_params: Vec::new(),
            fields: instance_fields,
            owner: owner.clone(),
        };
        *allocation_cost = check_allocation(
            *allocation_cost,
            crate::ast_budget::item(&instance).map_err(budget_error)?,
        )?;
        additions.push(instance);
        if generated.len() > MAX_SPECIALIZATIONS {
            return Err(vec![Diagnostic::new(
                "E4003",
                format!("generic specialization limit ({MAX_SPECIALIZATIONS}) exceeded"),
                None,
            )]);
        }
    }
    Ok(additions)
}

/// Records for an instance the bound uses of its template, with the type
/// arguments of the instance.
fn carry_bound_calls(
    program: &mut Program,
    specialization: &Specialization,
    probed: &BoundCalls,
    bound_calls: &mut BoundCalls,
) {
    if probed.methods.is_empty() && probed.iterations.is_empty() {
        return;
    }
    let template = expression_ids(program, &specialization.template);
    let instance = expression_ids(program, &specialization.name);
    let specialize = |call: &BoundCall| {
        let mut call = call.clone();
        substitute_type(&mut call.target, &specialization.arguments);
        for bound in &mut call.bounds {
            for argument in &mut bound.arguments {
                substitute_type(argument, &specialization.arguments);
            }
        }
        call
    };
    for (template, instance) in template.into_iter().zip(instance) {
        if let Some(call) = probed.methods.get(&template) {
            bound_calls.methods.insert(instance, specialize(call));
        }
        if let Some(call) = probed.iterations.get(&template) {
            bound_calls.iterations.insert(instance, specialize(call));
        }
    }
}

/// The identities of a function's expressions in a fixed traversal order, which
/// a template shares with its instances.
fn expression_ids(program: &mut Program, name: &str) -> Vec<usize> {
    let function = program.items.iter_mut().find_map(|item| match item {
        Item::Function(function) if function.name == name => Some(function),
        Item::Extend(extension) => extension
            .methods
            .iter_mut()
            .find(|method| method.function.name == name)
            .map(|method| &mut method.function),
        _ => None,
    });
    // Substituting no arguments visits every expression and changes nothing.
    function.map_or_else(Vec::new, |function| {
        specialize_function(function, &HashMap::new())
    })
}

fn function_template<'a>(program: &'a Program, name: &str) -> Option<&'a FunctionDecl> {
    program.items.iter().find_map(|item| match item {
        Item::Function(function) if function.name == name => Some(function),
        Item::Extend(extension) => extension
            .methods
            .iter()
            .find(|method| method.function.name == name)
            .map(|method| &method.function),
        _ => None,
    })
}

fn budget_error(message: String) -> Vec<Diagnostic> {
    vec![Diagnostic::new("E4003", message, None)]
}

fn check_allocation(current: usize, additional: usize) -> Result<usize, Vec<Diagnostic>> {
    current
        .checked_add(additional)
        .filter(|cost| *cost <= crate::ast_budget::MAX_COST)
        .ok_or_else(|| budget_error("compiler specialization allocation budget exceeded".into()))
}

fn check_analysis_cost(
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, Vec<Diagnostic>> {
    current
        .checked_add(additional)
        .filter(|cost| *cost <= limit)
        .ok_or_else(|| budget_error("compiler specialization analysis budget exceeded".into()))
}

/// Substitutes type arguments and returns the identities of the visited
/// expressions.
fn specialize_function(
    function: &mut FunctionDecl,
    arguments: &HashMap<String, TypeAnnotation>,
) -> Vec<usize> {
    let mut visited = Vec::new();
    for parameter in &mut function.params {
        if let Some(ty) = &mut parameter.ty {
            substitute_type(ty, arguments);
        }
        if let Some(default) = &mut parameter.default {
            substitute_expr(default, arguments, &mut visited);
        }
    }
    if let Some(result) = &mut function.result_type {
        substitute_type(result, arguments);
    }
    substitute_block(&mut function.body, arguments, &mut visited);
    visited
}

fn substitute_type(ty: &mut TypeAnnotation, arguments: &HashMap<String, TypeAnnotation>) {
    if let TypeKind::TypeParameter(name) = &ty.kind {
        if let Some(replacement) = arguments.get(name) {
            let nilable = ty.nilable || replacement.nilable;
            *ty = replacement.clone();
            ty.nilable = nilable;
        }
        return;
    }
    match &mut ty.kind {
        TypeKind::Array(element) => substitute_type(element, arguments),
        TypeKind::Applied {
            arguments: types, ..
        }
        | TypeKind::ResolvedApplied {
            arguments: types, ..
        } => {
            for argument in types {
                substitute_type(argument, arguments);
            }
        }
        TypeKind::Tuple(elements) => {
            for element in elements {
                substitute_type(element, arguments);
            }
        }
        TypeKind::Function { parameters, result } => {
            for parameter in parameters {
                substitute_type(parameter, arguments);
            }
            substitute_type(result, arguments);
        }
        TypeKind::Primitive(_)
        | TypeKind::Bytes
        | TypeKind::Named(_)
        | TypeKind::ResolvedName { .. }
        | TypeKind::TypeParameter(_)
        | TypeKind::SelfType => {}
    }
}

fn substitute_block(
    block: &mut Block,
    arguments: &HashMap<String, TypeAnnotation>,
    visited: &mut Vec<usize>,
) {
    for statement in &mut block.stmts {
        substitute_stmt(statement, arguments, visited);
    }
    if let Some(tail) = &mut block.tail {
        substitute_expr(tail, arguments, visited);
    }
}

fn substitute_stmt(
    statement: &mut Stmt,
    arguments: &HashMap<String, TypeAnnotation>,
    visited: &mut Vec<usize>,
) {
    match statement {
        Stmt::Located(_, inner) => substitute_stmt(inner, arguments, visited),
        Stmt::VarDecl { ty, value, .. }
        | Stmt::Const { ty, value, .. }
        | Stmt::Comptime { ty, value, .. } => {
            if let Some(ty) = ty {
                substitute_type(ty, arguments);
            }
            substitute_expr(value, arguments, visited);
        }
        Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
            substitute_expr(target, arguments, visited);
            substitute_expr(value, arguments, visited);
        }
        Stmt::Return(value) => {
            if let Some(value) = value {
                substitute_expr(value, arguments, visited);
            }
        }
        Stmt::Defer { body } => substitute_block(body, arguments, visited),
        Stmt::While { cond, body } => {
            substitute_expr(cond, arguments, visited);
            substitute_block(body, arguments, visited);
        }
        Stmt::Try {
            body, catch_block, ..
        } => {
            substitute_block(body, arguments, visited);
            substitute_block(catch_block, arguments, visited);
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
        }
        | Stmt::When {
            value: cond,
            then_block,
            else_block,
            ..
        } => {
            substitute_expr(cond, arguments, visited);
            substitute_block(then_block, arguments, visited);
            if let Some(else_block) = else_block {
                substitute_block(else_block, arguments, visited);
            }
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            substitute_expr(start, arguments, visited);
            substitute_expr(end, arguments, visited);
            if let Some(step) = step {
                substitute_expr(step, arguments, visited);
            }
            substitute_block(body, arguments, visited);
        }
        Stmt::ForEach { iterable, body, .. } => {
            substitute_expr(iterable, arguments, visited);
            substitute_block(body, arguments, visited);
        }
        Stmt::ForZip { clauses, body } => {
            for clause in clauses {
                match clause {
                    ForClause::Range {
                        start, end, step, ..
                    } => {
                        substitute_expr(start, arguments, visited);
                        substitute_expr(end, arguments, visited);
                        if let Some(step) = step {
                            substitute_expr(step, arguments, visited);
                        }
                    }
                    ForClause::Each { iterable, .. } => {
                        substitute_expr(iterable, arguments, visited)
                    }
                }
            }
            substitute_block(body, arguments, visited);
        }
        Stmt::Crash { value, .. } | Stmt::Expr(value) => substitute_expr(value, arguments, visited),
        Stmt::Invalid { .. } | Stmt::Use { .. } | Stmt::Break | Stmt::Continue => {}
    }
}

fn substitute_expr(
    expr: &mut Expr,
    arguments: &HashMap<String, TypeAnnotation>,
    visited: &mut Vec<usize>,
) {
    visited.push(crate::mutability::expr_id(expr));
    match expr {
        Expr::Located(_, inner)
        | Expr::Propagate(inner)
        | Expr::Unary(_, inner)
        | Expr::Cast(inner, _) => substitute_expr(inner, arguments, visited),
        Expr::TypeLiteral(ty) => substitute_type(ty, arguments),
        Expr::TypedAggregate(value, ty) => {
            substitute_type(ty, arguments);
            substitute_expr(value, arguments, visited);
        }
        Expr::Closure {
            params,
            result_type,
            body,
            ..
        } => {
            for parameter in params {
                if let Some(ty) = &mut parameter.ty {
                    substitute_type(ty, arguments);
                }
            }
            if let Some(ty) = result_type {
                substitute_type(ty, arguments);
            }
            substitute_block(body, arguments, visited);
        }
        Expr::Array(items) | Expr::Tuple(items) => {
            for item in items {
                substitute_expr(item, arguments, visited);
            }
        }
        Expr::Record(fields) => {
            for (_, value) in fields {
                substitute_expr(value, arguments, visited);
            }
        }
        Expr::SpecializedCall(_, types, call_args) => {
            for ty in types {
                substitute_type(ty, arguments);
            }
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments, visited);
            }
        }
        Expr::AssociatedCall {
            target,
            type_arguments,
            args: call_args,
            ..
        } => {
            substitute_type(target, arguments);
            for ty in type_arguments {
                substitute_type(ty, arguments);
            }
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments, visited);
            }
        }
        Expr::BracketApply(receiver, call_args) => {
            substitute_expr(receiver, arguments, visited);
            for argument in call_args {
                substitute_expr(argument, arguments, visited);
            }
        }
        Expr::Apply(receiver, call_args) => {
            substitute_expr(receiver, arguments, visited);
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments, visited);
            }
        }
        Expr::Call(_, call_args) => {
            for argument in call_args {
                substitute_expr(&mut argument.value, arguments, visited);
            }
        }
        Expr::MethodCall(receiver, _, call_args) => {
            substitute_expr(receiver, arguments, visited);
            for argument in call_args {
                substitute_expr(argument, arguments, visited);
            }
        }
        Expr::Index(receiver, index) | Expr::Binary(_, receiver, index) => {
            substitute_expr(receiver, arguments, visited);
            substitute_expr(index, arguments, visited);
        }
        Expr::Interpolated(parts) => {
            for part in parts {
                if let StringPart::Expression(value) = part {
                    substitute_expr(value, arguments, visited);
                }
            }
        }
        Expr::Fallback {
            value, fallback, ..
        } => {
            substitute_expr(value, arguments, visited);
            substitute_expr(fallback, arguments, visited);
        }
        Expr::Block(block) | Expr::Capture(block) => substitute_block(block, arguments, visited),
        Expr::Match { value, arms, .. } => {
            substitute_expr(value, arguments, visited);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    substitute_expr(guard, arguments, visited);
                }
                substitute_block(&mut arm.body, arguments, visited);
            }
        }
        Expr::Try {
            body, catch_block, ..
        } => {
            substitute_block(body, arguments, visited);
            substitute_block(catch_block, arguments, visited);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            substitute_expr(cond, arguments, visited);
            substitute_expr(then_branch, arguments, visited);
            if let Some(else_branch) = else_branch {
                substitute_expr(else_branch, arguments, visited);
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

#[cfg(test)]
mod tests {
    use super::{check_analysis_cost, expand_with_analysis_limit};
    use crate::ast_budget;
    use std::collections::HashMap;

    #[test]
    fn generic_expansion_charges_each_full_program_analysis_pass() {
        let parsed = crate::frontend::parse(
            "generic-budget.cog",
            "fn identity[T: type](value: T) -> T { value }\
             fn main() { identity(1); }",
        );
        assert!(parsed.diagnostics.is_empty());
        let source_cost = ast_budget::program(&parsed.program).unwrap();

        let errors = match expand_with_analysis_limit(&parsed.program, &HashMap::new(), source_cost)
        {
            Err(errors) => errors,
            Ok(_) => panic!("generic expansion exceeded the injected analysis budget"),
        };
        assert!(
            errors[0]
                .message
                .contains("specialization analysis budget exceeded"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn mutation_errors_take_precedence_over_a_later_specialization_budget_error() {
        let parsed = crate::frontend::parse(
            "generic-budget-mutation.cog",
            "fn identity[T: type](value: T) -> T { value }\
             fn main() { const values = [1]; values.append(2); identity(1); }",
        );
        assert!(parsed.diagnostics.is_empty());
        let source_cost = ast_budget::program(&parsed.program).unwrap();

        let errors = match expand_with_analysis_limit(&parsed.program, &HashMap::new(), source_cost)
        {
            Err(errors) => errors,
            Ok(_) => panic!("the const mutation was accepted"),
        };
        assert!(
            errors
                .iter()
                .all(|error| error.message.contains("cannot mutate")),
            "{:?}",
            errors
                .iter()
                .map(|error| &error.message)
                .collect::<Vec<_>>()
        );
        assert!(!errors.is_empty());
    }

    #[test]
    fn analysis_budget_rejects_overflow_and_excess_before_addition() {
        assert_eq!(check_analysis_cost(3, 4, 7).unwrap(), 7);
        let error = check_analysis_cost(usize::MAX, 1, usize::MAX).unwrap_err();
        assert!(error[0].message.contains("specialization analysis budget"));
        let error = check_analysis_cost(6, 2, 7).unwrap_err();
        assert!(error[0].message.contains("specialization analysis budget"));
    }
}
