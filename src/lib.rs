use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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

struct GraphqlParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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

/// Maximum nesting of selection sets, list/object values and list types the
/// recursive-descent projection follows. Deeper input is still preserved as
/// flat `unparsed` tokens, so it is never certified as style-only and the
/// Wasm stack cannot be exhausted by adversarial nesting. It also keeps the
/// emitted JSON (two levels per node) under the 128-level serde_json limit.
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Punct,
    Name,
    Number,
    Str,
    Unknown,
}

/// A lexical GraphQL token. Positions are 0-based lines (split on `\n`) and
/// UTF-8 byte columns within the line.
#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
    start: (u32, u32),
    end: (u32, u32),
}

const BOM: &[u8] = "\u{feff}".as_bytes();

/// Byte width of the UTF-8 character introduced by `lead` (1 for a stray
/// continuation byte, so the scanner always makes progress).
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// GraphQL lexer (October 2021 spec §2.1). Whitespace, line terminators,
/// commas, the BOM and `#` comments are insignificant and dropped; a `#`
/// inside a string or block string is part of the string value.
fn tokenize(source: &str) -> Vec<Token<'_>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 0u32;
    let mut line_start = 0usize;

    // Advance over `source[from..to]`, updating the line counters.
    fn advance(bytes: &[u8], from: usize, to: usize, line: &mut u32, line_start: &mut usize) {
        for (offset, byte) in bytes[from..to].iter().enumerate() {
            if *byte == b'\n' {
                *line += 1;
                *line_start = from + offset + 1;
            }
        }
    }

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b' ' | b'\t' | b'\r' | b',' => {
                index += 1;
                continue;
            }
            b'\n' => {
                index += 1;
                line += 1;
                line_start = index;
                continue;
            }
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            _ => {}
        }
        // All delimiter checks are byte-wise: `index`/`cursor` may sit on any
        // byte of a multibyte character inside a string, so `&source[i..]`
        // would panic there. Every token still ends on a char boundary.
        if bytes[index..].starts_with(BOM) {
            index += BOM.len();
            continue;
        }

        let start_index = index;
        let start = (line, (index - line_start) as u32);
        let kind;
        if bytes[index..].starts_with(b"\"\"\"") {
            // Block string: runs to the next unescaped `"""` (or EOF).
            kind = TokenKind::Str;
            let mut cursor = index + 3;
            loop {
                if cursor >= bytes.len() {
                    break;
                }
                if bytes[cursor..].starts_with(b"\\\"\"\"") {
                    cursor += 4;
                } else if bytes[cursor..].starts_with(b"\"\"\"") {
                    cursor += 3;
                    break;
                } else {
                    cursor += 1;
                }
            }
            index = cursor;
        } else if byte == b'"' {
            // String value: runs to the closing quote; an unterminated string
            // stops at the line end so later lines still tokenize.
            kind = TokenKind::Str;
            let mut cursor = index + 1;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() && bytes[cursor + 1] != b'\n' {
                    cursor += 2;
                } else if bytes[cursor] == b'"' {
                    cursor += 1;
                    break;
                } else {
                    cursor += 1;
                }
            }
            index = cursor;
        } else if bytes[index..].starts_with(b"...") {
            kind = TokenKind::Punct;
            index += 3;
        } else if b"!$&()=:@[]{}|".contains(&byte) {
            kind = TokenKind::Punct;
            index += 1;
        } else if byte == b'_' || byte.is_ascii_alphabetic() {
            kind = TokenKind::Name;
            while index < bytes.len() && (bytes[index] == b'_' || bytes[index].is_ascii_alphanumeric()) {
                index += 1;
            }
        } else if byte == b'-' || byte.is_ascii_digit() {
            kind = TokenKind::Number;
            index += 1;
            while index < bytes.len() {
                let current = bytes[index];
                let previous = bytes[index - 1];
                if current.is_ascii_alphanumeric()
                    || current == b'.'
                    || ((current == b'+' || current == b'-') && (previous == b'e' || previous == b'E'))
                {
                    index += 1;
                } else {
                    break;
                }
            }
        } else {
            kind = TokenKind::Unknown;
            // `index` is on a char boundary here (every branch above stops
            // on an ASCII byte or EOF); step over one whole character.
            index = (index + utf8_width(byte)).min(bytes.len());
        }
        // Block strings may span lines; keep the counters exact.
        advance(bytes, start_index, index, &mut line, &mut line_start);
        let end = (line, (index - line_start) as u32);
        tokens.push(Token {
            kind,
            text: source.get(start_index..index).unwrap_or_default(),
            start,
            end,
        });
    }
    tokens
}

/// An id-less projection node; ids and hashes are assigned once the tree is final.
#[derive(Clone, Debug)]
struct Node {
    node_type: &'static str,
    label: String,
    start: (u32, u32),
    end: (u32, u32),
    children: Vec<Node>,
}

impl Node {
    fn new(node_type: &'static str, label: impl Into<String>, start: (u32, u32), end: (u32, u32)) -> Self {
        Node {
            node_type,
            label: label.into(),
            start,
            end,
            children: Vec::new(),
        }
    }

