//! Tree-sitter language detection — maps file extensions to tree-sitter
//! language parsers. Supports Rust, Python, TypeScript/JavaScript, Go, Java.

use tree_sitter::Language;

/// The supported languages and their tree-sitter language functions.
/// Each entry: (language name, extensions, tree-sitter language fn).
#[derive(Clone, Copy)]
pub struct DetectedLanguage {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
}

/// Detect a tree-sitter Language for a given file extension.
pub fn detect_language(ext: &str) -> Option<DetectedLanguage> {
    for lang in SUPPORTED {
        if lang.extensions.contains(&ext) {
            return Some(lang.clone());
        }
    }
    None
}

/// Get the tree-sitter Language for a given file extension.
pub fn language_for_ext(ext: &str) -> Option<Language> {
    match ext {
        "rs" => Some(tree_sitter_rust::language()),
        "py" => Some(tree_sitter_python::language()),
        "ts" | "mts" | "cts" => Some(tree_sitter_typescript::language_typescript()),
        "tsx" => Some(tree_sitter_typescript::language_tsx()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::language()),
        "go" => Some(tree_sitter_go::language()),
        "java" => Some(tree_sitter_java::language()),
        _ => None,
    }
}

/// All supported languages with their extensions.
pub const SUPPORTED: &[DetectedLanguage] = &[
    DetectedLanguage {
        name: "Rust",
        extensions: &["rs"],
    },
    DetectedLanguage {
        name: "Python",
        extensions: &["py"],
    },
    DetectedLanguage {
        name: "TypeScript/JavaScript",
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
    },
    DetectedLanguage {
        name: "Go",
        extensions: &["go"],
    },
    DetectedLanguage {
        name: "Java",
        extensions: &["java"],
    },
];

/// Is this extension recognized by at least one extractor (tree-sitter or
/// lexical)? Used by the file walker to decide whether to process a file.
pub fn is_supported_ext(ext: &str) -> bool {
    detect_language(ext).is_some()
        || matches!(
            ext,
            "c" | "h" | "cpp" | "hpp" | "rb" | "cs" | "kt" | "swift"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust() {
        let lang = detect_language("rs");
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name, "Rust");
    }

    #[test]
    fn detects_typescript() {
        let lang = detect_language("ts");
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name, "TypeScript/JavaScript");
    }

    #[test]
    fn detects_typescript_jsx() {
        let lang = detect_language("tsx");
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name, "TypeScript/JavaScript");
    }

    #[test]
    fn detects_python() {
        let lang = detect_language("py");
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name, "Python");
    }

    #[test]
    fn unknown_extension_returns_none() {
        assert!(detect_language("xyz").is_none());
    }

    #[test]
    fn language_for_ext_rust() {
        assert!(language_for_ext("rs").is_some());
    }

    #[test]
    fn language_for_ext_unknown_returns_none() {
        assert!(language_for_ext("xyz").is_none());
    }
}
