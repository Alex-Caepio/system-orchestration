//! Pure-Rust token-stream AST extractor covering PHP, Blade, Rust, TypeScript/JS,
//! Python, Go, Java, C#, Ruby, Kotlin, Swift, and Scala.

use super::{
    AstGrammar, AstImport, AstRef, ExtractCtx, clip_str,
    lex::{Lexed, Token, TokenKind},
    semantics::{emit_semantic_from_attribute, emit_semantic_from_call, emit_sql_table_edges},
};

pub(super) fn extract_by_grammar(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    match ctx.grammar {
        AstGrammar::Php => extract_php(ctx, lexed),
        AstGrammar::Blade => extract_blade(ctx, lexed),
        AstGrammar::Rust => extract_rust(ctx, lexed),
        AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript => {
            extract_ts_js(ctx, lexed);
        }
        AstGrammar::Python => extract_python(ctx, lexed),
        AstGrammar::Go => extract_go(ctx, lexed),
        AstGrammar::Java
        | AstGrammar::CSharp
        | AstGrammar::Kotlin
        | AstGrammar::Swift
        | AstGrammar::Scala => {
            extract_c_family(ctx, lexed);
        }
        AstGrammar::Ruby => extract_ruby(ctx, lexed),
    }

    // Extract SQL table dependencies from all string literals using exact enclosing symbol spans.
    for s in &lexed.strings {
        if s.raw.len() <= super::MAX_SQL_LITERAL_BYTES {
            let owner = ctx.enclosing_symbol_at_line(s.line);
            emit_sql_table_edges(ctx, &s.unquoted, owner, s.line);
        }
    }
}

// =========================================================================
// Shared Token-Stream & Expression Helpers
// =========================================================================

fn matching_delim(tokens: &[Token<'_>], open_idx: usize, open: char, close: char) -> usize {
    let mut depth = 0isize;
    let mut i = open_idx;
    while i < tokens.len() {
        let k = &tokens[i].kind;
        let is_open = matches!(k, TokenKind::Punct(c) if *c == open)
            || (open == '[' && matches!(k, TokenKind::Sym("#[") | TokenKind::Sym("#![")));
        if is_open {
            depth += 1;
        } else if matches!(k, TokenKind::Punct(c) if *c == close) {
            depth -= 1;
            if depth <= 0 {
                return i;
            }
        }
        i += 1;
    }
    tokens.len().saturating_sub(1)
}

fn split_top_level(tokens: &[Token<'_>], lo: usize, hi: usize, sep: char) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if lo >= hi {
        return out;
    }
    let mut start = lo;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut angle = 0i32;
    let mut i = lo;
    while i < hi {
        match tokens[i].kind {
            TokenKind::Punct('(') => paren += 1,
            TokenKind::Punct(')') => paren = (paren - 1).max(0),
            TokenKind::Punct('[') | TokenKind::Sym("#[") | TokenKind::Sym("#![") => bracket += 1,
            TokenKind::Punct(']') => bracket = (bracket - 1).max(0),
            TokenKind::Punct('{') => brace += 1,
            TokenKind::Punct('}') => brace = (brace - 1).max(0),
            TokenKind::Punct('<') => angle += 1,
            TokenKind::Punct('>') => angle = (angle - 1).max(0),
            TokenKind::Punct(c)
                if c == sep && paren == 0 && bracket == 0 && brace == 0 && angle == 0 =>
            {
                out.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < hi {
        out.push((start, hi));
    }
    out
}

fn collect_string_args(tokens: &[Token<'_>], lo: usize, hi: usize) -> Vec<String> {
    let mut out = Vec::new();
    for t in &tokens[lo..hi.min(tokens.len())] {
        if let TokenKind::Str { unquoted, .. } = &t.kind
            && !unquoted.is_empty()
        {
            out.push(unquoted.clone());
        }
    }
    out
}

fn short_type(raw: &str) -> String {
    let base = raw
        .trim()
        .split(['<', '['])
        .next()
        .unwrap_or("")
        .trim_start_matches(['?', '&', '*', '(', ')'])
        .trim();
    let without_qual = base
        .strip_prefix("mut ")
        .or_else(|| base.strip_prefix("dyn "))
        .or_else(|| base.strip_prefix("impl "))
        .unwrap_or(base)
        .trim();
    without_qual
        .rsplit(['\\', ':', '.'])
        .next()
        .unwrap_or(without_qual)
        .trim()
        .to_owned()
}

fn is_upper_ident(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn is_builtin_type(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "int"
            | "integer"
            | "float"
            | "double"
            | "string"
            | "str"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "callable"
            | "iterable"
            | "mixed"
            | "void"
            | "never"
            | "null"
            | "nil"
            | "none"
            | "undefined"
            | "any"
            | "unknown"
            | "number"
            | "false"
            | "true"
            | "self"
            | "static"
            | "parent"
            | "this"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "char"
            | "error"
            | "byte"
            | "rune"
    )
}

fn strip_leading_words(sig: &str, words: &[&str]) -> String {
    let mut parts = Vec::new();
    let mut skip = true;
    for tok in sig.split_whitespace() {
        if skip && words.contains(&tok) {
            continue;
        }
        skip = false;
        parts.push(tok);
    }
    parts.join(" ")
}

fn strip_php_vis_from_sig(sig: &str) -> String {
    let mut cur = sig.trim();
    loop {
        cur = cur.trim_start();
        let next = cur.split_whitespace().next().unwrap_or("");
        if matches!(next, "public" | "protected" | "private" | "var")
            || next.starts_with("public(")
            || next.starts_with("protected(")
            || next.starts_with("private(")
        {
            cur = cur[next.len()..].trim_start();
            if cur.starts_with("(set)") {
                cur = cur[5..].trim_start();
            }
            continue;
        }
        break;
    }
    cur.to_owned()
}

fn strip_rust_vis_from_sig(sig: &str) -> String {
    let trimmed = sig.trim();
    if let Some(rest) = trimmed.strip_prefix("pub")
        && !rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        let after_ws = rest.trim_start();
        if let Some(in_paren) = after_ws.strip_prefix('(') {
            let mut depth = 1usize;
            for (idx, ch) in in_paren.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return in_paren[idx + 1..].trim_start().to_owned();
                        }
                    }
                    _ => {}
                }
            }
        }
        return after_ws.to_owned();
    }
    trimmed.to_owned()
}

fn normalize_sig(raw: &str) -> String {
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    for pat in [" (", " )", " ,", "( ", " ;"] {
        while s.contains(pat) {
            let rep = match pat {
                " (" => "(",
                " )" => ")",
                " ," => ",",
                "( " => "(",
                " ;" => ";",
                _ => pat,
            };
            s = s.replace(pat, rep);
        }
    }
    s
}

