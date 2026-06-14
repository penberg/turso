// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! The binary (prepared-statement) protocol: `COM_STMT_PREPARE` responses,
//! `COM_STMT_EXECUTE` parameter decoding, and binary result rows.
//!
//! Where the text protocol renders every value as a string, the binary protocol
//! encodes each value according to its column type: integers as fixed-width
//! little-endian fields, floats as IEEE-754, strings/blobs as length-encoded
//! bytes, and temporal values as packed date/time structures. This module owns
//! those encodings in both directions; it stays free of any engine types, so a
//! caller maps its own value representation to and from [`BinaryValue`].

use crate::constants::{column_type, OK_HEADER};
use crate::error::{ProtocolError, Result};
use crate::packet::{PacketReader, PacketWriter};

/// A value as carried by the binary protocol, in either direction. Decoded from
/// `COM_STMT_EXECUTE` parameters and supplied for binary result rows. Temporal
/// values are kept as structured components so the caller need not know the
/// packed wire layout.
#[derive(Debug, Clone, PartialEq)]
pub enum BinaryValue {
    Null,
    /// A signed integer (any of the `TINY`..`LONGLONG` widths).
    I64(i64),
    /// An unsigned integer that does not fit in `i64`.
    U64(u64),
    /// A `FLOAT` or `DOUBLE`.
    F64(f64),
    /// A string, blob, decimal, or JSON value — anything length-encoded.
    Bytes(Vec<u8>),
    /// A `DATE`, `DATETIME`, or `TIMESTAMP`.
    DateTime {
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        micros: u32,
    },
    /// A `TIME` (a signed duration, not a point in time).
    Time {
        negative: bool,
        days: u32,
        hour: u8,
        minute: u8,
        second: u8,
        micros: u32,
    },
}

/// The `COM_STMT_PREPARE` success response: the statement id and the parameter
/// and column counts. The caller follows it with that many parameter- and
/// column-definition packets.
#[derive(Debug, Clone)]
pub struct StmtPrepareOk {
    pub statement_id: u32,
    pub num_columns: u16,
    pub num_params: u16,
    pub warning_count: u16,
}

impl StmtPrepareOk {
    /// Serializes the fixed `COM_STMT_PREPARE_OK` header.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(12);
        w.u8(OK_HEADER); // status: 0x00 = OK
        w.u32(self.statement_id);
        w.u16(self.num_columns);
        w.u16(self.num_params);
        w.u8(0x00); // reserved filler
        w.u16(self.warning_count);
        w.into_bytes()
    }
}

/// A decoded `COM_STMT_EXECUTE` command. Parameter values cannot be parsed until
/// the parameter count is known, so the variable-length tail is kept verbatim
/// and decoded by [`StmtExecute::parse_params`] once the caller looks the count
/// up from the prepared statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtExecute {
    pub statement_id: u32,
    pub flags: u8,
    /// Everything after the iteration count: the NULL bitmap, the
    /// new-parameters-bound flag, the parameter type list, and the values.
    pub params_tail: Vec<u8>,
}

impl StmtExecute {
    /// Decodes the bound parameters, given the statement's parameter count.
    ///
    /// The high bit of each parameter's type flag marks it unsigned. A value
    /// flagged NULL in the bitmap contributes no bytes. Returns the values in
    /// parameter order, ready to bind.
    pub fn parse_params(&self, num_params: usize) -> Result<Vec<BinaryValue>> {
        if num_params == 0 {
            return Ok(Vec::new());
        }
        let mut r = PacketReader::new(&self.params_tail);
        let bitmap_len = num_params.div_ceil(8);
        let null_bitmap = r.bytes(bitmap_len)?.to_vec();
        let new_params_bound = r.u8()?;
        if new_params_bound != 1 {
            // The first execute always sends the types; a client that omits them
            // expects the server to have cached them from a previous execute,
            // which we do not do.
            return Err(ProtocolError::Malformed(
                "COM_STMT_EXECUTE without a parameter-type list is not supported".to_string(),
            ));
        }
        let mut types = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            let type_code = r.u8()?;
            let unsigned = r.u8()? & 0x80 != 0;
            types.push((type_code, unsigned));
        }
        let mut out = Vec::with_capacity(num_params);
        for (i, (type_code, unsigned)) in types.into_iter().enumerate() {
            if null_bitmap[i / 8] & (1 << (i % 8)) != 0 {
                out.push(BinaryValue::Null);
            } else {
                out.push(decode_value(&mut r, type_code, unsigned)?);
            }
        }
        Ok(out)
    }
}

