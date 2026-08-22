//! RELAX NG `datatypeLibrary` support: the mandatory built-in library
//! (`""`, `string`/`token`, §5.2), the XSD library (all primitive and
//! derived W3C XML Schema datatypes and facets, per the "Guidelines for
//! using W3C XML Schema Datatypes with RELAX NG"), and the
//! [`registry::DatatypeLibrary`] plugin trait a caller uses to add its own
//! non-standard library (e.g. vnu's `http://whattf.org/datatype-draft`,
//! which this crate deliberately does not implement — see
//! `plan/DECISIONS.md`).

mod builtin;
mod registry;
mod xsd;

pub use registry::XSD_DATATYPE_LIBRARY;
pub use registry::{DatatypeContext, DatatypeError, DatatypeLibrary, DatatypeRegistry};

/// Whitespace-collapse: replace every run of whitespace with a single
/// space and trim the ends. Shared by the built-in library's `token`
/// (§5.2) and the XSD library's `NCName`/`Name`/`NMTOKEN`/... family and
/// `base64Binary` facet (Part 2's `collapse` `whiteSpace` facet).
pub(super) fn collapse_whitespace(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}
