use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::SchemaResolver;
use crate::grammar::{Combine, Grammar, GrammarItem, NameClass, Pattern, Root, SchemaDocument};
use crate::syntax::parse_with_inherited_ns;

/// A parsed schema after composition/simplification (RELAX NG spec §4):
/// `include`/`div`/`combine` resolved, nested `grammar`s flattened, every
/// pattern reduced to the handful of primitive forms the validator
/// understands. Produced by [`simplify`] (or [`crate::Schema::compile`],
/// which also parses); pass it to [`crate::validate`] (or
/// [`crate::Schema::validate`]) to check documents against it.
#[derive(Clone, Debug)]
pub struct CompiledSchema {
    pub(crate) start: Pattern,
    pub(crate) definitions: BTreeMap<String, Pattern>,
}

impl CompiledSchema {
    /// How many named `define`s survived simplification — mainly useful
    /// for diagnostics/tests, not something normal validation needs.
    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }
    /// Whether this schema's `start` pattern can match anything at all
    /// (`false` only for a schema that's valid RELAX NG but pointless —
    /// its `start` simplified all the way down to `notAllowed`).
    pub fn has_start(&self) -> bool {
        !matches!(self.start, Pattern::NotAllowed)
    }
}

/// A schema parsed without error but invalid once composed/simplified —
/// an undefined `ref`, illegal recursion, a duplicate `define` with
/// inconsistent `combine`, a §7 restriction violation, and so on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaError {
    message: String,
}

impl SchemaError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SchemaError {}

/// Composes and simplifies `document` (RELAX NG spec §4) into a
/// [`CompiledSchema`], resolving any `include`/`externalRef` it uses via
/// `resolver`. See [`crate::Schema::compile`] for a convenience wrapper
/// that also parses.
pub fn simplify(
    document: &SchemaDocument,
    resolver: &impl SchemaResolver,
) -> Result<CompiledSchema, SchemaError> {
    let mut loading = BTreeSet::new();
    simplify_root(document, resolver, &mut loading)
}

fn simplify_root(
    document: &SchemaDocument,
    resolver: &impl SchemaResolver,
    loading: &mut BTreeSet<String>,
) -> Result<CompiledSchema, SchemaError> {
    let compiled = match &document.root {
        Root::Pattern(pattern) => {
            let mut extra = BTreeMap::new();
            let mut next_id = 0usize;
            let start = simplify_pattern(
                pattern,
                resolver,
                loading,
                &mut extra,
                &mut next_id,
                &BTreeSet::new(),
                None,
            )?;
            Ok(CompiledSchema {
                start,
                definitions: extra,
            })
        }
        Root::Grammar(grammar) => simplify_grammar(grammar, resolver, loading),
    }?;
    check_illegal_recursion(&compiled.start, &compiled.definitions)?;
    let mut scope = RestrictionScope {
        definitions: &compiled.definitions,
        visiting: BTreeSet::new(),
        verified: BTreeSet::new(),
    };
    check_restrictions(&compiled.start, false, false, false, false, &mut scope)?;
    // `ref` is transparent above, so every definition reachable from
    // `start` was already checked under its *actual* ancestor context
    // (which is what §7.1-§7.3 restrict) — a `define` is free to rely on
    // every one of its `ref` sites providing e.g. a `oneOrMore` ancestor,
    // as real-world schemas do. A definition unreachable from `start` has
    // no such context to inherit, so it's checked standalone instead.
    let mut reachable = BTreeSet::new();
    collect_reachable_defines(&compiled.start, &compiled.definitions, &mut reachable);
    for (def_name, pattern) in compiled.definitions.iter() {
        if reachable.contains(def_name.as_str()) {
            continue;
        }
        check_restrictions(pattern, false, false, false, false, &mut scope)?;
    }
    check_group_restrictions(&compiled.start, &compiled.definitions)?;
    for pattern in compiled.definitions.values() {
        check_group_restrictions(pattern, &compiled.definitions)?;
    }
    check_start_restrictions(&compiled.start, &compiled.definitions, &mut BTreeSet::new())?;
    Ok(compiled)
}

/// RELAX NG §4.19: a `define` must not be able to reach itself again
/// through a chain of `ref`s without an intervening `element` — that
/// recursion would never ground out. Legal recursion (through an
/// `element`) is unaffected, since we don't follow `ref`s found inside an
/// `element`'s own body when building the dependency graph below.
///
/// Unlike §4.16, this is only checked for `define`s actually reachable
/// from `start` — a self-referencing `define` that's never used is dead
/// code, not an infinite loop, and the spec test suite confirms it's
/// still a *correct* schema.
fn check_illegal_recursion(
    start: &Pattern,
    definitions: &BTreeMap<String, Pattern>,
) -> Result<(), SchemaError> {
    let mut reachable = BTreeSet::new();
    collect_reachable_defines(start, definitions, &mut reachable);
    let mut graph: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in &reachable {
        let pattern = &definitions[*name];
        let mut edges = Vec::new();
        collect_ref_edges(pattern, &mut edges);
        graph.insert(*name, edges);
    }
    let mut state: BTreeMap<&str, RefVisit> = BTreeMap::new();
    for name in graph.keys().copied().collect::<Vec<_>>() {
        if !state.contains_key(name) {
            visit_ref_graph(name, &graph, &mut state)?;
        }
    }
    Ok(())
}