/// Scans to the next `{` or `;` at bracket depth 0.
fn find_stmt_end(tokens: &[Token<'_>], start: usize, hi: usize) -> usize {
    let mut i = start;
    while i < hi {
        match tokens[i].kind {
            TokenKind::Punct('(') => i = matching_delim(tokens, i, '(', ')') + 1,
            TokenKind::Punct('[') | TokenKind::Sym("#[") | TokenKind::Sym("#![") => {
                i = matching_delim(tokens, i, '[', ']') + 1;
            }
            TokenKind::Punct('{') | TokenKind::Punct(';') | TokenKind::Punct('}') => return i,
            _ => i += 1,
        }
    }
    hi
}

/// Scans until `;` (or end-of-block `}`), treating `{...}`, `(...)`, `[...]` as balanced.
/// Essential for Rust `use crate::facade::{A, B};` and PHP `use App\Domain\{Order, Item};`.
fn find_semi_stmt_end(tokens: &[Token<'_>], start: usize, hi: usize) -> usize {
    let mut i = start;
    while i < hi {
        match tokens[i].kind {
            TokenKind::Punct('(') => i = matching_delim(tokens, i, '(', ')') + 1,
            TokenKind::Punct('[') | TokenKind::Sym("#[") | TokenKind::Sym("#![") => {
                i = matching_delim(tokens, i, '[', ']') + 1;
            }
            TokenKind::Punct('{') => i = matching_delim(tokens, i, '{', '}') + 1,
            TokenKind::Punct(';') | TokenKind::Punct('}') => return i,
            _ => i += 1,
        }
    }
    hi
}

/// Scans until `;` OR a newline at bracket depth 0 (after optional continuation like `from`).
fn find_import_stmt_end(tokens: &[Token<'_>], start: usize, hi: usize, require_str_for_ts: bool) -> usize {
    let mut i = start;
    let mut saw_from_or_str = !require_str_for_ts;
    let mut last_line = tokens.get(start.saturating_sub(1)).map_or(1, |t| t.line);
    while i < hi {
        let cur_line = tokens[i].line;
        if cur_line > last_line && saw_from_or_str {
            return i;
        }
        last_line = cur_line;
        match tokens[i].kind {
            TokenKind::Punct('(') => {
                let c = matching_delim(tokens, i, '(', ')');
                last_line = tokens[c.min(hi - 1)].line;
                i = c + 1;
            }
            TokenKind::Punct('[') | TokenKind::Sym("#[") | TokenKind::Sym("#![") => {
                let c = matching_delim(tokens, i, '[', ']');
                last_line = tokens[c.min(hi - 1)].line;
                i = c + 1;
            }
            TokenKind::Punct('{') => {
                let c = matching_delim(tokens, i, '{', '}');
                last_line = tokens[c.min(hi - 1)].line;
                i = c + 1;
            }
            TokenKind::Str { .. } => {
                saw_from_or_str = true;
                i += 1;
            }
            TokenKind::Punct(';') | TokenKind::Punct('}') => return i,
            _ => i += 1,
        }
    }
    hi
}

fn is_chain_sep(tok: &Token<'_>) -> bool {
    matches!(
        tok.kind,
        TokenKind::Punct('.' | '\\') | TokenKind::Sym("::" | "->" | "?->" | "?.")
    )
}

/// Walks backwards over a member/path chain (`a::b::c` or `$this->svc`), never crossing
/// whitespace-separated keywords like `return Foo::bar()`.
fn walk_chain_left(tokens: &[Token<'_>], anchor_ident_idx: usize, lo: usize) -> usize {
    let mut left = anchor_ident_idx;
    while left >= lo + 2 && is_chain_sep(&tokens[left - 1]) {
        if matches!(
            tokens[left - 2].kind,
            TokenKind::Ident(_) | TokenKind::DollarIdent(_) | TokenKind::PrivateIdent(_)
        ) {
            left -= 2;
        } else {
            break;
        }
    }
    if left > lo && tokens[left - 1].is_punct('\\') {
        left -= 1;
    }
    left
}

fn find_ts_stmt_end(tokens: &[Token<'_>], start: usize, hi: usize) -> usize {
    let mut i = start;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    while i < hi {
        if paren == 0 && bracket == 0 && brace == 0 && i > start && tokens[i].line > tokens[i - 1].line {
            let prev = &tokens[i - 1];
            let cont_prev = matches!(
                prev.kind,
                TokenKind::Punct('=' | '.' | ',' | ':' | '+' | '-' | '*' | '/' | '?' | '\\')
                    | TokenKind::Sym("=>" | "&&" | "||" | "?." | "::")
            ) || matches!(
                prev.ident(),
                Some("export" | "default" | "const" | "let" | "var" | "import" | "from" | "as" | "return")
            );
            let cont_cur = matches!(
                tokens[i].kind,
                TokenKind::Punct('.' | '?' | ':') | TokenKind::Sym("?." | "=>" | "&&" | "||")
            ) || matches!(tokens[i].ident(), Some("from" | "as" | "extends" | "implements"));
            if !cont_prev && !cont_cur {
                return i;
            }
        }
        match tokens[i].kind {
            TokenKind::Punct('(') => paren += 1,
            TokenKind::Punct(')') => paren = (paren - 1).max(0),
            TokenKind::Punct('[') | TokenKind::Sym("#[") => bracket += 1,
            TokenKind::Punct(']') => bracket = (bracket - 1).max(0),
            TokenKind::Punct('{') => brace += 1,
            TokenKind::Punct('}') => {
                if brace == 0 {
                    return i;
                }
                brace -= 1;
            }
            TokenKind::Punct(';') if paren == 0 && bracket == 0 && brace == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    hi
}

fn push_type_refs(
    ctx: &mut ExtractCtx<'_>,
    raw_type: &str,
    context: &str,
    enclosing: Option<&str>,
    line: u32,
    evidence: &str,
) {
    for part in raw_type.split(['|', '&', ',', '<', '>', '(', ')', '[', ']', '?']) {
        let short = short_type(part);
        if !short.is_empty() && !is_builtin_type(&short) && is_upper_ident(&short) {
            ctx.push_ref(AstRef {
                target_name: part.trim().trim_start_matches('?').to_owned(),
                member: None,
                context: context.to_owned(),
                access: "type".to_owned(),
                member_kind: None,
                owner_type: Some(short),
                enclosing_symbol: enclosing.map(str::to_owned),
                is_static: false,
                line,
                evidence: clip_str(evidence, 160),
            });
        }
    }
}

fn scan_calls_and_refs(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    skip_fn_decls: bool,
) {
    let tokens = &lexed.tokens;
    let hi = hi.min(tokens.len());

    // Pre-collect simple local variable instantiations (`$s = new TokenSigner()`, `let [mut] owned = TokenConfig { ... }`)
    let mut p = lo;
    while p < hi {
        if tokens[p].is_kw("new") && p + 1 < hi && p >= lo + 2 && tokens[p - 1].is_punct('=') {
            let rhs_start = p + 1;
            let mut rhs_end = rhs_start;
            while rhs_end < hi
                && matches!(
                    tokens[rhs_end].kind,
                    TokenKind::Ident(_) | TokenKind::Punct('\\' | '.') | TokenKind::Sym("::")
                )
            {
                rhs_end += 1;
            }
            let cls = short_type(lexed.slice_text(rhs_start, rhs_end));
            if !cls.is_empty() {
                let encl = ctx.enclosing_symbol_at_line(tokens[p].line);
                let scope = encl.as_deref().unwrap_or("");
                match tokens[p - 2].kind {
                    TokenKind::DollarIdent(v) => ctx.record_var_type(scope, v, &cls),
                    TokenKind::Ident(v) => ctx.record_var_type(scope, v, &cls),
                    _ => {}
                }
            }
        } else if ctx.grammar == AstGrammar::Rust
            && tokens[p].is_punct('=')
            && p >= lo + 1
            && p + 2 < hi
            && let Some(lhs_var) = tokens[p - 1].ident()
        {
            let mut j = p + 1;
            while j < hi && matches!(tokens[j].kind, TokenKind::Ident(_) | TokenKind::Sym("::")) {
                j += 1;
            }
            if j > p + 1 && tokens.get(j).is_some_and(|t| t.is_punct('{')) {
                let ty_raw = lexed.slice_text(p + 1, j).replace(' ', "");
                let short = short_type(&ty_raw);
                if is_upper_ident(&short) {
                    let encl = ctx.enclosing_symbol_at_line(tokens[p].line);
                    ctx.record_var_type(encl.as_deref().unwrap_or(""), lhs_var, &short);
                }
            }
        }
        p += 1;
    }

    let mut i = lo;
    while i < hi {
        // Rust struct literal `TypeName { field: value, ... }` -> `write_init` member refs
        if ctx.grammar == AstGrammar::Rust
            && tokens[i].is_punct('{')
            && i > lo
            && let Some(ty_tok) = tokens[i - 1].ident()
            && is_upper_ident(ty_tok)
            && !tokens.get(i.saturating_sub(2)).is_some_and(|t| {
                matches!(t.ident(), Some("struct" | "enum" | "trait" | "union" | "impl" | "match"))
            })
        {
            let left = walk_chain_left(tokens, i - 1, lo);
            let not_decl_header = !tokens.get(left.saturating_sub(1)).is_some_and(|t| {
                matches!(t.ident(), Some("struct" | "enum" | "trait" | "union" | "impl" | "fn" | "for"))
            });
            if not_decl_header {
                let raw_ty = lexed.slice_text(left, i).replace(' ', "");
                let short = short_type(&raw_ty);
                let close = matching_delim(tokens, i, '{', '}').min(hi.saturating_sub(1));
                let line = tokens[left].line;
                let encl = ctx.enclosing_symbol_at_line(line);
                let ev = clip_str(lexed.slice_text(left, close + 1), 160);
                for (f_lo, f_hi) in split_top_level(tokens, i + 1, close, ',') {
                    if let Some(fname) = tokens.get(f_lo).and_then(|t| t.ident())
                        && (f_lo + 1 == f_hi
                            || tokens.get(f_lo + 1).is_some_and(|t| t.is_punct(':')))
                    {
                        ctx.push_ref(AstRef {
                            target_name: raw_ty.clone(),
                            member: Some(fname.to_owned()),
                            context: "field".to_owned(),
                            access: "write_init".to_owned(),
                            member_kind: Some("field".to_owned()),
                            owner_type: Some(short.clone()),
                            enclosing_symbol: encl.clone(),
                            is_static: false,
                            line: tokens[f_lo].line,
                            evidence: ev.clone(),
                        });
                    }
                }
            }
        }
        // Instantiations: `new ClassName(...)`
        if tokens[i].is_kw("new") && i + 1 < hi {
            let mut j = i + 1;
            while j < hi
                && matches!(
                    tokens[j].kind,
                    TokenKind::Ident(_) | TokenKind::Punct('\\' | '.') | TokenKind::Sym("::")
                )
            {
                j += 1;
            }
            if j > i + 1 {
                let raw_ty = lexed.slice_text(i + 1, j).replace(' ', "");
                let short = short_type(&raw_ty);
                if !short.is_empty() && !is_builtin_type(&short) {
                    let line = tokens[i].line;
                    let encl = ctx.enclosing_symbol_at_line(line);
                    ctx.push_ref(AstRef {
                        target_name: raw_ty,
                        member: None,
                        context: "constructor".to_owned(),
                        access: "call".to_owned(),
                        member_kind: None,
                        owner_type: Some(short),
                        enclosing_symbol: encl,
                        is_static: false,
                        line,
                        evidence: clip_str(lexed.slice_text(i, (j + 3).min(hi)), 160),
                    });
                }
                i = j;
                continue;
            }
        }

        // `Class::class`, `Class::CONST`, `Class::$static_prop` references
        if tokens[i].is_sym("::") && i > lo && i + 1 < hi {
            let mut left = i - 1;
            if matches!(tokens[left].kind, TokenKind::Ident(_) | TokenKind::DollarIdent(_)) {
                left = walk_chain_left(tokens, left, lo);
            } else {
                left = i;
            }
            if !tokens.get(i + 2).is_some_and(|t| t.is_punct('(')) {
                let lhs = lexed.slice_text(left, i).replace(' ', "");
                let short = short_type(&lhs);
                let line = tokens[i].line;
                let encl = ctx.enclosing_symbol_at_line(line);
                match tokens[i + 1].kind {
                    TokenKind::Ident(mem) if mem == "class" => {
                        ctx.push_ref(AstRef {
                            target_name: lhs,
                            member: None,
                            context: "class_ref".to_owned(),
                            access: "type".to_owned(),
                            member_kind: None,
                            owner_type: (!short.is_empty()).then_some(short),
                            enclosing_symbol: encl,
                            is_static: true,
                            line,
                            evidence: clip_str(lexed.slice_text(left, i + 2), 160),
                        });
                    }
                    TokenKind::Ident(mem) if is_upper_ident(&short) && super::is_upper_snake(mem) => {
                        ctx.push_ref(AstRef {
                            target_name: lhs,
                            member: Some(mem.to_owned()),
                            context: "field".to_owned(),
                            access: "const".to_owned(),
                            member_kind: Some("const".to_owned()),
                            owner_type: Some(short),
                            enclosing_symbol: encl,
                            is_static: true,
                            line,
                            evidence: clip_str(lexed.slice_text(left, i + 2), 160),
                        });
                    }
                    TokenKind::DollarIdent(mem) => {
                        let is_write = tokens.get(i + 2).is_some_and(|t| t.is_punct('='));
                        ctx.push_ref(AstRef {
                            target_name: lhs,
                            member: Some(mem.to_owned()),
                            context: "field".to_owned(),
                            access: if is_write { "static_write" } else { "static_read" }.to_owned(),
                            member_kind: Some("property".to_owned()),
                            owner_type: (!short.is_empty()).then_some(short),
                            enclosing_symbol: encl,
                            is_static: true,
                            line,
                            evidence: clip_str(lexed.slice_text(left, i + 2), 160),
                        });
                    }
                    _ => {}
                }
            }
        }

        // Skip function/method declaration signatures so `fn foo(...)` isn't re-emitted as a call
        if skip_fn_decls
            && matches!(
                tokens[i].kind,
                TokenKind::Ident("fn" | "function" | "def" | "func")
            )
            && i + 1 < hi
            && tokens[i + 1].ident().is_some()
            && let Some(lp) = (i + 2..hi).find(|&k| tokens[k].is_punct('(') || tokens[k].is_punct('{'))
            && tokens[lp].is_punct('(')
        {
            i = matching_delim(tokens, lp, '(', ')') + 1;
            continue;
        }

        // Calls: `callee(...)`, `recv.method(...)`, `recv->method(...)`, `scope::callee(...)`
        if tokens[i].is_punct('(') && i > lo {
            let close = matching_delim(tokens, i, '(', ')').min(hi.saturating_sub(1));
            let args_text = lexed.slice_text(i + 1, close);
            let str_args = collect_string_args(tokens, i + 1, close);
            let line = tokens[i].line;
            let encl = ctx.enclosing_symbol_at_line(line);
            let cur_owner = encl
                .as_deref()
                .and_then(|s| s.split_once("::").map(|(o, _)| o.to_owned()));

            let mut chain_end = close + 1;
            while chain_end < hi {
                let is_hop = matches!(
                    tokens[chain_end].kind,
                    TokenKind::Punct('.') | TokenKind::Sym("->" | "?->" | "?.")
                );
                if !is_hop || chain_end + 1 >= hi {
                    break;
                }
                let next_idx = chain_end + 1;
                if tokens[next_idx].ident().is_none() {
                    break;
                }
                if tokens.get(next_idx + 1).is_some_and(|t| t.is_punct('(')) {
                    let next_close = matching_delim(tokens, next_idx + 1, '(', ')').min(hi - 1);
                    chain_end = next_close + 1;
                } else {
                    chain_end = next_idx + 1;
                }
            }

            if let Some(callee) = tokens[i - 1].ident()
                && !matches!(
                    callee,
                    "if" | "for" | "while" | "match" | "switch" | "catch" | "return" | "typeof" | "sizeof" | "fn"
                )
            {
                let left = walk_chain_left(tokens, i - 1, lo);
                let chain_text = lexed.slice_text(left, chain_end.min(hi));

                // TS/JS dynamic `import('./lazy')` & `require('./lazy')`
                if matches!(
                    ctx.grammar,
                    AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript
                ) && matches!(callee, "import" | "require")
                    && let Some(spec) = str_args.first()
                {
                    ctx.push_import(AstImport {
                        path: spec.clone(),
                        symbols: Vec::new(),
                        alias: None,
                        is_wildcard: false,
                        is_reexport: false,
                        reexport_visibility: None,
                        kind: "import".to_owned(),
                        line,
                        evidence: clip_str(chain_text, 160),
                    });
                }

                // PHP `view('orders.show')` -> blade-view include
                if ctx.grammar == AstGrammar::Php
                    && callee == "view"
                    && let Some(v) = str_args.first()
                {
                    ctx.push_import(AstImport {
                        path: format!("blade-view:{v}"),
                        symbols: vec![v.clone()],
                        alias: None,
                        is_wildcard: false,
                        is_reexport: false,
                        reexport_visibility: None,
                        kind: "include".to_owned(),
                        line,
                        evidence: clip_str(chain_text, 160),
                    });
                }

                if i >= lo + 3 && tokens[i - 2].is_sym("::") {
                    let scope_raw = lexed.slice_text(left, i - 2).replace(' ', "");
                    let short = short_type(&scope_raw);
                    let is_type = is_upper_ident(&short);
                    emit_semantic_from_call(
                        ctx,
                        Some(&scope_raw),
                        callee,
                        args_text,
                        &str_args,
                        chain_text,
                        encl.clone(),
                        line,
                    );
                    let target_name = if ctx.grammar == AstGrammar::Php
                        || (is_type && !scope_raw.contains("::"))
                    {
                        scope_raw.clone()
                    } else {
                        format!("{scope_raw}::{callee}")
                    };
                    ctx.push_ref(AstRef {
                        target_name,
                        member: Some(callee.to_owned()),
                        context: "call".to_owned(),
                        access: "call".to_owned(),
                        member_kind: Some(if is_type { "method" } else { "function" }.to_owned()),
                        owner_type: is_type.then_some(short),
                        enclosing_symbol: encl,
                        is_static: true,
                        line,
                        evidence: clip_str(chain_text, 160),
                    });
                } else if i >= lo + 3
                    && matches!(
                        tokens[i - 2].kind,
                        TokenKind::Punct('.') | TokenKind::Sym("->" | "?->" | "?.")
                    )
                {
                    let recv_slice = lexed.slice_text(left, i - 2).trim();
                    let recv_tok = &tokens[i - 3];
                    let (recv_leaf, is_dollar) = match recv_tok.kind {
                        TokenKind::DollarIdent(d) => (d.trim_start_matches('$'), true),
                        TokenKind::Ident(s) => (s, false),
                        TokenKind::PrivateIdent(p) => (p, false),
                        _ => (recv_slice, false),
                    };
                    let is_this_prop = i >= lo + 5
                        && matches!(tokens[i - 4].kind, TokenKind::Sym("->") | TokenKind::Punct('.'))
                        && matches!(
                            tokens[i - 5].kind,
                            TokenKind::DollarIdent("$this") | TokenKind::Ident("this" | "self")
                        );
                    let owner_ty = if matches!(recv_leaf, "this" | "self" | "cls" | "static")
                        || matches!(recv_tok.kind, TokenKind::DollarIdent("$this"))
                    {
                        cur_owner.clone()
                    } else if is_this_prop {
                        ctx.lookup_var_type(cur_owner.as_deref(), recv_leaf)
                    } else {
                        ctx.lookup_var_type(encl.as_deref(), recv_leaf)
                    };
                    emit_semantic_from_call(
                        ctx,
                        Some(recv_slice),
                        callee,
                        args_text,
                        &str_args,
                        chain_text,
                        encl.clone(),
                        line,
                    );
                    if let Some(ref ty) = owner_ty {
                        let short = short_type(ty);
                        ctx.push_ref(AstRef {
                            target_name: if is_dollar {
                                format!("${recv_leaf}")
                            } else {
                                ty.clone()
                            },
                            member: Some(callee.to_owned()),
                            context: "call".to_owned(),
                            access: "call".to_owned(),
                            member_kind: Some("method".to_owned()),
                            owner_type: Some(short),
                            enclosing_symbol: encl,
                            is_static: false,
                            line,
                            evidence: clip_str(chain_text, 160),
                        });
                    } else {
                        let recv_is_ty = is_upper_ident(recv_leaf);
                        ctx.push_ref(AstRef {
                            target_name: if recv_is_ty {
                                recv_slice.to_owned()
                            } else if is_dollar {
                                format!("${recv_leaf}")
                            } else {
                                recv_leaf.to_owned()
                            },
                            member: Some(callee.to_owned()),
                            context: "call".to_owned(),
                            access: "call".to_owned(),
                            member_kind: Some("method".to_owned()),
                            owner_type: recv_is_ty.then(|| short_type(recv_leaf)),
                            enclosing_symbol: encl,
                            is_static: false,
                            line,
                            evidence: clip_str(chain_text, 160),
                        });
                    }
                } else {
                    let full_callee = lexed.slice_text(left, i).replace(' ', "");
                    let clean_callee = full_callee.trim_start_matches('\\');
                    emit_semantic_from_call(
                        ctx,
                        None,
                        callee,
                        args_text,
                        &str_args,
                        chain_text,
                        encl.clone(),
                        line,
                    );
                    ctx.push_ref(AstRef {
                        target_name: clean_callee.to_owned(),
                        member: None,
                        context: "call".to_owned(),
                        access: "call".to_owned(),
                        member_kind: Some("function".to_owned()),
                        owner_type: None,
                        enclosing_symbol: encl,
                        is_static: false,
                        line,
                        evidence: clip_str(chain_text, 160),
                    });
                }

                // Route prefix calls (`Route::group`, `router.route`)
                let callee_l = callee.to_ascii_lowercase();
                if matches!(callee_l.as_str(), "group" | "prefix" | "route")
                    && let Some(pref) = str_args.first()
                {
                    let mark = ctx.push_call_route_prefix(pref);
                    scan_calls_and_refs(ctx, lexed, i + 1, close, skip_fn_decls);
                    ctx.truncate_call_route_prefixes(mark);
                    i = close + 1;
                    continue;
                }
            }
        }

        // Field read/write: `$obj->prop`, `$this->prop`, `$this->config->ttl`, `this.prop`, `obj.prop`
        if matches!(tokens[i].kind, TokenKind::Sym("->") | TokenKind::Punct('.'))
            && i > lo
            && i + 1 < hi
            && let Some(mem) = tokens[i + 1].ident()
            && !tokens.get(i + 2).is_some_and(|t| t.is_punct('('))
        {
            let line = tokens[i].line;
            let is_import_line = tokens[lo..=i]
                .iter()
                .rev()
                .take_while(|t| t.line == line)
                .last()
                .is_some_and(|first| {
                    matches!(
                        first.ident(),
                        Some("import" | "from" | "use" | "package" | "namespace")
                    )
                });
            if is_import_line {
                i += 1;
                continue;
            }
            let encl = ctx.enclosing_symbol_at_line(line);
            let cur_owner = encl
                .as_deref()
                .and_then(|s| s.split_once("::").map(|(o, _)| o.to_owned()));
            let is_dollar_lang = ctx.grammar == AstGrammar::Php;
            let is_dollar = matches!(tokens[i - 1].kind, TokenKind::DollarIdent(_));
            let recv_name = match tokens[i - 1].kind {
                TokenKind::DollarIdent(d) => d.trim_start_matches('$'),
                TokenKind::Ident(s) | TokenKind::PrivateIdent(s) => s,
                _ => "",
            };
            let is_this = matches!(
                tokens[i - 1].kind,
                TokenKind::DollarIdent("$this") | TokenKind::Ident("this" | "self")
            );
            let is_this_prop = i >= lo + 3
                && matches!(tokens[i - 2].kind, TokenKind::Sym("->") | TokenKind::Punct('.'))
                && matches!(
                    tokens[i - 3].kind,
                    TokenKind::DollarIdent("$this") | TokenKind::Ident("this" | "self")
                );
            let resolved_owner = if is_this {
                cur_owner
            } else if is_this_prop {
                ctx.lookup_var_type(cur_owner.as_deref(), recv_name)
            } else if !recv_name.is_empty() {
                ctx.lookup_var_type(encl.as_deref(), recv_name)
            } else {
                None
            };
            let is_write = tokens.get(i + 2).is_some_and(|t| t.is_punct('='));
            if let Some(owner_ty) = resolved_owner {
                let target_name = if is_this || is_this_prop {
                    owner_ty.clone()
                } else if is_dollar {
                    format!("${recv_name}")
                } else {
                    owner_ty.clone()
                };
                let mk = if matches!(
                    ctx.grammar,
                    AstGrammar::Rust | AstGrammar::Go | AstGrammar::Java | AstGrammar::CSharp
                ) {
                    "field"
                } else {
                    "property"
                };
                ctx.push_ref(AstRef {
                    target_name,
                    member: Some(if is_dollar_lang {
                        format!("${mem}")
                    } else {
                        mem.to_owned()
                    }),
                    context: "field".to_owned(),
                    access: if is_write { "write" } else { "read" }.to_owned(),
                    member_kind: Some(mk.to_owned()),
                    owner_type: Some(owner_ty),
                    enclosing_symbol: encl,
                    is_static: false,
                    line,
                    evidence: clip_str(lexed.slice_text(i - 1, i + 2), 160),
                });
            } else if matches!(
                ctx.grammar,
                AstGrammar::TypeScript
                    | AstGrammar::Tsx
                    | AstGrammar::JavaScript
                    | AstGrammar::Python
            ) && !is_this
                && !is_this_prop
                && !recv_name.is_empty()
                && !matches!(
                    recv_name,
                    "window" | "document" | "console" | "Math" | "Object" | "Array" | "JSON" | "cls"
                )
            {
                let is_ty = is_upper_ident(recv_name);
                if super::is_upper_snake(mem) && !is_write {
                    ctx.push_ref(AstRef {
                        target_name: recv_name.to_owned(),
                        member: Some(mem.to_owned()),
                        context: "field".to_owned(),
                        access: "const".to_owned(),
                        member_kind: Some("const".to_owned()),
                        owner_type: is_ty.then(|| recv_name.to_owned()),
                        enclosing_symbol: encl,
                        is_static: is_ty,
                        line,
                        evidence: clip_str(lexed.slice_text(i - 1, i + 2), 160),
                    });
                } else {
                    ctx.push_ref(AstRef {
                        target_name: recv_name.to_owned(),
                        member: Some(mem.to_owned()),
                        context: "field".to_owned(),
                        access: if is_write { "write" } else { "read" }.to_owned(),
                        member_kind: Some("property".to_owned()),
                        owner_type: is_ty.then(|| recv_name.to_owned()),
                        enclosing_symbol: encl,
                        is_static: is_ty,
                        line,
                        evidence: clip_str(lexed.slice_text(i - 1, i + 2), 160),
                    });
                }
            }
        }

        i += 1;
    }
}

// =========================================================================
// 1. PHP & Blade
// =========================================================================

fn parse_php_qualifiers(
    tokens: &[Token<'_>],
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
) -> (Option<String>, Vec<String>, usize) {
    let mut vis: Option<String> = None;
    let mut mods: Vec<String> = Vec::new();
    let mut i = lo;
    while i < hi {
        match tokens[i].ident() {
            Some(kw @ ("public" | "protected" | "private")) => {
                if tokens.get(i + 1).is_some_and(|t| t.is_punct('(')) {
                    let rp = matching_delim(tokens, i + 1, '(', ')').min(hi.saturating_sub(1));
                    let suffix = lexed.slice_text(i + 1, rp + 1).replace(' ', "");
                    vis = Some(format!("{kw}{suffix}"));
                    i = rp + 1;
                } else {
                    vis = Some(kw.to_owned());
                    i += 1;
                }
            }
            Some("var") => {
                if vis.is_none() {
                    vis = Some("public".to_owned());
                }
                i += 1;
            }
            Some(m @ ("static" | "abstract" | "final" | "readonly")) => {
                let ms = m.to_owned();
                if !mods.contains(&ms) {
                    mods.push(ms);
                }
                i += 1;
            }
            _ => break,
        }
    }
    (vis, mods, i)
}

fn collect_php_attributes(
    tokens: &[Token<'_>],
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
) -> (Vec<(String, String, Vec<String>, u32)>, usize) {
    let mut attrs = Vec::new();
    let mut i = lo;
    while i < hi && tokens[i].is_sym("#[") {
        let close = matching_delim(tokens, i, '[', ']').min(hi.saturating_sub(1));
        for (a_lo, a_hi) in split_top_level(tokens, i + 1, close, ',') {
            let text = lexed.slice_text(a_lo, a_hi).trim();
            if !text.is_empty() {
                let name = text.split('(').next().unwrap_or(text).trim().to_owned();
                let str_args = collect_string_args(tokens, a_lo, a_hi);
                attrs.push((name, text.to_owned(), str_args, tokens[a_lo].line));
            }
        }
        i = close + 1;
    }
    (attrs, i)
}

fn extract_php(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    walk_php_block(ctx, lexed, 0, lexed.tokens.len(), None, 0);
    scan_calls_and_refs(ctx, lexed, 0, lexed.tokens.len(), true);
}

fn walk_php_block(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    cur_class: Option<&str>,
    fn_depth: usize,
) {
    let tokens = &lexed.tokens;
    let mut i = lo;
    while i < hi {
        let (attrs, stmt_lo) = collect_php_attributes(tokens, lexed, i, hi);
        if stmt_lo >= hi {
            break;
        }
        if tokens[stmt_lo].is_punct('{') {
            let close = matching_delim(tokens, stmt_lo, '{', '}').min(hi.saturating_sub(1));
            walk_php_block(ctx, lexed, stmt_lo + 1, close, cur_class, fn_depth);
            i = close + 1;
            continue;
        }
        if tokens[stmt_lo].is_punct(';') || tokens[stmt_lo].is_punct('}') {
            i = stmt_lo + 1;
            continue;
        }

        // Namespace
        if tokens[stmt_lo].is_kw("namespace") && cur_class.is_none() {
            let end = find_stmt_end(tokens, stmt_lo + 1, hi);
            let ns = lexed
                .slice_text(stmt_lo + 1, end)
                .replace(' ', "")
                .trim_matches('\\')
                .to_owned();
            if !ns.is_empty() {
                ctx.namespace = Some(ns);
            }
            if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
                let close = matching_delim(tokens, end, '{', '}').min(hi.saturating_sub(1));
                walk_php_block(ctx, lexed, end + 1, close, None, fn_depth);
                i = close + 1;
            } else {
                i = (end + 1).min(hi);
            }
            continue;
        }

        // `use ...;` (supports group uses `use App\Domain\{Order, Item as O};`!)
        if tokens[stmt_lo].is_kw("use") {
            let end = find_semi_stmt_end(tokens, stmt_lo + 1, hi);
            let line = tokens[stmt_lo].line;
            let raw = lexed.slice_text(stmt_lo + 1, end).trim();
            if let Some(_owner) = cur_class {
                for seg in raw.split(',') {
                    let full_tr = seg.trim().trim_start_matches('\\').to_owned();
                    let short = full_tr.rsplit('\\').next().unwrap_or(&full_tr).trim().to_owned();
                    if !short.is_empty() {
                        ctx.push_import(AstImport {
                            path: full_tr,
                            symbols: vec![short],
                            alias: None,
                            is_wildcard: false,
                            is_reexport: false,
                            reexport_visibility: None,
                            kind: "trait_use".to_owned(),
                            line,
                            evidence: clip_str(lexed.slice_text(stmt_lo, end), 160),
                        });
                    }
                }
            } else {
                parse_php_use(ctx, raw, line, lexed.slice_text(stmt_lo, (end + 1).min(hi)));
            }
            i = (end + 1).min(hi);
            continue;
        }

        // `require` / `include`
        if matches!(
            tokens[stmt_lo].ident(),
            Some("require" | "require_once" | "include" | "include_once")
        ) {
            let end = find_semi_stmt_end(tokens, stmt_lo + 1, hi);
            let strs = collect_string_args(tokens, stmt_lo + 1, end);
            if let Some(path) = strs.first() {
                ctx.push_import(AstImport {
                    path: path.clone(),
                    symbols: Vec::new(),
                    alias: None,
                    is_wildcard: false,
                    is_reexport: false,
                    reexport_visibility: None,
                    kind: "include".to_owned(),
                    line: tokens[stmt_lo].line,
                    evidence: clip_str(lexed.slice_text(stmt_lo, (end + 1).min(hi)), 160),
                });
            }
            i = (end + 1).min(hi);
            continue;
        }

        let (vis, mods, kw_idx) = parse_php_qualifiers(tokens, lexed, stmt_lo, hi);
        let vis_final = vis.clone().or_else(|| Some("public".to_owned()));

        // Type declaration: class / interface / trait / enum
        if kw_idx < hi
            && let Some(decl_kw @ ("class" | "interface" | "trait" | "enum")) = tokens[kw_idx].ident()
            && kw_idx + 1 < hi
            && let Some(name) = tokens[kw_idx + 1].ident()
        {
            let body_open = (kw_idx + 2..hi).find(|&k| tokens[k].is_punct('{')).unwrap_or(hi);
            let body_close = if body_open < hi {
                matching_delim(tokens, body_open, '{', '}').min(hi.saturating_sub(1))
            } else {
                hi.saturating_sub(1)
            };
            let line = tokens[kw_idx + 1].line;
            let e_line = tokens.get(body_close).map_or(line, |t| t.line);
            let sig_end = tokens.get(body_open).map_or(tokens[kw_idx + 1].end, |t| t.start);
            let sig = normalize_sig(&lexed.header_without_comments(tokens[stmt_lo].start, sig_end));
            let params_sig = strip_php_vis_from_sig(&sig);

            let mut route_pref: Option<String> = None;
            for (aname, atext, sargs, aline) in &attrs {
                let short_a = aname.rsplit(['\\', ':', '.']).next().unwrap_or(aname);
                if short_a.eq_ignore_ascii_case("Route")
                    && let Some(p) = sargs.first()
                {
                    route_pref = Some(p.clone());
                }
                emit_semantic_from_attribute(ctx, aname, atext, sargs, Some(name), &[], *aline);
            }

            // Containers have empty body_fingerprint so child changes do not mark the class modified.
            ctx.push_decl(
                decl_kw,
                name,
                None,
                line,
                e_line,
                vis_final,
                mods,
                sig,
                params_sig,
                String::new(),
            );

            let mut h = kw_idx + 2;
            while h < body_open {
                if tokens[h].is_kw("extends") || tokens[h].is_kw("implements") {
                    let rel = if tokens[h].is_kw("extends") {
                        "extends"
                    } else {
                        "implements"
                    };
                    let list_start = h + 1;
                    let mut list_end = list_start;
                    while list_end < body_open
                        && !tokens[list_end].is_kw("extends")
                        && !tokens[list_end].is_kw("implements")
                    {
                        list_end += 1;
                    }
                    let list_txt = lexed.slice_text(list_start, list_end);
                    for parent_raw in list_txt.split(',') {
                        let clean = short_type(parent_raw);
                        if !clean.is_empty() {
                            ctx.push_inheritance(name, &clean, rel, line, list_txt);
                        }
                    }
                    h = list_end;
                } else {
                    h += 1;
                }
            }

            let mark = route_pref.as_deref().map(|p| ctx.push_attr_route_prefix(p));
            if body_open < body_close {
                walk_php_block(ctx, lexed, body_open + 1, body_close, Some(name), fn_depth);
            }
            if let Some(m) = mark {
                ctx.truncate_attr_route_prefixes(m);
            }
            i = (body_close + 1).min(hi);
            continue;
        }

        // Function or method declaration
        if kw_idx < hi && tokens[kw_idx].is_kw("function") && fn_depth == 0 {
            let mut name_idx = kw_idx + 1;
            if tokens.get(name_idx).is_some_and(|t| t.is_punct('&')) {
                name_idx += 1;
            }
            if let Some(name) = tokens.get(name_idx).and_then(|t| t.ident())
                && let Some(lp) = (name_idx + 1..hi).find(|&k| tokens[k].is_punct('('))
            {
                let rp = matching_delim(tokens, lp, '(', ')').min(hi.saturating_sub(1));
                let mut scan = rp + 1;
                let mut body_open = None;
                let mut stmt_semi = None;
                while scan < hi {
                    if tokens[scan].is_punct('{') {
                        body_open = Some(scan);
                        break;
                    }
                    if tokens[scan].is_punct(';') {
                        stmt_semi = Some(scan);
                        break;
                    }
                    scan += 1;
                }
                let body_close = body_open
                    .map(|bo| matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1)))
                    .or(stmt_semi)
                    .unwrap_or(rp);
                let line = tokens[kw_idx].line;
                let e_line = tokens[body_close].line;
                let sig_end = body_open
                    .map(|bo| tokens[bo].start)
                    .unwrap_or(tokens[body_close].end);
                let sig = normalize_sig(
                    lexed
                        .header_without_comments(tokens[stmt_lo].start, sig_end)
                        .trim_end_matches(';'),
                );
                let params_sig = strip_php_vis_from_sig(&sig);
                let body_fp = if let Some(bo) = body_open {
                    lexed.body_fingerprint(tokens[bo].start, tokens[body_close].end)
                } else {
                    String::new()
                };
                let is_method = cur_class.is_some();
                let decl_kind = if is_method { "method" } else { "function" };
                let sym_label = if let Some(owner) = cur_class {
                    format!("{owner}::{name}")
                } else {
                    name.to_owned()
                };

                let mut param_types = Vec::new();
                for (p_lo, p_hi) in split_top_level(tokens, lp + 1, rp, ',') {
                    let (p_vis, p_mods, after_p_vis) = parse_php_qualifiers(tokens, lexed, p_lo, p_hi);
                    if let Some(var_pos) =
                        (after_p_vis..p_hi).find(|&k| matches!(tokens[k].kind, TokenKind::DollarIdent(_)))
                    {
                        let var_name = tokens[var_pos].text(lexed.source).to_owned();
                        let raw_ty = lexed
                            .slice_text(after_p_vis, var_pos)
                            .replace(' ', "")
                            .trim_matches(['&', '.'])
                            .to_owned();
                        if !raw_ty.is_empty() {
                            param_types.push(raw_ty.clone());
                            let short = short_type(&raw_ty);
                            ctx.record_var_type(&sym_label, &var_name, &short);
                            push_type_refs(
                                ctx,
                                &raw_ty,
                                "parameter_type",
                                Some(&sym_label),
                                tokens[var_pos].line,
                                lexed.slice_text(p_lo, p_hi),
                            );
                        }
                        if (p_vis.is_some() || !p_mods.is_empty())
                            && name == "__construct"
                            && let Some(owner) = cur_class
                        {
                            let p_line = tokens[p_lo].line;
                            let p_sig = normalize_sig(lexed.slice_text(p_lo, p_hi));
                            let p_params = strip_php_vis_from_sig(&p_sig);
                            if !raw_ty.is_empty() {
                                ctx.record_var_type(owner, &var_name, &short_type(&raw_ty));
                            }
                            ctx.push_decl(
                                "property",
                                &var_name,
                                Some(owner.to_owned()),
                                p_line,
                                p_line,
                                p_vis.or_else(|| Some("public".to_owned())),
                                p_mods,
                                p_sig,
                                p_params,
                                String::new(),
                            );
                        }
                    }
                }

                if rp + 1 < body_open.unwrap_or(stmt_semi.unwrap_or(rp + 1)) {
                    let ret_txt = lexed
                        .slice_text(rp + 1, body_open.unwrap_or(stmt_semi.unwrap_or(rp + 1)))
                        .trim_start_matches(':')
                        .trim();
                    if !ret_txt.is_empty() {
                        push_type_refs(ctx, ret_txt, "return_type", Some(&sym_label), line, &sig);
                    }
                }

                for (aname, atext, sargs, aline) in &attrs {
                    emit_semantic_from_attribute(
                        ctx,
                        aname,
                        atext,
                        sargs,
                        Some(&sym_label),
                        &param_types,
                        *aline,
                    );
                }

                if matches!(name, "handle" | "__invoke")
                    && let Some(first_ty) = param_types.first()
                {
                    let short = short_type(first_ty);
                    if !is_builtin_type(&short)
                        && (short.ends_with("Event")
                            || short.ends_with("Message")
                            || short.ends_with("Job")
                            || short.ends_with("Notification"))
                    {
                        ctx.push_semantic(
                            "handles_event",
                            format!("event:{short}"),
                            Some(sym_label.clone()),
                            line,
                            &sig,
                        );
                    }
                }

                ctx.push_decl(
                    decl_kind,
                    name,
                    cur_class.map(str::to_owned),
                    line,
                    e_line,
                    vis_final,
                    mods,
                    sig,
                    params_sig,
                    body_fp,
                );

                if let Some(bo) = body_open {
                    walk_php_block(ctx, lexed, bo + 1, body_close, cur_class, fn_depth + 1);
                }
                i = (body_close + 1).min(hi);
                continue;
            }
        }

        // Const declaration
        if kw_idx < hi && tokens[kw_idx].is_kw("const") && fn_depth == 0 {
            let end = find_semi_stmt_end(tokens, kw_idx + 1, hi);
            for (c_lo, c_hi) in split_top_level(tokens, kw_idx + 1, end, ',') {
                if let Some(name) = tokens.get(c_lo).and_then(|t| t.ident()) {
                    let line = tokens[c_lo].line;
                    let eq_pos = (c_lo + 1..c_hi).find(|&k| tokens[k].is_punct('='));
                    let sig_end = eq_pos.map_or(tokens[c_hi - 1].end, |k| tokens[k].start);
                    let sig = normalize_sig(
                        &lexed.header_without_comments(tokens[stmt_lo].start, sig_end),
                    );
                    let params_sig = strip_php_vis_from_sig(&sig);
                    let body_fp = eq_pos
                        .map(|eq| lexed.body_fingerprint(tokens[eq + 1].start, tokens[c_hi - 1].end))
                        .unwrap_or_default();
                    ctx.push_decl(
                        "const",
                        name,
                        cur_class.map(str::to_owned),
                        line,
                        tokens[c_hi - 1].line,
                        vis_final.clone(),
                        mods.clone(),
                        sig,
                        params_sig,
                        body_fp,
                    );
                }
            }
            i = (end + 1).min(hi);
            continue;
        }

        // Class property declaration (`public ?UserRepo $repo = ...;`, `public private(set) int $balance = 0;`)
        if let Some(owner) = cur_class
            && fn_depth == 0
            && (vis.is_some() || !mods.is_empty())
        {
            let end = find_semi_stmt_end(tokens, kw_idx, hi);
            if let Some(first_var) =
                (kw_idx..end).find(|&k| matches!(tokens[k].kind, TokenKind::DollarIdent(_)))
            {
                let raw_ty = lexed.slice_text(kw_idx, first_var).replace(' ', "");
                for (p_lo, p_hi) in split_top_level(tokens, first_var, end, ',') {
                    if let Some(vp) =
                        (p_lo..p_hi).find(|&k| matches!(tokens[k].kind, TokenKind::DollarIdent(_)))
                    {
                        let prop_name = tokens[vp].text(lexed.source).to_owned();
                        let line = tokens[vp].line;
                        let eq_pos = (vp + 1..p_hi).find(|&k| tokens[k].is_punct('='));
                        let sig_end = eq_pos.map_or(tokens[p_hi - 1].end, |k| tokens[k].start);
                        let sig = normalize_sig(
                            &lexed.header_without_comments(tokens[stmt_lo].start, sig_end),
                        );
                        let params_sig = strip_php_vis_from_sig(&sig);
                        let body_fp = eq_pos
                            .map(|eq| {
                                lexed.body_fingerprint(tokens[eq + 1].start, tokens[p_hi - 1].end)
                            })
                            .unwrap_or_default();
                        if !raw_ty.is_empty() {
                            ctx.record_var_type(owner, &prop_name, &short_type(&raw_ty));
                            push_type_refs(ctx, &raw_ty, "field", Some(owner), line, &sig);
                        }
                        if prop_name == "$table"
                            && let Some(eq) = eq_pos
                            && let Some(TokenKind::Str { unquoted, .. }) =
                                tokens.get(eq + 1).map(|t| &t.kind)
                            && let Some(tbl) = super::semantics::clean_sql_table_name(unquoted)
                        {
                            ctx.push_semantic(
                                "reads_table",
                                format!("table:{tbl}"),
                                Some(owner.to_owned()),
                                line,
                                &sig,
                            );
                        }
                        ctx.push_decl(
                            "property",
                            &prop_name,
                            Some(owner.to_owned()),
                            line,
                            tokens[p_hi - 1].line,
                            vis_final.clone(),
                            mods.clone(),
                            sig,
                            params_sig,
                            body_fp,
                        );
                    }
                }
                i = (end + 1).min(hi);
                continue;
            }
        }

        let end = find_stmt_end(tokens, stmt_lo + 1, hi);
        if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
            let close = matching_delim(tokens, end, '{', '}').min(hi.saturating_sub(1));
            walk_php_block(ctx, lexed, end + 1, close, cur_class, fn_depth);
            i = close + 1;
        } else {
            i = (end + 1).min(hi);
        }
    }
}

fn parse_php_use(ctx: &mut ExtractCtx<'_>, raw: &str, line: u32, evidence: &str) {
    let trimmed = raw
        .trim()
        .trim_start_matches("function ")
        .trim_start_matches("const ")
        .trim();
    if let (Some(lb), Some(rb)) = (trimmed.find('{'), trimmed.rfind('}'))
        && lb < rb
    {
        let prefix = trimmed[..lb].replace(' ', "").trim_end_matches('\\').to_owned();
        for item in trimmed[lb + 1..rb].split(',') {
            let part = item.trim();
            if part.is_empty() {
                continue;
            }
            let (target, alias) = split_as_alias(part);
            let clean_target = target.replace(' ', "").trim_start_matches('\\').to_owned();
            let full = if prefix.is_empty() {
                clean_target
            } else {
                format!("{prefix}\\{clean_target}")
            };
            let orig_sym = full.rsplit('\\').next().unwrap_or(&full).to_owned();
            ctx.push_import(AstImport {
                path: full,
                symbols: vec![orig_sym],
                alias,
                is_wildcard: false,
                is_reexport: false,
                reexport_visibility: None,
                kind: "use".to_owned(),
                line,
                evidence: clip_str(evidence, 160),
            });
        }
        return;
    }
    for part in trimmed.split(',') {
        let (target, alias) = split_as_alias(part.trim());
        let path = target.replace(' ', "").trim_start_matches('\\').to_owned();
        if path.is_empty() {
            continue;
        }
        let orig_sym = path.rsplit('\\').next().unwrap_or(&path).to_owned();
        ctx.push_import(AstImport {
            path,
            symbols: vec![orig_sym],
            alias,
            is_wildcard: false,
            is_reexport: false,
            reexport_visibility: None,
            kind: "use".to_owned(),
            line,
            evidence: clip_str(evidence, 160),
        });
    }
}

fn split_as_alias(raw: &str) -> (&str, Option<String>) {
    if let Some((left, right)) = raw.split_once(" as ").or_else(|| raw.split_once(" AS ")) {
        (left.trim(), Some(right.trim().to_owned()))
    } else {
        (raw.trim(), None)
    }
}

fn extract_blade(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    let src = lexed.source;
    for (idx, line_str) in src.lines().enumerate() {
        let line = (idx + 1) as u32;
        for dir in ["@extends", "@include", "@component"] {
            if let Some(pos) = line_str.find(dir) {
                let after = &line_str[pos + dir.len()..];
                if let Some(arg) = after
                    .split(['\'', '"'])
                    .nth(1)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    ctx.push_import(AstImport {
                        path: format!("blade-view:{arg}"),
                        symbols: vec![arg.to_owned()],
                        alias: None,
                        is_wildcard: false,
                        is_reexport: false,
                        reexport_visibility: None,
                        kind: "include".to_owned(),
                        line,
                        evidence: clip_str(line_str, 160),
                    });
                }
            }
        }
        if let Some(pos) = line_str.find("@livewire") {
            let after = &line_str[pos + "@livewire".len()..];
            if let Some(arg) = after
                .split(['\'', '"'])
                .nth(1)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                ctx.push_import(AstImport {
                    path: format!("blade-livewire:{arg}"),
                    symbols: vec![arg.to_owned()],
                    alias: None,
                    is_wildcard: false,
                    is_reexport: false,
                    reexport_visibility: None,
                    kind: "include".to_owned(),
                    line,
                    evidence: clip_str(line_str, 160),
                });
            }
        }
        let mut rest = line_str;
        while let Some(pos) = rest.find("<x-") {
            let tail = &rest[pos + 3..];
            let end = tail
                .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '.' | ':'))
                .unwrap_or(tail.len());
            let tag = tail[..end].trim();
            if !tag.is_empty() {
                ctx.push_import(AstImport {
                    path: format!("blade-component:{tag}"),
                    symbols: vec![tag.to_owned()],
                    alias: None,
                    is_wildcard: false,
                    is_reexport: false,
                    reexport_visibility: None,
                    kind: "include".to_owned(),
                    line,
                    evidence: clip_str(line_str, 160),
                });
            }
            rest = &tail[end..];
        }
        let mut lw_rest = line_str;
        while let Some(pos) = lw_rest.find("<livewire:") {
            let tail = &lw_rest[pos + "<livewire:".len()..];
            let end = tail
                .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '.' | ':'))
                .unwrap_or(tail.len());
            let tag = tail[..end].trim();
            if !tag.is_empty() {
                ctx.push_import(AstImport {
                    path: format!("blade-livewire:{tag}"),
                    symbols: vec![tag.to_owned()],
                    alias: None,
                    is_wildcard: false,
                    is_reexport: false,
                    reexport_visibility: None,
                    kind: "include".to_owned(),
                    line,
                    evidence: clip_str(line_str, 160),
                });
            }
            lw_rest = &tail[end..];
        }
    }
}

