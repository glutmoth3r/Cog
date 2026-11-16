//! Small, conservative source warnings that never affect compilation success.
use crate::ast::{
    BinOp, Block, Expr, ForClause, FunctionDecl, Item, MatchPattern, Program, Stmt, StringPart,
    TypeKind, UnOp, VarType,
};
use crate::diagnostic::{Diagnostic, SourceSpan};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct Local {
    read: bool,
    report_unused: bool,
    boolean: bool,
    task_group: Option<usize>,
    span: Option<SourceSpan>,
}

struct TaskGroup {
    name: String,
    span: Option<SourceSpan>,
    started: bool,
    escaped: bool,
    report: bool,
}

#[derive(Default)]
struct Linter {
    scopes: Vec<HashMap<String, Local>>,
    warnings: Vec<Diagnostic>,
    async_functions: HashSet<String>,
    async_method_call_sites: HashSet<usize>,
    task_groups: Vec<TaskGroup>,
}

pub(crate) fn check_with_names(
    program: &Program,
    names: &std::collections::BTreeMap<String, String>,
) -> Vec<Diagnostic> {
    let mut async_functions = HashSet::new();
    for item in &program.items {
        match item {
            Item::Function(function) if function.asynchronous => {
                async_functions.insert(function.name.clone());
            }
            Item::Extend(extension) => {
                async_functions.extend(
                    extension
                        .methods
                        .iter()
                        .filter(|method| method.function.asynchronous)
                        .map(|method| method.function.name.clone()),
                );
            }
            _ => {}
        }
    }
    let resolved_methods =
        crate::analysis::check_with_methods_and_names(program, &HashMap::new(), names);
    let async_specializations = resolved_methods
        .specializations
        .iter()
        .filter(|specialization| async_functions.contains(&specialization.template))
        .map(|specialization| specialization.name.clone())
        .collect::<HashSet<_>>();
    let async_method_call_sites = resolved_methods
        .method_calls
        .into_iter()
        .filter_map(|((_, expression), function)| {
            async_functions.contains(&function).then_some(expression)
        })
        .chain(
            resolved_methods
                .call_specializations
                .into_iter()
                .filter_map(|((_, expression), function)| {
                    async_specializations
                        .contains(&function)
                        .then_some(expression)
                }),
        )
        .collect();
    let mut linter = Linter {
        async_functions,
        async_method_call_sites,
        ..Linter::default()
    };
    for item in &program.items {
        match item {
            Item::Function(function) => linter.function(function),
            Item::Extend(extension) => {
                for method in &extension.methods {
                    linter.function(&method.function);
                }
            }
            Item::GlobalConst { value, .. }
            | Item::GlobalVar { value, .. }
            | Item::Comptime { value, .. } => linter.expr(value, true),
            Item::StructDecl { fields, .. } => {
                for field in fields {
                    if let Some(default) = &field.default {
                        linter.expr(default, true);
                    }
                }
            }
            Item::Invalid { .. }
            | Item::Use(_)
            | Item::ComptimeInclude { .. }
            | Item::Trait(_)
            | Item::EnumDecl { .. } => {}
        }
    }
    linter.warnings.sort_by_key(diagnostic_position);
    linter.warnings
}

fn diagnostic_position(diagnostic: &Diagnostic) -> (String, usize) {
    diagnostic.span.as_ref().map_or_else(
        || (String::new(), 0),
        |span| (span.source.name.clone(), span.range.start),
    )
}

impl Linter {
    fn function(&mut self, function: &FunctionDecl) {
        self.scopes.push(HashMap::new());
        for parameter in &function.params {
            let boolean = parameter
                .ty
                .as_ref()
                .is_some_and(|ty| matches!(ty.kind, TypeKind::Primitive(VarType::Bool)));
            self.define_with_type(&parameter.name, false, None, boolean, None);
        }
        self.block(&function.body, true);
        self.pop_scope();
    }

