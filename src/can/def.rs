use crate::can::annotation::{canonicalize_annotation, Annotation};
use crate::can::env::Env;
use crate::can::expr::Expr::{self, *};
use crate::can::expr::{
    canonicalize_expr, local_successors, references_from_call, references_from_local, Output,
    Recursive,
};
use crate::can::ident::{Ident, Lowercase};
use crate::can::pattern::PatternType;
use crate::can::pattern::{canonicalize_pattern, Pattern};
use crate::can::problem::Problem;
use crate::can::problem::RuntimeError;
use crate::can::procedure::References;
use crate::can::scope::Scope;
use crate::collections::{default_hasher, ImSet, MutMap, MutSet, SendMap};
use crate::graph::{strongly_connected_components, topological_sort_into_groups};
use crate::module::symbol::Symbol;
use crate::parse::ast;
use crate::region::{Located, Region};
use crate::subs::{VarStore, Variable};
use crate::types::Type;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub loc_pattern: Located<Pattern>,
    pub loc_expr: Located<Expr>,
    pub expr_var: Variable,
    pub pattern_vars: SendMap<Symbol, Variable>,
    pub annotation: Option<(Type, SendMap<Variable, Lowercase>)>,
}

#[derive(Debug)]
pub struct CanDefs {
    // TODO don't store the Ident in here (lots of cloning!) - instead,
    // make refs_by_symbol be something like MutMap<Symbol, (Region, References)>
    pub refs_by_symbol: MutMap<Symbol, (Located<Ident>, References)>,
    pub can_defs_by_symbol: MutMap<Symbol, Def>,
    pub idents_introduced: MutMap<Ident, (Symbol, Region)>,
}
/// A Def that has had patterns and type annnotations canonicalized,
/// but no Expr canonicalization has happened yet. Also, it has had spaces
/// and nesting resolved, and knows whether annotations are standalone or not.
#[derive(Debug, Clone, PartialEq)]
enum PendingDef<'a> {
    /// A standalone annotation with no body
    AnnotationOnly(
        &'a Located<ast::Pattern<'a>>,
        Located<Pattern>,
        Located<Annotation>,
    ),
    /// A body with no type annotation
    Body(
        &'a Located<ast::Pattern<'a>>,
        Located<Pattern>,
        &'a Located<ast::Expr<'a>>,
    ),
    /// A body with a type annotation
    TypedBody(
        &'a Located<ast::Pattern<'a>>,
        Located<Pattern>,
        Located<Annotation>,
        &'a Located<ast::Expr<'a>>,
    ),
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Declaration {
    Declare(Def),
    DeclareRec(Vec<Def>),
    InvalidCycle(
        Vec<Located<Ident>>,
        Vec<(Region /* pattern */, Region /* expr */)>,
    ),
}

impl Declaration {
    pub fn def_count(&self) -> usize {
        use Declaration::*;
        match self {
            Declare(_) => 1,
            DeclareRec(defs) => defs.len(),
            InvalidCycle(_, _) => 0,
        }
    }
}