    fn from_token(node_type: &'static str, token: &Token<'_>) -> Self {
        Node::new(node_type, token.text, token.start, token.end)
    }
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    pos: usize,
    depth: usize,
    last_end: (u32, u32),
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Parser {
            tokens: tokenize(source),
            pos: 0,
            depth: 0,
            last_end: (0, 0),
        }
    }

    fn peek(&self) -> Option<Token<'a>> {
        self.tokens.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<Token<'a>> {
        self.tokens.get(self.pos + offset).copied()
    }

    fn at_punct(&self, punct: &str) -> bool {
        self.peek().is_some_and(|t| t.kind == TokenKind::Punct && t.text == punct)
    }

    fn at_name(&self) -> bool {
        self.peek().is_some_and(|t| t.kind == TokenKind::Name)
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        self.peek().is_some_and(|t| t.kind == TokenKind::Name && t.text == keyword)
    }

    fn bump(&mut self) -> Option<Token<'a>> {
        let token = self.peek()?;
        self.pos += 1;
        self.last_end = token.end;
        Some(token)
    }

    fn eat_punct(&mut self, punct: &str) -> bool {
        if self.at_punct(punct) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_name(&mut self) -> Option<Token<'a>> {
        if self.at_name() {
            self.bump()
        } else {
            None
        }
    }

    /// Run `parse` and return its rendering with the source span of the
    /// tokens it consumed. When it consumed nothing (a missing type or
    /// default in partially edited input) the span is zero-width at
    /// `anchor`, the end of the preceding `:` / `=`, so it is never reversed.
    fn spanned(&mut self, anchor: (u32, u32), parse: fn(&mut Self) -> String) -> (String, (u32, u32), (u32, u32)) {
        let before = self.pos;
        let start = self.peek().map_or(anchor, |t| t.start);
        let rendered = parse(self);
        if self.pos == before {
            (rendered, anchor, anchor)
        } else {
            (rendered, start, self.last_end)
        }
    }

    /// Preserve one token that does not fit the grammar so it can never be
    /// silently dropped (and always makes progress).
    fn unparsed(&mut self) -> Option<Node> {
        self.bump().map(|token| Node::from_token("unparsed", &token))
    }

    fn document(&mut self) -> Vec<Node> {
        let mut definitions = Vec::new();
        while self.peek().is_some() {
            if let Some(definition) = self.definition() {
                definitions.push(definition);
            }
        }
        definitions
    }

    fn description(&mut self) -> Option<Node> {
        match self.peek() {
            Some(token) if token.kind == TokenKind::Str => {
                self.bump();
                Some(Node::from_token("description", &token))
            }
            _ => None,
        }
    }

    fn definition(&mut self) -> Option<Node> {
        let description = self.description();
        let mut definition = match self.peek() {
            None => return description,
            Some(token) if token.kind == TokenKind::Punct && token.text == "{" => self.operation(None),
            Some(token) if token.kind == TokenKind::Name => match token.text {
                "query" | "mutation" | "subscription" => self.operation(Some(token)),
                "fragment" => self.fragment(),
                "schema" => self.schema("schema_definition"),
                "scalar" | "type" | "interface" | "union" | "enum" | "input" => {
                    self.type_definition("type_definition")
                }
                "directive" => self.directive_definition(),
                "extend" => {
                    self.bump();
                    if self.at_keyword("schema") {
                        self.schema("schema_extension")
                    } else if self.peek().is_some_and(|t| {
                        t.kind == TokenKind::Name
                            && matches!(t.text, "scalar" | "type" | "interface" | "union" | "enum" | "input")
                    }) {
                        self.type_definition("type_extension")
                    } else {
                        Node::from_token("unparsed", &token)
                    }
                }
                _ => self.unparsed()?,
            },
            Some(_) => self.unparsed()?,
        };
        if let Some(description) = description {
            definition.children.insert(0, description);
        }
        Some(definition)
    }

    /// `query|mutation|subscription [Name] [VariableDefinitions] Directives SelectionSet`,
    /// or the `{ ... }` query shorthand. The operation kind is kept as an
    /// `operation_type` child so `query X` and `mutation X` never hash alike.
    fn operation(&mut self, kind: Option<Token<'a>>) -> Node {
        let (kind_label, kind_node) = match kind {
            Some(token) => {
                self.bump();
                (token.text, Node::from_token("operation_type", &token))
            }
            None => {
                let brace = self.peek().map_or((0, 0), |t| t.start);
                ("query", Node::new("operation_type", "query", brace, brace))
            }
        };
        let mut node = match self.eat_name() {
            Some(name) => Node::from_token("operation_definition", &name),
            None => Node::new("operation_definition", kind_label, kind_node.start, kind_node.end),
        };
        node.children.push(kind_node);
        if self.at_punct("(") {
            self.variable_definitions(&mut node.children);
        }
        self.directives(&mut node.children);
        if self.at_punct("{") {
            self.selection_set(&mut node.children);
        }
        node
    }

    fn variable_definitions(&mut self, out: &mut Vec<Node>) {
        self.bump();
        while self.peek().is_some() && !self.eat_punct(")") {
            if self.at_punct("$") {
                let dollar = self.bump().expect("peeked");
                let name = self.eat_name().map_or("", |t| t.text);
                let mut label = format!("${name}");
                if self.eat_punct(":") {
                    label.push_str(": ");
                    label.push_str(&self.type_ref());
                }
                if self.eat_punct("=") {
                    label.push_str(" = ");
                    label.push_str(&self.value());
                }
                let mut node = Node::new("argument", label, dollar.start, self.last_end);
                self.directives(&mut node.children);
                out.push(node);
            } else if let Some(node) = self.unparsed() {
                out.push(node);
            }
        }
    }

    fn selection_set(&mut self, out: &mut Vec<Node>) {
        if self.depth >= MAX_DEPTH {
            if let Some(node) = self.unparsed() {
                out.push(node);
            }
            return;
        }
        self.depth += 1;
        self.bump();
        while self.peek().is_some() && !self.eat_punct("}") {
            if let Some(selection) = self.selection() {
                out.push(selection);
            }
        }
        self.depth -= 1;
    }

    fn selection(&mut self) -> Option<Node> {
        let token = self.peek()?;
        if token.kind == TokenKind::Punct && token.text == "..." {
            self.bump();
            let next = self.peek();
            let mut node = match next {
                Some(on) if on.kind == TokenKind::Name && on.text == "on" => {
                    self.bump();
                    let condition = self.eat_name().map_or("", |t| t.text);
                    Node::new("inline_fragment", format!("... on {condition}"), token.start, self.last_end)
                }
                Some(name) if name.kind == TokenKind::Name => {
                    self.bump();
                    Node::new("fragment_spread", format!("...{}", name.text), token.start, name.end)
                }
                _ => Node::new("inline_fragment", "...", token.start, token.end),
            };
            self.directives(&mut node.children);
            if node.node_type == "inline_fragment" && self.at_punct("{") {
                self.selection_set(&mut node.children);
            }
            return Some(node);
        }
        if token.kind != TokenKind::Name {
            return self.unparsed();
        }
        self.bump();
        // `alias: name` — the field name is the label; the alias is kept as
        // a child so renaming the response key is still a change.
        let mut alias = None;
        let mut name = token;
        if self.at_punct(":") && self.peek_at(1).is_some_and(|t| t.kind == TokenKind::Name) {
            self.bump();
            alias = Some(token);
            name = self.bump().expect("peeked");
        }
        let mut node = Node::from_token("field", &name);
        if let Some(alias) = alias {
            node.children.push(Node::from_token("alias", &alias));
        }
        if self.at_punct("(") {
            self.arguments(&mut node.children);
        }
        self.directives(&mut node.children);
        if self.at_punct("{") {
            self.selection_set(&mut node.children);
        }
        Some(node)
    }

    /// `(name: Value, ...)` — each argument label is `name: <canonical value>`.
    fn arguments(&mut self, out: &mut Vec<Node>) {
        self.bump();
        while self.peek().is_some() && !self.eat_punct(")") {
            match self.peek() {
                Some(name) if name.kind == TokenKind::Name => {
                    self.bump();
                    let mut label = name.text.to_string();
                    if self.eat_punct(":") {
                        label.push_str(": ");
                        label.push_str(&self.value());
                    }
                    out.push(Node::new("argument", label, name.start, self.last_end));
                }
                _ => {
                    if let Some(node) = self.unparsed() {
                        out.push(node);
                    }
                }
            }
        }
    }

    /// Render a (const or variable) value canonically: insignificant layout
    /// and commas vanish, every literal (including quoted `#`) is kept.
    fn value(&mut self) -> String {
        let Some(token) = self.peek() else {
            return String::new();
        };
        if token.kind == TokenKind::Punct && (token.text == "[" || token.text == "{") && self.depth >= MAX_DEPTH {
            self.bump();
            return token.text.to_string();
        }
        match (token.kind, token.text) {
            (TokenKind::Punct, "$") => {
                self.bump();
                format!("${}", self.eat_name().map_or("", |t| t.text))
            }
            (TokenKind::Punct, "[") => {
                self.depth += 1;
                self.bump();
                let mut items = Vec::new();
                while self.peek().is_some() && !self.eat_punct("]") {
                    if self.at_punct(")") || self.at_punct("}") {
                        break;
                    }
                    items.push(self.value());
                }
                self.depth -= 1;
                format!("[{}]", items.join(", "))
            }
            (TokenKind::Punct, "{") => {
                self.depth += 1;
                self.bump();
                let mut fields = Vec::new();
                while self.peek().is_some() && !self.eat_punct("}") {
                    if self.at_punct(")") || self.at_punct("]") {
                        break;
                    }
                    match self.eat_name() {
                        Some(name) => {
                            let mut field = name.text.to_string();
                            if self.eat_punct(":") {
                                field.push_str(": ");
                                field.push_str(&self.value());
                            }
                            fields.push(field);
                        }
                        None => fields.push(self.bump().map_or(String::new(), |t| t.text.to_string())),
                    }
                }
                self.depth -= 1;
                format!("{{{}}}", fields.join(", "))
            }
            (TokenKind::Punct, ")" | "}" | "]") => String::new(),
            _ => {
                self.bump();
                token.text.to_string()
            }
        }
    }

    /// `Name`, `[Type]`, each optionally followed by `!`.
    fn type_ref(&mut self) -> String {
        let mut rendered = if self.at_punct("[") && self.depth < MAX_DEPTH {
            self.depth += 1;
            self.bump();
            let inner = self.type_ref();
            self.eat_punct("]");
            self.depth -= 1;
            format!("[{inner}]")
        } else {
            match self.peek() {
                Some(token) if token.kind == TokenKind::Name || token.text == "[" => {
                    self.bump();
                    token.text.to_string()
                }
                _ => String::new(),
            }
        };
        if self.eat_punct("!") {
            rendered.push('!');
        }
        rendered
    }

    /// `@name(args)` repeated; arguments are children of the directive.
    fn directives(&mut self, out: &mut Vec<Node>) {
        while self.at_punct("@") {
            let at = self.bump().expect("peeked");
            let name = self.eat_name().map_or("", |t| t.text);
            let mut node = Node::new("directive", format!("@{name}"), at.start, self.last_end);
            if self.at_punct("(") {
                self.arguments(&mut node.children);
            }
            out.push(node);
        }
    }

    fn fragment(&mut self) -> Node {
        let keyword = self.bump().expect("peeked");
        let mut node = match self.eat_name() {
            Some(name) => Node::from_token("fragment_definition", &name),
            None => Node::from_token("fragment_definition", &keyword),
        };
        if self.at_keyword("on") {
            let on = self.bump().expect("peeked");
            let condition = self.eat_name().map_or("", |t| t.text);
            node.children
                .push(Node::new("type_condition", format!("on {condition}"), on.start, self.last_end));
        }
        self.directives(&mut node.children);
        if self.at_punct("{") {
            self.selection_set(&mut node.children);
        }
        node
    }

    fn schema(&mut self, node_type: &'static str) -> Node {
        let keyword = self.bump().expect("peeked");
        let mut node = Node::from_token(node_type, &keyword);
        self.directives(&mut node.children);
        if self.eat_punct("{") {
            while self.peek().is_some() && !self.eat_punct("}") {
                match self.peek() {
                    Some(operation) if operation.kind == TokenKind::Name => {
                        self.bump();
                        let mut label = operation.text.to_string();
                        if self.eat_punct(":") {
                            label.push_str(": ");
                            label.push_str(self.eat_name().map_or("", |t| t.text));
                        }
                        node.children
                            .push(Node::new("root_operation_type", label, operation.start, self.last_end));
                    }
                    _ => {
                        if let Some(unparsed) = self.unparsed() {
                            node.children.push(unparsed);
                        }
                    }
                }
            }
        }
        node
    }

    /// Object, interface, input, enum, union and scalar definitions (or their
    /// `extend` forms). The keyword is kept as a `definition_kind` child.
    fn type_definition(&mut self, node_type: &'static str) -> Node {
        let keyword = self.bump().expect("peeked");
        let mut node = match self.eat_name() {
            Some(name) => Node::from_token(node_type, &name),
            None => Node::from_token(node_type, &keyword),
        };
        node.children.push(Node::from_token("definition_kind", &keyword));
        if self.at_keyword("implements") {
            self.bump();
            self.eat_punct("&");
            while let Some(interface) = self.eat_name() {
                node.children.push(Node::from_token("implements", &interface));
                if !self.eat_punct("&") && !self.at_name() {
                    break;
                }
            }
        }
        self.directives(&mut node.children);
        if keyword.text == "union" {
            if self.eat_punct("=") {
                self.eat_punct("|");
                while let Some(member) = self.eat_name() {
                    node.children.push(Node::from_token("union_member", &member));
                    if !self.eat_punct("|") {
                        break;
                    }
                }
            }
        } else if keyword.text != "scalar" && self.eat_punct("{") {
            while self.peek().is_some() && !self.eat_punct("}") {
                match keyword.text {
                    "enum" => self.enum_value(&mut node.children),
                    "input" => node.children.extend(self.input_value("field")),
                    _ => node.children.extend(self.field_definition()),
                }
            }
        }
        node
    }

    /// `[Description] VALUE Directives` inside an enum body.
    fn enum_value(&mut self, out: &mut Vec<Node>) {
        let description = self.description();
        match self.eat_name() {
            Some(value) => {
                let mut value = Node::from_token("enum_value", &value);
                value.children.extend(description);
                self.directives(&mut value.children);
                out.push(value);
            }
            None => {
                out.extend(description);
                out.extend(self.unparsed());
            }
        }
    }

    /// `[Description] name[(args)]: Type Directives` inside a type/interface.
    /// The declared type is kept as a `field_type` child (`String` vs `String!`
    /// vs `Int` all hash differently).
    fn field_definition(&mut self) -> Option<Node> {
        let description = self.description();
        let Some(name) = self.eat_name() else {
            // Keep a dangling description and the unexpected token.
            return description.or_else(|| self.unparsed());
        };
        let mut node = Node::from_token("field", &name);
        node.children.extend(description);
        if self.eat_punct("(") {
            while self.peek().is_some() && !self.eat_punct(")") {
                node.children.extend(self.input_value("argument"));
            }
        }
        if self.at_punct(":") {
            let colon = self.bump().expect("peeked");
            let (rendered, start, end) = self.spanned(colon.end, Self::type_ref);
            node.children.push(Node::new("field_type", rendered, start, end));
        }
        self.directives(&mut node.children);
        Some(node)
    }

    /// `[Description] name: Type [= Default] Directives` — an argument
    /// definition (label `name: Type[ = Default]`) or an input object field
    /// (label `name`, with `field_type` / `default_value` children).
    fn input_value(&mut self, node_type: &'static str) -> Option<Node> {
        let description = self.description();
        let Some(name) = self.eat_name() else {
            return description.or_else(|| self.unparsed());
        };
        let mut type_node = None;
        let mut default_node = None;
        if self.at_punct(":") {
            let colon = self.bump().expect("peeked");
            let (rendered, start, end) = self.spanned(colon.end, Self::type_ref);
            type_node = Some(Node::new("field_type", rendered, start, end));
        }
        if self.at_punct("=") {
            let equals = self.bump().expect("peeked");
            let (rendered, start, end) = self.spanned(equals.end, Self::value);
            default_node = Some(Node::new("default_value", rendered, start, end));
        }
        let mut node = if node_type == "argument" {
            let mut label = name.text.to_string();
            if let Some(type_node) = &type_node {
                label.push_str(": ");
                label.push_str(&type_node.label);
            }
            if let Some(default_node) = &default_node {
                label.push_str(" = ");
                label.push_str(&default_node.label);
            }
            Node::new("argument", label, name.start, self.last_end)
        } else {
            let mut field = Node::from_token(node_type, &name);
            field.children.extend(type_node);
            field.children.extend(default_node);
            field
        };
        if let Some(description) = description {
            node.children.insert(0, description);
        }
        self.directives(&mut node.children);
        Some(node)
    }

    /// `directive @name[(args)] [repeatable] on Location | ...`
    fn directive_definition(&mut self) -> Node {
        let keyword = self.bump().expect("peeked");
        let mut node = if self.at_punct("@") {
            let at = self.bump().expect("peeked");
            let name = self.eat_name().map_or("", |t| t.text);
            Node::new("directive_definition", format!("@{name}"), at.start, self.last_end)
        } else {
            Node::from_token("directive_definition", &keyword)
        };
        if self.eat_punct("(") {
            while self.peek().is_some() && !self.eat_punct(")") {
                node.children.extend(self.input_value("argument"));
            }
        }
        if self.at_keyword("repeatable") {
            let repeatable = self.bump().expect("peeked");
            node.children.push(Node::from_token("modifier", &repeatable));
        }
        if self.at_keyword("on") {
            self.bump();
            self.eat_punct("|");
            while let Some(location) = self.eat_name() {
                node.children.push(Node::from_token("directive_location", &location));
                if !self.eat_punct("|") {
                    break;
                }
            }
        }
        node
    }
}

