// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A small hand-written lexer for MySQL SQL.
//!
//! It recognizes the token vocabulary needed by the supported grammar:
//! identifiers (bare, backtick-, and double-quoted), string and numeric
//! literals, and the handful of punctuation characters used by
//! `CREATE TABLE`. Comments (`-- ...`, `# ...`, and `/* ... */`) and whitespace
//! are skipped.

use crate::error::{ParseError, Result};
use crate::token::Token;

/// Lexes a byte slice into a vector of `(token, byte offset)` pairs.
pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    /// Creates a lexer over `input`.
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Tokenizes the whole input.
    pub fn tokenize(mut self) -> Result<Vec<(Token, usize)>> {
        let mut tokens = Vec::new();
        while let Some(tok) = self.next_token()? {
            tokens.push(tok);
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.input.get(self.pos + n).copied()
    }

    fn next_token(&mut self) -> Result<Option<(Token, usize)>> {
        self.skip_trivia();
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let token = match c {
            b'(' => self.single(Token::LParen),
            b')' => self.single(Token::RParen),
            b',' => self.single(Token::Comma),
            // `.5` is a numeric literal; a bare `.` (e.g. `t.col`) is the
            // qualifier dot.
            b'.' if matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()) => self.read_number(),
            b'.' => self.single(Token::Dot),
            b';' => self.single(Token::Semicolon),
            b'=' => self.single(Token::Eq),
            b'*' => self.single(Token::Star),
            b'<' => match self.peek_at(1) {
                Some(b'=') => self.double(Token::Le),
                Some(b'>') => self.double(Token::Ne),
                _ => self.single(Token::Lt),
            },
            b'>' => match self.peek_at(1) {
                Some(b'=') => self.double(Token::Ge),
                _ => self.single(Token::Gt),
            },
            b'!' => match self.peek_at(1) {
                Some(b'=') => self.double(Token::Ne),
                _ => self.single(Token::Other('!')),
            },
            b'-' => self.single(Token::Minus),
            b'+' => self.single(Token::Plus),
            b'`' => self.read_delimited(b'`', "identifier")?,
            b'"' => self.read_delimited(b'"', "identifier")?,
            b'\'' => self.read_string()?,
            c if c.is_ascii_digit() => self.read_number(),
            c if is_ident_start(c) => self.read_word(),
            other => self.single(Token::Other(other as char)),
        };
        Ok(Some((token, start)))
    }

    fn single(&mut self, token: Token) -> Token {
        self.pos += 1;
        token
    }

    fn double(&mut self, token: Token) -> Token {
        self.pos += 2;
        token
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => self.pos += 1,
                // `# ...` line comment (MySQL extension).
                Some(b'#') => self.skip_line(),
                // `-- ...` line comment (requires whitespace after `--`).
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    self.pos += 2;
                    self.skip_line();
                }
                // `/* ... */` block comment.
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.pos += 2;
                    while self.pos < self.input.len() {
                        if self.peek() == Some(b'*') && self.peek_at(1) == Some(b'/') {
                            self.pos += 2;
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == b'\n' {
                break;
            }
        }
    }

    fn read_word(&mut self) -> Token {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_ident_cont(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        // Identifier bytes are ASCII by construction, so this is valid UTF-8.
        let s = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        Token::Word(s)
    }

    fn read_number(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        // Optional exponent.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let s = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        Token::Num(s)
    }

    /// Reads a delimited identifier (backtick or double quote). The delimiter is
    /// escaped by doubling it.
    fn read_delimited(&mut self, delim: u8, kind: &'static str) -> Result<Token> {
        let start = self.pos;
        self.pos += 1; // opening delimiter

        // Accumulate raw bytes and decode as UTF-8 at the end, so multi-byte
        // characters survive (pushing each byte `as char` would mangle them).
        let mut value: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError::Unterminated {
                        offset: start,
                        kind,
                    })
                }
                Some(c) if c == delim => {
                    if self.peek_at(1) == Some(delim) {
                        value.push(delim);
                        self.pos += 2;
                    } else {
                        self.pos += 1; // closing delimiter
                        return Ok(Token::QuotedIdent(bytes_to_string(value)));
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// Reads a single-quoted string literal. Handles `''` doubling and the
    /// common backslash escapes.
    fn read_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.pos += 1; // opening quote

        // Accumulate raw bytes and decode as UTF-8 at the end, so multi-byte
        // characters survive (pushing each byte `as char` would mangle them).
        let mut value: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError::Unterminated {
                        offset: start,
                        kind: "string literal",
                    })
                }
                Some(b'\'') => {
                    if self.peek_at(1) == Some(b'\'') {
                        value.push(b'\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1; // closing quote
                        return Ok(Token::Str(bytes_to_string(value)));
                    }
                }
                Some(b'\\') => {
                    self.pos += 1;
                    // MySQL's default backslash escapes (NO_BACKSLASH_ESCAPES off).
                    match self.peek() {
                        Some(b'0') => value.push(0),
                        Some(b'b') => value.push(0x08), // backspace
                        Some(b'n') => value.push(b'\n'),
                        Some(b'r') => value.push(b'\r'),
                        Some(b't') => value.push(b'\t'),
                        Some(b'Z') => value.push(0x1A), // ctrl-Z
                        Some(b'\\') => value.push(b'\\'),
                        Some(b'\'') => value.push(b'\''),
                        Some(b'"') => value.push(b'"'),
                        // `\%` and `\_` keep the backslash: MySQL preserves them
                        // so they survive into a LIKE pattern as escaped wildcards.
                        Some(b'%') => value.extend_from_slice(b"\\%"),
                        Some(b'_') => value.extend_from_slice(b"\\_"),
                        // Any other escaped character is itself (backslash dropped).
                        Some(other) => value.push(other),
                        None => {
                            return Err(ParseError::Unterminated {
                                offset: start,
                                kind: "string literal",
                            })
                        }
                    }
                    self.pos += 1;
                }
                Some(c) => {
                    value.push(c);
                    self.pos += 1;
                }
            }
        }
    }
}

