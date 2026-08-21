#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![doc = "A pure-Rust implementation of RELAX NG."]

mod datatypes;
mod grammar;
mod resolver;
mod schema;
mod simplify;
mod syntax;
mod validator;

pub use datatypes::{
    DatatypeContext, DatatypeError, DatatypeLibrary, DatatypeRegistry, XSD_DATATYPE_LIBRARY,
};
pub use grammar::SchemaDocument;
pub use resolver::{ResolveError, SchemaResolver, SchemaSource, SchemaSyntax};
pub use schema::{CompileError, Schema};
pub use simplify::{CompiledSchema, SchemaError, simplify};
pub use syntax::{ParseError, parse};
pub use validator::{
    Content, Element, ExpandedName, ValidationError, ValidationErrorKind, validate,
};