#[inline(always)]
pub fn canonicalize_defs<'a>(
    env: &mut Env<'a>,
    found_rigids: &mut SendMap<Variable, Lowercase>,
    var_store: &VarStore,
    original_scope: &mut Scope,
    loc_defs: &'a bumpalo::collections::Vec<'a, &'a Located<ast::Def<'a>>>,
    pattern_type: PatternType,
) -> CanDefs {
    // Canonicalizing defs while detecting shadowing involves a multi-step process:
    //
    // 1. Go through each of the patterns.
    // 2. For each identifier pattern, get the scope.symbol() for the ident. (That symbol will use the home module for its module.)
    // 3. If that symbol is already in scope, then we're about to shadow it. Error!
    // 4. Otherwise, add it to the scope immediately, so we can detect shadowing within the same
    //    pattern (e.g. (Foo a a) = ...)
    // 5. Add this canonicalized pattern and its corresponding ast::Expr to pending_exprs.
    // 5. Once every pattern has been processed and added to scope, go back and canonicalize the exprs from
    //    pending_exprs, this time building up a canonical def for each one.
    //
    // This way, whenever any expr is doing lookups, it knows everything that's in scope -
    // even defs that appear after it in the source.
    //
    // This naturally handles recursion too, because a given exper which refers
    // to itself won't be processed until after its def has been added to scope.

    use crate::parse::ast::Def::*;

    // Record both the original and final idents from the scope,
    // so we can diff them while detecting unused defs.
    let mut scope = original_scope.clone();
    let original_idents = {
        let mut set = HashSet::with_capacity_and_hasher(scope.num_idents(), default_hasher());

        for (ident, _) in original_scope.idents() {
            set.insert(ident);
        }

        set
    };
    let num_defs = loc_defs.len();
    let mut refs_by_symbol = MutMap::default();
    let mut can_defs_by_symbol = HashMap::with_capacity_and_hasher(num_defs, default_hasher());
    let mut pending = Vec::with_capacity(num_defs); // TODO bump allocate this!
    let mut iter = loc_defs.iter().peekable();

    // Canonicalize all the patterns, record shadowing problems, and store
    // the ast::Expr values in pending_exprs for further canonicalization
    // once we've finished assembling the entire scope.
    while let Some(loc_def) = iter.next() {
        // Any time we have an Annotation followed immediately by a Body,
        // check to see if their patterns are equivalent. If they are,
        // turn it into a TypedBody. Otherwies, give an error.
        let pending_def = match &loc_def.value {
            Annotation(pattern, annotation) | Nested(Annotation(pattern, annotation)) => {
                match iter.peek() {
                    Some(Located {
                        value: Body(body_pattern, body_expr),
                        ..
                    }) => {
                        if pattern.value.equivalent(&body_pattern.value) {
                            iter.next();

                            pending_typed_body(
                                env,
                                body_pattern,
                                annotation,
                                body_expr,
                                var_store,
                                &mut scope,
                                pattern_type,
                            )
                        } else {
                            panic!("TODO gracefully handle the case where a type annotation appears immediately before a body def, but the patterns are different. This should be an error; put a newline or comment between them!");
                        }
                    }
                    _ => to_pending_def(env, var_store, &loc_def.value, &mut scope, pattern_type),
                }
            }
            _ => to_pending_def(env, var_store, &loc_def.value, &mut scope, pattern_type),
        };

        // Record the ast::Expr for later. We'll do another pass through these
        // once we have the entire scope assembled. If we were to canonicalize
        // the exprs right now, they wouldn't have symbols in scope from defs
        // that get would have gotten added later in the defs list!
        pending.push(pending_def);
    }

    // Now that we have the scope completely assembled, and shadowing resolved,
    // we're ready to canonicalize any body exprs.
    for pending_def in pending.into_iter() {
        canonicalize_pending_def(
            env,
            found_rigids,
            pending_def,
            &original_scope,
            &mut scope,
            &mut can_defs_by_symbol,
            var_store,
            &mut refs_by_symbol,
        );

        // TODO we should do something with these references; they include
        // things like type annotations.
    }

    // Determine which idents we introduced in the course of this process.
    let idents_introduced = {
        let mut map = HashMap::with_capacity_and_hasher(
            scope.num_idents() - original_idents.len(),
            default_hasher(),
        );

        for (ident, value) in scope.idents() {
            if !original_idents.contains(ident) {
                map.insert(ident.clone(), value.clone());
            }
        }

        map
    };

    CanDefs {
        idents_introduced,
        refs_by_symbol,
        can_defs_by_symbol,
    }
}

