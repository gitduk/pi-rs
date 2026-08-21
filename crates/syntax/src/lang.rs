use std::path::Path;

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

    /// Kinds that sit *beside* the thing they annotate rather than inside it.
    /// Pointing a block op at one has to reach the declaration that follows, or
    /// the op replaces a bare `#[inline]` and orphans its function.
    ///
    /// Plain comments are deliberately absent: a standalone comment is its own
    /// line, and sweeping the next declaration into it would surprise.
    pub(crate) fn attributes(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["attribute_item"],
            Lang::Go | Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &["decorator"],
            // Python wraps both in `decorated_definition`; nothing to absorb.
            Lang::Python | Lang::Json | Lang::Markdown => &[],
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
