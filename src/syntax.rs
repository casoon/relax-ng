use std::collections::BTreeMap;
use std::fmt;

use roxmltree::Node;

use crate::grammar::{
    Combine, Context, Grammar, GrammarItem, NameClass, Param, Pattern, Root, SchemaDocument,
};
use crate::{SchemaSource, SchemaSyntax};

const RNG_NAMESPACE: &str = "http://relaxng.org/ns/structure/1.0";

/// A schema's text isn't well-formed RELAX NG in its declared
/// [`SchemaSyntax`] — malformed XML/compact-syntax tokens, an unknown
/// element/attribute, a structural violation the syntax layer itself
/// checks (wrong child count, an `attribute`/`start` with more than one
/// pattern child, ...).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parses `source`'s text (in its declared [`SchemaSyntax`]) into a
/// [`SchemaDocument`] — this step alone doesn't resolve `include`/
/// `externalRef` or check most of RELAX NG's composition-time
/// restrictions; pass the result to [`crate::simplify`] for that (or use
/// [`crate::Schema::compile`], which does both steps).
pub fn parse(source: &SchemaSource) -> Result<SchemaDocument, ParseError> {
    parse_with_inherited_ns(source, None)
}

/// Like [`parse`], but for `externalRef`/`include`-referenced content
/// specifically: §4.6/§4.7 have the referencing element's own (already
/// §4.9-resolved) `ns` transfer to the referenced content's root as a
/// fallback — used only if that root doesn't set its own `ns` — exactly
/// as if the reference were replaced in-place by the referenced content.
/// `datatypeLibrary` deliberately does *not* transfer this way (per spec),
/// so this only ever affects `ns`.
pub(crate) fn parse_with_inherited_ns(
    source: &SchemaSource,
    inherited_ns: Option<String>,
) -> Result<SchemaDocument, ParseError> {
    match source.syntax() {
        SchemaSyntax::Xml => parse_xml(source, inherited_ns),
        SchemaSyntax::Compact => CompactParser::new(source).parse(),
    }
}

/// The bare `Context` a schema's syntax layer starts from, before any
/// element's own attributes (`xml_context`/`CompactParser::declaration`)
/// narrow it further.
fn initial_context(
    source: &SchemaSource,
    datatype_library: Option<String>,
    ns: Option<String>,
) -> Context {
    Context {
        base_uri: source.base_uri().to_owned(),
        namespaces: BTreeMap::new(),
        default_namespace: None,
        datatype_library,
        ns,
    }
}

fn parse_xml(
    source: &SchemaSource,
    inherited_ns: Option<String>,
) -> Result<SchemaDocument, ParseError> {
    let document = roxmltree::Document::parse(source.text())
        .map_err(|error| ParseError::new(format!("invalid XML schema: {error}")))?;
    let root = document.root_element();
    if root.tag_name().namespace() != Some(RNG_NAMESPACE) {
        return Err(ParseError::new(
            "schema root must use the RELAX NG namespace",
        ));
    }
    // §4.3: the default when no ancestor sets `datatypeLibrary` is the
    // empty string, not "unset" — the built-in datatype library. §4.9: no
    // ancestor to inherit `ns` from yet, other than what an
    // externalRef/include passed in — the root element's own `ns` (if any)
    // still wins over this, picked up by `xml_context` below.
    let root_context = initial_context(source, Some(String::new()), inherited_ns);
    let context = xml_context(root, &root_context)?;
    let parsed = match root.tag_name().name() {
        "grammar" => Root::Grammar(xml_grammar(root, context)?),
        _ => Root::Pattern(xml_pattern(root, context)?),
    };
    Ok(SchemaDocument { root: parsed })
}

fn xml_context(node: Node<'_, '_>, parent: &Context) -> Result<Context, ParseError> {
    let mut namespaces = BTreeMap::new();
    let mut default_namespace = None;
    for namespace in node.namespaces() {
        let uri = namespace.uri().to_owned();
        match namespace.name() {
            Some(prefix) => {
                namespaces.insert(prefix.to_owned(), uri);
            }
            None => default_namespace = Some(uri),
        }
    }
    namespaces
        .entry("xml".to_owned())
        .or_insert_with(|| "http://www.w3.org/XML/1998/namespace".to_owned());
    // §4.3: this node's own `datatypeLibrary` wins; otherwise inherit the
    // nearest ancestor's (which may itself already be inherited).
    let datatype_library = match token_attribute(node, "datatypeLibrary") {
        Some(value) => {
            if !is_valid_datatype_library_uri(&value) {
                return Err(ParseError::new(format!(
                    "`datatypeLibrary` must be an absolute URI with no fragment identifier, got `{value}`"
                )));
            }
            Some(value)
        }
        None => parent.datatype_library.clone(),
    };
    // §4.9: this node's own `ns` wins; otherwise inherit the nearest
    // ancestor's (which may itself already be inherited, or `None` if no
    // ancestor ever set one — resolved to "" wherever `ns` is used).
    let ns = token_attribute(node, "ns").or_else(|| parent.ns.clone());
    // XML Base: `xml:base` establishes a new base URI for this node and
    // its descendants, resolved against the nearest ancestor's — needed
    // for `include`/`externalRef` `href`s nested under it to resolve
    // correctly (the actual `href` resolution itself is still entirely
    // the `SchemaResolver`'s job; this only tracks *which* base a given
    // node's `href` should be resolved against).
    let base_uri = match node.attribute(("http://www.w3.org/XML/1998/namespace", "base")) {
        Some(value) => resolve_relative_reference(&parent.base_uri, value.trim()),
        None => parent.base_uri.clone(),
    };
    Ok(Context {
        base_uri,
        namespaces,
        default_namespace,
        datatype_library,
        ns,
    })
}

/// A minimal RFC 3986 §5.3-style merge for `xml:base` chains: an absolute
/// `reference` (one with a `scheme:` prefix) replaces `base` outright;
/// otherwise `reference` is appended to `base`'s directory (everything up
/// to and including the last `/`, or nothing if `base` has none). This
/// crate has no opinion on URI schemes beyond that — a `SchemaResolver`
/// resolving a relative `href` against the resulting base does the same
/// kind of merge itself, using whatever scheme-specific rules apply.
fn resolve_relative_reference(base: &str, reference: &str) -> String {
    let is_absolute = reference
        .split_once(':')
        .is_some_and(|(scheme, _)| is_valid_uri_scheme(scheme));
    if is_absolute {
        return reference.to_owned();
    }
    let directory = match base.rfind('/') {
        Some(index) => &base[..=index],
        None => "",
    };
    format!("{directory}{reference}")
}

