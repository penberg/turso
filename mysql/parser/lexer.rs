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
                // `<=>` (NULL-safe equality) must be checked before `<=`.
                Some(b'=') if self.peek_at(2) == Some(b'>') => self.triple(Token::Spaceship),
                Some(b'=') => self.double(Token::Le),
                Some(b'>') => self.double(Token::Ne),
                Some(b'<') => self.double(Token::ShiftLeft),
                _ => self.single(Token::Lt),
            },
            b'>' => match self.peek_at(1) {
                Some(b'=') => self.double(Token::Ge),
                Some(b'>') => self.double(Token::ShiftRight),
                _ => self.single(Token::Gt),
            },
            b'!' => match self.peek_at(1) {
                Some(b'=') => self.double(Token::Ne),
                _ => self.single(Token::Other('!')),
            },
            // `&&` is logical AND; a single `&` is the bitwise operator, which
            // lexes as `Other('&')` like the other bitwise characters.
            b'&' if self.peek_at(1) == Some(b'&') => self.double(Token::AmpAmp),
            // `->>` (JSON extract-and-unquote) before `->` (JSON extract) before
            // a bare `-`. (A `--` line comment was already skipped as trivia.)
            b'-' => match self.peek_at(1) {
                Some(b'>') if self.peek_at(2) == Some(b'>') => self.triple(Token::ArrowDouble),
                Some(b'>') => self.double(Token::Arrow),
                _ => self.single(Token::Minus),
            },
            b'+' => self.single(Token::Plus),
            b'?' => self.single(Token::Param),
            b'`' => self.read_delimited(b'`', "identifier")?,
            b'"' => self.read_delimited(b'"', "identifier")?,
            b'\'' => self.read_string()?,
            // MySQL hex-string literal `X'41'` / `x'41'` — checked before the
            // identifier path, which `x` would otherwise take.
            b'x' | b'X' if self.peek_at(1) == Some(b'\'') => self.read_hex_string()?,
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

    fn triple(&mut self, token: Token) -> Token {
        self.pos += 3;
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
        // MySQL hex literal `0x41` — a binary string, not an integer. Requires at
        // least one hex digit after `0x`; otherwise fall through to a plain `0`.
        if self.peek() == Some(b'0')
            && matches!(self.peek_at(1), Some(b'x') | Some(b'X'))
            && matches!(self.peek_at(2), Some(c) if c.is_ascii_hexdigit())
        {
            self.pos += 2; // `0x`
            let hex_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let mut hex = String::from_utf8_lossy(&self.input[hex_start..self.pos]).into_owned();
            // MySQL left-pads an odd number of digits to an even count.
            if hex.len() % 2 == 1 {
                hex.insert(0, '0');
            }
            return Token::Blob(hex);
        }
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

    /// Reads a MySQL hex-string literal `X'41'` / `x'41'` (the `x`/`X` and
    /// opening quote are at the cursor). Like MySQL, the digit count must be
    /// even — `X'4'` is an error — and an empty `X''` is a valid empty blob.
    fn read_hex_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.pos += 2; // `x` and the opening `'`
        let hex_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        let hex = String::from_utf8_lossy(&self.input[hex_start..self.pos]).into_owned();
        if self.peek() != Some(b'\'') {
            return Err(ParseError::Unterminated {
                offset: start,
                kind: "hex literal",
            });
        }
        self.pos += 1; // closing `'`
        if hex.len() % 2 == 1 {
            return Err(ParseError::Unsupported(format!(
                "hex literal X'{hex}' must contain an even number of digits"
            )));
        }
        Ok(Token::Blob(hex))
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
    fn json_arrow_operators_lex() {
        // `->>` and `->` are distinct tokens; a bare `-` and `- -` are unaffected.
        assert_eq!(tokens("a ->> b"), vec![word("a"), Token::ArrowDouble, word("b")]);
        assert_eq!(tokens("a -> b"), vec![word("a"), Token::Arrow, word("b")]);
        assert_eq!(tokens("a - b"), vec![word("a"), Token::Minus, word("b")]);
        assert_eq!(
            tokens("5 - -2"),
            vec![Token::Num("5".into()), Token::Minus, Token::Minus, Token::Num("2".into())]
        );
    }

    fn word(w: &str) -> Token {
        Token::Word(w.to_string())
    }

    #[test]
    fn hex_literals_lex_to_blob() {
        // `0x..` and `X'..'` / `x'..'` are hex (blob) literals, not `0` + `x..`.
        assert_eq!(tokens("0x41"), vec![Token::Blob("41".into())]);
        assert_eq!(tokens("0X4142"), vec![Token::Blob("4142".into())]);
        assert_eq!(tokens("X'41'"), vec![Token::Blob("41".into())]);
        assert_eq!(tokens("x'4142'"), vec![Token::Blob("4142".into())]);
        // An odd number of `0x` digits is left-padded to even (as MySQL does).
        assert_eq!(tokens("0xABC"), vec![Token::Blob("0ABC".into())]);
        // `0x` with no hex digit is a plain `0` followed by an identifier.
        assert_eq!(tokens("0xZ"), vec![Token::Num("0".into()), word("xZ")]);
        // A space breaks the `x'..'` form into an identifier and a string.
        assert_eq!(tokens("x '41'"), vec![word("x"), Token::Str("41".into())]);
        // An odd-length `X'..'` is rejected.
        assert!(Lexer::new(b"X'4'").tokenize().is_err());
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
