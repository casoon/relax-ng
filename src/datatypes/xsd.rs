//! The XSD datatype library (`http://www.w3.org/2001/XMLSchema-datatypes`):
//! all 19 W3C XML Schema Part 2 primitive datatypes, their built-in derived
//! datatypes, and the facets that apply to each, per the "Guidelines for
//! using W3C XML Schema Datatypes with RELAX NG". Independently implemented
//! from the XSD Part 2 spec — not ported from any existing datatype engine.
//!
//! Known, deliberate simplifications (see `plan/DECISIONS.md`'s Phase 06
//! entry for the full rationale): `decimal`/the integer family use a
//! digit-string based representation (arbitrary lexical precision, no
//! external bignum dependency) rather than a fully general arbitrary-
//! precision arithmetic library; `dateTime`/`date`/`time` ordering
//! normalizes timezone-aware values to UTC and compares timezone-less
//! values component-wise, which is not a byte-for-byte reproduction of
//! XSD Part 2's four-valued (less/greater/equal/incomparable) order
//! relation; `duration` ordering approximates months as 30 days and years
//! as 365 days, since XSD itself only partially orders durations. `pattern`
//! uses the `regex` crate's dialect, which differs in some corners from
//! XSD's own regular expression language (Part 2 Appendix F).

use std::cmp::Ordering;

use regex::Regex;

use super::collapse_whitespace;
use super::registry::{DatatypeContext, DatatypeLibrary};

pub(crate) struct XsdLibrary;

impl DatatypeLibrary for XsdLibrary {
    fn validate_params(&self, type_name: &str, params: &[(&str, &str)]) -> Result<(), String> {
        let kind = TypeKind::lookup(type_name)
            .ok_or_else(|| format!("unknown XSD datatype `{type_name}`"))?;
        for (name, value) in params {
            validate_facet(kind, name, value)?;
        }
        Ok(())
    }

    fn matches(
        &self,
        type_name: &str,
        params: &[(&str, &str)],
        value: &str,
        context: &DatatypeContext,
    ) -> bool {
        let Some(kind) = TypeKind::lookup(type_name) else {
            return false;
        };
        let Some(parsed) = kind.parse(value, context) else {
            return false;
        };
        // `pattern`/`enumeration` combine by union across repeated
        // occurrences of the *same* facet name (XSD's facet combination
        // rules — a `data` element with two `<param name="enumeration">`
        // children means "either value is acceptable", not "both at
        // once"). Every other facet, and every distinct facet name
        // overall, must all be satisfied.
        let (patterns, enumerations): (Vec<_>, Vec<_>) = params
            .iter()
            .filter(|(name, _)| *name == "pattern" || *name == "enumeration")
            .partition(|(name, _)| *name == "pattern");
        let patterns_ok = patterns.is_empty()
            || patterns
                .iter()
                .any(|(_, pattern)| compiled_pattern(pattern).is_match(value.trim()));
        let enumerations_ok = enumerations.is_empty()
            || enumerations.iter().any(|(_, literal)| {
                kind.parse(literal, context)
                    .is_some_and(|literal| literal.equals(&parsed))
            });
        patterns_ok
            && enumerations_ok
            && params
                .iter()
                .filter(|(name, _)| *name != "pattern" && *name != "enumeration")
                .all(|(name, param)| facet_satisfied(kind, name, param, &parsed, context))
    }

