// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Maps turso column types onto MySQL wire column metadata.
//!
//! A result column carries two hints from the engine: the *declared* type string
//! (`get_column_decltype`, e.g. `"BIGINT UNSIGNED"`, `"VARCHAR"`) for columns that
//! come straight from a table, and the normalized SQLite *affinity*
//! (`get_column_type_name`, one of `INTEGER`/`REAL`/`TEXT`/`BLOB`/`NUMERIC`) which
//! is always available, including for expression columns. We prefer the declared
//! type because it preserves MySQL distinctions the affinity collapses — `TINYINT`
//! vs `BIGINT`, signedness, `DECIMAL` — and fall back to the affinity otherwise.

use turso_core::{Numeric, Value};
use turso_mysql_protocol::constants::{collation, column_flags, column_type};
use turso_mysql_protocol::{BinaryValue, ColumnDefinition};

/// The wire attributes of a column type: the MySQL type code plus the metadata
/// fields a `ColumnDefinition41` carries alongside it.
struct WireType {
    code: u8,
    charset: u8,
    column_length: u32,
    decimals: u8,
    /// Type-intrinsic flags (e.g. `UNSIGNED_FLAG`, `BLOB_FLAG`). Combined with
    /// column-level flags such as `NOT_NULL_FLAG` by the caller.
    flags: u16,
}

/// `decimals` value for columns with no fixed scale (everything but DECIMAL/time).
const NOT_FIXED_DEC: u8 = 0x1f;

/// Builds the `ColumnDefinition41` for result column `idx` of `stmt`.
pub fn column_definition(stmt: &turso_core::Statement, idx: usize) -> ColumnDefinition {
    let name = stmt.get_column_name(idx).to_string();
    let table = stmt
        .get_column_table_name(idx)
        .map(|t| t.to_string())
        .unwrap_or_default();
    let decltype = stmt.get_column_decltype(idx);
    let affinity = stmt.get_column_type_name(idx);
    let wire = map_type(decltype.as_deref(), affinity.as_deref());
    ColumnDefinition {
        name,
        table,
        column_type: wire.code,
        charset: wire.charset,
        column_length: wire.column_length,
        flags: wire.flags,
        decimals: wire.decimals,
    }
}

/// Resolves a column to its wire type, preferring the declared type over the
/// affinity. `decltype` is the raw declared string (e.g. `"BIGINT UNSIGNED"`);
/// `affinity` is the SQLite affinity name.
fn map_type(decltype: Option<&str>, affinity: Option<&str>) -> WireType {
    if let Some(decl) = decltype {
        if let Some(wire) = map_decltype(decl) {
            return wire;
        }
    }
    map_affinity(affinity)
}

