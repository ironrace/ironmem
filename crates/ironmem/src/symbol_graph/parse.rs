//! Language detection and heuristic source parsers for Rust and Python.
//!
//! v0 scope: Rust (.rs) and Python (.py) only. TypeScript/JavaScript are
//! explicitly unsupported. All parsers are regex/heuristic — no tree-sitter
//! dependency. Every emitted row carries a `confidence` field; parsers must
//! never panic on any input.
//!
//! MAX_SNIPPET_LEN is applied to all `signature` and `raw` fields.

use std::path::Path;

use regex::Regex;

use super::model::{ParsedFile, ParsedImport, ParsedSymbol, MAX_SNIPPET_LEN};

// ── Language detection ────────────────────────────────────────────────────────

/// Detect language from file extension. Returns "rust", "python", or "".
pub fn detect_language(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        _ => "",
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse a source file at `path` from its `content`.
///
/// - Unsupported extensions: returns `ParsedFile::unsupported`, never panics.
/// - Files larger than 1 MiB are skipped with a warning (no symbols emitted).
/// - Never panics on any input.
pub fn parse_file(path: &Path, content: &str) -> ParsedFile {
    let lang = detect_language(path);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("<none>");

    if lang.is_empty() {
        return ParsedFile::unsupported(ext);
    }

    // Guard: skip very large files to bound memory usage.
    const MAX_FILE_BYTES: usize = 1 << 20; // 1 MiB
    if content.len() > MAX_FILE_BYTES {
        return ParsedFile {
            language: lang.to_string(),
            symbols: vec![],
            imports: vec![],
            warnings: vec![format!(
                "file too large ({} bytes > {MAX_FILE_BYTES}): symbols skipped",
                content.len()
            )],
        };
    }

    match lang {
        "rust" => parse_rust(content),
        "python" => parse_python(content),
        _ => unreachable!("detect_language only returns rust/python/empty"),
    }
}

// ── Rust parser ───────────────────────────────────────────────────────────────

/// Parse Rust source with regex heuristics.
/// Extracts: use imports, mod declarations, fn/struct/enum/trait/impl/const/
///           static/type/macro_rules! declarations.
///
/// Limitations (v0, documented):
/// - Does not parse macro-generated items.
/// - Nested items (fn inside impl) are detected but parent assignment is line-based.
/// - `impl Trait for Type` is captured as one "impl" symbol.
fn parse_rust(content: &str) -> ParsedFile {
    // ── Import pattern: `use path::...;` ──────────────────────────────────
    // Matches: use X; use X as Y; use X::{A, B}; pub use X;
    let use_re = Regex::new(
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s+([\w:]+(?:::\{[^}]*\}|::[\w*]+)?)\s*(?:as\s+(\w+))?\s*;",
    )
    .unwrap();

    let mut imports: Vec<ParsedImport> = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let line_no = (line_idx + 1) as u32;
        if let Some(cap) = use_re.captures(line) {
            let full_path = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
            let alias = cap.get(2).map(|m| m.as_str().to_string());
            let raw = truncate(line.trim());

            // Parse "std::collections::{HashMap, BTreeMap}" → module + symbol(s)
            // For simplicity in v0, emit one import per `use` statement.
            // module = everything up to the last `::` segment (or the full path if no `{}`).
            let (module, symbol) = if full_path.contains("::") {
                let without_braces = full_path.trim_end_matches('}');
                if let Some(brace_pos) = without_braces.rfind('{') {
                    // use a::b::{X, Y} → module = a::b, symbol = "X, Y" (multi-import)
                    let module_part = without_braces[..brace_pos]
                        .trim_end_matches("::")
                        .to_string();
                    let symbols_part = &without_braces[brace_pos + 1..];
                    (module_part, Some(symbols_part.to_string()))
                } else if let Some(last_sep) = full_path.rfind("::") {
                    let module_part = full_path[..last_sep].to_string();
                    let sym_part = &full_path[last_sep + 2..];
                    if sym_part == "*" {
                        (module_part, None) // glob import
                    } else {
                        (module_part, Some(sym_part.to_string()))
                    }
                } else {
                    (full_path.to_string(), None)
                }
            } else {
                (full_path.to_string(), None)
            };

            if !module.is_empty() {
                imports.push(ParsedImport {
                    module,
                    symbol,
                    alias,
                    raw: Some(raw),
                    line: line_no,
                    confidence: 0.9,
                });
            }
        }
    }

    // ── Symbol pattern ─────────────────────────────────────────────────────
    // Matches top-level and nested declarations.
    let sym_re = Regex::new(
        r"(?m)^([ \t]*)(?:(pub(?:\([^)]*\))?)\s+)?(fn|struct|enum|trait|impl|mod|const|static|type|macro_rules!|macro)\s+(\w+)",
    ).unwrap();

    let mut symbols: Vec<ParsedSymbol> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for cap in sym_re.captures_iter(content) {
        let indent = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let visibility_raw = cap.get(2).map(|m| m.as_str().to_string());
        let kind_raw = cap.get(3).map(|m| m.as_str()).unwrap_or("");
        let name = cap
            .get(4)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        // Find line number by scanning for this match position.
        let match_start = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line_no = content[..match_start]
            .chars()
            .filter(|&c| c == '\n')
            .count()
            + 1;

        // Map to canonical kind.
        let kind = match kind_raw {
            "macro_rules!" | "macro" => "macro",
            other => other,
        };

        // Visibility: normalize to "pub", "pub(crate)", "pub(super)", or None (private).
        let visibility = visibility_raw.map(|v| {
            if v.starts_with("pub(") {
                v
            } else {
                "pub".to_string()
            }
        });

        // Signature: first line of the declaration (heuristic).
        let sig_line = lines.get(line_no.saturating_sub(1)).unwrap_or(&"");
        let signature = Some(rust_signature(sig_line, kind));

        // Qualified name: for nested items, prefix with the enclosing item name.
        // v0: use indent depth as a cheap nesting proxy.
        let indent_depth = indent.len();
        let parent_qualified_name = if indent_depth > 0 {
            // Find the nearest enclosing symbol with less indent.
            symbols
                .iter()
                .rev()
                .find(|s| {
                    // Crude: assume each indent level = 4 spaces.
                    let parent_indent = s.qualified_name.matches("::").count() * 4;
                    parent_indent < indent_depth
                })
                .map(|s| s.qualified_name.clone())
        } else {
            None
        };

        let qualified_name = if let Some(ref parent) = parent_qualified_name {
            format!("{parent}::{name}")
        } else {
            name.clone()
        };

        symbols.push(ParsedSymbol {
            name,
            qualified_name,
            kind: kind.to_string(),
            visibility,
            signature,
            start_line: line_no as u32,
            start_col: indent_depth as u32,
            end_line: None, // v0: end line not tracked
            parent_qualified_name,
            confidence: 0.85,
        });
    }

    ParsedFile {
        language: "rust".to_string(),
        symbols,
        imports,
        warnings: vec![],
    }
}