#[inline(always)]
pub fn sort_can_defs(
    env: &mut Env<'_>,
    defs: CanDefs,
    mut output: Output,
) -> (Result<Vec<Declaration>, RuntimeError>, Output) {
    let CanDefs {
        idents_introduced,
        refs_by_symbol,
        can_defs_by_symbol,
    } = defs;

    // Determine the full set of references by traversing the graph.
    let mut visited_symbols = MutSet::default();

    let returned_lookups = ImSet::clone(&output.references.lookups);

    // Start with the return expression's referenced locals. They're the only ones that count!
    //
    // If I have two defs which reference each other, but neither of them is referenced
    // in the return expression, I don't want either of them (or their references) to end up
    // in the final output.references. They were unused, and so were their references!
    //
    // The reason we need a graph here is so we don't overlook transitive dependencies.
    // For example, if I have `a = b + 1` and the def returns `a + 1`, then the
    // def as a whole references both `a` *and* `b`, even though it doesn't
    // directly mention `b` - because `a` depends on `b`. If we didn't traverse a graph here,
    // we'd erroneously give a warning that `b` was unused since it wasn't directly referenced.
    for symbol in returned_lookups.into_iter() {
        // We only care about local symbols in this analysis.
        if symbol.module_id() == env.home {
            // Traverse the graph and look up *all* the references for this local symbol.
            let refs =
                references_from_local(symbol, &mut visited_symbols, &refs_by_symbol, &env.closures);

            output.references = output.references.union(refs);
        }
    }

    for symbol in ImSet::clone(&output.references.calls).into_iter() {
        // Traverse the graph and look up *all* the references for this call.
        // Reuse the same visited_symbols as before; if we already visited it,
        // we won't learn anything new from visiting it again!
        let refs =
            references_from_call(symbol, &mut visited_symbols, &refs_by_symbol, &env.closures);

        output.references = output.references.union(refs);
    }

    // Now that we've collected all the references, check to see if any of the new idents
    // we defined went unused by the return expression. If any were unused, report it.
    for (ident, (symbol, region)) in idents_introduced {
        if !output.references.has_lookup(&symbol) {
            let loc_ident = Located {
                region,
                value: ident.clone(),
            };

            env.problem(Problem::UnusedDef(loc_ident));
        }
    }

    let mut defined_symbols: Vec<Symbol> = Vec::new();
    let mut defined_symbols_set: ImSet<Symbol> = ImSet::default();

    for symbol in can_defs_by_symbol.keys().into_iter() {
        defined_symbols.push(symbol.clone());
        defined_symbols_set.insert(symbol.clone());
    }

    // Use topological sort to reorder the defs based on their dependencies to one another.
    // This way, during code gen, no def will refer to a value that hasn't been initialized yet.
    // As a bonus, the topological sort also reveals any cycles between the defs, allowing
    // us to give a CircularAssignment error for invalid (mutual) recursion, and a `DeclareRec` for mutually
    // recursive definitions.

    // All successors that occur in the body of a symbol.
    let all_successors_without_self = |symbol: &Symbol| -> ImSet<Symbol> {
        // This may not be in refs_by_symbol. For example, the `f` in `f x` here:
        //
        // f = \z -> z
        //
        // (\x ->
        //     a = f x
        //     x
        // )
        //
        // It's not part of the current defs (the one with `a = f x`); rather,
        // it's in the enclosing scope. It's still referenced though, so successors
        // will receive it as an argument!
        match refs_by_symbol.get(symbol) {
            Some((_, references)) => {
                // We can only sort the symbols at the current level. That is safe because
                // symbols defined at higher levels cannot refer to symbols at lower levels.
                // Therefore they can never form a cycle!
                //
                // In the above example, `f` cannot reference `a`, and in the closure
                // a call to `f` cannot cycle back to `a`.
                let mut loc_succ = local_successors(&references, &env.closures);

                // if the current symbol is a closure, peek into its body
                if let Some(References { lookups, .. }) = env.closures.get(symbol) {
                    let home = env.home;

                    for lookup in lookups {
                        if lookup != symbol && lookup.module_id() == home {
                            // DO NOT register a self-call behind a lambda!
                            //
                            // We allow `boom = \_ -> boom {}`, but not `x = x`
                            loc_succ.insert(*lookup);
                        }
                    }
                }

                // remove anything that is not defined in the current block
                loc_succ.retain(|key| defined_symbols_set.contains(key));

                loc_succ
            }
            None => ImSet::default(),
        }
    };

    // All successors that occur in the body of a symbol, including the symbol itself
    // This is required to determine whether a symbol is recursive. Recursive symbols
    // (that are not faulty) always need a DeclareRec, even if there is just one symbol in the
    // group
    let mut all_successors_with_self = |symbol: &Symbol| -> ImSet<Symbol> {
        // This may not be in refs_by_symbol. For example, the `f` in `f x` here:
        //
        // f = \z -> z
        //
        // (\x ->
        //     a = f x
        //     x
        // )
        //
        // It's not part of the current defs (the one with `a = f x`); rather,
        // it's in the enclosing scope. It's still referenced though, so successors
        // will receive it as an argument!
        match refs_by_symbol.get(symbol) {
            Some((_, references)) => {
                // We can only sort the symbols at the current level. That is safe because
                // symbols defined at higher levels cannot refer to symbols at lower levels.
                // Therefore they can never form a cycle!
                //
                // In the above example, `f` cannot reference `a`, and in the closure
                // a call to `f` cannot cycle back to `a`.
                let mut loc_succ = local_successors(&references, &env.closures);

                // if the current symbol is a closure, peek into its body
                if let Some(References { lookups, .. }) = env.closures.get(symbol) {
                    for lookup in lookups {
                        loc_succ.insert(*lookup);
                    }
                }

                // remove anything that is not defined in the current block
                loc_succ.retain(|key| defined_symbols_set.contains(key));

                loc_succ
            }
            None => ImSet::default(),
        }
    };

    // If a symbol is a direct successor of itself, there is an invalid cycle.
    // The difference with the function above is that this one does not look behind lambdas,
    // but does consider direct self-recursion.
    let direct_successors = |symbol: &Symbol| -> ImSet<Symbol> {
        match refs_by_symbol.get(symbol) {
            Some((_, references)) => {
                let mut loc_succ = local_successors(&references, &env.closures);

                // NOTE: if the symbol is a closure we DONT look into its body

                // remove anything that is not defined in the current block
                loc_succ.retain(|key| defined_symbols_set.contains(key));

                // NOTE: direct recursion does matter here: `x = x` is invalid recursion!

                loc_succ
            }
            None => ImSet::default(),
        }
    };

    // TODO also do the same `addDirects` check elm/compiler does, so we can
    // report an error if a recursive definition can't possibly terminate!
    match topological_sort_into_groups(defined_symbols.as_slice(), all_successors_without_self) {
        Ok(groups) => {
            let mut declarations = Vec::new();

            // groups are in reversed order
            for group in groups.into_iter().rev() {
                group_to_declaration(
                    group,
                    &env.closures,
                    &mut all_successors_with_self,
                    &can_defs_by_symbol,
                    &mut declarations,
                );
            }

            (Ok(declarations), output)
        }
        Err((groups, nodes_in_cycle)) => {
            let mut declarations = Vec::new();
            let mut problems = Vec::new();

            // groups are in reversed order
            for group in groups.into_iter().rev() {
                group_to_declaration(
                    group,
                    &env.closures,
                    &mut all_successors_with_self,
                    &can_defs_by_symbol,
                    &mut declarations,
                );
            }

            // nodes_in_cycle are symbols that form a syntactic cycle. That isn't always a problem,
            // and in general it's impossible to decide whether it is. So we use a crude heuristic:
            //
            // Definitions where the cycle occurs behind a lambda are OK
            //
            // boom = \_ -> boom {}
            //
            // But otherwise we report an error, e.g.
            //
            // foo = if b then foo else bar

            for cycle in strongly_connected_components(&nodes_in_cycle, all_successors_without_self)
            {
                // check whether the cycle is faulty, which is when has a direct successor in the
                // current cycle. This catches things like:
                //
                // x = x
                //
                // or
                //
                // p = q
                // q = p
                let is_invalid_cycle = cycle
                    .get(0)
                    .map(|symbol| {
                        let mut succs = direct_successors(symbol);
                        succs.retain(|key| cycle.contains(key));
                        !succs.is_empty()
                    })
                    .unwrap_or(false);

                if is_invalid_cycle {
                    // We want to show the entire cycle in the error message, so expand it out.
                    let mut loc_idents_in_cycle: Vec<Located<Ident>> = Vec::new();
                    for symbol in cycle {
                        let refs = refs_by_symbol.get(&symbol).unwrap_or_else(|| {
                            panic!(
                            "Symbol not found in refs_by_symbol: {:?} - refs_by_symbol was: {:?}",
                            symbol, refs_by_symbol
                        )
                        });

                        loc_idents_in_cycle.push(refs.0.clone());
                    }

                    // Sort them to make the report more helpful.
                    loc_idents_in_cycle.sort();

                    problems.push(Problem::CircularDef(loc_idents_in_cycle.clone()));

                    let mut regions = Vec::with_capacity(can_defs_by_symbol.len());
                    for def in can_defs_by_symbol.values() {
                        regions.push((def.loc_pattern.region, def.loc_expr.region));
                    }

                    declarations.push(Declaration::InvalidCycle(loc_idents_in_cycle, regions));
                } else {
                    // slightly inefficient, because we know this becomes exactly one DeclareRec already
                    group_to_declaration(
                        cycle,
                        &env.closures,
                        &mut all_successors_with_self,
                        &can_defs_by_symbol,
                        &mut declarations,
                    );
                }
            }

            for problem in problems {
                env.problem(problem);
            }

            (Ok(declarations), output)
        }
    }
}

