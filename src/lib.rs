use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "graphql";
const ROOT_NODE_TYPE: &str = "graphql_document";
const DETECT_EXTENSIONS: &[&str] = &[".graphql", ".gql"];
const DEFAULT_OLD: &str = "type User {\n  id: ID\n}\n";
const DEFAULT_NEW: &str = "type User {\n  id: ID\n  name: String\n}\n";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

#[derive(Clone, Debug)]
struct ParsedLine {
    indent: usize,
    line: u32,
    raw: String,
}

#[derive(Clone, Debug)]
struct DefinitionFrame {
    id: String,
    indent: usize,
    node: SemanticNode,
}

struct GraphqlParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let lower = filename.to_lowercase();
    if DETECT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

fn parse_graphql(source: &str) -> String {
    let lines = normalized_lines(source);
    let mut root_children: Vec<SemanticNode> = Vec::new();
    let mut stack: Vec<DefinitionFrame> = Vec::new();
    let mut total_lines = 0u32;

    for parsed in lines {
        total_lines = parsed.line;
        close_completed_frames(parsed.indent, &mut stack, &mut root_children);
        let normalized = normalize_statement(&parsed.raw);
        if normalized.is_empty() || normalized == "}" {
            continue;
        }
        if let Some((node_type, label)) = definition_kind_and_label(&normalized) {
            let id = next_child_id(
                stack.last().map(|frame| frame.id.as_str()),
                &stack,
                &root_children,
            );
            let children = metadata_children(&id, &normalized, parsed.line, parsed.indent as u32);
            let node = node(
                &id,
                node_type,
                &label,
                parsed.line,
                parsed.indent as u32,
                &children,
            );
            stack.push(DefinitionFrame {
                id,
                indent: parsed.indent,
                node,
            });
            continue;
        }

        let id = next_child_id(
            stack.last().map(|frame| frame.id.as_str()),
            &stack,
            &root_children,
        );
        let field = field_node(&id, &normalized, parsed.line, parsed.indent as u32);
        if let Some(parent) = stack.last_mut() {
            parent.node.children.push(field);
        } else {
            root_children.push(field);
        }
    }
    close_all_frames(&mut stack, &mut root_children);

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        0,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &root_children),
    )
    .children(root_children)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

fn normalized_lines(source: &str) -> Vec<ParsedLine> {
    source
        .lines()
        .enumerate()
        .filter_map(|(index, raw)| {
            let without_comment = raw.split('#').next().unwrap_or("").trim_end();
            let trimmed = without_comment.trim();
            if trimmed.is_empty() {
                return None;
            }
            let indent = without_comment
                .chars()
                .take_while(|ch| ch.is_whitespace())
                .count();
            Some(ParsedLine {
                indent,
                line: index as u32,
                raw: trimmed.to_string(),
            })
        })
        .collect()
}

fn normalize_statement(raw: &str) -> String {
    raw.trim()
        .trim_end_matches('{')
        .trim_end_matches('}')
        .trim()
        .to_string()
}

fn definition_kind_and_label(statement: &str) -> Option<(&'static str, String)> {
    let mut parts = statement.split_whitespace();
    let first = parts.next()?;
    match first {
        "query" | "mutation" | "subscription" => Some((
            "operation_definition",
            clean_name(parts.next().unwrap_or(first)),
        )),
        "fragment" => Some(("fragment_definition", clean_name(parts.next()?))),
        "type" | "interface" | "input" | "enum" | "union" | "scalar" => {
            Some(("type_definition", clean_name(parts.next()?)))
        }
        "extend" => {
            let extended = parts.next()?;
            let name = parts.next().unwrap_or(extended);
            Some(("type_extension", clean_name(name)))
        }
        _ => None,
    }
}

fn clean_name(raw: &str) -> String {
    raw.split(|ch: char| ch == '(' || ch == ':' || ch == '{')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

fn field_node(id: &str, statement: &str, line: u32, col: u32) -> SemanticNode {
    let label = field_label(statement);
    let children = metadata_children(id, statement, line, col);
    node(id, "field", &label, line, col, &children)
}

fn metadata_children(id: &str, statement: &str, line: u32, col: u32) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (index, argument) in extract_arguments(statement).into_iter().enumerate() {
        children.push(node(
            &format!("{id}.{index}"),
            "argument",
            &argument,
            line,
            col,
            &[],
        ));
    }
    let directive_start = children.len();
    for (index, directive) in extract_directives(statement).into_iter().enumerate() {
        children.push(node(
            &format!("{id}.{}", directive_start + index),
            "directive",
            &directive,
            line,
            col,
            &[],
        ));
    }
    children
}

fn field_label(statement: &str) -> String {
    let before_args = statement
        .split_once('(')
        .map_or(statement, |(left, _)| left);
    let before_directive = before_args
        .split_once('@')
        .map_or(before_args, |(left, _)| left);
    let before_type = before_directive
        .split_once(':')
        .map_or(before_directive, |(left, _)| left);
    before_type
        .split_whitespace()
        .next()
        .unwrap_or(statement)
        .trim()
        .to_string()
}

