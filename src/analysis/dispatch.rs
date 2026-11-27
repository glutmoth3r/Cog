//! Method selection through trait bounds.
//!
//! A call on a value of a bounded type parameter runs the implementation that
//! proved the bound. Instances are checked with concrete types, where a method
//! name alone could select an unrelated extension, so each template is first
//! checked with opaque type parameters. That probe records the calls made
//! through bounds, specialization carries them to the instances, and the
//! instance check selects the method by trait, trait arguments and type.
use super::*;

/// Spells the opaque type standing for a type parameter while its template is
/// probed. Source code cannot name it.
const PARAMETER_MARK: &str = "$parameter::";

/// A use of a trait bound by one expression of a generic function.
#[derive(Clone)]
pub(crate) struct BoundCall {
    /// The receiver type: the type parameter in the template and its
    /// replacement in an instance.
    pub target: TypeAnnotation,
    /// The bounds declaring the called method, or the iteration bounds.
    pub bounds: Vec<TraitBound>,
}

/// Uses of trait bounds, keyed by the identity of the method call or of the
/// iterated expression.
#[derive(Clone, Default)]
pub(crate) struct BoundCalls {
    pub methods: HashMap<usize, BoundCall>,
    pub iterations: HashMap<usize, BoundCall>,
}

/// How a method call selects its callee.
pub(super) enum Dispatch<'a> {
    /// An ordinary call resolves the method name on the receiver type.
    ByName,
    /// The method of the implementation registered for this type.
    Method(Type, &'a ExtensionMethod),
    /// The built-in structural clone proved the `Clone` bound.
    StructuralClone,
    /// A diagnostic has rejected the call.
    Rejected,
}

impl<'a> Checker<'a> {
    /// Checks a template with bounded type parameters for the calls it makes
    /// through those bounds. Nothing else the check produces is kept: concrete
    /// instances report the diagnostics and request further specializations.
    pub(super) fn probe_template(
        &mut self,
        function: &FunctionDecl,
        function_modules: &HashMap<String, String>,
    ) {
        if function.comptime
            || function
                .type_params
                .iter()
                .all(|parameter| parameter.bounds.is_empty())
        {
            return;
        }
        let diagnostics = self.diagnostics.len();
        let symbol_names = std::mem::take(&mut self.access_facts.symbol_names);
        let access_facts = std::mem::replace(
            &mut self.access_facts,
            crate::mutability::Facts {
                symbol_names,
                ..Default::default()
            },
        );
        let globals = self.globals.clone();
        macro_rules! isolated {
            ($($field:ident),* ; $body:block) => {{
                $(let $field = std::mem::take(&mut self.$field);)*
                $body
                $(self.$field = $field;)*
            }};
        }
        isolated!(
            method_calls,
            iteration_calls,
            call_specializations,
            struct_call_specializations,
            specializations,
            specialization_names,
            struct_specializations,
            struct_specialization_names,
            uint_literals,
            uint_expressions,
            bigint_literals,
            bigint_range_expressions,
            literal_types,
            dynamic_bindings,
            unresolved_empty_arrays;
            {
                self.probe = Some(function.type_params.clone());
                self.check_body(function, function_modules);
                self.probe = None;
            }
        );
        let symbol_names = std::mem::take(&mut self.access_facts.symbol_names);
        self.access_facts = access_facts;
        self.access_facts.symbol_names = symbol_names;
        self.globals = globals;
        self.diagnostics.truncate(diagnostics);
    }

    /// The opaque type of a type parameter of the probed template.
    pub(super) fn probed_parameter_type(&self, name: &str) -> Option<Type> {
        self.probe
            .as_ref()?
            .iter()
            .any(|parameter| parameter.name == name)
            .then(|| Type::Named(format!("{PARAMETER_MARK}{name}")))
    }

    fn probed_parameter(&self, ty: &Type) -> Option<&TypeParameter> {
        let Type::Named(symbol) = ty else {
            return None;
        };
        let name = symbol.strip_prefix(PARAMETER_MARK)?;
        self.probe
            .as_ref()?
            .iter()
            .find(|parameter| parameter.name == name)
    }

