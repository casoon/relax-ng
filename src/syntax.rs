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
    let root_context = Context {
        base_uri: source.base_uri().to_owned(),
        namespaces: BTreeMap::new(),
        default_namespace: None,
        // §4.3: the default when no ancestor sets `datatypeLibrary` is the
        // empty string, not "unset" — the built-in datatype library.
        datatype_library: Some(String::new()),
        // §4.9: no ancestor to inherit `ns` from yet, other than what an
        // externalRef/include passed in — the root element's own `ns` (if
        // any) still wins over this, picked up by `xml_context` below.
        ns: inherited_ns,
    };
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

fn required_token_attribute(node: Node<'_, '_>, name: &str) -> Result<String, ParseError> {
    token_attribute(node, name)
        .ok_or_else(|| ParseError::new(format!("missing required `{name}` attribute")))
}

/// Approximates the XML Names `NCName` production (`(Letter|'_') (Letter |
/// Digit | '.' | '-' | '_' | CombiningChar | Extender)*`) with Unicode's
/// XID_Start/XID_Continue (`unicode-ident`, the same crate rustc itself
/// uses for identifiers) rather than XML 1.0's own (dated, Unicode 2.0)
/// character tables — crucially, unlike `char::is_alphabetic`, XID_Start
/// correctly excludes combining marks (e.g. Thai vowel signs), which are
/// only valid as non-initial characters.
fn is_valid_ncname(value: &str) -> bool {
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
fn reject_unknown_attributes(node: Node<'_, '_>, allowed: &[&str]) -> Result<(), ParseError> {
    for attr in node.attributes() {
        let name = attr.name();
        let is_foreign = matches!(attr.namespace(), Some(namespace) if namespace != RNG_NAMESPACE);
        if is_foreign || name == "ns" || name == "datatypeLibrary" || allowed.contains(&name) {
            continue;
        }
        return Err(ParseError::new(format!(
            "`<{}>` does not allow a `{name}` attribute",
            node.tag_name().name()
        )));
    }
    Ok(())
}

fn require_at_least_one_child(node: Node<'_, '_>) -> Result<(), ParseError> {
    if rng_children(node).next().is_none() {
        return Err(ParseError::new(format!(
            "`<{}>` requires at least one child",
            node.tag_name().name()
        )));
    }
    Ok(())
}

fn reject_children(node: Node<'_, '_>) -> Result<(), ParseError> {
    if rng_children(node).next().is_some() {
        return Err(ParseError::new(format!(
            "`<{}>` must not have children",
            node.tag_name().name()
        )));
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
        return Err(ParseError::new(format!(
            "`<{}>` must not have child elements",
            node.tag_name().name()
        )));
    }
    Ok(())
}

fn xml_patterns(node: Node<'_, '_>, context: &Context) -> Result<Vec<Pattern>, ParseError> {
    rng_children(node)
        .map(|child| xml_pattern(child, xml_context(child, context)?))
        .collect()
}

fn xml_pattern(node: Node<'_, '_>, context: Context) -> Result<Pattern, ParseError> {
    let children = || xml_patterns(node, &context);
    let container = || -> Result<Vec<Pattern>, ParseError> {
        reject_unknown_attributes(node, &[])?;
        require_at_least_one_child(node)?;
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
            reject_unknown_attributes(node, &[])?;
            reject_children(node)?;
            Ok(Pattern::Empty)
        }
        "notAllowed" => {
            reject_unknown_attributes(node, &[])?;
            reject_children(node)?;
            Ok(Pattern::NotAllowed)
        }
        "text" => {
            reject_unknown_attributes(node, &[])?;
            reject_children(node)?;
            Ok(Pattern::Text)
        }
        "ref" => {
            reject_unknown_attributes(node, &["name"])?;
            reject_children(node)?;
            let name = required_token_attribute(node, "name")?;
            require_ncname(&name, "ref name")?;
            Ok(Pattern::Ref(name))
        }
        "parentRef" => {
            reject_unknown_attributes(node, &["name"])?;
            reject_children(node)?;
            let name = required_token_attribute(node, "name")?;
            require_ncname(&name, "parentRef name")?;
            Ok(Pattern::ParentRef(name))
        }
        "externalRef" => {
            reject_unknown_attributes(node, &["href"])?;
            reject_children(node)?;
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
                    reject_unknown_attributes(child, &["name"])?;
                    reject_all_children(child)?;
                    Ok(Param {
                        name: required_attribute(child, "name")?,
                        value: child.text().unwrap_or_default().to_owned(),
                        context: xml_context(child, &context)?,
                    })
                })
                .collect::<Result<Vec<_>, ParseError>>()?;
            let mut except_children =
                rng_children(node).filter(|child| child.tag_name().name() == "except");
            let except = match (except_children.next(), except_children.next()) {
                (None, _) => Vec::new(),
                (Some(only), None) => {
                    reject_unknown_attributes(only, &[])?;
                    require_at_least_one_child(only)?;
                    xml_patterns(only, &context)?
                }
                (Some(_), Some(_)) => {
                    return Err(ParseError::new("`<data>` may have at most one `except`"));
                }
            };
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
            reject_unknown_attributes(node, &["type"])?;
            reject_all_children(node)?;
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
            let namespace = Some(
                token_attribute(node, "ns")
                    .unwrap_or_else(|| context.ns.clone().unwrap_or_default()),
            );
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
            let uri =
                context.namespaces.get(prefix).cloned().ok_or_else(|| {
                    ParseError::new(format!("unknown namespace prefix `{prefix}`"))
                })?;
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
    let body = children
        .map(|child| xml_pattern(child, xml_context(child, context)?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((name, finish_body(body)?))
}

fn xml_name_class(node: Node<'_, '_>, context: Context) -> Result<NameClass, ParseError> {
    match node.tag_name().name() {
        "name" => {
            reject_unknown_attributes(node, &[])?;
            reject_all_children(node)?;
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
            let mut except_children =
                rng_children(node).filter(|child| child.tag_name().name() == "except");
            let except = match (except_children.next(), except_children.next()) {
                (None, _) => Vec::new(),
                (Some(only), None) => {
                    reject_unknown_attributes(only, &[])?;
                    require_at_least_one_child(only)?;
                    rng_children(only)
                        .map(|nested| xml_name_class(nested, xml_context(nested, &context)?))
                        .collect::<Result<Vec<_>, _>>()?
                }
                (Some(_), Some(_)) => {
                    return Err(ParseError::new(format!(
                        "`{}` may have at most one `except`",
                        node.tag_name().name()
                    )));
                }
            };
            if node.tag_name().name() == "anyName" {
                Ok(NameClass::Any { except, context })
            } else {
                // §4.9: `nsName` inherits `ns` like `name`/`value` do.
                let namespace = Some(
                    token_attribute(node, "ns")
                        .unwrap_or_else(|| context.ns.clone().unwrap_or_default()),
                );
                Ok(NameClass::Namespace {
                    namespace,
                    except,
                    context,
                })
            }
        }
        "choice" => {
            reject_unknown_attributes(node, &[])?;
            require_at_least_one_child(node)?;
            Ok(NameClass::Choice(
                rng_children(node)
                    .map(|child| xml_name_class(child, xml_context(child, &context)?))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        name => Err(ParseError::new(format!(
            "unsupported RELAX NG name class `{name}`"
        ))),
    }
}

fn xml_grammar(node: Node<'_, '_>, context: Context) -> Result<Grammar, ParseError> {
    reject_unknown_attributes(node, &[])?;
    let items = rng_children(node)
        .map(|child| xml_grammar_item(child, xml_context(child, &context)?))
        .collect::<Result<Vec<_>, _>>()?;
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
            reject_unknown_attributes(node, &["combine"])?;
            require_at_least_one_child(node)?;
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
            reject_unknown_attributes(node, &["name", "combine"])?;
            require_at_least_one_child(node)?;
            let name = required_token_attribute(node, "name")?;
            require_ncname(&name, "define name")?;
            Ok(GrammarItem::Define {
                name,
                combine: xml_combine(node)?,
                body: implicit_group(xml_patterns(node, &context)?),
            })
        }
        "div" => {
            reject_unknown_attributes(node, &[])?;
            Ok(GrammarItem::Div(
                rng_children(node)
                    .map(|child| xml_grammar_item(child, xml_context(child, &context)?))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        "include" => {
            reject_unknown_attributes(node, &["href"])?;
            Ok(GrammarItem::Include {
                href: required_attribute(node, "href")?,
                inherit_namespace: token_attribute(node, "ns"),
                body: rng_children(node)
                    .map(|child| xml_grammar_item(child, xml_context(child, &context)?))
                    .collect::<Result<Vec<_>, _>>()?,
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
    attribute(node, name)
        .ok_or_else(|| ParseError::new(format!("missing required `{name}` attribute")))
}

struct CompactParser {
    tokens: Vec<Token>,
    position: usize,
    context: Context,
}

impl CompactParser {
    fn new(source: &SchemaSource) -> Self {
        let mut context = Context {
            base_uri: source.base_uri().to_owned(),
            namespaces: BTreeMap::new(),
            default_namespace: None,
            datatype_library: None,
            ns: None,
        };
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
                self.word("include")?;
                let href = self.string()?;
                let inherit_namespace = self.inherit_namespace()?;
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
    fn choice(&mut self) -> Result<Pattern, ParseError> {
        let mut values = vec![self.interleave()?];
        while self.peek_symbol('|') {
            self.symbol('|')?;
            values.push(self.interleave()?);
        }
        Ok(if values.len() == 1 {
            values.pop().unwrap()
        } else {
            Pattern::Choice(values)
        })
    }
    fn interleave(&mut self) -> Result<Pattern, ParseError> {
        let mut values = vec![self.group()?];
        while self.peek_symbol('&') {
            self.symbol('&')?;
            values.push(self.group()?);
        }
        Ok(if values.len() == 1 {
            values.pop().unwrap()
        } else {
            Pattern::Interleave(values)
        })
    }
    fn group(&mut self) -> Result<Pattern, ParseError> {
        let mut values = vec![self.postfix()?];
        while self.peek_symbol(',') {
            self.symbol(',')?;
            values.push(self.postfix()?);
        }
        Ok(if values.len() == 1 {
            values.pop().unwrap()
        } else {
            Pattern::Group(values)
        })
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
            self.word("external")?;
            let href = self.string()?;
            let inherit_namespace = self.inherit_namespace()?;
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
            return Ok(Pattern::Data {
                datatype,
                datatype_library,
                params: Vec::new(),
                except: Vec::new(),
                context: self.context.clone(),
            });
        }
        if name == "string" || name == "token" {
            return Ok(Pattern::Data {
                datatype: name,
                datatype_library: Some(String::new()),
                params: Vec::new(),
                except: Vec::new(),
                context: self.context.clone(),
            });
        }
        Ok(Pattern::Ref(name))
    }
    /// `is_attribute` is only relevant to unprefixed plain names (not
    /// wildcards): per the Compact Syntax spec, `default namespace`
    /// applies to unprefixed *element* names but never to attribute
    /// names, which always get the empty (no) namespace unless explicitly
    /// prefixed. It propagates into `except` name classes too, since
    /// those still denote possible attribute names in that position.
    fn name_class(&mut self, is_attribute: bool) -> Result<NameClass, ParseError> {
        let mut choices = vec![self.name_class_atom(is_attribute)?];
        while self.peek_symbol('|') {
            self.symbol('|')?;
            choices.push(self.name_class_atom(is_attribute)?);
        }
        Ok(if choices.len() == 1 {
            choices.pop().unwrap()
        } else {
            NameClass::Choice(choices)
        })
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
        self.context
            .namespaces
            .get(prefix)
            .cloned()
            .ok_or_else(|| ParseError::new(format!("unknown namespace prefix `{prefix}`")))
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
        self.context
            .namespaces
            .get(&prefix)
            .cloned()
            .ok_or_else(|| {
                ParseError::new(format!("unknown inherited namespace prefix `{prefix}`"))
            })
            .map(Some)
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
    fn word(&mut self, word: &str) -> Result<(), ParseError> {
        if self.peek_word(word) {
            self.position += 1;
            Ok(())
        } else {
            Err(ParseError::new(format!("expected `{word}`")))
        }
    }
    fn symbol(&mut self, symbol: char) -> Result<(), ParseError> {
        if self.peek_symbol(symbol) {
            self.position += 1;
            Ok(())
        } else {
            Err(ParseError::new(format!("expected `{symbol}`")))
        }
    }
    fn identifier(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Word(value) => {
                self.position += 1;
                Ok(value)
            }
            _ => Err(ParseError::new("expected identifier")),
        }
    }
    fn string(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::String(value) => {
                self.position += 1;
                Ok(value)
            }
            _ => Err(ParseError::new("expected string literal")),
        }
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
