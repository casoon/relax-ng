//! Minimal end-to-end example: compile a schema, then validate a valid and
//! an invalid in-memory document against it.
//!
//! Run with `cargo run --example validate`.
//!
//! This crate never parses XML/HTML itself — a real caller implements
//! [`Element`] over whatever tree it already has (an XML DOM, an HTML
//! parser's tree, ...). Here, `Doc` is a tiny hand-rolled tree, built
//! purely to keep this example self-contained (no XML-parser
//! dev-dependency needed just to demonstrate the API).

use relax_ng::{
    Content, DatatypeRegistry, Element, ExpandedName, ResolveError, Schema, SchemaResolver,
    SchemaSource, SchemaSyntax,
};
use std::collections::BTreeMap;

/// An in-memory resolver for `include`/`externalRef` — this crate never
/// reads files or the network itself, so the caller supplies one. This
/// one serves schema text from a `BTreeMap` keyed by `href`; a real
/// resolver would typically read from disk or an HTTP client instead,
/// applying whatever security boundary (allowed schemes, path
/// restrictions) fits the caller.
struct InMemoryResolver {
    sources: BTreeMap<&'static str, &'static str>,
}

impl SchemaResolver for InMemoryResolver {
    fn resolve(&self, href: &str, _base_uri: &str) -> Result<SchemaSource, ResolveError> {
        let text = self
            .sources
            .get(href)
            .ok_or_else(|| ResolveError::new(format!("no such resource: {href}")))?;
        Ok(SchemaSource::new(*text, href, SchemaSyntax::Compact))
    }
}

/// A minimal hand-rolled document tree, just for this example — implement
/// [`Element`] over your own tree type instead.
#[derive(Clone)]
struct Doc {
    name: &'static str,
    attributes: Vec<(&'static str, &'static str)>,
    children: Vec<Doc>,
    text: Option<&'static str>,
}

impl Doc {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            attributes: Vec::new(),
            children: Vec::new(),
            text: None,
        }
    }

    fn attr(mut self, name: &'static str, value: &'static str) -> Self {
        self.attributes.push((name, value));
        self
    }

    fn text(mut self, text: &'static str) -> Self {
        self.text = Some(text);
        self
    }

    fn child(mut self, child: Doc) -> Self {
        self.children.push(child);
        self
    }
}

impl Element for Doc {
    fn name(&self) -> ExpandedName {
        ExpandedName::new(None::<String>, self.name)
    }

    fn attributes(&self) -> impl Iterator<Item = (ExpandedName, String)> {
        self.attributes.iter().map(|(name, value)| {
            (
                ExpandedName::new(None::<String>, *name),
                (*value).to_owned(),
            )
        })
    }

    fn children(&self) -> impl Iterator<Item = Content<Self>> {
        self.children
            .iter()
            .cloned()
            .map(Content::Element)
            .chain(self.text.map(|text| Content::Text(text.to_owned())))
    }
}

fn main() {
    // The schema is self-contained here, but wiring an `include` through
    // the resolver works the same way a multi-file schema would.
    let resolver = InMemoryResolver {
        sources: BTreeMap::new(),
    };

    let source = SchemaSource::new(
        r#"element note {
             attribute author { text },
             element body { text }
           }"#,
        "memory:/note.rnc",
        SchemaSyntax::Compact,
    );
    let schema = Schema::compile(&source, &resolver).expect("schema compiles");

    // The built-in `string`/`token` datatypes are all this schema uses;
    // `DatatypeRegistry::new()` also has XSD built in — see its docs for
    // how to register a caller-specific library on top.
    let registry = DatatypeRegistry::new();

    let valid = Doc::new("note")
        .attr("author", "jane")
        .child(Doc::new("body").text("Remember the milk."));
    let errors = schema
        .validate(&registry, &valid)
        .expect("known datatypes only");
    println!("valid document: {} problem(s)", errors.len());
    assert!(errors.is_empty());

    let invalid = Doc::new("note").child(Doc::new("body").text("Missing the author attribute."));
    let errors = schema
        .validate(&registry, &invalid)
        .expect("known datatypes only");
    println!("invalid document: {} problem(s)", errors.len());
    for error in &errors {
        println!("  - {error}");
    }
    assert!(!errors.is_empty());
}