/// Collects the names of every `define` reachable (transitively, through
/// `ref`, crossing `element` freely) from `pattern`.
fn collect_reachable_defines<'a>(
    pattern: &'a Pattern,
    definitions: &'a BTreeMap<String, Pattern>,
    reachable: &mut BTreeSet<&'a str>,
) {
    match pattern {
        Pattern::Ref(name) => {
            if let Some(definition) = definitions.get(name)
                && reachable.insert(name.as_str())
            {
                collect_reachable_defines(definition, definitions, reachable);
            }
        }
        Pattern::Element { body, .. } | Pattern::Attribute { body, .. } => {
            for child in body {
                collect_reachable_defines(child, definitions, reachable);
            }
        }
        Pattern::Group(values)
        | Pattern::Interleave(values)
        | Pattern::Choice(values)
        | Pattern::OneOrMore(values)
        | Pattern::List(values)
        // Present only pre-simplification (Optional/ZeroOrMore/Mixed are
        // rewritten away by then) — harmless to include unconditionally.
        | Pattern::Optional(values)
        | Pattern::ZeroOrMore(values)
        | Pattern::Mixed(values) => {
            for child in values {
                collect_reachable_defines(child, definitions, reachable);
            }
        }
        Pattern::Data { except, .. } => {
            for child in except {
                collect_reachable_defines(child, definitions, reachable);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefVisit {
    Visiting,
    Done,
}

fn visit_ref_graph<'a>(
    name: &'a str,
    graph: &BTreeMap<&'a str, Vec<&'a str>>,
    state: &mut BTreeMap<&'a str, RefVisit>,
) -> Result<(), SchemaError> {
    state.insert(name, RefVisit::Visiting);
    if let Some(edges) = graph.get(name) {
        for &target in edges {
            match state.get(target) {
                Some(RefVisit::Visiting) => {
                    return Err(SchemaError::new(format!(
                        "RELAX NG §4.19: `{target}` illegally refers to itself without an intervening element"
                    )));
                }
                Some(RefVisit::Done) => {}
                None if graph.contains_key(target) => {
                    visit_ref_graph(target, graph, state)?;
                }
                None => {}
            }
        }
    }
    state.insert(name, RefVisit::Done);
    Ok(())
}

/// Collects `ref` targets reachable from `pattern` without crossing an
/// `element` boundary (transparent through `group`/`interleave`/`choice`/
/// `oneOrMore`; `attribute`/`list`/`data`'s `except` can't contain a `ref`
/// at all per §7.1.1/§7.1.3/§7.1.4, so they need no special case here).
fn collect_ref_edges<'a>(pattern: &'a Pattern, edges: &mut Vec<&'a str>) {
    match pattern {
        Pattern::Ref(name) => edges.push(name.as_str()),
        Pattern::Group(values)
        | Pattern::Interleave(values)
        | Pattern::Choice(values)
        | Pattern::OneOrMore(values)
        // Present only pre-simplification — see collect_reachable_defines.
        | Pattern::Optional(values)
        | Pattern::ZeroOrMore(values)
        | Pattern::Mixed(values) => {
            for child in values {
                collect_ref_edges(child, edges);
            }
        }
        _ => {}
    }
}

fn simplify_grammar(
    grammar: &Grammar,
    resolver: &impl SchemaResolver,
    loading: &mut BTreeSet<String>,
) -> Result<CompiledSchema, SchemaError> {
    let items = flatten_items(&grammar.items, resolver, loading)?;
    let (starts, definitions) = collect_items(items)?;
    let start = merge("start", starts)?.ok_or_else(|| SchemaError::new("grammar has no start"))?;
    let definitions: BTreeMap<String, Pattern> = definitions
        .into_iter()
        .map(|(name, fragments)| {
            let pattern = merge(&name, fragments)?
                .ok_or_else(|| SchemaError::new(format!("`{name}` has no pattern")))?;
            Ok((name, pattern))
        })
        .collect::<Result<_, SchemaError>>()?;
    // §4.19, checked here (pre-`notAllowed`-collapse) as well as again on
    // the fully-simplified form in `simplify_root` — this pass sees
    // `ref`s that §4.20 will later erase, but (unlike the raw form) can't
    // see into a nested `grammar` pattern's own flattened/renamed defines,
    // so neither check alone is a strict superset of the other.
    check_illegal_recursion(&start, &definitions)?;
    let mut extra = BTreeMap::new();
    let mut next_id = 0usize;
    let scope: BTreeSet<String> = definitions.keys().cloned().collect();
    let start = simplify_pattern(
        &start,
        resolver,
        loading,
        &mut extra,
        &mut next_id,
        &scope,
        None,
    )?;
    let mut compiled_definitions: BTreeMap<String, Pattern> = definitions
        .into_iter()
        .map(|(name, pattern)| {
            Ok((
                name,
                simplify_pattern(
                    &pattern,
                    resolver,
                    loading,
                    &mut extra,
                    &mut next_id,
                    &scope,
                    None,
                )?,
            ))
        })
        .collect::<Result<_, SchemaError>>()?;
    compiled_definitions.extend(extra);
    Ok(CompiledSchema {
        start,
        definitions: compiled_definitions,
    })
}

/// Collects the `start` fragments and `define` fragments (keyed by name) out
/// of an already-flattened item list, preserving each fragment's own
/// `combine` separately — consistency between fragments (§4.17) is checked
/// uniformly by `merge`, the single source of truth for that rule. Shared
/// by the top-level grammar and by nested `grammar` patterns (see
/// `simplify_pattern`'s `Pattern::Grammar` case).
#[allow(clippy::type_complexity)]
fn collect_items(
    items: Vec<GrammarItem>,
) -> Result<
    (
        Vec<(Option<Combine>, Vec<Pattern>)>,
        BTreeMap<String, Vec<(Option<Combine>, Vec<Pattern>)>>,
    ),
    SchemaError,
> {
    let mut starts = Vec::new();
    let mut definitions: BTreeMap<String, Vec<(Option<Combine>, Vec<Pattern>)>> = BTreeMap::new();
    for item in items {
        match item {
            GrammarItem::Start { combine, body } => starts.push((combine, body)),
            GrammarItem::Define {
                name,
                combine,
                body,
            } => definitions.entry(name).or_default().push((combine, body)),
            GrammarItem::Div(_) | GrammarItem::Include { .. } => {
                unreachable!("items are flattened")
            }
        }
    }
    Ok((starts, definitions))
}

fn flatten_items(
    items: &[GrammarItem],
    resolver: &impl SchemaResolver,
    loading: &mut BTreeSet<String>,
) -> Result<Vec<GrammarItem>, SchemaError> {
    let mut output = Vec::new();
    for item in items {
        match item {
            GrammarItem::Div(children) => {
                output.extend(flatten_items(children, resolver, loading)?)
            }
            GrammarItem::Include {
                href,
                body,
                context,
                ..
            } => {
                let key = format!("{}::{href}", context.base_uri);
                if !loading.insert(key.clone()) {
                    return Err(SchemaError::new("include cycle"));
                }
                let result = resolver
                    .resolve(href, &context.base_uri)
                    .map_err(|error| SchemaError::new(error.to_string()))
                    .and_then(|source| {
                        // §4.6/§4.7: this element's own `ns` transfers to
                        // the referenced content as a fallback.
                        parse_with_inherited_ns(&source, context.ns.clone())
                            .map_err(|error| SchemaError::new(error.to_string()))
                    })
                    .and_then(|included| {
                        let Root::Grammar(grammar) = included.root else {
                            return Err(SchemaError::new("include must resolve to a grammar"));
                        };
                        let mut included_items =
                            flatten_items(&grammar.items, resolver, loading)?;
                        let overriding_items = flatten_items(body, resolver, loading)?;
                        check_include_overrides_exist(&included_items, &overriding_items)?;
                        let local_names: BTreeSet<_> = overriding_items
                            .iter()
                            .filter_map(|item| match item {
                                GrammarItem::Define { name, .. } => Some(name),
                                _ => None,
                            })
                            .collect();
                        included_items.retain(|item| !matches!(item, GrammarItem::Define { name, .. } if local_names.contains(name)));
                        included_items.extend(overriding_items);
                        Ok(included_items)
                    });
                loading.remove(&key);
                output.extend(result?);
            }
            item => output.push(item.clone()),
        }
    }
    Ok(output)
}

/// RELAX NG §4.7: an `include`'s override body may only override
/// components the included grammar already has — a `start` override
/// requires the included grammar to already declare a `start`; a
/// `define` override requires a `define` of that exact name to already
/// exist there. `included`/`overriding` are both already flattened
/// (`Div`-transparent, matching how the rest of `include` resolution
/// treats them).
fn check_include_overrides_exist(
    included: &[GrammarItem],
    overriding: &[GrammarItem],
) -> Result<(), SchemaError> {
    let has_start = included
        .iter()
        .any(|item| matches!(item, GrammarItem::Start { .. }));
    let define_names: BTreeSet<&String> = included
        .iter()
        .filter_map(|item| match item {
            GrammarItem::Define { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    for item in overriding {
        match item {
            GrammarItem::Start { .. } if !has_start => {
                return Err(SchemaError::new(
                    "RELAX NG §4.7: include overrides `start`, but the included grammar has none",
                ));
            }
            GrammarItem::Define { name, .. } if !define_names.contains(name) => {
                return Err(SchemaError::new(format!(
                    "RELAX NG §4.7: include overrides `define name=\"{name}\"`, but the included grammar has no such define"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn merge(
    name: &str,
    fragments: Vec<(Option<Combine>, Vec<Pattern>)>,
) -> Result<Option<Pattern>, SchemaError> {
    let mut combine = None;
    let mut without_combine = 0usize;
    let mut patterns = Vec::new();
    for (method, bodies) in fragments {
        // §4.17: at most one fragment may omit `combine` — the rest must
        // specify it, and agree when they do. (A *single* fragment lacking
        // `combine`, i.e. no duplicates at all, is of course fine.)
        match method {
            Some(method) => {
                if combine.is_some_and(|existing| existing != method) {
                    return Err(SchemaError::new(format!(
                        "`{name}` has inconsistent combine"
                    )));
                }
                combine = Some(method);
            }
            None => without_combine += 1,
        }
        patterns.extend(bodies);
    }
    if without_combine > 1 {
        return Err(SchemaError::new(format!(
            "`{name}` has more than one definition without combine"
        )));
    }
    if patterns.is_empty() {
        return Ok(None);
    }
    if patterns.len() == 1 {
        return Ok(Some(patterns.pop().expect("one pattern")));
    }
    match combine {
        Some(Combine::Choice) => Ok(Some(Pattern::Choice(patterns))),
        Some(Combine::Interleave) => Ok(Some(Pattern::Interleave(patterns))),
        None => Err(SchemaError::new(format!(
            "duplicate `{name}` without combine"
        ))),
    }
}

fn simplify_pattern(
    pattern: &Pattern,
    resolver: &impl SchemaResolver,
    loading: &mut BTreeSet<String>,
    extra: &mut BTreeMap<String, Pattern>,
    next_id: &mut usize,
    // §4.18: valid `ref`/`parentRef` targets at this point in the tree —
    // checked *here*, during the normal bottom-up recursion, rather than
    // on the fully-simplified `compiled.definitions` afterwards, so an
    // undefined `ref` is still caught even if it sits inside a branch that
    // `notAllowed`-propagation (§4.20) will later collapse away. `scope`
    // is `ref`'s namespace (the nearest enclosing grammar's own defines);
    // `parent_scope` is `parentRef`'s (the grammar enclosing *that one*).
    scope: &BTreeSet<String>,
    parent_scope: Option<&BTreeSet<String>>,
) -> Result<Pattern, SchemaError> {
    let mut children = |patterns: &[Pattern]| {
        patterns
            .iter()
            .map(|child| {
                simplify_pattern(
                    child,
                    resolver,
                    loading,
                    extra,
                    next_id,
                    scope,
                    parent_scope,
                )
            })
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match pattern {
        Pattern::Ref(name) => {
            if !scope.contains(name) {
                return Err(SchemaError::new(format!(
                    "RELAX NG §4.18: `ref` to undefined `{name}`"
                )));
            }
            pattern.clone()
        }
        Pattern::ParentRef(name) => {
            if !parent_scope.is_some_and(|names| names.contains(name)) {
                return Err(SchemaError::new(format!(
                    "RELAX NG §4.18: `parentRef` to undefined `{name}`"
                )));
            }
            pattern.clone()
        }
        Pattern::Optional(values) => choice(vec![group(children(values)?), Pattern::Empty]),
        Pattern::ZeroOrMore(values) => {
            let repeated = one_or_more(group(children(values)?));
            choice(vec![repeated, Pattern::Empty])
        }
        Pattern::Mixed(values) => interleave(vec![group(children(values)?), Pattern::Text]),
        Pattern::OneOrMore(values) => one_or_more(group(children(values)?)),
        Pattern::List(values) => {
            let body = group(children(values)?);
            if matches!(body, Pattern::NotAllowed) {
                Pattern::NotAllowed
            } else {
                Pattern::List(vec![body])
            }
        }
        Pattern::Group(values) => group(children(values)?),
        Pattern::Choice(values) => choice(children(values)?),
        Pattern::Interleave(values) => interleave(children(values)?),
        Pattern::Element {
            name,
            body,
            context,
        } => {
            check_name_class_constraints(name)?;
            Pattern::Element {
                name: name.clone(),
                body: vec![group(children(body)?)],
                context: context.clone(),
            }
        }
        Pattern::Attribute {
            name,
            body,
            context,
        } => {
            check_name_class_constraints(name)?;
            check_attribute_name_class_constraints(name)?;
            let body = group(children(body)?);
            if matches!(body, Pattern::NotAllowed) {
                Pattern::NotAllowed
            } else {
                Pattern::Attribute {
                    name: name.clone(),
                    body: vec![body],
                    context: context.clone(),
                }
            }
        }
        Pattern::Data {
            datatype,
            datatype_library,
            params,
            except,
            context,
        } => {
            check_builtin_datatype_constraints(datatype, datatype_library, !params.is_empty())?;
            Pattern::Data {
                datatype: datatype.clone(),
                datatype_library: datatype_library.clone(),
                params: params.clone(),
                // §4.20: an `except` with a `notAllowed` child is removed —
                // excluding "no value" excludes nothing.
                except: children(except)?
                    .into_iter()
                    .filter(|value| !matches!(value, Pattern::NotAllowed))
                    .collect(),
                context: context.clone(),
            }
        }
        Pattern::Value {
            datatype: Some(datatype),
            datatype_library,
            ..
        } => {
            check_builtin_datatype_constraints(datatype, datatype_library, false)?;
            pattern.clone()
        }
        Pattern::ExternalRef { href, context, .. } => {
            let key = format!("{}::{href}", context.base_uri);
            if !loading.insert(key.clone()) {
                return Err(SchemaError::new("externalRef cycle"));
            }
            // `key` must stay marked "loading" for the *entire* recursive
            // simplification below, not just while resolving/parsing —
            // otherwise a schema that externalRefs right back to itself
            // never re-hits the cycle check and recurses forever. Compute
            // the result without an early `?` so `loading.remove` always
            // runs first, success or failure, mirroring `Include`'s
            // (already-correct) handling above.
            let result = resolver
                .resolve(href, &context.base_uri)
                .map_err(|error| SchemaError::new(error.to_string()))
                .and_then(|source| {
                    // §4.6: this element's own `ns` transfers to the
                    // referenced content as a fallback.
                    parse_with_inherited_ns(&source, context.ns.clone())
                        .map_err(|error| SchemaError::new(error.to_string()))
                })
                .and_then(|resolved| match resolved.root {
                    Root::Pattern(value) => simplify_pattern(
                        &value,
                        resolver,
                        loading,
                        extra,
                        next_id,
                        &BTreeSet::new(),
                        None,
                    ),
                    Root::Grammar(_) => {
                        Err(SchemaError::new("externalRef must resolve to a pattern"))
                    }
                });
            loading.remove(&key);
            result?
        }
        Pattern::Grammar(nested) => {
            let items = flatten_items(&nested.items, resolver, loading)?;
            let (starts, raw_defines) = collect_items(items)?;
            let start_body = merge("start", starts)?
                .ok_or_else(|| SchemaError::new("nested grammar has no start"))?;
            let raw_defines: BTreeMap<String, Pattern> = raw_defines
                .into_iter()
                .map(|(name, fragments)| {
                    let pattern = merge(&name, fragments)?
                        .ok_or_else(|| SchemaError::new(format!("`{name}` has no pattern")))?;
                    Ok((name, pattern))
                })
                .collect::<Result<_, SchemaError>>()?;
            let nested_scope: BTreeSet<String> = raw_defines.keys().cloned().collect();

            let simplified_start = simplify_pattern(
                &start_body,
                resolver,
                loading,
                extra,
                next_id,
                &nested_scope,
                Some(scope),
            )?;
            let mut simplified_defines = BTreeMap::new();
            for (name, body) in &raw_defines {
                simplified_defines.insert(
                    name.clone(),
                    simplify_pattern(
                        body,
                        resolver,
                        loading,
                        extra,
                        next_id,
                        &nested_scope,
                        Some(scope),
                    )?,
                );
            }

            // §4.19: a `grammar` occurring in pattern position (rather than
            // as the schema root) is flattened away — its `define`s move up
            // into the surrounding definition table under fresh names, and
            // its `ref`s are rewritten to match. `parentRef` becomes a plain
            // `ref` into the *enclosing* scope, left for that scope's own
            // flattening pass (or the top-level table) to resolve.
            *next_id += 1;
            let tag = *next_id;
            let rename: BTreeMap<String, String> = raw_defines
                .keys()
                .map(|name| (name.clone(), format!("{name}#{tag}")))
                .collect();
            for (name, pattern) in simplified_defines {
                extra.insert(rename[&name].clone(), rewrite_refs(&pattern, &rename));
            }
            rewrite_refs(&simplified_start, &rename)
        }
        value => value.clone(),
    })
}

/// Rewrites `ref`/`parentRef` occurrences after a nested `grammar` pattern
/// has been flattened: local `ref`s follow the rename map into their fresh
/// name, and `parentRef`s become plain `ref`s into the enclosing scope.
fn rewrite_refs(pattern: &Pattern, rename: &BTreeMap<String, String>) -> Pattern {
    let rewrite_all = |values: &[Pattern]| -> Vec<Pattern> {
        values
            .iter()
            .map(|value| rewrite_refs(value, rename))
            .collect()
    };
    match pattern {
        Pattern::Ref(name) => {
            Pattern::Ref(rename.get(name).cloned().unwrap_or_else(|| name.clone()))
        }
        Pattern::ParentRef(name) => Pattern::Ref(name.clone()),
        Pattern::Element {
            name,
            body,
            context,
        } => Pattern::Element {
            name: name.clone(),
            body: rewrite_all(body),
            context: context.clone(),
        },
        Pattern::Attribute {
            name,
            body,
            context,
        } => Pattern::Attribute {
            name: name.clone(),
            body: rewrite_all(body),
            context: context.clone(),
        },
        Pattern::Group(values) => Pattern::Group(rewrite_all(values)),
        Pattern::Interleave(values) => Pattern::Interleave(rewrite_all(values)),
        Pattern::Choice(values) => Pattern::Choice(rewrite_all(values)),
        Pattern::Optional(values) => Pattern::Optional(rewrite_all(values)),
        Pattern::ZeroOrMore(values) => Pattern::ZeroOrMore(rewrite_all(values)),
        Pattern::OneOrMore(values) => Pattern::OneOrMore(rewrite_all(values)),
        Pattern::List(values) => Pattern::List(rewrite_all(values)),
        Pattern::Mixed(values) => Pattern::Mixed(rewrite_all(values)),
        Pattern::Data {
            datatype,
            datatype_library,
            params,
            except,
            context,
        } => Pattern::Data {
            datatype: datatype.clone(),
            datatype_library: datatype_library.clone(),
            params: params.clone(),
            except: rewrite_all(except),
            context: context.clone(),
        },
        Pattern::Grammar(_) => unreachable!("nested grammars are flattened before rewriting refs"),
        value => value.clone(),
    }
}

/// `empty` is the identity element for `group`/`interleave` composition
/// (`p,empty ≡ p`, `p&empty ≡ p`) — drop it so a formerly-multi-child
/// group/interleave collapses down to its one remaining real operand
/// instead of staying wrapped around it. Safe now that both `<start>`
/// (§4.12) and the `oneOrMore//group//attribute` family of §7.1
/// restrictions no longer depend on a `group`/`interleave` node
/// surviving purely because an `empty` sibling kept its child count above
/// one — see plan/00-STATUS.md for the case that used to make this unsafe.
fn drop_identity_empty(mut values: Vec<Pattern>) -> Vec<Pattern> {
    if values.len() > 1 {
        values.retain(|value| !matches!(value, Pattern::Empty));
    }
    values
}

/// Builds a (already-`Vec`-simplification-normalized) `group` — also used
/// by the validator (Phase 05) to keep derivative-produced patterns
/// canonical/small the same way simplification does.
pub(crate) fn group(values: Vec<Pattern>) -> Pattern {
    if values
        .iter()
        .any(|value| matches!(value, Pattern::NotAllowed))
    {
        return Pattern::NotAllowed;
    }
    let mut values = drop_identity_empty(values);
    if values.is_empty() {
        Pattern::Empty
    } else if values.len() == 1 {
        values.pop().expect("one")
    } else {
        Pattern::Group(values)
    }
}
pub(crate) fn choice(mut values: Vec<Pattern>) -> Pattern {
    values.retain(|value| !matches!(value, Pattern::NotAllowed));
    if values.is_empty() {
        Pattern::NotAllowed
    } else if values.len() == 1 {
        values.pop().expect("one")
    } else {
        Pattern::Choice(values)
    }
}
pub(crate) fn interleave(values: Vec<Pattern>) -> Pattern {
    if values
        .iter()
        .any(|value| matches!(value, Pattern::NotAllowed))
    {
        return Pattern::NotAllowed;
    }
    let mut values = drop_identity_empty(values);
    if values.is_empty() {
        Pattern::Empty
    } else if values.len() == 1 {
        values.pop().expect("one")
    } else {
        Pattern::Interleave(values)
    }
}
pub(crate) fn one_or_more(body: Pattern) -> Pattern {
    if matches!(body, Pattern::NotAllowed) {
        Pattern::NotAllowed
    } else {
        Pattern::OneOrMore(vec![body])
    }
}

/// Shared traversal state for [`check_restrictions`], bundled into one
/// struct purely to keep that function's argument count reasonable.
struct RestrictionScope<'a> {
    definitions: &'a BTreeMap<String, Pattern>,
    // `ref` is transparent for these restrictions — a schema is free to
    // put e.g. the `oneOrMore` that §7.3 requires around a `ref` instead of
    // inside the `define` it points to (this is exactly how validation
    // itself treats `ref`, and real-world schemas rely on it). `visiting`
    // guards against re-entering a `ref` already being expanded on the
    // current path; §4.19 (checked earlier) already guarantees such a
    // cycle can only happen through an `element`, which resets every flag
    // here to `false` anyway, so this is a defensive backstop rather than
    // something expected to trigger.
    visiting: BTreeSet<String>,
    // A `define` that's shared by many `ref`s (common in real-world
    // schemas, e.g. an attribute group referenced from hundreds of
    // elements) would otherwise get re-walked once per occurrence, and
    // since occurrences nest, that's exponential in schema size. Once a
    // `(name, flags)` combination has passed, it always will (the check is
    // a pure function of the definition's structure and these flags), so
    // it's cached and skipped on repeat.
    verified: BTreeSet<(String, bool, bool, bool, bool)>,
}

fn check_restrictions(
    pattern: &Pattern,
    in_attribute: bool,
    in_list: bool,
    in_one_or_more: bool,
    // §7.1.2: descendant of a `group`/`interleave` that is itself a
    // descendant of a `oneOrMore` (not the same as `in_one_or_more` alone —
    // `oneOrMore { attribute a }` is fine, only wrapping a group/interleave
    // that mixes in an attribute is banned).
    in_one_or_more_container: bool,
    scope: &mut RestrictionScope,
) -> Result<(), SchemaError> {
    match pattern {
        Pattern::Ref(name) => {
            let key = (
                name.clone(),
                in_attribute,
                in_list,
                in_one_or_more,
                in_one_or_more_container,
            );
            if scope.verified.contains(&key) {
                return Ok(());
            }
            if let Some(definition) = scope.definitions.get(name)
                && scope.visiting.insert(name.clone())
            {
                check_restrictions(
                    definition,
                    in_attribute,
                    in_list,
                    in_one_or_more,
                    in_one_or_more_container,
                    scope,
                )?;
                scope.visiting.remove(name);
                scope.verified.insert(key);
            }
        }
        Pattern::Attribute { name, body, .. } => {
            if in_list {
                return Err(SchemaError::new(
                    "RELAX NG §7.1.3: a list pattern cannot contain attribute",
                ));
            }
            if in_attribute {
                return Err(SchemaError::new(
                    "RELAX NG §7.1.1: an attribute pattern cannot contain an attribute",
                ));
            }
            if in_one_or_more_container {
                return Err(SchemaError::new(
                    "RELAX NG §7.1.2: a oneOrMore pattern cannot wrap a group/interleave containing an attribute",
                ));
            }
            if name_class_has_infinite_names(name) && !in_one_or_more {
                return Err(SchemaError::new(
                    "RELAX NG §7.3: an attribute with an anyName or nsName name class must have a oneOrMore ancestor",
                ));
            }
            for child in body {
                check_restrictions(
                    child,
                    true,
                    in_list,
                    in_one_or_more,
                    in_one_or_more_container,
                    scope,
                )?;
            }
        }
        Pattern::List(body) => {
            if in_list {
                return Err(SchemaError::new(
                    "RELAX NG §7.1.3: a list pattern cannot contain list",
                ));
            }
            for child in body {
                check_restrictions(
                    child,
                    in_attribute,
                    true,
                    in_one_or_more,
                    in_one_or_more_container,
                    scope,
                )?;
            }
        }
        Pattern::Interleave(values) if in_list => {
            return Err(SchemaError::new(
                "RELAX NG §7.1.3: a list pattern cannot contain interleave",
            ));
        }
        Pattern::Text if in_list => {
            return Err(SchemaError::new(
                "RELAX NG §7.1.3: a list pattern cannot contain text",
            ));
        }
        Pattern::Element { .. } if in_attribute => {
            return Err(SchemaError::new(
                "RELAX NG §7.1.1: an attribute pattern cannot contain an element",
            ));
        }
        Pattern::Element { .. } if in_list => {
            return Err(SchemaError::new(
                "RELAX NG §7.1.3: a list pattern cannot contain element",
            ));
        }
        // `p*` desugars to `choice(empty, oneOrMore(p))` (§4.15), so for
        // restriction purposes it grants the same oneOrMore ancestor as a
        // literal `oneOrMore` — real-world schemas commonly write `attr*`
        // rather than `(attr)+` for a repeatable wildcard attribute.
        Pattern::OneOrMore(values) | Pattern::ZeroOrMore(values) => {
            for child in values {
                check_restrictions(
                    child,
                    in_attribute,
                    in_list,
                    true,
                    in_one_or_more_container,
                    scope,
                )?;
            }
        }
        Pattern::Group(values) | Pattern::Interleave(values) => {
            let in_container = in_one_or_more_container || in_one_or_more;
            for child in values {
                check_restrictions(
                    child,
                    in_attribute,
                    in_list,
                    in_one_or_more,
                    in_container,
                    scope,
                )?;
            }
        }
        Pattern::Choice(values) | Pattern::Optional(values) | Pattern::Mixed(values) => {
            for child in values {
                check_restrictions(
                    child,
                    in_attribute,
                    in_list,
                    in_one_or_more,
                    in_one_or_more_container,
                    scope,
                )?;
            }
        }
        Pattern::Element { body, .. } => {
            for child in body {
                check_restrictions(child, false, false, false, false, scope)?;
            }
        }
        Pattern::Data { except, .. } => {
            for child in except {
                check_except_restrictions(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// RELAX NG §4.16: a `data`/`value` must use a datatype known to its
/// library. We don't implement datatype libraries (Phase 06) or
/// `datatypeLibrary` inheritance (§4.3) yet, but the built-in library
/// (`datatypeLibrary=""`, explicitly) is part of the spec itself: it
/// defines exactly `string` and `token`, neither taking any parameters.
/// Only checked when `datatypeLibrary=""` is explicit — an *absent*
/// attribute may still inherit a real library from an ancestor, which
/// we can't yet resolve, so treating it as empty would misfire.
fn check_builtin_datatype_constraints(
    datatype: &str,
    datatype_library: &Option<String>,
    has_params: bool,
) -> Result<(), SchemaError> {
    if datatype_library.as_deref() != Some("") {
        return Ok(());
    }
    if datatype != "string" && datatype != "token" {
        return Err(SchemaError::new(format!(
            "RELAX NG §4.16: `{datatype}` is not a datatype of the built-in (empty) datatype library"
        )));
    }
    if has_params {
        return Err(SchemaError::new(
            "RELAX NG §4.16: the built-in (empty) datatype library's datatypes take no parameters",
        ));
    }
    Ok(())
}

/// RELAX NG §7.1.4: a `data/except` may only contain `data`, `value` or
/// `choice` (recursively) — not `attribute`, `ref`, `text`, `list`,
/// `group`, `interleave`, `oneOrMore` or `empty`.
fn check_except_restrictions(pattern: &Pattern) -> Result<(), SchemaError> {
    match pattern {
        Pattern::Data { except, .. } => {
            for child in except {
                check_except_restrictions(child)?;
            }
            Ok(())
        }
        Pattern::Value { .. } => Ok(()),
        Pattern::Choice(values) => {
            for child in values {
                check_except_restrictions(child)?;
            }
            Ok(())
        }
        _ => Err(SchemaError::new(
            "RELAX NG §7.1.4: a data/except pattern may only contain data, value or choice",
        )),
    }
}

/// RELAX NG §7.1.5: `start` may only (transitively, without crossing an
/// `element`) be composed of `choice` and `ref` around an `element` (or
/// `notAllowed`) — not `attribute`, `data`, `value`, `text`, `list`,
/// `group`, `interleave`, `oneOrMore` or `empty`.
fn check_start_restrictions(
    pattern: &Pattern,
    definitions: &BTreeMap<String, Pattern>,
    visiting: &mut BTreeSet<String>,
) -> Result<(), SchemaError> {
    match pattern {
        Pattern::Element { .. } | Pattern::NotAllowed => Ok(()),
        Pattern::Choice(values) => {
            for child in values {
                check_start_restrictions(child, definitions, visiting)?;
            }
            Ok(())
        }
        Pattern::Ref(name) => {
            if let Some(definition) = definitions.get(name)
                && visiting.insert(name.clone())
            {
                check_start_restrictions(definition, definitions, visiting)?;
                visiting.remove(name);
            }
            Ok(())
        }
        Pattern::Attribute { .. } => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain attribute",
        )),
        Pattern::Data { .. } => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain data",
        )),
        Pattern::Value { .. } => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain value",
        )),
        Pattern::Text => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain text",
        )),
        Pattern::List(_) => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain list",
        )),
        Pattern::Group(_) => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain group",
        )),
        Pattern::Interleave(_) => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain interleave",
        )),
        Pattern::OneOrMore(_) => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain oneOrMore",
        )),
        Pattern::Empty => Err(SchemaError::new(
            "RELAX NG §7.1.5: start cannot contain empty",
        )),
        Pattern::Optional(_)
        | Pattern::ZeroOrMore(_)
        | Pattern::Mixed(_)
        | Pattern::ExternalRef { .. }
        | Pattern::ParentRef(_)
        | Pattern::Grammar(_) => {
            unreachable!("resolved into other patterns by simplify_pattern")
        }
    }
}