// =========================================================================
// 2. Rust
// =========================================================================

fn parse_rust_visibility(
    tokens: &[Token<'_>],
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
) -> (Option<String>, usize) {
    if lo < hi && tokens[lo].is_kw("pub") {
        if tokens.get(lo + 1).is_some_and(|t| t.is_punct('(')) {
            let rp = matching_delim(tokens, lo + 1, '(', ')').min(hi.saturating_sub(1));
            let vis = lexed.slice_text(lo, rp + 1).replace("pub (", "pub(");
            return (Some(vis), rp + 1);
        }
        return (Some("public".to_owned()), lo + 1);
    }
    (None, lo)
}

fn collect_rust_attrs(tokens: &[Token<'_>], lo: usize, hi: usize) -> (Vec<(usize, usize)>, usize) {
    let mut attrs = Vec::new();
    let mut i = lo;
    while i < hi && matches!(tokens[i].kind, TokenKind::Sym("#[") | TokenKind::Sym("#![")) {
        let close = matching_delim(tokens, i, '[', ']').min(hi.saturating_sub(1));
        attrs.push((i, close));
        i = close + 1;
    }
    (attrs, i)
}

fn extract_rust(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    walk_rust_block(ctx, lexed, 0, lexed.tokens.len(), None, false, 0);
    scan_calls_and_refs(ctx, lexed, 0, lexed.tokens.len(), true);
}

fn walk_rust_block(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    owner_name: Option<&str>,
    owner_is_trait_or_trait_impl: bool,
    fn_depth: usize,
) {
    let tokens = &lexed.tokens;
    let mut i = lo;
    while i < hi {
        let (attrs, stmt_lo) = collect_rust_attrs(tokens, i, hi);
        let mut mod_path_override: Option<String> = None;
        for &(a_lo, a_hi) in &attrs {
            let attr_txt = lexed.slice_text(a_lo, a_hi + 1);
            if attr_txt.contains("test") {
                ctx.test_lines.insert(tokens[a_lo].line);
            }
            if attr_txt.contains("path")
                && let Some(s) = collect_string_args(tokens, a_lo, a_hi + 1).into_iter().next()
            {
                mod_path_override = Some(s);
            }
        }
        if stmt_lo >= hi {
            break;
        }
        if tokens[stmt_lo].is_punct(';') || tokens[stmt_lo].is_punct('}') {
            i = stmt_lo + 1;
            continue;
        }
        if tokens[stmt_lo].is_punct('{') {
            let close = matching_delim(tokens, stmt_lo, '{', '}').min(hi.saturating_sub(1));
            walk_rust_block(
                ctx,
                lexed,
                stmt_lo + 1,
                close,
                owner_name,
                owner_is_trait_or_trait_impl,
                fn_depth,
            );
            i = close + 1;
            continue;
        }

        let (vis_opt, after_vis) = parse_rust_visibility(tokens, lexed, stmt_lo, hi);

        // `use ...;` and `pub use ...;` (using `find_semi_stmt_end` so `{...}` groups stay intact!)
        if after_vis < hi && tokens[after_vis].is_kw("use") {
            let end = find_semi_stmt_end(tokens, after_vis + 1, hi);
            let raw = lexed.slice_text(after_vis + 1, end).replace(' ', "");
            parse_rust_use(
                ctx,
                &raw,
                vis_opt.is_some(),
                vis_opt.clone(),
                tokens[after_vis].line,
                lexed.slice_text(stmt_lo, (end + 1).min(hi)),
            );
            i = (end + 1).min(hi);
            continue;
        }

        // `mod foo;` / `mod foo { ... }`
        if after_vis < hi
            && tokens[after_vis].is_kw("mod")
            && after_vis + 1 < hi
            && let Some(mod_name) = tokens[after_vis + 1].ident()
        {
            let next = tokens.get(after_vis + 2);
            if next.is_some_and(|t| t.is_punct('{')) {
                let close = matching_delim(tokens, after_vis + 2, '{', '}').min(hi.saturating_sub(1));
                walk_rust_block(
                    ctx,
                    lexed,
                    after_vis + 3,
                    close,
                    owner_name,
                    owner_is_trait_or_trait_impl,
                    fn_depth,
                );
                i = close + 1;
            } else {
                let end = find_semi_stmt_end(tokens, after_vis + 1, hi);
                ctx.push_import(AstImport {
                    path: mod_path_override.unwrap_or_else(|| mod_name.to_owned()),
                    symbols: vec![mod_name.to_owned()],
                    alias: None,
                    is_wildcard: false,
                    is_reexport: vis_opt.is_some(),
                    reexport_visibility: vis_opt,
                    kind: "mod_decl".to_owned(),
                    line: tokens[after_vis].line,
                    evidence: clip_str(lexed.slice_text(stmt_lo, (end + 1).min(hi)), 160),
                });
                i = (end + 1).min(hi);
            }
            continue;
        }

        // `struct`, `enum`, `trait`, `type`
        if after_vis < hi
            && let Some(dk @ ("struct" | "enum" | "trait" | "type")) = tokens[after_vis].ident()
            && fn_depth == 0
            && after_vis + 1 < hi
            && let Some(name) = tokens[after_vis + 1].ident()
        {
            let end = find_stmt_end(tokens, after_vis + 2, hi);
            let has_brace = tokens.get(end).is_some_and(|t| t.is_punct('{'));
            let body_close = if has_brace {
                matching_delim(tokens, end, '{', '}').min(hi.saturating_sub(1))
            } else {
                end
            };
            let line = tokens[after_vis + 1].line;
            let e_line = tokens.get(body_close).map_or(line, |t| t.line);
            let sig_end = if has_brace {
                tokens[end].start
            } else {
                tokens[end.min(hi - 1)].end
            };
            let sig = normalize_sig(
                lexed
                    .header_without_comments(tokens[stmt_lo].start, sig_end)
                    .trim_end_matches(';'),
            );
            let params_sig = strip_rust_vis_from_sig(&sig);
            let vis = vis_opt.unwrap_or_else(|| "private".to_owned());
            ctx.push_decl(
                dk,
                name,
                None,
                line,
                e_line,
                Some(vis),
                Vec::new(),
                sig,
                params_sig,
                String::new(),
            );
            if has_brace && dk == "struct" {
                extract_rust_struct_fields(ctx, lexed, end + 1, body_close, name);
            } else if has_brace && dk == "trait" {
                walk_rust_block(ctx, lexed, end + 1, body_close, Some(name), true, fn_depth);
            }
            i = (body_close + 1).min(hi);
            continue;
        }

        // `impl`
        if after_vis < hi && tokens[after_vis].is_kw("impl") && fn_depth == 0 {
            let bo = (after_vis + 1..hi).find(|&k| tokens[k].is_punct('{')).unwrap_or(hi);
            let bc = if bo < hi {
                matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1))
            } else {
                hi.saturating_sub(1)
            };
            let mut h_idx = after_vis + 1;
            if tokens.get(h_idx).is_some_and(|t| t.is_punct('<')) {
                let mut d = 1i32;
                h_idx += 1;
                while h_idx < bo && d > 0 {
                    if tokens[h_idx].is_punct('<') {
                        d += 1;
                    } else if tokens[h_idx].is_punct('>') {
                        d -= 1;
                    }
                    h_idx += 1;
                }
            }
            let for_pos = (h_idx..bo).find(|&k| tokens[k].is_kw("for"));
            let (trait_opt, target_raw) = match for_pos {
                Some(fp) => (
                    Some(lexed.slice_text(h_idx, fp).trim()),
                    lexed.slice_text(fp + 1, bo).trim(),
                ),
                None => (None, lexed.slice_text(h_idx, bo).trim()),
            };
            let target_ty = short_type(target_raw.split("where").next().unwrap_or(target_raw));
            let line = tokens[after_vis].line;
            if let Some(tr) = trait_opt {
                let short_tr = short_type(tr);
                if !target_ty.is_empty() && !short_tr.is_empty() {
                    ctx.push_inheritance(
                        &target_ty,
                        &short_tr,
                        "implements",
                        line,
                        lexed.slice_text(after_vis, bo),
                    );
                }
            }
            if bo < bc {
                walk_rust_block(
                    ctx,
                    lexed,
                    bo + 1,
                    bc,
                    (!target_ty.is_empty()).then_some(target_ty.as_str()),
                    trait_opt.is_some(),
                    fn_depth,
                );
            }
            i = (bc + 1).min(hi);
            continue;
        }

        // `const` / `static [mut]`
        if after_vis < hi
            && matches!(tokens[after_vis].ident(), Some("const" | "static"))
            && !tokens
                .get(after_vis + 1)
                .is_some_and(|t| t.is_kw("fn") || t.is_kw("unsafe") || t.is_kw("extern"))
            && fn_depth == 0
        {
            let is_static = tokens[after_vis].is_kw("static");
            let mut cursor = after_vis + 1;
            let mut mods = Vec::new();
            if is_static {
                mods.push("static".to_owned());
                if tokens.get(cursor).is_some_and(|t| t.is_kw("mut")) {
                    mods.push("mut".to_owned());
                    cursor += 1;
                }
            }
            if let Some(name) = tokens.get(cursor).and_then(|t| t.ident()) {
                let end = find_semi_stmt_end(tokens, cursor + 1, hi);
                let eq_pos = (cursor + 1..end).find(|&k| tokens[k].is_punct('='));
                let sig_end = eq_pos.map_or(tokens[end.min(hi - 1)].end, |eq| tokens[eq].start);
                let sig =
                    normalize_sig(&lexed.header_without_comments(tokens[stmt_lo].start, sig_end));
                let params_sig = strip_rust_vis_from_sig(&sig);
                let vis = vis_opt.unwrap_or_else(|| {
                    if owner_is_trait_or_trait_impl {
                        "public".to_owned()
                    } else {
                        "private".to_owned()
                    }
                });
                let body_fp = eq_pos
                    .map(|eq| lexed.body_fingerprint(tokens[eq + 1].start, tokens[end.min(hi - 1)].end))
                    .unwrap_or_default();
                ctx.push_decl(
                    "const",
                    name,
                    owner_name.map(str::to_owned),
                    tokens[cursor].line,
                    tokens[end.min(hi - 1)].line,
                    Some(vis),
                    mods,
                    sig,
                    params_sig,
                    body_fp,
                );
                i = (end + 1).min(hi);
                continue;
            }
        }

        // Functions & methods
        let mut fn_mods = Vec::new();
        let mut mk = after_vis;
        while mk < hi {
            if let Some(m @ ("async" | "unsafe" | "const" | "extern")) = tokens[mk].ident() {
                let ms = m.to_owned();
                if !fn_mods.contains(&ms) {
                    fn_mods.push(ms);
                }
                mk += 1;
                if m == "extern"
                    && matches!(tokens.get(mk).map(|t| &t.kind), Some(TokenKind::Str { .. }))
                {
                    mk += 1;
                }
            } else {
                break;
            }
        }
        if mk < hi
            && tokens[mk].is_kw("fn")
            && fn_depth == 0
            && mk + 1 < hi
            && let Some(name) = tokens[mk + 1].ident()
            && let Some(lp) = (mk + 2..hi).find(|&k| tokens[k].is_punct('('))
        {
            let rp = matching_delim(tokens, lp, '(', ')').min(hi.saturating_sub(1));
            let mut scan = rp + 1;
            let mut bo = None;
            let mut semi = None;
            while scan < hi {
                if tokens[scan].is_punct('{') {
                    bo = Some(scan);
                    break;
                }
                if tokens[scan].is_punct(';') {
                    semi = Some(scan);
                    break;
                }
                scan += 1;
            }
            let bc = bo
                .map(|b| matching_delim(tokens, b, '{', '}').min(hi.saturating_sub(1)))
                .or(semi)
                .unwrap_or(rp);
            let line = tokens[mk + 1].line;
            let e_line = tokens[bc].line;
            let sig_end = bo.map_or(tokens[bc].end, |b| tokens[b].start);
            let sig = normalize_sig(
                lexed
                    .header_without_comments(tokens[stmt_lo].start, sig_end)
                    .trim_end_matches(';'),
            );
            let params_sig = strip_rust_vis_from_sig(&sig);
            let vis = vis_opt.unwrap_or_else(|| {
                if owner_is_trait_or_trait_impl {
                    "public".to_owned()
                } else {
                    "private".to_owned()
                }
            });
            let decl_kind = if owner_name.is_some() {
                "method"
            } else {
                "function"
            };
            let body_fp = bo
                .map(|b| lexed.body_fingerprint(tokens[b].start, tokens[bc].end))
                .unwrap_or_default();
            let sym_label = owner_name.map_or_else(|| name.to_owned(), |o| format!("{o}::{name}"));

            for (p_lo, p_hi) in split_top_level(tokens, lp + 1, rp, ',') {
                if let Some(colon) = (p_lo..p_hi).find(|&k| tokens[k].is_punct(':')) {
                    let pname = lexed
                        .slice_text(p_lo, colon)
                        .trim()
                        .trim_start_matches("mut ")
                        .trim();
                    let pty = lexed.slice_text(colon + 1, p_hi).trim();
                    if !pty.is_empty() {
                        ctx.record_var_type(&sym_label, pname, &short_type(pty));
                        push_type_refs(
                            ctx,
                            pty,
                            "parameter_type",
                            Some(&sym_label),
                            tokens[p_lo].line,
                            &sig,
                        );
                    }
                }
            }
            if rp + 1 < bo.unwrap_or(semi.unwrap_or(rp + 1)) {
                let tail = lexed.slice_text(rp + 1, bo.unwrap_or(semi.unwrap_or(rp + 1)));
                if let Some((_, ret)) = tail.split_once("->") {
                    push_type_refs(
                        ctx,
                        ret.split("where").next().unwrap_or(ret),
                        "return_type",
                        Some(&sym_label),
                        line,
                        &sig,
                    );
                }
            }

            ctx.push_decl(
                decl_kind,
                name,
                owner_name.map(str::to_owned),
                line,
                e_line,
                Some(vis),
                fn_mods,
                sig,
                params_sig,
                body_fp,
            );
            if let Some(b) = bo {
                walk_rust_block(
                    ctx,
                    lexed,
                    b + 1,
                    bc,
                    owner_name,
                    owner_is_trait_or_trait_impl,
                    fn_depth + 1,
                );
            }
            i = (bc + 1).min(hi);
            continue;
        }

        let end = find_stmt_end(tokens, stmt_lo + 1, hi);
        if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
            let close = matching_delim(tokens, end, '{', '}').min(hi.saturating_sub(1));
            walk_rust_block(
                ctx,
                lexed,
                end + 1,
                close,
                owner_name,
                owner_is_trait_or_trait_impl,
                fn_depth,
            );
            i = close + 1;
        } else {
            i = (end + 1).min(hi);
        }
    }
}

