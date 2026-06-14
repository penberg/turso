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
            // Recognized statement keywords that are simply not implemented yet.
            "ALTER" | "RENAME" | "SET" | "SHOW" | "USE" | "DESCRIBE" | "DESC" | "EXPLAIN"
            | "SAVEPOINT" | "GRANT" | "REVOKE" | "CALL" | "DO" | "WITH" | "VALUES" | "TABLE"
            | "PREPARE" | "EXECUTE" | "DEALLOCATE" | "LOCK" | "UNLOCK" | "ANALYZE" | "OPTIMIZE"
            | "CHECK" | "REPAIR" | "FLUSH" | "KILL" | "LOAD" | "HANDLER" | "IMPORT" => Err(
                ParseError::Unsupported(format!("{keyword} is not supported yet")),
            ),
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
            self.create_table(temporary)
        } else {
            let what = match self.peek() {
                Some(Token::Word(w)) => w.to_ascii_uppercase(),
                _ => "?".to_string(),
            };
            Err(ParseError::Unsupported(format!(
                "CREATE {what} is not supported yet (only CREATE TABLE is implemented)"
            )))
        }
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

        // Only the explicit column-list form is supported (not LIKE / AS SELECT).
        if !self.is(&Token::LParen) {
            return Err(ParseError::Unsupported(
                "CREATE TABLE without a column list (LIKE / AS SELECT)".to_string(),
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
        } else {
            let what = match self.peek() {
                Some(Token::Word(w)) => w.to_ascii_uppercase(),
                _ => "?".to_string(),
            };
            Err(ParseError::Unsupported(format!(
                "DROP {what} is not supported yet (only DROP TABLE is implemented)"
            )))
        }
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

    /// Parses the basic `INSERT INTO tbl [(cols)] VALUES (...)[, (...)]` form.
    /// `INSERT ... SELECT`, `INSERT ... SET`, `ON DUPLICATE KEY UPDATE`, and the
    /// priority/`IGNORE` modifiers are rejected as unsupported.
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
        for modifier in ["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE"] {
            if self.is_keyword(modifier) {
                return Err(ParseError::Unsupported(format!(
                    "{kw} {modifier} is not supported yet"
                )));
            }
        }

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

        if self.is_keyword("SET") {
            return Err(ParseError::Unsupported(
                "INSERT ... SET is not supported yet".to_string(),
            ));
        }

        // Only the VALUES / VALUE form is supported.
        if !(self.eat_keyword("VALUES") || self.eat_keyword("VALUE")) {
            if self.is_keyword("SELECT") || self.is(&Token::LParen) {
                return Err(ParseError::Unsupported(
                    "INSERT ... SELECT is not supported yet".to_string(),
                ));
            }
            return Err(self.unexpected("`VALUES`"));
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

    /// Parses a `SELECT` body (everything after the `SELECT` keyword), including
    /// any `UNION [ALL]` / `INTERSECT` / `EXCEPT` compounds and a trailing
    /// `ORDER BY` / `LIMIT` that applies to the whole result, into an
    /// `ast::Select`. Shared by the top-level statement and `IN`/`EXISTS`
    /// subqueries.
    fn parse_select(&mut self) -> Result<ast::Select> {
        let first = self.parse_one_select()?;

        // Set-operation compounds. Each branch starts a fresh `SELECT`; the
        // operators map straight onto the engine's identical semantics (`UNION`
        // and `INTERSECT`/`EXCEPT` deduplicate; `UNION ALL` does not).
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
            self.expect_keyword("SELECT")?;
            let select = self.parse_one_select()?;
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

        // `SQL_CALC_FOUND_ROWS` is a SELECT modifier that asks MySQL to remember
        // the unlimited row count for a later `FOUND_ROWS()`. It does not change
        // the rows this query returns, so it is consumed here; the server
        // recognizes it (from the SQL text) and maintains the count separately.
        self.eat_keyword("SQL_CALC_FOUND_ROWS");

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

    /// Parses the `FROM` clause: a table reference optionally followed by
    /// `[INNER] JOIN` / `LEFT [OUTER] JOIN` joins, each with an `ON` condition.
    /// These map identically onto the engine. Comma joins, `RIGHT`/`FULL`/
    /// `CROSS`/`NATURAL`/`STRAIGHT_JOIN`, `USING`, ON-less joins, and subqueries
    /// are rejected as unsupported.
    // Not a constructor: the `from_` prefix names the SQL `FROM` clause, so the
    // `wrong_self_convention` heuristic does not apply here.
    #[allow(clippy::wrong_self_convention)]
    fn from_clause(&mut self) -> Result<ast::FromClause> {
        let select = Box::new(self.table_ref()?);

        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "SELECT from multiple tables (comma join) is not supported yet".to_string(),
            ));
        }

        let mut joins = Vec::new();
        while let Some(operator) = self.join_operator()? {
            let table = Box::new(self.table_ref()?);
            let constraint = if self.eat_keyword("ON") {
                Some(ast::JoinConstraint::On(Box::new(self.expr()?)))
            } else if self.is_keyword("USING") {
                return Err(ParseError::Unsupported(
                    "JOIN ... USING is not supported yet".to_string(),
                ));
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
        if self.eat_keyword("JOIN") {
            return Ok(Some(ast::JoinOperator::TypedJoin(Some(
                ast::JoinType::INNER,
            ))));
        }
        for kw in ["RIGHT", "FULL", "CROSS", "NATURAL", "STRAIGHT_JOIN"] {
            if self.is_keyword(kw) {
                return Err(ParseError::Unsupported(format!(
                    "{kw} join is not supported yet"
                )));
            }
        }
        Ok(None)
    }

    /// Parses an optional `GROUP BY [HAVING]` clause.
    ///
    /// GROUP BY terms must be column expressions, not integer ordinals: MySQL
    /// treats `GROUP BY 1` as "the first output column", but SQLite treats it as
    /// the constant `1` (one group) — a divergence, so ordinals are rejected.
    /// `HAVING` is only accepted together with `GROUP BY`.
    fn group_by(&mut self) -> Result<Option<ast::GroupBy>> {
        if !self.eat_keyword("GROUP") {
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

        if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
            return Err(ParseError::Unsupported(
                "ORDER BY / LIMIT on UPDATE is not supported yet".to_string(),
            ));
        }

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

        // The multi-table form is `DELETE t1 FROM ...` — i.e. a table list
        // before `FROM`.
        if !self.is_keyword("FROM") {
            return Err(ParseError::Unsupported(
                "multi-table DELETE is not supported yet".to_string(),
            ));
        }
        self.expect_keyword("FROM")?;

        let tbl_name = self.qualified_name()?;
        if self.is(&Token::Comma) {
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

        if self.is_keyword("ORDER") || self.is_keyword("LIMIT") {
            return Err(ParseError::Unsupported(
                "ORDER BY / LIMIT on DELETE is not supported yet".to_string(),
            ));
        }

        Ok(ast::Stmt::Delete {
            with: None,
            tbl_name,
            indexed: None,
            where_clause,
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
        let mut lhs = self.and_expr()?;
        while self.eat_keyword("OR") {
            let rhs = self.and_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::Or, rhs);
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.not_expr()?;
        while self.eat_keyword("AND") {
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
        let lhs = self.additive_expr()?;

        // `IS [NOT] NULL`
        if self.eat_keyword("IS") {
            let not = self.eat_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(if not {
                ast::Expr::not_null(lhs)
            } else {
                ast::Expr::is_null(lhs)
            });
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
            return Ok(ast::Expr::like(
                lhs,
                not,
                ast::LikeOperator::Like,
                rhs,
                None,
            ));
        }
        // `REGEXP` and its synonym `RLIKE` map onto the engine's `REGEXP`
        // operator (the `regexp` function). The engine's regex is
        // case-sensitive, unlike MySQL's default case-insensitive REGEXP — a
        // documented divergence (see `mysql/COMPAT.md`).
        if self.eat_keyword("REGEXP") || self.eat_keyword("RLIKE") {
            let rhs = self.additive_expr()?;
            return Ok(ast::Expr::like(
                lhs,
                not,
                ast::LikeOperator::Regexp,
                rhs,
                None,
            ));
        }
        if not {
            return Err(self.unexpected("`IN`, `BETWEEN`, `LIKE`, or `REGEXP` after `NOT`"));
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
        let rhs = self.additive_expr()?;
        Ok(ast::Expr::binary(lhs, op, rhs))
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
            let rhs = self.multiplicative_expr()?;
            lhs = ast::Expr::binary(lhs, op, rhs);
        }
        Ok(lhs)
    }

    /// Multiplicative tier: `*` only. `/` and `%` are intentionally not parsed —
    /// their MySQL semantics differ from SQLite (float division, float modulo),
    /// so they produce a clean parse error rather than a wrong answer.
    fn multiplicative_expr(&mut self) -> Result<ast::Expr> {
        let mut lhs = self.collate_expr()?;
        while self.is(&Token::Star) {
            self.advance();
            let rhs = self.collate_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::Multiply, rhs);
        }
        Ok(lhs)
    }

    /// A primary expression optionally followed by a `COLLATE collation_name`
    /// postfix. MySQL's `COLLATE` overrides the collation used for comparison and
    /// sorting; the engine is effectively single-collation (binary), so the
    /// clause is parsed and discarded and the underlying value is unchanged.
    /// This is an intentional divergence (collation is not honored); see
    /// `mysql/COMPAT.md`. `COLLATE` binds tighter than the arithmetic operators,
    /// so it is applied here at the primary tier.
    fn collate_expr(&mut self) -> Result<ast::Expr> {
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
            // Parenthesized sub-expression. The wrapper node is kept so the
            // rendered SQL preserves the original grouping.
            Some(Token::LParen) => {
                self.advance();
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

        // MySQL's `LENGTH(x)` is a BYTE count. The engine's `length()` counts
        // characters, but `length()` of a BLOB counts bytes, so lower it to
        // `length(CAST(x AS BLOB))`.
        if upper == "LENGTH" {
            return self.length_call();
        }

        // MySQL date-part extractors (`YEAR`, `MONTH`, `DAY`, ...) lower to the
        // engine's `strftime()`, cast to an integer to match MySQL's numeric
        // return (no zero-padding).
        if let Some(fmt) = date_part_format(&upper) {
            return self.date_part_call(fmt);
        }

        // `DATE_ADD` / `DATE_SUB(x, INTERVAL n unit)` lower to the engine's
        // `datetime(x, '+n unit')` / `datetime(x, '-n unit')` modifier.
        if upper == "DATE_ADD" {
            return self.date_add_call(false);
        }
        if upper == "DATE_SUB" {
            return self.date_add_call(true);
        }

        // `DATE_FORMAT(x, fmt)` lowers to the engine's `strftime()` with the
        // format specifiers translated from MySQL to strftime spelling.
        if upper == "DATE_FORMAT" {
            return self.date_format_call();
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

    /// Parses the single argument of a `LENGTH(x)` call (the name and `(` are
    /// already consumed) and lowers it to `length(CAST(x AS BLOB))`. MySQL's
    /// `LENGTH` is a byte count; the engine's `length()` counts characters, but
    /// `length()` of a BLOB counts bytes, and casting to BLOB yields the value's
    /// UTF-8 byte sequence. `CHAR_LENGTH` (the character count) maps to the
    /// engine's `length()` directly elsewhere.
    fn length_call(&mut self) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
        let blob = ast::Expr::Cast {
            expr: Box::new(arg),
            type_name: Some(ast::Type {
                name: "BLOB".to_string(),
                size: None,
                array_dimensions: 0,
            }),
        };
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("length"),
            distinctness: None,
            args: vec![Box::new(blob)],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
    }

    /// Parses the single argument of a date-part extractor such as `YEAR(x)`
    /// (the name and `(` are already consumed) and lowers it to
    /// `CAST(strftime(fmt, x) AS INTEGER)`. The cast drops `strftime`'s
    /// zero-padding and string type so the result is an integer like MySQL's
    /// (e.g. `MONTH('2020-03-15')` is `3`, not `'03'`). NULL propagates.
    fn date_part_call(&mut self, fmt: &str) -> Result<ast::Expr> {
        let arg = self.expr()?;
        self.expect(&Token::RParen, "`)`")?;
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
    /// engine's `datetime(x, '<signed-n> <unit>')` modifier. Only an
    /// integer-literal interval value is supported; `WEEK` is expanded to days.
    /// `datetime()` returns `'YYYY-MM-DD HH:MM:SS'`, matching MySQL's result for
    /// a DATETIME argument.
    fn date_add_call(&mut self, subtract: bool) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        self.expect_keyword("INTERVAL")?;

        let negative = self.eat(&Token::Minus);
        let Some(Token::Num(n)) = self.peek() else {
            return Err(self.unexpected("an integer interval value"));
        };
        let value: i64 = n.parse().map_err(|_| {
            ParseError::Unsupported(
                "DATE_ADD / DATE_SUB INTERVAL value must be an integer literal".to_string(),
            )
        })?;
        self.advance();

        let Some(Token::Word(u)) = self.peek() else {
            return Err(self.unexpected("an interval unit"));
        };
        let unit = u.to_ascii_uppercase();
        self.advance();
        self.expect(&Token::RParen, "`)`")?;

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
                    "DATE_ADD / DATE_SUB with INTERVAL unit {other} is not supported yet"
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

    /// Parses `DATE_FORMAT(x, 'fmt')` (the name and `(` are already consumed)
    /// and lowers it to the engine's `strftime(translated_fmt, x)`. The format
    /// must be a string literal so its MySQL specifiers can be translated to
    /// strftime spelling at parse time (see [`translate_date_format`]).
    fn date_format_call(&mut self) -> Result<ast::Expr> {
        let target = self.expr()?;
        self.expect(&Token::Comma, "`,`")?;
        let Some(Token::Str(fmt)) = self.peek() else {
            return Err(self.unexpected("a string-literal DATE_FORMAT format"));
        };
        let fmt = fmt.clone();
        self.advance();
        self.expect(&Token::RParen, "`)`")?;

        let translated = translate_date_format(&fmt)?;
        Ok(ast::Expr::FunctionCall {
            name: ast::Name::from_string("strftime"),
            distinctness: None,
            args: vec![
                Box::new(ast::Expr::Literal(ast::Literal::String(requote(
                    &translated,
                )))),
                Box::new(target),
            ],
            order_by: Vec::new(),
            within_group: Vec::new(),
            filter_over: ast::FunctionTail {
                filter_clause: None,
                over_clause: None,
            },
        })
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
        | "REPLACE" | "SUBSTR" | "INSTR" | "TRIM"
        // Functions sharing behaviour with the engine under a different name;
        // renamed on emit (see `engine_function_name`).
        | "IF"
        | "SUBSTRING" | "MID" | "LCASE" | "UCASE" | "CHAR_LENGTH" | "CHARACTER_LENGTH"
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
        "HOUR" => "%H",
        "MINUTE" => "%M",
        "SECOND" => "%S",
        _ => return None,
    })
}

/// Translates a MySQL `DATE_FORMAT` format string into the engine's `strftime`
/// spelling. Only specifiers with a direct strftime equivalent are accepted;
/// `%i`/`%s` (MySQL minutes/seconds) become `%M`/`%S`, the shared codes
/// (`%Y %m %d %H`) and `%%` pass through, and literal characters are copied.
/// Any other specifier (e.g. `%M` month name, `%h`, `%p`, `%W`) is rejected so
/// the front-end never silently produces a different string than MySQL.
fn translate_date_format(mysql_fmt: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = mysql_fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str("%Y"),
            Some('m') => out.push_str("%m"),
            Some('d') => out.push_str("%d"),
            Some('H') => out.push_str("%H"),
            Some('i') => out.push_str("%M"),
            Some('s') => out.push_str("%S"),
            Some('%') => out.push_str("%%"),
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
    Ok(out)
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
        _ => return None,
    })
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
            "SELECT * FROM a, b",
            "SELECT * FROM a RIGHT JOIN b ON a.id = b.id",
            "SELECT * FROM a CROSS JOIN b",
            "SELECT * FROM a JOIN b USING (id)",
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
        for sql in [
            "INSERT INTO t SET a = 1",
            "INSERT INTO t SELECT * FROM u",
            "INSERT IGNORE INTO t VALUES (1)",
            "INSERT DELAYED INTO t VALUES (1)",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
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
        // Targets that diverge from the engine are rejected.
        for sql in ["CAST(a AS DATE)", "CAST(a AS DATETIME)", "CAST(a AS JSON)"] {
            assert!(
                matches!(parse_expr(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
        // A specifier without a strftime equivalent is rejected.
        assert!(matches!(
            parse_expr("DATE_FORMAT(d, '%W')").unwrap_err(),
            ParseError::Unsupported(_)
        ));
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
        // A non-literal interval value is rejected.
        assert!(parse_expr("DATE_ADD(d, INTERVAL x DAY)").is_err());
    }

    #[test]
    fn date_parts_lower_to_cast_strftime() {
        // YEAR(d) becomes CAST(strftime('%Y', d) AS INTEGER); same shape for the
        // other parts, differing only in the format code.
        for (sql, fmt) in [
            ("YEAR(d)", "'%Y'"),
            ("MONTH(d)", "'%m'"),
            ("DAY(d)", "'%d'"),
            ("HOUR(d)", "'%H'"),
            ("MINUTE(d)", "'%M'"),
            ("SECOND(d)", "'%S'"),
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
            "INSTR(s, 'x')",
            "TRIM(s)",
        ] {
            assert!(
                matches!(parse_expr(input).unwrap(), ast::Expr::FunctionCall { .. }),
                "expected `{input}` to parse as a function call"
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
        for input in ["SLEEP(1)", "ROUND(2.7)", "RAND()", "totally_made_up(1)"] {
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
            let ast::Expr::Like { op, .. } = parse_expr(sql).unwrap() else {
                panic!("expected `{sql}` to parse as a Like/Regexp expression");
            };
            assert_eq!(op, ast::LikeOperator::Regexp, "for `{sql}`");
        }
        let expr = parse_expr("name LIKE 'a%'").unwrap();
        let ast::Expr::Like { lhs, not, .. } = expr else {
            panic!("expected Like");
        };
        assert_eq!(*lhs, col("name"));
        assert!(!not);

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
            "UPDATE t SET a = 1 ORDER BY a",
            "UPDATE t SET a = 1 LIMIT 1",
            "UPDATE IGNORE t SET a = 1",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
    fn delete_unsupported_variants() {
        for sql in [
            "DELETE t1 FROM t1, t2 WHERE t1.id = t2.id",
            "DELETE FROM a, b",
            "DELETE FROM t USING u",
            "DELETE FROM t ORDER BY a",
            "DELETE FROM t LIMIT 1",
            "DELETE QUICK FROM t",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
            "DROP INDEX i ON t",
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
