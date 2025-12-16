use std::fmt::Display;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Begin {
    Deferred,
    Immediate,
    Concurrent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rollback;

/// Creates a savepoint within a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Savepoint(pub String);

/// Rolls back to a previously created savepoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackTo(pub String);

/// Releases a savepoint (removes it without rolling back).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseSavepoint(pub String);

impl Display for Begin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keyword = match self {
            Begin::Deferred => "",
            Begin::Immediate => "IMMEDIATE",
            Begin::Concurrent => "CONCURRENT",
        };
        write!(f, "BEGIN {keyword}")
    }
}

impl Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "COMMIT")
    }
}

impl Display for Rollback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ROLLBACK")
    }
}

impl Display for Savepoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SAVEPOINT {}", self.0)
    }
}

impl Display for RollbackTo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ROLLBACK TO {}", self.0)
    }
}

impl Display for ReleaseSavepoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RELEASE SAVEPOINT {}", self.0)
    }
}