fn extract_arguments(statement: &str) -> Vec<String> {
    let Some(start) = statement.find('(') else {
        return Vec::new();
    };
    let Some(end) = statement[start + 1..].find(')') else {
        return Vec::new();
    };
    statement[start + 1..start + 1 + end]
        .split(',')
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

fn extract_directives(statement: &str) -> Vec<String> {
    statement
        .split('@')
        .skip(1)
        .filter_map(|part| {
            let name = part
                .split(|ch: char| ch.is_whitespace() || ch == '(' || ch == '{' || ch == '}')
                .next()
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                None
            } else {
                Some(format!("@{name}"))
            }
        })
        .collect()
}

fn next_child_id(
    parent_id: Option<&str>,
    stack: &[DefinitionFrame],
    root_children: &[SemanticNode],
) -> String {
    match parent_id {
        Some(parent) => {
            let child_count = stack
                .last()
                .map(|frame| frame.node.children.len())
                .unwrap_or_default();
            format!("{parent}.{child_count}")
        }
        None => format!("0.{}", root_children.len()),
    }
}

fn close_completed_frames(
    indent: usize,
    stack: &mut Vec<DefinitionFrame>,
    root_children: &mut Vec<SemanticNode>,
) {
    while stack.last().is_some_and(|frame| indent <= frame.indent) {
        close_one_frame(stack, root_children);
    }
}

fn close_all_frames(stack: &mut Vec<DefinitionFrame>, root_children: &mut Vec<SemanticNode>) {
    while !stack.is_empty() {
        close_one_frame(stack, root_children);
    }
}

fn close_one_frame(stack: &mut Vec<DefinitionFrame>, root_children: &mut Vec<SemanticNode>) {
    let Some(mut frame) = stack.pop() else {
        return;
    };
    // The frame's hash was computed at push time, before fields were attached — a
    // child-blind hash makes adding/removing a field hash style-only (the host's
    // style-only shortcut compares root hashes). Recompute now that children are final.
    frame.node.structural_hash = stable_hash(
        &frame.node.node_type,
        &frame.node.label,
        &frame.node.children,
    );
    if let Some(parent) = stack.last_mut() {
        parent.node.children.push(frame.node);
    } else {
        root_children.push(frame.node);
    }
}

fn node(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    col: u32,
    children: &[SemanticNode],
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line,
        col,
        line,
        col + label.len() as u32,
        stable_hash(node_type, label, children),
    )
    .children(children.to_vec())
    .build()
}

fn stable_hash(node_type: &str, label: &str, children: &[SemanticNode]) -> String {
    let mut value = format!("{node_type}:{label}");
    for child in children {
        value.push('|');
        value.push_str(&child.structural_hash);
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

impl Guest for GraphqlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        parse_graphql(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(GraphqlParser);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_by_type(node: &SemanticNode, node_type: &str, labels: &mut Vec<String>) {
        if node.node_type == node_type {
            labels.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, labels);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(GraphqlParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn grammar_id_is_language_id() {
        assert_eq!(GraphqlParser::grammar_id(), LANGUAGE_ID);
        assert_eq!(GraphqlParser::language_ids(), vec![LANGUAGE_ID.to_string()]);
    }

    #[test]
    fn detects_graphql_extensions() {
        assert_eq!(
            detect_language_impl("schema.graphql", DEFAULT_NEW),
            LANGUAGE_ID
        );
        assert_eq!(detect_language_impl("query.gql", DEFAULT_NEW), LANGUAGE_ID);
    }

    #[test]
    fn process_returns_valid_json() {
        let parsed = parse_graphql(DEFAULT_NEW);
        intentdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
    }

    #[test]
    fn adding_a_field_changes_parent_and_root_hashes() {
        // Frame hashes were baked at push time (child-blind), so a field addition
        // hashed style-only — the playground example itself failed the supported-
        // language contract. Hashes must be recomputed when a frame closes.
        let old: SemanticNode = serde_json::from_str(&parse_graphql(DEFAULT_OLD)).unwrap();
        let new: SemanticNode = serde_json::from_str(&parse_graphql(DEFAULT_NEW)).unwrap();
        assert_ne!(
            old.structural_hash, new.structural_hash,
            "root hash must see the added field"
        );
        assert_ne!(
            old.children[0].structural_hash, new.children[0].structural_hash,
            "type_definition hash must see the added field"
        );
    }

    #[test]
    fn process_extracts_schema_operation_fragment_and_directives() {
        let parsed = parse_graphql(
            r#"
type User {
  id: ID!
  name: String @deprecated(reason: "Use displayName")
}

query UserCard($id: ID!) {
  user(id: $id) {
    ...UserFields
  }
}

fragment UserFields on User {
  displayName
}
"#,
        );
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut types = Vec::new();
        let mut operations = Vec::new();
        let mut fragments = Vec::new();
        let mut fields = Vec::new();
        let mut arguments = Vec::new();
        let mut directives = Vec::new();
        labels_by_type(&root, "type_definition", &mut types);
        labels_by_type(&root, "operation_definition", &mut operations);
        labels_by_type(&root, "fragment_definition", &mut fragments);
        labels_by_type(&root, "field", &mut fields);
        labels_by_type(&root, "argument", &mut arguments);
        labels_by_type(&root, "directive", &mut directives);

        assert!(types.contains(&"User".to_string()));
        assert!(operations.contains(&"UserCard".to_string()));
        assert!(fragments.contains(&"UserFields".to_string()));
        assert!(fields.contains(&"displayName".to_string()));
        assert!(arguments.contains(&"$id: ID!".to_string()));
        assert!(directives.contains(&"@deprecated".to_string()));
    }
}