    /// The trait arguments with which the bounds of a probed type parameter
    /// require `trait_name`. Returns nothing for other types.
    pub(super) fn probed_bound_arguments(
        &self,
        ty: &Type,
        trait_name: &str,
    ) -> Option<Vec<Vec<Type>>> {
        let parameter = self.probed_parameter(ty)?;
        let substitutions: HashMap<_, _> = self
            .probe
            .iter()
            .flatten()
            .filter_map(|parameter| {
                let ty = self.probed_parameter_type(&parameter.name)?;
                Some((parameter.name.clone(), ty))
            })
            .collect();
        Some(
            parameter
                .bounds
                .iter()
                .filter(|bound| bound.name == trait_name)
                .filter_map(|bound| {
                    bound
                        .arguments
                        .iter()
                        .map(|argument| self.concrete_annotation(argument, &substitutions, ty))
                        .collect()
                })
                .collect(),
        )
    }

    /// Records a probed method call whose receiver is a type parameter with a
    /// bound declaring the method, and returns the declared result.
    pub(super) fn probe_method_call(
        &mut self,
        expr: &Expr,
        receiver: &Type,
        method: &str,
        args: &[Expr],
    ) -> Option<Type> {
        // A repeated pass over the body can demote a binding to dynamic, so
        // only the last visit of an expression counts.
        self.probed
            .methods
            .remove(&crate::mutability::expr_id(expr));
        let receiver = receiver.non_nil();
        let parameter = self.probed_parameter(&receiver)?;
        let name = parameter.name.clone();
        let bounds: Vec<_> = parameter
            .bounds
            .iter()
            .filter(|bound| {
                self.traits.get(&bound.name).is_some_and(|declaration| {
                    declaration
                        .methods
                        .iter()
                        .any(|required| required.name == method)
                })
            })
            .cloned()
            .collect();
        let bound = bounds.first()?;
        let declaration = *self.traits.get(&bound.name)?;
        let required = declaration
            .methods
            .iter()
            .find(|required| required.name == method)?;
        let target = TypeAnnotation {
            kind: TypeKind::TypeParameter(name),
            nilable: false,
        };
        let result = required.result_type.clone().map(|mut result| {
            let substitutions = declaration
                .type_params
                .iter()
                .cloned()
                .zip(bound.arguments.iter().cloned())
                .collect();
            substitute_trait_type(&mut result, &substitutions);
            replace_self_type(&mut result, &target);
            result
        });
        let asynchronous = required.asynchronous;
        for argument in args {
            self.expr(argument);
        }
        self.invalidate_call_flow_facts();
        self.probed.methods.insert(
            crate::mutability::expr_id(expr),
            BoundCall { target, bounds },
        );
        let result = result.map_or(Type::Dynamic, |result| self.annotation_type(&result));
        Some(if asynchronous {
            Type::Task(Box::new(result))
        } else {
            result
        })
    }

    /// Records a probed iteration of a type parameter bounded by
    /// `IntoIterator` and returns the item type.
    pub(super) fn probe_iteration(
        &mut self,
        collection: &Type,
        iterable: &Expr,
        into_iterator: &str,
    ) -> Option<Type> {
        self.probed
            .iterations
            .remove(&crate::mutability::expr_id(iterable));
        let name = self.probed_parameter(collection)?.name.clone();
        let arguments = self.probed_bound_arguments(collection, into_iterator)?;
        let item = arguments.first()?.get(1)?.clone();
        let bounds = self
            .probed_parameter(collection)?
            .bounds
            .iter()
            .filter(|bound| bound.name == into_iterator)
            .cloned()
            .collect();
        self.probed.iterations.insert(
            crate::mutability::expr_id(iterable),
            BoundCall {
                target: TypeAnnotation {
                    kind: TypeKind::TypeParameter(name),
                    nilable: false,
                },
                bounds,
            },
        );
        Some(item)
    }

    /// The `IntoIterator` arguments required by the bound through which an
    /// instance iterates this expression.
    pub(super) fn bound_iteration_arguments(&mut self, iterable: &Expr) -> Option<Vec<Vec<Type>>> {
        let calls = self.bound_calls;
        let call = calls
            .iterations
            .get(&crate::mutability::expr_id(iterable))?;
        let mut candidates = Vec::new();
        for bound in &call.bounds {
            let arguments: Vec<_> = bound
                .arguments
                .iter()
                .map(|argument| self.annotation_type(argument))
                .collect();
            if !candidates.contains(&arguments) {
                candidates.push(arguments);
            }
        }
        Some(candidates)
    }