/// RFC 2396 §3: `datatypeLibrary` (when not the empty-string built-in-
/// library sentinel) must be an `absoluteURI` — `scheme ":" (hier_part |
/// opaque_part)` — with no fragment identifier.
fn is_valid_datatype_library_uri(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let Some((scheme, rest)) = value.split_once(':') else {
        return false; // no scheme => a relative reference, not absolute
    };
    is_valid_uri_scheme(scheme)
        && !rest.is_empty()
        && !rest.contains('#')
        && is_valid_uri_rest(rest)
}

fn is_valid_uri_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Validates the part of a URI after `scheme ":"` — permissively, against
/// the union of `hier_part`'s and `opaque_part`'s `uric` character sets
/// (RFC 2396 doesn't need us to tell which of the two productions applies
/// here, just that every character is legal in *some* URI position), with
/// `%` escapes required to be exactly two hex digits.
fn is_valid_uri_rest(rest: &str) -> bool {
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let (Some(a), Some(b)) = (chars.next(), chars.next()) else {
                return false;
            };
            if !a.is_ascii_hexdigit() || !b.is_ascii_hexdigit() {
                return false;
            }
        } else if !is_uric_char(c) {
            return false;
        }
    }
    true
}

fn is_uric_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '_'
                | '.'
                | '!'
                | '~'
                | '*'
                | '\''
                | '('
                | ')'
                | ';'
                | '/'
                | '?'
                | ':'
                | '@'
                | '&'
                | '='
                | '+'
                | '$'
                | ','
        )
}

fn rng_children<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children()
        .filter(|child| child.is_element() && child.tag_name().namespace() == Some(RNG_NAMESPACE))
}

fn attribute(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.attribute(name).map(str::to_owned)
}

/// `name`, `ns` and `combine` are token-typed (NCName/anyURI-like)
/// attributes — per XML attribute-value normalization their leading and
/// trailing whitespace must be stripped before comparison.
fn token_attribute(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.attribute(name).map(|value| value.trim().to_owned())
}

fn require_attribute_value(value: Option<String>, name: &str) -> Result<String, ParseError> {
    value.ok_or_else(|| ParseError::new(format!("missing required `{name}` attribute")))
}

fn required_token_attribute(node: Node<'_, '_>, name: &str) -> Result<String, ParseError> {
    require_attribute_value(token_attribute(node, name), name)
}

/// The required `name` attribute, additionally checked to be a valid
/// NCName under `label` — `ref`/`parentRef`/`define` all resolve their
/// identifying name this way.
fn required_ncname_attribute(node: Node<'_, '_>, label: &str) -> Result<String, ParseError> {
    let name = required_token_attribute(node, "name")?;
    require_ncname(&name, label)?;
    Ok(name)
}

/// Approximates the XML Names `NCName` production (`(Letter|'_') (Letter |
/// Digit | '.' | '-' | '_' | CombiningChar | Extender)*`) with Unicode's
/// XID_Start/XID_Continue (`unicode-ident`, the same crate rustc itself
/// uses for identifiers) rather than XML 1.0's own (dated, Unicode 2.0)
/// character tables — crucially, unlike `char::is_alphabetic`, XID_Start
/// correctly excludes combining marks (e.g. Thai vowel signs), which are
/// only valid as non-initial characters.
pub(crate) fn is_valid_ncname(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if unicode_ident::is_xid_start(first) || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| unicode_ident::is_xid_continue(c) || c == '-' || c == '.')
}

fn require_ncname(value: &str, label: &str) -> Result<(), ParseError> {
    if is_valid_ncname(value) {
        Ok(())
    } else {
        Err(ParseError::new(format!("`{value}` is not a valid {label}")))
    }
}

/// Rejects any attribute on `node` that isn't in `allowed`. `ns` and
/// `datatypeLibrary` are "common attributes" the spec allows on *every*
/// element regardless of `allowed`. A "foreign attribute" — one the spec
/// always permits, for annotation purposes — is one whose namespace is
/// neither absent (unprefixed) *nor the RELAX NG namespace itself*: an
/// attribute explicitly in the RNG namespace (however prefixed) is still
/// RNG's own attribute set and must be checked against `allowed`.
/// Builds a `ParseError` in the `` `<tag>` <message> `` shape every
/// structural check below reports — `node`'s own tag name is filled in
/// automatically.
fn tag_error(node: Node<'_, '_>, message: impl fmt::Display) -> ParseError {
    ParseError::new(format!("`<{}>` {message}", node.tag_name().name()))
}

fn reject_unknown_attributes(node: Node<'_, '_>, allowed: &[&str]) -> Result<(), ParseError> {
    for attr in node.attributes() {
        let name = attr.name();
        let is_foreign = matches!(attr.namespace(), Some(namespace) if namespace != RNG_NAMESPACE);
        if is_foreign || name == "ns" || name == "datatypeLibrary" || allowed.contains(&name) {
            continue;
        }
        return Err(tag_error(
            node,
            format!("does not allow a `{name}` attribute"),
        ));
    }
    Ok(())
}

fn require_at_least_one_child(node: Node<'_, '_>) -> Result<(), ParseError> {
    if rng_children(node).next().is_none() {
        return Err(tag_error(node, "requires at least one child"));
    }
    Ok(())
}

fn reject_children(node: Node<'_, '_>) -> Result<(), ParseError> {
    if rng_children(node).next().is_some() {
        return Err(tag_error(node, "must not have children"));
    }
    Ok(())
}

/// Like `reject_children`, but for the handful of elements (`name`,
/// `value`, `param`) whose content is plain text — unlike every other RNG
/// element, these have no "annotation slot", so even a *foreign* child
/// element (which `reject_children`/`rng_children` would silently ignore
/// elsewhere) is invalid here.
fn reject_all_children(node: Node<'_, '_>) -> Result<(), ParseError> {
    if node.children().any(|child| child.is_element()) {
        return Err(tag_error(node, "must not have child elements"));
    }
    Ok(())
}

/// `reject_unknown_attributes` followed by `require_at_least_one_child` —
/// the pairing every container element (`group`, `start`, `define`, an
/// `except`'s own body, ...) starts with.
fn require_children(node: Node<'_, '_>, allowed: &[&str]) -> Result<(), ParseError> {
    reject_unknown_attributes(node, allowed)?;
    require_at_least_one_child(node)
}

/// `reject_unknown_attributes` followed by `reject_children` — the pairing
/// every childless leaf element (`empty`, `text`, `ref`, ...) starts with.
fn require_no_children(node: Node<'_, '_>, allowed: &[&str]) -> Result<(), ParseError> {
    reject_unknown_attributes(node, allowed)?;
    reject_children(node)
}

/// `reject_unknown_attributes` followed by `reject_all_children` — for the
/// plain-text elements (`name`, `value`, `param`).
fn require_no_child_elements(node: Node<'_, '_>, allowed: &[&str]) -> Result<(), ParseError> {
    reject_unknown_attributes(node, allowed)?;
    reject_all_children(node)
}