// ── Python parser ─────────────────────────────────────────────────────────────

/// Parse Python source with regex heuristics.
/// Extracts: import/from-import statements, def/async def, class declarations.
fn parse_python(content: &str) -> ParsedFile {
    // ── Import patterns ────────────────────────────────────────────────────
    // `import X` / `import X as Y`
    let import_re =
        Regex::new(r"(?m)^[ \t]*import\s+([\w.]+(?:\s*,\s*[\w.]+)*)\s*(?:as\s+(\w+))?").unwrap();
    // `from X import Y` / `from X import Y as Z` / `from X import *`
    let from_import_re =
        Regex::new(r"(?m)^[ \t]*from\s+([\w.]+)\s+import\s+((?:\w+(?:\s+as\s+\w+)?\s*,?\s*)+|\*)")
            .unwrap();

    let mut imports: Vec<ParsedImport> = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_no = (line_idx + 1) as u32;
        let raw = truncate(line.trim());

        // Check `from X import Y` first (more specific).
        if let Some(cap) = from_import_re.captures(line) {
            let module = cap
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let sym_part = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");
            let (symbol, alias) = if sym_part == "*" {
                (None, None)
            } else if let Some((symbol, alias)) = sym_part.split_once(" as ") {
                (
                    Some(symbol.trim().to_string()),
                    Some(alias.trim().trim_end_matches(',').to_string()),
                )
            } else {
                (Some(sym_part.to_string()), None)
            };
            if !module.is_empty() {
                imports.push(ParsedImport {
                    module,
                    symbol,
                    alias,
                    raw: Some(raw),
                    line: line_no,
                    confidence: 0.9,
                });
            }
        } else if let Some(cap) = import_re.captures(line) {
            let modules_str = cap
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let alias = cap.get(2).map(|m| m.as_str().to_string());
            // Multiple modules: `import os, sys` → emit one import per module.
            for module in modules_str.split(',') {
                let module = module.trim().to_string();
                if !module.is_empty() {
                    imports.push(ParsedImport {
                        module,
                        symbol: None,
                        alias: alias.clone(),
                        raw: Some(raw.clone()),
                        line: line_no,
                        confidence: 0.9,
                    });
                }
            }
        }
    }

    // ── Symbol patterns ────────────────────────────────────────────────────
    // `def name(` / `async def name(` / `class name(`/:
    let sym_re = Regex::new(r"(?m)^([ \t]*)(async\s+)?(def|class)\s+(\w+)").unwrap();

    let mut symbols: Vec<ParsedSymbol> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for cap in sym_re.captures_iter(content) {
        let indent = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let is_async = cap.get(2).is_some();
        let kw = cap.get(3).map(|m| m.as_str()).unwrap_or("def");
        let name = cap
            .get(4)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        let match_start = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line_no = content[..match_start]
            .chars()
            .filter(|&c| c == '\n')
            .count()
            + 1;

        let kind = match kw {
            "class" => "class",
            "def" if is_async && !indent.is_empty() => "method",
            "def" if !indent.is_empty() => "method", // indented def → method
            _ => "fn",
        };

        let sig_line = lines.get(line_no.saturating_sub(1)).unwrap_or(&"");
        let signature = Some(python_signature(sig_line));
        let indent_depth = indent.len();

        let parent_qualified_name = if indent_depth > 0 {
            symbols
                .iter()
                .rev()
                .find(|s| {
                    let parent_indent = s.qualified_name.matches('.').count() * 4;
                    parent_indent < indent_depth
                })
                .map(|s| s.qualified_name.clone())
        } else {
            None
        };

        let qualified_name = if let Some(ref parent) = parent_qualified_name {
            format!("{parent}.{name}")
        } else {
            name.clone()
        };

        // In Python, names starting with '_' (including dunders) are private by convention.
        let visibility = if name.starts_with('_') {
            None
        } else {
            Some("pub".to_string()) // Python has no explicit visibility; top-level = public
        };

        symbols.push(ParsedSymbol {
            name,
            qualified_name,
            kind: kind.to_string(),
            visibility,
            signature,
            start_line: line_no as u32,
            start_col: indent_depth as u32,
            end_line: None,
            parent_qualified_name,
            confidence: 0.85,
        });
    }

    ParsedFile {
        language: "python".to_string(),
        symbols,
        imports,
        warnings: vec![],
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate a string to at most `MAX_SNIPPET_LEN` bytes at a valid UTF-8 boundary.
fn truncate(s: &str) -> String {
    if s.len() <= MAX_SNIPPET_LEN {
        s.to_string()
    } else {
        let mut end = MAX_SNIPPET_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

fn rust_signature(line: &str, kind: &str) -> String {
    let trimmed = line.trim();
    let header = match kind {
        "const" | "static" => trimmed.split_once('=').map(|(head, _)| head.trim()),
        _ => trimmed.split_once('{').map(|(head, _)| head.trim()),
    }
    .unwrap_or(trimmed);
    truncate(header)
}

fn python_signature(line: &str) -> String {
    let trimmed = line.trim();
    let header = trimmed
        .find(':')
        .map(|idx| trimmed[..=idx].trim())
        .unwrap_or(trimmed);
    truncate(header)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── Language detection ────────────────────────────────────────────────

    #[test]
    fn detect_language_rs() {
        assert_eq!(detect_language(Path::new("src/lib.rs")), "rust");
    }

    #[test]
    fn detect_language_py() {
        assert_eq!(detect_language(Path::new("main.py")), "python");
    }

    #[test]
    fn detect_language_unknown() {
        assert_eq!(detect_language(Path::new("config.toml")), "");
        assert_eq!(detect_language(Path::new("script.ts")), "");
        assert_eq!(detect_language(Path::new("noext")), "");
    }

    // ── Unsupported extension returns ParsedFile::unsupported ─────────────

    #[test]
    fn unsupported_extension_yields_empty_with_warning_no_panic() {
        let result = parse_file(Path::new("main.ts"), "const x = 1;");
        assert_eq!(result.language, "unknown");
        assert!(result.symbols.is_empty());
        assert!(result.imports.is_empty());
        assert!(
            !result.warnings.is_empty(),
            "unsupported extension must produce a warning"
        );
        assert!(
            result.warnings[0].contains("unsupported"),
            "warning should mention 'unsupported', got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn no_extension_file_yields_empty_with_warning() {
        let result = parse_file(Path::new("Makefile"), "all:\n\techo hi");
        assert_eq!(result.language, "unknown");
        assert!(result.symbols.is_empty());
        assert!(!result.warnings.is_empty());
    }

    // ── Rust parser: imports ──────────────────────────────────────────────

    #[test]
    fn rust_parses_simple_use() {
        let src = "use std::io;\n";
        let result = parse_file(Path::new("lib.rs"), src);
        assert!(result.warnings.is_empty(), "no warnings on valid rust");
        let imp = result
            .imports
            .iter()
            .find(|i| i.module == "std" && i.symbol.as_deref() == Some("io"));
        assert!(imp.is_some(), "should parse 'use std::io;' as import");
    }

    #[test]
    fn rust_parses_qualified_use() {
        let src = "use std::collections::HashMap;\n";
        let result = parse_file(Path::new("lib.rs"), src);
        assert!(!result.imports.is_empty(), "should have at least 1 import");
        let imp = &result.imports[0];
        assert_eq!(imp.module, "std::collections");
        assert_eq!(imp.symbol.as_deref(), Some("HashMap"));
        assert!(imp.line == 1);
    }

    #[test]
    fn rust_parses_use_with_alias() {
        let src = "use std::collections::HashMap as HM;\n";
        let result = parse_file(Path::new("lib.rs"), src);
        assert!(!result.imports.is_empty());
        let imp = &result.imports[0];
        assert_eq!(imp.alias.as_deref(), Some("HM"));
    }

    #[test]
    fn rust_parses_brace_use() {
        let src = "use std::io::{Read, Write};\n";
        let result = parse_file(Path::new("lib.rs"), src);
        assert!(!result.imports.is_empty());
        let imp = &result.imports[0];
        assert_eq!(imp.module, "std::io");
        // symbol should contain both Read and Write
        let sym = imp.symbol.as_deref().unwrap_or("");
        assert!(
            sym.contains("Read"),
            "brace import should include 'Read', got: {sym}"
        );
    }

    // ── Rust parser: symbols ──────────────────────────────────────────────

    #[test]
    fn rust_parses_fn() {
        let src = "pub fn parse(input: &str) -> Result<(), ()> {\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "parse");
        assert!(sym.is_some(), "should find 'parse' fn");
        let sym = sym.unwrap();
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.visibility.as_deref(), Some("pub"));
        assert_eq!(sym.start_line, 1);
        assert!(sym.confidence > 0.0 && sym.confidence <= 1.0);
    }

    #[test]
    fn rust_parses_struct() {
        let src = "pub struct Foo {\n    x: i32,\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "Foo");
        assert!(sym.is_some(), "should find 'Foo' struct");
        assert_eq!(sym.unwrap().kind, "struct");
    }

    #[test]
    fn rust_parses_enum() {
        let src = "pub enum Color { Red, Green, Blue }\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "Color");
        assert!(sym.is_some(), "should find 'Color' enum");
        assert_eq!(sym.unwrap().kind, "enum");
    }

    #[test]
    fn rust_parses_trait() {
        let src = "pub trait Drawable {\n    fn draw(&self);\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "Drawable");
        assert!(sym.is_some(), "should find 'Drawable' trait");
        assert_eq!(sym.unwrap().kind, "trait");
    }

    #[test]
    fn rust_parses_impl() {
        let src = "impl Foo {\n    pub fn new() -> Self { Foo {} }\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "Foo");
        assert!(sym.is_some(), "should find 'Foo' impl");
        assert_eq!(sym.unwrap().kind, "impl");
    }

    #[test]
    fn rust_parses_const_static_type() {
        let src =
            "pub const MAX: usize = 100;\npub static NAME: &str = \"hi\";\npub type Alias = u32;\n";
        let result = parse_file(Path::new("lib.rs"), src);
        assert!(
            result
                .symbols
                .iter()
                .any(|s| s.name == "MAX" && s.kind == "const"),
            "should find const MAX"
        );
        assert!(
            result
                .symbols
                .iter()
                .any(|s| s.name == "NAME" && s.kind == "static"),
            "should find static NAME"
        );
        assert!(
            result
                .symbols
                .iter()
                .any(|s| s.name == "Alias" && s.kind == "type"),
            "should find type Alias"
        );
    }

    #[test]
    fn rust_parses_macro_rules() {
        let src = "macro_rules! my_macro {\n    () => {}\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "my_macro");
        assert!(sym.is_some(), "should find 'my_macro'");
        assert_eq!(sym.unwrap().kind, "macro");
    }

    #[test]
    fn rust_parses_mod() {
        let src = "pub mod my_module {\n    pub fn foo() {}\n}\n";
        let result = parse_file(Path::new("lib.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "my_module");
        assert!(sym.is_some(), "should find 'my_module' mod");
        assert_eq!(sym.unwrap().kind, "mod");
    }

    #[test]
    fn rust_spans_have_line_numbers() {
        let src = "// comment\n// another\npub fn bar() {}\n";
        let result = parse_file(Path::new("x.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "bar");
        assert!(sym.is_some(), "should find 'bar'");
        assert_eq!(sym.unwrap().start_line, 3, "bar is on line 3");
    }

    #[test]
    fn rust_parser_does_not_panic_on_empty() {
        let result = parse_file(Path::new("x.rs"), "");
        assert!(result.symbols.is_empty());
        assert!(result.imports.is_empty());
    }

    #[test]
    fn rust_parser_does_not_panic_on_binary_content() {
        let content = "\x00\x01\x02fn hello() {}";
        // Should not panic; may produce 0 or more results.
        let _result = parse_file(Path::new("x.rs"), content);
    }

    // ── Python parser: imports ────────────────────────────────────────────

    #[test]
    fn python_parses_simple_import() {
        let src = "import os\n";
        let result = parse_file(Path::new("main.py"), src);
        let imp = result.imports.iter().find(|i| i.module == "os");
        assert!(imp.is_some(), "should find 'os' import");
        assert_eq!(imp.unwrap().line, 1);
    }

    #[test]
    fn python_parses_from_import() {
        let src = "from collections import OrderedDict\n";
        let result = parse_file(Path::new("main.py"), src);
        let imp = result.imports.iter().find(|i| i.module == "collections");
        assert!(imp.is_some(), "should find 'collections' from-import");
        let imp = imp.unwrap();
        assert_eq!(imp.symbol.as_deref(), Some("OrderedDict"));
    }

    #[test]
    fn python_parses_from_import_alias() {
        let src = "from collections import OrderedDict as OD\n";
        let result = parse_file(Path::new("main.py"), src);
        let imp = result.imports.iter().find(|i| i.module == "collections");
        assert!(imp.is_some(), "should find aliased from-import");
        let imp = imp.unwrap();
        assert_eq!(imp.symbol.as_deref(), Some("OrderedDict"));
        assert_eq!(imp.alias.as_deref(), Some("OD"));
    }

    #[test]
    fn python_parses_from_import_wildcard() {
        let src = "from os.path import *\n";
        let result = parse_file(Path::new("main.py"), src);
        let imp = result.imports.iter().find(|i| i.module == "os.path");
        assert!(imp.is_some(), "should find 'os.path' wildcard import");
        assert!(imp.unwrap().symbol.is_none(), "wildcard → symbol=None");
    }

    #[test]
    fn python_parses_multi_module_import() {
        let src = "import os, sys\n";
        let result = parse_file(Path::new("main.py"), src);
        assert!(
            result.imports.iter().any(|i| i.module == "os"),
            "should have 'os'"
        );
        assert!(
            result.imports.iter().any(|i| i.module == "sys"),
            "should have 'sys'"
        );
    }

    // ── Python parser: symbols ────────────────────────────────────────────

    #[test]
    fn python_parses_def() {
        let src = "def parse(text):\n    pass\n";
        let result = parse_file(Path::new("main.py"), src);
        let sym = result.symbols.iter().find(|s| s.name == "parse");
        assert!(sym.is_some(), "should find 'parse' def");
        let sym = sym.unwrap();
        assert_eq!(sym.kind, "fn");
        assert_eq!(sym.start_line, 1);
    }

    #[test]
    fn python_parses_async_def() {
        let src = "async def fetch(url):\n    pass\n";
        let result = parse_file(Path::new("main.py"), src);
        let sym = result.symbols.iter().find(|s| s.name == "fetch");
        assert!(sym.is_some(), "should find 'fetch' async def");
        assert_eq!(sym.unwrap().kind, "fn");
    }

    #[test]
    fn python_parses_class() {
        let src = "class MyModel:\n    pass\n";
        let result = parse_file(Path::new("main.py"), src);
        let sym = result.symbols.iter().find(|s| s.name == "MyModel");
        assert!(sym.is_some(), "should find 'MyModel' class");
        assert_eq!(sym.unwrap().kind, "class");
    }

    #[test]
    fn python_parses_nested_method() {
        let src = "class Foo:\n    def bar(self):\n        pass\n";
        let result = parse_file(Path::new("main.py"), src);
        let method = result.symbols.iter().find(|s| s.name == "bar");
        assert!(method.is_some(), "should find 'bar' method");
        assert_eq!(method.unwrap().kind, "method");
        // Parent should be Foo
        assert_eq!(
            method.unwrap().parent_qualified_name.as_deref(),
            Some("Foo"),
            "bar's parent should be Foo"
        );
        assert_eq!(method.unwrap().qualified_name, "Foo.bar");
    }

    #[test]
    fn python_spans_have_line_numbers() {
        let src = "# comment\nclass A:\n    pass\n";
        let result = parse_file(Path::new("x.py"), src);
        let sym = result.symbols.iter().find(|s| s.name == "A");
        assert!(sym.is_some());
        assert_eq!(sym.unwrap().start_line, 2, "class A is on line 2");
    }

    #[test]
    fn python_parser_does_not_panic_on_empty() {
        let result = parse_file(Path::new("x.py"), "");
        assert!(result.symbols.is_empty());
        assert!(result.imports.is_empty());
    }

    #[test]
    fn python_confidence_in_range() {
        let src = "def foo():\n    pass\nimport os\n";
        let result = parse_file(Path::new("x.py"), src);
        for sym in &result.symbols {
            assert!(
                sym.confidence > 0.0 && sym.confidence <= 1.0,
                "symbol confidence out of range: {}",
                sym.confidence
            );
        }
        for imp in &result.imports {
            assert!(
                imp.confidence > 0.0 && imp.confidence <= 1.0,
                "import confidence out of range: {}",
                imp.confidence
            );
        }
    }

    // ── Snippet truncation ────────────────────────────────────────────────

    #[test]
    fn signature_truncated_to_max_snippet_len() {
        // Generate a very long function signature line.
        let long_params = "x: i32, ".repeat(200);
        let src = format!("pub fn big({long_params}) {{}}\n");
        let result = parse_file(Path::new("x.rs"), &src);
        for sym in &result.symbols {
            if let Some(sig) = &sym.signature {
                assert!(
                    sig.len() <= MAX_SNIPPET_LEN,
                    "signature must be truncated, got len {}",
                    sig.len()
                );
            }
        }
    }

    #[test]
    fn rust_signature_excludes_inline_body() {
        let src = "pub fn greet(name: &str) -> String { name.to_string() }\n";
        let result = parse_file(Path::new("x.rs"), src);
        let sym = result.symbols.iter().find(|s| s.name == "greet").unwrap();
        let signature = sym.signature.as_deref().unwrap_or_default();
        assert_eq!(signature, "pub fn greet(name: &str) -> String");
        assert!(
            !signature.contains("name.to_string"),
            "signature must not persist inline body: {signature}"
        );
    }

    #[test]
    fn python_signature_excludes_inline_body() {
        let src = "def greet(name): return name\nclass Model: pass\n";
        let result = parse_file(Path::new("x.py"), src);
        let fn_sym = result.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(fn_sym.signature.as_deref(), Some("def greet(name):"));
        let class_sym = result.symbols.iter().find(|s| s.name == "Model").unwrap();
        assert_eq!(class_sym.signature.as_deref(), Some("class Model:"));
    }

    #[test]
    fn import_raw_truncated_to_max_snippet_len() {
        // A very long use statement.
        let long = "a::".repeat(200);
        let src = format!("use {long}b;\n");
        let result = parse_file(Path::new("x.rs"), &src);
        for imp in &result.imports {
            if let Some(raw) = &imp.raw {
                assert!(
                    raw.len() <= MAX_SNIPPET_LEN,
                    "import raw must be truncated, got len {}",
                    raw.len()
                );
            }
        }
    }
}