/// RELAX NG §4.16: an `anyName`'s `except` must not have any `anyName`
/// descendant; an `nsName`'s `except` must not have any `nsName` or
/// `anyName` descendant. Checked independently of ref resolution/
/// reachability, so it runs during the normal top-down walk before any
/// dead branch could be collapsed away.
fn check_name_class_constraints(class: &NameClass) -> Result<(), SchemaError> {
    match class {
        NameClass::Any { except, .. } => {
            for item in except {
                if name_class_contains_any(item) {
                    return Err(SchemaError::new(
                        "RELAX NG §4.16: an anyName's except must not contain anyName",
                    ));
                }
                check_name_class_constraints(item)?;
            }
            Ok(())
        }
        NameClass::Namespace { except, .. } => {
            for item in except {
                if name_class_contains_any(item) || name_class_contains_namespace(item) {
                    return Err(SchemaError::new(
                        "RELAX NG §4.16: an nsName's except must not contain nsName or anyName",
                    ));
                }
                check_name_class_constraints(item)?;
            }
            Ok(())
        }
        NameClass::Name { .. } => Ok(()),
        NameClass::Choice(options) => {
            for item in options {
                check_name_class_constraints(item)?;
            }
            Ok(())
        }
    }
}

const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns";

/// The spec's own wording gives this URI without a trailing slash, but the
/// well-known XML Namespaces URI for `xmlns` has one — accept either.
fn is_xmlns_namespace(namespace: &Option<String>) -> bool {
    namespace
        .as_deref()
        .is_some_and(|value| value.trim_end_matches('/') == XMLNS_NAMESPACE)
}

