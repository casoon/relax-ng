//! Runs the vendored official RELAX NG test suite (`tests/corpus/relaxng/`,
//! see `UPSTREAM.md`) against this crate's full pipeline: every `testCase`
//! classifies its schema as `<correct>` or `<incorrect>` (checked by
//! [`spec_test_suite_schema_classification`], which maps directly onto
//! `Ok`/`Err` from `parse`/`simplify`), and a `<correct>` schema may also
//! carry `<valid>`/`<invalid>` instance documents (checked by
//! [`spec_test_suite_instance_validation`] against Phase 05/06's
//! `validate`, with the full built-in-plus-XSD [`DatatypeRegistry`]).
//!
//! `testCase`s using `<resource>`/`<dir>` (for `include`/`externalRef`)
//! are served by [`MultiFileResolver`], an in-memory resolver built from
//! each case's own inline resource tree — see its docs.

use std::collections::BTreeMap;
use std::ops::Range;

use relax_ng::{
    Content, DatatypeRegistry, Element, ExpandedName, ResolveError, SchemaResolver, SchemaSource,
    SchemaSyntax, parse, simplify, validate,
};
use roxmltree::Node;

const SUITE_PATH: &str = "tests/corpus/relaxng/spectest.xml";

/// The suite's `<!DOCTYPE testSuite [ <!ENTITY dii "..."> ]>` internal
/// subset declares one custom entity (used by a single §4.5 name-class
/// test with Thai-script characters) — `roxmltree` refuses to parse any
/// document with a DTD at all. Strip the DOCTYPE and substitute the one
/// entity reference with its literal expansion so the file parses as
/// plain, well-formed XML.
fn load_suite_text() -> String {
    let raw = std::fs::read_to_string(SUITE_PATH).expect("spectest.xml is vendored");
    let start = raw.find("<!DOCTYPE").expect("suite has a DOCTYPE");
    let end = raw[start..]
        .find("]>")
        .expect("DOCTYPE has an internal subset")
        + start
        + 2;
    let mut cleaned = String::with_capacity(raw.len());
    cleaned.push_str(&raw[..start]);
    cleaned.push_str(&raw[end..]);
    let dii = format!(
        "<{}{}/>",
        char::from_u32(0xE14).expect("valid scalar"),
        char::from_u32(0xE35).expect("valid scalar")
    );
    cleaned.replace("&dii;", &dii)
}

/// An in-memory resolver built from one `testCase`'s own inline
/// `<resource>`/`<dir>` tree (see [`collect_resources`]) — an empty map
/// (the common case: most `testCase`s have no such tree) makes every
/// `href` fail to resolve, exactly like a "no resolver available" stub
/// would.
struct MultiFileResolver<'a> {
    resources: &'a BTreeMap<String, String>,
}

impl SchemaResolver for MultiFileResolver<'_> {
    fn resolve(&self, href: &str, base_uri: &str) -> Result<SchemaSource, ResolveError> {
        let path = resolve_relative_reference(base_uri, href);
        let text = self.resources.get(&path).ok_or_else(|| {
            ResolveError::new(format!(
                "no such resource `{path}` (href=`{href}`, base=`{base_uri}`)"
            ))
        })?;
        Ok(SchemaSource::new(text.clone(), path, SchemaSyntax::Xml))
    }
}

/// Mirrors `src/syntax.rs`'s private `xml:base`-chain merge — this is an
/// integration test, so it can't reach that directly. A minimal RFC 3986
/// §5.3-style merge: an absolute `reference` (with a `scheme:` prefix)
/// replaces `base` outright; otherwise `reference` is appended to
/// `base`'s directory (everything up to and including the last `/`, or
/// nothing if `base` has none). Sufficient for this suite's plain
/// relative paths — none of its `<resource>`/`<dir>` cases use `.`/`..`
/// segments or absolute references.
fn resolve_relative_reference(base: &str, reference: &str) -> String {
    let is_absolute = reference
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.chars().all(|c| c.is_ascii_alphanumeric()));
    if is_absolute {
        return reference.to_owned();
    }
    let directory = match base.rfind('/') {
        Some(index) => &base[..=index],
        None => "",
    };
    format!("{directory}{reference}")
}

/// Flattens a `testCase`'s `<resource name="...">...</resource>`/
/// `<dir name="...">...</dir>` tree into a map from merged path (e.g. a
/// `<resource name="x">` nested inside `<dir name="sub">` becomes
/// `"sub/x"`) to that resource's schema text — the same shape
/// [`resolve_relative_reference`] produces when merging an `href` against
/// an accumulated `xml:base`.
fn collect_resources(text: &str, node: Node, prefix: &str, out: &mut BTreeMap<String, String>) {
    for child in element_children(node) {
        let Some(name) = child.attribute("name") else {
            continue;
        };
        if child.has_tag_name("resource") {
            if let Some(root) = element_children(child).next() {
                out.insert(format!("{prefix}{name}"), slice(text, root.range()));
            }
        } else if child.has_tag_name("dir") {
            collect_resources(text, child, &format!("{prefix}{name}/"), out);
        }
    }
}