/// Decodes one non-NULL binary parameter value of the given type.
fn decode_value(r: &mut PacketReader, type_code: u8, unsigned: bool) -> Result<BinaryValue> {
    use column_type::*;
    Ok(match type_code {
        MYSQL_TYPE_TINY => int_value(r.u8()? as u64, 1, unsigned),
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => int_value(r.u16()? as u64, 2, unsigned),
        MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 => int_value(r.u32()? as u64, 4, unsigned),
        MYSQL_TYPE_LONGLONG => {
            let raw = read_u64(r)?;
            int_value(raw, 8, unsigned)
        }
        MYSQL_TYPE_FLOAT => BinaryValue::F64(f32::from_le_bytes(read_array::<4>(r)?) as f64),
        MYSQL_TYPE_DOUBLE => BinaryValue::F64(f64::from_le_bytes(read_array::<8>(r)?)),
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => decode_datetime(r)?,
        MYSQL_TYPE_TIME => decode_time(r)?,
        // Everything else is length-encoded bytes: strings, blobs, decimals, JSON.
        _ => BinaryValue::Bytes(r.lenenc_bytes()?.to_vec()),
    })
}

/// Sign-extends or widens a little-endian integer of `width` bytes into a
/// [`BinaryValue`]. Unsigned values that overflow `i64` are kept as `U64`.
fn int_value(raw: u64, width: usize, unsigned: bool) -> BinaryValue {
    if unsigned {
        if raw > i64::MAX as u64 {
            BinaryValue::U64(raw)
        } else {
            BinaryValue::I64(raw as i64)
        }
    } else {
        // Sign-extend from the field's width.
        let bits = width * 8;
        let signed = if bits < 64 && raw & (1 << (bits - 1)) != 0 {
            (raw | (!0u64 << bits)) as i64
        } else {
            raw as i64
        };
        BinaryValue::I64(signed)
    }
}

/// Decodes a packed `DATE`/`DATETIME`/`TIMESTAMP`: a length byte (0, 4, 7, or
/// 11) selects which trailing fields are present.
fn decode_datetime(r: &mut PacketReader) -> Result<BinaryValue> {
    let len = r.u8()?;
    let (mut year, mut month, mut day) = (0u16, 0u8, 0u8);
    let (mut hour, mut minute, mut second, mut micros) = (0u8, 0u8, 0u8, 0u32);
    if len >= 4 {
        year = r.u16()?;
        month = r.u8()?;
        day = r.u8()?;
    }
    if len >= 7 {
        hour = r.u8()?;
        minute = r.u8()?;
        second = r.u8()?;
    }
    if len >= 11 {
        micros = r.u32()?;
    }
    Ok(BinaryValue::DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    })
}

/// Decodes a packed `TIME`: a length byte (0, 8, or 12) selects the fields.
fn decode_time(r: &mut PacketReader) -> Result<BinaryValue> {
    let len = r.u8()?;
    let (mut negative, mut days) = (false, 0u32);
    let (mut hour, mut minute, mut second, mut micros) = (0u8, 0u8, 0u8, 0u32);
    if len >= 8 {
        negative = r.u8()? != 0;
        days = r.u32()?;
        hour = r.u8()?;
        minute = r.u8()?;
        second = r.u8()?;
    }
    if len >= 12 {
        micros = r.u32()?;
    }
    Ok(BinaryValue::Time {
        negative,
        days,
        hour,
        minute,
        second,
        micros,
    })
}

fn read_array<const N: usize>(r: &mut PacketReader) -> Result<[u8; N]> {
    let bytes = r.bytes(N)?;
    let mut arr = [0u8; N];
    arr.copy_from_slice(bytes);
    Ok(arr)
}

fn read_u64(r: &mut PacketReader) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array::<8>(r)?))
}

