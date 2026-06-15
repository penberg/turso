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
    /// Byte offset just past the end of input, for end-of-input errors.
    eof: usize,
    /// Number of positional `?` placeholders seen so far. MySQL parameters are
    /// purely positional, so each `?` takes the next index in appearance order.
    params: u32,
}

impl Parser {
    // === Entry points ===

    /// Tokenizes `input` and prepares a parser.
    pub fn new(input: &[u8]) -> Result<Self> {
        let tokens = Lexer::new(input).tokenize()?;
        Ok(Self {
            tokens,
            pos: 0,
            eof: input.len(),
            params: 0,
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
            // Recognized statement keywords that are simply not implemented yet.
            "SET" | "SHOW" | "USE" | "DESCRIBE" | "DESC" | "EXPLAIN" | "SAVEPOINT" | "GRANT"
            | "REVOKE" | "CALL" | "DO" | "VALUES" | "TABLE" | "PREPARE" | "EXECUTE"
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
        loop {
            if self.next_is_table_constraint() {
                self.table_constraint(&mut constraints)?;
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
    ///   - `DROP [COLUMN] col` → `ALTER TABLE ... DROP COLUMN`,
    ///   - `RENAME [TO|AS] new` → `ALTER TABLE ... RENAME TO`, and
    ///   - `RENAME COLUMN old TO new` → `ALTER TABLE ... RENAME COLUMN`.
    ///
    /// Everything else — `ADD PRIMARY KEY`/`FOREIGN KEY`/`SPATIAL`/`CONSTRAINT`,
    /// `DROP {INDEX|PRIMARY KEY|FOREIGN KEY}`, the type-changing `CHANGE`/`MODIFY`
    /// operations, `RENAME INDEX`, and the comma-separated multi-operation form —
    /// is rejected as unsupported.
    fn alter(&mut self) -> Result<ast::Stmt> {
        self.expect_keyword("TABLE")?;
        let name = self.qualified_name()?;

        if self.eat_keyword("DROP") {
            return self.alter_drop(name);
        }
        if self.eat_keyword("RENAME") {
            return self.alter_rename(name);
        }
        if !self.eat_keyword("ADD") {
            return Err(ParseError::Unsupported(
                "only ALTER TABLE ... ADD / DROP COLUMN / RENAME is supported yet".to_string(),
            ));
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

        // `COLUMN` is optional after `ADD`. Any other index/constraint add starts
        // with one of these keywords and has no single-statement engine
        // equivalent.
        self.eat_keyword("COLUMN");
        for kw in ["PRIMARY", "CONSTRAINT", "SPATIAL", "FOREIGN", "CHECK"] {
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
        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "ALTER TABLE with multiple operations is not supported yet".to_string(),
            ));
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

    /// Lowers `DROP [COLUMN] col` (the `DROP` keyword is already consumed) to the
    /// engine's `ALTER TABLE ... DROP COLUMN`. Dropping an index, primary key, or
    /// foreign key (`DROP {INDEX|KEY|PRIMARY KEY|FOREIGN KEY|CONSTRAINT|CHECK}`)
    /// has no single-statement engine equivalent and is rejected.
    fn alter_drop(&mut self, name: ast::QualifiedName) -> Result<ast::Stmt> {
        // `DROP {INDEX|KEY} idx_name` drops a secondary index, mirroring the
        // `ADD KEY` -> `CREATE INDEX` lowering; it becomes the engine's
        // `DROP INDEX idx_name` (index names are per-database here, so the table
        // is implied). `DROP PRIMARY KEY` / `DROP FOREIGN KEY` have no in-place
        // engine form.
        if self.eat_keyword("INDEX") || self.eat_keyword("KEY") {
            let idx_name = self.qualified_name()?;
            return Ok(ast::Stmt::DropIndex {
                if_exists: false,
                idx_name,
            });
        }
        for kw in ["PRIMARY", "FOREIGN", "CONSTRAINT", "CHECK"] {
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
        let (constraints, auto_increment) = self.column_constraints()?;
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
    /// size is dropped.
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

    /// Parses zero or more inline column constraints. Returns the constraints
    /// and whether `AUTO_INCREMENT` was declared on the column.
    fn column_constraints(&mut self) -> Result<(Vec<ast::NamedColumnConstraint>, bool)> {
        let mut out: Vec<ast::NamedColumnConstraint> = Vec::new();
        let mut auto_increment = false;
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
                let _ = self.name()?;
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
            } else if self.is_keyword("REFERENCES")
                || self.is_keyword("CHECK")
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
        Ok((out, auto_increment))
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

    fn table_constraint(&mut self, out: &mut Vec<ast::NamedTableConstraint>) -> Result<()> {
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
            if !self.is(&Token::LParen)
                && matches!(
                    self.peek(),
                    Some(Token::Word(_)) | Some(Token::QuotedIdent(_))
                )
            {
                let _ = self.name()?;
            }
            let columns = self.sorted_column_list()?;
            out.push(ast::NamedTableConstraint {
                name,
                constraint: ast::TableConstraint::Unique {
                    columns,
                    conflict_clause: None,
                },
            });
        } else if self.is_keyword("KEY")
            || self.is_keyword("INDEX")
            || self.is_keyword("FOREIGN")
            || self.is_keyword("CHECK")
            || self.is_keyword("FULLTEXT")
            || self.is_keyword("SPATIAL")
        {
            // Index definitions and constraints we do not model yet: skip them.
            self.skip_to_item_boundary();
        } else {
            return Err(self.unexpected("a table constraint"));
        }
        Ok(())
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

        let mut tbl_name = self.qualified_name()?;

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

        if temporary {
            if tbl_name.db_name.is_some() {
                return Err(ParseError::Unsupported(
                    "DROP TEMPORARY TABLE with a schema qualifier is not supported yet".to_string(),
                ));
            }
            // Resolve against the temp schema only, so a base table of the same
            // name is never dropped.
            tbl_name = ast::QualifiedName::fullname(ast::Name::from_string("temp"), tbl_name.name);
        }

        Ok(ast::Stmt::DropTable {
            if_exists,
            tbl_name,
        })
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
        let kw = if or_conflict.is_some() {
            "REPLACE"
        } else {
            "INSERT"
        };
        for modifier in ["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY"] {
            if self.is_keyword(modifier) {
                return Err(ParseError::Unsupported(format!(
                    "{kw} {modifier} is not supported yet"
                )));
            }
        }

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

        // Optional explicit column list.
        let mut columns = Vec::new();
        if self.eat(&Token::LParen) {
            loop {
                columns.push(self.name()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
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
                    row.push(Box::new(self.expr()?));
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

        // `ON DUPLICATE KEY UPDATE` is an INSERT-only clause; REPLACE has its
        // own conflict resolution and does not take it.
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

    /// Parses the right-hand side of an `ON DUPLICATE KEY UPDATE` assignment. A
    /// leading `VALUES(col)` is lowered to `excluded.col`; anything else is an
    /// ordinary expression (a bare column refers to the existing row's value, as
    /// in MySQL). `VALUES(...)` nested inside a larger expression is not modeled
    /// and falls out as a parse error.
    fn upsert_assignment_value(&mut self) -> Result<ast::Expr> {
        if self.is_keyword("VALUES") {
            self.advance();
            self.expect(&Token::LParen, "`(`")?;
            let col = self.name()?;
            self.expect(&Token::RParen, "`)`")?;
            return Ok(ast::Expr::Qualified(
                ast::Name::from_string("excluded"),
                col,
            ));
        }
        self.expr()
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
        // `UNION ALL` does not). Each branch may be parenthesized.
        let mut compounds = Vec::new();
        loop {
            let operator = if self.eat_keyword("UNION") {
                if self.eat_keyword("ALL") {
                    ast::CompoundOperator::UnionAll
                } else {
                    ast::CompoundOperator::Union
                }
            } else if self.eat_keyword("INTERSECT") {
                ast::CompoundOperator::Intersect
            } else if self.eat_keyword("EXCEPT") {
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
    /// `FOR UPDATE`, `FOR SHARE`, or `LOCK IN SHARE MODE`. The engine is a single
    /// writer, so explicit row locking is a no-op and the locked query returns
    /// exactly the same rows as the unlocked one; see `mysql/COMPAT.md`. The
    /// `OF tbl` / `NOWAIT` / `SKIP LOCKED` refinements are not consumed here, so
    /// they fall through and are rejected as unsupported.
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
        if self.is_keyword("DISTINCTROW") {
            // MySQL synonym for DISTINCT; not modeled.
            return Err(ParseError::Unsupported(
                "SELECT DISTINCTROW is not supported yet".to_string(),
            ));
        }
        let distinctness = if self.eat_keyword("DISTINCT") {
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
            Some(self.from_clause()?)
        } else {
            None
        };

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };

        let group_by = self.group_by()?;

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
                let expr = self.expr()?;
                let alias = self.column_alias()?;
                columns.push(ast::ResultColumn::Expr(Box::new(expr), alias));
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(columns)
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
                    if t.contains(ast::JoinType::CROSS) || t.contains(ast::JoinType::NATURAL)
            ) {
                // A `CROSS JOIN` (Cartesian product) and a `NATURAL JOIN` (joins
                // on the common columns) both take no explicit condition.
                None
            } else {
                return Err(ParseError::Unsupported(
                    "JOIN without an ON condition is not supported yet".to_string(),
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
        let alias = self.table_alias()?;
        Ok(ast::SelectTable::Table(tbl_name, alias, None))
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
        let first = self.expr()?;
        if self.eat(&Token::Comma) {
            let count = self.expr()?;
            Ok(Some(ast::Limit {
                expr: Box::new(count),
                offset: Some(Box::new(first)),
            }))
        } else if self.eat_keyword("OFFSET") {
            let offset = self.expr()?;
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
        let count = self.expr()?;
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

    /// Parses `UPDATE tbl SET col = expr [, ...] [WHERE expr]`. Multi-table
    /// updates, `ORDER BY`/`LIMIT`, and the `LOW_PRIORITY`/`IGNORE` modifiers are
    /// rejected as unsupported.
    fn update(&mut self) -> Result<ast::Stmt> {
        // `UPDATE` has already been consumed.
        if self.is_keyword("LOW_PRIORITY") || self.is_keyword("IGNORE") {
            return Err(ParseError::Unsupported(
                "UPDATE LOW_PRIORITY / IGNORE is not supported yet".to_string(),
            ));
        }

        let tbl_name = self.qualified_name()?;
        if self.is(&Token::Comma) || self.is_keyword("JOIN") {
            return Err(ParseError::Unsupported(
                "multi-table UPDATE is not supported yet".to_string(),
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

        // `ORDER BY` is rejected — the engine cannot order an UPDATE — but the
        // count-only `LIMIT` is honored.
        if self.is_keyword("ORDER") {
            return Err(ParseError::Unsupported(
                "ORDER BY on UPDATE is not supported yet".to_string(),
            ));
        }
        let limit = self.row_limit()?;

        Ok(ast::Stmt::Update(ast::Update {
            with: None,
            or_conflict: None,
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

    /// Parses `ROLLBACK [WORK]`. `ROLLBACK TO [SAVEPOINT]` and the
    /// `AND CHAIN`/`RELEASE` modifiers are rejected. `ROLLBACK` has already been
    /// consumed.
    fn rollback_transaction(&mut self) -> Result<ast::Stmt> {
        self.eat_keyword("WORK");
        if self.is_keyword("TO") {
            return Err(ParseError::Unsupported(
                "ROLLBACK TO SAVEPOINT is not supported yet".to_string(),
            ));
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
        // `DELETE` has already been consumed.
        if self.is_keyword("LOW_PRIORITY") || self.is_keyword("QUICK") || self.is_keyword("IGNORE")
        {
            return Err(ParseError::Unsupported(
                "DELETE LOW_PRIORITY / QUICK / IGNORE is not supported yet".to_string(),
            ));
        }

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

        // `ORDER BY` is rejected — the engine cannot order a DELETE — but the
        // count-only `LIMIT` is honored.
        if self.is_keyword("ORDER") {
            return Err(ParseError::Unsupported(
                "ORDER BY on DELETE is not supported yet".to_string(),
            ));
        }
        let limit = self.row_limit()?;

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
        let mut lhs = self.multiplicative_expr()?;
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

    /// Multiplicative tier: `*` and the MySQL keyword operators `DIV` (integer
    /// division) and `MOD` (modulo). The symbolic `/` and `%` are intentionally
    /// not parsed — their MySQL semantics differ from SQLite (float division,
    /// float modulo) — but `DIV`/`MOD` have a well-defined integer mapping:
    ///   - `a DIV b` → `CAST(a / b AS INTEGER)`: the quotient truncated toward
    ///     zero, regardless of whether the engine divides as integer or float.
    ///   - `a MOD b` → `a - b * CAST(a / b AS INTEGER)`: the remainder, which
    ///     takes the sign of `a` and is exact for float operands too (where the
    ///     engine's `%` would wrongly truncate to integers).
    fn multiplicative_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.collate_expr()?;
        loop {
            if self.is(&Token::Star) {
                self.advance();
                let rhs = self.collate_expr()?;
                lhs = ast::Expr::binary(lhs, ast::Operator::Multiply, rhs);
            } else if self.eat_keyword("DIV") {
                let rhs = self.collate_expr()?;
                lhs = integer_division(lhs, rhs);
            } else if self.eat_keyword("MOD") {
                let rhs = self.collate_expr()?;
                lhs = modulo(lhs, rhs);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// A primary expression optionally wrapped by the `BINARY` prefix operator
    /// and/or followed by a `COLLATE collation_name` postfix.
    ///
    /// MySQL's `COLLATE` overrides the collation used for comparison and sorting,
    /// and the `BINARY expr` prefix forces a binary (case- and accent-sensitive)
    /// comparison. The engine is effectively single-collation (binary), which is
    /// already what `BINARY` asks for, so the operator is dropped and the value
    /// returned unchanged; `COLLATE` is likewise parsed and discarded. Honoring a
    /// case-insensitive collation would need engine support — an intentional
    /// divergence (see `mysql/COMPAT.md`). Both bind tighter than the arithmetic
    /// operators, so they are applied here at the primary tier.
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
        // `BINARY expr` — drop the operator; the engine compares binary already.
        if self.is_keyword("BINARY") {
            self.advance();
            return self.collate_expr();
        }
        let expr = self.primary_expr()?;
        while self.eat_keyword("COLLATE") {
            // The collation name is an identifier (e.g. `utf8mb4_general_ci`);
            // consume and drop it.
            self.name()?;
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
            // usable as a value, possibly correlated — or an ordinary
            // parenthesized expression. The `Parenthesized` wrapper is kept so
            // the rendered SQL preserves the original grouping.
            Some(Token::LParen) => {
                self.advance();
                if self.eat_keyword("SELECT") {
                    let select = self.parse_select()?;
                    self.expect(&Token::RParen, "`)`")?;
                    return Ok(ast::Expr::Subquery(select));
                }
                let inner = self.expr()?;
                self.expect(&Token::RParen, "`)`")?;
                Ok(ast::Expr::Parenthesized(vec![Box::new(inner)]))
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
        Ok(ast::Expr::Cast {
            expr: Box::new(expr),
            type_name: Some(type_name),
        })
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
        Ok(ast::Expr::Cast {
            expr: Box::new(expr),
            type_name: Some(type_name),
        })
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

        // `CONCAT(a, b, ...)` lowers to the engine's `||` concatenation, which —
        // like MySQL's CONCAT — yields NULL if any argument is NULL. (The
        // engine's own `concat()` skips NULLs instead, so it is not used here.)
        if upper == "CONCAT" {
            return self.concat_call();
        }

        // `CHAR(n, ...)` builds a string from character codes, mapping to the
        // engine's `char()` (see `char_call`).
        if upper == "CHAR" {
            return self.char_call();
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

        // `GET_LOCK(name[, timeout])` / `RELEASE_LOCK(name)` are MySQL advisory
        // locks. This is a single-node engine with no cross-session lock table,
        // so they fold to a constant `1` ("acquired" / "released") — matching
        // MySQL for the uncontended acquire/release flow WordPress uses. The lock
        // name and timeout are parsed and discarded. The contended/not-held cases
        // (where MySQL returns 0 or NULL) are not modeled (see `mysql/COMPAT.md`).
        if upper == "GET_LOCK" || upper == "RELEASE_LOCK" {
            return self.advisory_lock_call();
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
        if upper == "DATE_FORMAT" {
            return self.date_format_call();
        }

        // `DATEDIFF(a, b)` is the whole-day difference `a - b`, ignoring the time
        // parts, which is `CAST(julianday(date(a)) - julianday(date(b)) AS INTEGER)`.
        if upper == "DATEDIFF" {
            return self.datediff_call();
        }

        // `TIMESTAMPDIFF(unit, a, b)` is `b - a` in whole `unit`s. The
        // fixed-duration units lower to integer division of the epoch-second
        // difference.
        if upper == "TIMESTAMPDIFF" {
            return self.timestampdiff_call();
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
            return Ok(ast::Expr::FunctionCallStar {
                name,
                filter_over: ast::FunctionTail {
                    filter_clause: None,
                    over_clause: None,
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
                over_clause: None,
            },
        })
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
    /// Unicode code points of its integer arguments. For the common ASCII and
    /// control-character codes (e.g. `CHAR(10)` newline, `CHAR(72, 73)` -> `HI`)
    /// this matches MySQL exactly. Two documented divergences: MySQL skips NULL
    /// arguments whereas the engine stops at the first NULL, and for code points
    /// above 127 MySQL emits raw bytes (a number can span several) while the
    /// engine emits the single UTF-8 code point. The `CHAR(... USING charset)`
    /// form is rejected (the `USING` clause is left unparsed). At least one
    /// argument is required.
    fn char_call(&mut self) -> Result<ast::Expr> {
        let mut args = Vec::new();
        loop {
            args.push(self.expr()?);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen, "`)`")?;
        Ok(call_fn("char", args))
    }

    /// Parses a `FIELD(x, a, b, ...)` call (the name and `(` are already
    /// consumed) and lowers it to `CASE x WHEN a THEN 1 WHEN b THEN 2 ... ELSE 0
    /// END`, which the engine evaluates the same way MySQL's `FIELD` does: the
    /// 1-based index of the first argument among the rest, or 0 if absent or
    /// NULL. At least one argument is required.
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
    /// and lowers it to `CASE n WHEN 1 THEN a WHEN 2 THEN b ... END` — the `n`-th
    /// string argument (1-based), which the engine evaluates the same way MySQL's
    /// `ELT` does. The `CASE` has no `ELSE`, so an out-of-range or NULL `n` (which
    /// matches no `WHEN`) yields NULL, matching MySQL. At least two arguments (the
    /// index and one string) are required.
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
        Ok(ast::Expr::Case {
            base: Some(Box::new(index)),
            when_then_pairs,
            else_expr: None,
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

    /// Parses an `ISNULL(x)` call (the name and `(` are already consumed) and
    /// lowers it to the `x IS NULL` predicate, which the engine evaluates to 1
    /// when `x` is NULL and 0 otherwise — exactly MySQL's `ISNULL`. Exactly one
    /// argument is required.
    fn isnull_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        Ok(ast::Expr::is_null(arg))
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

    /// Parses a `GET_LOCK(name[, timeout])` / `RELEASE_LOCK(name)` advisory-lock
    /// call (the name and `(` are already consumed), discards its arguments, and
    /// folds it to the literal `1`. This single-node engine has no cross-session
    /// lock table, so the no-op model always reports success; the contended and
    /// not-held cases (where MySQL returns 0 or NULL) are not reproduced.
    fn advisory_lock_call(&mut self) -> Result<ast::Expr> {
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
        Ok(ast::Expr::Literal(ast::Literal::Numeric("1".to_string())))
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
                Box::new(len_arg),
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
        // `-len`, built as `0 - len` to avoid a unary-minus node.
        let neg_len = ast::Expr::binary(
            ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
            ast::Operator::Subtract,
            len_arg.clone(),
        );
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("substr"),
            distinctness: None,
            args: vec![Box::new(str_arg), Box::new(neg_len), Box::new(len_arg)],
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
    /// already consumed) and lowers it to the same `CAST(strftime(fmt, expr) AS
    /// INTEGER)` as the date-part extractor functions. Only the single calendar
    /// units that map to an engine `strftime` code are supported; `QUARTER`,
    /// `WEEK`, `MICROSECOND`, and the compound units (`YEAR_MONTH`, `DAY_HOUR`,
    /// …) are rejected.
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
    /// lowers it to `CAST(strftime(fmt, d) AS INTEGER)`. MySQL's week `mode`
    /// (default `0`, MySQL's `default_week_format`) selects among eight week
    /// numbering schemes; only the three whose definition matches an engine
    /// strftime format are supported:
    ///   - mode 0 → `%U` (Sunday-first, 0–53, week 1 = first week with a Sunday),
    ///   - mode 3 → `%V` (ISO 8601, Monday-first, 1–53),
    ///   - mode 5 → `%W` (Monday-first, 0–53, week 1 = first week with a Monday).
    ///
    /// The other modes (1/2/4/6/7) have no exact strftime equivalent and are
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

        let fmt = match mode {
            0 => "%U",
            3 => "%V",
            5 => "%W",
            other => {
                return Err(ParseError::Unsupported(format!(
                    "WEEK() mode {other} is not supported yet \
                     (only modes 0, 3, and 5 map to an engine week format)"
                )))
            }
        };

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
        Ok(ast::Expr::Cast {
            expr: Box::new(strftime),
            type_name: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            }),
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
        let negative = self.eat(&Token::Minus);
        // MySQL takes the interval amount either as a number or as a quoted
        // numeric string and coerces the string to an integer; accept both.
        let raw = match self.peek() {
            Some(Token::Num(n) | Token::Str(n)) => n.clone(),
            _ => return Err(self.unexpected("an integer interval value")),
        };
        let value: i64 = raw.trim().parse().map_err(|_| {
            ParseError::Unsupported("INTERVAL value must be an integer literal".to_string())
        })?;
        self.advance();

        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("an interval unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();

        // Map the MySQL unit onto the engine's modifier unit; `WEEK` has no
        // engine modifier and is expanded to days.
        let (engine_unit, multiplier) = match unit.as_str() {
            "DAY" => ("days", 1),
            "WEEK" => ("days", 7),
            "MONTH" => ("months", 1),
            "YEAR" => ("years", 1),
            "HOUR" => ("hours", 1),
            "MINUTE" => ("minutes", 1),
            "SECOND" => ("seconds", 1),
            other => {
                return Err(ParseError::Unsupported(format!(
                    "INTERVAL unit {other} is not supported yet"
                )))
            }
        };

        let mut amount = value.saturating_mul(multiplier);
        if negative {
            amount = -amount;
        }
        if subtract {
            amount = -amount;
        }
        // `{:+}` renders an explicit sign, e.g. `+5 days` / `-1 days`.
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

    /// Parses `DATE_FORMAT(x, 'fmt')` (the name and `(` are already consumed) and
    /// lowers it via [`date_format_expr`] — a `strftime` over `x` for the
    /// directly-translatable specifiers, with month/weekday name specifiers
    /// expanded to `CASE` lookups and concatenated. The format must be a string
    /// literal so it can be translated at parse time.
    fn date_format_call(&mut self) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let Some(Token::Str(fmt)) = self.peek() else {
            return Err(self.unexpected("a string-literal DATE_FORMAT format"));
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
        Ok(ast::Expr::binary(hm, ast::Operator::Add, seconds))
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
        Ok(repeat_expr(s, n))
    }

    /// Lowers `SPACE(n)` (the name and `(` are already consumed) to `REPEAT(' ',
    /// n)` via [`repeat_expr`] — a string of `n` spaces, the empty string for a
    /// non-positive `n`, and NULL for a NULL `n`, matching MySQL. Exactly one
    /// argument is required.
    fn space_call(&mut self) -> Result<ast::Expr> {
        let n = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let space = ast::Expr::Literal(ast::Literal::String(requote(" ")));
        Ok(repeat_expr(space, n))
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

    /// Lowers `TIMESTAMPDIFF(unit, a, b)` (the name and `(` are already
    /// consumed) to the whole-`unit` count of `b - a` — note the operand order
    /// is the reverse of `DATEDIFF`. The fixed-duration units divide the
    /// epoch-second difference `unixepoch(b) - unixepoch(a)` by the unit's length
    /// in seconds; SQLite's integer division truncates toward zero, matching
    /// MySQL's "complete units" semantics for both signs. The calendar units
    /// (`MICROSECOND`, `MONTH`, `QUARTER`, `YEAR`) have no fixed length and are
    /// rejected. NULL propagates.
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

        let seconds_per_unit: i64 = match unit.as_str() {
            "SECOND" => 1,
            "MINUTE" => 60,
            "HOUR" => 3600,
            "DAY" => 86400,
            "WEEK" => 604800,
            other => {
                return Err(ParseError::Unsupported(format!(
                    "TIMESTAMPDIFF unit {other} is not supported yet \
                     (only SECOND, MINUTE, HOUR, DAY, and WEEK have a fixed length)"
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
            Ok(ast::Expr::Qualified(first, second))
        } else {
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

fn numeric_expr(value: &str) -> Box<ast::Expr> {
    Box::new(ast::Expr::Literal(ast::Literal::Numeric(value.to_string())))
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
/// padding. (A negative `len` yields the empty string rather than MySQL's NULL,
/// and an empty `pad` yields `target` unchanged — minor documented edges.)
fn pad_expr(left: bool, target: ast::Expr, len: ast::Expr, pad: ast::Expr) -> ast::Expr {
    let one = || ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
    let filler = repeat_expr(pad, len.clone());
    let body = if left {
        let fill_len = ast::Expr::binary(
            len.clone(),
            ast::Operator::Subtract,
            unary_fn("length", target.clone()),
        );
        let fill = substr_fn(filler, one(), fill_len);
        ast::Expr::binary(fill, ast::Operator::Concat, target)
    } else {
        ast::Expr::binary(target, ast::Operator::Concat, filler)
    };
    substr_fn(body, one(), len)
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

/// Re-quotes a lexed (unescaped) string as a SQL single-quoted literal.
fn requote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Keywords that, appearing where a column type would, mean the type is absent.
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
        // Scalar functions.
        "COALESCE" | "NULLIF" | "IFNULL" | "ABS" | "LOWER" | "UPPER"
        // String functions sharing both name and behaviour with the engine.
        // `LTRIM`/`RTRIM` strip leading/trailing spaces (their one-argument
        // MySQL form), like the engine's same-named functions. (`TRIM` is handled
        // separately by `trim_call`, which also parses the `TRIM(... FROM ...)`
        // forms.)
        | "REPLACE" | "SUBSTR" | "LTRIM" | "RTRIM"
        // `CONCAT_WS(sep, ...)` joins the non-NULL arguments with `sep`, skipping
        // NULLs (and yielding NULL only for a NULL separator) — exactly the
        // engine's `concat_ws`. (Distinct from `CONCAT`, which is lowered to `||`
        // so it propagates NULL; see `concat_call`.)
        | "CONCAT_WS"
        // Functions sharing behaviour with the engine under a different name;
        // renamed on emit (see `engine_function_name`).
        | "IF"
        | "SUBSTRING" | "MID" | "LCASE" | "UCASE" | "CHAR_LENGTH" | "CHARACTER_LENGTH"
        // The scalar `GREATEST` / `LEAST` map to the engine's multi-argument
        // `max` / `min`, which — like MySQL — return NULL if any argument is NULL.
        | "GREATEST" | "LEAST"
        // The single-argument date/time extractors `DATE`/`TIME`/`TIMESTAMP` map
        // onto the engine's `date`/`time`/`datetime` (renamed below). They return
        // the date, time, or full datetime of the value, like MySQL.
        | "DATE" | "TIME" | "TIMESTAMP"
        // `SIGN(x)` returns -1/0/1 (an integer on both). `LAST_INSERT_ID()`
        // returns the connection's last auto-increment id — the engine's
        // `last_insert_rowid()` (renamed below), which matches because MySQL
        // `AUTO_INCREMENT` is lowered to the rowid-alias integer primary key.
        | "SIGN" | "LAST_INSERT_ID"
        // Aggregate functions.
        | "COUNT" | "SUM" | "MIN" | "MAX"
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
/// integer (which drops the zero padding). The pieces are concatenated with
/// `||`. A NULL `target` makes every piece NULL, so the whole result is NULL, as
/// in MySQL. Any other specifier (`%h`, `%p`, `%D`, the `%u`/`%V` week modes, …)
/// is rejected rather than silently mistranslated.
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
    matches!(upper_name, "COUNT" | "SUM" | "MIN" | "MAX")
}

/// The engine's name for a MySQL function that shares its behaviour but not its
/// spelling, or `None` to keep the name as written. `CHAR_LENGTH` maps to
/// `length` because the engine's `length()` counts characters (MySQL's `LENGTH`,
/// which counts bytes, is excluded). `upper_name` must already be uppercased.
fn engine_function_name(upper_name: &str) -> Option<&'static str> {
    Some(match upper_name {
        "IF" => "iif",
        "SUBSTRING" | "MID" => "substr",
        "LCASE" => "lower",
        "UCASE" => "upper",
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => "length",
        "GREATEST" => "max",
        "LEAST" => "min",
        "DATE" => "date",
        "TIME" => "time",
        "TIMESTAMP" => "datetime",
        "LAST_INSERT_ID" => "last_insert_rowid",
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
        ] {
            assert!(
                matches!(parse(sql).unwrap(), ast::Stmt::Select(_)),
                "expected `{sql}` to parse as a SELECT"
            );
        }
        // A `FOR`-prefixed clause that is not a locking read is still rejected
        // (the stray `FOR` is left for the end-of-input check).
        assert!(parse("SELECT a FROM t FOR somethingelse").is_err());
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
            "SELECT DISTINCTROW a FROM t",
            "SELECT * FROM a FULL JOIN b ON a.id = b.id",
            "SELECT * FROM a FULL OUTER JOIN b ON a.id = b.id",
            "SELECT * FROM a JOIN b",
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
    fn avg_and_group_by_ordinal_are_unsupported() {
        // AVG diverges (DECIMAL formatting); GROUP BY ordinal diverges.
        assert!(matches!(
            parse("SELECT AVG(a) FROM t").unwrap_err(),
            ParseError::Unsupported(_)
        ));
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
    fn insert_unsupported_variants() {
        for sql in ["INSERT DELAYED INTO t VALUES (1)"] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
    fn insert_on_duplicate_values_inside_expression_is_rejected() {
        // `VALUES(...)` is only modeled as a whole RHS; nested in an expression
        // it hits the function allow-list and is rejected.
        assert!(
            parse("INSERT INTO t (n) VALUES (1) ON DUPLICATE KEY UPDATE n = n + VALUES(n)")
                .is_err()
        );
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
            ast::Stmt::Rollback { .. }
        ));
    }

    #[test]
    fn transaction_unsupported_variants() {
        for sql in [
            "START TRANSACTION READ ONLY",
            "START TRANSACTION WITH CONSISTENT SNAPSHOT",
            "ROLLBACK TO SAVEPOINT sp",
            "ROLLBACK TO sp",
            "SAVEPOINT sp",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
        // `ADD COLUMN` and the COLUMN-elided `ADD` both lower to AddColumn.
        for sql in [
            "ALTER TABLE t ADD COLUMN c INT DEFAULT 0",
            "ALTER TABLE t ADD c INT DEFAULT 0",
        ] {
            let ast::Stmt::AlterTable(alter) = parse(sql).unwrap() else {
                panic!("expected `{sql}` to parse as ALTER TABLE");
            };
            assert_eq!(alter.name.name.as_str(), "t");
            let ast::AlterTableBody::AddColumn(col) = &alter.body else {
                panic!("expected an ADD COLUMN body for `{sql}`");
            };
            assert_eq!(col.col_name.as_str(), "c");
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
    fn alter_table_unsupported_variants() {
        // Primary/foreign-key and other constraint adds, index/key drops, the
        // type-changing CHANGE/MODIFY operations, RENAME INDEX, and the
        // multi-operation comma form are all rejected (a real mysqld accepts
        // them, but the engine has no in-place equivalent).
        for sql in [
            "ALTER TABLE t ADD PRIMARY KEY (id)",
            "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (c) REFERENCES u (id)",
            "ALTER TABLE t ADD SPATIAL KEY sp (c)",
            "ALTER TABLE t ADD COLUMN c INT AUTO_INCREMENT",
            "ALTER TABLE t ADD a INT, ADD b INT",
            "ALTER TABLE t DROP PRIMARY KEY",
            "ALTER TABLE t DROP FOREIGN KEY fk",
            "ALTER TABLE t CHANGE COLUMN a b INT",
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
            ("CAST(a AS SIGNED)", "INTEGER"),
            ("CAST(a AS SIGNED INTEGER)", "INTEGER"),
            ("CAST(a AS UNSIGNED)", "INTEGER"),
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
        // Specifiers still without a lowering are rejected (12-hour time `%r`,
        // microseconds `%f`, week-year `%X`).
        for fmt in [
            "DATE_FORMAT(d, '%r')",
            "DATE_FORMAT(d, '%f')",
            "DATE_FORMAT(d, '%X')",
        ] {
            assert!(matches!(
                parse_expr(fmt).unwrap_err(),
                ParseError::Unsupported(_)
            ));
        }
        // A non-literal format is rejected.
        assert!(parse_expr("DATE_FORMAT(d, f)").is_err());
    }

    #[test]
    fn date_add_sub_lower_to_datetime_modifier() {
        // Each lowers to datetime(target, '<signed-n> <unit>').
        let cases = [
            ("DATE_ADD(d, INTERVAL 5 DAY)", "'+5 days'"),
            ("DATE_SUB(d, INTERVAL 1 DAY)", "'-1 days'"),
            ("DATE_ADD(d, INTERVAL 1 WEEK)", "'+7 days'"),
            ("DATE_ADD(d, INTERVAL 2 MONTH)", "'+2 months'"),
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
        // EXTRACT units with no engine strftime code are rejected.
        for sql in [
            "EXTRACT(QUARTER FROM d)",
            "EXTRACT(WEEK FROM d)",
            "EXTRACT(YEAR_MONTH FROM d)",
        ] {
            assert!(
                parse_expr(sql).is_err(),
                "expected `{sql}` to be unsupported"
            );
        }
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
        // Modes with no exact engine equivalent are rejected.
        for mode in [1, 2, 4, 6, 7] {
            assert!(
                parse_expr(&format!("WEEK(d, {mode})")).is_err(),
                "WEEK mode {mode} should be unsupported"
            );
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

        // Calendar units have no fixed length and are rejected.
        for unit in ["MICROSECOND", "MONTH", "QUARTER", "YEAR"] {
            assert!(
                parse_expr(&format!("TIMESTAMPDIFF({unit}, a, b)")).is_err(),
                "TIMESTAMPDIFF unit {unit} should be unsupported"
            );
        }
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

        // The 3-argument LOCATE(substr, str, pos) form searches from `pos`,
        // lowering to a guarded CASE over an offset instr(); INSTR stays 2-arg.
        assert!(matches!(
            parse_expr("LOCATE('a', 'banana', 3)").unwrap(),
            ast::Expr::Case { .. }
        ));
        assert!(parse_expr("INSTR('banana', 'a', 3)").is_err());
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
    fn advisory_locks_fold_to_one() {
        // GET_LOCK / RELEASE_LOCK fold to the literal 1 regardless of arguments.
        for sql in ["GET_LOCK('x', 0)", "GET_LOCK('x', 10)", "RELEASE_LOCK('x')"] {
            assert!(
                matches!(parse_expr(sql).unwrap(), ast::Expr::Literal(ast::Literal::Numeric(n)) if n == "1"),
                "expected `{sql}` to fold to 1"
            );
        }
    }

    #[test]
    fn field_lowers_to_case() {
        // FIELD(x, a, b) -> CASE x WHEN a THEN 1 WHEN b THEN 2 ELSE 0 END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("FIELD(id, 3, 1, 2)").unwrap()
        else {
            panic!("expected FIELD to lower to a CASE");
        };
        assert_eq!(base.as_deref(), Some(&col("id")));
        assert_eq!(when_then_pairs.len(), 3);
        // The THEN results are the 1-based indices.
        for (i, (_, then)) in when_then_pairs.iter().enumerate() {
            assert_eq!(**then, num(&(i + 1).to_string()));
        }
        assert_eq!(else_expr.as_deref(), Some(&num("0")));
    }

    #[test]
    fn elt_lowers_to_case_without_else() {
        // ELT(n, a, b, c) -> CASE n WHEN 1 THEN a WHEN 2 THEN b WHEN 3 THEN c END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("ELT(n, 'a', 'b', 'c')").unwrap()
        else {
            panic!("expected ELT to lower to a CASE");
        };
        assert_eq!(base.as_deref(), Some(&col("n")));
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

        // `~` (unsigned in MySQL, signed here) is not modeled.
        assert!(parse_expr("~a").is_err());
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
        // CHAR(72, 73) -> char(72, 73).
        let ast::Expr::FunctionCall { name, args, .. } = parse_expr("CHAR(72, 73)").unwrap() else {
            panic!("expected CHAR to lower to a function call");
        };
        assert_eq!(name.as_str(), "char");
        assert_eq!(args.len(), 2);
        assert_eq!(*args[0], num("72"));

        // The `USING charset` form is rejected.
        assert!(parse_expr("CHAR(72 USING utf8)").is_err());
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
        // CONVERT(expr, type) is the same as CAST(expr AS type).
        let ast::Expr::Cast { type_name, .. } = parse_expr("CONVERT(a, SIGNED)").unwrap() else {
            panic!("expected CONVERT(expr, type) to parse as a Cast");
        };
        assert_eq!(type_name.unwrap().name, "INTEGER");
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
    }

    #[test]
    fn repeat_lowers_to_zeroblob_replace_with_null_guard() {
        // REPEAT(s, n) -> CASE WHEN n IS NULL THEN NULL
        //                      ELSE replace(hex(zeroblob(n)), '00', s) END.
        let ast::Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } = parse_expr("REPEAT('ab', n)").unwrap()
        else {
            panic!("expected REPEAT to lower to a CASE");
        };
        assert!(base.is_none(), "searched CASE, no base expression");

        // The single WHEN guards a NULL count: `n IS NULL` -> NULL.
        assert_eq!(when_then_pairs.len(), 1);
        assert_eq!(*when_then_pairs[0].0, ast::Expr::is_null(col("n")));
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
        // Both LPAD and RPAD lower to an outer substr(..., 1, len) over a
        // concatenation involving REPEAT(pad, len).
        for sql in ["LPAD(s, n, p)", "RPAD(s, n, p)"] {
            let ast::Expr::FunctionCall { name, args, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to lower to a function call");
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
    fn collate_clause_is_parsed_and_dropped() {
        // `expr COLLATE name` parses to just `expr` (collation is not honored).
        assert_eq!(parse_expr("a COLLATE utf8mb4_bin").unwrap(), col("a"));
        // COLLATE binds tighter than arithmetic: `a + b COLLATE x` is `a + b`.
        assert_eq!(
            parse_expr("a + b COLLATE utf8mb4_general_ci").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Add, col("b"))
        );
    }

    #[test]
    fn binary_operator_is_parsed_and_dropped() {
        // `BINARY expr` parses to just `expr` (the engine is binary already), on
        // either operand, and composes with a trailing COLLATE.
        assert_eq!(parse_expr("BINARY a").unwrap(), col("a"));
        // Composes with a trailing COLLATE (both dropped).
        assert_eq!(
            parse_expr("BINARY a COLLATE utf8mb4_bin").unwrap(),
            col("a")
        );
        assert_eq!(
            parse_expr("BINARY a = b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Equals, col("b"))
        );
        assert_eq!(
            parse_expr("a = BINARY b").unwrap(),
            ast::Expr::binary(col("a"), ast::Operator::Equals, col("b"))
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
        // The divergent operators `/`, `%`, `||` are intentionally not parsed.
        for input in ["a / b", "a % b", "a || b"] {
            let mut p = Parser::new(input.as_bytes()).unwrap();
            let fully_parsed = p.expr().is_ok() && p.peek().is_none();
            assert!(!fully_parsed, "expected `{input}` to be rejected");
        }
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
        for input in [
            "REPLACE(s, '-', '_')",
            "SUBSTR(s, 2, 3)",
            "TRIM(s)",
            "LTRIM(s)",
            "RTRIM(s)",
            "CONCAT_WS('-', a, b)",
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

        // SUM/MIN/MAX also accept DISTINCT; ALL is the default and elided.
        for input in ["SUM(DISTINCT v)", "MIN(DISTINCT v)", "MAX(DISTINCT v)"] {
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
            "SLEEP(1)",
            "ROUND(2.7)",
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
        for (input, engine) in [
            ("SUBSTRING('s', 1, 2)", "substr"),
            ("MID('s', 1, 2)", "substr"),
            ("LCASE('S')", "lower"),
            ("UCASE('s')", "upper"),
            ("CHAR_LENGTH('s')", "length"),
            ("CHARACTER_LENGTH('s')", "length"),
            ("GREATEST(1, 2, 3)", "max"),
            ("LEAST(1, 2, 3)", "min"),
            ("DATE('2020-01-01 10:00')", "date"),
            ("TIME('2020-01-01 10:00')", "time"),
            ("TIMESTAMP('2020-01-01')", "datetime"),
            ("LAST_INSERT_ID()", "last_insert_rowid"),
        ] {
            let ast::Expr::FunctionCall { name, .. } = parse_expr(input).unwrap() else {
                panic!("expected `{input}` to parse as a function call");
            };
            assert_eq!(name.as_str().to_ascii_lowercase(), engine, "{input}");
        }
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
            "UPDATE a, b SET a.x = 1",
            // ORDER BY (the engine cannot order an UPDATE) and a LIMIT with an
            // offset stay rejected.
            "UPDATE t SET a = 1 ORDER BY a",
            "UPDATE t SET a = 1 LIMIT 1, 2",
            "UPDATE IGNORE t SET a = 1",
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
            // ORDER BY (unorderable) and an offset on the LIMIT stay rejected.
            "DELETE FROM t ORDER BY a",
            "DELETE FROM t LIMIT 1, 2",
            "DELETE QUICK FROM t",
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
    fn drop_table_unsupported_variants() {
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