enum Expectation {
    Correct,
    Incorrect,
}

struct Case {
    index: usize,
    section: String,
    expectation: Expectation,
    schema_xml: String,
    valid_instances: Vec<String>,
    invalid_instances: Vec<String>,
    /// This case's own `<resource>`/`<dir>` tree, flattened by
    /// [`collect_resources`] — empty for the (large majority of) cases
    /// that don't declare one.
    resources: BTreeMap<String, String>,
}

fn element_children<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(Node::is_element)
}

fn slice(text: &str, range: Range<usize>) -> String {
    text[range].to_owned()
}

/// Every root-element child of each `<tag>` sibling of `test_case` — used
/// for `<valid>`/`<invalid>`, which may appear zero, one, or (in principle)
/// several times per `testCase`, each wrapping exactly one instance
/// document root.
fn instance_texts(text: &str, test_case: Node, tag: &str) -> Vec<String> {
    element_children(test_case)
        .filter(|child| child.has_tag_name(tag))
        .filter_map(|wrapper| element_children(wrapper).next())
        .map(|root| slice(text, root.range()))
        .collect()
}

fn collect_cases(text: &str) -> Vec<Case> {
    let document = roxmltree::Document::parse(text).expect("cleaned suite is well-formed XML");
    let mut cases = Vec::new();
    for (index, test_case) in document
        .descendants()
        .filter(|node| node.has_tag_name("testCase"))
        .enumerate()
    {
        let children: Vec<Node> = element_children(test_case).collect();
        let mut resources = BTreeMap::new();
        collect_resources(text, test_case, "", &mut resources);
        let section = children
            .iter()
            .find(|child| child.has_tag_name("section"))
            .and_then(|child| child.text())
            .unwrap_or("?")
            .trim()
            .to_owned();
        let (expectation, schema) =
            if let Some(node) = children.iter().find(|child| child.has_tag_name("correct")) {
                (Expectation::Correct, node)
            } else if let Some(node) = children
                .iter()
                .find(|child| child.has_tag_name("incorrect"))
            {
                (Expectation::Incorrect, node)
            } else {
                continue; // no schema in this test case
            };
        let Some(root) = element_children(*schema).next() else {
            continue;
        };
        cases.push(Case {
            index: index + 1,
            section,
            expectation,
            schema_xml: slice(text, root.range()),
            valid_instances: instance_texts(text, test_case, "valid"),
            invalid_instances: instance_texts(text, test_case, "invalid"),
            resources,
        });
    }
    cases
}

#[test]
fn spec_test_suite_schema_classification() {
    let text = load_suite_text();
    let cases = collect_cases(&text);
    assert!(cases.len() > 300, "expected most of the suite to be usable");

    let mut failures: Vec<String> = Vec::new();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // keep panics (bugs the suite finds) off stderr
    for case in &cases {
        let resolver = MultiFileResolver {
            resources: &case.resources,
        };
        let source = SchemaSource::new(case.schema_xml.clone(), "", SchemaSyntax::Xml);
        let outcome = std::panic::catch_unwind(|| match parse(&source) {
            Err(_) => false,
            Ok(document) => simplify(&document, &resolver).is_ok(),
        });
        let (accepted, panicked) = match outcome {
            Ok(accepted) => (accepted, false),
            Err(_) => (false, true),
        };
        let matches_expectation = !panicked
            && match case.expectation {
                Expectation::Correct => accepted,
                Expectation::Incorrect => !accepted,
            };
        if !matches_expectation {
            let label = if panicked {
                "panicked"
            } else {
                match case.expectation {
                    Expectation::Correct => "expected correct, was rejected",
                    Expectation::Incorrect => "expected incorrect, was accepted",
                }
            };
            failures.push(format!(
                "jing-spectest-{:04} section={} {label}",
                case.index, case.section
            ));
        }
    }
    std::panic::set_hook(previous_hook);

    eprintln!(
        "spectest (schema classification): {}/{} cases classified correctly ({} failing)",
        cases.len() - failures.len(),
        cases.len(),
        failures.len()
    );
    for failure in &failures {
        eprintln!("  {failure}");
    }

    // Every case usable without a vendored multi-file resolver (see
    // `collect_cases`, which skips `<resource>`/`<dir>` cases) now
    // classifies correctly. This is a completeness check, not just a
    // ratchet — a regression here means Phase 04's simplification pipeline
    // broke something the official conformance suite already covers.
    assert_eq!(
        failures,
        Vec::<String>::new(),
        "spectest regressed: {}/{} cases classified correctly",
        cases.len() - failures.len(),
        cases.len()
    );
}