fn xml_pattern_seq<'a, 'input: 'a>(
    children: impl Iterator<Item = Node<'a, 'input>>,
    context: &Context,
) -> Result<Vec<Pattern>, ParseError> {
    children
        .map(|child| xml_pattern(child, xml_context(child, context)?))
        .collect()
}

fn xml_patterns(node: Node<'_, '_>, context: &Context) -> Result<Vec<Pattern>, ParseError> {
    xml_pattern_seq(rng_children(node), context)
}

/// Parses an `except` child (at most one is allowed, and it must itself
/// have at least one child) shared by `data`'s and `anyName`/`nsName`'s
/// name classes — `too_many` builds the error for a second `except`,
/// `parse_child` parses each of its children (`xml_pattern` or
/// `xml_name_class`, respectively).
fn xml_except<'a, 'input: 'a, T>(
    node: Node<'a, 'input>,
    context: &Context,
    too_many: impl FnOnce() -> ParseError,
    parse_child: impl Fn(Node<'a, 'input>, Context) -> Result<T, ParseError>,
) -> Result<Vec<T>, ParseError> {
    let mut except_children =
        rng_children(node).filter(|child| child.tag_name().name() == "except");
    match (except_children.next(), except_children.next()) {
        (None, _) => Ok(Vec::new()),
        (Some(only), None) => {
            require_children(only, &[])?;
            rng_children(only)
                .map(|child| parse_child(child, xml_context(child, context)?))
                .collect()
        }
        (Some(_), Some(_)) => Err(too_many()),
    }
}

/// §4.9: `value` and `nsName` both fall back to the inherited `ns` when
/// they don't set their own.
fn inherited_ns_attribute(node: Node<'_, '_>, context: &Context) -> Option<String> {
    Some(token_attribute(node, "ns").unwrap_or_else(|| context.ns.clone().unwrap_or_default()))
}

fn xml_pattern(node: Node<'_, '_>, context: Context) -> Result<Pattern, ParseError> {
    let children = || xml_patterns(node, &context);
    let container = || -> Result<Vec<Pattern>, ParseError> {
        require_children(node, &[])?;
        children()
    };
    match node.tag_name().name() {
        "element" => {
            let (name, body) = xml_named_pattern(node, &context, None)?;
            Ok(Pattern::Element {
                name,
                body,
                context,
            })
        }
        "attribute" => {
            let (name, body) = xml_named_pattern(node, &context, Some(Pattern::Text))?;
            Ok(Pattern::Attribute {
                name,
                body,
                context,
            })
        }
        "group" => Ok(Pattern::Group(container()?)),
        "interleave" => Ok(Pattern::Interleave(container()?)),
        "choice" => Ok(Pattern::Choice(container()?)),
        "optional" => Ok(Pattern::Optional(container()?)),
        "zeroOrMore" => Ok(Pattern::ZeroOrMore(container()?)),
        "oneOrMore" => Ok(Pattern::OneOrMore(container()?)),
        "list" => Ok(Pattern::List(container()?)),
        "mixed" => Ok(Pattern::Mixed(container()?)),
        "empty" => {
            require_no_children(node, &[])?;
            Ok(Pattern::Empty)
        }
        "notAllowed" => {
            require_no_children(node, &[])?;
            Ok(Pattern::NotAllowed)
        }
        "text" => {
            require_no_children(node, &[])?;
            Ok(Pattern::Text)
        }
        "ref" => {
            require_no_children(node, &["name"])?;
            Ok(Pattern::Ref(required_ncname_attribute(node, "ref name")?))
        }
        "parentRef" => {
            require_no_children(node, &["name"])?;
            Ok(Pattern::ParentRef(required_ncname_attribute(
                node,
                "parentRef name",
            )?))
        }
        "externalRef" => {
            require_no_children(node, &["href"])?;
            Ok(Pattern::ExternalRef {
                href: required_attribute(node, "href")?,
                inherit_namespace: token_attribute(node, "ns"),
                context,
            })
        }
        "grammar" => Ok(Pattern::Grammar(xml_grammar(node, context)?)),
        "data" => {
            reject_unknown_attributes(node, &["type"])?;
            let params = rng_children(node)
                .filter(|child| child.tag_name().name() == "param")
                .map(|child| {
                    require_no_child_elements(child, &["name"])?;
                    Ok(Param {
                        name: required_attribute(child, "name")?,
                        value: child.text().unwrap_or_default().to_owned(),
                        context: xml_context(child, &context)?,
                    })
                })
                .collect::<Result<Vec<_>, ParseError>>()?;
            let except = xml_except(
                node,
                &context,
                || ParseError::new("`<data>` may have at most one `except`"),
                xml_pattern,
            )?;
            Ok(Pattern::Data {
                datatype: required_token_attribute(node, "type")?,
                // §4.3: `context.datatype_library` already resolved this
                // node's own attribute (if any) against its ancestors'.
                datatype_library: context.datatype_library.clone(),
                params,
                except,
                context,
            })
        }
        "value" => {
            require_no_child_elements(node, &["type"])?;
            let datatype = token_attribute(node, "type");
            // §4.4: a `value` with no `type` attribute defaults to `token`
            // in the *built-in* (empty-string) library, regardless of any
            // `datatypeLibrary` in scope — matching the compact-syntax
            // bare-string-literal case, which already does this.
            let datatype_library = if datatype.is_some() {
                context.datatype_library.clone()
            } else {
                None
            };
            // §4.9: `value` inherits `ns` like `name`/`nsName` do.
            let namespace = inherited_ns_attribute(node, &context);
            Ok(Pattern::Value {
                value: node.text().unwrap_or_default().to_owned(),
                datatype,
                datatype_library,
                namespace,
                context,
            })
        }
        name => Err(ParseError::new(format!(
            "unsupported RELAX NG pattern `{name}`"
        ))),
    }
}

/// Looks `prefix` up in `namespaces`, or fails with `error_prefix` (e.g.
/// `"unknown namespace prefix"`) followed by the prefix itself.
fn resolve_namespace(
    namespaces: &BTreeMap<String, String>,
    prefix: &str,
    error_prefix: &str,
) -> Result<String, ParseError> {
    namespaces
        .get(prefix)
        .cloned()
        .ok_or_else(|| ParseError::new(format!("{error_prefix} `{prefix}`")))
}

/// §4.10 QNames: a `name`/shorthand `name=` value of the form `prefix:local`
/// resolves `prefix` against the in-scope namespace bindings — unless an
/// explicit `ns` attribute is already present, which wins outright.
/// `unqualified_default` is what an unprefixed name with no explicit `ns`
/// resolves to — normally the §4.9-inherited `ns`, except §4.8's special
/// case for the `attribute` element's `name=` shorthand (see its caller).
fn resolve_name_lexical(
    lexical: String,
    explicit_ns: Option<String>,
    unqualified_default: Option<String>,
    context: &Context,
) -> Result<(String, Option<String>), ParseError> {
    if explicit_ns.is_some() {
        return Ok((lexical, explicit_ns));
    }
    match lexical.split_once(':') {
        Some((prefix, local)) => {
            let uri = resolve_namespace(&context.namespaces, prefix, "unknown namespace prefix")?;
            Ok((local.to_owned(), Some(uri)))
        }
        None => Ok((lexical, Some(unqualified_default.unwrap_or_default()))),
    }
}

