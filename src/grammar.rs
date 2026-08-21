use std::collections::BTreeMap;

/// A parsed (but not yet composed/simplified) RELAX NG schema, as returned
/// by [`crate::parse`]. Pass it to [`crate::simplify`] (or use
/// [`crate::Schema::compile`], which does both steps) to get a
/// [`crate::CompiledSchema`] ready to validate documents against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaDocument {
    pub(crate) root: Root,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Root {
    Pattern(Pattern),
    Grammar(Grammar),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Grammar {
    pub(crate) context: Context,
    pub(crate) items: Vec<GrammarItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GrammarItem {
    Start {
        combine: Option<Combine>,
        body: Vec<Pattern>,
    },
    Define {
        name: String,
        combine: Option<Combine>,
        body: Vec<Pattern>,
    },
    Div(Vec<GrammarItem>),
    Include {
        href: String,
        inherit_namespace: Option<String>,
        body: Vec<GrammarItem>,
        context: Context,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Combine {
    Choice,
    Interleave,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Pattern {
    Element {
        name: NameClass,
        body: Vec<Pattern>,
        context: Context,
    },
    Attribute {
        name: NameClass,
        body: Vec<Pattern>,
        context: Context,
    },
    Group(Vec<Pattern>),
    Interleave(Vec<Pattern>),
    Choice(Vec<Pattern>),
    Optional(Vec<Pattern>),
    ZeroOrMore(Vec<Pattern>),
    OneOrMore(Vec<Pattern>),
    List(Vec<Pattern>),
    Mixed(Vec<Pattern>),
    Empty,
    NotAllowed,
    Text,
    Data {
        datatype: String,
        datatype_library: Option<String>,
        params: Vec<Param>,
        except: Vec<Pattern>,
        context: Context,
    },
    Value {
        value: String,
        datatype: Option<String>,
        datatype_library: Option<String>,
        namespace: Option<String>,
        context: Context,
    },
    Ref(String),
    ParentRef(String),
    ExternalRef {
        href: String,
        inherit_namespace: Option<String>,
        context: Context,
    },
    Grammar(Grammar),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) context: Context,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NameClass {
    Name {
        lexical: String,
        namespace: Option<String>,
        context: Context,
    },
    Any {
        except: Vec<NameClass>,
        context: Context,
    },
    Namespace {
        namespace: Option<String>,
        except: Vec<NameClass>,
        context: Context,
    },
    Choice(Vec<NameClass>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Context {
    pub(crate) base_uri: String,
    pub(crate) namespaces: BTreeMap<String, String>,
    pub(crate) default_namespace: Option<String>,
    /// §4.3: the nearest ancestor's `datatypeLibrary`, inherited down for
    /// any `data`/`value` that doesn't specify its own.
    pub(crate) datatype_library: Option<String>,
    /// §4.9: the nearest ancestor's `ns`, inherited down for any `name`/
    /// `nsName`/`value` that doesn't specify its own (`None` here means
    /// "never set by any ancestor", which resolves to the empty string —
    /// distinct from an ancestor explicitly setting `ns=""`, though both
    /// end up meaning the same no-namespace default in practice).
    pub(crate) ns: Option<String>,
}
