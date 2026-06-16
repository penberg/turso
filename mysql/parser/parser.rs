// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A recursive-descent parser for the MySQL dialect.
//!
//! On success the parser emits a [`turso_parser::ast::Stmt`], so downstream code
//! can reuse the engine's AST, optimizer, and SQL renderer. Unsupported
//! constructs are reported as [`ParseError::Unsupported`].

use std::num::NonZeroU32;

use turso_parser::ast;

use crate::error::{ParseError, Result};
use crate::lexer::Lexer;
use crate::token::Token;

/// A recursive-descent parser over a buffered token stream.
pub struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
    /// The original input, kept so an unaliased select-list expression can be
    /// labelled with its verbatim source text (matching MySQL's column naming).
    input: Vec<u8>,
    /// Byte offset just past the end of input, for end-of-input errors.
    eof: usize,
    /// Number of positional `?` placeholders seen so far. MySQL parameters are
    /// purely positional, so each `?` takes the next index in appearance order.
    params: u32,
    /// Whether the parser is inside an `ON DUPLICATE KEY UPDATE` assignment
    /// right-hand side, where the `VALUES(col)` pseudo-function refers to the
    /// would-be-inserted value and lowers to the engine's `excluded.col`.
    in_upsert_assignment: bool,
    /// The MySQL 8.0.19+ `INSERT ... VALUES (...) AS alias` row-alias name, if
    /// given: inside `ON DUPLICATE KEY UPDATE`, `alias.col` is the would-be-
    /// inserted value (like `VALUES(col)`) and lowers to `excluded.col`.
    upsert_row_alias: Option<String>,
    /// Column aliases from `AS alias (c1, c2, ...)`, mapped positionally to the
    /// INSERT column list — `(column_alias, actual_column)` pairs.
    upsert_col_aliases: Vec<(String, String)>,
    /// `CREATE INDEX` statements deferred from a `CREATE TABLE`'s inline
    /// secondary `KEY`/`INDEX` definitions (the engine's `CREATE TABLE` has no
    /// inline secondary index), drained after the table by `statement_list`.
    pending_indexes: Vec<ast::Stmt>,
}

impl Parser {
    // === Entry points ===

    /// Tokenizes `input` and prepares a parser.
    pub fn new(input: &[u8]) -> Result<Self> {
        let tokens = Lexer::new(input).tokenize()?;
        Ok(Self {
            tokens,
            pos: 0,
            input: input.to_vec(),
            eof: input.len(),
            params: 0,
            in_upsert_assignment: false,
            upsert_row_alias: None,
            upsert_col_aliases: Vec::new(),
            pending_indexes: Vec::new(),
        })
    }

    /// Parses exactly one statement, then ensures only trailing semicolons
    /// remain. This is the crate's main entry point.
    pub fn parse_statement(&mut self) -> Result<ast::Stmt> {
        let stmt = self.statement()?;
        while self.eat(&Token::Semicolon) {}
        if self.pos < self.tokens.len() {
            return Err(self.unexpected("end of input"));
        }
        Ok(stmt)
    }

    /// Parses the input into one or more statements. This differs from
    /// [`Self::parse_statement`] only for a multi-table `DROP TABLE a, b, ...`,
    /// which has no single-statement engine form and is expanded into one
    /// `DROP TABLE` per table for the caller to run in sequence; every other
    /// input yields exactly one statement.
    pub fn parse_statement_list(&mut self) -> Result<Vec<ast::Stmt>> {
        let stmts = self.statement_list()?;
        while self.eat(&Token::Semicolon) {}
        if self.pos < self.tokens.len() {
            return Err(self.unexpected("end of input"));
        }
        Ok(stmts)
    }

    fn statement_list(&mut self) -> Result<Vec<ast::Stmt>> {
        // `DROP [TEMPORARY] TABLE` may list several tables; expand it. Everything
        // else is a single statement.
        if self.is_keyword("DROP") {
            let temp = matches!(self.peek_nth(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("TEMPORARY"));
            let table_at = if temp { 2 } else { 1 };
            if matches!(self.peek_nth(table_at), Some(Token::Word(w)) if w.eq_ignore_ascii_case("TABLE"))
            {
                self.advance(); // DROP
                let temporary = self.eat_keyword("TEMPORARY");
                self.expect_keyword("TABLE")?;
                return self.drop_table_list(temporary);
            }
        }

        // `ALTER TABLE t op1, op2, ...` may list several operations; expand each
        // into its own statement (the engine has no multi-operation ALTER).
        if self.is_keyword("ALTER")
            && matches!(self.peek_nth(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("TABLE"))
        {
            self.advance(); // ALTER
            self.expect_keyword("TABLE")?;
            let name = self.qualified_name()?;

            // A pure table-option ALTER -- `ENGINE=`, `CONVERT TO CHARACTER SET`,
            // `DEFAULT CHARSET=`, `ROW_FORMAT=`, `AUTO_INCREMENT=`, `COMMENT=`, ...
            // -- has no effect on the engine's fixed storage and single charset,
            // exactly as the same options are ignored on `CREATE TABLE`. WordPress
            // issues `ALTER TABLE ... CONVERT TO CHARACTER SET utf8mb4` (and
            // plugins set `ENGINE=`). Accept it as a no-op: consume the rest and
            // emit no statements, so the server replies OK without touching the
            // table. (`AUTO_INCREMENT=` is also ignored, a documented divergence.)
            if matches!(self.peek(), Some(Token::Word(w)) if is_table_option_keyword(w)) {
                while self.pos < self.tokens.len() && !self.is(&Token::Semicolon) {
                    self.advance();
                }
                return Ok(Vec::new());
            }

            let mut stmts = vec![self.alter_operation(name.clone())?];
            while self.eat(&Token::Comma) {
                stmts.push(self.alter_operation(name.clone())?);
            }
            return Ok(stmts);
        }

        // `DO expr [, expr]...` evaluates its expressions purely for their side
        // effects and returns no result set (it is a faster `SELECT` with the
        // rows discarded). MySQL's usual `DO` targets -- locking functions
        // (`GET_LOCK`), `SLEEP`, user-variable assignments -- have no engine
        // equivalent, so there is nothing to run: parse the expressions to
        // validate the syntax, then emit no statements. The server replies OK
        // with no result set, matching MySQL's contract.
        if self.is_keyword("DO") {
            self.advance(); // DO
            self.expr()?;
            while self.eat(&Token::Comma) {
                self.expr()?;
            }
            return Ok(Vec::new());
        }

        // A `CREATE TABLE` with inline secondary `KEY`/`INDEX` clauses defers them
        // as `CREATE INDEX` statements (the engine's `CREATE TABLE` has none);
        // emit them after the table.
        let stmt = self.statement()?;
        if self.pending_indexes.is_empty() {
            Ok(vec![stmt])
        } else {
            let mut stmts = vec![stmt];
            stmts.append(&mut self.pending_indexes);
            Ok(stmts)
        }
    }

    // === Statement dispatch ===

    fn statement(&mut self) -> Result<ast::Stmt> {
        // A statement beginning with `(` is a parenthesized leading select branch,
        // e.g. `(SELECT ...) UNION (SELECT ...)`.
        if self.is(&Token::LParen) {
            return self.paren_select_statement();
        }
        let keyword = match self.peek() {
            Some(Token::Word(w)) => w.to_ascii_uppercase(),
            None => return Err(ParseError::Empty),
            Some(_) => return Err(self.unexpected("a SQL statement")),
        };
        match keyword.as_str() {
            "CREATE" => {
                self.advance();
                self.create()
            }
            "DROP" => {
                self.advance();
                self.drop()
            }
            "INSERT" => {
                self.advance();
                self.insert(None)
            }
            "REPLACE" => {
                self.advance();
                self.insert(Some(ast::ResolveType::Replace))
            }
            "SELECT" => {
                self.advance();
                self.select()
            }
            "UPDATE" => {
                self.advance();
                self.update()
            }
            "DELETE" => {
                self.advance();
                self.delete()
            }
            "BEGIN" | "START" => {
                self.advance();
                self.begin_transaction(&keyword)
            }
            "COMMIT" => {
                self.advance();
                self.commit_transaction()
            }
            "ROLLBACK" => {
                self.advance();
                self.rollback_transaction()
            }
            "SAVEPOINT" => {
                self.advance();
                self.savepoint()
            }
            "RELEASE" => {
                self.advance();
                self.release_savepoint()
            }
            "TRUNCATE" => {
                self.advance();
                self.truncate_table()
            }
            "ALTER" => {
                self.advance();
                self.alter()
            }
            "RENAME" => {
                self.advance();
                self.rename_table()
            }
            "WITH" => {
                self.advance();
                self.with_select()
            }
            "TABLE" => {
                self.advance();
                self.table_statement()
            }
            // Recognized statement keywords that are simply not implemented yet.
            "SET" | "SHOW" | "USE" | "DESCRIBE" | "DESC" | "EXPLAIN" | "GRANT"
            | "REVOKE" | "CALL" | "DO" | "VALUES" | "PREPARE" | "EXECUTE"
            | "DEALLOCATE" | "LOCK" | "UNLOCK" | "ANALYZE" | "OPTIMIZE" | "CHECK" | "REPAIR"
            | "FLUSH" | "KILL" | "LOAD" | "HANDLER" | "IMPORT" => Err(ParseError::Unsupported(
                format!("{keyword} is not supported yet"),
            )),
            other => Err(ParseError::Unsupported(format!(
                "unrecognized statement starting with `{other}`"
            ))),
        }
    }

    // === CREATE TABLE ===

    fn create(&mut self) -> Result<ast::Stmt> {
        // `CREATE` has already been consumed.
        let temporary = self.eat_keyword("TEMPORARY");
        if self.eat_keyword("TABLE") {
            return self.create_table(temporary);
        }
        if temporary {
            return Err(ParseError::Unsupported(
                "CREATE TEMPORARY only applies to tables".to_string(),
            ));
        }
        // `CREATE [UNIQUE] INDEX idx ON tbl (cols)`.
        let unique = self.eat_keyword("UNIQUE");
        if self.eat_keyword("INDEX") {
            return self.create_index(unique);
        }
        let what = match self.peek() {
            Some(Token::Word(w)) => w.to_ascii_uppercase(),
            _ => "?".to_string(),
        };
        Err(ParseError::Unsupported(format!(
            "CREATE {what} is not supported yet (only CREATE TABLE / CREATE INDEX are implemented)"
        )))
    }

    /// Parses `CREATE [UNIQUE] INDEX idx_name [USING ...] ON tbl_name (cols)`
    /// (`CREATE [UNIQUE] INDEX` already consumed) into the engine's
    /// `CREATE [UNIQUE] INDEX`, which it executes natively. Like the
    /// `ALTER TABLE ... ADD KEY` lowering ([`Self::alter_add_index`]), column
    /// prefix lengths (`col(191)`) are dropped and an optional `USING
    /// BTREE/HASH` index type is ignored. An index name is required (MySQL has no
    /// auto-named standalone `CREATE INDEX`).
    fn create_index(&mut self, unique: bool) -> Result<ast::Stmt> {
        let idx_name = self.qualified_name()?;
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }
        self.expect_keyword("ON")?;
        let tbl_name = self.qualified_name()?;
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }
        let columns = self.sorted_column_list()?;
        Ok(ast::Stmt::CreateIndex {
            unique,
            if_not_exists: false,
            idx_name,
            tbl_name: tbl_name.name,
            using: None,
            columns,
            with_clause: Vec::new(),
            where_clause: None,
        })
    }

    fn create_table(&mut self, temporary: bool) -> Result<ast::Stmt> {
        let if_not_exists = if self.eat_keyword("IF") {
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };

        let tbl_name = self.qualified_name()?;

        // `CREATE TABLE name [AS] SELECT ...` — create-table-as-select. MySQL
        // makes the `AS` optional; the engine builds the table from the query's
        // result columns. (The form with an explicit leading column list before
        // the select is not modeled — the engine's body carries only the select.)
        let as_kw = self.eat_keyword("AS");
        if as_kw || self.is_keyword("SELECT") {
            self.expect_keyword("SELECT")?;
            let select = self.parse_select()?;
            return Ok(ast::Stmt::CreateTable {
                temporary,
                if_not_exists,
                tbl_name,
                body: ast::CreateTableBody::AsSelect(select),
            });
        }

        // Otherwise the explicit column-list form is required (not `LIKE`, which
        // has no engine equivalent).
        if !self.is(&Token::LParen) {
            return Err(ParseError::Unsupported(
                "CREATE TABLE without a column list or AS SELECT (e.g. LIKE)".to_string(),
            ));
        }
        self.expect(&Token::LParen, "`(`")?;

        // Columns carry an `auto_increment` flag alongside the definition; it is
        // resolved into the engine's rowid-alias autoincrement after parsing.
        let mut columns: Vec<(ast::ColumnDefinition, bool)> = Vec::new();
        let mut constraints = Vec::new();
        let mut inline_indexes: Vec<(Option<ast::Name>, Vec<ast::SortedColumn>, bool)> = Vec::new();
        loop {
            if self.next_is_table_constraint() {
                self.table_constraint(&mut constraints, &mut inline_indexes)?;
            } else {
                columns.push(self.column_def()?);
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen, "`)` or `,`")?;

        // Trailing table options (ENGINE=, DEFAULT CHARSET=, ...) are ignored.
        self.skip_table_options();

        if columns.is_empty() {
            return Err(self.unexpected("at least one column definition"));
        }

        Self::apply_auto_increment(&mut columns, &mut constraints)?;
        let columns: Vec<ast::ColumnDefinition> = columns.into_iter().map(|(c, _)| c).collect();

        // Each inline secondary key becomes a deferred CREATE INDEX, emitted by
        // `statement_list` after this table (the engine has no inline form). The
        // index inherits `IF NOT EXISTS` so re-running the CREATE TABLE is safe.
        for (idx_name, idx_columns, unique) in inline_indexes {
            let name = idx_name.unwrap_or_else(|| {
                let first = match idx_columns.first().map(|c| c.expr.as_ref()) {
                    Some(ast::Expr::Id(n)) => n.as_str(),
                    _ => "idx",
                };
                ast::Name::from_string(format!("{}_{}", tbl_name.name.as_str(), first))
            });
            self.pending_indexes.push(ast::Stmt::CreateIndex {
                unique,
                if_not_exists,
                idx_name: ast::QualifiedName::single(name),
                tbl_name: tbl_name.name.clone(),
                using: None,
                columns: idx_columns,
                with_clause: Vec::new(),
                where_clause: None,
            });
        }

        Ok(ast::Stmt::CreateTable {
            temporary,
            if_not_exists,
            tbl_name,
            body: ast::CreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                options: ast::TableOptions::empty(),
            },
        })
    }

    // === ALTER TABLE ===

    /// Parses an `ALTER TABLE tbl ...` statement (`ALTER` already consumed) and
    /// lowers it to an engine operation. MySQL allows many operations per
    /// `ALTER TABLE`; the forms with a single-statement engine equivalent are
    /// supported:
    ///   - `ADD [COLUMN] <column-def>` → `ALTER TABLE ... ADD COLUMN`,
    ///   - `ADD [UNIQUE] {KEY|INDEX} [name] (cols)` → `CREATE [UNIQUE] INDEX`
    ///     (see [`Self::alter_add_index`]),
    ///   - `ADD FULLTEXT [KEY|INDEX] [name] (cols)` → a plain `CREATE INDEX`,
    ///     since the engine has no full-text index (see [`Self::alter_add_index`]),
    ///   - `ADD PRIMARY KEY (cols)` → a `CREATE UNIQUE INDEX` standing in for the
    ///     in-place primary key the engine cannot add (see
    ///     [`Self::alter_add_primary_key`]),
    ///   - `DROP [COLUMN] col` → `ALTER TABLE ... DROP COLUMN`,
    ///   - `RENAME [TO|AS] new` → `ALTER TABLE ... RENAME TO`, and
    ///   - `RENAME COLUMN old TO new` → `ALTER TABLE ... RENAME COLUMN`.
    ///
    ///   - `DROP PRIMARY KEY` → `DROP INDEX <table>_primary`, the inverse of the
    ///     `ADD PRIMARY KEY` emulation (see [`Self::alter_drop`]).
    ///
    ///   - `CHANGE [COLUMN] old new <def>` with `old` ≠ `new` → `RENAME COLUMN`
    ///     (the rename; the redeclared type is advisory — see [`Self::alter_change`]).
    ///
    /// Everything else — `ADD FOREIGN KEY`/`SPATIAL`/`CONSTRAINT`,
    /// `DROP {FOREIGN KEY|CONSTRAINT}`, `MODIFY` and same-name `CHANGE` (an
    /// in-place column type change), and `RENAME INDEX` — is rejected as
    /// unsupported. The
    /// comma-separated multi-operation form has no single-statement engine
    /// equivalent and is rejected here, but [`Self::parse_statement_list`] expands
    /// it into one statement per operation.
    fn alter(&mut self) -> Result<ast::Stmt> {
        self.expect_keyword("TABLE")?;
        let name = self.qualified_name()?;
        let stmt = self.alter_operation(name)?;
        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "ALTER TABLE with multiple operations is not supported yet".to_string(),
            ));
        }
        Ok(stmt)
    }

    /// Parses a single `ALTER TABLE` operation given the already-parsed table
    /// `name` (the operation keyword — `ADD`/`DROP`/`RENAME` — has not been
    /// consumed) and lowers it to one engine statement. Stops before any trailing
    /// comma, so the multi-operation caller can split on it.
    fn alter_operation(&mut self, name: ast::QualifiedName) -> Result<ast::Stmt> {
        if self.eat_keyword("DROP") {
            return self.alter_drop(name);
        }
        if self.eat_keyword("RENAME") {
            return self.alter_rename(name);
        }
        // `CHANGE [COLUMN] old new <def>` renames `old` to `new` (and would
        // retype it); the engine can rename but not retype in place (see
        // `alter_change`).
        if self.eat_keyword("CHANGE") {
            return self.alter_change(name);
        }
        // `MODIFY [COLUMN] col <def>` only retypes a column — no engine
        // equivalent.
        if self.eat_keyword("MODIFY") {
            self.eat_keyword("COLUMN");
            let (def, _) = self.column_def()?;
            return Err(ParseError::Unsupported(format!(
                "ALTER TABLE MODIFY COLUMN `{}` is not supported: the engine \
                 cannot change a column's type in place",
                def.col_name.as_str()
            )));
        }
        if !self.eat_keyword("ADD") {
            return Err(ParseError::Unsupported(
                "only ALTER TABLE ... ADD / DROP / RENAME / CHANGE is supported yet".to_string(),
            ));
        }

        // `ADD [CONSTRAINT [symbol]] {PRIMARY KEY | UNIQUE | FOREIGN KEY | CHECK}`:
        // consume the optional `CONSTRAINT` keyword and its symbol name (the engine
        // names the index itself), then fall through to the `PRIMARY KEY`/`UNIQUE`
        // index lowering below. `FOREIGN KEY` / `CHECK` are still rejected further
        // down (no engine equivalent).
        if self.eat_keyword("CONSTRAINT") {
            let names_a_symbol = !matches!(self.peek(), Some(Token::Word(w))
                if w.eq_ignore_ascii_case("PRIMARY")
                    || w.eq_ignore_ascii_case("UNIQUE")
                    || w.eq_ignore_ascii_case("FOREIGN")
                    || w.eq_ignore_ascii_case("CHECK"));
            if names_a_symbol {
                let _ = self.name()?;
            }
        }

        // `ADD [UNIQUE] {KEY|INDEX} [name] (cols)` becomes a CREATE INDEX. The
        // `KEY`/`INDEX` keyword is optional only after `UNIQUE`.
        let unique = self.eat_keyword("UNIQUE");
        if self.eat_keyword("KEY") || self.eat_keyword("INDEX") || unique {
            return self.alter_add_index(name, unique);
        }

        // `ADD FULLTEXT [KEY|INDEX] [name] (cols)` has no engine full-text index,
        // so it degrades to a plain secondary index. The statement succeeds (as
        // it does on a real mysqld with a full-text-capable storage engine) and a
        // following `SHOW INDEX` reports the key, which is what WordPress's
        // `dbDelta()` relies on to avoid re-adding it. The index provides ordinary
        // indexed lookups only -- not `MATCH ... AGAINST` full-text search, which
        // the engine does not implement.
        if self.eat_keyword("FULLTEXT") {
            let _ = self.eat_keyword("KEY") || self.eat_keyword("INDEX");
            return self.alter_add_index(name, false);
        }

        // `ADD PRIMARY KEY (cols)` -- the engine cannot add a real (rowid)
        // primary key in place, so emulate it with a UNIQUE index over the key
        // columns. This enforces the key's uniqueness (its primary runtime
        // effect) and lets the statement succeed, which is what WordPress's
        // `dbDelta()` issues. See [`Self::alter_add_primary_key`].
        if self.eat_keyword("PRIMARY") {
            self.expect_keyword("KEY")?;
            return self.alter_add_primary_key(name);
        }

        // `COLUMN` is optional after `ADD`. Any other index/constraint add starts
        // with one of these keywords and has no single-statement engine
        // equivalent.
        self.eat_keyword("COLUMN");
        for kw in ["CONSTRAINT", "SPATIAL", "FOREIGN", "CHECK"] {
            if self.is_keyword(kw) {
                return Err(ParseError::Unsupported(format!(
                    "ALTER TABLE ... ADD {kw} is not supported yet"
                )));
            }
        }

        let (column, auto_increment) = self.column_def()?;
        if auto_increment {
            return Err(ParseError::Unsupported(
                "ALTER TABLE ... ADD COLUMN with AUTO_INCREMENT is not supported yet".to_string(),
            ));
        }

        // A trailing `FIRST` / `AFTER col` clause positions the new column. The
        // engine always appends, so the position is consumed and ignored
        // (WordPress's `dbDelta()` emits `ADD COLUMN ... AFTER ...`; column order
        // does not affect name-based access). `FIRST` takes no argument.
        if !self.eat_keyword("FIRST") && self.eat_keyword("AFTER") {
            let _ = self.name()?;
        }

        Ok(ast::Stmt::AlterTable(ast::AlterTable {
            name,
            body: ast::AlterTableBody::AddColumn(column),
        }))
    }

    /// Lowers `ADD [UNIQUE] {KEY|INDEX} [name] (cols)` (the `KEY`/`INDEX` keyword
    /// is already consumed) to `CREATE [UNIQUE] INDEX name ON tbl (cols)`. MySQL
    /// index-column prefix lengths (`col(191)`) are dropped — the engine indexes
    /// the whole column, which is still correct, just less selective. The engine
    /// requires a name, so when MySQL would auto-generate one (no name given)
    /// it is synthesized from the table and first column. Index names live in a
    /// per-database namespace here (unlike MySQL's per-table one), so the same
    /// index name on two tables would collide.
    fn alter_add_index(&mut self, tbl: ast::QualifiedName, unique: bool) -> Result<ast::Stmt> {
        // An optional index name precedes the column list; it is absent when the
        // next token opens the column list or an index-type clause.
        let explicit_name = if self.is(&Token::LParen) || self.is_keyword("USING") {
            None
        } else {
            Some(self.name()?)
        };
        // Optional `USING {BTREE|HASH}` index type, which the engine ignores.
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }

        let columns = self.sorted_column_list()?;

        let idx_name = explicit_name.unwrap_or_else(|| {
            let first = match columns.first().map(|c| c.expr.as_ref()) {
                Some(ast::Expr::Id(n)) => n.as_str(),
                _ => "idx",
            };
            ast::Name::from_string(format!("{}_{}", tbl.name.as_str(), first))
        });

        Ok(ast::Stmt::CreateIndex {
            unique,
            if_not_exists: false,
            idx_name: ast::QualifiedName::single(idx_name),
            tbl_name: tbl.name,
            using: None,
            columns,
            with_clause: Vec::new(),
            where_clause: None,
        })
    }

    /// Lowers `ADD PRIMARY KEY [USING ...] (cols)` (the `PRIMARY KEY` keywords
    /// already consumed) to a `CREATE UNIQUE INDEX` over the key columns. The
    /// engine cannot add a real, in-place primary key (SQLite/turso reserve that
    /// for `CREATE TABLE`'s rowid alias), so the unique index stands in: it
    /// enforces the key's uniqueness and the statement succeeds. As with
    /// [`Self::alter_add_index`], a column prefix length (`col(191)`) is dropped
    /// and an optional `USING {BTREE|HASH}` type is ignored. The index is named
    /// `<table>_primary` to be unique within the per-database index namespace
    /// (a table has at most one primary key). This is not a true primary key:
    /// `SHOW INDEX` reports it under that name rather than MySQL's `PRIMARY`, and
    /// the columns are not made implicitly `NOT NULL` -- see `mysql/COMPAT.md`.
    fn alter_add_primary_key(&mut self, tbl: ast::QualifiedName) -> Result<ast::Stmt> {
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }
        let columns = self.sorted_column_list()?;
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }
        let idx_name = ast::Name::from_string(format!("{}_primary", tbl.name.as_str()));
        Ok(ast::Stmt::CreateIndex {
            unique: true,
            if_not_exists: false,
            idx_name: ast::QualifiedName::single(idx_name),
            tbl_name: tbl.name,
            using: None,
            columns,
            with_clause: Vec::new(),
            where_clause: None,
        })
    }

    /// Lowers `DROP [COLUMN] col` (the `DROP` keyword is already consumed) to the
    /// engine's `ALTER TABLE ... DROP COLUMN`. `DROP {INDEX|KEY} name` becomes a
    /// `DROP INDEX`, and `DROP PRIMARY KEY` drops the `<table>_primary` index that
    /// [`Self::alter_add_primary_key`] created. Dropping a foreign key or other
    /// constraint (`DROP {FOREIGN KEY|CONSTRAINT|CHECK}`) has no engine equivalent
    /// and is rejected.
    fn alter_drop(&mut self, name: ast::QualifiedName) -> Result<ast::Stmt> {
        // `DROP {INDEX|KEY} idx_name` drops a secondary index, mirroring the
        // `ADD KEY` -> `CREATE INDEX` lowering; it becomes the engine's
        // `DROP INDEX idx_name` (index names are per-database here, so the table
        // is implied).
        if self.eat_keyword("INDEX") || self.eat_keyword("KEY") {
            let idx_name = self.qualified_name()?;
            return Ok(ast::Stmt::DropIndex {
                if_exists: false,
                idx_name,
            });
        }
        // `DROP PRIMARY KEY` is symmetric with the `ADD PRIMARY KEY` emulation:
        // drop the `<table>_primary` UNIQUE index that stood in for the primary
        // key, so an ADD/DROP cycle round-trips. (A table whose primary key came
        // from `CREATE TABLE` instead -- the engine's rowid alias -- has no such
        // index, so this errors there; the engine cannot drop a rowid primary key
        // in place.)
        if self.eat_keyword("PRIMARY") {
            self.expect_keyword("KEY")?;
            let idx_name = ast::Name::from_string(format!("{}_primary", name.name.as_str()));
            return Ok(ast::Stmt::DropIndex {
                if_exists: false,
                idx_name: ast::QualifiedName::single(idx_name),
            });
        }
        for kw in ["FOREIGN", "CONSTRAINT", "CHECK"] {
            if self.is_keyword(kw) {
                return Err(ParseError::Unsupported(format!(
                    "ALTER TABLE ... DROP {kw} is not supported yet"
                )));
            }
        }
        // `COLUMN` is optional in MySQL.
        self.eat_keyword("COLUMN");
        let column = self.name()?;
        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "ALTER TABLE with multiple operations is not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::AlterTable(ast::AlterTable {
            name,
            body: ast::AlterTableBody::DropColumn(column),
        }))
    }

    /// Lowers the `RENAME` operations (the `RENAME` keyword is already consumed):
    /// `RENAME COLUMN old TO new` → `RENAME COLUMN`, and `RENAME [TO|AS] new` →
    /// `RENAME TO`. `RENAME {INDEX|KEY} old TO new` has no engine equivalent and
    /// is rejected.
    fn alter_rename(&mut self, name: ast::QualifiedName) -> Result<ast::Stmt> {
        if self.eat_keyword("COLUMN") {
            let old = self.name()?;
            self.expect_keyword("TO")?;
            let new = self.name()?;
            return Ok(ast::Stmt::AlterTable(ast::AlterTable {
                name,
                body: ast::AlterTableBody::RenameColumn { old, new },
            }));
        }
        if self.is_keyword("INDEX") || self.is_keyword("KEY") {
            return Err(ParseError::Unsupported(
                "ALTER TABLE ... RENAME INDEX is not supported yet".to_string(),
            ));
        }
        // `RENAME [TO|AS] new_table`; a database qualifier is tolerated.
        self.eat_keyword("TO");
        self.eat_keyword("AS");
        let new_table = self.qualified_name()?;
        Ok(ast::Stmt::AlterTable(ast::AlterTable {
            name,
            body: ast::AlterTableBody::RenameTo(new_table.name),
        }))
    }

    /// Parses `ALTER TABLE ... CHANGE [COLUMN] old new <definition>` and lowers a
    /// *rename* (`old` ≠ `new`) to the engine's `RENAME COLUMN old TO new`. The
    /// trailing column definition (the new name's type and constraints) is parsed
    /// to consume it but discarded: the engine's columns are affinity-typed, so
    /// the redeclared type is advisory — a same-affinity retype is a no-op, and a
    /// fundamental type change is not applied (a documented limitation). A
    /// same-name `CHANGE` is purely a type change with no rename, so it is
    /// rejected like `MODIFY`.
    fn alter_change(&mut self, name: ast::QualifiedName) -> Result<ast::Stmt> {
        self.eat_keyword("COLUMN");
        let old = self.name()?;
        // `new <type> <constraints>` is exactly a column definition.
        let (def, _) = self.column_def()?;
        let new = def.col_name;
        if old.as_str().eq_ignore_ascii_case(new.as_str()) {
            return Err(ParseError::Unsupported(format!(
                "ALTER TABLE CHANGE COLUMN `{}` to the same name is a column type \
                 change, which is not supported: the engine cannot change a \
                 column's type in place",
                old.as_str()
            )));
        }
        Ok(ast::Stmt::AlterTable(ast::AlterTable {
            name,
            body: ast::AlterTableBody::RenameColumn { old, new },
        }))
    }

    /// Parses the standalone `RENAME TABLE old_name TO new_name` statement
    /// (`RENAME` already consumed) and lowers it to the engine's
    /// `ALTER TABLE old RENAME TO new`, which it executes natively. MySQL's
    /// comma-separated multi-rename (`RENAME TABLE a TO b, c TO d`) is rejected,
    /// as is the `RENAME {DATABASE|USER}` form (only `TABLE` is recognized here).
    fn rename_table(&mut self) -> Result<ast::Stmt> {
        if !self.eat_keyword("TABLE") {
            return Err(ParseError::Unsupported(
                "only RENAME TABLE is supported yet".to_string(),
            ));
        }
        let old = self.qualified_name()?;
        self.expect_keyword("TO")?;
        let new = self.qualified_name()?;
        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "RENAME TABLE with multiple tables is not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::AlterTable(ast::AlterTable {
            name: old,
            body: ast::AlterTableBody::RenameTo(new.name),
        }))
    }

    /// Resolves MySQL `AUTO_INCREMENT` onto the engine's rowid-alias
    /// autoincrement, which only applies to a single-column `INTEGER PRIMARY
    /// KEY`. MySQL's int width is display-only, so the auto-increment key column
    /// is retyped to `INTEGER` (making it a rowid alias that auto-assigns
    /// sequential ids), and the primary key — whether declared inline or as a
    /// table-level `PRIMARY KEY (col)` — is marked autoincrement so ids are
    /// never reused, matching MySQL.
    ///
    /// Only this clean shape is accepted: an `AUTO_INCREMENT` column that is the
    /// table's sole, single-column primary key. Anything else (a non-key
    /// `AUTO_INCREMENT` column, a composite primary key, or more than one
    /// `AUTO_INCREMENT` column) is rejected as unsupported.
    fn apply_auto_increment(
        columns: &mut [(ast::ColumnDefinition, bool)],
        constraints: &mut [ast::NamedTableConstraint],
    ) -> Result<()> {
        let auto_inc: Vec<&str> = columns
            .iter()
            .filter(|(_, ai)| *ai)
            .map(|(c, _)| c.col_name.as_str())
            .collect();
        if auto_inc.is_empty() {
            return Ok(());
        }
        if auto_inc.len() > 1 {
            return Err(ParseError::Unsupported(
                "more than one AUTO_INCREMENT column is not supported".to_string(),
            ));
        }
        let ai_name = auto_inc[0].to_string();

        // Find the single-column primary key, whether inline or table-level, and
        // confirm it is exactly the AUTO_INCREMENT column.
        let inline_pk: Vec<&str> = columns
            .iter()
            .filter(|(c, _)| {
                c.constraints
                    .iter()
                    .any(|nc| matches!(nc.constraint, ast::ColumnConstraint::PrimaryKey { .. }))
            })
            .map(|(c, _)| c.col_name.as_str())
            .collect();
        let table_pk: Option<&[ast::SortedColumn]> =
            constraints.iter().find_map(|c| match &c.constraint {
                ast::TableConstraint::PrimaryKey { columns, .. } => Some(columns.as_slice()),
                _ => None,
            });

        let pk_is_ai = match (inline_pk.as_slice(), table_pk) {
            ([only], None) => only.eq_ignore_ascii_case(&ai_name),
            ([], Some(cols)) => {
                cols.len() == 1 && sorted_column_name(&cols[0]).eq_ignore_ascii_case(&ai_name)
            }
            _ => false,
        };
        if !pk_is_ai {
            return Err(ParseError::Unsupported(
                "AUTO_INCREMENT is only supported on a single-column PRIMARY KEY".to_string(),
            ));
        }

        // Retype the key column to INTEGER so the engine treats it as a rowid
        // alias (the only form that auto-assigns).
        for (col, _) in columns.iter_mut() {
            if col.col_name.as_str().eq_ignore_ascii_case(&ai_name) {
                col.col_type = Some(ast::Type {
                    name: "INTEGER".to_string(),
                    size: None,
                    array_dimensions: 0,
                });
            }
        }

        // Mark the primary key autoincrement (no id reuse). The inline case is
        // already handled by `column_constraints`; here we cover the table-level
        // `PRIMARY KEY (col)` form.
        for c in constraints.iter_mut() {
            if let ast::TableConstraint::PrimaryKey { auto_increment, .. } = &mut c.constraint {
                *auto_increment = true;
            }
        }

        Ok(())
    }

    /// Parses a column definition. Returns the definition together with whether
    /// the column was declared `AUTO_INCREMENT`, which `create_table` needs to
    /// map onto the engine's rowid-alias autoincrement.
    fn column_def(&mut self) -> Result<(ast::ColumnDefinition, bool)> {
        let col_name = self.name()?;
        let col_type = self.column_type()?;
        let (mut constraints, auto_increment, collation) = self.column_constraints()?;

        // MySQL's default collation (`utf8mb4_general_ci`) compares text
        // case-insensitively, while the engine — like SQLite's `BINARY` default —
        // compares case-sensitively. For a character column with no explicit
        // case-sensitive (`_bin`/`_cs`) collation, declare `COLLATE NOCASE` so
        // equality, `ORDER BY`, `UNIQUE`, and index lookups fold ASCII case as
        // MySQL does. (NOCASE folds only ASCII A–Z, not the full Unicode set MySQL
        // does, but that covers the slugs, option names, and emails WordPress
        // compares.) BLOB and `BINARY`/`VARBINARY` columns stay case-sensitive.
        let case_sensitive = collation.as_deref().is_some_and(is_case_sensitive_collation);
        if col_type.as_ref().is_some_and(is_character_type) && !case_sensitive {
            constraints.push(named(ast::ColumnConstraint::Collate {
                collation_name: ast::Name::from_string("NOCASE"),
            }));
        }

        // MySQL (in its default non-strict `sql_mode`, which WordPress's test
        // harness uses) supplies an implicit type default for a `NOT NULL` column
        // that has no explicit `DEFAULT`, so a row that omits the column still
        // inserts -- `0` for numeric types and `''` for string types. The engine
        // enforces `NOT NULL` strictly and would reject the row, so materialize
        // that implicit default as an explicit `DEFAULT` (see
        // `implicit_not_null_default`). Skip `AUTO_INCREMENT` and `PRIMARY KEY`
        // columns: the former generates its own values and the latter is left to
        // the engine's rowid handling (and so keeps its `NULL` `SHOW COLUMNS`
        // default).
        if !auto_increment {
            let is_not_null = constraints.iter().any(|c| {
                matches!(
                    &c.constraint,
                    ast::ColumnConstraint::NotNull {
                        nullable: false,
                        ..
                    }
                )
            });
            let has_default = constraints
                .iter()
                .any(|c| matches!(&c.constraint, ast::ColumnConstraint::Default(_)));
            let is_primary_key = constraints
                .iter()
                .any(|c| matches!(&c.constraint, ast::ColumnConstraint::PrimaryKey { .. }));
            if is_not_null && !has_default && !is_primary_key {
                if let Some(default) = col_type.as_ref().and_then(implicit_not_null_default) {
                    constraints.push(named(ast::ColumnConstraint::Default(default)));
                }
            }
        }

        Ok((
            ast::ColumnDefinition {
                col_name,
                col_type,
                constraints,
            },
            auto_increment,
        ))
    }

    /// Parses an optional column type: a name, an optional `(size[, scale])`,
    /// and trailing `UNSIGNED` / `ZEROFILL` / `SIGNED` modifiers (folded into
    /// the type name). `ENUM(...)`/`SET(...)` value lists are accepted but the
    /// size is dropped, and the type itself is lowered to `TEXT`.
    fn column_type(&mut self) -> Result<Option<ast::Type>> {
        let Some(Token::Word(w)) = self.peek() else {
            return Ok(None);
        };
        if is_column_constraint_keyword(w) {
            return Ok(None);
        }
        let mut name = w.clone();
        self.advance();

        let mut size = None;
        if self.is(&Token::LParen) {
            size = self.type_size()?;
        }

        // MySQL's `ENUM(...)` and `SET(...)` store their value as a string, so
        // lower both to `TEXT`. The engine has no such types -- and `SET` is a
        // reserved keyword there, so it cannot even appear as a column type name.
        // The value list was already dropped by `type_size`; the allowed-values
        // constraint is not enforced (any string is accepted, as `TEXT`).
        if name.eq_ignore_ascii_case("ENUM") || name.eq_ignore_ascii_case("SET") {
            name = "TEXT".to_string();
            size = None;
        }

        loop {
            if self.eat_keyword("UNSIGNED") {
                name.push_str(" UNSIGNED");
            } else if self.eat_keyword("ZEROFILL") {
                name.push_str(" ZEROFILL");
            } else if self.eat_keyword("SIGNED") {
                name.push_str(" SIGNED");
            } else {
                break;
            }
        }

        Ok(Some(ast::Type {
            name,
            size,
            array_dimensions: 0,
        }))
    }

    fn type_size(&mut self) -> Result<Option<ast::TypeSize>> {
        self.expect(&Token::LParen, "`(`")?;
        if let Some(Token::Num(n)) = self.peek() {
            let first = n.clone();
            self.advance();
            let size = if self.eat(&Token::Comma) {
                let Some(Token::Num(n2)) = self.peek() else {
                    return Err(self.unexpected("a number"));
                };
                let second = n2.clone();
                self.advance();
                ast::TypeSize::TypeSize(numeric_expr(&first), numeric_expr(&second))
            } else {
                ast::TypeSize::MaxSize(numeric_expr(&first))
            };
            self.expect(&Token::RParen, "`)`")?;
            Ok(Some(size))
        } else {
            // ENUM('a','b') / SET(...): consume the list, keep no size.
            self.skip_balanced_rest()?;
            Ok(None)
        }
    }

    /// Parses zero or more inline column constraints. Returns the constraints,
    /// whether `AUTO_INCREMENT` was declared, and any explicit `COLLATE` name (so
    /// the caller can decide the column's case sensitivity).
    fn column_constraints(
        &mut self,
    ) -> Result<(Vec<ast::NamedColumnConstraint>, bool, Option<String>)> {
        let mut out: Vec<ast::NamedColumnConstraint> = Vec::new();
        let mut auto_increment = false;
        let mut collation: Option<String> = None;
        let mut primary_key_at = None;

        loop {
            if self.eat_keyword("NOT") {
                self.expect_keyword("NULL")?;
                out.push(named(ast::ColumnConstraint::NotNull {
                    nullable: false,
                    conflict_clause: None,
                }));
            } else if self.eat_keyword("NULL") {
                out.push(named(ast::ColumnConstraint::NotNull {
                    nullable: true,
                    conflict_clause: None,
                }));
            } else if self.eat_keyword("PRIMARY") {
                self.eat_keyword("KEY");
                primary_key_at = Some(out.len());
                out.push(named(ast::ColumnConstraint::PrimaryKey {
                    order: None,
                    conflict_clause: None,
                    auto_increment: false,
                }));
            } else if self.eat_keyword("UNIQUE") {
                self.eat_keyword("KEY");
                out.push(named(ast::ColumnConstraint::Unique(None)));
            } else if self.eat_keyword("AUTO_INCREMENT") {
                auto_increment = true;
            } else if self.eat_keyword("DEFAULT") {
                let expr = self.default_value()?;
                out.push(named(ast::ColumnConstraint::Default(expr)));
            } else if self.eat_keyword("COMMENT") {
                self.expect_string()?;
            } else if self.eat_keyword("COLLATE") {
                collation = Some(self.name()?.as_str().to_string());
            } else if self.eat_keyword("CHARACTER") {
                self.expect_keyword("SET")?;
                let _ = self.name()?;
            } else if self.eat_keyword("CHARSET") {
                let _ = self.name()?;
            } else if self.eat_keyword("UNSIGNED")
                || self.eat_keyword("ZEROFILL")
                || self.eat_keyword("SIGNED")
            {
                // Stray type modifier; ignore.
            } else if self.eat_keyword("ON") {
                // `ON UPDATE <expr>` / `ON DELETE <expr>`: skip both tokens.
                let _ = self.name()?;
                let _ = self.default_value()?;
            } else if self.eat_keyword("CHECK") {
                // MySQL 8.0+ enforces CHECK, and so does the engine. Pass the
                // constraint through when its expression translates; if it uses
                // something the front-end cannot model, drop it (as before) so the
                // table is still created.
                let saved = self.pos;
                match self.check_constraint_expr() {
                    Ok(expr) => out.push(named(ast::ColumnConstraint::Check(Box::new(expr)))),
                    Err(_) => {
                        self.pos = saved;
                        self.skip_to_item_boundary();
                        break;
                    }
                }
            } else if self.is_keyword("REFERENCES")
                || self.is_keyword("GENERATED")
                || self.is_keyword("AS")
                || self.is_keyword("KEY")
            {
                // Inline clauses we do not model yet: skip to the item boundary.
                self.skip_to_item_boundary();
                break;
            } else {
                break;
            }
        }

        if auto_increment {
            if let Some(i) = primary_key_at {
                if let ast::ColumnConstraint::PrimaryKey {
                    auto_increment: ai, ..
                } = &mut out[i].constraint
                {
                    *ai = true;
                }
            }
        }
        Ok((out, auto_increment, collation))
    }

    /// Parses a column `DEFAULT` value into a literal expression.
    fn default_value(&mut self) -> Result<Box<ast::Expr>> {
        let negative = if self.eat(&Token::Minus) {
            true
        } else {
            self.eat(&Token::Plus);
            false
        };
        match self.peek() {
            Some(Token::Num(n)) => {
                let s = if negative { format!("-{n}") } else { n.clone() };
                self.advance();
                Ok(numeric_expr(&s))
            }
            Some(Token::Str(s)) => {
                let lit = requote(s);
                self.advance();
                Ok(Box::new(ast::Expr::Literal(ast::Literal::String(lit))))
            }
            Some(Token::Word(w)) => {
                let literal = match w.to_ascii_uppercase().as_str() {
                    "NULL" => ast::Literal::Null,
                    "TRUE" => ast::Literal::True,
                    "FALSE" => ast::Literal::False,
                    "CURRENT_TIMESTAMP" | "NOW" | "LOCALTIME" | "LOCALTIMESTAMP" => {
                        ast::Literal::CurrentTimestamp
                    }
                    "CURRENT_DATE" => ast::Literal::CurrentDate,
                    "CURRENT_TIME" => ast::Literal::CurrentTime,
                    _ => ast::Literal::Keyword(w.clone()),
                };
                self.advance();
                // Function-style default such as `NOW()`: drop the parens.
                if self.is(&Token::LParen) {
                    self.skip_balanced()?;
                }
                Ok(Box::new(ast::Expr::Literal(literal)))
            }
            // A parenthesized expression default: not modeled, treated as NULL.
            Some(Token::LParen) => {
                self.skip_balanced()?;
                Ok(Box::new(ast::Expr::Literal(ast::Literal::Null)))
            }
            _ => Err(self.unexpected("a default value")),
        }
    }

    fn next_is_table_constraint(&self) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if is_table_constraint_keyword(w))
    }

    fn table_constraint(
        &mut self,
        out: &mut Vec<ast::NamedTableConstraint>,
        indexes: &mut Vec<(Option<ast::Name>, Vec<ast::SortedColumn>, bool)>,
    ) -> Result<()> {
        let mut name = None;
        if self.eat_keyword("CONSTRAINT") {
            // Optional symbol name (absent if the constraint type follows).
            if matches!(self.peek(), Some(Token::QuotedIdent(_)))
                || matches!(self.peek(), Some(Token::Word(w)) if !is_table_constraint_keyword(w))
            {
                name = Some(self.name()?);
            }
        }

        if self.eat_keyword("PRIMARY") {
            self.expect_keyword("KEY")?;
            let columns = self.sorted_column_list()?;
            out.push(ast::NamedTableConstraint {
                name,
                constraint: ast::TableConstraint::PrimaryKey {
                    columns,
                    auto_increment: false,
                    conflict_clause: None,
                },
            });
        } else if self.eat_keyword("UNIQUE") {
            self.eat_keyword("KEY");
            self.eat_keyword("INDEX");
            // Optional index name before the column list.
            let idx_name = if !self.is(&Token::LParen)
                && matches!(
                    self.peek(),
                    Some(Token::Word(_)) | Some(Token::QuotedIdent(_))
                )
            {
                Some(self.name()?)
            } else {
                None
            };
            let columns = self.sorted_column_list()?;
            // A *named* unique key becomes a deferred `CREATE UNIQUE INDEX` so
            // `SHOW INDEX` reports it under that name (which `dbDelta` looks up by
            // name); the constraint symbol names it when the key itself is
            // unnamed. An entirely unnamed `UNIQUE (cols)` stays a table
            // constraint, whose index the engine auto-names.
            match idx_name.or(name) {
                Some(named) => indexes.push((Some(named), columns, true)),
                None => out.push(ast::NamedTableConstraint {
                    name: None,
                    constraint: ast::TableConstraint::Unique {
                        columns,
                        conflict_clause: None,
                    },
                }),
            }
        } else if self.eat_keyword("CHECK") {
            // A table-level CHECK, enforced by the engine like MySQL 8.0+. Pass it
            // through when translatable, else drop it (keeping the symbol name).
            let saved = self.pos;
            match self.check_constraint_expr() {
                Ok(expr) => out.push(ast::NamedTableConstraint {
                    name,
                    constraint: ast::TableConstraint::Check(Box::new(expr)),
                }),
                Err(_) => {
                    self.pos = saved;
                    self.skip_to_item_boundary();
                }
            }
        } else if self.is_keyword("FOREIGN") {
            // Foreign keys have no engine equivalent: skip them.
            self.skip_to_item_boundary();
        } else if self.is_keyword("KEY")
            || self.is_keyword("INDEX")
            || self.is_keyword("FULLTEXT")
            || self.is_keyword("SPATIAL")
        {
            // An inline secondary index `[FULLTEXT|SPATIAL] {KEY|INDEX} [name]
            // (cols)`. The engine's CREATE TABLE has no inline secondary index,
            // so capture it for a deferred CREATE INDEX (FULLTEXT/SPATIAL degrade
            // to a plain index, as the ALTER ADD forms do). A column prefix length
            // (`col(191)`) and a `USING` type are dropped by the column list.
            self.eat_keyword("FULLTEXT");
            self.eat_keyword("SPATIAL");
            let _ = self.eat_keyword("KEY") || self.eat_keyword("INDEX");
            let idx_name = if self.is(&Token::LParen) || self.is_keyword("USING") {
                None
            } else {
                Some(self.name()?)
            };
            if self.eat_keyword("USING") {
                let _ = self.name()?;
            }
            let columns = self.sorted_column_list()?;
            if self.eat_keyword("USING") {
                let _ = self.name()?;
            }
            indexes.push((idx_name, columns, false));
        } else {
            return Err(self.unexpected("a table constraint"));
        }
        Ok(())
    }

    /// Parses a `CHECK (expr)` constraint body (the `CHECK` keyword is already
    /// consumed), returning the bracketed expression.
    fn check_constraint_expr(&mut self) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        let expr = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(expr)
    }

    fn sorted_column_list(&mut self) -> Result<Vec<ast::SortedColumn>> {
        self.expect(&Token::LParen, "`(`")?;
        let mut columns = Vec::new();
        loop {
            let name = self.name()?;
            // Optional index prefix length, e.g. `name(10)`.
            if self.is(&Token::LParen) {
                self.skip_balanced()?;
            }
            let order = if self.eat_keyword("ASC") {
                Some(ast::SortOrder::Asc)
            } else if self.eat_keyword("DESC") {
                Some(ast::SortOrder::Desc)
            } else {
                None
            };
            columns.push(ast::SortedColumn {
                expr: Box::new(ast::Expr::Id(name)),
                order,
                nulls: None,
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(columns)
    }

    // === DROP TABLE ===

    fn drop(&mut self) -> Result<ast::Stmt> {
        // `DROP` has already been consumed.
        let temporary = self.eat_keyword("TEMPORARY");
        if self.eat_keyword("TABLE") {
            self.drop_table(temporary)
        } else if temporary {
            Err(ParseError::Unsupported(
                "DROP TEMPORARY only applies to tables".to_string(),
            ))
        } else if self.eat_keyword("INDEX") {
            self.drop_index()
        } else {
            let what = match self.peek() {
                Some(Token::Word(w)) => w.to_ascii_uppercase(),
                _ => "?".to_string(),
            };
            Err(ParseError::Unsupported(format!(
                "DROP {what} is not supported yet (only DROP TABLE / DROP INDEX are implemented)"
            )))
        }
    }

    /// Parses `DROP INDEX [IF EXISTS] idx_name [ON tbl_name]` (`DROP INDEX` is
    /// already consumed) and lowers it to the engine's `DROP INDEX idx_name`.
    /// MySQL spells it `DROP INDEX idx ON tbl`; the engine's index namespace is
    /// per-database (see [`Self::alter_add_index`]), so the `ON tbl` qualifier is
    /// parsed and ignored.
    fn drop_index(&mut self) -> Result<ast::Stmt> {
        let if_exists = if self.eat_keyword("IF") {
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let idx_name = self.qualified_name()?;
        if self.eat_keyword("ON") {
            let _ = self.qualified_name()?;
        }
        Ok(ast::Stmt::DropIndex {
            if_exists,
            idx_name,
        })
    }

    /// Parses the `DROP [TEMPORARY] TABLE [IF EXISTS] tbl_name` form. With
    /// `IF EXISTS`, dropping a non-existent table is a no-op success, matching
    /// MySQL. `DROP TEMPORARY TABLE` is qualified onto the engine's temp schema
    /// so it drops only the temporary table, never a base table of the same name
    /// — exactly MySQL's semantics. The multi-table and `RESTRICT`/`CASCADE`
    /// variants are still rejected as unsupported.
    fn drop_table(&mut self, temporary: bool) -> Result<ast::Stmt> {
        // `DROP [TEMPORARY] TABLE` has already been consumed.
        let if_exists = if self.eat_keyword("IF") {
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };

        let tbl_name = self.qualified_name()?;

        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "DROP TABLE with multiple tables is not supported yet".to_string(),
            ));
        }
        if self.is_keyword("RESTRICT") || self.is_keyword("CASCADE") {
            return Err(ParseError::Unsupported(
                "DROP TABLE RESTRICT / CASCADE is not supported yet".to_string(),
            ));
        }

        make_drop_table(temporary, if_exists, tbl_name)
    }

    /// Parses the comma-separated table list of a multi-table `DROP [TEMPORARY]
    /// TABLE [IF EXISTS] a, b, ...` (the `DROP [TEMPORARY] TABLE` keywords already
    /// consumed) into one [`ast::Stmt::DropTable`] per table — MySQL drops them
    /// in one statement, but the engine has no multi-table `DROP`, so the caller
    /// (the statement-list entry point) runs the produced statements in sequence.
    fn drop_table_list(&mut self, temporary: bool) -> Result<Vec<ast::Stmt>> {
        let if_exists = if self.eat_keyword("IF") {
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let mut stmts = Vec::new();
        loop {
            let tbl_name = self.qualified_name()?;
            stmts.push(make_drop_table(temporary, if_exists, tbl_name)?);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        if self.is_keyword("RESTRICT") || self.is_keyword("CASCADE") {
            return Err(ParseError::Unsupported(
                "DROP TABLE RESTRICT / CASCADE is not supported yet".to_string(),
            ));
        }
        Ok(stmts)
    }

    // === INSERT ===

    /// Parses the `INSERT INTO tbl [(cols)] VALUES (...)[, (...)]` and
    /// `INSERT INTO tbl [(cols)] SELECT ...` forms. `INSERT ... SET` and the
    /// priority modifiers (`LOW_PRIORITY`/`DELAYED`/`HIGH_PRIORITY`) are rejected
    /// as unsupported; `INSERT IGNORE` lowers to `INSERT OR IGNORE`.
    /// Parses an `INSERT ... VALUES` statement, or — when `or_conflict` is
    /// `Some(ResolveType::Replace)` — a `REPLACE ... VALUES` statement. MySQL's
    /// `REPLACE` deletes any row that conflicts on a primary/unique key before
    /// inserting, which is exactly the engine's `INSERT OR REPLACE`. The keyword
    /// has already been consumed.
    fn insert(&mut self, or_conflict: Option<ast::ResolveType>) -> Result<ast::Stmt> {
        // Reset any row-alias state from a previous INSERT in the same batch.
        self.upsert_row_alias = None;
        self.upsert_col_aliases.clear();
        // MySQL's priority modifiers (`LOW_PRIORITY`/`DELAYED`/`HIGH_PRIORITY`) are
        // locking/scheduling hints with no bearing on the result, and `DELAYED` is
        // deprecated and treated as a normal insert by modern MySQL too; consume
        // and ignore them. (They also precede `REPLACE`.)
        while self.eat_keyword("LOW_PRIORITY")
            || self.eat_keyword("DELAYED")
            || self.eat_keyword("HIGH_PRIORITY")
        {}

        // `INSERT IGNORE` downgrades row-level errors — notably duplicate-key
        // conflicts — to warnings and skips the offending row, which is exactly
        // the engine's `INSERT OR IGNORE`. `REPLACE IGNORE` is not valid MySQL,
        // so the modifier only applies when no conflict resolution is set yet.
        let or_conflict = if or_conflict.is_none() && self.eat_keyword("IGNORE") {
            Some(ast::ResolveType::Ignore)
        } else {
            or_conflict
        };

        self.eat_keyword("INTO"); // `INTO` is optional in MySQL
        let tbl_name = self.qualified_name()?;

        // Optional explicit column list. The empty list `()` is the MySQL
        // all-defaults form (`INSERT INTO t () VALUES ()`), handled below.
        let mut columns = Vec::new();
        if self.eat(&Token::LParen) {
            if !self.is(&Token::RParen) {
                loop {
                    columns.push(self.name()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen, "`)`")?;
        }

        // `INSERT [INTO] tbl SET col = expr, ...` is MySQL's assignment-list form,
        // equivalent to `INSERT INTO tbl (col, ...) VALUES (expr, ...)`. It only
        // applies without an explicit column list.
        if columns.is_empty() && self.eat_keyword("SET") {
            return self.insert_set(or_conflict, tbl_name);
        }

        // `INSERT [INTO] tbl [(cols)] SELECT ...`: the rows come from a query,
        // which the engine runs directly. The SELECT goes through the same
        // parser as a top-level one, so it supports the same subset. A trailing
        // `ON DUPLICATE KEY UPDATE` (valid MySQL, but not modeled here) is not
        // accepted: `ON` is consumed as the final column's alias, so the leftover
        // `DUPLICATE ...` surfaces as a syntax error.
        if self.eat_keyword("SELECT") {
            let select = self.parse_select()?;
            return Ok(ast::Stmt::Insert {
                with: None,
                or_conflict,
                tbl_name,
                columns,
                body: ast::InsertBody::Select(select, None),
                returning: Vec::new(),
            });
        }

        // Only the VALUES / VALUE form is supported otherwise.
        if !(self.eat_keyword("VALUES") || self.eat_keyword("VALUE")) {
            return Err(self.unexpected("`VALUES` or `SELECT`"));
        }

        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen, "`(`")?;
            let mut row = Vec::new();
            if !self.is(&Token::RParen) {
                loop {
                    row.push(Box::new(self.value_or_default()?));
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen, "`)`")?;
            rows.push(row);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }

        // MySQL 8.0.19+ row alias: `VALUES (...) AS alias [(col, ...)]` names the
        // new row so `ON DUPLICATE KEY UPDATE` can reference it as `alias.col`
        // instead of the deprecated `VALUES(col)`. Capture the alias (and any
        // column aliases, mapped positionally to the INSERT column list) so the
        // upsert assignment can rewrite `alias.col` to the engine's `excluded.col`.
        if or_conflict.is_none() && self.eat_keyword("AS") {
            let alias = self.name()?;
            self.upsert_row_alias = Some(alias.as_str().to_string());
            if self.eat(&Token::LParen) {
                let mut col_aliases = Vec::new();
                loop {
                    col_aliases.push(self.name()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen, "`)`")?;
                if columns.is_empty() || col_aliases.len() != columns.len() {
                    return Err(ParseError::Unsupported(
                        "INSERT ... AS alias (cols) needs a matching INSERT column list".to_string(),
                    ));
                }
                self.upsert_col_aliases = col_aliases
                    .iter()
                    .zip(&columns)
                    .map(|(a, c)| (a.as_str().to_string(), c.as_str().to_string()))
                    .collect();
            }
        }

        // `ON DUPLICATE KEY UPDATE` is an INSERT-only clause; REPLACE has its
        // own conflict resolution and does not take it.
        let upsert = if or_conflict.is_none() && self.eat_keyword("ON") {
            Some(Box::new(self.on_duplicate_key_update()?))
        } else {
            None
        };

        // `INSERT INTO t () VALUES ()` / `INSERT INTO t VALUES ()` — a single
        // empty row with no column list inserts one all-defaults row, which is
        // the engine's `DEFAULT VALUES`. (Multiple empty rows have no
        // single-statement engine equivalent and fall through.)
        if columns.is_empty() && upsert.is_none() && rows.len() == 1 && rows[0].is_empty() {
            return Ok(ast::Stmt::Insert {
                with: None,
                or_conflict,
                tbl_name,
                columns,
                body: ast::InsertBody::DefaultValues,
                returning: Vec::new(),
            });
        }

        Ok(ast::Stmt::Insert {
            with: None,
            or_conflict,
            tbl_name,
            columns,
            body: ast::InsertBody::Select(
                ast::Select {
                    with: None,
                    body: ast::SelectBody {
                        select: ast::OneSelect::Values(rows),
                        compounds: Vec::new(),
                    },
                    order_by: Vec::new(),
                    limit: None,
                },
                upsert,
            ),
            returning: Vec::new(),
        })
    }

    /// Parses the `INSERT [INTO] tbl SET col = expr [, ...]` assignment-list form
    /// (the `SET` keyword is already consumed) and builds the same statement as
    /// the equivalent `INSERT INTO tbl (col, ...) VALUES (expr, ...)`. A trailing
    /// `ON DUPLICATE KEY UPDATE` is honored for the non-`REPLACE` form, exactly as
    /// for the VALUES form.
    fn insert_set(
        &mut self,
        or_conflict: Option<ast::ResolveType>,
        tbl_name: ast::QualifiedName,
    ) -> Result<ast::Stmt> {
        let mut columns = Vec::new();
        let mut row = Vec::new();
        loop {
            columns.push(self.name()?);
            self.expect(&Token::Eq, "`=`")?;
            row.push(Box::new(self.expr()?));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }

        let upsert = if or_conflict.is_none() && self.eat_keyword("ON") {
            Some(Box::new(self.on_duplicate_key_update()?))
        } else {
            None
        };

        Ok(ast::Stmt::Insert {
            with: None,
            or_conflict,
            tbl_name,
            columns,
            body: ast::InsertBody::Select(
                ast::Select {
                    with: None,
                    body: ast::SelectBody {
                        select: ast::OneSelect::Values(vec![row]),
                        compounds: Vec::new(),
                    },
                    order_by: Vec::new(),
                    limit: None,
                },
                upsert,
            ),
            returning: Vec::new(),
        })
    }

    /// Parses MySQL `ON DUPLICATE KEY UPDATE col = expr [, ...]` and lowers it to
    /// the engine's target-less upsert (`ON CONFLICT DO UPDATE SET ...`), which —
    /// like MySQL — fires on a conflict with any unique or primary key. The
    /// `VALUES(col)` pseudo-function (the would-be-inserted value) is mapped to
    /// the engine's `excluded.col`. `ON` has already been consumed.
    fn on_duplicate_key_update(&mut self) -> Result<ast::Upsert> {
        self.expect_keyword("DUPLICATE")?;
        self.expect_keyword("KEY")?;
        self.expect_keyword("UPDATE")?;

        let mut sets = Vec::new();
        loop {
            let col = self.name()?;
            self.expect(&Token::Eq, "`=`")?;
            let expr = self.upsert_assignment_value()?;
            sets.push(ast::Set {
                col_names: vec![col],
                expr: Box::new(expr),
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }

        Ok(ast::Upsert {
            index: None,
            do_clause: ast::UpsertDo::Set {
                sets,
                where_clause: None,
            },
            next: None,
        })
    }

    /// Parses an `INSERT ... VALUES` element, recognizing the MySQL `DEFAULT`
    /// keyword (insert the column's declared default) as the engine's
    /// `Expr::Default`. Anything else is an ordinary expression. (The engine only
    /// honors `DEFAULT` in `INSERT ... VALUES`, not in `UPDATE ... SET`, so this
    /// is used there only.)
    fn value_or_default(&mut self) -> Result<ast::Expr> {
        if self.is_keyword("DEFAULT") && self.peek_nth(1) != Some(&Token::LParen) {
            self.advance();
            return Ok(ast::Expr::Default);
        }
        self.expr()
    }

    /// Parses the right-hand side of an `ON DUPLICATE KEY UPDATE` assignment as an
    /// ordinary expression, with `VALUES(col)` understood (anywhere in the
    /// expression) as the would-be-inserted value, lowered to `excluded.col` (see
    /// [`Self::function_call`]). A bare column refers to the existing row's value,
    /// as in MySQL.
    fn upsert_assignment_value(&mut self) -> Result<ast::Expr> {
        self.in_upsert_assignment = true;
        let result = self.expr();
        self.in_upsert_assignment = false;
        result
    }

    // === SELECT ===

    /// Parses a basic single-table `SELECT`:
    ///
    /// ```text
    /// SELECT <list> [FROM <table>] [WHERE <expr>]
    ///        [ORDER BY <expr> [ASC|DESC], ...] [LIMIT <n> [OFFSET <m>]]
    /// ```
    ///
    /// Comma joins, subqueries, set operations, and CTEs are rejected as
    /// unsupported; `INNER`/`LEFT` JOINs, `GROUP BY`/`HAVING`, `DISTINCT`, and
    /// aggregates are supported.
    fn select(&mut self) -> Result<ast::Stmt> {
        // `SELECT` has already been consumed.
        Ok(ast::Stmt::Select(self.parse_select()?))
    }

    /// Parses a `WITH ... SELECT ...` statement (the `WITH` keyword already
    /// consumed): a common-table-expression clause followed by a `SELECT` that it
    /// feeds. The engine evaluates CTEs the same as SQLite, so the clause is
    /// attached to the resulting `Select`. Only a `SELECT` main query is
    /// supported here (MySQL also allows `WITH` before `UPDATE`/`DELETE`).
    fn with_select(&mut self) -> Result<ast::Stmt> {
        let with = self.with_clause()?;
        self.expect_keyword("SELECT")?;
        let mut select = self.parse_select()?;
        select.with = Some(with);
        Ok(ast::Stmt::Select(select))
    }

    /// Parses MySQL 8's `TABLE tbl [ORDER BY ...] [LIMIT ...]` statement (the
    /// `TABLE` keyword already consumed), shorthand for `SELECT * FROM tbl [...]`.
    /// It is built as exactly that `SELECT *` and the trailing `ORDER BY`/`LIMIT`
    /// are parsed by the shared compound-select tail.
    fn table_statement(&mut self) -> Result<ast::Stmt> {
        let tbl_name = self.qualified_name()?;
        let one = ast::OneSelect::Select {
            distinctness: None,
            columns: vec![ast::ResultColumn::Star],
            from: Some(ast::FromClause {
                select: Box::new(ast::SelectTable::Table(tbl_name, None, None)),
                joins: Vec::new(),
            }),
            where_clause: None,
            group_by: None,
            window_clause: Vec::new(),
        };
        Ok(ast::Stmt::Select(self.finish_compound_select(one)?))
    }

    /// Parses the body of a `WITH [RECURSIVE] cte [, cte]...` clause (the `WITH`
    /// keyword already consumed). Each CTE is `name [(col, ...)] AS [[NOT]
    /// MATERIALIZED] (select)`; the optional materialization hint is preserved and
    /// the column-rename list, if present, is recorded.
    fn with_clause(&mut self) -> Result<ast::With> {
        let recursive = self.eat_keyword("RECURSIVE");
        let mut ctes = Vec::new();
        loop {
            let tbl_name = self.name()?;

            // Optional `(col, ...)` rename list for the CTE's output columns.
            let mut columns = Vec::new();
            if self.eat(&Token::LParen) {
                loop {
                    columns.push(ast::IndexedColumn {
                        col_name: self.name()?,
                        collation_name: None,
                        order: None,
                    });
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen, "`)`")?;
            }

            self.expect_keyword("AS")?;

            // Optional `MATERIALIZED` / `NOT MATERIALIZED` hint.
            let materialized = if self.eat_keyword("MATERIALIZED") {
                ast::Materialized::Yes
            } else if self.is_keyword("NOT")
                && matches!(self.peek_nth(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("MATERIALIZED"))
            {
                self.advance();
                self.advance();
                ast::Materialized::No
            } else {
                ast::Materialized::Any
            };

            self.expect(&Token::LParen, "`(`")?;
            self.expect_keyword("SELECT")?;
            let select = self.parse_select()?;
            self.expect(&Token::RParen, "`)`")?;

            ctes.push(ast::CommonTableExpr {
                tbl_name,
                columns,
                materialized,
                select,
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(ast::With { recursive, ctes })
    }

    /// Parses a `SELECT` body (everything after the `SELECT` keyword), including
    /// any `UNION [ALL]` / `INTERSECT` / `EXCEPT` compounds and a trailing
    /// `ORDER BY` / `LIMIT` that applies to the whole result, into an
    /// `ast::Select`. Shared by the top-level statement and `IN`/`EXISTS`
    /// subqueries.
    fn parse_select(&mut self) -> Result<ast::Select> {
        let first = self.parse_one_select()?;
        self.finish_compound_select(first)
    }

    /// Parses a single compound-select branch — either a bare `SELECT ...` or a
    /// parenthesized `(SELECT ...)`. MySQL allows each `UNION` branch to be
    /// parenthesized (often the whole query is `(SELECT ...) UNION (SELECT ...)`);
    /// the parentheses are purely grouping here and are stripped. A per-branch
    /// `ORDER BY` / `LIMIT` inside the parentheses cannot be represented in the
    /// engine's flat compound model and is rejected.
    fn parse_compound_branch(&mut self) -> Result<ast::OneSelect> {
        if self.eat(&Token::LParen) {
            self.expect_keyword("SELECT")?;
            let select = self.parse_one_select()?;
            if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
                return Err(ParseError::Unsupported(
                    "ORDER BY / LIMIT inside a parenthesized UNION branch is not supported yet"
                        .to_string(),
                ));
            }
            self.expect(&Token::RParen, "`)`")?;
            Ok(select)
        } else {
            self.expect_keyword("SELECT")?;
            self.parse_one_select()
        }
    }

    /// Completes a compound select given its already-parsed first branch: the
    /// `UNION`/`INTERSECT`/`EXCEPT` set-operation branches and the trailing
    /// whole-result `ORDER BY` / `LIMIT`. Shared by the bare and the
    /// leading-parenthesis (`(SELECT ...) UNION ...`) entry points.
    fn finish_compound_select(&mut self, first: ast::OneSelect) -> Result<ast::Select> {
        // Set-operation compounds. The operators map straight onto the engine's
        // identical semantics (`UNION` and `INTERSECT`/`EXCEPT` deduplicate;
        // `UNION ALL` does not). Each branch may be parenthesized. The explicit
        // `DISTINCT` quantifier is the default for every set operation, so it is
        // consumed and ignored (`UNION DISTINCT` == `UNION`).
        let mut compounds = Vec::new();
        loop {
            let operator = if self.eat_keyword("UNION") {
                if self.eat_keyword("ALL") {
                    ast::CompoundOperator::UnionAll
                } else {
                    self.eat_keyword("DISTINCT");
                    ast::CompoundOperator::Union
                }
            } else if self.eat_keyword("INTERSECT") {
                self.eat_keyword("DISTINCT");
                ast::CompoundOperator::Intersect
            } else if self.eat_keyword("EXCEPT") {
                self.eat_keyword("DISTINCT");
                ast::CompoundOperator::Except
            } else {
                break;
            };
            let select = self.parse_compound_branch()?;
            compounds.push(ast::CompoundSelect { operator, select });
        }

        let order_by = self.order_by()?;
        let limit = self.limit()?;
        self.skip_locking_clause();

        if self.is_keyword("INTO") {
            return Err(ParseError::Unsupported(
                "SELECT ... INTO is not supported yet".to_string(),
            ));
        }

        Ok(ast::Select {
            with: None,
            body: ast::SelectBody {
                select: first,
                compounds,
            },
            order_by,
            limit,
        })
    }

    /// Parses a statement that begins with `(` — a parenthesized leading select
    /// branch, as in `(SELECT ...) UNION (SELECT ...)`. The first branch's parens
    /// are stripped, then the rest of the compound is parsed normally.
    fn paren_select_statement(&mut self) -> Result<ast::Stmt> {
        self.expect(&Token::LParen, "`(`")?;
        self.expect_keyword("SELECT")?;
        let first = self.parse_one_select()?;
        if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
            return Err(ParseError::Unsupported(
                "ORDER BY / LIMIT inside a parenthesized UNION branch is not supported yet"
                    .to_string(),
            ));
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Stmt::Select(self.finish_compound_select(first)?))
    }

    /// Consumes and discards an optional trailing row-locking clause —
    /// `FOR UPDATE`, `FOR SHARE`, or `LOCK IN SHARE MODE`, including the
    /// `FOR ... [OF tbl [, tbl] ...] [NOWAIT | SKIP LOCKED]` refinements. The
    /// engine is a single writer, so explicit row locking is a no-op and the
    /// locked query returns exactly the same rows as the unlocked one (`NOWAIT` /
    /// `SKIP LOCKED` only change behaviour under contention, which cannot arise);
    /// see `mysql/COMPAT.md`.
    fn skip_locking_clause(&mut self) {
        if self.is_keyword("FOR") {
            // Only `FOR UPDATE` / `FOR SHARE` is a locking clause; leave anything
            // else beginning with `FOR` for the caller to reject.
            if matches!(
                self.peek_nth(1),
                Some(Token::Word(w)) if w.eq_ignore_ascii_case("UPDATE") || w.eq_ignore_ascii_case("SHARE")
            ) {
                self.advance(); // FOR
                self.advance(); // UPDATE | SHARE

                // `OF tbl [, tbl] ...` names the tables to lock; consume and
                // discard the (optionally `db.`-qualified) name list.
                if self.eat_keyword("OF") {
                    loop {
                        if matches!(self.peek(), Some(Token::Word(_))) {
                            self.advance();
                            if self.eat(&Token::Dot) {
                                self.advance(); // qualified table name
                            }
                        }
                        if self.eat(&Token::Comma) {
                            continue;
                        }
                        break;
                    }
                }

                // The lock-acquisition option `NOWAIT` or `SKIP LOCKED`.
                if !self.eat_keyword("NOWAIT") && self.eat_keyword("SKIP") {
                    self.eat_keyword("LOCKED");
                }
            }
        } else if self.is_keyword("LOCK") {
            // `LOCK IN SHARE MODE`.
            if matches!(self.peek_nth(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("IN")) {
                self.advance(); // LOCK
                self.eat_keyword("IN");
                self.eat_keyword("SHARE");
                self.eat_keyword("MODE");
            }
        }
    }

    /// Parses a single `SELECT` branch — distinctness, the column list, and the
    /// optional `FROM` / `WHERE` / `GROUP BY` clauses — without the trailing
    /// `ORDER BY` / `LIMIT` or any set-operation compound, which belong to the
    /// surrounding compound select.
    fn parse_one_select(&mut self) -> Result<ast::OneSelect> {
        // `DISTINCTROW` is MySQL's synonym for `DISTINCT`; treat it identically.
        let distinctness = if self.eat_keyword("DISTINCT") || self.eat_keyword("DISTINCTROW") {
            Some(ast::Distinctness::Distinct)
        } else {
            self.eat_keyword("ALL"); // the default quantifier; accepted and ignored
            None
        };

        // MySQL SELECT modifiers that are optimizer or query-cache hints with no
        // effect on the result set are consumed and ignored. `SQL_CALC_FOUND_ROWS`
        // is also consumed here, but the server separately detects it from the
        // SQL text to maintain the `FOUND_ROWS()` count. Lenient about order.
        loop {
            if self.eat_keyword("HIGH_PRIORITY")
                || self.eat_keyword("STRAIGHT_JOIN")
                || self.eat_keyword("SQL_SMALL_RESULT")
                || self.eat_keyword("SQL_BIG_RESULT")
                || self.eat_keyword("SQL_BUFFER_RESULT")
                || self.eat_keyword("SQL_NO_CACHE")
                || self.eat_keyword("SQL_CACHE")
                || self.eat_keyword("SQL_CALC_FOUND_ROWS")
            {
                continue;
            }
            break;
        }

        let columns = self.select_list()?;

        let from = if self.eat_keyword("FROM") {
            let from = self.from_clause()?;
            // MySQL's dummy `DUAL` table (`SELECT 1 FROM DUAL`) is equivalent to
            // having no `FROM` at all; drop it (the engine has no `DUAL` table).
            if from_is_dual(&from) {
                None
            } else {
                Some(from)
            }
        } else {
            None
        };

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };

        let group_by = self.group_by()?;

        // MySQL allows a standalone `HAVING` (no `GROUP BY`) to filter rows —
        // WordPress's custom-fields query is `SELECT DISTINCT meta_key ... HAVING
        // meta_key NOT LIKE '_%'`. The engine rejects a non-aggregate `HAVING`,
        // so fold such a `HAVING` into the `WHERE` clause, where its row filtering
        // is equivalent. An aggregate `HAVING` is a whole-table aggregate the
        // engine handles, so it stays in place — including one that filters on an
        // aggregate via its SELECT-list alias (`SELECT COUNT(*) c ... HAVING
        // c > 3`), which must not be folded into `WHERE` (an aggregate is not
        // allowed there).
        let (where_clause, group_by) = match group_by {
            Some(ast::GroupBy {
                exprs,
                having: Some(having),
            }) if exprs.is_empty()
                && !expr_contains_aggregate(&having)
                && !expr_references_name(&having, &aggregate_alias_names(&columns)) =>
            {
                let combined = match where_clause {
                    Some(w) => ast::Expr::binary(*w, ast::Operator::And, *having),
                    None => *having,
                };
                (Some(Box::new(combined)), None)
            }
            other => (where_clause, other),
        };

        Ok(ast::OneSelect::Select {
            distinctness,
            columns,
            from,
            where_clause,
            group_by,
            window_clause: Vec::new(),
        })
    }

    fn select_list(&mut self) -> Result<Vec<ast::ResultColumn>> {
        let mut columns = Vec::new();
        loop {
            if self.eat(&Token::Star) {
                columns.push(ast::ResultColumn::Star);
            } else if matches!(
                self.peek(),
                Some(Token::Word(_)) | Some(Token::QuotedIdent(_))
            ) && self.peek_nth(1) == Some(&Token::Dot)
                && self.peek_nth(2) == Some(&Token::Star)
            {
                // `tbl.*`
                let name = self.name()?;
                self.advance(); // `.`
                self.advance(); // `*`
                columns.push(ast::ResultColumn::TableStar(name));
            } else {
                let start = self.offset_here();
                let expr = self.expr()?;
                let end = self.offset_here();
                let alias = match self.column_alias()? {
                    Some(explicit) => Some(explicit),
                    // An unaliased non-trivial expression takes its verbatim
                    // source text as the column name, as MySQL does.
                    None => self.implicit_column_label(&expr, start, end),
                };
                columns.push(ast::ResultColumn::Expr(Box::new(expr), alias));
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(columns)
    }

    /// The byte offset of the current token (or end-of-input past the last one).
    fn offset_here(&self) -> usize {
        self.tokens.get(self.pos).map(|(_, off)| *off).unwrap_or(self.eof)
    }

    /// The default column label for an unaliased select-list expression spanning
    /// the source bytes `[start, end)`. MySQL labels such a column with the
    /// verbatim source text of the expression — `UPPER('x')`, `COUNT(*)`, `a+b` —
    /// rather than a re-rendered form (which the engine would otherwise print with
    /// stray spaces, qualified column names, and the lowered function bodies). A
    /// bare column reference (`a`, `t.a`) and a literal are excluded: the engine
    /// already labels those the way MySQL does (the column name / the value), and
    /// the verbatim text would wrongly keep the table qualifier or quotes. The
    /// label is attached as an [`ast::As::ImplicitColumnName`] so it names the
    /// column without becoming a referenceable alias.
    fn implicit_column_label(&self, expr: &ast::Expr, start: usize, end: usize) -> Option<ast::As> {
        match expr {
            // A bare/qualified column reference keeps the engine's label (the
            // column name), which already matches MySQL.
            ast::Expr::Id(_) | ast::Expr::Qualified(_, _) | ast::Expr::Name(_) => return None,
            // A string literal is labelled by its *decoded* value (`'it''s'` →
            // `it's`), which the engine would otherwise keep quoted.
            ast::Expr::Literal(ast::Literal::String(s)) => {
                let decoded = s
                    .strip_prefix('\'')
                    .and_then(|t| t.strip_suffix('\''))
                    .map(|t| t.replace("''", "'"))?;
                return Some(ast::As::ImplicitColumnName(ast::Name::exact(decoded)));
            }
            // A hex literal is labelled by its verbatim source (`0x41`, `X'41'`),
            // handled by the source slice below; the engine would render the blob.
            ast::Expr::Literal(ast::Literal::Blob(_)) => {}
            // Other literals (numeric, NULL) are labelled correctly by the engine
            // (the value / `NULL`).
            ast::Expr::Literal(_) => return None,
            _ => {}
        }
        let text = String::from_utf8_lossy(self.input.get(start..end)?);
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        // `Name::exact` stores the text verbatim; `from_string` would try to
        // interpret a leading quote (e.g. `'x' + 1`) as a quoted identifier and
        // panic when the closing quote doesn't match.
        Some(ast::As::ImplicitColumnName(ast::Name::exact(
            trimmed.to_string(),
        )))
    }

    /// Parses an optional select-list column alias: `AS name`, a backtick-quoted
    /// name, or a bare identifier that is not a clause keyword that ends the
    /// select list (e.g. `FROM`).
    fn column_alias(&mut self) -> Result<Option<ast::As>> {
        if self.eat_keyword("AS") {
            return Ok(Some(ast::As::As(self.alias_name()?)));
        }
        match self.peek() {
            Some(Token::QuotedIdent(_)) => Ok(Some(ast::As::Elided(self.name()?))),
            Some(Token::Word(w)) if !is_reserved_select_alias(w) => {
                Ok(Some(ast::As::Elided(self.name()?)))
            }
            _ => Ok(None),
        }
    }

    /// Parses a column alias following `AS`. MySQL allows the alias to be written
    /// as a string literal (`expr AS 'name'`), not just an identifier; the
    /// string's text is used as the (case-exact) alias name. Only the `AS` form
    /// accepts a string — the elided `SELECT 1 'name'` form is ambiguous in
    /// MySQL (adjacent string literals concatenate) and is not handled.
    fn alias_name(&mut self) -> Result<ast::Name> {
        if let Some(Token::Str(s)) = self.peek() {
            let name = ast::Name::exact(s.clone());
            self.advance();
            return Ok(name);
        }
        self.name()
    }

    /// Parses the `FROM` clause: a table reference optionally followed by comma
    /// joins (`a, b`) and/or `[INNER] JOIN` / `LEFT [OUTER] JOIN` joins with an
    /// `ON` condition. A comma join is an implicit cross join whose condition is
    /// supplied by `WHERE`; the engine evaluates it identically to MySQL.
    /// `RIGHT`/`FULL`/`CROSS`/`NATURAL`/`STRAIGHT_JOIN`, `USING`, ON-less keyword
    /// joins, and subqueries are rejected as unsupported.
    // Not a constructor: the `from_` prefix names the SQL `FROM` clause, so the
    // `wrong_self_convention` heuristic does not apply here.
    #[allow(clippy::wrong_self_convention)]
    fn from_clause(&mut self) -> Result<ast::FromClause> {
        let select = Box::new(self.table_ref()?);

        let mut joins = Vec::new();
        loop {
            // Comma join: `a, b` is a cross join; the `WHERE` clause supplies the
            // join condition, as in MySQL.
            if self.eat(&Token::Comma) {
                joins.push(ast::JoinedSelectTable {
                    operator: ast::JoinOperator::Comma,
                    table: Box::new(self.table_ref()?),
                    constraint: None,
                });
                continue;
            }
            let Some(operator) = self.join_operator()? else {
                break;
            };
            let table = Box::new(self.table_ref()?);
            let constraint = if self.eat_keyword("ON") {
                Some(ast::JoinConstraint::On(Box::new(self.expr()?)))
            } else if self.eat_keyword("USING") {
                // `USING (col, ...)`: an equi-join on the named columns shared by
                // both tables, which the engine evaluates the same as MySQL
                // (coalescing each join column into one output column).
                self.expect(&Token::LParen, "`(`")?;
                let mut cols = Vec::new();
                loop {
                    cols.push(self.name()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen, "`)`")?;
                Some(ast::JoinConstraint::Using(cols))
            } else if matches!(
                operator,
                ast::JoinOperator::TypedJoin(Some(t))
                    if t.contains(ast::JoinType::NATURAL) || !t.contains(ast::JoinType::OUTER)
            ) {
                // A join may omit the condition when it is a `NATURAL` join (which
                // joins on the common columns, including its `LEFT`/`RIGHT` forms)
                // or any inner join: MySQL treats a plain `JOIN` / `INNER JOIN` /
                // `STRAIGHT_JOIN` with no `ON`/`USING` as a `CROSS JOIN` (Cartesian
                // product), often with the predicate moved to `WHERE` instead. The
                // engine evaluates all of these identically. Only a non-NATURAL
                // OUTER (`LEFT`/`RIGHT`) join requires an explicit condition.
                None
            } else {
                return Err(ParseError::Unsupported(
                    "OUTER JOIN without an ON or USING condition is not supported yet".to_string(),
                ));
            };
            joins.push(ast::JoinedSelectTable {
                operator,
                table,
                constraint,
            });
        }

        Ok(ast::FromClause { select, joins })
    }

    /// Parses a single table reference: `tbl [[AS] alias]`. Subqueries and table
    /// functions are not modeled.
    fn table_ref(&mut self) -> Result<ast::SelectTable> {
        // A derived table: `(SELECT ...) alias`. MySQL requires the alias, which
        // is enforced here; parenthesized joins are not modeled.
        if self.eat(&Token::LParen) {
            if !self.eat_keyword("SELECT") {
                return Err(ParseError::Unsupported(
                    "only a derived table — `(SELECT ...)` — is supported in parentheses"
                        .to_string(),
                ));
            }
            let select = self.parse_select()?;
            self.expect(&Token::RParen, "`)`")?;
            let alias = self.table_alias()?;
            if alias.is_none() {
                return Err(ParseError::Unsupported(
                    "a derived table requires an alias".to_string(),
                ));
            }
            return Ok(ast::SelectTable::Select(select, alias));
        }
        let tbl_name = self.qualified_name()?;

        // `information_schema.TABLES` has no engine equivalent; rewrite a reference
        // to it into a derived table synthesized from the engine catalog (see
        // `information_schema_tables_select`). WordPress's upgrade and Site Health
        // routines query it. Other `information_schema` tables stay unsupported.
        if is_information_schema_tables(&tbl_name) {
            let alias = self.table_alias()?;
            self.skip_index_hints()?;
            return Ok(ast::SelectTable::Select(
                information_schema_tables_select()?,
                alias,
            ));
        }

        // `information_schema.COLUMNS`, likewise, is synthesized from the engine
        // catalog (see `information_schema_columns_select`). WordPress's charset
        // detection and Site Health read per-column metadata from it.
        if is_information_schema_columns(&tbl_name) {
            let alias = self.table_alias()?;
            self.skip_index_hints()?;
            return Ok(ast::SelectTable::Select(
                information_schema_columns_select(),
                alias,
            ));
        }

        // `information_schema.STATISTICS` (per-index-column metadata) is
        // synthesized the same way (see `information_schema_statistics_select`).
        if is_information_schema_statistics(&tbl_name) {
            let alias = self.table_alias()?;
            self.skip_index_hints()?;
            return Ok(ast::SelectTable::Select(
                information_schema_statistics_select(),
                alias,
            ));
        }

        // `information_schema.TABLE_CONSTRAINTS` (one row per primary-key / unique
        // constraint) is synthesized the same way (see
        // `information_schema_table_constraints_select`).
        if is_information_schema_table_constraints(&tbl_name) {
            let alias = self.table_alias()?;
            self.skip_index_hints()?;
            return Ok(ast::SelectTable::Select(
                information_schema_table_constraints_select(),
                alias,
            ));
        }

        // `information_schema.KEY_COLUMN_USAGE` (one row per key/unique-constraint
        // column) is synthesized the same way (see
        // `information_schema_key_column_usage_select`).
        if is_information_schema_key_column_usage(&tbl_name) {
            let alias = self.table_alias()?;
            self.skip_index_hints()?;
            return Ok(ast::SelectTable::Select(
                information_schema_key_column_usage_select(),
                alias,
            ));
        }

        let alias = self.table_alias()?;
        self.skip_index_hints()?;
        Ok(ast::SelectTable::Table(tbl_name, alias, None))
    }

    /// Consumes zero or more MySQL index hints following a table reference:
    /// `{USE|IGNORE|FORCE} {INDEX|KEY} [FOR {JOIN|ORDER BY|GROUP BY}] (idx, ...)`.
    /// Index hints only steer MySQL's optimizer; the engine plans its own access
    /// path, so they are parsed and discarded (the result set is identical). The
    /// index list may be empty (`USE INDEX ()`), and `PRIMARY` is accepted as a
    /// name.
    fn skip_index_hints(&mut self) -> Result<()> {
        loop {
            let is_hint = matches!(self.peek(), Some(Token::Word(w))
                if w.eq_ignore_ascii_case("USE")
                    || w.eq_ignore_ascii_case("FORCE")
                    || w.eq_ignore_ascii_case("IGNORE"))
                && matches!(self.peek_nth(1), Some(Token::Word(w))
                    if w.eq_ignore_ascii_case("INDEX") || w.eq_ignore_ascii_case("KEY"));
            if !is_hint {
                break;
            }
            self.advance(); // USE / FORCE / IGNORE
            self.advance(); // INDEX / KEY

            // Optional `FOR {JOIN | ORDER BY | GROUP BY}` scope.
            if self.eat_keyword("FOR") {
                if self.eat_keyword("JOIN") {
                    // no further tokens
                } else if self.eat_keyword("ORDER") || self.eat_keyword("GROUP") {
                    self.expect_keyword("BY")?;
                } else {
                    return Err(self.unexpected("`JOIN`, `ORDER BY`, or `GROUP BY`"));
                }
            }

            // The parenthesized index list (possibly empty); names may be
            // identifiers or the `PRIMARY` keyword.
            self.expect(&Token::LParen, "`(`")?;
            while !self.is(&Token::RParen) {
                match self.peek() {
                    Some(Token::Word(_)) | Some(Token::QuotedIdent(_)) => self.advance(),
                    _ => return Err(self.unexpected("an index name")),
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RParen, "`)`")?;
        }
        Ok(())
    }

    /// Parses an optional table alias: `AS name`, a backtick-quoted name, or a
    /// bare identifier that is not a keyword which may follow a table reference.
    fn table_alias(&mut self) -> Result<Option<ast::As>> {
        if self.eat_keyword("AS") {
            return Ok(Some(ast::As::As(self.name()?)));
        }
        match self.peek() {
            Some(Token::QuotedIdent(_)) => Ok(Some(ast::As::Elided(self.name()?))),
            Some(Token::Word(w)) if !is_reserved_after_table(w) => {
                Ok(Some(ast::As::Elided(self.name()?)))
            }
            _ => Ok(None),
        }
    }

    /// Parses a join operator, or `None` if no join follows. Only `[INNER] JOIN`
    /// and `LEFT [OUTER] JOIN` are modeled; the other join types are rejected.
    fn join_operator(&mut self) -> Result<Option<ast::JoinOperator>> {
        if self.eat_keyword("INNER") {
            self.expect_keyword("JOIN")?;
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::INNER,
            ))));
        }
        if self.eat_keyword("LEFT") {
            self.eat_keyword("OUTER");
            self.expect_keyword("JOIN")?;
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::LEFT | ast::JoinType::OUTER,
            ))));
        }
        if self.eat_keyword("RIGHT") {
            self.eat_keyword("OUTER");
            self.expect_keyword("JOIN")?;
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::RIGHT | ast::JoinType::OUTER,
            ))));
        }
        if self.eat_keyword("CROSS") {
            self.expect_keyword("JOIN")?;
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::INNER | ast::JoinType::CROSS,
            ))));
        }
        // `STRAIGHT_JOIN` is an `INNER JOIN` that forces left-to-right table
        // order; the engine has no such hint, so it lowers to a plain INNER join
        // (an identical result set).
        if self.eat_keyword("STRAIGHT_JOIN") {
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::INNER,
            ))));
        }
        // `NATURAL [LEFT|RIGHT [OUTER]] JOIN` joins on the columns common to both
        // tables, with no explicit condition; the engine evaluates it directly.
        if self.eat_keyword("NATURAL") {
            let side = if self.eat_keyword("LEFT") {
                self.eat_keyword("OUTER");
                ast::JoinType::LEFT | ast::JoinType::OUTER
            } else if self.eat_keyword("RIGHT") {
                self.eat_keyword("OUTER");
                ast::JoinType::RIGHT | ast::JoinType::OUTER
            } else {
                ast::JoinType::INNER
            };
            self.expect_keyword("JOIN")?;
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::NATURAL | side,
            ))));
        }
        if self.eat_keyword("JOIN") {
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::INNER,
            ))));
        }
        // MySQL has no `FULL [OUTER] JOIN`, so it is rejected (not accepted as a
        // non-MySQL extension even though the engine could evaluate it).
        if self.is_keyword("FULL") {
            return Err(ParseError::Unsupported(
                "FULL join is not supported yet".to_string(),
            ));
        }
        Ok(None)
    }

    /// Parses an optional `GROUP BY [HAVING]` clause, or a standalone `HAVING`.
    ///
    /// GROUP BY terms must be column expressions, not integer ordinals: MySQL
    /// treats `GROUP BY 1` as "the first output column", but SQLite treats it as
    /// the constant `1` (one group) — a divergence, so ordinals are rejected.
    ///
    /// `HAVING` without `GROUP BY` treats the whole result as a single group; it
    /// is modeled as an empty `GROUP BY` with a `HAVING`. MySQL and the engine
    /// both accept it for an aggregate condition (the engine rejects a
    /// non-aggregate `HAVING`, as MySQL's filtering form is not modeled).
    fn group_by(&mut self) -> Result<Option<ast::GroupBy>> {
        if !self.eat_keyword("GROUP") {
            if self.eat_keyword("HAVING") {
                return Ok(Some(ast::GroupBy {
                    exprs: Vec::new(),
                    having: Some(Box::new(self.expr()?)),
                }));
            }
            return Ok(None);
        }
        self.expect_keyword("BY")?;
        let mut exprs = Vec::new();
        loop {
            let expr = self.expr()?;
            if matches!(expr, ast::Expr::Literal(ast::Literal::Numeric(_))) {
                return Err(ParseError::Unsupported(
                    "GROUP BY with a column ordinal is not supported (use a column name)"
                        .to_string(),
                ));
            }
            exprs.push(Box::new(expr));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        let having = if self.eat_keyword("HAVING") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        Ok(Some(ast::GroupBy { exprs, having }))
    }

    /// Parses an optional `ORDER BY` clause (shared by SELECT). Returns an empty
    /// vec when absent.
    fn order_by(&mut self) -> Result<Vec<ast::SortedColumn>> {
        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let expr = self.expr()?;
                let order = if self.eat_keyword("ASC") {
                    Some(ast::SortOrder::Asc)
                } else if self.eat_keyword("DESC") {
                    Some(ast::SortOrder::Desc)
                } else {
                    None
                };
                order_by.push(ast::SortedColumn {
                    expr: Box::new(expr),
                    order,
                    nulls: None,
                });
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        Ok(order_by)
    }

    /// Parses an optional `LIMIT` clause, handling both MySQL spellings:
    /// `LIMIT count`, `LIMIT offset, count`, and `LIMIT count OFFSET offset`.
    fn limit(&mut self) -> Result<Option<ast::Limit>> {
        if !self.eat_keyword("LIMIT") {
            return Ok(None);
        }
        let first = clamp_limit_literal(self.expr()?);
        if self.eat(&Token::Comma) {
            let count = clamp_limit_literal(self.expr()?);
            Ok(Some(ast::Limit {
                expr: Box::new(count),
                offset: Some(Box::new(first)),
            }))
        } else if self.eat_keyword("OFFSET") {
            let offset = clamp_limit_literal(self.expr()?);
            Ok(Some(ast::Limit {
                expr: Box::new(first),
                offset: Some(Box::new(offset)),
            }))
        } else {
            Ok(Some(ast::Limit {
                expr: Box::new(first),
                offset: None,
            }))
        }
    }

    /// Parses an optional MySQL `LIMIT <count>` row-limit for `UPDATE`/`DELETE` —
    /// the count-only form (MySQL does not allow an `OFFSET` / `offset, count`
    /// here). The engine applies it to cap the number of affected rows. Without an
    /// `ORDER BY` (which the engine cannot honor on `UPDATE`/`DELETE`, so it stays
    /// rejected) MySQL likewise affects an unspecified `count` rows, so the two
    /// match.
    fn row_limit(&mut self) -> Result<Option<ast::Limit>> {
        if !self.eat_keyword("LIMIT") {
            return Ok(None);
        }
        let count = clamp_limit_literal(self.expr()?);
        if self.is(&Token::Comma) || self.is_keyword("OFFSET") {
            return Err(ParseError::Unsupported(
                "LIMIT on UPDATE/DELETE takes a row count only (no offset)".to_string(),
            ));
        }
        Ok(Some(ast::Limit {
            expr: Box::new(count),
            offset: None,
        }))
    }

    // === UPDATE ===

    /// Builds the `rowid IN (SELECT rowid FROM tbl [WHERE cond] ORDER BY ord
    /// LIMIT n)` predicate used to rewrite a single-table `DELETE`/`UPDATE ...
    /// ORDER BY ... LIMIT`. The engine cannot order a `DELETE`/`UPDATE` in place,
    /// so the ordering and row cap are folded into a subquery that selects
    /// exactly the rows MySQL would touch (by rowid), and the outer statement's
    /// `WHERE` becomes a membership test against them. `tbl_name` is the single
    /// target table; `where_clause`/`order_by`/`limit` are the parsed outer
    /// clauses, moved into the subquery.
    fn rowid_in_ordered_subquery(
        &self,
        tbl_name: &ast::QualifiedName,
        where_clause: Option<Box<ast::Expr>>,
        order_by: Vec<ast::SortedColumn>,
        limit: Option<ast::Limit>,
    ) -> ast::Expr {
        let select = ast::OneSelect::Select {
            distinctness: None,
            columns: vec![ast::ResultColumn::Expr(
                Box::new(ast::Expr::Id(ast::Name::from_string("rowid"))),
                None,
            )],
            from: Some(ast::FromClause {
                select: Box::new(ast::SelectTable::Table(tbl_name.clone(), None, None)),
                joins: Vec::new(),
            }),
            where_clause,
            group_by: None,
            window_clause: Vec::new(),
        };
        let subquery = ast::Select {
            with: None,
            body: ast::SelectBody {
                select,
                compounds: Vec::new(),
            },
            order_by,
            limit,
        };
        ast::Expr::InSelect {
            lhs: Box::new(ast::Expr::Id(ast::Name::from_string("rowid"))),
            not: false,
            rhs: subquery,
        }
    }

    /// Parses `UPDATE tbl SET col = expr [, ...] [WHERE expr] [ORDER BY ...]
    /// [LIMIT n]`. A bare `LIMIT n` caps the affected-row count directly; an
    /// `ORDER BY` (with or without `LIMIT`) is rewritten through a `rowid`
    /// subquery (see [`Self::rowid_in_ordered_subquery`]). The comma form of a
    /// multi-table update is handled by [`Self::multi_table_update`]; the
    /// `LOW_PRIORITY` modifier is not translated.
    fn update(&mut self) -> Result<ast::Stmt> {
        // `UPDATE` has already been consumed. `LOW_PRIORITY` is a locking hint
        // with no result effect; consume it. `UPDATE IGNORE` skips a row whose
        // update would raise an error (e.g. a duplicate-key violation) instead of
        // aborting — exactly the engine's `UPDATE OR IGNORE`.
        self.eat_keyword("LOW_PRIORITY");
        let or_conflict = if self.eat_keyword("IGNORE") {
            Some(ast::ResolveType::Ignore)
        } else {
            None
        };

        let tbl_name = self.qualified_name()?;
        // Multi-table comma form, with or without an alias on the target table:
        //   `UPDATE t1, ...`  |  `UPDATE t1 x, ...`  |  `UPDATE t1 AS x, ...`.
        // The first table is the update target; the rest are read-only sources.
        // Look ahead for the comma so a single-table target (whose alias the
        // existing path does not parse) is left completely untouched.
        let is_multi = self.is(&Token::Comma)
            || (self.is_alias_word() && self.peek_nth(1) == Some(&Token::Comma))
            || (self.is_keyword("AS") && self.peek_nth(2) == Some(&Token::Comma));
        if is_multi {
            let target_alias = self.table_alias()?;
            return self.multi_table_update(tbl_name, target_alias, or_conflict);
        }
        // The explicit-JOIN spelling of a multi-table update is not modeled.
        if self.is_keyword("JOIN")
            || self.is_keyword("INNER")
            || self.is_keyword("LEFT")
            || self.is_keyword("RIGHT")
            || self.is_keyword("CROSS")
            || self.is_keyword("STRAIGHT_JOIN")
            || self.is_keyword("NATURAL")
        {
            return Err(ParseError::Unsupported(
                "multi-table UPDATE with an explicit JOIN is not supported yet \
                 (use the comma-separated form)"
                    .to_string(),
            ));
        }

        self.expect_keyword("SET")?;
        let mut sets = Vec::new();
        loop {
            let col = self.name()?;
            self.expect(&Token::Eq, "`=`")?;
            let expr = self.expr()?;
            sets.push(ast::Set {
                col_names: vec![col],
                expr: Box::new(expr),
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };

        // `ORDER BY ... LIMIT n` picks the n rows to update by sort order. The
        // engine cannot order an UPDATE, so fold the ordering and cap into a
        // `rowid` subquery; a bare `LIMIT n` is passed through (the engine caps
        // the affected-row count directly).
        let order_by = self.order_by()?;
        let limit = self.row_limit()?;
        if !order_by.is_empty() {
            let where_clause =
                self.rowid_in_ordered_subquery(&tbl_name, where_clause, order_by, limit);
            return Ok(ast::Stmt::Update(ast::Update {
                with: None,
                or_conflict,
                tbl_name,
                indexed: None,
                sets,
                from: None,
                where_clause: Some(Box::new(where_clause)),
                returning: Vec::new(),
                order_by: Vec::new(),
                limit: None,
            }));
        }

        Ok(ast::Stmt::Update(ast::Update {
            with: None,
            or_conflict,
            tbl_name,
            indexed: None,
            sets,
            from: None,
            where_clause,
            returning: Vec::new(),
            order_by: Vec::new(),
            limit,
        }))
    }

    /// Parses the comma form of a multi-table update,
    /// `UPDATE t1, t2, ... SET t1.col = expr [, ...] [WHERE ...]`, after `t1`
    /// (`target`) and before the first comma. MySQL updates the table(s) named on
    /// the `SET` left-hand sides; this handles the common single-target case
    /// where that table is the one listed first.
    ///
    /// It lowers to the engine's `UPDATE target SET col = expr FROM <the other
    /// tables> WHERE ...`. The engine joins the source tables to the target and
    /// updates only the matching target rows — exactly MySQL's multi-table
    /// semantics (rows of `target` with no join match are left unchanged). A
    /// `SET` column qualified with any other table (which would update a
    /// different table) is rejected, as are `ORDER BY`/`LIMIT`, which MySQL does
    /// not allow on a multi-table update.
    fn multi_table_update(
        &mut self,
        mut target: ast::QualifiedName,
        target_alias: Option<ast::As>,
        or_conflict: Option<ast::ResolveType>,
    ) -> Result<ast::Stmt> {
        // The target is named in `SET`/`WHERE` by its alias if it has one, else
        // its table name; carry the alias onto the target so the engine's
        // `UPDATE <table> AS <alias>` resolves those references.
        let alias_name = target_alias.map(|a| {
            let (ast::As::As(n) | ast::As::Elided(n) | ast::As::ImplicitColumnName(n)) = a;
            n
        });
        let target_name = alias_name
            .as_ref()
            .map_or_else(|| target.name.as_str().to_string(), |n| n.as_str().to_string());
        target.alias = alias_name;

        // The remaining comma-separated references are the read-only sources.
        let mut sources = Vec::new();
        while self.eat(&Token::Comma) {
            sources.push(self.table_ref()?);
        }

        self.expect_keyword("SET")?;
        let mut sets = Vec::new();
        loop {
            // A `SET` target may be written `col` or `target.col`; a qualifier
            // naming any other table would update it, which this form does not do.
            let first = self.name()?;
            let col = if self.eat(&Token::Dot) {
                let column = self.name()?;
                if !first.as_str().eq_ignore_ascii_case(&target_name) {
                    return Err(ParseError::Unsupported(format!(
                        "multi-table UPDATE only updates the first-listed table \
                         `{target_name}`, not `{}`",
                        first.as_str()
                    )));
                }
                column
            } else {
                first
            };
            self.expect(&Token::Eq, "`=`")?;
            let expr = self.expr()?;
            sets.push(ast::Set {
                col_names: vec![col],
                expr: Box::new(expr),
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
            return Err(ParseError::Unsupported(
                "ORDER BY / LIMIT on a multi-table UPDATE is not supported".to_string(),
            ));
        }

        // The sources become the engine's FROM clause: the first is the primary,
        // the rest comma-joined (their join conditions live in WHERE, as in MySQL).
        let mut sources = sources.into_iter();
        let first_source = sources.next().expect("the comma guaranteed one source");
        let from = ast::FromClause {
            select: Box::new(first_source),
            joins: sources
                .map(|table| ast::JoinedSelectTable {
                    operator: ast::JoinOperator::Comma,
                    table: Box::new(table),
                    constraint: None,
                })
                .collect(),
        };

        Ok(ast::Stmt::Update(ast::Update {
            with: None,
            or_conflict,
            tbl_name: target,
            indexed: None,
            sets,
            from: Some(from),
            where_clause,
            returning: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        }))
    }

    // === TRANSACTIONS ===

    /// Parses `START TRANSACTION` or `BEGIN [WORK]` into the engine's `BEGIN`.
    /// MySQL `START TRANSACTION` and `BEGIN` both open an explicit transaction
    /// that the engine's deferred `BEGIN` matches. MySQL-only modifiers
    /// (`READ ONLY`/`READ WRITE`/`WITH CONSISTENT SNAPSHOT`) change isolation in
    /// ways the engine does not model, so they are rejected. `BEGIN`/`START` has
    /// already been consumed.
    fn begin_transaction(&mut self, keyword: &str) -> Result<ast::Stmt> {
        if keyword == "START" {
            self.expect_keyword("TRANSACTION")?;
        } else {
            self.eat_keyword("WORK");
        }
        if self.has_trailing_tokens() {
            return Err(ParseError::Unsupported(
                "transaction characteristics (READ ONLY / READ WRITE / WITH CONSISTENT SNAPSHOT) are not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::Begin {
            typ: None,
            name: None,
        })
    }

    /// Parses `COMMIT [WORK]`. `COMMIT` has already been consumed.
    fn commit_transaction(&mut self) -> Result<ast::Stmt> {
        self.eat_keyword("WORK");
        if self.has_trailing_tokens() {
            return Err(ParseError::Unsupported(
                "COMMIT ... AND CHAIN / RELEASE is not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::Commit { name: None })
    }

    /// Parses `ROLLBACK [WORK]` and `ROLLBACK [WORK] TO [SAVEPOINT] name`. The
    /// latter undoes the statements since that savepoint, which the engine
    /// supports natively (the `SAVEPOINT` keyword is optional, as in MySQL). The
    /// `AND CHAIN`/`RELEASE` modifiers are rejected. `ROLLBACK` has already been
    /// consumed.
    fn rollback_transaction(&mut self) -> Result<ast::Stmt> {
        self.eat_keyword("WORK");
        if self.eat_keyword("TO") {
            self.eat_keyword("SAVEPOINT");
            let name = self.name()?;
            return Ok(ast::Stmt::Rollback {
                tx_name: None,
                savepoint_name: Some(name),
            });
        }
        if self.has_trailing_tokens() {
            return Err(ParseError::Unsupported(
                "ROLLBACK ... AND CHAIN / RELEASE is not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::Rollback {
            tx_name: None,
            savepoint_name: None,
        })
    }

    /// Parses `SAVEPOINT name`, marking a point a later `ROLLBACK TO` can return
    /// to — the engine's native savepoint. `SAVEPOINT` has already been consumed.
    fn savepoint(&mut self) -> Result<ast::Stmt> {
        let name = self.name()?;
        Ok(ast::Stmt::Savepoint { name })
    }

    /// Parses `RELEASE SAVEPOINT name`, which discards a savepoint without
    /// rolling back (MySQL requires the `SAVEPOINT` keyword here). `RELEASE` has
    /// already been consumed.
    fn release_savepoint(&mut self) -> Result<ast::Stmt> {
        self.expect_keyword("SAVEPOINT")?;
        let name = self.name()?;
        Ok(ast::Stmt::Release { name })
    }

    /// Whether any non-terminating token remains before the end of the
    /// statement (a trailing `;` and end-of-input both count as the end).
    fn has_trailing_tokens(&self) -> bool {
        self.pos < self.tokens.len() && !self.is(&Token::Semicolon)
    }

    // === DELETE ===

    /// Parses `DELETE FROM tbl [WHERE expr]`. Multi-table deletes,
    /// `DELETE ... USING`, `ORDER BY`/`LIMIT`, and the
    /// `LOW_PRIORITY`/`QUICK`/`IGNORE` modifiers are rejected as unsupported.
    /// Parses `TRUNCATE [TABLE] tbl`. The engine has no `TRUNCATE`, so this is
    /// translated to an unfiltered `DELETE FROM tbl`, which leaves the table
    /// empty just like `TRUNCATE`. The behavioral differences — `TRUNCATE`'s
    /// implicit commit, `AUTO_INCREMENT` reset, and zero reported affected-row
    /// count — are not reproduced; see `mysql/COMPAT.md`. `TRUNCATE` has already
    /// been consumed.
    fn truncate_table(&mut self) -> Result<ast::Stmt> {
        self.eat_keyword("TABLE");
        let tbl_name = self.qualified_name()?;
        if self.has_trailing_tokens() {
            return Err(ParseError::Unsupported(
                "TRUNCATE with trailing tokens is not supported yet".to_string(),
            ));
        }
        Ok(ast::Stmt::Delete {
            with: None,
            tbl_name,
            indexed: None,
            where_clause: None,
            returning: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        })
    }

    fn delete(&mut self) -> Result<ast::Stmt> {
        // `DELETE` has already been consumed. `LOW_PRIORITY`/`QUICK` are
        // scheduling/space hints with no result effect. `DELETE IGNORE` only
        // downgrades errors that the engine does not raise on a single-table
        // delete anyway (it enforces no foreign keys here), so all three are
        // consumed and ignored.
        while self.eat_keyword("LOW_PRIORITY")
            || self.eat_keyword("QUICK")
            || self.eat_keyword("IGNORE")
        {}

        // The multi-table form is `DELETE t1 FROM ...` — a target list before
        // `FROM`, rather than `DELETE FROM tbl`.
        if !self.is_keyword("FROM") {
            return self.multi_table_delete();
        }
        self.expect_keyword("FROM")?;

        let tbl_name = self.qualified_name()?;
        if self.is(&Token::Comma) {
            // `DELETE FROM t1, t2 USING ...` — the other multi-table spelling.
            return Err(ParseError::Unsupported(
                "multi-table DELETE is not supported yet".to_string(),
            ));
        }
        if self.is_keyword("USING") {
            return Err(ParseError::Unsupported(
                "DELETE ... USING is not supported yet".to_string(),
            ));
        }

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };

        // `ORDER BY ... LIMIT n` deletes the n rows that sort first. The engine
        // cannot order a DELETE, so fold the ordering and cap into a `rowid`
        // subquery; a bare `LIMIT n` is passed through (the engine caps the
        // affected-row count directly).
        let order_by = self.order_by()?;
        let limit = self.row_limit()?;
        if !order_by.is_empty() {
            let where_clause =
                self.rowid_in_ordered_subquery(&tbl_name, where_clause, order_by, limit);
            return Ok(ast::Stmt::Delete {
                with: None,
                tbl_name,
                indexed: None,
                where_clause: Some(Box::new(where_clause)),
                returning: Vec::new(),
                order_by: Vec::new(),
                limit: None,
            });
        }

        Ok(ast::Stmt::Delete {
            with: None,
            tbl_name,
            indexed: None,
            where_clause,
            returning: Vec::new(),
            order_by: Vec::new(),
            limit,
        })
    }

    /// Parses a multi-table `DELETE <targets> FROM <refs> [WHERE ...]` (the
    /// target list precedes `FROM`). It is lowered to
    /// `DELETE FROM <table> WHERE rowid IN (SELECT t1.rowid FROM <refs> [WHERE]
    /// [UNION SELECT t2.rowid ...])`, which the engine evaluates identically to
    /// MySQL: the `rowid` subquery (including the `UNION` of every target's
    /// rowids) is materialized against the pre-delete state before any row is
    /// removed, so no two-phase delete is needed. All targets must resolve to
    /// the **same** table — differing tables would need separate deletes and are
    /// rejected. `DELETE` has already been consumed.
    fn multi_table_delete(&mut self) -> Result<ast::Stmt> {
        let mut targets = vec![self.qualified_name()?];
        while self.eat(&Token::Comma) {
            targets.push(self.qualified_name()?);
        }
        self.expect_keyword("FROM")?;
        let from = self.from_clause()?;
        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
            return Err(ParseError::Unsupported(
                "ORDER BY / LIMIT on DELETE is not supported yet".to_string(),
            ));
        }

        // Resolve every target alias/name to its underlying table; they must all
        // be the same table for the single-DELETE-with-UNION lowering to match
        // MySQL's multi-table delete.
        let mut resolved = Vec::with_capacity(targets.len());
        for target in &targets {
            let alias = target.name.as_str();
            let Some(table) = resolve_delete_target(&from, alias) else {
                return Err(ParseError::Unsupported(format!(
                    "multi-table DELETE target `{alias}` is not a table in the FROM clause"
                )));
            };
            resolved.push((alias.to_string(), table));
        }
        let table = resolved[0].1.clone();
        if resolved
            .iter()
            .any(|(_, t)| !t.name.as_str().eq_ignore_ascii_case(table.name.as_str()))
        {
            return Err(ParseError::Unsupported(
                "multi-table DELETE across different tables is not supported yet".to_string(),
            ));
        }

        // One `SELECT <target>.rowid FROM <refs> [WHERE ...]` per target.
        let rowid_select = |alias: &str| ast::OneSelect::Select {
            distinctness: None,
            columns: vec![ast::ResultColumn::Expr(
                Box::new(ast::Expr::Qualified(
                    ast::Name::from_string(alias),
                    ast::Name::from_string("rowid"),
                )),
                None,
            )],
            from: Some(from.clone()),
            where_clause: where_clause.clone(),
            group_by: None,
            window_clause: Vec::new(),
        };
        let select = rowid_select(&resolved[0].0);
        let compounds = resolved[1..]
            .iter()
            .map(|(alias, _)| ast::CompoundSelect {
                operator: ast::CompoundOperator::Union,
                select: rowid_select(alias),
            })
            .collect();

        let subquery = ast::Select {
            with: None,
            body: ast::SelectBody { select, compounds },
            order_by: Vec::new(),
            limit: None,
        };

        Ok(ast::Stmt::Delete {
            with: None,
            tbl_name: table,
            indexed: None,
            where_clause: Some(Box::new(ast::Expr::InSelect {
                lhs: Box::new(ast::Expr::Id(ast::Name::from_string("rowid"))),
                not: false,
                rhs: subquery,
            })),
            returning: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        })
    }

    // === Expressions ===
    //
    // A small expression grammar for WHERE predicates, INSERT values, and
    // SET assignments. Precedence, lowest to highest:
    //
    //     OR  <  AND  <  NOT  <  comparison (= <> < <= > >=, IS NULL, IN,
    //     BETWEEN, LIKE)  <  additive (+ -)  <  multiplicative (*)  <  primary
    //
    // The divergent operators `/`, `%`, and `||` are intentionally not parsed.

    /// Parses an expression at the lowest precedence level (`OR`).
    fn expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.xor_expr()?;
        while self.eat_keyword("OR") {
            let rhs = self.xor_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::Or, rhs);
        }
        Ok(lhs)
    }

    /// Logical-XOR tier: `XOR`, between `OR` and `AND` in MySQL precedence,
    /// left-associative. The engine has no `XOR`, so `a XOR b` lowers to
    /// `(a <> 0) <> (b <> 0)` — 1 when exactly one operand is truthy. NULL
    /// propagates naturally (`NULL <> 0` is NULL). This matches MySQL for numeric
    /// and boolean operands; a non-numeric string's truthiness diverges (the
    /// engine does not coerce it to 0), a documented edge (see `mysql/COMPAT.md`).
    fn xor_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.and_expr()?;
        while self.eat_keyword("XOR") {
            let rhs = self.and_expr()?;
            lhs = logical_xor(lhs, rhs);
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.not_expr()?;
        // `&&` is a MySQL synonym for the `AND` keyword, at the same precedence.
        while self.eat_keyword("AND") || self.eat(&Token::AmpAmp) {
            let rhs = self.not_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::And, rhs);
        }
        Ok(lhs)
    }

    fn not_expr(&mut self) -> Result<ast::Expr> {
        if self.eat_keyword("NOT") {
            let inner = self.not_expr()?;
            return Ok(ast::Expr::unary(ast::UnaryOperator::Not, inner));
        }
        self.comparison_expr()
    }

    fn comparison_expr(&mut self) -> Result<ast::Expr> {
        let lhs = self.bitor_expr()?;

        // `IS [NOT] {NULL | UNKNOWN | TRUE | FALSE}`. `UNKNOWN` is a synonym for
        // `NULL`. The boolean tests never yield NULL in MySQL, so `IS TRUE`
        // lowers to `coalesce(x <> 0, 0)` and `IS FALSE` to `coalesce(x = 0, 0)`
        // (their `IS NOT` forms flip the comparison and default to 1).
        if self.eat_keyword("IS") {
            let not = self.eat_keyword("NOT");
            if self.eat_keyword("NULL") || self.eat_keyword("UNKNOWN") {
                return Ok(if not {
                    ast::Expr::not_null(lhs)
                } else {
                    ast::Expr::is_null(lhs)
                });
            }
            if self.eat_keyword("TRUE") {
                let (op, default) = if not {
                    (ast::Operator::Equals, "1")
                } else {
                    (ast::Operator::NotEquals, "0")
                };
                return Ok(coalesce_truthiness(lhs, op, default));
            }
            if self.eat_keyword("FALSE") {
                let (op, default) = if not {
                    (ast::Operator::NotEquals, "1")
                } else {
                    (ast::Operator::Equals, "0")
                };
                return Ok(coalesce_truthiness(lhs, op, default));
            }
            return Err(self.unexpected("`NULL`, `UNKNOWN`, `TRUE`, or `FALSE`"));
        }

        // Infix `[NOT] IN / BETWEEN / LIKE`. At this point any prefix `NOT` has
        // already been consumed by `not_expr`, so a `NOT` here is infix.
        let not = self.eat_keyword("NOT");
        if self.eat_keyword("IN") {
            return self.in_list(lhs, not);
        }
        if self.eat_keyword("BETWEEN") {
            return self.between(lhs, not);
        }
        if self.eat_keyword("LIKE") {
            let rhs = self.additive_expr()?;
            // MySQL's `LIKE` uses backslash as the default escape character (so
            // `\%` matches a literal `%`), which is what `$wpdb->esc_like()`
            // relies on. The engine's `LIKE` has no default escape, so supply
            // `\` unless the query gives an explicit `ESCAPE` clause.
            let escape = if self.eat_keyword("ESCAPE") {
                Some(self.additive_expr()?)
            } else {
                Some(ast::Expr::Literal(ast::Literal::String(requote("\\"))))
            };
            return Ok(ast::Expr::like(
                lhs,
                not,
                ast::LikeOperator::Like,
                rhs,
                escape,
            ));
        }
        // `REGEXP` and its synonym `RLIKE` map onto the engine's `REGEXP`
        // operator (the `regexp` function, backed by the Rust regex crate).
        // MySQL's REGEXP is case-insensitive under the default collation, while
        // the engine's is case-sensitive, so prepend the regex crate's inline
        // `(?i)` flag to the pattern — `pattern` becomes `'(?i)' || pattern`.
        // `(?i)` at the start of a pattern applies case-insensitivity to the
        // whole expression (including character classes), and a NULL pattern
        // stays NULL through `||`.
        //
        // A `BINARY` subject (MySQL `CAST(x AS BINARY)`, which lowers to a BLOB
        // cast) forces a case-sensitive match. Detect it, unwrap the cast so the
        // match runs on the text value, and skip the `(?i)` flag. WordPress's
        // `WP_Meta_Query` uses this for case-sensitive `compare_key` REGEXPs.
        if self.eat_keyword("REGEXP") || self.eat_keyword("RLIKE") {
            let rhs = self.additive_expr()?;
            let (subject, case_insensitive) = match lhs {
                ast::Expr::Cast {
                    expr,
                    type_name: Some(ref t),
                } if t.name == "BLOB" => (*expr, false),
                other => (other, true),
            };
            let pattern = if case_insensitive {
                ast::Expr::binary(
                    ast::Expr::Literal(ast::Literal::String(requote("(?i)"))),
                    ast::Operator::Concat,
                    rhs,
                )
            } else {
                rhs
            };
            return Ok(ast::Expr::like(
                subject,
                not,
                ast::LikeOperator::Regexp,
                pattern,
                None,
            ));
        }
        if not {
            return Err(self.unexpected("`IN`, `BETWEEN`, `LIKE`, or `REGEXP` after `NOT`"));
        }

        // `a <=> b` — NULL-safe equality, at the comparison tier.
        if self.is(&Token::Spaceship) {
            self.advance();
            let rhs = self.bitor_expr()?;
            return Ok(null_safe_equals(lhs, rhs));
        }

        let op = match self.peek() {
            Some(Token::Eq) => ast::Operator::Equals,
            Some(Token::Ne) => ast::Operator::NotEquals,
            Some(Token::Lt) => ast::Operator::Less,
            Some(Token::Le) => ast::Operator::LessEquals,
            Some(Token::Gt) => ast::Operator::Greater,
            Some(Token::Ge) => ast::Operator::GreaterEquals,
            _ => return Ok(lhs),
        };
        self.advance();

        // `op {ANY | SOME | ALL} (subquery)` — a quantified comparison. Only the
        // two forms exactly equivalent to `IN` / `NOT IN` are modeled: `= ANY`
        // (and its synonym `= SOME`) is `IN (subquery)`, and `<> ALL` / `!= ALL`
        // is `NOT IN (subquery)`. The other operator/quantifier pairs need
        // MIN/MAX or EXISTS rewrites with subtle NULL and empty-set semantics, so
        // they are rejected rather than mistranslated. The quantifier is only
        // recognized immediately before `(`, so a column named `any` is not
        // misread.
        let quantifier_is_all = match (self.peek(), self.peek_nth(1)) {
            (Some(Token::Word(w)), Some(Token::LParen))
                if w.eq_ignore_ascii_case("ANY") || w.eq_ignore_ascii_case("SOME") =>
            {
                Some(false)
            }
            (Some(Token::Word(w)), Some(Token::LParen)) if w.eq_ignore_ascii_case("ALL") => {
                Some(true)
            }
            _ => None,
        };
        if let Some(all) = quantifier_is_all {
            self.advance(); // the quantifier keyword
            self.expect(&Token::LParen, "`(`")?;
            self.expect_keyword("SELECT")?;
            let rhs = self.parse_select()?;
            self.expect(&Token::RParen, "`)`")?;
            let not = match (op, all) {
                (ast::Operator::Equals, false) => false,  // `= ANY` / `= SOME` → IN
                (ast::Operator::NotEquals, true) => true, // `<> ALL` / `!= ALL` → NOT IN
                _ => {
                    return Err(ParseError::Unsupported(
                        "only the `= ANY` / `= SOME` (as IN) and `<> ALL` (as NOT IN) \
                         quantified comparisons are supported yet"
                            .to_string(),
                    ))
                }
            };
            return Ok(ast::Expr::InSelect {
                lhs: Box::new(lhs),
                not,
                rhs,
            });
        }

        let rhs = self.bitor_expr()?;
        Ok(ast::Expr::binary(lhs, op, rhs))
    }

    /// Bitwise-OR tier: `|`, left-associative. Binds looser than `&` and the
    /// arithmetic operators, but tighter than the comparison operators — matching
    /// MySQL.
    ///
    /// MySQL's bitwise operators work on unsigned 64-bit integers, whereas the
    /// engine's are signed, so a result with bit 63 set prints differently (e.g.
    /// MySQL's huge unsigned value vs a negative number). For the common case of
    /// small non-negative operands (flag masks) the results match. The unary `~`
    /// (which always sets high bits) and `^` (bitwise XOR) are not modeled.
    /// (See `mysql/COMPAT.md`.)
    fn bitor_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.bitand_expr()?;
        while matches!(self.peek(), Some(Token::Other('|'))) {
            self.advance();
            let rhs = self.bitand_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::BitwiseOr, rhs);
        }
        Ok(lhs)
    }

    /// Bitwise-AND tier: `&`, left-associative. Binds tighter than `|` and looser
    /// than the shift operators, as in MySQL.
    fn bitand_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.shift_expr()?;
        while matches!(self.peek(), Some(Token::Other('&'))) {
            self.advance();
            let rhs = self.shift_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::BitwiseAnd, rhs);
        }
        Ok(lhs)
    }

    /// Shift tier: `<<` / `>>`, left-associative. Binds tighter than `&` and
    /// looser than `+`/`-`, as in MySQL. Like the other bitwise operators these
    /// act on unsigned 64-bit integers in MySQL but signed in the engine, so a
    /// result with bit 63 set (e.g. `1 << 63`) or a right shift of a negative
    /// value diverges; small non-negative shifts match. (See `mysql/COMPAT.md`.)
    fn shift_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.additive_expr()?;
        loop {
            let op = match self.peek() {
                Some(Token::ShiftLeft) => ast::Operator::LeftShift,
                Some(Token::ShiftRight) => ast::Operator::RightShift,
                _ => break,
            };
            self.advance();
            let rhs = self.additive_expr()?;
            lhs = ast::Expr::binary(lhs, op, rhs);
        }
        Ok(lhs)
    }

    /// `expr [NOT] IN (v1, v2, ...)` (a value list) or `expr [NOT] IN (SELECT ...)`
    /// (an uncorrelated subquery — evaluated identically on both engines).
    fn in_list(&mut self, lhs: ast::Expr, not: bool) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        if self.eat_keyword("SELECT") {
            let rhs = self.parse_select()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(ast::Expr::InSelect {
                lhs: Box::new(lhs),
                not,
                rhs,
            });
        }
        // MySQL accepts an empty IN list, which SQLite and the engine do not:
        // `x IN ()` is always 0 and `x NOT IN ()` always 1, for any `x`
        // (including NULL). Fold the empty form to that constant. The left-hand
        // side is dropped because the result never depends on it.
        if self.eat(&Token::RParen) {
            return Ok(ast::Expr::Literal(ast::Literal::Numeric(
                if not { "1" } else { "0" }.to_string(),
            )));
        }
        let mut rhs = Vec::new();
        loop {
            rhs.push(Box::new(self.expr()?));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::InList {
            lhs: Box::new(lhs),
            not,
            rhs,
        })
    }

    /// `expr [NOT] BETWEEN a AND b`. The bounds are additive expressions so the
    /// `AND` separator is not swallowed by the logical-AND layer.
    fn between(&mut self, lhs: ast::Expr, not: bool) -> Result<ast::Expr> {
        let start = self.additive_expr()?;
        self.expect_keyword("AND")?;
        let end = self.additive_expr()?;
        Ok(ast::Expr::Between {
            lhs: Box::new(lhs),
            not,
            start: Box::new(start),
            end: Box::new(end),
        })
    }

    /// Additive tier: `+` and `-`, left-associative.
    fn additive_expr(&mut self) -> Result<ast::Expr> {
        // Prefix interval: `INTERVAL n unit + date` is the mirror of the postfix
        // `date + INTERVAL n unit` (MySQL accepts the interval on either side of
        // `+`), lowered identically. `INTERVAL` followed by `(` is instead the
        // `INTERVAL(n, n1, ...)` function, which is left to the primary tier.
        let mut lhs = if self.is_keyword("INTERVAL")
            && !matches!(self.peek_nth(1), Some(Token::LParen))
        {
            self.advance(); // `INTERVAL`
            let spec = self.parse_interval_spec()?;
            // MySQL only allows `INTERVAL ... + expr`; the interval is not a
            // standalone value, and `INTERVAL ... - expr` is not a valid form.
            self.expect(&Token::Plus, "`+`")?;
            let target = self.multiplicative_expr()?;
            build_interval(target, &spec, false)?
        } else {
            self.multiplicative_expr()?
        };
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => ast::Operator::Add,
                Some(Token::Minus) => ast::Operator::Subtract,
                _ => break,
            };
            self.advance();
            // `date ± INTERVAL n unit` is date arithmetic, the operator form of
            // DATE_ADD/DATE_SUB; lower it to the same `datetime()` modifier.
            if self.is_keyword("INTERVAL") {
                self.advance();
                lhs = self.apply_interval(lhs, op == ast::Operator::Subtract)?;
                continue;
            }
            let rhs = self.multiplicative_expr()?;
            lhs = ast::Expr::binary(lhs, op, rhs);
        }
        Ok(lhs)
    }

    /// Multiplicative tier: `*`, the float division `/`, the modulo operators `%`
    /// and `MOD`, and the integer-division keyword `DIV`. The symbolic operators
    /// are lowered to match MySQL's semantics, which differ from the engine's for
    /// integer operands:
    ///   - `a / b` → `CAST(a AS REAL) / b`: float division (`5 / 2` is `2.5`, not
    ///     `2`), since MySQL's `/` always returns a fractional result (see
    ///     [`float_division`]).
    ///   - `a DIV b` → `CAST(a / b AS INTEGER)`: the quotient truncated toward
    ///     zero, regardless of whether the engine divides as integer or float.
    ///   - `a MOD b` / `a % b` → `a - b * CAST(a / b AS INTEGER)`: the remainder,
    ///     which takes the sign of `a` and is exact for float operands too (where
    ///     the engine's own `%` would wrongly truncate them to integers, e.g.
    ///     `5.5 % 2` is `1.5`, not `1`). `%` and `MOD` are synonyms in MySQL.
    fn multiplicative_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.bitxor_expr()?;
        loop {
            if self.is(&Token::Star) {
                self.advance();
                let rhs = self.bitxor_expr()?;
                lhs = ast::Expr::binary(lhs, ast::Operator::Multiply, rhs);
            } else if self.eat(&Token::Other('/')) {
                let rhs = self.bitxor_expr()?;
                lhs = float_division(lhs, rhs);
            } else if self.eat_keyword("DIV") {
                let rhs = self.bitxor_expr()?;
                lhs = integer_division(lhs, rhs);
            } else if self.eat_keyword("MOD") || self.eat(&Token::Other('%')) {
                let rhs = self.bitxor_expr()?;
                lhs = modulo(lhs, rhs);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// Bitwise-XOR tier: `^`, left-associative. In MySQL's precedence `^` binds
    /// tighter than `*`/`/` and looser than the unary `-`/`~` prefixes, so it
    /// sits between [`Self::multiplicative_expr`] and [`Self::collate_expr`]
    /// (`-a ^ b` is `(-a) ^ b`, and `a * b ^ c` is `a * (b ^ c)`). The engine has
    /// no `^` operator, so it lowers via [`bitwise_xor`].
    fn bitxor_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.collate_expr()?;
        while self.eat(&Token::Other('^')) {
            let rhs = self.collate_expr()?;
            lhs = bitwise_xor(lhs, rhs);
        }
        Ok(lhs)
    }

    /// A primary expression optionally wrapped by the `BINARY` prefix operator
    /// and/or followed by a `COLLATE collation_name` postfix.
    ///
    /// MySQL's `COLLATE` overrides the collation used for comparison and sorting,
    /// and the `BINARY expr` prefix forces a binary (case-sensitive) comparison.
    /// Character columns default to the engine's case-insensitive `NOCASE`
    /// collation (matching MySQL's `utf8mb4_general_ci`; see `column_def`), so
    /// these override it: `BINARY expr` becomes `expr COLLATE BINARY`, and a
    /// `COLLATE <name>` postfix maps to `BINARY` for a `_bin`/`_cs` collation or
    /// `NOCASE` otherwise. Both bind tighter than the arithmetic operators, so
    /// they are applied here at the primary tier.
    fn collate_expr(&mut self) -> Result<ast::Expr> {
        // `!expr` — logical NOT, MySQL's high-precedence prefix form (distinct
        // from the low-precedence `NOT` keyword). It maps to the same engine
        // `NOT`, whose truthiness matches MySQL (`!0` = 1, `!5` = 0, `!NULL` is
        // NULL); binding it here makes it tighter than the comparison operators,
        // as in MySQL (`!a = b` is `(!a) = b`).
        if matches!(self.peek(), Some(Token::Other('!'))) {
            self.advance();
            let inner = self.collate_expr()?;
            return Ok(ast::Expr::unary(ast::UnaryOperator::Not, inner));
        }
        // `~expr` — bitwise NOT (unary), high precedence like the other unary
        // prefixes. It maps to the engine's `~`, whose result is bit-for-bit
        // identical to MySQL's; the engine prints it as a signed integer where
        // MySQL prints the unsigned 64-bit value (the same divergence as the
        // other bitwise operators — see `mysql/COMPAT.md`), but masked/combined
        // results agree.
        if matches!(self.peek(), Some(Token::Other('~'))) {
            self.advance();
            let inner = self.collate_expr()?;
            return Ok(ast::Expr::unary(ast::UnaryOperator::BitwiseNot, inner));
        }
        // `BINARY expr` — force a case-sensitive comparison with `COLLATE BINARY`
        // (character columns are `NOCASE` by default, so the operator can no
        // longer be a no-op).
        if self.is_keyword("BINARY") {
            self.advance();
            let inner = self.collate_expr()?;
            return Ok(ast::Expr::collate(inner, ast::Name::from_string("BINARY")));
        }
        // Unary minus / plus on an expression (`-a`, `-ABS(x)`, `-(a + 1)`, `+a`).
        // A signed *numeric literal* (`-5`) is folded into the literal by
        // `primary_expr`, so only a non-numeric operand is negated here — at this
        // tight precedence (tighter than `*`/`+`), so `-a * b` is `(-a) * b`.
        if matches!(self.peek(), Some(Token::Minus) | Some(Token::Plus))
            && !matches!(self.peek_nth(1), Some(Token::Num(_)))
        {
            let negative = self.is(&Token::Minus);
            self.advance();
            let inner = self.collate_expr()?;
            let op = if negative {
                ast::UnaryOperator::Negative
            } else {
                ast::UnaryOperator::Positive
            };
            return Ok(ast::Expr::unary(op, inner));
        }
        let mut expr = self.primary_expr()?;
        loop {
            // `doc -> path` / `doc ->> path` — MySQL's JSON extract operators,
            // mapping straight onto the engine's `->` (returns the JSON value,
            // keeping its quoting) and `->>` (returns the unquoted scalar). They
            // bind tightly here (like `COLLATE`), so `doc ->> '$.a' = 'x'` is
            // `(doc ->> '$.a') = 'x'`, and they chain left-to-right. The path
            // operand is a primary expression (a quoted path literal).
            if self.eat(&Token::Arrow) {
                let path = self.primary_expr()?;
                expr = ast::Expr::binary(expr, ast::Operator::ArrowRight, path);
            } else if self.eat(&Token::ArrowDouble) {
                let path = self.primary_expr()?;
                expr = ast::Expr::binary(expr, ast::Operator::ArrowRightShift, path);
            } else if self.eat_keyword("COLLATE") {
                // Map the MySQL collation (e.g. `utf8mb4_general_ci`) onto the
                // engine collation that compares the same way: `BINARY` for a
                // case-sensitive `_bin`/`_cs` collation, else `NOCASE`.
                let name = self.name()?;
                let engine = if is_case_sensitive_collation(name.as_str()) {
                    "BINARY"
                } else {
                    "NOCASE"
                };
                expr = ast::Expr::collate(expr, ast::Name::from_string(engine));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    /// Allocates the next positional parameter and returns the AST node for it.
    /// MySQL `?` placeholders are unnamed and numbered by appearance, so the
    /// engine sees `Variable { index: 1.. }` with no name.
    fn next_param(&mut self) -> ast::Variable {
        self.params += 1;
        ast::Variable {
            index: NonZeroU32::new(self.params).expect("parameter index is >= 1"),
            name: None,
            col_type: None,
        }
    }

    fn primary_expr(&mut self) -> Result<ast::Expr> {
        match self.peek() {
            // A parenthesized group: either a scalar subquery `(SELECT ...)` —
            // usable as a value, possibly correlated — an ordinary parenthesized
            // expression `(expr)`, or a row-value tuple `(a, b, ...)` (used in
            // row comparisons like `(a, b) = (1, 2)` and `(a, b) IN (...)`). The
            // `Parenthesized` wrapper carries one or more expressions and is kept
            // so the rendered SQL preserves the original grouping.
            Some(Token::LParen) => {
                self.advance();
                if self.eat_keyword("SELECT") {
                    let select = self.parse_select()?;
                    self.expect(&Token::RParen, "`)`")?;
                    return Ok(ast::Expr::Subquery(select));
                }
                let mut exprs = vec![Box::new(self.expr()?)];
                while self.eat(&Token::Comma) {
                    exprs.push(Box::new(self.expr()?));
                }
                self.expect(&Token::RParen, "`)`")?;
                Ok(ast::Expr::Parenthesized(exprs))
            }
            Some(Token::Num(n)) => {
                let n = n.clone();
                self.advance();
                Ok(ast::Expr::Literal(ast::Literal::Numeric(n)))
            }
            Some(Token::Str(s)) => {
                let lit = requote(s);
                self.advance();
                Ok(ast::Expr::Literal(ast::Literal::String(lit)))
            }
            // A MySQL hex literal (`0x41` / `X'41'`) is a binary string; lower it
            // to the engine's blob literal, which holds the same hex digits.
            Some(Token::Blob(b)) => {
                let b = b.clone();
                self.advance();
                Ok(ast::Expr::Literal(ast::Literal::Blob(b)))
            }
            // A `?` positional placeholder, bound at execution time. Each one
            // takes the next 1-based parameter index in appearance order.
            Some(Token::Param) => {
                self.advance();
                Ok(ast::Expr::Variable(self.next_param()))
            }
            // A signed numeric literal; the sign is folded into the literal.
            Some(Token::Minus) | Some(Token::Plus) => {
                let negative = self.is(&Token::Minus);
                self.advance();
                let Some(Token::Num(n)) = self.peek() else {
                    return Err(self.unexpected("a number"));
                };
                let n = if negative { format!("-{n}") } else { n.clone() };
                self.advance();
                Ok(ast::Expr::Literal(ast::Literal::Numeric(n)))
            }
            Some(Token::Word(w)) => {
                match w.to_ascii_uppercase().as_str() {
                    "NULL" => {
                        self.advance();
                        Ok(ast::Expr::Literal(ast::Literal::Null))
                    }
                    "TRUE" => {
                        self.advance();
                        Ok(ast::Expr::Literal(ast::Literal::True))
                    }
                    "FALSE" => {
                        self.advance();
                        Ok(ast::Expr::Literal(ast::Literal::False))
                    }
                    "CASE" => {
                        self.advance();
                        self.case_expr()
                    }
                    "EXISTS" if self.peek_nth(1) == Some(&Token::LParen) => {
                        self.advance();
                        self.exists_expr()
                    }
                    // `CAST(expr AS type)` is real cast syntax, not a function
                    // call, so it is parsed separately from the function path.
                    "CAST" if self.peek_nth(1) == Some(&Token::LParen) => {
                        self.advance();
                        self.cast_expr()
                    }
                    // `CONVERT(...)` has its own grammar (a `USING` charset
                    // clause or a `, type` cast), so it too is parsed here.
                    "CONVERT" if self.peek_nth(1) == Some(&Token::LParen) => {
                        self.advance();
                        self.convert_expr()
                    }
                    // MySQL typed temporal literals `DATE 'str'`, `TIME 'str'`,
                    // and `TIMESTAMP 'str'` (keyword directly before a quoted
                    // string — the form with `(` is the date/time function on the
                    // function path). Lower to the engine's `date`/`time`/
                    // `datetime` of the string, which normalizes it as MySQL does
                    // (`TIMESTAMP '2026-03-01'` → `2026-03-01 00:00:00`). MySQL has
                    // no `DATETIME 'str'` literal, so it is not included.
                    "DATE" if matches!(self.peek_nth(1), Some(Token::Str(_))) => {
                        self.temporal_literal("date")
                    }
                    "TIME" if matches!(self.peek_nth(1), Some(Token::Str(_))) => {
                        self.temporal_literal("time")
                    }
                    "TIMESTAMP" if matches!(self.peek_nth(1), Some(Token::Str(_))) => {
                        self.temporal_literal("datetime")
                    }
                    // The SQL-standard niladic date/time keywords are valid
                    // *without* parentheses: `CURRENT_TIMESTAMP`, `CURRENT_DATE`,
                    // `CURRENT_TIME`, `LOCALTIME`, `LOCALTIMESTAMP`, and the
                    // `UTC_*` forms. (Their parenthesized forms, and `NOW()` /
                    // `CURDATE()` etc. which require parentheses, go through
                    // `function_call`.) Lower the bare form to the same engine
                    // call so it is not mistaken for a column reference.
                    kw @ ("CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIME"
                    | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" | "UTC_DATE" | "UTC_TIME")
                        if self.peek_nth(1) != Some(&Token::LParen) =>
                    {
                        let engine_fn =
                            current_time_function(kw).expect("niladic date/time keyword");
                        self.advance();
                        Ok(call_fn(
                            engine_fn,
                            vec![ast::Expr::Literal(ast::Literal::String(requote("now")))],
                        ))
                    }
                    // `CURRENT_USER` is likewise a SQL-standard niladic keyword,
                    // valid without parentheses; it folds to the same literal as
                    // `CURRENT_USER()`. (`USER`/`SESSION_USER`/`SYSTEM_USER` require
                    // parentheses in MySQL, so they stay function-only.)
                    "CURRENT_USER" if self.peek_nth(1) != Some(&Token::LParen) => {
                        self.advance();
                        Ok(introspection_literal("CURRENT_USER")
                            .expect("CURRENT_USER is an introspection keyword"))
                    }
                    // A bare identifier followed by `(` is a function call;
                    // otherwise it is a column reference.
                    _ if self.peek_nth(1) == Some(&Token::LParen) => self.function_call(),
                    _ => self.column_ref(),
                }
            }
            Some(Token::QuotedIdent(_)) => self.column_ref(),
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// Parses `CAST(expr AS type)`. The engine's `CAST` follows SQLite affinity
    /// rules, so MySQL's cast target types are mapped to a type name with the
    /// matching affinity (see [`Self::cast_type`]). `CAST` has already been
    /// consumed.
    /// Parses a MySQL typed temporal literal (`DATE`/`TIME`/`TIMESTAMP` keyword
    /// already at the cursor, immediately followed by a quoted string) into
    /// `<engine_fn>('str')` — `date`/`time`/`datetime` of the string.
    fn temporal_literal(&mut self, engine_fn: &'static str) -> Result<ast::Expr> {
        self.advance(); // the DATE / TIME / TIMESTAMP keyword
        let s = match self.peek() {
            Some(Token::Str(s)) => s.clone(),
            _ => return Err(self.unexpected("a quoted temporal literal")),
        };
        self.advance(); // the string literal
        Ok(call_fn(
            engine_fn,
            vec![ast::Expr::Literal(ast::Literal::String(requote(&s)))],
        ))
    }

    fn cast_expr(&mut self) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        let expr = self.expr()?;
        self.expect_keyword("AS")?;

        // `DATE`/`DATETIME`/`TIME` have no SQLite type affinity, so a plain
        // `CAST` would not parse or reformat the value. Lower them to the
        // engine's `date()`/`datetime()`/`time()` functions, which render the
        // 'YYYY-MM-DD' / 'YYYY-MM-DD HH:MM:SS' / 'HH:MM:SS' forms MySQL returns.
        let date_func = match self.peek() {
            Some(Token::Word(w)) => match w.to_ascii_uppercase().as_str() {
                "DATE" => Some("date"),
                "DATETIME" => Some("datetime"),
                "TIME" => Some("time"),
                _ => None,
            },
            _ => None,
        };
        if let Some(func) = date_func {
            self.advance();
            // An optional fractional-seconds precision (`DATETIME(6)`) is dropped.
            if self.is(&Token::LParen) {
                let _ = self.type_size()?;
            }
            self.expect(&Token::RParen, "`)`")?;
            return Ok(unary_fn(func, expr));
        }

        let type_name = self.cast_type()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(build_cast(expr, type_name))
    }

    /// Parses `CONVERT(...)`, which has two MySQL forms:
    /// `CONVERT(expr USING charset)` coerces a string's charset — the engine is
    /// single-charset (UTF-8), so the charset is dropped and the value passes
    /// through unchanged — and `CONVERT(expr, type)` is identical to
    /// `CAST(expr AS type)`. `CONVERT` has already been consumed.
    fn convert_expr(&mut self) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        let expr = self.expr()?;
        if self.eat_keyword("USING") {
            // Charset name (an identifier such as `utf8mb4`); consumed and dropped.
            self.name()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(expr);
        }
        self.expect(&Token::Comma, "`,` or `USING`")?;
        let type_name = self.cast_type()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(build_cast(expr, type_name))
    }

    /// Parses and maps a MySQL `CAST` target type onto an engine type whose
    /// SQLite affinity matches the intended conversion: `CHAR`→text,
    /// `SIGNED`/`UNSIGNED`→integer, `DECIMAL`→numeric, `DOUBLE`/`FLOAT`/`REAL`→
    /// real, `BINARY`→blob. Date/time and JSON targets diverge from the engine
    /// and are rejected. A length/precision is accepted but dropped (the engine
    /// does not enforce it); rounding of fractional values to an integer also
    /// differs from MySQL — see `mysql/COMPAT.md`.
    fn cast_type(&mut self) -> Result<ast::Type> {
        let Some(Token::Word(w)) = self.peek() else {
            return Err(self.unexpected("a CAST target type"));
        };
        let kw = w.to_ascii_uppercase();
        self.advance();
        // A trailing length/precision (`CHAR(8)`, `DECIMAL(10,2)`) parses but is
        // not carried onto the cast.
        if self.is(&Token::LParen) {
            let _ = self.type_size()?;
        }
        let name = match kw.as_str() {
            "CHAR" | "NCHAR" | "CHARACTER" => "CHAR",
            "SIGNED" | "UNSIGNED" => {
                self.eat_keyword("INTEGER");
                "INTEGER"
            }
            "DECIMAL" | "DEC" | "NUMERIC" | "FIXED" => "DECIMAL",
            "DOUBLE" | "FLOAT" | "REAL" => "REAL",
            "BINARY" => "BLOB",
            other => {
                return Err(ParseError::Unsupported(format!(
                    "CAST to {other} is not supported yet"
                )))
            }
        };
        Ok(ast::Type {
            name: name.to_string(),
            size: None,
            array_dimensions: 0,
        })
    }

    /// Parses a `CASE` expression — both the searched form
    /// (`CASE WHEN cond THEN result ... [ELSE result] END`) and the simple form
    /// (`CASE operand WHEN value THEN result ... [ELSE result] END`). Standard
    /// SQL, evaluated identically by the engine. `CASE` has already been consumed.
    fn case_expr(&mut self) -> Result<ast::Expr> {
        // A simple `CASE operand WHEN ...` has an operand before the first WHEN.
        let base = if self.is_keyword("WHEN") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };

        let mut when_then_pairs = Vec::new();
        while self.eat_keyword("WHEN") {
            let when = self.expr()?;
            self.expect_keyword("THEN")?;
            let then = self.expr()?;
            when_then_pairs.push((Box::new(when), Box::new(then)));
        }
        if when_then_pairs.is_empty() {
            return Err(self.unexpected("`WHEN ... THEN ...`"));
        }

        let else_expr = if self.eat_keyword("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_keyword("END")?;

        Ok(ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        })
    }

    /// Parses an `EXISTS (SELECT ...)` predicate. `NOT EXISTS` is handled by the
    /// prefix-`NOT` layer wrapping this. `EXISTS` has already been consumed.
    fn exists_expr(&mut self) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        self.expect_keyword("SELECT")?;
        let select = self.parse_select()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::Exists(select))
    }

    /// Parses a function call `name(arg, ...)`. The name must be in the clean
    /// allow-list (functions whose MySQL semantics match SQLite/turso exactly);
    /// any other function is rejected as unsupported.
    fn function_call(&mut self) -> Result<ast::Expr> {
        let name = self.name()?;
        let upper = name.as_str().to_ascii_uppercase();
        self.expect(&Token::LParen, "`(`")?;

        // Inside an `ON DUPLICATE KEY UPDATE` assignment, `VALUES(col)` is the
        // would-be-inserted value; it lowers to the engine's `excluded.col`. This
        // is recognized anywhere in the assignment expression (e.g. `c =
        // c + VALUES(c)`), not only as the bare right-hand side.
        if upper == "VALUES" && self.in_upsert_assignment {
            let col = self.name()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(ast::Expr::Qualified(
                ast::Name::from_string("excluded"),
                col,
            ));
        }

        // `CONCAT(a, b, ...)` lowers to the engine's `||` concatenation, which —
        // like MySQL's CONCAT — yields NULL if any argument is NULL. (The
        // engine's own `concat()` skips NULLs instead, so it is not used here.)
        if upper == "CONCAT" {
            return self.concat_call();
        }

        // `TRUNCATE(x, d)` truncates `x` to `d` decimal places toward zero. The
        // engine's `trunc` only truncates to an integer, so scale by `10^d` first
        // (see `truncate_call`).
        if upper == "TRUNCATE" {
            return self.truncate_call();
        }

        // `STRCMP(a, b)` compares two strings, returning -1 / 0 / 1 (see
        // `strcmp_call`).
        if upper == "STRCMP" {
            return self.strcmp_call();
        }

        // `MD5(s)` / `SHA1(s)` (and its `SHA` alias) / `SHA2(s, n)` hash a string
        // and return its lowercase hex digest, mapped onto the crypto extension
        // (see `crypto_hash_call` / `sha2_call`). WordPress hashes heavily (cache
        // and transient keys, `$wpdb` placeholders).
        if upper == "MD5" {
            return self.crypto_hash_call("crypto_md5");
        }
        if upper == "SHA1" || upper == "SHA" {
            return self.crypto_hash_call("crypto_sha1");
        }
        if upper == "SHA2" {
            return self.sha2_call();
        }

        // `UUID()` generates a UUID string, mapped to the engine's `uuid4_str`.
        // MySQL returns a (time-based) version-1 UUID and the engine a random
        // version-4 one; both are 36-character hyphenated UUIDs, and the value is
        // non-deterministic either way. Takes no arguments.
        if upper == "UUID" {
            self.expect(&Token::RParen, "`)`")?;
            return Ok(call_fn("uuid4_str", Vec::new()));
        }

        // `TO_BASE64(s)` / `FROM_BASE64(s)` base64-encode / -decode a string via
        // the crypto extension (see `base64_call`).
        if upper == "TO_BASE64" {
            return self.base64_call("crypto_encode", true);
        }
        if upper == "FROM_BASE64" {
            return self.base64_call("crypto_decode", false);
        }

        // `ANY_VALUE(x)` is just `x`: in MySQL it marks a non-aggregated column as
        // intentionally unconstrained, suppressing the `ONLY_FULL_GROUP_BY`
        // error. The engine already allows a bare column in a `GROUP BY` query
        // (returning a value from some row of each group, as ANY_VALUE does), so
        // the wrapper is dropped.
        if upper == "ANY_VALUE" {
            let arg = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(arg);
        }

        // `CEIL`/`CEILING`/`FLOOR` and the single-argument `ROUND(x)` produce a
        // whole number, which MySQL types as an integer — but the engine's
        // `ceil`/`floor`/`round` return a real, printing `6.0` where MySQL prints
        // `6`. Wrap the result in `CAST(... AS INTEGER)` to match. (A magnitude
        // above 2^63 saturates the cast — a documented edge; MySQL keeps such a
        // value as a double.) The two-argument `ROUND(x, d)` keeps `d` decimal
        // places and stays a real.
        if upper == "CEIL" || upper == "CEILING" {
            let arg = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(cast_to_integer(call_fn("ceil", vec![arg])));
        }
        if upper == "FLOOR" {
            let arg = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(cast_to_integer(call_fn("floor", vec![arg])));
        }
        if upper == "ROUND" {
            let arg = self.expr()?;
            if self.eat(&Token::Comma) {
                let digits = self.expr()?;
                self.expect(&Token::RParen, "`)`")?;
                // A negative literal `d` rounds to the left of the decimal point
                // (`ROUND(1234.5, -2)` → `1200`), which the engine's `round` does
                // not do: scale down by `10^|d|`, round, and scale back, then cast
                // to an integer (the MySQL result has no fractional part).
                if let Some(magnitude) = negative_integer_literal(&digits) {
                    let scale = || {
                        call_fn(
                            "pow",
                            vec![*numeric_expr("10"), *numeric_expr(&magnitude.to_string())],
                        )
                    };
                    let scaled_down = float_division(arg, scale());
                    let rounded = call_fn("round", vec![scaled_down]);
                    let scaled_up = ast::Expr::binary(rounded, ast::Operator::Multiply, scale());
                    return Ok(cast_to_integer(scaled_up));
                }
                // `ROUND(x, 0)` is an integer like `ROUND(x)`; `ROUND(x, d)` with
                // `d > 0` keeps `d` decimals and stays a real.
                let round = call_fn("round", vec![arg, digits.clone()]);
                if matches!(&digits, ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0") {
                    return Ok(cast_to_integer(round));
                }
                return Ok(round);
            }
            self.expect(&Token::RParen, "`)`")?;
            return Ok(cast_to_integer(call_fn("round", vec![arg])));
        }

        // `HEX(x)` is overloaded: the uppercase hex of a number, or the hex of a
        // string's bytes (see `hex_call`).
        if upper == "HEX" {
            return self.hex_call();
        }

        // `OCT(n)` is the octal string of `n` (see `oct_call`).
        if upper == "OCT" {
            return self.oct_call();
        }

        // `INTERVAL(n, n1, n2, ...)` returns how many of the (ascending) bounds
        // `n` reaches or exceeds (see `interval_call`).
        if upper == "INTERVAL" {
            return self.interval_call();
        }

        // `LOG(x)` is the natural log in MySQL (the engine's 1-arg `log` is
        // base-10), while `LOG(b, x)` is the base-`b` log on both (see `log_call`).
        if upper == "LOG" {
            return self.log_call();
        }

        // `ATAN(x)` is the arctangent; MySQL also accepts `ATAN(y, x)` as a synonym
        // for `ATAN2(y, x)`, which the engine spells `atan2` (see `atan_call`).
        if upper == "ATAN" {
            return self.atan_call();
        }

        // `COT(x)` (cotangent) has no engine builtin; lower it to `1 / tan(x)`
        // (see `cot_call`).
        if upper == "COT" {
            return self.cot_call();
        }

        // `CHAR(n, ...)` builds a string from character codes, mapping to the
        // engine's `char()` (see `char_call`).
        if upper == "CHAR" {
            return self.char_call();
        }

        // `QUOTE(str)` produces a single-quoted, escaped SQL string literal (see
        // `quote_call`). The engine's own `quote` uses SQLite's escaping (doubled
        // quotes), so the MySQL form (backslash escapes) is synthesized.
        if upper == "QUOTE" {
            return self.quote_call();
        }

        // `UUID_TO_BIN(uuid[, swap])` / `BIN_TO_UUID(bin[, swap])` convert between
        // a dashed UUID string and its 16-byte form (see `uuid_bin_call`).
        if upper == "UUID_TO_BIN" {
            return self.uuid_bin_call(true);
        }
        if upper == "BIN_TO_UUID" {
            return self.uuid_bin_call(false);
        }

        // `ASCII(str)` / `ORD(str)` return the code point of the first character,
        // mapping to the engine's `unicode()` with MySQL's edge cases restored
        // (see `ascii_call`).
        if upper == "ASCII" || upper == "ORD" {
            return self.ascii_call();
        }

        // MySQL's `LENGTH(x)` is a BYTE count. The engine's `length()` counts
        // characters, but `length()` of a BLOB counts bytes, so lower it to
        // `length(CAST(x AS BLOB))`.
        if upper == "LENGTH" {
            return self.length_call();
        }

        // `OCTET_LENGTH` is a MySQL synonym for `LENGTH` (byte count); `BIT_LENGTH`
        // is that times eight. Both reuse the byte-length lowering.
        if upper == "OCTET_LENGTH" {
            return self.octet_length_call();
        }
        if upper == "BIT_LENGTH" {
            return self.bit_length_call();
        }

        // `TRIM([{BOTH|LEADING|TRAILING}] [remstr] FROM str)` (and the bare
        // `TRIM(str)`) lower to the engine's `trim`/`ltrim`/`rtrim`. The `FROM`
        // form needs dedicated parsing, so all TRIM spellings are handled here.
        if upper == "TRIM" {
            return self.trim_call();
        }

        // MySQL date-part extractors (`YEAR`, `MONTH`, `DAY`, ...) lower to the
        // engine's `strftime()`, cast to an integer to match MySQL's numeric
        // return (no zero-padding).
        if let Some(fmt) = date_part_format(&upper) {
            return self.date_part_call(fmt);
        }

        // `DAYOFWEEK` (1=Sunday..7=Saturday) and `WEEKDAY` (0=Monday..6=Sunday)
        // lower to integer arithmetic over `strftime('%w', d)`, which yields
        // 0=Sunday..6=Saturday: `%w + 1` and `(%w + 6) % 7` respectively.
        if upper == "DAYOFWEEK" {
            return self.day_of_week_call(1, false);
        }
        if upper == "WEEKDAY" {
            return self.day_of_week_call(6, true);
        }

        // `WEEK(d[, mode])` lowers to `CAST(strftime(fmt, d) AS INTEGER)` for the
        // three modes whose definition matches an engine strftime week format.
        if upper == "WEEK" {
            return self.week_call();
        }

        // `YEARWEEK(d[, mode])` combines the week-owning year and the week number
        // into `year * 100 + week` (see `yearweek_call`).
        if upper == "YEARWEEK" {
            return self.yearweek_call();
        }

        // `DAYNAME(d)` / `MONTHNAME(d)` map the weekday / month to its English
        // name via a CASE over `strftime`.
        if upper == "DAYNAME" {
            return self.dayname_call();
        }
        if upper == "MONTHNAME" {
            return self.monthname_call();
        }

        // `FIELD(x, a, b, ...)` (the 1-based index of `x` in the list, else 0)
        // lowers to a `CASE x WHEN a THEN 1 WHEN b THEN 2 ... ELSE 0 END`.
        if upper == "FIELD" {
            return self.field_call();
        }

        // `ELT(n, a, b, ...)` (the `n`-th string, else NULL) is the inverse of
        // FIELD and lowers to `CASE n WHEN 1 THEN a WHEN 2 THEN b ... END`.
        if upper == "ELT" {
            return self.elt_call();
        }

        // `MAKE_SET(bits, s1, s2, ...)` joins the strings whose corresponding bit
        // in `bits` is set, comma-separated (see `make_set_call`).
        if upper == "MAKE_SET" {
            return self.make_set_call();
        }

        // `GREATEST(...)` / `LEAST(...)` — the largest / smallest argument under a
        // case-insensitive comparison (see `greatest_least_call`).
        if upper == "GREATEST" {
            return self.greatest_least_call(true);
        }
        if upper == "LEAST" {
            return self.greatest_least_call(false);
        }

        // `INET_NTOA(n)` renders a 32-bit number as a dotted-quad IPv4 address
        // (see `inet_ntoa_call`).
        if upper == "INET_NTOA" {
            return self.inet_ntoa_call();
        }

        // `EXPORT_SET(bits, on, off[, sep[, n]])` writes `on`/`off` per bit of
        // `bits`, separated by `sep` (see `export_set_call`).
        if upper == "EXPORT_SET" {
            return self.export_set_call();
        }

        // `REGEXP_LIKE(str, pattern[, match_type])` is the functional form of the
        // `REGEXP` operator (see `regexp_like_call`).
        if upper == "REGEXP_LIKE" {
            return self.regexp_like_call();
        }

        // `BIT_COUNT(n)` counts the set bits of `n` (see `bit_count_call`).
        if upper == "BIT_COUNT" {
            return self.bit_count_call();
        }

        // `BIN(n)` is the binary string of `n` (see `bin_call`).
        if upper == "BIN" {
            return self.bin_call();
        }

        // `FIND_IN_SET(str, strlist)` — the 1-based index of `str` in the
        // comma-separated `strlist`, or 0; synthesized from comma-wrapped
        // string surgery.
        if upper == "FIND_IN_SET" {
            return self.find_in_set_call();
        }

        // `ISNULL(x)` returns 1 if `x` is NULL else 0; lower to the `x IS NULL`
        // predicate, which the engine evaluates to the same 1/0.
        if upper == "ISNULL" {
            return self.isnull_call();
        }

        // `NULLIF(x, y)` is NULL when `x` equals `y` (case-insensitively, like
        // MySQL's default collation), else `x` (see `nullif_call`).
        if upper == "NULLIF" {
            return self.nullif_call();
        }

        // `MOD(a, b)` is the function spelling of the `a MOD b` operator; MySQL
        // defines them identically, so lower it the same way (exact for floats,
        // unlike the engine's `%`).
        if upper == "MOD" {
            return self.mod_call();
        }

        // `REPEAT(s, n)` builds `n` copies of `s`; the engine has no `repeat()`,
        // so synthesize one from `zeroblob`/`hex`/`replace`.
        if upper == "REPEAT" {
            return self.repeat_call();
        }

        // `SPACE(n)` is `REPEAT(' ', n)` — a run of `n` spaces.
        if upper == "SPACE" {
            return self.space_call();
        }

        // `LPAD`/`RPAD` pad a string to a length with a fill string; synthesize
        // them from `REPEAT`, `substr`, and `||` (see `pad_expr`).
        if upper == "LPAD" {
            return self.pad_call(true);
        }
        if upper == "RPAD" {
            return self.pad_call(false);
        }

        // `INSERT(str, pos, len, newstr)` splices `newstr` into `str`, replacing
        // `len` characters from `pos`; synthesized from `substr` and `||`.
        if upper == "INSERT" {
            return self.insert_string_call();
        }

        // `INSTR(str, substr)` and `LOCATE(substr, str)` (note the swapped
        // operand order) find the 1-based position of a substring. MySQL's are
        // case-insensitive under the default collation, so both lower to
        // `instr(lower(str), lower(substr))`.
        if upper == "INSTR" {
            return self.instr_call(false);
        }
        if upper == "LOCATE" {
            return self.instr_call(true);
        }

        // `POSITION(substr IN str)` is the SQL-standard spelling of
        // `LOCATE(substr, str)` — same lowering, with the `IN` keyword separating
        // the operands.
        if upper == "POSITION" {
            return self.position_call();
        }

        // `SUBSTRING`/`SUBSTR` accept both the comma form `(str, pos[, len])` and
        // the SQL-standard `(str FROM pos [FOR len])`; both lower to a guarded
        // `substr`. `MID` is the comma-form synonym and shares the same lowering
        // (so its out-of-range edge cases match too).
        if upper == "SUBSTRING" || upper == "SUBSTR" || upper == "MID" {
            return self.substring_call();
        }

        // `SUBSTRING_INDEX(str, delim, count)` returns the part of `str` before
        // the count-th occurrence of `delim` (see `substring_index_call`).
        if upper == "SUBSTRING_INDEX" {
            return self.substring_index_call();
        }

        // `GROUP_CONCAT(expr [SEPARATOR 's'])` maps to the engine's
        // `group_concat(expr[, 's'])`, which uses the same default `,` separator.
        if upper == "GROUP_CONCAT" {
            return self.group_concat_call();
        }

        // `RAND()` lowers to a random float in `[0, 1)` built from the engine's
        // `random()`. A seed argument is accepted but not honored.
        if upper == "RAND" {
            return self.rand_call();
        }

        // `LEFT(str, len)` (the leftmost `len` characters) lowers to the engine's
        // `substr(str, 1, len)`.
        if upper == "LEFT" {
            return self.left_call();
        }

        // `RIGHT(str, len)` (the rightmost `len` characters) lowers to the
        // engine's `substr(str, -len, len)`.
        if upper == "RIGHT" {
            return self.right_call();
        }

        // `TIMEDIFF(a, b)` is `a - b` rendered as a MySQL `TIME` string
        // (`[-]HH:MM:SS`, hours unbounded). The engine's own `timediff` renders a
        // different (SQLite) format, so the MySQL spelling is synthesized.
        if upper == "TIMEDIFF" {
            return self.timediff_call();
        }

        // The MySQL advisory-lock functions. This is a single-node engine with no
        // cross-session lock table, so they fold to constants matching MySQL's
        // result when no lock is actually held — the uncontended flow WordPress
        // uses: `GET_LOCK` (acquired) and `RELEASE_LOCK` (released) and
        // `IS_FREE_LOCK` (free) are `1`, `IS_USED_LOCK` (no holder) is NULL, and
        // `RELEASE_ALL_LOCKS` (nothing to release) is `0`. The lock name and
        // timeout are parsed and discarded. The contended/held cases (where MySQL
        // would return 0, a connection id, or a non-zero count) are not modeled
        // (see `mysql/COMPAT.md`).
        if upper == "GET_LOCK" || upper == "RELEASE_LOCK" || upper == "IS_FREE_LOCK" {
            return self.noop_constant_call(*numeric_expr("1"));
        }
        if upper == "IS_USED_LOCK" {
            return self.noop_constant_call(ast::Expr::Literal(ast::Literal::Null));
        }
        if upper == "RELEASE_ALL_LOCKS" {
            return self.noop_constant_call(*numeric_expr("0"));
        }

        // `SLEEP(seconds)` pauses, and `BENCHMARK(count, expr)` evaluates `expr`
        // `count` times for timing; both return `0` in MySQL. The engine models
        // neither the delay nor the repeated evaluation, so they fold to `0` —
        // which also keeps a time-based probe (`... OR SLEEP(10)`) from stalling
        // the server.
        if upper == "SLEEP" || upper == "BENCHMARK" {
            return self.noop_constant_call(*numeric_expr("0"));
        }

        // `DATE_ADD` / `DATE_SUB(x, INTERVAL n unit)` lower to the engine's
        // `datetime(x, '+n unit')` / `datetime(x, '-n unit')` modifier.
        if upper == "DATE_ADD" {
            return self.date_add_call(false);
        }
        if upper == "DATE_SUB" {
            return self.date_add_call(true);
        }

        // `ADDDATE`/`SUBDATE` are `DATE_ADD`/`DATE_SUB` for the INTERVAL form, and
        // also take an integer number of days directly.
        if upper == "ADDDATE" {
            return self.adddate_call(false);
        }
        if upper == "SUBDATE" {
            return self.adddate_call(true);
        }

        // `DATE_FORMAT(x, fmt)` lowers to the engine's `strftime()` with the
        // format specifiers translated from MySQL to strftime spelling.
        // `TIME_FORMAT(x, fmt)` shares the lowering — for a time-only format it
        // matches MySQL, since those specifiers read just the time part.
        if upper == "DATE_FORMAT" {
            return self.format_call("DATE_FORMAT");
        }
        if upper == "TIME_FORMAT" {
            return self.format_call("TIME_FORMAT");
        }

        // `DATEDIFF(a, b)` is the whole-day difference `a - b`, ignoring the time
        // parts, which is `CAST(julianday(date(a)) - julianday(date(b)) AS INTEGER)`.
        if upper == "DATEDIFF" {
            return self.datediff_call();
        }

        // `TO_DAYS(d)` is the day number since year 0; `FROM_DAYS(n)` is its
        // inverse. Both are offsets of the engine's Julian day.
        if upper == "TO_DAYS" {
            return self.to_days_call();
        }
        if upper == "FROM_DAYS" {
            return self.from_days_call();
        }

        // `TO_SECONDS(d)` is the seconds since year 0 — `TO_DAYS(d) * 86400`
        // plus the time-of-day seconds.
        if upper == "TO_SECONDS" {
            return self.to_seconds_call();
        }

        // `PERIOD_DIFF(p1, p2)` is the month count between two `YYYYMM`/`YYMM`
        // periods; `PERIOD_ADD(p, n)` shifts a period by `n` months. Both are
        // integer arithmetic on the period format (see their call methods).
        if upper == "PERIOD_DIFF" {
            return self.period_diff_call();
        }
        if upper == "PERIOD_ADD" {
            return self.period_add_call();
        }

        // `TIMESTAMPDIFF(unit, a, b)` is `b - a` in whole `unit`s. The
        // fixed-duration units lower to integer division of the epoch-second
        // difference.
        if upper == "TIMESTAMPDIFF" {
            return self.timestampdiff_call();
        }

        // `TIMESTAMPADD(unit, n, datetime)` shifts the datetime by `n` units,
        // like `DATE_ADD(datetime, INTERVAL n unit)`.
        if upper == "TIMESTAMPADD" {
            return self.timestampadd_call();
        }

        // `ADDTIME(expr, t)` / `SUBTIME(expr, t)` add/subtract a time of day to a
        // datetime or time (see `time_add_call`).
        if upper == "ADDTIME" {
            return self.time_add_call(false);
        }
        if upper == "SUBTIME" {
            return self.time_add_call(true);
        }

        // `TIME_TO_SEC(t)` is the seconds since midnight of the time part;
        // `SEC_TO_TIME(s)` is the inverse.
        if upper == "TIME_TO_SEC" {
            return self.time_to_sec_call();
        }
        if upper == "SEC_TO_TIME" {
            return self.sec_to_time_call();
        }

        // `LAST_DAY(d)` is the last day of `d`'s month, which the engine's date
        // modifiers compute as `date(d, 'start of month', '+1 month', '-1 day')`.
        if upper == "LAST_DAY" {
            return self.last_day_call();
        }

        // `CONVERT_TZ(dt, from_tz, to_tz)` shifts a datetime between numeric
        // UTC offsets (see `convert_tz_call`).
        if upper == "CONVERT_TZ" {
            return self.convert_tz_call();
        }

        // `MAKEDATE(year, dayofyear)` builds a date from a year and a 1-based day
        // of year.
        if upper == "MAKEDATE" {
            return self.makedate_call();
        }

        // `MAKETIME(hour, minute, second)` builds a time string from components.
        if upper == "MAKETIME" {
            return self.maketime_call();
        }

        // `EXTRACT(unit FROM d)` is the SQL-standard date-part extractor; the
        // single calendar units share the date-part `strftime` lowering.
        if upper == "EXTRACT" {
            return self.extract_call();
        }

        // `QUARTER(d)` (1–4) is `(MONTH(d) + 2) / 3` with integer division.
        if upper == "QUARTER" {
            return self.quarter_call();
        }

        // `WEEKOFYEAR(d)` is the ISO-8601 week (a synonym for `WEEK(d, 3)`),
        // which the engine computes as `strftime('%V', d)`.
        if upper == "WEEKOFYEAR" {
            let arg = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(cast_strftime_int("%V", arg));
        }

        // Current date/time functions (`NOW()`, `CURDATE()`, ...) lower to the
        // engine's `datetime('now')` / `date('now')` / `time('now')`.
        if let Some(engine_fn) = current_time_function(&upper) {
            return self.current_time_call(engine_fn);
        }

        // `UNIX_TIMESTAMP([d])` lowers to `unixepoch(d)` (or `unixepoch('now')`),
        // and `FROM_UNIXTIME(n)` to `datetime(n, 'unixepoch')`.
        if upper == "UNIX_TIMESTAMP" {
            return self.unix_timestamp_call();
        }
        if upper == "FROM_UNIXTIME" {
            return self.from_unixtime_call();
        }

        // Server/connection introspection functions (`VERSION()`, `DATABASE()`,
        // ...) fold to the same canned literal the server reports for the
        // standalone forms, so they also work inside larger expressions. They
        // take no arguments.
        if let Some(literal) = introspection_literal(&upper) {
            self.expect(&Token::RParen, "`)`")?;
            return Ok(literal);
        }

        if !is_supported_function(&upper) {
            return Err(ParseError::Unsupported(format!(
                "function {upper} is not supported yet"
            )));
        }

        // `DISTINCT` / `ALL` quantifiers are only valid for aggregates and behave
        // identically on both engines (`ALL` is the default and ignored).
        let distinct = self.eat_keyword("DISTINCT");
        if !distinct {
            self.eat_keyword("ALL");
        }
        if distinct && !is_aggregate_function(&upper) {
            return Err(ParseError::Unsupported(format!(
                "DISTINCT is not valid in {upper}()"
            )));
        }

        // `COUNT(*)` is the only star form (and `COUNT(DISTINCT *)` is invalid).
        if self.is(&Token::Star) {
            if upper != "COUNT" || distinct {
                return Err(ParseError::Unsupported(format!(
                    "{upper}(*) is not supported"
                )));
            }
            self.advance();
            self.expect(&Token::RParen, "`)`")?;
            let over_clause = self.parse_over_clause()?;
            return Ok(ast::Expr::FunctionCallStar {
                name,
                filter_over: ast::FunctionTail {
                    filter_clause: None,
                    over_clause,
                },
            });
        }

        let mut args = Vec::new();
        if !self.is(&Token::RParen) {
            loop {
                args.push(Box::new(self.expr()?));
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, "`)`")?;

        // An aggregate may carry an `OVER (...)` window spec, turning it into a
        // windowed aggregate (e.g. a running total); a dedicated window function
        // like `ROW_NUMBER()` always carries one. The engine evaluates both.
        // Plain scalar functions take no window.
        let over_clause = if is_aggregate_function(&upper) || is_window_function(&upper) {
            self.parse_over_clause()?
        } else {
            None
        };

        // Some MySQL functions differ from the engine only in name; rename them.
        let name = match engine_function_name(&upper) {
            Some(engine) => ast::Name::from_string(engine),
            None => name,
        };

        Ok(ast::Expr::FunctionCall {
            name,
            distinctness: distinct.then_some(ast::Distinctness::Distinct),
            args,
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause,
            },
        })
    }

    /// Parses an optional `OVER ( [PARTITION BY ...] [ORDER BY ...] )` window
    /// specification following an aggregate. The engine evaluates windowed
    /// aggregates, so `SUM(x) OVER ()` (whole-partition total), `... OVER
    /// (PARTITION BY g)`, and `... OVER (ORDER BY y)` (a running total under the
    /// default frame, as in MySQL) all work. A named window (`OVER w`, which
    /// needs a `WINDOW` clause) and an explicit frame (`ROWS`/`RANGE`/`GROUPS
    /// ...`) are not modeled and are rejected.
    fn parse_over_clause(&mut self) -> Result<Option<ast::Over>> {
        if !self.eat_keyword("OVER") {
            return Ok(None);
        }
        if !self.is(&Token::LParen) {
            return Err(ParseError::Unsupported(
                "OVER with a named window is not supported yet".to_string(),
            ));
        }
        self.expect(&Token::LParen, "`(`")?;

        let mut partition_by = Vec::new();
        if self.eat_keyword("PARTITION") {
            self.expect_keyword("BY")?;
            loop {
                partition_by.push(Box::new(self.expr()?));
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }

        let order_by = self.order_by()?;

        if self.is_keyword("ROWS") || self.is_keyword("RANGE") || self.is_keyword("GROUPS") {
            return Err(ParseError::Unsupported(
                "an explicit window frame (ROWS/RANGE) is not supported yet".to_string(),
            ));
        }
        self.expect(&Token::RParen, "`)`")?;

        Ok(Some(ast::Over::Window(ast::Window {
            base: None,
            partition_by,
            order_by,
            frame_clause: None,
        })))
    }

    /// Parses the arguments of a `CONCAT(a, b, ...)` call (the name and `(` are
    /// already consumed) and lowers them to a left-associative chain of the
    /// engine's `||` concatenation operator. MySQL's `CONCAT` yields NULL when
    /// any argument is NULL, which is exactly the engine's `||` behaviour; the
    /// engine's `concat()` function instead treats NULL as empty, so it is
    /// deliberately not used. At least one argument is required.
    fn concat_call(&mut self) -> Result<ast::Expr> {
        let mut args = Vec::new();
        if !self.is(&Token::RParen) {
            loop {
                args.push(self.expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, "`)`")?;

        let mut iter = args.into_iter();
        let Some(mut acc) = iter.next() else {
            return Err(ParseError::Unsupported(
                "CONCAT() with no arguments is not supported".to_string(),
            ));
        };
        for next in iter {
            acc = ast::Expr::binary(acc, ast::Operator::Concat, next);
        }
        Ok(acc)
    }

    /// Parses a `CHAR(n1, n2, ...)` call (the name and `(` are already consumed)
    /// and lowers it to the engine's `char()`, which builds a string from the
    /// Unicode code points of its integer arguments. Each code is coerced to an
    /// integer the way `CAST(... AS SIGNED)` is — a numeric value rounds
    /// (`CHAR(65.9)` → `B`), a string parses its leading integer (`CHAR('66')` →
    /// `B`) — so a non-integer code matches MySQL. For the common ASCII and
    /// control-character codes (e.g. `CHAR(10)` newline, `CHAR(72, 73)` -> `HI`)
    /// this matches MySQL exactly. Two documented divergences: MySQL skips NULL
    /// arguments whereas the engine stops at the first NULL, and for code points
    /// above 127 MySQL emits raw bytes (a number can span several) while the
    /// engine emits the single UTF-8 code point. An optional trailing
    /// `USING charset` clause is parsed and ignored (the engine always builds
    /// from Unicode code points, matching MySQL's default `utf8mb4`). At least
    /// one argument is required.
    fn char_call(&mut self) -> Result<ast::Expr> {
        let mut args = Vec::new();
        loop {
            // MySQL rounds/parses each code to an integer before building the byte.
            args.push(integer_arg(self.expr()?));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        // An optional trailing `USING charset_name` selects how the resulting
        // bytes are interpreted. The engine builds the string from Unicode code
        // points (MySQL's default `utf8mb4` behaviour), so the charset is parsed
        // and ignored.
        if self.eat_keyword("USING") {
            let _ = self.name()?;
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(call_fn("char", args))
    }

    /// Parses a `QUOTE(str)` call (the name and `(` are already consumed) and
    /// lowers it to MySQL's quoted, escaped string literal: the value wrapped in
    /// single quotes with `'` → `\'`, `\` → `\\`, and Ctrl-Z (`0x1A`) → `\Z`,
    /// built from nested `replace()`s (backslash escaped first so the escapes
    /// added afterwards are not re-escaped). A NULL argument yields the literal
    /// string `NULL` (unquoted), as in MySQL. A non-string argument is coerced to
    /// a string first, exactly as MySQL coerces it.
    ///
    /// One divergence from MySQL (see `mysql/COMPAT.md`): a NUL (`0x00`) byte in
    /// the value is not escaped to `\0`, because the engine's `replace()` treats
    /// strings as NUL-terminated and cannot match a NUL needle. Such embedded-NUL
    /// strings do not occur in practice.
    fn quote_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let str_lit = |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
        // Escape the backslash first, then the single quote and Ctrl-Z; doing the
        // backslash first keeps the backslashes introduced by the later escapes
        // from being doubled.
        let mut escaped = call_fn(
            "replace",
            vec![arg.clone(), str_lit("\\"), str_lit("\\\\")],
        );
        escaped = call_fn("replace", vec![escaped, str_lit("'"), str_lit("\\'")]);
        escaped = call_fn(
            "replace",
            vec![
                escaped,
                call_fn("char", vec![*numeric_expr("26")]),
                str_lit("\\Z"),
            ],
        );

        // Wrap in single quotes: `'` || escaped || `'`.
        let wrapped = ast::Expr::binary(
            ast::Expr::binary(str_lit("'"), ast::Operator::Concat, escaped),
            ast::Operator::Concat,
            str_lit("'"),
        );

        // QUOTE(NULL) is the literal string `NULL`, not SQL NULL.
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(arg)),
                Box::new(str_lit("NULL")),
            )],
            else_expr: Some(Box::new(wrapped)),
        })
    }

    /// Parses `UUID_TO_BIN(uuid[, swap])` (`to_bin` true) or `BIN_TO_UUID(bin[,
    /// swap])` (`to_bin` false) — the name and `(` already consumed — and lowers
    /// the conversion between a dashed 36-character UUID string and its packed
    /// 16-byte form.
    ///
    /// `UUID_TO_BIN(u)` is `unhex(replace(u, '-', ''))` — the dashes stripped and
    /// the 32 hex digits decoded to bytes. `BIN_TO_UUID(b)` is the inverse, the
    /// 32 hex digits of `b` regrouped `8-4-4-4-12` and lower-cased. The optional
    /// `swap` flag (a literal; non-zero swaps, as in MySQL) reorders the first
    /// three time fields — `time-low`, `time-mid`, `time-high` become
    /// `time-high`, `time-mid`, `time-low` — which makes time-ordered UUIDs sort
    /// by their binary form. A NULL argument propagates. The `swap` flag must be an
    /// integer literal (the lowering it selects is fixed at translation time).
    fn uuid_bin_call(&mut self, to_bin: bool) -> Result<ast::Expr> {
        let arg = self.expr()?;
        // The optional second argument is the swap flag, a literal whose value
        // picks the field order at translation time.
        let swap = if self.eat(&Token::Comma) {
            // A leading `-` is accepted but irrelevant: any non-zero value swaps.
            self.eat(&Token::Minus);
            let Some(Token::Num(n)) = self.peek() else {
                return Err(self.unexpected("an integer literal UUID swap flag"));
            };
            let value: i64 = n.trim().parse().map_err(|_| {
                ParseError::Unsupported("UUID swap flag must be an integer literal".to_string())
            })?;
            self.advance();
            value != 0
        } else {
            false
        };
        self.expect(&Token::RParen, "`)`")?;

        let str_lit = |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
        // A `start, len` slice of the 32-hex-digit string.
        let slice = |s: &ast::Expr, start: &str, len: &str| {
            substr_fn(s.clone(), *numeric_expr(start), *numeric_expr(len))
        };
        let concat = |parts: Vec<ast::Expr>| {
            parts
                .into_iter()
                .reduce(|acc, p| ast::Expr::binary(acc, ast::Operator::Concat, p))
                .expect("at least one part")
        };

        if to_bin {
            // h = replace(uuid, '-', '') — the 32 hex digits.
            let h = call_fn("replace", vec![arg, str_lit("-"), str_lit("")]);
            let hex = if swap {
                // time-high (13,4) | time-mid (9,4) | time-low (1,8) | rest (17,16).
                concat(vec![
                    slice(&h, "13", "4"),
                    slice(&h, "9", "4"),
                    slice(&h, "1", "8"),
                    slice(&h, "17", "16"),
                ])
            } else {
                h
            };
            Ok(call_fn("unhex", vec![hex]))
        } else {
            // x = hex(bin) — the 32 hex digits, in the stored (possibly swapped)
            // order. The engine's `hex(NULL)` is the empty string rather than
            // NULL, so a guard restores MySQL's NULL-propagating result.
            let null_guard = ast::Expr::is_null(arg.clone());
            let x = call_fn("hex", vec![arg]);
            let dash = || str_lit("-");
            let groups = if swap {
                // Undo the swap: time-low is at 9..16, time-high at 1..4.
                vec![
                    slice(&x, "9", "8"),
                    dash(),
                    slice(&x, "5", "4"),
                    dash(),
                    slice(&x, "1", "4"),
                    dash(),
                    slice(&x, "17", "4"),
                    dash(),
                    slice(&x, "21", "12"),
                ]
            } else {
                vec![
                    slice(&x, "1", "8"),
                    dash(),
                    slice(&x, "9", "4"),
                    dash(),
                    slice(&x, "13", "4"),
                    dash(),
                    slice(&x, "17", "4"),
                    dash(),
                    slice(&x, "21", "12"),
                ]
            };
            let formatted = call_fn("lower", vec![concat(groups)]);
            Ok(ast::Expr::Case {
                base: None,
                when_then_pairs: vec![(
                    Box::new(null_guard),
                    Box::new(ast::Expr::Literal(ast::Literal::Null)),
                )],
                else_expr: Some(Box::new(formatted)),
            })
        }
    }

    /// Parses an `ASCII(str)` / `ORD(str)` call (the name and `(` are already
    /// consumed) and lowers it to the code point of the first character via the
    /// engine's `unicode()`, with MySQL's edges restored:
    /// `CASE WHEN str = '' THEN 0 ELSE unicode(str) END`. MySQL's `ASCII('')` is
    /// `0` (the engine's `unicode('')` is NULL), and a NULL argument stays NULL
    /// (the `= ''` test is NULL, so the `ELSE` runs `unicode(NULL)` = NULL).
    ///
    /// For an ASCII first character this matches MySQL exactly (`ASCII` and `ORD`
    /// agree there). It diverges for a non-ASCII first character: MySQL's `ASCII`
    /// returns the leading *byte* (0-255) and `ORD` a byte-weighted value, while
    /// this returns the Unicode code point; a string whose first byte is NUL also
    /// diverges (MySQL `0`, here NULL). Documented in COMPAT.md.
    fn ascii_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let is_empty = ast::Expr::binary(
            arg.clone(),
            ast::Operator::Equals,
            ast::Expr::Literal(ast::Literal::String(requote(""))),
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(is_empty),
                Box::new(ast::Expr::Literal(ast::Literal::Numeric("0".to_string()))),
            )],
            else_expr: Some(Box::new(unary_fn("unicode", arg))),
        })
    }

    /// Parses a `FIELD(x, a, b, ...)` call (the name and `(` are already
    /// consumed) and lowers it to `CASE x COLLATE NOCASE WHEN a THEN 1 WHEN b THEN
    /// 2 ... ELSE 0 END`, which the engine evaluates the same way MySQL's `FIELD`
    /// does: the 1-based index of the first argument among the rest, or 0 if
    /// absent or NULL. The `COLLATE NOCASE` on the base makes the `WHEN`
    /// comparisons fold ASCII case like MySQL's default collation
    /// (`FIELD('a', 'A', 'b')` is `1`); it is harmless for a numeric `x`. At least
    /// one argument is required.
    fn field_call(&mut self) -> Result<ast::Expr> {
        let mut args = Vec::new();
        if !self.is(&Token::RParen) {
            loop {
                args.push(self.expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, "`)`")?;

        let mut iter = args.into_iter();
        let Some(base) = iter.next() else {
            return Err(ParseError::Unsupported(
                "FIELD() requires at least one argument".to_string(),
            ));
        };
        let base = ast::Expr::collate(base, ast::Name::from_string("NOCASE"));
        let when_then_pairs = iter
            .enumerate()
            .map(|(i, value)| {
                let index = ast::Expr::Literal(ast::Literal::Numeric((i + 1).to_string()));
                (Box::new(value), Box::new(index))
            })
            .collect();
        Ok(ast::Expr::Case {
            base: Some(Box::new(base)),
            when_then_pairs,
            else_expr: Some(Box::new(ast::Expr::Literal(ast::Literal::Numeric(
                "0".to_string(),
            )))),
        })
    }

    /// Parses an `ELT(n, a, b, ...)` call (the name and `(` are already consumed)
    /// and lowers it to `CASE <int n> WHEN 1 THEN a WHEN 2 THEN b ... END` — the
    /// `n`-th string argument (1-based). MySQL coerces `n` to an integer index the
    /// way `CAST(n AS SIGNED)` does (a numeric `1.9` rounds to `2`, a string `'2'`
    /// parses to `2`), so the index is wrapped in [`build_cast`] to an integer;
    /// otherwise a non-integer `n` would match no `WHEN`. The `CASE` has no `ELSE`,
    /// so an out-of-range or NULL `n` (matching no `WHEN`) yields NULL, as in
    /// MySQL. At least two arguments (the index and one string) are required.
    fn elt_call(&mut self) -> Result<ast::Expr> {
        let index = self.expr()?;
        let mut strings = Vec::new();
        while self.eat(&Token::Comma) {
            strings.push(self.expr()?);
        }
        self.expect(&Token::RParen, "`)`")?;
        if strings.is_empty() {
            return Err(ParseError::Unsupported(
                "ELT() requires at least two arguments".to_string(),
            ));
        }
        let when_then_pairs = strings
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let idx = ast::Expr::Literal(ast::Literal::Numeric((i + 1).to_string()));
                (Box::new(idx), Box::new(value))
            })
            .collect();
        // MySQL rounds/parses `n` to an integer index, so coerce it the same way a
        // `CAST(n AS SIGNED)` would before matching the `WHEN` arms.
        let int_index = build_cast(
            index,
            ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            },
        );
        Ok(ast::Expr::Case {
            base: Some(Box::new(int_index)),
            when_then_pairs,
            else_expr: None,
        })
    }

    /// Parses a `MAKE_SET(bits, s1, s2, ...)` call (the name and `(` are already
    /// consumed) and lowers it to the comma-joined set of the strings whose
    /// corresponding bit in `bits` is set: string `s_i` (1-based `i`) is in the
    /// result when bit `i-1` of `bits` is on.
    ///
    /// The lowering is `CONCAT_WS(',', CASE WHEN bits & 1 THEN s1 END,
    /// CASE WHEN bits & 2 THEN s2 END, ...)`. Each `CASE` yields its string when
    /// the bit is set and NULL otherwise; `CONCAT_WS` joins the present strings
    /// with `,` and skips the NULL (unset) slots — and also skips a NULL string
    /// argument even when its bit is set, exactly as MySQL does. A NULL `bits`
    /// makes every `bits & mask` NULL, so `CONCAT_WS` would return the empty
    /// string; an outer guard restores MySQL's NULL result. At least one string
    /// argument is required; strings past the 64th cannot be addressed by the
    /// 64-bit mask and are dropped (their bit is always zero).
    fn make_set_call(&mut self) -> Result<ast::Expr> {
        let bits = self.expr()?;
        let mut strings = Vec::new();
        while self.eat(&Token::Comma) {
            strings.push(self.expr()?);
        }
        self.expect(&Token::RParen, "`)`")?;
        if strings.is_empty() {
            return Err(ParseError::Unsupported(
                "MAKE_SET() requires at least one string argument".to_string(),
            ));
        }

        let mut args = vec![ast::Expr::Literal(ast::Literal::String(requote(",")))];
        for (i, s) in strings.into_iter().enumerate() {
            if i >= 64 {
                break;
            }
            let mask = ast::Expr::Literal(ast::Literal::Numeric((1u64 << i).to_string()));
            let test = ast::Expr::binary(bits.clone(), ast::Operator::BitwiseAnd, mask);
            args.push(ast::Expr::Case {
                base: None,
                when_then_pairs: vec![(Box::new(test), Box::new(s))],
                else_expr: None,
            });
        }
        let joined = call_fn("concat_ws", args);

        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(bits)),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(joined)),
        })
    }

    /// Parses `GREATEST(a, b, ...)` (`is_greatest` true) or `LEAST(a, b, ...)` (the
    /// name and `(` already consumed) and lowers it to the largest / smallest
    /// argument under a **case-insensitive** comparison, matching MySQL's default
    /// collation. The engine's `max`/`min` compare strings case-sensitively, so
    /// each pairwise comparison applies `COLLATE NOCASE` instead — which the engine
    /// ignores for a numeric operand, so numbers still compare numerically.
    ///
    /// The result is a balanced reduction of `CASE WHEN a >= (b COLLATE NOCASE)
    /// THEN a ELSE b END` (`<=` for `LEAST`), so the expression stays `O(n²)`
    /// rather than the exponential blow-up of a left-linear fold. A guard returns
    /// NULL when any argument is NULL, as in MySQL (and as the engine's `max`/`min`
    /// did). At least two arguments are required.
    fn greatest_least_call(&mut self, is_greatest: bool) -> Result<ast::Expr> {
        let mut args = vec![self.expr()?];
        while self.eat(&Token::Comma) {
            args.push(self.expr()?);
        }
        self.expect(&Token::RParen, "`)`")?;
        if args.len() < 2 {
            return Err(ParseError::Unsupported(
                "GREATEST/LEAST requires at least two arguments".to_string(),
            ));
        }

        // NULL in any argument makes the whole result NULL.
        let null_guard = args
            .iter()
            .map(|a| ast::Expr::is_null(a.clone()))
            .reduce(|acc, g| ast::Expr::binary(acc, ast::Operator::Or, g))
            .expect("at least two arguments");

        // Balanced pairwise reduction so the argument fan-out is quadratic, not
        // exponential.
        let mut level = args;
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            let mut it = level.into_iter();
            while let Some(a) = it.next() {
                match it.next() {
                    Some(b) => next.push(case_insensitive_extremum(a, b, is_greatest)),
                    None => next.push(a),
                }
            }
            level = next;
        }
        let extremum = level.into_iter().next().expect("non-empty");

        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(null_guard),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(extremum)),
        })
    }

    /// Parses `INET_NTOA(n)` (the name and `(` are already consumed) and lowers
    /// it to the dotted-quad IPv4 string of the 32-bit number `n`:
    /// `((n >> 24) & 255) || '.' || ((n >> 16) & 255) || '.' || ((n >> 8) & 255)
    /// || '.' || (n & 255)`. Each octet is the corresponding byte of `n`, and the
    /// `||` concatenation (with the integer octets coerced to text) propagates
    /// NULL, so `INET_NTOA(NULL)` is NULL, as in MySQL. Values outside the
    /// 0..2^32-1 IPv4 range are not meaningful (as in MySQL). Exactly one argument
    /// is required.
    fn inet_ntoa_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let num = |v: i64| ast::Expr::Literal(ast::Literal::Numeric(v.to_string()));
        // The byte of `n` at `shift` bits (the low byte when `shift` is 0).
        let octet = |shift: i64| {
            let shifted = if shift == 0 {
                n.clone()
            } else {
                ast::Expr::binary(n.clone(), ast::Operator::RightShift, num(shift))
            };
            ast::Expr::binary(shifted, ast::Operator::BitwiseAnd, num(255))
        };
        let dot = || ast::Expr::Literal(ast::Literal::String(requote(".")));
        let concat = |a, b| ast::Expr::binary(a, ast::Operator::Concat, b);
        let result = concat(octet(24), dot());
        let result = concat(result, octet(16));
        let result = concat(result, dot());
        let result = concat(result, octet(8));
        let result = concat(result, dot());
        Ok(concat(result, octet(0)))
    }

    /// Parses `EXPORT_SET(bits, on, off[, separator[, number_of_bits]])` (the
    /// name and `(` are already consumed) and lowers it to the string with one
    /// entry per low bit of `bits` — `on` where the bit is set, `off` where it is
    /// not — joined by `separator` (default `,`), for `number_of_bits` bits
    /// (default 64, clamped to `0..=64`).
    ///
    /// The lowering is `CONCAT_WS(sep, CASE WHEN bits & 1 THEN on ELSE off END,
    /// CASE WHEN bits & 2 THEN on ELSE off END, ...)`. With `on`/`off` non-NULL
    /// every entry is present, so `CONCAT_WS` joins exactly `number_of_bits` of
    /// them. An outer guard returns NULL when `bits`, `on`, or `off` is NULL (and
    /// a NULL `separator` makes `CONCAT_WS` itself NULL) — matching MySQL, which
    /// returns NULL for a NULL argument. `number_of_bits` must be an integer
    /// literal so the entry count is fixed at parse time.
    fn export_set_call(&mut self) -> Result<ast::Expr> {
        let bits = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let on = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let off = self.expr()?;
        let sep = if self.eat(&Token::Comma) {
            self.expr()?
        } else {
            ast::Expr::Literal(ast::Literal::String(requote(",")))
        };
        // `number_of_bits` (only after a separator) defaults to 64 and must be a
        // literal so the number of entries is known now; MySQL clamps it to 64.
        let num_bits = if self.eat(&Token::Comma) {
            match self.expr()? {
                ast::Expr::Literal(ast::Literal::Numeric(n)) => {
                    n.parse::<i64>().unwrap_or(64).clamp(0, 64)
                }
                _ => {
                    return Err(ParseError::Unsupported(
                        "EXPORT_SET number_of_bits must be an integer literal".to_string(),
                    ))
                }
            }
        } else {
            64
        };
        self.expect(&Token::RParen, "`)`")?;

        let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
        let mut args = vec![sep];
        for i in 0..num_bits {
            // Test bit `i` as `(bits >> i) & 1` rather than `bits & 2^i`, so the
            // 64th mask (`2^63`) does not overflow the engine's signed 64-bit
            // integer; the arithmetic shift still reads the sign bit correctly.
            let shifted = if i == 0 {
                bits.clone()
            } else {
                ast::Expr::binary(
                    bits.clone(),
                    ast::Operator::RightShift,
                    ast::Expr::Literal(ast::Literal::Numeric(i.to_string())),
                )
            };
            let test = ast::Expr::binary(shifted, ast::Operator::BitwiseAnd, one());
            args.push(ast::Expr::Case {
                base: None,
                when_then_pairs: vec![(Box::new(test), Box::new(on.clone()))],
                else_expr: Some(Box::new(off.clone())),
            });
        }
        // With no bits the result is empty (CONCAT_WS needs at least one value).
        let joined = if num_bits == 0 {
            ast::Expr::Literal(ast::Literal::String(requote("")))
        } else {
            call_fn("concat_ws", args)
        };

        // A NULL `bits`/`on`/`off` yields NULL (a NULL separator already makes
        // CONCAT_WS NULL).
        let guard = ast::Expr::binary(
            ast::Expr::binary(
                ast::Expr::is_null(bits),
                ast::Operator::Or,
                ast::Expr::is_null(on),
            ),
            ast::Operator::Or,
            ast::Expr::is_null(off),
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(guard),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(joined)),
        })
    }

    /// Parses `REGEXP_LIKE(str, pattern[, match_type])` (the name and `(` already
    /// consumed) and lowers it to the same `str REGEXP pattern` the operator
    /// produces — the engine's `REGEXP` over a pattern carrying the inline flags.
    /// Like the operator, the match defaults to case-insensitive (MySQL's default
    /// under the standard collation), realized by prepending `(?i)` to the
    /// pattern. An optional `match_type` string literal overrides the flags:
    /// `c` case-sensitive, `i` case-insensitive, `m` multi-line (`^`/`$` at line
    /// breaks), `n` dot-matches-newline; `u` (Unix line endings) is accepted and
    /// ignored. A NULL `str` or `pattern` yields NULL. The `match_type` must be a
    /// literal so the flags are known at parse time.
    fn regexp_like_call(&mut self) -> Result<ast::Expr> {
        let subject = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let pattern_arg = self.expr()?;
        let match_type = if self.eat(&Token::Comma) {
            let Some(Token::Str(mt)) = self.peek() else {
                return Err(self.unexpected("a string-literal REGEXP_LIKE match type"));
            };
            let mt = mt.clone();
            self.advance();
            Some(mt)
        } else {
            None
        };
        self.expect(&Token::RParen, "`)`")?;

        let prefix = regexp_flag_prefix(match_type.as_deref())?;
        let pattern = if prefix.is_empty() {
            pattern_arg
        } else {
            ast::Expr::binary(
                ast::Expr::Literal(ast::Literal::String(requote(&prefix))),
                ast::Operator::Concat,
                pattern_arg,
            )
        };
        Ok(ast::Expr::like(
            subject,
            false,
            ast::LikeOperator::Regexp,
            pattern,
            None,
        ))
    }

    /// Parses `BIT_COUNT(n)` (the name and `(` are already consumed) and lowers
    /// it to the number of set bits of `n`, the sum of its 64 bits each tested as
    /// `(n >> i) & 1` (the shift elided for bit 0). The engine's arithmetic shift
    /// reads the sign bit, so the count is over the unsigned 64-bit value, as in
    /// MySQL (`BIT_COUNT(-1)` is 64). A NULL argument makes every bit NULL, so the
    /// sum is NULL. The 64 terms are folded into a *balanced* tree of additions
    /// so the expression stays shallow — a left-nested 64-deep sum overflows the
    /// engine's recursive evaluator.
    fn bit_count_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
        let mut terms: Vec<ast::Expr> = (0..64)
            .map(|i| {
                let shifted = if i == 0 {
                    n.clone()
                } else {
                    ast::Expr::binary(
                        n.clone(),
                        ast::Operator::RightShift,
                        ast::Expr::Literal(ast::Literal::Numeric(i.to_string())),
                    )
                };
                ast::Expr::binary(shifted, ast::Operator::BitwiseAnd, one())
            })
            .collect();
        while terms.len() > 1 {
            let mut next = Vec::with_capacity(terms.len().div_ceil(2));
            let mut iter = terms.into_iter();
            while let Some(a) = iter.next() {
                match iter.next() {
                    Some(b) => next.push(ast::Expr::binary(a, ast::Operator::Add, b)),
                    None => next.push(a),
                }
            }
            terms = next;
        }
        Ok(terms.pop().expect("64 bit terms reduce to one"))
    }

    /// Parses `BIN(n)` (the name and `(` are already consumed) and lowers it to
    /// the base-2 string of `n` (taken as unsigned 64-bit), with no leading
    /// zeros. It builds the 64 bit characters most-significant first — each
    /// `CASE WHEN (n >> i) & 1 THEN '1' ELSE '0' END` (the arithmetic shift reads
    /// the sign bit, so `BIN(-1)` is 64 ones, as in MySQL) — joins them with the
    /// engine's flat `concat` (not the front-end `CONCAT`, whose `||` chain would
    /// nest 64 deep and overflow the evaluator), and strips the leading zeros with
    /// `ltrim(..., '0')`. A guard returns `'0'` for `n = 0` (where the trim would
    /// leave the empty string) and NULL for a NULL argument.
    fn bin_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let bit_char = |c: &str| ast::Expr::Literal(ast::Literal::String(requote(c)));
        let bits: Vec<ast::Expr> = (0..64)
            .rev()
            .map(|i| {
                let shifted = if i == 0 {
                    n.clone()
                } else {
                    ast::Expr::binary(
                        n.clone(),
                        ast::Operator::RightShift,
                        ast::Expr::Literal(ast::Literal::Numeric(i.to_string())),
                    )
                };
                let test = ast::Expr::binary(
                    shifted,
                    ast::Operator::BitwiseAnd,
                    ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
                );
                ast::Expr::Case {
                    base: None,
                    when_then_pairs: vec![(Box::new(test), Box::new(bit_char("1")))],
                    else_expr: Some(Box::new(bit_char("0"))),
                }
            })
            .collect();
        let joined = call_fn("concat", bits);
        let trimmed = call_fn("ltrim", vec![joined, bit_char("0")]);
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![
                (
                    Box::new(ast::Expr::is_null(n.clone())),
                    Box::new(ast::Expr::Literal(ast::Literal::Null)),
                ),
                (
                    Box::new(ast::Expr::binary(
                        n,
                        ast::Operator::Equals,
                        ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
                    )),
                    Box::new(bit_char("0")),
                ),
            ],
            else_expr: Some(Box::new(trimmed)),
        })
    }

    /// Parses a `FIND_IN_SET(str, strlist)` call (the name and `(` are already
    /// consumed) and lowers it to the 1-based index of `str` among the
    /// comma-separated elements of `strlist`, or 0 if absent.
    ///
    /// With `h = lower(',' || strlist || ',')` and `n = lower(',' || str || ',')`,
    /// the match's position is `instr(h, n)`; the element index is the number of
    /// commas in `substr(h, 1, instr(h, n))`, counted as
    /// `length(prefix) - length(replace(prefix, ',', ''))`. When `str` is absent
    /// `instr` is 0, the prefix is empty, and the count is 0. Wrapping in `lower`
    /// gives MySQL's default case-insensitive match (ASCII). NULL propagates.
    /// (A `str` that itself contains a comma returns 0 in MySQL but may match here
    /// — a documented edge.)
    fn find_in_set_call(&mut self) -> Result<ast::Expr> {
        let needle = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let strlist = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let needle = comma_wrapped_lower(needle);
        let pos = call_fn("instr", vec![comma_wrapped_lower(strlist.clone()), needle]);
        let prefix = substr_fn(
            comma_wrapped_lower(strlist),
            ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
            pos,
        );
        let without_commas = call_fn(
            "replace",
            vec![
                prefix.clone(),
                ast::Expr::Literal(ast::Literal::String(requote(","))),
                ast::Expr::Literal(ast::Literal::String(requote(""))),
            ],
        );
        Ok(ast::Expr::binary(
            call_fn("length", vec![prefix]),
            ast::Operator::Subtract,
            call_fn("length", vec![without_commas]),
        ))
    }

    /// Parses a `TIMEDIFF(a, b)` call (the name and `(` are already consumed) and
    /// lowers it to MySQL's `a - b` rendered as a `TIME` string — `[-]HH:MM:SS`,
    /// where the hour field is unbounded (e.g. `25:30:00`) and a negative result
    /// carries a leading `-`. The difference is taken in whole seconds via
    /// `CAST(ROUND((julianday(a) - julianday(b)) * 86400) AS INTEGER)` — `julianday`
    /// parses both DATETIME and bare TIME strings, and for two times the implied
    /// date cancels — then formatted with `printf`. A NULL or unparseable argument
    /// makes the inner cast NULL, so the guarding `CASE` returns NULL, as in MySQL.
    ///
    /// The engine has its own `timediff`, but it renders the SQLite
    /// `±YYYY-MM-DD HH:MM:SS.SSS` form rather than MySQL's, so the MySQL spelling
    /// is synthesized here. Divergences from MySQL (see `mysql/COMPAT.md`): the
    /// result is not clamped to the TIME range `±838:59:59`, mismatched argument
    /// types (one TIME and one DATETIME) are not detected (MySQL returns NULL),
    /// and a fractional-second part is truncated to whole seconds.
    fn timediff_call(&mut self) -> Result<ast::Expr> {
        let a = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let b = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        // d = CAST(ROUND((julianday(a) - julianday(b)) * 86400) AS INTEGER)
        let seconds = ast::Expr::binary(
            ast::Expr::binary(
                unary_fn("julianday", a),
                ast::Operator::Subtract,
                unary_fn("julianday", b),
            ),
            ast::Operator::Multiply,
            *numeric_expr("86400"),
        );
        let d = ast::Expr::Cast {
            expr: Box::new(call_fn("round", vec![seconds])),
            type_name: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            }),
        };

        // sign = CASE WHEN d < 0 THEN '-' ELSE '' END
        let sign = ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::binary(
                    d.clone(),
                    ast::Operator::Less,
                    *numeric_expr("0"),
                )),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("-")))),
            )],
            else_expr: Some(Box::new(ast::Expr::Literal(ast::Literal::String(requote(""))))),
        };

        // The HH/MM/SS fields of the absolute second count (hours unbounded).
        let abs_d = || unary_fn("abs", d.clone());
        let hh = ast::Expr::binary(abs_d(), ast::Operator::Divide, *numeric_expr("3600"));
        let mm = ast::Expr::binary(
            ast::Expr::binary(abs_d(), ast::Operator::Modulus, *numeric_expr("3600")),
            ast::Operator::Divide,
            *numeric_expr("60"),
        );
        let ss = ast::Expr::binary(abs_d(), ast::Operator::Modulus, *numeric_expr("60"));

        let body = call_fn(
            "printf",
            vec![
                ast::Expr::Literal(ast::Literal::String(requote("%s%02d:%02d:%02d"))),
                sign,
                hh,
                mm,
                ss,
            ],
        );

        // A NULL or unparseable argument makes `d` NULL; return SQL NULL then
        // (printf would otherwise coerce the NULL fields to zeros).
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(d.clone())),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(body)),
        })
    }

    /// Parses an `ISNULL(x)` call (the name and `(` are already consumed) and
    /// lowers it to the `x IS NULL` predicate, which the engine evaluates to 1
    /// when `x` is NULL and 0 otherwise — exactly MySQL's `ISNULL`. Exactly one
    /// argument is required.
    fn isnull_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::is_null(arg))
    }

    /// Parses `NULLIF(x, y)` (the name and `(` are already consumed) and lowers it
    /// to `CASE WHEN x = (y COLLATE NOCASE) THEN NULL ELSE x END` — NULL when `x`
    /// equals `y`, else `x`. The `COLLATE NOCASE` makes the equality fold ASCII
    /// case on a string comparison, like MySQL's default collation
    /// (`NULLIF('a', 'A')` is NULL), and is ignored for a numeric operand. The
    /// engine's own `nullif` compares case-sensitively, hence the explicit
    /// lowering. A NULL `x` (where `x = y` is NULL, not true) returns `x` (NULL),
    /// and a NULL `y` returns `x`, both as in MySQL. Exactly two arguments are
    /// required.
    fn nullif_call(&mut self) -> Result<ast::Expr> {
        let x = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let y = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let equal = ast::Expr::binary(
            x.clone(),
            ast::Operator::Equals,
            ast::Expr::collate(y, ast::Name::from_string("NOCASE")),
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(equal),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(x)),
        })
    }

    /// Parses an `INSTR(str, substr)` or `LOCATE(substr, str[, pos])` call (the
    /// name and `(` are already consumed) and lowers it to `instr(lower(str),
    /// lower(substr))` — the 1-based position of the substring, or 0 if absent.
    /// `swap_args` is true for `LOCATE`, whose operands are the reverse of
    /// `INSTR`. Wrapping both operands in `lower()` makes the search
    /// case-insensitive, matching MySQL's default-collation behaviour (ASCII case
    /// folding; positions are unchanged so the result matches). NULL propagates.
    ///
    /// `LOCATE(substr, str, pos)` searches from the 1-based `pos`: it lowers to
    /// `CASE WHEN instr(lower(substr(str, pos)), lower(substr)) = 0 THEN 0 ELSE
    /// pos - 1 + that_instr END` — the position relative to `pos`, shifted back to
    /// an absolute position, or 0 when not found. Only the normal `pos >= 1` range
    /// matches MySQL (a non-positive `pos` is a documented edge). The three-arg
    /// form is `LOCATE`-only; an `INSTR` with three arguments is rejected.
    fn instr_call(&mut self, swap_args: bool) -> Result<ast::Expr> {
        let first = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let second = self.expr()?;
        let pos = if self.eat(&Token::Comma) {
            if !swap_args {
                return Err(ParseError::Unsupported(
                    "INSTR takes exactly two arguments".to_string(),
                ));
            }
            Some(self.expr()?)
        } else {
            None
        };
        self.expect(&Token::RParen, "`)`")?;
        let (haystack, needle) = if swap_args {
            (second, first)
        } else {
            (first, second)
        };

        let Some(pos) = pos else {
            return Ok(call_fn(
                "instr",
                vec![unary_fn("lower", haystack), unary_fn("lower", needle)],
            ));
        };
        // MySQL rounds a fractional start position to an integer; without this the
        // fraction would leak into the absolute result (`pos - 1 + rel`).
        let pos = integer_arg(pos);

        // Search from `pos`: find the needle in the tail `substr(haystack, pos)`,
        // then shift the relative position back to absolute (`pos - 1 + rel`).
        let relative = call_fn(
            "instr",
            vec![
                unary_fn("lower", call_fn("substr", vec![haystack, pos.clone()])),
                unary_fn("lower", needle),
            ],
        );
        let zero = || ast::Expr::Literal(ast::Literal::Numeric("0".to_string()));
        let absolute = ast::Expr::binary(
            ast::Expr::binary(
                pos,
                ast::Operator::Subtract,
                ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
            ),
            ast::Operator::Add,
            relative.clone(),
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::binary(relative, ast::Operator::Equals, zero())),
                Box::new(zero()),
            )],
            else_expr: Some(Box::new(absolute)),
        })
    }

    /// Parses the SQL-standard `POSITION(substr IN str)` (the name and `(` are
    /// already consumed) and lowers it exactly like `LOCATE(substr, str)`:
    /// `instr(lower(str), lower(substr))` — the 1-based position of the substring
    /// (case-insensitive, matching MySQL's default collation), or 0 if absent.
    /// NULL propagates.
    fn position_call(&mut self) -> Result<ast::Expr> {
        // The operands are `bit_expr`s (below the comparison/`IN` tier), so parse
        // the substring at the bitwise level — `self.expr()` would otherwise
        // swallow the separating `IN` as an `IN`-list operator.
        let needle = self.bitor_expr()?;
        self.expect_keyword("IN")?;
        let haystack = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(call_fn(
            "instr",
            vec![unary_fn("lower", haystack), unary_fn("lower", needle)],
        ))
    }

    /// Parses `SUBSTRING`/`SUBSTR` (the name and `(` are already consumed) in both
    /// the comma form `(str, pos[, len])` and the SQL-standard form
    /// `(str FROM pos [FOR len])`, lowering either to the engine's
    /// `substr(str, pos[, len])` (1-indexed, negative position from the end, like
    /// MySQL). `FROM`/`FOR` are keywords, not operators, so the operands parse as
    /// ordinary expressions.
    fn substring_call(&mut self) -> Result<ast::Expr> {
        let target = self.expr()?;
        let (pos, len) = if self.eat_keyword("FROM") {
            let pos = self.expr()?;
            let len = if self.eat_keyword("FOR") {
                Some(self.expr()?)
            } else {
                None
            };
            (pos, len)
        } else {
            self.expect(&Token::Comma, "`,` or `FROM`")?;
            let pos = self.expr()?;
            let len = if self.eat(&Token::Comma) {
                Some(self.expr()?)
            } else {
                None
            };
            (pos, len)
        };
        self.expect(&Token::RParen, "`)`")?;
        // MySQL rounds a fractional position/length to an integer.
        Ok(guarded_substr(
            target,
            integer_arg(pos),
            len.map(integer_arg),
        ))
    }

    /// Parses `SUBSTRING_INDEX(str, delim, count)` (the name and `(` already
    /// consumed) and lowers it to the part of `str` around the count-th
    /// occurrence of `delim`. `count = 1` returns the part before the first
    /// delimiter (`SUBSTRING_INDEX('a.b.c', '.', 1)` → `a`), `count = -1` the part
    /// after the last (→ `c`), and `count = 0` is the empty string; if the
    /// delimiter is absent, the whole string is returned. The delimiter match is
    /// case-sensitive, as in MySQL. NULL arguments propagate.
    ///
    /// The `count = 1` case lowers to `CASE WHEN instr(str, delim) = 0 THEN str
    /// ELSE substr(str, 1, instr(str, delim) - 1) END`, and `count = -1` applies
    /// that to the reversed string and delimiter and reverses the result back.
    ///
    /// `count` must be an integer literal with `|count| <= 1`. A larger count
    /// would have to count to the n-th delimiter, which has no bounded
    /// closed-form expression — only an unrolled one whose size grows with the
    /// count — and the engine's evaluator overflows its stack on the resulting
    /// deep expression (especially once such calls are nested), so a larger or
    /// runtime count is rejected rather than risking a crash (see
    /// `mysql/COMPAT.md`). (Divergence: like the engine's other string lowerings,
    /// `instr`/`length`/`substr` work on characters, so a multi-byte delimiter is
    /// matched per character rather than per byte as MySQL does.)
    fn substring_index_call(&mut self) -> Result<ast::Expr> {
        let s = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let delim = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let negative = self.eat(&Token::Minus);
        let Some(Token::Num(raw)) = self.peek() else {
            return Err(self.unexpected("an integer literal count"));
        };
        let raw = raw.clone();
        self.advance();
        self.expect(&Token::RParen, "`)`")?;

        let magnitude: i64 = raw.trim().parse().map_err(|_| {
            ParseError::Unsupported(
                "SUBSTRING_INDEX count must be an integer literal".to_string(),
            )
        })?;
        // `count = 0` is the empty string.
        if magnitude == 0 {
            return Ok(ast::Expr::Literal(ast::Literal::String(requote(""))));
        }
        if magnitude != 1 {
            return Err(ParseError::Unsupported(
                "SUBSTRING_INDEX with |count| > 1 is not supported yet".to_string(),
            ));
        }

        if negative {
            // SUBSTRING_INDEX(s, d, -1) == reverse(SUBSTRING_INDEX(reverse(s),
            // reverse(d), 1)) — the field after the last delimiter.
            let before = substring_index_before_first(
                unary_fn("string_reverse", s),
                unary_fn("string_reverse", delim),
            );
            Ok(unary_fn("string_reverse", before))
        } else {
            Ok(substring_index_before_first(s, delim))
        }
    }

    /// Parses a `GROUP_CONCAT([DISTINCT] expr [SEPARATOR 's'])` call (the name and
    /// `(` are already consumed) and lowers it to the engine's
    /// `group_concat([DISTINCT] expr[, 's'])`. MySQL's default separator is `,`,
    /// which is also the engine's default, so a bare `GROUP_CONCAT(expr)` becomes
    /// `group_concat(expr)`. `DISTINCT` concatenates only the distinct values, as
    /// in MySQL. Like MySQL — and the engine — the concatenation order is
    /// unspecified without an `ORDER BY`.
    ///
    /// The inner `ORDER BY`, the multi-expression form, and `DISTINCT` *combined
    /// with* a custom `SEPARATOR` (the engine forbids a `DISTINCT` aggregate with
    /// more than one argument) are not modeled and are rejected.
    fn group_concat_call(&mut self) -> Result<ast::Expr> {
        let distinct = self.eat_keyword("DISTINCT");
        if !distinct {
            self.eat_keyword("ALL");
        }
        let expr = self.expr()?;
        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "GROUP_CONCAT with multiple expressions is not supported yet".to_string(),
            ));
        }
        if self.is_keyword("ORDER") {
            return Err(ParseError::Unsupported(
                "GROUP_CONCAT(... ORDER BY ...) is not supported yet".to_string(),
            ));
        }
        let mut args = vec![Box::new(expr)];
        if self.eat_keyword("SEPARATOR") {
            if distinct {
                return Err(ParseError::Unsupported(
                    "GROUP_CONCAT(DISTINCT ... SEPARATOR ...) is not supported yet".to_string(),
                ));
            }
            args.push(Box::new(self.expr()?));
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("group_concat"),
            distinctness: distinct.then_some(ast::Distinctness::Distinct),
            args,
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses a `RAND([seed])` call (the name and `(` are already consumed) and
    /// lowers it to `abs(random() % 1000000000) / 1000000000.0`, a pseudo-random
    /// float in `[0, 1)` like MySQL's `RAND()`. A seed argument is parsed but
    /// discarded — the engine's RNG is not seedable, so `RAND(n)` is not the
    /// deterministic sequence MySQL produces (see `mysql/COMPAT.md`).
    fn rand_call(&mut self) -> Result<ast::Expr> {
        if !self.is(&Token::RParen) {
            loop {
                let _ = self.expr()?;
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, "`)`")?;

        let random = ast::Expr::FunctionCall {
            name: ast::Name::from_string("random"),
            distinctness: None,
            args: Vec::new(),
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        };
        let modulo = ast::Expr::binary(
            random,
            ast::Operator::Modulus,
            ast::Expr::Literal(ast::Literal::Numeric("1000000000".to_string())),
        );
        let magnitude = ast::Expr::FunctionCall {
            name: ast::Name::from_string("abs"),
            distinctness: None,
            args: vec![Box::new(modulo)],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        };
        Ok(ast::Expr::binary(
            magnitude,
            ast::Operator::Divide,
            ast::Expr::Literal(ast::Literal::Numeric("1000000000.0".to_string())),
        ))
    }

    /// Parses a no-op function call (the name and `(` are already consumed),
    /// discards its arguments, and folds it to the constant `result` — the value
    /// MySQL returns once its (unmodeled) side effect would have run. Used for the
    /// advisory locks and the timing functions (`SLEEP`/`BENCHMARK`); the constant
    /// is chosen per function at the call site.
    fn noop_constant_call(&mut self, result: ast::Expr) -> Result<ast::Expr> {
        if !self.is(&Token::RParen) {
            loop {
                let _ = self.expr()?;
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(result)
    }

    /// Parses `TRUNCATE(x, d)` (the name and `(` are already consumed) and lowers
    /// it to `trunc(x * pow(10, d)) / pow(10, d)`: truncate `x` to `d` decimal
    /// places toward zero, using the engine's integer-truncating `trunc` after
    /// scaling by `10^d`. A negative `d` truncates left of the decimal point
    /// (`TRUNCATE(1234.5, -2)` = 1200), and NULL in either argument propagates.
    fn truncate_call(&mut self) -> Result<ast::Expr> {
        let x = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let d = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        // With a literal `d <= 0` the result is a whole number, which MySQL types
        // as an integer (`TRUNCATE(3.7, 0)` is `3`, `TRUNCATE(1234.5, -2)` is
        // `1200`); the engine's `trunc` is a real, so cast it like `FLOOR`/`ROUND`.
        // A positive (or non-literal) `d` keeps the fractional part as a real.
        let to_integer = matches!(&d,
            ast::Expr::Literal(ast::Literal::Numeric(n))
                if n.parse::<f64>().is_ok_and(|v| v <= 0.0));
        let scale_num = call_fn("pow", vec![*numeric_expr("10"), d.clone()]);
        let scale_den = call_fn("pow", vec![*numeric_expr("10"), d]);
        let scaled = ast::Expr::binary(x, ast::Operator::Multiply, scale_num);
        let truncated = unary_fn("trunc", scaled);
        let result = float_division(truncated, scale_den);
        Ok(if to_integer {
            cast_to_integer(result)
        } else {
            result
        })
    }

    /// Parses MySQL `STRCMP(a, b)` (the name and `(` are already consumed),
    /// lowering it to a `CASE` that yields `-1` / `0` / `1` for `a < b` / `a = b`
    /// / `a > b`, and NULL when either argument is NULL. The comparison is taken
    /// under `COLLATE NOCASE` so it folds ASCII case like MySQL's default
    /// case-insensitive collation (`STRCMP('a', 'A')` is `0`, as in MySQL),
    /// matching the case-insensitive lowering of `INSTR`/`LOCATE`/`FIND_IN_SET`.
    fn strcmp_call(&mut self) -> Result<ast::Expr> {
        let a = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let b = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let either_null = ast::Expr::binary(
            ast::Expr::is_null(a.clone()),
            ast::Operator::Or,
            ast::Expr::is_null(b.clone()),
        );
        // An explicit `COLLATE NOCASE` on one operand makes the comparison
        // case-insensitive regardless of the operands' own collations.
        let a = ast::Expr::collate(a, ast::Name::from_string("NOCASE"));
        let less = ast::Expr::binary(a.clone(), ast::Operator::Less, b.clone());
        let greater = ast::Expr::binary(a, ast::Operator::Greater, b);
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![
                (
                    Box::new(either_null),
                    Box::new(ast::Expr::Literal(ast::Literal::Null)),
                ),
                (Box::new(less), numeric_expr("-1")),
                (Box::new(greater), numeric_expr("1")),
            ],
            else_expr: Some(numeric_expr("0")),
        })
    }

    /// Parses a single-argument base64 function (`TO_BASE64`/`FROM_BASE64`) — the
    /// name and `(` already consumed — and lowers it to the crypto extension's
    /// `engine_fn` with a `'base64'` format argument: `<engine_fn>(s, 'base64')`.
    /// For the encode direction (`cast_arg`), the argument is cast to text so a
    /// numeric argument encodes as its string form (`TO_BASE64(255)` is the
    /// base64 of `'255'`). A NULL argument yields NULL (the crypto functions
    /// error on NULL). Note `TO_BASE64` does not insert MySQL's 76-character line
    /// breaks, and `FROM_BASE64` errors on base64 that decodes to non-UTF-8 bytes
    /// (it returns text, not a binary string) — see `mysql/COMPAT.md`.
    fn base64_call(&mut self, engine_fn: &str, cast_arg: bool) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let payload = if cast_arg {
            ast::Expr::Cast {
                expr: Box::new(arg.clone()),
                type_name: Some(ast::Type {
                    name: "TEXT".to_string(),
                    size: None,
                    array_dimensions: 0,
                }),
            }
        } else {
            arg.clone()
        };
        let call = call_fn(
            engine_fn,
            vec![
                payload,
                ast::Expr::Literal(ast::Literal::String(requote("base64"))),
            ],
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(arg)),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(call)),
        })
    }

    /// Parses a single-argument hash function (`MD5`, `SHA1`/`SHA`) — the name
    /// and `(` already consumed — and lowers it to the lowercase hex digest of
    /// the crypto extension's `engine_fn` (see [`crypto_hex_digest`]).
    fn crypto_hash_call(&mut self, engine_fn: &str) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(crypto_hex_digest(engine_fn, arg))
    }

    /// Parses `SHA2(s, n)` (the name and `(` already consumed), where `n` selects
    /// the SHA-2 variant — 256, 384, or 512 (`0` is MySQL's alias for 256). It
    /// lowers to the lowercase hex digest of the matching crypto hash. `n` must
    /// be an integer literal; 224 has no engine hash and is rejected.
    fn sha2_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let bits = match self.expr()? {
            ast::Expr::Literal(ast::Literal::Numeric(n)) => n.parse::<i64>().map_err(|_| {
                ParseError::Unsupported("SHA2() length must be an integer literal".to_string())
            })?,
            _ => {
                return Err(ParseError::Unsupported(
                    "SHA2() length must be an integer literal".to_string(),
                ))
            }
        };
        self.expect(&Token::RParen, "`)`")?;
        let engine_fn = match bits {
            0 | 256 => "crypto_sha256",
            384 => "crypto_sha384",
            512 => "crypto_sha512",
            other => {
                return Err(ParseError::Unsupported(format!(
                    "SHA2() length {other} is not supported yet (only 256, 384, and 512)"
                )))
            }
        };
        Ok(crypto_hex_digest(engine_fn, arg))
    }

    /// Parses MySQL `OCT(n)` (the name and `(` are already consumed): the octal
    /// string of `n`, synthesized as `printf('%o', n)`. The engine's `printf`
    /// formats `%o` from the unsigned 64-bit value, so a negative `n` matches
    /// MySQL (`OCT(-8)` → `1777777777777777777770`). A NULL guard is needed since
    /// `printf('%o', NULL)` is `'0'`, not NULL.
    fn oct_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(n.clone())),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(call_fn(
                "printf",
                vec![ast::Expr::Literal(ast::Literal::String(requote("%o"))), n],
            ))),
        })
    }

    /// Parses MySQL `INTERVAL(n, n1, n2, ..., nk)` (the name and `(` are already
    /// consumed). It returns 0 if `n < n1`, 1 if `n < n2`, ..., `k` if `n >= nk`,
    /// i.e. how many of the (ascending) bounds `n` reaches or exceeds. This is
    /// the sum of the boolean comparisons `(n >= n1) + (n >= n2) + ... + (n >= nk)`,
    /// since the engine yields 1/0 for a comparison. MySQL returns -1 when `n` is
    /// NULL, so a NULL guard wraps the sum. At least one bound is required.
    fn interval_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let mut bounds = vec![self.expr()?];
        while self.eat(&Token::Comma) {
            bounds.push(self.expr()?);
        }
        self.expect(&Token::RParen, "`)`")?;
        // (n >= n1) + (n >= n2) + ... + (n >= nk)
        let mut sum: Option<ast::Expr> = None;
        for bound in bounds {
            let cmp = ast::Expr::binary(n.clone(), ast::Operator::GreaterEquals, bound);
            sum = Some(match sum {
                None => cmp,
                Some(acc) => ast::Expr::binary(acc, ast::Operator::Add, cmp),
            });
        }
        let sum = sum.expect("at least one bound");
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(n)),
                numeric_expr("-1"),
            )],
            else_expr: Some(Box::new(sum)),
        })
    }

    /// Parses MySQL `HEX(x)` (the name and `(` are already consumed). `HEX` is
    /// overloaded: for a number it is the uppercase hexadecimal of the value
    /// (`HEX(255)` → `FF`), and for a string it is the hex of the string's bytes
    /// (`HEX('A')` → `41`). The two cannot be told apart at parse time, so it
    /// dispatches on the runtime type: `printf('%X', x)` for an integer/real,
    /// else the engine's `hex(x)`, with a NULL guard (the engine's `hex(NULL)` is
    /// the empty string, not NULL).
    fn hex_call(&mut self) -> Result<ast::Expr> {
        let x = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let string_lit =
            |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
        let typeof_x = unary_fn("typeof", x.clone());
        let is_numeric = ast::Expr::binary(
            ast::Expr::binary(typeof_x.clone(), ast::Operator::Equals, string_lit("integer")),
            ast::Operator::Or,
            ast::Expr::binary(typeof_x, ast::Operator::Equals, string_lit("real")),
        );
        let numeric_hex = call_fn("printf", vec![string_lit("%X"), x.clone()]);
        let bytes_hex = unary_fn("hex", x.clone());

        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![
                (
                    Box::new(ast::Expr::is_null(x)),
                    Box::new(ast::Expr::Literal(ast::Literal::Null)),
                ),
                (Box::new(is_numeric), Box::new(numeric_hex)),
            ],
            else_expr: Some(Box::new(bytes_hex)),
        })
    }

    /// Parses MySQL `LOG(x)` or `LOG(b, x)` (the name and `(` are already
    /// consumed). The one-argument form is the **natural** log, so it lowers to
    /// the engine's `ln(x)` (the engine's own one-argument `log` is base-10). The
    /// two-argument `LOG(b, x)` is the base-`b` logarithm, identical on both, so
    /// it passes through as `log(b, x)`.
    fn log_call(&mut self) -> Result<ast::Expr> {
        let first = self.expr()?;
        if self.eat(&Token::Comma) {
            let x = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            Ok(call_fn("log", vec![first, x]))
        } else {
            self.expect(&Token::RParen, "`)`")?;
            Ok(unary_fn("ln", first))
        }
    }

    /// Parses MySQL `ATAN(x)` or `ATAN(y, x)` (the name and `(` are already
    /// consumed). The one-argument form is the arctangent — the engine's `atan`.
    /// The two-argument `ATAN(y, x)` is a MySQL synonym for `ATAN2(y, x)`, which
    /// the engine spells `atan2`.
    fn atan_call(&mut self) -> Result<ast::Expr> {
        let first = self.expr()?;
        if self.eat(&Token::Comma) {
            let x = self.expr()?;
            self.expect(&Token::RParen, "`)`")?;
            Ok(call_fn("atan2", vec![first, x]))
        } else {
            self.expect(&Token::RParen, "`)`")?;
            Ok(unary_fn("atan", first))
        }
    }

    /// Parses MySQL `COT(x)` (the name and `(` are already consumed) and lowers
    /// the cotangent to `1 / tan(x)` — the engine has `tan` but no `cot`. The
    /// engine's real division yields a float (the literal `1` is promoted), and a
    /// NULL argument makes `tan(x)` NULL, so the quotient is NULL as in MySQL.
    /// (`COT(0)` divides by zero; MySQL raises an out-of-range error there, while
    /// the engine yields its own division-by-zero result — an edge that is not
    /// reached in practice.)
    fn cot_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::binary(
            ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
            ast::Operator::Divide,
            unary_fn("tan", arg),
        ))
    }

    /// Parses a `LEFT(str, len)` call (the name and `(` are already consumed)
    /// and lowers it to the engine's `substr(str, 1, len)`, which returns the
    /// same leftmost `len` characters and propagates NULL the same way. Exactly
    /// two arguments are required.
    fn left_call(&mut self) -> Result<ast::Expr> {
        let str_arg = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let len_arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("substr"),
            distinctness: None,
            args: vec![
                Box::new(str_arg),
                Box::new(ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))),
                // MySQL rounds a fractional length (`LEFT('abcd', 2.9)` → `abc`).
                Box::new(integer_arg(len_arg)),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses a `RIGHT(str, len)` call (the name and `(` are already consumed)
    /// and lowers it to the engine's `substr(str, -len, len)` — a negative start
    /// counts `len` characters from the end. This reproduces MySQL's rightmost-
    /// `len` semantics across the edge cases: `len` past the string length yields
    /// the whole string (the start clamps to the front), `len` of zero yields the
    /// empty string, and NULL propagates. Exactly two arguments are required.
    fn right_call(&mut self) -> Result<ast::Expr> {
        let str_arg = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let len_arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        // MySQL rounds a fractional length (`RIGHT('abcd', 2.9)` → `bcd`).
        let len = integer_arg(len_arg);
        // `-len`, built as `0 - len` to avoid a unary-minus node.
        let neg_len = ast::Expr::binary(
            ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
            ast::Operator::Subtract,
            len.clone(),
        );
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("substr"),
            distinctness: None,
            args: vec![Box::new(str_arg), Box::new(neg_len), Box::new(len)],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses the single argument of a `LENGTH(x)` call (the name and `(` are
    /// already consumed) and lowers it to `length(CAST(x AS BLOB))`. MySQL's
    /// `LENGTH` is a byte count; the engine's `length()` counts characters, but
    /// `length()` of a BLOB counts bytes, and casting to BLOB yields the value's
    /// UTF-8 byte sequence. `CHAR_LENGTH` (the character count) maps to the
    /// engine's `length()` directly elsewhere.
    fn length_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(byte_length_expr(arg))
    }

    /// Parses a `TRIM(...)` call (the name and `(` are already consumed) in any
    /// of its MySQL forms and lowers it to the engine's `trim`/`ltrim`/`rtrim`:
    ///
    ///   - `TRIM(str)` / `TRIM([BOTH] FROM str)`   → `trim(str)`
    ///   - `TRIM(LEADING FROM str)`                → `ltrim(str)`
    ///   - `TRIM(TRAILING FROM str)`               → `rtrim(str)`
    ///   - `TRIM([BOTH] remstr FROM str)`          → `trim(str, remstr)`
    ///   - `TRIM(LEADING remstr FROM str)`         → `ltrim(str, remstr)`
    ///   - `TRIM(TRAILING remstr FROM str)`        → `rtrim(str, remstr)`
    ///
    /// The engine's two-argument trim removes any of the *characters* in `remstr`
    /// from the end(s); this matches MySQL only when `remstr` is a single
    /// character (or the default space). For a multi-character `remstr` MySQL
    /// strips the whole substring instead — a documented divergence (see
    /// `mysql/COMPAT.md`). NULL propagates. A direction keyword requires `FROM`.
    fn trim_call(&mut self) -> Result<ast::Expr> {
        let (engine_fn, has_direction) = if self.eat_keyword("LEADING") {
            ("ltrim", true)
        } else if self.eat_keyword("TRAILING") {
            ("rtrim", true)
        } else if self.eat_keyword("BOTH") {
            ("trim", true)
        } else {
            ("trim", false)
        };

        let (remstr, target) = if self.eat_keyword("FROM") {
            // `[direction] FROM str` — no remove-string.
            (None, self.expr()?)
        } else {
            let first = self.expr()?;
            if self.eat_keyword("FROM") {
                // `[direction] remstr FROM str`.
                (Some(first), self.expr()?)
            } else if has_direction {
                // `TRIM(LEADING str)` without `FROM` is not valid MySQL.
                return Err(self.unexpected("`FROM` in TRIM(...)"));
            } else {
                // The bare `TRIM(str)` form.
                (None, first)
            }
        };
        self.expect(&Token::RParen, "`)`")?;

        let args = match remstr {
            Some(remstr) => vec![Box::new(target), Box::new(remstr)],
            None => vec![Box::new(target)],
        };
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string(engine_fn),
            distinctness: None,
            args,
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Lowers `OCTET_LENGTH(x)` (the name and `(` are already consumed). In MySQL
    /// `OCTET_LENGTH` is a synonym for `LENGTH` — the byte count — so it shares
    /// the exact lowering. Exactly one argument is required.
    fn octet_length_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(byte_length_expr(arg))
    }

    /// Lowers `BIT_LENGTH(x)` (the name and `(` are already consumed) to
    /// `8 * length(CAST(x AS BLOB))` — MySQL's `BIT_LENGTH` is the byte length
    /// times eight. NULL propagates through the multiplication. Exactly one
    /// argument is required.
    fn bit_length_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::binary(
            ast::Expr::Literal(ast::Literal::Numeric("8".to_string())),
            ast::Operator::Multiply,
            byte_length_expr(arg),
        ))
    }

    /// Parses the single argument of a date-part extractor such as `YEAR(x)`
    /// (the name and `(` are already consumed) and lowers it to
    /// `CAST(strftime(fmt, x) AS INTEGER)`. The cast drops `strftime`'s
    /// zero-padding and string type so the result is an integer like MySQL's
    /// (e.g. `MONTH('2020-03-15')` is `3`, not `'03'`). NULL propagates.
    fn date_part_call(&mut self, fmt: &str) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(cast_strftime_int(fmt, arg))
    }

    /// Parses a SQL-standard `EXTRACT(unit FROM expr)` call (the name and `(` are
    /// already consumed) and lowers it like the matching date-part function. The
    /// single calendar units map to a `CAST(strftime(code, expr) AS INTEGER)`;
    /// `WEEK` uses the default Sunday-first mode (`strftime('%U')`, like `WEEK(d)`)
    /// and `QUARTER` is `(month + 2) / 3` (like `QUARTER(d)`). `MICROSECOND` and
    /// the compound units (`YEAR_MONTH`, `DAY_HOUR`, …) are rejected.
    fn extract_call(&mut self) -> Result<ast::Expr> {
        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("an EXTRACT unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();
        self.expect_keyword("FROM")?;
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let fmt = match unit.as_str() {
            "YEAR" => "%Y",
            "MONTH" => "%m",
            "DAY" => "%d",
            "HOUR" => "%H",
            "MINUTE" => "%M",
            "SECOND" => "%S",
            // The default week mode (0, Sunday-first) is strftime `%U`.
            "WEEK" => "%U",
            // `QUARTER` is `(month + 2) / 3`, like `QUARTER(d)`.
            "QUARTER" => {
                let month_plus_two = ast::Expr::binary(
                    cast_strftime_int("%m", arg),
                    ast::Operator::Add,
                    ast::Expr::Literal(ast::Literal::Numeric("2".to_string())),
                );
                return Ok(ast::Expr::binary(
                    month_plus_two,
                    ast::Operator::Divide,
                    ast::Expr::Literal(ast::Literal::Numeric("3".to_string())),
                ));
            }
            // Compound units combine their fields into one integer (e.g.
            // `YEAR_MONTH` is `year*100 + month`, `DAY_SECOND` is
            // `day*1000000 + hour*10000 + minute*100 + second`). Verified against
            // MySQL 8.4. The `*_MICROSECOND` units are omitted (the engine's
            // strftime has only millisecond, not microsecond, precision).
            "YEAR_MONTH" => return Ok(extract_compound(arg, &[("%Y", 100), ("%m", 1)])),
            "DAY_HOUR" => return Ok(extract_compound(arg, &[("%d", 100), ("%H", 1)])),
            "DAY_MINUTE" => {
                return Ok(extract_compound(
                    arg,
                    &[("%d", 10000), ("%H", 100), ("%M", 1)],
                ))
            }
            "DAY_SECOND" => {
                return Ok(extract_compound(
                    arg,
                    &[("%d", 1000000), ("%H", 10000), ("%M", 100), ("%S", 1)],
                ))
            }
            "HOUR_MINUTE" => return Ok(extract_compound(arg, &[("%H", 100), ("%M", 1)])),
            "HOUR_SECOND" => {
                return Ok(extract_compound(
                    arg,
                    &[("%H", 10000), ("%M", 100), ("%S", 1)],
                ))
            }
            "MINUTE_SECOND" => return Ok(extract_compound(arg, &[("%M", 100), ("%S", 1)])),
            other => {
                return Err(ParseError::Unsupported(format!(
                    "EXTRACT({other} FROM ...) is not supported yet"
                )))
            }
        };
        Ok(cast_strftime_int(fmt, arg))
    }

    /// Lowers `QUARTER(d)` (the name and `(` are already consumed) to
    /// `(CAST(strftime('%m', d) AS INTEGER) + 2) / 3`. Integer division maps each
    /// month to its 1–4 quarter (months 1–3 → 1, 4–6 → 2, …); NULL propagates.
    /// Exactly one argument is required.
    fn quarter_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let month_plus_two = ast::Expr::binary(
            cast_strftime_int("%m", arg),
            ast::Operator::Add,
            ast::Expr::Literal(ast::Literal::Numeric("2".to_string())),
        );
        Ok(ast::Expr::binary(
            month_plus_two,
            ast::Operator::Divide,
            ast::Expr::Literal(ast::Literal::Numeric("3".to_string())),
        ))
    }

    /// Parses a `DAYNAME(d)` call (the name and `(` are already consumed) and
    /// lowers it to the English weekday name via [`name_from_date`] over
    /// `strftime('%w', d)` (0=Sunday..6=Saturday). NULL propagates. Exactly one
    /// argument is required.
    fn dayname_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(name_from_date("%w", &WEEKDAY_NAMES, 0, arg))
    }

    /// Parses a `MONTHNAME(d)` call (the name and `(` are already consumed) and
    /// lowers it to the English month name via [`name_from_date`] over
    /// `strftime('%m', d)` (01..12). NULL propagates. Exactly one argument is
    /// required.
    fn monthname_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(name_from_date("%m", &MONTH_NAMES, 1, arg))
    }

    /// Parses a `DAYOFWEEK(d)` / `WEEKDAY(d)` call (the name and `(` are already
    /// consumed) and lowers it to integer arithmetic over the engine's
    /// `strftime('%w', d)`, which yields 0=Sunday..6=Saturday. The result is
    /// `(strftime('%w', d) + add)`, optionally taken `% 7`: `DAYOFWEEK` uses
    /// `add = 1` (1=Sunday..7=Saturday) and `WEEKDAY` uses `add = 6` with the
    /// modulo (0=Monday..6=Sunday). NULL propagates through both.
    fn day_of_week_call(&mut self, add: i64, modulo: bool) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let strftime = ast::Expr::FunctionCall {
            name: ast::Name::from_string("strftime"),
            distinctness: None,
            args: vec![
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("%w")))),
                Box::new(arg),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        };
        let dow = ast::Expr::Cast {
            expr: Box::new(strftime),
            type_name: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            }),
        };
        let shifted = ast::Expr::binary(
            dow,
            ast::Operator::Add,
            ast::Expr::Literal(ast::Literal::Numeric(add.to_string())),
        );
        Ok(if modulo {
            ast::Expr::binary(
                shifted,
                ast::Operator::Modulus,
                ast::Expr::Literal(ast::Literal::Numeric("7".to_string())),
            )
        } else {
            shifted
        })
    }

    /// Parses `WEEK(d[, mode])` (the name and `(` are already consumed) and
    /// lowers it to a `strftime`-based week number. MySQL's week `mode` (default
    /// `0`, MySQL's `default_week_format`) selects among eight numbering schemes.
    /// Three map directly to an engine strftime format:
    ///   - mode 0 → `%U` (Sunday-first, 0–53, week 1 = first week with a Sunday),
    ///   - mode 3 → `%V` (ISO 8601, Monday-first, 1–53),
    ///   - mode 5 → `%W` (Monday-first, 0–53, week 1 = first week with a Monday).
    ///
    /// Mode 1 (Monday-first, 0–53, week 1 = first week with more than three days
    /// this year — which WordPress's `WP_Date_Query` uses when the week starts on
    /// Monday) has no single strftime code, but it equals the ISO week (`%V`)
    /// except in the partial week at a year boundary: an early-January date whose
    /// ISO week belongs to the previous ISO year (`%G` < `%Y`) is week 0, and a
    /// late-December date whose ISO week belongs to the next ISO year (`%G` >
    /// `%Y`) is week 53. This identity was verified against MySQL 8.4 for every
    /// day from 2016 to 2031.
    ///
    /// Modes 2, 4, and 7 are built from those codes with adjustments (see the
    /// inline comments and [`week_push_zero`]): 2 and 7 are the 1–53 siblings of
    /// 0 and 5, and 4 is the Sunday-first "4 or more days" rule. Only mode 6 (the
    /// Sunday-first 1–53 "4 or more days" rule) has no engine equivalent and is
    /// rejected. The `mode` must be an integer literal.
    fn week_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        let mode = if self.eat(&Token::Comma) {
            match self.expr()? {
                ast::Expr::Literal(ast::Literal::Numeric(n)) => n.parse::<i64>().map_err(|_| {
                    ParseError::Unsupported("WEEK() mode must be an integer literal".to_string())
                })?,
                _ => {
                    return Err(ParseError::Unsupported(
                        "WEEK() mode must be an integer literal".to_string(),
                    ))
                }
            }
        } else {
            0
        };
        self.expect(&Token::RParen, "`)`")?;

        // Mode 1: ISO week (`%V`), but 0 in a previous-year partial week and 53
        // in a next-year partial week (see the doc comment).
        if mode == 1 {
            let iso_year_below = ast::Expr::binary(
                strftime_int("%G", arg.clone()),
                ast::Operator::Less,
                strftime_int("%Y", arg.clone()),
            );
            let iso_year_above = ast::Expr::binary(
                strftime_int("%G", arg.clone()),
                ast::Operator::Greater,
                strftime_int("%Y", arg.clone()),
            );
            return Ok(ast::Expr::Case {
                base: None,
                when_then_pairs: vec![
                    (Box::new(iso_year_below), numeric_expr("0")),
                    (Box::new(iso_year_above), numeric_expr("53")),
                ],
                else_expr: Some(Box::new(strftime_int("%V", arg))),
            });
        }

        // Modes 2 and 7 are the 1–53 ("week year") siblings of modes 0 (`%U`,
        // Sunday-first) and 5 (`%W`, Monday-first): identical except a date in the
        // year's leading partial week — which the 0–53 mode numbers 0 — is instead
        // numbered as the previous year's last week (see `week_push_zero`).
        if mode == 2 {
            return Ok(week_push_zero(arg, "%U"));
        }
        if mode == 7 {
            return Ok(week_push_zero(arg, "%W"));
        }

        // Mode 4 is Sunday-first, 0–53, with MySQL's "4 or more days" rule for week
        // 1 (the week whose Wednesday — its fourth day — falls in this year), rather
        // than `%U`'s "first week with a Sunday". The two number Sunday-weeks
        // identically apart from a constant per-year offset: when January 1 is a
        // Monday, Tuesday, or Wednesday, that week's Wednesday is already in the
        // year, so the leading partial week is week 1 (not `%U`'s week 0) and every
        // week shifts up by one. Verified against MySQL 8.4 for every day of
        // 2018–2027 and every year-boundary week of 2000–2040.
        if mode == 4 {
            let jan1_wday = strftime_int(
                "%w",
                call_fn(
                    "date",
                    vec![
                        arg.clone(),
                        ast::Expr::Literal(ast::Literal::String(requote("start of year"))),
                    ],
                ),
            );
            // 1 when January 1 is Mon/Tue/Wed (`%w` 1..=3), else 0.
            let leads_year = ast::Expr::binary(
                ast::Expr::binary(jan1_wday.clone(), ast::Operator::Greater, *numeric_expr("0")),
                ast::Operator::And,
                ast::Expr::binary(jan1_wday, ast::Operator::Less, *numeric_expr("4")),
            );
            let offset = ast::Expr::Case {
                base: None,
                when_then_pairs: vec![(Box::new(leads_year), numeric_expr("1"))],
                else_expr: Some(numeric_expr("0")),
            };
            return Ok(ast::Expr::binary(
                strftime_int("%U", arg),
                ast::Operator::Add,
                offset,
            ));
        }

        let fmt = match mode {
            0 => "%U",
            3 => "%V",
            5 => "%W",
            other => {
                return Err(ParseError::Unsupported(format!(
                    "WEEK() mode {other} is not supported yet \
                     (only modes 0–5 and 7 map to an engine week number; mode 6 \
                     has no engine equivalent)"
                )))
            }
        };
        Ok(strftime_int(fmt, arg))
    }

    /// Lowers `YEARWEEK(d[, mode])` (the name and `(` are already consumed) to the
    /// `year * 100 + week` value MySQL returns, where the year is the one that
    /// *owns* the week (which differs from the calendar year for a week that
    /// straddles a year boundary). The supported modes mirror [`Self::week_call`].
    ///
    /// Modes 1 and 3 are the ISO year-week: `strftime('%G', d) * 100 +
    /// strftime('%V', d)`. ISO `%G` already attributes a straddling week to the
    /// right year, and YEARWEEK never has a week 0, so the two modes coincide.
    ///
    /// Modes 0 (Sunday weeks, `%U`) and 5 (Monday weeks, `%W`) number within the
    /// calendar year. YEARWEEK pushes a date in "week 0" — before the year's first
    /// numbered week — into the previous year's last week: when the week number is
    /// 0, the result is `(year - 1) * 100 + <that week's number>`, taken as the
    /// week number of the previous year's last day (`date(d, 'start of year',
    /// '-1 day')`). A NULL argument propagates.
    fn yearweek_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        let mode = if self.eat(&Token::Comma) {
            match self.expr()? {
                ast::Expr::Literal(ast::Literal::Numeric(n)) => n.parse::<i64>().map_err(|_| {
                    ParseError::Unsupported("YEARWEEK() mode must be an integer literal".to_string())
                })?,
                _ => {
                    return Err(ParseError::Unsupported(
                        "YEARWEEK() mode must be an integer literal".to_string(),
                    ))
                }
            }
        } else {
            0
        };
        self.expect(&Token::RParen, "`)`")?;

        if mode == 1 || mode == 3 {
            let year = ast::Expr::binary(
                strftime_int("%G", arg.clone()),
                ast::Operator::Multiply,
                *numeric_expr("100"),
            );
            return Ok(ast::Expr::binary(
                year,
                ast::Operator::Add,
                strftime_int("%V", arg),
            ));
        }

        let code = match mode {
            0 => "%U",
            5 => "%W",
            other => {
                return Err(ParseError::Unsupported(format!(
                    "YEARWEEK() mode {other} is not supported yet \
                     (only modes 0, 1, 3, and 5 map to an engine week number)"
                )))
            }
        };
        let year = strftime_int("%Y", arg.clone());
        let week = strftime_int(code, arg.clone());
        let prev_year_end = call_fn(
            "date",
            vec![
                arg,
                ast::Expr::Literal(ast::Literal::String(requote("start of year"))),
                ast::Expr::Literal(ast::Literal::String(requote("-1 day"))),
            ],
        );
        let prev_week = strftime_int(code, prev_year_end);
        let this_yw = ast::Expr::binary(
            ast::Expr::binary(year.clone(), ast::Operator::Multiply, *numeric_expr("100")),
            ast::Operator::Add,
            week.clone(),
        );
        let prev_yw = ast::Expr::binary(
            ast::Expr::binary(
                ast::Expr::binary(year, ast::Operator::Subtract, *numeric_expr("1")),
                ast::Operator::Multiply,
                *numeric_expr("100"),
            ),
            ast::Operator::Add,
            prev_week,
        );
        let is_week_zero = ast::Expr::binary(week, ast::Operator::Equals, *numeric_expr("0"));
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(Box::new(is_week_zero), Box::new(prev_yw))],
            else_expr: Some(Box::new(this_yw)),
        })
    }

    /// Parses `DATE_ADD(x, INTERVAL n unit)` (or `DATE_SUB`, when `subtract` is
    /// true) — the name and `(` are already consumed — and lowers it to the
    /// engine's `datetime(x, '<signed-n> <unit>')` modifier. The interval value
    /// may be an integer literal or a quoted numeric string (`INTERVAL '30'
    /// SECOND`, which WordPress emits and MySQL coerces); `WEEK` is expanded to
    /// days. `datetime()` returns `'YYYY-MM-DD HH:MM:SS'`, matching MySQL's
    /// result for a DATETIME argument.
    fn date_add_call(&mut self, subtract: bool) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        self.expect_keyword("INTERVAL")?;
        let result = self.apply_interval(target, subtract)?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(result)
    }

    /// Lowers `ADDDATE`/`SUBDATE` (the name and `(` are already consumed);
    /// `subtract` is true for `SUBDATE`. Two forms:
    ///
    ///   - `ADDDATE(d, INTERVAL n unit)` is exactly `DATE_ADD`/`DATE_SUB`, so it
    ///     reuses [`Self::apply_interval`].
    ///   - `ADDDATE(d, n)` adds (or subtracts) `n` whole days, lowered to
    ///     `datetime(d, printf('%+d days', ±n))`. The `printf` gives the modifier
    ///     its explicit sign so a negative amount stays valid.
    ///
    /// As with `DATE_ADD`, on a bare DATE value MySQL returns a DATE while the
    /// engine keeps the time part — a documented divergence (see `mysql/COMPAT.md`).
    fn adddate_call(&mut self, subtract: bool) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;

        if self.eat_keyword("INTERVAL") {
            let result = self.apply_interval(target, subtract)?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(result);
        }

        let days = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        // SUBDATE negates the amount so the shared `+n days` modifier subtracts.
        let amount = if subtract {
            ast::Expr::binary(
                ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
                ast::Operator::Subtract,
                days,
            )
        } else {
            days
        };
        let modifier = ast::Expr::FunctionCall {
            name: ast::Name::from_string("printf"),
            distinctness: None,
            args: vec![
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(
                    "%+d days",
                )))),
                Box::new(amount.clone()),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        };
        let shifted = ast::Expr::FunctionCall {
            name: ast::Name::from_string("datetime"),
            distinctness: None,
            args: vec![Box::new(target), Box::new(modifier)],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        };
        // `printf` treats a NULL amount as 0, so guard a NULL days count back to
        // NULL (MySQL propagates it); a NULL target is already handled by
        // `datetime` returning NULL.
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::is_null(amount)),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(shifted)),
        })
    }

    /// Parses an interval `[-]value unit` (the `INTERVAL` keyword already
    /// consumed) and lowers `target ± interval` to the engine's
    /// `datetime(target, '<signed-amount> <engine-unit>')` modifier. Shared by
    /// `DATE_ADD`/`DATE_SUB` and the `+`/`-` `INTERVAL` arithmetic operators;
    /// `subtract` is true for `DATE_SUB` and the `-` operator. The value may be a
    /// number or a quoted numeric string (which MySQL coerces); `WEEK` is
    /// expanded to days, and other units are rejected.
    fn apply_interval(&mut self, target: ast::Expr, subtract: bool) -> Result<ast::Expr> {
        let spec = self.parse_interval_spec()?;
        build_interval(target, &spec, subtract)
    }

    /// Parses an interval `[-]value unit` (the `INTERVAL` keyword already
    /// consumed) into an [`IntervalSpec`], without applying it to a target — so
    /// the spec can be applied to a target that appears either after the interval
    /// (the prefix `INTERVAL n unit + date` form) or before it (the postfix
    /// `date + INTERVAL n unit` form). The value may be a number or a quoted
    /// numeric string (which MySQL coerces).
    fn parse_interval_spec(&mut self) -> Result<IntervalSpec> {
        let negative = self.eat(&Token::Minus);
        let raw = match self.peek() {
            Some(Token::Num(n) | Token::Str(n)) => n.clone(),
            _ => return Err(self.unexpected("an integer interval value")),
        };
        self.advance();

        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("an interval unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();

        Ok(IntervalSpec {
            negative,
            raw,
            unit,
        })
    }

    /// Parses `DATE_FORMAT(x, 'fmt')` / `TIME_FORMAT(x, 'fmt')` (the name in
    /// `fn_name`, and the name and `(` already consumed) and lowers it via
    /// [`date_format_expr`] — a `strftime` over `x` for the directly-translatable
    /// specifiers, with month/weekday name specifiers expanded to `CASE` lookups
    /// and concatenated. The format must be a string literal so it can be
    /// translated at parse time.
    ///
    /// `TIME_FORMAT` shares this lowering: for a time-only format (`%H %i %s %h
    /// %p %k %T` …) it produces exactly MySQL's result, since those specifiers
    /// read only the time part. (MySQL's `TIME_FORMAT` returns NULL for a *date*
    /// specifier whereas this evaluates it — an invalid-usage edge documented in
    /// `mysql/COMPAT.md`.)
    fn format_call(&mut self, fn_name: &str) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let Some(Token::Str(fmt)) = self.peek() else {
            return Err(self.unexpected(&format!("a string-literal {fn_name} format")));
        };
        let fmt = fmt.clone();
        self.advance();
        self.expect(&Token::RParen, "`)`")?;

        date_format_expr(&fmt, target)
    }

    /// Lowers a MySQL current date/time function (`NOW()`, `CURDATE()`, ...) to
    /// the engine's `datetime('now')` / `date('now')` / `time('now')`. The name
    /// and `(` are already consumed; these forms take no arguments here. The
    /// engine has no session time zone, so the result is UTC (a documented
    /// divergence from MySQL's session-local `NOW()`; see `mysql/COMPAT.md`).
    fn current_time_call(&mut self, engine_fn: &'static str) -> Result<ast::Expr> {
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string(engine_fn),
            distinctness: None,
            args: vec![Box::new(ast::Expr::Literal(ast::Literal::String(requote(
                "now",
            ))))],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Lowers `DATEDIFF(a, b)` (the name and `(` are already consumed) to
    /// `CAST(julianday(date(a)) - julianday(date(b)) AS INTEGER)`. MySQL's
    /// `DATEDIFF` is the whole-day count `a - b` using only the date parts, so
    /// each operand is reduced to its date with `date()` before taking the
    /// Julian-day difference; both dates land on midnight, so the difference is
    /// an exact integer. NULL propagates. Exactly two arguments are required.
    fn datediff_call(&mut self) -> Result<ast::Expr> {
        let a = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let b = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let diff = ast::Expr::binary(
            unary_fn("julianday", unary_fn("date", a)),
            ast::Operator::Subtract,
            unary_fn("julianday", unary_fn("date", b)),
        );
        Ok(ast::Expr::Cast {
            expr: Box::new(diff),
            type_name: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            }),
        })
    }

    /// Lowers `TO_DAYS(d)` (the name and `(` are already consumed) to the MySQL
    /// day number — days since year 0 — as `CAST(julianday(date(d)) AS INTEGER)`
    /// minus the constant `1721059`. The `date()` wrapper drops any time part (so
    /// it stays the whole-day count regardless of the hour), and the offset shifts
    /// the engine's Julian day onto MySQL's proleptic-Gregorian day count. NULL
    /// propagates. (Like MySQL, only modern Gregorian dates are meaningful.)
    /// Exactly one argument is required.
    #[allow(clippy::wrong_self_convention)]
    fn to_days_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(to_days_expr(arg))
    }

    /// Lowers `TO_SECONDS(d)` (the name and `(` are already consumed) to the
    /// seconds from year 0 to `d`: `TO_DAYS(d) * 86400 + TIME_TO_SEC(d)`. It
    /// reuses the day-number and time-of-day lowerings, so it carries their
    /// divergences (only modern Gregorian dates and a normal time-of-day range
    /// are meaningful). NULL propagates through both terms. Exactly one argument
    /// is required.
    #[allow(clippy::wrong_self_convention)]
    fn to_seconds_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let days = ast::Expr::binary(
            to_days_expr(arg.clone()),
            ast::Operator::Multiply,
            ast::Expr::Literal(ast::Literal::Numeric("86400".to_string())),
        );
        Ok(ast::Expr::binary(
            days,
            ast::Operator::Add,
            time_to_sec_expr(arg),
        ))
    }

    /// Lowers `PERIOD_DIFF(p1, p2)` (the name and `(` are already consumed) to
    /// the number of months between the two periods, `months(p1) - months(p2)`,
    /// where `months` turns a `YYYYMM`/`YYMM` period into an absolute month count
    /// (see [`period_to_months`]). NULL propagates.
    fn period_diff_call(&mut self) -> Result<ast::Expr> {
        let p1 = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let p2 = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::binary(
            period_to_months(p1),
            ast::Operator::Subtract,
            period_to_months(p2),
        ))
    }

    /// Lowers `PERIOD_ADD(p, n)` (the name and `(` are already consumed) to the
    /// `YYYYMM` period `n` months after `p`. The absolute month count
    /// `total = months(p) + n` (1-based month) is converted back to a period:
    /// `((total - 1) / 12) * 100 + ((total - 1) % 12 + 1)`. The integer division
    /// and remainder assume `total > 0` (always true for real periods); a NULL
    /// argument propagates.
    fn period_add_call(&mut self) -> Result<ast::Expr> {
        let p = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
        let twelve = || ast::Expr::Literal(ast::Literal::Numeric("12".to_string()));
        // total = months(p) + n; then total - 1 splits cleanly into (year, month).
        let total = ast::Expr::binary(period_to_months(p), ast::Operator::Add, n);
        let total_m1 = ast::Expr::binary(total, ast::Operator::Subtract, one());
        let year = ast::Expr::binary(total_m1.clone(), ast::Operator::Divide, twelve());
        let year_part = ast::Expr::binary(
            year,
            ast::Operator::Multiply,
            ast::Expr::Literal(ast::Literal::Numeric("100".to_string())),
        );
        let month_part = ast::Expr::binary(
            ast::Expr::binary(total_m1, ast::Operator::Modulus, twelve()),
            ast::Operator::Add,
            one(),
        );
        Ok(ast::Expr::binary(year_part, ast::Operator::Add, month_part))
    }

    /// Lowers `FROM_DAYS(n)` (the name and `(` are already consumed) — the inverse
    /// of `TO_DAYS` — to `date(n + 1721059.5)`. Adding the offset (with the `.5`
    /// for the midnight-vs-noon Julian convention) turns the day number back into
    /// a Julian day, which `date()` renders as `'YYYY-MM-DD'`. NULL propagates.
    /// Exactly one argument is required.
    #[allow(clippy::wrong_self_convention)]
    fn from_days_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let julian = ast::Expr::binary(
            arg,
            ast::Operator::Add,
            ast::Expr::Literal(ast::Literal::Numeric("1721059.5".to_string())),
        );
        Ok(unary_fn("date", julian))
    }

    /// Lowers `TIME_TO_SEC(t)` (the name and `(` are already consumed) to the
    /// seconds since midnight of `t`'s time part:
    /// `H*3600 + M*60 + S`, each part being `CAST(strftime(code, t) AS INTEGER)`.
    /// NULL propagates. MySQL `TIME` values outside `00:00:00`..`23:59:59` (it
    /// allows up to 838 hours and negatives) wrap or fail in the engine, so only
    /// the normal time-of-day range matches — a documented divergence. Exactly one
    /// argument is required.
    fn time_to_sec_call(&mut self) -> Result<ast::Expr> {
        let t = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(time_to_sec_expr(t))
    }

    /// Lowers `SEC_TO_TIME(s)` (the name and `(` are already consumed) to
    /// `time(s, 'unixepoch')` — the `'HH:MM:SS'` time `s` seconds after midnight.
    /// NULL propagates. The engine wraps at 24 hours, so only `0`..`86399` matches
    /// MySQL (which would render e.g. `25:00:00`) — a documented divergence.
    /// Exactly one argument is required.
    fn sec_to_time_call(&mut self) -> Result<ast::Expr> {
        let s = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("time"),
            distinctness: None,
            args: vec![
                Box::new(s),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(
                    "unixepoch",
                )))),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Lowers `MOD(a, b)` (the name and `(` are already consumed) to the same
    /// `a - b * CAST(a / b AS INTEGER)` remainder as the `a MOD b` operator,
    /// which takes the sign of `a` and is exact for float operands. NULL
    /// propagates. Exactly two arguments are required.
    fn mod_call(&mut self) -> Result<ast::Expr> {
        let a = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let b = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(modulo(a, b))
    }

    /// Lowers `REPEAT(s, n)` (the name and `(` are already consumed) via
    /// [`repeat_expr`]. Exactly two arguments are required.
    fn repeat_call(&mut self) -> Result<ast::Expr> {
        let s = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        // MySQL rounds a fractional count (`REPEAT('x', 2.9)` → `xxx`).
        Ok(repeat_expr(s, integer_arg(n)))
    }

    /// Lowers `SPACE(n)` (the name and `(` are already consumed) to `REPEAT(' ',
    /// n)` via [`repeat_expr`] — a string of `n` spaces, the empty string for a
    /// non-positive `n`, and NULL for a NULL `n`, matching MySQL. Exactly one
    /// argument is required.
    fn space_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let space = ast::Expr::Literal(ast::Literal::String(requote(" ")));
        // MySQL rounds a fractional count (`SPACE(2.9)` is three spaces).
        Ok(repeat_expr(space, integer_arg(n)))
    }

    /// Lowers `INSERT(str, pos, len, newstr)` (the name and `(` are already
    /// consumed) — replace `len` characters of `str` starting at the 1-based
    /// `pos` with `newstr` — to
    /// `CASE WHEN pos < 1 OR pos > length(str) THEN str
    ///       ELSE substr(str, 1, pos - 1) || newstr || substr(str, pos + len) END`.
    ///
    /// The guard returns `str` unchanged when `pos` is out of range, as in MySQL;
    /// otherwise the prefix before `pos`, `newstr`, and the suffix from `pos+len`
    /// are concatenated (a `len` past the end simply yields an empty suffix). The
    /// engine's `length`/`substr` are character-based, matching MySQL's
    /// per-character positions, and a NULL in any argument falls through the
    /// guard and propagates via the concatenation. A negative `len` is a
    /// documented edge. Exactly four arguments are required.
    fn insert_string_call(&mut self) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let pos = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let len = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let newstr = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        // MySQL rounds a fractional position/length to an integer.
        let pos = integer_arg(pos);
        let len = integer_arg(len);

        let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
        // pos < 1 OR pos > length(str)
        let cond = ast::Expr::binary(
            ast::Expr::binary(pos.clone(), ast::Operator::Less, one()),
            ast::Operator::Or,
            ast::Expr::binary(
                pos.clone(),
                ast::Operator::Greater,
                call_fn("length", vec![target.clone()]),
            ),
        );
        let prefix = substr_fn(
            target.clone(),
            one(),
            ast::Expr::binary(pos.clone(), ast::Operator::Subtract, one()),
        );
        let suffix = call_fn(
            "substr",
            vec![
                target.clone(),
                ast::Expr::binary(pos, ast::Operator::Add, len),
            ],
        );
        let spliced = ast::Expr::binary(
            ast::Expr::binary(prefix, ast::Operator::Concat, newstr),
            ast::Operator::Concat,
            suffix,
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(Box::new(cond), Box::new(target))],
            else_expr: Some(Box::new(spliced)),
        })
    }

    /// Lowers `LPAD(str, len, pad)` / `RPAD(str, len, pad)` (the name and `(` are
    /// already consumed) via [`pad_expr`]; `left` is true for `LPAD`. Exactly
    /// three arguments are required.
    fn pad_call(&mut self, left: bool) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let len = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let pad = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(pad_expr(left, target, len, pad))
    }

    /// Lowers `LAST_DAY(d)` (the name and `(` are already consumed) to
    /// `date(d, 'start of month', '+1 month', '-1 day')`, which the engine's
    /// date modifiers evaluate to the last day of `d`'s month — matching MySQL,
    /// including the correct 28/29 for February in common and leap years. The
    /// result is a `'YYYY-MM-DD'` date (the time part, if any, is dropped) and
    /// NULL propagates. Exactly one argument is required.
    fn last_day_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let modifier = |m: &str| Box::new(ast::Expr::Literal(ast::Literal::String(requote(m))));
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("date"),
            distinctness: None,
            args: vec![
                Box::new(arg),
                modifier("start of month"),
                modifier("+1 month"),
                modifier("-1 day"),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses `CONVERT_TZ(dt, from_tz, to_tz)` (the name and `(` already consumed)
    /// and lowers it to a shift of `dt` between two **numeric UTC offsets**. Each
    /// `±HH:MM` offset is read as a signed minute count, and `dt` is shifted by
    /// their difference via `datetime(dt, '<diff> minutes')` — so `CONVERT_TZ(dt,
    /// '+00:00', '+05:30')` moves the time forward 5½ hours, as in MySQL. A DATE
    /// argument is treated as midnight (the result is a DATETIME, like MySQL).
    ///
    /// A guard returns NULL unless both offsets have the `±HH:MM` shape, so a NULL
    /// or unparseable offset yields NULL (and a NULL `dt` propagates through
    /// `datetime`). This matches a real mysqld with **no time-zone tables loaded**,
    /// the common deployment. The named-zone form (`'US/Eastern'`, `'UTC'`), which
    /// needs those tables, is the one divergence: it returns NULL here rather than
    /// a converted value (see `mysql/COMPAT.md`).
    fn convert_tz_call(&mut self) -> Result<ast::Expr> {
        let dt = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let from_tz = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let to_tz = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let str_lit = |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
        let cast_int = |e: ast::Expr| ast::Expr::Cast {
            expr: Box::new(e),
            type_name: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            }),
        };
        // The signed minute count of an `±HH:MM` offset:
        // (-1 if it starts with '-', else 1) * (HH * 60 + MM).
        let offset_minutes = |o: &ast::Expr| {
            let sign = ast::Expr::Case {
                base: None,
                when_then_pairs: vec![(
                    Box::new(ast::Expr::binary(
                        substr_fn(o.clone(), *numeric_expr("1"), *numeric_expr("1")),
                        ast::Operator::Equals,
                        str_lit("-"),
                    )),
                    Box::new(*numeric_expr("-1")),
                )],
                else_expr: Some(numeric_expr("1")),
            };
            let hours = cast_int(substr_fn(o.clone(), *numeric_expr("2"), *numeric_expr("2")));
            let minutes = cast_int(substr_fn(o.clone(), *numeric_expr("5"), *numeric_expr("2")));
            let magnitude = ast::Expr::binary(
                ast::Expr::binary(hours, ast::Operator::Multiply, *numeric_expr("60")),
                ast::Operator::Add,
                minutes,
            );
            ast::Expr::binary(sign, ast::Operator::Multiply, magnitude)
        };
        // Whether an offset has the `±HH:MM` shape (a sign then a `:` separator).
        let is_offset = |o: &ast::Expr| {
            let first = substr_fn(o.clone(), *numeric_expr("1"), *numeric_expr("1"));
            let signed = ast::Expr::binary(
                ast::Expr::binary(first.clone(), ast::Operator::Equals, str_lit("+")),
                ast::Operator::Or,
                ast::Expr::binary(first, ast::Operator::Equals, str_lit("-")),
            );
            let colon = ast::Expr::binary(
                substr_fn(o.clone(), *numeric_expr("4"), *numeric_expr("1")),
                ast::Operator::Equals,
                str_lit(":"),
            );
            ast::Expr::binary(signed, ast::Operator::And, colon)
        };

        // diff = to_minutes - from_minutes, applied as `datetime(dt, '+diff minutes')`.
        let diff = ast::Expr::binary(
            offset_minutes(&to_tz),
            ast::Operator::Subtract,
            offset_minutes(&from_tz),
        );
        let modifier = call_fn("printf", vec![str_lit("%+d minutes"), diff]);
        let shifted = call_fn("datetime", vec![dt, modifier]);

        // NULL unless both offsets are numeric (a NULL `dt` propagates through
        // `datetime`); the named-zone form falls here, as on a tz-table-less server.
        let both_numeric =
            ast::Expr::binary(is_offset(&from_tz), ast::Operator::And, is_offset(&to_tz));
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(Box::new(both_numeric), Box::new(shifted))],
            else_expr: Some(Box::new(ast::Expr::Literal(ast::Literal::Null))),
        })
    }

    /// Lowers `MAKEDATE(year, dayofyear)` (the name and `(` are already consumed)
    /// to `date(printf('%04d-01-01', year), printf('%+d days', dayofyear - 1))` —
    /// the year's January 1st advanced by `dayofyear - 1` days, so day 1 is
    /// Jan 1 and a `dayofyear` past the year's length rolls into the next year,
    /// like MySQL. A `CASE` guards the cases MySQL returns NULL for: a NULL
    /// argument or a `dayofyear` below 1 (`printf` would otherwise coerce them).
    /// Exactly two arguments are required.
    fn makedate_call(&mut self) -> Result<ast::Expr> {
        let year = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let dayofyear = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        // year IS NULL OR dayofyear IS NULL OR dayofyear < 1
        let guard = ast::Expr::binary(
            ast::Expr::binary(
                ast::Expr::is_null(year.clone()),
                ast::Operator::Or,
                ast::Expr::is_null(dayofyear.clone()),
            ),
            ast::Operator::Or,
            ast::Expr::binary(
                dayofyear.clone(),
                ast::Operator::Less,
                ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
            ),
        );
        let jan_first = call_fn(
            "printf",
            vec![
                ast::Expr::Literal(ast::Literal::String(requote("%04d-01-01"))),
                year,
            ],
        );
        let day_offset = call_fn(
            "printf",
            vec![
                ast::Expr::Literal(ast::Literal::String(requote("%+d days"))),
                ast::Expr::binary(
                    dayofyear,
                    ast::Operator::Subtract,
                    ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
                ),
            ],
        );
        let made = call_fn("date", vec![jan_first, day_offset]);
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(guard),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(made)),
        })
    }

    /// Lowers `MAKETIME(hour, minute, second)` (the name and `(` are already
    /// consumed) to `printf('%s%02d:%02d:%02d', sign, abs(hour), minute, second)`,
    /// guarded by a `CASE` that returns NULL when any argument is NULL or
    /// `minute`/`second` is outside `0..=59` (which MySQL also rejects to NULL).
    /// The hour may exceed 23 (MySQL `TIME` spans to 838 hours). The sign is split
    /// from the magnitude so a negative hour renders as MySQL's `-01:..` (the sign
    /// before the zero-padded hour) rather than `-1:..`. One divergence not
    /// modeled: an hour past 838 is not clamped (see `mysql/COMPAT.md`). Exactly
    /// three arguments are required.
    fn maketime_call(&mut self) -> Result<ast::Expr> {
        let hour = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let minute = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let second = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let num = |n: &str| ast::Expr::Literal(ast::Literal::Numeric(n.to_string()));
        // `v < 0 OR v > 59`
        let out_of_range = |v: ast::Expr| {
            ast::Expr::binary(
                ast::Expr::binary(v.clone(), ast::Operator::Less, num("0")),
                ast::Operator::Or,
                ast::Expr::binary(v, ast::Operator::Greater, num("59")),
            )
        };
        let any_null = ast::Expr::binary(
            ast::Expr::binary(
                ast::Expr::is_null(hour.clone()),
                ast::Operator::Or,
                ast::Expr::is_null(minute.clone()),
            ),
            ast::Operator::Or,
            ast::Expr::is_null(second.clone()),
        );
        let bad_range = ast::Expr::binary(
            out_of_range(minute.clone()),
            ast::Operator::Or,
            out_of_range(second.clone()),
        );
        let guard = ast::Expr::binary(any_null, ast::Operator::Or, bad_range);

        // MySQL renders a negative hour as `-01:..` — the sign before the
        // zero-padded magnitude — so split the sign from `abs(hour)` (a plain
        // `%02d` of `-1` would be `-1`, putting the sign inside the field width).
        let sign = ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(ast::Expr::binary(hour.clone(), ast::Operator::Less, num("0"))),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("-")))),
            )],
            else_expr: Some(Box::new(ast::Expr::Literal(ast::Literal::String(requote(""))))),
        };
        let made = call_fn(
            "printf",
            vec![
                ast::Expr::Literal(ast::Literal::String(requote("%s%02d:%02d:%02d"))),
                sign,
                unary_fn("abs", hour),
                minute,
                second,
            ],
        );
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(
                Box::new(guard),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            )],
            else_expr: Some(Box::new(made)),
        })
    }

    /// Lowers `TIMESTAMPADD(unit, value, datetime)` (the name and `(` are already
    /// consumed) to `datetime(dt, '+<value × mult> <engine-unit>')` — the same
    /// modifier as `DATE_ADD(dt, INTERVAL value unit)` (see
    /// [`Self::apply_interval`]). The value must be an integer literal (optionally
    /// signed). As with `DATE_ADD`, a bare DATE argument keeps the engine's
    /// `00:00:00` time. `MICROSECOND` and any unit without an engine modifier are
    /// rejected. Exactly three arguments are required.
    fn timestampadd_call(&mut self) -> Result<ast::Expr> {
        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("a TIMESTAMPADD unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();
        self.expect(&Token::Comma, "`,`")?;

        let negative = self.eat(&Token::Minus);
        let raw = match self.peek() {
            Some(Token::Num(n) | Token::Str(n)) => n.clone(),
            _ => return Err(self.unexpected("an integer TIMESTAMPADD value")),
        };
        let value: i64 = raw.trim().parse().map_err(|_| {
            ParseError::Unsupported("TIMESTAMPADD value must be an integer literal".to_string())
        })?;
        self.advance();
        self.expect(&Token::Comma, "`,`")?;

        let target = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let (engine_unit, multiplier) = interval_unit_modifier(&unit).ok_or_else(|| {
            ParseError::Unsupported(format!("TIMESTAMPADD unit {unit} is not supported yet"))
        })?;
        let mut amount = value.saturating_mul(multiplier);
        if negative {
            amount = -amount;
        }
        let modifier = format!("{amount:+} {engine_unit}");
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("datetime"),
            distinctness: None,
            args: vec![
                Box::new(target),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(&modifier)))),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Lowers `ADDTIME(expr, t)` / `SUBTIME(expr, t)` (`subtract` for SUBTIME; the
    /// name and `(` are already consumed) — `expr` plus or minus the time-of-day
    /// `t`. The engine's `datetime`/`time` accept a `'HH:MM:SS'` argument as a
    /// signed time offset, so the shift is `datetime(expr, t)` when `expr` is a
    /// datetime and `time(expr, t)` when it is a bare time; SUBTIME prepends `-`
    /// to `t`. The two are told apart at runtime by whether `expr` contains a `-`
    /// (a date has one, a time of day does not), so the result keeps `expr`'s
    /// type as MySQL does. NULL propagates.
    ///
    /// Edges that diverge (documented in COMPAT.md): a time-of-day result past
    /// `24:00:00` wraps in the engine (MySQL's `TIME` runs to 838 h), a negative
    /// `TIME` `expr` (which contains `-`) takes the datetime branch, and a bare
    /// `DATE` `expr` is treated as midnight rather than MySQL's odd time coercion.
    fn time_add_call(&mut self, subtract: bool) -> Result<ast::Expr> {
        let expr = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let amount = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        let modifier = if subtract {
            ast::Expr::binary(
                ast::Expr::Literal(ast::Literal::String(requote("-"))),
                ast::Operator::Concat,
                amount,
            )
        } else {
            amount
        };
        let is_datetime = ast::Expr::like(
            expr.clone(),
            false,
            ast::LikeOperator::Like,
            ast::Expr::Literal(ast::Literal::String(requote("%-%"))),
            None,
        );
        let as_datetime = call_fn("datetime", vec![expr.clone(), modifier.clone()]);
        let as_time = call_fn("time", vec![expr, modifier]);
        Ok(ast::Expr::Case {
            base: None,
            when_then_pairs: vec![(Box::new(is_datetime), Box::new(as_datetime))],
            else_expr: Some(Box::new(as_time)),
        })
    }

    /// Lowers `TIMESTAMPDIFF(unit, a, b)` (the name and `(` are already
    /// consumed) to the whole-`unit` count of `b - a` — note the operand order
    /// is the reverse of `DATEDIFF`. The fixed-duration units divide the
    /// epoch-second difference `unixepoch(b) - unixepoch(a)` by the unit's length
    /// in seconds; SQLite's integer division truncates toward zero, matching
    /// MySQL's "complete units" semantics for both signs. The calendar units
    /// `MONTH`/`QUARTER`/`YEAR` (no fixed length) are counted by month via
    /// [`timestampdiff_months`]. `MICROSECOND` is rejected: the engine's
    /// datetimes carry only millisecond precision. NULL propagates.
    fn timestampdiff_call(&mut self) -> Result<ast::Expr> {
        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("a TIMESTAMPDIFF unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();
        self.expect(&Token::Comma, "`,`")?;
        let a = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let b = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;

        // The calendar units have no fixed second-length; count whole months
        // and scale (a quarter is 3 months, a year is 12).
        match unit.as_str() {
            "MONTH" => return Ok(timestampdiff_months(a, b, 1)),
            "QUARTER" => return Ok(timestampdiff_months(a, b, 3)),
            "YEAR" => return Ok(timestampdiff_months(a, b, 12)),
            _ => {}
        }

        let seconds_per_unit: i64 = match unit.as_str() {
            "SECOND" => 1,
            "MINUTE" => 60,
            "HOUR" => 3600,
            "DAY" => 86400,
            "WEEK" => 604800,
            other => {
                return Err(ParseError::Unsupported(format!(
                    "TIMESTAMPDIFF unit {other} is not supported yet \
                     (the engine's datetimes have only millisecond precision, \
                     so MICROSECOND cannot be counted)"
                )))
            }
        };

        // unixepoch returns integer seconds, so `b - a` is an integer and the
        // division below is an integer (truncating) division.
        let diff = ast::Expr::binary(
            unary_fn("unixepoch", b),
            ast::Operator::Subtract,
            unary_fn("unixepoch", a),
        );
        if seconds_per_unit == 1 {
            return Ok(diff);
        }
        Ok(ast::Expr::binary(
            diff,
            ast::Operator::Divide,
            ast::Expr::Literal(ast::Literal::Numeric(seconds_per_unit.to_string())),
        ))
    }

    /// Lowers `UNIX_TIMESTAMP([d])` to the engine's `unixepoch(...)`: with an
    /// argument, the epoch of that datetime; with none, the current epoch
    /// (`unixepoch('now')`). The engine treats the datetime as UTC (see
    /// `mysql/COMPAT.md`). The name and `(` are already consumed.
    fn unix_timestamp_call(&mut self) -> Result<ast::Expr> {
        let arg = if self.is(&Token::RParen) {
            ast::Expr::Literal(ast::Literal::String(requote("now")))
        } else {
            self.expr()?
        };
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("unixepoch"),
            distinctness: None,
            args: vec![Box::new(arg)],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Lowers `FROM_UNIXTIME(n)` to the engine's `datetime(n, 'unixepoch')`,
    /// which renders the epoch as a `'YYYY-MM-DD HH:MM:SS'` UTC datetime. The
    /// two-argument formatting form is not supported. The name and `(` are
    /// already consumed.
    // Named after the MySQL `FROM_UNIXTIME` function, not a conversion constructor.
    #[allow(clippy::wrong_self_convention)]
    fn from_unixtime_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("datetime"),
            distinctness: None,
            args: vec![
                Box::new(arg),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(
                    "unixepoch",
                )))),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses a column reference: `col` or `tbl.col`.
    fn column_ref(&mut self) -> Result<ast::Expr> {
        let first = self.name()?;
        if self.eat(&Token::Dot) {
            let second = self.name()?;
            // Inside `ON DUPLICATE KEY UPDATE`, a reference qualified by the
            // VALUES row alias (`alias.col`, MySQL 8.0.19+) is the would-be-
            // inserted value — the same as `VALUES(col)` — and lowers to the
            // engine's `excluded.col`. A column alias is mapped to the actual
            // column it stands for.
            if self.in_upsert_assignment
                && self
                    .upsert_row_alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(first.as_str()))
            {
                let actual = self
                    .upsert_col_aliases
                    .iter()
                    .find(|(a, _)| a.eq_ignore_ascii_case(second.as_str()))
                    .map(|(_, c)| ast::Name::from_string(c))
                    .unwrap_or(second);
                return Ok(ast::Expr::Qualified(
                    ast::Name::from_string("excluded"),
                    actual,
                ));
            }
            Ok(ast::Expr::Qualified(first, second))
        } else {
            // A bare reference to a VALUES column alias (`AS alias (c1, c2)`)
            // inside the upsert is the new row's value, lowered to the engine's
            // `excluded.<actual column>`. (The unqualified spelling is how MySQL
            // exposes column aliases; the `alias.col` spelling is handled above.)
            if self.in_upsert_assignment {
                if let Some((_, actual)) = self
                    .upsert_col_aliases
                    .iter()
                    .find(|(a, _)| a.eq_ignore_ascii_case(first.as_str()))
                {
                    return Ok(ast::Expr::Qualified(
                        ast::Name::from_string("excluded"),
                        ast::Name::from_string(actual),
                    ));
                }
            }
            Ok(ast::Expr::Id(first))
        }
    }

    // === Identifiers and shared parse helpers ===

    fn qualified_name(&mut self) -> Result<ast::QualifiedName> {
        let first = self.name()?;
        if self.eat(&Token::Dot) {
            let second = self.name()?;
            Ok(ast::QualifiedName::fullname(first, second))
        } else {
            Ok(ast::QualifiedName::single(first))
        }
    }

    fn name(&mut self) -> Result<ast::Name> {
        let name = match self.peek() {
            Some(Token::Word(w)) => ast::Name::from_string(w),
            Some(Token::QuotedIdent(s)) => ast::Name::exact(s.clone()),
            _ => return Err(self.unexpected("an identifier")),
        };
        self.advance();
        Ok(name)
    }

    fn expect_string(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Str(s)) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(self.unexpected("a string literal")),
        }
    }

    /// Consumes a balanced `( ... )` group, including the opening paren.
    fn skip_balanced(&mut self) -> Result<()> {
        self.expect(&Token::LParen, "`(`")?;
        self.skip_balanced_rest()
    }

    /// Consumes up to and including the `)` that matches an already-consumed `(`.
    fn skip_balanced_rest(&mut self) -> Result<()> {
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek() {
                None => return Err(self.unexpected("`)`")),
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => depth -= 1,
                _ => {}
            }
            self.advance();
        }
        Ok(())
    }

    /// Consumes tokens until the next top-level `,` or `)` (the separators
    /// between items in the column/constraint list), respecting nested parens.
    fn skip_to_item_boundary(&mut self) {
        let mut depth = 0usize;
        while let Some(tok) = self.peek() {
            match tok {
                Token::LParen => depth += 1,
                Token::RParen if depth == 0 => break,
                Token::RParen => depth -= 1,
                Token::Comma if depth == 0 => break,
                _ => {}
            }
            self.advance();
        }
    }

    /// Skips trailing table options after the column list, up to `;` or EOF.
    fn skip_table_options(&mut self) {
        while let Some(tok) = self.peek() {
            if *tok == Token::Semicolon {
                break;
            }
            self.advance();
        }
    }

    // === Low-level token cursor ===

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn peek_nth(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n).map(|(t, _)| t)
    }

    fn offset(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.eof, |(_, o)| *o)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn is_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// Whether the current token could begin a bare (non-`AS`) table alias — a
    /// quoted identifier or a word that is not a clause keyword reserved after a
    /// table reference. Used to look past a table's alias when deciding whether
    /// an `UPDATE` is the multi-table comma form.
    fn is_alias_word(&self) -> bool {
        match self.peek() {
            Some(Token::QuotedIdent(_)) => true,
            Some(Token::Word(w)) => !is_reserved_after_table(w),
            _ => false,
        }
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.is_keyword(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("keyword `{kw}`")))
        }
    }

    fn is(&self, tok: &Token) -> bool {
        self.peek() == Some(tok)
    }

    fn eat(&mut self, tok: &Token) -> bool {
        if self.is(tok) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Token, desc: &str) -> Result<()> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.unexpected(desc))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        let found = match self.peek() {
            Some(t) => t.describe(),
            None => "end of input".to_string(),
        };
        ParseError::Unexpected {
            offset: self.offset(),
            expected: expected.to_string(),
            found,
        }
    }
}

fn named(constraint: ast::ColumnConstraint) -> ast::NamedColumnConstraint {
    ast::NamedColumnConstraint {
        name: None,
        constraint,
    }
}

/// Whether a column's declared type is a non-binary character type, for which
/// MySQL's default collation is case-insensitive. The `BLOB` and
/// `BINARY`/`VARBINARY` byte-string types are excluded — they compare by byte.
fn is_character_type(col_type: &ast::Type) -> bool {
    let base = col_type
        .name
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
    )
}

/// Whether an explicit MySQL `COLLATE <name>` selects a case-sensitive
/// collation — a `_bin` (binary) or `_cs` (case-sensitive) collation, or the
/// `binary` collation. Any other collation (notably the `_ci` default) is
/// case-insensitive.
fn is_case_sensitive_collation(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "binary" || name.ends_with("_bin") || name.ends_with("_cs")
}

fn numeric_expr(value: &str) -> Box<ast::Expr> {
    Box::new(ast::Expr::Literal(ast::Literal::Numeric(value.to_string())))
}

/// MySQL's implicit type default for a `NOT NULL` column with no explicit
/// `DEFAULT` (see [`Parser::column_def`]): `0` for numeric types and `''` for
/// string/binary types. Date/time types (whose MySQL implicit default is the
/// zero date `'0000-00-00'`), `ENUM`/`SET` (the first member), `JSON`, and
/// unrecognized types return `None` -- their defaults do not map cleanly onto
/// the engine, so those columns stay strictly `NOT NULL`.
fn implicit_not_null_default(col_type: &ast::Type) -> Option<Box<ast::Expr>> {
    let base = col_type
        .name
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let numeric = matches!(
        base.as_str(),
        "INT"
            | "INTEGER"
            | "TINYINT"
            | "SMALLINT"
            | "MEDIUMINT"
            | "BIGINT"
            | "DECIMAL"
            | "DEC"
            | "NUMERIC"
            | "FIXED"
            | "FLOAT"
            | "DOUBLE"
            | "REAL"
            | "BIT"
            | "BOOL"
            | "BOOLEAN"
    );
    let string = matches!(
        base.as_str(),
        "CHAR"
            | "VARCHAR"
            | "TEXT"
            | "TINYTEXT"
            | "MEDIUMTEXT"
            | "LONGTEXT"
            | "BLOB"
            | "TINYBLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
            | "BINARY"
            | "VARBINARY"
    );
    if numeric {
        Some(numeric_expr("0"))
    } else if string {
        Some(Box::new(ast::Expr::Literal(ast::Literal::String(
            "''".to_string(),
        ))))
    } else {
        None
    }
}

/// Builds a single-argument engine function call, e.g. `julianday(arg)`.
/// Builds `coalesce(expr <op> 0, default)`, the lowering for the MySQL boolean
/// tests `IS [NOT] TRUE/FALSE`: the comparison yields 1/0 (or NULL when `expr`
/// is NULL), and the `coalesce` default replaces that NULL so the result is
/// always 0 or 1, never NULL.
fn coalesce_truthiness(expr: ast::Expr, op: ast::Operator, default: &str) -> ast::Expr {
    let cmp = ast::Expr::binary(
        expr,
        op,
        ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
    );
    ast::Expr::FunctionCall {
        name: ast::Name::from_string("coalesce"),
        distinctness: None,
        args: vec![
            Box::new(cmp),
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(
                default.to_string(),
            ))),
        ],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// Builds a single `DROP TABLE` statement, applying MySQL's `DROP TEMPORARY
/// TABLE` semantics: a temporary drop is qualified onto the engine's `temp`
/// schema so it never touches a base table of the same name (a schema qualifier
/// on a temporary drop is rejected). Shared by the single- and multi-table
/// `DROP TABLE` paths.
fn make_drop_table(
    temporary: bool,
    if_exists: bool,
    mut tbl_name: ast::QualifiedName,
) -> Result<ast::Stmt> {
    if temporary {
        if tbl_name.db_name.is_some() {
            return Err(ParseError::Unsupported(
                "DROP TEMPORARY TABLE with a schema qualifier is not supported yet".to_string(),
            ));
        }
        tbl_name = ast::QualifiedName::fullname(ast::Name::from_string("temp"), tbl_name.name);
    }
    Ok(ast::Stmt::DropTable {
        if_exists,
        tbl_name,
    })
}

/// A parsed `[-]value unit` interval, separated from the target it shifts so the
/// same spec can be applied whether the target precedes the interval (postfix
/// `date + INTERVAL n unit`) or follows it (prefix `INTERVAL n unit + date`).
/// See [`Parser::parse_interval_spec`] and [`build_interval`].
struct IntervalSpec {
    negative: bool,
    raw: String,
    unit: String,
}

/// Lowers `target ± interval` to the engine's `datetime(target, '<signed-amount>
/// <engine-unit>')` modifier. `subtract` is true for `DATE_SUB` and the `-`
/// operator. A simple unit coerces the value to an integer; a compound unit
/// (`HOUR_MINUTE`, `DAY_SECOND`, ...) splits a multi-field string like `'1:30'`
/// into fields. A month/year step that overflows a shorter target month is
/// clamped to that month's last day, matching MySQL (the engine would otherwise
/// roll over).
fn build_interval(target: ast::Expr, spec: &IntervalSpec, subtract: bool) -> Result<ast::Expr> {
    // A compound unit (`HOUR_MINUTE`, `DAY_SECOND`, ...) takes a multi-field
    // string like `'1:30'`; WordPress's GMT-offset upgrade uses
    // `INTERVAL '<h>:<m>' HOUR_MINUTE`.
    if let Some(fields) = compound_interval_units(&spec.unit) {
        return build_compound_interval(
            target,
            &spec.raw,
            &spec.unit,
            fields,
            spec.negative ^ subtract,
        );
    }

    let value: i64 = spec.raw.trim().parse().map_err(|_| {
        ParseError::Unsupported("INTERVAL value must be an integer literal".to_string())
    })?;
    // Map the MySQL unit onto the engine's modifier unit (`WEEK`/`QUARTER`
    // are expanded to days/months).
    let (engine_unit, multiplier) = interval_unit_modifier(&spec.unit).ok_or_else(|| {
        ParseError::Unsupported(format!("INTERVAL unit {} is not supported yet", spec.unit))
    })?;

    let mut amount = value.saturating_mul(multiplier);
    if spec.negative {
        amount = -amount;
    }
    if subtract {
        amount = -amount;
    }
    // `{:+}` renders an explicit sign, e.g. `+5 days` / `-1 days`.
    let modifier = format!("{amount:+} {engine_unit}");

    // A month or year step can overflow a shorter target month — the engine
    // rolls `Jan 31 + 1 month` into March, but MySQL clamps to the last day
    // of the month (`Feb 28`). Wrap those with the clamp; day/time steps never
    // overflow and pass through as a plain `datetime(target, '<modifier>')`.
    if engine_unit == "months" || engine_unit == "years" {
        Ok(clamp_month_overflow(target, &modifier))
    } else {
        Ok(call_fn(
            "datetime",
            vec![
                target,
                ast::Expr::Literal(ast::Literal::String(requote(&modifier))),
            ],
        ))
    }
}

/// Maps a MySQL date/time interval unit to the engine's `datetime()` modifier
/// unit and a multiplier on the amount (`WEEK` → 7 `days`, `QUARTER` → 3
/// `months`; the engine has no `weeks`/`quarters` modifier). Shared by `INTERVAL`
/// arithmetic and `TIMESTAMPADD`. Returns `None` for a unit without an engine
/// modifier (e.g. `MICROSECOND`). `unit` must already be uppercased.
fn interval_unit_modifier(unit: &str) -> Option<(&'static str, i64)> {
    Some(match unit {
        "DAY" => ("days", 1),
        "WEEK" => ("days", 7),
        "MONTH" => ("months", 1),
        "QUARTER" => ("months", 3),
        "YEAR" => ("years", 1),
        "HOUR" => ("hours", 1),
        "MINUTE" => ("minutes", 1),
        "SECOND" => ("seconds", 1),
        _ => return None,
    })
}

/// The ordered engine modifier units of a MySQL compound interval unit (e.g.
/// `HOUR_MINUTE` → hours then minutes), or `None` if `unit` is not compound. The
/// interval value is a string with one numeric field per returned unit.
fn compound_interval_units(unit: &str) -> Option<&'static [&'static str]> {
    Some(match unit {
        "YEAR_MONTH" => &["years", "months"],
        "DAY_HOUR" => &["days", "hours"],
        "DAY_MINUTE" => &["days", "hours", "minutes"],
        "DAY_SECOND" => &["days", "hours", "minutes", "seconds"],
        "HOUR_MINUTE" => &["hours", "minutes"],
        "HOUR_SECOND" => &["hours", "minutes", "seconds"],
        "MINUTE_SECOND" => &["minutes", "seconds"],
        _ => return None,
    })
}

/// Lowers a compound `INTERVAL '<fields>' UNIT` (e.g. `'1:30' HOUR_MINUTE`) to a
/// multi-modifier `datetime(target, '±h hours', '±m minutes')`. The value string
/// is split into one numeric field per engine unit (on any non-digit run, so
/// `:`, space, and `-` all separate). A leading `-` on the whole string negates
/// every field; `net_negate` folds in that the caller is `DATE_SUB` or had a
/// `-` token before the literal. Verified against MySQL 8.4.
fn build_compound_interval(
    target: ast::Expr,
    raw: &str,
    unit: &str,
    fields: &[&str],
    net_negate: bool,
) -> Result<ast::Expr> {
    let s = raw.trim();
    let (s, leading_neg) = match s.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (s.strip_prefix('+').unwrap_or(s), false),
    };
    let parts: Vec<&str> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() != fields.len() {
        return Err(ParseError::Unsupported(format!(
            "INTERVAL '{raw}' {unit} expects {} numeric field(s)",
            fields.len()
        )));
    }
    let negate = net_negate ^ leading_neg;

    let mut args = vec![Box::new(target)];
    for (part, field) in parts.iter().zip(fields) {
        let value: i64 = part.parse().map_err(|_| {
            ParseError::Unsupported(format!("INTERVAL '{raw}' {unit} has a non-integer field"))
        })?;
        let signed = if negate { -value } else { value };
        args.push(Box::new(ast::Expr::Literal(ast::Literal::String(requote(
            &format!("{signed:+} {field}"),
        )))));
    }
    Ok(ast::Expr::FunctionCall {
        name: ast::Name::from_string("datetime"),
        distinctness: None,
        args,
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    })
}

/// Builds `CAST(a / b AS INTEGER)` — the integer quotient (truncated toward
/// zero), used to lower the MySQL `DIV` operator and the quotient inside `MOD`.
fn integer_division(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    ast::Expr::Cast {
        expr: Box::new(ast::Expr::binary(a, ast::Operator::Divide, b)),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    }
}

/// Builds `length(CAST(x AS BLOB))` — a byte count. The engine's `length()`
/// counts characters, but `length()` of a BLOB counts bytes, so casting first
/// gives MySQL's byte semantics. Shared by `LENGTH`, `OCTET_LENGTH`, and (times
/// eight) `BIT_LENGTH`.
fn byte_length_expr(arg: ast::Expr) -> ast::Expr {
    let blob = ast::Expr::Cast {
        expr: Box::new(arg),
        type_name: Some(ast::Type {
            name: "BLOB".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    };
    unary_fn("length", blob)
}

/// Builds the lowering that repeats string `s` `n` times:
/// `CASE WHEN n IS NULL THEN NULL ELSE replace(hex(zeroblob(n)), '00', s) END`.
///
/// The engine has no `repeat()`. But `zeroblob(n)` is `n` zero bytes, whose
/// `hex()` is the text `'00'` repeated `n` times, so replacing every `'00'` with
/// `s` yields `s` repeated `n` times. A non-positive `n` makes an empty blob and
/// thus the empty string (matching MySQL), and a NULL `s` propagates through
/// `replace`. The `CASE` guard is needed only because `zeroblob(NULL)` is an
/// empty blob rather than NULL, so without it a NULL count would wrongly yield
/// `''` instead of NULL. Shared by `REPEAT(s, n)` and `SPACE(n)`.
fn repeat_expr(s: ast::Expr, n: ast::Expr) -> ast::Expr {
    let blob_hex = unary_fn("hex", unary_fn("zeroblob", n.clone()));
    let repeated = ast::Expr::FunctionCall {
        name: ast::Name::from_string("replace"),
        distinctness: None,
        args: vec![
            Box::new(blob_hex),
            Box::new(ast::Expr::Literal(ast::Literal::String(requote("00")))),
            Box::new(s),
        ],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    };
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(
            Box::new(ast::Expr::is_null(n)),
            Box::new(ast::Expr::Literal(ast::Literal::Null)),
        )],
        else_expr: Some(Box::new(repeated)),
    }
}

/// Builds `CASE WHEN s IS NULL THEN NULL ELSE lower(hex(<engine_fn>(CAST(s AS
/// TEXT)))) END` — the lowercase hex digest MySQL's hash functions
/// (`MD5`/`SHA1`/`SHA2`) return. MySQL hashes the *string* form of its argument,
/// so a numeric `s` is cast to text first (`MD5(123)` is `MD5('123')`). The
/// crypto extension's hash returns the raw digest *bytes*, so `hex` renders them
/// and `lower` matches MySQL's lowercase output; the guard propagates a NULL
/// argument as MySQL does.
fn crypto_hex_digest(engine_fn: &str, arg: ast::Expr) -> ast::Expr {
    let text = ast::Expr::Cast {
        expr: Box::new(arg.clone()),
        type_name: Some(ast::Type {
            name: "TEXT".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    };
    let digest = unary_fn("lower", unary_fn("hex", call_fn(engine_fn, vec![text])));
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(
            Box::new(ast::Expr::is_null(arg)),
            Box::new(ast::Expr::Literal(ast::Literal::Null)),
        )],
        else_expr: Some(Box::new(digest)),
    }
}

/// If `expr` is a negative integer literal (the unary-minus form `-2` or a signed
/// numeric literal `"-2"`), returns its magnitude (`2`), else `None`. Used to
/// recognize a negative `ROUND` precision at translation time.
fn negative_integer_literal(expr: &ast::Expr) -> Option<i64> {
    match expr {
        ast::Expr::Unary(ast::UnaryOperator::Negative, inner) => match inner.as_ref() {
            ast::Expr::Literal(ast::Literal::Numeric(n)) => {
                n.trim().parse::<i64>().ok().filter(|&v| v > 0)
            }
            _ => None,
        },
        ast::Expr::Literal(ast::Literal::Numeric(n)) => {
            n.trim().parse::<i64>().ok().filter(|&v| v < 0).map(i64::abs)
        }
        _ => None,
    }
}

/// Wraps an expression in `CAST(expr AS INTEGER)` — used to give the
/// whole-number math functions (`CEIL`/`FLOOR`/`ROUND`) MySQL's integer result
/// type rather than the engine's real.
fn cast_to_integer(expr: ast::Expr) -> ast::Expr {
    ast::Expr::Cast {
        expr: Box::new(expr),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    }
}

/// Builds a function-call expression `name(args...)`.
fn call_fn(name: &str, args: Vec<ast::Expr>) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::from_string(name),
        distinctness: None,
        args: args.into_iter().map(Box::new).collect(),
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// Builds `CAST(strftime(fmt, arg) AS INTEGER)` — a single `strftime` field as
/// an integer (which also strips any zero padding). Used by the `WEEK` lowering.
fn strftime_int(fmt: &str, arg: ast::Expr) -> ast::Expr {
    ast::Expr::Cast {
        expr: Box::new(call_fn(
            "strftime",
            vec![ast::Expr::Literal(ast::Literal::String(requote(fmt))), arg],
        )),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    }
}

/// Builds the `WEEK(d, mode)` lowering for the 1–53 "week year" modes 2 (`%U`,
/// Sunday-first) and 7 (`%W`, Monday-first). These match their 0–53 siblings
/// (modes 0 and 5) except that a date in the year's leading partial week — which
/// the strftime code numbers `0` — is instead numbered as the previous year's
/// last week. That number is the same code applied to the last day of the
/// previous year (`date(d, 'start of year', '-1 day')`), which always lies in
/// that final week. Verified against MySQL 8.4 for every year-boundary week of
/// 2000–2040.
fn week_push_zero(arg: ast::Expr, code: &str) -> ast::Expr {
    let prev_year_end = call_fn(
        "date",
        vec![
            arg.clone(),
            ast::Expr::Literal(ast::Literal::String(requote("start of year"))),
            ast::Expr::Literal(ast::Literal::String(requote("-1 day"))),
        ],
    );
    let prev_week = strftime_int(code, prev_year_end);
    let is_week_zero =
        ast::Expr::binary(strftime_int(code, arg.clone()), ast::Operator::Equals, *numeric_expr("0"));
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(Box::new(is_week_zero), Box::new(prev_week))],
        else_expr: Some(Box::new(strftime_int(code, arg))),
    }
}

/// Builds `lower(',' || x || ',')` — the value `x` wrapped in commas and
/// lower-cased, the building block of the [`Parser::find_in_set_call`] lowering.
fn comma_wrapped_lower(x: ast::Expr) -> ast::Expr {
    let comma = || ast::Expr::Literal(ast::Literal::String(requote(",")));
    let wrapped = ast::Expr::binary(
        ast::Expr::binary(comma(), ast::Operator::Concat, x),
        ast::Operator::Concat,
        comma(),
    );
    call_fn("lower", vec![wrapped])
}

/// Builds the `SUBSTRING_INDEX(s, d, 1)` lowering — the part of `s` before the
/// first occurrence of `d`, or the whole of `s` when `d` is absent:
/// `CASE WHEN instr(s, d) = 0 THEN s ELSE substr(s, 1, instr(s, d) - 1) END`.
/// `instr` is the engine's case-sensitive search (matching MySQL's delimiter
/// match), and a NULL argument propagates (the `instr`/`substr` are NULL). The
/// `count = -1` form reverses `s` and `d`, takes this prefix, and reverses back.
fn substring_index_before_first(s: ast::Expr, d: ast::Expr) -> ast::Expr {
    let pos = || call_fn("instr", vec![s.clone(), d.clone()]);
    // first = substr(s, 1, instr(s, d) - 1) — the part before the first delim.
    let first = substr_fn(
        s.clone(),
        *numeric_expr("1"),
        ast::Expr::binary(pos(), ast::Operator::Subtract, *numeric_expr("1")),
    );
    // The guard: no delimiter (`instr = 0`) returns the whole string.
    let no_delim = ast::Expr::binary(pos(), ast::Operator::Equals, *numeric_expr("0"));
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(Box::new(no_delim), Box::new(s))],
        else_expr: Some(Box::new(first)),
    }
}

/// Builds a `CAST(expr AS type)`, with one MySQL-faithful refinement for an
/// integer target (`SIGNED`/`UNSIGNED`, mapped to `INTEGER`): MySQL **rounds** a
/// numeric argument (`CAST(3.7 AS SIGNED)` is `4`) but parses a string by its
/// leading integer (`CAST('12.9' AS UNSIGNED)` is `12`), whereas the engine's
/// plain `CAST ... AS INTEGER` truncates a numeric too. A runtime `typeof` guard
/// restores MySQL's behaviour — a numeric (`integer`/`real`) value is rounded
/// before the cast, while a string/blob/NULL value is cast directly (the
/// leading-integer parse). Every other target is a plain cast.
fn build_cast(expr: ast::Expr, type_name: ast::Type) -> ast::Expr {
    if type_name.name != "INTEGER" {
        return ast::Expr::Cast {
            expr: Box::new(expr),
            type_name: Some(type_name),
        };
    }
    let int_cast = |e: ast::Expr| ast::Expr::Cast {
        expr: Box::new(e),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    };
    let type_is = |s: &str, e: ast::Expr| {
        ast::Expr::binary(
            unary_fn("typeof", e),
            ast::Operator::Equals,
            ast::Expr::Literal(ast::Literal::String(requote(s))),
        )
    };
    // `typeof(expr) = 'integer' OR typeof(expr) = 'real'` — a numeric value.
    let is_numeric = ast::Expr::binary(
        type_is("integer", expr.clone()),
        ast::Operator::Or,
        type_is("real", expr.clone()),
    );
    let rounded = int_cast(call_fn("round", vec![expr.clone()]));
    let truncated = int_cast(expr);
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(Box::new(is_numeric), Box::new(rounded))],
        else_expr: Some(Box::new(truncated)),
    }
}

/// Coerces a count/length/position argument to an integer the way MySQL does — a
/// numeric value rounds, a string parses its leading integer (see [`build_cast`]).
/// The engine would otherwise truncate a fractional argument toward zero, so
/// `LEFT('abcd', 2.9)` would be `ab` rather than MySQL's `abc`.
fn integer_arg(x: ast::Expr) -> ast::Expr {
    build_cast(
        x,
        ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        },
    )
}

/// Builds the case-insensitive pairwise extremum `CASE WHEN a >= (b COLLATE
/// NOCASE) THEN a ELSE b END` for `GREATEST` (`<=` for `LEAST`). Applying `COLLATE
/// NOCASE` to the comparison folds ASCII case like MySQL's default collation for
/// string operands, and is ignored for a numeric operand (so numbers still
/// compare numerically). The arms return the original `a`/`b` values.
fn case_insensitive_extremum(a: ast::Expr, b: ast::Expr, is_greatest: bool) -> ast::Expr {
    let op = if is_greatest {
        ast::Operator::GreaterEquals
    } else {
        ast::Operator::LessEquals
    };
    let b_nocase = ast::Expr::collate(b.clone(), ast::Name::from_string("NOCASE"));
    let cmp = ast::Expr::binary(a.clone(), op, b_nocase);
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(Box::new(cmp), Box::new(a))],
        else_expr: Some(Box::new(b)),
    }
}

/// Builds `substr(s, start, len)`.
fn substr_fn(s: ast::Expr, start: ast::Expr, len: ast::Expr) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::from_string("substr"),
        distinctness: None,
        args: vec![Box::new(s), Box::new(start), Box::new(len)],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// Lowers MySQL `SUBSTRING(str, pos[, len])` (and `SUBSTR`/`MID`) to the engine's
/// `substr`, guarded so the out-of-range cases match MySQL rather than SQLite.
/// They agree for an in-range position (1-based, negative counts from the end),
/// but MySQL yields `''` where SQLite returns the whole string or a backward
/// slice: when `pos = 0`, when `pos` is more than `length(str)` before the start
/// (`pos < -length(str)`), and — with a length — when `len < 0`. A `CASE` returns
/// `''` in those cases; NULL operands fall through to `substr`, which yields NULL
/// as MySQL does.
fn guarded_substr(target: ast::Expr, pos: ast::Expr, len: Option<ast::Expr>) -> ast::Expr {
    let zero = || ast::Expr::Literal(ast::Literal::Numeric("0".to_string()));
    // pos = 0
    let pos_is_zero = ast::Expr::binary(pos.clone(), ast::Operator::Equals, zero());
    // pos < -length(str)  (length counts characters, as MySQL's check does)
    let neg_length = ast::Expr::unary(
        ast::UnaryOperator::Negative,
        call_fn("length", vec![target.clone()]),
    );
    let pos_underflows = ast::Expr::binary(pos.clone(), ast::Operator::Less, neg_length);
    let mut out_of_range = ast::Expr::binary(pos_is_zero, ast::Operator::Or, pos_underflows);

    let substr = match len {
        Some(len) => {
            // A negative length is also empty in MySQL.
            let len_negative = ast::Expr::binary(len.clone(), ast::Operator::Less, zero());
            out_of_range = ast::Expr::binary(out_of_range, ast::Operator::Or, len_negative);
            substr_fn(target, pos, len)
        }
        None => call_fn("substr", vec![target, pos]),
    };

    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(
            Box::new(out_of_range),
            Box::new(ast::Expr::Literal(ast::Literal::String(requote("")))),
        )],
        else_expr: Some(Box::new(substr)),
    }
}

/// Builds the lowering for `LPAD`/`RPAD`: pad `target` to `len` characters using
/// `pad`, on the left when `left` is true, otherwise the right.
///
/// `REPEAT(pad, len)` makes at least `len` characters of padding (each copy of
/// `pad` is one or more chars). For `RPAD`, `substr(target || REPEAT(pad, len),
/// 1, len)` appends the padding then keeps the first `len` chars — which also
/// truncates a too-long `target` to its left `len` chars, like MySQL. For
/// `LPAD`, `substr(substr(REPEAT(pad, len), 1, len - length(target)) || target,
/// 1, len)` takes exactly the missing number of pad chars, prepends them, and
/// the same outer `substr` truncates when `target` is too long (the inner length
/// goes non-positive, contributing no padding). NULL in any argument propagates,
/// because the padding is always concatenated with `target` before truncation.
/// `length()` here is the engine's character count, matching MySQL's per-char
/// padding.
///
/// Two guards match MySQL's edge semantics, which the bare `substr`/`REPEAT`
/// lowering gets wrong: a negative `len` yields `NULL` (not the empty string),
/// and an empty `pad` when padding is actually needed (`len > length(target)`)
/// yields the empty string (not `target` unchanged — with no fill characters
/// MySQL cannot reach the requested length). Both guards fall through to the
/// `ELSE` body on a NULL operand (`len < 0`, `length(pad) = 0`, and the
/// comparisons all evaluate to NULL, never true), so NULL still propagates.
fn pad_expr(left: bool, target: ast::Expr, len: ast::Expr, pad: ast::Expr) -> ast::Expr {
    let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
    let zero = || ast::Expr::Literal(ast::Literal::Numeric("0".to_string()));
    let filler = repeat_expr(pad.clone(), len.clone());
    let body = if left {
        let fill_len = ast::Expr::binary(
            len.clone(),
            ast::Operator::Subtract,
            unary_fn("length", target.clone()),
        );
        let fill = substr_fn(filler, one(), fill_len);
        ast::Expr::binary(fill, ast::Operator::Concat, target.clone())
    } else {
        ast::Expr::binary(target.clone(), ast::Operator::Concat, filler)
    };
    let body = substr_fn(body, one(), len.clone());

    // len < 0 -> NULL, like MySQL.
    let len_negative = ast::Expr::binary(len.clone(), ast::Operator::Less, zero());
    // length(pad) = 0 AND len > length(target) -> '' (cannot pad with nothing).
    let pad_empty = ast::Expr::binary(unary_fn("length", pad), ast::Operator::Equals, zero());
    let needs_pad = ast::Expr::binary(len, ast::Operator::Greater, unary_fn("length", target));
    let unpaddable = ast::Expr::binary(pad_empty, ast::Operator::And, needs_pad);
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (
                Box::new(len_negative),
                Box::new(ast::Expr::Literal(ast::Literal::Null)),
            ),
            (
                Box::new(unpaddable),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("")))),
            ),
        ],
        else_expr: Some(Box::new(body)),
    }
}

/// Builds the lowering for MySQL's `a XOR b`: `(a <> 0) <> (b <> 0)` — the
/// boolean exclusive-or, 1 when exactly one operand is truthy. `x <> 0` is 1 for
/// a non-zero number and 0 for zero, and NULL for a NULL operand, so a NULL
/// propagates through the outer `<>`, matching MySQL. (A non-numeric string's
/// truthiness diverges, since the engine does not coerce it to 0.)
fn logical_xor(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    let zero = || ast::Expr::Literal(ast::Literal::Numeric("0".to_string()));
    let a_bool = ast::Expr::binary(a, ast::Operator::NotEquals, zero());
    let b_bool = ast::Expr::binary(b, ast::Operator::NotEquals, zero());
    ast::Expr::binary(a_bool, ast::Operator::NotEquals, b_bool)
}

/// Builds the lowering for MySQL's `a <=> b` (NULL-safe equality):
/// `CASE WHEN a IS NULL AND b IS NULL THEN 1
///       WHEN a IS NULL OR b IS NULL THEN 0 ELSE a = b END`.
///
/// It yields 1 when both sides are NULL, 0 when exactly one is, and the ordinary
/// equality otherwise — never NULL, as in MySQL. The lowering uses only `IS
/// NULL`, `=`, `AND`, and `OR`, so it needs no engine support for a general `IS`
/// operator.
fn null_safe_equals(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    let both_null = ast::Expr::binary(
        ast::Expr::is_null(a.clone()),
        ast::Operator::And,
        ast::Expr::is_null(b.clone()),
    );
    let either_null = ast::Expr::binary(
        ast::Expr::is_null(a.clone()),
        ast::Operator::Or,
        ast::Expr::is_null(b.clone()),
    );
    let equal = ast::Expr::binary(a, ast::Operator::Equals, b);
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (
                Box::new(both_null),
                Box::new(ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))),
            ),
            (
                Box::new(either_null),
                Box::new(ast::Expr::Literal(ast::Literal::Numeric("0".to_string()))),
            ),
        ],
        else_expr: Some(Box::new(equal)),
    }
}

/// Builds `a - b * CAST(a / b AS INTEGER)` — the MySQL remainder, which takes
/// the sign of `a` and is exact for float operands too (where the engine's `%`
/// would wrongly truncate to integers). Shared by the `a MOD b` operator and
/// the `MOD(a, b)` function form, which MySQL defines identically.
fn modulo(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    let quotient = integer_division(a.clone(), b.clone());
    let product = ast::Expr::binary(b, ast::Operator::Multiply, quotient);
    ast::Expr::binary(a, ast::Operator::Subtract, product)
}

/// Lowers MySQL's bitwise XOR `a ^ b`, which the engine has no operator for, to
/// `(a & ~b) | (~a & b)` using the engine's `&` / `|` / `~`. This is bit-for-bit
/// MySQL's result; like the other bitwise operators it prints as a signed
/// integer where MySQL prints the unsigned 64-bit value, but small non-negative
/// operands match (see `mysql/COMPAT.md`).
fn bitwise_xor(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    let not = |e: ast::Expr| ast::Expr::unary(ast::UnaryOperator::BitwiseNot, e);
    let and = |l: ast::Expr, r: ast::Expr| ast::Expr::binary(l, ast::Operator::BitwiseAnd, r);
    let left = and(a.clone(), not(b.clone()));
    let right = and(not(a), b);
    ast::Expr::binary(left, ast::Operator::BitwiseOr, right)
}

/// Lowers MySQL's `a / b`, which is always float division (`5 / 2` is `2.5`, not
/// `2`), by forcing the dividend to `REAL` so the engine divides as floats
/// rather than truncating two integers. Division by zero yields NULL, as in
/// MySQL. (The engine carries full double precision, so a non-terminating
/// quotient like `10 / 3` keeps more digits than MySQL's DECIMAL scale — a
/// documented edge in `mysql/COMPAT.md`.)
fn float_division(a: ast::Expr, b: ast::Expr) -> ast::Expr {
    let a_real = ast::Expr::Cast {
        expr: Box::new(a),
        type_name: Some(ast::Type {
            name: "REAL".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    };
    ast::Expr::binary(a_real, ast::Operator::Divide, b)
}

/// Clamps a `LIMIT`/`OFFSET` integer literal that overflows `i64` down to
/// `i64::MAX`. MySQL allows a `LIMIT`/`OFFSET` up to `2^64 - 1` and the idiom
/// `LIMIT 18446744073709551615` means "all remaining rows" (used after an
/// `OFFSET`); the engine stores the bound as a signed 64-bit integer, so such a
/// value would overflow. Since no table holds anywhere near `2^63` rows,
/// `i64::MAX` returns every remaining row just the same. Non-literal or
/// in-range bounds are returned unchanged.
fn clamp_limit_literal(expr: ast::Expr) -> ast::Expr {
    if let ast::Expr::Literal(ast::Literal::Numeric(ref s)) = expr {
        if s.parse::<i64>().is_err() && s.parse::<u64>().is_ok() {
            return ast::Expr::Literal(ast::Literal::Numeric(i64::MAX.to_string()));
        }
    }
    expr
}

fn unary_fn(name: &str, arg: ast::Expr) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::from_string(name),
        distinctness: None,
        args: vec![Box::new(arg)],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// English weekday names, indexed by `strftime('%w')` (0 = Sunday). Used by
/// `DAYNAME` and `DATE_FORMAT`'s `%W`.
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Abbreviated English weekday names, for `DATE_FORMAT`'s `%a`.
const WEEKDAY_NAMES_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// English month names, indexed by `strftime('%m')` (with start = 1). Used by
/// `MONTHNAME` and `DATE_FORMAT`'s `%M`.
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Abbreviated English month names, for `DATE_FORMAT`'s `%b`.
const MONTH_NAMES_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Builds `CASE CAST(strftime(fmt, arg) AS INTEGER) WHEN start THEN names[0] ...
/// END` — a date component (a weekday or month number) mapped to its English
/// name. The `CASE` has no `ELSE`, so a NULL date (which makes the integer NULL,
/// matching no `WHEN`) yields NULL, as MySQL does. Used by `DAYNAME`/`MONTHNAME`.
fn name_from_date(fmt: &str, names: &[&str], start: i64, arg: ast::Expr) -> ast::Expr {
    let number = cast_strftime_int(fmt, arg);
    let when_then_pairs = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let key = ast::Expr::Literal(ast::Literal::Numeric((start + i as i64).to_string()));
            (
                Box::new(key),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(name)))),
            )
        })
        .collect();
    ast::Expr::Case {
        base: Some(Box::new(number)),
        when_then_pairs,
        else_expr: None,
    }
}

/// Builds the `AM`/`PM` meridiem of `target`'s hour, for `DATE_FORMAT`'s `%p`:
/// `CASE WHEN h < 12 THEN 'AM' WHEN h >= 12 THEN 'PM' END` where `h` is the
/// 24-hour `CAST(strftime('%H', target) AS INTEGER)`. The `CASE` has no `ELSE`,
/// so a NULL hour matches neither arm and yields NULL.
fn meridiem_expr(target: ast::Expr) -> ast::Expr {
    let hour = cast_strftime_int("%H", target);
    let twelve = || ast::Expr::Literal(ast::Literal::Numeric("12".to_string()));
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (
                Box::new(ast::Expr::binary(
                    hour.clone(),
                    ast::Operator::Less,
                    twelve(),
                )),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("AM")))),
            ),
            (
                Box::new(ast::Expr::binary(
                    hour,
                    ast::Operator::GreaterEquals,
                    twelve(),
                )),
                Box::new(ast::Expr::Literal(ast::Literal::String(requote("PM")))),
            ),
        ],
        else_expr: None,
    }
}

/// Builds the 12-hour clock value of `target`'s hour, for `DATE_FORMAT`'s `%l`
/// (no leading zero) / `%h` and `%I` (`padded`, two digits). The 24-hour `h` is
/// mapped to 1-12 by `CASE WHEN h % 12 = 0 THEN 12 ELSE h % 12 END` (so 0→12,
/// 13→1); when `padded`, `substr('0' || value, -2)` left-pads it to two digits.
/// A NULL hour propagates (the `CASE` is NULL, and concatenating/`substr`-ing a
/// NULL stays NULL).
fn hour12_expr(target: ast::Expr, padded: bool) -> ast::Expr {
    let hour = cast_strftime_int("%H", target);
    let modulo12 = modulo(
        hour,
        ast::Expr::Literal(ast::Literal::Numeric("12".to_string())),
    );
    let value = ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(
            Box::new(ast::Expr::binary(
                modulo12.clone(),
                ast::Operator::Equals,
                ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
            )),
            Box::new(ast::Expr::Literal(ast::Literal::Numeric("12".to_string()))),
        )],
        else_expr: Some(Box::new(modulo12)),
    };
    if !padded {
        return value;
    }
    // Two-digit pad: the last two characters of `'0' || value`.
    let with_lead = ast::Expr::binary(
        ast::Expr::Literal(ast::Literal::String(requote("0"))),
        ast::Operator::Concat,
        value,
    );
    call_fn(
        "substr",
        vec![
            with_lead,
            ast::Expr::Literal(ast::Literal::Numeric("-2".to_string())),
        ],
    )
}

/// Builds the day of month with its English ordinal suffix, for `DATE_FORMAT`'s
/// `%D`: `day || CASE WHEN day BETWEEN 11 AND 13 THEN 'th' WHEN day % 10 = 1 THEN
/// 'st' WHEN day % 10 = 2 THEN 'nd' WHEN day % 10 = 3 THEN 'rd' ELSE 'th' END`,
/// where `day` is `CAST(strftime('%d', target) AS INTEGER)`. The teens (11-13)
/// are special-cased to `th` before the last-digit rules, matching MySQL. A NULL
/// day makes the leading `day ||` NULL, so the result is NULL.
fn ordinal_day_expr(target: ast::Expr) -> ast::Expr {
    let day = cast_strftime_int("%d", target);
    let num = |n: &str| ast::Expr::Literal(ast::Literal::Numeric(n.to_string()));
    let text = |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
    let last_digit_is = |n: &str| {
        ast::Expr::binary(
            modulo(day.clone(), num("10")),
            ast::Operator::Equals,
            num(n),
        )
    };
    let teens = ast::Expr::binary(
        ast::Expr::binary(day.clone(), ast::Operator::GreaterEquals, num("11")),
        ast::Operator::And,
        ast::Expr::binary(day.clone(), ast::Operator::LessEquals, num("13")),
    );
    let suffix = ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (Box::new(teens), Box::new(text("th"))),
            (Box::new(last_digit_is("1")), Box::new(text("st"))),
            (Box::new(last_digit_is("2")), Box::new(text("nd"))),
            (Box::new(last_digit_is("3")), Box::new(text("rd"))),
        ],
        else_expr: Some(Box::new(text("th"))),
    };
    ast::Expr::binary(day, ast::Operator::Concat, suffix)
}

/// Builds `CAST(strftime(fmt, arg) AS INTEGER)`, the lowering shared by the
/// MySQL date-part extractor functions (`YEAR`, `MONTH`, …) and `EXTRACT`. The
/// integer cast strips strftime's zero-padding to match MySQL's numeric result.
fn cast_strftime_int(fmt: &str, arg: ast::Expr) -> ast::Expr {
    let strftime = ast::Expr::FunctionCall {
        name: ast::Name::from_string("strftime"),
        distinctness: None,
        args: vec![
            Box::new(ast::Expr::Literal(ast::Literal::String(requote(fmt)))),
            Box::new(arg),
        ],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    };
    ast::Expr::Cast {
        expr: Box::new(strftime),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    }
}

/// Builds a MySQL compound `EXTRACT` value as the weighted sum of its date-part
/// fields — e.g. `EXTRACT(YEAR_MONTH FROM d)` is `year*100 + month` — from a list
/// of `(strftime_code, multiplier)` pairs. `arg` is cloned for each field; a NULL
/// `arg` makes every term NULL, so the whole result is NULL, as in MySQL.
fn extract_compound(arg: ast::Expr, parts: &[(&str, i64)]) -> ast::Expr {
    let mut result: Option<ast::Expr> = None;
    for (code, mult) in parts {
        let part = cast_strftime_int(code, arg.clone());
        let term = if *mult == 1 {
            part
        } else {
            ast::Expr::binary(
                part,
                ast::Operator::Multiply,
                ast::Expr::Literal(ast::Literal::Numeric(mult.to_string())),
            )
        };
        result = Some(match result {
            None => term,
            Some(acc) => ast::Expr::binary(acc, ast::Operator::Add, term),
        });
    }
    result.expect("at least one field")
}

/// Builds MySQL's calendar-based `TIMESTAMPDIFF(MONTH|QUARTER|YEAR, a, b)` as a
/// whole count of months from `a` to `b` (the `b - a` operand order), divided by
/// `divisor` (1 for MONTH, 3 for QUARTER, 12 for YEAR).
///
/// The raw month span is `(year_b*12 + month_b) - (year_a*12 + month_a)`. MySQL
/// counts only *complete* months, so a partial trailing month is dropped: when
/// `b` is after `a` (a positive span) but `b`'s day-and-time within its month is
/// earlier than `a`'s, the span is reduced by one — and symmetrically by one
/// toward zero for a negative span. The day-and-time position is compared as the
/// integer `DDhhmmss` (`strftime('%d%H%M%S', x)`), which is monotonic in
/// (day, hour, minute, second). SQLite's integer division then truncates the
/// month count toward zero for QUARTER/YEAR, matching MySQL. A NULL operand
/// makes every `strftime` NULL, so the whole result is NULL.
fn timestampdiff_months(a: ast::Expr, b: ast::Expr, divisor: i64) -> ast::Expr {
    let zero = || ast::Expr::Literal(ast::Literal::Numeric("0".to_string()));
    let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
    let ym = |x: ast::Expr| extract_compound(x, &[("%Y", 12), ("%m", 1)]);
    let raw = ast::Expr::binary(ym(b.clone()), ast::Operator::Subtract, ym(a.clone()));
    let sfx_a = cast_strftime_int("%d%H%M%S", a);
    let sfx_b = cast_strftime_int("%d%H%M%S", b);

    // Positive span with an incomplete trailing month: drop it.
    let pos_partial = ast::Expr::binary(
        ast::Expr::binary(raw.clone(), ast::Operator::Greater, zero()),
        ast::Operator::And,
        ast::Expr::binary(sfx_b.clone(), ast::Operator::Less, sfx_a.clone()),
    );
    // Negative span with an incomplete trailing month: pull it toward zero.
    let neg_partial = ast::Expr::binary(
        ast::Expr::binary(raw.clone(), ast::Operator::Less, zero()),
        ast::Operator::And,
        ast::Expr::binary(sfx_b, ast::Operator::Greater, sfx_a),
    );
    let months = ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (
                Box::new(pos_partial),
                Box::new(ast::Expr::binary(raw.clone(), ast::Operator::Subtract, one())),
            ),
            (
                Box::new(neg_partial),
                Box::new(ast::Expr::binary(raw.clone(), ast::Operator::Add, one())),
            ),
        ],
        else_expr: Some(Box::new(raw)),
    };
    if divisor == 1 {
        months
    } else {
        ast::Expr::binary(
            months,
            ast::Operator::Divide,
            ast::Expr::Literal(ast::Literal::Numeric(divisor.to_string())),
        )
    }
}

/// Builds `TO_DAYS(d)`: the MySQL day number (days since year 0), as
/// `CAST(julianday(date(d)) AS INTEGER) - 1721059`. The `date()` wrapper drops
/// any time part; the offset shifts the engine's Julian day onto MySQL's
/// proleptic-Gregorian day count. NULL propagates. Shared by `TO_DAYS` and the
/// day component of `TO_SECONDS`.
fn to_days_expr(arg: ast::Expr) -> ast::Expr {
    let julian = ast::Expr::Cast {
        expr: Box::new(unary_fn("julianday", unary_fn("date", arg))),
        type_name: Some(ast::Type {
            name: "INTEGER".to_string(),
            size: None,
            array_dimensions: 0,
        }),
    };
    ast::Expr::binary(
        julian,
        ast::Operator::Subtract,
        ast::Expr::Literal(ast::Literal::Numeric("1721059".to_string())),
    )
}

/// Builds `TIME_TO_SEC(t)`: the seconds since midnight of `t`'s time part,
/// `H*3600 + M*60 + S` (each `CAST(strftime(code, t) AS INTEGER)`). NULL
/// propagates. Shared by `TIME_TO_SEC` and the time component of `TO_SECONDS`.
fn time_to_sec_expr(t: ast::Expr) -> ast::Expr {
    let hours = ast::Expr::binary(
        cast_strftime_int("%H", t.clone()),
        ast::Operator::Multiply,
        ast::Expr::Literal(ast::Literal::Numeric("3600".to_string())),
    );
    let minutes = ast::Expr::binary(
        cast_strftime_int("%M", t.clone()),
        ast::Operator::Multiply,
        ast::Expr::Literal(ast::Literal::Numeric("60".to_string())),
    );
    let seconds = cast_strftime_int("%S", t);
    let hm = ast::Expr::binary(hours, ast::Operator::Add, minutes);
    ast::Expr::binary(hm, ast::Operator::Add, seconds)
}

/// Builds the absolute month count of a MySQL period `p` (`YYYYMM` or `YYMM`),
/// `normalized_year * 12 + month`, shared by `PERIOD_DIFF` and `PERIOD_ADD`.
///
/// The month is `p % 100` and the year `p / 100`; a two-digit year is normalized
/// the way MySQL does — `< 70` becomes `20YY`, `< 100` becomes `19YY`, and a
/// four-digit year is taken as-is. The month is kept 1-based (it cancels in
/// `PERIOD_DIFF`'s subtraction and is undone in `PERIOD_ADD`). A NULL period
/// makes every comparison NULL, so the `CASE` falls to its `ELSE` (`NULL` year)
/// and the whole count is NULL.
fn period_to_months(p: ast::Expr) -> ast::Expr {
    let hundred = || ast::Expr::Literal(ast::Literal::Numeric("100".to_string()));
    let year = ast::Expr::binary(p.clone(), ast::Operator::Divide, hundred());
    let month = ast::Expr::binary(p, ast::Operator::Modulus, hundred());
    let year_less_than = |bound: i64| {
        ast::Expr::binary(
            year.clone(),
            ast::Operator::Less,
            ast::Expr::Literal(ast::Literal::Numeric(bound.to_string())),
        )
    };
    let year_plus = |add: i64| {
        ast::Expr::binary(
            year.clone(),
            ast::Operator::Add,
            ast::Expr::Literal(ast::Literal::Numeric(add.to_string())),
        )
    };
    let normalized_year = ast::Expr::Case {
        base: None,
        when_then_pairs: vec![
            (Box::new(year_less_than(70)), Box::new(year_plus(2000))),
            (Box::new(year_less_than(100)), Box::new(year_plus(1900))),
        ],
        else_expr: Some(Box::new(year.clone())),
    };
    let year_months = ast::Expr::binary(
        normalized_year,
        ast::Operator::Multiply,
        ast::Expr::Literal(ast::Literal::Numeric("12".to_string())),
    );
    ast::Expr::binary(year_months, ast::Operator::Add, month)
}

/// Builds the inline-flag prefix (`(?…)`, or empty) for a `REGEXP_LIKE`
/// `match_type`, translating MySQL's flag letters to the Rust regex crate's: `i`
/// case-insensitive, `c` case-sensitive, `m` multi-line, `n` dot-matches-newline
/// (the crate's `s`); `u` (Unix line endings) is accepted and ignored. The match
/// defaults to case-insensitive (MySQL's default under the standard collation),
/// so `i` is on unless `c` turns it off. An unknown flag is rejected.
fn regexp_flag_prefix(match_type: Option<&str>) -> Result<String> {
    let mut case_insensitive = true;
    let mut multi_line = false;
    let mut dot_newline = false;
    if let Some(mt) = match_type {
        for flag in mt.chars() {
            match flag {
                'i' => case_insensitive = true,
                'c' => case_insensitive = false,
                'm' => multi_line = true,
                'n' => dot_newline = true,
                'u' => {}
                other => {
                    return Err(ParseError::Unsupported(format!(
                        "REGEXP_LIKE match type `{other}` is not supported"
                    )))
                }
            }
        }
    }
    let mut flags = String::new();
    if case_insensitive {
        flags.push('i');
    }
    if multi_line {
        flags.push('m');
    }
    if dot_newline {
        flags.push('s');
    }
    Ok(if flags.is_empty() {
        String::new()
    } else {
        format!("(?{flags})")
    })
}

/// Re-quotes a lexed (unescaped) string as a SQL single-quoted literal.
fn requote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Keywords that, appearing where a column type would, mean the type is absent.
/// Whether a `FROM` clause is just MySQL's dummy `DUAL` table — `SELECT expr
/// FROM DUAL`, equivalent to a `FROM`-less select — so the front-end can drop it
/// (the engine has no `DUAL` table). Only a single unaliased, unqualified `DUAL`
/// with no joins qualifies. (As in MySQL, an unquoted `dual` is always the dummy;
/// a real table actually named `dual` would be shadowed — but such a table is
/// never created in practice.)
fn from_is_dual(from: &ast::FromClause) -> bool {
    from.joins.is_empty()
        && matches!(from.select.as_ref(), ast::SelectTable::Table(tbl, None, _)
            if tbl.db_name.is_none() && tbl.name.as_str().eq_ignore_ascii_case("DUAL"))
}

fn is_column_constraint_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "NOT"
            | "NULL"
            | "PRIMARY"
            | "UNIQUE"
            | "KEY"
            | "DEFAULT"
            | "AUTO_INCREMENT"
            | "COMMENT"
            | "COLLATE"
            | "REFERENCES"
            | "CHECK"
            | "GENERATED"
            | "AS"
            | "ON"
    )
}

/// Whether `word` begins a table-level option in `ALTER TABLE` (`ENGINE=`,
/// `CONVERT TO CHARACTER SET`, `DEFAULT CHARSET=`, `ROW_FORMAT=`,
/// `AUTO_INCREMENT=`, `COMMENT=`, and the storage/statistics knobs). These have
/// no effect on the engine and are accepted as a no-op when they make up the
/// whole `ALTER TABLE` (the same options are ignored on `CREATE TABLE`). None of
/// these overlaps the column-operation keywords (`ADD`/`DROP`/`RENAME`/`CHANGE`/
/// `MODIFY`/`ALTER`), so a leading one unambiguously marks a table-option ALTER.
fn is_table_option_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ENGINE"
            | "CONVERT"
            | "DEFAULT"
            | "CHARSET"
            | "CHARACTER"
            | "COLLATE"
            | "ROW_FORMAT"
            | "AUTO_INCREMENT"
            | "COMMENT"
            | "PACK_KEYS"
            | "CHECKSUM"
            | "DELAY_KEY_WRITE"
            | "MAX_ROWS"
            | "MIN_ROWS"
            | "AVG_ROW_LENGTH"
            | "KEY_BLOCK_SIZE"
            | "STATS_AUTO_RECALC"
            | "STATS_PERSISTENT"
            | "STATS_SAMPLE_PAGES"
            | "TABLESPACE"
            | "COMPRESSION"
            | "ENCRYPTION"
    )
}

/// Whether `word` is a keyword that may legitimately follow a table reference
/// in a `FROM` clause, and therefore is **not** a bare table alias.
fn is_reserved_after_table(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "NATURAL"
            | "JOIN"
            | "STRAIGHT_JOIN"
            | "ON"
            | "USING"
            | "WHERE"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "HAVING"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "INTO"
            | "FOR"
            | "LOCK"
            | "WINDOW"
            | "AS"
            // `SET` ends the table-reference list of a multi-table `UPDATE`, so it
            // must not be swallowed as the last source table's alias.
            | "SET"
            // Index-hint keywords (`USE`/`FORCE`/`IGNORE INDEX`) follow a table
            // reference and must not be mistaken for an alias.
            | "USE"
            | "FORCE"
            | "IGNORE"
    )
}

/// Whether `word` is a keyword that ends or continues the select list, and so
/// is **not** a bare column alias.
fn is_reserved_select_alias(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "FROM"
            | "WHERE"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "HAVING"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "INTO"
            | "FOR"
            | "WINDOW"
            | "AS"
    )
}

/// Extracts the column name from a `SortedColumn` whose expression is a plain
/// column reference — the only form `sorted_column_list` produces.
fn sorted_column_name(sc: &ast::SortedColumn) -> &str {
    match sc.expr.as_ref() {
        ast::Expr::Id(name) => name.as_str(),
        _ => "",
    }
}

/// Functions whose MySQL semantics are identical to SQLite/turso, and are
/// therefore safe to pass straight through to the engine. `upper_name` must be
/// already uppercased. Covers the clean scalar set and the clean aggregates
/// (`AVG` is excluded — its DECIMAL formatting diverges).
fn is_supported_function(upper_name: &str) -> bool {
    matches!(
        upper_name,
        // Scalar functions. (`NULLIF` is handled by `nullif_call`, which compares
        // case-insensitively like MySQL's default collation.)
        "COALESCE" | "IFNULL" | "ABS" | "LOWER" | "UPPER"
        // String functions sharing both name and behaviour with the engine.
        // `LTRIM`/`RTRIM` strip leading/trailing spaces (their one-argument
        // MySQL form), like the engine's same-named functions. (`TRIM` and
        // `SUBSTR`/`SUBSTRING` are handled separately by `trim_call` /
        // `substring_call`, which also parse their SQL-standard `FROM` forms.)
        | "REPLACE" | "LTRIM" | "RTRIM"
        // `UNHEX(s)` is the inverse of `HEX` for the string case: it decodes a hex
        // string to the bytes it represents, mapping straight onto the engine's
        // `unhex` (a NULL or invalid/odd-length hex string yields NULL, as in
        // MySQL). The result is a binary string. (`HEX` is overloaded and handled
        // separately by `hex_call`.)
        | "UNHEX"
        // `CONCAT_WS(sep, ...)` joins the non-NULL arguments with `sep`, skipping
        // NULLs (and yielding NULL only for a NULL separator) — exactly the
        // engine's `concat_ws`. (Distinct from `CONCAT`, which is lowered to `||`
        // so it propagates NULL; see `concat_call`.)
        | "CONCAT_WS"
        // `REVERSE(s)` reverses the characters of `s`, mapping onto the engine's
        // `string_reverse` (renamed on emit). NULL propagates and a number is
        // reversed as its decimal string, as in MySQL. (MySQL reverses raw bytes,
        // so a multi-byte character diverges; documented in COMPAT.md.)
        | "REVERSE"
        // Functions sharing behaviour with the engine under a different name;
        // renamed on emit (see `engine_function_name`).
        | "IF"
        | "LCASE" | "UCASE" | "CHAR_LENGTH" | "CHARACTER_LENGTH"
        // The single-argument date/time extractors `DATE`/`TIME`/`TIMESTAMP` map
        // onto the engine's `date`/`time`/`datetime` (renamed below). They return
        // the date, time, or full datetime of the value, like MySQL.
        | "DATE" | "TIME" | "TIMESTAMP"
        // `SIGN(x)` returns -1/0/1 (an integer on both). `LAST_INSERT_ID()`
        // returns the connection's last auto-increment id — the engine's
        // `last_insert_rowid()` (renamed below), which matches because MySQL
        // `AUTO_INCREMENT` is lowered to the rowid-alias integer primary key.
        | "SIGN" | "LAST_INSERT_ID"
        // Numeric functions that share both name and behaviour with the engine.
        // `ROUND(x[, d])` rounds (to `d` decimals), `FLOOR`/`CEIL` round toward
        // -inf/+inf, `POW(x, y)` raises to a power, and `SQRT`/`EXP`/`LN` are the
        // square root, exponential, and natural log. `CEILING`/`POWER` are MySQL
        // synonyms renamed to `ceil`/`pow` below.
        | "ROUND" | "FLOOR" | "CEIL" | "CEILING" | "POW" | "POWER" | "SQRT" | "EXP" | "LN"
        // `PI()` is the constant, and `LOG2`/`LOG10` are the base-2 / base-10
        // logarithms — all identical to the engine's. (`LOG`, whose 1-argument
        // form is the natural log in MySQL but base-10 in the engine, is handled
        // separately by `log_call`.)
        | "PI" | "LOG2" | "LOG10"
        // Trigonometric functions, in radians, identical to the engine's:
        // `SIN`/`COS`/`TAN` and their inverses `ASIN`/`ACOS`, plus `ATAN2(y, x)`
        // and the angle conversions `DEGREES`/`RADIANS`. (`ATAN`, which also has a
        // two-argument `ATAN(y, x)` = `ATAN2` form in MySQL, is handled by
        // `atan_call`.)
        | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN2" | "DEGREES" | "RADIANS"
        // `JSON_VALID(x)` returns 1 if `x` parses as valid JSON, 0 if not, and
        // NULL if `x` is NULL — exactly the engine's `json_valid` (renamed on
        // emit). Note the engine's other JSON builders (`json_object`,
        // `json_array`) are *not* exposed here: their serialization omits the
        // spaces MySQL emits (`{"k": 1}` vs `{"k":1}`), so they would diverge.
        | "JSON_VALID"
        // Aggregate functions. `AVG` (the mean, ignoring NULLs — and `AVG(DISTINCT
        // x)`) maps to the engine's `avg`, which behaves identically.
        | "COUNT" | "SUM" | "MIN" | "MAX" | "AVG"
        // Window function. `ROW_NUMBER()` (always with an `OVER` clause) maps to
        // the engine's `row_number`, which numbers the rows of each partition in
        // the window's order, identically to MySQL. The other MySQL window
        // functions (`RANK`, `LAG`, `NTILE`, …) have no engine equivalent yet.
        | "ROW_NUMBER"
    )
}

/// The `strftime` format code for a MySQL date-part extractor function, or
/// `None` if the name is not one. The extracted component is cast to an integer
/// (see [`Parser::date_part_call`]). `upper_name` must already be uppercased.
fn date_part_format(upper_name: &str) -> Option<&'static str> {
    Some(match upper_name {
        "YEAR" => "%Y",
        "MONTH" => "%m",
        "DAY" | "DAYOFMONTH" => "%d",
        "DAYOFYEAR" => "%j",
        "HOUR" => "%H",
        "MINUTE" => "%M",
        "SECOND" => "%S",
        _ => return None,
    })
}

/// Lowers a MySQL `DATE_FORMAT(target, fmt)` to an expression over `target`.
///
/// Specifiers with a direct strftime equivalent are translated into strftime
/// format runs (`%i`/`%s` → `%M`/`%S`; `%Y %m %d %H %j %w %U` pass through; `%v`
/// → `%V`; `%T` → `%H:%M:%S`; `%%` and literal characters copied), each rendered
/// as `strftime(run, target)`. The name specifiers — `%M` (month name), `%b`
/// (abbreviated month), `%W` (weekday name), `%a` (abbreviated weekday) — have no
/// strftime form, so each becomes a `CASE`-over-`strftime` name lookup (see
/// [`name_from_date`]). The no-leading-zero numeric specifiers `%e` (day), `%c`
/// (month), and `%k` (hour) become the corresponding strftime code cast to an
/// integer (which drops the zero padding). The 12-hour clock (`%h`/`%I`/`%l`
/// hour, `%p` meridiem, `%r` = `hh:mm:ss AM/PM`), the ordinal day `%D` (1st,
/// 2nd…), and the two-digit year `%y` are each a `strftime`/`CASE` expression.
/// The pieces are concatenated with `||`. A NULL `target` makes every piece NULL,
/// so the whole result is NULL, as in MySQL. The week-of-year modes with no
/// matching strftime form (`%u`, `%V`, `%X`, `%x`) and microseconds (`%f`, no
/// sub-second precision) are rejected rather than silently mistranslated.
fn date_format_expr(mysql_fmt: &str, target: ast::Expr) -> Result<ast::Expr> {
    // A piece is a strftime format run, a name lookup, or an integer extraction
    // (a strftime code cast to an integer, which renders without leading zeros).
    enum Piece {
        Fmt(String),
        Name(&'static str, &'static [&'static str], i64),
        Int(&'static str),
        Meridiem,
        Hour12 { padded: bool },
        OrdinalDay,
        YearTwoDigit,
    }
    let mut pieces: Vec<Piece> = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, pieces: &mut Vec<Piece>| {
        if !run.is_empty() {
            pieces.push(Piece::Fmt(std::mem::take(run)));
        }
    };

    let mut chars = mysql_fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            run.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => run.push_str("%Y"),
            Some('m') => run.push_str("%m"),
            Some('d') => run.push_str("%d"),
            Some('H') => run.push_str("%H"),
            Some('i') => run.push_str("%M"),
            Some('s') => run.push_str("%S"),
            // `%j` (day of year, 001-366) and `%w` (weekday, 0=Sunday..6) have
            // the same name, range, and zero-padding in MySQL and strftime.
            Some('j') => run.push_str("%j"),
            Some('w') => run.push_str("%w"),
            // Week numbers: MySQL `%U` (Sunday-first, mode 0) is strftime `%U`,
            // and MySQL `%v` (Monday-first ISO, mode 3) is strftime `%V` — the
            // same mode-to-format mapping the `WEEK()` function uses (and which
            // its conformance test verifies). MySQL `%u`/`%V` (modes 1/2) have no
            // matching strftime format and stay rejected.
            Some('U') => run.push_str("%U"),
            Some('v') => run.push_str("%V"),
            // `%T` is the 24-hour `HH:MM:SS` time, i.e. `%H:%i:%s`.
            Some('T') => run.push_str("%H:%M:%S"),
            Some('%') => run.push_str("%%"),
            // The name specifiers have no strftime form; each is a CASE lookup.
            Some('M') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Name("%m", &MONTH_NAMES, 1));
            }
            Some('b') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Name("%m", &MONTH_NAMES_ABBR, 1));
            }
            Some('W') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Name("%w", &WEEKDAY_NAMES, 0));
            }
            Some('a') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Name("%w", &WEEKDAY_NAMES_ABBR, 0));
            }
            // No-leading-zero numeric specifiers: `%e` day (1-31), `%c` month
            // (1-12), `%k` hour (0-23). strftime only zero-pads, so cast the
            // padded value to an integer, which renders without the zero.
            Some('e') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Int("%d"));
            }
            Some('c') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Int("%m"));
            }
            Some('k') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Int("%H"));
            }
            // 12-hour clock and meridiem. `%p` is AM/PM; `%l` is the 12-hour hour
            // without a leading zero; `%h`/`%I` are the two-digit padded form.
            Some('p') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Meridiem);
            }
            Some('l') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Hour12 { padded: false });
            }
            Some('h') | Some('I') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Hour12 { padded: true });
            }
            // `%D` is the day of month with an English ordinal suffix (1st, 2nd…).
            Some('D') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::OrdinalDay);
            }
            // `%y` is the two-digit year (the last two digits of `%Y`).
            Some('y') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::YearTwoDigit);
            }
            // `%r` is the 12-hour `hh:mm:ss AM/PM` time: `%h:%i:%s %p`.
            Some('r') => {
                flush(&mut run, &mut pieces);
                pieces.push(Piece::Hour12 { padded: true });
                pieces.push(Piece::Fmt(":%M:%S ".to_string()));
                pieces.push(Piece::Meridiem);
            }
            Some(other) => {
                return Err(ParseError::Unsupported(format!(
                    "DATE_FORMAT specifier %{other} is not supported yet"
                )))
            }
            None => {
                return Err(ParseError::Unsupported(
                    "DATE_FORMAT format ends with a dangling `%`".to_string(),
                ))
            }
        }
    }
    flush(&mut run, &mut pieces);

    let mut exprs = pieces.into_iter().map(|piece| match piece {
        Piece::Fmt(fmt) => strftime_text(&fmt, target.clone()),
        Piece::Name(fmt, names, start) => name_from_date(fmt, names, start, target.clone()),
        Piece::Int(fmt) => cast_strftime_int(fmt, target.clone()),
        Piece::Meridiem => meridiem_expr(target.clone()),
        Piece::Hour12 { padded } => hour12_expr(target.clone(), padded),
        Piece::OrdinalDay => ordinal_day_expr(target.clone()),
        Piece::YearTwoDigit => year_two_digit_expr(target.clone()),
    });
    // An empty format renders strftime('', target) — the empty string for a
    // valid target, NULL for a NULL one, matching MySQL.
    let Some(mut acc) = exprs.next() else {
        return Ok(strftime_text("", target));
    };
    for next in exprs {
        acc = ast::Expr::binary(acc, ast::Operator::Concat, next);
    }
    Ok(acc)
}

/// Wraps a `datetime(target, '<modifier>')` month/year step with MySQL's
/// end-of-month clamping. The engine rolls an overflowing day into the next
/// month (`Jan 31 + 1 month` → March 3), whereas MySQL clamps to the month's last
/// day (`Feb 28`). When the rolled result's day-of-month is smaller than the
/// input's — i.e. it overflowed a shorter month — subtract that many days to land
/// on the previous month's last day, which preserves the time of day. A NULL
/// target makes the comparison NULL, so the `CASE` falls to the unclamped (also
/// NULL) result, matching MySQL.
fn clamp_month_overflow(target: ast::Expr, modifier: &str) -> ast::Expr {
    let string_lit = |s: &str| ast::Expr::Literal(ast::Literal::String(requote(s)));
    let rolled = || call_fn("datetime", vec![target.clone(), string_lit(modifier)]);
    let overflowed = ast::Expr::binary(
        cast_strftime_int("%d", rolled()),
        ast::Operator::Less,
        cast_strftime_int("%d", target.clone()),
    );
    // `'-' || strftime('%d', rolled) || ' days'`, the days to step back.
    let back = ast::Expr::binary(
        ast::Expr::binary(
            string_lit("-"),
            ast::Operator::Concat,
            strftime_text("%d", rolled()),
        ),
        ast::Operator::Concat,
        string_lit(" days"),
    );
    let clamped = call_fn("datetime", vec![rolled(), back]);
    ast::Expr::Case {
        base: None,
        when_then_pairs: vec![(Box::new(overflowed), Box::new(clamped))],
        else_expr: Some(Box::new(rolled())),
    }
}

/// Builds the two-digit year (`%y`) — `substr(strftime('%Y', arg), 3, 2)`, the
/// last two digits of the engine's four-digit zero-padded year. NULL propagates
/// through `strftime`/`substr`.
fn year_two_digit_expr(arg: ast::Expr) -> ast::Expr {
    call_fn(
        "substr",
        vec![
            strftime_text("%Y", arg),
            *numeric_expr("3"),
            *numeric_expr("2"),
        ],
    )
}

/// Builds `strftime(fmt, arg)` (the text form, not cast to an integer).
fn strftime_text(fmt: &str, arg: ast::Expr) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::from_string("strftime"),
        distinctness: None,
        args: vec![
            Box::new(ast::Expr::Literal(ast::Literal::String(requote(fmt)))),
            Box::new(arg),
        ],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// The canned literal a server/connection introspection function folds to, or
/// `None` if `upper_name` is not one. The values mirror the standalone-query
/// answers in the server's `session` module (placeholder server identity — see
/// `mysql/COMPAT.md`); `DATABASE()`/`SCHEMA()` are genuinely `NULL` because the
/// front-end has no current schema. `upper_name` must already be uppercased.
fn introspection_literal(upper_name: &str) -> Option<ast::Expr> {
    let literal = match upper_name {
        "VERSION" => ast::Literal::String(requote("8.0.0-turso")),
        "DATABASE" | "SCHEMA" => ast::Literal::Null,
        "CONNECTION_ID" => ast::Literal::Numeric("1".to_string()),
        "USER" | "CURRENT_USER" | "SESSION_USER" | "SYSTEM_USER" => {
            ast::Literal::String(requote("root@localhost"))
        }
        _ => return None,
    };
    Some(ast::Expr::Literal(literal))
}

/// The engine function (`datetime`/`date`/`time`) that a MySQL current
/// date/time function maps to, or `None` if `upper_name` is not one. Each is
/// applied to the `'now'` time string. `upper_name` must already be uppercased.
fn current_time_function(upper_name: &str) -> Option<&'static str> {
    Some(match upper_name {
        "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP"
        | "SYSDATE" => "datetime",
        "CURDATE" | "CURRENT_DATE" | "UTC_DATE" => "date",
        "CURTIME" | "CURRENT_TIME" | "UTC_TIME" => "time",
        _ => return None,
    })
}

/// The aggregate functions, which (unlike the scalar ones) accept a `DISTINCT`
/// quantifier. `upper_name` must already be uppercased.
fn is_aggregate_function(upper_name: &str) -> bool {
    matches!(upper_name, "COUNT" | "SUM" | "MIN" | "MAX" | "AVG")
}

/// The dedicated window functions, which always carry an `OVER` clause (and so
/// must parse one) but are not aggregates. Only `ROW_NUMBER` is supported — the
/// one the engine implements. `upper_name` must already be uppercased.
fn is_window_function(upper_name: &str) -> bool {
    matches!(upper_name, "ROW_NUMBER")
}

/// Whether a qualified name refers to `information_schema.TABLES`
/// (case-insensitive on both the schema and the table).
fn is_information_schema_tables(name: &ast::QualifiedName) -> bool {
    name.db_name
        .as_ref()
        .is_some_and(|db| db.as_str().eq_ignore_ascii_case("information_schema"))
        && name.name.as_str().eq_ignore_ascii_case("TABLES")
}

/// Builds the derived-table `SELECT` that emulates MySQL's
/// `information_schema.TABLES` from the engine catalog (`sqlite_schema`), exposing
/// the columns WordPress's upgrade and Site Health routines read. The engine keeps
/// no table statistics, so the row-count and size columns are `0` and `ENGINE` is
/// the fixed `InnoDB`. `TABLE_SCHEMA` is a placeholder (the front-end does not
/// track the connection's database name), so a query that filters on it matches
/// nothing — a documented limitation in `mysql/COMPAT.md`. SQLite's `sqlite_%` and
/// turso's `__turso_internal_*` bookkeeping tables are excluded, as in `SHOW
/// TABLES`.
fn information_schema_tables_select() -> Result<ast::Select> {
    const SQL: &str = "SELECT \
         name AS TABLE_NAME, \
         'def' AS TABLE_SCHEMA, \
         'InnoDB' AS ENGINE, \
         'BASE TABLE' AS TABLE_TYPE, \
         0 AS TABLE_ROWS, \
         0 AS DATA_LENGTH, \
         0 AS INDEX_LENGTH \
         FROM sqlite_schema \
         WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' \
         AND substr(name, 1, 17) <> '__turso_internal_'";
    match Parser::new(SQL.as_bytes())?.parse_statement()? {
        ast::Stmt::Select(select) => Ok(select),
        _ => unreachable!("the information_schema.TABLES emulation parses as a SELECT"),
    }
}

/// Whether `name` refers to `information_schema.COLUMNS` (the schema qualifier and
/// table name compared case-insensitively, as MySQL treats them).
fn is_information_schema_columns(name: &ast::QualifiedName) -> bool {
    name.db_name
        .as_ref()
        .is_some_and(|db| db.as_str().eq_ignore_ascii_case("information_schema"))
        && name.name.as_str().eq_ignore_ascii_case("COLUMNS")
}

/// Builds the derived-table `SELECT` that emulates MySQL's
/// `information_schema.COLUMNS` from the engine catalog, exposing the per-column
/// metadata WordPress's charset detection (`get_table_charset`) and Site Health
/// read. One row per column of every base table is produced by joining
/// `sqlite_schema` with the `pragma_table_info` table-valued function. The fixed
/// single charset is reported as `utf8mb4` / `utf8mb4_general_ci` on the character
/// columns and NULL elsewhere, and `COLUMN_KEY` is `PRI` for a primary-key column.
///
/// Parsed with the engine parser (`turso_parser`) rather than the front-end's,
/// because the front-end FROM grammar does not model the `pragma_table_info`
/// table function.
///
/// Divergences from a real mysqld (see `mysql/COMPAT.md`): `TABLE_SCHEMA` is the
/// placeholder `def` (the front-end does not track the connection's database, as
/// with the `TABLES` emulation), so a query filtering on it matches nothing;
/// `COLLATION_NAME` is the front-end's fixed `utf8mb4_general_ci` rather than a
/// server's default (e.g. 8.4's `utf8mb4_0900_ai_ci`); and `COLUMN_TYPE` carries
/// no length/precision (`varchar`, not `varchar(20)`) because the engine catalog
/// drops it. `sqlite_%` and `__turso_internal_*` bookkeeping tables are excluded.
fn information_schema_columns_select() -> ast::Select {
    const SQL: &[u8] = b"SELECT \
         'def' AS TABLE_CATALOG, \
         'def' AS TABLE_SCHEMA, \
         m.name AS TABLE_NAME, \
         p.name AS COLUMN_NAME, \
         p.cid + 1 AS ORDINAL_POSITION, \
         p.dflt_value AS COLUMN_DEFAULT, \
         CASE WHEN p.\"notnull\" = 1 OR p.pk > 0 THEN 'NO' ELSE 'YES' END AS IS_NULLABLE, \
         lower(p.type) AS DATA_TYPE, \
         CASE WHEN lower(p.type) IN ('char', 'varchar', 'text', 'tinytext', 'mediumtext', 'longtext', 'enum', 'set') THEN 'utf8mb4' ELSE NULL END AS CHARACTER_SET_NAME, \
         CASE WHEN lower(p.type) IN ('char', 'varchar', 'text', 'tinytext', 'mediumtext', 'longtext', 'enum', 'set') THEN 'utf8mb4_general_ci' ELSE NULL END AS COLLATION_NAME, \
         lower(p.type) AS COLUMN_TYPE, \
         CASE WHEN p.pk > 0 THEN 'PRI' ELSE '' END AS COLUMN_KEY, \
         '' AS EXTRA \
         FROM sqlite_schema m \
         JOIN pragma_table_info(m.name) p \
         WHERE m.type = 'table' \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_'";
    let mut parser = turso_parser::parser::Parser::new(SQL);
    match parser.next() {
        Some(Ok(ast::Cmd::Stmt(ast::Stmt::Select(select)))) => select,
        _ => unreachable!("the information_schema.COLUMNS emulation parses as a SELECT"),
    }
}

/// Whether `name` refers to `information_schema.STATISTICS` (both parts compared
/// case-insensitively, as MySQL treats them).
fn is_information_schema_statistics(name: &ast::QualifiedName) -> bool {
    name.db_name
        .as_ref()
        .is_some_and(|db| db.as_str().eq_ignore_ascii_case("information_schema"))
        && name.name.as_str().eq_ignore_ascii_case("STATISTICS")
}

/// Builds the derived-table `SELECT` that emulates MySQL's
/// `information_schema.STATISTICS` (one row per indexed column) from the engine
/// catalog. It is the union of two sources: the primary key, synthesized from the
/// `pragma_table_info` columns flagged as part of it (so it is named `PRIMARY` as
/// in MySQL, rather than the engine's auto-index name, and covers the rowid-alias
/// key that has no separate index), and the secondary indexes from
/// `pragma_index_list` / `pragma_index_info` (the `origin = 'c'` rows — the named
/// `KEY` / `UNIQUE KEY` indexes the front-end creates). `NON_UNIQUE` is `0` for a
/// unique/primary index and `1` otherwise, and `NULLABLE` reflects the column's
/// `NOT NULL` flag. WordPress and migration tools read index metadata from it; the
/// `SHOW INDEX` form is also supported.
///
/// Parsed with the engine parser (`turso_parser`), like the `COLUMNS` emulation,
/// because of the `pragma_*` table functions. Divergences (see `mysql/COMPAT.md`):
/// `TABLE_SCHEMA` is the placeholder `def` (filtering on it matches nothing);
/// `CARDINALITY` is `0` (no statistics); and an unnamed unique constraint
/// (SQLite's `origin = 'u'` auto-index) is not reported, since its engine name is
/// not MySQL's.
fn information_schema_statistics_select() -> ast::Select {
    const SQL: &[u8] = b"SELECT \
         'def' AS TABLE_CATALOG, \
         'def' AS TABLE_SCHEMA, \
         m.name AS TABLE_NAME, \
         0 AS NON_UNIQUE, \
         'PRIMARY' AS INDEX_NAME, \
         ROW_NUMBER() OVER (PARTITION BY m.name ORDER BY p.cid) AS SEQ_IN_INDEX, \
         p.name AS COLUMN_NAME, \
         'A' AS COLLATION, \
         0 AS CARDINALITY, \
         '' AS NULLABLE, \
         'BTREE' AS INDEX_TYPE \
         FROM sqlite_schema m \
         JOIN pragma_table_info(m.name) p \
         WHERE m.type = 'table' AND p.pk > 0 \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_' \
         UNION ALL \
         SELECT \
         'def', \
         'def', \
         m.name, \
         CASE WHEN il.\"unique\" = 1 THEN 0 ELSE 1 END, \
         il.name, \
         ii.seqno + 1, \
         ii.name, \
         'A', \
         0, \
         CASE WHEN ti.\"notnull\" = 1 THEN '' ELSE 'YES' END, \
         'BTREE' \
         FROM sqlite_schema m \
         JOIN pragma_index_list(m.name) il \
         JOIN pragma_index_info(il.name) ii \
         JOIN pragma_table_info(m.name) ti ON ti.name = ii.name \
         WHERE m.type = 'table' AND il.origin = 'c' \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_'";
    let mut parser = turso_parser::parser::Parser::new(SQL);
    match parser.next() {
        Some(Ok(ast::Cmd::Stmt(ast::Stmt::Select(select)))) => select,
        _ => unreachable!("the information_schema.STATISTICS emulation parses as a SELECT"),
    }
}

/// Whether `name` refers to `information_schema.TABLE_CONSTRAINTS` (both parts
/// compared case-insensitively, as MySQL treats them).
fn is_information_schema_table_constraints(name: &ast::QualifiedName) -> bool {
    name.db_name
        .as_ref()
        .is_some_and(|db| db.as_str().eq_ignore_ascii_case("information_schema"))
        && name.name.as_str().eq_ignore_ascii_case("TABLE_CONSTRAINTS")
}

/// Builds the derived-table `SELECT` that emulates MySQL's
/// `information_schema.TABLE_CONSTRAINTS` (one row per primary-key or unique
/// constraint) from the engine catalog. It is the union of the primary key
/// (`PRIMARY` / `PRIMARY KEY`, a `DISTINCT` row per table that has one, so a
/// composite key still yields a single row) and the named unique indexes
/// (`UNIQUE`, from `pragma_index_list` rows that are unique and `origin = 'c'`).
/// Constraints are `ENFORCED`. Non-unique keys are not constraints and are
/// excluded.
///
/// Parsed with the engine parser (`turso_parser`), like the other
/// `information_schema` emulations, because of the `pragma_*` table functions.
/// Divergences (see `mysql/COMPAT.md`): `TABLE_SCHEMA` / `CONSTRAINT_SCHEMA` are
/// the placeholder `def` (filtering on them matches nothing); and `FOREIGN KEY` /
/// `CHECK` constraints and unnamed unique constraints are not reported (the engine
/// does not enforce or name them as MySQL does).
fn information_schema_table_constraints_select() -> ast::Select {
    const SQL: &[u8] = b"SELECT DISTINCT \
         'def' AS CONSTRAINT_CATALOG, \
         'def' AS CONSTRAINT_SCHEMA, \
         'PRIMARY' AS CONSTRAINT_NAME, \
         'def' AS TABLE_SCHEMA, \
         m.name AS TABLE_NAME, \
         'PRIMARY KEY' AS CONSTRAINT_TYPE, \
         'YES' AS ENFORCED \
         FROM sqlite_schema m \
         JOIN pragma_table_info(m.name) p \
         WHERE m.type = 'table' AND p.pk >= 1 \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_' \
         UNION ALL \
         SELECT \
         'def', \
         'def', \
         il.name, \
         'def', \
         m.name, \
         'UNIQUE', \
         'YES' \
         FROM sqlite_schema m \
         JOIN pragma_index_list(m.name) il \
         WHERE m.type = 'table' AND il.origin = 'c' AND il.\"unique\" = 1 \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_'";
    let mut parser = turso_parser::parser::Parser::new(SQL);
    match parser.next() {
        Some(Ok(ast::Cmd::Stmt(ast::Stmt::Select(select)))) => select,
        _ => unreachable!("the information_schema.TABLE_CONSTRAINTS emulation parses as a SELECT"),
    }
}

/// Whether `name` refers to `information_schema.KEY_COLUMN_USAGE` (both parts
/// compared case-insensitively, as MySQL treats them).
fn is_information_schema_key_column_usage(name: &ast::QualifiedName) -> bool {
    name.db_name
        .as_ref()
        .is_some_and(|db| db.as_str().eq_ignore_ascii_case("information_schema"))
        && name.name.as_str().eq_ignore_ascii_case("KEY_COLUMN_USAGE")
}

/// Builds the derived-table `SELECT` that emulates MySQL's
/// `information_schema.KEY_COLUMN_USAGE` (one row per column of a primary-key or
/// unique constraint) from the engine catalog. It is the union of the primary-key
/// columns (named `PRIMARY`, numbered by `ROW_NUMBER` so a composite key counts
/// `1, 2, ...`) and the named unique-index columns (`pragma_index_list` /
/// `pragma_index_info`, the unique `origin = 'c'` indexes). `ORDINAL_POSITION` is
/// the column's 1-based position within the constraint. A non-unique `KEY` is not
/// a constraint and is excluded.
///
/// This engine has no foreign keys, so the foreign-key columns
/// (`POSITION_IN_UNIQUE_CONSTRAINT`, `REFERENCED_*`) are always NULL. Parsed with
/// the engine parser (`turso_parser`), like the other emulations. Same
/// `TABLE_SCHEMA` placeholder limitation as `TABLES` (filtering on it matches
/// nothing); see `mysql/COMPAT.md`.
fn information_schema_key_column_usage_select() -> ast::Select {
    const SQL: &[u8] = b"SELECT \
         'def' AS CONSTRAINT_CATALOG, \
         'def' AS CONSTRAINT_SCHEMA, \
         'PRIMARY' AS CONSTRAINT_NAME, \
         'def' AS TABLE_CATALOG, \
         'def' AS TABLE_SCHEMA, \
         m.name AS TABLE_NAME, \
         p.name AS COLUMN_NAME, \
         ROW_NUMBER() OVER (PARTITION BY m.name ORDER BY p.cid) AS ORDINAL_POSITION, \
         NULL AS POSITION_IN_UNIQUE_CONSTRAINT, \
         NULL AS REFERENCED_TABLE_SCHEMA, \
         NULL AS REFERENCED_TABLE_NAME, \
         NULL AS REFERENCED_COLUMN_NAME \
         FROM sqlite_schema m \
         JOIN pragma_table_info(m.name) p \
         WHERE m.type = 'table' AND p.pk > 0 \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_' \
         UNION ALL \
         SELECT \
         'def', \
         'def', \
         il.name, \
         'def', \
         'def', \
         m.name, \
         ii.name, \
         ii.seqno + 1, \
         NULL, \
         NULL, \
         NULL, \
         NULL \
         FROM sqlite_schema m \
         JOIN pragma_index_list(m.name) il \
         JOIN pragma_index_info(il.name) ii \
         WHERE m.type = 'table' AND il.origin = 'c' AND il.\"unique\" = 1 \
         AND m.name NOT LIKE 'sqlite_%' \
         AND substr(m.name, 1, 17) <> '__turso_internal_'";
    let mut parser = turso_parser::parser::Parser::new(SQL);
    match parser.next() {
        Some(Ok(ast::Cmd::Stmt(ast::Stmt::Select(select)))) => select,
        _ => unreachable!("the information_schema.KEY_COLUMN_USAGE emulation parses as a SELECT"),
    }
}

/// Whether `expr` calls an aggregate function anywhere in its own scope. Used to
/// tell an aggregate `HAVING` (a whole-table aggregate the engine handles) from
/// a non-aggregate one (a row filter, foldable into `WHERE`). Subqueries are not
/// descended into — an aggregate there belongs to the subquery, not this clause.
/// The lowercased SELECT-list aliases whose defining expression is an aggregate
/// (e.g. the `c` in `SELECT COUNT(*) c`). Used to recognize a `HAVING` that
/// filters on an aggregate through its alias.
fn aggregate_alias_names(columns: &[ast::ResultColumn]) -> Vec<String> {
    columns
        .iter()
        .filter_map(|column| match column {
            ast::ResultColumn::Expr(expr, Some(as_)) if expr_contains_aggregate(expr) => {
                let (ast::As::As(name)
                | ast::As::Elided(name)
                | ast::As::ImplicitColumnName(name)) = as_;
                Some(name.as_str().to_ascii_lowercase())
            }
            _ => None,
        })
        .collect()
}

/// Whether `expr` references any of `names` as a bare identifier (compared
/// case-insensitively). Mirrors [`expr_contains_aggregate`]'s traversal.
fn expr_references_name(expr: &ast::Expr, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    match expr {
        ast::Expr::Id(name) => names.iter().any(|n| n.eq_ignore_ascii_case(name.as_str())),
        ast::Expr::FunctionCall { args, .. } => {
            args.iter().any(|a| expr_references_name(a, names))
        }
        ast::Expr::Binary(l, _, r) => {
            expr_references_name(l, names) || expr_references_name(r, names)
        }
        ast::Expr::Unary(_, e)
        | ast::Expr::IsNull(e)
        | ast::Expr::NotNull(e)
        | ast::Expr::Collate(e, _)
        | ast::Expr::Cast { expr: e, .. } => expr_references_name(e, names),
        ast::Expr::Between {
            lhs, start, end, ..
        } => {
            expr_references_name(lhs, names)
                || expr_references_name(start, names)
                || expr_references_name(end, names)
        }
        ast::Expr::Like {
            lhs, rhs, escape, ..
        } => {
            expr_references_name(lhs, names)
                || expr_references_name(rhs, names)
                || escape.as_deref().is_some_and(|e| expr_references_name(e, names))
        }
        ast::Expr::InList { lhs, rhs, .. } => {
            expr_references_name(lhs, names) || rhs.iter().any(|e| expr_references_name(e, names))
        }
        ast::Expr::Parenthesized(exprs) => {
            exprs.iter().any(|e| expr_references_name(e, names))
        }
        ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            base.as_deref().is_some_and(|e| expr_references_name(e, names))
                || when_then_pairs
                    .iter()
                    .any(|(w, t)| expr_references_name(w, names) || expr_references_name(t, names))
                || else_expr.as_deref().is_some_and(|e| expr_references_name(e, names))
        }
        _ => false,
    }
}

fn expr_contains_aggregate(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::FunctionCallStar { .. } => true,
        ast::Expr::FunctionCall { name, args, .. } => {
            is_aggregate_function(&name.as_str().to_ascii_uppercase())
                || args.iter().any(|a| expr_contains_aggregate(a))
        }
        ast::Expr::Binary(l, _, r) => expr_contains_aggregate(l) || expr_contains_aggregate(r),
        ast::Expr::Unary(_, e)
        | ast::Expr::IsNull(e)
        | ast::Expr::NotNull(e)
        | ast::Expr::Collate(e, _)
        | ast::Expr::Cast { expr: e, .. } => expr_contains_aggregate(e),
        ast::Expr::Between {
            lhs, start, end, ..
        } => {
            expr_contains_aggregate(lhs)
                || expr_contains_aggregate(start)
                || expr_contains_aggregate(end)
        }
        ast::Expr::Like {
            lhs, rhs, escape, ..
        } => {
            expr_contains_aggregate(lhs)
                || expr_contains_aggregate(rhs)
                || escape.as_deref().is_some_and(expr_contains_aggregate)
        }
        ast::Expr::InList { lhs, rhs, .. } => {
            expr_contains_aggregate(lhs) || rhs.iter().any(|e| expr_contains_aggregate(e))
        }
        ast::Expr::Parenthesized(exprs) => exprs.iter().any(|e| expr_contains_aggregate(e)),
        ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            base.as_deref().is_some_and(expr_contains_aggregate)
                || when_then_pairs
                    .iter()
                    .any(|(w, t)| expr_contains_aggregate(w) || expr_contains_aggregate(t))
                || else_expr.as_deref().is_some_and(expr_contains_aggregate)
        }
        // Leaves (Id, Literal, Qualified, ...) and subqueries hold no aggregate
        // that belongs to this clause.
        _ => false,
    }
}

/// The engine's name for a MySQL function that shares its behaviour but not its
/// spelling, or `None` to keep the name as written. `CHAR_LENGTH` maps to
/// `length` because the engine's `length()` counts characters (MySQL's `LENGTH`,
/// which counts bytes, is excluded). `upper_name` must already be uppercased.
fn engine_function_name(upper_name: &str) -> Option<&'static str> {
    Some(match upper_name {
        "IF" => "iif",
        "LCASE" => "lower",
        "UCASE" => "upper",
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => "length",
        "CEILING" => "ceil",
        "POWER" => "pow",
        "DATE" => "date",
        "TIME" => "time",
        "TIMESTAMP" => "datetime",
        "LAST_INSERT_ID" => "last_insert_rowid",
        "JSON_VALID" => "json_valid",
        "REVERSE" => "string_reverse",
        _ => return None,
    })
}

/// Finds the underlying table named by a multi-table `DELETE` target, matching
/// `target` against each `FROM` table's alias (`t AS a` / `t a`) or its own
/// name. Returns the table's qualified name, or `None` if no `FROM` table
/// matches.
fn resolve_delete_target(from: &ast::FromClause, target: &str) -> Option<ast::QualifiedName> {
    let first = std::iter::once(from.select.as_ref());
    let rest = from.joins.iter().map(|join| join.table.as_ref());
    for table in first.chain(rest) {
        let ast::SelectTable::Table(name, alias, _) = table else {
            continue;
        };
        let alias_matches = alias.as_ref().is_some_and(|a| {
            let (ast::As::As(n) | ast::As::Elided(n) | ast::As::ImplicitColumnName(n)) = a;
            n.as_str().eq_ignore_ascii_case(target)
        });
        if alias_matches || name.name.as_str().eq_ignore_ascii_case(target) {
            return Some(name.clone());
        }
    }
    None
}

/// Keywords that begin a table-level constraint or index definition.
fn is_table_constraint_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "CONSTRAINT"
            | "PRIMARY"
            | "UNIQUE"
            | "KEY"
            | "INDEX"
            | "FULLTEXT"
            | "SPATIAL"
            | "FOREIGN"
            | "CHECK"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Result<ast::Stmt> {
        Parser::new(sql.as_bytes())?.parse_statement()
    }

    #[test]
    fn create_index_lowers_to_create_index() {
        // CREATE [UNIQUE] INDEX idx ON tbl (cols) builds the engine's CreateIndex;
        // a prefix length is dropped and the USING clause ignored.
        let cases = [
            ("CREATE INDEX name_idx ON t (name)", false, "name_idx", 1),
            ("CREATE UNIQUE INDEX u ON t (code(10))", true, "u", 1),
            ("CREATE INDEX nc ON t USING BTREE (a, b)", false, "nc", 2),
        ];
        for (sql, want_unique, want_name, want_cols) in cases {
            let ast::Stmt::CreateIndex {
                unique,
                idx_name,
                tbl_name,
                columns,
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to CREATE INDEX");
            };
            assert_eq!(unique, want_unique, "{sql}");
            assert_eq!(idx_name.name.as_str(), want_name, "{sql}");
            assert_eq!(tbl_name.as_str(), "t", "{sql}");
            assert_eq!(columns.len(), want_cols, "{sql}");
        }
    }

    #[test]
    fn create_table_basic() {
        let stmt = parse("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(255) NOT NULL)")
            .expect("should parse");
        let ast::Stmt::CreateTable { tbl_name, body, .. } = stmt else {
            panic!("expected CreateTable");
        };
        assert_eq!(tbl_name.name.as_str(), "users");
        let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
            panic!("expected columns");
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].col_name.as_str(), "id");
        assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INT");
        assert_eq!(columns[1].col_name.as_str(), "name");
    }

    #[test]
    fn not_null_columns_get_implicit_type_default() {
        let stmt = parse(
            "CREATE TABLE t (\
                id INT PRIMARY KEY AUTO_INCREMENT, \
                i INT NOT NULL, \
                v VARCHAR(10) NOT NULL, \
                n INT, \
                e INT NOT NULL DEFAULT 7, \
                dt DATETIME NOT NULL)",
        )
        .expect("should parse");
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            panic!("expected CreateTable");
        };
        let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
            panic!("expected columns");
        };
        let default_of = |name: &str| -> Option<ast::Expr> {
            let col = columns
                .iter()
                .find(|c| c.col_name.as_str() == name)
                .unwrap();
            col.constraints.iter().find_map(|c| match &c.constraint {
                ast::ColumnConstraint::Default(e) => Some((**e).clone()),
                _ => None,
            })
        };
        // A NOT NULL numeric column defaults to 0, a string column to ''.
        assert_eq!(
            default_of("i"),
            Some(ast::Expr::Literal(ast::Literal::Numeric("0".to_string())))
        );
        assert_eq!(
            default_of("v"),
            Some(ast::Expr::Literal(ast::Literal::String("''".to_string())))
        );
        // The AUTO_INCREMENT PRIMARY KEY column, a nullable column, and a
        // DATETIME column (no clean engine default) get no synthesized default.
        for name in ["id", "n", "dt"] {
            assert_eq!(
                default_of(name),
                None,
                "column `{name}` should have no default"
            );
        }
        // An explicit DEFAULT is preserved unchanged.
        assert_eq!(
            default_of("e"),
            Some(ast::Expr::Literal(ast::Literal::Numeric("7".to_string())))
        );
    }

    #[test]
    fn character_columns_default_to_nocase_collation() {
        let stmt = parse(
            "CREATE TABLE t (\
                id INT PRIMARY KEY, \
                name VARCHAR(50), \
                body TEXT, \
                n INT, \
                bn VARCHAR(50) COLLATE utf8mb4_bin, \
                ci VARCHAR(50) COLLATE utf8mb4_general_ci, \
                blob_col BLOB, \
                bin_col VARBINARY(16))",
        )
        .expect("should parse");
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            panic!("expected CreateTable");
        };
        let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
            panic!("expected columns");
        };
        let collation_of = |name: &str| -> Option<String> {
            let col = columns.iter().find(|c| c.col_name.as_str() == name).unwrap();
            col.constraints.iter().find_map(|c| match &c.constraint {
                ast::ColumnConstraint::Collate { collation_name } => {
                    Some(collation_name.as_str().to_string())
                }
                _ => None,
            })
        };
        // Character columns (incl. an explicit `_ci`) get NOCASE; numeric, BLOB,
        // and binary columns get none; an explicit `_bin` stays case-sensitive.
        assert_eq!(collation_of("name").as_deref(), Some("NOCASE"));
        assert_eq!(collation_of("body").as_deref(), Some("NOCASE"));
        assert_eq!(collation_of("ci").as_deref(), Some("NOCASE"));
        assert_eq!(collation_of("n"), None);
        assert_eq!(collation_of("blob_col"), None);
        assert_eq!(collation_of("bin_col"), None);
        assert_eq!(collation_of("bn"), None);
    }

    #[test]
    fn enum_and_set_columns_lower_to_text() {
        // ENUM(...) and SET(...) are stored as strings; both lower to TEXT (and
        // SET, a reserved keyword in the engine, could not otherwise be a type
        // name). The value list is dropped.
        let stmt = parse("CREATE TABLE t (k ENUM('a', 'b'), f SET('x', 'y', 'z'))").unwrap();
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            panic!("expected CreateTable");
        };
        let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
            panic!("expected columns");
        };
        for col in &columns {
            let ty = col.col_type.as_ref().unwrap();
            assert_eq!(ty.name, "TEXT", "column `{}`", col.col_name.as_str());
            assert!(ty.size.is_none());
        }
    }

    #[test]
    fn create_table_as_select() {
        // CREATE TABLE name AS SELECT ... -> an AS-SELECT body.
        for sql in [
            "CREATE TABLE t AS SELECT id, n FROM src WHERE n > 0",
            // The AS keyword is optional.
            "CREATE TABLE t SELECT id FROM src",
        ] {
            let ast::Stmt::CreateTable { tbl_name, body, .. } = parse(sql).unwrap() else {
                panic!("expected CreateTable for `{sql}`");
            };
            assert_eq!(tbl_name.name.as_str(), "t");
            assert!(
                matches!(body, ast::CreateTableBody::AsSelect(_)),
                "expected an AS-SELECT body for `{sql}`"
            );
        }

        // The TEMPORARY and IF NOT EXISTS modifiers compose with AS SELECT.
        let ast::Stmt::CreateTable {
            temporary, body, ..
        } = parse("CREATE TEMPORARY TABLE IF NOT EXISTS t AS SELECT 1").unwrap()
        else {
            panic!("expected CreateTable");
        };
        assert!(temporary);
        assert!(matches!(body, ast::CreateTableBody::AsSelect(_)));

        // The LIKE form has no engine equivalent and is rejected.
        assert!(matches!(
            parse("CREATE TABLE t LIKE src").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn create_table_renders_back_to_sql() {
        let stmt = parse("CREATE TABLE `t` (a BIGINT UNSIGNED, b TEXT)").unwrap();
        // The emitted AST round-trips through the engine's SQL renderer.
        let sql = stmt.to_string();
        assert!(sql.to_uppercase().contains("CREATE TABLE"), "{sql}");
        assert!(sql.contains('a') && sql.contains('b'), "{sql}");
    }

    #[test]
    fn auto_increment_attaches_to_primary_key() {
        let stmt = parse("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY)").unwrap();
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            unreachable!()
        };
        let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
            unreachable!()
        };
        let has_autoinc = columns[0].constraints.iter().any(|c| {
            matches!(
                c.constraint,
                ast::ColumnConstraint::PrimaryKey {
                    auto_increment: true,
                    ..
                }
            )
        });
        assert!(has_autoinc);
        // Retyped to INTEGER so the engine treats it as an auto-assigning rowid alias.
        assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INTEGER");
    }

    #[test]
    fn check_constraints_pass_through_or_fall_back() {
        fn body(sql: &str) -> (Vec<ast::ColumnDefinition>, Vec<ast::NamedTableConstraint>) {
            let ast::Stmt::CreateTable { body, .. } = parse(sql).unwrap() else {
                panic!("expected CREATE TABLE");
            };
            let ast::CreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                ..
            } = body
            else {
                panic!("expected a column/constraint body");
            };
            (columns, constraints)
        }

        // A translatable column-level CHECK is kept as a Check constraint.
        let (columns, _) = body("CREATE TABLE t (id INT PRIMARY KEY, c INT CHECK (c > 0))");
        assert!(columns[1]
            .constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::ColumnConstraint::Check(_))));

        // A translatable table-level CHECK is kept (the symbol name preserved).
        let (_, constraints) =
            body("CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT ab CHECK (a < b))");
        assert!(constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::TableConstraint::Check(_))));

        // A CHECK the front-end cannot translate (an unsupported function) is
        // dropped, so the table still parses with no Check constraint.
        let (columns, _) = body("CREATE TABLE t (id INT PRIMARY KEY, s TEXT CHECK (CRC32(s) = 0))");
        assert!(!columns[1]
            .constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::ColumnConstraint::Check(_))));
    }

    #[test]
    fn auto_increment_table_level_pk_maps_to_rowid_alias() {
        // The WordPress schema shape: AUTO_INCREMENT column, table-level PK.
        let stmt = parse(
            "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, name VARCHAR(50), PRIMARY KEY (id))",
        )
        .unwrap();
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            unreachable!()
        };
        let ast::CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints,
            ..
        } = body
        else {
            unreachable!()
        };
        // The key column is retyped to INTEGER (a rowid alias on the engine).
        assert_eq!(columns[0].col_name.as_str(), "id");
        assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INTEGER");
        // The table-level primary key is marked autoincrement (no id reuse).
        assert!(matches!(
            constraints[0].constraint,
            ast::TableConstraint::PrimaryKey {
                auto_increment: true,
                ..
            }
        ));
    }

    #[test]
    fn auto_increment_on_non_key_column_rejected() {
        assert!(matches!(
            parse("CREATE TABLE t (id INT AUTO_INCREMENT, name VARCHAR(8))").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn auto_increment_on_composite_primary_key_rejected() {
        assert!(matches!(
            parse("CREATE TABLE t (a INT AUTO_INCREMENT, b INT, PRIMARY KEY (a, b))").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn table_level_primary_key() {
        let stmt = parse("CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))").unwrap();
        let ast::Stmt::CreateTable { body, .. } = stmt else {
            unreachable!()
        };
        let ast::CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints,
            ..
        } = body
        else {
            unreachable!()
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(constraints.len(), 1);
    }

    #[test]
    fn select_star_from_table() {
        let stmt = parse("SELECT * FROM users").unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { columns, from, .. } = select.body.select else {
            panic!("expected OneSelect::Select");
        };
        assert_eq!(columns.len(), 1);
        assert!(matches!(columns[0], ast::ResultColumn::Star));
        assert!(from.is_some());
    }

    #[test]
    fn select_constant_without_from() {
        let stmt = parse("SELECT 1").unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { from, .. } = select.body.select else {
            panic!("expected OneSelect::Select");
        };
        assert!(from.is_none());
    }

    #[test]
    fn non_aggregate_having_folds_into_where() {
        fn one_select(sql: &str) -> (Option<Box<ast::Expr>>, Option<ast::GroupBy>) {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT");
            };
            let ast::OneSelect::Select {
                where_clause,
                group_by,
                ..
            } = select.body.select
            else {
                panic!("expected OneSelect::Select");
            };
            (where_clause, group_by)
        }

        // A non-aggregate HAVING with no GROUP BY folds into WHERE; the GROUP BY
        // is gone and the WHERE is an AND of the original WHERE and the HAVING.
        let (where_clause, group_by) = one_select("SELECT a FROM t WHERE a > 0 HAVING b < 5");
        assert!(group_by.is_none());
        assert!(matches!(
            where_clause.as_deref(),
            Some(ast::Expr::Binary(_, ast::Operator::And, _))
        ));

        // With no prior WHERE, the HAVING becomes the whole WHERE.
        let (where_clause, group_by) = one_select("SELECT a FROM t HAVING a > 0");
        assert!(group_by.is_none());
        assert!(where_clause.is_some());

        // An aggregate HAVING is left in place (the engine handles the
        // whole-table aggregate).
        let (_, group_by) = one_select("SELECT COUNT(*) FROM t HAVING COUNT(*) > 2");
        let gb = group_by.expect("aggregate HAVING stays as a GROUP BY");
        assert!(gb.exprs.is_empty());
        assert!(gb.having.is_some());

        // A real GROUP BY with a HAVING is untouched.
        let (_, group_by) = one_select("SELECT a FROM t GROUP BY a HAVING a > 0");
        assert!(group_by.is_some_and(|gb| !gb.exprs.is_empty()));
    }

    #[test]
    fn hex_literal_lowers_to_blob() {
        // `0x41` and `X'41'` lower to the engine's blob literal with the same
        // hex digits.
        for sql in ["0x41", "X'41'", "x'41'"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::Literal(ast::Literal::Blob(b)) if b == "41"),
                "expected `{sql}` to lower to Blob(\"41\")"
            );
        }
        // An odd-length `0x` literal is left-padded to even.
        assert!(matches!(
            parse_expr("0xABC").unwrap(),
            ast::Expr::Literal(ast::Literal::Blob(b)) if b == "0ABC"
        ));
    }

    #[test]
    fn unaliased_expression_takes_verbatim_source_label() {
        // Returns the result columns of a parsed SELECT.
        fn columns(sql: &str) -> Vec<ast::ResultColumn> {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT");
            };
            let ast::OneSelect::Select { columns, .. } = select.body.select else {
                panic!("expected OneSelect::Select");
            };
            columns
        }
        // The implicit label of the n-th column, or None.
        fn label(col: &ast::ResultColumn) -> Option<&str> {
            match col {
                ast::ResultColumn::Expr(_, Some(ast::As::ImplicitColumnName(n))) => Some(n.as_str()),
                _ => None,
            }
        }

        // A function call / arithmetic gets the verbatim source text, spacing
        // preserved, even across the front-end's lowering of LENGTH.
        let cols = columns("SELECT UPPER('a'),  a +  b , LENGTH('x') FROM t");
        assert_eq!(label(&cols[0]), Some("UPPER('a')"));
        assert_eq!(label(&cols[1]), Some("a +  b"));
        assert_eq!(label(&cols[2]), Some("LENGTH('x')"));

        // A bare/qualified column reference and a numeric/NULL literal get no
        // implicit label (the engine labels them like MySQL: the column name /
        // the value).
        let cols = columns("SELECT a, t.b, 5, NULL FROM t");
        for col in &cols {
            assert_eq!(label(col), None);
        }

        // A string literal is labelled by its decoded value, and a hex literal
        // by its verbatim source.
        let cols = columns("SELECT 'hi', 'it''s', 0x41, X'4142' FROM t");
        assert_eq!(label(&cols[0]), Some("hi"));
        assert_eq!(label(&cols[1]), Some("it's"));
        assert_eq!(label(&cols[2]), Some("0x41"));
        assert_eq!(label(&cols[3]), Some("X'4142'"));

        // An explicit alias is kept as-is (not an implicit label).
        let cols = columns("SELECT UPPER('a') AS up FROM t");
        assert!(matches!(
            &cols[0],
            ast::ResultColumn::Expr(_, Some(ast::As::As(n))) if n.as_str() == "up"
        ));
    }

    #[test]
    fn select_columns_where_order_limit() {
        let stmt =
            parse("SELECT id, name AS who FROM users WHERE age >= 18 ORDER BY id DESC LIMIT 5")
                .unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        assert_eq!(select.order_by.len(), 1);
        assert_eq!(select.order_by[0].order, Some(ast::SortOrder::Desc));
        assert!(select.limit.is_some());
        let ast::OneSelect::Select {
            columns,
            where_clause,
            ..
        } = select.body.select
        else {
            panic!("expected OneSelect::Select");
        };
        assert_eq!(columns.len(), 2);
        assert!(where_clause.is_some());
    }

    #[test]
    fn table_statement_lowers_to_select_star() {
        // `TABLE t` -> `SELECT * FROM t`.
        let ast::Stmt::Select(select) = parse("TABLE t").unwrap() else {
            panic!("expected TABLE to lower to a SELECT");
        };
        let ast::OneSelect::Select { columns, from, .. } = &select.body.select else {
            panic!("expected OneSelect::Select");
        };
        assert!(matches!(columns.as_slice(), [ast::ResultColumn::Star]));
        let Some(from) = from else {
            panic!("expected a FROM clause");
        };
        assert!(matches!(from.select.as_ref(), ast::SelectTable::Table(t, None, _) if t.name.as_str() == "t"));

        // The trailing ORDER BY / LIMIT are honored.
        let ast::Stmt::Select(select) = parse("TABLE t ORDER BY id DESC LIMIT 2, 3").unwrap() else {
            unreachable!()
        };
        assert_eq!(select.order_by.len(), 1);
        assert_eq!(select.order_by[0].order, Some(ast::SortOrder::Desc));
        let limit = select.limit.unwrap();
        assert!(limit.offset.is_some());
    }

    #[test]
    fn from_dual_is_dropped() {
        // `FROM DUAL` is MySQL's dummy table; it lowers to a FROM-less select.
        for sql in ["SELECT 1 FROM DUAL", "SELECT 1 FROM dual WHERE 1 = 1"] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select { from, .. } = select.body.select else {
                panic!("expected OneSelect::Select for `{sql}`");
            };
            assert!(from.is_none(), "expected `{sql}` to drop the FROM clause");
        }

        // A real table is kept, and DUAL with an alias is treated as a table.
        for sql in ["SELECT 1 FROM users", "SELECT 1 FROM dual d"] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                unreachable!()
            };
            let ast::OneSelect::Select { from, .. } = select.body.select else {
                unreachable!()
            };
            assert!(from.is_some(), "expected `{sql}` to keep the FROM clause");
        }
    }

    #[test]
    fn select_limit_offset_forms() {
        // `LIMIT count OFFSET offset`
        let a = parse("SELECT * FROM t LIMIT 10 OFFSET 5").unwrap();
        // `LIMIT offset, count`
        let b = parse("SELECT * FROM t LIMIT 5, 10").unwrap();
        for stmt in [a, b] {
            let ast::Stmt::Select(select) = stmt else {
                unreachable!()
            };
            let limit = select.limit.unwrap();
            assert!(limit.offset.is_some());
        }
    }

    #[test]
    fn limit_literal_overflowing_i64_is_clamped() {
        // MySQL's `LIMIT 18446744073709551615` ("all remaining rows") overflows
        // the engine's signed 64-bit bound, so it is clamped to i64::MAX.
        let max = i64::MAX.to_string();
        let ast::Stmt::Select(select) =
            parse("SELECT * FROM t LIMIT 2, 18446744073709551615").unwrap()
        else {
            unreachable!()
        };
        let limit = select.limit.unwrap();
        assert_eq!(
            *limit.expr,
            ast::Expr::Literal(ast::Literal::Numeric(max.clone()))
        );
        // The in-range offset is left untouched.
        assert_eq!(
            *limit.offset.unwrap(),
            ast::Expr::Literal(ast::Literal::Numeric("2".to_string()))
        );

        // An in-range LIMIT is unchanged.
        let ast::Stmt::Select(select) = parse("SELECT * FROM t LIMIT 50").unwrap() else {
            unreachable!()
        };
        assert_eq!(
            *select.limit.unwrap().expr,
            ast::Expr::Literal(ast::Literal::Numeric("50".to_string()))
        );

        // The clamp also applies to UPDATE/DELETE row limits.
        let ast::Stmt::Delete { limit, .. } =
            parse("DELETE FROM t LIMIT 18446744073709551615").unwrap()
        else {
            unreachable!()
        };
        assert_eq!(
            *limit.unwrap().expr,
            ast::Expr::Literal(ast::Literal::Numeric(max))
        );
    }

    #[test]
    fn select_renders_back_to_sql() {
        let sql = parse("SELECT id, name FROM users WHERE id = 1 ORDER BY id LIMIT 2")
            .unwrap()
            .to_string();
        let upper = sql.to_uppercase();
        assert!(upper.contains("SELECT") && upper.contains("FROM"), "{sql}");
        assert!(
            upper.contains("WHERE") && upper.contains("ORDER BY"),
            "{sql}"
        );
    }

    #[test]
    fn select_locking_clause_is_accepted_and_ignored() {
        // The trailing row-locking clause parses (and is dropped); the SELECT is
        // otherwise unchanged.
        for sql in [
            "SELECT a FROM t FOR UPDATE",
            "SELECT a FROM t FOR SHARE",
            "SELECT a FROM t LOCK IN SHARE MODE",
            "SELECT a FROM t WHERE a = 1 ORDER BY a FOR UPDATE",
            // The `OF tbl [, tbl] ...` and `NOWAIT` / `SKIP LOCKED` refinements.
            "SELECT a FROM t FOR UPDATE NOWAIT",
            "SELECT a FROM t FOR UPDATE SKIP LOCKED",
            "SELECT a FROM t FOR SHARE NOWAIT",
            "SELECT a FROM t FOR SHARE SKIP LOCKED",
            "SELECT a FROM t FOR UPDATE OF t",
            "SELECT a FROM t AS x FOR UPDATE OF x, t",
            "SELECT a FROM t FOR UPDATE OF t NOWAIT",
        ] {
            assert!(
                matches!(parse(sql).unwrap(), ast::Stmt::Select(_)),
                "expected `{sql}` to parse as a SELECT"
            );
        }
        // A `FOR`-prefixed clause that is not a locking read is still rejected
        // (the stray `FOR` is left for the end-of-input check).
        assert!(parse("SELECT a FROM t FOR somethingelse").is_err());
        // A trailing token after a valid locking clause is still rejected.
        assert!(parse("SELECT a FROM t FOR UPDATE GARBAGE").is_err());
    }

    #[test]
    fn select_distinct() {
        let stmt = parse("SELECT DISTINCT cat FROM t").unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { distinctness, .. } = select.body.select else {
            panic!("expected OneSelect::Select");
        };
        assert_eq!(distinctness, Some(ast::Distinctness::Distinct));

        // `ALL` is the default quantifier and yields no DISTINCT.
        let ast::Stmt::Select(select) = parse("SELECT ALL cat FROM t").unwrap() else {
            unreachable!()
        };
        let ast::OneSelect::Select { distinctness, .. } = select.body.select else {
            unreachable!()
        };
        assert_eq!(distinctness, None);
    }

    #[test]
    fn select_inner_join_with_aliases() {
        let stmt = parse(
            "SELECT t.*, tt.* FROM terms AS t INNER JOIN term_taxonomy AS tt ON t.id = tt.term_id WHERE t.id = 1",
        )
        .unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { from, .. } = select.body.select else {
            panic!("expected a plain select");
        };
        let from = from.expect("a FROM clause");
        assert_eq!(from.joins.len(), 1);
        let join = &from.joins[0];
        assert!(matches!(
            join.operator,
            ast::JoinOperator::TypedJoin(Some(t)) if t == ast::JoinType::INNER
        ));
        assert!(matches!(join.constraint, Some(ast::JoinConstraint::On(_))));
    }

    #[test]
    fn select_comma_join() {
        // `FROM a, b` is a comma (cross) join with no ON constraint; the WHERE
        // clause supplies the condition.
        let stmt = parse("SELECT a.x FROM a, b, c WHERE a.x = b.y").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { from, .. } = s.body.select else {
            unreachable!()
        };
        let from = from.unwrap();
        assert_eq!(from.joins.len(), 2);
        for join in &from.joins {
            assert!(matches!(join.operator, ast::JoinOperator::Comma));
            assert!(join.constraint.is_none());
        }
    }

    #[test]
    fn select_plain_and_left_join() {
        // Plain JOIN is INNER; LEFT JOIN sets LEFT|OUTER.
        let plain = parse("SELECT * FROM a JOIN b ON a.id = b.id").unwrap();
        let ast::Stmt::Select(s) = plain else {
            unreachable!()
        };
        let ast::OneSelect::Select { from, .. } = s.body.select else {
            unreachable!()
        };
        assert!(matches!(
            from.unwrap().joins[0].operator,
            ast::JoinOperator::TypedJoin(Some(t)) if t == ast::JoinType::INNER
        ));

        let left = parse("SELECT * FROM a LEFT JOIN b ON a.id = b.id").unwrap();
        let ast::Stmt::Select(s) = left else {
            unreachable!()
        };
        let ast::OneSelect::Select { from, .. } = s.body.select else {
            unreachable!()
        };
        assert!(matches!(
            from.unwrap().joins[0].operator,
            ast::JoinOperator::TypedJoin(Some(t)) if t == ast::JoinType::LEFT | ast::JoinType::OUTER
        ));
    }

    #[test]
    fn select_bare_table_alias() {
        let stmt = parse("SELECT p.id FROM posts p WHERE p.id = 1").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { from, .. } = s.body.select else {
            unreachable!()
        };
        let from = from.unwrap();
        let ast::SelectTable::Table(_, Some(alias), _) = from.select.as_ref() else {
            panic!("expected an aliased table");
        };
        assert!(matches!(alias, ast::As::Elided(n) if n.as_str() == "p"));
    }

    #[test]
    fn select_bare_column_alias() {
        let stmt = parse("SELECT id user_id, n AS amount FROM t").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { columns, from, .. } = s.body.select else {
            unreachable!()
        };
        // Two columns; FROM was recognised, not swallowed as an alias.
        assert_eq!(columns.len(), 2);
        assert!(from.is_some());
        let ast::ResultColumn::Expr(_, Some(ast::As::Elided(n))) = &columns[0] else {
            panic!("expected a bare (elided) alias on the first column");
        };
        assert_eq!(n.as_str(), "user_id");
        assert!(matches!(
            &columns[1],
            ast::ResultColumn::Expr(_, Some(ast::As::As(_)))
        ));
    }

    #[test]
    fn sql_calc_found_rows_modifier_is_stripped() {
        // The modifier is consumed; the SELECT parses as if it were not there.
        let stmt = parse("SELECT SQL_CALC_FOUND_ROWS a FROM t LIMIT 2").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { columns, from, .. } = s.body.select else {
            panic!("expected a plain select body");
        };
        assert_eq!(columns.len(), 1);
        assert!(from.is_some());
        assert!(s.limit.is_some());
    }

    #[test]
    fn select_string_literal_alias() {
        // MySQL allows `expr AS 'name'`; the string text becomes the alias.
        let stmt = parse("SELECT a AS 'row id' FROM t").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { columns, .. } = s.body.select else {
            unreachable!()
        };
        let ast::ResultColumn::Expr(_, Some(ast::As::As(n))) = &columns[0] else {
            panic!("expected an `AS` alias");
        };
        assert_eq!(n.as_str(), "row id");
    }

    #[test]
    fn select_no_alias_when_clause_keyword_follows() {
        // `FROM`/`WHERE`/`ORDER` after an expression are clauses, not aliases.
        let stmt = parse("SELECT a FROM t WHERE a = 1 ORDER BY a").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { columns, .. } = s.body.select else {
            unreachable!()
        };
        assert!(matches!(columns[0], ast::ResultColumn::Expr(_, None)));
    }

    #[test]
    fn exists_subquery() {
        // EXISTS (SELECT ...) parses as an Exists predicate.
        assert!(matches!(
            parse_expr("EXISTS (SELECT 1 FROM b WHERE b.ref = a.id)").unwrap(),
            ast::Expr::Exists(_)
        ));

        // NOT EXISTS wraps the Exists in a unary NOT.
        let ast::Expr::Unary(ast::UnaryOperator::Not, inner) =
            parse_expr("NOT EXISTS (SELECT 1 FROM b)").unwrap()
        else {
            panic!("expected NOT(Exists)");
        };
        assert!(matches!(inner.as_ref(), ast::Expr::Exists(_)));

        // `exists` not followed by `(` is an ordinary column reference.
        assert!(matches!(parse_expr("exists").unwrap(), ast::Expr::Id(_)));
    }

    #[test]
    fn in_subquery() {
        // `IN (SELECT ...)` parses as an InSelect with the subquery body.
        let stmt = parse("SELECT id FROM a WHERE id IN (SELECT ref FROM b WHERE x = 1)").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { where_clause, .. } = s.body.select else {
            unreachable!()
        };
        let where_clause = where_clause.unwrap();
        let ast::Expr::InSelect { not, rhs, .. } = where_clause.as_ref() else {
            panic!("expected an IN-subquery in WHERE");
        };
        assert!(!not);
        // The subquery itself is a parsed SELECT with a FROM and WHERE.
        let ast::OneSelect::Select {
            from, where_clause, ..
        } = &rhs.body.select
        else {
            unreachable!()
        };
        assert!(from.is_some());
        assert!(where_clause.is_some());

        // NOT IN (SELECT ...) carries the negation.
        let stmt = parse("SELECT id FROM a WHERE id NOT IN (SELECT ref FROM b)").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { where_clause, .. } = s.body.select else {
            unreachable!()
        };
        assert!(matches!(
            where_clause.unwrap().as_ref(),
            ast::Expr::InSelect { not: true, .. }
        ));

        // A plain value list still parses as InList.
        assert!(matches!(
            parse_expr("id IN (1, 2, 3)").unwrap(),
            ast::Expr::InList { .. }
        ));
    }

    #[test]
    fn unary_minus_on_expression() {
        // `-a` (non-literal) is a unary Negative operator.
        assert!(matches!(
            parse_expr("-a").unwrap(),
            ast::Expr::Unary(ast::UnaryOperator::Negative, _)
        ));
        // `+a` is a unary Positive operator.
        assert!(matches!(
            parse_expr("+a").unwrap(),
            ast::Expr::Unary(ast::UnaryOperator::Positive, _)
        ));
        // Negating a function call and a parenthesized expression.
        assert!(matches!(
            parse_expr("-ABS(a)").unwrap(),
            ast::Expr::Unary(ast::UnaryOperator::Negative, _)
        ));
        assert!(matches!(
            parse_expr("-(a + 1)").unwrap(),
            ast::Expr::Unary(ast::UnaryOperator::Negative, _)
        ));
        // A signed numeric literal is still folded into the literal, not a Unary.
        assert!(matches!(
            parse_expr("-5").unwrap(),
            ast::Expr::Literal(ast::Literal::Numeric(ref n)) if n == "-5"
        ));
        // Unary minus binds tighter than `*`: `-a * b` is `(-a) * b`.
        let ast::Expr::Binary(lhs, ast::Operator::Multiply, _) = parse_expr("-a * b").unwrap()
        else {
            panic!("expected a multiplication at the top");
        };
        assert!(matches!(
            lhs.as_ref(),
            ast::Expr::Unary(ast::UnaryOperator::Negative, _)
        ));
    }

    #[test]
    fn temporal_literals_lower_to_date_functions() {
        // DATE/TIME/TIMESTAMP 'str' -> date/time/datetime('str').
        for (sql, func) in [
            ("DATE '2026-03-01'", "date"),
            ("TIME '10:30:00'", "time"),
            ("TIMESTAMP '2026-03-01 09:00:00'", "datetime"),
        ] {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a function call");
            };
            assert_eq!(name.as_str(), func, "{sql}");
            assert!(matches!(
                args[0].as_ref(),
                ast::Expr::Literal(ast::Literal::String(_))
            ));
        }

        // The keyword followed by `(` is still the date/time function, and a
        // keyword not before a string is an ordinary identifier.
        assert!(matches!(
            parse_expr("DATE(d)").unwrap(),
            ast::Expr::FunctionCall { .. }
        ));
        assert!(matches!(parse_expr("date").unwrap(), ast::Expr::Id(_)));
    }

    #[test]
    fn quantified_comparison_lowers_to_in() {
        // `= ANY` / `= SOME` is IN; `<> ALL` / `!= ALL` is NOT IN.
        for sql in ["a = ANY (SELECT b FROM s)", "a = SOME (SELECT b FROM s)"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::InSelect { not: false, .. }),
                "expected `{sql}` to lower to IN (subquery)"
            );
        }
        for sql in ["a <> ALL (SELECT b FROM s)", "a != ALL (SELECT b FROM s)"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::InSelect { not: true, .. }),
                "expected `{sql}` to lower to NOT IN (subquery)"
            );
        }

        // Other operator/quantifier pairs are rejected (no clean IN equivalent).
        assert!(parse_expr("a > ALL (SELECT b FROM s)").is_err());
        assert!(parse_expr("a = ALL (SELECT b FROM s)").is_err());
        assert!(parse_expr("a <> ANY (SELECT b FROM s)").is_err());

        // `ANY` only quantifies immediately before `(`, so a column named `any`
        // is still an ordinary reference.
        assert!(matches!(
            parse_expr("a = any").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Equals, _)
        ));
    }

    #[test]
    fn scalar_subquery_in_expression() {
        // `(SELECT ...)` in an expression parses as a scalar subquery.
        assert!(matches!(
            parse_expr("(SELECT COUNT(*) FROM t WHERE t.a = u.b)").unwrap(),
            ast::Expr::Subquery(_)
        ));
        // It composes in a comparison.
        let expr = parse_expr("(SELECT MIN(x) FROM t) = 5").unwrap();
        let ast::Expr::Binary(lhs, ast::Operator::Equals, _) = expr else {
            panic!("expected a comparison with a subquery on the left");
        };
        assert!(matches!(*lhs, ast::Expr::Subquery(_)));
        // A plain parenthesized expression is still parenthesized, not a subquery.
        assert!(matches!(
            parse_expr("(1 + 2)").unwrap(),
            ast::Expr::Parenthesized(_)
        ));
    }

    #[test]
    fn empty_in_list_folds_to_constant() {
        // `x IN ()` folds to 0 and `x NOT IN ()` to 1 (MySQL semantics); the
        // engine has no empty-list IN.
        assert!(matches!(
            parse_expr("id IN ()").unwrap(),
            ast::Expr::Literal(ast::Literal::Numeric(ref n)) if n == "0"
        ));
        assert!(matches!(
            parse_expr("id NOT IN ()").unwrap(),
            ast::Expr::Literal(ast::Literal::Numeric(ref n)) if n == "1"
        ));
    }

    #[test]
    fn select_unsupported_variants() {
        for sql in [
            "SELECT * FROM a FULL JOIN b ON a.id = b.id",
            "SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id",
            "SELECT * FROM a LEFT JOIN b",
            "SELECT * FROM (SELECT 1)",
            "SELECT a FROM t GROUP BY 1",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn distinctrow_is_a_distinct_synonym() {
        for sql in ["SELECT DISTINCTROW a FROM t", "SELECT DISTINCT a FROM t"] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select { distinctness, .. } = select.body.select else {
                panic!("expected OneSelect::Select");
            };
            assert_eq!(distinctness, Some(ast::Distinctness::Distinct), "{sql}");
        }
    }

    #[test]
    fn join_using_parses() {
        // JOIN ... USING (cols) builds the engine's USING constraint.
        let ast::Stmt::Select(select) = parse("SELECT * FROM a JOIN b USING (id, ref)").unwrap()
        else {
            panic!("expected a SELECT");
        };
        let ast::OneSelect::Select {
            from: Some(from), ..
        } = &select.body.select
        else {
            panic!("expected a FROM clause");
        };
        let Some(ast::JoinConstraint::Using(cols)) = &from.joins[0].constraint else {
            panic!("expected a USING constraint");
        };
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].as_str(), "id");
        assert_eq!(cols[1].as_str(), "ref");
    }

    #[test]
    fn having_without_group_by_parses() {
        // `HAVING` with no `GROUP BY` becomes an empty GROUP BY carrying the
        // HAVING (the whole result is one group).
        let ast::Stmt::Select(select) =
            parse("SELECT COUNT(*) FROM t HAVING COUNT(*) > 2").unwrap()
        else {
            panic!("expected a SELECT");
        };
        let ast::OneSelect::Select {
            group_by: Some(group_by),
            ..
        } = select.body.select
        else {
            panic!("expected a GROUP BY carrying the HAVING");
        };
        assert!(group_by.exprs.is_empty());
        assert!(group_by.having.is_some());
    }

    #[test]
    fn having_on_aggregate_alias_is_kept_not_folded() {
        // A HAVING that filters on an aggregate via its SELECT-list alias stays a
        // standalone HAVING (an aggregate cannot move into WHERE), whereas a
        // non-aggregate HAVING is folded into WHERE.
        let group_by_of = |sql: &str| {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select {
                group_by,
                where_clause,
                ..
            } = select.body.select
            else {
                panic!("expected a plain SELECT for `{sql}`");
            };
            (group_by, where_clause)
        };

        // `HAVING c > 3` where `c` is `COUNT(*)`: kept as a HAVING, no WHERE.
        let (group_by, where_clause) = group_by_of("SELECT COUNT(*) c FROM t HAVING c > 3");
        let group_by = group_by.expect("the aggregate-alias HAVING should be kept");
        assert!(group_by.exprs.is_empty());
        assert!(group_by.having.is_some());
        assert!(where_clause.is_none());

        // `HAVING d > 3` where `d` is the non-aggregate `id * 2`: folded into WHERE.
        let (group_by, where_clause) = group_by_of("SELECT id * 2 d FROM t HAVING d > 3");
        assert!(group_by.is_none(), "a non-aggregate HAVING should fold away");
        assert!(where_clause.is_some(), "it should become a WHERE filter");
    }

    #[test]
    fn select_noop_modifiers_are_consumed() {
        // The optimizer/cache hints are consumed and the query parses to the
        // same thing as without them.
        let baseline = parse("SELECT id FROM t WHERE id = 1").unwrap();
        for sql in [
            "SELECT SQL_NO_CACHE id FROM t WHERE id = 1",
            "SELECT HIGH_PRIORITY id FROM t WHERE id = 1",
            "SELECT SQL_BIG_RESULT SQL_BUFFER_RESULT id FROM t WHERE id = 1",
            "SELECT SQL_NO_CACHE SQL_CALC_FOUND_ROWS id FROM t WHERE id = 1",
        ] {
            assert_eq!(parse(sql).unwrap(), baseline, "for `{sql}`");
        }
    }

    #[test]
    fn natural_join_parses() {
        // NATURAL [LEFT|RIGHT] JOIN carries the NATURAL flag and takes no
        // constraint.
        for (sql, side) in [
            ("SELECT * FROM a NATURAL JOIN b", ast::JoinType::INNER),
            ("SELECT * FROM a NATURAL LEFT JOIN b", ast::JoinType::LEFT),
            (
                "SELECT * FROM a NATURAL RIGHT OUTER JOIN b",
                ast::JoinType::RIGHT,
            ),
        ] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select {
                from: Some(from), ..
            } = &select.body.select
            else {
                panic!("expected a FROM clause for `{sql}`");
            };
            let ast::JoinOperator::TypedJoin(Some(t)) = from.joins[0].operator else {
                panic!("expected a typed join for `{sql}`");
            };
            assert!(t.contains(ast::JoinType::NATURAL), "for `{sql}`");
            assert!(t.contains(side), "for `{sql}`");
            assert!(from.joins[0].constraint.is_none(), "for `{sql}`");
        }
    }

    #[test]
    fn cross_and_straight_join_parse() {
        // CROSS JOIN takes no constraint and carries the CROSS flag.
        let ast::Stmt::Select(select) = parse("SELECT * FROM a CROSS JOIN b").unwrap() else {
            panic!("expected a SELECT");
        };
        let ast::OneSelect::Select {
            from: Some(from), ..
        } = &select.body.select
        else {
            panic!("expected a FROM clause");
        };
        let ast::JoinOperator::TypedJoin(Some(t)) = from.joins[0].operator else {
            panic!("expected a typed join");
        };
        assert!(t.contains(ast::JoinType::CROSS));
        assert!(from.joins[0].constraint.is_none());

        // STRAIGHT_JOIN lowers to a plain INNER join (the hint is dropped).
        let ast::Stmt::Select(select) =
            parse("SELECT * FROM a STRAIGHT_JOIN b ON a.id = b.id").unwrap()
        else {
            panic!("expected a SELECT");
        };
        let ast::OneSelect::Select {
            from: Some(from), ..
        } = &select.body.select
        else {
            panic!("expected a FROM clause");
        };
        let ast::JoinOperator::TypedJoin(Some(t)) = from.joins[0].operator else {
            panic!("expected a typed join");
        };
        assert!(t.contains(ast::JoinType::INNER) && !t.contains(ast::JoinType::CROSS));
    }

    #[test]
    fn index_hints_are_parsed_and_ignored() {
        // Index hints after a table reference parse and are discarded -- the
        // statement is otherwise identical to the unhinted form.
        let baseline = parse("SELECT id FROM t WHERE c > 1").unwrap();
        for hint in [
            "USE INDEX (PRIMARY)",
            "FORCE INDEX (a, b)",
            "IGNORE INDEX (a)",
            "USE KEY (a)",
            "USE INDEX ()",
            "FORCE INDEX FOR ORDER BY (a)",
            "USE INDEX FOR JOIN (a) IGNORE INDEX FOR GROUP BY (b)",
        ] {
            let sql = format!("SELECT id FROM t {hint} WHERE c > 1");
            assert_eq!(parse(&sql).unwrap(), baseline, "for `{sql}`");
        }

        // Hints attach to joined tables too.
        assert!(
            parse("SELECT * FROM a USE INDEX (x) JOIN b FORCE INDEX (y) ON a.id = b.id").is_ok()
        );
    }

    #[test]
    fn inner_join_without_condition_is_a_cross_join() {
        // A plain `JOIN` / `INNER JOIN` / `STRAIGHT_JOIN` with no ON/USING is a
        // cross join (no constraint), like an explicit CROSS JOIN. MySQL allows
        // this (the predicate usually lives in WHERE).
        for sql in [
            "SELECT * FROM a JOIN b",
            "SELECT * FROM a INNER JOIN b",
            "SELECT * FROM a STRAIGHT_JOIN b",
            "SELECT * FROM a JOIN b WHERE a.id = b.id",
        ] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select {
                from: Some(from), ..
            } = &select.body.select
            else {
                panic!("expected a FROM clause for `{sql}`");
            };
            assert!(
                from.joins[0].constraint.is_none(),
                "expected no join constraint for `{sql}`"
            );
        }

        // An OUTER (LEFT/RIGHT) join still requires a condition.
        for sql in [
            "SELECT * FROM a LEFT JOIN b",
            "SELECT * FROM a RIGHT JOIN b",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to require a condition"
            );
        }
    }

    #[test]
    fn right_join_parses() {
        // RIGHT [OUTER] JOIN parses into the engine's RIGHT join type.
        for sql in [
            "SELECT * FROM a RIGHT JOIN b ON a.id = b.id",
            "SELECT * FROM a RIGHT OUTER JOIN b ON a.id = b.id",
        ] {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select {
                from: Some(from), ..
            } = &select.body.select
            else {
                panic!("expected a FROM clause for `{sql}`");
            };
            let ast::JoinOperator::TypedJoin(Some(t)) = from.joins[0].operator else {
                panic!("expected a typed join for `{sql}`");
            };
            assert!(t.contains(ast::JoinType::RIGHT), "for `{sql}`");
        }
    }

    #[test]
    fn derived_table_in_from() {
        let stmt =
            parse("SELECT s.id FROM (SELECT id FROM t WHERE n > 1) s WHERE s.id > 0").unwrap();
        let ast::Stmt::Select(sel) = stmt else {
            unreachable!()
        };
        let ast::OneSelect::Select { from, .. } = sel.body.select else {
            unreachable!()
        };
        let from = from.unwrap();
        let ast::SelectTable::Select(_, Some(alias)) = from.select.as_ref() else {
            panic!("expected a derived table with an alias");
        };
        assert!(matches!(alias, ast::As::Elided(n) if n.as_str() == "s"));

        // A derived table without an alias is rejected (MySQL requires one).
        assert!(matches!(
            parse("SELECT * FROM (SELECT 1) WHERE 1 = 1").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn select_union_compounds() {
        // UNION / UNION ALL / INTERSECT / EXCEPT, with a trailing ORDER BY that
        // applies to the whole result.
        let stmt = parse("SELECT a FROM t UNION SELECT b FROM u ORDER BY a").unwrap();
        let ast::Stmt::Select(s) = stmt else {
            unreachable!()
        };
        assert_eq!(s.body.compounds.len(), 1);
        assert!(matches!(
            s.body.compounds[0].operator,
            ast::CompoundOperator::Union
        ));
        assert_eq!(s.order_by.len(), 1);

        for (sql, op) in [
            (
                "SELECT a FROM t UNION ALL SELECT b FROM u",
                ast::CompoundOperator::UnionAll,
            ),
            (
                "SELECT a FROM t INTERSECT SELECT b FROM u",
                ast::CompoundOperator::Intersect,
            ),
            (
                "SELECT a FROM t EXCEPT SELECT b FROM u",
                ast::CompoundOperator::Except,
            ),
        ] {
            let ast::Stmt::Select(s) = parse(sql).unwrap() else {
                unreachable!()
            };
            assert_eq!(s.body.compounds[0].operator, op, "{sql}");
        }

        // A chain of UNIONs accumulates into the compounds list.
        let ast::Stmt::Select(s) =
            parse("SELECT a FROM t UNION SELECT b FROM u UNION ALL SELECT c FROM v").unwrap()
        else {
            unreachable!()
        };
        assert_eq!(s.body.compounds.len(), 2);

        // The explicit `DISTINCT` quantifier is the default, so `UNION DISTINCT`
        // == `UNION` (and likewise for INTERSECT / EXCEPT).
        for (sql, op) in [
            (
                "SELECT a FROM t UNION DISTINCT SELECT b FROM u",
                ast::CompoundOperator::Union,
            ),
            (
                "SELECT a FROM t INTERSECT DISTINCT SELECT b FROM u",
                ast::CompoundOperator::Intersect,
            ),
            (
                "SELECT a FROM t EXCEPT DISTINCT SELECT b FROM u",
                ast::CompoundOperator::Except,
            ),
        ] {
            let ast::Stmt::Select(s) = parse(sql).unwrap() else {
                unreachable!()
            };
            assert_eq!(s.body.compounds.len(), 1, "{sql}");
            assert_eq!(s.body.compounds[0].operator, op, "{sql}");
        }
        assert_eq!(
            parse("SELECT a FROM t UNION DISTINCT SELECT b FROM u").unwrap(),
            parse("SELECT a FROM t UNION SELECT b FROM u").unwrap()
        );
    }

    #[test]
    fn aggregate_over_window_clause() {
        // An aggregate with `OVER (...)` carries a window spec. `OVER ()`.
        let ast::Expr::FunctionCall { filter_over, .. } =
            parse_expr("SUM(amt) OVER ()").unwrap()
        else {
            panic!("expected a function call");
        };
        let Some(ast::Over::Window(w)) = filter_over.over_clause else {
            panic!("expected an OVER window clause");
        };
        assert!(w.partition_by.is_empty() && w.order_by.is_empty());

        // PARTITION BY and ORDER BY are captured.
        let ast::Expr::FunctionCall { filter_over, .. } =
            parse_expr("SUM(amt) OVER (PARTITION BY g, h ORDER BY id DESC)").unwrap()
        else {
            panic!("expected a function call");
        };
        let Some(ast::Over::Window(w)) = filter_over.over_clause else {
            panic!("expected an OVER window clause");
        };
        assert_eq!(w.partition_by.len(), 2);
        assert_eq!(w.order_by.len(), 1);

        // `COUNT(*) OVER ()` carries the window on the star form too.
        let ast::Expr::FunctionCallStar { filter_over, .. } =
            parse_expr("COUNT(*) OVER (PARTITION BY g)").unwrap()
        else {
            panic!("expected a star function call");
        };
        assert!(matches!(filter_over.over_clause, Some(ast::Over::Window(_))));

        // An explicit frame and a named window are rejected.
        assert!(parse_expr("SUM(amt) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)").is_err());
        assert!(parse_expr("SUM(amt) OVER w").is_err());

        // A scalar function does not take a window (the OVER is left unparsed).
        let mut p = Parser::new(b"ABS(x) OVER ()").unwrap();
        assert!(!(p.expr().is_ok() && p.peek().is_none()));

        // ROW_NUMBER() is a window function: it parses its (required) OVER clause
        // and keeps its name for the engine.
        let ast::Expr::FunctionCall { name, args, filter_over, .. } =
            parse_expr("ROW_NUMBER() OVER (PARTITION BY g ORDER BY v)").unwrap()
        else {
            panic!("expected ROW_NUMBER to parse as a function call");
        };
        assert_eq!(name.as_str().to_ascii_lowercase(), "row_number");
        assert!(args.is_empty());
        let Some(ast::Over::Window(w)) = filter_over.over_clause else {
            panic!("expected an OVER window clause on ROW_NUMBER");
        };
        assert_eq!(w.partition_by.len(), 1);
        assert_eq!(w.order_by.len(), 1);
    }

    #[test]
    fn aggregate_count_star() {
        let stmt = parse("SELECT COUNT(*) FROM t").unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { columns, .. } = select.body.select else {
            panic!("expected OneSelect::Select");
        };
        let ast::ResultColumn::Expr(expr, _) = &columns[0] else {
            panic!("expected an expression column");
        };
        assert!(matches!(**expr, ast::Expr::FunctionCallStar { .. }));
    }

    #[test]
    fn aggregates_with_group_by_and_having() {
        let stmt = parse(
            "SELECT cat, COUNT(*), SUM(n) FROM t GROUP BY cat HAVING COUNT(*) > 1 ORDER BY cat",
        )
        .unwrap();
        let ast::Stmt::Select(select) = stmt else {
            panic!("expected Select");
        };
        let ast::OneSelect::Select { group_by, .. } = select.body.select else {
            panic!("expected OneSelect::Select");
        };
        let group_by = group_by.expect("expected GROUP BY");
        assert_eq!(group_by.exprs.len(), 1);
        assert!(group_by.having.is_some());
    }

    #[test]
    fn aggregate_min_max_sum_parse() {
        for sql in [
            "SELECT MIN(a) FROM t",
            "SELECT MAX(a) FROM t",
            "SELECT SUM(a) FROM t",
        ] {
            assert!(matches!(parse(sql).unwrap(), ast::Stmt::Select(_)), "{sql}");
        }
    }

    #[test]
    fn group_by_ordinal_is_unsupported() {
        // `GROUP BY 1` (group by output-column position) is not modeled. (`AVG`
        // is supported — see `aggregate_distinct`; its result is a plain double
        // rather than MySQL's fixed-scale DECIMAL, but the value matches.)
        assert!(matches!(
            parse("SELECT a FROM t GROUP BY 1").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn replace_lowers_to_insert_or_replace() {
        // REPLACE [INTO] becomes an INSERT with REPLACE conflict resolution.
        for sql in [
            "REPLACE INTO t (a, b) VALUES (1, 'x')",
            "REPLACE t (a) VALUES (1)",
        ] {
            let ast::Stmt::Insert { or_conflict, .. } = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as an Insert");
            };
            assert_eq!(or_conflict, Some(ast::ResolveType::Replace), "for `{sql}`");
        }
        // Plain INSERT keeps no conflict resolution.
        let ast::Stmt::Insert { or_conflict, .. } = parse("INSERT INTO t (a) VALUES (1)").unwrap()
        else {
            unreachable!()
        };
        assert_eq!(or_conflict, None);
    }

    #[test]
    fn insert_basic_with_columns() {
        let stmt = parse("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')").unwrap();
        let ast::Stmt::Insert {
            tbl_name,
            columns,
            body,
            ..
        } = stmt
        else {
            panic!("expected Insert");
        };
        assert_eq!(tbl_name.name.as_str(), "t");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].as_str(), "id");
        let ast::InsertBody::Select(select, _) = body else {
            panic!("expected VALUES body");
        };
        let ast::OneSelect::Values(rows) = select.body.select else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
    }

    #[test]
    fn insert_without_column_list_and_into_optional() {
        assert!(matches!(
            parse("INSERT INTO t VALUES (1)").unwrap(),
            ast::Stmt::Insert { .. }
        ));
        // `INTO` is optional in MySQL.
        assert!(matches!(
            parse("INSERT t VALUES (1)").unwrap(),
            ast::Stmt::Insert { .. }
        ));
    }

    #[test]
    fn insert_empty_values_lowers_to_default_values() {
        // `INSERT INTO t () VALUES ()` and `INSERT INTO t VALUES ()` both insert
        // one all-defaults row -> the engine's DEFAULT VALUES body.
        for sql in ["INSERT INTO t () VALUES ()", "INSERT INTO t VALUES ()"] {
            let ast::Stmt::Insert { columns, body, .. } = parse(sql).unwrap() else {
                panic!("expected Insert for `{sql}`");
            };
            assert!(columns.is_empty(), "{sql}");
            assert!(
                matches!(body, ast::InsertBody::DefaultValues),
                "expected DEFAULT VALUES for `{sql}`"
            );
        }

        // A non-empty row keeps the VALUES body even with an empty column list.
        let ast::Stmt::Insert { body, .. } = parse("INSERT INTO t () VALUES (1)").unwrap() else {
            unreachable!()
        };
        assert!(matches!(body, ast::InsertBody::Select(_, _)));
    }

    #[test]
    fn insert_values_default_keyword_lowers_to_expr_default() {
        // `INSERT ... VALUES (1, DEFAULT)` lowers the DEFAULT keyword to the
        // engine's `Expr::Default` (use the column's declared default); other
        // values stay ordinary expressions.
        let ast::Stmt::Insert { body, .. } =
            parse("INSERT INTO t (a, b) VALUES (1, DEFAULT)").unwrap()
        else {
            panic!("expected Insert");
        };
        let ast::InsertBody::Select(select, _) = body else {
            panic!("expected a VALUES body");
        };
        let ast::OneSelect::Values(rows) = &select.body.select else {
            panic!("expected a VALUES list");
        };
        assert!(matches!(rows[0][0].as_ref(), ast::Expr::Literal(_)));
        assert!(matches!(rows[0][1].as_ref(), ast::Expr::Default));

        // The `DEFAULT(col)` function form is not this keyword (and stays
        // unsupported).
        assert!(parse("INSERT INTO t (a) VALUES (DEFAULT(a))").is_err());
    }

    #[test]
    fn insert_renders_back_to_sql() {
        let sql = parse("INSERT INTO t (a, b) VALUES (1, 'x')")
            .unwrap()
            .to_string();
        let upper = sql.to_uppercase();
        assert!(
            upper.contains("INSERT") && upper.contains("VALUES"),
            "{sql}"
        );
    }

    #[test]
    fn dml_priority_and_ignore_modifiers_are_handled() {
        // Priority hints (LOW_PRIORITY / DELAYED / HIGH_PRIORITY / QUICK) are
        // consumed as no-ops on every data-change statement.
        for sql in [
            "INSERT LOW_PRIORITY INTO t (a) VALUES (1)",
            "INSERT DELAYED INTO t (a) VALUES (1)",
            "INSERT HIGH_PRIORITY INTO t (a) VALUES (1)",
            "REPLACE LOW_PRIORITY INTO t (a) VALUES (1)",
            "UPDATE LOW_PRIORITY t SET a = 1",
            "DELETE LOW_PRIORITY FROM t",
            "DELETE QUICK FROM t",
            "DELETE IGNORE FROM t",
        ] {
            assert!(parse(sql).is_ok(), "expected `{sql}` to parse");
        }

        // `UPDATE IGNORE` maps to the engine's `UPDATE OR IGNORE`.
        let ast::Stmt::Update(update) = parse("UPDATE IGNORE t SET a = 1").unwrap() else {
            panic!("expected an UPDATE");
        };
        assert!(matches!(update.or_conflict, Some(ast::ResolveType::Ignore)));

        // `INSERT LOW_PRIORITY IGNORE` keeps the IGNORE -> OR IGNORE mapping.
        let ast::Stmt::Insert { body, .. } =
            parse("INSERT LOW_PRIORITY IGNORE INTO t (a) VALUES (1)").unwrap()
        else {
            panic!("expected an INSERT");
        };
        let ast::InsertBody::Select(_, _) = body else {
            panic!("expected a VALUES insert");
        };
    }

    #[test]
    fn insert_set_lowers_to_columns_and_values() {
        // INSERT ... SET col = expr, ... becomes the equivalent columns + VALUES.
        let ast::Stmt::Insert {
            tbl_name,
            columns,
            body,
            ..
        } = parse("INSERT INTO t SET a = 1, b = 'x'").unwrap()
        else {
            panic!("expected Insert");
        };
        assert_eq!(tbl_name.name.as_str(), "t");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].as_str(), "a");
        assert_eq!(columns[1].as_str(), "b");
        let ast::InsertBody::Select(select, _) = body else {
            panic!("expected a VALUES body");
        };
        let ast::OneSelect::Values(rows) = select.body.select else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);

        // REPLACE ... SET carries the conflict modifier.
        let ast::Stmt::Insert { or_conflict, .. } = parse("REPLACE INTO t SET a = 1").unwrap()
        else {
            panic!("expected Insert");
        };
        assert_eq!(or_conflict, Some(ast::ResolveType::Replace));
    }

    #[test]
    fn insert_select_parses_query_body() {
        // INSERT [(cols)] SELECT ... carries the query into the insert body, with
        // or without a column list, preserving the conflict modifier.
        for (sql, want_cols, want_conflict) in [
            ("INSERT INTO t (a, b) SELECT a, b FROM u", 2, None),
            ("INSERT INTO t SELECT * FROM u", 0, None),
            (
                "INSERT IGNORE INTO t (a) SELECT a FROM u",
                1,
                Some(ast::ResolveType::Ignore),
            ),
        ] {
            let ast::Stmt::Insert {
                columns,
                body,
                or_conflict,
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected Insert for `{sql}`");
            };
            assert_eq!(columns.len(), want_cols, "{sql}");
            assert_eq!(or_conflict, want_conflict, "{sql}");
            let ast::InsertBody::Select(select, upsert) = body else {
                panic!("expected a SELECT body for `{sql}`");
            };
            assert!(upsert.is_none(), "{sql}");
            assert!(
                !matches!(select.body.select, ast::OneSelect::Values(_)),
                "expected a real query, not VALUES, for `{sql}`"
            );
        }
    }

    #[test]
    fn insert_ignore_lowers_to_insert_or_ignore() {
        // `INSERT IGNORE [INTO]` becomes an INSERT with IGNORE conflict
        // resolution; the optional `INTO` keyword does not change that.
        for sql in [
            "INSERT IGNORE INTO t (a) VALUES (1)",
            "INSERT IGNORE t (a) VALUES (1)",
        ] {
            let ast::Stmt::Insert { or_conflict, .. } = parse(sql).unwrap() else {
                panic!("expected Insert for `{sql}`");
            };
            assert_eq!(or_conflict, Some(ast::ResolveType::Ignore), "{sql}");
        }
    }

    #[test]
    fn insert_on_duplicate_key_update_maps_to_upsert() {
        let stmt = parse(
            "INSERT INTO t (id, n) VALUES (1, 2) ON DUPLICATE KEY UPDATE n = VALUES(n), n = n + 1",
        )
        .unwrap();
        let ast::Stmt::Insert { body, .. } = stmt else {
            panic!("expected Insert");
        };
        let ast::InsertBody::Select(_, Some(upsert)) = body else {
            panic!("expected an upsert");
        };
        // Target-less, matching MySQL's "any unique/primary key".
        assert!(upsert.index.is_none());
        let ast::UpsertDo::Set { sets, where_clause } = &upsert.do_clause else {
            panic!("expected DO UPDATE SET");
        };
        assert!(where_clause.is_none());
        assert_eq!(sets.len(), 2);
        // `VALUES(n)` is lowered to `excluded.n`.
        assert_eq!(sets[0].col_names[0].as_str(), "n");
        assert!(matches!(
            sets[0].expr.as_ref(),
            ast::Expr::Qualified(tbl, col) if tbl.as_str() == "excluded" && col.as_str() == "n"
        ));
        // A bare column on the RHS stays a plain reference (the existing row).
        assert!(matches!(sets[1].expr.as_ref(), ast::Expr::Binary(..)));
    }

    #[test]
    fn insert_on_duplicate_values_inside_expression_lowers_to_excluded() {
        // `VALUES(col)` is recognized anywhere in the assignment expression (not
        // only as a whole RHS) and lowers to `excluded.col`.
        let stmt = parse("INSERT INTO t (n) VALUES (1) ON DUPLICATE KEY UPDATE n = n + VALUES(n)")
            .unwrap();
        let ast::Stmt::Insert {
            body: ast::InsertBody::Select(_, Some(upsert)),
            ..
        } = stmt
        else {
            panic!("expected an upsert");
        };
        let ast::UpsertDo::Set { sets, .. } = &upsert.do_clause else {
            panic!("expected DO UPDATE SET");
        };
        // `n + VALUES(n)` -> `n + excluded.n`.
        let ast::Expr::Binary(_, ast::Operator::Add, rhs) = sets[0].expr.as_ref() else {
            panic!("expected an addition");
        };
        assert!(matches!(
            rhs.as_ref(),
            ast::Expr::Qualified(tbl, col) if tbl.as_str() == "excluded" && col.as_str() == "n"
        ));

        // `VALUES(col)` outside an upsert assignment is still rejected (the flag
        // is scoped to the assignment RHS).
        assert!(parse("SELECT VALUES(n) FROM t").is_err());
    }

    #[test]
    fn insert_row_alias_lowers_to_excluded() {
        // Returns the single upsert assignment's RHS expression.
        fn upsert_rhs(sql: &str) -> ast::Expr {
            let ast::Stmt::Insert {
                body: ast::InsertBody::Select(_, Some(upsert)),
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected an upsert for `{sql}`");
            };
            let ast::UpsertDo::Set { sets, .. } = upsert.do_clause else {
                panic!("expected DO UPDATE SET");
            };
            *sets.into_iter().next().unwrap().expr
        }

        // The MySQL 8.0.19+ row alias: `... AS new ... new.a` -> `excluded.a`.
        assert!(matches!(
            upsert_rhs("INSERT INTO t (a) VALUES (1) AS new ON DUPLICATE KEY UPDATE a = new.a"),
            ast::Expr::Qualified(tbl, col) if tbl.as_str() == "excluded" && col.as_str() == "a"
        ));

        // Column aliases: `AS new (na) ... na` maps to the actual column `a`.
        assert!(matches!(
            upsert_rhs("INSERT INTO t (a) VALUES (1) AS new (na) ON DUPLICATE KEY UPDATE a = na"),
            ast::Expr::Qualified(tbl, col) if tbl.as_str() == "excluded" && col.as_str() == "a"
        ));

        // A column-alias list must match the INSERT column count.
        assert!(parse(
            "INSERT INTO t (a, b) VALUES (1, 2) AS new (x) ON DUPLICATE KEY UPDATE a = x"
        )
        .is_err());

        // The alias does not leak: a later plain INSERT's `new.a` is unchanged.
        let stmt = parse("INSERT INTO t (a) VALUES (new.a)");
        // (References `new.a` outside any upsert — parses as a normal qualified
        // column, not rewritten to `excluded`.)
        assert!(stmt.is_ok());
    }

    #[test]
    fn transaction_control_statements() {
        assert!(matches!(
            parse("START TRANSACTION").unwrap(),
            ast::Stmt::Begin { .. }
        ));
        assert!(matches!(parse("BEGIN").unwrap(), ast::Stmt::Begin { .. }));
        assert!(matches!(
            parse("BEGIN WORK").unwrap(),
            ast::Stmt::Begin { .. }
        ));
        assert!(matches!(parse("COMMIT").unwrap(), ast::Stmt::Commit { .. }));
        assert!(matches!(
            parse("COMMIT WORK").unwrap(),
            ast::Stmt::Commit { .. }
        ));
        assert!(matches!(
            parse("ROLLBACK").unwrap(),
            ast::Stmt::Rollback {
                savepoint_name: None,
                ..
            }
        ));

        // Savepoints pass through to the engine's native ones.
        let ast::Stmt::Savepoint { name } = parse("SAVEPOINT sp1").unwrap() else {
            panic!("expected a SAVEPOINT");
        };
        assert_eq!(name.as_str(), "sp1");
        // ROLLBACK TO [SAVEPOINT] name carries the savepoint name.
        for sql in ["ROLLBACK TO sp1", "ROLLBACK TO SAVEPOINT sp1", "ROLLBACK WORK TO SAVEPOINT sp1"] {
            let ast::Stmt::Rollback { savepoint_name, .. } = parse(sql).unwrap() else {
                panic!("expected a ROLLBACK for `{sql}`");
            };
            assert_eq!(savepoint_name.as_ref().map(|n| n.as_str()), Some("sp1"), "{sql}");
        }
        // RELEASE SAVEPOINT name.
        let ast::Stmt::Release { name } = parse("RELEASE SAVEPOINT sp1").unwrap() else {
            panic!("expected a RELEASE");
        };
        assert_eq!(name.as_str(), "sp1");
    }

    #[test]
    fn transaction_unsupported_variants() {
        for sql in [
            "START TRANSACTION READ ONLY",
            "START TRANSACTION WITH CONSISTENT SNAPSHOT",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
        // RELEASE requires the SAVEPOINT keyword in MySQL, so the bare form fails.
        assert!(parse("RELEASE sp").is_err());
    }

    #[test]
    fn ignores_engine_and_charset_options() {
        let stmt = parse("CREATE TABLE t (id INT) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4").unwrap();
        assert!(matches!(stmt, ast::Stmt::CreateTable { .. }));
    }

    #[test]
    fn drop_table_basic() {
        let stmt = parse("DROP TABLE users").unwrap();
        let ast::Stmt::DropTable {
            if_exists,
            tbl_name,
        } = stmt
        else {
            panic!("expected DropTable");
        };
        assert!(!if_exists);
        assert_eq!(tbl_name.name.as_str(), "users");
    }

    #[test]
    fn drop_table_if_exists() {
        let stmt = parse("DROP TABLE IF EXISTS users").unwrap();
        let ast::Stmt::DropTable {
            if_exists,
            tbl_name,
        } = stmt
        else {
            panic!("expected DropTable");
        };
        assert!(if_exists);
        assert_eq!(tbl_name.name.as_str(), "users");
    }

    #[test]
    fn drop_table_if_without_exists_is_error() {
        // `IF` not followed by `EXISTS` is a syntax error, not a silent accept.
        assert!(parse("DROP TABLE IF users").is_err());
    }

    #[test]
    fn drop_table_qualified_and_quoted() {
        let stmt = parse("DROP TABLE `mydb`.`t`").unwrap();
        let ast::Stmt::DropTable { tbl_name, .. } = stmt else {
            panic!("expected DropTable");
        };
        assert_eq!(tbl_name.db_name.as_ref().unwrap().as_str(), "mydb");
        assert_eq!(tbl_name.name.as_str(), "t");
    }

    #[test]
    fn drop_table_renders_back_to_sql() {
        let sql = parse("DROP TABLE t").unwrap().to_string();
        assert!(sql.to_uppercase().contains("DROP TABLE"), "{sql}");
        assert!(sql.contains('t'), "{sql}");
    }

    #[test]
    fn alter_table_add_column_lowers_to_add_column() {
        // `ADD COLUMN` and the COLUMN-elided `ADD` both lower to AddColumn; a
        // trailing `FIRST` / `AFTER col` position clause is consumed and ignored.
        for sql in [
            "ALTER TABLE t ADD COLUMN c INT DEFAULT 0",
            "ALTER TABLE t ADD c INT DEFAULT 0",
            "ALTER TABLE t ADD COLUMN c INT DEFAULT 0 FIRST",
            "ALTER TABLE t ADD COLUMN c INT DEFAULT 0 AFTER other",
            "ALTER TABLE t ADD c INT AFTER other",
        ] {
            let ast::Stmt::AlterTable(alter) = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as ALTER TABLE");
            };
            assert_eq!(alter.name.name.as_str(), "t");
            let ast::AlterTableBody::AddColumn(col) = &alter.body else {
                panic!("expected an ADD COLUMN body for `{sql}`");
            };
            assert_eq!(col.col_name.as_str(), "c", "for `{sql}`");
        }
    }

    #[test]
    fn alter_add_constraint_unique_and_primary_key() {
        // `ADD CONSTRAINT [symbol] UNIQUE (cols)` lowers to a UNIQUE CREATE INDEX.
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT uq UNIQUE (a)",
            "ALTER TABLE t ADD CONSTRAINT UNIQUE (a)",
            "ALTER TABLE t ADD CONSTRAINT uq UNIQUE KEY (a)",
        ] {
            let ast::Stmt::CreateIndex { unique, .. } = parse(sql).unwrap() else {
                panic!("expected `{sql}` to lower to CREATE INDEX");
            };
            assert!(unique, "expected a UNIQUE index for `{sql}`");
        }

        // `ADD CONSTRAINT [symbol] PRIMARY KEY (cols)` lowers like ADD PRIMARY KEY.
        let ast::Stmt::CreateIndex {
            unique, idx_name, ..
        } = parse("ALTER TABLE t ADD CONSTRAINT pk PRIMARY KEY (id)").unwrap()
        else {
            panic!("expected a CREATE INDEX");
        };
        assert!(unique);
        assert_eq!(idx_name.name.as_str(), "t_primary");

        // FOREIGN KEY / CHECK constraints are still unsupported.
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES u (id)",
            "ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn alter_table_add_index_lowers_to_create_index() {
        // ADD KEY / ADD INDEX / ADD UNIQUE KEY become CREATE [UNIQUE] INDEX;
        // a prefix length is dropped and an omitted name is synthesized. ADD
        // FULLTEXT (with or without KEY/INDEX) degrades to a plain, non-unique
        // CREATE INDEX since the engine has no full-text index.
        let cases = [
            (
                "ALTER TABLE t ADD KEY name_idx (name)",
                false,
                "name_idx",
                1,
            ),
            ("ALTER TABLE t ADD INDEX (name)", false, "t_name", 1),
            ("ALTER TABLE t ADD UNIQUE KEY u (code(10))", true, "u", 1),
            (
                "ALTER TABLE t ADD UNIQUE INDEX combo (a, b)",
                true,
                "combo",
                2,
            ),
            ("ALTER TABLE t ADD FULLTEXT KEY ft (body)", false, "ft", 1),
            ("ALTER TABLE t ADD FULLTEXT (body)", false, "t_body", 1),
        ];
        for (sql, want_unique, want_name, want_cols) in cases {
            let ast::Stmt::CreateIndex {
                unique,
                idx_name,
                tbl_name,
                columns,
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to CREATE INDEX");
            };
            assert_eq!(unique, want_unique, "{sql}");
            assert_eq!(idx_name.name.as_str(), want_name, "{sql}");
            assert_eq!(tbl_name.as_str(), "t", "{sql}");
            assert_eq!(columns.len(), want_cols, "{sql}");
        }
    }

    #[test]
    fn alter_table_add_primary_key_lowers_to_unique_index() {
        // ADD PRIMARY KEY (cols) becomes a UNIQUE CREATE INDEX named
        // `<table>_primary`; a prefix length is dropped and the composite form is
        // supported.
        let cases = [
            ("ALTER TABLE t ADD PRIMARY KEY (id)", "t_primary", 1),
            ("ALTER TABLE t ADD PRIMARY KEY (a, b)", "t_primary", 2),
            ("ALTER TABLE t ADD PRIMARY KEY (k(10))", "t_primary", 1),
        ];
        for (sql, want_name, want_cols) in cases {
            let ast::Stmt::CreateIndex {
                unique,
                idx_name,
                tbl_name,
                columns,
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to CREATE INDEX");
            };
            assert!(unique, "primary key must lower to a UNIQUE index: {sql}");
            assert_eq!(idx_name.name.as_str(), want_name, "{sql}");
            assert_eq!(tbl_name.as_str(), "t", "{sql}");
            assert_eq!(columns.len(), want_cols, "{sql}");
        }

        // The non-PRIMARY constraint adds are still unsupported.
        for sql in [
            "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES u (b)",
            "ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn rename_table_lowers_to_alter_rename() {
        // RENAME TABLE old TO new -> ALTER TABLE old RENAME TO new.
        let ast::Stmt::AlterTable(alter) = parse("RENAME TABLE old_t TO new_t").unwrap() else {
            panic!("expected RENAME TABLE to parse as ALTER TABLE");
        };
        assert_eq!(alter.name.name.as_str(), "old_t");
        let ast::AlterTableBody::RenameTo(new) = &alter.body else {
            panic!("expected a RENAME TO body");
        };
        assert_eq!(new.as_str(), "new_t");

        // The multi-table form and non-TABLE renames are rejected.
        for sql in ["RENAME TABLE a TO b, c TO d", "RENAME USER u1 TO u2"] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn alter_table_drop_and_rename_lower_to_engine_ops() {
        // DROP [COLUMN] col -> DropColumn.
        for sql in ["ALTER TABLE t DROP COLUMN c", "ALTER TABLE t DROP c"] {
            let ast::Stmt::AlterTable(alter) = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as ALTER TABLE");
            };
            let ast::AlterTableBody::DropColumn(col) = &alter.body else {
                panic!("expected DROP COLUMN for `{sql}`");
            };
            assert_eq!(col.as_str(), "c");
        }

        // RENAME COLUMN old TO new -> RenameColumn.
        let ast::Stmt::AlterTable(alter) = parse("ALTER TABLE t RENAME COLUMN a TO b").unwrap()
        else {
            panic!("expected ALTER TABLE");
        };
        let ast::AlterTableBody::RenameColumn { old, new } = &alter.body else {
            panic!("expected RENAME COLUMN");
        };
        assert_eq!(old.as_str(), "a");
        assert_eq!(new.as_str(), "b");

        // RENAME [TO|AS] new_table -> RenameTo.
        for sql in ["ALTER TABLE t RENAME TO u", "ALTER TABLE t RENAME u"] {
            let ast::Stmt::AlterTable(alter) = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as ALTER TABLE");
            };
            let ast::AlterTableBody::RenameTo(new) = &alter.body else {
                panic!("expected RENAME TO for `{sql}`");
            };
            assert_eq!(new.as_str(), "u");
        }
    }

    #[test]
    fn alter_table_change_column_renames() {
        // CHANGE [COLUMN] old new <def> with old != new -> RENAME COLUMN; the
        // trailing definition is consumed and discarded.
        for sql in [
            "ALTER TABLE t CHANGE COLUMN a b INT",
            "ALTER TABLE t CHANGE a b VARCHAR(50) NOT NULL DEFAULT ''",
        ] {
            let ast::Stmt::AlterTable(alter) = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as ALTER TABLE");
            };
            let ast::AlterTableBody::RenameColumn { old, new } = &alter.body else {
                panic!("expected RENAME COLUMN for `{sql}`");
            };
            assert_eq!(old.as_str(), "a", "{sql}");
            assert_eq!(new.as_str(), "b", "{sql}");
        }

        // A same-name CHANGE is a pure type change and is rejected.
        assert!(matches!(
            parse("ALTER TABLE t CHANGE COLUMN a a BIGINT").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn alter_table_unsupported_variants() {
        // Foreign-key and other constraint adds and drops, the in-place type
        // change `MODIFY` (and same-name `CHANGE`), `RENAME INDEX`, and the
        // multi-operation comma form are all rejected (a real mysqld accepts
        // them, but the engine has no in-place equivalent). `ADD`/`DROP PRIMARY
        // KEY` are the exceptions -- they lower to creating / dropping a UNIQUE
        // index (see `alter_table_add_primary_key_lowers_to_unique_index` and
        // `alter_table_drop_primary_key_drops_the_emulated_index`).
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (c) REFERENCES u (id)",
            "ALTER TABLE t ADD SPATIAL KEY sp (c)",
            "ALTER TABLE t ADD COLUMN c INT AUTO_INCREMENT",
            "ALTER TABLE t ADD a INT, ADD b INT",
            "ALTER TABLE t DROP FOREIGN KEY fk",
            "ALTER TABLE t MODIFY COLUMN a BIGINT",
            "ALTER TABLE t RENAME INDEX a TO b",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn drop_index_lowers_to_engine_drop_index() {
        // Standalone `DROP INDEX idx ON t` and `ALTER TABLE t DROP {INDEX|KEY}
        // idx` both become the engine's DROP INDEX by name (the table is implied).
        for sql in [
            "DROP INDEX idx ON t",
            "ALTER TABLE t DROP INDEX idx",
            "ALTER TABLE t DROP KEY idx",
        ] {
            let ast::Stmt::DropIndex { idx_name, .. } = parse(sql).unwrap() else {
                panic!("expected `{sql}` to lower to DROP INDEX");
            };
            assert_eq!(idx_name.name.as_str(), "idx", "{sql}");
        }
    }

    #[test]
    fn alter_table_drop_primary_key_drops_the_emulated_index() {
        // DROP PRIMARY KEY drops the `<table>_primary` index that ADD PRIMARY KEY
        // creates, so an ADD/DROP cycle round-trips.
        let ast::Stmt::DropIndex { idx_name, .. } =
            parse("ALTER TABLE t DROP PRIMARY KEY").unwrap()
        else {
            panic!("expected DROP PRIMARY KEY to lower to DROP INDEX");
        };
        assert_eq!(idx_name.name.as_str(), "t_primary");
    }

    /// Parses `input` as a single complete expression.
    fn parse_expr(input: &str) -> Result<ast::Expr> {
        let mut p = Parser::new(input.as_bytes())?;
        let expr = p.expr()?;
        assert!(
            p.peek().is_none(),
            "expression `{input}` was not fully consumed"
        );
        Ok(expr)
    }

    fn num(s: &str) -> ast::Expr {
        ast::Expr::Literal(ast::Literal::Numeric(s.to_string()))
    }

    fn col(s: &str) -> ast::Expr {
        ast::Expr::Id(ast::Name::from_string(s))
    }

    #[test]
    fn expr_literals() {
        assert_eq!(parse_expr("42").unwrap(), num("42"));
        assert_eq!(parse_expr("-7").unwrap(), num("-7"));
        assert_eq!(parse_expr("3.5").unwrap(), num("3.5"));
        assert_eq!(
            parse_expr("'it''s'").unwrap(),
            ast::Expr::Literal(ast::Literal::String("'it''s'".to_string()))
        );
        assert_eq!(
            parse_expr("NULL").unwrap(),
            ast::Expr::Literal(ast::Literal::Null)
        );
        assert_eq!(
            parse_expr("TRUE").unwrap(),
            ast::Expr::Literal(ast::Literal::True)
        );
    }

    #[test]
    fn expr_column_refs() {
        assert_eq!(parse_expr("age").unwrap(), col("age"));
        assert_eq!(
            parse_expr("t.age").unwrap(),
            ast::Expr::Qualified(ast::Name::from_string("t"), ast::Name::from_string("age"))
        );
        assert_eq!(parse_expr("`select`").unwrap(), col("select"));
    }

    #[test]
    fn expr_comparisons() {
        assert_eq!(
            parse_expr("age = 30").unwrap(),
            ast::Expr::binary(col("age"), ast::Operator::Equals, num("30"))
        );
        // `<>` and `!=` are the same operator.
        assert_eq!(parse_expr("a <> 1").unwrap(), parse_expr("a != 1").unwrap());
        assert_eq!(
            parse_expr("a <= 1").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::LessEquals, num("1"))
        );
        assert_eq!(
            parse_expr("a >= 1").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::GreaterEquals, num("1"))
        );
    }

    #[test]
    fn expr_is_null() {
        assert_eq!(
            parse_expr("a IS NULL").unwrap(),
            ast::Expr::is_null(col("a"))
        );
        assert_eq!(
            parse_expr("a IS NOT NULL").unwrap(),
            ast::Expr::not_null(col("a"))
        );
    }

    #[test]
    fn expr_is_truthiness() {
        // IS [NOT] UNKNOWN is exactly IS [NOT] NULL.
        assert_eq!(
            parse_expr("a IS UNKNOWN").unwrap(),
            ast::Expr::is_null(col("a"))
        );
        assert_eq!(
            parse_expr("a IS NOT UNKNOWN").unwrap(),
            ast::Expr::not_null(col("a"))
        );

        // IS TRUE / IS FALSE lower to coalesce(a <op> 0, default).
        for (sql, op, default) in [
            ("a IS TRUE", ast::Operator::NotEquals, "0"),
            ("a IS FALSE", ast::Operator::Equals, "0"),
            ("a IS NOT TRUE", ast::Operator::Equals, "1"),
            ("a IS NOT FALSE", ast::Operator::NotEquals, "1"),
        ] {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to coalesce(...)");
            };
            assert_eq!(name.as_str(), "coalesce", "{sql}");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Binary(_, o, _) if *o == op),
                "{sql}"
            );
            assert!(
                matches!(args[1].as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == default),
                "{sql}"
            );
        }
    }

    #[test]
    fn expr_precedence_and_binds_tighter_than_or() {
        // a = 1 OR b = 2 AND c = 3  ==>  a = 1 OR (b = 2 AND c = 3)
        let expr = parse_expr("a = 1 OR b = 2 AND c = 3").unwrap();
        let expected = ast::Expr::binary(
            ast::Expr::binary(col("a"), ast::Operator::Equals, num("1")),
            ast::Operator::Or,
            ast::Expr::binary(
                ast::Expr::binary(col("b"), ast::Operator::Equals, num("2")),
                ast::Operator::And,
                ast::Expr::binary(col("c"), ast::Operator::Equals, num("3")),
            ),
        );
        assert_eq!(expr, expected);
    }

    #[test]
    fn cast_maps_mysql_types_to_engine_affinity() {
        // CAST is a real cast, not a function call; MySQL target types map to an
        // engine type with the matching affinity.
        let cases = [
            ("CAST(a AS CHAR)", "CHAR"),
            ("CAST(a AS DECIMAL)", "DECIMAL"),
            ("CAST(a AS DOUBLE)", "REAL"),
            ("CAST(a AS BINARY)", "BLOB"),
            ("CAST(a AS CHAR(8))", "CHAR"), // length parses but is dropped
        ];
        for (sql, expected) in cases {
            let ast::Expr::Cast { type_name, .. } = parse_expr(sql).unwrap() else {
                panic!("expected a Cast for `{sql}`");
            };
            let ty = type_name.expect("cast has a target type");
            assert_eq!(ty.name, expected, "for `{sql}`");
            assert!(ty.size.is_none(), "length must be dropped for `{sql}`");
        }
        // An integer target (SIGNED/UNSIGNED) lowers to a `typeof`-guarded CASE
        // that rounds a numeric value before the `CAST ... AS INTEGER` and casts a
        // string/NULL directly (MySQL's argument-type-dependent rounding).
        for sql in [
            "CAST(a AS SIGNED)",
            "CAST(a AS SIGNED INTEGER)",
            "CAST(a AS UNSIGNED)",
        ] {
            let ast::Expr::Case { else_expr, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            let else_branch = else_expr.unwrap();
            let ast::Expr::Cast { type_name, .. } = else_branch.as_ref() else {
                panic!("expected the ELSE of `{sql}` to be a CAST AS INTEGER");
            };
            assert_eq!(type_name.as_ref().unwrap().name, "INTEGER", "for `{sql}`");
        }
        // DATE/DATETIME/TIME have no affinity, so they lower to the engine's
        // date()/datetime()/time() functions instead of a Cast.
        for (sql, func) in [
            ("CAST(a AS DATE)", "date"),
            ("CAST(a AS DATETIME)", "datetime"),
            ("CAST(a AS DATETIME(6))", "datetime"),
            ("CAST(a AS TIME)", "time"),
        ] {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a function call");
            };
            assert_eq!(name.as_str(), func, "for `{sql}`");
            assert_eq!(args.len(), 1, "for `{sql}`");
        }
        // Other targets that diverge from the engine are still rejected.
        assert!(matches!(
            parse_expr("CAST(a AS JSON)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn introspection_functions_fold_to_literals() {
        // VERSION()/USER() fold to string literals, DATABASE() to NULL,
        // CONNECTION_ID() to a number — usable mid-expression.
        assert!(matches!(
            parse_expr("VERSION()").unwrap(),
            ast::Expr::Literal(ast::Literal::String(_))
        ));
        assert!(matches!(
            parse_expr("DATABASE()").unwrap(),
            ast::Expr::Literal(ast::Literal::Null)
        ));
        assert!(matches!(
            parse_expr("CONNECTION_ID()").unwrap(),
            ast::Expr::Literal(ast::Literal::Numeric(_))
        ));
        assert!(matches!(
            parse_expr("CURRENT_USER()").unwrap(),
            ast::Expr::Literal(ast::Literal::String(_))
        ));
        // Usable inside a larger expression (the case that used to error).
        assert!(parse_expr("LENGTH(VERSION()) > 0").is_ok());
    }

    #[test]
    fn unix_time_functions_lower_to_engine() {
        // UNIX_TIMESTAMP(d) -> unixepoch(d); the no-arg form uses 'now'.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("UNIX_TIMESTAMP(d)").unwrap()
        else {
            panic!("expected unixepoch call");
        };
        assert_eq!(name.as_str(), "unixepoch");
        assert_eq!(args.len(), 1);

        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("UNIX_TIMESTAMP()").unwrap()
        else {
            panic!("expected unixepoch call");
        };
        assert_eq!(name.as_str(), "unixepoch");
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'now'")
        );

        // FROM_UNIXTIME(n) -> datetime(n, 'unixepoch').
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("FROM_UNIXTIME(n)").unwrap()
        else {
            panic!("expected datetime call");
        };
        assert_eq!(name.as_str(), "datetime");
        assert!(
            matches!(args[1].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'unixepoch'")
        );
    }

    #[test]
    fn current_time_functions_lower_to_now() {
        // Each maps to the engine datetime/date/time function applied to 'now'.
        let cases = [
            ("NOW()", "datetime"),
            ("CURRENT_TIMESTAMP()", "datetime"),
            ("UTC_TIMESTAMP()", "datetime"),
            ("CURDATE()", "date"),
            ("CURRENT_DATE()", "date"),
            ("CURTIME()", "time"),
        ];
        for (sql, engine_fn) in cases {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a function call");
            };
            assert_eq!(name.as_str(), engine_fn, "for `{sql}`");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'now'"),
                "expected a 'now' argument for `{sql}`"
            );
        }
    }

    #[test]
    fn bare_current_datetime_keywords_lower_to_now() {
        // The SQL-standard niladic keywords work without parentheses and lower to
        // the same `<fn>('now')` engine call as their parenthesized forms.
        let cases = [
            ("CURRENT_TIMESTAMP", "datetime"),
            ("LOCALTIME", "datetime"),
            ("LOCALTIMESTAMP", "datetime"),
            ("UTC_TIMESTAMP", "datetime"),
            ("CURRENT_DATE", "date"),
            ("UTC_DATE", "date"),
            ("CURRENT_TIME", "time"),
            ("UTC_TIME", "time"),
        ];
        for (sql, engine_fn) in cases {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected bare `{sql}` to lower to a function call");
            };
            assert_eq!(name.as_str(), engine_fn, "for `{sql}`");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'now'"),
                "expected a 'now' argument for bare `{sql}`"
            );
        }

        // The bare form and the parenthesized form lower identically.
        assert_eq!(
            parse_expr("CURRENT_TIMESTAMP").unwrap(),
            parse_expr("CURRENT_TIMESTAMP()").unwrap()
        );
    }

    #[test]
    fn bare_current_user_folds_like_the_function() {
        // `CURRENT_USER` works without parentheses and folds to the same literal
        // as `CURRENT_USER()`.
        assert_eq!(
            parse_expr("CURRENT_USER").unwrap(),
            parse_expr("CURRENT_USER()").unwrap()
        );
        assert!(matches!(
            parse_expr("CURRENT_USER").unwrap(),
            ast::Expr::Literal(ast::Literal::String(_))
        ));

        // `USER` / `SESSION_USER` / `SYSTEM_USER` require parentheses in MySQL, so
        // the bare forms stay column references (not the user literal).
        for input in ["USER", "SESSION_USER", "SYSTEM_USER"] {
            assert!(
                matches!(parse_expr(input).unwrap(), ast::Expr::Id(_)),
                "expected bare `{input}` to be a column reference"
            );
        }
    }

    #[test]
    fn date_format_lowers_to_strftime_with_translated_codes() {
        // DATE_FORMAT(d, fmt) becomes strftime(translated_fmt, d); %i/%s map to
        // %M/%S, the rest pass through.
        let ast::Expr::FunctionCall { name, args, .. } =
            parse_expr("DATE_FORMAT(d, '%Y-%m-%d %H:%i:%s')").unwrap()
        else {
            panic!("expected DATE_FORMAT to lower to strftime");
        };
        assert_eq!(name.as_str(), "strftime");
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%Y-%m-%d %H:%M:%S'"),
            "format was not translated correctly"
        );
        // `%j`/`%w` pass through; `%U`/`%v` map to strftime `%U`/`%V`; `%T`
        // expands to `%H:%M:%S`.
        let ast::Expr::FunctionCall { args, .. } =
            parse_expr("DATE_FORMAT(d, '%j-%w %U %v %T')").unwrap()
        else {
            unreachable!()
        };
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%j-%w %U %V %H:%M:%S'")
        );
        // The name specifiers `%M`/`%W`/`%b`/`%a` have no strftime form, so they
        // lower to a CASE name lookup; `%W` alone is therefore a bare CASE.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%W')").unwrap(),
            ast::Expr::Case { .. }
        ));
        // A name specifier mixed with strftime runs becomes a concatenation
        // (`||`) of strftime segments and CASE lookups.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%Y %M')").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Concat, _)
        ));
        // The no-leading-zero numeric `%e`/`%c`/`%k` lower to an integer CAST of
        // the strftime code; `%e` alone is therefore a bare CAST.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%e')").unwrap(),
            ast::Expr::Cast { .. }
        ));
        // `%p` (AM/PM) and `%l` (12-hour) lower to a CASE; `%h` (padded 12-hour)
        // wraps that in a `substr` for the two-digit pad.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%p')").unwrap(),
            ast::Expr::Case { .. }
        ));
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%l')").unwrap(),
            ast::Expr::Case { .. }
        ));
        let ast::Expr::FunctionCall { name, .. } = parse_expr("DATE_FORMAT(d, '%h')").unwrap()
        else {
            panic!("expected %h to lower to a substr() call");
        };
        assert_eq!(name.as_str(), "substr");
        // `%D` (day with ordinal suffix) is `day || CASE ...`, a concatenation.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%D')").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Concat, _)
        ));
        // Specifiers still without a lowering are rejected (microseconds `%f`,
        // and the week-of-year modes `%u`/`%V`/`%X`/`%x`).
        for fmt in [
            "DATE_FORMAT(d, '%f')",
            "DATE_FORMAT(d, '%u')",
            "DATE_FORMAT(d, '%V')",
            "DATE_FORMAT(d, '%X')",
        ] {
            assert!(matches!(
                parse_expr(fmt).unwrap_err(),
                ParseError::Unsupported(_)
            ));
        }
        // A non-literal format is rejected.
        assert!(parse_expr("DATE_FORMAT(d, f)").is_err());

        // TIME_FORMAT shares the DATE_FORMAT lowering exactly, so the same
        // format produces an identical expression.
        assert_eq!(
            parse_expr("TIME_FORMAT(d, '%H:%i:%s')").unwrap(),
            parse_expr("DATE_FORMAT(d, '%H:%i:%s')").unwrap()
        );
        assert_eq!(
            parse_expr("TIME_FORMAT(d, '%h:%i %p')").unwrap(),
            parse_expr("DATE_FORMAT(d, '%h:%i %p')").unwrap()
        );
        // A non-literal TIME_FORMAT format is likewise rejected.
        assert!(parse_expr("TIME_FORMAT(d, f)").is_err());
    }

    #[test]
    fn date_add_sub_lower_to_datetime_modifier() {
        // Each day/time interval lowers to datetime(target, '<signed-n> <unit>').
        // (MONTH/YEAR steps add a clamping CASE — see
        // `month_and_year_intervals_clamp_to_month_end`.)
        let cases = [
            ("DATE_ADD(d, INTERVAL 5 DAY)", "'+5 days'"),
            ("DATE_SUB(d, INTERVAL 1 DAY)", "'-1 days'"),
            ("DATE_ADD(d, INTERVAL 1 WEEK)", "'+7 days'"),
            ("DATE_SUB(d, INTERVAL 3 HOUR)", "'-3 hours'"),
            // A quoted numeric string is coerced like MySQL does (WordPress
            // emits `INTERVAL '30' SECOND`).
            ("DATE_ADD(d, INTERVAL '30' SECOND)", "'+30 seconds'"),
            ("DATE_SUB(d, INTERVAL '5' DAY)", "'-5 days'"),
            // The `+`/`-` INTERVAL operators share the same lowering.
            ("d + INTERVAL 5 DAY", "'+5 days'"),
            ("d - INTERVAL 1 DAY", "'-1 days'"),
            ("d - INTERVAL 3 HOUR", "'-3 hours'"),
        ];
        for (sql, modifier) in cases {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a datetime() call");
            };
            assert_eq!(name.as_str(), "datetime");
            assert_eq!(args.len(), 2);
            assert!(
                matches!(args[1].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == modifier),
                "wrong modifier for `{sql}`"
            );
        }
        // A non-literal interval value, or a non-numeric string, is rejected.
        assert!(parse_expr("DATE_ADD(d, INTERVAL x DAY)").is_err());
        assert!(parse_expr("DATE_ADD(d, INTERVAL 'abc' DAY)").is_err());
    }

    #[test]
    fn month_and_year_intervals_clamp_to_month_end() {
        // MONTH / QUARTER / YEAR steps can overflow a shorter month, so they wrap
        // the datetime() call in a CASE that clamps to the month's last day.
        for sql in [
            "DATE_ADD(d, INTERVAL 1 MONTH)",
            "DATE_SUB(d, INTERVAL 1 MONTH)",
            "DATE_ADD(d, INTERVAL 1 QUARTER)",
            "DATE_ADD(d, INTERVAL 1 YEAR)",
            "d + INTERVAL 1 MONTH",
        ] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::Case { .. }),
                "expected `{sql}` to clamp via a CASE"
            );
        }
        // Day and time steps never overflow, so they stay a plain datetime() call.
        for sql in ["DATE_ADD(d, INTERVAL 5 DAY)", "DATE_ADD(d, INTERVAL 3 HOUR)"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::FunctionCall { .. }),
                "expected `{sql}` to stay a plain datetime() call"
            );
        }
    }

    #[test]
    fn prefix_interval_lowers_like_the_postfix_form() {
        // `INTERVAL n unit + date` is the mirror of `date + INTERVAL n unit` and
        // lowers to exactly the same expression.
        for (prefix, postfix) in [
            ("INTERVAL 5 DAY + d", "d + INTERVAL 5 DAY"),
            ("INTERVAL 1 MONTH + d", "d + INTERVAL 1 MONTH"),
            ("INTERVAL 3 HOUR + d", "d + INTERVAL 3 HOUR"),
            ("INTERVAL '1:30' HOUR_MINUTE + d", "d + INTERVAL '1:30' HOUR_MINUTE"),
        ] {
            assert_eq!(
                parse_expr(prefix).unwrap(),
                parse_expr(postfix).unwrap(),
                "`{prefix}` should lower like `{postfix}`"
            );
        }

        // A leading interval must be followed by `+`: a standalone interval, or
        // the `- INTERVAL` prefix, is rejected (as in MySQL).
        assert!(parse_expr("INTERVAL 3 DAY").is_err());
        assert!(parse_expr("INTERVAL 3 DAY - d").is_err());

        // `INTERVAL(n, ...)` (the count-of-bounds function) is still the function
        // call, not a prefix interval.
        assert!(matches!(
            parse_expr("INTERVAL(5, 1, 10)").unwrap(),
            ast::Expr::Case { .. }
        ));
    }

    #[test]
    fn compound_interval_lowers_to_multi_modifier_datetime() {
        // `INTERVAL 'h:m' HOUR_MINUTE` -> datetime(d, '+h hours', '+m minutes').
        let datetime_mods = |sql: &str| -> Vec<String> {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a datetime() call");
            };
            assert_eq!(name.as_str(), "datetime");
            args[1..]
                .iter()
                .map(|a| match a.as_ref() {
                    ast::Expr::Literal(ast::Literal::String(s)) => s.clone(),
                    other => panic!("expected a string modifier, got {other:?}"),
                })
                .collect()
        };

        assert_eq!(
            datetime_mods("DATE_ADD(d, INTERVAL '5:30' HOUR_MINUTE)"),
            ["'+5 hours'", "'+30 minutes'"]
        );
        // A leading `-` on the string negates every field.
        assert_eq!(
            datetime_mods("DATE_ADD(d, INTERVAL '-5:30' HOUR_MINUTE)"),
            ["'-5 hours'", "'-30 minutes'"]
        );
        // DATE_SUB also negates every field.
        assert_eq!(
            datetime_mods("DATE_SUB(d, INTERVAL '5:30' HOUR_MINUTE)"),
            ["'-5 hours'", "'-30 minutes'"]
        );
        // Three- and four-field units, and `-`/space separators.
        assert_eq!(
            datetime_mods("DATE_ADD(d, INTERVAL '1:2:3' HOUR_SECOND)"),
            ["'+1 hours'", "'+2 minutes'", "'+3 seconds'"]
        );
        assert_eq!(
            datetime_mods("DATE_ADD(d, INTERVAL '2-3' YEAR_MONTH)"),
            ["'+2 years'", "'+3 months'"]
        );
        assert_eq!(
            datetime_mods("DATE_ADD(d, INTERVAL '1 2:3:4' DAY_SECOND)"),
            ["'+1 days'", "'+2 hours'", "'+3 minutes'", "'+4 seconds'"]
        );

        // The wrong number of fields for the unit is rejected.
        assert!(parse_expr("DATE_ADD(d, INTERVAL '5' HOUR_MINUTE)").is_err());
        assert!(parse_expr("DATE_ADD(d, INTERVAL '1:2:3' HOUR_MINUTE)").is_err());
    }

    #[test]
    fn dayname_monthname_lower_to_case_over_strftime() {
        // DAYNAME -> CASE over strftime('%w') with 7 names; MONTHNAME 12 over '%m'.
        for (sql, want_branches, first_name) in [
            ("DAYNAME(d)", 7, "'Sunday'"),
            ("MONTHNAME(d)", 12, "'January'"),
        ] {
            let ast::Expr::Case {
                base,
                when_then_pairs,
                else_expr,
            } = parse_expr(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            // The base is CAST(strftime(...) AS INTEGER).
            assert!(
                matches!(base.as_deref(), Some(ast::Expr::Cast { .. })),
                "{sql}"
            );
            assert_eq!(when_then_pairs.len(), want_branches, "{sql}");
            // No ELSE, so a NULL date yields NULL.
            assert!(else_expr.is_none(), "{sql}");
            // The first branch maps to the first name.
            assert!(
                matches!(when_then_pairs[0].1.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == first_name),
                "{sql}"
            );
        }
    }

    #[test]
    fn time_to_sec_and_sec_to_time_lower() {
        // TIME_TO_SEC(t) is an addition (H*3600 + M*60 + S), so the top is a `+`.
        assert!(matches!(
            parse_expr("TIME_TO_SEC(t)").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Add, _)
        ));

        // SEC_TO_TIME(s) -> time(s, 'unixepoch').
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("SEC_TO_TIME(s)").unwrap()
        else {
            panic!("expected SEC_TO_TIME to lower to a function call");
        };
        assert_eq!(name.as_str(), "time");
        assert_eq!(args.len(), 2);
        assert_eq!(*args[0], col("s"));
        assert!(
            matches!(args[1].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'unixepoch'")
        );
    }

    #[test]
    fn adddate_subdate_forms() {
        // The INTERVAL form is exactly DATE_ADD / DATE_SUB.
        assert_eq!(
            parse_expr("ADDDATE(d, INTERVAL 5 DAY)").unwrap(),
            parse_expr("DATE_ADD(d, INTERVAL 5 DAY)").unwrap()
        );
        assert_eq!(
            parse_expr("SUBDATE(d, INTERVAL 1 DAY)").unwrap(),
            parse_expr("DATE_SUB(d, INTERVAL 1 DAY)").unwrap()
        );

        // The integer-days form lowers to a NULL-guarded datetime(printf(...))
        // shift; the ELSE branch is the datetime() call.
        let ast::Expr::Case { else_expr, .. } = parse_expr("ADDDATE(d, 5)").unwrap() else {
            panic!("expected ADDDATE(d, n) to lower to a guarded CASE");
        };
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE branch to be a datetime() call");
        };
        assert_eq!(name.as_str(), "datetime");
        // datetime(target, printf('%+d days', n)).
        let ast::Expr::FunctionCall {
            name: pf_name,
            args: pf_args,
            ..
        } = args[1].as_ref()
        else {
            panic!("expected the modifier to be a printf() call");
        };
        assert_eq!(pf_name.as_str(), "printf");
        assert!(
            matches!(pf_args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%+d days'")
        );
    }

    #[test]
    fn date_parts_lower_to_cast_strftime() {
        // YEAR(d) becomes CAST(strftime('%Y', d) AS INTEGER); same shape for the
        // other parts, differing only in the format code.
        for (sql, fmt) in [
            ("YEAR(d)", "'%Y'"),
            ("MONTH(d)", "'%m'"),
            ("DAY(d)", "'%d'"),
            ("DAYOFYEAR(d)", "'%j'"),
            ("HOUR(d)", "'%H'"),
            ("MINUTE(d)", "'%M'"),
            ("SECOND(d)", "'%S'"),
            // EXTRACT(unit FROM d) shares the same lowering.
            ("EXTRACT(YEAR FROM d)", "'%Y'"),
            ("EXTRACT(MONTH FROM d)", "'%m'"),
            ("EXTRACT(SECOND FROM d)", "'%S'"),
        ] {
            let ast::Expr::Cast { expr, type_name } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CAST");
            };
            assert_eq!(type_name.unwrap().name, "INTEGER");
            let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
                panic!("expected strftime call inside the cast for `{sql}`");
            };
            assert_eq!(name.as_str(), "strftime");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == fmt),
                "wrong format code for `{sql}`"
            );
        }
        // EXTRACT(WEEK) maps to strftime %U; EXTRACT(QUARTER) to (month + 2) / 3.
        let ast::Expr::Cast { expr, .. } = parse_expr("EXTRACT(WEEK FROM d)").unwrap() else {
            panic!("expected EXTRACT(WEEK) to be a CAST of strftime");
        };
        let ast::Expr::FunctionCall { args, .. } = expr.as_ref() else {
            unreachable!()
        };
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%U'")
        );
        assert_eq!(
            parse_expr("EXTRACT(QUARTER FROM d)").unwrap(),
            parse_expr("QUARTER(d)").unwrap()
        );

        // MICROSECOND and the `*_MICROSECOND` compound units remain rejected
        // (the engine's strftime has no microsecond precision).
        for sql in ["EXTRACT(MICROSECOND FROM d)", "EXTRACT(DAY_MICROSECOND FROM d)"] {
            assert!(
                parse_expr(sql).is_err(),
                "expected `{sql}` to be unsupported"
            );
        }

        // The non-microsecond compound units combine their fields into one
        // integer, e.g. YEAR_MONTH is `year*100 + month`.
        let ast::Expr::Binary(year_term, ast::Operator::Add, month_term) =
            parse_expr("EXTRACT(YEAR_MONTH FROM d)").unwrap()
        else {
            panic!("expected YEAR_MONTH to be year*100 + month");
        };
        assert!(matches!(
            year_term.as_ref(),
            ast::Expr::Binary(_, ast::Operator::Multiply, _)
        ));
        assert!(matches!(month_term.as_ref(), ast::Expr::Cast { .. }));
        // The four-field DAY_SECOND parses too.
        assert!(parse_expr("EXTRACT(DAY_SECOND FROM d)").is_ok());
    }

    #[test]
    fn quarter_and_weekofyear_lower_to_strftime() {
        // QUARTER(d) -> (CAST(strftime('%m', d) AS INTEGER) + 2) / 3.
        let ast::Expr::Binary(num, ast::Operator::Divide, three) =
            parse_expr("QUARTER(d)").unwrap()
        else {
            panic!("expected QUARTER to lower to a division");
        };
        assert!(matches!(three.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "3"));
        let ast::Expr::Binary(_, ast::Operator::Add, two) = num.as_ref() else {
            panic!("expected `month + 2` on the left of the division");
        };
        assert!(matches!(two.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "2"));

        // WEEKOFYEAR(d) -> CAST(strftime('%V', d) AS INTEGER).
        let ast::Expr::Cast { expr, .. } = parse_expr("WEEKOFYEAR(d)").unwrap() else {
            panic!("expected WEEKOFYEAR to lower to a CAST");
        };
        let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
            panic!("expected strftime inside the cast");
        };
        assert_eq!(name.as_str(), "strftime");
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%V'")
        );
    }

    #[test]
    fn day_of_week_functions_lower_to_weekday_arithmetic() {
        // DAYOFWEEK(d) -> CAST(strftime('%w', d) AS INTEGER) + 1.
        let ast::Expr::Binary(lhs, ast::Operator::Add, rhs) = parse_expr("DAYOFWEEK(d)").unwrap()
        else {
            panic!("expected DAYOFWEEK to lower to an addition");
        };
        assert!(matches!(lhs.as_ref(), ast::Expr::Cast { .. }));
        assert!(matches!(rhs.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1"));

        // WEEKDAY(d) -> (CAST(strftime('%w', d) AS INTEGER) + 6) % 7.
        let ast::Expr::Binary(inner, ast::Operator::Modulus, modulus) =
            parse_expr("WEEKDAY(d)").unwrap()
        else {
            panic!("expected WEEKDAY to lower to a modulo");
        };
        assert!(
            matches!(modulus.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "7")
        );
        let ast::Expr::Binary(_, ast::Operator::Add, six) = inner.as_ref() else {
            panic!("expected an addition inside the modulo");
        };
        assert!(matches!(six.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "6"));
    }

    #[test]
    fn week_lowers_to_cast_strftime() {
        // WEEK(d) defaults to mode 0 (%U); modes 3 and 5 map to %V and %W.
        for (sql, fmt) in [
            ("WEEK(d)", "'%U'"),
            ("WEEK(d, 0)", "'%U'"),
            ("WEEK(d, 3)", "'%V'"),
            ("WEEK(d, 5)", "'%W'"),
        ] {
            let ast::Expr::Cast { expr, type_name } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CAST");
            };
            assert_eq!(type_name.unwrap().name, "INTEGER");
            let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
                panic!("expected strftime call inside the cast for `{sql}`");
            };
            assert_eq!(name.as_str(), "strftime");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == fmt),
                "wrong format code for `{sql}`"
            );
        }
        // Only mode 6 has no clean engine equivalent and is rejected.
        assert!(
            parse_expr("WEEK(d, 6)").is_err(),
            "WEEK mode 6 should be unsupported"
        );
        // Modes 2, 4, and 7 are supported (they lower to a CASE / arithmetic
        // expression rather than a bare CAST, so they parse without error).
        for mode in [2, 4, 7] {
            assert!(
                parse_expr(&format!("WEEK(d, {mode})")).is_ok(),
                "WEEK mode {mode} should be supported"
            );
        }
    }

    #[test]
    fn week_mode_2_and_7_push_week_zero_to_previous_year() {
        // WEEK(d, 2) / WEEK(d, 7) -> CASE WHEN <code>(d) = 0 THEN <code>(prev year
        // end) ELSE <code>(d) END, where <code> is %U (mode 2) / %W (mode 7).
        for (sql, fmt) in [("WEEK(d, 2)", "'%U'"), ("WEEK(d, 7)", "'%W'")] {
            let ast::Expr::Case {
                base,
                when_then_pairs,
                else_expr,
            } = parse_expr(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            assert!(base.is_none());
            assert_eq!(when_then_pairs.len(), 1);
            // Guard: <code>(d) = 0.
            let ast::Expr::Binary(_, ast::Operator::Equals, zero) = when_then_pairs[0].0.as_ref()
            else {
                panic!("expected an equality guard for `{sql}`");
            };
            assert!(matches!(zero.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0"));
            // The else branch is CAST(strftime(<fmt>, d) AS INTEGER).
            let ast::Expr::Cast { expr, .. } = else_expr.unwrap().as_ref().clone() else {
                panic!("expected a CAST else branch for `{sql}`");
            };
            let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
                panic!("expected a strftime call for `{sql}`");
            };
            assert_eq!(name.as_str(), "strftime");
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == fmt),
                "wrong format code for `{sql}`"
            );
        }
    }

    #[test]
    fn week_mode_4_adds_year_start_offset_to_sunday_week() {
        // WEEK(d, 4) -> CAST(strftime('%U', d) AS INTEGER) + CASE ... offset.
        let ast::Expr::Binary(lhs, ast::Operator::Add, rhs) = parse_expr("WEEK(d, 4)").unwrap()
        else {
            panic!("expected WEEK(d, 4) to lower to an addition");
        };
        // Left side is the Sunday-first week number, CAST(strftime('%U', d) ...).
        let ast::Expr::Cast { expr, .. } = lhs.as_ref() else {
            panic!("expected a CAST on the left of the addition");
        };
        let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
            panic!("expected a strftime call");
        };
        assert_eq!(name.as_str(), "strftime");
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%U'")
        );
        // Right side is a CASE yielding the 0/1 per-year offset.
        assert!(matches!(rhs.as_ref(), ast::Expr::Case { .. }));
    }

    #[test]
    fn week_mode_1_lowers_to_iso_week_with_boundary_case() {
        // WEEK(d, 1) -> CASE WHEN %G < %Y THEN 0 WHEN %G > %Y THEN 53 ELSE %V END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("WEEK(d, 1)").unwrap()
        else {
            panic!("expected WEEK(d, 1) to lower to a CASE");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 2);
        // First guard compares ISO year below the calendar year, yielding 0.
        assert!(matches!(
            when_then_pairs[0].0.as_ref(),
            ast::Expr::Binary(_, ast::Operator::Less, _)
        ));
        assert!(matches!(&*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0"));
        assert!(matches!(
            when_then_pairs[1].0.as_ref(),
            ast::Expr::Binary(_, ast::Operator::Greater, _)
        ));
        assert!(matches!(&*when_then_pairs[1].1, ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "53"));
        // The else branch is the ISO week, CAST(strftime('%V', d) AS INTEGER).
        let else_branch = else_expr.unwrap();
        let ast::Expr::Cast { expr, .. } = else_branch.as_ref() else {
            panic!("expected a CAST in the else branch");
        };
        let ast::Expr::FunctionCall { name, args, .. } = expr.as_ref() else {
            panic!("expected strftime in the else branch");
        };
        assert_eq!(name.as_str(), "strftime");
        assert!(matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%V'"));
    }

    #[test]
    fn yearweek_lowers_by_mode() {
        // Modes 1 and 3 are the ISO year-week: %G * 100 + %V (an addition whose
        // left side multiplies the ISO year by 100).
        for sql in ["YEARWEEK(d, 1)", "YEARWEEK(d, 3)"] {
            let ast::Expr::Binary(year, ast::Operator::Add, week) = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to be `year + week`");
            };
            let ast::Expr::Binary(g, ast::Operator::Multiply, hundred) = year.as_ref() else {
                panic!("expected `%G * 100` for `{sql}`");
            };
            assert!(matches!(hundred.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "100"));
            // %G inside the cast.
            let ast::Expr::Cast { expr, .. } = g.as_ref() else { panic!("expected a cast") };
            let ast::Expr::FunctionCall { args, .. } = expr.as_ref() else { unreachable!() };
            assert!(matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%G'"));
            // %V week on the right.
            let ast::Expr::Cast { expr, .. } = week.as_ref() else { panic!("expected a cast") };
            let ast::Expr::FunctionCall { args, .. } = expr.as_ref() else { unreachable!() };
            assert!(matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%V'"));
        }

        // Modes 0 (default, %U) and 5 (%W) lower to a CASE with the week-zero
        // backward-push guard.
        for (sql, code) in [("YEARWEEK(d)", "'%U'"), ("YEARWEEK(d, 0)", "'%U'"), ("YEARWEEK(d, 5)", "'%W'")] {
            let ast::Expr::Case { when_then_pairs, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            // The guard is `week == 0`, week being CAST(strftime(code, d) AS INTEGER).
            let ast::Expr::Binary(week, ast::Operator::Equals, zero) = when_then_pairs[0].0.as_ref()
            else {
                panic!("expected a `week = 0` guard for `{sql}`");
            };
            assert!(matches!(zero.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0"));
            let ast::Expr::Cast { expr, .. } = week.as_ref() else { panic!("expected a cast") };
            let ast::Expr::FunctionCall { args, .. } = expr.as_ref() else { unreachable!() };
            assert!(
                matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == code),
                "wrong week code for `{sql}`"
            );
        }

        // The unclean modes are rejected, as for WEEK.
        for mode in [2, 4, 6, 7] {
            assert!(parse_expr(&format!("YEARWEEK(d, {mode})")).is_err());
        }
    }

    #[test]
    fn datediff_lowers_to_julianday_difference() {
        // DATEDIFF(a, b) -> CAST(julianday(date(a)) - julianday(date(b)) AS INTEGER).
        let ast::Expr::Cast { expr, type_name } = parse_expr("DATEDIFF(d1, d2)").unwrap() else {
            panic!("expected DATEDIFF to lower to a CAST");
        };
        assert_eq!(type_name.unwrap().name, "INTEGER");
        let ast::Expr::Binary(lhs, ast::Operator::Subtract, rhs) = expr.as_ref() else {
            panic!("expected a subtraction inside the cast");
        };
        for side in [lhs.as_ref(), rhs.as_ref()] {
            let ast::Expr::FunctionCall { name, args, .. } = side else {
                panic!("expected julianday(...) on each side");
            };
            assert_eq!(name.as_str(), "julianday");
            assert!(matches!(
                args[0].as_ref(),
                ast::Expr::FunctionCall { name, .. } if name.as_str() == "date"
            ));
        }
    }

    #[test]
    fn last_day_lowers_to_date_modifiers() {
        // LAST_DAY(d) -> date(d, 'start of month', '+1 month', '-1 day').
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("LAST_DAY(d)").unwrap() else {
            panic!("expected LAST_DAY to lower to a function call");
        };
        assert_eq!(name.as_str(), "date");
        let modifiers: Vec<&str> = args[1..]
            .iter()
            .map(|a| match a.as_ref() {
                ast::Expr::Literal(ast::Literal::String(s)) => s.as_str(),
                _ => panic!("expected string modifiers"),
            })
            .collect();
        assert_eq!(modifiers, ["'start of month'", "'+1 month'", "'-1 day'"]);
    }

    #[test]
    fn timestampdiff_lowers_to_epoch_division() {
        // A fixed-duration unit divides the epoch-second difference (b - a).
        let ast::Expr::Binary(diff, ast::Operator::Divide, divisor) =
            parse_expr("TIMESTAMPDIFF(HOUR, a, b)").unwrap()
        else {
            panic!("expected a division");
        };
        assert!(
            matches!(divisor.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "3600")
        );
        let ast::Expr::Binary(left, ast::Operator::Subtract, _) = diff.as_ref() else {
            panic!("expected unixepoch(b) - unixepoch(a)");
        };
        assert!(matches!(
            left.as_ref(),
            ast::Expr::FunctionCall { name, .. } if name.as_str() == "unixepoch"
        ));

        // SECOND needs no division — just the epoch-second difference.
        assert!(matches!(
            parse_expr("TIMESTAMPDIFF(SECOND, a, b)").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Subtract, _)
        ));

        // The calendar units count whole months: MONTH lowers to a CASE (the
        // complete-month adjustment), QUARTER/YEAR divide that by 3 / 12.
        assert!(matches!(
            parse_expr("TIMESTAMPDIFF(MONTH, a, b)").unwrap(),
            ast::Expr::Case { .. }
        ));
        for (unit, div) in [("QUARTER", "3"), ("YEAR", "12")] {
            let ast::Expr::Binary(months, ast::Operator::Divide, divisor) =
                parse_expr(&format!("TIMESTAMPDIFF({unit}, a, b)")).unwrap()
            else {
                panic!("expected {unit} to divide the month count");
            };
            assert!(matches!(months.as_ref(), ast::Expr::Case { .. }), "{unit}");
            assert!(
                matches!(divisor.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == div),
                "{unit}"
            );
        }

        // MICROSECOND stays rejected: the engine has only millisecond precision.
        assert!(parse_expr("TIMESTAMPDIFF(MICROSECOND, a, b)").is_err());
    }

    #[test]
    fn timestampadd_lowers_to_datetime_modifier() {
        // TIMESTAMPADD(unit, n, dt) -> datetime(dt, '+<n × mult> <engine-unit>'),
        // matching DATE_ADD(dt, INTERVAL n unit). WEEK and QUARTER scale the
        // amount (7 days, 3 months).
        for (sql, modifier) in [
            ("TIMESTAMPADD(DAY, 5, d)", "'+5 days'"),
            ("TIMESTAMPADD(HOUR, 2, d)", "'+2 hours'"),
            ("TIMESTAMPADD(WEEK, 1, d)", "'+7 days'"),
            ("TIMESTAMPADD(QUARTER, 1, d)", "'+3 months'"),
            ("TIMESTAMPADD(MINUTE, -30, d)", "'-30 minutes'"),
        ] {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to datetime()");
            };
            assert_eq!(name.as_str(), "datetime");
            assert!(
                matches!(args[1].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == modifier),
                "wrong modifier for `{sql}`"
            );
        }
        // TIMESTAMPADD matches the equivalent DATE_ADD lowering.
        assert_eq!(
            parse_expr("TIMESTAMPADD(DAY, 5, d)").unwrap(),
            parse_expr("DATE_ADD(d, INTERVAL 5 DAY)").unwrap()
        );
        // A unit without an engine modifier and a non-literal amount are rejected.
        assert!(parse_expr("TIMESTAMPADD(MICROSECOND, 1, d)").is_err());
        assert!(parse_expr("TIMESTAMPADD(DAY, x, d)").is_err());
    }

    #[test]
    fn addtime_subtime_lower_to_typed_time_shift() {
        // ADDTIME(e, t) -> CASE WHEN e LIKE '%-%' THEN datetime(e, t)
        //                       ELSE time(e, t) END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("ADDTIME(e, t)").unwrap()
        else {
            panic!("expected ADDTIME to lower to a CASE");
        };
        // The guard is a LIKE on the first argument.
        let ast::Expr::Like { op, .. } = when_then_pairs[0].0.as_ref() else {
            panic!("expected a LIKE guard");
        };
        assert_eq!(*op, ast::LikeOperator::Like);
        // THEN is datetime(e, t); ELSE is time(e, t), both with the bare amount.
        let ast::Expr::FunctionCall { name, args, .. } = when_then_pairs[0].1.as_ref() else {
            unreachable!()
        };
        assert_eq!(name.as_str(), "datetime");
        assert_eq!(*args[1], col("t"));
        let else_branch = else_expr.unwrap();
        let ast::Expr::FunctionCall { name, .. } = else_branch.as_ref() else {
            unreachable!()
        };
        assert_eq!(name.as_str(), "time");

        // SUBTIME negates the amount: the datetime/time modifier is `'-' || t`.
        let ast::Expr::Case { when_then_pairs, .. } = parse_expr("SUBTIME(e, t)").unwrap() else {
            unreachable!()
        };
        let ast::Expr::FunctionCall { args, .. } = when_then_pairs[0].1.as_ref() else {
            unreachable!()
        };
        let ast::Expr::Binary(minus, ast::Operator::Concat, amt) = args[1].as_ref() else {
            panic!("expected `'-' || t` modifier");
        };
        assert!(matches!(minus.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'-'"));
        assert_eq!(**amt, col("t"));
    }

    #[test]
    fn maketime_lowers_to_guarded_printf() {
        // MAKETIME(h, m, s) -> CASE WHEN <null/range guard> THEN NULL ELSE
        // printf('%s%02d:%02d:%02d', sign, abs(h), m, s) -- the sign is split from
        // the hour so a negative hour renders as `-01:..`, not `-1:..`.
        let ast::Expr::Case {
            base, else_expr, ..
        } = parse_expr("MAKETIME(h, m, s)").unwrap()
        else {
            panic!("expected MAKETIME to lower to a guarded CASE");
        };
        assert!(base.is_none());
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE to be a printf() call");
        };
        assert_eq!(name.as_str(), "printf");
        assert_eq!(args.len(), 5);
        assert!(
            matches!(args[0].as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'%s%02d:%02d:%02d'")
        );
        // The hour magnitude is abs(h).
        assert!(
            matches!(args[2].as_ref(), ast::Expr::FunctionCall { name, .. } if name.as_str() == "abs")
        );
    }

    #[test]
    fn makedate_lowers_to_guarded_date() {
        // MAKEDATE(y, doy) -> CASE WHEN <null/<1 guard> THEN NULL ELSE date(...).
        let ast::Expr::Case {
            base, else_expr, ..
        } = parse_expr("MAKEDATE(y, doy)").unwrap()
        else {
            panic!("expected MAKEDATE to lower to a guarded CASE");
        };
        assert!(base.is_none());
        // The ELSE is date(printf('%04d-01-01', y), printf('%+d days', doy - 1)).
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE to be a date() call");
        };
        assert_eq!(name.as_str(), "date");
        assert_eq!(args.len(), 2);
        for arg in &args {
            assert!(
                matches!(arg.as_ref(), ast::Expr::FunctionCall { name, .. } if name.as_str() == "printf")
            );
        }
    }

    #[test]
    fn to_days_and_from_days_lower_to_julian_offset() {
        // TO_DAYS(d) -> CAST(julianday(date(d)) AS INTEGER) - 1721059.
        let ast::Expr::Binary(lhs, ast::Operator::Subtract, rhs) =
            parse_expr("TO_DAYS(d)").unwrap()
        else {
            panic!("expected TO_DAYS to lower to a subtraction");
        };
        assert!(matches!(lhs.as_ref(), ast::Expr::Cast { .. }));
        assert!(
            matches!(rhs.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1721059")
        );

        // FROM_DAYS(n) -> date(n + 1721059.5).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("FROM_DAYS(n)").unwrap() else {
            panic!("expected FROM_DAYS to lower to a date() call");
        };
        assert_eq!(name.as_str(), "date");
        let ast::Expr::Binary(_, ast::Operator::Add, off) = args[0].as_ref() else {
            panic!("expected `n + offset`");
        };
        assert!(
            matches!(off.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1721059.5")
        );
    }

    #[test]
    fn to_seconds_lowers_to_days_times_86400_plus_time() {
        // TO_SECONDS(d) -> TO_DAYS(d) * 86400 + TIME_TO_SEC(d).
        let ast::Expr::Binary(days_term, ast::Operator::Add, time_term) =
            parse_expr("TO_SECONDS(d)").unwrap()
        else {
            panic!("expected TO_SECONDS to lower to an addition");
        };
        // The left term is the day number scaled by 86400.
        let ast::Expr::Binary(to_days, ast::Operator::Multiply, scale) = days_term.as_ref() else {
            panic!("expected `TO_DAYS(d) * 86400`");
        };
        assert_eq!(**to_days, parse_expr("TO_DAYS(d)").unwrap());
        assert!(matches!(scale.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "86400"));
        // The right term is the time-of-day seconds.
        assert_eq!(*time_term, parse_expr("TIME_TO_SEC(d)").unwrap());
    }

    #[test]
    fn period_diff_and_add_lower_to_month_arithmetic() {
        // PERIOD_DIFF(p1, p2) -> months(p1) - months(p2): a subtraction of the
        // two period-to-month conversions (each a `* 12 + month` over a
        // year-normalizing CASE).
        let ast::Expr::Binary(left, ast::Operator::Subtract, _right) =
            parse_expr("PERIOD_DIFF(a, b)").unwrap()
        else {
            panic!("expected PERIOD_DIFF to lower to a subtraction");
        };
        // Each side is `normalized_year * 12 + month`.
        let ast::Expr::Binary(ny12, ast::Operator::Add, _month) = left.as_ref() else {
            panic!("expected `year_months + month`");
        };
        let ast::Expr::Binary(ny, ast::Operator::Multiply, twelve) = ny12.as_ref() else {
            panic!("expected `normalized_year * 12`");
        };
        assert!(matches!(twelve.as_ref(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "12"));
        assert!(matches!(ny.as_ref(), ast::Expr::Case { .. }));

        // PERIOD_ADD(p, n) -> year_part + month_part, the top being an addition.
        assert!(matches!(
            parse_expr("PERIOD_ADD(p, 3)").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Add, _)
        ));
        // Both require two arguments.
        assert!(parse_expr("PERIOD_DIFF(a)").is_err());
        assert!(parse_expr("PERIOD_ADD(a)").is_err());
    }

    #[test]
    fn length_lowers_to_length_of_blob_cast() {
        // LENGTH(b) becomes length(CAST(b AS BLOB)) to get a byte count.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("LENGTH(b)").unwrap() else {
            panic!("expected LENGTH to lower to a function call");
        };
        assert_eq!(name.as_str(), "length");
        assert_eq!(args.len(), 1);
        let ast::Expr::Cast { type_name, .. } = args[0].as_ref() else {
            panic!("expected the argument to be a CAST");
        };
        assert_eq!(type_name.as_ref().unwrap().name, "BLOB");

        // OCTET_LENGTH is a synonym for LENGTH and lowers identically.
        assert_eq!(
            parse_expr("OCTET_LENGTH(b)").unwrap(),
            parse_expr("LENGTH(b)").unwrap()
        );

        // BIT_LENGTH(b) is 8 * length(CAST(b AS BLOB)).
        let ast::Expr::Binary(lhs, ast::Operator::Multiply, rhs) =
            parse_expr("BIT_LENGTH(b)").unwrap()
        else {
            panic!("expected BIT_LENGTH to lower to a multiplication");
        };
        assert!(matches!(
            lhs.as_ref(),
            ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "8"
        ));
        assert_eq!(*rhs, parse_expr("LENGTH(b)").unwrap());
    }

    #[test]
    fn left_lowers_to_substr() {
        // LEFT(b, 4) becomes substr(b, 1, 4).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("LEFT(b, 4)").unwrap() else {
            panic!("expected LEFT to lower to a function call");
        };
        assert_eq!(name.as_str(), "substr");
        assert_eq!(args.len(), 3);
        assert!(matches!(
            args[1].as_ref(),
            ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1"
        ));
    }

    #[test]
    fn right_lowers_to_substr_from_end() {
        // RIGHT(b, 4) becomes substr(b, 0 - 4, 4): a negative start counts from
        // the end, and the third argument is the same length.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("RIGHT(b, 4)").unwrap() else {
            panic!("expected RIGHT to lower to a function call");
        };
        assert_eq!(name.as_str(), "substr");
        assert_eq!(args.len(), 3);
        assert!(
            matches!(
                args[1].as_ref(),
                ast::Expr::Binary(_, ast::Operator::Subtract, _)
            ),
            "expected the start argument to be `0 - len`"
        );
    }

    #[test]
    fn group_concat_lowers_to_engine_group_concat() {
        // GROUP_CONCAT(v) -> group_concat(v); the SEPARATOR becomes a 2nd arg.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("GROUP_CONCAT(v)").unwrap()
        else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "group_concat");
        assert_eq!(args.len(), 1);

        let ast::Expr::FunctionCall { args, .. } =
            parse_expr("GROUP_CONCAT(v SEPARATOR '-')").unwrap()
        else {
            panic!("expected a function call");
        };
        assert_eq!(args.len(), 2);
        assert!(matches!(
            args[1].as_ref(),
            ast::Expr::Literal(ast::Literal::String(s)) if s == "'-'"
        ));

        // DISTINCT lowers to a DISTINCT group_concat (single argument).
        let ast::Expr::FunctionCall {
            name,
            distinctness,
            args,
            ..
        } = parse_expr("GROUP_CONCAT(DISTINCT v)").unwrap()
        else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "group_concat");
        assert_eq!(distinctness, Some(ast::Distinctness::Distinct));
        assert_eq!(args.len(), 1);

        // An inner ORDER BY, multiple expressions, and DISTINCT with a custom
        // SEPARATOR (a DISTINCT engine aggregate may take only one argument) are
        // rejected.
        for sql in [
            "GROUP_CONCAT(v ORDER BY v)",
            "GROUP_CONCAT(a, b)",
            "GROUP_CONCAT(DISTINCT v SEPARATOR '-')",
        ] {
            assert!(
                parse_expr(sql).is_err(),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn instr_and_locate_lower_to_case_insensitive_instr() {
        // INSTR(str, substr) -> instr(lower(str), lower(substr)).
        let ast::Expr::FunctionCall { name, args, .. } =
            parse_expr("INSTR('Haystack', 'NEEDLE')").unwrap()
        else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "instr");
        assert_eq!(args.len(), 2);
        for a in &args {
            assert!(matches!(
                a.as_ref(),
                ast::Expr::FunctionCall { name, .. } if name.as_str() == "lower"
            ));
        }

        // LOCATE swaps the operands: LOCATE(needle, haystack) puts the haystack
        // first, as instr() expects.
        let ast::Expr::FunctionCall { args, .. } =
            parse_expr("LOCATE('NEEDLE', 'Haystack')").unwrap()
        else {
            panic!("expected a function call");
        };
        let inner = |e: &ast::Expr| match e {
            ast::Expr::FunctionCall { args, .. } => match args[0].as_ref() {
                ast::Expr::Literal(ast::Literal::String(s)) => s.clone(),
                _ => panic!("expected a string literal inside lower()"),
            },
            _ => panic!("expected lower(...)"),
        };
        assert_eq!(inner(args[0].as_ref()), "'Haystack'");
        assert_eq!(inner(args[1].as_ref()), "'NEEDLE'");

        // POSITION(substr IN str) is the SQL-standard LOCATE and lowers the same
        // way (instr(lower(str), lower(substr))).
        assert_eq!(
            parse_expr("POSITION('NEEDLE' IN 'Haystack')").unwrap(),
            parse_expr("LOCATE('NEEDLE', 'Haystack')").unwrap()
        );

        // The 3-argument LOCATE(substr, str, pos) form searches from `pos`,
        // lowering to a guarded CASE over an offset instr(); INSTR stays 2-arg.
        assert!(matches!(
            parse_expr("LOCATE('a', 'banana', 3)").unwrap(),
            ast::Expr::Case { .. }
        ));
        assert!(parse_expr("INSTR('banana', 'a', 3)").is_err());
    }

    #[test]
    fn substring_from_for_lowers_like_comma_form() {
        // The SQL-standard SUBSTRING(str FROM pos FOR len) lowers identically to
        // the comma form SUBSTRING(str, pos, len) -> a CASE-guarded substr (the
        // guard matches MySQL's out-of-range semantics).
        let from_for = parse_expr("SUBSTRING('hello' FROM 2 FOR 3)").unwrap();
        assert_eq!(from_for, parse_expr("SUBSTRING('hello', 2, 3)").unwrap());
        // The lowering is a CASE whose else branch is substr(str, pos, len).
        let ast::Expr::Case { else_expr, .. } = &from_for else {
            panic!("expected a CASE-guarded substr");
        };
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.as_deref().unwrap() else {
            panic!("expected substr in the else branch");
        };
        assert_eq!(name.as_str(), "substr");
        assert_eq!(args.len(), 3);

        // FROM without FOR is the two-argument substr(str, pos).
        let from_only = parse_expr("SUBSTRING('hello' FROM 2)").unwrap();
        assert_eq!(from_only, parse_expr("SUBSTRING('hello', 2)").unwrap());

        // SUBSTR and MID share the exact same lowering.
        assert_eq!(
            parse_expr("SUBSTR('hello' FROM 2 FOR 3)").unwrap(),
            parse_expr("SUBSTRING('hello', 2, 3)").unwrap()
        );
        assert_eq!(
            parse_expr("MID('hello', 2, 3)").unwrap(),
            parse_expr("SUBSTRING('hello', 2, 3)").unwrap()
        );
    }

    #[test]
    fn rand_lowers_to_random_division() {
        // RAND() -> abs(random() % N) / N.0 (a float in [0, 1)).
        let ast::Expr::Binary(_, ast::Operator::Divide, _) = parse_expr("RAND()").unwrap() else {
            panic!("expected RAND() to lower to a division");
        };
        // A seed argument is accepted (and discarded), still a division.
        assert!(matches!(
            parse_expr("RAND(5)").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Divide, _)
        ));
    }

    #[test]
    fn advisory_locks_fold_to_constants() {
        // GET_LOCK / RELEASE_LOCK / IS_FREE_LOCK fold to 1 regardless of arguments.
        for sql in [
            "GET_LOCK('x', 0)",
            "GET_LOCK('x', 10)",
            "RELEASE_LOCK('x')",
            "IS_FREE_LOCK('x')",
        ] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1"),
                "expected `{sql}` to fold to 1"
            );
        }
        // IS_USED_LOCK folds to NULL, RELEASE_ALL_LOCKS to 0.
        assert!(matches!(
            parse_expr("IS_USED_LOCK('x')").unwrap(),
            ast::Expr::Literal(ast::Literal::Null)
        ));
        assert!(
            matches!(parse_expr("RELEASE_ALL_LOCKS()").unwrap(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0")
        );
    }

    #[test]
    fn sleep_and_benchmark_fold_to_zero() {
        // The timing functions are no-ops folding to 0, regardless of arguments.
        for sql in ["SLEEP(0)", "SLEEP(10)", "BENCHMARK(1, 1 + 1)", "BENCHMARK(1000, x)"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "0"),
                "expected `{sql}` to fold to 0"
            );
        }
    }

    #[test]
    fn field_lowers_to_case() {
        // FIELD(x, a, b) -> CASE x COLLATE NOCASE WHEN a THEN 1 WHEN b THEN 2
        // ELSE 0 END (the NOCASE base folds case, like MySQL's default collation).
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("FIELD(id, 3, 1, 2)").unwrap()
        else {
            panic!("expected FIELD to lower to a CASE");
        };
        assert_eq!(
            base.as_deref(),
            Some(&ast::Expr::collate(col("id"), ast::Name::from_string("NOCASE")))
        );
        assert_eq!(when_then_pairs.len(), 3);
        // The THEN results are the 1-based indices.
        for (i, (_, then)) in when_then_pairs.iter().enumerate() {
            assert_eq!(**then, num(&(i + 1).to_string()));
        }
        assert_eq!(else_expr.as_deref(), Some(&num("0")));
    }

    #[test]
    fn elt_lowers_to_case_without_else() {
        // ELT(n, a, b, c) -> CASE <int n> WHEN 1 THEN a WHEN 2 THEN b WHEN 3 THEN c
        // END, where <int n> is the index coerced to an integer like CAST(n AS
        // SIGNED) (so a numeric/string index selects the right arm).
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("ELT(n, 'a', 'b', 'c')").unwrap()
        else {
            panic!("expected ELT to lower to a CASE");
        };
        // The base is the integer-coerced index: the same `typeof`-guarded CASE
        // `CAST(n AS SIGNED)` produces.
        assert_eq!(base.as_deref(), Some(&parse_expr("CAST(n AS SIGNED)").unwrap()));
        assert_eq!(when_then_pairs.len(), 3);
        // The WHEN labels are the 1-based indices.
        for (i, (when, _)) in when_then_pairs.iter().enumerate() {
            assert_eq!(**when, num(&(i + 1).to_string()));
        }
        // No ELSE, so an out-of-range / NULL index yields NULL.
        assert!(else_expr.is_none());

        // ELT requires the index plus at least one string.
        assert!(matches!(
            parse_expr("ELT(1)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn make_set_lowers_to_concat_ws_of_bit_cases() {
        // MAKE_SET(bits, a, b, c) -> CASE WHEN bits IS NULL THEN NULL ELSE
        // concat_ws(',', CASE WHEN bits & 1 THEN a END, ..., bits & 4 ...) END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("MAKE_SET(bits, 'a', 'b', 'c')").unwrap()
        else {
            panic!("expected MAKE_SET to lower to a CASE");
        };
        assert!(base.is_none());
        // The outer guard is the NULL-bits check.
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Null));

        // The ELSE is concat_ws(',', <three bit CASEs>).
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE to be concat_ws()");
        };
        assert_eq!(name.as_str(), "concat_ws");
        assert_eq!(args.len(), 4); // separator + three strings
        assert_eq!(*args[0], ast::Expr::Literal(ast::Literal::String("','".to_string())));
        // Each remaining arg is a `bits & 2^i` test guarding the string.
        for (i, arg) in args[1..].iter().enumerate() {
            let ast::Expr::Case { when_then_pairs, .. } = arg.as_ref() else {
                panic!("expected a bit-test CASE");
            };
            let ast::Expr::Binary(_, ast::Operator::BitwiseAnd, mask) = &*when_then_pairs[0].0 else {
                panic!("expected a `bits & mask` test");
            };
            assert_eq!(**mask, num(&(1u64 << i).to_string()));
        }

        // At least one string argument is required.
        assert!(matches!(
            parse_expr("MAKE_SET(5)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn inet_ntoa_lowers_to_octet_concatenation() {
        // INET_NTOA(n) -> ((n>>24)&255) || '.' || ... || (n&255): a left-nested
        // chain of `||` whose leaves are the four masked octets and three dots.
        let expr = parse_expr("INET_NTOA(n)").unwrap();
        // Collect the operands of the `||` chain in order.
        fn flatten(e: &ast::Expr, out: &mut Vec<ast::Expr>) {
            if let ast::Expr::Binary(l, ast::Operator::Concat, r) = e {
                flatten(l, out);
                out.push((**r).clone());
            } else {
                out.push(e.clone());
            }
        }
        let mut parts = Vec::new();
        flatten(&expr, &mut parts);
        assert_eq!(parts.len(), 7, "four octets and three dots");
        // The dots are at the odd positions.
        for i in [1, 3, 5] {
            assert_eq!(
                parts[i],
                ast::Expr::Literal(ast::Literal::String("'.'".to_string()))
            );
        }
        // Each octet is `<x> & 255`.
        for i in [0, 2, 4, 6] {
            let ast::Expr::Binary(_, ast::Operator::BitwiseAnd, mask) = &parts[i] else {
                panic!("expected an octet mask at {i}");
            };
            assert_eq!(**mask, num("255"));
        }
        // The first octet shifts right by 24; the last is not shifted.
        let ast::Expr::Binary(shifted, ast::Operator::BitwiseAnd, _) = &parts[0] else {
            unreachable!()
        };
        assert!(matches!(
            shifted.as_ref(),
            ast::Expr::Binary(_, ast::Operator::RightShift, _)
        ));
        let ast::Expr::Binary(low, ast::Operator::BitwiseAnd, _) = &parts[6] else {
            unreachable!()
        };
        assert_eq!(**low, col("n"));
    }

    #[test]
    fn bit_count_lowers_to_balanced_sum_of_bits() {
        // BIT_COUNT(n) sums 64 bit-tests `(n >> i) & 1` in a balanced tree of
        // additions, so the top node is an addition and the tree stays shallow.
        let expr = parse_expr("BIT_COUNT(n)").unwrap();
        let ast::Expr::Binary(_, ast::Operator::Add, _) = &expr else {
            panic!("expected the top of BIT_COUNT to be an addition");
        };
        // Collect the leaves and the tree depth.
        fn walk(e: &ast::Expr, depth: usize, leaves: &mut usize, max_depth: &mut usize) {
            match e {
                ast::Expr::Binary(l, ast::Operator::Add, r) => {
                    walk(l, depth + 1, leaves, max_depth);
                    walk(r, depth + 1, leaves, max_depth);
                }
                other => {
                    *leaves += 1;
                    *max_depth = (*max_depth).max(depth);
                    // Each leaf is `<bit> & 1`.
                    let ast::Expr::Binary(_, ast::Operator::BitwiseAnd, mask) = other else {
                        panic!("expected a `<bit> & 1` leaf");
                    };
                    assert_eq!(**mask, num("1"));
                }
            }
        }
        let mut leaves = 0;
        let mut max_depth = 0;
        walk(&expr, 0, &mut leaves, &mut max_depth);
        assert_eq!(leaves, 64, "one term per bit");
        // A balanced tree of 64 leaves is depth 6, far below a 64-deep chain.
        assert!(max_depth <= 6, "tree should be balanced, got depth {max_depth}");
    }

    #[test]
    fn bin_lowers_to_trimmed_flat_concat_of_bits() {
        // BIN(n) -> CASE WHEN n IS NULL THEN NULL WHEN n = 0 THEN '0'
        //           ELSE ltrim(concat(<64 bit CASEs>), '0') END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("BIN(n)").unwrap()
        else {
            panic!("expected BIN to lower to a guard CASE");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 2);
        // Guards: NULL -> NULL, 0 -> '0'.
        assert_eq!(*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Null));
        assert_eq!(
            *when_then_pairs[1].1,
            ast::Expr::Literal(ast::Literal::String("'0'".to_string()))
        );
        // ELSE is ltrim(concat(...), '0') — a flat 64-arg concat (no `||` nest).
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected ltrim()");
        };
        assert_eq!(name.as_str(), "ltrim");
        assert_eq!(args.len(), 2);
        let ast::Expr::FunctionCall { name, args, .. } = args[0].as_ref() else {
            panic!("expected concat()");
        };
        assert_eq!(name.as_str(), "concat");
        assert_eq!(args.len(), 64, "one bit char per bit, flat");
    }

    #[test]
    fn export_set_lowers_to_guarded_concat_ws() {
        // EXPORT_SET(bits, on, off, sep, n) -> CASE WHEN <null guard> THEN NULL
        // ELSE concat_ws(sep, CASE WHEN bits&1 THEN on ELSE off END, ... n ...).
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("EXPORT_SET(bits, 'Y', 'N', '-', 4)").unwrap()
        else {
            panic!("expected EXPORT_SET to lower to a guard CASE");
        };
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Null));
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected concat_ws");
        };
        assert_eq!(name.as_str(), "concat_ws");
        assert_eq!(args.len(), 5); // separator + four bit entries
        // Each entry is `CASE WHEN (bits >> i) & 1 THEN on ELSE off END` (the
        // shift is elided for bit 0), with the `off` value as the ELSE.
        for (i, arg) in args[1..].iter().enumerate() {
            let ast::Expr::Case { when_then_pairs, else_expr, .. } = arg.as_ref() else {
                panic!("expected a bit CASE");
            };
            let ast::Expr::Binary(left, ast::Operator::BitwiseAnd, mask) = &*when_then_pairs[0].0
            else {
                panic!("expected `<bit> & 1`");
            };
            assert_eq!(**mask, num("1"));
            if i == 0 {
                assert_eq!(**left, col("bits"));
            } else {
                assert!(matches!(
                    left.as_ref(),
                    ast::Expr::Binary(_, ast::Operator::RightShift, _)
                ));
            }
            assert!(else_expr.is_some(), "the off value is the ELSE");
        }

        // The separator and bit count default (',' and 64 entries).
        let ast::Expr::Case { else_expr, .. } = parse_expr("EXPORT_SET(b, 'Y', 'N')").unwrap()
        else {
            unreachable!()
        };
        let ast::Expr::FunctionCall { args, .. } = else_expr.unwrap().as_ref().clone() else {
            unreachable!()
        };
        assert_eq!(args.len(), 65); // separator + 64 entries
        assert_eq!(*args[0], ast::Expr::Literal(ast::Literal::String("','".to_string())));

        // A non-literal bit count is rejected.
        assert!(matches!(
            parse_expr("EXPORT_SET(b, 'Y', 'N', ',', c)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn logical_not_prefix_lowers_to_unary_not() {
        // !a -> NOT a (unary), at high precedence.
        assert_eq!(
            parse_expr("!a").unwrap(),
            ast::Expr::unary(ast::UnaryOperator::Not, col("a"))
        );

        // `!a = b` is `(!a) = b` (the `!` binds tighter than `=`).
        let ast::Expr::Binary(lhs, ast::Operator::Equals, _) = parse_expr("!a = b").unwrap() else {
            panic!("expected the top operator to be `=`");
        };
        assert_eq!(*lhs, ast::Expr::unary(ast::UnaryOperator::Not, col("a")));

        // `!=` (not-equal) is unaffected by the `!` prefix.
        assert_eq!(
            parse_expr("a != b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::NotEquals, col("b"))
        );
    }

    #[test]
    fn logical_xor_lowers_to_nested_not_equals() {
        // a XOR b -> (a <> 0) <> (b <> 0).
        let ast::Expr::Binary(lhs, ast::Operator::NotEquals, rhs) = parse_expr("a XOR b").unwrap()
        else {
            panic!("expected XOR to lower to a `<>`");
        };
        assert_eq!(
            *lhs,
            ast::Expr::binary(col("a"), ast::Operator::NotEquals, num("0"))
        );
        assert_eq!(
            *rhs,
            ast::Expr::binary(col("b"), ast::Operator::NotEquals, num("0"))
        );

        // Precedence: AND binds tighter than XOR, XOR tighter than OR.
        // `a OR b XOR c` parses as `a OR (b XOR c)`.
        let ast::Expr::Binary(_, ast::Operator::Or, or_rhs) = parse_expr("a OR b XOR c").unwrap()
        else {
            panic!("expected the top operator to be OR");
        };
        // The OR's right side is the XOR lowering (a `<>` of two `<>`s).
        assert!(matches!(
            or_rhs.as_ref(),
            ast::Expr::Binary(_, ast::Operator::NotEquals, _)
        ));
    }

    #[test]
    fn logical_and_operator_is_a_synonym_for_and() {
        // `a && b` lowers to the same AND as the keyword.
        assert_eq!(
            parse_expr("a && b").unwrap(),
            parse_expr("a AND b").unwrap()
        );
        assert_eq!(
            parse_expr("a && b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::And, col("b"))
        );

        // A single `&` is still bitwise AND, unaffected by the `&&` lexing.
        assert_eq!(
            parse_expr("a & b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::BitwiseAnd, col("b"))
        );
    }

    #[test]
    fn null_safe_equal_lowers_to_case() {
        // a <=> b -> CASE WHEN a IS NULL AND b IS NULL THEN 1
        //                 WHEN a IS NULL OR b IS NULL THEN 0 ELSE a = b END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("a <=> b").unwrap()
        else {
            panic!("expected <=> to lower to a CASE");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 2);
        // The two WHEN results are 1 (both NULL) then 0 (either NULL).
        assert_eq!(*when_then_pairs[0].1, num("1"));
        assert_eq!(*when_then_pairs[1].1, num("0"));
        // The ELSE is the ordinary equality.
        assert_eq!(
            *else_expr.unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Equals, col("b"))
        );

        // The new `<=>` lexing does not disturb `<=`, `>=`, or `<>`.
        for (sql, op) in [
            ("a <= b", ast::Operator::LessEquals),
            ("a >= b", ast::Operator::GreaterEquals),
            ("a <> b", ast::Operator::NotEquals),
        ] {
            assert_eq!(
                parse_expr(sql).unwrap(),
                ast::Expr::binary(col("a"), op, col("b")),
                "{sql}"
            );
        }
    }

    #[test]
    fn bitwise_and_or_precedence() {
        use ast::Operator::{Add, BitwiseAnd, BitwiseOr, Equals};

        // a & b and a | b lower to the engine's bitwise operators.
        assert_eq!(
            parse_expr("a & b").unwrap(),
            ast::Expr::binary(col("a"), BitwiseAnd, col("b"))
        );
        assert_eq!(
            parse_expr("a | b").unwrap(),
            ast::Expr::binary(col("a"), BitwiseOr, col("b"))
        );

        // `&` binds tighter than `|`: a | b & c == a | (b & c).
        assert_eq!(
            parse_expr("a | b & c").unwrap(),
            ast::Expr::binary(
                col("a"),
                BitwiseOr,
                ast::Expr::binary(col("b"), BitwiseAnd, col("c")),
            )
        );

        // `+` binds tighter than `&`: a + b & c == (a + b) & c.
        assert_eq!(
            parse_expr("a + b & c").unwrap(),
            ast::Expr::binary(
                ast::Expr::binary(col("a"), Add, col("b")),
                BitwiseAnd,
                col("c"),
            )
        );

        // Bitwise binds tighter than comparison: a & b = c == (a & b) = c.
        assert_eq!(
            parse_expr("a & b = c").unwrap(),
            ast::Expr::binary(
                ast::Expr::binary(col("a"), BitwiseAnd, col("b")),
                Equals,
                col("c"),
            )
        );

        // `^` (XOR) has no engine operator, so it lowers to `(a & ~b) | (~a & b)`.
        let ast::Expr::Binary(left, BitwiseOr, right) = parse_expr("a ^ b").unwrap() else {
            panic!("expected `a ^ b` to lower to a BitwiseOr of two BitwiseAnd terms");
        };
        assert!(matches!(left.as_ref(), ast::Expr::Binary(_, BitwiseAnd, _)));
        assert!(matches!(right.as_ref(), ast::Expr::Binary(_, BitwiseAnd, _)));

        // `^` binds tighter than `*` (`a * b ^ c` is `a * (b ^ c)`).
        let ast::Expr::Binary(_, ast::Operator::Multiply, rhs) = parse_expr("a * b ^ c").unwrap()
        else {
            panic!("expected a multiplication at the top of `a * b ^ c`");
        };
        // The right operand is the XOR lowering (a BitwiseOr), not a bare column.
        assert!(matches!(rhs.as_ref(), ast::Expr::Binary(_, BitwiseOr, _)));
    }

    #[test]
    fn shift_operator_precedence() {
        use ast::Operator::{Add, BitwiseAnd, LeftShift, RightShift};

        assert_eq!(
            parse_expr("a << b").unwrap(),
            ast::Expr::binary(col("a"), LeftShift, col("b"))
        );
        assert_eq!(
            parse_expr("a >> b").unwrap(),
            ast::Expr::binary(col("a"), RightShift, col("b"))
        );

        // `+` binds tighter than `<<`: a + b << c == (a + b) << c.
        assert_eq!(
            parse_expr("a + b << c").unwrap(),
            ast::Expr::binary(
                ast::Expr::binary(col("a"), Add, col("b")),
                LeftShift,
                col("c"),
            )
        );

        // `<<` binds tighter than `&`: a << b & c == (a << b) & c.
        assert_eq!(
            parse_expr("a << b & c").unwrap(),
            ast::Expr::binary(
                ast::Expr::binary(col("a"), LeftShift, col("b")),
                BitwiseAnd,
                col("c"),
            )
        );

        // Left-associative: a >> b >> c == (a >> b) >> c.
        assert_eq!(
            parse_expr("a >> b >> c").unwrap(),
            ast::Expr::binary(
                ast::Expr::binary(col("a"), RightShift, col("b")),
                RightShift,
                col("c"),
            )
        );

        // The comparison operators are unaffected by the new `<<`/`>>` lexing.
        assert_eq!(
            parse_expr("a <> b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::NotEquals, col("b"))
        );
    }

    #[test]
    fn json_arrow_operators_parse() {
        // `j -> '$.a'` and `j ->> '$.a'` map to the engine's ArrowRight /
        // ArrowRightShift operators.
        assert_eq!(
            parse_expr("j -> '$.a'").unwrap(),
            ast::Expr::binary(
                col("j"),
                ast::Operator::ArrowRight,
                ast::Expr::Literal(ast::Literal::String("'$.a'".to_string())),
            )
        );
        let ast::Expr::Binary(_, ast::Operator::ArrowRightShift, _) =
            parse_expr("j ->> '$.a'").unwrap()
        else {
            panic!("expected `->>` to lower to ArrowRightShift");
        };

        // They bind tighter than `=`: `j ->> '$.a' = 'x'` is `(j ->> '$.a') = 'x'`.
        let ast::Expr::Binary(lhs, ast::Operator::Equals, _) =
            parse_expr("j ->> '$.a' = 'x'").unwrap()
        else {
            panic!("expected the top operator to be `=`");
        };
        assert!(matches!(
            lhs.as_ref(),
            ast::Expr::Binary(_, ast::Operator::ArrowRightShift, _)
        ));

        // They chain left-to-right.
        let ast::Expr::Binary(lhs, ast::Operator::ArrowRight, _) =
            parse_expr("j -> '$.a' -> '$.b'").unwrap()
        else {
            panic!("expected a chained `->`");
        };
        assert!(matches!(
            lhs.as_ref(),
            ast::Expr::Binary(_, ast::Operator::ArrowRight, _)
        ));
    }

    #[test]
    fn bitwise_not_is_a_tight_unary_prefix() {
        // `~a` is unary bitwise NOT.
        assert_eq!(
            parse_expr("~a").unwrap(),
            ast::Expr::unary(ast::UnaryOperator::BitwiseNot, col("a"))
        );

        // It binds tighter than `&`: `~a & b` == `(~a) & b`.
        assert_eq!(
            parse_expr("~a & b").unwrap(),
            ast::Expr::binary(
                ast::Expr::unary(ast::UnaryOperator::BitwiseNot, col("a")),
                ast::Operator::BitwiseAnd,
                col("b"),
            )
        );

        // And tighter than `*`: `~a * b` == `(~a) * b`.
        assert_eq!(
            parse_expr("~a * b").unwrap(),
            ast::Expr::binary(
                ast::Expr::unary(ast::UnaryOperator::BitwiseNot, col("a")),
                ast::Operator::Multiply,
                col("b"),
            )
        );
    }

    #[test]
    fn parenthesized_union_branches_parse() {
        // A leading parenthesized branch parses as a compound Select, equivalent
        // to the unparenthesized form.
        let ast::Stmt::Select(paren) = parse("(SELECT a FROM t) UNION (SELECT b FROM u)").unwrap()
        else {
            panic!("expected a compound Select");
        };
        assert_eq!(paren.body.compounds.len(), 1);
        assert_eq!(
            paren.body.compounds[0].operator,
            ast::CompoundOperator::Union
        );
        let ast::Stmt::Select(bare) = parse("SELECT a FROM t UNION SELECT b FROM u").unwrap()
        else {
            unreachable!()
        };
        assert_eq!(paren.body, bare.body);

        // A trailing ORDER BY applies to the whole result (one compound, ordered).
        let ast::Stmt::Select(ordered) =
            parse("(SELECT a FROM t) UNION (SELECT b FROM u) ORDER BY a").unwrap()
        else {
            unreachable!()
        };
        assert!(!ordered.order_by.is_empty());

        // An inner ORDER BY / LIMIT in a parenthesized branch is rejected.
        assert!(parse("(SELECT a FROM t ORDER BY a) UNION (SELECT b FROM u)").is_err());
        assert!(parse("(SELECT a FROM t LIMIT 1) UNION (SELECT b FROM u)").is_err());
    }

    #[test]
    fn with_clause_parses_into_select() {
        // A single non-recursive CTE attaches to the SELECT.
        let ast::Stmt::Select(select) =
            parse("WITH c AS (SELECT id FROM t) SELECT id FROM c").unwrap()
        else {
            panic!("expected WITH ... SELECT to parse as a Select");
        };
        let with = select.with.expect("the WITH clause should be attached");
        assert!(!with.recursive);
        assert_eq!(with.ctes.len(), 1);
        assert_eq!(with.ctes[0].tbl_name.as_str(), "c");
        assert!(with.ctes[0].columns.is_empty());

        // A column-rename list is recorded.
        let ast::Stmt::Select(select) =
            parse("WITH c(x, y) AS (SELECT a, b FROM t) SELECT x FROM c").unwrap()
        else {
            unreachable!()
        };
        let cols = &select.with.unwrap().ctes[0].columns;
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].col_name.as_str(), "x");
        assert_eq!(cols[1].col_name.as_str(), "y");

        // RECURSIVE and multiple CTEs.
        let ast::Stmt::Select(select) = parse(
            "WITH RECURSIVE a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a UNION SELECT * FROM b",
        )
        .unwrap() else {
            unreachable!()
        };
        let with = select.with.unwrap();
        assert!(with.recursive);
        assert_eq!(with.ctes.len(), 2);
    }

    #[test]
    fn find_in_set_lowers_to_comma_count() {
        // FIND_IN_SET(s, list) -> length(prefix) - length(replace(prefix, ',','')).
        let ast::Expr::Binary(lhs, ast::Operator::Subtract, rhs) =
            parse_expr("FIND_IN_SET(s, list)").unwrap()
        else {
            panic!("expected FIND_IN_SET to lower to a subtraction");
        };
        // Both sides are length(...) calls.
        for side in [lhs.as_ref(), rhs.as_ref()] {
            let ast::Expr::FunctionCall { name, .. } = side else {
                panic!("expected a length() call");
            };
            assert_eq!(name.as_str(), "length");
        }
        // The right side strips commas via replace(..., ',', '').
        let ast::Expr::FunctionCall { args, .. } = rhs.as_ref() else {
            unreachable!()
        };
        let ast::Expr::FunctionCall { name: rname, .. } = args[0].as_ref() else {
            panic!("expected replace() inside the right length()");
        };
        assert_eq!(rname.as_str(), "replace");
    }

    #[test]
    fn isnull_lowers_to_is_null_predicate() {
        // ISNULL(x) -> x IS NULL.
        assert_eq!(
            parse_expr("ISNULL(v)").unwrap(),
            ast::Expr::is_null(col("v"))
        );
    }

    #[test]
    fn trim_forms_lower_to_engine_trims() {
        // Direction selects trim/ltrim/rtrim; an optional remove-string becomes a
        // second argument (target first, remove-string second).
        let cases = [
            ("TRIM(s)", "trim", 1),
            ("TRIM(BOTH FROM s)", "trim", 1),
            ("TRIM(LEADING FROM s)", "ltrim", 1),
            ("TRIM(TRAILING FROM s)", "rtrim", 1),
            ("TRIM('x' FROM s)", "trim", 2),
            ("TRIM(LEADING 'x' FROM s)", "ltrim", 2),
            ("TRIM(TRAILING 'x' FROM s)", "rtrim", 2),
        ];
        for (sql, want_fn, want_args) in cases {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a function call");
            };
            assert_eq!(name.as_str(), want_fn, "{sql}");
            assert_eq!(args.len(), want_args, "{sql}");
            // The target (the trimmed string) is always the first argument.
            assert_eq!(*args[0], col("s"), "{sql}");
        }

        // A direction keyword without FROM is a syntax error.
        assert!(parse_expr("TRIM(LEADING 'x')").is_err());
    }

    #[test]
    fn char_lowers_to_engine_char() {
        // CHAR(72, 73) -> char(<int 72>, <int 73>), each code coerced to an
        // integer like CAST(code AS SIGNED) so a numeric/string code rounds/parses.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("CHAR(72, 73)").unwrap() else {
            panic!("expected CHAR to lower to a function call");
        };
        assert_eq!(name.as_str(), "char");
        assert_eq!(args.len(), 2);
        assert_eq!(*args[0], parse_expr("CAST(72 AS SIGNED)").unwrap());

        // A trailing `USING charset` clause is parsed and ignored; the call
        // still lowers to `char()` with the same code-point arguments.
        let ast::Expr::FunctionCall { name, args, .. } =
            parse_expr("CHAR(72, 105 USING utf8mb4)").unwrap()
        else {
            panic!("expected CHAR ... USING to lower to a function call");
        };
        assert_eq!(name.as_str(), "char");
        assert_eq!(args.len(), 2);
        assert_eq!(*args[1], parse_expr("CAST(105 AS SIGNED)").unwrap());
    }

    #[test]
    fn ascii_and_ord_lower_to_guarded_unicode() {
        // ASCII(s) / ORD(s) -> CASE WHEN s = '' THEN 0 ELSE unicode(s) END.
        for sql in ["ASCII(s)", "ORD(s)"] {
            let ast::Expr::Case {
                base,
                when_then_pairs,
                else_expr,
            } = parse_expr(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            assert!(base.is_none(), "{sql}");
            assert_eq!(when_then_pairs.len(), 1, "{sql}");
            // The guard is `s = ''`, returning the integer 0.
            assert_eq!(*when_then_pairs[0].1, num("0"), "{sql}");
            // The ELSE is unicode(s).
            let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone()
            else {
                panic!("expected `{sql}` ELSE to be unicode()");
            };
            assert_eq!(name.as_str(), "unicode", "{sql}");
            assert_eq!(args.len(), 1, "{sql}");
            assert_eq!(*args[0], col("s"), "{sql}");
        }
    }

    #[test]
    fn concat_lowers_to_concat_operator_chain() {
        // CONCAT(a, b, c) becomes ((a || b) || c).
        let expected = ast::Expr::binary(
            ast::Expr::binary(col("a"), ast::Operator::Concat, col("b")),
            ast::Operator::Concat,
            col("c"),
        );
        assert_eq!(parse_expr("CONCAT(a, b, c)").unwrap(), expected);
        // A single argument is returned unwrapped.
        assert_eq!(parse_expr("CONCAT(a)").unwrap(), col("a"));
    }

    #[test]
    fn convert_using_drops_charset_and_type_form_is_a_cast() {
        // CONVERT(expr USING charset) drops the charset and yields the bare expr.
        assert_eq!(parse_expr("CONVERT(a USING utf8mb4)").unwrap(), col("a"));
        // CONVERT(expr, type) is the same as CAST(expr AS type): a non-integer
        // target is a plain Cast, ...
        let ast::Expr::Cast { type_name, .. } = parse_expr("CONVERT(a, CHAR)").unwrap() else {
            panic!("expected CONVERT(expr, type) to parse as a Cast");
        };
        assert_eq!(type_name.unwrap().name, "CHAR");
        // ... and an integer target gets the same rounding CASE as `CAST AS SIGNED`.
        assert_eq!(
            parse_expr("CONVERT(a, SIGNED)").unwrap(),
            parse_expr("CAST(a AS SIGNED)").unwrap()
        );
        assert!(matches!(
            parse_expr("CONVERT(a, SIGNED)").unwrap(),
            ast::Expr::Case { .. }
        ));
    }

    #[test]
    fn div_and_mod_operators_lower_to_integer_arithmetic() {
        // a DIV b -> CAST(a / b AS INTEGER).
        let ast::Expr::Cast { expr, type_name } = parse_expr("a DIV b").unwrap() else {
            panic!("expected DIV to lower to a CAST");
        };
        assert_eq!(type_name.unwrap().name, "INTEGER");
        assert!(matches!(
            expr.as_ref(),
            ast::Expr::Binary(_, ast::Operator::Divide, _)
        ));

        // a MOD b -> a - b * CAST(a / b AS INTEGER).
        let ast::Expr::Binary(lhs, ast::Operator::Subtract, rhs) = parse_expr("a MOD b").unwrap()
        else {
            panic!("expected MOD to lower to a subtraction");
        };
        assert_eq!(*lhs, col("a"));
        let ast::Expr::Binary(_, ast::Operator::Multiply, q) = rhs.as_ref() else {
            panic!("expected `b * quotient`");
        };
        assert!(matches!(q.as_ref(), ast::Expr::Cast { .. }));

        // Left-associative and same precedence as `*`.
        assert!(matches!(
            parse_expr("17 DIV 5 MOD 2").unwrap(),
            ast::Expr::Binary(_, ast::Operator::Subtract, _)
        ));

        // The MOD(a, b) function form lowers identically to the `a MOD b`
        // operator (MySQL defines them the same).
        assert_eq!(
            parse_expr("MOD(a, b)").unwrap(),
            parse_expr("a MOD b").unwrap()
        );

        // The `%` operator is a synonym for `MOD` and lowers identically, at the
        // same (multiplicative) precedence — so `2 + a % b` parses the same as
        // `2 + a MOD b` (the modulo binding tighter than the addition).
        assert_eq!(parse_expr("a % b").unwrap(), parse_expr("a MOD b").unwrap());
        assert_eq!(
            parse_expr("2 + a % b").unwrap(),
            parse_expr("2 + a MOD b").unwrap()
        );

        // `a / b` is MySQL float division: `CAST(a AS REAL) / b`.
        let ast::Expr::Binary(lhs, ast::Operator::Divide, rhs) = parse_expr("a / b").unwrap()
        else {
            panic!("expected `/` to lower to a division");
        };
        let ast::Expr::Cast { type_name, .. } = lhs.as_ref() else {
            panic!("expected the dividend cast to REAL");
        };
        assert_eq!(type_name.as_ref().unwrap().name, "REAL");
        assert_eq!(*rhs, col("b"));
    }

    #[test]
    fn repeat_lowers_to_zeroblob_replace_with_null_guard() {
        // REPEAT(s, n) -> CASE WHEN <int n> IS NULL THEN NULL
        //                      ELSE replace(hex(zeroblob(<int n>)), '00', s) END,
        // where <int n> is the count coerced to an integer like CAST(n AS SIGNED)
        // (MySQL rounds a fractional count).
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("REPEAT('ab', n)").unwrap()
        else {
            panic!("expected REPEAT to lower to a CASE");
        };
        assert!(base.is_none(), "searched CASE, no base expression");

        // The single WHEN guards a NULL (coerced) count -> NULL.
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(
            *when_then_pairs[0].0,
            ast::Expr::is_null(parse_expr("CAST(n AS SIGNED)").unwrap())
        );
        assert_eq!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Null)
        );

        // The ELSE branch is replace(hex(zeroblob(n)), '00', s).
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE branch to be a function call");
        };
        assert_eq!(name.as_str(), "replace");
        assert_eq!(args.len(), 3);
        assert_eq!(
            *args[1],
            ast::Expr::Literal(ast::Literal::String("'00'".to_string()))
        );

        // SPACE(n) is REPEAT(' ', n) and lowers identically.
        assert_eq!(
            parse_expr("SPACE(n)").unwrap(),
            parse_expr("REPEAT(' ', n)").unwrap()
        );
    }

    #[test]
    fn timediff_lowers_to_printf_with_null_guard() {
        // TIMEDIFF(a, b) -> CASE WHEN <secs> IS NULL THEN NULL
        //                        ELSE printf('%s%02d:%02d:%02d', sign, hh, mm, ss) END,
        // where <secs> = CAST(ROUND((julianday(a) - julianday(b)) * 86400) AS INTEGER).
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("TIMEDIFF(a, b)").unwrap()
        else {
            panic!("expected TIMEDIFF to lower to a CASE");
        };
        assert!(base.is_none(), "searched CASE, no base expression");

        // The single WHEN guards a NULL/unparseable difference -> NULL. The
        // guarded expression is the integer-seconds cast.
        assert_eq!(when_then_pairs.len(), 1);
        let ast::Expr::IsNull(guarded) = &*when_then_pairs[0].0 else {
            panic!("the WHEN guard is an IS NULL check");
        };
        assert!(
            matches!(
                guarded.as_ref(),
                ast::Expr::Cast {
                    type_name: Some(ast::Type { name, .. }),
                    ..
                } if name == "INTEGER"
            ),
            "the guarded value is the CAST(... AS INTEGER) second count"
        );
        assert_eq!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Null)
        );

        // The ELSE branch is printf('%s%02d:%02d:%02d', sign, hh, mm, ss).
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone() else {
            panic!("expected the ELSE branch to be a printf call");
        };
        assert_eq!(name.as_str(), "printf");
        assert_eq!(args.len(), 5);
        assert_eq!(
            *args[0],
            ast::Expr::Literal(ast::Literal::String("'%s%02d:%02d:%02d'".to_string()))
        );
    }

    #[test]
    fn quote_lowers_to_escaping_replaces_with_null_word_guard() {
        // QUOTE(s) -> CASE WHEN s IS NULL THEN 'NULL'
        //                  ELSE '\'' || replace(replace(replace(s, ...)...)) || '\'' END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("QUOTE(s)").unwrap()
        else {
            panic!("expected QUOTE to lower to a CASE");
        };
        assert!(base.is_none(), "searched CASE, no base expression");

        // The single WHEN guards NULL, returning the literal word `NULL`.
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(*when_then_pairs[0].0, ast::Expr::is_null(col("s")));
        assert_eq!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::String("'NULL'".to_string()))
        );

        // The ELSE wraps the escaped value in single quotes: `'` || <esc> || `'`.
        // A single-quote literal is stored requoted as four quote characters.
        let wrapped = else_expr.unwrap();
        let ast::Expr::Binary(left, ast::Operator::Concat, right) = wrapped.as_ref() else {
            panic!("expected the ELSE to concatenate a trailing quote");
        };
        assert_eq!(
            **right,
            ast::Expr::Literal(ast::Literal::String("''''".to_string()))
        );
        // The left side is `'` || replace(...), the outermost escape being Ctrl-Z.
        let ast::Expr::Binary(_, ast::Operator::Concat, esc) = left.as_ref() else {
            panic!("expected `'` || <escaped> on the left");
        };
        let ast::Expr::FunctionCall { name, args, .. } = esc.as_ref() else {
            panic!("expected the escaped value to be a replace() call");
        };
        assert_eq!(name.as_str(), "replace");
        // Ctrl-Z (char(26)) is the outermost replacement; its target is `\Z`.
        assert_eq!(
            *args[2],
            ast::Expr::Literal(ast::Literal::String("'\\Z'".to_string()))
        );
    }

    #[test]
    fn uuid_to_bin_and_bin_to_uuid_lower_to_hex_surgery() {
        // UUID_TO_BIN(u) -> unhex(replace(u, '-', '')).
        let ast::Expr::FunctionCall { name, args, .. } =
            parse_expr("UUID_TO_BIN(u)").unwrap()
        else {
            panic!("expected UUID_TO_BIN to lower to a function call");
        };
        assert_eq!(name.as_str(), "unhex");
        let ast::Expr::FunctionCall { name: inner, .. } = args[0].as_ref() else {
            panic!("expected unhex(replace(...))");
        };
        assert_eq!(inner.as_str(), "replace");

        // The swap form reorders the hex groups, so the unhex argument is a
        // concatenation rather than the bare replace().
        let ast::Expr::FunctionCall { args: swap_args, .. } =
            parse_expr("UUID_TO_BIN(u, 1)").unwrap()
        else {
            panic!("expected a function call");
        };
        assert!(matches!(
            swap_args[0].as_ref(),
            ast::Expr::Binary(_, ast::Operator::Concat, _)
        ));

        // BIN_TO_UUID(b) -> CASE WHEN b IS NULL THEN NULL ELSE lower(...) END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("BIN_TO_UUID(b)").unwrap()
        else {
            panic!("expected BIN_TO_UUID to lower to a guarded CASE");
        };
        assert_eq!(*when_then_pairs[0].0, ast::Expr::is_null(col("b")));
        let else_branch = else_expr.unwrap();
        let ast::Expr::FunctionCall { name, .. } = else_branch.as_ref() else {
            panic!("expected the ELSE to be lower(...)");
        };
        assert_eq!(name.as_str(), "lower");

        // The swap flag must be an integer literal.
        assert!(parse_expr("UUID_TO_BIN(u, n)").is_err());
    }

    #[test]
    fn convert_tz_lowers_to_guarded_datetime_shift() {
        // CONVERT_TZ(dt, f, t) -> CASE WHEN <both numeric offsets> THEN
        //                              datetime(dt, printf('%+d minutes', ...))
        //                         ELSE NULL END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("CONVERT_TZ(dt, f, t)").unwrap()
        else {
            panic!("expected CONVERT_TZ to lower to a guarded CASE");
        };
        // The guard returns NULL for a non-numeric (or NULL) offset.
        assert_eq!(
            **else_expr.as_ref().unwrap(),
            ast::Expr::Literal(ast::Literal::Null)
        );
        // The THEN branch is a datetime() shift.
        let ast::Expr::FunctionCall { name, args, .. } = when_then_pairs[0].1.as_ref() else {
            panic!("expected the THEN to be a datetime() call");
        };
        assert_eq!(name.as_str(), "datetime");
        // datetime(dt, printf('%+d minutes', diff)).
        let ast::Expr::FunctionCall { name: inner, .. } = args[1].as_ref() else {
            panic!("expected a printf() modifier");
        };
        assert_eq!(inner.as_str(), "printf");
    }

    #[test]
    fn substring_index_lowers_to_guarded_substr() {
        // SUBSTRING_INDEX(s, '.', 1) -> CASE WHEN instr(s,'.')=0 THEN s
        //                                    ELSE substr(s, 1, instr(s,'.')-1) END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("SUBSTRING_INDEX(s, '.', 1)").unwrap()
        else {
            panic!("expected SUBSTRING_INDEX to lower to a CASE");
        };
        assert!(base.is_none());
        // The guard returns the whole string when there is no delimiter.
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(*when_then_pairs[0].1, col("s"));
        // The ELSE is substr(s, 1, ...).
        let else_branch = else_expr.unwrap();
        let ast::Expr::FunctionCall { name, .. } = else_branch.as_ref() else {
            panic!("expected the ELSE to be a substr() call");
        };
        assert_eq!(name.as_str(), "substr");

        // The `-1` form reverses, takes the prefix, and reverses back.
        let ast::Expr::FunctionCall { name, .. } =
            parse_expr("SUBSTRING_INDEX(s, '.', -1)").unwrap()
        else {
            panic!("expected the -1 form to lower to a string_reverse() call");
        };
        assert_eq!(name.as_str(), "string_reverse");

        // count = 0 is the empty string; a literal is required and |count| <= 1.
        assert_eq!(
            parse_expr("SUBSTRING_INDEX(s, '.', 0)").unwrap(),
            ast::Expr::Literal(ast::Literal::String("''".to_string()))
        );
        assert!(parse_expr("SUBSTRING_INDEX(s, '.', 2)").is_err());
        assert!(parse_expr("SUBSTRING_INDEX(s, '.', -3)").is_err());
        assert!(parse_expr("SUBSTRING_INDEX(s, '.', n)").is_err());
    }

    #[test]
    fn insert_function_lowers_to_guarded_splice() {
        // INSERT(s, pos, len, new) -> a guarded CASE; the ELSE splices via
        // substr/concat.
        let ast::Expr::Case {
            base, else_expr, ..
        } = parse_expr("INSERT(s, 3, 4, 'X')").unwrap()
        else {
            panic!("expected INSERT() to lower to a CASE");
        };
        assert!(base.is_none());
        // The ELSE is a concatenation (prefix || new || suffix).
        assert!(matches!(
            else_expr.unwrap().as_ref(),
            ast::Expr::Binary(_, ast::Operator::Concat, _)
        ));

        // Exactly four arguments are required.
        assert!(parse_expr("INSERT(s, 3, 4)").is_err());
    }

    #[test]
    fn pad_lowers_to_substr_of_repeat() {
        // Both LPAD and RPAD lower to a guard CASE whose ELSE is the outer
        // substr(..., 1, len) over a concatenation involving REPEAT(pad, len).
        for sql in ["LPAD(s, n, p)", "RPAD(s, n, p)"] {
            let ast::Expr::Case {
                base,
                when_then_pairs,
                else_expr,
            } = parse_expr(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to a guard CASE");
            };
            assert!(base.is_none(), "{sql}");
            // Two guards: negative len -> NULL, unpaddable empty pad -> ''.
            assert_eq!(when_then_pairs.len(), 2, "{sql}");
            assert_eq!(*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Null), "{sql}");
            let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone()
            else {
                panic!("expected `{sql}` ELSE to be a function call");
            };
            assert_eq!(name.as_str(), "substr", "{sql}");
            assert_eq!(args.len(), 3, "{sql}");
            // substr starts at 1 and runs for `len`.
            assert_eq!(*args[1], num("1"), "{sql}");
            assert_eq!(*args[2], col("n"), "{sql}");
        }

        // LPAD prepends, RPAD appends, so the two differ.
        assert_ne!(
            parse_expr("LPAD(s, n, p)").unwrap(),
            parse_expr("RPAD(s, n, p)").unwrap()
        );

        // Both require exactly three arguments.
        assert!(parse_expr("LPAD(s, n)").is_err());
        assert!(parse_expr("RPAD(s, n)").is_err());
    }

    #[test]
    fn collate_clause_maps_to_engine_collation() {
        let collated = |name: &str, collation: &str| {
            ast::Expr::collate(col(name), ast::Name::from_string(collation))
        };
        // `expr COLLATE name` maps to the engine collation comparing the same way.
        assert_eq!(
            parse_expr("a COLLATE utf8mb4_bin").unwrap(),
            collated("a", "BINARY")
        );
        // COLLATE binds tighter than arithmetic: `a + b COLLATE x` is
        // `a + (b COLLATE x)`.
        assert_eq!(
            parse_expr("a + b COLLATE utf8mb4_general_ci").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Add, collated("b", "NOCASE"))
        );
    }

    #[test]
    fn binary_and_collate_map_to_engine_collations() {
        let collated = |name: &str, collation: &str| {
            ast::Expr::collate(col(name), ast::Name::from_string(collation))
        };
        // `BINARY expr` forces a case-sensitive comparison via `COLLATE BINARY`
        // (character columns are NOCASE by default).
        assert_eq!(parse_expr("BINARY a").unwrap(), collated("a", "BINARY"));
        assert_eq!(
            parse_expr("BINARY a = b").unwrap(),
            ast::Expr::binary(collated("a", "BINARY"), ast::Operator::Equals, col("b"))
        );
        assert_eq!(
            parse_expr("a = BINARY b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Equals, collated("b", "BINARY"))
        );
        // A `COLLATE` postfix maps onto the engine collation that compares the
        // same way: `_bin`/`_cs` -> BINARY, any `_ci` (or other) -> NOCASE.
        assert_eq!(
            parse_expr("a COLLATE utf8mb4_bin").unwrap(),
            collated("a", "BINARY")
        );
        assert_eq!(
            parse_expr("a COLLATE utf8mb4_general_ci").unwrap(),
            collated("a", "NOCASE")
        );
    }

    #[test]
    fn expr_not_and_parentheses() {
        let expr = parse_expr("NOT (a = 1 OR b = 2)").unwrap();
        let inner = ast::Expr::binary(
            ast::Expr::binary(col("a"), ast::Operator::Equals, num("1")),
            ast::Operator::Or,
            ast::Expr::binary(col("b"), ast::Operator::Equals, num("2")),
        );
        let expected = ast::Expr::unary(
            ast::UnaryOperator::Not,
            ast::Expr::Parenthesized(vec![Box::new(inner)]),
        );
        assert_eq!(expr, expected);
        // Parentheses survive the round trip back to SQL, preserving grouping.
        let sql = expr.to_string();
        assert!(sql.contains('('), "{sql}");
    }

    #[test]
    fn positional_placeholders_are_numbered_by_appearance() {
        // Each `?` becomes an unnamed Variable whose index is its 1-based
        // position in the statement.
        let stmt = parse("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        let indices = collect_param_indices(&stmt.to_string());
        // The renderer keeps placeholders as `?N`, so the round-tripped SQL
        // carries the assigned positions.
        assert_eq!(indices, vec![1, 2], "{stmt}");
    }

    #[test]
    fn placeholder_in_insert_values() {
        let stmt = parse("INSERT INTO t (a, b, c) VALUES (?, ?, ?)").unwrap();
        assert_eq!(collect_param_indices(&stmt.to_string()), vec![1, 2, 3]);
    }

    /// Extracts the indices from rendered `?N` placeholders, in order.
    fn collect_param_indices(sql: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'?' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                out.push(sql[start..end].parse().expect("placeholder has an index"));
                i = end;
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn expr_unsupported_forms_are_not_fully_parsed() {
        // `||` (MySQL logical OR, not string concatenation) is intentionally not
        // parsed. (`/` and `%` are supported — they lower to float division and
        // the `MOD` remainder.)
        let mut p = Parser::new(b"a || b").unwrap();
        let fully_parsed = p.expr().is_ok() && p.peek().is_none();
        assert!(!fully_parsed, "expected `a || b` to be rejected");
    }

    #[test]
    fn function_call_allowed() {
        let expr = parse_expr("COALESCE(a, 1)").unwrap();
        let ast::Expr::FunctionCall { name, args, .. } = expr else {
            panic!("expected FunctionCall");
        };
        assert_eq!(name.as_str().to_ascii_uppercase(), "COALESCE");
        assert_eq!(args.len(), 2);

        // Case-insensitive, nested, and zero-arg-ish forms parse.
        assert!(matches!(
            parse_expr("abs(-3)").unwrap(),
            ast::Expr::FunctionCall { .. }
        ));
        assert!(matches!(
            parse_expr("IFNULL(a, b * 2)").unwrap(),
            ast::Expr::FunctionCall { .. }
        ));

        // String functions sharing name and behaviour with the engine.
        // (SUBSTR/SUBSTRING/MID lower to a guarded substr, tested separately.)
        for input in [
            "REPLACE(s, '-', '_')",
            "TRIM(s)",
            "LTRIM(s)",
            "RTRIM(s)",
            "CONCAT_WS('-', a, b)",
            "UNHEX('41')",
        ] {
            let ast::Expr::FunctionCall { name, .. } = parse_expr(input).unwrap() else {
                panic!("expected `{input}` to parse as a function call");
            };
            // These keep their name (no lowering / renaming).
            assert!(
                input
                    .to_ascii_uppercase()
                    .starts_with(&name.as_str().to_ascii_uppercase()),
                "`{input}` should keep its name, got `{}`",
                name.as_str()
            );
        }
    }

    #[test]
    fn math_functions_parse_and_rename_synonyms() {
        // Functions that keep their name (the engine resolves them
        // case-insensitively).
        for input in ["ROUND(x, 2)", "POW(x, 2)", "SQRT(x)", "EXP(x)", "LN(x)"] {
            let ast::Expr::FunctionCall { name, .. } = parse_expr(input).unwrap() else {
                panic!("expected `{input}` to parse as a function call");
            };
            assert!(
                input
                    .to_ascii_uppercase()
                    .starts_with(&name.as_str().to_ascii_uppercase()),
                "`{input}` should keep its name, got `{}`",
                name.as_str()
            );
        }

        // `POWER` is a MySQL synonym renamed to the engine's `pow`.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("POWER(x, 3)").unwrap() else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "pow");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn ceil_floor_round_return_integer() {
        // CEIL/CEILING/FLOOR and 1-arg ROUND wrap the engine call in CAST(...
        // AS INTEGER) so they type as an integer like MySQL.
        for (input, engine) in [
            ("CEIL(x)", "ceil"),
            ("CEILING(x)", "ceil"),
            ("FLOOR(x)", "floor"),
            ("ROUND(x)", "round"),
        ] {
            let ast::Expr::Cast { expr, type_name } = parse_expr(input).unwrap() else {
                panic!("expected `{input}` to lower to a CAST");
            };
            assert_eq!(type_name.unwrap().name, "INTEGER");
            let ast::Expr::FunctionCall { name, .. } = expr.as_ref() else {
                panic!("expected a function call inside the cast for `{input}`");
            };
            assert_eq!(name.as_str(), engine, "for `{input}`");
        }
        // ROUND(x, 2) keeps decimals and stays a real (a bare call); ROUND(x, 0)
        // is an integer, so it casts.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("ROUND(x, 2)").unwrap() else {
            panic!("expected ROUND(x, 2) to stay a function call");
        };
        assert_eq!(name.as_str(), "round");
        assert_eq!(args.len(), 2);
        assert!(matches!(
            parse_expr("ROUND(x, 0)").unwrap(),
            ast::Expr::Cast { .. }
        ));
    }

    #[test]
    fn log_one_arg_is_natural_log_two_arg_is_base_log() {
        // LOG(x) -> ln(x) (MySQL's one-arg LOG is the natural log; the engine's
        // own one-arg log is base-10).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("LOG(x)").unwrap() else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "ln");
        assert_eq!(args.len(), 1);

        // LOG(b, x) -> log(b, x) (base-b logarithm, identical on both).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("LOG(2, 8)").unwrap() else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "log");
        assert_eq!(args.len(), 2);

        // LOG2 / LOG10 / PI keep their name (engine resolves them).
        for input in ["LOG2(x)", "LOG10(x)", "PI()"] {
            assert!(
                matches!(parse_expr(input).unwrap(), ast::Expr::FunctionCall { .. }),
                "expected `{input}` to parse as a function call"
            );
        }
    }

    #[test]
    fn trig_functions_parse_and_atan_handles_two_args() {
        // The trig functions keep their name (the engine resolves them).
        for input in [
            "SIN(x)",
            "COS(x)",
            "TAN(x)",
            "ASIN(x)",
            "ACOS(x)",
            "ATAN2(y, x)",
            "DEGREES(x)",
            "RADIANS(x)",
        ] {
            assert!(
                matches!(parse_expr(input).unwrap(), ast::Expr::FunctionCall { .. }),
                "expected `{input}` to parse as a function call"
            );
        }

        // ATAN(x) -> atan(x); ATAN(y, x) -> atan2(y, x) (MySQL's two-arg synonym).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("ATAN(x)").unwrap() else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "atan");
        assert_eq!(args.len(), 1);
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("ATAN(y, x)").unwrap() else {
            panic!("expected a function call");
        };
        assert_eq!(name.as_str(), "atan2");
        assert_eq!(args.len(), 2);

        // COT(x) has no engine builtin, so it lowers to 1 / tan(x).
        let ast::Expr::Binary(one, ast::Operator::Divide, tan) = parse_expr("COT(x)").unwrap()
        else {
            panic!("expected COT to lower to a division");
        };
        assert_eq!(*one, num("1"));
        let ast::Expr::FunctionCall { name, args, .. } = tan.as_ref() else {
            panic!("expected the divisor to be tan(x)");
        };
        assert_eq!(name.as_str(), "tan");
        assert_eq!(args.len(), 1);
        assert_eq!(*args[0], col("x"));
    }

    #[test]
    fn hex_dispatches_on_runtime_type() {
        // HEX(x) -> CASE WHEN x IS NULL THEN NULL
        //                WHEN typeof(x) IN (integer, real) THEN printf('%X', x)
        //                ELSE hex(x) END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("HEX(x)").unwrap()
        else {
            panic!("expected HEX to lower to a CASE");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 2);
        // The NULL guard comes first.
        assert!(matches!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Null)
        ));
        // The numeric branch is printf('%X', x); the else is hex(x).
        let ast::Expr::FunctionCall { name, .. } = when_then_pairs[1].1.as_ref() else {
            panic!("expected printf for the numeric branch");
        };
        assert_eq!(name.as_str(), "printf");
        let else_expr = else_expr.unwrap();
        let ast::Expr::FunctionCall { name, .. } = else_expr.as_ref() else {
            panic!("expected hex for the else branch");
        };
        assert_eq!(name.as_str(), "hex");
    }

    #[test]
    fn oct_lowers_to_printf_with_null_guard() {
        // OCT(n) -> CASE WHEN n IS NULL THEN NULL ELSE printf('%o', n) END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("OCT(n)").unwrap()
        else {
            panic!("expected OCT to lower to a CASE");
        };
        assert_eq!(when_then_pairs.len(), 1);
        assert!(matches!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Null)
        ));
        let else_expr = else_expr.unwrap();
        let ast::Expr::FunctionCall { name, args, .. } = else_expr.as_ref() else {
            panic!("expected printf in the else branch");
        };
        assert_eq!(name.as_str(), "printf");
        assert!(matches!(
            args[0].as_ref(),
            ast::Expr::Literal(ast::Literal::String(s)) if s == "'%o'"
        ));
    }

    #[test]
    fn interval_lowers_to_sum_of_comparisons() {
        // INTERVAL(n, n1, n2) -> CASE WHEN n IS NULL THEN -1
        //                        ELSE (n >= n1) + (n >= n2) END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("INTERVAL(n, 1, 3, 7)").unwrap()
        else {
            panic!("expected INTERVAL to lower to a CASE");
        };
        assert_eq!(when_then_pairs.len(), 1);
        // The NULL guard returns -1.
        assert!(matches!(
            &*when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Numeric(s)) if s == "-1"
        ));
        // The else branch sums three comparisons: ((c1 + c2) + c3).
        let ast::Expr::Binary(_, ast::Operator::Add, _) = else_expr.unwrap().as_ref() else {
            panic!("expected a sum of comparisons in the else branch");
        };

        // A single bound is allowed; a missing bound is rejected.
        assert!(parse_expr("INTERVAL(5, 3)").is_ok());
        assert!(parse_expr("INTERVAL(5)").is_err());
    }

    #[test]
    fn json_valid_lowers_to_engine_json_valid() {
        // JSON_VALID(x) -> json_valid(x): same single argument, renamed.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("JSON_VALID(doc)").unwrap()
        else {
            panic!("expected JSON_VALID to lower to a function call");
        };
        assert_eq!(name.as_str(), "json_valid");
        assert_eq!(args.len(), 1);

        // The non-matching JSON builders stay unsupported (their output spacing
        // diverges from MySQL).
        assert!(parse_expr("JSON_OBJECT('k', 1)").is_err());
        assert!(parse_expr("JSON_ARRAY(1, 2)").is_err());
    }

    #[test]
    fn strcmp_lowers_to_comparison_case() {
        // STRCMP(a, b) -> CASE WHEN a IS NULL OR b IS NULL THEN NULL
        //                      WHEN a < b THEN -1 WHEN a > b THEN 1 ELSE 0 END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("STRCMP(a, b)").unwrap()
        else {
            panic!("expected STRCMP to lower to a CASE");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 3);
        // First branch handles NULL -> NULL.
        assert!(matches!(
            *when_then_pairs[0].1,
            ast::Expr::Literal(ast::Literal::Null)
        ));
        // The `<` and `>` branches yield -1 and 1; the else is 0.
        assert_eq!(*when_then_pairs[1].1, num("-1"));
        assert_eq!(*when_then_pairs[2].1, num("1"));
        assert_eq!(*else_expr.unwrap(), num("0"));
        // The `<` comparison is taken with a `COLLATE NOCASE` left operand, so
        // STRCMP folds ASCII case like MySQL's default collation.
        let ast::Expr::Binary(lhs, ast::Operator::Less, _) = &*when_then_pairs[1].0 else {
            panic!("expected the second branch to be a `<` comparison");
        };
        assert!(
            matches!(lhs.as_ref(), ast::Expr::Collate(_, name) if name.as_str() == "NOCASE"),
            "expected the left operand to carry COLLATE NOCASE"
        );
    }

    #[test]
    fn hash_functions_lower_to_crypto_hex_digest() {
        // MD5/SHA1/SHA/SHA2 -> CASE WHEN s IS NULL THEN NULL ELSE
        // lower(hex(<crypto fn>(CAST(s AS TEXT)))) END.
        let crypto_fn = |sql: &str| -> String {
            let ast::Expr::Case { else_expr, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone()
            else {
                panic!("expected the else branch to be lower(...) for `{sql}`");
            };
            assert_eq!(name.as_str(), "lower");
            let ast::Expr::FunctionCall { name, args, .. } = args[0].as_ref() else {
                panic!("expected hex(...) inside lower for `{sql}`");
            };
            assert_eq!(name.as_str(), "hex");
            let ast::Expr::FunctionCall { name, args, .. } = args[0].as_ref() else {
                panic!("expected the crypto call inside hex for `{sql}`");
            };
            // Its argument is a CAST(... AS TEXT) so numeric inputs hash as text.
            assert!(matches!(
                args[0].as_ref(),
                ast::Expr::Cast { type_name: Some(t), .. } if t.name == "TEXT"
            ));
            name.as_str().to_string()
        };
        assert_eq!(crypto_fn("MD5(s)"), "crypto_md5");
        assert_eq!(crypto_fn("SHA1(s)"), "crypto_sha1");
        assert_eq!(crypto_fn("SHA(s)"), "crypto_sha1");
        assert_eq!(crypto_fn("SHA2(s, 256)"), "crypto_sha256");
        assert_eq!(crypto_fn("SHA2(s, 0)"), "crypto_sha256");
        assert_eq!(crypto_fn("SHA2(s, 384)"), "crypto_sha384");
        assert_eq!(crypto_fn("SHA2(s, 512)"), "crypto_sha512");
        // An unsupported SHA2 length (e.g. 224) and a non-literal length are
        // rejected.
        assert!(parse_expr("SHA2(s, 224)").is_err());
        assert!(parse_expr("SHA2(s, n)").is_err());
    }

    #[test]
    fn information_schema_tables_rewrites_to_derived_table() {
        // A reference to information_schema.TABLES becomes a derived (sub-)SELECT
        // over the catalog; a normal table stays a plain table reference.
        let base_table = |sql: &str| {
            let ast::Stmt::Select(select) = parse(sql).unwrap() else {
                panic!("expected a SELECT for `{sql}`");
            };
            let ast::OneSelect::Select {
                from: Some(from), ..
            } = select.body.select
            else {
                panic!("expected a FROM clause for `{sql}`");
            };
            *from.select
        };
        assert!(matches!(
            base_table("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_NAME = 'x'"),
            ast::SelectTable::Select(..)
        ));
        // Case-insensitive on both parts.
        assert!(matches!(
            base_table("SELECT 1 FROM INFORMATION_SCHEMA.tables"),
            ast::SelectTable::Select(..)
        ));
        // information_schema.COLUMNS rewrites the same way (and is case-insensitive).
        assert!(matches!(
            base_table(
                "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'x'"
            ),
            ast::SelectTable::Select(..)
        ));
        assert!(matches!(
            base_table("SELECT 1 FROM INFORMATION_SCHEMA.columns c"),
            ast::SelectTable::Select(..)
        ));
        // information_schema.STATISTICS and TABLE_CONSTRAINTS rewrite too.
        assert!(matches!(
            base_table(
                "SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME = 'x'"
            ),
            ast::SelectTable::Select(..)
        ));
        assert!(matches!(
            base_table(
                "SELECT CONSTRAINT_NAME FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_NAME = 'x'"
            ),
            ast::SelectTable::Select(..)
        ));
        assert!(matches!(
            base_table(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME = 'x'"
            ),
            ast::SelectTable::Select(..)
        ));
        // A plain table, and an unemulated information_schema table, are unchanged.
        assert!(matches!(
            base_table("SELECT id FROM wp_posts"),
            ast::SelectTable::Table(..)
        ));
        assert!(matches!(
            base_table("SELECT 1 FROM information_schema.REFERENTIAL_CONSTRAINTS"),
            ast::SelectTable::Table(..)
        ));
    }

    #[test]
    fn any_value_is_dropped() {
        // ANY_VALUE(x) lowers to just x (the engine allows the bare column).
        assert_eq!(parse_expr("ANY_VALUE(n)").unwrap(), col("n"));
        assert_eq!(parse_expr("ANY_VALUE(5)").unwrap(), num("5"));
        assert_eq!(
            parse_expr("ANY_VALUE(a + 1)").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Add, num("1"))
        );
        // It takes exactly one argument.
        assert!(parse_expr("ANY_VALUE()").is_err());
        assert!(parse_expr("ANY_VALUE(a, b)").is_err());
    }

    #[test]
    fn base64_functions_lower_to_crypto_encode_decode() {
        // TO_BASE64(s) -> CASE WHEN s IS NULL THEN NULL ELSE
        // crypto_encode(CAST(s AS TEXT), 'base64') END; FROM_BASE64 uses
        // crypto_decode on the argument directly.
        let crypto = |sql: &str| -> (String, ast::Expr) {
            let ast::Expr::Case { else_expr, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a CASE");
            };
            let ast::Expr::FunctionCall { name, args, .. } = else_expr.unwrap().as_ref().clone()
            else {
                panic!("expected the else branch to be a crypto call for `{sql}`");
            };
            // The format argument is the 'base64' literal.
            assert!(matches!(
                args[1].as_ref(),
                ast::Expr::Literal(ast::Literal::String(s)) if s == "'base64'"
            ));
            (name.as_str().to_string(), args[0].as_ref().clone())
        };
        let (encode, payload) = crypto("TO_BASE64(s)");
        assert_eq!(encode, "crypto_encode");
        // The encode payload is CAST(s AS TEXT).
        assert!(matches!(
            payload,
            ast::Expr::Cast { type_name: Some(t), .. } if t.name == "TEXT"
        ));
        let (decode, payload) = crypto("FROM_BASE64(s)");
        assert_eq!(decode, "crypto_decode");
        // The decode payload is the argument as-is.
        assert_eq!(payload, col("s"));
    }

    #[test]
    fn uuid_lowers_to_engine_uuid4() {
        // UUID() -> uuid4_str() (a no-argument generator).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("UUID()").unwrap() else {
            panic!("expected UUID() to lower to a function call");
        };
        assert_eq!(name.as_str(), "uuid4_str");
        assert!(args.is_empty());
        // It takes no arguments.
        assert!(parse_expr("UUID(1)").is_err());
    }

    #[test]
    fn truncate_lowers_to_scaled_trunc() {
        // TRUNCATE(x, d) -> trunc(x * pow(10, d)) / pow(10, d), built as
        // `CAST(trunc(...) AS REAL) / pow(10, d)`.
        let ast::Expr::Binary(num, ast::Operator::Divide, den) =
            parse_expr("TRUNCATE(x, 2)").unwrap()
        else {
            panic!("expected TRUNCATE to lower to a division");
        };
        // The denominator is pow(10, 2).
        let ast::Expr::FunctionCall { name, .. } = den.as_ref() else {
            panic!("expected the denominator to be pow(10, d)");
        };
        assert_eq!(name.as_str(), "pow");
        // The numerator is CAST(trunc(x * pow(10, 2)) AS REAL).
        let ast::Expr::Cast { expr, .. } = num.as_ref() else {
            panic!("expected the numerator cast to REAL");
        };
        let ast::Expr::FunctionCall { name, .. } = expr.as_ref() else {
            panic!("expected trunc(...)");
        };
        assert_eq!(name.as_str(), "trunc");

        // With a literal `d <= 0` the whole-number result is cast to INTEGER (an
        // outer CAST wrapping the division); a positive `d` stays a real division.
        for d in ["0", "-2"] {
            let ast::Expr::Cast { expr, type_name } =
                parse_expr(&format!("TRUNCATE(x, {d})")).unwrap()
            else {
                panic!("expected TRUNCATE(x, {d}) to cast to INTEGER");
            };
            assert_eq!(type_name.unwrap().name, "INTEGER");
            assert!(matches!(expr.as_ref(), ast::Expr::Binary(_, ast::Operator::Divide, _)));
        }
    }

    #[test]
    fn aggregate_distinct() {
        let expr = parse_expr("COUNT(DISTINCT v)").unwrap();
        let ast::Expr::FunctionCall {
            distinctness, args, ..
        } = expr
        else {
            panic!("expected FunctionCall");
        };
        assert!(matches!(distinctness, Some(ast::Distinctness::Distinct)));
        assert_eq!(args.len(), 1);

        // SUM/MIN/MAX/AVG also accept DISTINCT; ALL is the default and elided.
        for input in [
            "SUM(DISTINCT v)",
            "MIN(DISTINCT v)",
            "MAX(DISTINCT v)",
            "AVG(DISTINCT v)",
        ] {
            assert!(matches!(
                parse_expr(input).unwrap(),
                ast::Expr::FunctionCall {
                    distinctness: Some(ast::Distinctness::Distinct),
                    ..
                }
            ));
        }
        assert!(matches!(
            parse_expr("COUNT(ALL v)").unwrap(),
            ast::Expr::FunctionCall {
                distinctness: None,
                ..
            }
        ));
    }

    #[test]
    fn distinct_rejected_for_scalar_and_count_star() {
        // DISTINCT is only valid in an aggregate, and not with `*`.
        assert!(matches!(
            parse_expr("ABS(DISTINCT v)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
        assert!(matches!(
            parse_expr("COUNT(DISTINCT *)").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn case_expression_forms() {
        // Searched CASE: no base operand, two WHEN/THEN, an ELSE.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("CASE WHEN a = 1 THEN 'x' WHEN a = 2 THEN 'y' ELSE 'z' END").unwrap()
        else {
            panic!("expected Case");
        };
        assert!(base.is_none());
        assert_eq!(when_then_pairs.len(), 2);
        assert!(else_expr.is_some());

        // Simple CASE: a base operand, one WHEN/THEN, no ELSE.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("CASE status WHEN 'publish' THEN 1 END").unwrap()
        else {
            panic!("expected Case");
        };
        assert!(base.is_some());
        assert_eq!(when_then_pairs.len(), 1);
        assert!(else_expr.is_none());

        // A CASE with no WHEN is an error.
        assert!(parse_expr("CASE END").is_err());
    }

    #[test]
    fn function_call_renders_back_to_sql() {
        let sql = parse("SELECT UPPER(name) FROM t").unwrap().to_string();
        assert!(sql.to_uppercase().contains("UPPER"), "{sql}");
    }

    #[test]
    fn function_call_not_in_allow_list_is_unsupported() {
        for input in [
            "CONV('A', 16, 2)",
            "CRC32('x')",
            "SOUNDEX('x')",
            "totally_made_up(1)",
        ] {
            let err = Parser::new(input.as_bytes()).unwrap().expr().unwrap_err();
            assert!(
                matches!(err, ParseError::Unsupported(_)),
                "expected `{input}` to be unsupported, got {err:?}"
            );
        }
    }

    #[test]
    fn if_is_renamed_to_iif() {
        // MySQL `IF` maps to the engine's `IIF`.
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("IF(a > 0, 'p', 'n')").unwrap()
        else {
            panic!("expected FunctionCall");
        };
        assert_eq!(name.as_str().to_ascii_lowercase(), "iif");
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn function_synonyms_are_renamed() {
        // (SUBSTRING / SUBSTR / MID lower to a CASE-guarded substr, covered by
        // `substring_from_for_lowers_like_comma_form`.)
        for (input, engine) in [
            ("LCASE('S')", "lower"),
            ("UCASE('s')", "upper"),
            ("CHAR_LENGTH('s')", "length"),
            ("CHARACTER_LENGTH('s')", "length"),
            ("DATE('2020-01-01 10:00')", "date"),
            ("TIME('2020-01-01 10:00')", "time"),
            ("TIMESTAMP('2020-01-01')", "datetime"),
            ("LAST_INSERT_ID()", "last_insert_rowid"),
            ("REVERSE('abc')", "string_reverse"),
        ] {
            let ast::Expr::FunctionCall { name, .. } = parse_expr(input).unwrap() else {
                panic!("expected `{input}` to parse as a function call");
            };
            assert_eq!(name.as_str().to_ascii_lowercase(), engine, "{input}");
        }
    }

    #[test]
    fn greatest_least_lower_to_nocase_case_fold_with_null_guard() {
        // GREATEST(a, b) -> CASE WHEN a IS NULL OR b IS NULL THEN NULL
        //   ELSE CASE WHEN a >= (b COLLATE NOCASE) THEN a ELSE b END END.
        let ast::Expr::Case {
            when_then_pairs,
            else_expr,
            ..
        } = parse_expr("GREATEST(a, b)").unwrap()
        else {
            panic!("expected GREATEST to lower to a CASE");
        };
        // The guard is `a IS NULL OR b IS NULL`.
        assert_eq!(
            *when_then_pairs[0].0,
            ast::Expr::binary(
                ast::Expr::is_null(col("a")),
                ast::Operator::Or,
                ast::Expr::is_null(col("b")),
            )
        );
        assert_eq!(*when_then_pairs[0].1, ast::Expr::Literal(ast::Literal::Null));
        // The ELSE is the case-insensitive pairwise comparison.
        let else_branch = else_expr.unwrap();
        let ast::Expr::Case { when_then_pairs, .. } = else_branch.as_ref() else {
            panic!("expected the ELSE to be a comparison CASE");
        };
        let ast::Expr::Binary(_, ast::Operator::GreaterEquals, rhs) = when_then_pairs[0].0.as_ref()
        else {
            panic!("expected `a >= b COLLATE NOCASE`");
        };
        assert!(matches!(rhs.as_ref(), ast::Expr::Collate(_, n) if n.as_str() == "NOCASE"));

        // LEAST uses `<=`, and at least two arguments are required.
        let ast::Expr::Case { else_expr, .. } = parse_expr("LEAST(a, b)").unwrap() else {
            panic!("expected LEAST to lower to a CASE");
        };
        let else_branch = else_expr.unwrap();
        let ast::Expr::Case { when_then_pairs, .. } = else_branch.as_ref() else {
            panic!("expected a comparison CASE");
        };
        assert!(matches!(
            when_then_pairs[0].0.as_ref(),
            ast::Expr::Binary(_, ast::Operator::LessEquals, _)
        ));
        assert!(parse_expr("GREATEST(1)").is_err());
    }

    #[test]
    fn expr_arithmetic_precedence() {
        // `*` binds tighter than `+`, both tighter than comparison.
        let expr = parse_expr("a + b * 2 = 10").unwrap();
        let expected = ast::Expr::binary(
            ast::Expr::binary(
                col("a"),
                ast::Operator::Add,
                ast::Expr::binary(col("b"), ast::Operator::Multiply, num("2")),
            ),
            ast::Operator::Equals,
            num("10"),
        );
        assert_eq!(expr, expected);
    }

    #[test]
    fn expr_in_list() {
        let expr = parse_expr("id IN (1, 2, 3)").unwrap();
        let ast::Expr::InList { lhs, not, rhs } = expr else {
            panic!("expected InList");
        };
        assert_eq!(*lhs, col("id"));
        assert!(!not);
        assert_eq!(rhs.len(), 3);

        // `NOT IN`
        let ast::Expr::InList { not, .. } = parse_expr("id NOT IN (1)").unwrap() else {
            panic!("expected InList");
        };
        assert!(not);
    }

    #[test]
    fn row_value_tuples_parse() {
        // `(a, b)` is a row value — Parenthesized with two expressions; a single
        // `(a)` keeps one (an ordinary parenthesized expression).
        let ast::Expr::Parenthesized(exprs) = parse_expr("(a, b)").unwrap() else {
            panic!("expected a Parenthesized row value");
        };
        assert_eq!(exprs.len(), 2);
        assert_eq!(*exprs[0], col("a"));
        assert_eq!(*exprs[1], col("b"));

        assert!(matches!(
            parse_expr("(a)").unwrap(),
            ast::Expr::Parenthesized(e) if e.len() == 1
        ));

        // `(a, b) = (1, 2)` compares two row values.
        let ast::Expr::Binary(lhs, ast::Operator::Equals, rhs) =
            parse_expr("(a, b) = (1, 2)").unwrap()
        else {
            panic!("expected a row-value equality");
        };
        assert!(matches!(lhs.as_ref(), ast::Expr::Parenthesized(e) if e.len() == 2));
        assert!(matches!(rhs.as_ref(), ast::Expr::Parenthesized(e) if e.len() == 2));

        // `(a, b) IN ((1, 2), (3, 4))` keeps the scalar-IN list of two tuples.
        let ast::Expr::InList { rhs, .. } = parse_expr("(a, b) IN ((1, 2), (3, 4))").unwrap()
        else {
            panic!("expected InList");
        };
        assert_eq!(rhs.len(), 2);
        assert!(matches!(rhs[0].as_ref(), ast::Expr::Parenthesized(e) if e.len() == 2));
    }

    #[test]
    fn expr_between() {
        let expr = parse_expr("age BETWEEN 18 AND 65").unwrap();
        let ast::Expr::Between {
            lhs,
            not,
            start,
            end,
        } = expr
        else {
            panic!("expected Between");
        };
        assert_eq!(*lhs, col("age"));
        assert!(!not);
        assert_eq!(*start, num("18"));
        assert_eq!(*end, num("65"));

        // BETWEEN's AND is not swallowed by the logical AND.
        let outer = parse_expr("age BETWEEN 1 AND 10 AND id = 2").unwrap();
        assert!(matches!(outer, ast::Expr::Binary(_, ast::Operator::And, _)));
    }

    #[test]
    fn expr_like() {
        for sql in [
            "name REGEXP '^a'",
            "name RLIKE '^a'",
            "name NOT REGEXP '^a'",
        ] {
            let ast::Expr::Like { op, rhs, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to parse as a Like/Regexp expression");
            };
            assert_eq!(op, ast::LikeOperator::Regexp, "for `{sql}`");
            // The pattern is prefixed with the regex crate's `(?i)` flag so the
            // match is case-insensitive, like MySQL's default REGEXP.
            let ast::Expr::Binary(flag, ast::Operator::Concat, _) = rhs.as_ref() else {
                panic!("expected the pattern to be `'(?i)' || <pattern>` for `{sql}`");
            };
            assert!(
                matches!(flag.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'(?i)'"),
                "for `{sql}`"
            );
        }

        // A BINARY subject (CAST AS BINARY) forces a case-sensitive match: the
        // cast is unwrapped to its text operand and the `(?i)` flag is dropped.
        let ast::Expr::Like { lhs, rhs, .. } =
            parse_expr("CAST(name AS BINARY) REGEXP '^a'").unwrap()
        else {
            panic!("expected a Regexp expression");
        };
        assert_eq!(*lhs, col("name"));
        assert!(
            matches!(rhs.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'^a'"),
            "BINARY REGEXP pattern should not get the (?i) flag"
        );

        // REGEXP_LIKE(str, pat) is the functional form: a Regexp Like, case-
        // insensitive by default like the operator.
        let ast::Expr::Like { op, lhs, rhs, .. } = parse_expr("REGEXP_LIKE(name, '^a')").unwrap()
        else {
            panic!("expected REGEXP_LIKE to parse as a Regexp expression");
        };
        assert_eq!(op, ast::LikeOperator::Regexp);
        assert_eq!(*lhs, col("name"));
        let ast::Expr::Binary(flag, ast::Operator::Concat, _) = rhs.as_ref() else {
            panic!("expected `'(?i)' || <pattern>`");
        };
        assert!(matches!(flag.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'(?i)'"));

        // A `c` match type forces case-sensitivity: no flag prefix, bare pattern.
        let ast::Expr::Like { rhs, .. } = parse_expr("REGEXP_LIKE(name, '^a', 'c')").unwrap()
        else {
            unreachable!()
        };
        assert!(
            matches!(rhs.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'^a'"),
            "case-sensitive REGEXP_LIKE should not prefix `(?i)`"
        );

        // A multi-line match type adds `m` to the inline flags (`(?im)`).
        let ast::Expr::Like { rhs, .. } = parse_expr("REGEXP_LIKE(name, '^a', 'm')").unwrap()
        else {
            unreachable!()
        };
        let ast::Expr::Binary(flag, ast::Operator::Concat, _) = rhs.as_ref() else {
            panic!("expected a flag-prefixed pattern");
        };
        assert!(matches!(flag.as_ref(), ast::Expr::Literal(ast::Literal::String(s)) if s == "'(?im)'"));

        // An unknown match type and a non-literal match type are rejected.
        assert!(matches!(
            parse_expr("REGEXP_LIKE(name, 'a', 'z')").unwrap_err(),
            ParseError::Unsupported(_)
        ));
        assert!(parse_expr("REGEXP_LIKE(name, 'a', flags)").is_err());

        let expr = parse_expr("name LIKE 'a%'").unwrap();
        let ast::Expr::Like {
            lhs, not, escape, ..
        } = expr
        else {
            panic!("expected Like");
        };
        assert_eq!(*lhs, col("name"));
        assert!(!not);
        // A plain LIKE gets MySQL's default backslash escape.
        assert_eq!(
            escape.as_deref(),
            Some(&ast::Expr::Literal(ast::Literal::String(
                "'\\'".to_string()
            )))
        );

        // An explicit ESCAPE clause overrides the default.
        let ast::Expr::Like { escape, .. } = parse_expr("name LIKE 'a!%' ESCAPE '!'").unwrap()
        else {
            panic!("expected Like");
        };
        assert_eq!(
            escape.as_deref(),
            Some(&ast::Expr::Literal(ast::Literal::String("'!'".to_string())))
        );

        let ast::Expr::Like { not, .. } = parse_expr("name NOT LIKE 'a%'").unwrap() else {
            panic!("expected Like");
        };
        assert!(not);
    }

    #[test]
    fn update_basic() {
        let stmt = parse("UPDATE users SET name = 'x', age = 30 WHERE id = 2").unwrap();
        let ast::Stmt::Update(update) = stmt else {
            panic!("expected Update");
        };
        assert_eq!(update.tbl_name.name.as_str(), "users");
        assert_eq!(update.sets.len(), 2);
        assert_eq!(update.sets[0].col_names[0].as_str(), "name");
        assert!(update.where_clause.is_some());
    }

    #[test]
    fn update_renders_back_to_sql() {
        let sql = parse("UPDATE t SET a = 1 WHERE b = 2").unwrap().to_string();
        let upper = sql.to_uppercase();
        assert!(upper.contains("UPDATE") && upper.contains("SET"), "{sql}");
    }

    #[test]
    fn update_unsupported_variants() {
        for sql in [
            // The explicit-JOIN multi-table form is not modeled (only the comma form).
            "UPDATE a JOIN b ON a.id = b.id SET a.x = 1",
            // Updating a table other than the first-listed one is not supported.
            "UPDATE a, b SET b.x = 1",
            // ORDER BY / LIMIT are not valid on a multi-table UPDATE.
            "UPDATE a, b SET a.x = 1 WHERE a.id = b.id LIMIT 2",
            // A LIMIT with an offset stays rejected (MySQL allows only a count).
            "UPDATE t SET a = 1 LIMIT 1, 2",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }

        // A count-only LIMIT is honored.
        let ast::Stmt::Update(update) = parse("UPDATE t SET a = 1 WHERE b = 2 LIMIT 3").unwrap()
        else {
            panic!("expected an Update");
        };
        assert!(update.limit.is_some());
    }

    #[test]
    fn multi_table_update_lowers_to_update_from() {
        // `UPDATE a, b SET a.v = b.v WHERE a.id = b.id` becomes the engine's
        // `UPDATE a SET v = b.v FROM b WHERE a.id = b.id`.
        let ast::Stmt::Update(update) =
            parse("UPDATE a, b SET a.v = b.v WHERE a.id = b.id").unwrap()
        else {
            panic!("expected an Update");
        };
        // The target is the first table; the SET column has its qualifier stripped.
        assert_eq!(update.tbl_name.name.as_str(), "a");
        assert_eq!(update.sets.len(), 1);
        assert_eq!(update.sets[0].col_names[0].as_str(), "v");
        // The second table is moved into a FROM clause.
        let from = update.from.expect("expected a FROM clause");
        let ast::SelectTable::Table(name, _, _) = from.select.as_ref() else {
            panic!("expected a table in FROM");
        };
        assert_eq!(name.name.as_str(), "b");
        assert!(from.joins.is_empty());
        assert!(update.where_clause.is_some());

        // A third source table comma-joins into the FROM clause, and an aliased
        // source is preserved.
        let ast::Stmt::Update(update) =
            parse("UPDATE a, b x, c SET a.v = x.v + c.v WHERE a.id = x.id AND a.k = c.k").unwrap()
        else {
            panic!("expected an Update");
        };
        let from = update.from.expect("expected a FROM clause");
        assert_eq!(from.joins.len(), 1);
        assert!(matches!(from.joins[0].operator, ast::JoinOperator::Comma));

        // An unqualified SET column is taken as the target's.
        let ast::Stmt::Update(update) =
            parse("UPDATE a, b SET v = b.v WHERE a.id = b.id").unwrap()
        else {
            panic!("expected an Update");
        };
        assert_eq!(update.sets[0].col_names[0].as_str(), "v");

        // An aliased target (`UPDATE a x, b y SET x.v = y.v ...`) carries the
        // alias onto the engine's UPDATE target and matches the SET qualifier
        // against it.
        for sql in [
            "UPDATE a x, b y SET x.v = y.v WHERE x.id = y.id",
            "UPDATE a AS x, b AS y SET x.v = y.v WHERE x.id = y.id",
        ] {
            let ast::Stmt::Update(update) = parse(sql).unwrap() else {
                panic!("expected an Update for `{sql}`");
            };
            assert_eq!(update.tbl_name.name.as_str(), "a", "{sql}");
            assert_eq!(
                update.tbl_name.alias.as_ref().map(|n| n.as_str()),
                Some("x"),
                "{sql}"
            );
            assert_eq!(update.sets[0].col_names[0].as_str(), "v", "{sql}");
        }
    }

    #[test]
    fn update_order_by_limit_rewrites_to_rowid_subquery() {
        // `UPDATE ... ORDER BY ... LIMIT n` becomes
        // `UPDATE ... WHERE rowid IN (SELECT rowid FROM t WHERE ... ORDER BY ... LIMIT n)`.
        let ast::Stmt::Update(update) =
            parse("UPDATE t SET a = 1 WHERE b = 2 ORDER BY a DESC LIMIT 5").unwrap()
        else {
            panic!("expected an Update");
        };
        // The outer ORDER BY / LIMIT are folded away.
        assert!(update.order_by.is_empty());
        assert!(update.limit.is_none());
        // The WHERE is now `rowid IN (SELECT rowid FROM t ... ORDER BY ... LIMIT 5)`.
        let ast::Expr::InSelect { lhs, not, rhs } = update.where_clause.as_deref().unwrap() else {
            panic!("expected a rowid IN (subquery) WHERE");
        };
        assert!(!not);
        assert!(matches!(lhs.as_ref(), ast::Expr::Id(n) if n.as_str() == "rowid"));
        assert_eq!(rhs.order_by.len(), 1);
        assert!(rhs.limit.is_some());
    }

    #[test]
    fn delete_basic_and_all() {
        let stmt = parse("DELETE FROM users WHERE id = 1").unwrap();
        let ast::Stmt::Delete {
            tbl_name,
            where_clause,
            ..
        } = stmt
        else {
            panic!("expected Delete");
        };
        assert_eq!(tbl_name.name.as_str(), "users");
        assert!(where_clause.is_some());

        // No WHERE deletes all rows.
        let ast::Stmt::Delete { where_clause, .. } = parse("DELETE FROM users").unwrap() else {
            panic!("expected Delete");
        };
        assert!(where_clause.is_none());
    }

    #[test]
    fn delete_renders_back_to_sql() {
        let sql = parse("DELETE FROM t WHERE a = 1").unwrap().to_string();
        assert!(sql.to_uppercase().contains("DELETE"), "{sql}");
    }

    #[test]
    fn single_target_multi_table_delete_lowers_to_rowid_subquery() {
        // `DELETE a FROM a, b WHERE ...` becomes
        // `DELETE FROM <a's table> WHERE rowid IN (SELECT a.rowid FROM a, b WHERE ...)`.
        let ast::Stmt::Delete {
            tbl_name,
            where_clause,
            ..
        } = parse("DELETE a FROM posts a, terms b WHERE a.id = b.ref").unwrap()
        else {
            panic!("expected a Delete");
        };
        assert_eq!(tbl_name.name.as_str(), "posts"); // alias `a` resolved to its table
        assert!(matches!(
            where_clause.as_deref(),
            Some(ast::Expr::InSelect { not: false, .. })
        ));

        // More than one target table is still unsupported.
        assert!(matches!(
            parse("DELETE a, b FROM posts a, terms b WHERE a.id = b.ref").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn delete_unsupported_variants() {
        for sql in [
            "DELETE t1, t2 FROM t1, t2 WHERE t1.id = t2.id", // multiple target tables
            "DELETE FROM a, b",
            "DELETE FROM t USING u",
            // An offset on the LIMIT stays rejected (MySQL allows only a count).
            "DELETE FROM t LIMIT 1, 2",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }

        // A count-only LIMIT is honored.
        let ast::Stmt::Delete { limit, .. } = parse("DELETE FROM t WHERE a = 1 LIMIT 5").unwrap()
        else {
            panic!("expected a Delete");
        };
        assert!(limit.is_some());
    }

    #[test]
    fn delete_order_by_limit_rewrites_to_rowid_subquery() {
        // `DELETE ... ORDER BY ... LIMIT n` becomes
        // `DELETE ... WHERE rowid IN (SELECT rowid FROM t WHERE ... ORDER BY ... LIMIT n)`.
        let ast::Stmt::Delete {
            where_clause,
            order_by,
            limit,
            ..
        } = parse("DELETE FROM t WHERE a > 0 ORDER BY a LIMIT 2").unwrap()
        else {
            panic!("expected a Delete");
        };
        assert!(order_by.is_empty());
        assert!(limit.is_none());
        let ast::Expr::InSelect { lhs, rhs, .. } = where_clause.as_deref().unwrap() else {
            panic!("expected a rowid IN (subquery) WHERE");
        };
        assert!(matches!(lhs.as_ref(), ast::Expr::Id(n) if n.as_str() == "rowid"));
        assert_eq!(rhs.order_by.len(), 1);
        assert!(rhs.limit.is_some());
        // The original WHERE moved into the subquery.
        let ast::OneSelect::Select { where_clause, .. } = &rhs.body.select else {
            panic!("expected a SELECT subquery");
        };
        assert!(where_clause.is_some());
    }

    #[test]
    fn truncate_lowers_to_delete_all() {
        // TRUNCATE [TABLE] tbl becomes an unfiltered DELETE FROM tbl.
        for sql in ["TRUNCATE TABLE users", "TRUNCATE users"] {
            let ast::Stmt::Delete {
                tbl_name,
                where_clause,
                ..
            } = parse(sql).unwrap()
            else {
                panic!("expected `{sql}` to lower to Delete");
            };
            assert_eq!(tbl_name.name.as_str(), "users");
            assert!(where_clause.is_none(), "TRUNCATE must delete all rows");
        }
    }

    #[test]
    fn create_table_inline_keys_become_deferred_create_index() {
        let parse_all = |sql: &str| Parser::new(sql.as_bytes()).unwrap().parse_statement_list();

        // CREATE TABLE with inline KEY/INDEX clauses yields the CREATE TABLE plus
        // one CREATE INDEX per secondary key (the engine has no inline form).
        let stmts = parse_all(
            "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, KEY ka (a), INDEX (b))",
        )
        .unwrap();
        assert_eq!(stmts.len(), 3);
        assert!(matches!(stmts[0], ast::Stmt::CreateTable { .. }));
        // The named key keeps its name; the unnamed one is `<table>_<col>`.
        let ast::Stmt::CreateIndex {
            unique,
            idx_name,
            tbl_name,
            columns,
            ..
        } = &stmts[1]
        else {
            panic!("expected a CREATE INDEX for the inline KEY");
        };
        assert!(!unique);
        assert_eq!(idx_name.name.as_str(), "ka");
        assert_eq!(tbl_name.as_str(), "t");
        assert_eq!(columns.len(), 1);
        let ast::Stmt::CreateIndex { idx_name, .. } = &stmts[2] else {
            panic!("expected a CREATE INDEX for the unnamed inline INDEX");
        };
        assert_eq!(idx_name.name.as_str(), "t_b");

        // A *named* inline UNIQUE KEY becomes a deferred CREATE UNIQUE INDEX
        // keeping its name (so SHOW INDEX reports it, not an engine auto-name);
        // an unnamed UNIQUE stays a table constraint in the CREATE TABLE.
        let stmts =
            parse_all("CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY ua (a), UNIQUE (b))")
                .unwrap();
        assert_eq!(stmts.len(), 2); // table (with the unnamed UNIQUE) + the named one
        let ast::Stmt::CreateIndex {
            unique, idx_name, ..
        } = &stmts[1]
        else {
            panic!("expected a CREATE UNIQUE INDEX for the named UNIQUE KEY");
        };
        assert!(unique);
        assert_eq!(idx_name.name.as_str(), "ua");

        // A table with no inline secondary key yields just the CREATE TABLE.
        let stmts = parse_all("CREATE TABLE t (id INT PRIMARY KEY, a INT)").unwrap();
        assert_eq!(stmts.len(), 1);

        // FULLTEXT/SPATIAL inline keys degrade to a plain index, FOREIGN KEY is
        // dropped (no engine equivalent).
        let stmts = parse_all(
            "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, FULLTEXT KEY fa (a), \
             FOREIGN KEY (b) REFERENCES u (id))",
        )
        .unwrap();
        assert_eq!(stmts.len(), 2); // table + the fulltext-as-plain index only
        assert!(matches!(stmts[1], ast::Stmt::CreateIndex { .. }));
    }

    #[test]
    fn drop_table_unsupported_variants() {
        // `parse` is single-statement: a multi-table drop has no single engine
        // form, so it is rejected here (it is expanded by `parse_statement_list`).
        for sql in [
            "DROP TABLE a, b",
            "DROP TABLE t RESTRICT",
            "DROP TABLE t CASCADE",
            "DROP DATABASE d",
            "DROP TEMPORARY TABLE mydb.t", // schema-qualified temp drop
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }

    #[test]
    fn multi_table_drop_expands_to_one_per_table() {
        let parse_all = |sql: &str| Parser::new(sql.as_bytes()).unwrap().parse_statement_list();

        // `DROP TABLE a, b, c` -> three DropTable statements.
        let stmts = parse_all("DROP TABLE a, b, c").unwrap();
        assert_eq!(stmts.len(), 3);
        let names: Vec<_> = stmts
            .iter()
            .map(|s| match s {
                ast::Stmt::DropTable {
                    if_exists,
                    tbl_name,
                } => {
                    assert!(!if_exists);
                    tbl_name.name.as_str().to_string()
                }
                _ => panic!("expected DropTable"),
            })
            .collect();
        assert_eq!(names, ["a", "b", "c"]);

        // IF EXISTS applies to every table.
        let stmts = parse_all("DROP TABLE IF EXISTS a, b").unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts.iter().all(|s| matches!(
            s,
            ast::Stmt::DropTable {
                if_exists: true,
                ..
            }
        )));

        // A single-table drop yields one statement; a non-DROP yields one too.
        assert_eq!(parse_all("DROP TABLE a").unwrap().len(), 1);
        assert_eq!(parse_all("SELECT 1").unwrap().len(), 1);

        // RESTRICT/CASCADE on a multi-table drop is still rejected.
        assert!(parse_all("DROP TABLE a, b RESTRICT").is_err());
    }

    #[test]
    fn do_statement_emits_no_statements() {
        let parse_all = |sql: &str| Parser::new(sql.as_bytes()).unwrap().parse_statement_list();

        // `DO expr` parses its expression(s) for validation and yields nothing,
        // so the server replies OK with no result set.
        assert_eq!(parse_all("DO 1 + 1").unwrap().len(), 0);
        assert_eq!(parse_all("DO 1, 2, 3").unwrap().len(), 0);
        assert_eq!(parse_all("DO ABS(-5), POW(2, 3)").unwrap().len(), 0);
        assert_eq!(parse_all("DO (SELECT 1)").unwrap().len(), 0);

        // A malformed expression is still rejected.
        assert!(parse_all("DO ,").is_err());
        assert!(parse_all("DO 1 +").is_err());
    }

    #[test]
    fn multi_op_alter_expands_to_one_per_operation() {
        let parse_all = |sql: &str| Parser::new(sql.as_bytes()).unwrap().parse_statement_list();

        // Each comma-separated operation becomes its own statement, lowered as
        // usual: ADD COLUMN and DROP COLUMN -> AlterTable, ADD KEY -> CreateIndex.
        let stmts = parse_all("ALTER TABLE t ADD a INT, ADD KEY k (a), DROP COLUMN b").unwrap();
        assert_eq!(stmts.len(), 3);
        assert!(matches!(
            stmts[0],
            ast::Stmt::AlterTable(ast::AlterTable {
                body: ast::AlterTableBody::AddColumn(_),
                ..
            })
        ));
        assert!(matches!(stmts[1], ast::Stmt::CreateIndex { .. }));
        assert!(matches!(
            stmts[2],
            ast::Stmt::AlterTable(ast::AlterTable {
                body: ast::AlterTableBody::DropColumn(_),
                ..
            })
        ));

        // A single-operation ALTER yields one statement.
        assert_eq!(parse_all("ALTER TABLE t ADD a INT").unwrap().len(), 1);

        // The single-statement `parse` still rejects the multi-operation form.
        assert!(matches!(
            parse("ALTER TABLE t ADD a INT, ADD b INT").unwrap_err(),
            ParseError::Unsupported(_)
        ));
    }

    #[test]
    fn table_option_alter_is_a_noop() {
        let parse_all = |sql: &str| Parser::new(sql.as_bytes()).unwrap().parse_statement_list();

        // A pure table-option ALTER expands to no statements (the server replies
        // OK without touching the table).
        for sql in [
            "ALTER TABLE t ENGINE=InnoDB",
            "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4",
            "ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            "ALTER TABLE t DEFAULT CHARSET=utf8mb4",
            "ALTER TABLE t ROW_FORMAT=DYNAMIC",
            "ALTER TABLE t AUTO_INCREMENT=100",
            "ALTER TABLE t COMMENT='hi'",
            "ALTER TABLE t ENGINE=InnoDB ROW_FORMAT=DYNAMIC",
        ] {
            assert!(
                parse_all(sql).unwrap().is_empty(),
                "expected `{sql}` to be a no-op (no statements)"
            );
        }

        // Column operations are unaffected -- they still expand normally.
        assert_eq!(parse_all("ALTER TABLE t ADD a INT").unwrap().len(), 1);
        assert_eq!(parse_all("ALTER TABLE t DROP COLUMN a").unwrap().len(), 1);
    }

    #[test]
    fn drop_temporary_table_targets_temp_schema() {
        // `DROP TEMPORARY TABLE t` is qualified onto the temp schema so only the
        // temporary table is dropped.
        let ast::Stmt::DropTable {
            if_exists,
            tbl_name,
        } = parse("DROP TEMPORARY TABLE t").unwrap()
        else {
            panic!("expected DropTable");
        };
        assert!(!if_exists);
        assert_eq!(tbl_name.db_name.as_ref().unwrap().as_str(), "temp");
        assert_eq!(tbl_name.name.as_str(), "t");

        // `IF EXISTS` carries through.
        let ast::Stmt::DropTable { if_exists, .. } =
            parse("DROP TEMPORARY TABLE IF EXISTS t").unwrap()
        else {
            panic!("expected DropTable");
        };
        assert!(if_exists);
    }
}
