//! Pure-Rust multi-language lexer with comment stripping, string extraction,
//! and balanced body-fingerprint hashing (no C tree-sitter dependencies).

use super::{AstGrammar, unquote_literal};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TokenKind<'a> {
    Ident(&'a str),
    DollarIdent(&'a str),
    PrivateIdent(&'a str),
    Str { raw: &'a str, unquoted: String },
    Number(&'a str),
    Sym(&'static str),
    Punct(char),
}

#[derive(Clone, Debug)]
pub(super) struct Token<'a> {
    pub(super) kind: TokenKind<'a>,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) line: u32,
}

impl<'a> Token<'a> {
    pub(super) fn text<'s>(&self, source: &'s str) -> &'s str {
        &source[self.start..self.end]
    }

    pub(super) fn ident(&self) -> Option<&'a str> {
        match self.kind {
            TokenKind::Ident(s) => Some(s),
            _ => None,
        }
    }

    pub(super) fn is_kw(&self, kw: &str) -> bool {
        matches!(self.kind, TokenKind::Ident(s) if s == kw)
    }

    pub(super) fn is_punct(&self, ch: char) -> bool {
        matches!(self.kind, TokenKind::Punct(c) if c == ch)
    }

    pub(super) fn is_sym(&self, sym: &str) -> bool {
        matches!(self.kind, TokenKind::Sym(s) if s == sym)
    }
}

#[derive(Clone, Debug)]
pub(super) struct StringLit<'a> {
    pub(super) raw: &'a str,
    pub(super) unquoted: String,
    pub(super) line: u32,
}

pub(super) struct Lexed<'a> {
    pub(super) source: &'a str,
    #[allow(dead_code)]
    pub(super) grammar: AstGrammar,
    pub(super) tokens: Vec<Token<'a>>,
    pub(super) comment_spans: Vec<(usize, usize)>,
    pub(super) strings: Vec<StringLit<'a>>,
    pub(super) identifiers: BTreeSet<String>,
    line_starts: Vec<usize>,
    pub(super) line_indents: Vec<usize>,
}

impl<'a> Lexed<'a> {
    pub(super) fn line_of(&self, byte_pos: usize) -> u32 {
        match self.line_starts.binary_search(&byte_pos) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i.max(1) as u32,
        }
    }

    pub(super) fn indent_of_line(&self, line: u32) -> usize {
        self.line_indents
            .get(line.saturating_sub(1) as usize)
            .copied()
            .unwrap_or(0)
    }

    pub(super) fn header_without_comments(&self, start_byte: usize, end_byte: usize) -> String {
        let start = start_byte.min(self.source.len());
        let end = end_byte.min(self.source.len()).max(start);
        if start >= end {
            return String::new();
        }
        let mut raw = String::with_capacity(end - start);
        let mut cursor = start;
        for &(cs, ce) in &self.comment_spans {
            if ce <= start {
                continue;
            }
            if cs >= end {
                break;
            }
            let c_start = cs.max(start);
            let c_end = ce.min(end);
            if c_start > cursor
                && let Some(slice) = self.source.get(cursor..c_start)
            {
                raw.push_str(slice);
            }
            cursor = cursor.max(c_end);
            raw.push(' ');
        }
        if cursor < end
            && let Some(slice) = self.source.get(cursor..end)
        {
            raw.push_str(slice);
        }
        raw.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    pub(super) fn body_fingerprint(&self, start_byte: usize, end_byte: usize) -> String {
        if start_byte >= end_byte {
            return String::new();
        }
        let lo = self.tokens.partition_point(|t| t.start < start_byte);
        let mut tokens: Vec<&str> = Vec::new();
        for tok in &self.tokens[lo..] {
            if tok.end > end_byte {
                break;
            }
            let txt = tok.text(self.source).trim();
            if !txt.is_empty() {
                tokens.push(txt);
            }
        }
        if tokens.is_empty() {
            return String::new();
        }
        let joined = tokens.join(" ");
        format!("{:032x}", xxhash_rust::xxh3::xxh3_128(joined.as_bytes()))
    }

    pub(super) fn slice_text(&self, lo: usize, hi_excl: usize) -> &'a str {
        if lo >= hi_excl || lo >= self.tokens.len() {
            return "";
        }
        let start = self.tokens[lo].start;
        let end = self.tokens[hi_excl - 1].end;
        self.source.get(start..end).unwrap_or("")
    }
}

