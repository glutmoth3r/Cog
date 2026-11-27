//! Iterative accounting before cloning or analyzing generated syntax.
use crate::ast::*;

pub(crate) const MAX_COST: usize = 64 * 1024 * 1024;
const NODE_COST: usize = 256;
const MAX_PENDING_NODES: usize = MAX_COST / NODE_COST;

struct Pending<'a> {
    nodes: Vec<Node<'a>>,
    limit: usize,
    exceeded: bool,
}

impl<'a> Pending<'a> {
    fn new(limit: usize) -> Self {
        Self {
            nodes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn reserve(&mut self, additional: usize) -> bool {
        if self.exceeded {
            return false;
        }
        if additional > self.limit.saturating_sub(self.nodes.len())
            || self.nodes.try_reserve(additional).is_err()
        {
            self.exceeded = true;
            return false;
        }
        true
    }

    fn push(&mut self, node: Node<'a>) {
        if self.reserve(1) {
            self.nodes.push(node);
        }
    }

    fn extend(&mut self, nodes: impl IntoIterator<Item = Node<'a>>) {
        let nodes = nodes.into_iter();
        let (lower, upper) = nodes.size_hint();
        if let Some(count) = upper.filter(|upper| *upper == lower) {
            if self.reserve(count) {
                self.nodes.extend(nodes);
            }
            return;
        }
        for node in nodes {
            self.push(node);
            if self.exceeded {
                break;
            }
        }
    }

    fn pop(&mut self) -> Option<Node<'a>> {
        self.nodes.pop()
    }
}

enum Node<'a> {
    Item(&'a Item),
    Function(&'a FunctionDecl),
    Field(&'a StructField),
    Block(&'a Block),
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    BindingPattern(&'a BindingPattern),
    Type(&'a TypeAnnotation, usize),
}

pub(crate) fn program(program: &Program) -> Result<usize, String> {
    measure(program.items.iter().map(Node::Item))
}

pub(crate) fn item(item: &Item) -> Result<usize, String> {
    measure(std::iter::once(Node::Item(item)))
}
pub(crate) fn function(function: &FunctionDecl) -> Result<usize, String> {
    measure(std::iter::once(Node::Function(function)))
}
pub(crate) fn fields(fields: &[StructField]) -> Result<usize, String> {
    measure(fields.iter().map(Node::Field))
}
pub(crate) fn annotation(ty: &TypeAnnotation) -> Result<usize, String> {
    measure(std::iter::once(Node::Type(ty, 0)))
}

fn measure<'a>(nodes: impl IntoIterator<Item = Node<'a>>) -> Result<usize, String> {
    measure_with_pending_limit(nodes, MAX_PENDING_NODES)
}

fn measure_with_pending_limit<'a>(
    nodes: impl IntoIterator<Item = Node<'a>>,
    pending_limit: usize,
) -> Result<usize, String> {
    let mut pending = Pending::new(pending_limit);
    pending.extend(nodes);
    if pending.exceeded {
        return Err("compiler syntax accounting queue budget exceeded".into());
    }
    let mut cost = 0usize;
    while let Some(node) = pending.pop() {
        cost = cost.saturating_add(NODE_COST);
        match node {
            Node::Item(item) => match item {
                Item::Function(function) => pending.push(Node::Function(function)),
                Item::GlobalConst { ty, value, .. }
                | Item::GlobalVar { ty, value, .. }
                | Item::Comptime { ty, value, .. } => {
                    pending.push(Node::Expr(value));
                    if let Some(ty) = ty {
                        pending.push(Node::Type(ty, 0));
                    }
                }
                Item::StructDecl {
                    name,
                    owner,
                    fields,
                    ..
                } => {
                    cost = cost.saturating_add(name.len());
                    if let Some(owner) = owner {
                        cost = cost.saturating_add(owner.len());
                    }
                    pending.extend(fields.iter().map(Node::Field));
                }
                Item::EnumDecl { variants, .. } => {
                    for variant in variants {
                        pending.extend(variant.payload.iter().map(Node::Field));
                    }
                }
                Item::Extend(extension) => {
                    pending.push(Node::Type(&extension.target, 0));
                    pending.extend(extension.trait_arguments.iter().map(|ty| Node::Type(ty, 0)));
                    pending.extend(
                        extension
                            .methods
                            .iter()
                            .map(|m| Node::Function(&m.function)),
                    );
                }
                Item::Trait(declaration) => {
                    for method in &declaration.methods {
                        pending.extend(method.params.iter().map(Node::Field));
                        if let Some(ty) = &method.result_type {
                            pending.push(Node::Type(ty, 0));
                        }
                    }
                }
                _ => {}
            },
            Node::Function(function) => {
                cost = cost.saturating_add(function.name.len());
                if let Some(owner) = &function.owner {
                    cost = cost.saturating_add(owner.len());
                }
                pending.push(Node::Block(&function.body));
                pending.extend(function.params.iter().map(Node::Field));
                if let Some(ty) = &function.result_type {
                    pending.push(Node::Type(ty, 0));
                }
                for parameter in &function.type_params {
                    for bound in &parameter.bounds {
                        cost = cost.saturating_add(bound.name.len());
                        pending.extend(bound.arguments.iter().map(|ty| Node::Type(ty, 0)));
                    }
                }
            }
            Node::Field(field) => {
                cost = cost.saturating_add(field.name.len());
                if let Some(ty) = &field.ty {
                    pending.push(Node::Type(ty, 0));
                }
                if let Some(value) = &field.default {
                    pending.push(Node::Expr(value));
                }
            }
            Node::Block(block) => {
                pending.extend(block.stmts.iter().map(Node::Stmt));
                if let Some(tail) = &block.tail {
                    pending.push(Node::Expr(tail));
                }
            }
            Node::Stmt(stmt) => match stmt {
                Stmt::Located(_, inner) => pending.push(Node::Stmt(inner)),
                Stmt::VarDecl { pattern, ty, value } | Stmt::Const { pattern, ty, value } => {
                    pending.push(Node::BindingPattern(pattern));
                    if let Some(ty) = ty {
                        pending.push(Node::Type(ty, 0));
                    }
                    pending.push(Node::Expr(value));
                }
                Stmt::Comptime { name, ty, value } => {
                    cost = cost.saturating_add(name.len());
                    if let Some(ty) = ty {
                        pending.push(Node::Type(ty, 0));
                    }
                    pending.push(Node::Expr(value));
                }
                Stmt::Assign { target, value } | Stmt::CompoundAssign { target, value, .. } => {
                    pending.push(Node::Expr(target));
                    pending.push(Node::Expr(value));
                }
                Stmt::Return(Some(value)) | Stmt::Expr(value) | Stmt::Crash { value, .. } => {
                    pending.push(Node::Expr(value))
                }
                Stmt::Defer { body } | Stmt::While { body, .. } => {
                    pending.push(Node::Block(body));
                    if let Stmt::While { cond, .. } = stmt {
                        pending.push(Node::Expr(cond));
                    }
                }
                Stmt::Try {
                    body, catch_block, ..
                } => {
                    pending.push(Node::Block(body));
                    pending.push(Node::Block(catch_block));
                }
                Stmt::If {
                    cond: value,
                    then_block,
                    else_block,
                }
                | Stmt::When {
                    value,
                    then_block,
                    else_block,
                    ..
                } => {
                    pending.push(Node::Expr(value));
                    pending.push(Node::Block(then_block));
                    if let Some(block) = else_block {
                        pending.push(Node::Block(block));
                    }
                }
                Stmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    pending.push(Node::Expr(start));
                    pending.push(Node::Expr(end));
                    pending.push(Node::Block(body));
                    if let Some(step) = step {
                        pending.push(Node::Expr(step));
                    }
                }
                Stmt::ForEach {
                    pattern,
                    iterable,
                    body,
                } => {
                    pending.push(Node::BindingPattern(pattern));
                    pending.push(Node::Expr(iterable));
                    pending.push(Node::Block(body));
                }
                Stmt::ForZip { clauses, body } => {
                    pending.push(Node::Block(body));
                    for clause in clauses {
                        match clause {
                            ForClause::Range {
                                var,
                                start,
                                end,
                                step,
                            } => {
                                cost = cost.saturating_add(var.len());
                                pending.push(Node::Expr(start));
                                pending.push(Node::Expr(end));
                                if let Some(step) = step {
                                    pending.push(Node::Expr(step));
                                }
                            }
                            ForClause::Each { pattern, iterable } => {
                                pending.push(Node::BindingPattern(pattern));
                                pending.push(Node::Expr(iterable));
                            }
                        }
                    }
                }
                _ => {}
            },
            Node::BindingPattern(pattern) => match pattern {
                BindingPattern::Name(name) => cost = cost.saturating_add(name.len()),
                BindingPattern::Tuple(items) => {
                    pending.extend(items.iter().map(Node::BindingPattern));
                }
            },
            Node::Expr(expr) => match expr {
                Expr::Str(text) | Expr::Ident(text) => cost = cost.saturating_add(text.len()),
                Expr::Bytes(bytes) => cost = cost.saturating_add(bytes.len()),
                Expr::TypeLiteral(ty) => pending.push(Node::Type(ty, 0)),
                Expr::TypedAggregate(value, ty) => {
                    pending.push(Node::Expr(value));
                    pending.push(Node::Type(ty, 0));
                }
                Expr::Located(_, inner)
                | Expr::Unary(_, inner)
                | Expr::Propagate(inner)
                | Expr::Cast(inner, _) => pending.push(Node::Expr(inner)),
                Expr::Index(a, b)
                | Expr::Binary(_, a, b)
                | Expr::Fallback {
                    value: a,
                    fallback: b,
                    ..
                } => {
                    pending.push(Node::Expr(a));
                    pending.push(Node::Expr(b));
                }
                Expr::Array(items) | Expr::Tuple(items) => {
                    pending.extend(items.iter().map(Node::Expr))
                }
                Expr::Record(fields) => {
                    for (name, value) in fields {
                        cost = cost.saturating_add(name.len());
                        pending.push(Node::Expr(value));
                    }
                }
                Expr::Call(name, args) | Expr::SpecializedCall(name, _, args) => {
                    cost = cost.saturating_add(name.len());
                    pending.extend(args.iter().map(|a| Node::Expr(&a.value)));
                    if let Expr::SpecializedCall(_, types, _) = expr {
                        pending.extend(types.iter().map(|ty| Node::Type(ty, 0)));
                    }
                }
                Expr::AssociatedCall {
                    target,
                    method,
                    type_arguments,
                    args,
                } => {
                    cost = cost.saturating_add(method.len());
                    pending.push(Node::Type(target, 0));
                    pending.extend(type_arguments.iter().map(|ty| Node::Type(ty, 0)));
                    pending.extend(args.iter().map(|argument| Node::Expr(&argument.value)));
                }
                Expr::BracketApply(receiver, args) | Expr::MethodCall(receiver, _, args) => {
                    pending.push(Node::Expr(receiver));
                    pending.extend(args.iter().map(Node::Expr));
                }
                Expr::Apply(receiver, args) => {
                    pending.push(Node::Expr(receiver));
                    pending.extend(args.iter().map(|a| Node::Expr(&a.value)));
                }
                Expr::Interpolated(parts) => {
                    for part in parts {
                        match part {
                            StringPart::Text(text) => cost = cost.saturating_add(text.len()),
                            StringPart::Expression(value) => pending.push(Node::Expr(value)),
                        }
                    }
                }
                Expr::Block(block) | Expr::Capture(block) => pending.push(Node::Block(block)),
                Expr::Try {
                    body, catch_block, ..
                } => {
                    pending.push(Node::Block(body));
                    pending.push(Node::Block(catch_block));
                }
                Expr::Match { value, arms, .. } => {
                    pending.push(Node::Expr(value));
                    for arm in arms {
                        if let MatchPattern::StringLiteral(pattern) = &arm.pattern {
                            cost = cost.saturating_add(pattern.len());
                        }
                        pending.push(Node::Block(&arm.body));
                        if let Some(guard) = &arm.guard {
                            pending.push(Node::Expr(guard));
                        }
                    }
                }
                Expr::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    pending.push(Node::Expr(cond));
                    pending.push(Node::Expr(then_branch));
                    if let Some(other) = else_branch {
                        pending.push(Node::Expr(other));
                    }
                }
                _ => {}
            },
            Node::Type(ty, depth) => {
                if depth >= 64 {
                    return Err("compiler type depth limit exceeded".into());
                }
                match &ty.kind {
                    TypeKind::Named(name) | TypeKind::TypeParameter(name) => {
                        cost = cost.saturating_add(name.len())
                    }
                    TypeKind::ResolvedName { source, symbol } => {
                        cost = cost.saturating_add(source.len() + symbol.len())
                    }
                    TypeKind::Array(inner) => pending.push(Node::Type(inner, depth + 1)),
                    TypeKind::Tuple(items) => {
                        pending.extend(items.iter().map(|ty| Node::Type(ty, depth + 1)))
                    }
                    TypeKind::Applied { source, arguments } => {
                        cost = cost.saturating_add(source.len());
                        pending.extend(arguments.iter().map(|ty| Node::Type(ty, depth + 1)));
                    }
                    TypeKind::ResolvedApplied {
                        source,
                        symbol,
                        arguments,
                    } => {
                        cost = cost.saturating_add(source.len() + symbol.len());
                        pending.extend(arguments.iter().map(|ty| Node::Type(ty, depth + 1)));
                    }
                    _ => {}
                }
            }
        }
        if pending.exceeded {
            return Err("compiler syntax accounting queue budget exceeded".into());
        }
        if cost > MAX_COST {
            return Err("compiler syntax allocation budget (64 MiB) exceeded".into());
        }
    }
    Ok(cost)
}

#[cfg(test)]
mod tests {
    use super::{Node, measure_with_pending_limit};
    use crate::ast::Expr;

    #[test]
    fn wide_ast_is_rejected_before_its_work_queue_exceeds_the_budget() {
        let expression = Expr::Array(vec![Expr::Bool(false); 3]);
        let error =
            measure_with_pending_limit(std::iter::once(Node::Expr(&expression)), 2).unwrap_err();
        assert_eq!(error, "compiler syntax accounting queue budget exceeded");
    }
}
