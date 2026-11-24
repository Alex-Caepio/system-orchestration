//! Unified pure-Rust code AST, symbol, reference, and framework/DB semantic extractor.
//!
//! Covers all 14 `layers::CODE` grammars (`php`, `blade`, `rust`, `typescript`, `tsx`,
//! `javascript`, `python`, `go`, `java`, `csharp`, `ruby`, `kotlin`, `swift`, `scala` plus
//! aliases) in pure Rust with zero C tree-sitter dependencies.

use crate::{
    graph::file_types::is_blade_template,
    graph_deps::DeclaredMember,
    graph_live::{CreatedEntity, ExtractedDecl, Language, clean_rel_key},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

mod extract;
mod lex;
mod semantics;

pub(crate) use self::semantics::normalize_route_contract;

const MAX_AST_BYTES: usize = crate::graph_map::SOURCE_LIMIT as usize;
const MAX_SQL_LITERAL_BYTES: usize = 8192;

pub(crate) const GRAPH_MANIFEST_FILES: &[&str] = &[
    "composer.json",
    "tsconfig.json",
    "jsconfig.json",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "cargo.toml",
];
pub(crate) const GRAPH_ARCHITECTURE_FILE: &str = ".harness-architecture.json";

/// Grammar variant selected for pure-Rust code parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AstGrammar {
    Php,
    Blade,
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Java,
    CSharp,
    Ruby,
    Kotlin,
    Swift,
    Scala,
}

impl AstGrammar {
    pub(crate) fn from_rel_path(rel_path: &str) -> Option<Self> {
        if is_blade_template(rel_path) {
            return Some(Self::Blade);
        }
        let ext = rel_path.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "php" | "phtml" | "inc" => Some(Self::Php),
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            "rb" => Some(Self::Ruby),
            "kt" | "kts" => Some(Self::Kotlin),
            "swift" => Some(Self::Swift),
            "scala" | "sc" => Some(Self::Scala),
            _ => None,
        }
    }

    pub(crate) fn to_live_language(self) -> Language {
        match self {
            Self::Php | Self::Blade => Language::Php,
            Self::Rust => Language::Rust,
            Self::TypeScript | Self::Tsx | Self::JavaScript => Language::TsJs,
            Self::Python => Language::Python,
            Self::Go => Language::Go,
            Self::Java => Language::Java,
            Self::CSharp => Language::CSharp,
            Self::Ruby => Language::Ruby,
            Self::Kotlin => Language::Kotlin,
            Self::Swift => Language::Swift,
            Self::Scala => Language::Scala,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Php => "php",
            Self::Blade => "blade",
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::Scala => "scala",
        }
    }
}

/// Structured import, include, mod declaration, or re-export item extracted from code.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AstImport {
    pub path: String,
    pub symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub is_wildcard: bool,
    pub is_reexport: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reexport_visibility: Option<String>,
    pub kind: String,
    pub line: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AstInheritance {
    pub child: String,
    pub parent: String,
    pub kind: String,
    pub line: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AstRef {
    pub target_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    pub context: String,
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<String>,
    pub is_static: bool,
    pub line: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SemanticEdge {
    pub kind: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_symbol: Option<String>,
    pub line: u32,
    pub evidence: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AstFileExtraction {
    pub rel_path: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub declarations: Vec<ExtractedDecl>,
    pub declared_members: Vec<DeclaredMember>,
    pub imports: Vec<AstImport>,
    pub inheritance: Vec<AstInheritance>,
    pub calls_and_refs: Vec<AstRef>,
    pub semantic_edges: Vec<SemanticEdge>,
    #[serde(default)]
    pub test_spans: Vec<(u32, u32)>,
    #[serde(default, skip_serializing)]
    pub identifiers: BTreeSet<String>,
}

pub(crate) fn is_graph_manifest_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            GRAPH_MANIFEST_FILES.contains(&lower.as_str())
                || ((lower.starts_with("tsconfig.") || lower.starts_with("jsconfig."))
                    && lower.ends_with(".json"))
        })
}

pub(crate) fn is_graph_architecture_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(GRAPH_ARCHITECTURE_FILE))
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
struct TsRouterLeaves {
    direct: Vec<(String, String, u32, String)>,
    nested: Vec<(String, String)>,
}