fn extract_rust_struct_fields(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    owner: &str,
) {
    let tokens = &lexed.tokens;
    for (f_lo, f_hi) in split_top_level(tokens, lo, hi, ',') {
        let (_, f_start) = collect_rust_attrs(tokens, f_lo, f_hi);
        if f_start >= f_hi {
            continue;
        }
        let (vis_opt, after_vis) = parse_rust_visibility(tokens, lexed, f_start, f_hi);
        if let Some(fname) = tokens.get(after_vis).and_then(|t| t.ident())
            && let Some(colon) = (after_vis + 1..f_hi).find(|&k| tokens[k].is_punct(':'))
        {
            let fty = lexed.slice_text(colon + 1, f_hi).trim();
            let line = tokens[after_vis].line;
            let sig = normalize_sig(
                &lexed.header_without_comments(tokens[f_start].start, tokens[f_hi - 1].end),
            );
            let params_sig = strip_rust_vis_from_sig(&sig);
            ctx.record_var_type(owner, fname, &short_type(fty));
            push_type_refs(ctx, fty, "field", Some(owner), line, &sig);
            ctx.push_decl(
                "field",
                fname,
                Some(owner.to_owned()),
                line,
                tokens[f_hi - 1].line,
                Some(vis_opt.unwrap_or_else(|| "private".to_owned())),
                Vec::new(),
                sig,
                params_sig,
                String::new(),
            );
        }
    }
}