/// Decodes accumulated string-literal bytes as UTF-8, replacing any invalid
/// sequences (string tokens must be valid Rust `String`s).
fn bytes_to_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_string(sql: &str) -> String {
        let tokens = Lexer::new(sql.as_bytes()).tokenize().unwrap();
        tokens
            .into_iter()
            .find_map(|(t, _)| match t {
                Token::Str(s) | Token::QuotedIdent(s) => Some(s),
                _ => None,
            })
            .expect("a string token")
    }

    #[test]
    fn multibyte_string_literals_survive() {
        // UTF-8 multi-byte characters are preserved, not split into bytes.
        assert_eq!(first_string("SELECT 'café'"), "café");
        assert_eq!(first_string("SELECT 'naïve résumé'"), "naïve résumé");
        // Backtick-quoted identifiers too.
        assert_eq!(first_string("SELECT `tëst`"), "tëst");
        // Escapes and doubled quotes still work.
        assert_eq!(first_string(r"SELECT 'a\tb'"), "a\tb");
        assert_eq!(first_string("SELECT 'it''s'"), "it's");
    }

    fn tokens(sql: &str) -> Vec<Token> {
        Lexer::new(sql.as_bytes())
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn leading_dot_float_literal() {
        // `.5` is one numeric literal.
        assert_eq!(
            tokens("SELECT .5"),
            vec![Token::Word("SELECT".into()), Token::Num(".5".into())]
        );
        // The dot in a qualified name is still a Dot token.
        assert_eq!(
            tokens("a.b"),
            vec![Token::Word("a".into()), Token::Dot, Token::Word("b".into())]
        );
        // `tbl.*` keeps the dot too.
        assert_eq!(
            tokens("t.*"),
            vec![Token::Word("t".into()), Token::Dot, Token::Star]
        );
    }

    #[test]
    fn mysql_backslash_escapes() {
        // `\%` and `\_` keep the backslash; other C escapes apply.
        assert_eq!(first_string(r"SELECT '\%'"), r"\%");
        assert_eq!(first_string(r"SELECT '\_'"), r"\_");
        assert_eq!(first_string(r"SELECT '\b'"), "\u{0008}"); // backspace
        assert_eq!(first_string(r"SELECT '\Z'"), "\u{001A}"); // ctrl-Z
        assert_eq!(first_string(r"SELECT '\0'"), "\0");
        // An unknown escape drops the backslash.
        assert_eq!(first_string(r"SELECT 'a\xb'"), "axb");
    }
}
