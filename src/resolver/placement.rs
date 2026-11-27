//! Where `extend` declarations may appear.
//!
//! A trait implementation belongs to the module that declares the trait or the
//! module that declares the target type. An inherent extension belongs to the
//! module that declares the target type; only the standard library extends
//! built-in types. Every such module is loaded whenever the trait and type are
//! used together, so a method cannot appear or disappear with an unrelated
//! import.
use super::*;

/// Bounds the chain of compile-time type aliases followed to a declaration.
const MAX_ALIAS_DEPTH: usize = 64;

/// The nominal head of an extension target.
#[derive(Clone, Copy)]
enum Head<'a> {
    /// A primitive, array, tuple, function, or task type, which no module
    /// declares.
    Builtin,
    /// A struct or enum symbol.
    Declared(&'a str),
    /// A type alias computed at compile time.
    Computed,
    /// An invalid target, reported elsewhere.
    Unknown,
}

struct Declarations<'a> {
    /// Declaring module and source name of every symbol.
    symbols: BTreeMap<&'a str, (&'a ModuleKey, &'a str)>,
    types: BTreeSet<&'a str>,
    traits: BTreeSet<&'a str>,
    aliases: BTreeMap<&'a str, &'a Expr>,
}

impl<'a> Declarations<'a> {
    /// The declaring module and source name of a nominal head.
    fn type_owner(&self, head: Head<'_>) -> Option<(&'a ModuleKey, &'a str)> {
        match head {
            Head::Declared(symbol) => self.symbols.get(symbol).copied(),
            Head::Builtin | Head::Computed | Head::Unknown => None,
        }
    }

    /// Explains why no module declares a target without a nominal head.
    fn unowned_note(&self, target: &TypeAnnotation, head: Head<'_>) -> String {
        let name = target.display();
        if matches!(head, Head::Computed) {
            format!("the declaring module of the computed type `{name}` is unknown")
        } else if matches!(
            &target.kind,
            TypeKind::ResolvedName { symbol, .. } if self.aliases.contains_key(symbol.as_str())
        ) {
            format!("`{name}` is an alias of a built-in type, which no module declares")
        } else {
            format!("no module declares the built-in type `{name}`")
        }
    }