/// RELAX NG §4.16: within an `attribute`'s name class, `xmlns` (in no
/// namespace) and the `http://www.w3.org/2000/xmlns` namespace are
/// reserved — an attribute pattern must not be able to match either, since
/// neither denotes a real XML attribute.
fn check_attribute_name_class_constraints(class: &NameClass) -> Result<(), SchemaError> {
    match class {
        NameClass::Name {
            lexical, namespace, ..
        } => {
            if ns_eq(namespace, &None) && lexical == "xmlns" {
                return Err(SchemaError::new(
                    "RELAX NG §4.16: an attribute name class must not match `xmlns` in no namespace",
                ));
            }
            if is_xmlns_namespace(namespace) {
                return Err(SchemaError::new(format!(
                    "RELAX NG §4.16: an attribute name class must not use the {XMLNS_NAMESPACE} namespace"
                )));
            }
            Ok(())
        }
        NameClass::Namespace {
            namespace, except, ..
        } => {
            if is_xmlns_namespace(namespace) {
                return Err(SchemaError::new(format!(
                    "RELAX NG §4.16: an attribute name class must not use the {XMLNS_NAMESPACE} namespace"
                )));
            }
            except
                .iter()
                .try_for_each(check_attribute_name_class_constraints)
        }
        NameClass::Any { except, .. } => except
            .iter()
            .try_for_each(check_attribute_name_class_constraints),
        NameClass::Choice(options) => options
            .iter()
            .try_for_each(check_attribute_name_class_constraints),
    }
}

