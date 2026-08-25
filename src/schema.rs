//! The public entry point: [`Schema`] ties `parse`/`simplify`/`validate`
//! together into a single "compile once, validate many documents"
//! workflow, which is almost always what a caller wants — the lower-level
//! [`crate::parse`]/[`crate::simplify`]/[`crate::validate`] functions
//! remain available separately for callers who need the intermediate
//! [`crate::SchemaDocument`]/[`CompiledSchema`] (e.g. to inspect a schema
//! without validating anything against it).

use std::fmt;

use crate::{
    CompiledSchema, DatatypeError, DatatypeRegistry, Element, ParseError, SchemaError,
    SchemaResolver, SchemaSource, ValidationError, parse, simplify, validate,
};

/// A parsed and composed ("simplified", in RELAX NG spec terms) schema,
/// ready to validate any number of documents against.
///
/// Compiling — parsing the schema text and resolving/composing any
/// `include`/`externalRef`/`div`/`combine` it uses — is the relatively
/// expensive step; [`Schema::compile`] does it once, and the resulting
/// `Schema` is then reused for every document validated against it.
///
/// # Example
///
/// ```
/// use relax_ng::{
///     Content, DatatypeRegistry, Element, ExpandedName, ResolveError, Schema, SchemaResolver,
///     SchemaSource, SchemaSyntax,
/// };
///
/// // This schema is self-contained (no `include`/`externalRef`), so the
/// // resolver is never actually called — it only needs to exist to
/// // satisfy `Schema::compile`'s signature.
/// struct NoResolver;
/// impl SchemaResolver for NoResolver {
///     fn resolve(&self, href: &str, _base_uri: &str) -> Result<SchemaSource, ResolveError> {
///         Err(ResolveError::new(format!("no such resource: {href}")))
///     }
/// }
///
/// let source = SchemaSource::new(
///     "element greeting { attribute lang { text }, text }",
///     "memory:/greeting.rnc",
///     SchemaSyntax::Compact,
/// );
/// let schema = Schema::compile(&source, &NoResolver).expect("schema compiles");
///
/// // A minimal `Element` implementation over an in-memory tree — a real
/// // caller implements this over whatever tree type it already has (an
/// // XML DOM, an HTML parser's tree, ...); this crate parses none of
/// // that itself.
/// struct Doc {
///     name: &'static str,
///     attributes: Vec<(&'static str, &'static str)>,
///     text: &'static str,
/// }
/// impl Element for Doc {
///     type Location = ();
///
///     fn name(&self) -> ExpandedName {
///         ExpandedName::new(None::<String>, self.name)
///     }
///     fn attributes(&self) -> impl Iterator<Item = (ExpandedName, String)> {
///         self.attributes
///             .iter()
///             .map(|(name, value)| (ExpandedName::new(None::<String>, *name), value.to_string()))
///     }
///     fn children(&self) -> impl Iterator<Item = Content<Self>> {
///         std::iter::once(Content::Text(self.text.to_owned()))
///     }
/// }
///
/// let document = Doc {
///     name: "greeting",
///     attributes: vec![("lang", "en")],
///     text: "hello",
/// };
///
/// // The built-in `string`/`token` datatypes are all this schema needs.
/// let errors = schema
///     .validate(&DatatypeRegistry::new(), &document)
///     .expect("only built-in/XSD datatypes are used");
/// assert!(errors.is_empty());
/// ```
#[derive(Clone, Debug)]
pub struct Schema {
    compiled: CompiledSchema,
}

impl Schema {
    /// Parses `source` and composes it into a ready-to-validate schema.
    /// `resolver` is consulted for any `include`/`externalRef` the schema
    /// (transitively) uses; if the schema is known to be self-contained,
    /// any resolver that always returns `Err` works (it will never
    /// actually be called).
    pub fn compile(
        source: &SchemaSource,
        resolver: &impl SchemaResolver,
    ) -> Result<Schema, CompileError> {
        let document = parse(source)?;
        let compiled = simplify(&document, resolver)?;
        Ok(Schema { compiled })
    }

    /// Validates `root` (and, transitively, everything reachable from it)
    /// against this schema. `registry` supplies the `data`/`value`
    /// pattern semantics — see [`DatatypeRegistry`] for what's built in
    /// and how to add more.
    ///
    /// Returns the list of problems found — empty means `root` is valid.
    /// Returns [`DatatypeError`] instead of validating at all if the
    /// schema uses a `datatypeLibrary`/type/parameter `registry` doesn't
    /// recognize, since whether the document would pass a check that
    /// can't be performed is unknowable.
    pub fn validate<E: Element>(
        &self,
        registry: &DatatypeRegistry,
        root: &E,
    ) -> Result<Vec<ValidationError<E::Location>>, DatatypeError> {
        validate(&self.compiled, registry, root)
    }

    /// The underlying compiled representation, for callers that need it
    /// directly (e.g. [`CompiledSchema::definition_count`] for
    /// diagnostics) rather than going through [`Schema::validate`].
    pub fn compiled(&self) -> &CompiledSchema {
        &self.compiled
    }
}

/// Either step [`Schema::compile`] performs can fail on its own: `source`
/// might not even parse (malformed `.rnc`/`.rng` syntax — [`ParseError`]),
/// or it might parse but fail composition (an undefined `ref`, illegal
/// recursion, a §7 restriction violation, ... — [`SchemaError`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompileError {
    /// `source` isn't well-formed RELAX NG in its declared syntax.
    Parse(ParseError),
    /// `source` parsed, but composing/simplifying it failed (an undefined
    /// `ref`, illegal recursion, a §7 restriction violation, ...).
    Schema(SchemaError),
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "{error}"),
            Self::Schema(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Schema(error) => Some(error),
        }
    }
}

impl From<ParseError> for CompileError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl From<SchemaError> for CompileError {
    fn from(error: SchemaError) -> Self {
        Self::Schema(error)
    }
}