fn xml_named_pattern(
    node: Node<'_, '_>,
    context: &Context,
    // §4.12: an `attribute` with only a name class gets an implicit `text`
    // body; `element` has no such default and must spell out its content.
    empty_body_default: Option<Pattern>,
) -> Result<(NameClass, Vec<Pattern>), ParseError> {
    reject_unknown_attributes(node, &["name"])?;
    // §4.12 groups multi-child bodies for `element` (and several other
    // elements) automatically, but says nothing of the kind for
    // `attribute` — only the empty-body-defaults-to-`text` case is
    // defined. `empty_body_default` is only `Some` for `attribute`, so it
    // doubles as that discriminator here.
    let finish_body = |body: Vec<Pattern>| -> Result<Vec<Pattern>, ParseError> {
        if empty_body_default.is_some() && body.len() > 1 {
            return Err(ParseError::new(format!(
                "`<{}>` must have at most one pattern child",
                node.tag_name().name()
            )));
        }
        if !body.is_empty() {
            return Ok(body);
        }
        empty_body_default
            .clone()
            .map(|pattern| vec![pattern])
            .ok_or_else(|| {
                ParseError::new(format!(
                    "`<{}>` requires at least one pattern child",
                    node.tag_name().name()
                ))
            })
    };
    if let Some(name) = token_attribute(node, "name") {
        // §4.8: the `name=` shorthand on `attribute` (unlike `element`)
        // desugars an unprefixed, `ns`-less name to `ns=""` — no `ns`
        // inheritance, unlike the `<name>` child-element form.
        let unqualified_default = if empty_body_default.is_some() {
            Some(String::new())
        } else {
            context.ns.clone()
        };
        let (lexical, namespace) = resolve_name_lexical(
            name,
            token_attribute(node, "ns"),
            unqualified_default,
            context,
        )?;
        require_ncname(&lexical, "name")?;
        return Ok((
            NameClass::Name {
                lexical,
                namespace,
                context: context.clone(),
            },
            finish_body(xml_patterns(node, context)?)?,
        ));
    }
    let mut children = rng_children(node);
    let name = children
        .next()
        .ok_or_else(|| ParseError::new("element or attribute requires a name class"))?;
    let name = xml_name_class(name, xml_context(name, context)?)?;
    let body = xml_pattern_seq(children, context)?;
    Ok((name, finish_body(body)?))
}

fn xml_name_class_seq<'a, 'input: 'a>(
    children: impl Iterator<Item = Node<'a, 'input>>,
    context: &Context,
) -> Result<Vec<NameClass>, ParseError> {
    children
        .map(|child| xml_name_class(child, xml_context(child, context)?))
        .collect()
}

fn xml_name_class(node: Node<'_, '_>, context: Context) -> Result<NameClass, ParseError> {
    match node.tag_name().name() {
        "name" => {
            require_no_child_elements(node, &[])?;
            let raw = node.text().unwrap_or_default().trim().to_owned();
            let (lexical, namespace) = resolve_name_lexical(
                raw,
                token_attribute(node, "ns"),
                context.ns.clone(),
                &context,
            )?;
            require_ncname(&lexical, "name")?;
            Ok(NameClass::Name {
                lexical,
                namespace,
                context,
            })
        }
        "anyName" | "nsName" => {
            let allowed: &[&str] = if node.tag_name().name() == "nsName" {
                &["ns"]
            } else {
                &[]
            };
            reject_unknown_attributes(node, allowed)?;
            let except = xml_except(
                node,
                &context,
                || {
                    ParseError::new(format!(
                        "`{}` may have at most one `except`",
                        node.tag_name().name()
                    ))
                },
                xml_name_class,
            )?;
            if node.tag_name().name() == "anyName" {
                Ok(NameClass::Any { except, context })
            } else {
                // §4.9: `nsName` inherits `ns` like `name`/`value` do.
                let namespace = inherited_ns_attribute(node, &context);
                Ok(NameClass::Namespace {
                    namespace,
                    except,
                    context,
                })
            }
        }
        "choice" => {
            require_children(node, &[])?;
            Ok(NameClass::Choice(xml_name_class_seq(
                rng_children(node),
                &context,
            )?))
        }
        name => Err(ParseError::new(format!(
            "unsupported RELAX NG name class `{name}`"
        ))),
    }
}

fn xml_grammar_item_seq<'a, 'input: 'a>(
    children: impl Iterator<Item = Node<'a, 'input>>,
    context: &Context,
) -> Result<Vec<GrammarItem>, ParseError> {
    children
        .map(|child| xml_grammar_item(child, xml_context(child, context)?))
        .collect()
}

fn xml_grammar(node: Node<'_, '_>, context: Context) -> Result<Grammar, ParseError> {
    reject_unknown_attributes(node, &[])?;
    let items = xml_grammar_item_seq(rng_children(node), &context)?;
    Ok(Grammar { context, items })
}

/// §4.12: `start`/`define` with more than one child pattern get those
/// children wrapped in an implicit `group` — unlike multiple *separate*
/// same-named `start`/`define` elements (which require `combine` to
/// merge), this is a single element's own content.
fn implicit_group(body: Vec<Pattern>) -> Vec<Pattern> {
    if body.len() > 1 {
        vec![Pattern::Group(body)]
    } else {
        body
    }
}

fn xml_grammar_item(node: Node<'_, '_>, context: Context) -> Result<GrammarItem, ParseError> {
    match node.tag_name().name() {
        "start" => {
            require_children(node, &["combine"])?;
            let body = xml_patterns(node, &context)?;
            // §4.12 defines implicit grouping for `define`'s multiple
            // children, but not for `start`'s — unlike `define`, `start`
            // isn't listed there, so (like `attribute`) it's simply
            // limited to exactly one pattern child.
            if body.len() > 1 {
                return Err(ParseError::new(
                    "`<start>` must have at most one pattern child",
                ));
            }
            Ok(GrammarItem::Start {
                combine: xml_combine(node)?,
                body,
            })
        }
        "define" => {
            require_children(node, &["name", "combine"])?;
            let name = required_ncname_attribute(node, "define name")?;
            Ok(GrammarItem::Define {
                name,
                combine: xml_combine(node)?,
                body: implicit_group(xml_patterns(node, &context)?),
            })
        }
        "div" => {
            reject_unknown_attributes(node, &[])?;
            Ok(GrammarItem::Div(xml_grammar_item_seq(
                rng_children(node),
                &context,
            )?))
        }
        "include" => {
            reject_unknown_attributes(node, &["href"])?;
            Ok(GrammarItem::Include {
                href: required_attribute(node, "href")?,
                inherit_namespace: token_attribute(node, "ns"),
                body: xml_grammar_item_seq(rng_children(node), &context)?,
                context,
            })
        }
        name => Err(ParseError::new(format!(
            "unsupported RELAX NG grammar item `{name}`"
        ))),
    }
}