fn group_to_declaration(
    group: Vec<Symbol>,
    closures: &MutMap<Symbol, References>,
    successors: &mut dyn FnMut(&Symbol) -> ImSet<Symbol>,
    can_defs_by_symbol: &MutMap<Symbol, Def>,
    declarations: &mut Vec<Declaration>,
) {
    use Declaration::*;

    // We want only successors in the current group, otherwise definitions get duplicated
    let filtered_successors = |symbol: &Symbol| -> ImSet<Symbol> {
        let mut result = successors(symbol);
        result.retain(|key| group.contains(key));
        result
    };

    // patterns like
    //
    // { x, y } = someDef
    //
    // Can Bind multiple symbols, when not incorrectly recursive (which is guaranteed in this function)
    // normally `someDef` would be inserted twice. We use the region of the pattern as a unique key
    // for a definition, so every definition is only inserted (thus typechecked, code-gen'd) once
    let mut seen_pattern_regions: ImSet<Region> = ImSet::default();

    for cycle in strongly_connected_components(&group, filtered_successors) {
        if cycle.len() == 1 {
            let symbol = &cycle[0];
            if let Some(can_def) = can_defs_by_symbol.get(&symbol) {
                let mut new_def = can_def.clone();

                // Determine recursivity of closures that are not tail-recursive
                if let Closure(fn_var, name, Recursive::NotRecursive, args, body) =
                    new_def.loc_expr.value
                {
                    let recursion = closure_recursivity(symbol.clone(), closures);

                    new_def.loc_expr.value = Closure(fn_var, name, recursion, args, body);
                }

                let is_recursive = successors(&symbol).contains(&symbol);

                if !seen_pattern_regions.contains(&new_def.loc_pattern.region) {
                    if is_recursive {
                        declarations.push(DeclareRec(vec![new_def.clone()]));
                    } else {
                        declarations.push(Declare(new_def.clone()));
                    }
                }
                seen_pattern_regions.insert(new_def.loc_pattern.region);
            }
        } else {
            let mut can_defs = Vec::new();
            // Topological sort gives us the reverse of the sorting we want!
            for symbol in cycle.into_iter().rev() {
                if let Some(can_def) = can_defs_by_symbol.get(&symbol) {
                    let mut new_def = can_def.clone();

                    // Determine recursivity of closures that are not tail-recursive
                    if let Closure(fn_var, name, Recursive::NotRecursive, args, body) =
                        new_def.loc_expr.value
                    {
                        let recursion = closure_recursivity(symbol.clone(), closures);

                        new_def.loc_expr.value = Closure(fn_var, name, recursion, args, body);
                    }

                    if !seen_pattern_regions.contains(&new_def.loc_pattern.region) {
                        can_defs.push(new_def.clone());
                    }
                    seen_pattern_regions.insert(new_def.loc_pattern.region);
                }
            }
            declarations.push(DeclareRec(can_defs));
        }
    }
}