fn parse_rust_use(
    ctx: &mut ExtractCtx<'_>,
    raw: &str,
    is_reexport: bool,
    reexport_visibility: Option<String>,
    line: u32,
    evidence: &str,
) {
    fn expand(prefix: &str, body: &str, out: &mut Vec<(String, Option<String>, bool)>) {
        let mut depth = 0i32;
        let mut parts = Vec::new();
        let mut start = 0usize;
        for (idx, ch) in body.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth = (depth - 1).max(0),
                ',' if depth == 0 => {
                    parts.push(body[start..idx].trim());
                    start = idx + 1;
                }
                _ => {}
            }
        }
        parts.push(body[start..].trim());
        for p in parts {
            if p.is_empty() {
                continue;
            }
            if let (Some(lb), Some(rb)) = (p.find('{'), p.rfind('}')) {
                let sub_pref = p[..lb].trim().trim_end_matches("::");
                let combined = if prefix.is_empty() {
                    sub_pref.to_owned()
                } else if sub_pref.is_empty() {
                    prefix.to_owned()
                } else {
                    format!("{prefix}::{sub_pref}")
                };
                expand(&combined, &p[lb + 1..rb], out);
            } else {
                let (item, alias) = split_as_alias(p);
                let item_clean = if item == "self" { "" } else { item };
                let full = if prefix.is_empty() {
                    item_clean.to_owned()
                } else if item_clean.is_empty() {
                    prefix.to_owned()
                } else {
                    format!("{prefix}::{item_clean}")
                };
                let is_wc = full.ends_with("::*") || item == "*";
                out.push((full, alias, is_wc));
            }
        }
    }

    let mut expanded = Vec::new();
    expand("", raw, &mut expanded);
    for (path, alias, is_wc) in expanded {
        if path.is_empty() {
            continue;
        }
        let orig_sym = path.rsplit("::").next().unwrap_or(&path).to_owned();
        ctx.push_import(AstImport {
            path,
            symbols: if is_wc { Vec::new() } else { vec![orig_sym] },
            alias,
            is_wildcard: is_wc,
            is_reexport,
            reexport_visibility: reexport_visibility.clone(),
            kind: "use".to_owned(),
            line,
            evidence: clip_str(evidence, 160),
        });
    }
}

// =========================================================================
// 3. TypeScript / TSX / JavaScript
// =========================================================================

fn extract_ts_js(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    walk_ts_block(ctx, lexed, 0, lexed.tokens.len(), None, 0);
    scan_ts_provider_bindings(ctx, lexed);
    scan_calls_and_refs(ctx, lexed, 0, lexed.tokens.len(), true);
}

fn scan_ts_provider_bindings(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    let tokens = &lexed.tokens;
    for i in 0..tokens.len().saturating_sub(2) {
        if tokens[i].is_kw("provide")
            && tokens[i + 1].is_punct(':')
            && let Some(tok_str) = match &tokens[i + 2].kind {
                TokenKind::Str { unquoted, .. } if !unquoted.is_empty() => Some(unquoted.clone()),
                TokenKind::Ident(id) if is_upper_ident(id) => Some((*id).to_owned()),
                _ => None,
            }
        {
            let line = tokens[i].line;
            let encl = ctx.enclosing_symbol_at_line(line);
            ctx.push_semantic(
                "di_binds",
                format!("service:{tok_str}"),
                encl,
                line,
                lexed.slice_text(i.saturating_sub(1), (i + 4).min(tokens.len())),
            );
        }
    }
}