fn xml_combine(node: Node<'_, '_>) -> Result<Option<Combine>, ParseError> {
    match token_attribute(node, "combine").as_deref() {
        None => Ok(None),
        Some("choice") => Ok(Some(Combine::Choice)),
        Some("interleave") => Ok(Some(Combine::Interleave)),
        Some(value) => Err(ParseError::new(format!("unknown combine value `{value}`"))),
    }
}

fn required_attribute(node: Node<'_, '_>, name: &str) -> Result<String, ParseError> {
    require_attribute_value(attribute(node, name), name)
}

struct CompactParser {
    tokens: Vec<Token>,
    position: usize,
    context: Context,
}

impl CompactParser {
    fn new(source: &SchemaSource) -> Self {
        let mut context = initial_context(source, None, None);
        context
            .namespaces
            .insert("xml".into(), "http://www.w3.org/XML/1998/namespace".into());
        context.namespaces.insert(
            "xsd".into(),
            "http://www.w3.org/2001/XMLSchema-datatypes".into(),
        );
        Self {
            tokens: decode_unicode_escapes(source.text())
                .and_then(|text| lex(&text))
                .unwrap_or_else(|error| vec![Token::Error(error)]),
            position: 0,
            context,
        }
    }
    fn parse(mut self) -> Result<SchemaDocument, ParseError> {
        if let Some(Token::Error(message)) = self.tokens.first() {
            return Err(ParseError::new(message.clone()));
        }
        self.skip_annotations()?;
        while self.peek_word("namespace")
            || self.peek_word("default")
            || self.peek_word("datatypes")
        {
            self.declaration()?;
            self.skip_annotations()?;
        }
        let root = if self.peek_word("grammar") {
            self.word("grammar")?;
            Root::Grammar(self.grammar()?)
        } else if self.peek_word("start")
            || self.peek_word("include")
            || self.peek_word("div")
            || self.looks_like_definition()
        {
            Root::Grammar(self.grammar_contents(false)?)
        } else {
            Root::Pattern(self.pattern()?)
        };
        self.end()?;
        Ok(SchemaDocument { root })
    }

    fn declaration(&mut self) -> Result<(), ParseError> {
        if self.peek_word("namespace") {
            self.word("namespace")?;
            let prefix = self.identifier()?;
            self.symbol('=')?;
            let uri = self.string()?;
            self.context.namespaces.insert(prefix, uri);
        } else if self.peek_word("datatypes") {
            self.word("datatypes")?;
            let prefix = self.identifier()?;
            self.symbol('=')?;
            let uri = self.string()?;
            self.context.namespaces.insert(prefix, uri);
        } else {
            self.word("default")?;
            self.word("namespace")?;
            if matches!(self.peek(), Token::Word(_)) {
                self.position += 1;
            }
            self.symbol('=')?;
            self.context.default_namespace = Some(self.string()?);
        }
        Ok(())
    }

    fn grammar(&mut self) -> Result<Grammar, ParseError> {
        self.symbol('{')?;
        let grammar = self.grammar_contents(true)?;
        self.symbol('}')?;
        Ok(grammar)
    }
    fn grammar_contents(&mut self, braces: bool) -> Result<Grammar, ParseError> {
        let mut items = Vec::new();
        while !(matches!(self.peek(), Token::End) || braces && self.peek_symbol('}')) {
            self.skip_annotations()?;
            if braces && self.peek_symbol('}') {
                break;
            }
            if self.peek_word("div") {
                self.word("div")?;
                self.symbol('{')?;
                let nested = self.grammar_contents(true)?;
                self.symbol('}')?;
                items.push(GrammarItem::Div(nested.items));
                continue;
            }
            if self.peek_word("include") {
                let (href, inherit_namespace) = self.href_and_inherit_namespace("include")?;
                let body = if self.peek_symbol('{') {
                    self.symbol('{')?;
                    let nested = self.grammar_contents(true)?;
                    self.symbol('}')?;
                    nested.items
                } else {
                    Vec::new()
                };
                items.push(GrammarItem::Include {
                    href,
                    inherit_namespace,
                    body,
                    context: self.context.clone(),
                });
                continue;
            }
            let name = self.identifier()?;
            let combine = if self.peek_symbol('|') {
                self.symbol('|')?;
                self.symbol('=')?;
                Some(Combine::Choice)
            } else if self.peek_symbol('&') {
                self.symbol('&')?;
                self.symbol('=')?;
                Some(Combine::Interleave)
            } else {
                self.symbol('=')?;
                None
            };
            let body = vec![self.pattern()?];
            if name == "start" {
                items.push(GrammarItem::Start { combine, body });
            } else {
                items.push(GrammarItem::Define {
                    name,
                    combine,
                    body,
                });
            }
        }
        Ok(Grammar {
            context: self.context.clone(),
            items,
        })
    }