    fn block(&mut self, block: &Block, report: bool) {
        self.scopes.push(HashMap::new());
        let mut reachable = report;
        let mut reported_unreachable = false;
        for statement in &block.stmts {
            if !reachable && report && !reported_unreachable {
                if let Some(span) = statement.span() {
                    self.warnings.push(Diagnostic::warning(
                        "W0002",
                        "statement is unreachable",
                        Some(span.clone()),
                    ));
                }
                reported_unreachable = true;
            }
            let statement_is_reachable = reachable;
            let terminates = self.stmt(statement, statement_is_reachable);
            if reachable && terminates {
                reachable = false;
            }
        }
        if reachable && let Some(tail) = &block.tail {
            self.expr(tail, report);
        }
        self.pop_scope();
    }

    fn stmt(&mut self, statement: &Stmt, report: bool) -> bool {
        match statement.node() {
            Stmt::Located(_, _) => unreachable!("node removes source locations"),
            Stmt::Invalid { .. } | Stmt::Use { .. } => {}
            Stmt::VarDecl { pattern, ty, value } | Stmt::Const { pattern, ty, value } => {
                let cold_task_call = self.is_cold_task_call(value);
                let task_alias = if matches!(pattern, crate::ast::BindingPattern::Name(_)) {
                    self.task_group_of_alias(value)
                } else {
                    None
                };
                if task_alias.is_some() {
                    self.read_task_alias(value, report);
                } else {
                    self.expr(value, report);
                }
                let boolean = ty
                    .as_ref()
                    .is_some_and(|ty| matches!(ty.kind, TypeKind::Primitive(VarType::Bool)))
                    || self.expr_is_boolean(value);
                match pattern {
                    crate::ast::BindingPattern::Name(name) => {
                        let task_group = if cold_task_call {
                            Some(self.new_task_group(name, statement.span(), report))
                        } else {
                            task_alias
                        };
                        self.define_with_type(name, report, statement.span(), boolean, task_group);
                    }
                    crate::ast::BindingPattern::Tuple(_) => {
                        pattern.visit_names(&mut |name| {
                            self.define(name, report, statement.span());
                        });
                    }
                }
            }
            Stmt::Comptime { name, ty, value } => {
                self.expr(value, report);
                let boolean = ty
                    .as_ref()
                    .is_some_and(|ty| matches!(ty.kind, TypeKind::Primitive(VarType::Bool)))
                    || self.expr_is_boolean(value);
                self.define_with_type(name, report, statement.span(), boolean, None);
            }
            Stmt::Assign { target, value } => {
                self.lvalue(target, report);
                self.expr(value, report);
            }
            Stmt::CompoundAssign { target, value, .. } => {
                self.expr(target, report);
                self.expr(value, report);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, report);
                }
            }
            Stmt::Defer { body } => self.block(body, report),
            Stmt::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, report);
                self.scopes.push(HashMap::new());
                self.define(catch_name, report, statement.span());
                self.block(catch_block, report);
                self.pop_scope();
            }
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => {
                self.expr(cond, report);
                if report && let Expr::Bool(value) = cond.node() {
                    self.warnings.push(Diagnostic::warning(
                        "W0003",
                        if *value {
                            "condition is always true; this `if` can be simplified to its `then` branch"
                        } else if else_block.is_some() {
                            "condition is always false; this `if` can be simplified to its `else` branch"
                        } else {
                            "condition is always false; this `if` body can be removed"
                        },
                        statement.span().cloned(),
                    ));
                }
                let then_reachable = report && !matches!(cond.node(), Expr::Bool(false));
                let else_reachable = report && !matches!(cond.node(), Expr::Bool(true));
                self.block(then_block, then_reachable);
                if let Some(block) = else_block {
                    self.block(block, else_reachable);
                }
            }
            Stmt::When {
                name,
                value,
                then_block,
                else_block,
            } => {
                self.expr(value, report);
                self.scopes.push(HashMap::new());
                self.define(name, report, statement.span());
                self.block(then_block, report);
                self.pop_scope();
                if let Some(block) = else_block {
                    self.block(block, report);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond, report);
                let body_reachable = report && !matches!(cond.node(), Expr::Bool(false));
                if report && !body_reachable {
                    self.warnings.push(
                        Diagnostic::warning(
                            "W0006",
                            "loop condition is always false; this loop body will never run",
                            statement.span().cloned(),
                        )
                        .note("change the condition or remove the loop"),
                    );
                }
                self.block(body, body_reachable);
            }
            Stmt::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.expr(start, report);
                self.expr(end, report);
                if let Some(step) = step {
                    self.expr(step, report);
                }
                let empty_range = step.is_none()
                    && matches!(
                        (start.node(), end.node()),
                        (Expr::Int(start), Expr::Int(end)) if start >= end
                    );
                if report && empty_range {
                    self.warnings.push(
                        Diagnostic::warning(
                            "W0007",
                            "range is statically empty; this loop body will never run",
                            statement.span().cloned(),
                        )
                        .note("check the range bounds or remove the loop"),
                    );
                }
                self.scopes.push(HashMap::new());
                self.define(var, report, statement.span());
                self.block(body, report && !empty_range);
                self.pop_scope();
            }
            Stmt::ForEach {
                pattern,
                iterable,
                body,
            } => {
                self.expr(iterable, report);
                self.scopes.push(HashMap::new());
                let mut names = Vec::new();
                pattern.visit_names(&mut |name| names.push(name.to_owned()));
                for name in names {
                    self.define(&name, report, statement.span());
                }
                self.block(body, report);
                self.pop_scope();
            }
            Stmt::ForZip { clauses, body } => {
                for clause in clauses {
                    match clause {
                        ForClause::Range {
                            start, end, step, ..
                        } => {
                            self.expr(start, report);
                            self.expr(end, report);
                            if let Some(step) = step {
                                self.expr(step, report);
                            }
                        }
                        ForClause::Each { iterable, .. } => self.expr(iterable, report),
                    }
                }
                self.scopes.push(HashMap::new());
                for clause in clauses {
                    match clause {
                        ForClause::Range { var, .. } => {
                            self.define(var, report, statement.span());
                        }
                        ForClause::Each { pattern, .. } => {
                            let mut names = Vec::new();
                            pattern.visit_names(&mut |name| names.push(name.to_owned()));
                            for name in names {
                                self.define(&name, report, statement.span());
                            }
                        }
                    }
                }
                self.block(body, report);
                self.pop_scope();
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Crash { value, .. } => self.expr(value, report),
            Stmt::Expr(expr) => {
                if report && self.is_cold_task_call(expr) {
                    self.warnings.push(
                        Diagnostic::warning(
                            "W0005",
                            "cold async task is discarded and will not run",
                            expr.span().cloned(),
                        )
                        .note("use `await` to run it now or `spawn` to start it as a child task"),
                    );
                }
                self.expr(expr, report);
            }
        }
        self.terminates(statement)
    }

    fn expr(&mut self, expression: &Expr, report: bool) {
        match expression.node() {
            Expr::Located(_, _) => unreachable!("node removes source locations"),
            Expr::Ident(name) => {
                if report {
                    let mut task_group = None;
                    for scope in self.scopes.iter_mut().rev() {
                        if let Some(local) = scope.get_mut(name) {
                            local.read = true;
                            task_group = local.task_group;
                            break;
                        }
                    }
                    if let Some(group) = task_group
                        && let Some(task) = self.task_groups.get_mut(group)
                    {
                        task.escaped = true;
                    }
                }
            }
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let StringPart::Expression(expression) = part {
                        self.expr(expression, report);
                    }
                }
            }
            Expr::Array(items) | Expr::Tuple(items) => {
                for item in items {
                    self.expr(item, report);
                }
            }
            Expr::Record(fields) => {
                for (_, value) in fields {
                    self.expr(value, report);
                }
            }
            Expr::Call(name, args) => {
                for argument in args {
                    if name == "spawn" && argument.name.is_none() {
                        self.mark_task_started(&argument.value, report);
                    } else {
                        self.expr(&argument.value, report);
                    }
                }
            }
            Expr::Apply(callee, args) => {
                self.expr(callee, report);
                for argument in args {
                    self.expr(&argument.value, report);
                }
            }
            Expr::BracketApply(target, args) => {
                self.expr(target, report);
                for argument in args {
                    self.expr(argument, report);
                }
            }
            Expr::SpecializedCall(_, _, args) => {
                for argument in args {
                    self.expr(&argument.value, report);
                }
            }
            Expr::AssociatedCall { args, .. } => {
                for argument in args {
                    self.expr(&argument.value, report);
                }
            }
            Expr::MethodCall(receiver, _, args) => {
                self.expr(receiver, report);
                for argument in args {
                    self.expr(argument, report);
                }
            }
            Expr::Index(target, index) | Expr::Binary(_, target, index) => {
                self.expr(target, report);
                self.expr(index, report);
            }
            Expr::Unary(UnOp::Await, value) => self.mark_task_started(value, report),
            Expr::Unary(_, value) | Expr::Propagate(value) | Expr::Cast(value, _) => {
                self.expr(value, report)
            }
            Expr::Fallback {
                value, fallback, ..
            } => {
                self.expr(value, report);
                self.expr(fallback, report);
            }
            Expr::Block(block) => self.block(block, report),
            Expr::Match { value, arms, .. } => {
                self.expr(value, report);
                for arm in arms {
                    self.scopes.push(HashMap::new());
                    if let MatchPattern::Variant { bindings, .. } = &arm.pattern {
                        for binding in bindings {
                            self.define(
                                binding,
                                report,
                                arm.body.stmts.first().and_then(Stmt::span),
                            );
                        }
                    }
                    if let Some(guard) = &arm.guard {
                        self.expr(guard, report);
                    }
                    self.block(&arm.body, report);
                    self.pop_scope();
                }
            }
            Expr::Try {
                body,
                catch_name,
                catch_block,
            } => {
                self.block(body, report);
                self.scopes.push(HashMap::new());
                self.define(catch_name, report, expression.span());
                self.block(catch_block, report);
                self.pop_scope();
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, report);
                if report && let Expr::Bool(value) = cond.node() {
                    self.warnings.push(Diagnostic::warning(
                        "W0003",
                        if *value {
                            "condition is always true; this conditional can be simplified to its `then` branch"
                        } else if else_branch.is_some() {
                            "condition is always false; this conditional can be simplified to its `else` branch"
                        } else {
                            "condition is always false; this conditional body can be removed"
                        },
                        expression.span().cloned(),
                    ));
                } else if report
                    && self.expr_is_boolean(cond)
                    && branch_bool(then_branch) == Some(true)
                    && else_branch.as_deref().and_then(branch_bool) == Some(false)
                {
                    self.warnings.push(Diagnostic::warning(
                        "W0004",
                        "this conditional returns its boolean condition; use the condition directly",
                        expression.span().cloned(),
                    ));
                }
                let then_reachable = report && !matches!(cond.node(), Expr::Bool(false));
                let else_reachable = report && !matches!(cond.node(), Expr::Bool(true));
                self.expr(then_branch, then_reachable);
                if let Some(branch) = else_branch {
                    self.expr(branch, else_reachable);
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
            | Expr::TypeLiteral(_) => {}
        }
    }

    fn is_cold_task_call(&self, expression: &Expr) -> bool {
        if self
            .async_method_call_sites
            .contains(&(expression as *const Expr as usize))
        {
            return true;
        }
        let name = match expression.node() {
            Expr::Call(name, _) | Expr::SpecializedCall(name, _, _) => name,
            _ => return false,
        };
        self.async_functions.contains(name)
            || crate::builtins::Builtin::from_name(name)
                .is_some_and(|builtin| builtin.returns_task())
    }

    fn task_group_of_alias(&self, expression: &Expr) -> Option<usize> {
        let Expr::Ident(name) = expression.node() else {
            return None;
        };
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return local.task_group;
            }
        }
        None
    }

    fn read_task_alias(&mut self, expression: &Expr, report: bool) {
        let Expr::Ident(name) = expression.node() else {
            return;
        };
        if report {
            for scope in self.scopes.iter_mut().rev() {
                if let Some(local) = scope.get_mut(name) {
                    local.read = true;
                    return;
                }
            }
        }
    }

    fn new_task_group(&mut self, name: &str, span: Option<&SourceSpan>, report: bool) -> usize {
        let group = self.task_groups.len();
        self.task_groups.push(TaskGroup {
            name: name.to_owned(),
            span: span.cloned(),
            started: false,
            escaped: false,
            report,
        });
        group
    }

    fn mark_task_started(&mut self, expression: &Expr, report: bool) {
        if let Expr::Ident(name) = expression.node()
            && report
        {
            let mut found = false;
            let mut task_group = None;
            for scope in self.scopes.iter_mut().rev() {
                if let Some(local) = scope.get_mut(name) {
                    local.read = true;
                    found = true;
                    task_group = local.task_group;
                    break;
                }
            }
            if let Some(group) = task_group
                && let Some(task) = self.task_groups.get_mut(group)
            {
                task.started = true;
            }
            if found {
                return;
            }
        }
        self.expr(expression, report);
    }

    fn lvalue(&mut self, target: &Expr, report: bool) {
        match target.node() {
            Expr::Ident(_) => {}
            Expr::Index(receiver, index) => {
                self.expr(receiver, report);
                self.expr(index, report);
            }
            _ => self.expr(target, report),
        }
    }

    fn define(&mut self, name: &str, report_unused: bool, span: Option<&SourceSpan>) {
        self.define_with_type(name, report_unused, span, false, None);
    }

    fn define_with_type(
        &mut self,
        name: &str,
        report_unused: bool,
        span: Option<&SourceSpan>,
        boolean: bool,
        task_group: Option<usize>,
    ) {
        if name.starts_with('_') {
            return;
        }
        self.scopes
            .last_mut()
            .expect("a function or block scope is active")
            .insert(
                name.to_owned(),
                Local {
                    read: false,
                    report_unused,
                    boolean,
                    task_group,
                    span: span.cloned(),
                },
            );
    }

    fn pop_scope(&mut self) {
        let Some(scope) = self.scopes.pop() else {
            return;
        };
        let candidates: HashSet<_> = scope
            .values()
            .filter_map(|local| local.task_group)
            .collect();
        let mut warned_groups = HashSet::new();
        for group in candidates {
            let still_in_scope = self
                .scopes
                .iter()
                .any(|scope| scope.values().any(|local| local.task_group == Some(group)));
            if still_in_scope {
                continue;
            }
            let Some(task) = self.task_groups.get(group) else {
                continue;
            };
            if task.report && !task.started && !task.escaped {
                self.warnings.push(
                    Diagnostic::warning(
                        "W0005",
                        format!("cold async task `{}` is never started", task.name),
                        task.span.clone(),
                    )
                    .note("use `await` to run it now or `spawn` to start it as a child task"),
                );
                warned_groups.insert(group);
            }
        }
        for (name, local) in scope {
            if local
                .task_group
                .is_some_and(|group| warned_groups.contains(&group))
            {
                continue;
            }
            if local.report_unused && !local.read {
                self.warnings.push(
                    Diagnostic::warning(
                        "W0001",
                        format!("local binding `{name}` is never read"),
                        local.span,
                    )
                    .note("prefix the name with `_` if the unused binding is intentional"),
                );
            }
        }
    }

    fn terminates(&self, statement: &Stmt) -> bool {
        match statement.node() {
            Stmt::Return(_) | Stmt::Break | Stmt::Continue | Stmt::Crash { .. } => true,
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => match cond.node() {
                Expr::Bool(true) => self.block_terminates(then_block),
                Expr::Bool(false) => else_block
                    .as_ref()
                    .is_some_and(|block| self.block_terminates(block)),
                _ => {
                    self.block_terminates(then_block)
                        && else_block
                            .as_ref()
                            .is_some_and(|block| self.block_terminates(block))
                }
            },
            _ => false,
        }
    }

    fn block_terminates(&self, block: &Block) -> bool {
        for statement in &block.stmts {
            if self.terminates(statement) {
                return true;
            }
        }
        false
    }

    fn expr_is_boolean(&self, expression: &Expr) -> bool {
        match expression.node() {
            Expr::Bool(_) => true,
            Expr::Ident(name) => self
                .scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
                .is_some_and(|local| local.boolean),
            Expr::Binary(
                BinOp::Eq
                | BinOp::NotEq
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
                | BinOp::And
                | BinOp::Or
                | BinOp::Xor,
                _,
                _,
            )
            | Expr::Unary(UnOp::Not, _)
            | Expr::Cast(_, VarType::Bool) => true,
            Expr::If {
                then_branch,
                else_branch: Some(else_branch),
                ..
            } => branch_bool(then_branch).is_some() && branch_bool(else_branch).is_some(),
            _ => false,
        }
    }
}

fn branch_bool(expression: &Expr) -> Option<bool> {
    match expression.node() {
        Expr::Bool(value) => Some(*value),
        Expr::Block(block) if block.stmts.is_empty() => block.tail.as_deref().and_then(branch_bool),
        _ => None,
    }
}