/// Encodes one binary result row. `cells` pairs each column's MySQL type code
/// with its value; `BinaryValue::Null` marks SQL NULL.
///
/// A binary row opens with a `0x00` header and a NULL bitmap whose bit for
/// column `i` lives at bit `i + 2` (the first two bits are reserved). Non-NULL
/// values follow in column order, each encoded per its type code.
pub fn encode_binary_row(cells: &[(u8, BinaryValue)]) -> Vec<u8> {
    let mut w = PacketWriter::with_capacity(16 + cells.len() * 4);
    w.u8(0x00);

    let bitmap_len = (cells.len() + 2).div_ceil(8);
    let mut bitmap = vec![0u8; bitmap_len];
    for (i, (_, value)) in cells.iter().enumerate() {
        if matches!(value, BinaryValue::Null) {
            let bit = i + 2;
            bitmap[bit / 8] |= 1 << (bit % 8);
        }
    }
    w.bytes(&bitmap);

    for (type_code, value) in cells {
        if matches!(value, BinaryValue::Null) {
            continue;
        }
        encode_value(&mut w, *type_code, value);
    }
    w.into_bytes()
}

/// Encodes one non-NULL value according to its column type code. The value is
/// coerced toward the column type where they disagree, so a stringly-typed
/// value in a numeric column still serializes sensibly.
fn encode_value(w: &mut PacketWriter, type_code: u8, value: &BinaryValue) {
    use column_type::*;
    match type_code {
        MYSQL_TYPE_TINY => {
            w.u8(as_i64(value) as u8);
        }
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => {
            w.u16(as_i64(value) as u16);
        }
        MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 => {
            w.u32(as_i64(value) as u32);
        }
        MYSQL_TYPE_LONGLONG => {
            let raw = match value {
                BinaryValue::U64(u) => *u,
                other => as_i64(other) as u64,
            };
            w.bytes(&raw.to_le_bytes());
        }
        MYSQL_TYPE_FLOAT => {
            w.bytes(&(as_f64(value) as f32).to_le_bytes());
        }
        MYSQL_TYPE_DOUBLE => {
            w.bytes(&as_f64(value).to_le_bytes());
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            encode_datetime(w, value);
        }
        MYSQL_TYPE_TIME => {
            encode_time(w, value);
        }
        // Strings, blobs, decimals, JSON: length-encoded bytes.
        _ => {
            w.lenenc_bytes(&as_bytes(value));
        }
    }
}

/// Writes a packed `DATETIME`, choosing the shortest length that preserves the
/// value (no time → 4, no micros → 7, otherwise 11).
fn encode_datetime(w: &mut PacketWriter, value: &BinaryValue) {
    let BinaryValue::DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    } = value
    else {
        // A non-temporal value in a temporal column: emit a zero date rather
        // than corrupt the framing.
        w.u8(0);
        return;
    };
    let has_time = *hour != 0 || *minute != 0 || *second != 0 || *micros != 0;
    let len = if *micros != 0 {
        11
    } else if has_time {
        7
    } else {
        4
    };
    w.u8(len);
    w.u16(*year);
    w.u8(*month);
    w.u8(*day);
    if len >= 7 {
        w.u8(*hour);
        w.u8(*minute);
        w.u8(*second);
    }
    if len >= 11 {
        w.u32(*micros);
    }
}

/// Writes a packed `TIME`.
fn encode_time(w: &mut PacketWriter, value: &BinaryValue) {
    let BinaryValue::Time {
        negative,
        days,
        hour,
        minute,
        second,
        micros,
    } = value
    else {
        w.u8(0);
        return;
    };
    let len = if *micros != 0 { 12 } else { 8 };
    w.u8(len);
    w.u8(*negative as u8);
    w.u32(*days);
    w.u8(*hour);
    w.u8(*minute);
    w.u8(*second);
    if len >= 12 {
        w.u32(*micros);
    }
}

