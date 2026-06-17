// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A small hand-written lexer for MySQL SQL.
//!
//! It recognizes the token vocabulary needed by the supported grammar:
//! identifiers (bare and backtick-quoted), string literals (single- or, in
//! MySQL's default mode, double-quoted), numeric literals, and the handful of
//! punctuation characters used by
//! `CREATE TABLE`. Comments (`-- ...`, `# ...`, and `/* ... */`) and whitespace
//! are skipped.

use crate::error::{ParseError, Result};
use crate::token::Token;

/// The server version the front-end reports (`8.0.0` → `80000`), used to decide
/// whether a MySQL version-gated executable comment `/*!##### ... */` runs. It
/// mirrors the `8.0.0-turso` banner the server sends; every gate WordPress and
/// `mysqldump` emit (e.g. `40101`, `50503`) is below it, so they execute.
const SERVER_VERSION_ID: u32 = 80000;

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
            // MySQL system-variable reference `@@[scope.]name`. A single `@` (a
            // user variable) is not part of the supported grammar and lexes as
            // `Other('@')` below.
            b'@' if self.peek_at(1) == Some(b'@') => self.read_system_var(),
            b'`' => self.read_delimited(b'`', "identifier")?,
            // In MySQL's default `sql_mode` a double-quoted token is a string
            // literal, not an identifier (which uses backticks); `ANSI_QUOTES`
            // would make it an identifier, but WordPress does not set it.
            b'"' => self.read_string(b'"')?,
            b'\'' => self.read_string(b'\'')?,
            // MySQL hex-string literal `X'41'` / `x'41'` — checked before the
            // identifier path, which `x` would otherwise take.
            b'x' | b'X' if self.peek_at(1) == Some(b'\'') => self.read_hex_string()?,
            // MySQL bit-value literal `b'101'` / `B'101'`, checked before the
            // word rule so the `b` is not read as an identifier.
            b'b' | b'B' if self.peek_at(1) == Some(b'\'') => self.read_bit_string()?,
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
                // MySQL version-gated executable comment `/*!##### ... */`: its
                // content runs as ordinary SQL when the server version is at least
                // `#####` (a missing version always runs), so `/*!40101 SET NAMES
                // utf8mb4 */` executes the `SET`. We report 8.0, so we execute the
                // content by skipping only the `/*!#####` opener here and the `*/`
                // closer below; a higher gate is skipped like a plain comment.
                Some(b'/') if self.peek_at(1) == Some(b'*') && self.peek_at(2) == Some(b'!') => {
                    self.pos += 3; // `/*!`
                    let digits_start = self.pos;
                    while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    let gate: u32 = std::str::from_utf8(&self.input[digits_start..self.pos])
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    if gate > SERVER_VERSION_ID {
                        // Gate too new: skip the whole comment like `/* ... */`.
                        while self.pos < self.input.len() {
                            if self.peek() == Some(b'*') && self.peek_at(1) == Some(b'/') {
                                self.pos += 2;
                                break;
                            }
                            self.pos += 1;
                        }
                    }
                    // Otherwise the content is lexed as SQL; its closing `*/` is
                    // consumed by the `*/` case below.
                }
                // The `*/` closing an executed version-gated comment (whose content
                // was lexed as SQL). A `*/` is invalid SQL anywhere else, so
                // treating a stray one as trivia is harmless.
                Some(b'*') if self.peek_at(1) == Some(b'/') => self.pos += 2,
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
        // The run is ASCII identifier bytes and/or whole multibyte UTF-8
        // characters (every byte of one is `>= 0x80`, so a character is never
        // split), which is valid UTF-8; `from_utf8_lossy` is a safe fallback.
        let s = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        Token::Word(s)
    }

    /// Reads a MySQL system-variable reference `@@[scope.]name`, the leading `@@`
    /// at the cursor. A `session.` / `global.` / `local.` scope qualifier is
    /// recognized and stripped, leaving the bare variable name in the token.
    fn read_system_var(&mut self) -> Token {
        self.pos += 2; // consume `@@`
        let start = self.pos;
        while let Some(c) = self.peek() {
            // The name may carry a `scope.` prefix, so `.` is part of the run.
            if is_ident_cont(c) || c == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let raw = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        let name = match raw.split_once('.') {
            Some((scope, rest))
                if matches!(
                    scope.to_ascii_lowercase().as_str(),
                    "session" | "global" | "local"
                ) =>
            {
                rest.to_string()
            }
            _ => raw,
        };
        Token::SystemVar(name)
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
        // MySQL bit-value literal `0b101` — a binary string like the hex `0x41`.
        // Requires at least one binary digit after `0b`.
        if self.peek() == Some(b'0')
            && matches!(self.peek_at(1), Some(b'b') | Some(b'B'))
            && matches!(self.peek_at(2), Some(b'0') | Some(b'1'))
        {
            self.pos += 2; // `0b`
            let bits_start = self.pos;
            while matches!(self.peek(), Some(b'0') | Some(b'1')) {
                self.pos += 1;
            }
            let bits = String::from_utf8_lossy(&self.input[bits_start..self.pos]);
            return Token::Blob(binary_to_hex(&bits));
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

    /// Reads a MySQL bit-value literal `b'101'` / `B'101'` (the `b`/`B` and the
    /// opening quote are at the cursor). The binary digits are converted to
    /// MySQL's byte representation — the same blob the `0b101` form produces — so
    /// `b'1000001'` is the byte `0x41` (`'A'`). An empty `b''` is a valid empty
    /// blob.
    fn read_bit_string(&mut self) -> Result<Token> {
        let start = self.pos;
        self.pos += 2; // `b` and the opening `'`
        let bits_start = self.pos;
        while matches!(self.peek(), Some(b'0') | Some(b'1')) {
            self.pos += 1;
        }
        let bits = String::from_utf8_lossy(&self.input[bits_start..self.pos]).into_owned();
        if self.peek() != Some(b'\'') {
            return Err(ParseError::Unterminated {
                offset: start,
                kind: "bit literal",
            });
        }
        self.pos += 1; // closing `'`
        Ok(Token::Blob(binary_to_hex(&bits)))
    }

    /// Reads a backtick-delimited identifier. The delimiter is escaped by
    /// doubling it.
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

    /// Reads a string literal delimited by `delim` — a single `'` or, in MySQL's
    /// default `sql_mode` (no `ANSI_QUOTES`), a double `"`. Handles the delimiter
    /// doubled (`''` / `""`) and the common backslash escapes.
    fn read_string(&mut self, delim: u8) -> Result<Token> {
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
                Some(c) if c == delim => {
                    if self.peek_at(1) == Some(delim) {
                        value.push(delim);
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

/// Converts the digits of a MySQL bit-value literal (`0b101` / `b'101'`) to the
/// hex digits of its byte representation, the form a [`Token::Blob`] holds.
/// MySQL left-pads the bits to a whole number of bytes, so `101` becomes one
/// byte `05` and `101000001` (9 bits) becomes two bytes `0141`. An empty
/// bit string yields an empty (zero-byte) blob.
fn binary_to_hex(bits: &str) -> String {
    if bits.is_empty() {
        return String::new();
    }
    // Left-pad with `0` to a multiple of 8 bits (a whole number of bytes).
    let pad = (8 - bits.len() % 8) % 8;
    let mut padded = "0".repeat(pad);
    padded.push_str(bits);
    // Each 4-bit nibble is one hex digit.
    padded
        .as_bytes()
        .chunks(4)
        .map(|nibble| {
            let value = nibble.iter().fold(0u8, |acc, &b| (acc << 1) | (b - b'0'));
            char::from_digit(value as u32, 16)
                .unwrap()
                .to_ascii_uppercase()
        })
        .collect()
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
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || is_ident_multibyte(c)
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || is_ident_multibyte(c)
}

/// Whether `c` is a byte of a multibyte UTF-8 character (a lead or continuation
/// byte, `>= 0x80`). MySQL admits Unicode characters in the basic multilingual
/// plane in an unquoted identifier (`SELECT 1 AS café`), so the lexer treats any
/// non-ASCII byte as an identifier character: a multibyte sequence is consumed
/// whole, since all of its bytes are `>= 0x80`. (An ASCII-only identifier still
/// goes through the cheaper checks first.)
fn is_ident_multibyte(c: u8) -> bool {
    c >= 0x80
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
    fn lexes_system_variable() {
        // `@@name` becomes a `SystemVar` token holding the bare name.
        assert_eq!(
            tokens("@@max_allowed_packet"),
            vec![Token::SystemVar("max_allowed_packet".to_string())]
        );
        // The `session.` / `global.` / `local.` scope prefix is stripped.
        assert_eq!(
            tokens("@@global.autocommit"),
            vec![Token::SystemVar("autocommit".to_string())]
        );
        assert_eq!(
            tokens("@@SESSION.sql_mode"),
            vec![Token::SystemVar("sql_mode".to_string())]
        );
        // It composes with operators: `@@x > 0`.
        assert_eq!(
            tokens("@@version_compile_os>0"),
            vec![
                Token::SystemVar("version_compile_os".to_string()),
                Token::Gt,
                Token::Num("0".to_string()),
            ]
        );
        // A single `@` (a user variable) is not a system variable.
        assert_eq!(tokens("@x"), vec![Token::Other('@'), Token::Word("x".to_string())]);
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

    #[test]
    fn double_quoted_token_is_a_string_not_an_identifier() {
        // In MySQL's default mode `"..."` is a string literal (Str), while a
        // backtick token stays an identifier (QuotedIdent).
        assert_eq!(tokens(r#""hi""#), vec![Token::Str("hi".into())]);
        assert_eq!(tokens("`hi`"), vec![Token::QuotedIdent("hi".into())]);
        // The delimiter is escaped by doubling (`""`) or a backslash (`\"`).
        assert_eq!(first_string(r#"SELECT "a""b""#), "a\"b");
        assert_eq!(first_string(r#"SELECT "a\"b""#), "a\"b");
        // Backslash escapes and embedded single quotes behave as in a
        // single-quoted string.
        assert_eq!(first_string(r#"SELECT "a\tb""#), "a\tb");
        assert_eq!(first_string(r#"SELECT "it's""#), "it's");
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
    fn version_gated_executable_comments_run_their_contents() {
        // `/*! ... */` and `/*!##### ... */` (a gate at most the reported 8.0)
        // lex their contents as SQL; the opener, version, and closing `*/` are
        // skipped, so a wholly wrapped statement is not empty.
        assert_eq!(tokens("/*!40100 SELECT 7 */"), vec![word("SELECT"), Token::Num("7".into())]);
        assert_eq!(tokens("/*! SELECT 7 */"), vec![word("SELECT"), Token::Num("7".into())]);
        // Mid-statement contents splice into the surrounding tokens (`1 + 2`).
        assert_eq!(
            tokens("1 /*!40101 + 2 */"),
            vec![Token::Num("1".into()), Token::Plus, Token::Num("2".into())]
        );
        // A gate above the reported server version is discarded like a comment.
        assert_eq!(tokens("1 /*!99999 + 2 */"), vec![Token::Num("1".into())]);
        // A plain `/* ... */` comment is still discarded entirely.
        assert_eq!(tokens("1 /* + 2 */"), vec![Token::Num("1".into())]);
        // The markers are lexical, so an identical sequence inside a string
        // literal is left untouched.
        assert_eq!(first_string("SELECT '/*!40101 x */'"), "/*!40101 x */");
    }

    #[test]
    fn unquoted_identifiers_admit_multibyte_characters() {
        // A multibyte UTF-8 character is consumed whole as one identifier word,
        // matching MySQL's Unicode unquoted identifiers.
        assert_eq!(tokens("café"), vec![word("café")]);
        assert_eq!(tokens("naïve"), vec![word("naïve")]);
        assert_eq!(tokens("Ω"), vec![word("Ω")]);
        assert_eq!(tokens("текст"), vec![word("текст")]);
        // An identifier may mix ASCII and multibyte characters, and stops at a
        // separator just like an ASCII one.
        assert_eq!(
            tokens("SELECT café AS x"),
            vec![word("SELECT"), word("café"), word("AS"), word("x")]
        );
        // A `.` still separates a qualified name (it is not an identifier byte).
        assert_eq!(
            tokens("tÄble.cölumn"),
            vec![word("tÄble"), Token::Dot, word("cölumn")]
        );
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
    fn bit_literals_lex_to_byte_blob() {
        // `0b..` and `b'..'` / `B'..'` are bit-value literals, lexed to the blob
        // of their byte representation (left-padded to whole bytes like MySQL).
        assert_eq!(tokens("0b101"), vec![Token::Blob("05".into())]);
        assert_eq!(tokens("b'1111'"), vec![Token::Blob("0F".into())]);
        assert_eq!(tokens("B'1000001'"), vec![Token::Blob("41".into())]); // 'A'
        // Nine bits span two bytes.
        assert_eq!(tokens("0b101000001"), vec![Token::Blob("0141".into())]);
        // An empty `b''` is a valid empty blob.
        assert_eq!(tokens("b''"), vec![Token::Blob(String::new())]);
        // `0b` with no binary digit is a plain `0` followed by an identifier, and
        // a `b` not adjacent to a quote stays an identifier.
        assert_eq!(tokens("0b2"), vec![Token::Num("0".into()), word("b2")]);
        assert_eq!(tokens("b '101'"), vec![word("b"), Token::Str("101".into())]);
        assert_eq!(tokens("bar"), vec![word("bar")]);
        // A non-binary digit inside `b'..'` breaks the literal (unterminated).
        assert!(Lexer::new(b"b'102'").tokenize().is_err());
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