fn compute_line_metadata(source: &str) -> (Vec<usize>, Vec<usize>) {
    let mut starts = vec![0usize];
    let bytes = source.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' && idx + 1 <= bytes.len() {
            starts.push(idx + 1);
        }
    }
    let mut indents = Vec::with_capacity(starts.len());
    for (idx, &ls) in starts.iter().enumerate() {
        let le = starts
            .get(idx + 1)
            .map(|n| n.saturating_sub(1))
            .unwrap_or(bytes.len());
        let mut count = 0usize;
        let mut pos = ls;
        while pos < le {
            match bytes[pos] {
                b' ' => count += 1,
                b'\t' => count += 4,
                _ => break,
            }
            pos += 1;
        }
        indents.push(count);
    }
    (starts, indents)
}

fn is_ident_start(b: u8, grammar: AstGrammar) -> bool {
    b.is_ascii_alphabetic()
        || b == b'_'
        || (b == b'$'
            && matches!(
                grammar,
                AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript
            ))
}

fn is_ident_cont(b: u8, grammar: AstGrammar) -> bool {
    b.is_ascii_alphanumeric()
        || b == b'_'
        || (b == b'$'
            && matches!(
                grammar,
                AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript
            ))
}

fn char_len_at(source: &str, idx: usize) -> usize {
    source[idx..].chars().next().map_or(1, char::len_utf8)
}

fn scan_quoted(source: &str, quote_pos: usize, delim: u8) -> usize {
    let bytes = source.as_bytes();
    let mut i = quote_pos + 1;
    let allow_escape = delim != b'`' || !source.is_empty();
    while i < bytes.len() {
        let b = bytes[i];
        if allow_escape && b == b'\\' {
            i += 1;
            if i < bytes.len() {
                i += char_len_at(source, i);
            }
            continue;
        }
        if b == delim {
            return i + 1;
        }
        i += if b < 0x80 { 1 } else { char_len_at(source, i) };
    }
    bytes.len()
}

fn scan_triple_quoted(source: &str, start: usize, delim: &[u8; 3]) -> usize {
    let bytes = source.as_bytes();
    let mut i = start + 3;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 1;
            if i < bytes.len() {
                i += char_len_at(source, i);
            }
            continue;
        }
        if i + 3 <= bytes.len() && &bytes[i..i + 3] == delim {
            return i + 3;
        }
        i += if bytes[i] < 0x80 {
            1
        } else {
            char_len_at(source, i)
        };
    }
    bytes.len()
}

fn try_rust_raw_string(source: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut i = pos;
    if bytes.get(i) == Some(&b'b') || bytes.get(i) == Some(&b'c') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;
    let mut hashes = 0usize;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let content_start = i + 1;
    let mut cur = content_start;
    while cur < bytes.len() {
        if bytes[cur] == b'"' {
            let mut h = 0usize;
            while h < hashes && bytes.get(cur + 1 + h) == Some(&b'#') {
                h += 1;
            }
            if h == hashes {
                return Some((cur + 1 + hashes, content_start));
            }
        }
        cur += if bytes[cur] < 0x80 {
            1
        } else {
            char_len_at(source, cur)
        };
    }
    Some((bytes.len(), content_start))
}

fn try_rust_char_literal(source: &str, pos: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(pos) != Some(&b'\'') {
        return None;
    }
    let after = pos + 1;
    let b = *bytes.get(after)?;
    if b == b'\'' || b == b'\n' || b == b'\r' {
        return None;
    }
    let mut end = after;
    if b == b'\\' {
        end += 1;
        while end < bytes.len() && end - after <= 12 {
            if bytes[end] == b'\'' {
                return Some(end + 1);
            }
            if bytes[end] == b'\n' {
                return None;
            }
            end += char_len_at(source, end);
        }
        return None;
    }
    end += char_len_at(source, after);
    (bytes.get(end) == Some(&b'\'')).then_some(end + 1)
}

fn try_prefixed_string(source: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut i = pos;
    let mut len = 0usize;
    while len < 2 && i < bytes.len() {
        let ch = bytes[i].to_ascii_lowercase();
        if matches!(ch, b'r' | b'f' | b'b' | b'u') {
            i += 1;
            len += 1;
        } else {
            break;
        }
    }
    if len == 0 {
        return None;
    }
    let q = *bytes.get(i)?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    if i + 3 <= bytes.len() && bytes[i..i + 3] == [q, q, q] {
        let end = scan_triple_quoted(source, i, &[q, q, q]);
        return Some((end, i));
    }
    let end = scan_quoted(source, i, q);
    Some((end, i))
}