/// Maps a declared MySQL type string to its wire type. Trailing `UNSIGNED` /
/// `SIGNED` / `ZEROFILL` modifiers (folded into the name by the parser) are
/// stripped and reflected in the flags. Returns `None` for an unrecognized name
/// so the caller can fall back to the affinity.
fn map_decltype(decl: &str) -> Option<WireType> {
    let upper = decl.to_ascii_uppercase();
    let mut flags = 0u16;
    if upper.contains("UNSIGNED") {
        flags |= column_flags::UNSIGNED_FLAG;
    }
    if upper.contains("ZEROFILL") {
        flags |= column_flags::ZEROFILL_FLAG;
    }
    // The base name is the first word; modifiers and any size text follow it.
    let base = upper.split([' ', '(']).next().unwrap_or(&upper).trim();

    let wire = match base {
        "TINYINT" | "BOOL" | "BOOLEAN" => int_type(column_type::MYSQL_TYPE_TINY, 4, flags),
        "SMALLINT" => int_type(column_type::MYSQL_TYPE_SHORT, 6, flags),
        "MEDIUMINT" => int_type(column_type::MYSQL_TYPE_INT24, 9, flags),
        "INT" | "INTEGER" => int_type(column_type::MYSQL_TYPE_LONG, 11, flags),
        "BIGINT" => int_type(column_type::MYSQL_TYPE_LONGLONG, 20, flags),
        "FLOAT" => float_type(column_type::MYSQL_TYPE_FLOAT, 12, flags),
        "DOUBLE" | "REAL" => float_type(column_type::MYSQL_TYPE_DOUBLE, 22, flags),
        "DECIMAL" | "NUMERIC" | "DEC" | "FIXED" => WireType {
            code: column_type::MYSQL_TYPE_NEWDECIMAL,
            charset: collation::BINARY,
            column_length: 11,
            decimals: 0,
            flags: flags | column_flags::NUM_FLAG,
        },
        "YEAR" => int_type(column_type::MYSQL_TYPE_YEAR, 4, flags),
        "DATE" => temporal_type(column_type::MYSQL_TYPE_DATE, 10),
        "TIME" => temporal_type(column_type::MYSQL_TYPE_TIME, 10),
        "DATETIME" => temporal_type(column_type::MYSQL_TYPE_DATETIME, 19),
        "TIMESTAMP" => WireType {
            flags: column_flags::TIMESTAMP_FLAG,
            ..temporal_type(column_type::MYSQL_TYPE_TIMESTAMP, 19)
        },
        "CHAR" | "CHARACTER" => text_type(column_type::MYSQL_TYPE_STRING),
        "VARCHAR" | "VARYING" => text_type(column_type::MYSQL_TYPE_VAR_STRING),
        "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "CLOB" => {
            text_type(column_type::MYSQL_TYPE_BLOB)
        }
        "JSON" => text_type(column_type::MYSQL_TYPE_JSON),
        "ENUM" => WireType {
            flags: column_flags::ENUM_FLAG,
            ..text_type(column_type::MYSQL_TYPE_STRING)
        },
        "SET" => WireType {
            flags: column_flags::SET_FLAG,
            ..text_type(column_type::MYSQL_TYPE_STRING)
        },
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "BYTEA" => {
            blob_type()
        }
        _ => return None,
    };
    Some(wire)
}

/// Maps a SQLite affinity name to a wire type, used for expression columns where
/// no declared type exists. Affinity loses signedness and width, so integers map
/// to `LONGLONG` and reals to `DOUBLE`.
fn map_affinity(affinity: Option<&str>) -> WireType {
    match affinity {
        Some("INTEGER") => int_type(column_type::MYSQL_TYPE_LONGLONG, 20, 0),
        Some("REAL") => float_type(column_type::MYSQL_TYPE_DOUBLE, 22, 0),
        Some("BLOB") => blob_type(),
        Some("NUMERIC") => WireType {
            code: column_type::MYSQL_TYPE_NEWDECIMAL,
            charset: collation::BINARY,
            column_length: 11,
            decimals: 0,
            flags: column_flags::NUM_FLAG,
        },
        // "TEXT", None, and anything unexpected: a text string is the safe default.
        _ => text_type(column_type::MYSQL_TYPE_VAR_STRING),
    }
}

fn int_type(code: u8, column_length: u32, flags: u16) -> WireType {
    WireType {
        code,
        charset: collation::BINARY,
        column_length,
        decimals: 0,
        flags: flags | column_flags::NUM_FLAG,
    }
}

fn float_type(code: u8, column_length: u32, flags: u16) -> WireType {
    WireType {
        code,
        charset: collation::BINARY,
        column_length,
        decimals: NOT_FIXED_DEC,
        flags: flags | column_flags::NUM_FLAG,
    }
}

fn temporal_type(code: u8, column_length: u32) -> WireType {
    WireType {
        code,
        charset: collation::BINARY,
        column_length,
        decimals: 0,
        flags: 0,
    }
}

fn text_type(code: u8) -> WireType {
    WireType {
        code,
        charset: collation::UTF8MB4_GENERAL_CI,
        column_length: 65535,
        decimals: NOT_FIXED_DEC,
        flags: 0,
    }
}

fn blob_type() -> WireType {
    WireType {
        code: column_type::MYSQL_TYPE_BLOB,
        charset: collation::BINARY,
        column_length: 65535,
        decimals: NOT_FIXED_DEC,
        flags: column_flags::BLOB_FLAG | column_flags::BINARY_FLAG,
    }
}