fn canonicalize_pending_def<'a>(
    env: &mut Env<'a>,
    found_rigids: &mut SendMap<Variable, Lowercase>,
    pending_def: PendingDef<'a>,
    original_scope: &Scope,
    scope: &mut Scope,
    can_defs_by_symbol: &mut MutMap<Symbol, Def>,
    var_store: &VarStore,
    refs_by_symbol: &mut MutMap<Symbol, (Located<Ident>, References)>,
) {
    use PendingDef::*;

    // Make types for the body expr, even if we won't end up having a body.
    let expr_var = var_store.fresh();
    let mut vars_by_symbol = SendMap::default();

    match pending_def {
        AnnotationOnly(_, loc_can_pattern, loc_ann) => {
            // TODO we have ann.references here, which includes information about
            // which symbols were referenced in type annotations, but we never
            // use them. We discard them!
            let ann = loc_ann.value;

            // union seen rigids with already found ones
            for (k, v) in ann.ftv {
                found_rigids.insert(k, v);
            }

            let typ = ann.typ;
            let arity = typ.arity();

            // Fabricate a body for this annotation, that will error at runtime
            let value = Expr::RuntimeError(RuntimeError::NoImplementation);
            let is_closure = arity > 0;
            let loc_can_expr = if !is_closure {
                Located {
                    value,
                    region: loc_ann.region,
                }
            } else {
                let symbol = env.gen_unique_symbol();

                // generate a fake pattern for each argument. this makes signatures
                // that are functions only crash when they are applied.
                let mut underscores = Vec::with_capacity(arity);

                for _ in 0..arity {
                    let underscore: Located<Pattern> = Located {
                        value: Pattern::Underscore,
                        region: Region::zero(),
                    };

                    underscores.push((var_store.fresh(), underscore));
                }

                let body_expr = Located {
                    value,
                    region: loc_ann.region,
                };

                let body = Box::new((body_expr, var_store.fresh()));

                Located {
                    value: Closure(
                        var_store.fresh(),
                        symbol,
                        Recursive::NotRecursive,
                        underscores,
                        body,
                    ),
                    region: loc_ann.region,
                }
            };

            for (ident, (symbol, _)) in scope.idents() {
                // TODO Could we do this by symbol instead, to avoid cloning idents?
                if original_scope.contains_ident(ident) {
                    continue;
                }

                // We could potentially avoid some clones here by using Rc strategically,
                // but the total amount of cloning going on here should typically be minimal.
                can_defs_by_symbol.insert(
                    *symbol,
                    Def {
                        expr_var,
                        // TODO try to remove this .clone()!
                        loc_pattern: loc_can_pattern.clone(),
                        loc_expr: Located {
                            region: loc_can_expr.region,
                            // TODO try to remove this .clone()!
                            value: loc_can_expr.value.clone(),
                        },
                        pattern_vars: im::HashMap::clone(&vars_by_symbol),
                        annotation: Some((typ.clone(), found_rigids.clone())),
                    },
                );
            }
        }
        TypedBody(loc_pattern, loc_can_pattern, loc_annotation, loc_expr) => {
            // TODO we have ann.references here, which includes information about
            // which symbols were referenced in type annotations, but we never
            // use them. We discard them!
            let ann = loc_annotation.value;
            let typ = ann.typ;

            // union seen rigids with already found ones
            for (k, v) in ann.ftv {
                found_rigids.insert(k, v);
            }

            // bookkeeping for tail-call detection. If we're assigning to an
            // identifier (e.g. `f = \x -> ...`), then this symbol can be tail-called.
            let outer_identifier = env.tailcallable_symbol.clone();

            if let &Pattern::Identifier(ref defined_symbol) = &loc_can_pattern.value {
                env.tailcallable_symbol = Some(defined_symbol.clone());
                vars_by_symbol.insert(defined_symbol.clone(), expr_var);
            };

            let (mut loc_can_expr, can_output) =
                canonicalize_expr(env, var_store, scope, loc_expr.region, &loc_expr.value);

            // reset the tailcallable_symbol
            env.tailcallable_symbol = outer_identifier;

            // see below: a closure needs a fresh References!
            let mut is_closure = false;

            // First, make sure we are actually assigning an identifier instead of (for example) a tag.
            //
            // If we're assigning (UserId userId) = ... then this is certainly not a closure declaration,
            // which also implies it's not a self tail call!
            //
            // Only defs of the form (foo = ...) can be closure declarations or self tail calls.
            if let (
                &ast::Pattern::Identifier(ref _name),
                &Pattern::Identifier(ref defined_symbol),
                &Closure(fn_var, ref symbol, _, ref arguments, ref body),
            ) = (
                &loc_pattern.value,
                &loc_can_pattern.value,
                &loc_can_expr.value.clone(),
            ) {
                is_closure = true;

                // Since everywhere in the code it'll be referred to by its defined name,
                // remove its generated name from the closure map. (We'll re-insert it later.)
                let references = env.closures.remove(&symbol).unwrap_or_else(|| {
                    panic!(
                        "Tried to remove symbol {:?} from procedures, but it was not found: {:?}",
                        symbol, env.closures
                    )
                });

                // Re-insert the closure into the map, under its defined name.
                // closures don't have a name, and therefore pick a fresh symbol. But in this
                // case, the closure has a proper name (e.g. `foo` in `foo = \x y -> ...`
                // and we want to reference it by that name.
                env.closures.insert(defined_symbol.clone(), references);

                // The closure is self tail recursive iff it tail calls itself (by defined name).
                let is_recursive = match can_output.tail_call {
                    Some(ref symbol) if symbol == defined_symbol => Recursive::TailRecursive,
                    _ => Recursive::NotRecursive,
                };

                // Recursion doesn't count as referencing. (If it did, all recursive functions
                // would result in circular def errors!)
                refs_by_symbol
                    .entry(defined_symbol.clone())
                    .and_modify(|(_, refs)| {
                        refs.lookups = refs.lookups.without(defined_symbol);
                    });

                // renamed_closure_def = Some(&defined_symbol);
                loc_can_expr.value = Closure(
                    fn_var,
                    symbol.clone(),
                    is_recursive,
                    arguments.clone(),
                    body.clone(),
                );
            }

            // Store the referenced locals in the refs_by_symbol map, so we can later figure out
            // which defined names reference each other.
            for (ident, (symbol, region)) in scope.idents() {
                if original_scope.contains_ident(ident) {
                    continue;
                }

                let refs =
                    // Functions' references don't count in defs.
                    // See 3d5a2560057d7f25813112dfa5309956c0f9e6a9 and its
                    // parent commit for the bug this fixed!
                    if is_closure {
                        References::new()
                    } else {
                        can_output.references.clone()
                    };

                refs_by_symbol.insert(
                    symbol.clone(),
                    (
                        Located {
                            value: ident.clone(),
                            region: region.clone(),
                        },
                        refs,
                    ),
                );

                can_defs_by_symbol.insert(
                    *symbol,
                    Def {
                        expr_var,
                        // TODO try to remove this .clone()!
                        loc_pattern: loc_can_pattern.clone(),
                        loc_expr: Located {
                            region: loc_can_expr.region,
                            // TODO try to remove this .clone()!
                            value: loc_can_expr.value.clone(),
                        },
                        pattern_vars: im::HashMap::clone(&vars_by_symbol),
                        annotation: Some((typ.clone(), found_rigids.clone())),
                    },
                );
            }
        }
        // If we have a pattern, then the def has a body (that is, it's not a
        // standalone annotation), so we need to canonicalize the pattern and expr.
        Body(loc_pattern, loc_can_pattern, loc_expr) => {
            // bookkeeping for tail-call detection. If we're assigning to an
            // identifier (e.g. `f = \x -> ...`), then this symbol can be tail-called.
            let outer_identifier = env.tailcallable_symbol.clone();

            if let (
                &ast::Pattern::Identifier(ref _name),
                &Pattern::Identifier(ref defined_symbol),
            ) = (&loc_pattern.value, &loc_can_pattern.value)
            {
                env.tailcallable_symbol = Some(defined_symbol.clone());

                // TODO isn't types_by_symbol enough? Do we need vars_by_symbol too?
                vars_by_symbol.insert(defined_symbol.clone(), expr_var);
            };

            let (mut loc_can_expr, can_output) =
                canonicalize_expr(env, var_store, scope, loc_expr.region, &loc_expr.value);

            // reset the tailcallable_symbol
            env.tailcallable_symbol = outer_identifier;

            // see below: a closure needs a fresh References!
            let mut is_closure = false;

            // First, make sure we are actually assigning an identifier instead of (for example) a tag.
            //
            // If we're assigning (UserId userId) = ... then this is certainly not a closure declaration,
            // which also implies it's not a self tail call!
            //
            // Only defs of the form (foo = ...) can be closure declarations or self tail calls.
            if let (
                &ast::Pattern::Identifier(ref _name),
                &Pattern::Identifier(ref defined_symbol),
                &Closure(fn_var, ref symbol, _, ref arguments, ref body),
            ) = (
                &loc_pattern.value,
                &loc_can_pattern.value,
                &loc_can_expr.value.clone(),
            ) {
                is_closure = true;

                // Since everywhere in the code it'll be referred to by its defined name,
                // remove its generated name from the closure map. (We'll re-insert it later.)
                let references = env.closures.remove(&symbol).unwrap_or_else(|| {
                    panic!(
                        "Tried to remove symbol {:?} from procedures, but it was not found: {:?}",
                        symbol, env.closures
                    )
                });

                // Re-insert the closure into the map, under its defined name.
                // closures don't have a name, and therefore pick a fresh symbol. But in this
                // case, the closure has a proper name (e.g. `foo` in `foo = \x y -> ...`
                // and we want to reference it by that name.
                env.closures.insert(defined_symbol.clone(), references);

                // The closure is self tail recursive iff it tail calls itself (by defined name).
                let is_recursive = match can_output.tail_call {
                    Some(ref symbol) if symbol == defined_symbol => Recursive::TailRecursive,
                    _ => Recursive::NotRecursive,
                };

                // Recursion doesn't count as referencing. (If it did, all recursive functions
                // would result in circular def errors!)
                refs_by_symbol
                    .entry(defined_symbol.clone())
                    .and_modify(|(_, refs)| {
                        refs.lookups = refs.lookups.without(defined_symbol);
                    });

                loc_can_expr.value = Closure(
                    fn_var,
                    symbol.clone(),
                    is_recursive,
                    arguments.clone(),
                    body.clone(),
                );
            }

            // Store the referenced locals in the refs_by_symbol map, so we can later figure out
            // which defined names reference each other.
            for (ident, (symbol, region)) in scope.idents() {
                if original_scope.contains_ident(ident) {
                    continue;
                }

                let refs =
                    // Functions' references don't count in defs.
                    // See 3d5a2560057d7f25813112dfa5309956c0f9e6a9 and its
                    // parent commit for the bug this fixed!
                    if is_closure {
                        References::new()
                    } else {
                        can_output.references.clone()
                    };

                refs_by_symbol.insert(
                    symbol.clone(),
                    (
                        Located {
                            value: ident.clone(),
                            region: region.clone(),
                        },
                        refs,
                    ),
                );

                can_defs_by_symbol.insert(
                    *symbol,
                    Def {
                        expr_var,
                        // TODO try to remove this .clone()!
                        loc_pattern: loc_can_pattern.clone(),
                        loc_expr: Located {
                            // TODO try to remove this .clone()!
                            region: loc_can_expr.region,
                            value: loc_can_expr.value.clone(),
                        },
                        pattern_vars: im::HashMap::clone(&vars_by_symbol),
                        annotation: None,
                    },
                );
            }
        }
    };
}