    fn values_equal(
        &self,
        type_name: &str,
        expected: &str,
        expected_context: &DatatypeContext,
        actual: &str,
        actual_context: &DatatypeContext,
    ) -> bool {
        let Some(kind) = TypeKind::lookup(type_name) else {
            return false;
        };
        match (
            kind.parse(expected, expected_context),
            kind.parse(actual, actual_context),
        ) {
            (Some(left), Some(right)) => left.equals(&right),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------
// Type table: every primitive and built-in-derived XSD datatype, and the
// family of lexical/value-space rules ("kind") it shares with its peers.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypeKind {
    /// `string` and every type derived from it by restriction only
    /// (`normalizedString`, `token`, `language`, `Name`, `NCName`,
    /// `NMTOKEN`, `ID`, `IDREF`, `ENTITY`, `anyURI`) — share the same value
    /// space (character sequences) and differ only in `whiteSpace`
    /// handling and an extra lexical-shape check.
    StringLike(StringShape),
    /// `NMTOKENS`, `IDREFS`, `ENTITIES` — whitespace-separated lists of an
    /// `NMTOKEN`/`IDREF`/`ENTITY` each.
    ListOf(StringShape),
    Boolean,
    /// `decimal` and its integer derivations. `min`/`max` bound the value
    /// (`None` = unbounded, as for `decimal`/`integer` themselves);
    /// `integer_only` forbids a fractional part.
    Decimal {
        integer_only: bool,
        min: Option<i128>,
        max: Option<i128>,
    },
    Float,
    Double,
    Duration,
    Temporal(TemporalShape),
    Binary(BinaryShape),
    QNameLike,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringShape {
    Any,
    Normalized,
    Token,
    Language,
    Name,
    NCName,
    Nmtoken,
    AnyUri,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TemporalShape {
    DateTime,
    Time,
    Date,
    GYearMonth,
    GYear,
    GMonthDay,
    GDay,
    GMonth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BinaryShape {
    Hex,
    Base64,
}

const INTEGER_DERIVATIONS: &[(&str, Option<i128>, Option<i128>)] = &[
    ("integer", None, None),
    ("nonPositiveInteger", None, Some(0)),
    ("negativeInteger", None, Some(-1)),
    ("nonNegativeInteger", Some(0), None),
    ("positiveInteger", Some(1), None),
    ("long", Some(i64::MIN as i128), Some(i64::MAX as i128)),
    ("int", Some(i32::MIN as i128), Some(i32::MAX as i128)),
    ("short", Some(i16::MIN as i128), Some(i16::MAX as i128)),
    ("byte", Some(i8::MIN as i128), Some(i8::MAX as i128)),
    ("unsignedLong", Some(0), Some(u64::MAX as i128)),
    ("unsignedInt", Some(0), Some(u32::MAX as i128)),
    ("unsignedShort", Some(0), Some(u16::MAX as i128)),
    ("unsignedByte", Some(0), Some(u8::MAX as i128)),
];

impl TypeKind {
    fn lookup(type_name: &str) -> Option<TypeKind> {
        use StringShape as S;
        use TemporalShape as T;
        Some(match type_name {
            "string" => TypeKind::StringLike(S::Any),
            "normalizedString" => TypeKind::StringLike(S::Normalized),
            "token" => TypeKind::StringLike(S::Token),
            "language" => TypeKind::StringLike(S::Language),
            "Name" => TypeKind::StringLike(S::Name),
            "NCName" | "ID" | "IDREF" | "ENTITY" => TypeKind::StringLike(S::NCName),
            "NMTOKEN" => TypeKind::StringLike(S::Nmtoken),
            "anyURI" => TypeKind::StringLike(S::AnyUri),
            "NMTOKENS" => TypeKind::ListOf(S::Nmtoken),
            "IDREFS" => TypeKind::ListOf(S::NCName),
            "ENTITIES" => TypeKind::ListOf(S::NCName),
            "boolean" => TypeKind::Boolean,
            "decimal" => TypeKind::Decimal {
                integer_only: false,
                min: None,
                max: None,
            },
            "float" => TypeKind::Float,
            "double" => TypeKind::Double,
            "duration" => TypeKind::Duration,
            "dateTime" => TypeKind::Temporal(T::DateTime),
            "time" => TypeKind::Temporal(T::Time),
            "date" => TypeKind::Temporal(T::Date),
            "gYearMonth" => TypeKind::Temporal(T::GYearMonth),
            "gYear" => TypeKind::Temporal(T::GYear),
            "gMonthDay" => TypeKind::Temporal(T::GMonthDay),
            "gDay" => TypeKind::Temporal(T::GDay),
            "gMonth" => TypeKind::Temporal(T::GMonth),
            "hexBinary" => TypeKind::Binary(BinaryShape::Hex),
            "base64Binary" => TypeKind::Binary(BinaryShape::Base64),
            "QName" | "NOTATION" => TypeKind::QNameLike,
            _ => {
                for (name, min, max) in INTEGER_DERIVATIONS {
                    if *name == type_name {
                        return Some(TypeKind::Decimal {
                            integer_only: true,
                            min: *min,
                            max: *max,
                        });
                    }
                }
                return None;
            }
        })
    }

    fn parse(self, value: &str, context: &DatatypeContext) -> Option<Value> {
        match self {
            TypeKind::StringLike(shape) => {
                let normalized = normalize_whitespace(value, shape);
                shape_is_satisfied(shape, &normalized).then_some(Value::Text(normalized))
            }
            TypeKind::ListOf(shape) => {
                let mut items = Vec::new();
                for token in value.split_ascii_whitespace() {
                    if !shape_is_satisfied(shape, token) {
                        return None;
                    }
                    items.push(token.to_owned());
                }
                Some(Value::List(items))
            }
            TypeKind::Boolean => parse_boolean(value).map(Value::Boolean),
            TypeKind::Decimal {
                integer_only,
                min,
                max,
            } => {
                let decimal = Decimal::parse(value.trim())?;
                if integer_only && !decimal.fraction_digits.is_empty() {
                    return None;
                }
                if min.is_some() || max.is_some() {
                    // Only the built-in-derived integer types (`byte`,
                    // `unsignedLong`, `positiveInteger`, ...) carry an
                    // implicit range; `decimal`/`integer` themselves have
                    // neither bound and skip this check entirely, so a
                    // magnitude too large for `i128` (astronomically
                    // unlikely in practice, but not a lexical error) isn't
                    // penalized just for not fitting a fixed-width bound
                    // that doesn't apply to it.
                    let magnitude = decimal.to_i128()?;
                    if min.is_some_and(|bound| magnitude < bound)
                        || max.is_some_and(|bound| magnitude > bound)
                    {
                        return None;
                    }
                }
                Some(Value::Decimal(decimal))
            }
            TypeKind::Float => parse_xsd_float(value.trim()).map(Value::Float),
            TypeKind::Double => parse_xsd_float(value.trim()).map(Value::Float),
            TypeKind::Duration => Duration::parse(value.trim()).map(Value::Duration),
            TypeKind::Temporal(shape) => Temporal::parse(shape, value.trim()).map(Value::Temporal),
            TypeKind::Binary(BinaryShape::Hex) => {
                is_hex_binary(value.trim()).then(|| Value::Text(normalize_case(value.trim())))
            }
            TypeKind::Binary(BinaryShape::Base64) => {
                is_base64_binary(value.trim()).then(|| Value::Text(collapse_whitespace(value)))
            }
            TypeKind::QNameLike => parse_qname(value.trim(), context).map(Value::QName),
        }
    }
}

// ---------------------------------------------------------------------
// Values: the parsed form of a lexical value, used for facet checks and
// equality/ordering.
// ---------------------------------------------------------------------

enum Value {
    Text(String),
    List(Vec<String>),
    Boolean(bool),
    Decimal(Decimal),
    Float(f64),
    Duration(Duration),
    Temporal(Temporal),
    QName(ResolvedQName),
}

impl Value {
    fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Decimal(a), Value::Decimal(b)) => a.cmp_value(b) == Ordering::Equal,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a.approx_seconds() == b.approx_seconds(),
            (Value::Temporal(a), Value::Temporal(b)) => a.equals(b),
            (Value::QName(a), Value::QName(b)) => a == b,
            _ => false,
        }
    }

    fn ordering_key(&self) -> Option<f64> {
        match self {
            Value::Decimal(decimal) => Some(decimal.to_f64()),
            Value::Float(value) => Some(*value),
            Value::Duration(duration) => Some(duration.approx_seconds()),
            Value::Temporal(temporal) => temporal.ordering_key(),
            _ => None,
        }
    }

    fn text_length(&self) -> Option<usize> {
        match self {
            Value::Text(text) => Some(text.chars().count()),
            Value::List(items) => Some(items.len()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedQName {
    // Not `Option<String>`: "no default namespace bound" (a document with
    // no `xmlns=` in scope, where `DatatypeContext::resolve("")` finds
    // nothing) and "default namespace explicitly bound to the empty
    // string" (a schema `<value>` whose `ns` — §4.9-inherited or not —
    // is simply unset, always resolved by `schema_value_context` to an
    // explicit `""` binding) must compare equal — the same "absent ≡
    // empty string" convention `simplify::ns_eq` already uses for name
    // classes.
    namespace: String,
    local: String,
}

fn parse_qname(value: &str, context: &DatatypeContext) -> Option<ResolvedQName> {
    let (prefix, local) = match value.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, value),
    };
    if !is_ncname(local) || prefix.is_some_and(|prefix| !is_ncname(prefix)) {
        return None;
    }
    let namespace = match prefix {
        Some(prefix) => context.resolve(prefix)?.to_owned(),
        None => context.resolve("").unwrap_or("").to_owned(),
    };
    Some(ResolvedQName {
        namespace,
        local: local.to_owned(),
    })
}

// ---------------------------------------------------------------------
// String-like family: whiteSpace normalization and lexical shape.
// ---------------------------------------------------------------------

fn normalize_whitespace(value: &str, shape: StringShape) -> String {
    match shape {
        StringShape::Any => value.to_owned(),
        StringShape::Normalized => replace_whitespace(value),
        _ => collapse_whitespace(value),
    }
}

fn replace_whitespace(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn shape_is_satisfied(shape: StringShape, normalized: &str) -> bool {
    match shape {
        StringShape::Any | StringShape::Normalized | StringShape::Token => true,
        StringShape::Language => is_language_tag(normalized),
        StringShape::Name => is_name(normalized),
        StringShape::NCName => is_ncname(normalized),
        StringShape::Nmtoken => !normalized.is_empty() && normalized.chars().all(is_nmtoken_char),
        StringShape::AnyUri => true,
    }
}

fn is_nmtoken_char(c: char) -> bool {
    unicode_ident::is_xid_continue(c) || matches!(c, '.' | '-' | ':' | '_')
}

fn is_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if unicode_ident::is_xid_start(c) || c == ':' || c == '_' => {}
        _ => return false,
    }
    chars.all(is_nmtoken_char)
}

/// XSD's `NCName` is the same production the syntax layer already
/// approximates for RELAX NG's own `name`/`define`/... identifiers — reuse
/// it rather than a second near-identical scanner.
fn is_ncname(value: &str) -> bool {
    crate::syntax::is_valid_ncname(value)
}

fn is_language_tag(value: &str) -> bool {
    // RFC 3066 (as referenced by XSD 1.0): subtag ('-' subtag)*, each
    // subtag 1-8 ASCII letters/digits, first subtag ASCII letters only.
    let mut subtags = value.split('-');
    let Some(first) = subtags.next() else {
        return false;
    };
    if first.is_empty() || first.len() > 8 || !first.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    subtags.all(|subtag| {
        !subtag.is_empty() && subtag.len() <= 8 && subtag.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

// ---------------------------------------------------------------------
// Boolean.
// ---------------------------------------------------------------------

fn parse_boolean(value: &str) -> Option<bool> {
    match value.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Decimal / integer family: a digit-string representation with unbounded
// lexical precision (no external bignum dependency — see module docs).
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Decimal {
    negative: bool,
    integer_digits: String,
    fraction_digits: String,
}

/// Splits off a leading `-` sign (`true` if present); an unsigned `value`
/// (or one that only strips a leading `+`, which `Decimal::parse` handles
/// itself) is returned unchanged with `false`.
fn strip_negative(value: &str) -> (bool, &str) {
    match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    }
}

fn is_ascii_digits(value: &str) -> bool {
    value.chars().all(|c| c.is_ascii_digit())
}

impl Decimal {
    fn parse(value: &str) -> Option<Decimal> {
        let (negative, rest) = strip_negative(value);
        let rest = if negative {
            rest
        } else {
            rest.strip_prefix('+').unwrap_or(rest)
        };
        let (integer_part, fraction_part) = match rest.split_once('.') {
            Some((int_part, frac_part)) => (int_part, frac_part),
            None => (rest, ""),
        };
        if integer_part.is_empty() && fraction_part.is_empty() {
            return None;
        }
        if !is_ascii_digits(integer_part) || !is_ascii_digits(fraction_part) {
            return None;
        }
        let integer_digits = integer_part.trim_start_matches('0');
        let integer_digits = if integer_digits.is_empty() {
            "0"
        } else {
            integer_digits
        };
        let fraction_digits = fraction_part.trim_end_matches('0');
        let negative = negative && (integer_digits != "0" || !fraction_digits.is_empty());
        Some(Decimal {
            negative,
            integer_digits: integer_digits.to_owned(),
            fraction_digits: fraction_digits.to_owned(),
        })
    }

    fn cmp_value(&self, other: &Decimal) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            _ => {}
        }
        let magnitude = compare_magnitude(self, other);
        if self.negative {
            magnitude.reverse()
        } else {
            magnitude
        }
    }

    fn total_digits(&self) -> usize {
        let integer_len = if self.integer_digits == "0" {
            0
        } else {
            self.integer_digits.len()
        };
        integer_len + self.fraction_digits.len()
    }

    fn fraction_digit_count(&self) -> usize {
        self.fraction_digits.len()
    }

    fn to_i128(&self) -> Option<i128> {
        if !self.fraction_digits.is_empty() {
            return None;
        }
        let magnitude: i128 = self.integer_digits.parse().ok()?;
        Some(if self.negative { -magnitude } else { magnitude })
    }

    fn to_f64(&self) -> f64 {
        let text = format!(
            "{}{}.{}",
            if self.negative { "-" } else { "" },
            self.integer_digits,
            if self.fraction_digits.is_empty() {
                "0"
            } else {
                &self.fraction_digits
            }
        );
        text.parse().unwrap_or(0.0)
    }
}

fn compare_magnitude(a: &Decimal, b: &Decimal) -> Ordering {
    match a.integer_digits.len().cmp(&b.integer_digits.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    match a.integer_digits.cmp(&b.integer_digits) {
        Ordering::Equal => {}
        other => return other,
    }
    let max_fraction_len = a.fraction_digits.len().max(b.fraction_digits.len());
    let a_fraction = pad_right(&a.fraction_digits, max_fraction_len);
    let b_fraction = pad_right(&b.fraction_digits, max_fraction_len);
    a_fraction.cmp(&b_fraction)
}

fn pad_right(digits: &str, len: usize) -> String {
    let mut owned = digits.to_owned();
    while owned.len() < len {
        owned.push('0');
    }
    owned
}

fn parse_xsd_float(value: &str) -> Option<f64> {
    match value {
        "NaN" => Some(f64::NAN),
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        _ => {
            if value.is_empty() || value.contains(char::is_whitespace) {
                return None;
            }
            value.parse().ok()
        }
    }
}

// ---------------------------------------------------------------------
// Duration: PnYnMnDTnHnMnS, approximated to a total-seconds ordering key
// (30-day months, 365-day years — see module docs).
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Duration {
    negative: bool,
    years: u64,
    months: u64,
    days: u64,
    hours: u64,
    minutes: u64,
    seconds: f64,
}

impl Duration {
    fn parse(value: &str) -> Option<Duration> {
        let (negative, rest) = strip_negative(value);
        let rest = rest.strip_prefix('P')?;
        let (date_part, time_part) = match rest.split_once('T') {
            Some((date, time)) => (date, Some(time)),
            None => (rest, None),
        };
        let mut duration = Duration {
            negative,
            years: 0,
            months: 0,
            days: 0,
            hours: 0,
            minutes: 0,
            seconds: 0.0,
        };
        let mut any_field = false;
        let mut cursor = date_part;
        for suffix in ['Y', 'M', 'D'] {
            if let Some(index) = cursor.find(suffix) {
                let (digits, remainder) = cursor.split_at(index);
                let value: u64 = digits.parse().ok()?;
                match suffix {
                    'Y' => duration.years = value,
                    'M' => duration.months = value,
                    'D' => duration.days = value,
                    _ => unreachable!(),
                }
                cursor = &remainder[1..];
                any_field = true;
            }
        }
        if !cursor.is_empty() {
            return None;
        }
        if let Some(time_part) = time_part {
            let mut cursor = time_part;
            for suffix in ['H', 'M'] {
                if let Some(index) = cursor.find(suffix) {
                    let (digits, remainder) = cursor.split_at(index);
                    let value: u64 = digits.parse().ok()?;
                    match suffix {
                        'H' => duration.hours = value,
                        'M' => duration.minutes = value,
                        _ => unreachable!(),
                    }
                    cursor = &remainder[1..];
                    any_field = true;
                }
            }
            if let Some(index) = cursor.find('S') {
                let digits = &cursor[..index];
                duration.seconds = digits.parse().ok()?;
                cursor = &cursor[index + 1..];
                any_field = true;
            }
            if !cursor.is_empty() {
                return None;
            }
        }
        any_field.then_some(duration)
    }

    fn approx_seconds(&self) -> f64 {
        let magnitude = self.years as f64 * 365.0 * 86400.0
            + self.months as f64 * 30.0 * 86400.0
            + self.days as f64 * 86400.0
            + self.hours as f64 * 3600.0
            + self.minutes as f64 * 60.0
            + self.seconds;
        if self.negative { -magnitude } else { magnitude }
    }
}

// ---------------------------------------------------------------------
// Temporal family: dateTime/time/date/gYearMonth/gYear/gMonthDay/gDay/
// gMonth, normalized to UTC-relative ordinal seconds when a timezone is
// present.
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Temporal {
    year: Option<i64>,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<f64>,
    timezone_offset_minutes: Option<i32>,
}

impl Temporal {
    fn parse(shape: TemporalShape, value: &str) -> Option<Temporal> {
        let (body, timezone_offset_minutes) = split_timezone(value)?;
        let mut temporal = Temporal {
            year: None,
            month: None,
            day: None,
            hour: None,
            minute: None,
            second: None,
            timezone_offset_minutes,
        };
        match shape {
            TemporalShape::DateTime => {
                let (date, time) = body.split_once('T')?;
                parse_date_into(date, &mut temporal)?;
                parse_time_into(time, &mut temporal)?;
            }
            TemporalShape::Time => parse_time_into(body, &mut temporal)?,
            TemporalShape::Date => parse_date_into(body, &mut temporal)?,
            TemporalShape::GYearMonth => {
                let (year, month) = body.split_once('-')?;
                temporal.year = Some(parse_year(year)?);
                temporal.month = Some(parse_component(month, 1, 12)?);
            }
            TemporalShape::GYear => temporal.year = Some(parse_year(body)?),
            TemporalShape::GMonthDay => {
                let body = body.strip_prefix("--")?;
                let (month, day) = body.split_once('-')?;
                temporal.month = Some(parse_component(month, 1, 12)?);
                temporal.day = Some(parse_component(day, 1, 31)?);
            }
            TemporalShape::GDay => {
                let body = body.strip_prefix("---")?;
                temporal.day = Some(parse_component(body, 1, 31)?);
            }
            TemporalShape::GMonth => {
                let body = body.strip_prefix("--")?;
                temporal.month = Some(parse_component(body, 1, 12)?);
            }
        }
        Some(temporal)
    }

    /// UTC-normalized ordinal seconds from an epoch, used for ordering and
    /// equality — only meaningful when the two values being compared
    /// share the same set of present fields (guaranteed here, since they
    /// share a schema `type_name`/`TemporalShape`) and at least one of
    /// year/day/etc. is present (true for every XSD temporal shape).
    fn ordering_key(&self) -> Option<f64> {
        let days = days_since_epoch(
            self.year.unwrap_or(1972),
            self.month.unwrap_or(1),
            self.day.unwrap_or(1),
        );
        let mut seconds = days as f64 * 86400.0
            + self.hour.unwrap_or(0) as f64 * 3600.0
            + self.minute.unwrap_or(0) as f64 * 60.0
            + self.second.unwrap_or(0.0);
        if let Some(offset) = self.timezone_offset_minutes {
            seconds -= offset as f64 * 60.0;
        }
        Some(seconds)
    }

    fn equals(&self, other: &Temporal) -> bool {
        self.ordering_key() == other.ordering_key()
    }
}

fn days_since_epoch(year: i64, month: u32, day: u32) -> i64 {
    // Days from a fixed epoch (0000-03-01) using the standard proleptic-
    // Gregorian day-count algorithm (Howard Hinnant's `days_from_civil`).
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * month_index + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn split_timezone(value: &str) -> Option<(&str, Option<i32>)> {
    if let Some(body) = value.strip_suffix('Z') {
        return Some((body, Some(0)));
    }
    // A trailing `[+-]HH:MM`, but not the leading sign of the year itself.
    if value.len() > 6 {
        let tail = &value[value.len() - 6..];
        if let Some(sign_char) = tail.chars().next()
            && (sign_char == '+' || sign_char == '-')
            && tail.as_bytes()[3] == b':'
        {
            let hours: i32 = tail[1..3].parse().ok()?;
            let minutes: i32 = tail[4..6].parse().ok()?;
            let offset = hours * 60 + minutes;
            let offset = if sign_char == '-' { -offset } else { offset };
            return Some((&value[..value.len() - 6], Some(offset)));
        }
    }
    Some((value, None))
}

fn parse_date_into(date: &str, temporal: &mut Temporal) -> Option<()> {
    let mut parts = date.splitn(3, '-');
    // A negative year keeps its leading `-` attached to the first split
    // segment via `splitn` only if we special-case it — dates never start
    // with `-` in practice for HTML content, so a leading '-' year is
    // treated as: sign, then the normal Y-M-D split of the remainder.
    let (year_str, month_str, day_str) = if let Some(rest) = date.strip_prefix('-') {
        let mut parts = rest.splitn(3, '-');
        (
            format!("-{}", parts.next()?),
            parts.next()?.to_owned(),
            parts.next()?.to_owned(),
        )
    } else {
        (
            parts.next()?.to_owned(),
            parts.next()?.to_owned(),
            parts.next()?.to_owned(),
        )
    };
    temporal.year = Some(parse_year(&year_str)?);
    temporal.month = Some(parse_component(&month_str, 1, 12)?);
    temporal.day = Some(parse_component(&day_str, 1, 31)?);
    Some(())
}

fn parse_time_into(time: &str, temporal: &mut Temporal) -> Option<()> {
    let mut parts = time.splitn(3, ':');
    let hour_str = parts.next()?;
    let minute_str = parts.next()?;
    let second_str = parts.next()?;
    let hour: u32 = hour_str.parse().ok()?;
    let minute: u32 = parse_component(minute_str, 0, 59)?;
    let second: f64 = second_str.parse().ok()?;
    if hour > 24 || (hour == 24 && (minute != 0 || second != 0.0)) || second >= 60.0 {
        return None;
    }
    temporal.hour = Some(hour);
    temporal.minute = Some(minute);
    temporal.second = Some(second);
    Some(())
}

fn parse_year(value: &str) -> Option<i64> {
    let (negative, digits) = strip_negative(value);
    if digits.len() < 4 || !is_ascii_digits(digits) {
        return None;
    }
    let magnitude: i64 = digits.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

fn parse_component(value: &str, min: u32, max: u32) -> Option<u32> {
    if value.len() != 2 || !is_ascii_digits(value) {
        return None;
    }
    let parsed: u32 = value.parse().ok()?;
    (min..=max).contains(&parsed).then_some(parsed)
}

// ---------------------------------------------------------------------
// Binary family.
// ---------------------------------------------------------------------

fn is_hex_binary(value: &str) -> bool {
    value.len().is_multiple_of(2) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn normalize_case(value: &str) -> String {
    value.to_ascii_uppercase()
}

fn is_base64_binary(value: &str) -> bool {
    let collapsed = collapse_whitespace(value);
    let stripped: String = collapsed.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.is_empty() {
        return true;
    }
    if !stripped.len().is_multiple_of(4) {
        return false;
    }
    let body = stripped.trim_end_matches('=');
    if stripped.len() - body.len() > 2 {
        return false;
    }
    body.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

// ---------------------------------------------------------------------
// Facets.
// ---------------------------------------------------------------------

fn is_ordered(kind: TypeKind) -> bool {
    matches!(
        kind,
        TypeKind::Decimal { .. }
            | TypeKind::Float
            | TypeKind::Double
            | TypeKind::Duration
            | TypeKind::Temporal(_)
    )
}

fn has_length(kind: TypeKind) -> bool {
    matches!(kind, TypeKind::StringLike(_) | TypeKind::ListOf(_))
}

fn validate_facet(kind: TypeKind, name: &str, value: &str) -> Result<(), String> {
    match name {
        "pattern" => Regex::new(&format!("^(?:{value})$"))
            .map(drop)
            .map_err(|error| format!("invalid `pattern` facet: {error}")),
        "enumeration" => Ok(()),
        "whiteSpace" => {
            if matches!(value, "preserve" | "replace" | "collapse") {
                Ok(())
            } else {
                Err(format!("invalid `whiteSpace` facet value `{value}`"))
            }
        }
        "length" | "minLength" | "maxLength" => {
            if !has_length(kind) {
                return Err(format!("`{name}` does not apply to this datatype"));
            }
            value
                .parse::<usize>()
                .map(drop)
                .map_err(|_| format!("`{name}` must be a non-negative integer, found `{value}`"))
        }
        "totalDigits" => match kind {
            TypeKind::Decimal { .. } => value
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(drop)
                .ok_or_else(|| {
                    format!("`totalDigits` must be a positive integer, found `{value}`")
                }),
            _ => Err("`totalDigits` only applies to decimal-derived datatypes".to_owned()),
        },
        "fractionDigits" => match kind {
            TypeKind::Decimal { .. } => value.parse::<usize>().map(drop).map_err(|_| {
                format!("`fractionDigits` must be a non-negative integer, found `{value}`")
            }),
            _ => Err("`fractionDigits` only applies to decimal-derived datatypes".to_owned()),
        },
        "minInclusive" | "minExclusive" | "maxInclusive" | "maxExclusive" => {
            if !is_ordered(kind) {
                return Err(format!("`{name}` does not apply to this datatype"));
            }
            kind.parse(value, &DatatypeContext::empty())
                .map(drop)
                .ok_or_else(|| {
                    format!("`{name}` value `{value}` is not a valid literal of this datatype")
                })
        }
        _ => Err(format!("unknown facet `{name}`")),
    }
}

/// Checks one facet other than `pattern`/`enumeration` — those two combine
/// by union across repeated occurrences of the same name, so `matches`
/// (the only caller) handles them itself before ever calling this.
fn facet_satisfied(
    kind: TypeKind,
    name: &str,
    param: &str,
    parsed: &Value,
    context: &DatatypeContext,
) -> bool {
    match name {
        "whiteSpace" => true, // already reflected in how `parsed` was built
        "length" => parsed.text_length() == param.parse().ok(),
        "minLength" => param
            .parse()
            .is_ok_and(|min: usize| parsed.text_length().is_some_and(|len| len >= min)),
        "maxLength" => param
            .parse()
            .is_ok_and(|max: usize| parsed.text_length().is_some_and(|len| len <= max)),
        "totalDigits" => match parsed {
            Value::Decimal(decimal) => param.parse() == Ok(decimal.total_digits().max(1)),
            _ => true,
        },
        "fractionDigits" => match parsed {
            Value::Decimal(decimal) => param
                .parse()
                .is_ok_and(|max: usize| decimal.fraction_digit_count() <= max),
            _ => true,
        },
        "minInclusive" => compare_ordered(kind, param, parsed, context)
            .is_some_and(|order| order != Ordering::Greater),
        "minExclusive" => compare_ordered(kind, param, parsed, context)
            .is_some_and(|order| order == Ordering::Less),
        "maxInclusive" => compare_ordered(kind, param, parsed, context)
            .is_some_and(|order| order != Ordering::Less),
        "maxExclusive" => compare_ordered(kind, param, parsed, context)
            .is_some_and(|order| order == Ordering::Greater),
        _ => true,
    }
}

/// Orders the facet's literal `param` relative to `parsed`, from the
/// facet's point of view (`Less` means the facet bound is less than the
/// value). `Decimal`s compare via exact digit-string arithmetic; every
/// other ordered kind compares via its `f64` ordering key (NaN never
/// satisfies any bound, matching IEEE 754/XSD `float`/`double` semantics).
fn compare_ordered(
    kind: TypeKind,
    param: &str,
    parsed: &Value,
    context: &DatatypeContext,
) -> Option<Ordering> {
    let bound = kind.parse(param, context)?;
    if let (Value::Decimal(bound), Value::Decimal(value)) = (&bound, parsed) {
        return Some(bound.cmp_value(value));
    }
    let bound_key = bound.ordering_key()?;
    let value_key = parsed.ordering_key()?;
    bound_key.partial_cmp(&value_key)
}

/// Compiles `pattern` fresh on every call. `validate_params` already
/// rejected any `pattern` facet that doesn't compile, so this always
/// succeeds in practice; a per-schema pattern cache would save repeated
/// compilation across many `matches` calls, but that's a performance
/// concern deliberately deferred (see `plan/05-validation-engine.md`'s
/// "not prematurely optimizing" note, which applies here too) rather than
/// something to build speculatively now.
fn compiled_pattern(pattern: &str) -> Regex {
    Regex::new(&format!("^(?:{pattern})$")).expect("validated by validate_params")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(type_name: &str, params: &[(&str, &str)], value: &str) -> bool {
        XsdLibrary.matches(type_name, params, value, &DatatypeContext::empty())
    }

    fn equal(type_name: &str, expected: &str, actual: &str) -> bool {
        XsdLibrary.values_equal(
            type_name,
            expected,
            &DatatypeContext::empty(),
            actual,
            &DatatypeContext::empty(),
        )
    }

    #[test]
    fn boolean_lexical_forms() {
        assert!(matches("boolean", &[], "true"));
        assert!(matches("boolean", &[], "1"));
        assert!(matches("boolean", &[], "false"));
        assert!(matches("boolean", &[], "0"));
        assert!(!matches("boolean", &[], "yes"));
        assert!(equal("boolean", "true", "1"));
        assert!(equal("boolean", "false", "0"));
        assert!(!equal("boolean", "true", "false"));
    }

    #[test]
    fn decimal_equality_ignores_lexical_spelling() {
        assert!(equal("decimal", "1.50", "01.5"));
        assert!(equal("decimal", "0", "-0.0"));
        assert!(!equal("decimal", "1.5", "1.500001"));
    }

    #[test]
    fn decimal_min_max_inclusive_facets() {
        assert!(matches(
            "decimal",
            &[("minInclusive", "0"), ("maxInclusive", "10")],
            "10"
        ));
        assert!(!matches(
            "decimal",
            &[("minInclusive", "0"), ("maxInclusive", "10")],
            "10.01"
        ));
        assert!(!matches("decimal", &[("minExclusive", "0")], "0"));
    }

    #[test]
    fn decimal_total_and_fraction_digits_facets() {
        assert!(matches("decimal", &[("totalDigits", "3")], "1.23"));
        assert!(!matches("decimal", &[("totalDigits", "3")], "1.234"));
        assert!(matches("decimal", &[("fractionDigits", "2")], "1.2"));
        assert!(!matches("decimal", &[("fractionDigits", "2")], "1.234"));
    }

    #[test]
    fn integer_derivations_enforce_their_implicit_range() {
        assert!(matches("byte", &[], "127"));
        assert!(matches("byte", &[], "-128"));
        assert!(!matches("byte", &[], "128"));
        assert!(!matches("byte", &[], "-129"));
        assert!(matches("unsignedByte", &[], "255"));
        assert!(!matches("unsignedByte", &[], "-1"));
        assert!(!matches("unsignedByte", &[], "256"));
        assert!(matches("positiveInteger", &[], "1"));
        assert!(!matches("positiveInteger", &[], "0"));
        assert!(matches("nonNegativeInteger", &[], "0"));
        assert!(!matches("nonNegativeInteger", &[], "-1"));
        assert!(!matches("byte", &[], "1.5")); // integer types reject a fraction
    }

    #[test]
    fn float_and_double_special_values() {
        assert!(matches("float", &[], "NaN"));
        assert!(matches("float", &[], "INF"));
        assert!(matches("float", &[], "-INF"));
        assert!(matches("double", &[], "3.14"));
        // NaN never satisfies any bound, matching IEEE 754.
        assert!(!matches("float", &[("minInclusive", "0")], "NaN"));
        assert!(!equal("float", "NaN", "NaN"));
    }

    #[test]
    fn duration_lexical_shape_and_rough_ordering() {
        assert!(matches("duration", &[], "P1Y2M3DT4H5M6S"));
        assert!(matches("duration", &[], "PT1H"));
        assert!(matches("duration", &[], "-P1D"));
        assert!(!matches("duration", &[], "P"));
        assert!(!matches("duration", &[], "1Y"));
        assert!(matches("duration", &[("minInclusive", "PT1H")], "PT2H"));
        assert!(!matches("duration", &[("minInclusive", "PT3H")], "PT2H"));
    }

    #[test]
    fn date_time_lexical_shape() {
        assert!(matches("dateTime", &[], "2024-01-02T03:04:05"));
        assert!(matches("dateTime", &[], "2024-01-02T03:04:05Z"));
        assert!(matches("dateTime", &[], "2024-01-02T03:04:05+02:00"));
        assert!(matches("dateTime", &[], "2024-01-02T24:00:00"));
        assert!(!matches("dateTime", &[], "2024-01-02"));
        assert!(!matches("dateTime", &[], "2024-13-02T00:00:00"));
    }

    #[test]
    fn date_time_equality_normalizes_timezones_to_utc() {
        assert!(equal(
            "dateTime",
            "2024-01-02T03:00:00Z",
            "2024-01-02T05:00:00+02:00"
        ));
        assert!(!equal(
            "dateTime",
            "2024-01-02T03:00:00Z",
            "2024-01-02T03:00:00+02:00"
        ));
    }

    #[test]
    fn date_time_ordering_facets() {
        assert!(matches(
            "dateTime",
            &[("minInclusive", "2024-01-01T00:00:00Z")],
            "2024-06-01T00:00:00Z"
        ));
        assert!(!matches(
            "dateTime",
            &[("maxExclusive", "2024-01-01T00:00:00Z")],
            "2024-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn date_and_gregorian_fragment_shapes() {
        assert!(matches("date", &[], "2024-01-02"));
        assert!(matches("date", &[], "-0044-01-02")); // a negative (BCE) year
        assert!(matches("gYearMonth", &[], "2024-01"));
        assert!(matches("gYear", &[], "2024"));
        assert!(matches("gMonthDay", &[], "--01-02"));
        assert!(matches("gDay", &[], "---02"));
        assert!(matches("gMonth", &[], "--01"));
        assert!(!matches("gMonth", &[], "--13"));
        assert!(matches("time", &[], "03:04:05"));
    }

    #[test]
    fn binary_family() {
        assert!(matches("hexBinary", &[], "0FB7"));
        assert!(!matches("hexBinary", &[], "0FB")); // odd length
        assert!(!matches("hexBinary", &[], "0FBZ"));
        assert!(equal("hexBinary", "0fb7", "0FB7"));
        assert!(matches("base64Binary", &[], "YQ=="));
        assert!(matches("base64Binary", &[], ""));
        assert!(!matches("base64Binary", &[], "!!!!"));
    }

    #[test]
    fn ncname_language_and_name_shapes() {
        assert!(matches("NCName", &[], "foo-bar_1"));
        assert!(!matches("NCName", &[], "1foo")); // can't start with a digit
        assert!(!matches("NCName", &[], "foo:bar")); // no colons allowed
        assert!(matches("Name", &[], "foo:bar")); // Name allows colons
        assert!(matches("language", &[], "en-US"));
        assert!(!matches("language", &[], "thisIsWayTooLongForASubtag"));
        assert!(matches("anyURI", &[], "https://example.org/x?y=1"));
    }

    #[test]
    fn list_derived_types() {
        assert!(matches("NMTOKENS", &[], "a b c"));
        assert!(matches("NMTOKENS", &[], "a b:c d")); // NMTOKEN allows ':'
        assert!(!matches("NMTOKENS", &[], "a b c!")); // '!' is not a NameChar
        assert!(matches("IDREFS", &[], "a b c"));
        assert!(!matches("IDREFS", &[], "1a b")); // not valid NCNames
    }

    #[test]
    fn length_and_enumeration_facets() {
        assert!(matches("string", &[("length", "3")], "abc"));
        assert!(!matches("string", &[("length", "3")], "ab"));
        assert!(matches(
            "string",
            &[("minLength", "2"), ("maxLength", "4")],
            "abc"
        ));
        assert!(!matches(
            "string",
            &[("minLength", "2"), ("maxLength", "4")],
            "abcde"
        ));
        assert!(matches(
            "string",
            &[("enumeration", "a"), ("enumeration", "b")],
            "b"
        ));
        assert!(!matches(
            "string",
            &[("enumeration", "a"), ("enumeration", "b")],
            "c"
        ));
    }

    #[test]
    fn pattern_facet_is_fully_anchored() {
        assert!(matches("string", &[("pattern", "[a-z]+")], "abc"));
        assert!(!matches("string", &[("pattern", "[a-z]+")], "abc1"));
        assert!(!matches("string", &[("pattern", "[a-z]+")], "1abc"));
    }

    #[test]
    fn qname_requires_a_resolvable_prefix() {
        let bound = DatatypeContext::from_bindings(vec![("h".to_owned(), "urn:h".to_owned())]);
        assert!(XsdLibrary.matches("QName", &[], "h:widget", &bound));
        assert!(!XsdLibrary.matches("QName", &[], "h:widget", &DatatypeContext::empty()));
        assert!(XsdLibrary.matches("QName", &[], "widget", &DatatypeContext::empty()));
    }

    #[test]
    fn validate_params_rejects_unknown_types_and_inapplicable_facets() {
        assert!(XsdLibrary.validate_params("string", &[]).is_ok());
        assert!(XsdLibrary.validate_params("not-a-type", &[]).is_err());
        assert!(
            XsdLibrary
                .validate_params("string", &[("totalDigits", "3")])
                .is_err()
        );
        assert!(
            XsdLibrary
                .validate_params("decimal", &[("minInclusive", "0")])
                .is_ok()
        );
        assert!(
            XsdLibrary
                .validate_params("string", &[("pattern", "[")])
                .is_err()
        );
    }
}
