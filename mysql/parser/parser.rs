// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A recursive-descent parser for the MySQL dialect.
//!
//! On success the parser emits a [`turso_parser::ast::Stmt`], so downstream code
//! can reuse the engine's AST, optimizer, and SQL renderer. Unsupported
//! constructs are reported as [`ParseError::Unsupported`].

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
                self.insert()
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
            // Recognized statement keywords that are simply not implemented yet.
            "REPLACE" | "ALTER" | "TRUNCATE" | "RENAME" | "SET" | "SHOW" | "USE" | "DESCRIBE"
            | "DESC" | "EXPLAIN" | "SAVEPOINT" | "GRANT" | "REVOKE" | "CALL" | "DO" | "WITH"
            | "VALUES" | "TABLE" | "PREPARE" | "EXECUTE" | "DEALLOCATE" | "LOCK" | "UNLOCK"
            | "ANALYZE" | "OPTIMIZE" | "CHECK" | "REPAIR" | "FLUSH" | "KILL" | "LOAD"
            | "HANDLER" | "IMPORT" => Err(ParseError::Unsupported(format!(
                "{keyword} is not supported yet"
            ))),
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
    fn insert(&mut self) -> Result<ast::Stmt> {
        // `INSERT` has already been consumed.
        for modifier in ["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE"] {
            if self.is_keyword(modifier) {
                return Err(ParseError::Unsupported(format!(
                    "INSERT {modifier} is not supported yet"
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

        let upsert = if self.eat_keyword("ON") {
            Some(Box::new(self.on_duplicate_key_update()?))
        } else {
            None
        };

        Ok(ast::Stmt::Insert {
            with: None,
            or_conflict: None,
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

        let order_by = self.order_by()?;
        let limit = self.limit()?;

        if self.is_keyword("UNION") || self.is_keyword("INTERSECT") || self.is_keyword("EXCEPT") {
            return Err(ParseError::Unsupported(
                "set operations (UNION/INTERSECT/EXCEPT) are not supported yet".to_string(),
            ));
        }
        if self.is_keyword("INTO") {
            return Err(ParseError::Unsupported(
                "SELECT ... INTO is not supported yet".to_string(),
            ));
        }

        Ok(ast::Stmt::Select(ast::Select {
            with: None,
            body: ast::SelectBody {
                select: ast::OneSelect::Select {
                    distinctness,
                    columns,
                    from,
                    where_clause,
                    group_by,
                    window_clause: Vec::new(),
                },
                compounds: Vec::new(),
            },
            order_by,
            limit,
        }))
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
            return Ok(Some(ast::As::As(self.name()?)));
        }
        match self.peek() {
            Some(Token::QuotedIdent(_)) => Ok(Some(ast::As::Elided(self.name()?))),
            Some(Token::Word(w)) if !is_reserved_select_alias(w) => {
                Ok(Some(ast::As::Elided(self.name()?)))
            }
            _ => Ok(None),
        }
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
        if self.is(&Token::LParen) {
            return Err(ParseError::Unsupported(
                "SELECT from a subquery / derived table is not supported yet".to_string(),
            ));
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
        if not {
            return Err(self.unexpected("`IN`, `BETWEEN`, or `LIKE` after `NOT`"));
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

    /// `expr [NOT] IN (v1, v2, ...)` — value lists only (not subqueries).
    fn in_list(&mut self, lhs: ast::Expr, not: bool) -> Result<ast::Expr> {
        self.expect(&Token::LParen, "`(`")?;
        if self.is_keyword("SELECT") {
            return Err(ParseError::Unsupported(
                "IN (SELECT ...) is not supported yet".to_string(),
            ));
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
        let mut lhs = self.primary_expr()?;
        while self.is(&Token::Star) {
            self.advance();
            let rhs = self.primary_expr()?;
            lhs = ast::Expr::binary(lhs, ast::Operator::Multiply, rhs);
        }
        Ok(lhs)
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

    /// Parses a function call `name(arg, ...)`. The name must be in the clean
    /// allow-list (functions whose MySQL semantics match SQLite/turso exactly);
    /// any other function is rejected as unsupported.
    fn function_call(&mut self) -> Result<ast::Expr> {
        let name = self.name()?;
        let upper = name.as_str().to_ascii_uppercase();
        self.expect(&Token::LParen, "`(`")?;

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

        // MySQL `IF` is the engine's `IIF`; they differ only in name.
        let name = if upper == "IF" {
            ast::Name::from_string("iif")
        } else {
            name
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
        // `IF` shares behaviour with the engine's `IIF` (it is renamed on emit).
        | "IF"
        // Aggregate functions.
        | "COUNT" | "SUM" | "MIN" | "MAX"
    )
}

/// The aggregate functions, which (unlike the scalar ones) accept a `DISTINCT`
/// quantifier. `upper_name` must already be uppercased.
fn is_aggregate_function(upper_name: &str) -> bool {
    matches!(upper_name, "COUNT" | "SUM" | "MIN" | "MAX")
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
            "SELECT * FROM a UNION SELECT * FROM b",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
        for input in [
            "CONCAT('a', 'b')",
            "LENGTH(name)",
            "NOW()",
            "ROUND(2.7)",
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