/// A `roxmltree` element, wrapped to implement [`Element`] — the adapter
/// this harness uses to feed the suite's `<valid>`/`<invalid>` instance
/// documents into [`validate`]. Not part of this crate's public API (no
/// XML parser dependency in the core, see `plan/DECISIONS.md`); this is
/// test-only glue, analogous to what a real caller (e.g. `html-conform`,
/// over its own `xmloxide`-derived tree) would write for its own tree
/// type.
struct RoxmlElement<'a, 'input>(Node<'a, 'input>);

impl<'a, 'input> Element for RoxmlElement<'a, 'input> {
    fn name(&self) -> ExpandedName {
        let tag = self.0.tag_name();
        ExpandedName::new(tag.namespace(), tag.name())
    }

    fn attributes(&self) -> impl Iterator<Item = (ExpandedName, String)> {
        self.0.attributes().map(|attribute| {
            (
                ExpandedName::new(attribute.namespace(), attribute.name()),
                attribute.value().to_owned(),
            )
        })
    }

    fn children(&self) -> impl Iterator<Item = Content<Self>> {
        let mut result = Vec::new();
        let mut pending_text = String::new();
        for child in self.0.children() {
            if child.is_element() {
                if !pending_text.is_empty() {
                    result.push(Content::Text(std::mem::take(&mut pending_text)));
                }
                result.push(Content::Element(RoxmlElement(child)));
            } else if child.is_text() {
                pending_text.push_str(child.text().unwrap_or_default());
            }
            // Comments/processing instructions aren't part of the
            // `Element` model (see its docs) and are skipped here.
        }
        if !pending_text.is_empty() {
            result.push(Content::Text(pending_text));
        }
        result.into_iter()
    }

    fn namespace_bindings(&self) -> impl Iterator<Item = (String, String)> {
        self.0.namespaces().map(|namespace| {
            (
                namespace.name().unwrap_or("").to_owned(),
                namespace.uri().to_owned(),
            )
        })
    }
}

#[test]
fn spec_test_suite_instance_validation() {
    let text = load_suite_text();
    let cases = collect_cases(&text);
    let registry = DatatypeRegistry::new();

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for case in &cases {
        // Only a `<correct>` schema can be checked against instances at
        // all — an `<incorrect>` one never compiles, so it has none.
        if !matches!(case.expectation, Expectation::Correct) {
            continue;
        }
        if case.valid_instances.is_empty() && case.invalid_instances.is_empty() {
            continue;
        }
        let resolver = MultiFileResolver {
            resources: &case.resources,
        };
        let source = SchemaSource::new(case.schema_xml.clone(), "", SchemaSyntax::Xml);
        let Ok(document) = parse(&source) else {
            // Already reported by `spec_test_suite_schema_classification`
            // — don't double-count a schema-compile failure here too.
            continue;
        };
        let Ok(compiled) = simplify(&document, &resolver) else {
            continue;
        };

        for (label, instance_xml, should_be_valid) in case
            .valid_instances
            .iter()
            .map(|xml| ("valid", xml, true))
            .chain(
                case.invalid_instances
                    .iter()
                    .map(|xml| ("invalid", xml, false)),
            )
        {
            checked += 1;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let instance_document =
                    roxmltree::Document::parse(instance_xml).expect("instance is well-formed XML");
                let root = RoxmlElement(instance_document.root_element());
                validate(&compiled, &registry, &root)
            }));
            let (accepted, panicked) = match outcome {
                Ok(Ok(errors)) => (errors.is_empty(), false),
                Ok(Err(_datatype_error)) => (false, false),
                Err(_) => (false, true),
            };
            if accepted != should_be_valid {
                let reason = if panicked {
                    "panicked"
                } else if should_be_valid {
                    "expected valid, was rejected"
                } else {
                    "expected invalid, was accepted"
                };
                failures.push(format!(
                    "jing-spectest-{:04} section={} instance={label} {reason}",
                    case.index, case.section
                ));
            }
        }
    }
    std::panic::set_hook(previous_hook);

    eprintln!(
        "spectest (instance validation): {}/{} instances classified correctly ({} failing)",
        checked - failures.len(),
        checked,
        failures.len()
    );
    for failure in &failures {
        eprintln!("  {failure}");
    }

    // Same completeness-check convention as schema classification (see
    // `plan/DECISIONS.md`'s Phase 07 entry for the 100%-of-usable-cases
    // target this asserts).
    assert_eq!(
        failures,
        Vec::<String>::new(),
        "spectest instance validation regressed: {}/{} instances classified correctly",
        checked - failures.len(),
        checked
    );
}
