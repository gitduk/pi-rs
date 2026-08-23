use std::path::Path;

/// What makes a node an annotation of what follows it.
///
/// Three cases because the grammars offer three different signals, and taking
/// the strongest one each offers is the difference between right and nearly
/// right: tree-sitter-rust tags its own doc comments, and knows that `////` is
/// not one — a check on the `///` prefix does not.
pub(crate) enum Mark {
    /// Any node of this kind: an attribute, a decorator.
    Kind(&'static str),
    /// A comment the grammar itself marks as documentation *of what follows*.
    /// Rust's `//!` and `/*!` document the enclosing module instead, and are
    /// the same node kind carrying the same `doc` field.
    Outer(&'static str),
    /// A comment whose opener says so, where the grammar draws no line.
    Opener(&'static str, &'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Json,
    Markdown,
}

impl Lang {
    pub fn of(path: &str) -> Option<Self> {
        let ext = Path::new(path).extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "json" => Lang::Json,
            "md" | "markdown" => Lang::Markdown,
            _ => return None,
        })
    }

    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
        }
    }

    /// Node kinds worth showing in an outline. Everything else is body.
    pub(crate) fn declarations(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "impl_item",
                "mod_item",
                "type_item",
                "const_item",
                "static_item",
                "macro_definition",
                "union_item",
            ],
            Lang::Python => &[
                "function_definition",
                "class_definition",
                "decorated_definition",
            ],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
                "function_declaration",
                "generator_function_declaration",
                "class_declaration",
                "method_definition",
                "lexical_declaration",
                "variable_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
                "abstract_class_declaration",
                "export_statement",
            ],
            Lang::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
                "const_declaration",
                "var_declaration",
            ],
            Lang::Json => &["pair"],
            Lang::Markdown => &["atx_heading", "setext_heading"],
        }
    }

    /// How a node says it annotates whatever it touches.
    ///
    /// A construct owns the annotations directly above it, so replacing a
    /// function replaces its `#[inline]` and its `///` with it — orphaning
    /// either is what a reader would call a bug. Only adjacency binds them: a
    /// blank line between a comment and the next declaration means the comment
    /// was talking about something else.
    pub(crate) fn annotations(self) -> &'static [Mark] {
        match self {
            Lang::Rust => &[
                Mark::Kind("attribute_item"),
                Mark::Outer("line_comment"),
                Mark::Outer("block_comment"),
            ],
            // godoc reads the plain comment above a declaration as its
            // documentation, and the grammar draws no line for us.
            Lang::Go => &[Mark::Opener("comment", "//")],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                &[Mark::Kind("decorator"), Mark::Opener("comment", "/**")]
            }
            // A docstring sits inside the body; nothing above a `def` attaches.
            Lang::Python => &[Mark::Kind("decorator")],
            // A markdown heading annotates nothing above it, and a comment is
            // not JSON.
            Lang::Json | Lang::Markdown => &[],
        }
    }

    /// Declarations that hold other declarations, so an outline descends into
    /// them rather than stopping at the wrapper.
    pub(crate) fn containers(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["impl_item", "mod_item", "trait_item"],
            Lang::Python => &["class_definition", "decorated_definition"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
                "class_declaration",
                "abstract_class_declaration",
                "export_statement",
                "class_body",
            ],
            Lang::Go => &["type_declaration"],
            Lang::Json | Lang::Markdown => &[],
        }
    }
}