fn name_class_contains_any(class: &NameClass) -> bool {
    match class {
        NameClass::Any { .. } => true,
        NameClass::Name { .. } => false,
        NameClass::Namespace { except, .. } => except.iter().any(name_class_contains_any),
        NameClass::Choice(options) => options.iter().any(name_class_contains_any),
    }
}

fn name_class_contains_namespace(class: &NameClass) -> bool {
    match class {
        NameClass::Namespace { .. } => true,
        NameClass::Name { .. } => false,
        NameClass::Any { except, .. } => except.iter().any(name_class_contains_namespace),
        NameClass::Choice(options) => options.iter().any(name_class_contains_namespace),
    }
}

/// True if `class` (through any `choice` branch) is an `anyName` or
/// `nsName` — i.e. can match more than one distinct attribute name.
fn name_class_has_infinite_names(class: &NameClass) -> bool {
    match class {
        NameClass::Name { .. } => false,
        NameClass::Any { .. } | NameClass::Namespace { .. } => true,
        NameClass::Choice(options) => options.iter().any(name_class_has_infinite_names),
    }
}

/// Flattens `choice` name classes into their leaf (`name`/`nsName`/`anyName`)
/// alternatives.
fn name_class_atoms(class: &NameClass) -> Vec<&NameClass> {
    match class {
        NameClass::Choice(options) => options.iter().flat_map(name_class_atoms).collect(),
        atom => vec![atom],
    }
}

