//! Syntactic summary of the local bindings a piece of code can rebind. Flow
//! narrowing uses it to discard facts conservatively. Matching is by name, so a
//! shadowing binding only loses precision.
use crate::ast::*;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Writes {
    /// Names that are the target of an assignment.
    pub assigned: HashSet<String>,
    /// Names captured mutably by a closure; any call may rebind them.
    pub captured: HashSet<String>,
    /// Names assigned by a `defer` block, which runs at an implicit scope exit.
    pub deferred: HashSet<String>,
    pub has_defer: bool,
    in_defer: bool,
}

impl Writes {
    pub fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
    }

    pub fn stmt(&mut self, stmt: &Stmt) {
        match stmt.node() {
            Stmt::Located(_, _) => unreachable!("node removes locations"),
            Stmt::VarDecl { value, .. }
            | Stmt::Const { value, .. }
            | Stmt::Comptime { value, .. }
            | Stmt::Crash { value, .. }
            | Stmt::Expr(value)
            | Stmt::Return(Some(value)) => self.expr(value),
            Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
                if let Expr::Ident(name) = target.node() {
                    self.assigned.insert(name.clone());
                    if self.in_defer {
                        self.deferred.insert(name.clone());
                    }
                }
                self.expr(target);
                self.expr(value);
            }
            Stmt::Defer { body } => {
                self.has_defer = true;
                let previous = std::mem::replace(&mut self.in_defer, true);
                self.block(body);
                self.in_defer = previous;
            }
            Stmt::Try {
                body, catch_block, ..
            } => {
                self.block(body);
                self.block(catch_block);
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
                self.expr(cond);
                self.block(then_block);
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            Stmt::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                self.expr(start);
                self.expr(end);
                if let Some(step) = step {
                    self.expr(step);
                }
                self.block(body);
            }
            Stmt::ForEach { iterable, body, .. } => {
                self.expr(iterable);
                self.block(body);
            }
            Stmt::ForZip { clauses, body } => {
                for clause in clauses {
                    match clause {
                        ForClause::Range {
                            start, end, step, ..
                        } => {
                            self.expr(start);
                            self.expr(end);
                            if let Some(step) = step {
                                self.expr(step);
                            }
                        }
                        ForClause::Each { iterable, .. } => self.expr(iterable),
                    }
                }
                self.block(body);
            }
            Stmt::Invalid { .. }
            | Stmt::Use { .. }
            | Stmt::Return(None)
            | Stmt::Break
            | Stmt::Continue => {}
        }
    }

    pub fn expr(&mut self, expr: &Expr) {
        match expr.node() {
            Expr::Located(_, _) => unreachable!("node removes locations"),
            Expr::Closure { captures, body, .. } => {
                self.captured.extend(
                    captures
                        .iter()
                        .filter(|capture| capture.mutable)
                        .map(|capture| capture.name.clone()),
                );
                self.block(body);
            }
            Expr::Array(items) | Expr::Tuple(items) => {
                for item in items {
                    self.expr(item);
                }
            }
            Expr::Record(fields) => {
                for (_, value) in fields {
                    self.expr(value);
                }
            }
            Expr::Call(_, args) | Expr::SpecializedCall(_, _, args) => {
                for argument in args {
                    self.expr(&argument.value);
                }
            }
            Expr::AssociatedCall { args, .. } => {
                for argument in args {
                    self.expr(&argument.value);
                }
            }
            Expr::Apply(callee, args) => {
                self.expr(callee);
                for argument in args {
                    self.expr(&argument.value);
                }
            }
            Expr::MethodCall(receiver, _, args) | Expr::BracketApply(receiver, args) => {
                self.expr(receiver);
                for argument in args {
                    self.expr(argument);
                }
            }
            Expr::Index(left, right) | Expr::Binary(_, left, right) => {
                self.expr(left);
                self.expr(right);
            }
            Expr::Unary(_, inner) | Expr::Propagate(inner) | Expr::Cast(inner, _) => {
                self.expr(inner)
            }
            Expr::Interpolated(parts) => {
                for part in parts {
                    if let StringPart::Expression(value) = part {
                        self.expr(value);
                    }
                }
            }
            Expr::Fallback {
                value, fallback, ..
            } => {
                self.expr(value);
                self.expr(fallback);
            }
            Expr::Block(block) | Expr::Capture(block) => self.block(block),
            Expr::Match { value, arms, .. } => {
                self.expr(value);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.expr(guard);
                    }
                    self.block(&arm.body);
                }
            }
            Expr::Try {
                body, catch_block, ..
            } => {
                self.block(body);
                self.block(catch_block);
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond);
                self.expr(then_branch);
                if let Some(else_branch) = else_branch {
                    self.expr(else_branch);
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
            | Expr::Ident(_) => {}
        }
    }
}