    /// The method of the implementation proving that `ty` implements the trait
    /// with these arguments.
    pub(super) fn implementation_method(
        &self,
        ty: &Type,
        trait_name: &str,
        arguments: &[Type],
        method: &str,
    ) -> Option<&'a ExtensionMethod> {
        let key = (ty.clone(), trait_name.to_owned(), arguments.to_vec());
        let implementation = self.trait_implementations.get(&key).copied().or_else(|| {
            self.generic_trait_implementations
                .iter()
                .copied()
                .find(|extension| {
                    self.generic_trait_implementation_arguments(
                        extension,
                        ty,
                        trait_name,
                        Some(arguments),
                        &mut HashSet::new(),
                    )
                    .is_some()
                })
        })?;
        implementation
            .methods
            .iter()
            .find(|candidate| candidate.name == method)
    }

    /// Selects the callee of a method call that was forced by the compiler or
    /// that goes through a trait bound.
    pub(super) fn dispatch_method(&mut self, expr: &Expr, method: &str) -> Dispatch<'a> {
        let id = crate::mutability::expr_id(expr);
        if let Some((_, ty, selected)) = self.forced_method.take_if(|(forced, ..)| *forced == id) {
            return Dispatch::Method(ty, selected);
        }
        let calls = self.bound_calls;
        let Some(call) = calls.methods.get(&id) else {
            return Dispatch::ByName;
        };
        let target = self.annotation_type(&call.target);
        let mut selected: Vec<(&str, Option<&'a ExtensionMethod>)> = Vec::new();
        let mut labels = Vec::new();
        for bound in &call.bounds {
            let arguments: Vec<_> = bound
                .arguments
                .iter()
                .map(|argument| self.annotation_type(argument))
                .collect();
            let implementation =
                self.implementation_method(&target, &bound.name, &arguments, method);
            let structural = implementation.is_none()
                && bound.name == CLONE_TRAIT
                && method == "clone"
                && self.implements_trait(&target, &bound.name, &arguments);
            if implementation.is_none() && !structural {
                continue;
            }
            let repeated = selected.iter().any(|(previous_trait, previous)| {
                match (previous, implementation) {
                    (Some(previous), Some(current)) => {
                        std::ptr::eq(*previous, current)
                            || equivalent_equality(
                                (previous_trait, previous),
                                (&bound.name, current),
                            )
                    }
                    (None, None) => true,
                    _ => false,
                }
            });
            if !repeated {
                selected.push((&bound.name, implementation));
                labels.push(self.source_bound_name(&bound.name, &arguments));
            }
        }
        match selected.as_slice() {
            // An unsatisfied bound has already been reported.
            [] => Dispatch::ByName,
            [(_, Some(implementation))] => Dispatch::Method(target, implementation),
            [(_, None)] => Dispatch::StructuralClone,
            [_, _, ..] => {
                let (first, second) = (&labels[0], &labels[1]);
                self.diagnostics.push(
                    Diagnostic::new(
                        "E2001",
                        format!(
                            "method `{method}()` is ambiguous: bounds `{first}` and `{second}` both declare it"
                        ),
                        expr.span().cloned().or_else(|| self.location.clone()),
                    )
                    .help(
                        "require only one of these bounds here, or give the trait methods different names",
                    ),
                );
                Dispatch::Rejected
            }
        }
    }

    /// Spells a bound the way the source names its trait, for diagnostics.
    fn source_bound_name(&self, symbol: &str, arguments: &[Type]) -> String {
        let name = self
            .access_facts
            .symbol_names
            .get(symbol)
            .map_or(symbol, String::as_str);
        if arguments.is_empty() {
            name.to_owned()
        } else {
            let arguments: Vec<_> = arguments
                .iter()
                .map(|argument| self.source_type_name(argument))
                .collect();
            format!("{name}[{}]", arguments.join(", "))
        }
    }
}