fn try_js_regex_end(source: &str, pos: usize, prev: Option<&Token<'_>>) -> Option<usize> {
    let allow = match prev {
        None => true,
        Some(t) => {
            matches!(
                t.kind,
                TokenKind::Punct(
                    '=' | '(' | '[' | ',' | ':' | '!' | '?' | ';' | '{' | '&' | '|'
                ) | TokenKind::Sym("=>")
                    | TokenKind::Sym("==")
                    | TokenKind::Sym("!=")
                    | TokenKind::Sym("===")
                    | TokenKind::Sym("!==")
            ) || t.is_kw("return")
                || t.is_kw("throw")
                || t.is_kw("case")
                || t.is_kw("await")
                || t.is_kw("typeof")
        }
    };
    if !allow {
        return None;
    }
    let bytes = source.as_bytes();
    if bytes.get(pos) != Some(&b'/') || matches!(bytes.get(pos + 1), Some(b'/' | b'*')) {
        return None;
    }
    let mut i = pos + 1;
    let mut in_class = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' || b == b'\r' {
            return None;
        }
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'[' {
            in_class = true;
        } else if b == b']' {
            in_class = false;
        } else if b == b'/' && !in_class {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            return Some(i);
        }
        i += if b < 0x80 { 1 } else { char_len_at(source, i) };
    }
    None
}

fn try_php_heredoc(source: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    if !bytes.get(pos..pos + 3).is_some_and(|s| s == b"<<<") {
        return None;
    }
    let mut i = pos + 3;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let mut quote = None;
    if matches!(bytes.get(i), Some(b'\'' | b'"')) {
        quote = Some(bytes[i]);
        i += 1;
    }
    let label_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    let label = &source[label_start..i];
    if label.is_empty() {
        return None;
    }
    if let Some(q) = quote {
        if bytes.get(i) != Some(&q) {
            return None;
        }
        i += 1;
    }
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    let body_start = (i + 1).min(bytes.len());
    let mut line_pos = body_start;
    while line_pos < bytes.len() {
        let line_end = bytes[line_pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| line_pos + p)
            .unwrap_or(bytes.len());
        let trimmed = source[line_pos..line_end].trim().trim_end_matches(';');
        if trimmed == label {
            return Some((line_end, body_start));
        }
        line_pos = (line_end + 1).min(bytes.len());
    }
    Some((bytes.len(), body_start))
}

