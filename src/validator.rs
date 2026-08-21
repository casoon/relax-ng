//! Validates a document (via the generic [`Element`] trait) against a
//! [`CompiledSchema`](crate::CompiledSchema) using Brzozowski-derivative
//! pattern matching — the standard approach for RELAX NG validators,
//! derived independently from the pattern algebra in the spec (not ported
//! from any other implementation; see `plan/DECISIONS.md`).
//!
//! # The derivative idea
//!
//! A pattern describes the set of "things" (child sequences, attribute
//! sets, text values) it accepts. Given one such thing has just been seen
//! (an attribute, a child element, a run of text), the *derivative* of the
//! pattern with respect to it is a new pattern describing what must still
//! follow. A pattern is *nullable* if it accepts having nothing more
//! follow at all. Validating a whole element reduces to: derive its
//! content pattern through every attribute (unordered) and then every
//! child in order, and check the result is nullable.
//!
//! Every derivative function here transparently follows `ref` — including
//! through `interleave` — which is deliberate: the most serious bug found
//! in the `xmloxide` RELAX NG parser (the reason this crate exists at all,
//! see `plan/DECISIONS.md`) was exactly a `ref` inside `interleave` not
//! being resolved for attribute matching, rejecting almost every attribute
//! in a modular schema. [`attribute_matches_through_ref_and_interleave`]
//! (in the test module) is a standing regression test for that failure
//! mode.
//!
//! # Datatypes
//!
//! `data`/`value` matching is delegated to a [`DatatypeRegistry`] the
//! caller supplies to [`validate`] — this module never hardcodes datatype
//! semantics itself (see `crate::datatypes`). Every `data`/`value`
//! reachable in the schema is checked against the registry once, upfront,
//! before any document is touched, so an unregistered `datatypeLibrary` or
//! an invalid facet fails fast with a single, clear [`DatatypeError`]
//! rather than partway through validation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::CompiledSchema;
use crate::datatypes::{DatatypeContext, DatatypeError, DatatypeRegistry};
use crate::grammar::{Context, Param, Pattern};
use crate::simplify::{choice, group, interleave, name_class_matches};

type Definitions = BTreeMap<String, Pattern>;

/// An expanded (namespace-resolved) name: local name plus an optional
/// namespace URI. `None` means "no namespace" — the caller's document
/// implementation is responsible for resolving any prefixes itself (this
/// crate has no XML/namespace parser of its own).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ExpandedName {
    /// The namespace URI, or `None` for "no namespace".
    pub namespace: Option<String>,
    /// The local (unprefixed) name.
    pub local: String,
}

impl ExpandedName {
    /// Builds an expanded name from a namespace (`None` for "no
    /// namespace") and a local name.
    pub fn new(namespace: Option<impl Into<String>>, local: impl Into<String>) -> Self {
        Self {
            namespace: namespace.map(Into::into),
            local: local.into(),
        }
    }
}

impl fmt::Display for ExpandedName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Some(namespace) => write!(formatter, "{{{namespace}}}{}", self.local),
            None => formatter.write_str(&self.local),
        }
    }
}

/// One child of an element, in document order. Comments and processing
/// instructions are not part of this model — a correct `Element`
/// implementation simply never yields them here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content<E> {
    /// A child element.
    Element(E),
    /// A single, non-empty, already-merged run of text — if two text
    /// nodes are adjacent in the underlying document (however "adjacent"
    /// is defined there, e.g. around a comment), the implementation must
    /// merge them into one `Text` before they reach here.
    Text(String),
}

/// The minimal view of a document element this crate needs to validate it.
/// Implement this over whatever tree type the caller already has — no
/// document is parsed by this crate itself.
pub trait Element: Sized {
    /// This element's expanded name.
    fn name(&self) -> ExpandedName;
    /// This element's attributes, in any order (attribute order is never
    /// significant in XML, and RELAX NG never treats it as such either).
    /// `xmlns`/`xmlns:*` namespace declarations are not attributes for
    /// RELAX NG's purposes and must not be yielded here.
    fn attributes(&self) -> impl Iterator<Item = (ExpandedName, String)>;
    /// This element's children, in document order.
    fn children(&self) -> impl Iterator<Item = Content<Self>>;
    /// An optional human-readable location (e.g. `line:column` or an
    /// XPath-like pointer), included in any [`ValidationError`] raised
    /// against this element when available.
    fn location(&self) -> Option<String> {
        None
    }
    /// The XML namespace prefix bindings in scope at this element
    /// (`("", uri)` for the default namespace), including any inherited
    /// from ancestors — needed only to resolve `QName`/`NOTATION`-typed
    /// attribute/text *values* (not element/attribute names, which arrive
    /// already-expanded via [`Self::name`]). Default: no bindings, so
    /// `QName`/`NOTATION` values only match when unprefixed and the
    /// default namespace (if any) is what the schema expects.
    fn namespace_bindings(&self) -> impl Iterator<Item = (String, String)> {
        std::iter::empty()
    }
}

/// Validates `root` against `schema` using `registry` to check `data`/
/// `value` patterns. Returns the list of problems found (empty means
/// valid) — or, if the schema uses a `datatypeLibrary`/type/parameter
/// `registry` doesn't recognize, a [`DatatypeError`] *instead of*
/// attempting validation at all, since we can't know whether the document
/// would pass a check we can't perform.
pub fn validate<E: Element>(
    schema: &CompiledSchema,
    registry: &DatatypeRegistry,
    root: &E,
) -> Result<Vec<ValidationError>, DatatypeError> {
    let mut checked_refs = BTreeSet::new();
    check_datatypes_supported(
        &schema.start,
        &schema.definitions,
        registry,
        &mut checked_refs,
    )?;
    for pattern in schema.definitions.values() {
        check_datatypes_supported(pattern, &schema.definitions, registry, &mut checked_refs)?;
    }

    let mut errors = Vec::new();
    let mut path = Vec::new();
    match find_element_contents(&schema.start, &root.name(), &schema.definitions) {
        Some(content) => validate_element_content(
            &content,
            root,
            &schema.definitions,
            registry,
            &mut path,
            &mut errors,
        ),
        None => errors.push(ValidationError {
            kind: ValidationErrorKind::UnexpectedElement(root.name()),
            path: Vec::new(),
            location: root.location(),
        }),
    }
    Ok(errors)
}

/// A single validation problem, with the path of element names from the
/// document root down to (and including) the element it occurred on, and
/// an optional location if [`Element::location`] provided one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    kind: ValidationErrorKind,
    path: Vec<ExpandedName>,
    location: Option<String>,
}

impl ValidationError {
    /// What kind of problem this is.
    pub fn kind(&self) -> &ValidationErrorKind {
        &self.kind
    }
    /// The element names from the document root down to (and including)
    /// the element this problem occurred on.
    pub fn path(&self) -> &[ExpandedName] {
        &self.path
    }
    /// The location [`Element::location`] provided for the affected
    /// element, if any.
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.kind)?;
        if let Some(location) = &self.location {
            write!(formatter, " at {location}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

/// What kind of problem a [`ValidationError`] describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationErrorKind {
    /// An element with this name isn't allowed at this position.
    UnexpectedElement(ExpandedName),
    /// A text child isn't allowed at this position (and isn't part of a
    /// `data`/`value`/`list` that could accept it).
    UnexpectedText,
    /// This attribute name isn't allowed on this element at all.
    UnexpectedAttribute(ExpandedName),
    /// This attribute is allowed on this element, but its value doesn't
    /// match what the schema requires.
    InvalidAttributeValue(ExpandedName, String),
    /// The element ended while the schema still required more content
    /// (a missing child element, missing text, or a missing attribute).
    MissingContent,
}

impl fmt::Display for ValidationErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedElement(name) => write!(formatter, "unexpected element `{name}`"),
            Self::UnexpectedText => formatter.write_str("unexpected text"),
            Self::UnexpectedAttribute(name) => write!(formatter, "unexpected attribute `{name}`"),
            Self::InvalidAttributeValue(name, value) => {
                write!(formatter, "invalid value `{value}` for attribute `{name}`")
            }
            Self::MissingContent => formatter.write_str("missing required content"),
        }
    }
}

