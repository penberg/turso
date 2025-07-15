use super::dbsp::{Stream, ZSet};
use crate::{types::ImmutableRecord, LimboError, Result};
use fallible_iterator::FallibleIterator;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use turso_sqlite3_parser::{
    ast::{Cmd, Stmt},
    lexer::sql::Parser,
};

/// A simplified WHERE clause predicate for demonstration
#[derive(Debug, Clone)]
pub enum WherePredicate {
    /// Column > value (e.g., "x > 2")
    GreaterThan { column: String, value: i64 },
    /// Column < value (e.g., "x < 10")
    LessThan { column: String, value: i64 },
    /// Column = value (e.g., "x = 5")
    Equals { column: String, value: i64 },
    /// No WHERE clause (accept all)
    None,
}

/// Incremental view that maintains a stream of row keys using DBSP functional composition
/// The actual record data is stored separately and accessed via the row key
#[derive(Debug)]
pub struct IncrementalView {
    stream: Stream<i64>,
    name: String,
    // Store the actual record data separately, keyed by row_key
    pub records: HashMap<i64, ImmutableRecord>,
    // WHERE clause predicate for filtering
    pub where_predicate: WherePredicate,
}

impl IncrementalView {
    pub fn from_sql(sql: &str) -> Result<Self> {
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next()?;
        let cmd = cmd.expect("View is an empty statement");
        match cmd {
            Cmd::Stmt(Stmt::CreateView {
                temporary,
                if_not_exists,
                view_name,
                columns,
                select,
            }) => IncrementalView::from_stmt(temporary, if_not_exists, view_name, columns, select),
            _ => Err(LimboError::ParseError(format!(
                "View is not a CREATE VIEW statement: {}",
                sql
            ))),
        }
    }

    pub fn from_stmt(
        _temporary: bool,
        _if_not_exists: bool,
        view_name: turso_sqlite3_parser::ast::QualifiedName,
        _columns: Option<Vec<turso_sqlite3_parser::ast::IndexedColumn>>,
        select: Box<turso_sqlite3_parser::ast::Select>,
    ) -> Result<Self> {
        let name = view_name.name.0.clone();

        // Parse the WHERE clause from the SELECT statement
        let where_predicate = Self::parse_where_clause(&select);

        let initial_data = Vec::new(); // Empty for now
        Ok(Self::new_with_predicate(
            name,
            initial_data,
            where_predicate,
        ))
    }

    pub fn new(name: String, initial_data: Vec<(i64, ImmutableRecord)>) -> Self {
        Self::new_with_predicate(name, initial_data, WherePredicate::None)
    }