#[inline(always)]
pub fn can_defs_with_return<'a>(
    env: &mut Env<'a>,
    var_store: &VarStore,
    mut scope: Scope,
    loc_defs: &'a bumpalo::collections::Vec<'a, &'a Located<ast::Def<'a>>>,
    loc_ret: &'a Located<ast::Expr<'a>>,
) -> (Expr, Output) {
    let mut found_rigids = SendMap::default();
    let unsorted = canonicalize_defs(
        env,
        &mut found_rigids,
        var_store,
        &mut scope,
        loc_defs,
        PatternType::DefExpr,
    );

    // The def as a whole is a tail call iff its return expression is a tail call.
    // Use its output as a starting point because its tail_call already has the right answer!
    let (ret_expr, output) =
        canonicalize_expr(env, var_store, &mut scope, loc_ret.region, &loc_ret.value);

    let (can_defs, mut output) = sort_can_defs(env, unsorted, output);

    output.rigids = output.rigids.union(found_rigids);

    match can_defs {
        Ok(decls) => {
            let mut loc_expr: Located<Expr> = ret_expr;

            for declaration in decls.into_iter().rev() {
                loc_expr = Located {
                    region: Region::zero(),
                    value: decl_to_let(var_store, declaration, loc_expr),
                };
            }

            (loc_expr.value, output)
        }
        Err(err) => (RuntimeError(err), output),
    }
}