fn into_semantic(node: Node, id: String) -> SemanticNode {
    let children: Vec<SemanticNode> = node
        .children
        .into_iter()
        .enumerate()
        .map(|(index, child)| into_semantic(child, format!("{id}.{index}")))
        .collect();
    let hash = stable_hash(node.node_type, &node.label, &children);
    SemanticNodeBuilder::new(
        id,
        node.node_type,
        node.label,
        node.start.0,
        node.start.1,
        node.end.0,
        node.end.1,
        hash,
    )
    .children(children)
    .build()
}

fn parse_graphql(source: &str) -> String {
    let definitions = Parser::new(source).document();
    let root_children: Vec<SemanticNode> = definitions
        .into_iter()
        .enumerate()
        .map(|(index, definition)| into_semantic(definition, format!("0.{index}")))
        .collect();
    let end_line = source.matches('\n').count() as u32;
    let end_col = source.rsplit('\n').next().map_or(0, str::len) as u32;

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        end_line,
        end_col,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &root_children),
    )
    .children(root_children)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
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

    fn first_of_type<'n>(node: &'n SemanticNode, node_type: &str) -> Option<&'n SemanticNode> {
        if node.node_type == node_type {
            return Some(node);
        }
        node.children.iter().find_map(|child| first_of_type(child, node_type))
    }

    fn tree(source: &str) -> SemanticNode {
        serde_json::from_str(&parse_graphql(source)).unwrap()
    }

    /// Every node's range must lie inside the source, on UTF-8 boundaries,
    /// and ids must be unique (mirrors core's assessment tree check).
    fn assert_tree_valid(node: &SemanticNode, source: &str, ids: &mut Vec<String>) {
        let lines: Vec<&str> = source.split('\n').collect();
        let mut seen = std::collections::HashSet::new();
        assert_node_valid(node, &lines, &mut seen);
        ids.extend(seen);
    }

    fn assert_node_valid(node: &SemanticNode, lines: &[&str], ids: &mut std::collections::HashSet<String>) {
        for (line, col) in [
            (node.position.start_line, node.position.start_col),
            (node.position.end_line, node.position.end_col),
        ] {
            let text = lines.get(line as usize).unwrap_or_else(|| panic!("{}: line {line} outside source", node.id));
            assert!(text.is_char_boundary(col as usize), "{}: col {col} invalid", node.id);
        }
        assert!(
            (node.position.start_line, node.position.start_col)
                <= (node.position.end_line, node.position.end_col),
            "{}: reversed range",
            node.id
        );
        assert!(ids.insert(node.id.clone()), "duplicate id {}", node.id);
        for child in &node.children {
            assert_node_valid(child, lines, ids);
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
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
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
    fn graphql_argument_spans_match_source_byte_columns() {
        let source = "query RecentInvoices {\n  invoices(limit: 25, offset: 5) {\n    reference\n  }\n}\n";
        let root: SemanticNode = serde_json::from_str(&parse_graphql(source)).unwrap();
        // children[0] of the operation is its `operation_type`; the field follows.
        let arguments: Vec<&SemanticNode> = root.children[0].children[1]
            .children
            .iter()
            .filter(|node| node.node_type == "argument")
            .collect();
        let line = source.lines().nth(1).unwrap();

        assert_eq!(arguments.len(), 2);
        for (node, label) in arguments.iter().zip(["limit: 25", "offset: 5"]) {
            let expected_start = line.find(label).unwrap() as u32;
            assert_eq!(node.node_type, "argument");
            assert_eq!(node.label, label);
            assert_eq!(node.position.start_line, 1);
            assert_eq!(node.position.end_line, 1);
            assert_eq!(node.position.start_col, expected_start);
            assert_eq!(node.position.end_col, expected_start + label.len() as u32);
        }
    }

    #[test]
    fn graphql_numeric_mutation_has_source_anchored_old_and_new_spans() {
        let old_source = "query Q {\n  invoices(limit: 25)\n}\n";
        let new_source = "query Q {\n  invoices(limit: 26)\n}\n";
        let old: SemanticNode = serde_json::from_str(&parse_graphql(old_source)).unwrap();
        let new: SemanticNode = serde_json::from_str(&parse_graphql(new_source)).unwrap();
        let old_arg = &old.children[0].children[1].children[0];
        let new_arg = &new.children[0].children[1].children[0];

        assert_ne!(old.structural_hash, new.structural_hash);
        assert_ne!(old_arg.structural_hash, new_arg.structural_hash);
        assert_eq!(old_arg.label, "limit: 25");
        assert_eq!(new_arg.label, "limit: 26");
        assert_eq!(old_arg.position.start_col, 11);
        assert_eq!(new_arg.position.start_col, 11);
        assert_eq!(old_arg.position.end_col, 20);
        assert_eq!(new_arg.position.end_col, 20);
    }

    #[test]
    fn graphql_argument_columns_are_utf8_byte_offsets() {
        let source = "query Q {\n  α: invoices(limit: 25)\n}\n";
        let root: SemanticNode = serde_json::from_str(&parse_graphql(source)).unwrap();
        // `α` is not a GraphQL Name, so it is preserved as an `unparsed`
        // token ahead of the field; locate the argument by type.
        let argument = first_of_type(&root, "argument").expect("argument node");
        assert_eq!(argument.label, "limit: 25");
        let source_line = source.lines().nth(1).unwrap();
        let start = source_line.find("limit: 25").unwrap() as u32;

        assert_eq!(argument.position.start_col, start);
        assert_eq!(argument.position.end_col, start + "limit: 25".len() as u32);
        assert_eq!(&source_line.as_bytes()[start as usize..argument.position.end_col as usize], b"limit: 25");
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

    #[test]
    fn argument_and_directive_positions_use_original_utf8_source_bytes() {
        // Regression for Python's 74-language source-judged corpus:
        // "  invoices(limit: 25) {" previously anchored the argument at
        // column 2 (the field), rather than column 11 (the argument).
        let statement = "  invoices(limit: 25) {";
        let source = format!("query RecentInvoices {{\n{statement}\n}}\n");
        let root: serde_json::Value = serde_json::from_str(&parse_graphql(&source)).unwrap();
        let argument = &root["children"][0]["children"][1]["children"][0];
        assert_eq!(argument["label"], "limit: 25");
        assert_eq!(argument["position"]["start_line"].as_u64(), Some(1));
        assert_eq!(argument["position"]["start_col"].as_u64(), Some(11));
        assert_eq!(argument["position"]["end_col"].as_u64(), Some(20));

        // Multiple arguments and a directive, including multibyte UTF-8
        // before subsequent arguments and repeated argument labels.
        let statement = "  invoices(note: \"café\", limit: 25, limit: 25) @cache";
        let source = format!("query RecentInvoices {{\n{statement}\n}}\n");
        let root: serde_json::Value = serde_json::from_str(&parse_graphql(&source)).unwrap();
        let children = root["children"][0]["children"][1]["children"].as_array().unwrap();
        let labels = ["note: \"café\"", "limit: 25", "limit: 25", "@cache"];
        assert_eq!(children.len(), labels.len());
        let mut cursor = 0;
        for (child, label) in children.iter().zip(labels) {
            let start = statement[cursor..].find(label).unwrap() + cursor;
            assert_eq!(child["label"], label);
            assert_eq!(child["position"]["start_col"].as_u64(), Some(start as u64));
            assert_eq!(
                child["position"]["end_col"].as_u64(),
                Some((start + label.len()) as u64)
            );
            cursor = start + label.len();
        }

        // A numeric literal change must remain source-anchored, not style-only.
        let old: SemanticNode = serde_json::from_str(
            &parse_graphql("query RecentInvoices {\n  invoices(limit: 5)\n}\n"),
        ).unwrap();
        let new: SemanticNode = serde_json::from_str(
            &parse_graphql("query RecentInvoices {\n  invoices(limit: 25)\n}\n"),
        ).unwrap();
        assert_ne!(old.structural_hash, new.structural_hash);
        let old_arg = &old.children[0].children[1].children[0];
        let new_arg = &new.children[0].children[1].children[0];
        assert_eq!(old_arg.position.start_col, 11);
        assert_eq!(new_arg.position.start_col, 11);
        assert_eq!(old_arg.label, "limit: 5");
        assert_eq!(new_arg.label, "limit: 25");
    }

    #[test]
    fn declaration_and_operation_names_use_original_utf8_source_columns() {
        let source = "type User {\n  id: ID\n}\n\nquery RecentInvoices {\n  invoices\n}\n";
        let parsed: serde_json::Value =
            serde_json::from_str(&parse_graphql(source)).unwrap();
        let definitions = parsed["children"].as_array().unwrap();
        assert_eq!(definitions.len(), 2);
        let ambiguous: serde_json::Value =
            serde_json::from_str(&parse_graphql("type t {\n  id: ID\n}\n")).unwrap();
        assert_eq!(ambiguous["children"][0]["label"], "t");
        assert_eq!(ambiguous["children"][0]["position"]["start_col"], 5);
        assert_eq!(ambiguous["children"][0]["position"]["end_col"], 6);

        assert_eq!(definitions[0]["label"], "User");
        assert_eq!(definitions[0]["position"]["start_col"].as_u64(), Some(5));
        assert_eq!(definitions[0]["position"]["end_col"].as_u64(), Some(9));
        assert_eq!(definitions[1]["label"], "RecentInvoices");
        assert_eq!(definitions[1]["position"]["start_col"].as_u64(), Some(6));
        assert_eq!(
            definitions[1]["position"]["end_col"].as_u64(),
            Some(6 + "RecentInvoices".len() as u64)
        );
    }

    // ---- Regressions for buchochelliq-labs/intentumdiff-graphql-parser#8 ----

    /// Exact old/new pairs from issue #8 and core's language assessment that
    /// were previously hashed identically (reported style-only).
    const CHANGED_PAIRS: &[(&str, &str, &str)] = &[
        ("issue_field_type", "type Query {\n  value: Int\n}\n", "type Query {\n  value: String\n}\n"),
        ("issue_non_null", "type Query {\n  value: Int\n}\n", "type Query {\n  value: Int!\n}\n"),
        ("issue_kind", "query Do {\n  value\n}\n", "mutation Do {\n  value\n}\n"),
        ("issue_inline_field", "type Query { value: Int }\n", "type Query { other: String }\n"),
        (
            "issue_hash_string",
            "query Q {\n  search(text: \"#safe\")\n}\n",
            "query Q {\n  search(text: \"#danger\")\n}\n",
        ),
        (
            "assessment_meaningful_2_type",
            "type User {\n  id: ID\n  name: String\n  loginCount: Int\n}\n",
            "type User {\n  id: ID\n  name: String!\n  loginCount: Int\n}\n",
        ),
        ("assessment_operation_kind", "query Read { item { id } }\n", "mutation Read { item { id } }\n"),
        (
            "assessment_hash_in_string",
            "query Search { search(text: \"#safe\") }\n",
            "query Search { search(text: \"#danger\") }\n",
        ),
        ("assessment_inline_field_type", "type User { id: ID name: String }\n", "type User { id: ID name: Int }\n"),
        // Same constructs across lines / other positions.
        ("subscription_kind", "query Do { value }\n", "subscription Do { value }\n"),
        ("anonymous_kind", "{ value }\n", "mutation { value }\n"),
        ("list_type", "type Q {\n  v: [Int]\n}\n", "type Q {\n  v: [Int!]\n}\n"),
        ("argument_type", "type Q {\n  v(a: Int): Int\n}\n", "type Q {\n  v(a: Int!): Int\n}\n"),
        ("argument_default", "type Q {\n  v(a: Int = 1): Int\n}\n", "type Q {\n  v(a: Int = 2): Int\n}\n"),
        ("input_field_type", "input I {\n  a: Int\n}\n", "input I {\n  a: Float\n}\n"),
        ("definition_kind", "type T {\n  a: Int\n}\n", "input T {\n  a: Int\n}\n"),
        ("variable_type", "query Q($a: Int) { v(a: $a) }\n", "query Q($a: Int!) { v(a: $a) }\n"),
        ("variable_default", "query Q($a: Int = 1) { v }\n", "query Q($a: Int = 2) { v }\n"),
        (
            "directive_argument",
            "type T {\n  a: Int @deprecated(reason: \"x\")\n}\n",
            "type T {\n  a: Int @deprecated(reason: \"y\")\n}\n",
        ),
        ("nested_selection", "query Q { a { b } }\n", "query Q { a { c } }\n"),
        ("second_inline_selection", "query Q { a b }\n", "query Q { a c }\n"),
        ("alias", "query Q { x: a }\n", "query Q { y: a }\n"),
        (
            "hash_in_block_string",
            "query Q {\n  s(t: \"\"\"#a\"\"\")\n}\n",
            "query Q {\n  s(t: \"\"\"#b\"\"\")\n}\n",
        ),
        ("hash_and_brace_in_string", "query Q { s(t: \"} # {\") a }\n", "query Q { s(t: \"} # {\") b }\n"),
        ("enum_value", "enum E { A B }\n", "enum E { A C }\n"),
        ("union_member", "union U = A | B\n", "union U = A | C\n"),
        ("implements", "type T implements A { a: Int }\n", "type T implements B { a: Int }\n"),
        ("fragment_condition", "fragment F on A { a }\n", "fragment F on B { a }\n"),
        ("inline_fragment", "query Q { ... on A { a } }\n", "query Q { ... on B { a } }\n"),
        ("schema_root", "schema { query: Q }\n", "schema { query: R }\n"),
        ("directive_location", "directive @d on FIELD\n", "directive @d on QUERY\n"),
        ("object_value", "query Q { a(i: {x: 1}) }\n", "query Q { a(i: {x: 2}) }\n"),
        ("list_value", "query Q { a(i: [1, 2]) }\n", "query Q { a(i: [1, 3]) }\n"),
        ("unknown_trailing_token", "type T { a: Int }\n", "type T { a: Int }\nty\n"),
    ];

    #[test]
    fn issue_8_semantic_edits_change_hashes() {
        for (case, old, new) in CHANGED_PAIRS {
            let old_tree = tree(old);
            let new_tree = tree(new);
            assert_ne!(
                old_tree.structural_hash, new_tree.structural_hash,
                "{case}: semantic edit hashed as style-only"
            );
            assert_tree_valid(&old_tree, old, &mut Vec::new());
            assert_tree_valid(&new_tree, new, &mut Vec::new());
        }
    }

    /// Identity, whitespace, comma and comment-only edits keep the hash.
    #[test]
    fn layout_and_comment_only_edits_keep_hashes() {
        let controls: &[(&str, &str, &str)] = &[
            ("identity", "type Query { value: Int }\n", "type Query { value: Int }\n"),
            ("reflow", "type User { id: ID name: String }\n", "type User {\n  id: ID\n  name: String\n}\n"),
            ("commas", "query Q { a(x: 1, y: 2) }\n", "query Q { a(x: 1 y: 2) }\n"),
            ("spacing", "query Q($a:Int!){v(a:$a)}\n", "query Q ( $a : Int ! ) { v ( a : $a ) }\n"),
            ("list_spacing", "query Q { a(i: [1,2]) }\n", "query Q { a(i: [ 1 2 ]) }\n"),
            ("comment_line", "type T { a: Int }\n", "# note\ntype T { a: Int }\n"),
            ("trailing_comment", "type T {\n  a: Int\n}\n", "type T {\n  a: Int # was Float\n}\n"),
            ("comment_with_quote", "type T { a: Int }\n", "type T { a: Int } # \"quoted\" {\n"),
            ("crlf", "type T {\n  a: Int\n}\n", "type T {\r\n  a: Int\r\n}\r\n"),
        ];
        for (case, old, new) in controls {
            assert_eq!(
                tree(old).structural_hash,
                tree(new).structural_hash,
                "{case}: layout/comment-only edit changed the hash"
            );
        }
    }

    #[test]
    fn field_types_and_operation_kinds_are_retained_with_source_spans() {
        let source = "type User { id: ID name: String! }\nmutation Read { item { id } }\n";
        let root = tree(source);
        let user = &root.children[0];
        assert_eq!(user.label, "User");
        let fields: Vec<&SemanticNode> = user.children.iter().filter(|n| n.node_type == "field").collect();
        assert_eq!(fields.len(), 2, "both same-line fields must be retained");
        assert_eq!(fields[1].label, "name");
        let field_type = first_of_type(fields[1], "field_type").unwrap();
        assert_eq!(field_type.label, "String!");
        let first_line = source.lines().next().unwrap();
        let start = first_line.find("String!").unwrap() as u32;
        assert_eq!((field_type.position.start_line, field_type.position.start_col), (0, start));
        assert_eq!((field_type.position.end_line, field_type.position.end_col), (0, start + 7));

        let operation = &root.children[1];
        assert_eq!(operation.node_type, "operation_definition");
        assert_eq!(operation.label, "Read");
        let kind = first_of_type(operation, "operation_type").unwrap();
        assert_eq!(kind.label, "mutation");
        assert_eq!((kind.position.start_line, kind.position.start_col, kind.position.end_col), (1, 0, 8));
        let item = first_of_type(operation, "field").unwrap();
        assert_eq!(item.label, "item");
        assert_eq!(item.children[0].label, "id", "nested selection is a child of its field");
    }

    #[test]
    fn quoted_hash_is_string_data_not_a_comment() {
        let source = "query Search { search(text: \"#danger\") } # trailing comment\n";
        let root = tree(source);
        let argument = first_of_type(&root, "argument").unwrap();
        assert_eq!(argument.label, "text: \"#danger\"");
        let start = source.find("text").unwrap() as u32;
        let end = source.find(") }").unwrap() as u32;
        assert_eq!((argument.position.start_col, argument.position.end_col), (start, end));
        let mut labels = Vec::new();
        labels_by_type(&root, "unparsed", &mut labels);
        assert!(labels.is_empty(), "trailing comment must not leak: {labels:?}");
    }

    #[test]
    fn block_strings_spanning_lines_keep_exact_positions() {
        let source = "\"\"\"\nDoc # not a comment\n\"\"\"\ntype T {\n  a: Int\n}\n";
        let root = tree(source);
        assert_tree_valid(&root, source, &mut Vec::new());
        let definition = &root.children[0];
        assert_eq!(definition.label, "T");
        assert_eq!((definition.position.start_line, definition.position.start_col), (3, 5));
        let description = first_of_type(definition, "description").unwrap();
        assert_eq!((description.position.start_line, description.position.end_line), (0, 2));
        assert!(description.label.contains("# not a comment"));
    }

    #[test]
    fn malformed_and_adversarial_input_is_bounded_and_preserved() {
        let inputs = [
            "type User {\n  id: ID\n}\n\nty",
            "query Q { a(t: \"unterminated) }\n",
            "query Q { a(t: \"\"\"never closed\n",
            "}}} ))) ]]] @@@ ... $$$ !!! ===",
            "query { α λ € }\n",
            "extend nonsense",
            "",
            "\u{feff}# only a comment\n",
        ];
        for source in inputs {
            let root = tree(source);
            assert_tree_valid(&root, source, &mut Vec::new());
            assert_eq!(parse_graphql(source), parse_graphql(source), "deterministic");
        }
        let deep = format!("query Q {}{}", "{ a ".repeat(10_000), "}".repeat(10_000));
        let root = tree(&deep);
        assert_tree_valid(&root, &deep, &mut Vec::new());
        let deep_value = format!("query Q {{ a(x: {}1{}) }}", "[".repeat(10_000), "]".repeat(10_000));
        assert_tree_valid(&tree(&deep_value), &deep_value, &mut Vec::new());
        // A truncated trailing token is retained, not certified as style.
        assert_ne!(
            tree("type User {\n  id: ID\n}\n").structural_hash,
            tree("type User {\n  id: ID\n}\n\nty").structural_hash
        );
    }

    #[test]
    fn block_strings_with_non_ascii_text_do_not_panic() {
        // Codex P1 on PR #9: the block-string scanner advanced one byte at a
        // time and sliced `&str` inside multibyte characters.
        let cases = [
            ("\"\"\"café\"\"\" type T { a: Int }", "café"),
            ("\"\"\"rocket 🚀 launch\"\"\"\ntype T { a: Int }\n", "🚀"),
            ("\"\"\"用户类型\"\"\"\ntype T { a: Int }\n", "用户类型"),
            ("\"\"\"say \\\"\"\" é \\\"\"\" ok\"\"\"\ntype T { a: Int }\n", "\\\"\"\" é"),
            ("\"\"\"ends with é\"\"\"\ntype T { a: Int }\n", "é"),
            ("\"\"\"ends with 🚀\"\"\"\ntype T { a: Int }\n", "🚀"),
            ("\"\"\"ends with 字\"\"\"\ntype T { a: Int }\n", "字"),
            ("\"\"\"\nmulti é\nline 🚀\n\"\"\"\ntype T { a: Int }\n", "line 🚀"),
        ];
        for (source, needle) in cases {
            let root = tree(source);
            assert_tree_valid(&root, source, &mut Vec::new());
            let definition = &root.children[0];
            assert_eq!(definition.label, "T", "{source:?}");
            let description = first_of_type(definition, "description").expect("description");
            assert!(description.label.contains(needle), "{source:?}: {:?}", description.label);
            assert!(description.label.ends_with("\"\"\""), "{source:?}: {:?}", description.label);
            let mut labels = Vec::new();
            labels_by_type(&root, "unparsed", &mut labels);
            assert!(labels.is_empty(), "{source:?}: {labels:?}");
        }
        // Block strings as argument values and unterminated non-ASCII ones.
        for source in [
            "query Q { a(t: \"\"\"naïve ☃\"\"\") }\n",
            "query Q { a(t: \"\"\"never closed é",
            "query Q { a(t: \"\"\"never closed 🚀\n",
            "\"\"\"é",
            "\"\"\"",
            "\"\"\"\"",
            "\"\"\"\\",
            "\"\"\"é\\\"\"",
        ] {
            let root = tree(source);
            assert_tree_valid(&root, source, &mut Vec::new());
        }
        let value = tree("query Q { a(t: \"\"\"naïve ☃\"\"\") }\n");
        let mut arguments = Vec::new();
        labels_by_type(&value, "argument", &mut arguments);
        assert_eq!(arguments, vec!["t: \"\"\"naïve ☃\"\"\"".to_string()]);
    }

    #[test]
    fn non_ascii_block_string_edits_change_hashes() {
        let old = tree("\"\"\"café\"\"\"\ntype T { a: Int }\n");
        let new = tree("\"\"\"cafe\"\"\"\ntype T { a: Int }\n");
        assert_ne!(old.structural_hash, new.structural_hash);
    }

    #[test]
    fn missing_declared_types_and_defaults_never_reverse_spans() {
        // Codex P2 on PR #9: an absent type/default took `start` from the
        // next token but `end` from the `:` / `=`, reversing the range.
        let sources = [
            "type T { a: }",
            "type T { a:\n}\n",
            "interface I { a: }",
            "input I { a: }",
            "input I { a:\n}\n",
            "input I { a: Int = }",
            "input I { a: Int =\n}\n",
            "type T { f(x: ): Int }",
            "type T { f(x:\n): Int }",
            "type T { f(x: Int = ): Int }",
            "directive @d(x: ) on FIELD",
            "type T { a: @deprecated }",
            "type T { a:",
            "input I { a: Int =",
        ];
        for source in sources {
            let root = tree(source);
            assert_tree_valid(&root, source, &mut Vec::new());
        }
        // A missing type is a zero-width marker right after the colon.
        let root = tree("type T { a:\n}\n");
        let field_type = first_of_type(&root, "field_type").expect("field_type");
        assert_eq!(field_type.label, "");
        assert_eq!(
            (field_type.position.start_line, field_type.position.start_col, field_type.position.end_line, field_type.position.end_col),
            (0, 11, 0, 11)
        );
        // A present type keeps its exact source span.
        let root = tree("type T { a: [Int!]! }");
        let field_type = first_of_type(&root, "field_type").expect("field_type");
        assert_eq!(field_type.label, "[Int!]!");
        assert_eq!((field_type.position.start_col, field_type.position.end_col), (12, 19));
    }

    #[test]
    fn every_truncation_of_non_ascii_source_is_valid() {
        // Partially typed schemas: every char-boundary prefix must parse to
        // a valid tree (no panic, no reversed or off-boundary spans).
        let source = "\"\"\"Café 🚀 用户 \\\"\"\" é\"\"\"\ntype T implements I & J @d(x: \"ü\") {\n  \"naïve\" a(x: [Int!]! = [1, 2]): [Str!]!\n  b: Int = 3\n}\ninput In { v: Int = 1 }\nquery Q($x: Int = 2) { a(t: \"\"\"☃\"\"\") { ...F ... on T { b } } }\n";
        for (end, _) in source.char_indices().chain(std::iter::once((source.len(), ' '))) {
            let prefix = &source[..end];
            assert_tree_valid(&tree(prefix), prefix, &mut Vec::new());
        }
    }
}