    pub fn new_with_predicate(
        name: String,
        initial_data: Vec<(i64, ImmutableRecord)>,
        where_predicate: WherePredicate,
    ) -> Self {
        let mut records = HashMap::new();
        let mut row_keys = Vec::new();

        for (row_key, record) in initial_data {
            records.insert(row_key, record);
            row_keys.push(row_key);
        }

        let zset = ZSet::from_items(row_keys);
        Self {
            stream: Stream::from_zset(zset),
            name,
            records,
            where_predicate,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn current_data(&self) -> Vec<(i64, ImmutableRecord)> {
        self.stream
            .to_vec()
            .into_iter()
            .filter_map(|row_key| {
                self.records
                    .get(&row_key)
                    .map(|record| (row_key, record.clone()))
            })
            .collect()
    }

    /// Apply incremental changes to the view using DBSP delta processing
    pub fn apply_delta(&mut self, delta: &ZSet<i64>) {
        self.stream.apply_delta(delta);
    }

    /// Parse WHERE clause from SELECT statement (simplified implementation)
    fn parse_where_clause(select: &turso_sqlite3_parser::ast::Select) -> WherePredicate {
        use turso_sqlite3_parser::ast::*;

        if let OneSelect::Select(select_stmt) = &*select.body.select {
            if let Some(where_clause) = &select_stmt.where_clause {
                return Self::parse_expr_predicate(where_clause);
            }
        }

        WherePredicate::None
    }

    /// Parse expression into a simple predicate (basic implementation)
    fn parse_expr_predicate(expr: &turso_sqlite3_parser::ast::Expr) -> WherePredicate {
        use turso_sqlite3_parser::ast::*;

        match expr {
            Expr::Binary(lhs, op, rhs) => {
                // Try to parse "column > value" pattern
                if let (Expr::Id(column_name), Expr::Literal(Literal::Numeric(value_str))) =
                    (&**lhs, &**rhs)
                {
                    if let Ok(value) = value_str.parse::<i64>() {
                        match op {
                            Operator::Greater => {
                                return WherePredicate::GreaterThan {
                                    column: column_name.0.clone(),
                                    value,
                                }
                            }
                            Operator::Less => {
                                return WherePredicate::LessThan {
                                    column: column_name.0.clone(),
                                    value,
                                }
                            }
                            Operator::Equals => {
                                return WherePredicate::Equals {
                                    column: column_name.0.clone(),
                                    value,
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }

        WherePredicate::None
    }

    /// Evaluate WHERE predicate against a record
    pub fn evaluate_predicate(&self, record: &ImmutableRecord) -> bool {
        match &self.where_predicate {
            WherePredicate::None => true,
            WherePredicate::GreaterThan { column, value } => {
                // For demonstration, assume single column 'x' with integer value
                // In a real implementation, this would parse the record based on schema
                if column == "x" {
                    self.extract_column_value(record, column)
                        .map_or(false, |v| v > *value)
                } else {
                    false
                }
            }
            WherePredicate::LessThan { column, value } => {
                if column == "x" {
                    self.extract_column_value(record, column)
                        .map_or(false, |v| v < *value)
                } else {
                    false
                }
            }
            WherePredicate::Equals { column, value } => {
                if column == "x" {
                    self.extract_column_value(record, column)
                        .map_or(false, |v| v == *value)
                } else {
                    false
                }
            }
        }
    }

    /// Extract column value from record (simplified for demo)
    fn extract_column_value(&self, record: &ImmutableRecord, _column: &str) -> Option<i64> {
        // For demonstration, assume the record contains a single integer value
        // In a real implementation, this would parse the record based on schema
        let payload = record.get_payload();
        if payload.len() >= 3 {
            // Simple assumption: the value is stored as a varint after the first byte
            // This is a very simplified implementation
            Some(payload[2] as i64)
        } else {
            None
        }
    }
}

/// Represents a change event in a table
#[derive(Debug, Clone)]
pub enum TableChangeEvent {
    Insert {
        table_name: String,
        row_key: i64,
        record: ImmutableRecord,
    },
    Delete {
        table_name: String,
        row_key: i64,
    },
}

/// A stream of change events for a specific table using DBSP stream composition
#[derive(Debug)]
pub struct TableEventStream {
    table_name: String,
    // Store connected views for direct functional composition
    connected_views: Vec<Arc<Mutex<IncrementalView>>>,
}

impl TableEventStream {
    pub fn new(table_name: String) -> Self {
        Self {
            table_name,
            connected_views: Vec::new(),
        }
    }

    /// Connect a view to this table stream using DBSP functional composition
    pub fn connect_view(&mut self, view: Arc<Mutex<IncrementalView>>) {
        self.connected_views.push(view);
    }

    /// Emit a change event and apply functional composition to connected views
    pub fn emit_change(&mut self, event: TableChangeEvent) {
        // Apply functional composition: table_stream.filter(where_predicate).map(transform_event).apply_to_views()
        for view_arc in &self.connected_views {
            if let Ok(mut view) = view_arc.lock() {
                // Apply DBSP filter operator based on WHERE clause
                let should_include = match &event {
                    TableChangeEvent::Insert { record, .. } => {
                        // Filter using WHERE clause predicate
                        view.evaluate_predicate(record)
                    }
                    TableChangeEvent::Delete { row_key, .. } => {
                        // For deletes, check if the row was previously in the view
                        view.records.contains_key(row_key)
                    }
                };

                if should_include {
                    // Transform table change event -> row key delta using DBSP operators
                    let (row_key, weight) = match &event {
                        TableChangeEvent::Insert { row_key, .. } => (*row_key, 1),
                        TableChangeEvent::Delete { row_key, .. } => (*row_key, -1),
                    };

                    // Apply the transformation as a delta to the view stream
                    let mut delta = ZSet::new();
                    delta.insert(row_key, weight);

                    // Store record data for inserts, remove for deletes
                    match &event {
                        TableChangeEvent::Insert {
                            row_key, record, ..
                        } => {
                            view.records.insert(*row_key, record.clone());
                            println!("INSERT to view '{}': row_key={}", view.name(), row_key);
                        }
                        TableChangeEvent::Delete { row_key, .. } => {
                            view.records.remove(row_key);
                            println!("DELETE from view '{}': row_key={}", view.name(), row_key);
                        }
                    }

                    // Apply delta using DBSP stream operators
                    view.apply_delta(&delta);
                } else {
                    // Filtered out.
                }
            }
        }
    }

    /// Get the number of connected views
    pub fn view_count(&self) -> usize {
        self.connected_views.len()
    }
}

/// Global registry of table event streams using DBSP functional composition
#[derive(Debug)]
pub struct TableEventRegistry {
    streams: Arc<Mutex<HashMap<String, Arc<Mutex<TableEventStream>>>>>,
    views: Arc<Mutex<HashMap<String, Arc<Mutex<IncrementalView>>>>>,
}

impl TableEventRegistry {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            views: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a table event stream for a given table (called during schema parsing)
    /// Returns existing stream if it already exists
    pub fn create_stream(&self, table_name: &str) -> Arc<Mutex<TableEventStream>> {
        let mut streams = self.streams.lock().unwrap();

        if let Some(existing_stream) = streams.get(table_name) {
            return existing_stream.clone();
        }

        let stream = Arc::new(Mutex::new(TableEventStream::new(table_name.to_string())));
        streams.insert(table_name.to_string(), stream.clone());
        stream
    }

    /// Get an existing table event stream (panics if none exists)
    pub fn get_stream(&self, table_name: &str) -> Arc<Mutex<TableEventStream>> {
        let streams = self.streams.lock().unwrap();

        streams.get(table_name).cloned().unwrap_or_else(|| {
            panic!(
                "No stream found for table '{}' - streams must be created during schema parsing",
                table_name
            )
        })
    }

    /// Register a view for a specific table using DBSP functional composition
    pub fn register_view(&self, table_name: &str, view: Arc<Mutex<IncrementalView>>) {
        {
            let mut views = self.views.lock().unwrap();
            views.insert(format!("{}_view", table_name), view.clone());
        }

        // Connect the view to the table stream using DBSP functional composition
        if let Ok(mut stream) = self.get_stream(table_name).lock() {
            stream.connect_view(view);
        } else {
            eprintln!("Failed to lock stream for table {}", table_name);
        }
    }

    /// Emit a change event to the appropriate table stream using DBSP functional composition
    pub fn emit_change(&self, event: TableChangeEvent) {
        let table_name = match &event {
            TableChangeEvent::Insert { table_name, .. } => table_name,
            TableChangeEvent::Delete { table_name, .. } => table_name,
        };

        // Emit to the table stream, which will functionally compose with connected views
        if let Ok(mut stream) = self.get_stream(table_name).lock() {
            stream.emit_change(event);
        } else {
            eprintln!("Failed to lock stream for table {}", table_name);
        }
    }
}

impl Default for TableEventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// Tests will be added later when the system is fully integrated