pub(super) struct ExtractCtx<'a> {
    rel_path: String,
    grammar: AstGrammar,
    #[allow(dead_code)]
    source: &'a str,
    namespace: Option<String>,
    declarations: Vec<ExtractedDecl>,
    imports: Vec<AstImport>,
    inheritance: Vec<AstInheritance>,
    calls_and_refs: Vec<AstRef>,
    semantic_edges: Vec<SemanticEdge>,
    test_spans: Vec<(u32, u32)>,
    test_lines: BTreeSet<u32>,
    seen_semantic: BTreeSet<(String, String, u32)>,
    seen_decls: BTreeSet<(String, Option<String>, String, u32)>,
    attr_route_prefix_stack: Vec<String>,
    call_route_prefix_stack: Vec<String>,
    #[allow(dead_code)]
    ts_router_leaves: BTreeMap<String, TsRouterLeaves>,
    var_types: BTreeMap<String, BTreeMap<String, String>>,
}

impl<'a> ExtractCtx<'a> {
    fn new(rel_path: &str, grammar: AstGrammar, source: &'a str) -> Self {
        Self {
            rel_path: rel_path.to_owned(),
            grammar,
            source,
            namespace: None,
            declarations: Vec::new(),
            imports: Vec::new(),
            inheritance: Vec::new(),
            calls_and_refs: Vec::new(),
            semantic_edges: Vec::new(),
            test_spans: Vec::new(),
            test_lines: BTreeSet::new(),
            seen_semantic: BTreeSet::new(),
            seen_decls: BTreeSet::new(),
            attr_route_prefix_stack: Vec::new(),
            call_route_prefix_stack: Vec::new(),
            ts_router_leaves: BTreeMap::new(),
            var_types: BTreeMap::new(),
        }
    }

    pub(super) fn record_var_type(&mut self, scope: &str, var: &str, ty: &str) {
        let clean_v = var.trim().trim_start_matches('$');
        let clean_t = ty.trim();
        if !clean_v.is_empty() && !clean_t.is_empty() {
            self.var_types
                .entry(scope.to_owned())
                .or_default()
                .insert(clean_v.to_owned(), clean_t.to_owned());
        }
    }

    pub(super) fn lookup_var_type(&self, encl: Option<&str>, var: &str) -> Option<String> {
        let clean_v = var.trim().trim_start_matches('$');
        if let Some(sym) = encl {
            if let Some(t) = self.var_types.get(sym).and_then(|m| m.get(clean_v)) {
                return Some(t.clone());
            }
            if let Some((owner, _)) = sym.split_once("::")
                && let Some(t) = self.var_types.get(owner).and_then(|m| m.get(clean_v))
            {
                return Some(t.clone());
            }
        }
        self.var_types.get("").and_then(|m| m.get(clean_v)).cloned()
    }

    fn compose_segments(prefixes: &[String], leaf_path: &str) -> String {
        let leaf = leaf_path.trim();
        if prefixes.is_empty() || leaf.starts_with("http://") || leaf.starts_with("https://") {
            return leaf.to_owned();
        }
        let mut segments: Vec<&str> = Vec::new();
        for pref in prefixes {
            for seg in pref.trim().split('/').filter(|s| !s.is_empty()) {
                segments.push(seg);
            }
        }
        let (leaf_route, leaf_suffix) = match leaf.find(['?', '#']) {
            Some(idx) => (&leaf[..idx], &leaf[idx..]),
            None => (leaf, ""),
        };
        for seg in leaf_route.split('/').filter(|s| !s.is_empty()) {
            segments.push(seg);
        }
        if segments.is_empty() {
            format!("/{leaf_suffix}")
        } else {
            format!("/{}{}", segments.join("/"), leaf_suffix)
        }
    }

    fn push_attr_route_prefix(&mut self, prefix: &str) -> usize {
        let mark = self.attr_route_prefix_stack.len();
        let clean = prefix.trim();
        if !clean.is_empty() && clean != "/" {
            self.attr_route_prefix_stack.push(clean.to_owned());
        }
        mark
    }

    fn truncate_attr_route_prefixes(&mut self, len: usize) {
        self.attr_route_prefix_stack.truncate(len);
    }

    fn compose_attr_route_path(&self, leaf_path: &str) -> String {
        Self::compose_segments(&self.attr_route_prefix_stack, leaf_path)
    }

