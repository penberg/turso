// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! The token vocabulary produced by the [`crate::lexer`].

/// A lexical token. Words carry their original spelling; keyword recognition is
/// done case-insensitively by the parser, since MySQL keywords are not
/// case-sensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// An unquoted identifier or keyword, e.g. `CREATE`, `users`, `INT`.
    Word(String),
    /// A delimited identifier: `` `name` `` or `"name"` (contents unescaped).
    QuotedIdent(String),
    /// A single-quoted string literal (contents unescaped).
    Str(String),
    /// A numeric literal, kept verbatim.
    Num(String),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `;`
    Semicolon,
    /// `=`
    Eq,
    /// `-`
    Minus,
    /// `+`
    Plus,
    /// Any other single character (operators like `*`, `<`, `=`, etc.). The
    /// supported grammar never consumes these, but lexing them keeps
    /// unsupported statements parseable enough to be reported cleanly.
    Other(char),
}

impl Token {
    /// A short human-readable description used in error messages.
    pub fn describe(&self) -> String {
        match self {
            Token::Word(w) => format!("`{w}`"),
            Token::QuotedIdent(s) => format!("identifier `{s}`"),
            Token::Str(_) => "a string literal".to_string(),
            Token::Num(n) => format!("number `{n}`"),
            Token::LParen => "`(`".to_string(),
            Token::RParen => "`)`".to_string(),
            Token::Comma => "`,`".to_string(),
            Token::Dot => "`.`".to_string(),
            Token::Semicolon => "`;`".to_string(),
            Token::Eq => "`=`".to_string(),
            Token::Minus => "`-`".to_string(),
            Token::Plus => "`+`".to_string(),
            Token::Other(c) => format!("`{c}`"),
        }
    }
}