pub(super) fn lex<'a>(grammar: AstGrammar, source: &'a str) -> Lexed<'a> {
    let (line_starts, line_indents) = compute_line_metadata(source);
    let mut lexed = Lexed {
        source,
        grammar,
        tokens: Vec::with_capacity(source.len() / 6 + 16),
        comment_spans: Vec::new(),
        strings: Vec::new(),
        identifiers: BTreeSet::new(),
        line_starts,
        line_indents,
    };
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    let supports_slash_comments = !matches!(grammar, AstGrammar::Python | AstGrammar::Ruby);
    let supports_hash_comments = matches!(
        grammar,
        AstGrammar::Python | AstGrammar::Ruby | AstGrammar::Php
    );
    let supports_triple = matches!(
        grammar,
        AstGrammar::Python
            | AstGrammar::Java
            | AstGrammar::Kotlin
            | AstGrammar::Swift
            | AstGrammar::Scala
    );

    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Skip PHP open/close tags cleanly
        if grammar == AstGrammar::Php && b == b'<' && bytes.get(i + 1) == Some(&b'?') {
            if bytes.get(i..i + 5) == Some(b"<?php") {
                i += 5;
                continue;
            } else if bytes.get(i..i + 3) == Some(b"<?=") {
                i += 3;
                continue;
            }
            i += 2;
            continue;
        }
        if grammar == AstGrammar::Php && b == b'?' && bytes.get(i + 1) == Some(&b'>') {
            i += 2;
            while i < len {
                if bytes[i] == b'<' && bytes.get(i + 1) == Some(&b'?') {
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Line comment `//`
        if supports_slash_comments && b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            lexed.comment_spans.push((start, i));
            continue;
        }

        // Block comment `/* ... */`
        if supports_slash_comments && b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += if bytes[i] < 0x80 {
                    1
                } else {
                    char_len_at(source, i)
                };
            }
            i = (i + 2).min(len);
            lexed.comment_spans.push((start, i));
            continue;
        }

        // Attributes vs `#` comments
        if b == b'#' {
            if matches!(grammar, AstGrammar::Rust | AstGrammar::Php) && bytes.get(i + 1) == Some(&b'[')
            {
                let line = lexed.line_of(i);
                lexed.tokens.push(Token {
                    kind: TokenKind::Sym("#["),
                    start: i,
                    end: i + 2,
                    line,
                });
                i += 2;
                continue;
            }
            if grammar == AstGrammar::Rust
                && bytes.get(i + 1) == Some(&b'!')
                && bytes.get(i + 2) == Some(&b'[')
            {
                let line = lexed.line_of(i);
                lexed.tokens.push(Token {
                    kind: TokenKind::Sym("#!["),
                    start: i,
                    end: i + 3,
                    line,
                });
                i += 3;
                continue;
            }
            if supports_hash_comments {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
                lexed.comment_spans.push((start, i));
                continue;
            }
            if matches!(
                grammar,
                AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript
            ) && bytes.get(i + 1).is_some_and(|n| is_ident_start(*n, grammar))
            {
                let start = i;
                i += 1;
                while i < len && is_ident_cont(bytes[i], grammar) {
                    i += 1;
                }
                let raw = &source[start..i];
                let line = lexed.line_of(start);
                lexed.identifiers.insert(raw.trim_start_matches('#').to_owned());
                lexed.tokens.push(Token {
                    kind: TokenKind::PrivateIdent(raw),
                    start,
                    end: i,
                    line,
                });
                continue;
            }
        }

        // JS/TS regex literal `/.../`
        if matches!(
            grammar,
            AstGrammar::TypeScript | AstGrammar::Tsx | AstGrammar::JavaScript
        ) && b == b'/'
            && let Some(end) = try_js_regex_end(source, i, lexed.tokens.last())
        {
            let line = lexed.line_of(i);
            let raw = &source[i..end];
            lexed.tokens.push(Token {
                kind: TokenKind::Str {
                    raw,
                    unquoted: raw.to_owned(),
                },
                start: i,
                end,
                line,
            });
            i = end;
            continue;
        }

        // PHP heredoc/nowdoc
        if grammar == AstGrammar::Php
            && b == b'<'
            && let Some((end, body_start)) = try_php_heredoc(source, i)
        {
            let line = lexed.line_of(i);
            let raw = &source[i..end];
            let unquoted = source[body_start..end].to_owned();
            lexed.strings.push(StringLit {
                raw,
                unquoted: unquoted.clone(),
                line,
            });
            lexed.tokens.push(Token {
                kind: TokenKind::Str { raw, unquoted },
                start: i,
                end,
                line,
            });
            i = end;
            continue;
        }

        // Rust raw/byte string literals (`r#"..."#`, `b"..."`, `br#"..."#`)
        if grammar == AstGrammar::Rust {
            if let Some((end, content_start)) = try_rust_raw_string(source, i) {
                let line = lexed.line_of(i);
                let raw = &source[i..end];
                let trimmed = raw[content_start - i..].trim_end_matches('#').trim_end_matches('"');
                let unquoted = trimmed.to_owned();
                lexed.strings.push(StringLit {
                    raw,
                    unquoted: unquoted.clone(),
                    line,
                });
                lexed.tokens.push(Token {
                    kind: TokenKind::Str { raw, unquoted },
                    start: i,
                    end,
                    line,
                });
                i = end;
                continue;
            }
            if (b == b'b' || b == b'c') && bytes.get(i + 1) == Some(&b'"') {
                let end = scan_quoted(source, i + 1, b'"');
                let line = lexed.line_of(i);
                let raw = &source[i..end];
                let unquoted = unquote_literal(&source[i + 1..end]).unwrap_or_default();
                lexed.strings.push(StringLit {
                    raw,
                    unquoted: unquoted.clone(),
                    line,
                });
                lexed.tokens.push(Token {
                    kind: TokenKind::Str { raw, unquoted },
                    start: i,
                    end,
                    line,
                });
                i = end;
                continue;
            }
            if b == b'\'' {
                let line = lexed.line_of(i);
                if let Some(end) = try_rust_char_literal(source, i) {
                    let raw = &source[i..end];
                    lexed.tokens.push(Token {
                        kind: TokenKind::Str {
                            raw,
                            unquoted: unquote_literal(raw).unwrap_or_default(),
                        },
                        start: i,
                        end,
                        line,
                    });
                    i = end;
                    continue;
                }
                // Rust lifetime `'a` or label: emit Punct('\'') so the ident follows cleanly
                lexed.tokens.push(Token {
                    kind: TokenKind::Punct('\''),
                    start: i,
                    end: i + 1,
                    line,
                });
                i += 1;
                continue;
            }
        }

        // Python prefixed string (`r"..."`, `f"""..."""`, `b'...'`)
        if grammar == AstGrammar::Python
            && let Some((end, quote_pos)) = try_prefixed_string(source, i)
        {
            let line = lexed.line_of(i);
            let raw = &source[i..end];
            let unquoted = unquote_literal(&source[quote_pos..end]).unwrap_or_default();
            lexed.strings.push(StringLit {
                raw,
                unquoted: unquoted.clone(),
                line,
            });
            lexed.tokens.push(Token {
                kind: TokenKind::Str { raw, unquoted },
                start: i,
                end,
                line,
            });
            i = end;
            continue;
        }

        // Quoted strings
        if b == b'"' || b == b'\'' || b == b'`' {
            let line = lexed.line_of(i);
            let end = if supports_triple
                && b != b'`'
                && i + 3 <= len
                && bytes[i..i + 3] == [b, b, b]
            {
                scan_triple_quoted(source, i, &[b, b, b])
            } else {
                scan_quoted(source, i, b)
            };
            let raw = &source[i..end];
            let unquoted = unquote_literal(raw).unwrap_or_default();
            lexed.strings.push(StringLit {
                raw,
                unquoted: unquoted.clone(),
                line,
            });
            lexed.tokens.push(Token {
                kind: TokenKind::Str { raw, unquoted },
                start: i,
                end,
                line,
            });
            i = end;
            continue;
        }

        // PHP `$var`
        if grammar == AstGrammar::Php
            && b == b'$'
            && bytes
                .get(i + 1)
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_')
        {
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let raw = &source[start..i];
            let line = lexed.line_of(start);
            lexed.identifiers.insert(raw.to_owned());
            lexed.identifiers.insert(raw[1..].to_owned());
            lexed.tokens.push(Token {
                kind: TokenKind::DollarIdent(raw),
                start,
                end: i,
                line,
            });
            continue;
        }

        // Standard identifiers
        if is_ident_start(b, grammar) {
            let start = i;
            i += 1;
            while i < len && is_ident_cont(bytes[i], grammar) {
                i += 1;
            }
            let raw = &source[start..i];
            let line = lexed.line_of(start);
            lexed.identifiers.insert(raw.to_owned());
            lexed.tokens.push(Token {
                kind: TokenKind::Ident(raw),
                start,
                end: i,
                line,
            });
            continue;
        }

        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || (bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)))
            {
                i += 1;
            }
            let raw = &source[start..i];
            let line = lexed.line_of(start);
            lexed.tokens.push(Token {
                kind: TokenKind::Number(raw),
                start,
                end: i,
                line,
            });
            continue;
        }

        // Multi-char symbols
        let line = lexed.line_of(i);
        let sym3 = bytes.get(i..i + 3).and_then(|s| match s {
            b"..." => Some("..."),
            b"===" => Some("==="),
            b"!==" => Some("!=="),
            b"?->" => Some("?->"),
            _ => None,
        });
        if let Some(sym) = sym3 {
            lexed.tokens.push(Token {
                kind: TokenKind::Sym(sym),
                start: i,
                end: i + 3,
                line,
            });
            i += 3;
            continue;
        }
        let sym2 = bytes.get(i..i + 2).and_then(|s| match s {
            b"::" => Some("::"),
            b"->" => Some("->"),
            b"=>" => Some("=>"),
            b"?." => Some("?."),
            b":=" => Some(":="),
            b"<=" => Some("<="),
            b">=" => Some(">="),
            b"==" => Some("=="),
            b"!=" => Some("!="),
            b"&&" => Some("&&"),
            b"||" => Some("||"),
            _ => None,
        });
        if let Some(sym) = sym2 {
            lexed.tokens.push(Token {
                kind: TokenKind::Sym(sym),
                start: i,
                end: i + 2,
                line,
            });
            i += 2;
            continue;
        }

        // Single-char punctuation (skip non-ASCII safely if it appears outside comments/strings)
        if b < 0x80 {
            lexed.tokens.push(Token {
                kind: TokenKind::Punct(b as char),
                start: i,
                end: i + 1,
                line,
            });
            i += 1;
        } else {
            i += char_len_at(source, i);
        }
    }

    lexed
}