fn collect_ts_decorators(
    tokens: &[Token<'_>],
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
) -> (Vec<(String, String, Vec<String>, u32)>, usize) {
    let mut out = Vec::new();
    let mut i = lo;
    while i < hi && tokens[i].is_punct('@') {
        let mut cursor = i + 1;
        let dec_line = tokens[i].line;
        while cursor < hi
            && tokens[cursor].line == dec_line
            && matches!(tokens[cursor].kind, TokenKind::Ident(_) | TokenKind::Punct('.'))
        {
            cursor += 1;
        }
        let name = lexed.slice_text(i + 1, cursor).trim().to_owned();
        let mut str_args = Vec::new();
        if tokens.get(cursor).is_some_and(|t| t.is_punct('(')) {
            let rp = matching_delim(tokens, cursor, '(', ')').min(hi.saturating_sub(1));
            str_args = collect_string_args(tokens, cursor + 1, rp);
            cursor = rp + 1;
        }
        let text = lexed.slice_text(i, cursor).to_owned();
        out.push((name, text, str_args, tokens[i].line));
        i = cursor;
    }
    (out, i)
}

fn push_ts_import_clause(
    ctx: &mut ExtractCtx<'_>,
    clause_raw: &str,
    spec: &str,
    is_reexport: bool,
    line: u32,
    evidence: &str,
) {
    let trimmed = clause_raw.trim().trim_end_matches("from").trim();
    if trimmed.is_empty() {
        ctx.push_import(AstImport {
            path: spec.to_owned(),
            symbols: Vec::new(),
            alias: None,
            is_wildcard: false,
            is_reexport,
            reexport_visibility: None,
            kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
            line,
            evidence: clip_str(evidence, 160),
        });
        return;
    }
    if trimmed == "*" {
        ctx.push_import(AstImport {
            path: spec.to_owned(),
            symbols: Vec::new(),
            alias: None,
            is_wildcard: true,
            is_reexport,
            reexport_visibility: None,
            kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
            line,
            evidence: clip_str(evidence, 160),
        });
        return;
    }
    if let Some((lb, rb)) = trimmed.find('{').zip(trimmed.rfind('}')) {
        let before = trimmed[..lb].trim().trim_end_matches(',').trim();
        if !before.is_empty() && !before.starts_with('*') {
            ctx.push_import(AstImport {
                path: spec.to_owned(),
                symbols: vec![before.to_owned()],
                alias: None,
                is_wildcard: false,
                is_reexport,
                reexport_visibility: None,
                kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
                line,
                evidence: clip_str(evidence, 160),
            });
        }
        for spec_part in trimmed[lb + 1..rb].split(',') {
            let item = spec_part.trim().trim_start_matches("type ").trim();
            if item.is_empty() {
                continue;
            }
            let (orig, alias) = split_as_alias(item);
            if !orig.is_empty() {
                ctx.push_import(AstImport {
                    path: spec.to_owned(),
                    symbols: vec![orig.to_owned()],
                    alias,
                    is_wildcard: false,
                    is_reexport,
                    reexport_visibility: None,
                    kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
                    line,
                    evidence: clip_str(evidence, 160),
                });
            }
        }
        return;
    }
    if let Some(ns_part) = trimmed.strip_prefix('*') {
        let (_, ns_alias) = split_as_alias(&format!("*{ns_part}"));
        ctx.push_import(AstImport {
            path: spec.to_owned(),
            symbols: Vec::new(),
            alias: ns_alias,
            is_wildcard: true,
            is_reexport,
            reexport_visibility: None,
            kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
            line,
            evidence: clip_str(evidence, 160),
        });
        return;
    }
    ctx.push_import(AstImport {
        path: spec.to_owned(),
        symbols: vec![trimmed.trim_start_matches("type ").trim().to_owned()],
        alias: None,
        is_wildcard: false,
        is_reexport,
        reexport_visibility: None,
        kind: if is_reexport { "reexport" } else { "import" }.to_owned(),
        line,
        evidence: clip_str(evidence, 160),
    });
}

fn walk_ts_block(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    cur_class: Option<&str>,
    fn_depth: usize,
) {
    let tokens = &lexed.tokens;
    let mut i = lo;
    while i < hi {
        let (decs, stmt_lo) = collect_ts_decorators(tokens, lexed, i, hi);
        if stmt_lo >= hi {
            break;
        }
        if tokens[stmt_lo].is_punct(';') || tokens[stmt_lo].is_punct('}') {
            i = stmt_lo + 1;
            continue;
        }
        if tokens[stmt_lo].is_punct('{') {
            let close = matching_delim(tokens, stmt_lo, '{', '}').min(hi.saturating_sub(1));
            walk_ts_block(ctx, lexed, stmt_lo + 1, close, cur_class, fn_depth);
            i = close + 1;
            continue;
        }

        // `import ... from '...'` (matches balanced `{ ... }` cleanly!)
        if tokens[stmt_lo].is_kw("import") && cur_class.is_none() {
            let end = find_import_stmt_end(tokens, stmt_lo + 1, hi, true);
            let strs = collect_string_args(tokens, stmt_lo + 1, end);
            let raw = lexed.slice_text(stmt_lo, end);
            if let Some(mod_path) = strs.last() {
                let before_from = raw
                    .strip_prefix("import")
                    .unwrap_or(raw)
                    .rsplit_once(" from ")
                    .map(|(c, _)| c)
                    .unwrap_or("");
                push_ts_import_clause(ctx, before_from, mod_path, false, tokens[stmt_lo].line, raw);
            }
            i = if tokens.get(end).is_some_and(|t| t.is_punct(';')) {
                (end + 1).min(hi)
            } else {
                end.min(hi)
            };
            continue;
        }

        let mut cursor = stmt_lo;
        let mut vis: Option<String> = None;
        let mut mods: Vec<String> = Vec::new();
        while cursor < hi {
            match tokens[cursor].ident() {
                Some(v @ ("public" | "protected" | "private")) => {
                    vis = Some(v.to_owned());
                    cursor += 1;
                }
                Some(
                    m @ (
                        "export"
                        | "default"
                        | "declare"
                        | "static"
                        | "async"
                        | "readonly"
                        | "abstract"
                        | "override"
                    ),
                ) => {
                    let ms = m.to_owned();
                    if !mods.contains(&ms) {
                        mods.push(ms);
                    }
                    cursor += 1;
                }
                _ => break,
            }
        }

        // Re-exports: `export { A, b as c } from './x'` or `export * from './x'`
        if mods.iter().any(|m| m == "export")
            && cursor < hi
            && (tokens[cursor].is_punct('{') || tokens[cursor].is_punct('*'))
        {
            let end = find_import_stmt_end(tokens, cursor, hi, true);
            let raw = lexed.slice_text(stmt_lo, end);
            let strs = collect_string_args(tokens, cursor, end);
            if let Some(spec) = strs.last()
                && let Some((clause, _)) = raw.rsplit_once(" from ")
            {
                push_ts_import_clause(
                    ctx,
                    clause.trim_start_matches("export").trim(),
                    spec,
                    true,
                    tokens[stmt_lo].line,
                    raw,
                );
            }
            i = if tokens.get(end).is_some_and(|t| t.is_punct(';')) {
                (end + 1).min(hi)
            } else {
                end.min(hi)
            };
            continue;
        }

        // `class`, `interface`, `enum`, `type`
        if cursor < hi
            && let Some(dk @ ("class" | "interface" | "enum" | "type")) = tokens[cursor].ident()
            && fn_depth == 0
            && cursor + 1 < hi
            && let Some(name) = tokens[cursor + 1].ident()
        {
            let line = tokens[cursor + 1].line;
            if dk == "type" {
                let end = find_semi_stmt_end(tokens, cursor + 2, hi);
                let eq_pos = (cursor + 2..end).find(|&k| tokens[k].is_punct('='));
                let sig_end = eq_pos.map_or(tokens[end.min(hi - 1)].end, |eq| tokens[eq].start);
                let sig = normalize_sig(
                    &lexed.header_without_comments(tokens[stmt_lo].start, sig_end),
                );
                let params_sig =
                    strip_leading_words(&sig, &["export", "public", "protected", "private"]);
                ctx.push_decl(
                    "type",
                    name,
                    None,
                    line,
                    tokens[end.min(hi - 1)].line,
                    Some("public".to_owned()),
                    mods,
                    sig,
                    params_sig,
                    String::new(),
                );
                i = (end + 1).min(hi);
                continue;
            }

            let bo = (cursor + 2..hi).find(|&k| tokens[k].is_punct('{')).unwrap_or(hi);
            let bc = if bo < hi {
                matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1))
            } else {
                hi.saturating_sub(1)
            };
            let sig_end = tokens.get(bo).map_or(tokens[bc].end, |t| t.start);
            let sig = normalize_sig(
                &lexed.header_without_comments(tokens[stmt_lo].start, sig_end),
            );
            let params_sig =
                strip_leading_words(&sig, &["export", "public", "protected", "private"]);

            let mut route_pref = None;
            for (dname, dtext, dargs, dline) in &decs {
                if dname.eq_ignore_ascii_case("Controller")
                    && let Some(p) = dargs.first()
                {
                    route_pref = Some(p.clone());
                }
                emit_semantic_from_attribute(ctx, dname, dtext, dargs, Some(name), &[], *dline);
            }

            // Empty body_fingerprint on container types
            ctx.push_decl(
                dk,
                name,
                None,
                line,
                tokens[bc].line,
                Some(vis.unwrap_or_else(|| "public".to_owned())),
                mods,
                sig,
                params_sig,
                String::new(),
            );

            let mut h = cursor + 2;
            while h < bo {
                if tokens[h].is_kw("extends") || tokens[h].is_kw("implements") {
                    let rel = if tokens[h].is_kw("extends") {
                        "extends"
                    } else {
                        "implements"
                    };
                    let mut h_end = h + 1;
                    while h_end < bo
                        && !tokens[h_end].is_kw("extends")
                        && !tokens[h_end].is_kw("implements")
                    {
                        h_end += 1;
                    }
                    let txt = lexed.slice_text(h + 1, h_end);
                    for p_raw in txt.split(',') {
                        let clean = short_type(p_raw);
                        if !clean.is_empty() {
                            ctx.push_inheritance(name, &clean, rel, line, txt);
                        }
                    }
                    h = h_end;
                } else {
                    h += 1;
                }
            }

            // Interfaces do not emit method/property sub-decls in graph_live
            if dk == "class" && bo < bc {
                let mark = route_pref.as_deref().map(|p| ctx.push_attr_route_prefix(p));
                walk_ts_block(ctx, lexed, bo + 1, bc, Some(name), fn_depth);
                if let Some(m) = mark {
                    ctx.truncate_attr_route_prefixes(m);
                }
            }
            i = (bc + 1).min(hi);
            continue;
        }

        // Top-level `function name(...)`
        if cursor < hi
            && tokens[cursor].is_kw("function")
            && fn_depth == 0
            && cursor + 1 < hi
            && let Some(name) = tokens[cursor + 1].ident()
            && let Some(lp) = (cursor + 2..hi).find(|&k| tokens[k].is_punct('('))
        {
            let rp = matching_delim(tokens, lp, '(', ')').min(hi.saturating_sub(1));
            let bo = (rp + 1..hi)
                .find(|&k| tokens[k].is_punct('{') || tokens[k].is_punct(';'))
                .unwrap_or(rp);
            let is_body = tokens.get(bo).is_some_and(|t| t.is_punct('{'));
            let bc = if is_body {
                matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1))
            } else {
                bo
            };
            let line = tokens[cursor + 1].line;
            let sig_end = if is_body {
                tokens[bo].start
            } else {
                tokens[bc].end
            };
            let sig = normalize_sig(
                lexed
                    .header_without_comments(tokens[stmt_lo].start, sig_end)
                    .trim_end_matches(';'),
            );
            let params_sig =
                strip_leading_words(&sig, &["export", "public", "protected", "private"]);
            let body_fp = if is_body {
                lexed.body_fingerprint(tokens[bo].start, tokens[bc].end)
            } else {
                String::new()
            };
            ctx.push_decl(
                "function",
                name,
                None,
                line,
                tokens[bc].line,
                Some(vis.unwrap_or_else(|| "public".to_owned())),
                mods,
                sig,
                params_sig,
                body_fp,
            );
            if is_body {
                walk_ts_block(ctx, lexed, bo + 1, bc, cur_class, fn_depth + 1);
            }
            i = (bc + 1).min(hi);
            continue;
        }

        // `const`/`let`/`var` at top level (`export const FOO = ...`, `const fn = (...) => { ... }`)
        if cursor < hi
            && matches!(tokens[cursor].ident(), Some("const" | "let" | "var"))
            && cur_class.is_none()
            && fn_depth == 0
            && cursor + 1 < hi
            && let Some(name) = tokens[cursor + 1].ident()
        {
            let eq = (cursor + 2..hi).find(|&k| tokens[k].is_punct('='));
            let end = find_ts_stmt_end(tokens, cursor + 2, hi);
            let has_arrow = eq.is_some_and(|e| (e..end).any(|k| tokens[k].is_sym("=>")));
            if has_arrow && let Some(e) = eq {
                let bo_opt = (e..end)
                    .find(|&k| tokens[k].is_punct('{') && k > e && tokens[k - 1].is_sym("=>"));
                let bc = bo_opt
                    .map(|bo| matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1)))
                    .unwrap_or(end);
                let line = tokens[cursor + 1].line;
                let sig_end = bo_opt.map_or(tokens[bc.min(hi - 1)].end, |bo| tokens[bo].start);
                let sig = normalize_sig(
                    &lexed.header_without_comments(tokens[stmt_lo].start, sig_end),
                );
                let params_sig =
                    strip_leading_words(&sig, &["export", "public", "protected", "private"]);
                let body_fp = bo_opt
                    .map(|bo| lexed.body_fingerprint(tokens[bo].start, tokens[bc].end))
                    .unwrap_or_default();
                ctx.push_decl(
                    "function",
                    name,
                    None,
                    line,
                    tokens[bc.min(hi - 1)].line,
                    Some("public".to_owned()),
                    mods,
                    sig,
                    params_sig,
                    body_fp,
                );
                if let Some(bo) = bo_opt {
                    walk_ts_block(ctx, lexed, bo + 1, bc, None, fn_depth + 1);
                }
                i = if tokens.get(bc).is_some_and(|t| t.is_punct(';') || t.is_punct('}')) {
                    (bc + 1).min(hi)
                } else {
                    bc.min(hi)
                };
                continue;
            }
            if tokens[cursor].is_kw("const") && let Some(e) = eq {
                let exported = mods.iter().any(|m| m == "export");
                if super::is_upper_snake(name) || exported {
                    let line = tokens[cursor + 1].line;
                    let sig = normalize_sig(
                        &lexed.header_without_comments(tokens[stmt_lo].start, tokens[e].start),
                    );
                    let params_sig =
                        strip_leading_words(&sig, &["export", "public", "protected", "private"]);
                    let val_end = end.saturating_sub(1).max(e);
                    let body_fp = lexed.body_fingerprint(tokens[e + 1].start, tokens[val_end].end);
                    let const_vis = if exported { "export" } else { "private" };
                    ctx.push_decl(
                        "const",
                        name,
                        None,
                        line,
                        tokens[val_end].line,
                        Some(const_vis.to_owned()),
                        mods,
                        sig,
                        params_sig,
                        body_fp,
                    );
                }
                i = if tokens.get(end).is_some_and(|t| t.is_punct(';')) {
                    (end + 1).min(hi)
                } else {
                    end.min(hi)
                };
                continue;
            }
        }

        // Class members inside `cur_class`: methods and properties (`#priv`, `foo: T;`, arrow fields)
        if let Some(owner) = cur_class && fn_depth == 0 && cursor < hi {
            let mut m_idx = cursor;
            if matches!(tokens[m_idx].ident(), Some("get" | "set"))
                && tokens.get(m_idx + 1).and_then(|t| t.ident()).is_some()
                && tokens.get(m_idx + 2).is_some_and(|t| t.is_punct('('))
            {
                m_idx += 1;
            }
            let member_name_raw = match tokens[m_idx].kind {
                TokenKind::Ident(s) | TokenKind::PrivateIdent(s) => Some(s),
                _ => None,
            };
            if let Some(mname) = member_name_raw {
                let m_vis = if mname.starts_with('#') {
                    "#private".to_owned()
                } else {
                    vis.unwrap_or_else(|| "public".to_owned())
                };
                if tokens
                    .get(m_idx + 1)
                    .is_some_and(|t| t.is_punct('(') || t.is_punct('<'))
                {
                    let lp = (m_idx + 1..hi).find(|&k| tokens[k].is_punct('(')).unwrap_or(hi);
                    if lp < hi {
                        let rp = matching_delim(tokens, lp, '(', ')').min(hi.saturating_sub(1));
                        let mut bo_opt = None;
                        let mut semi_opt = None;
                        let mut s = rp + 1;
                        while s < hi {
                            if tokens[s].is_punct('{') {
                                bo_opt = Some(s);
                                break;
                            }
                            if tokens[s].is_punct(';') || tokens[s].line > tokens[rp].line + 1 {
                                semi_opt = Some(s);
                                break;
                            }
                            s += 1;
                        }
                        let bc = bo_opt
                            .map(|bo| matching_delim(tokens, bo, '{', '}').min(hi.saturating_sub(1)))
                            .or(semi_opt)
                            .unwrap_or(rp);
                        let line = tokens[m_idx].line;
                        let sig_end = bo_opt.map_or(tokens[bc].end, |bo| tokens[bo].start);
                        let sig = normalize_sig(
                            lexed
                                .header_without_comments(tokens[stmt_lo].start, sig_end)
                                .trim_end_matches(';'),
                        );
                        let params_sig = strip_leading_words(
                            &sig,
                            &["export", "public", "protected", "private"],
                        );
                        let body_fp = bo_opt
                            .map(|bo| lexed.body_fingerprint(tokens[bo].start, tokens[bc].end))
                            .unwrap_or_default();
                        let sym_label = format!("{owner}::{mname}");
                        for (dname, dtext, dargs, dline) in &decs {
                            emit_semantic_from_attribute(
                                ctx,
                                dname,
                                dtext,
                                dargs,
                                Some(&sym_label),
                                &[],
                                *dline,
                            );
                        }
                        ctx.push_decl(
                            "method",
                            mname,
                            Some(owner.to_owned()),
                            line,
                            tokens[bc].line,
                            Some(m_vis),
                            mods.clone(),
                            sig,
                            params_sig,
                            body_fp,
                        );
                        if mname == "constructor" {
                            for (p_lo, p_hi) in split_top_level(tokens, lp + 1, rp, ',') {
                                let (pdecs, after_pdec) =
                                    collect_ts_decorators(tokens, lexed, p_lo, p_hi);
                                for (dname, dtext, dargs, dline) in &pdecs {
                                    emit_semantic_from_attribute(
                                        ctx,
                                        dname,
                                        dtext,
                                        dargs,
                                        Some(&sym_label),
                                        &[],
                                        *dline,
                                    );
                                }
                                let mut pk = after_pdec;
                                let mut pv: Option<String> = None;
                                let mut pmods = Vec::new();
                                while pk < p_hi {
                                    match tokens[pk].ident() {
                                        Some(v @ ("public" | "protected" | "private")) => {
                                            pv = Some(v.to_owned());
                                            pk += 1;
                                        }
                                        Some(m @ ("readonly" | "override")) => {
                                            pmods.push(m.to_owned());
                                            pk += 1;
                                        }
                                        _ => break,
                                    }
                                }
                                if (pv.is_some() || !pmods.is_empty())
                                    && let Some(pname) = tokens.get(pk).and_then(|t| t.ident())
                                {
                                    if let Some(colon) =
                                        (pk + 1..p_hi).find(|&k| tokens[k].is_punct(':'))
                                    {
                                        let pty_end = (colon + 1..p_hi)
                                            .find(|&k| tokens[k].is_punct('='))
                                            .unwrap_or(p_hi);
                                        let pty = lexed.slice_text(colon + 1, pty_end).trim();
                                        let short = short_type(pty);
                                        ctx.record_var_type(owner, pname, &short);
                                        ctx.record_var_type(&sym_label, pname, &short);
                                    }
                                    let psig = normalize_sig(lexed.slice_text(after_pdec, p_hi));
                                    let ppar = strip_leading_words(
                                        &psig,
                                        &["export", "public", "protected", "private"],
                                    );
                                    ctx.push_decl(
                                        "property",
                                        pname,
                                        Some(owner.to_owned()),
                                        tokens[pk].line,
                                        tokens[pk].line,
                                        pv.or_else(|| Some("public".to_owned())),
                                        pmods,
                                        psig,
                                        ppar,
                                        String::new(),
                                    );
                                }
                            }
                        }
                        if let Some(bo) = bo_opt {
                            walk_ts_block(ctx, lexed, bo + 1, bc, cur_class, fn_depth + 1);
                        }
                        i = (bc + 1).min(hi);
                        continue;
                    }
                } else {
                    // Property: ASI-aware statement end (`#secret: string`, `version: number = 2`, `onExpire = ...`)
                    let end = find_ts_stmt_end(tokens, m_idx + 1, hi);
                    let eq_pos = (m_idx + 1..end).find(|&k| tokens[k].is_punct('='));
                    let ty_end = eq_pos.unwrap_or(end);
                    if let Some(colon) = (m_idx + 1..ty_end).find(|&k| tokens[k].is_punct(':')) {
                        let fty = lexed.slice_text(colon + 1, ty_end).trim();
                        ctx.record_var_type(owner, mname, &short_type(fty));
                    }
                    let sig_end = eq_pos.map_or(
                        tokens[end.saturating_sub(1).max(m_idx)].end,
                        |eq| tokens[eq].start,
                    );
                    let sig = normalize_sig(
                        lexed
                            .header_without_comments(tokens[stmt_lo].start, sig_end)
                            .trim_end_matches(';'),
                    );
                    let params_sig = strip_leading_words(
                        &sig,
                        &["export", "public", "protected", "private"],
                    );
                    let val_end = end.saturating_sub(1).max(m_idx);
                    let body_fp = eq_pos
                        .map(|eq| lexed.body_fingerprint(tokens[eq + 1].start, tokens[val_end].end))
                        .unwrap_or_default();
                    ctx.push_decl(
                        "property",
                        mname,
                        Some(owner.to_owned()),
                        tokens[m_idx].line,
                        tokens[val_end].line,
                        Some(m_vis),
                        mods,
                        sig,
                        params_sig,
                        body_fp,
                    );
                    i = if tokens.get(end).is_some_and(|t| t.is_punct(';')) {
                        (end + 1).min(hi)
                    } else {
                        end.min(hi)
                    };
                    continue;
                }
            }
        }

        let end = find_stmt_end(tokens, stmt_lo + 1, hi);
        if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
            let close = matching_delim(tokens, end, '{', '}').min(hi.saturating_sub(1));
            walk_ts_block(ctx, lexed, end + 1, close, cur_class, fn_depth);
            i = close + 1;
        } else {
            i = (end + 1).min(hi);
        }
    }
}