fn validate_element_content<E: Element>(
    content: &Pattern,
    element: &E,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    path: &mut Vec<ExpandedName>,
    errors: &mut Vec<ValidationError>,
) {
    path.push(element.name());
    let mut pattern = content.clone();
    let context = DatatypeContext::from_bindings(element.namespace_bindings().collect());

    // Attributes are unordered — fold the derivative over them in
    // whatever order the caller's iterator happens to yield.
    for (name, value) in element.attributes() {
        if !attribute_name_known(&pattern, &name, definitions) {
            errors.push(ValidationError {
                kind: ValidationErrorKind::UnexpectedAttribute(name),
                path: path.clone(),
                location: element.location(),
            });
            continue;
        }
        let derived = deriv_attribute(&pattern, &name, &value, definitions, registry, &context);
        if matches!(derived, Pattern::NotAllowed) {
            errors.push(ValidationError {
                kind: ValidationErrorKind::InvalidAttributeValue(name, value),
                path: path.clone(),
                location: element.location(),
            });
        } else {
            pattern = derived;
        }
    }

    // Children are ordered.
    for child in element.children() {
        match child {
            Content::Text(text) => {
                let derived = deriv_text(&pattern, &text, definitions, registry, &context);
                if matches!(derived, Pattern::NotAllowed) {
                    // §6.2.7: a text node that consists entirely of
                    // whitespace is treated as if it weren't there at all
                    // when it doesn't otherwise fit the content model —
                    // but *only* if nothing reachable there actually wants
                    // text (`wants_text`); a bare `data`/`value` (or a
                    // `choice` where every branch wants text) still must
                    // see a real, matching event. Non-whitespace text that
                    // doesn't fit is always an error.
                    if !text.trim().is_empty() || wants_text(&pattern, definitions) {
                        errors.push(ValidationError {
                            kind: ValidationErrorKind::UnexpectedText,
                            path: path.clone(),
                            location: element.location(),
                        });
                    }
                } else {
                    pattern = derived;
                }
            }
            Content::Element(child_element) => {
                let name = child_element.name();
                match find_element_contents(&pattern, &name, definitions) {
                    Some(child_content) => {
                        validate_element_content(
                            &child_content,
                            &child_element,
                            definitions,
                            registry,
                            path,
                            errors,
                        );
                        pattern = deriv_element_event(&pattern, &name, definitions);
                    }
                    None => errors.push(ValidationError {
                        kind: ValidationErrorKind::UnexpectedElement(name),
                        path: path.clone(),
                        location: child_element.location(),
                    }),
                }
            }
        }
    }

    if !nullable(&pattern, definitions, registry) {
        errors.push(ValidationError {
            kind: ValidationErrorKind::MissingContent,
            path: path.clone(),
            location: element.location(),
        });
    }
    path.pop();
}

fn param_pairs(params: &[Param]) -> Vec<(&str, &str)> {
    params
        .iter()
        .map(|param| (param.name.as_str(), param.value.as_str()))
        .collect()
}

/// The namespace-resolution context for a `value` pattern's *own* literal
/// text, as written in the schema — used as the "expected" side of
/// [`DatatypeLibrary::values_equal`] for `QName`/`NOTATION`-like types.
/// Deliberately built from the schema, never the document: `namespace` is
/// §4.9's (already-inherited) `ns` attribute, used to resolve the literal
/// if it's unprefixed; `schema_context.namespaces` are the schema XML's
/// own in-scope `xmlns:` bindings, used to resolve it if it has a prefix.
fn schema_value_context(namespace: &Option<String>, schema_context: &Context) -> DatatypeContext {
    let mut bindings: Vec<(String, String)> = schema_context
        .namespaces
        .iter()
        .map(|(prefix, uri)| (prefix.clone(), uri.clone()))
        .collect();
    bindings.push((String::new(), namespace.clone().unwrap_or_default()));
    DatatypeContext::from_bindings(bindings)
}

