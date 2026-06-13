// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Errors produced by the MySQL parser.

use thiserror::Error;

/// An error encountered while lexing or parsing a MySQL statement.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The input contained no statement.
    #[error("empty input: no statement to parse")]
    Empty,

    /// A token did not match what the grammar expected at this point.
    #[error("syntax error at byte {offset}: expected {expected}, found {found}")]
    Unexpected {
        offset: usize,
        expected: String,
        found: String,
    },

    /// The statement is syntactically recognized but not implemented yet. The
    /// MySQL front-end currently only supports `CREATE TABLE`.
    #[error("unsupported statement: {0}")]
    Unsupported(String),

    /// A quoted string or identifier was never closed.
    #[error("unterminated {kind} starting at byte {offset}")]
    Unterminated { offset: usize, kind: &'static str },
}

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, ParseError>;