fn decl_to_let(var_store: &VarStore, decl: Declaration, loc_ret: Located<Expr>) -> Expr {
    match decl {
        Declaration::Declare(def) => {
            Expr::LetNonRec(Box::new(def), Box::new(loc_ret), var_store.fresh())
        }
        Declaration::DeclareRec(defs) => Expr::LetRec(defs, Box::new(loc_ret), var_store.fresh()),
        Declaration::InvalidCycle(symbols, regions) => {
            Expr::RuntimeError(RuntimeError::CircularDef(symbols, regions))
        }
    }
}

fn closure_recursivity(symbol: Symbol, closures: &MutMap<Symbol, References>) -> Recursive {
    let mut visited = MutSet::default();

    let mut stack = Vec::new();

    if let Some(references) = closures.get(&symbol) {
        for v in &references.calls {
            stack.push(v.clone());
        }

        // while there are symbols left to visit
        while let Some(nested_symbol) = stack.pop() {
            if nested_symbol.clone() == symbol {
                return Recursive::Recursive;
            }

            // if the called symbol not yet in the graph
            if !visited.contains(&nested_symbol) {
                // add it to the visited set
                // if it calls any functions
                if let Some(nested_references) = closures.get(&nested_symbol) {
                    // add its called to the stack
                    for v in &nested_references.calls {
                        stack.push(v.clone());
                    }
                }
                visited.insert(nested_symbol);
            }
        }
    }

    Recursive::NotRecursive
}