/// Every `data`/`value` reachable in the schema must use a `datatypeLibrary`
/// `registry` has an implementation for, with a known type name and
/// well-formed, applicable parameters — checked once, upfront, over the
/// *whole* schema (not lazily as validation happens to reach one), so the
/// caller gets a clear, consistent error regardless of which document path
/// would have triggered it.
fn check_datatypes_supported(
    pattern: &Pattern,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    checked_refs: &mut BTreeSet<String>,
) -> Result<(), DatatypeError> {
    match pattern {
        Pattern::Data {
            datatype,
            datatype_library,
            params,
            except,
            ..
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let implementation = registry
                .get(library)
                .ok_or_else(|| DatatypeError::unsupported_library(library))?;
            implementation
                .validate_params(datatype, &param_pairs(params))
                .map_err(|reason| DatatypeError::type_problem(library, datatype, &reason))?;
            for child in except {
                check_datatypes_supported(child, definitions, registry, checked_refs)?;
            }
            Ok(())
        }
        Pattern::Value {
            datatype,
            datatype_library,
            ..
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let type_name = datatype.as_deref().unwrap_or("token");
            let implementation = registry
                .get(library)
                .ok_or_else(|| DatatypeError::unsupported_library(library))?;
            implementation
                .validate_params(type_name, &[])
                .map_err(|reason| DatatypeError::type_problem(library, type_name, &reason))
        }
        Pattern::Element { body, .. } | Pattern::Attribute { body, .. } => {
            body.iter().try_for_each(|child| {
                check_datatypes_supported(child, definitions, registry, checked_refs)
            })
        }
        Pattern::Group(values)
        | Pattern::Interleave(values)
        | Pattern::Choice(values)
        | Pattern::OneOrMore(values)
        | Pattern::List(values) => values.iter().try_for_each(|child| {
            check_datatypes_supported(child, definitions, registry, checked_refs)
        }),
        Pattern::Ref(name) => {
            if checked_refs.insert(name.clone())
                && let Some(target) = definitions.get(name)
            {
                check_datatypes_supported(target, definitions, registry, checked_refs)?;
            }
            Ok(())
        }
        Pattern::Empty | Pattern::NotAllowed | Pattern::Text => Ok(()),
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

/// Whether `pattern` accepts having nothing more follow — i.e. whether the
/// content/attributes/text matched *so far* (already folded into
/// `pattern` via the various `deriv_*` functions below) are already
/// enough. `ref` is always resolved: recursion through it is guaranteed to
/// terminate because RELAX NG §4.19 (checked at schema-compile time,
/// before any document is ever validated) already rules out a `ref`
/// reaching itself again without crossing an `element` — and none of the
/// constructs `nullable` recurses through (`group`/`choice`/`interleave`/
/// `oneOrMore`/`ref`/`list`) is an `element`. `data`/`value` nullability
/// asks whether `registry` would accept the implicit empty string (e.g.
/// the built-in `string`/`token` do; most XSD types don't).
pub(crate) fn nullable(
    pattern: &Pattern,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
) -> bool {
    match pattern {
        Pattern::Empty => true,
        Pattern::NotAllowed => false,
        Pattern::Text => true,
        Pattern::Element { .. } | Pattern::Attribute { .. } => false,
        Pattern::Data {
            datatype,
            datatype_library,
            params,
            except,
            ..
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let implementation = registry
                .get(library)
                .expect("checked upfront by check_datatypes_supported");
            implementation.matches(
                datatype,
                &param_pairs(params),
                "",
                &DatatypeContext::empty(),
            ) && !text_matches_except(except, "", definitions, registry, &DatatypeContext::empty())
        }
        Pattern::Value {
            value,
            datatype,
            datatype_library,
            namespace,
            context: schema_context,
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let type_name = datatype.as_deref().unwrap_or("token");
            let implementation = registry
                .get(library)
                .expect("checked upfront by check_datatypes_supported");
            implementation.values_equal(
                type_name,
                value,
                &schema_value_context(namespace, schema_context),
                "",
                &DatatypeContext::empty(),
            )
        }
        // A `list` with no text child at all behaves like its inner
        // pattern seeing zero tokens — same as `list_matches` below would
        // compute for empty/whitespace-only text, without needing an
        // actual `Content::Text` event to run that logic. Not `nullable`
        // (rule-3-inclusive): a `list` around a bare `data`/`value` slot
        // still needs an actual token, even though that datatype alone
        // would also tolerate the literal empty string — see
        // `structurally_nullable`'s docs.
        Pattern::List(values) => structurally_nullable(&values[0], definitions),
        Pattern::Choice(values) => values
            .iter()
            .any(|value| nullable(value, definitions, registry)),
        Pattern::Group(values) | Pattern::Interleave(values) => values
            .iter()
            .all(|value| nullable(value, definitions, registry)),
        Pattern::OneOrMore(values) => nullable(&values[0], definitions, registry),
        Pattern::Ref(name) => nullable(
            definitions
                .get(name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            definitions,
            registry,
        ),
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

/// Derives `values` (the operands of a `group`/`interleave`/attribute-
/// matching-`group`) with respect to one event, trying every position
/// `deriv_one` succeeds at and rebuilding the container (`wrap`) around
/// whichever one operand changed, leaving the rest untouched. If
/// `sequential`, a position is only reachable once every earlier operand's
/// *child content* is satisfied (`group`'s ordered semantics for
/// element/text content, gated by [`child_reachable`] rather than plain
/// [`nullable`] — see there for why); if not, every position is always
/// reachable (`interleave`'s semantics — and, deliberately, `group`'s
/// *attribute*-matching semantics too, since unlike child content,
/// attributes have no order in XML at all).
fn deriv_positions(
    values: &[Pattern],
    sequential: bool,
    wrap: fn(Vec<Pattern>) -> Pattern,
    definitions: &Definitions,
    deriv_one: &dyn Fn(&Pattern) -> Pattern,
) -> Pattern {
    let mut alternatives = Vec::new();
    let mut reachable = true;
    for (index, value) in values.iter().enumerate() {
        if reachable {
            let derived = deriv_one(value);
            if !matches!(derived, Pattern::NotAllowed) {
                let mut branch = values.to_vec();
                if sequential {
                    // Standard Brzozowski concatenation-derivative rule:
                    // D_c(A·B)'s "skip A" branch (taken because A is
                    // already nullable) is exactly D_c(B) — A itself is
                    // *closed*, fixed to `Empty`, not left as A. Left
                    // unchanged, A would wrongly stay available to match
                    // *more* events later, after B has already started
                    // (e.g. `group(text, element bar)` would let text
                    // reappear after `bar`, which isn't sequential at
                    // all). Interleave/attribute-matching (`!sequential`)
                    // has no such "prefix must close" concept — every
                    // other position, before or after `index`, genuinely
                    // does stay open regardless of order.
                    for earlier in branch.iter_mut().take(index) {
                        *earlier = close_child_position(earlier, definitions);
                    }
                }
                branch[index] = derived;
                alternatives.push(wrap(branch));
            }
        }
        if sequential {
            reachable = reachable && structurally_nullable(value, definitions);
        }
    }
    choice(alternatives)
}

/// Whether `pattern` is satisfied by *zero* events, in the strict
/// structural sense — unlike [`nullable`], a bare `data`/`value`/`list`
/// (which structurally requires exactly one text event to occur at all)
/// is never considered satisfied here just because it happens to accept
/// the *literal empty string* as a value (that narrower equivalence is
/// §6.2.7 weak-match rule 3, and belongs only at the true top level —
/// deciding whether a whole element/attribute that saw *no* relevant
/// event at all is still valid, which [`nullable`] already handles).
///
/// This stricter notion is what §6.2.7 weak-match rule 2 actually needs
/// (dropping a whitespace-only text/attribute event requires the pattern
/// to *already*, strictly, accept zero events — not "would accept the
/// empty string if offered one"), what deciding whether a `list` has
/// enough tokens needs (a `list` with a `data`/`value` slot no token ever
/// reached is not satisfied just because that slot's datatype tolerates
/// `""`), and what gates `group`'s sequential "skip a satisfied-so-far
/// prefix" logic in [`deriv_positions`] (an `attribute` operand is the one
/// exception: attributes are orthogonal to child/token sequencing
/// entirely, matched separately by [`deriv_attribute`]).
fn structurally_nullable(pattern: &Pattern, definitions: &Definitions) -> bool {
    match pattern {
        Pattern::Attribute { .. } | Pattern::Empty | Pattern::Text => true,
        Pattern::NotAllowed
        | Pattern::Element { .. }
        | Pattern::Data { .. }
        | Pattern::Value { .. } => false,
        Pattern::List(values) => structurally_nullable(&values[0], definitions),
        Pattern::Choice(values) => values
            .iter()
            .any(|value| structurally_nullable(value, definitions)),
        Pattern::Group(values) | Pattern::Interleave(values) => values
            .iter()
            .all(|value| structurally_nullable(value, definitions)),
        Pattern::OneOrMore(values) => structurally_nullable(&values[0], definitions),
        Pattern::Ref(name) => structurally_nullable(
            definitions
                .get(name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            definitions,
        ),
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

/// Whether some *reachable* position in `pattern` is a text-desiring leaf
/// (`text`/`data`/`value`/`list`) that a whitespace-only text/attribute
/// event, once it fails to match normally, should be treated as a real
/// (failed) candidate for — rather than silently dropped as
/// formatting/insignificant whitespace (§6.2.7). "Reachable" mirrors
/// `deriv_positions`'s own sequential-prefix gating for `group`
/// ([`structurally_nullable`] decides what counts as "already past"); for
/// `choice`, a branch not taken is never a candidate for *this* event, so
/// only refuse to drop when *every* branch wants text — if even one
/// doesn't, that's the branch this whitespace can harmlessly fall through
/// to (e.g. `choice(value "x", element bar {empty})`: leading whitespace
/// before `<bar/>` is fine, even though the *other* branch wants text).
/// `interleave`/`group`, by contrast, require *every* operand to
/// eventually be satisfied (unordered or ordered respectively), so any
/// one of them still wanting text is enough to not drop.
fn wants_text(pattern: &Pattern, definitions: &Definitions) -> bool {
    match pattern {
        Pattern::Text | Pattern::Data { .. } | Pattern::Value { .. } | Pattern::List(_) => true,
        Pattern::Attribute { .. }
        | Pattern::Empty
        | Pattern::NotAllowed
        | Pattern::Element { .. } => false,
        Pattern::Choice(values) => values.iter().all(|value| wants_text(value, definitions)),
        Pattern::Interleave(values) => values.iter().any(|value| wants_text(value, definitions)),
        Pattern::Group(values) => {
            let mut reachable = true;
            let mut found = false;
            for value in values {
                if reachable {
                    found = found || wants_text(value, definitions);
                }
                reachable = reachable && structurally_nullable(value, definitions);
            }
            found
        }
        Pattern::OneOrMore(values) => wants_text(&values[0], definitions),
        Pattern::Ref(name) => wants_text(
            definitions
                .get(name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            definitions,
        ),
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

/// The "closed" form of a `group` position that a later position's event
/// was matched *past* (see [`deriv_positions`]'s `sequential` case):
/// standard Brzozowski concatenation-derivative rule, D_c(A·B)'s "skip A"
/// branch is exactly D_c(B), not A·D_c(B) — so A is fixed to `Empty`
/// (done, contributing nothing further to *child* matching), not left
/// open to match more child events later. `attribute` sub-patterns are the
/// one exception: they're orthogonal to child sequencing (matched
/// entirely separately, see [`deriv_attribute`]) and must stay exactly as
/// they were — closing one here would wrongly mark a not-yet-matched
/// attribute as satisfied.
fn close_child_position(pattern: &Pattern, definitions: &Definitions) -> Pattern {
    match pattern {
        Pattern::Attribute { .. } => pattern.clone(),
        Pattern::Group(values) => group(
            values
                .iter()
                .map(|value| close_child_position(value, definitions))
                .collect(),
        ),
        Pattern::Interleave(values) => interleave(
            values
                .iter()
                .map(|value| close_child_position(value, definitions))
                .collect(),
        ),
        Pattern::Choice(values) => choice(
            values
                .iter()
                .map(|value| close_child_position(value, definitions))
                .collect(),
        ),
        Pattern::OneOrMore(values) => close_child_position(&values[0], definitions),
        Pattern::Ref(name) => close_child_position(
            definitions
                .get(name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            definitions,
        ),
        _ => Pattern::Empty,
    }
}

/// The derivative of `pattern` with respect to one *attribute* named
/// `name` with string value `value`. Transparently follows `ref` — see
/// the module docs — and, critically, treats `group` the same as
/// `interleave` (attributes have no order), unlike [`deriv_element_event`]
/// and [`deriv_text`], where `group` really is sequential.
fn deriv_attribute(
    pattern: &Pattern,
    name: &ExpandedName,
    value: &str,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    context: &DatatypeContext,
) -> Pattern {
    match pattern {
        Pattern::Attribute {
            name: name_class,
            body,
            ..
        } => {
            if name_class_matches(name_class, &name.namespace, &name.local)
                && value_matches(&body[0], value, definitions, registry, context)
            {
                Pattern::Empty
            } else {
                Pattern::NotAllowed
            }
        }
        Pattern::Group(values) => deriv_positions(values, false, group, definitions, &|v| {
            deriv_attribute(v, name, value, definitions, registry, context)
        }),
        Pattern::Interleave(values) => {
            deriv_positions(values, false, interleave, definitions, &|v| {
                deriv_attribute(v, name, value, definitions, registry, context)
            })
        }
        Pattern::Choice(values) => choice(
            values
                .iter()
                .map(|v| deriv_attribute(v, name, value, definitions, registry, context))
                .collect(),
        ),
        Pattern::OneOrMore(values) => one_or_more_deriv(&values[0], pattern, &|v| {
            deriv_attribute(v, name, value, definitions, registry, context)
        }),
        Pattern::Ref(ref_name) => deriv_attribute(
            definitions
                .get(ref_name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            name,
            value,
            definitions,
            registry,
            context,
        ),
        Pattern::Element { .. }
        | Pattern::Text
        | Pattern::Data { .. }
        | Pattern::Value { .. }
        | Pattern::List(_)
        | Pattern::Empty
        | Pattern::NotAllowed => Pattern::NotAllowed,
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

/// The derivative of `pattern` with respect to a single child *element*
/// event (just its name — the element's own content is validated
/// separately and recursively, see [`find_element_contents`]).
fn deriv_element_event(
    pattern: &Pattern,
    name: &ExpandedName,
    definitions: &Definitions,
) -> Pattern {
    match pattern {
        Pattern::Element {
            name: name_class, ..
        } => {
            if name_class_matches(name_class, &name.namespace, &name.local) {
                Pattern::Empty
            } else {
                Pattern::NotAllowed
            }
        }
        Pattern::Group(values) => deriv_positions(values, true, group, definitions, &|v| {
            deriv_element_event(v, name, definitions)
        }),
        Pattern::Interleave(values) => {
            deriv_positions(values, false, interleave, definitions, &|v| {
                deriv_element_event(v, name, definitions)
            })
        }
        Pattern::Choice(values) => choice(
            values
                .iter()
                .map(|v| deriv_element_event(v, name, definitions))
                .collect(),
        ),
        Pattern::OneOrMore(values) => one_or_more_deriv(&values[0], pattern, &|v| {
            deriv_element_event(v, name, definitions)
        }),
        Pattern::Ref(ref_name) => deriv_element_event(
            definitions
                .get(ref_name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            name,
            definitions,
        ),
        Pattern::Attribute { .. }
        | Pattern::Text
        | Pattern::Data { .. }
        | Pattern::Value { .. }
        | Pattern::List(_)
        | Pattern::Empty
        | Pattern::NotAllowed => Pattern::NotAllowed,
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

/// The derivative of `pattern` with respect to a text *value* — used both
/// for a child `Content::Text` event (the whole merged run of text handed
/// over as one value) and, from [`list_matches`], for each individual
/// whitespace-separated token inside a `list`.
fn deriv_text(
    pattern: &Pattern,
    text: &str,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    context: &DatatypeContext,
) -> Pattern {
    match pattern {
        // `text` absorbs any amount of text, including across separate
        // (non-adjacent, interleaved-with-elements) runs, so it stays
        // `text` rather than being "used up".
        Pattern::Text => Pattern::Text,
        Pattern::Data {
            datatype,
            datatype_library,
            params,
            except,
            ..
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let implementation = registry
                .get(library)
                .expect("checked upfront by check_datatypes_supported");
            if implementation.matches(datatype, &param_pairs(params), text, context)
                && !text_matches_except(except, text, definitions, registry, context)
            {
                Pattern::Empty
            } else {
                Pattern::NotAllowed
            }
        }
        Pattern::Value {
            value,
            datatype,
            datatype_library,
            namespace,
            context: schema_context,
        } => {
            let library = datatype_library.as_deref().unwrap_or("");
            let type_name = datatype.as_deref().unwrap_or("token");
            let implementation = registry
                .get(library)
                .expect("checked upfront by check_datatypes_supported");
            if implementation.values_equal(
                type_name,
                value,
                &schema_value_context(namespace, schema_context),
                text,
                context,
            ) {
                Pattern::Empty
            } else {
                Pattern::NotAllowed
            }
        }
        Pattern::List(values) => {
            if list_matches(&values[0], text, definitions, registry, context) {
                Pattern::Empty
            } else {
                Pattern::NotAllowed
            }
        }
        Pattern::Group(values) => deriv_positions(values, true, group, definitions, &|v| {
            deriv_text(v, text, definitions, registry, context)
        }),
        Pattern::Interleave(values) => {
            deriv_positions(values, false, interleave, definitions, &|v| {
                deriv_text(v, text, definitions, registry, context)
            })
        }
        Pattern::Choice(values) => choice(
            values
                .iter()
                .map(|v| deriv_text(v, text, definitions, registry, context))
                .collect(),
        ),
        Pattern::OneOrMore(values) => one_or_more_deriv(&values[0], pattern, &|v| {
            deriv_text(v, text, definitions, registry, context)
        }),
        Pattern::Ref(ref_name) => deriv_text(
            definitions
                .get(ref_name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            text,
            definitions,
            registry,
            context,
        ),
        Pattern::Element { .. }
        | Pattern::Attribute { .. }
        | Pattern::Empty
        | Pattern::NotAllowed => Pattern::NotAllowed,
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

/// `oneOrMore(p)`'s derivative w.r.t. any event: consume one occurrence of
/// `p` now, then either the `oneOrMore` continues or (having satisfied the
/// "one") it may now stop.
fn one_or_more_deriv(
    inner: &Pattern,
    whole: &Pattern,
    deriv_one: &dyn Fn(&Pattern) -> Pattern,
) -> Pattern {
    group(vec![
        deriv_one(inner),
        choice(vec![whole.clone(), Pattern::Empty]),
    ])
}

fn text_matches_except(
    except: &[Pattern],
    text: &str,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    context: &DatatypeContext,
) -> bool {
    except.iter().any(|item| {
        nullable(
            &deriv_text(item, text, definitions, registry, context),
            definitions,
            registry,
        )
    })
}

/// Whether `text`, split on XML whitespace into tokens, matches `pattern`
/// (a `list`'s inner pattern) when each token is fed through in sequence.
fn list_matches(
    pattern: &Pattern,
    text: &str,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    context: &DatatypeContext,
) -> bool {
    let mut current = pattern.clone();
    for token in text.split_ascii_whitespace() {
        current = deriv_text(&current, token, definitions, registry, context);
        if matches!(current, Pattern::NotAllowed) {
            return false;
        }
    }
    // Not `nullable`: a slot no token ever reached (e.g. the second
    // `data` in `list { data type="token", data type="token" }` given
    // only one token) must not be excused just because its datatype
    // would also tolerate the literal empty string — see
    // `structurally_nullable`'s docs.
    structurally_nullable(&current, definitions)
}

/// Whether `value` (an attribute's string value, or — recursively — a
/// `list` element's whole text) satisfies `pattern` on its own — including
/// §6.2.7: a value that's whitespace-only (or empty) and doesn't otherwise
/// fit still matches if nothing in `pattern` actually wants text (e.g.
/// `attribute id { empty }` accepts `id=" "`) — see [`wants_text`].
fn value_matches(
    pattern: &Pattern,
    value: &str,
    definitions: &Definitions,
    registry: &DatatypeRegistry,
    context: &DatatypeContext,
) -> bool {
    let derived = deriv_text(pattern, value, definitions, registry, context);
    if !matches!(derived, Pattern::NotAllowed) && structurally_nullable(&derived, definitions) {
        return true;
    }
    value.trim().is_empty() && !wants_text(pattern, definitions)
}

/// Whether *some* `attribute` pattern reachable in `pattern` (transparent
/// through `ref`, `group`, `interleave`, `choice`, `oneOrMore` — the exact
/// same positions [`deriv_attribute`] would look at) has a name class
/// matching `name`, regardless of value. Used to tell "this attribute
/// isn't allowed at all" apart from "this attribute is allowed, but its
/// value is wrong".
fn attribute_name_known(pattern: &Pattern, name: &ExpandedName, definitions: &Definitions) -> bool {
    match pattern {
        Pattern::Attribute {
            name: name_class, ..
        } => name_class_matches(name_class, &name.namespace, &name.local),
        Pattern::Group(values) | Pattern::Interleave(values) | Pattern::Choice(values) => values
            .iter()
            .any(|value| attribute_name_known(value, name, definitions)),
        Pattern::OneOrMore(values) => attribute_name_known(&values[0], name, definitions),
        Pattern::Ref(ref_name) => attribute_name_known(
            definitions
                .get(ref_name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            name,
            definitions,
        ),
        _ => false,
    }
}

/// The union (as a `choice`) of every `element` pattern's content reachable
/// in `pattern` (transparently through `ref`/`group`(sequential, like
/// [`deriv_element_event`])/`interleave`/`choice`/`oneOrMore`) whose name
/// class matches `name` — `None` if none do. Used both to find what a
/// child element's own content must be validated against, and (via
/// [`nullable`]/emptiness) to tell whether this element name is expected
/// here at all.
fn find_element_contents(
    pattern: &Pattern,
    name: &ExpandedName,
    definitions: &Definitions,
) -> Option<Pattern> {
    let mut found = Vec::new();
    collect_element_contents(pattern, name, definitions, &mut found);
    if found.is_empty() {
        None
    } else {
        Some(choice(found))
    }
}

fn collect_element_contents(
    pattern: &Pattern,
    name: &ExpandedName,
    definitions: &Definitions,
    out: &mut Vec<Pattern>,
) {
    match pattern {
        Pattern::Element {
            name: name_class,
            body,
            ..
        } if name_class_matches(name_class, &name.namespace, &name.local) => {
            out.push(body[0].clone());
        }
        Pattern::Choice(values) | Pattern::Interleave(values) => {
            for value in values {
                collect_element_contents(value, name, definitions, out);
            }
        }
        Pattern::Group(values) => {
            let mut reachable = true;
            for value in values {
                if !reachable {
                    break;
                }
                collect_element_contents(value, name, definitions, out);
                reachable = structurally_nullable(value, definitions);
            }
        }
        Pattern::OneOrMore(values) => collect_element_contents(&values[0], name, definitions, out),
        Pattern::Ref(ref_name) => collect_element_contents(
            definitions
                .get(ref_name)
                .expect("RELAX NG §4.18 guarantees every ref target exists"),
            name,
            definitions,
            out,
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::DatatypeLibrary;
    use crate::{ResolveError, SchemaResolver, SchemaSource, SchemaSyntax};

    struct NoResolver;

    impl SchemaResolver for NoResolver {
        fn resolve(&self, href: &str, _base_uri: &str) -> Result<SchemaSource, ResolveError> {
            Err(ResolveError::new(format!(
                "validator tests don't use includes (requested `{href}`)"
            )))
        }
    }

    fn schema(text: &str) -> CompiledSchema {
        let document = crate::parse(&SchemaSource::new(
            text,
            "memory:/main.rnc",
            SchemaSyntax::Compact,
        ))
        .expect("schema parses");
        crate::simplify(&document, &NoResolver).expect("schema simplifies")
    }

    fn xml_schema(text: &str) -> CompiledSchema {
        let document = crate::parse(&SchemaSource::new(
            text,
            "memory:/main.rng",
            SchemaSyntax::Xml,
        ))
        .expect("schema parses");
        crate::simplify(&document, &NoResolver).expect("schema simplifies")
    }

    fn registry() -> DatatypeRegistry {
        DatatypeRegistry::new()
    }

    #[derive(Clone, Debug)]
    struct TestElement {
        name: ExpandedName,
        attributes: Vec<(ExpandedName, String)>,
        children: Vec<Content<TestElement>>,
        namespace_bindings: Vec<(String, String)>,
    }

    impl TestElement {
        fn new(local: &str) -> Self {
            Self {
                name: ExpandedName::new(None::<String>, local),
                attributes: Vec::new(),
                children: Vec::new(),
                namespace_bindings: Vec::new(),
            }
        }

        fn new_namespaced(namespace: &str, local: &str) -> Self {
            Self {
                name: ExpandedName::new(Some(namespace), local),
                attributes: Vec::new(),
                children: Vec::new(),
                namespace_bindings: Vec::new(),
            }
        }

        fn attr(mut self, local: &str, value: &str) -> Self {
            self.attributes
                .push((ExpandedName::new(None::<String>, local), value.to_owned()));
            self
        }

        fn text(mut self, text: &str) -> Self {
            self.children.push(Content::Text(text.to_owned()));
            self
        }

        fn child(mut self, element: TestElement) -> Self {
            self.children.push(Content::Element(element));
            self
        }

        fn xmlns(mut self, prefix: &str, uri: &str) -> Self {
            self.namespace_bindings
                .push((prefix.to_owned(), uri.to_owned()));
            self
        }
    }

    impl Element for TestElement {
        fn name(&self) -> ExpandedName {
            self.name.clone()
        }
        fn attributes(&self) -> impl Iterator<Item = (ExpandedName, String)> {
            self.attributes.clone().into_iter()
        }
        fn children(&self) -> impl Iterator<Item = Content<Self>> {
            self.children.clone().into_iter()
        }
        fn namespace_bindings(&self) -> impl Iterator<Item = (String, String)> {
            self.namespace_bindings.clone().into_iter()
        }
    }

    fn kinds(errors: &[ValidationError]) -> Vec<ValidationErrorKind> {
        errors.iter().map(|error| error.kind().clone()).collect()
    }

    fn name(local: &str) -> ExpandedName {
        ExpandedName::new(None::<String>, local)
    }

    // --- nullable ---

    #[test]
    fn nullable_base_cases() {
        let definitions = Definitions::new();
        let registry = registry();
        assert!(nullable(&Pattern::Empty, &definitions, &registry));
        assert!(!nullable(&Pattern::NotAllowed, &definitions, &registry));
        assert!(nullable(&Pattern::Text, &definitions, &registry));
    }

    #[test]
    fn nullable_group_requires_every_operand_nullable() {
        let definitions = Definitions::new();
        let registry = registry();
        assert!(nullable(
            &Pattern::Group(vec![Pattern::Empty, Pattern::Text]),
            &definitions,
            &registry
        ));
        assert!(!nullable(
            &Pattern::Group(vec![Pattern::Empty, Pattern::NotAllowed]),
            &definitions,
            &registry
        ));
    }

    #[test]
    fn nullable_choice_requires_any_operand_nullable() {
        let definitions = Definitions::new();
        let registry = registry();
        assert!(nullable(
            &Pattern::Choice(vec![Pattern::NotAllowed, Pattern::Empty]),
            &definitions,
            &registry
        ));
        assert!(!nullable(
            &Pattern::Choice(vec![Pattern::NotAllowed, Pattern::NotAllowed]),
            &definitions,
            &registry
        ));
    }

    #[test]
    fn nullable_one_or_more_delegates_to_inner() {
        let definitions = Definitions::new();
        let registry = registry();
        assert!(nullable(
            &Pattern::OneOrMore(vec![Pattern::Empty]),
            &definitions,
            &registry
        ));
        assert!(!nullable(
            &Pattern::OneOrMore(vec![Pattern::NotAllowed]),
            &definitions,
            &registry
        ));
    }

    #[test]
    fn nullable_list_delegates_to_inner_same_as_zero_tokens_would() {
        let definitions = Definitions::new();
        let registry = registry();
        // `list { empty }` accepts having no text at all.
        assert!(nullable(
            &Pattern::List(vec![Pattern::Empty]),
            &definitions,
            &registry
        ));
        assert!(!nullable(
            &Pattern::List(vec![Pattern::NotAllowed]),
            &definitions,
            &registry
        ));
    }

    #[test]
    fn nullable_ref_resolves_through_definitions() {
        let definitions = Definitions::from([("x".to_owned(), Pattern::Empty)]);
        let registry = registry();
        assert!(nullable(&Pattern::Ref("x".into()), &definitions, &registry));
    }

    // --- individual derivative functions: element / attribute / text ---

    #[test]
    fn deriv_element_event_matches_by_name_and_rejects_others() {
        let compiled = schema("start = element foo { empty }");
        let registry = registry();
        assert!(nullable(
            &deriv_element_event(&compiled.start, &name("foo"), &compiled.definitions),
            &compiled.definitions,
            &registry
        ));
        assert!(matches!(
            deriv_element_event(&compiled.start, &name("bar"), &compiled.definitions),
            Pattern::NotAllowed
        ));
    }

    #[test]
    fn deriv_attribute_checks_name_and_value() {
        let compiled = schema(r#"start = element foo { attribute id { "x" } }"#);
        let registry = registry();
        let context = DatatypeContext::empty();
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        assert!(nullable(
            &deriv_attribute(
                &body[0],
                &name("id"),
                "x",
                &compiled.definitions,
                &registry,
                &context
            ),
            &compiled.definitions,
            &registry
        ));
        assert!(matches!(
            deriv_attribute(
                &body[0],
                &name("id"),
                "y",
                &compiled.definitions,
                &registry,
                &context
            ),
            Pattern::NotAllowed
        ));
        assert!(matches!(
            deriv_attribute(
                &body[0],
                &name("other"),
                "x",
                &compiled.definitions,
                &registry,
                &context
            ),
            Pattern::NotAllowed
        ));
    }

    #[test]
    fn deriv_text_data_accepts_any_string_value_requires_exact_match() {
        let registry = registry();
        let context = DatatypeContext::empty();
        let compiled = schema("start = element foo { token }");
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        assert!(nullable(
            &deriv_text(
                &body[0],
                "whatever text",
                &compiled.definitions,
                &registry,
                &context
            ),
            &compiled.definitions,
            &registry
        ));

        let compiled = schema(r#"start = element foo { "exact" }"#);
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        assert!(nullable(
            &deriv_text(
                &body[0],
                "exact",
                &compiled.definitions,
                &registry,
                &context
            ),
            &compiled.definitions,
            &registry
        ));
        assert!(matches!(
            deriv_text(
                &body[0],
                "different",
                &compiled.definitions,
                &registry,
                &context
            ),
            Pattern::NotAllowed
        ));
    }

    #[test]
    fn deriv_text_token_equality_collapses_whitespace() {
        let registry = registry();
        let context = DatatypeContext::empty();
        let compiled = schema(r#"start = element foo { token "a b" }"#);
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        assert!(nullable(
            &deriv_text(
                &body[0],
                "a   b",
                &compiled.definitions,
                &registry,
                &context
            ),
            &compiled.definitions,
            &registry
        ));
    }

    // --- `group` (sequential/"after") vs `interleave` (unordered) ---

    #[test]
    fn group_derivative_is_sequential() {
        let registry = registry();
        let compiled = schema("start = element root { element a { empty }, element b { empty } }");
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        let content = &body[0];
        // "b" cannot be matched before "a" — group is ordered.
        assert!(matches!(
            deriv_element_event(content, &name("b"), &compiled.definitions),
            Pattern::NotAllowed
        ));
        let after_a = deriv_element_event(content, &name("a"), &compiled.definitions);
        let after_a_b = deriv_element_event(&after_a, &name("b"), &compiled.definitions);
        assert!(nullable(&after_a_b, &compiled.definitions, &registry));
    }

    #[test]
    fn interleave_derivative_allows_either_order() {
        let registry = registry();
        let compiled = schema("start = element root { element a { empty } & element b { empty } }");
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        let content = &body[0];
        // Unlike `group`, "b" is allowed to come before "a".
        let after_b = deriv_element_event(content, &name("b"), &compiled.definitions);
        assert!(!matches!(after_b, Pattern::NotAllowed));
        let after_b_a = deriv_element_event(&after_b, &name("a"), &compiled.definitions);
        assert!(nullable(&after_b_a, &compiled.definitions, &registry));
    }

    // --- the xmloxide regression this crate exists to fix (issue #56):
    // `ref` inside `interleave` must resolve for *attribute* matching. ---

    #[test]
    fn attribute_matches_through_ref_and_interleave_xmloxide_issue_56() {
        let compiled = schema(
            "start = element foo { attrs & text }\n\
             attrs = attribute id { text }",
        );
        let document = TestElement::new("foo").attr("id", "x").text("hello");
        let errors = validate(&compiled, &registry(), &document).expect("builtin datatypes only");
        assert_eq!(kinds(&errors), Vec::<ValidationErrorKind>::new());
    }

    #[test]
    fn attribute_name_known_resolves_ref_through_group_and_choice_too() {
        // The same transparency, exercised for `group` (attribute-matching
        // semantics, unordered) and `choice`.
        let compiled = schema(
            "start = element foo { a, b }\n\
             a = attribute x { text } | attribute y { text }\n\
             b = empty",
        );
        let Pattern::Element { body, .. } = &compiled.start else {
            panic!("expected an element pattern");
        };
        assert!(attribute_name_known(
            &body[0],
            &name("x"),
            &compiled.definitions
        ));
        assert!(attribute_name_known(
            &body[0],
            &name("y"),
            &compiled.definitions
        ));
        assert!(!attribute_name_known(
            &body[0],
            &name("z"),
            &compiled.definitions
        ));
    }

    #[test]
    fn ref_is_transparent_for_child_elements_through_choice_and_group() {
        let compiled = schema(
            "start = element foo { greeting, farewell }\n\
             greeting = element hello { empty } | element hi { empty }\n\
             farewell = element bye { empty }",
        );
        let document = TestElement::new("foo")
            .child(TestElement::new("hi"))
            .child(TestElement::new("bye"));
        let errors = validate(&compiled, &registry(), &document).expect("builtin datatypes only");
        assert_eq!(kinds(&errors), Vec::<ValidationErrorKind>::new());
    }

    // --- end-to-end `validate()`: hand-picked valid/invalid document pairs ---

    #[test]
    fn valid_document_with_required_attribute_and_text() {
        let compiled = schema("start = element foo { attribute id { text }, text }");
        let document = TestElement::new("foo").attr("id", "1").text("hello");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn missing_required_attribute_is_reported() {
        let compiled = schema("start = element foo { attribute id { text }, text }");
        let document = TestElement::new("foo").text("hello");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(kinds(&errors), vec![ValidationErrorKind::MissingContent]);
    }

    #[test]
    fn unexpected_attribute_is_reported() {
        let compiled = schema("start = element foo { empty }");
        let document = TestElement::new("foo").attr("id", "1");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(
            kinds(&errors),
            vec![ValidationErrorKind::UnexpectedAttribute(name("id"))]
        );
    }

    #[test]
    fn invalid_attribute_value_is_reported_distinctly_from_unexpected_attribute() {
        let compiled = schema(r#"start = element foo { attribute id { "x" } }"#);
        let document = TestElement::new("foo").attr("id", "y");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        // The mismatched attribute is left unconsumed (so later attributes
        // keep being checked against the schema rather than being drowned
        // out by one bad value), which is also, truthfully, "required
        // content still missing" once the element ends.
        assert_eq!(
            kinds(&errors),
            vec![
                ValidationErrorKind::InvalidAttributeValue(name("id"), "y".to_owned()),
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn unexpected_child_element_is_reported() {
        let compiled = schema("start = element foo { element bar { empty } }");
        let document = TestElement::new("foo").child(TestElement::new("baz"));
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(
            kinds(&errors),
            vec![
                ValidationErrorKind::UnexpectedElement(name("baz")),
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn unexpected_root_element_is_reported() {
        let compiled = schema("start = element foo { empty }");
        let document = TestElement::new("bar");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(
            kinds(&errors),
            vec![ValidationErrorKind::UnexpectedElement(name("bar"))]
        );
    }

    #[test]
    fn unexpected_text_is_reported() {
        let compiled = schema("start = element foo { empty }");
        let document = TestElement::new("foo").text("surprise");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(kinds(&errors), vec![ValidationErrorKind::UnexpectedText]);
    }

    #[test]
    fn one_or_more_children_are_validated_recursively_and_repeatedly() {
        let compiled = schema("start = element foo { element item { text }+ }");
        let document = TestElement::new("foo")
            .child(TestElement::new("item").text("a"))
            .child(TestElement::new("item").text("b"));
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn one_or_more_requires_at_least_one_occurrence() {
        let compiled = schema("start = element foo { element item { empty }+ }");
        let document = TestElement::new("foo");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(kinds(&errors), vec![ValidationErrorKind::MissingContent]);
    }

    #[test]
    fn interleaved_text_and_element_children_validate_regardless_of_order() {
        let compiled = schema("start = element foo { mixed { element bar { empty } } }");
        let document = TestElement::new("foo")
            .text("before ")
            .child(TestElement::new("bar"))
            .text(" after");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn list_datatype_matches_whitespace_separated_tokens() {
        let compiled = schema("start = element foo { list { token+ } }");
        let document = TestElement::new("foo").text("a b   c");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn list_datatype_rejects_a_token_that_does_not_match() {
        let compiled = schema(r#"start = element foo { list { "a", "b" } }"#);
        let document = TestElement::new("foo").text("a c");
        let errors = validate(&compiled, &registry(), &document).unwrap();
        assert_eq!(
            kinds(&errors),
            vec![
                ValidationErrorKind::UnexpectedText,
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn unsupported_datatype_library_fails_fast_before_validating() {
        // Deliberately the whattf library, not XSD — `DatatypeRegistry::new`
        // now supports XSD out of the box (see `plan/DECISIONS.md`), so
        // this needs a genuinely unregistered library to still exercise
        // the fail-fast path; the whattf library is exactly that (a
        // caller like html-conform must register its own implementation).
        // XML syntax, not compact: compact-syntax identifiers don't accept
        // `-`, and `non-empty-string` is vnu's real type name.
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo">
                 <data type="non-empty-string" datatypeLibrary="http://whattf.org/datatype-draft"/>
               </element>"#,
        );
        let document = TestElement::new("foo").text("hello");
        let error = validate(&compiled, &registry(), &document).unwrap_err();
        assert_eq!(error.library(), "http://whattf.org/datatype-draft");
    }

    #[test]
    fn builtin_only_registry_rejects_xsd_too() {
        let compiled = schema(
            "datatypes xsd = \"http://www.w3.org/2001/XMLSchema-datatypes\"\n\
             start = element foo { xsd:string }",
        );
        let document = TestElement::new("foo").text("hello");
        let error = validate(&compiled, &DatatypeRegistry::builtin_only(), &document).unwrap_err();
        assert_eq!(
            error.library(),
            "http://www.w3.org/2001/XMLSchema-datatypes"
        );
    }

    // --- XSD, via the default registry ---

    #[test]
    fn xsd_string_and_integer_types_match() {
        // Note: RNG XML's `type` attribute is always a bare string, never a
        // namespace-prefixed QName — the library comes from the separate
        // `datatypeLibrary` attribute (or §4.3 inheritance from an
        // ancestor), unlike compact syntax's `prefix:typeName` sugar.
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes">
                 <group>
                   <attribute name="count"><data type="positiveInteger"/></attribute>
                   <data type="string"/>
                 </group>
               </element>"#,
        );
        let valid = TestElement::new("foo").attr("count", "3").text("hello");
        assert!(validate(&compiled, &registry(), &valid).unwrap().is_empty());

        let invalid = TestElement::new("foo").attr("count", "0").text("hello");
        let errors = validate(&compiled, &registry(), &invalid).unwrap();
        assert_eq!(
            kinds(&errors),
            vec![
                ValidationErrorKind::InvalidAttributeValue(name("count"), "0".to_owned()),
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn xsd_pattern_facet_is_enforced() {
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes">
                 <data type="string"><param name="pattern">[a-z]+</param></data>
               </element>"#,
        );
        let valid = TestElement::new("foo").text("abc");
        assert!(validate(&compiled, &registry(), &valid).unwrap().is_empty());
        let invalid = TestElement::new("foo").text("ABC");
        assert_eq!(
            kinds(&validate(&compiled, &registry(), &invalid).unwrap()),
            vec![
                ValidationErrorKind::UnexpectedText,
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn xsd_decimal_value_equality_ignores_lexical_differences() {
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" xmlns:xsd="http://www.w3.org/2001/XMLSchema-datatypes" name="foo">
                 <value type="decimal" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes">1.50</value>
               </element>"#,
        );
        // `1.5` and `01.50` are the same `decimal` value, just spelled
        // differently — real bignum-style equality, not string equality.
        let document = TestElement::new("foo").text("01.500");
        assert!(
            validate(&compiled, &registry(), &document)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn xsd_qname_resolves_prefix_via_document_namespace_bindings() {
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo">
                 <data type="QName" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes"/>
               </element>"#,
        );
        let document = TestElement::new("foo")
            .xmlns("h", "urn:example:h")
            .text("h:widget");
        assert!(
            validate(&compiled, &registry(), &document)
                .unwrap()
                .is_empty()
        );

        // No `h` binding in scope at all — the QName can't be resolved, so
        // it isn't a legal QName *value* here even though it's lexically
        // shaped like one.
        let unresolved = TestElement::new("foo").text("h:widget");
        assert_eq!(
            kinds(&validate(&compiled, &registry(), &unresolved).unwrap()),
            vec![
                ValidationErrorKind::UnexpectedText,
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    #[test]
    fn xsd_unknown_type_name_is_a_datatype_error() {
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo" datatypeLibrary="http://www.w3.org/2001/XMLSchema-datatypes">
                 <data type="not-a-real-type"/>
               </element>"#,
        );
        let document = TestElement::new("foo").text("hello");
        let error = validate(&compiled, &registry(), &document).unwrap_err();
        assert_eq!(
            error.library(),
            "http://www.w3.org/2001/XMLSchema-datatypes"
        );
    }

    // --- external plugin: a dummy library, demonstrating the trait
    // contract a caller like html-conform would implement for vnu's
    // `http://whattf.org/datatype-draft` (not shipped by this crate). ---

    struct DummyNonEmptyStringLibrary;

    impl DatatypeLibrary for DummyNonEmptyStringLibrary {
        fn validate_params(&self, type_name: &str, params: &[(&str, &str)]) -> Result<(), String> {
            if type_name != "non-empty-string" {
                return Err(format!("unknown dummy type `{type_name}`"));
            }
            if let Some((param, _)) = params.first() {
                return Err(format!(
                    "`non-empty-string` takes no parameters, found `{param}`"
                ));
            }
            Ok(())
        }

        fn matches(
            &self,
            type_name: &str,
            _params: &[(&str, &str)],
            value: &str,
            _context: &DatatypeContext,
        ) -> bool {
            type_name == "non-empty-string" && !value.is_empty()
        }

        fn values_equal(
            &self,
            type_name: &str,
            expected: &str,
            _expected_context: &DatatypeContext,
            actual: &str,
            _actual_context: &DatatypeContext,
        ) -> bool {
            type_name == "non-empty-string" && expected == actual
        }
    }

    #[test]
    fn external_plugin_library_can_be_registered_and_used() {
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="foo">
                 <data type="non-empty-string" datatypeLibrary="http://whattf.org/datatype-draft"/>
               </element>"#,
        );
        let mut registry = DatatypeRegistry::new();
        registry.register(
            "http://whattf.org/datatype-draft",
            DummyNonEmptyStringLibrary,
        );

        let valid = TestElement::new("foo").text("hello");
        assert!(validate(&compiled, &registry, &valid).unwrap().is_empty());

        // An empty text *event* against a bare `data` isn't excused by
        // §6.2.7 weak-match rule 2: that rule only drops a whitespace-only
        // event when the content seen *so far* already, strictly,
        // satisfies the pattern without it — a bare `data`/`value` never
        // does (it structurally requires a real event), so this is
        // reported directly, same as a genuinely non-empty mismatch would
        // be.
        let invalid = TestElement::new("foo").text("");
        assert_eq!(
            kinds(&validate(&compiled, &registry, &invalid).unwrap()),
            vec![
                ValidationErrorKind::UnexpectedText,
                ValidationErrorKind::MissingContent,
            ]
        );
    }

    // --- adapter smoke test: a realistic html-conform/xmloxide-shaped tree
    // (namespace-qualified elements, an unprefixed attribute, nested
    // children, text) — this crate has no dependency on html-conform or
    // xmloxide (no HTML/XML-parser dependency in the core, see
    // `plan/DECISIONS.md`), so `TestElement` stands in for its sibling
    // project's `NormalizedNode` adapter output, confirming the `Element`
    // trait contract works for that shape before the Public-API phase. ---

    #[test]
    fn adapter_smoke_test_with_a_realistic_xhtml_shaped_tree() {
        const XHTML: &str = "http://www.w3.org/1999/xhtml";
        // Compact-syntax (`.rnc`) prefixed element names are a known,
        // separate gap outside Phase 05's scope (see `plan/00-STATUS.md`)
        // — the XML (`.rng`) syntax resolves `xmlns:`-qualified names
        // correctly, so this realistic, namespace-qualified smoke test
        // uses that syntax instead.
        let compiled = xml_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" xmlns:xhtml="http://www.w3.org/1999/xhtml" name="xhtml:html">
                 <group>
                   <element name="xhtml:head"><empty/></element>
                   <element name="xhtml:body">
                     <element name="xhtml:p">
                       <group>
                         <attribute name="id"><text/></attribute>
                         <text/>
                       </group>
                     </element>
                   </element>
                 </group>
               </element>"#,
        );
        let document = TestElement::new_namespaced(XHTML, "html")
            .child(TestElement::new_namespaced(XHTML, "head"))
            .child(
                TestElement::new_namespaced(XHTML, "body").child(
                    TestElement::new_namespaced(XHTML, "p")
                        .attr("id", "intro")
                        .text("hello"),
                ),
            );
        let errors = validate(&compiled, &registry(), &document).expect("builtin datatypes only");
        assert_eq!(kinds(&errors), Vec::<ValidationErrorKind>::new());
    }
}
