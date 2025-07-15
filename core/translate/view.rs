use crate::schema::Schema;
use crate::translate::schema::{emit_schema_entry, SchemaEntryType, SQLITE_TABLEID};
use crate::util::normalize_ident;
use crate::vdbe::builder::{CursorType, ProgramBuilder};
use crate::vdbe::insn::Insn;
use crate::{Connection, Result};
use std::sync::{Arc, Mutex};
use tracing::info;
use turso_sqlite3_parser::ast::{self, fmt::ToTokens};

pub fn translate_create_view(
    schema: &Schema,
    view_name: &str,
    select_stmt: &ast::Select,
    connection: Arc<Connection>,
    mut program: ProgramBuilder,
) -> Result<ProgramBuilder> {
    let normalized_view_name = normalize_ident(view_name);

    // Check if view already exists
    if schema.get_view(&normalized_view_name).is_some() {
        return Err(crate::LimboError::ParseError(format!(
            "View {} already exists",
            normalized_view_name
        )));
    }

    // Reconstruct the SQL string
    let sql = create_view_to_str(view_name, select_stmt);

    // Open cursor to sqlite_schema table
    let table = schema.get_btree_table(SQLITE_TABLEID).unwrap();
    let sqlite_schema_cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(table.clone()));
    program.emit_insn(Insn::OpenWrite {
        cursor_id: sqlite_schema_cursor_id,
        root_page: 1usize.into(),
        name: view_name.to_string(),
    });

    // Add the view entry to sqlite_schema
    emit_schema_entry(
        &mut program,
        sqlite_schema_cursor_id,
        SchemaEntryType::View,
        &normalized_view_name,
        &normalized_view_name, // for views, tbl_name is same as name
        0,                     // views don't have a root page
        Some(sql),
    );

    // Parse schema to load the new view
    program.emit_insn(Insn::ParseSchema {
        db: sqlite_schema_cursor_id,
        where_clause: Some(format!("name = '{}'", normalized_view_name)),
    });

    // Create incremental view and subscribe to table changes
    // For now, we'll do a simple subscription to the table mentioned in the FROM clause
    // TODO: Parse the SELECT statement properly to extract table dependencies
    let base_table_name = extract_base_table_name(select_stmt).unwrap_or("t".to_string());

    // Create an incremental view
    let incremental_view = crate::incremental::view::IncrementalView::from_stmt(
        false, // temporary
        false, // if_not_exists
        ast::QualifiedName {
            db_name: None,
            name: ast::Name(normalized_view_name.clone()),
            alias: None,
        },
        None, // columns
        Box::new(select_stmt.clone()),
    )?;

    // Wrap it in Arc<Mutex<>> for thread safety
    let view_arc = Arc::new(Mutex::new(incremental_view));

    // Register the view with the table event registry using functional composition
    connection
        .table_event_registry
        .register_view(&base_table_name, view_arc);

    // Add logging to demonstrate incremental updates
    info!(
        "Created incremental view '{}' subscribed to table '{}'",
        normalized_view_name, base_table_name
    );

    program.epilogue(crate::translate::emitter::TransactionMode::Write);
    Ok(program)
}

fn create_view_to_str(view_name: &str, select_stmt: &ast::Select) -> String {
    format!(
        "CREATE VIEW {} AS {}",
        view_name,
        select_stmt.format().unwrap()
    )
}

/// Extract the base table name from a SELECT statement
/// This is a simplified implementation - in practice, we'd need to handle joins, subqueries, etc.
fn extract_base_table_name(select_stmt: &ast::Select) -> Option<String> {
    // Try to get the table name from the FROM clause
    // This is a simplified version - real implementation would need more complex parsing
    if let ast::OneSelect::Select(select) = &*select_stmt.body.select {
        if let Some(from) = &select.from {
            if let Some(ast::SelectTable::Table(qualified_name, _, _)) = from.select.as_deref() {
                return Some(qualified_name.name.0.clone());
            }
        }
    }
    None
}
