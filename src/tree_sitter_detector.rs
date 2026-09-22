//! Language registry — the single source of truth mapping file extensions to
//! tree-sitter grammars. The scanner, the extractors, and the MCP tools all
//! consult this table, so "supported" means exactly one thing everywhere.

use serde::{Deserialize, Serialize};
use tree_sitter::Language;

/// A language with a structural extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Java,
}

/// Every supported language, in display order.
pub const ALL: &[Lang] = &[
    Lang::Rust,
    Lang::Python,
    Lang::TypeScript,
    Lang::Tsx,
    Lang::JavaScript,
    Lang::Go,
    Lang::Java,
];

impl Lang {
    /// Map a file extension (case-insensitive, without the dot) to a language.
    pub fn from_ext(ext: &str) -> Option<Lang> {
        let ext = ext.to_ascii_lowercase();
        ALL.iter()
            .copied()
            .find(|lang| lang.extensions().contains(&ext.as_str()))
    }

    /// Map a path (relative or absolute) to a language by its extension.
    pub fn from_path(path: &str) -> Option<Lang> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let (_, ext) = name.rsplit_once('.')?;
        Lang::from_ext(ext)
    }

    /// File extensions handled by this language.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["rs"],
            Lang::Python => &["py", "pyi"],
            Lang::TypeScript => &["ts", "mts", "cts"],
            Lang::Tsx => &["tsx"],
            Lang::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Lang::Go => &["go"],
            Lang::Java => &["java"],
        }
    }

    /// Human-readable language name.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::Python => "Python",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::JavaScript => "JavaScript",
            Lang::Go => "Go",
            Lang::Java => "Java",
        }
    }

    /// The tree-sitter grammar for this language.
    pub fn grammar(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }

    /// Separator used when qualifying a member with its container
    /// (`Type::method` in Rust, `Class.method` elsewhere).
    pub fn scope_separator(self) -> &'static str {
        match self {
            Lang::Rust => "::",
            _ => ".",
        }
    }

    /// Is this one of the ECMAScript dialects?
    pub fn is_ecmascript(self) -> bool {
        matches!(self, Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Is this extension handled by a structural extractor?
pub fn is_supported_ext(ext: &str) -> bool {
    Lang::from_ext(ext).is_some()
}

/// Every supported extension, flattened.
pub fn supported_extensions() -> impl Iterator<Item = &'static str> {
    ALL.iter()
        .flat_map(|lang| lang.extensions().iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension() {
        assert_eq!(Lang::from_ext("rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_ext("ts"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_ext("mts"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_ext("tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::from_ext("cjs"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_ext("py"), Some(Lang::Python));
        assert_eq!(Lang::from_ext("go"), Some(Lang::Go));
        assert_eq!(Lang::from_ext("java"), Some(Lang::Java));
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(Lang::from_ext("RS"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("src/App.TSX"), Some(Lang::Tsx));
    }

    #[test]
    fn unknown_extension_returns_none() {
        assert_eq!(Lang::from_ext("xyz"), None);
        assert_eq!(Lang::from_path("Makefile"), None);
        assert!(!is_supported_ext("md"));
    }

    #[test]
    fn from_path_uses_last_component() {
        assert_eq!(Lang::from_path("a.b/c/main.go"), Some(Lang::Go));
        assert_eq!(Lang::from_path("dir.rs/README"), None);
    }

    #[test]
    fn every_grammar_loads_into_a_parser() {
        for lang in ALL {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&lang.grammar())
                .unwrap_or_else(|e| panic!("{lang}: incompatible grammar: {e}"));
        }
    }

    #[test]
    fn extensions_are_unique_across_languages() {
        let mut seen = std::collections::HashSet::new();
        for ext in supported_extensions() {
            assert!(seen.insert(ext), "extension {ext} claimed twice");
        }
    }
}