/// Converts an engine value into its binary-protocol form for a result column
/// of the given MySQL type code.
///
/// Numbers and strings map directly. Temporal columns are special: the engine
/// stores dates and times as text (SQLite has no native temporal type), so we
/// parse the canonical string form into the structured value the binary
/// protocol expects. A value that does not parse falls back to its raw bytes.
pub fn value_to_binary(value: &Value, type_code: u8) -> BinaryValue {
    use column_type::*;
    match value {
        Value::Null => BinaryValue::Null,
        Value::Numeric(Numeric::Integer(i)) => BinaryValue::I64(*i),
        Value::Numeric(Numeric::Float(f)) => BinaryValue::F64(f64::from(*f)),
        Value::Blob(b) => BinaryValue::Bytes(b.clone()),
        Value::Text(t) => {
            let s = t.as_str();
            match type_code {
                MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
                    parse_datetime(s).unwrap_or_else(|| BinaryValue::Bytes(s.as_bytes().to_vec()))
                }
                MYSQL_TYPE_TIME => {
                    parse_time(s).unwrap_or_else(|| BinaryValue::Bytes(s.as_bytes().to_vec()))
                }
                _ => BinaryValue::Bytes(s.as_bytes().to_vec()),
            }
        }
    }
}

/// Converts a decoded binary-protocol parameter into an engine value for
/// binding. Integers and floats map directly; everything else (strings, blobs,
/// and temporal values) binds as text or a blob, matching how the engine stores
/// such values.
pub fn binary_to_value(value: BinaryValue) -> Value {
    match value {
        BinaryValue::Null => Value::Null,
        BinaryValue::I64(i) => Value::from_i64(i),
        // The engine has no unsigned 64-bit type; values that overflow i64 are
        // bound as their decimal text so no information is lost.
        BinaryValue::U64(u) => match i64::try_from(u) {
            Ok(i) => Value::from_i64(i),
            Err(_) => Value::build_text(u.to_string()),
        },
        BinaryValue::F64(f) => Value::from_f64(f),
        BinaryValue::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => Value::build_text(s),
            Err(e) => Value::Blob(e.into_bytes()),
        },
        BinaryValue::DateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micros,
        } => {
            let date = format!("{year:04}-{month:02}-{day:02}");
            let text = if hour != 0 || minute != 0 || second != 0 || micros != 0 {
                let base = format!("{date} {hour:02}:{minute:02}:{second:02}");
                if micros != 0 {
                    format!("{base}.{micros:06}")
                } else {
                    base
                }
            } else {
                date
            };
            Value::build_text(text)
        }
        BinaryValue::Time {
            negative,
            days,
            hour,
            minute,
            second,
            micros,
        } => {
            let total_hours = days * 24 + hour as u32;
            let sign = if negative { "-" } else { "" };
            let base = format!("{sign}{total_hours:02}:{minute:02}:{second:02}");
            let text = if micros != 0 {
                format!("{base}.{micros:06}")
            } else {
                base
            };
            Value::build_text(text)
        }
    }
}

/// Parses a `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` string into binary date/time
/// components. Returns `None` if the leading date does not parse.
fn parse_datetime(s: &str) -> Option<BinaryValue> {
    let s = s.trim();
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse().ok()?;
    let month = date_parts.next()?.parse().ok()?;
    let day = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }
    let (hour, minute, second, micros) = match time {
        Some(t) => parse_hms(t)?,
        None => (0, 0, 0, 0),
    };
    Some(BinaryValue::DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    })
}

/// Parses an `HH:MM:SS[.ffffff]` string into a binary `TIME` value.
fn parse_time(s: &str) -> Option<BinaryValue> {
    let s = s.trim();
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (hours, minute, second, micros) = parse_hms_wide(rest)?;
    Some(BinaryValue::Time {
        negative,
        days: hours / 24,
        hour: (hours % 24) as u8,
        minute,
        second,
        micros,
    })
}

/// Parses `HH:MM:SS[.ffffff]` where the hour is a single clock hour (0–23).
fn parse_hms(t: &str) -> Option<(u8, u8, u8, u32)> {
    let (hours, minute, second, micros) = parse_hms_wide(t)?;
    Some((hours as u8, minute, second, micros))
}