    fn pattern(&mut self) -> Result<Pattern, ParseError> {
        self.choice()
    }
    /// Parses a left-associative `separator`-delimited sequence of
    /// `parse_one`, collapsing a single value to itself and wrapping two or
    /// more in `wrap` — shared by `choice`/`interleave`/`group` (over
    /// `Pattern`) and `name_class` (over `NameClass`).
    fn left_assoc<T>(
        &mut self,
        separator: char,
        mut parse_one: impl FnMut(&mut Self) -> Result<T, ParseError>,
        wrap: impl FnOnce(Vec<T>) -> T,
    ) -> Result<T, ParseError> {
        let mut values = vec![parse_one(self)?];
        while self.peek_symbol(separator) {
            self.symbol(separator)?;
            values.push(parse_one(self)?);
        }
        Ok(if values.len() == 1 {
            values.pop().expect("one")
        } else {
            wrap(values)
        })
    }
    fn choice(&mut self) -> Result<Pattern, ParseError> {
        self.left_assoc('|', Self::interleave, Pattern::Choice)
    }
    fn interleave(&mut self) -> Result<Pattern, ParseError> {
        self.left_assoc('&', Self::group, Pattern::Interleave)
    }
    fn group(&mut self) -> Result<Pattern, ParseError> {
        self.left_assoc(',', Self::postfix, Pattern::Group)
    }
    fn postfix(&mut self) -> Result<Pattern, ParseError> {
        let mut pattern = self.primary()?;
        loop {
            pattern = match self.peek() {
                Token::Symbol('?') => {
                    self.position += 1;
                    Pattern::Optional(vec![pattern])
                }
                Token::Symbol('*') => {
                    self.position += 1;
                    Pattern::ZeroOrMore(vec![pattern])
                }
                Token::Symbol('+') => {
                    self.position += 1;
                    Pattern::OneOrMore(vec![pattern])
                }
                _ => break,
            };
        }
        self.skip_annotations()?;
        Ok(pattern)
    }
    fn primary(&mut self) -> Result<Pattern, ParseError> {
        if self.peek_symbol('(') {
            self.symbol('(')?;
            let value = self.pattern()?;
            self.symbol(')')?;
            return Ok(value);
        }
        if self.peek_word("element") || self.peek_word("attribute") {
            let element = self.peek_word("element");
            self.position += 1;
            let name = self.name_class(!element)?;
            self.symbol('{')?;
            let body = vec![self.pattern()?];
            self.symbol('}')?;
            return Ok(if element {
                Pattern::Element {
                    name,
                    body,
                    context: self.context.clone(),
                }
            } else {
                Pattern::Attribute {
                    name,
                    body,
                    context: self.context.clone(),
                }
            });
        }
        if self.peek_word("list") || self.peek_word("mixed") {
            let mixed = self.peek_word("mixed");
            self.position += 1;
            self.symbol('{')?;
            let body = vec![self.pattern()?];
            self.symbol('}')?;
            return Ok(if mixed {
                Pattern::Mixed(body)
            } else {
                Pattern::List(body)
            });
        }
        if self.peek_word("grammar") {
            self.word("grammar")?;
            return Ok(Pattern::Grammar(self.grammar()?));
        }
        if self.peek_word("external") {
            let (href, inherit_namespace) = self.href_and_inherit_namespace("external")?;
            return Ok(Pattern::ExternalRef {
                href,
                inherit_namespace,
                context: self.context.clone(),
            });
        }
        if self.peek_word("parent") {
            self.word("parent")?;
            return Ok(Pattern::ParentRef(self.identifier()?));
        }
        for (word, pattern) in [
            ("empty", Pattern::Empty),
            ("text", Pattern::Text),
            ("notAllowed", Pattern::NotAllowed),
        ] {
            if self.peek_word(word) {
                self.position += 1;
                return Ok(pattern);
            }
        }
        if let Token::String(value) = self.peek().clone() {
            self.position += 1;
            return Ok(Pattern::Value {
                value,
                datatype: None,
                datatype_library: None,
                namespace: None,
                context: self.context.clone(),
            });
        }
        let name = self.identifier()?;
        if matches!(self.peek(), Token::String(_)) {
            let value = self.string()?;
            let (datatype, datatype_library) = self.datatype_parts(name);
            return Ok(Pattern::Value {
                value,
                datatype: Some(datatype),
                datatype_library,
                namespace: None,
                context: self.context.clone(),
            });
        }
        if self.peek_symbol('{') {
            self.symbol('{')?;
            let mut params = Vec::new();
            while !self.peek_symbol('}') {
                let param = self.identifier()?;
                self.symbol('=')?;
                params.push(Param {
                    name: param,
                    value: self.string()?,
                    context: self.context.clone(),
                });
            }
            self.symbol('}')?;
            let (datatype, datatype_library) = self.datatype_parts(name);
            let except = if self.peek_symbol('-') {
                self.symbol('-')?;
                vec![self.pattern()?]
            } else {
                Vec::new()
            };
            return Ok(Pattern::Data {
                datatype,
                datatype_library,
                params,
                except,
                context: self.context.clone(),
            });
        }
        if name.contains(':') {
            let (datatype, datatype_library) = self.datatype_parts(name);
            return Ok(self.simple_data(datatype, datatype_library));
        }
        if name == "string" || name == "token" {
            return Ok(self.simple_data(name, Some(String::new())));
        }
        Ok(Pattern::Ref(name))
    }
    /// A `Pattern::Data` with no `param`/`except` children — the bare
    /// `datatype`-name and `string`/`token`-literal forms.
    fn simple_data(&self, datatype: String, datatype_library: Option<String>) -> Pattern {
        Pattern::Data {
            datatype,
            datatype_library,
            params: Vec::new(),
            except: Vec::new(),
            context: self.context.clone(),
        }
    }
    /// `is_attribute` is only relevant to unprefixed plain names (not
    /// wildcards): per the Compact Syntax spec, `default namespace`
    /// applies to unprefixed *element* names but never to attribute
    /// names, which always get the empty (no) namespace unless explicitly
    /// prefixed. It propagates into `except` name classes too, since
    /// those still denote possible attribute names in that position.
    fn name_class(&mut self, is_attribute: bool) -> Result<NameClass, ParseError> {
        self.left_assoc(
            '|',
            |parser| parser.name_class_atom(is_attribute),
            NameClass::Choice,
        )
    }
    fn name_class_atom(&mut self, is_attribute: bool) -> Result<NameClass, ParseError> {
        let base = if self.peek_symbol('(') {
            self.symbol('(')?;
            let inner = self.name_class(is_attribute)?;
            self.symbol(')')?;
            return Ok(inner);
        } else if self.peek_symbol('*') {
            self.symbol('*')?;
            NameClass::Any {
                except: Vec::new(),
                context: self.context.clone(),
            }
        } else {
            let lexical = self.identifier()?;
            if lexical.ends_with(':') && self.peek_symbol('*') {
                self.symbol('*')?;
                let prefix = lexical.trim_end_matches(':');
                let namespace = self.resolve_namespace_prefix(prefix)?;
                NameClass::Namespace {
                    namespace: Some(namespace),
                    except: Vec::new(),
                    context: self.context.clone(),
                }
            } else {
                let (lexical, namespace) = self.resolve_compact_name(lexical, is_attribute)?;
                NameClass::Name {
                    lexical,
                    namespace,
                    context: self.context.clone(),
                }
            }
        };
        if self.peek_symbol('-') {
            self.symbol('-')?;
            let except = self.name_class(is_attribute)?;
            match base {
                NameClass::Any { context, .. } => Ok(NameClass::Any {
                    except: vec![except],
                    context,
                }),
                NameClass::Namespace {
                    namespace, context, ..
                } => Ok(NameClass::Namespace {
                    namespace,
                    except: vec![except],
                    context,
                }),
                _ => Err(ParseError::new("only wildcard name classes may use except")),
            }
        } else {
            Ok(base)
        }
    }
    /// Resolves a namespace prefix (from `prefix:*` or `prefix:local`)
    /// against the schema's `namespace`/`datatypes` declarations, or
    /// fails with a clear error if it was never declared.
    fn resolve_namespace_prefix(&self, prefix: &str) -> Result<String, ParseError> {
        resolve_namespace(&self.context.namespaces, prefix, "unknown namespace prefix")
    }
    /// Resolves a plain (non-wildcard) element/attribute name: `prefix:local`
    /// resolves `prefix` against the schema's `namespace` declarations
    /// (an unknown prefix is a parse error); an unprefixed name gets the
    /// declared `default namespace` for an element, or always the empty
    /// (no) namespace for an attribute — see [`Self::name_class`]'s docs.
    fn resolve_compact_name(
        &self,
        lexical: String,
        is_attribute: bool,
    ) -> Result<(String, Option<String>), ParseError> {
        match lexical.split_once(':') {
            Some((prefix, local)) => {
                let namespace = self.resolve_namespace_prefix(prefix)?;
                Ok((local.to_owned(), Some(namespace)))
            }
            None => {
                let namespace = if is_attribute {
                    Some(String::new())
                } else {
                    self.context.default_namespace.clone()
                };
                Ok((lexical, namespace))
            }
        }
    }
    fn inherit_namespace(&mut self) -> Result<Option<String>, ParseError> {
        if !self.peek_word("inherit") {
            return Ok(None);
        }
        self.word("inherit")?;
        self.symbol('=')?;
        let prefix = self.identifier()?;
        resolve_namespace(
            &self.context.namespaces,
            &prefix,
            "unknown inherited namespace prefix",
        )
        .map(Some)
    }
    /// `word` followed by a string literal `href` and an optional `inherit
    /// = prefix` — the pairing both `external`'s and `include`'s syntax
    /// share.
    fn href_and_inherit_namespace(
        &mut self,
        word: &str,
    ) -> Result<(String, Option<String>), ParseError> {
        self.word(word)?;
        let href = self.string()?;
        let inherit_namespace = self.inherit_namespace()?;
        Ok((href, inherit_namespace))
    }
    fn datatype_parts(&self, name: String) -> (String, Option<String>) {
        match name.split_once(':') {
            Some((prefix, local)) => (
                local.to_owned(),
                self.context.namespaces.get(prefix).cloned(),
            ),
            None => (name, Some(String::new())),
        }
    }
    fn skip_annotations(&mut self) -> Result<(), ParseError> {
        while self.peek_symbol('[') {
            let mut depth = 0usize;
            loop {
                match self.peek() {
                    Token::Symbol('[') => {
                        depth += 1;
                        self.position += 1;
                    }
                    Token::Symbol(']') => {
                        depth -= 1;
                        self.position += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Token::End => {
                        return Err(ParseError::new("unterminated compact syntax annotation"));
                    }
                    _ => self.position += 1,
                }
            }
        }
        Ok(())
    }
    fn peek(&self) -> &Token {
        self.tokens.get(self.position).unwrap_or(&Token::End)
    }
    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Word(value) if value == word)
    }
    fn peek_symbol(&self, symbol: char) -> bool {
        matches!(self.peek(), Token::Symbol(value) if *value == symbol)
    }
    /// Advances past the current token if `matched`, else fails with
    /// `` expected `description` `` — shared by `word` and `symbol`.
    fn expect(&mut self, matched: bool, description: impl fmt::Display) -> Result<(), ParseError> {
        if matched {
            self.position += 1;
            Ok(())
        } else {
            Err(ParseError::new(format!("expected `{description}`")))
        }
    }
    fn word(&mut self, word: &str) -> Result<(), ParseError> {
        self.expect(self.peek_word(word), word)
    }
    fn symbol(&mut self, symbol: char) -> Result<(), ParseError> {
        self.expect(self.peek_symbol(symbol), symbol)
    }
    /// Advances past and returns the current token's payload if `extract`
    /// matches it, else fails with `` expected <description> `` — shared by
    /// `identifier` and `string`.
    fn expect_token(
        &mut self,
        extract: impl FnOnce(&Token) -> Option<String>,
        description: &str,
    ) -> Result<String, ParseError> {
        match extract(self.peek()) {
            Some(value) => {
                self.position += 1;
                Ok(value)
            }
            None => Err(ParseError::new(format!("expected {description}"))),
        }
    }
    fn identifier(&mut self) -> Result<String, ParseError> {
        self.expect_token(
            |token| match token {
                Token::Word(value) => Some(value.clone()),
                _ => None,
            },
            "identifier",
        )
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.expect_token(
            |token| match token {
                Token::String(value) => Some(value.clone()),
                _ => None,
            },
            "string literal",
        )
    }
    fn end(&self) -> Result<(), ParseError> {
        if matches!(self.peek(), Token::End) {
            Ok(())
        } else {
            Err(ParseError::new("unexpected trailing compact syntax"))
        }
    }
    fn looks_like_definition(&self) -> bool {
        matches!(self.peek(), Token::Word(_))
            && matches!(
                self.tokens.get(self.position + 1),
                Some(Token::Symbol('=')) | Some(Token::Symbol('|')) | Some(Token::Symbol('&'))
            )
    }
}