    fn push_call_route_prefix(&mut self, prefix: &str) -> usize {
        let mark = self.call_route_prefix_stack.len();
        let clean = prefix.trim();
        if !clean.is_empty() && clean != "/" {
            self.call_route_prefix_stack.push(clean.to_owned());
        }
        mark
    }

    fn truncate_call_route_prefixes(&mut self, len: usize) {
        self.call_route_prefix_stack.truncate(len);
    }

    fn compose_call_route_path(&self, leaf_path: &str) -> String {
        Self::compose_segments(&self.call_route_prefix_stack, leaf_path)
    }

    #[allow(clippy::too_many_arguments)]
    fn push_decl(
        &mut self,
        kind: &str,
        name: &str,
        parent: Option<String>,
        line: u32,
        end_line: u32,
        visibility: Option<String>,
        modifiers: Vec<String>,
        signature: String,
        params_signature: String,
        body_fingerprint: String,
    ) {
        let clean_name = name.trim();
        if clean_name.is_empty() {
            return;
        }
        let parent_val = parent.or_else(|| Some(self.rel_path.clone()));
        let key = (
            kind.to_owned(),
            parent_val.clone(),
            clean_name.to_owned(),
            line,
        );
        if !self.seen_decls.insert(key) {
            return;
        }
        let entity = CreatedEntity::new(kind, clean_name, parent_val, &self.rel_path, Some(line))
            .with_access(visibility.clone(), modifiers.clone());
        self.declarations.push(ExtractedDecl {
            entity,
            signature,
            params_signature,
            body_fingerprint,
            visibility,
            modifiers,
            start_line: line,
            end_line: end_line.max(line),
        });
    }

    fn push_import(&mut self, import: AstImport) {
        if !import.path.trim().is_empty() {
            self.imports.push(import);
        }
    }

    fn push_inheritance(
        &mut self,
        child: &str,
        parent: &str,
        kind: &str,
        line: u32,
        evidence: &str,
    ) {
        let c = child.trim();
        let p = parent.trim();
        if !c.is_empty() && !p.is_empty() {
            self.inheritance.push(AstInheritance {
                child: c.to_owned(),
                parent: p.to_owned(),
                kind: kind.to_owned(),
                line,
                evidence: clip_str(evidence, 160),
            });
        }
    }

    fn push_ref(&mut self, r: AstRef) {
        if !r.target_name.trim().is_empty() {
            self.calls_and_refs.push(r);
        }
    }

    fn push_semantic(
        &mut self,
        kind: &str,
        target: String,
        source_symbol: Option<String>,
        line: u32,
        evidence: &str,
    ) {
        if target.is_empty() {
            return;
        }
        let key = (kind.to_owned(), target.clone(), line);
        if self.seen_semantic.insert(key) {
            self.semantic_edges.push(SemanticEdge {
                kind: kind.to_owned(),
                target,
                source_symbol,
                line,
                evidence: clip_str(evidence, 160),
            });
        }
    }

    fn enclosing_symbol_at_line(&self, line: u32) -> Option<String> {
        if line == 0 {
            return None;
        }
        let mut best: Option<&ExtractedDecl> = None;
        let mut best_span = u32::MAX;
        for decl in &self.declarations {
            if line >= decl.start_line && line <= decl.end_line {
                let span = decl.end_line.saturating_sub(decl.start_line);
                let better = match best {
                    None => true,
                    Some(prev) => {
                        span < best_span || (span == best_span && decl.start_line > prev.start_line)
                    }
                };
                if better {
                    best = Some(decl);
                    best_span = span;
                }
            }
        }
        let decl = best?;
        match decl.entity.parent.as_deref() {
            Some(owner) if owner != self.rel_path => {
                Some(format!("{owner}::{}", decl.entity.name))
            }
            _ => Some(decl.entity.name.clone()),
        }
    }