fn as_i64(value: &BinaryValue) -> i64 {
    match value {
        BinaryValue::I64(i) => *i,
        BinaryValue::U64(u) => *u as i64,
        BinaryValue::F64(f) => *f as i64,
        BinaryValue::Bytes(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

fn as_f64(value: &BinaryValue) -> f64 {
    match value {
        BinaryValue::F64(f) => *f,
        BinaryValue::I64(i) => *i as f64,
        BinaryValue::U64(u) => *u as f64,
        BinaryValue::Bytes(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

fn as_bytes(value: &BinaryValue) -> Vec<u8> {
    match value {
        BinaryValue::Bytes(b) => b.clone(),
        BinaryValue::I64(i) => i.to_string().into_bytes(),
        BinaryValue::U64(u) => u.to_string().into_bytes(),
        BinaryValue::F64(f) => f.to_string().into_bytes(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_ok_layout() {
        let bytes = StmtPrepareOk {
            statement_id: 7,
            num_columns: 2,
            num_params: 1,
            warning_count: 0,
        }
        .encode();
        assert_eq!(bytes[0], OK_HEADER);
        assert_eq!(
            u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]),
            7
        );
        assert_eq!(u16::from_le_bytes([bytes[5], bytes[6]]), 2); // columns
        assert_eq!(u16::from_le_bytes([bytes[7], bytes[8]]), 1); // params
    }

    /// Builds a `COM_STMT_EXECUTE` parameter tail for the given typed values.
    fn execute_tail(params: &[(u8, bool, &[u8])]) -> StmtExecute {
        let n = params.len();
        let mut tail = vec![0u8; n.div_ceil(8)]; // NULL bitmap, none null
        tail.push(1); // new-params-bound
        for (code, unsigned, _) in params {
            tail.push(*code);
            tail.push(if *unsigned { 0x80 } else { 0 });
        }
        for (_, _, value) in params {
            tail.extend_from_slice(value);
        }
        StmtExecute {
            statement_id: 1,
            flags: 0,
            params_tail: tail,
        }
    }

    #[test]
    fn decodes_integer_and_string_params() {
        let exec = execute_tail(&[
            (
                column_type::MYSQL_TYPE_LONGLONG,
                false,
                &42i64.to_le_bytes(),
            ),
            (column_type::MYSQL_TYPE_VAR_STRING, false, &{
                let mut v = vec![3u8]; // lenenc length
                v.extend_from_slice(b"hey");
                v
            }),
        ]);
        let params = exec.parse_params(2).unwrap();
        assert_eq!(params[0], BinaryValue::I64(42));
        assert_eq!(params[1], BinaryValue::Bytes(b"hey".to_vec()));
    }

    #[test]
    fn decodes_negative_tinyint() {
        // -1 as a signed TINYINT is 0xff; it must sign-extend, not become 255.
        let exec = execute_tail(&[(column_type::MYSQL_TYPE_TINY, false, &[0xff])]);
        assert_eq!(exec.parse_params(1).unwrap()[0], BinaryValue::I64(-1));
    }

    #[test]
    fn large_unsigned_stays_u64() {
        let big = u64::MAX;
        let exec = execute_tail(&[(column_type::MYSQL_TYPE_LONGLONG, true, &big.to_le_bytes())]);
        assert_eq!(exec.parse_params(1).unwrap()[0], BinaryValue::U64(big));
    }

    #[test]
    fn null_param_via_bitmap() {
        // One param, flagged NULL in the bitmap; no value bytes follow.
        let exec = StmtExecute {
            statement_id: 1,
            flags: 0,
            params_tail: vec![0b0000_0001, 1, column_type::MYSQL_TYPE_LONG, 0],
        };
        assert_eq!(exec.parse_params(1).unwrap()[0], BinaryValue::Null);
    }

    #[test]
    fn binary_row_roundtrips_null_bitmap_and_values() {
        let cells = vec![
            (column_type::MYSQL_TYPE_LONG, BinaryValue::I64(5)),
            (column_type::MYSQL_TYPE_VAR_STRING, BinaryValue::Null),
            (
                column_type::MYSQL_TYPE_VAR_STRING,
                BinaryValue::Bytes(b"hi".to_vec()),
            ),
        ];
        let row = encode_binary_row(&cells);
        assert_eq!(row[0], 0x00);
        // bitmap is one byte; only column index 1 is null → bit (1+2)=3 set.
        assert_eq!(row[1], 0b0000_1000);
        let mut r = PacketReader::new(&row[2..]);
        assert_eq!(r.u32().unwrap(), 5);
        assert_eq!(r.lenenc_bytes().unwrap(), b"hi");
        assert!(r.is_empty());
    }

    #[test]
    fn datetime_roundtrip() {
        let dt = BinaryValue::DateTime {
            year: 2020,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            micros: 0,
        };
        let row = encode_binary_row(&[(column_type::MYSQL_TYPE_DATETIME, dt.clone())]);
        let mut r = PacketReader::new(&row[2..]);
        assert_eq!(decode_datetime(&mut r).unwrap(), dt);
    }
}