/// Parses `H[H..]:MM:SS[.ffffff]`, allowing an hour field wider than a day (as
/// MySQL `TIME` permits), and returns `(hours, minute, second, micros)`.
fn parse_hms_wide(t: &str) -> Option<(u32, u8, u8, u32)> {
    let (sec_field, micros) = match t.split_once('.') {
        Some((head, frac)) => {
            // Pad/truncate the fraction to exactly six digits of microseconds.
            let mut digits = frac.bytes().take_while(u8::is_ascii_digit).count();
            digits = digits.min(6);
            let mut micros: u32 = frac.get(..digits)?.parse().ok()?;
            for _ in digits..6 {
                micros *= 10;
            }
            (head, micros)
        }
        None => (t, 0),
    };
    let mut parts = sec_field.split(':');
    let hours = parts.next()?.parse().ok()?;
    let minute = parts.next()?.parse().ok()?;
    let second = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((hours, minute, second, micros))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(decl: &str) -> u8 {
        map_type(Some(decl), None).code
    }

    #[test]
    fn maps_integer_widths() {
        assert_eq!(code_of("TINYINT"), column_type::MYSQL_TYPE_TINY);
        assert_eq!(code_of("SMALLINT"), column_type::MYSQL_TYPE_SHORT);
        assert_eq!(code_of("MEDIUMINT"), column_type::MYSQL_TYPE_INT24);
        assert_eq!(code_of("INT"), column_type::MYSQL_TYPE_LONG);
        assert_eq!(code_of("INTEGER"), column_type::MYSQL_TYPE_LONG);
        assert_eq!(code_of("BIGINT"), column_type::MYSQL_TYPE_LONGLONG);
    }

    #[test]
    fn unsigned_modifier_sets_flag() {
        let wire = map_type(Some("BIGINT UNSIGNED"), None);
        assert_eq!(wire.code, column_type::MYSQL_TYPE_LONGLONG);
        assert_ne!(wire.flags & column_flags::UNSIGNED_FLAG, 0);
    }

    #[test]
    fn maps_string_and_blob_families() {
        assert_eq!(code_of("VARCHAR"), column_type::MYSQL_TYPE_VAR_STRING);
        assert_eq!(code_of("CHAR"), column_type::MYSQL_TYPE_STRING);
        assert_eq!(code_of("TEXT"), column_type::MYSQL_TYPE_BLOB);
        assert_eq!(code_of("JSON"), column_type::MYSQL_TYPE_JSON);

        let blob = map_type(Some("BLOB"), None);
        assert_eq!(blob.code, column_type::MYSQL_TYPE_BLOB);
        assert_eq!(blob.charset, collation::BINARY);
        assert_ne!(blob.flags & column_flags::BLOB_FLAG, 0);
    }

    #[test]
    fn maps_decimal_and_temporal() {
        assert_eq!(code_of("DECIMAL"), column_type::MYSQL_TYPE_NEWDECIMAL);
        assert_eq!(code_of("DATE"), column_type::MYSQL_TYPE_DATE);
        assert_eq!(code_of("DATETIME"), column_type::MYSQL_TYPE_DATETIME);
        assert_eq!(code_of("TIMESTAMP"), column_type::MYSQL_TYPE_TIMESTAMP);
    }

    #[test]
    fn falls_back_to_affinity_for_unknown_and_expression_columns() {
        // Unknown declared type → affinity wins.
        assert_eq!(
            map_type(Some("GEOMETRY"), Some("TEXT")).code,
            column_type::MYSQL_TYPE_VAR_STRING
        );
        // No declared type (expression column) → affinity drives the mapping.
        assert_eq!(
            map_type(None, Some("INTEGER")).code,
            column_type::MYSQL_TYPE_LONGLONG
        );
        assert_eq!(
            map_type(None, Some("REAL")).code,
            column_type::MYSQL_TYPE_DOUBLE
        );
        // Nothing at all → text default.
        assert_eq!(
            map_type(None, None).code,
            column_type::MYSQL_TYPE_VAR_STRING
        );
    }
}