    fn into_extraction(self, identifiers: BTreeSet<String>) -> AstFileExtraction {
        let rel = self.rel_path.clone();
        let declared_members = self
            .declarations
            .iter()
            .map(|decl| {
                let parent = decl
                    .entity
                    .parent
                    .clone()
                    .filter(|p| !p.is_empty() && *p != rel);
                let signature = if decl.signature.is_empty() {
                    None
                } else {
                    Some(decl.signature.clone())
                };
                DeclaredMember {
                    name: decl.entity.name.clone(),
                    kind: decl.entity.kind.clone(),
                    parent,
                    line: decl.entity.line,
                    signature,
                    visibility: decl.visibility.clone(),
                    previous_visibility: None,
                    modifiers: decl.modifiers.clone(),
                    change: "unchanged".to_owned(),
                    signature_changed: false,
                    visibility_narrowed: false,
                    used_by_caller: false,
                }
            })
            .collect();

        let mut test_spans = self.test_spans;
        if !self.test_lines.is_empty() {
            for decl in &self.declarations {
                let near_attr = self
                    .test_lines
                    .range(decl.start_line.saturating_sub(3)..=decl.start_line)
                    .next()
                    .is_some();
                if near_attr {
                    test_spans.push((decl.start_line, decl.end_line.max(decl.start_line)));
                }
            }
            for &line in &self.test_lines {
                test_spans.push((line, line));
            }
        }
        test_spans.sort_unstable();
        test_spans.dedup();

        AstFileExtraction {
            rel_path: self.rel_path,
            language: self.grammar.label().to_owned(),
            namespace: self.namespace,
            declarations: self.declarations,
            declared_members,
            imports: self.imports,
            inheritance: self.inheritance,
            calls_and_refs: self.calls_and_refs,
            semantic_edges: self.semantic_edges,
            test_spans,
            identifiers,
        }
    }
}

pub(super) fn clip_str(raw: &str, max_len: usize) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= max_len {
        return compact;
    }
    let mut end = max_len;
    while end > 0 && !compact.is_char_boundary(end) {
        end -= 1;
    }
    compact[..end].to_owned()
}

pub(super) fn unquote_literal(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.len() < 2 {
        return None;
    }
    for delim in ["\"\"\"", "'''"] {
        if t.starts_with(delim) && t.ends_with(delim) && t.len() >= delim.len() * 2 {
            return Some(t[delim.len()..t.len() - delim.len()].to_owned());
        }
    }
    let first = t.chars().next()?;
    let last = t.chars().last()?;
    if matches!(first, '\'' | '"' | '`') && first == last {
        return Some(t[1..t.len() - 1].to_owned());
    }
    None
}

pub(super) fn is_upper_snake(name: &str) -> bool {
    let clean = name.trim();
    !clean.is_empty()
        && clean
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && clean.chars().any(|c| c.is_ascii_uppercase())
}

pub(super) fn extract_class_ref_name(raw: &str) -> Option<String> {
    let t = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | ' ' | '\t' | '\n' | '\r'))
        .trim();
    for suffix in ["::class", ".class", "::TYPE"] {
        if let Some(base) = t.strip_suffix(suffix) {
            let short = base
                .rsplit(['\\', ':', '.'])
                .next()
                .unwrap_or(base)
                .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            if !short.is_empty() {
                return Some(short.to_owned());
            }
        }
    }
    None
}

pub(crate) fn extract_code_identifiers(rel_path: &str, source: &str) -> BTreeSet<String> {
    let rel = clean_rel_key(rel_path);
    let Some(grammar) = AstGrammar::from_rel_path(&rel) else {
        return BTreeSet::new();
    };
    if source.len() > MAX_AST_BYTES {
        return BTreeSet::new();
    }
    lex::lex(grammar, source).identifiers
}

pub(crate) fn parse_file_ir(rel_path: &str, source: &str) -> Option<AstFileExtraction> {
    let rel = clean_rel_key(rel_path);
    if rel.is_empty() || source.len() > MAX_AST_BYTES {
        return None;
    }
    let grammar = AstGrammar::from_rel_path(&rel)?;
    let lexed = lex::lex(grammar, source);
    let mut ctx = ExtractCtx::new(&rel, grammar, source);
    extract::extract_by_grammar(&mut ctx, &lexed);
    Some(ctx.into_extraction(lexed.identifiers))
}

pub(crate) fn extract_declarations_for_live(rel_path: &str, source: &str) -> Vec<ExtractedDecl> {
    parse_file_ir(rel_path, source)
        .map(|ir| ir.declarations)
        .unwrap_or_default()
}

pub(crate) fn extract_declared_members_ast(rel_path: &str, source: &str) -> Vec<DeclaredMember> {
    parse_file_ir(rel_path, source)
        .map(|ir| ir.declared_members)
        .unwrap_or_default()
}