fn to_pending_def<'a>(
    env: &mut Env<'a>,
    var_store: &VarStore,
    def: &'a ast::Def<'a>,
    scope: &mut Scope,
    pattern_type: PatternType,
) -> PendingDef<'a> {
    use crate::parse::ast::Def::*;

    match def {
        Annotation(loc_pattern, loc_ann) => {
            // This takes care of checking for shadowing and adding idents to scope.
            let loc_can_pattern = canonicalize_pattern(
                env,
                var_store,
                scope,
                pattern_type,
                &loc_pattern.value,
                loc_pattern.region,
            );

            // annotation sans body cannot introduce new rigids that are visible in other annotations
            // but the rigids can show up in type error messages, so still register them
            let loc_can_ann = Located {
                region: loc_ann.region,
                value: canonicalize_annotation(
                    env,
                    scope,
                    &loc_ann.value,
                    loc_ann.region,
                    var_store,
                ),
            };

            PendingDef::AnnotationOnly(loc_pattern, loc_can_pattern, loc_can_ann)
        }
        Body(loc_pattern, loc_expr) => {
            // This takes care of checking for shadowing and adding idents to scope.
            let loc_can_pattern = canonicalize_pattern(
                env,
                var_store,
                scope,
                pattern_type,
                &loc_pattern.value,
                loc_pattern.region,
            );

            PendingDef::Body(loc_pattern, loc_can_pattern, loc_expr)
        }
        TypedBody(loc_pattern, loc_ann, loc_expr) => pending_typed_body(
            env,
            loc_pattern,
            loc_ann,
            loc_expr,
            var_store,
            scope,
            pattern_type,
        ),

        SpaceBefore(sub_def, _) | SpaceAfter(sub_def, _) | Nested(sub_def) => {
            to_pending_def(env, var_store, sub_def, scope, pattern_type)
        }
    }
}

fn pending_typed_body<'a>(
    env: &mut Env<'a>,
    loc_pattern: &'a Located<ast::Pattern<'a>>,
    loc_ann: &'a Located<ast::TypeAnnotation<'a>>,
    loc_expr: &'a Located<ast::Expr<'a>>,
    var_store: &VarStore,
    scope: &mut Scope,
    pattern_type: PatternType,
) -> PendingDef<'a> {
    let loc_can_ann = Located {
        region: loc_ann.region,
        value: canonicalize_annotation(env, scope, &loc_ann.value, loc_ann.region, var_store),
    };
    // This takes care of checking for shadowing and adding idents to scope.
    let loc_can_pattern = canonicalize_pattern(
        env,
        var_store,
        scope,
        pattern_type,
        &loc_pattern.value,
        loc_pattern.region,
    );

    PendingDef::TypedBody(loc_pattern, loc_can_pattern, loc_can_ann, loc_expr)
}