#[derive(Clone, Debug)]
enum Token {
    Word(String),
    String(String),
    Symbol(char),
    End,
    Error(String),
}

fn lex(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            whitespace if whitespace.is_whitespace() => {}
            '#' => {
                while matches!(chars.peek(), Some(value) if *value != '\n') {
                    chars.next();
                }
            }
            '"' => {
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(next) => value.push(next),
                            None => return Err("unterminated escape".into()),
                        },
                        Some(next) => value.push(next),
                        None => return Err("unterminated string literal".into()),
                    }
                }
                tokens.push(Token::String(value));
            }
            '{' | '}' | '(' | ')' | '[' | ']' | ',' | '|' | '&' | '?' | '*' | '+' | '-' | '=' => {
                tokens.push(Token::Symbol(character))
            }
            _ => {
                // `-` is a valid NCName character (used e.g. in
                // `common.elem.flow` schemas' hyphenated names like
                // `browsing-context`), so once a word has started it keeps
                // consuming `-` like any other identifier character. Only a
                // *leading* `-` (matched above, before a word has started)
                // is the standalone `except` operator token — this matches
                // the Compact Syntax spec's longest-match tokenization rule.
                let mut value = String::from(character);
                while matches!(chars.peek(), Some(next) if !next.is_whitespace() && !"{}()[],|&?*+=\"#".contains(*next))
                {
                    value.push(chars.next().unwrap());
                }
                tokens.push(Token::Word(value));
            }
        }
    }
    tokens.push(Token::End);
    Ok(tokens)
}

