use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

fn language_for(path: &Path) -> Option<Language> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "rs" => tree_sitter_rust::LANGUAGE.into(),
            "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
            "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
            "lua" => tree_sitter_lua::LANGUAGE.into(),
            _ => return None,
        },
    )
}

pub fn parse(path: &Path, source: &str) -> Vec<Symbol> {
    let Some(language) = language_for(path) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    visit(tree.root_node(), source.as_bytes(), &mut out, false);
    out.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    out.dedup_by(|a, b| a.line == b.line && a.name == b.name && a.kind == b.kind);
    out
}

fn visit(node: Node<'_>, source: &[u8], out: &mut Vec<Symbol>, in_impl: bool) {
    let kind = node.kind();
    let symbol_kind = match kind {
        "function_item" if in_impl => Some("method"),
        "function_item" | "function_definition" | "function_declaration" => Some("function"),
        "method_definition" | "method_declaration" => Some("method"),
        "struct_item" | "struct_declaration" => Some("struct"),
        "enum_item" | "enum_declaration" => Some("enum"),
        "trait_item" | "interface_declaration" => Some("interface"),
        "class_item" | "class_declaration" => Some("class"),
        "type_item" | "type_alias_declaration" => Some("type"),
        "const_item"
        | "const_declaration"
        | "lexical_declaration"
        | "variable_declaration"
        | "let_declaration" => Some("variable"),
        "static_item" => Some("static"),
        "mod_item" | "internal_module" => Some("module"),
        _ => None,
    };
    if kind == "variable_list" {
        let mut cursor = node.walk();
        for child in node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable" || child.kind() == "identifier")
        {
            if let Ok(name) = child.utf8_text(source) {
                out.push(Symbol {
                    name: name.to_string(),
                    kind: "variable".into(),
                    line: node.start_position().row + 1,
                });
            }
        }
    }
    if let Some(symbol_kind) = symbol_kind {
        let named = node.child_by_field_name("name").or_else(|| {
            if kind == "let_declaration" {
                node.child_by_field_name("pattern")
            } else if kind == "lexical_declaration" || kind == "variable_declaration" {
                let mut cursor = node.walk();
                let found = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "variable_declarator")
                    .and_then(|declarator| declarator.child_by_field_name("name"));
                found
            } else {
                None
            }
        });
        if let Some(name_node) = named {
            let is_method = name_node.kind() == "method_index_expression";
            let resolved_name_node = if is_method {
                name_node.child_by_field_name("method")
            } else {
                Some(name_node)
            };
            if let Some(name_node) = resolved_name_node {
                let name = name_node.utf8_text(source).unwrap_or("").trim();
                if !name.is_empty() {
                    out.push(Symbol {
                        name: name.to_string(),
                        kind: if is_method { "method" } else { symbol_kind }.to_string(),
                        line: node.start_position().row + 1,
                    });
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(
            child,
            source,
            out,
            in_impl || matches!(kind, "impl_item" | "trait_item"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_only_supported_language_symbols() {
        let rust = parse(
            Path::new("lib.rs"),
            "pub struct HookController;\nimpl HookController { fn run(&self) {} }\n",
        );
        assert!(rust
            .iter()
            .any(|s| s.name == "HookController" && s.kind == "struct" && s.line == 1));
        assert!(rust
            .iter()
            .any(|s| s.name == "run" && s.kind == "method" && s.line == 2));
        assert!(parse(Path::new("file.py"), "def visible(): pass").is_empty());
    }
    #[test]
    fn extracts_javascript_declarations() {
        let result = parse(
            Path::new("file.ts"),
            "export class HookController { run() {} }\nconst handle = () => 1;",
        );
        assert!(result
            .iter()
            .any(|s| s.name == "HookController" && s.kind == "class"));
        assert!(result
            .iter()
            .any(|s| s.name == "handle" && s.kind == "variable"));
    }
}