/// `None` (no explicit `ns` attribute on a `name`/`nsName`, e.g. inside an
/// `attribute`, which — unlike `element` — never inherits a default
/// namespace) and `Some("")` both denote "no namespace"; treat them as
/// equal rather than as two different namespaces.
pub(crate) fn ns_eq(a: &Option<String>, b: &Option<String>) -> bool {
    a.as_deref().unwrap_or("") == b.as_deref().unwrap_or("")
}

/// Whether `class` matches the given expanded name. Shared with the
/// validator (Phase 05), which matches document element/attribute names
/// against schema name classes the same way the §7.3/§7.4 restriction
/// checks match schema name classes against each other.
pub(crate) fn name_class_matches(
    class: &NameClass,
    namespace: &Option<String>,
    local: &str,
) -> bool {
    match class {
        NameClass::Name {
            lexical,
            namespace: ns,
            ..
        } => ns_eq(ns, namespace) && lexical == local,
        NameClass::Any { except, .. } => !except
            .iter()
            .any(|item| name_class_matches(item, namespace, local)),
        NameClass::Namespace {
            namespace: ns,
            except,
            ..
        } => {
            ns_eq(ns, namespace)
                && !except
                    .iter()
                    .any(|item| name_class_matches(item, namespace, local))
        }
        NameClass::Choice(options) => options
            .iter()
            .any(|item| name_class_matches(item, namespace, local)),
    }
}

/// An `except` list is finite, so it can only ever exclude finitely many
/// local names from an (unbounded) namespace — unless one of its own
/// alternatives is itself an unrestricted `anyName`, which denotes "every
/// name" and so excludes everything.
fn excludes_everything(except: &[NameClass]) -> bool {
    except.iter().any(|item| {
        name_class_atoms(item)
            .into_iter()
            .any(|atom| matches!(atom, NameClass::Any { except, .. } if except.is_empty()))
    })
}

fn name_class_atom_overlaps(a: &NameClass, b: &NameClass) -> bool {
    match (a, b) {
        (
            NameClass::Name {
                lexical: l1,
                namespace: n1,
                ..
            },
            NameClass::Name {
                lexical: l2,
                namespace: n2,
                ..
            },
        ) => l1 == l2 && ns_eq(n1, n2),
        (
            NameClass::Name {
                lexical, namespace, ..
            },
            other,
        )
        | (
            other,
            NameClass::Name {
                lexical, namespace, ..
            },
        ) => name_class_matches(other, namespace, lexical),
        (
            NameClass::Namespace {
                namespace: n1,
                except: e1,
                ..
            },
            NameClass::Namespace {
                namespace: n2,
                except: e2,
                ..
            },
        ) => ns_eq(n1, n2) && !excludes_everything(e1) && !excludes_everything(e2),
        (NameClass::Namespace { except, .. }, NameClass::Any { except: other, .. })
        | (NameClass::Any { except: other, .. }, NameClass::Namespace { except, .. }) => {
            !excludes_everything(except) && !excludes_everything(other)
        }
        (NameClass::Any { except: e1, .. }, NameClass::Any { except: e2, .. }) => {
            !excludes_everything(e1) && !excludes_everything(e2)
        }
        (NameClass::Choice(_), _) | (_, NameClass::Choice(_)) => {
            unreachable!("choices are flattened into atoms before comparison")
        }
    }
}

