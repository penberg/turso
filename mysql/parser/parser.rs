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
            // Recognized statement keywords that are simply not implemented yet.
            "REPLACE" | "ALTER" | "TRUNCATE" | "RENAME" | "SET" | "SHOW" | "USE" | "DESCRIBE"
            | "DESC" | "EXPLAIN" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
            | "GRANT" | "REVOKE" | "CALL" | "DO" | "WITH" | "VALUES" | "TABLE" | "PREPARE"
            | "EXECUTE" | "DEALLOCATE" | "LOCK" | "UNLOCK" | "ANALYZE" | "OPTIMIZE" | "CHECK"
            | "REPAIR" | "FLUSH" | "KILL" | "LOAD" | "HANDLER" | "IMPORT" => Err(
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

        let mut columns = Vec::new();
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

    fn column_def(&mut self) -> Result<ast::ColumnDefinition> {
        let col_name = self.name()?;
        let col_type = self.column_type()?;
        let constraints = self.column_constraints()?;
        Ok(ast::ColumnDefinition {
            col_name,
            col_type,
            constraints,
        })
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

    /// Parses zero or more inline column constraints.
    fn column_constraints(&mut self) -> Result<Vec<ast::NamedColumnConstraint>> {
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
        Ok(out)
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
        if self.is_keyword("TEMPORARY") {
            return Err(ParseError::Unsupported(
                "DROP TEMPORARY TABLE is not supported yet".to_string(),
            ));
        }
        if self.eat_keyword("TABLE") {
            self.drop_table()
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

    /// Parses the basic `DROP TABLE tbl_name` form. The `IF EXISTS`,
    /// `TEMPORARY`, multi-table, and `RESTRICT`/`CASCADE` variants are
    /// explicitly rejected as unsupported.
    fn drop_table(&mut self) -> Result<ast::Stmt> {
        // `DROP TABLE` has already been consumed.
        if self.is_keyword("IF") {
            return Err(ParseError::Unsupported(
                "DROP TABLE IF EXISTS is not supported yet".to_string(),
            ));
        }

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

        Ok(ast::Stmt::DropTable {
            if_exists: false,
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

        if self.is_keyword("ON") {
            return Err(ParseError::Unsupported(
                "INSERT ... ON DUPLICATE KEY UPDATE is not supported yet".to_string(),
            ));
        }

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
                None,
            ),
            returning: Vec::new(),
        })
    }

    // === SELECT ===

    /// Parses a basic single-table `SELECT`:
    ///
    /// ```text
    /// SELECT <list> [FROM <table>] [WHERE <expr>]
    ///        [ORDER BY <expr> [ASC|DESC], ...] [LIMIT <n> [OFFSET <m>]]
    /// ```
    ///
    /// JOINs, multiple tables, subqueries, `GROUP BY`/`HAVING`, `DISTINCT`,
    /// aggregates, set operations, and CTEs are rejected as unsupported.
    fn select(&mut self) -> Result<ast::Stmt> {
        // `SELECT` has already been consumed.
        if self.is_keyword("DISTINCT") || self.is_keyword("DISTINCTROW") {
            return Err(ParseError::Unsupported(
                "SELECT DISTINCT is not supported yet".to_string(),
            ));
        }
        self.eat_keyword("ALL"); // the default quantifier; accepted and ignored

        let columns = self.select_list()?;

        let from = if self.eat_keyword("FROM") {
            Some(self.from_single_table()?)
        } else {
            None
        };

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };

        if self.is_keyword("GROUP") {
            return Err(ParseError::Unsupported(
                "GROUP BY is not supported yet".to_string(),
            ));
        }
        if self.is_keyword("HAVING") {
            return Err(ParseError::Unsupported(
                "HAVING is not supported yet".to_string(),
            ));
        }

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
                    distinctness: None,
                    columns,
                    from,
                    where_clause,
                    group_by: None,
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
                let alias = if self.eat_keyword("AS") {
                    Some(ast::As::As(self.name()?))
                } else {
                    None
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

    /// Parses the `FROM` clause, restricted to a single table reference.
    fn from_single_table(&mut self) -> Result<ast::FromClause> {
        if self.is(&Token::LParen) {
            return Err(ParseError::Unsupported(
                "SELECT from a subquery / derived table is not supported yet".to_string(),
            ));
        }
        let tbl_name = self.qualified_name()?;
        let alias = if self.eat_keyword("AS") {
            Some(ast::As::As(self.name()?))
        } else {
            None
        };

        if self.is(&Token::Comma) {
            return Err(ParseError::Unsupported(
                "SELECT from multiple tables (comma join) is not supported yet".to_string(),
            ));
        }
        for join_kw in [
            "JOIN",
            "INNER",
            "LEFT",
            "RIGHT",
            "FULL",
            "CROSS",
            "NATURAL",
            "STRAIGHT_JOIN",
        ] {
            if self.is_keyword(join_kw) {
                return Err(ParseError::Unsupported(
                    "SELECT with JOIN is not supported yet".to_string(),
                ));
            }
        }

        Ok(ast::FromClause {
            select: Box::new(ast::SelectTable::Table(tbl_name, alias, None)),
            joins: Vec::new(),
        })
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
        let lhs = self.primary_expr()?;
        if self.eat_keyword("IS") {
            let not = self.eat_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(if not {
                ast::Expr::not_null(lhs)
            } else {
                ast::Expr::is_null(lhs)
            });
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
        let rhs = self.primary_expr()?;
        Ok(ast::Expr::binary(lhs, op, rhs))
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
                    // Anything else is a column reference.
                    _ => self.column_ref(),
                }
            }
            Some(Token::QuotedIdent(_)) => self.column_ref(),
            _ => Err(self.unexpected("an expression")),
        }
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
    fn select_unsupported_variants() {
        for sql in [
            "SELECT DISTINCT a FROM t",
            "SELECT * FROM a, b",
            "SELECT * FROM a JOIN b ON a.id = b.id",
            "SELECT * FROM (SELECT 1)",
            "SELECT a FROM t GROUP BY a",
            "SELECT a FROM t HAVING a > 1",
            "SELECT * FROM a UNION SELECT * FROM b",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
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
            "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
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
        // Function calls, IN, LIKE, BETWEEN are not part of the grammar yet:
        // they either fail outright or leave unconsumed input (which statement
        // parsers turn into a syntax error).
        for input in ["f(x)", "a IN (1, 2)", "a LIKE 'x%'", "a BETWEEN 1 AND 2"] {
            let mut p = Parser::new(input.as_bytes()).unwrap();
            let fully_parsed = p.expr().is_ok() && p.peek().is_none();
            assert!(!fully_parsed, "expected `{input}` to be rejected");
        }
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
            "DROP TABLE IF EXISTS t",
            "DROP TEMPORARY TABLE t",
            "DROP TABLE a, b",
            "DROP TABLE t RESTRICT",
            "DROP TABLE t CASCADE",
            "DROP DATABASE d",
            "DROP INDEX i ON t",
        ] {
            assert!(
                matches!(parse(sql).unwrap_err(), ParseError::Unsupported(_)),
                "expected `{sql}` to be unsupported"
            );
        }
    }
}
