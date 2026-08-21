//! RELAX NG's built-in datatype library — the one identified by the empty
//! string `datatypeLibrary=""`, defined by the spec itself (§5.2) rather
//! than by an external library. It has exactly two datatypes, `string` and
//! `token`, neither taking any parameters (already enforced at schema-
//! simplification time, see `simplify::check_builtin_datatype_constraints`).

use super::registry::{DatatypeContext, DatatypeLibrary};

pub(crate) struct BuiltinLibrary;

/// §5.2: whitespace-collapse a `token` value — replace every whitespace
/// character with a space, then collapse runs of spaces and trim. `string`
/// does no processing at all; callers only call this for `token`.
fn collapse_whitespace(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

impl DatatypeLibrary for BuiltinLibrary {
    fn validate_params(&self, type_name: &str, params: &[(&str, &str)]) -> Result<(), String> {
        if type_name != "string" && type_name != "token" {
            return Err(format!(
                "unknown built-in type `{type_name}` (only `string` and `token` exist)"
            ));
        }
        if let Some((name, _)) = params.first() {
            return Err(format!("`{type_name}` takes no parameters, found `{name}`"));
        }
        Ok(())
    }

    /// Every string is a valid `string` or `token` value — neither
    /// restricts which strings are legal; the only place they differ is in
    /// equality (see [`Self::values_equal`]).
    fn matches(
        &self,
        type_name: &str,
        _params: &[(&str, &str)],
        _value: &str,
        _context: &DatatypeContext,
    ) -> bool {
        type_name == "string" || type_name == "token"
    }

    /// §5.2 equality: `string` compares codepoint-for-codepoint; `token`
    /// first collapses whitespace on both sides.
    fn values_equal(
        &self,
        type_name: &str,
        expected: &str,
        _expected_context: &DatatypeContext,
        actual: &str,
        _actual_context: &DatatypeContext,
    ) -> bool {
        if type_name == "token" {
            collapse_whitespace(expected) == collapse_whitespace(actual)
        } else {
            expected == actual
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> DatatypeContext {
        DatatypeContext::empty()
    }

    #[test]
    fn string_equality_is_exact() {
        assert!(!BuiltinLibrary.values_equal("string", "a  b", &context(), "a b", &context()));
        assert!(BuiltinLibrary.values_equal("string", "a b", &context(), "a b", &context()));
    }

    #[test]
    fn token_equality_collapses_whitespace() {
        assert!(BuiltinLibrary.values_equal("token", "a  b", &context(), "a b", &context()));
        assert!(BuiltinLibrary.values_equal("token", "  a b ", &context(), "a b", &context()));
        assert!(!BuiltinLibrary.values_equal("token", "a b", &context(), "a c", &context()));
    }

    #[test]
    fn only_string_and_token_are_known() {
        assert!(BuiltinLibrary.validate_params("string", &[]).is_ok());
        assert!(BuiltinLibrary.validate_params("token", &[]).is_ok());
        assert!(BuiltinLibrary.validate_params("NCName", &[]).is_err());
    }

    #[test]
    fn no_parameters_are_accepted() {
        assert!(
            BuiltinLibrary
                .validate_params("string", &[("pattern", "a*")])
                .is_err()
        );
    }
}