/// Whether two name classes admit at least one common expanded name.
fn name_classes_overlap(a: &NameClass, b: &NameClass) -> bool {
    let atoms_b = name_class_atoms(b);
    name_class_atoms(a)
        .into_iter()
        .any(|x| atoms_b.iter().any(|y| name_class_atom_overlaps(x, y)))
}

/// What a `group`/`interleave` operand structurally contains, gathered
/// transparently through everything except `element`/`attribute` bodies
/// (those start a fresh scope) and `list` (excluded per §7.2) — resolving
/// `ref`s (with cycle protection, since legal recursive grammars exist)
/// since restrictions apply just as much to factored-out definitions as to
/// inline patterns.
#[derive(Default)]
struct Occurrences<'a> {
    attributes: Vec<&'a NameClass>,
    elements: Vec<&'a NameClass>,
    has_text: bool,
    /// §7.2: can this operand produce an element, data, value, list or
    /// text node — i.e. "match a child"?
    child_capable: bool,
    /// §7.2: can this operand produce a data, value or list node — i.e.
    /// "match a single string"?
    string_capable: bool,
}

fn collect_occurrences<'a>(
    pattern: &'a Pattern,
    definitions: &'a BTreeMap<String, Pattern>,
    visiting: &mut BTreeSet<String>,
    out: &mut Occurrences<'a>,
) {
    match pattern {
        Pattern::Attribute { name, .. } => out.attributes.push(name),
        Pattern::Element { name, .. } => {
            out.elements.push(name);
            out.child_capable = true;
        }
        Pattern::Text => {
            out.has_text = true;
            out.child_capable = true;
        }
        Pattern::Data { .. } | Pattern::Value { .. } => {
            out.child_capable = true;
            out.string_capable = true;
        }
        Pattern::List(_) => {
            // §7.2 explicitly excludes patterns *within* list, but list
            // itself is a leaf that matches a (single-string) child.
            out.child_capable = true;
            out.string_capable = true;
        }
        Pattern::Ref(target) => {
            if let Some(definition) = definitions.get(target)
                && visiting.insert(target.clone())
            {
                collect_occurrences(definition, definitions, visiting, out);
                visiting.remove(target);
            }
        }
        Pattern::Group(values)
        | Pattern::Choice(values)
        | Pattern::Interleave(values)
        | Pattern::Optional(values)
        | Pattern::ZeroOrMore(values)
        | Pattern::OneOrMore(values)
        | Pattern::Mixed(values) => {
            for child in values {
                collect_occurrences(child, definitions, visiting, out);
            }
        }
        Pattern::Grammar(_) | Pattern::ExternalRef { .. } | Pattern::ParentRef(_) => {
            unreachable!(
                "nested grammars, externalRef and parentRef are resolved by simplify_pattern"
            )
        }
        Pattern::Empty | Pattern::NotAllowed => {}
    }
}