    fn head(&self, ty: &'a TypeAnnotation, depth: usize) -> Head<'a> {
        match &ty.kind {
            TypeKind::Primitive(_)
            | TypeKind::Bytes
            | TypeKind::Array(_)
            | TypeKind::Tuple(_)
            | TypeKind::Function { .. } => Head::Builtin,
            TypeKind::ResolvedName { symbol, .. } | TypeKind::ResolvedApplied { symbol, .. } => {
                self.symbol(symbol, depth)
            }
            TypeKind::Named(_)
            | TypeKind::Applied { .. }
            | TypeKind::TypeParameter(_)
            | TypeKind::SelfType => Head::Unknown,
        }
    }

    fn symbol(&self, symbol: &'a str, depth: usize) -> Head<'a> {
        if symbol == "Task" {
            Head::Builtin
        } else if self.types.contains(symbol) {
            Head::Declared(symbol)
        } else if depth < MAX_ALIAS_DEPTH
            && let Some(value) = self.aliases.get(symbol)
        {
            match value.node() {
                Expr::TypeLiteral(ty) => self.head(ty, depth + 1),
                Expr::Ident(name) if self.symbols.contains_key(name.as_str()) => {
                    self.symbol(name, depth + 1)
                }
                Expr::Ident(_) => Head::Builtin,
                _ => Head::Computed,
            }
        } else {
            Head::Unknown
        }
    }
}

impl Resolver {
    /// Reports extensions declared outside the modules that may declare them.
    pub(super) fn check_extension_placement(&mut self) {
        let mut declarations = Declarations {
            symbols: self
                .symbols
                .iter()
                .map(|((_, name), symbol)| (symbol.bound.as_str(), (&symbol.owner, name.as_str())))
                .collect(),
            types: BTreeSet::new(),
            traits: BTreeSet::new(),
            aliases: BTreeMap::new(),
        };
        for item in &self.output {
            match item {
                Item::StructDecl { name, .. } | Item::EnumDecl { name, .. } => {
                    declarations.types.insert(name);
                }
                Item::Trait(declaration) => {
                    declarations.traits.insert(&declaration.name);
                }
                Item::Comptime {
                    name,
                    value,
                    ty: Some(ty),
                    ..
                } if matches!(&ty.kind, TypeKind::Named(kind) if kind == "type") => {
                    declarations.aliases.insert(name, value);
                }
                _ => {}
            }
        }
        let mut diagnostics = Vec::new();
        for (owner, index) in &self.extensions {
            let Item::Extend(extension) = &self.output[*index] else {
                continue;
            };
            let head = declarations.head(&extension.target, 0);
            if matches!(head, Head::Unknown) {
                continue;
            }
            let diagnostic = match &extension.trait_name {
                None => self.misplaced_inherent_extension(owner, extension, head, &declarations),
                Some(symbol) => {
                    self.misplaced_implementation(owner, extension, symbol, head, &declarations)
                }
            };
            diagnostics.extend(diagnostic);
        }
        self.diagnostics.extend(diagnostics);
    }

    fn misplaced_inherent_extension(
        &self,
        owner: &ModuleKey,
        extension: &ExtendDecl,
        head: Head<'_>,
        declarations: &Declarations<'_>,
    ) -> Option<Diagnostic> {
        let type_owner = declarations.type_owner(head);
        let allowed = match head {
            Head::Builtin => owner.0 == CORE,
            _ => type_owner.is_some_and(|(module, _)| module == owner),
        };
        if allowed {
            return None;
        }
        let target = extension.target.display();
        let (message, note) = match type_owner {
            Some((module, name)) => (
                format!(
                    "inherent methods of `{target}` can be declared only in the module that declares `{name}`"
                ),
                format!("`{name}` is declared in {}", self.describe_module(module)),
            ),
            None => (
                format!("only the standard library can declare inherent methods of `{target}`"),
                declarations.unowned_note(&extension.target, head),
            ),
        };
        let mut diagnostic =
            Diagnostic::new("E2001", message, extension_span(extension)).note(note);
        if let Some(method) = extension.methods.first() {
            let parameters: Vec<_> = extension
                .type_params
                .iter()
                .map(|parameter| format!("{}: type", parameter.name))
                .collect();
            let generics = if parameters.is_empty() {
                String::new()
            } else {
                format!("[{}]", parameters.join(", "))
            };
            let receiver = if method
                .function
                .params
                .first()
                .is_some_and(|parameter| parameter.name == "self")
            {
                format!("value: {target}")
            } else {
                String::new()
            };
            diagnostic = diagnostic.help(format!(
                "declare a free function instead, such as `fn {}{generics}({receiver})`",
                method.name
            ));
        }
        Some(diagnostic)
    }

    fn misplaced_implementation(
        &self,
        owner: &ModuleKey,
        extension: &ExtendDecl,
        trait_symbol: &str,
        head: Head<'_>,
        declarations: &Declarations<'_>,
    ) -> Option<Diagnostic> {
        if !declarations.traits.contains(trait_symbol) {
            return None;
        }
        let &(trait_owner, trait_source) = declarations.symbols.get(trait_symbol)?;
        let type_owner = declarations.type_owner(head);
        if trait_owner == owner || type_owner.is_some_and(|(module, _)| module == owner) {
            return None;
        }
        let target = extension.target.display();
        let diagnostic = Diagnostic::new(
            "E2001",
            format!(
                "trait `{trait_source}` can be implemented for `{target}` only in the module that declares the trait or the type"
            ),
            extension_span(extension),
        )
        .note(format!(
            "`{trait_source}` is declared in {}",
            self.describe_module(trait_owner)
        ));
        let diagnostic = diagnostic.note(match type_owner {
            Some((module, name)) => {
                format!("`{name}` is declared in {}", self.describe_module(module))
            }
            None => declarations.unowned_note(&extension.target, head),
        });
        let wrapper =
            "wrap the value in a struct declared in this module and implement the trait for it";
        let mut homes = Vec::new();
        if trait_owner.0 != CORE {
            homes.push(self.describe_module(trait_owner));
        }
        if let Some((module, _)) = type_owner
            && module.0 != CORE
        {
            homes.push(self.describe_module(module));
        }
        Some(diagnostic.help(if homes.is_empty() {
            wrapper.to_owned()
        } else {
            format!(
                "move the implementation to {}, or {wrapper}",
                homes.join(" or ")
            )
        }))
    }

    fn describe_module(&self, (package, path): &ModuleKey) -> String {
        if *package == CORE {
            return format!("standard module `{path}`");
        }
        let alias = self
            .aliases
            .iter()
            .find_map(|((_, alias), target)| (target == package).then_some(alias));
        match (path.is_empty(), alias) {
            (true, None) => "the entry module".to_owned(),
            (true, Some(alias)) => format!("the root module of package `{alias}`"),
            (false, None) => format!("module `{path}`"),
            (false, Some(alias)) => format!("module `{path}` of package `{alias}`"),
        }
    }
}

/// An extension has no span of its own; its first method locates it.
fn extension_span(extension: &ExtendDecl) -> Option<SourceSpan> {
    extension.methods.first().and_then(|method| {
        let function = &method.function;
        function
            .result_span
            .clone()
            .or_else(|| function.body.stmts.first().and_then(Stmt::span).cloned())
            .or_else(|| function.body.tail.as_deref().and_then(Expr::span).cloned())
    })
}