// =========================================================================
// 4. Python (Indentation-Aware)
// =========================================================================

fn py_visibility(name: &str) -> String {
    if name.starts_with("__") && !name.ends_with("__") {
        "private".to_owned()
    } else if name.starts_with('_') && !name.starts_with("__") {
        "protected".to_owned()
    } else {
        "public".to_owned()
    }
}

fn find_py_block_end(
    tokens: &[Token<'_>],
    lexed: &Lexed<'_>,
    start: usize,
    base_indent: usize,
) -> usize {
    let mut j = start;
    while j < tokens.len() {
        let ind = lexed.indent_of_line(tokens[j].line);
        if ind <= base_indent {
            break;
        }
        j += 1;
    }
    j.max(start)
}

fn extract_python(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    let tokens = &lexed.tokens;
    let mut class_stack: Vec<(String, usize)> = Vec::new();
    let mut fn_indent_stack: Vec<usize> = Vec::new();
    let mut pending_decs: Vec<(String, String, Vec<String>, u32)> = Vec::new();

    let mut i = 0usize;
    while i < tokens.len() {
        let line = tokens[i].line;
        let indent = lexed.indent_of_line(line);

        while fn_indent_stack.last().is_some_and(|&d| indent <= d) {
            fn_indent_stack.pop();
        }
        while class_stack.last().is_some_and(|(_, d)| indent <= *d) {
            class_stack.pop();
        }

        if tokens[i].is_punct('@') && (i == 0 || tokens[i - 1].line < line) {
            let mut cur = i + 1;
            while cur < tokens.len()
                && tokens[cur].line == line
                && matches!(tokens[cur].kind, TokenKind::Ident(_) | TokenKind::Punct('.'))
            {
                cur += 1;
            }
            let dname = lexed.slice_text(i + 1, cur).trim().to_owned();
            let mut dargs = Vec::new();
            if tokens.get(cur).is_some_and(|t| t.is_punct('(')) {
                let rp = matching_delim(tokens, cur, '(', ')');
                dargs = collect_string_args(tokens, cur + 1, rp);
                cur = rp + 1;
            }
            pending_decs.push((dname, lexed.slice_text(i, cur).to_owned(), dargs, line));
            i = cur;
            continue;
        }

        if i > 0 && tokens[i - 1].line == line {
            i += 1;
            continue;
        }

        // Imports
        if tokens[i].is_kw("import") || tokens[i].is_kw("from") {
            let mut end = i + 1;
            while end < tokens.len() && tokens[end].line == line {
                if tokens[end].is_punct('(') {
                    end = matching_delim(tokens, end, '(', ')') + 1;
                } else {
                    end += 1;
                }
            }
            let raw = lexed.slice_text(i, end);
            if tokens[i].is_kw("from") {
                if let Some((_, rhs)) = raw.split_once(" import ") {
                    let mod_path = raw["from ".len()..raw.find(" import ").unwrap_or(raw.len())].trim();
                    for item in rhs.trim_matches(['(', ')']).split(',') {
                        let (orig, alias) = split_as_alias(item.trim());
                        if !orig.is_empty() {
                            ctx.push_import(AstImport {
                                path: mod_path.to_owned(),
                                symbols: if orig == "*" { Vec::new() } else { vec![orig.to_owned()] },
                                alias,
                                is_wildcard: orig == "*",
                                is_reexport: false,
                                reexport_visibility: None,
                                kind: "import".to_owned(),
                                line,
                                evidence: clip_str(raw, 160),
                            });
                        }
                    }
                }
            } else {
                for p in raw["import ".len()..].split(',') {
                    let (target, alias) = split_as_alias(p.trim());
                    if !target.is_empty() {
                        ctx.push_import(AstImport {
                            path: target.to_owned(),
                            symbols: vec![target.to_owned()],
                            alias,
                            is_wildcard: false,
                            is_reexport: false,
                            reexport_visibility: None,
                            kind: "import".to_owned(),
                            line,
                            evidence: clip_str(raw, 160),
                        });
                    }
                }
            }
            pending_decs.clear();
            i = end;
            continue;
        }

        // `class Name(Base):`
        if tokens[i].is_kw("class")
            && i + 1 < tokens.len()
            && let Some(name) = tokens[i + 1].ident()
        {
            let header_end = (i + 2..tokens.len())
                .find(|&k| tokens[k].is_punct(':'))
                .unwrap_or(i + 2);
            let block_start = (header_end + 1).min(tokens.len());
            let block_end = find_py_block_end(tokens, lexed, block_start, indent);
            let e_line = block_end
                .checked_sub(1)
                .and_then(|k| tokens.get(k))
                .map_or(line, |t| t.line);
            let sig = normalize_sig(
                &lexed.header_without_comments(tokens[i].start, tokens[header_end.min(tokens.len() - 1)].end),
            );
            ctx.push_decl(
                "class",
                name,
                None,
                line,
                e_line,
                Some(py_visibility(name)),
                Vec::new(),
                sig.clone(),
                sig,
                String::new(),
            );
            if tokens.get(i + 2).is_some_and(|t| t.is_punct('(')) {
                let rp = matching_delim(tokens, i + 2, '(', ')');
                let bases = lexed.slice_text(i + 3, rp);
                for b in bases.split(',') {
                    let clean = short_type(b);
                    if !clean.is_empty() {
                        ctx.push_inheritance(name, &clean, "extends", line, bases);
                    }
                }
            }
            for (dname, dtext, dargs, dline) in pending_decs.drain(..) {
                emit_semantic_from_attribute(ctx, &dname, &dtext, &dargs, Some(name), &[], dline);
            }
            class_stack.push((name.to_owned(), indent));
            i = block_start;
            continue;
        }

        // `[async] def name(...)`
        let (is_async, def_idx) =
            if tokens[i].is_kw("async") && tokens.get(i + 1).is_some_and(|t| t.is_kw("def")) {
                (true, i + 1)
            } else if tokens[i].is_kw("def") {
                (false, i)
            } else {
                (false, usize::MAX)
            };
        if def_idx < tokens.len()
            && let Some(name) = tokens.get(def_idx + 1).and_then(|t| t.ident())
            && fn_indent_stack.is_empty()
        {
            let lp = (def_idx + 2..tokens.len())
                .find(|&k| tokens[k].is_punct('('))
                .unwrap_or(def_idx + 2);
            let rp = if lp < tokens.len() {
                matching_delim(tokens, lp, '(', ')')
            } else {
                lp
            };
            let colon = (rp..tokens.len())
                .find(|&k| tokens[k].is_punct(':'))
                .unwrap_or(rp);
            let block_start = (colon + 1).min(tokens.len());
            let block_end = find_py_block_end(tokens, lexed, block_start, indent);
            let e_line = block_end
                .checked_sub(1)
                .and_then(|k| tokens.get(k))
                .map_or(line, |t| t.line);
            let owner = class_stack.last().map(|(c, _)| c.clone());
            let sig = normalize_sig(
                &lexed.header_without_comments(tokens[i].start, tokens[colon.min(tokens.len() - 1)].end),
            );
            let mut mods = Vec::new();
            if is_async {
                mods.push("async".to_owned());
            }
            let sym_label = owner
                .as_ref()
                .map_or_else(|| name.to_owned(), |o| format!("{o}::{name}"));
            for (p_lo, p_hi) in split_top_level(tokens, lp + 1, rp, ',') {
                if let Some(pname) = tokens.get(p_lo).and_then(|t| t.ident())
                    && let Some(c) = (p_lo + 1..p_hi).find(|&k| tokens[k].is_punct(':'))
                {
                    let pty = lexed.slice_text(c + 1, p_hi);
                    ctx.record_var_type(&sym_label, pname, &short_type(pty));
                }
            }
            for (dname, dtext, dargs, dline) in pending_decs.drain(..) {
                let short_d = dname.rsplit('.').next().unwrap_or(&dname);
                if matches!(
                    short_d,
                    "staticmethod"
                        | "classmethod"
                        | "property"
                        | "abstractmethod"
                        | "final"
                        | "override"
                ) {
                    mods.push(short_d.to_owned());
                }
                emit_semantic_from_attribute(ctx, &dname, &dtext, &dargs, Some(&sym_label), &[], dline);
            }
            let body_fp = if block_start < block_end {
                lexed.body_fingerprint(tokens[block_start].start, tokens[block_end - 1].end)
            } else {
                String::new()
            };
            if name.starts_with("test_") {
                ctx.test_lines.insert(line);
            }
            let decl_kind = if mods.iter().any(|m| m == "property") {
                "property"
            } else if owner.is_some() {
                "method"
            } else {
                "function"
            };
            ctx.push_decl(
                decl_kind,
                name,
                owner,
                line,
                e_line,
                Some(py_visibility(name)),
                mods,
                sig.clone(),
                sig,
                body_fp,
            );
            fn_indent_stack.push(indent);
            i = block_start;
            continue;
        }

        // Properties inside class (`self.x = ...` or `attr: Type = ...`) & top-level `UPPER_CASE = ...`
        if let Some((owner, _)) = class_stack.last() {
            if tokens[i].is_kw("self")
                && tokens.get(i + 1).is_some_and(|t| t.is_punct('.'))
                && let Some(prop) = tokens.get(i + 2).and_then(|t| t.ident())
                && tokens.get(i + 3).is_some_and(|t| t.is_punct('=') || t.is_punct(':'))
            {
                let mut end = i + 3;
                while end < tokens.len() && tokens[end].line == line {
                    end += 1;
                }
                if tokens[i + 3].is_punct('=')
                    && let Some(rhs_var) = tokens.get(i + 4).and_then(|t| t.ident())
                    && let Some(ty) = ctx.lookup_var_type(Some(&format!("{owner}::__init__")), rhs_var)
                {
                    ctx.record_var_type(owner, prop, &ty);
                }
                let sig = normalize_sig(lexed.slice_text(i, end));
                let already_declared = ctx.declarations.iter().any(|d| {
                    d.entity.kind == "property"
                        && d.entity.parent.as_deref() == Some(owner.as_str())
                        && d.entity.name == prop
                });
                if !already_declared {
                    ctx.push_decl(
                        "property",
                        prop,
                        Some(owner.clone()),
                        line,
                        line,
                        Some(py_visibility(prop)),
                        Vec::new(),
                        sig.clone(),
                        sig,
                        String::new(),
                    );
                }
            } else if fn_indent_stack.is_empty()
                && let Some(prop) = tokens[i].ident()
                && tokens.get(i + 1).is_some_and(|t| t.is_punct(':') || t.is_punct('='))
            {
                let mut end = i + 1;
                while end < tokens.len() && tokens[end].line == line {
                    end += 1;
                }
                let sig = normalize_sig(lexed.slice_text(i, end));
                let already_declared = ctx.declarations.iter().any(|d| {
                    d.entity.kind == "property"
                        && d.entity.parent.as_deref() == Some(owner.as_str())
                        && d.entity.name == prop
                });
                if !already_declared {
                    ctx.push_decl(
                        "property",
                        prop,
                        Some(owner.clone()),
                        line,
                        line,
                        Some(py_visibility(prop)),
                        Vec::new(),
                        sig.clone(),
                        sig,
                        String::new(),
                    );
                }
            }
        } else if fn_indent_stack.is_empty()
            && let Some(name) = tokens[i].ident()
            && super::is_upper_snake(name)
            && tokens.get(i + 1).is_some_and(|t| t.is_punct('='))
        {
            let mut end = i + 2;
            while end < tokens.len() && tokens[end].line == line {
                end += 1;
            }
            let sig = normalize_sig(lexed.slice_text(i, i + 1));
            let body_fp =
                lexed.body_fingerprint(tokens[i + 2].start, tokens[end.saturating_sub(1)].end);
            ctx.push_decl(
                "const",
                name,
                None,
                line,
                line,
                Some(py_visibility(name)),
                Vec::new(),
                sig.clone(),
                sig,
                body_fp,
            );
        }

        pending_decs.clear();
        i += 1;
    }

    scan_calls_and_refs(ctx, lexed, 0, tokens.len(), true);
}

// =========================================================================
// 5. Go
// =========================================================================

fn go_visibility(name: &str) -> String {
    if is_upper_ident(name) {
        "public".to_owned()
    } else {
        "package".to_owned()
    }
}

fn extract_go(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    let tokens = &lexed.tokens;
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].is_kw("package")
            && i + 1 < tokens.len()
            && let Some(pkg) = tokens[i + 1].ident()
        {
            ctx.namespace = Some(pkg.to_owned());
            i += 2;
            continue;
        }
        if tokens[i].is_kw("import") {
            let line = tokens[i].line;
            if tokens.get(i + 1).is_some_and(|t| t.is_punct('(')) {
                let rp = matching_delim(tokens, i + 1, '(', ')');
                for s in collect_string_args(tokens, i + 2, rp) {
                    ctx.push_import(AstImport {
                        path: s,
                        symbols: Vec::new(),
                        alias: None,
                        is_wildcard: false,
                        is_reexport: false,
                        reexport_visibility: None,
                        kind: "import".to_owned(),
                        line,
                        evidence: clip_str(lexed.slice_text(i, rp + 1), 160),
                    });
                }
                i = rp + 1;
            } else if i + 1 < tokens.len() {
                let mut end = i + 1;
                while end < tokens.len() && tokens[end].line == line {
                    end += 1;
                }
                for s in collect_string_args(tokens, i + 1, end) {
                    ctx.push_import(AstImport {
                        path: s,
                        symbols: Vec::new(),
                        alias: None,
                        is_wildcard: false,
                        is_reexport: false,
                        reexport_visibility: None,
                        kind: "import".to_owned(),
                        line,
                        evidence: clip_str(lexed.slice_text(i, end), 160),
                    });
                }
                i = end;
            } else {
                i += 1;
            }
            continue;
        }
        if tokens[i].is_kw("type")
            && i + 2 < tokens.len()
            && let Some(name) = tokens[i + 1].ident()
        {
            let kind = match tokens[i + 2].ident() {
                Some("struct") => "struct",
                Some("interface") => "interface",
                _ => "type",
            };
            let line = tokens[i + 1].line;
            let bo = (i + 2..tokens.len())
                .find(|&k| tokens[k].is_punct('{') && tokens[k].line >= line);
            let bc = if let Some(b) = bo
                && tokens
                    .get(i + 2)
                    .is_some_and(|t| matches!(t.ident(), Some("struct" | "interface")))
            {
                matching_delim(tokens, b, '{', '}')
            } else {
                i + 2
            };
            let sig = normalize_sig(lexed.slice_text(i, bo.unwrap_or(bc) + 1));
            ctx.push_decl(
                kind,
                name,
                None,
                line,
                tokens[bc].line,
                Some(go_visibility(name)),
                Vec::new(),
                sig.clone(),
                sig,
                String::new(),
            );
            i = bc + 1;
            continue;
        }
        if tokens[i].is_kw("func") {
            let mut cur = i + 1;
            let mut owner: Option<String> = None;
            if tokens.get(cur).is_some_and(|t| t.is_punct('(')) {
                let rp = matching_delim(tokens, cur, '(', ')');
                let recv = short_type(
                    lexed
                        .slice_text(cur + 1, rp)
                        .split_whitespace()
                        .last()
                        .unwrap_or(""),
                );
                if !recv.is_empty() {
                    owner = Some(recv);
                }
                cur = rp + 1;
            }
            if let Some(name) = tokens.get(cur).and_then(|t| t.ident())
                && let Some(bo) = (cur + 1..tokens.len()).find(|&k| tokens[k].is_punct('{'))
            {
                let bc = matching_delim(tokens, bo, '{', '}');
                let line = tokens[cur].line;
                let sig =
                    normalize_sig(&lexed.header_without_comments(tokens[i].start, tokens[bo].start));
                let body_fp = lexed.body_fingerprint(tokens[bo].start, tokens[bc].end);
                ctx.push_decl(
                    if owner.is_some() { "method" } else { "function" },
                    name,
                    owner,
                    line,
                    tokens[bc].line,
                    Some(go_visibility(name)),
                    Vec::new(),
                    sig.clone(),
                    sig,
                    body_fp,
                );
                i = bc + 1;
                continue;
            }
        }
        i += 1;
    }
    scan_calls_and_refs(ctx, lexed, 0, tokens.len(), true);
}