/// §7.2 (a "matches a child" and a "matches a single string" pattern must
/// be alternatives, both `group` and `interleave`), §7.3 (duplicate
/// attributes, both `group` and `interleave`) and §7.4 (overlapping
/// elements / duplicated text, `interleave` only).
fn check_pairwise(
    values: &[Pattern],
    definitions: &BTreeMap<String, Pattern>,
    is_interleave: bool,
) -> Result<(), SchemaError> {
    let occurrences: Vec<Occurrences> = values
        .iter()
        .map(|value| {
            let mut out = Occurrences::default();
            collect_occurrences(value, definitions, &mut BTreeSet::new(), &mut out);
            out
        })
        .collect();
    for i in 0..occurrences.len() {
        for j in (i + 1)..occurrences.len() {
            let (left, right) = (&occurrences[i], &occurrences[j]);
            if (left.child_capable && right.string_capable)
                || (left.string_capable && right.child_capable)
            {
                return Err(SchemaError::new(
                    "RELAX NG §7.2: a pattern matching a child and a pattern matching a single string must be alternatives",
                ));
            }
            for a in &occurrences[i].attributes {
                for b in &occurrences[j].attributes {
                    if name_classes_overlap(a, b) {
                        return Err(SchemaError::new(
                            "RELAX NG §7.3: a group/interleave must not allow two attributes with the same name",
                        ));
                    }
                }
            }
            if is_interleave {
                if occurrences[i].has_text && occurrences[j].has_text {
                    return Err(SchemaError::new(
                        "RELAX NG §7.4: a text pattern must not occur on both sides of an interleave",
                    ));
                }
                for a in &occurrences[i].elements {
                    for b in &occurrences[j].elements {
                        if name_classes_overlap(a, b) {
                            return Err(SchemaError::new(
                                "RELAX NG §7.4: an interleave must not allow overlapping element names on both sides",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Walks the whole (already-flattened) schema looking for `group`/
/// `interleave` nodes and applying `check_pairwise` at each. Does not
/// descend through `ref` — every definition is walked independently by the
/// caller, so following refs here would just re-check shared content
/// (and loop forever on legal recursive grammars).
fn check_group_restrictions(
    pattern: &Pattern,
    definitions: &BTreeMap<String, Pattern>,
) -> Result<(), SchemaError> {
    match pattern {
        Pattern::Group(values) => {
            check_pairwise(values, definitions, false)?;
            for child in values {
                check_group_restrictions(child, definitions)?;
            }
        }
        Pattern::Interleave(values) => {
            check_pairwise(values, definitions, true)?;
            for child in values {
                check_group_restrictions(child, definitions)?;
            }
        }
        Pattern::Element { body, .. } | Pattern::Attribute { body, .. } => {
            for child in body {
                check_group_restrictions(child, definitions)?;
            }
        }
        Pattern::Choice(values)
        | Pattern::Optional(values)
        | Pattern::ZeroOrMore(values)
        | Pattern::OneOrMore(values)
        | Pattern::Mixed(values) => {
            for child in values {
                check_group_restrictions(child, definitions)?;
            }
        }
        // `list` is excluded from §7.2 by spec note, and can't contain an
        // `attribute`, `ref` or `interleave` (§7.1.3) for §7.3/§7.4 to ever
        // find anything in — nothing to check inside it.
        // `data/except` is separately restricted to `data`/`value`/`choice`
        // (§7.1.4, see `check_except_restrictions`), none of which can
        // themselves contain a `group`/`interleave` — nothing to check.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SchemaSource, SchemaSyntax};

    struct Resolver(BTreeMap<String, SchemaSource>);

    impl SchemaResolver for Resolver {
        fn resolve(
            &self,
            href: &str,
            _base_uri: &str,
        ) -> Result<SchemaSource, crate::ResolveError> {
            self.0
                .get(href)
                .cloned()
                .ok_or_else(|| crate::ResolveError::new("not found"))
        }
    }

    fn compact(text: &str) -> SchemaDocument {
        crate::parse(&SchemaSource::new(
            text,
            "memory:/main.rnc",
            SchemaSyntax::Compact,
        ))
        .expect("schema parses")
    }

    fn xml(text: &str) -> SchemaDocument {
        crate::parse(&SchemaSource::new(
            text,
            "memory:/main.rng",
            SchemaSyntax::Xml,
        ))
        .expect("schema parses")
    }

    #[test]
    fn include_override_replaces_the_included_definition() {
        let resolver = Resolver(BTreeMap::from([(
            "base.rnc".into(),
            SchemaSource::new(
                "start = root\nroot = element base { empty }",
                "memory:/base.rnc",
                SchemaSyntax::Compact,
            ),
        )]));
        let schema = compact("include \"base.rnc\" { root = element override { empty } }");
        let compiled = simplify(&schema, &resolver).expect("schema simplifies");
        assert!(compiled.has_start());
        assert_eq!(compiled.definition_count(), 1);
    }

    #[test]
    fn combine_requires_consistent_methods() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { empty }\npart |= empty\npart &= empty");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn combine_allows_exactly_one_fragment_without_it() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = x\nx |= element foo1 { empty }\nx |= element foo2 { empty }\nx = element foo3 { empty }",
        );
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn combine_rejects_more_than_one_fragment_without_it() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = x\nx = element foo1 { empty }\nx |= element foo2 { empty }\nx = element foo3 { empty }",
        );
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn nested_grammar_is_flattened_with_renamed_definitions() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = element root { grammar { start = foo\nfoo = element leaf { empty } } }",
        );
        let compiled = simplify(&schema, &resolver).expect("schema simplifies");
        assert!(compiled.has_start());
        assert_eq!(compiled.definition_count(), 1);
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected element pattern");
        };
        let Pattern::Ref(name) = &body[0] else {
            panic!("expected ref to renamed nested definition");
        };
        assert_ne!(name, "foo");
        let target = compiled
            .definitions
            .get(name)
            .expect("renamed definition exists");
        assert!(matches!(target, Pattern::Element { .. }));
    }

    #[test]
    fn parent_ref_resolves_into_the_enclosing_scope() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = element root { grammar { start = element nested { parent shared } } }\nshared = element marker { empty }",
        );
        let compiled = simplify(&schema, &resolver).expect("schema simplifies");
        assert!(compiled.has_start());
        // The nested grammar has no `define`, only `start`, so its body is
        // spliced in place — only the top-level `shared` is a definition.
        assert_eq!(compiled.definition_count(), 1);
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected element pattern");
        };
        let Pattern::Element {
            body: nested_body, ..
        } = &body[0]
        else {
            panic!("expected nested element pattern");
        };
        assert_eq!(nested_body[0], Pattern::Ref("shared".into()));
        assert!(compiled.definitions.contains_key("shared"));
    }

    #[test]
    fn duplicate_attribute_names_in_a_group_are_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema =
            compact("start = element root { attribute id { text }, attribute id { text } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn duplicate_attribute_names_reached_through_refs_are_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = element root { a, b }\na = attribute id { text }\nb = attribute id { text }",
        );
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn distinct_attribute_names_in_a_group_are_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema =
            compact("start = element root { attribute id { text }, attribute name { text } }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn wildcard_attribute_without_one_or_more_ancestor_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { attribute * { text } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn attribute_cannot_contain_an_element() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { attribute bar { element baz { empty } } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn list_cannot_contain_an_element() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { list { element bar { empty } } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn one_or_more_cannot_wrap_a_group_containing_an_attribute() {
        let resolver = Resolver(BTreeMap::new());
        let schema =
            compact("start = element root { (attribute bar { text }, attribute baz { text })+ }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn one_or_more_around_a_bare_attribute_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { (attribute bar { text })+ }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn ref_to_an_undefined_name_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = missing");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn ref_reaching_itself_without_an_element_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { bar }\nbar = (element bar { empty } | bar)");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn ref_recursing_through_an_element_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = bar\nbar = element bar { bar? }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn an_unreachable_self_referencing_define_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { empty }\nunused = unused");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn wildcard_attribute_with_one_or_more_ancestor_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { (attribute * { text })+ }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn wildcard_attribute_with_zero_or_more_ancestor_is_allowed() {
        // `p*` desugars to `choice(empty, oneOrMore(p))` (§4.15), so it
        // grants the same oneOrMore ancestor a literal `oneOrMore` would.
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { (attribute * { text })* }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn wildcard_attribute_wrapped_in_one_or_more_only_at_the_ref_site_is_allowed() {
        // `ref` is transparent for §7.1-§7.3: a `define` may rely on every
        // one of its `ref` sites providing the required `oneOrMore`
        // ancestor, without wrapping itself — real-world schemas (e.g. the
        // vnu-derived HTML5 RELAX NG schema) rely on exactly this.
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = element root { wildcard-attr* }\nwildcard-attr = attribute * { text }",
        );
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn wildcard_attribute_ref_without_any_one_or_more_wrapping_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema =
            compact("start = element root { wildcard-attr }\nwildcard-attr = attribute * { text }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn duplicate_text_in_interleave_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { text & text }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn duplicate_referenced_element_names_in_interleave_are_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact(
            "start = element root { a & b }\na = element item { empty }\nb = element item { empty }",
        );
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn direct_duplicate_element_names_in_interleave_are_also_rejected() {
        // Confirmed against the official RELAX NG test suite (jing-spectest
        // cases for §7.4): duplicate element names in an interleave are
        // rejected whether reached directly or through a `ref`.
        let resolver = Resolver(BTreeMap::new());
        let schema =
            compact("start = element root { element item { empty } & element item { empty } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn start_cannot_be_a_bare_attribute() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = attribute foo { text }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn a_bare_pattern_root_is_also_checked_against_start_restrictions() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("text");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn data_except_cannot_contain_text() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { string {} - text }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn data_except_with_a_value_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { string {} - \"foo\" }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn unknown_type_in_the_built_in_datatype_library_is_rejected() {
        // No `datatypeLibrary` anywhere — §4.3 says that defaults to the
        // empty string (the built-in library), which only has `string`
        // and `token`.
        let resolver = Resolver(BTreeMap::new());
        let schema = xml(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo"><data type="tok"/></element>"#,
        );
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn datatype_library_is_inherited_from_an_ancestor_element() {
        let resolver = Resolver(BTreeMap::new());
        let schema = xml(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes"><data type="NCName"/></element>"#,
        );
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn a_string_pattern_sequenced_with_an_element_is_rejected() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { string, element child { empty } }");
        assert!(simplify(&schema, &resolver).is_err());
    }

    #[test]
    fn a_string_pattern_as_an_alternative_to_an_element_is_allowed() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { string | element child { empty } }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn string_sequences_inside_list_are_not_restricted_by_7_2() {
        let resolver = Resolver(BTreeMap::new());
        let schema = compact("start = element root { list { string, string } }");
        assert!(simplify(&schema, &resolver).is_ok());
    }

    #[test]
    fn external_reference_is_resolved_through_the_caller() {
        let resolver = Resolver(BTreeMap::from([(
            "part.rnc".into(),
            SchemaSource::new(
                "element child { empty }",
                "memory:/part.rnc",
                SchemaSyntax::Compact,
            ),
        )]));
        let schema = compact("start = external \"part.rnc\"");
        assert!(
            simplify(&schema, &resolver)
                .expect("external pattern simplifies")
                .has_start()
        );
    }
}