fn decode_unicode_escapes(input: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' || chars.peek() != Some(&'x') {
            output.push(character);
            continue;
        }
        chars.next();
        if chars.next() != Some('{') {
            return Err("expected `{` after `\\x`".into());
        }
        let mut hexadecimal = String::new();
        loop {
            match chars.next() {
                Some('}') => break,
                Some(value) if value.is_ascii_hexdigit() => hexadecimal.push(value),
                _ => return Err("invalid Unicode escape in compact syntax".into()),
            }
        }
        let scalar = u32::from_str_radix(&hexadecimal, 16)
            .map_err(|_| "invalid Unicode escape in compact syntax")?;
        output
            .push(char::from_u32(scalar).ok_or("invalid Unicode scalar value in compact syntax")?);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SchemaSyntax;

    fn source(text: &str, syntax: SchemaSyntax) -> SchemaSource {
        SchemaSource::new(text, "memory:/schema.rng", syntax)
    }

    #[test]
    fn parses_equivalent_simple_schemas() {
        let compact = parse(&source(
            "element book { attribute id { text }, element title { text } }",
            SchemaSyntax::Compact,
        ))
        .expect("compact schema parses");
        let xml = parse(&source(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="book"><attribute name="id"><text/></attribute><element name="title"><text/></element></element>"#,
            SchemaSyntax::Xml,
        ))
        .expect("XML schema parses");
        assert!(matches!(
            compact.root,
            Root::Pattern(Pattern::Element { .. })
        ));
        assert!(matches!(xml.root, Root::Pattern(Pattern::Element { .. })));
    }

    #[test]
    fn compact_parser_supports_declarations_and_grammar_content() {
        let document = parse(&source(
            "# comment\nnamespace h = \"urn:html\"\nstart = element h:html { text }\nitem |= element item { empty }",
            SchemaSyntax::Compact,
        ))
        .expect("compact grammar parses");
        let Root::Grammar(grammar) = document.root else {
            panic!("expected grammar")
        };
        assert_eq!(grammar.items.len(), 2);
        assert!(matches!(
            grammar.items[1],
            GrammarItem::Define {
                combine: Some(Combine::Choice),
                ..
            }
        ));
    }

    #[test]
    fn compact_parser_supports_annotations_datatypes_and_inheritance() {
        let document = parse(&source(
            r#"[ a:documentation [ "example" ] ]
               namespace h = "urn:html"
               datatypes x = "urn:types"
               include "base.rnc" inherit = h {
                 start = element h:root { x:number { min = "1" } - x:number "0" }
               }"#,
            SchemaSyntax::Compact,
        ))
        .expect("compact schema parses");
        let Root::Grammar(grammar) = document.root else {
            panic!("expected grammar")
        };
        assert!(matches!(
            grammar.items[0],
            GrammarItem::Include {
                inherit_namespace: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn compact_parser_decodes_unicode_escapes() {
        parse(&source(
            r"element \x{66}oo { empty }",
            SchemaSyntax::Compact,
        ))
        .expect("Unicode escape parses");
    }

    #[test]
    fn compact_parser_resolves_prefixed_element_and_attribute_names() {
        let document = parse(&source(
            "namespace h = \"urn:html\"\n\
             element h:foo { attribute h:bar { text } }",
            SchemaSyntax::Compact,
        ))
        .expect("compact schema parses");
        let Root::Pattern(Pattern::Element { name, body, .. }) = document.root else {
            panic!("expected an element pattern");
        };
        let NameClass::Name {
            lexical, namespace, ..
        } = &name
        else {
            panic!("expected a plain name class");
        };
        assert_eq!(lexical, "foo");
        assert_eq!(namespace.as_deref(), Some("urn:html"));
        let Pattern::Attribute {
            name: attr_name, ..
        } = &body[0]
        else {
            panic!("expected an attribute pattern");
        };
        let NameClass::Name {
            lexical, namespace, ..
        } = attr_name
        else {
            panic!("expected a plain name class");
        };
        assert_eq!(lexical, "bar");
        assert_eq!(namespace.as_deref(), Some("urn:html"));
    }

    #[test]
    fn compact_parser_applies_default_namespace_to_unprefixed_elements_but_not_attributes() {
        let document = parse(&source(
            "default namespace = \"urn:html\"\n\
             element foo { attribute bar { text } }",
            SchemaSyntax::Compact,
        ))
        .expect("compact schema parses");
        let Root::Pattern(Pattern::Element { name, body, .. }) = document.root else {
            panic!("expected an element pattern");
        };
        let NameClass::Name { namespace, .. } = &name else {
            panic!("expected a plain name class");
        };
        // §4.8/the Compact Syntax spec's equivalent: `default namespace`
        // applies to unprefixed *element* names...
        assert_eq!(namespace.as_deref(), Some("urn:html"));
        let Pattern::Attribute {
            name: attr_name, ..
        } = &body[0]
        else {
            panic!("expected an attribute pattern");
        };
        let NameClass::Name { namespace, .. } = attr_name else {
            panic!("expected a plain name class");
        };
        // ...but never to unprefixed *attribute* names, which always get
        // no namespace regardless of any declared default.
        assert_eq!(namespace.as_deref(), Some(""));
    }

    #[test]
    fn compact_parser_rejects_an_unknown_namespace_prefix() {
        let error = parse(&source(
            "start = element unknown:foo { empty }",
            SchemaSyntax::Compact,
        ))
        .expect_err("undeclared prefix must be a parse error");
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn compact_parser_allows_hyphens_inside_identifiers() {
        // `-` is a valid NCName character and must be consumed as part of
        // a longest-match identifier when adjacent to other identifier
        // characters, not split off as the `except` operator (real-world
        // schemas commonly use hyphenated names, e.g. `browsing-context`).
        let document = parse(&source(
            "browsing-context = empty\nstart = browsing-context",
            SchemaSyntax::Compact,
        ))
        .expect("hyphenated identifiers parse");
        assert!(matches!(document.root, Root::Grammar(_)));
    }

    #[test]
    fn compact_parser_supports_parenthesized_except_name_classes() {
        // `nameClass ::= ... | "(" nameClass ")"` — needed for excepting
        // more than one name, e.g. `attribute * - ( a | b ) { text }`.
        let document = parse(&source(
            "attribute * - ( a | b ) { text }",
            SchemaSyntax::Compact,
        ))
        .expect("parenthesized except name class parses");
        let Root::Pattern(Pattern::Attribute { name, .. }) = document.root else {
            panic!("expected an attribute pattern");
        };
        let NameClass::Any { except, .. } = &name else {
            panic!("expected an Any name class");
        };
        assert_eq!(except.len(), 1);
        assert!(matches!(except[0], NameClass::Choice(ref choices) if choices.len() == 2));
    }

    #[test]
    fn xml_parser_ignores_foreign_annotations() {
        let document = parse(&source(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" xmlns:a="http://relaxng.org/ns/compatibility/annotations/1.0" name="book"><a:documentation>Book</a:documentation><empty/></element>"#,
            SchemaSyntax::Xml,
        ))
        .expect("XML schema parses");
        let Root::Pattern(Pattern::Element { body, .. }) = document.root else {
            panic!("expected element")
        };
        assert_eq!(body, vec![Pattern::Empty]);
    }
}