// =========================================================================
// 6. C-Family (Java, C#, Kotlin, Swift, Scala) & Ruby
// =========================================================================

fn extract_c_family(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    walk_c_family_block(ctx, lexed, 0, lexed.tokens.len(), None, 0);
    scan_calls_and_refs(ctx, lexed, 0, lexed.tokens.len(), true);
}

fn find_c_family_line_or_semi_end(tokens: &[Token<'_>], start: usize, hi: usize) -> usize {
    let mut i = start;
    let start_line = tokens.get(start.saturating_sub(1)).map_or(1, |t| t.line);
    while i < hi {
        if tokens[i].line > start_line {
            return i;
        }
        if tokens[i].is_punct(';') || tokens[i].is_punct('}') || tokens[i].is_punct('{') {
            return i;
        }
        i += 1;
    }
    hi
}

fn walk_c_family_block(
    ctx: &mut ExtractCtx<'_>,
    lexed: &Lexed<'_>,
    lo: usize,
    hi: usize,
    cur_class: Option<&str>,
    fn_depth: usize,
) {
    let tokens = &lexed.tokens;
    let mut i = lo;
    while i < hi {
        let mut attrs = Vec::new();
        while i < hi {
            if tokens[i].is_punct('@') {
                let mut c = i + 1;
                while c < hi && matches!(tokens[c].kind, TokenKind::Ident(_) | TokenKind::Punct('.')) {
                    c += 1;
                }
                let name = lexed.slice_text(i + 1, c).to_owned();
                let mut sargs = Vec::new();
                if tokens.get(c).is_some_and(|t| t.is_punct('(')) {
                    let rp = matching_delim(tokens, c, '(', ')').min(hi - 1);
                    sargs = collect_string_args(tokens, c + 1, rp);
                    c = rp + 1;
                }
                attrs.push((name, lexed.slice_text(i, c).to_owned(), sargs, tokens[i].line));
                i = c;
            } else if matches!(tokens[i].kind, TokenKind::Punct('[') | TokenKind::Sym("#["))
                && ctx.grammar == AstGrammar::CSharp
            {
                let close = matching_delim(tokens, i, '[', ']').min(hi - 1);
                let text = lexed.slice_text(i + 1, close).to_owned();
                let name = text.split('(').next().unwrap_or(&text).trim().to_owned();
                let sargs = collect_string_args(tokens, i + 1, close);
                attrs.push((name, text, sargs, tokens[i].line));
                i = close + 1;
            } else {
                break;
            }
        }
        if i >= hi {
            break;
        }
        if tokens[i].is_punct('{') {
            let bc = matching_delim(tokens, i, '{', '}').min(hi - 1);
            walk_c_family_block(ctx, lexed, i + 1, bc, cur_class, fn_depth);
            i = bc + 1;
            continue;
        }
        if tokens[i].is_punct(';') || tokens[i].is_punct('}') {
            i += 1;
            continue;
        }
        if matches!(
            tokens[i].ident(),
            Some("import" | "using" | "package" | "namespace")
        ) && cur_class.is_none()
        {
            let kw = tokens[i].ident().unwrap_or("import");
            let end = if matches!(
                ctx.grammar,
                AstGrammar::Kotlin | AstGrammar::Swift | AstGrammar::Scala
            ) {
                find_c_family_line_or_semi_end(tokens, i + 1, hi)
            } else {
                find_semi_stmt_end(tokens, i + 1, hi)
            };
            let raw = lexed.slice_text(i + 1, end).replace(' ', "");
            if matches!(kw, "package" | "namespace") {
                ctx.namespace = Some(raw.clone());
                if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
                    let bc = matching_delim(tokens, end, '{', '}').min(hi - 1);
                    walk_c_family_block(ctx, lexed, end + 1, bc, None, fn_depth);
                    i = bc + 1;
                    continue;
                }
            } else {
                ctx.push_import(AstImport {
                    path: raw,
                    symbols: Vec::new(),
                    alias: None,
                    is_wildcard: false,
                    is_reexport: false,
                    reexport_visibility: None,
                    kind: kw.to_owned(),
                    line: tokens[i].line,
                    evidence: clip_str(lexed.slice_text(i, end.min(hi)), 160),
                });
            }
            i = if tokens.get(end).is_some_and(|t| t.is_punct(';')) {
                (end + 1).min(hi)
            } else {
                end.min(hi)
            };
            continue;
        }

        // Modifiers
        let stmt_lo = i;
        let mut vis: Option<String> = None;
        let mut mods = Vec::new();
        while i < hi {
            match tokens[i].ident() {
                Some(
                    v @ (
                        "open"
                        | "public"
                        | "protected"
                        | "private"
                        | "internal"
                        | "fileprivate"
                    ),
                ) => {
                    vis = Some(v.to_owned());
                    i += 1;
                }
                Some(
                    m @ (
                        "static"
                        | "final"
                        | "abstract"
                        | "sealed"
                        | "data"
                        | "async"
                        | "override"
                        | "suspend"
                        | "case"
                        | "readonly"
                    ),
                ) => {
                    mods.push(m.to_owned());
                    i += 1;
                }
                _ => break,
            }
        }
        if i >= hi {
            break;
        }

        // Type decl (`class`, `interface`, `struct`, `enum`, `trait`, `object`, `protocol`, `actor`, `record`)
        if let Some(
            dk @ (
                "class"
                | "interface"
                | "struct"
                | "enum"
                | "trait"
                | "object"
                | "protocol"
                | "actor"
                | "record"
            ),
        ) = tokens[i].ident()
            && fn_depth == 0
            && i + 1 < hi
            && let Some(name) = tokens[i + 1].ident()
        {
            let bo = (i + 2..hi).find(|&k| tokens[k].is_punct('{')).unwrap_or(hi);
            let bc = if bo < hi {
                matching_delim(tokens, bo, '{', '}').min(hi - 1)
            } else {
                hi - 1
            };
            let sig_end = tokens.get(bo).map_or(tokens[bc].end, |t| t.start);
            let sig = normalize_sig(&lexed.header_without_comments(tokens[stmt_lo].start, sig_end));
            let line = tokens[i + 1].line;
            for (aname, atext, sargs, aline) in &attrs {
                emit_semantic_from_attribute(ctx, aname, atext, sargs, Some(name), &[], *aline);
            }
            ctx.push_decl(
                dk,
                name,
                None,
                line,
                tokens[bc].line,
                Some(vis.unwrap_or_else(|| "public".to_owned())),
                mods,
                sig.clone(),
                strip_leading_words(&sig, &["open", "public", "protected", "private", "internal"]),
                String::new(),
            );
            // Heritage (`extends`, `implements`, `:`)
            let hdr_slice = lexed.slice_text(i + 2, bo);
            if hdr_slice.contains("extends") || hdr_slice.contains("implements") || hdr_slice.contains(':') {
                for seg in hdr_slice.split([',', ':']).skip(1) {
                    let clean = short_type(
                        seg.trim_start_matches("extends ")
                            .trim_start_matches("implements "),
                    );
                    if !clean.is_empty() && !is_builtin_type(&clean) {
                        ctx.push_inheritance(name, &clean, "extends", line, hdr_slice);
                    }
                }
            }
            if bo < bc {
                walk_c_family_block(ctx, lexed, bo + 1, bc, Some(name), fn_depth);
            }
            i = (bc + 1).min(hi);
            continue;
        }

        // Methods / Functions vs Class Fields
        if fn_depth == 0 {
            let end = find_stmt_end(tokens, i, hi);
            let lp_opt = (i..end).find(|&k| tokens[k].is_punct('('));
            if let Some(lp) = lp_opt
                && lp > i
                && let Some(mname) = tokens[lp - 1].ident()
                && !matches!(mname, "if" | "for" | "while" | "switch" | "catch" | "when")
            {
                let has_brace = tokens.get(end).is_some_and(|t| t.is_punct('{'));
                let bc = if has_brace {
                    matching_delim(tokens, end, '{', '}').min(hi - 1)
                } else {
                    end.min(hi - 1)
                };
                let line = tokens[lp - 1].line;
                let sig_end = if has_brace {
                    tokens[end].start
                } else {
                    tokens[bc].end
                };
                let sig = normalize_sig(
                    lexed
                        .header_without_comments(tokens[stmt_lo].start, sig_end)
                        .trim_end_matches(';'),
                );
                let owner = cur_class.map(str::to_owned);
                let sym_label = owner
                    .as_ref()
                    .map_or_else(|| mname.to_owned(), |o| format!("{o}::{mname}"));
                for (aname, atext, sargs, aline) in &attrs {
                    emit_semantic_from_attribute(
                        ctx,
                        aname,
                        atext,
                        sargs,
                        Some(&sym_label),
                        &[],
                        *aline,
                    );
                }
                ctx.push_decl(
                    if owner.is_some() { "method" } else { "function" },
                    mname,
                    owner,
                    line,
                    tokens[bc].line,
                    Some(vis.unwrap_or_else(|| "public".to_owned())),
                    mods,
                    sig.clone(),
                    strip_leading_words(
                        &sig,
                        &["open", "public", "protected", "private", "internal"],
                    ),
                    if has_brace {
                        lexed.body_fingerprint(tokens[end].start, tokens[bc].end)
                    } else {
                        String::new()
                    },
                );
                if has_brace {
                    walk_c_family_block(ctx, lexed, end + 1, bc, cur_class, fn_depth + 1);
                }
                i = (bc + 1).min(hi);
                continue;
            }
            if let Some(owner) = cur_class {
                let eq_or_colon = (i..end).find(|&k| tokens[k].is_punct('=') || tokens[k].is_punct(':'));
                let name_end = eq_or_colon.unwrap_or(end);
                if name_end > i
                    && let Some(fname) = tokens[name_end - 1].ident()
                {
                    let line = tokens[name_end - 1].line;
                    let sig = normalize_sig(
                        lexed
                            .header_without_comments(tokens[stmt_lo].start, tokens[name_end - 1].end)
                            .trim_end_matches(';'),
                    );
                    ctx.push_decl(
                        "field",
                        fname,
                        Some(owner.to_owned()),
                        line,
                        tokens[end.min(hi - 1)].line,
                        Some(vis.unwrap_or_else(|| "public".to_owned())),
                        mods,
                        sig.clone(),
                        strip_leading_words(
                            &sig,
                            &["open", "public", "protected", "private", "internal"],
                        ),
                        String::new(),
                    );
                }
            }
            if tokens.get(end).is_some_and(|t| t.is_punct('{')) {
                let bc = matching_delim(tokens, end, '{', '}').min(hi - 1);
                walk_c_family_block(ctx, lexed, end + 1, bc, cur_class, fn_depth);
                i = bc + 1;
            } else {
                i = (end + 1).min(hi);
            }
        } else {
            i += 1;
        }
    }
}

fn extract_ruby(ctx: &mut ExtractCtx<'_>, lexed: &Lexed<'_>) {
    let tokens = &lexed.tokens;
    let mut owners: Vec<Option<String>> = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let line = tokens[i].line;
        if matches!(tokens[i].ident(), Some("module" | "class")) && i + 1 < tokens.len() {
            if let Some(name) = tokens[i + 1].ident() {
                let kind = tokens[i].ident().unwrap_or("class");
                ctx.push_decl(
                    kind,
                    name,
                    None,
                    line,
                    line,
                    Some("public".to_owned()),
                    Vec::new(),
                    lexed.slice_text(i, i + 2).to_owned(),
                    lexed.slice_text(i, i + 2).to_owned(),
                    String::new(),
                );
                if tokens.get(i + 2).is_some_and(|t| t.is_punct('<'))
                    && let Some(parent) = tokens.get(i + 3).and_then(|t| t.ident())
                {
                    ctx.push_inheritance(
                        name,
                        parent,
                        "extends",
                        line,
                        lexed.slice_text(i, i + 4),
                    );
                }
                owners.push(Some(name.to_owned()));
                i += 2;
                continue;
            }
        } else if tokens[i].is_kw("def") && i + 1 < tokens.len() {
            if let Some(name) = tokens[i + 1].ident() {
                let parent = owners.iter().rev().flatten().next().cloned();
                ctx.push_decl(
                    if parent.is_some() { "method" } else { "function" },
                    name,
                    parent,
                    line,
                    line,
                    Some("public".to_owned()),
                    Vec::new(),
                    lexed.slice_text(i, i + 2).to_owned(),
                    lexed.slice_text(i, i + 2).to_owned(),
                    String::new(),
                );
                owners.push(None);
                i += 2;
                continue;
            }
        } else if matches!(
            tokens[i].ident(),
            Some("do" | "if" | "unless" | "case" | "begin" | "while")
        ) {
            owners.push(None);
        } else if tokens[i].is_kw("end") {
            owners.pop();
        } else if matches!(tokens[i].ident(), Some("require" | "require_relative"))
            && let Some(TokenKind::Str { unquoted, .. }) = tokens.get(i + 1).map(|t| &t.kind)
        {
            ctx.push_import(AstImport {
                path: unquoted.clone(),
                symbols: Vec::new(),
                alias: None,
                is_wildcard: false,
                is_reexport: false,
                reexport_visibility: None,
                kind: "require".to_owned(),
                line,
                evidence: clip_str(lexed.slice_text(i, i + 2), 160),
            });
        }
        i += 1;
    }
    scan_calls_and_refs(ctx, lexed, 0, tokens.len(), true);
}
