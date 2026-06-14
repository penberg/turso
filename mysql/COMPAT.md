# Turso MySQL Compatibility

This document tracks the current state of MySQL compatibility — both the wire
**protocol** and the **SQL statement** surface taken from the
[MySQL 8.0 SQL Statements reference](https://dev.mysql.com/doc/refman/8.0/en/sql-statements.html).

> [!WARNING]
> **The MySQL front-end is an early proof of concept.** Almost everything here
> is unimplemented (❌). A very small number of narrowly-scoped forms are
> implemented and validated against real MySQL by the conformance suite (✅), and
> a few are partially present (🚧). Treat anything not marked ✅ as not usable.
> A ✅ applies only to the exact scoped form on that row, not to the full MySQL
> statement and all its options.

## Legend

| Marker | Meaning                                                                       |
|--------|-------------------------------------------------------------------------------|
| ❌     | **No** — not implemented (the state of everything today).                      |
| 🚧     | Partial — implemented but incomplete or unverified.                            |
| ✅     | Yes — complete and validated against MySQL.                                    |

## Table of contents

- [Turso MySQL Compatibility](#turso-mysql-compatibility)
  - [Legend](#legend)
  - [Table of contents](#table-of-contents)
  - [Overview](#overview)
  - [Wire protocol](#wire-protocol)
    - [Connection phase](#connection-phase)
    - [Authentication methods](#authentication-methods)
    - [Capability flags](#capability-flags)
    - [Command phase packets](#command-phase-packets)
    - [Generic response packets](#generic-response-packets)
    - [Result sets](#result-sets)
    - [Prepared statements (binary protocol)](#prepared-statements-binary-protocol)
    - [Transport-level features](#transport-level-features)
  - [MySQL SQL statements](#mysql-sql-statements)
    - [Data Definition Statements](#data-definition-statements)
    - [Data Manipulation Statements](#data-manipulation-statements)
    - [Transactional and Locking Statements](#transactional-and-locking-statements)
    - [Replication Statements](#replication-statements)
    - [Prepared SQL Statements](#prepared-sql-statements)
    - [Compound Statement Syntax](#compound-statement-syntax)
    - [Database Administration Statements](#database-administration-statements)
      - [Account Management](#account-management)
      - [Resource Group Management](#resource-group-management)
      - [Table Maintenance](#table-maintenance)
      - [Component, Plugin, and Loadable Function](#component-plugin-and-loadable-function)
      - [SET Statements](#set-statements)
      - [SHOW Statements](#show-statements)
      - [Other Administrative Statements](#other-administrative-statements)
    - [Utility Statements](#utility-statements)

## Overview

* ❌ MySQL wire protocol [[status](#wire-protocol)] — only the minimum needed
  for a text-protocol `COM_QUERY` round trip is exercised by the proof of
  concept; the handshake accepts any credentials without verification.
* ❌ MySQL SQL statements [[status](#mysql-sql-statements)] — statements are
  forwarded verbatim to the Turso (SQLite-dialect) engine. MySQL-specific
  syntax, semantics, data types, and error codes are not yet translated.

The proof of concept currently:

* Sends a `HandshakeV10` greeting and accepts a `HandshakeResponse41` reply
  **without authenticating** the user.
* Handles `COM_QUERY` by running the SQL through the engine and streaming a
  **text-protocol** result set (or an `OK`/`ERR` packet).
* Handles `COM_PING`, `COM_INIT_DB` (acknowledged as a no-op), and `COM_QUIT`.

Everything else — binary protocol, prepared statements, TLS, real
authentication, MySQL dialect translation, the `information_schema`, `SHOW`,
session variables, multi-statement/multi-result, compression — is unimplemented.

## Wire protocol

### Connection phase

| Item                                                  | Status | Comment                                                            |
|-------------------------------------------------------|--------|--------------------------------------------------------------------|
| Initial handshake — `Protocol::HandshakeV10`          | ❌     | Server greeting is sent; encoded by `turso-mysql-protocol`.        |
| Legacy handshake — `Protocol::HandshakeV9`            | ❌     | Not implemented.                                                   |
| Handshake response — `HandshakeResponse41`            | ❌     | Parsed, but auth response is ignored (no verification).            |
| Handshake response — `HandshakeResponse320`           | ❌     | Pre-4.1 clients not supported.                                     |
| TLS upgrade — `Protocol::SSLRequest` / `CLIENT_SSL`   | ❌     | No TLS; connections are plaintext only.                            |
| Auth method switch — `AuthSwitchRequest`/`Response`   | ❌     | Not implemented.                                                   |
| Auth more data — `AuthMoreData`                       | ❌     | Not implemented.                                                   |
| Connection attributes — `CLIENT_CONNECT_ATTRS`        | ❌     | Not parsed.                                                        |
| Initial database — `CLIENT_CONNECT_WITH_DB`           | ❌     | Capability advertised; database name not acted upon.               |

### Authentication methods

| Method                        | Status | Comment                                              |
|-------------------------------|--------|------------------------------------------------------|
| `mysql_native_password`       | ❌     | Advertised in the handshake; response not verified.  |
| `caching_sha2_password`       | ❌     | Not implemented (MySQL 8.0 default).                 |
| `sha256_password`             | ❌     | Not implemented.                                     |
| `mysql_clear_password`        | ❌     | Not implemented.                                     |
| `auth_socket` / external      | ❌     | Not implemented.                                     |
| Credential checking / users   | ❌     | No user store; every login is accepted.              |

### Capability flags

| Flag                                       | Status | Comment                                            |
|--------------------------------------------|--------|----------------------------------------------------|
| `CLIENT_PROTOCOL_41`                       | ❌     | Advertised; assumed for all packet layouts.        |
| `CLIENT_LONG_PASSWORD` / `CLIENT_LONG_FLAG`| ❌     | Advertised.                                        |
| `CLIENT_SECURE_CONNECTION`                 | ❌     | Advertised.                                        |
| `CLIENT_PLUGIN_AUTH`                       | ❌     | Advertised.                                        |
| `CLIENT_CONNECT_WITH_DB`                   | ❌     | Advertised; not honored.                           |
| `CLIENT_TRANSACTIONS`                      | ❌     | Advertised; status flags are static.               |
| `CLIENT_DEPRECATE_EOF`                     | ❌     | Not advertised; classic EOF packets are used.      |
| `CLIENT_MULTI_STATEMENTS`                  | ❌     | Not advertised.                                    |
| `CLIENT_MULTI_RESULTS`                     | ❌     | Not advertised.                                    |
| `CLIENT_COMPRESS` / `CLIENT_ZSTD_*`        | ❌     | Not advertised; no compression.                    |
| `CLIENT_SSL`                               | ❌     | Not advertised; no TLS.                            |
| `CLIENT_SESSION_TRACK`                     | ❌     | Not advertised.                                    |
| `CLIENT_LOCAL_FILES`                       | ❌     | Not advertised.                                    |
| `CLIENT_OPTIONAL_RESULTSET_METADATA`       | ❌     | Not advertised.                                    |

### Command phase packets

| Command                       | Status | Comment                                                       |
|-------------------------------|--------|---------------------------------------------------------------|
| `COM_QUERY`                   | ❌     | Text protocol only; single statement; forwarded to engine.    |
| `COM_QUIT`                    | ❌     | Closes the connection.                                        |
| `COM_PING`                    | ❌     | Replies `OK`.                                                 |
| `COM_INIT_DB`                 | ❌     | Acknowledged as a no-op (single schema).                      |
| `COM_FIELD_LIST`              | ❌     | Not implemented (deprecated in MySQL).                        |
| `COM_STATISTICS`              | ❌     | Not implemented.                                              |
| `COM_PROCESS_INFO`            | ❌     | Not implemented.                                              |
| `COM_PROCESS_KILL`            | ❌     | Not implemented.                                              |
| `COM_DEBUG`                   | ❌     | Not implemented.                                              |
| `COM_CHANGE_USER`             | ❌     | Not implemented.                                              |
| `COM_RESET_CONNECTION`        | ❌     | Not implemented.                                              |
| `COM_SET_OPTION`              | ❌     | Not implemented.                                              |
| `COM_STMT_PREPARE`            | ❌     | Not implemented.                                              |
| `COM_STMT_EXECUTE`            | ❌     | Not implemented.                                              |
| `COM_STMT_SEND_LONG_DATA`     | ❌     | Not implemented.                                              |
| `COM_STMT_CLOSE`              | ❌     | Not implemented.                                              |
| `COM_STMT_RESET`              | ❌     | Not implemented.                                              |
| `COM_STMT_FETCH`              | ❌     | Not implemented.                                              |
| `COM_REFRESH` / `COM_SHUTDOWN`| ❌     | Not implemented (deprecated in MySQL).                        |

### Generic response packets

| Packet                         | Status | Comment                                                     |
|--------------------------------|--------|-------------------------------------------------------------|
| `OK_Packet`                    | ❌     | Encoded; `affected_rows`/`last_insert_id` from the engine.  |
| `ERR_Packet`                   | ❌     | Encoded; always SQLSTATE `HY000`, error code `1105`.        |
| `EOF_Packet`                   | ❌     | Encoded; used to terminate column lists and result sets.    |
| MySQL error code mapping       | ❌     | All errors collapse to a single generic code.               |
| SQLSTATE mapping               | ❌     | Always `HY000`.                                             |
| Session state change tracking  | ❌     | Not implemented.                                            |

### Result sets

| Item                                       | Status | Comment                                              |
|--------------------------------------------|--------|------------------------------------------------------|
| Column count packet                        | ❌     | Encoded.                                             |
| Column definition — `ColumnDefinition41`   | ❌     | Encoded; every column typed as `VAR_STRING`.         |
| Real column types / flags / decimals       | ❌     | Not derived from the schema; placeholders only.      |
| Text protocol rows                         | ❌     | Encoded; values rendered via the engine's `Display`. |
| Binary protocol rows                       | ❌     | Not implemented.                                     |
| `NULL` handling                            | ❌     | `0xfb` marker emitted for NULL.                      |
| Charset / collation per column             | ❌     | Hard-coded to `utf8mb4_general_ci`.                  |
| Multi-resultset (`SERVER_MORE_RESULTS`)    | ❌     | Not implemented.                                     |
| `LOCAL INFILE` response                    | ❌     | Not implemented.                                     |

### Prepared statements (binary protocol)

| Item                                  | Status | Comment              |
|---------------------------------------|--------|----------------------|
| Server-side prepare/execute           | ❌     | Not implemented.     |
| Parameter binding (binary)            | ❌     | Not implemented.     |
| `COM_STMT_*` packets                  | ❌     | Not implemented.     |
| Cursors / `COM_STMT_FETCH`            | ❌     | Not implemented.     |

### Transport-level features

| Item                                       | Status | Comment                                                       |
|--------------------------------------------|--------|---------------------------------------------------------------|
| Packet framing (3-byte len + seq id)       | ❌     | Implemented in `turso-mysql-protocol`.                        |
| Multi-frame payloads (>16 MiB) reassembly  | ❌     | Implemented in the decoder; not exercised end to end.         |
| Compression (`zlib` / `zstd`)              | ❌     | Not implemented.                                              |
| TLS / encryption                           | ❌     | Not implemented.                                              |
| Unix domain socket transport               | ❌     | TCP only.                                                     |
| Named pipe / shared memory transport       | ❌     | Not implemented.                                              |
| Connection / thread id                     | ❌     | Assigned per connection; not exposed via `CONNECTION_ID()`.   |

## MySQL SQL statements

Statement support reflects what is reachable through `COM_QUERY` today. Because
queries are passed to the SQLite-dialect engine unchanged, MySQL-specific
syntax and semantics for any statement below are **not** translated, even where
a superficially similar statement executes. All entries are therefore marked
incomplete.

### Data Definition Statements

| Statement                              | Status | Comment |
|----------------------------------------|--------|---------|
| ALTER DATABASE                         | ❌     |         |
| ALTER EVENT                            | ❌     |         |
| ALTER FUNCTION                         | ❌     |         |
| ALTER INSTANCE                         | ❌     |         |
| ALTER LOGFILE GROUP                    | ❌     |         |
| ALTER PROCEDURE                        | ❌     |         |
| ALTER SERVER                           | ❌     |         |
| ALTER TABLE                            | ❌     | MySQL clauses (ALGORITHM, partitioning, etc.) not translated. |
| ALTER TABLESPACE                       | ❌     |         |
| ALTER VIEW                             | ❌     |         |
| CREATE DATABASE                        | ❌     |         |
| CREATE EVENT                           | ❌     |         |
| CREATE FUNCTION                        | ❌     |         |
| CREATE INDEX                           | ❌     |         |
| CREATE LOGFILE GROUP                   | ❌     |         |
| CREATE PROCEDURE / CREATE FUNCTION     | ❌     |         |
| CREATE SERVER                          | ❌     |         |
| CREATE SPATIAL REFERENCE SYSTEM        | ❌     |         |
| CREATE TABLE                           | ❌     | MySQL types, storage engines, and table options not translated. |
| CREATE TEMPORARY TABLE                 | ✅     | Session-private and dropped at disconnect, like the engine's TEMP tables. |
| CREATE TABLE ... LIKE                  | ❌     |         |
| CREATE TABLE ... SELECT                | ❌     |         |
| CREATE TABLESPACE                      | ❌     |         |
| CREATE TRIGGER                         | ❌     |         |
| CREATE VIEW                            | ❌     |         |
| DROP DATABASE                          | ❌     |         |
| DROP EVENT                             | ❌     |         |
| DROP FUNCTION                          | ❌     |         |
| DROP INDEX                             | ❌     |         |
| DROP LOGFILE GROUP                     | ❌     |         |
| DROP PROCEDURE / DROP FUNCTION         | ❌     |         |
| DROP SERVER                            | ❌     |         |
| DROP SPATIAL REFERENCE SYSTEM          | ❌     |         |
| DROP TABLE *tbl_name* (single table)   | ✅     |         |
| DROP TABLE ... IF EXISTS               | ✅     | Dropping a non-existent table is a no-op success, as in MySQL. |
| DROP TEMPORARY TABLE [IF EXISTS]       | ✅     | Qualified onto the engine's temp schema so it drops only the temporary table, never a base table of the same name. A schema-qualified name (`db.t`) is rejected. |
| DROP TABLE *t1, t2, ...* (multiple)    | ❌     | **Not supported** — only a single table per statement. |
| DROP TABLE ... RESTRICT / CASCADE      | ❌     | **Not supported** — rejected as unsupported (no-ops in MySQL). |
| DROP TABLESPACE                        | ❌     |         |
| DROP TRIGGER                           | ❌     |         |
| DROP VIEW                              | ❌     |         |
| RENAME TABLE                           | ❌     |         |
| TRUNCATE TABLE                         | 🚧     | Translated to an unfiltered `DELETE FROM tbl` (same empty-table result). `TRUNCATE`'s implicit commit, `AUTO_INCREMENT` reset, and zero affected-row count are not reproduced. |

#### CREATE TABLE column attributes

| Attribute | Status | Comment |
|-----------|--------|---------|
| `NOT NULL` / `NULL`         | ✅ | |
| `DEFAULT <literal>`         | ✅ | Literal defaults; function/expression defaults are dropped to NULL. |
| `PRIMARY KEY` (inline / table-level, single column) | ✅ | |
| `PRIMARY KEY` (composite)   | ✅ | Parsed and forwarded; not valid with `AUTO_INCREMENT` (below). |
| `AUTO_INCREMENT`            | ✅ | Only on a single-column `PRIMARY KEY` (inline or table-level). The key column is retyped to `INTEGER` so the engine treats it as a rowid alias that auto-assigns sequential ids and never reuses them — identical to MySQL. MySQL's int width (`bigint(20)`, `int(11)`) is display-only and dropped. |
| `AUTO_INCREMENT` elsewhere  | ❌ | On a non-key column, a composite key, or more than one column: rejected as unsupported (MySQL would map differently). |

### Data Manipulation Statements

| Statement                              | Status | Comment |
|----------------------------------------|--------|---------|
| CALL                                   | ❌     |         |
| DELETE FROM tbl [WHERE] (single table) | ✅     |         |
| DELETE (multi-table / USING / ORDER BY / LIMIT) | ❌ | **Not supported.** |
| DO                                     | ❌     |         |
| EXCEPT clause                          | ❌     |         |
| HANDLER                                | ❌     |         |
| IMPORT TABLE                           | ❌     |         |
| INSERT ... VALUES (basic)              | ✅     |         |
| INSERT ... SET                         | ❌     | **Not supported.** |
| INSERT ... SELECT                      | ❌     | **Not supported.** |
| INSERT ... ON DUPLICATE KEY UPDATE     | ✅     | Lowered to the engine's target-less upsert (`ON CONFLICT DO UPDATE SET ...`), which fires on any unique/primary-key conflict like MySQL. `VALUES(col)` is mapped to `excluded.col`; `VALUES(...)` nested inside a larger expression is not modeled (parse error). |
| INSERT IGNORE / DELAYED / priority     | ❌     | **Not supported.** |
| INTERSECT clause                       | ❌     |         |
| LOAD DATA                              | ❌     |         |
| LOAD XML                               | ❌     |         |
| Parenthesized Query Expressions        | ❌     |         |
| REPLACE ... VALUES                     | ✅     | `REPLACE [INTO] tbl ... VALUES ...` lowers to the engine's `INSERT OR REPLACE`: a row conflicting on a primary/unique key is deleted before the new row is inserted, like MySQL. The `REPLACE ... SET` and `REPLACE ... SELECT` forms are not supported. |
| SELECT (single table, WHERE/ORDER BY/LIMIT) | ✅ |         |
| SELECT ... GROUP BY [HAVING]           | ✅     | GROUP BY column expressions (not integer ordinals — those diverge). |
| SELECT DISTINCT                        | ✅     | `DISTINCTROW` synonym not supported. |
| `SELECT SQL_CALC_FOUND_ROWS ...` + `SELECT FOUND_ROWS()` | 🚧 | The modifier is honored: the query returns its limited rows, and a following `FOUND_ROWS()` on the same connection returns the count the query would return without its `LIMIT` (computed by re-running it without the limit). Drives `WP_Query` pagination. `FOUND_ROWS()` is only meaningful right after a `SQL_CALC_FOUND_ROWS` query — it is not updated after ordinary `SELECT`s. |
| Column aliases (`expr AS a` / `expr a`) | ✅    | Both the `AS` and bare forms; resolvable in `ORDER BY`/`GROUP BY`. A string-literal alias (`expr AS 'name'`) is also accepted; the elided string form (`expr 'name'`) is not (ambiguous with literal concatenation). |
| SELECT ... INTO                        | ❌     | **Not supported.** |
| `[INNER] JOIN` / `LEFT [OUTER] JOIN` ... ON | ✅ | Table aliases (`t`, `t AS a`) and chained joins supported. Map identically onto the engine. |
| Comma join (`FROM a, b WHERE ...`)     | ✅     | Implicit cross join with the condition in `WHERE`; the engine evaluates it identically to MySQL. Used by WordPress term/post-count queries. |
| `RIGHT`/`FULL`/`CROSS`/`NATURAL`/`STRAIGHT_JOIN`, `JOIN ... USING`, ON-less keyword joins | ❌ | Rejected as unsupported (semantics differ or unmodeled). |
| UNION / UNION ALL                      | ✅     | `UNION` deduplicates, `UNION ALL` does not; a trailing `ORDER BY`/`LIMIT` applies to the whole result. Identical to MySQL 8.x. |
| INTERSECT / EXCEPT set operations      | ✅     | Deduplicating set operations, identical to MySQL 8.x. Mixed-operator precedence is not exercised. |
| Subqueries                             | ✅     | `IN (SELECT ...)`, `[NOT] EXISTS (SELECT ...)`, scalar `(SELECT ...)` in expressions, and derived tables in `FROM` — including correlated forms. See the Expressions section. |
| Derived / lateral derived tables       | ❌     |         |
| TABLE statement                        | ❌     |         |
| UPDATE tbl SET ... [WHERE] (single table) | ✅  |         |
| UPDATE (multi-table / ORDER BY / LIMIT) | ❌    | **Not supported.** |
| VALUES statement                       | ❌     |         |
| WITH (Common Table Expressions)        | ❌     |         |

### Expressions and operators

Only constructs whose MySQL semantics are identical to SQLite/turso are
implemented — each is proven by the dual-target conformance suite (it passes
against both real MySQL and the engine). Constructs that look similar but
diverge are deliberately excluded.

| Construct                              | Status | Comment |
|----------------------------------------|--------|---------|
| Comparisons `= <> != < <= > >=`        | ✅     |         |
| `AND` / `OR` / `NOT`, parentheses      | ✅     |         |
| `IS [NOT] NULL`                        | ✅     |         |
| Arithmetic `+` `-` `*`                 | ✅     |         |
| `[NOT] IN (value list)`                | ✅     | Includes the empty list: `x IN ()` folds to `0` and `x NOT IN ()` to `1` (MySQL semantics), since the engine has no empty-list `IN`. |
| `[NOT] BETWEEN a AND b`                | ✅     |         |
| `[NOT] LIKE` (ASCII patterns)          | ✅     | Backslash is the default escape character (so `\%` / `\_` match literally), as in MySQL — the front-end supplies `ESCAPE '\'` when the query gives no explicit `ESCAPE` clause. An explicit `LIKE ... ESCAPE 'c'` is honored. This is what `$wpdb->esc_like()` relies on. |
| `[NOT] REGEXP` / `RLIKE`               | 🚧     | Mapped to the engine's `REGEXP` operator (Rust `regex` crate). **Case-sensitive**, unlike MySQL's default case-insensitive `REGEXP`; the regex dialect also differs for advanced constructs. Common anchored/character-class patterns match on both. |
| `CASE` (searched and simple forms)     | ✅     | `CASE WHEN ... THEN ... [ELSE ...] END` and `CASE expr WHEN ... END`; standard SQL, identical. |
| `expr COLLATE collation_name`          | 🚧     | The `COLLATE` postfix is parsed and **discarded** — the engine is effectively single-collation (binary), so the named collation is not honored (comparisons/sorts use the engine default). The collation name must be an identifier (`COLLATE 'string'` is rejected). |
| `CAST(expr AS type)`                   | 🚧     | Real cast syntax (not a function). Targets map onto engine affinity: `CHAR`→text, `SIGNED`/`UNSIGNED`→integer, `DECIMAL`→numeric, `DOUBLE`/`FLOAT`/`REAL`→real, `BINARY`→blob. Length/precision (`CHAR(n)`, `DECIMAL(m,d)`) parses but is **not enforced**, integer rounding of fractional values differs from MySQL (truncates vs rounds), and `UNSIGNED` is not distinguished from `SIGNED`. Date/time and `JSON` targets are rejected. |
| `CONVERT(expr USING charset)` / `CONVERT(expr, type)` | 🚧 | `USING charset` is charset coercion: the engine is single-charset (UTF-8), so the charset is dropped and the value passes through unchanged. `CONVERT(expr, type)` is identical to `CAST(expr AS type)` (same mapping and divergences). |
| `/` (division)                         | ❌     | **Excluded** — MySQL float division (`5/2=2.5`) vs SQLite integer division (`5/2=2`). |
| `%` / `MOD`                            | ❌     | **Excluded** — float modulo differs. |
| `\|\|`                                 | ❌     | **Excluded** — MySQL `\|\|` is logical OR; SQLite `\|\|` is string concat. |
| `[NOT] IN (SELECT ...)`                | ✅     | Uncorrelated subquery in `IN`/`NOT IN`; evaluates identically. |
| `[NOT] EXISTS (SELECT ...)`            | ✅     | Correlated subqueries supported; identical semantics. |
| Derived table `FROM (SELECT ...) alias` | ✅    | Subquery in `FROM`; the alias is required (as in MySQL). |
| Scalar subquery `(SELECT ...)` in an expression | ✅ | A parenthesized `SELECT` returning a single value, usable in the select list or `WHERE`, including correlated subqueries. Evaluated identically to MySQL. |

#### Scalar functions

Accepted via a strict allow-list of functions whose MySQL semantics match
SQLite/turso exactly. Any other function is rejected as unsupported.

| Function                               | Status | Comment |
|----------------------------------------|--------|---------|
| `COALESCE`                             | ✅     |         |
| `NULLIF`                               | ✅     |         |
| `IFNULL`                               | ✅     |         |
| `ABS`                                  | ✅     |         |
| `LOWER` / `UPPER`                      | ✅     | ASCII case folding. |
| `REPLACE`                              | ✅     | Replaces every occurrence, case-sensitively. |
| `SUBSTR`                               | ✅     | 1-indexed, optional length, negative position from the end. |
| `INSTR`                                | ✅     | 1-indexed position of the first match, or 0. |
| `TRIM`                                 | ✅     | `TRIM(str)` (leading/trailing spaces); the `TRIM(... FROM ...)` form is not parsed. |
| `COUNT(*)` / `COUNT(expr)`             | ✅     | aggregate |
| `SUM` / `MIN` / `MAX`                  | ✅     | aggregate |
| `COUNT/SUM/MIN/MAX(DISTINCT expr)`     | ✅     | The `DISTINCT` quantifier on an aggregate; `ALL` is the default and ignored. `DISTINCT` on a scalar function or with `*` is rejected. |
| `AVG`                                  | ❌     | **Excluded** — MySQL returns DECIMAL padded to 4 places; SQLite returns a plain float (text differs). |
| `GROUP_CONCAT`                         | ❌     | **Excluded** — separator / `ORDER BY` syntax differs. |
| `CONCAT`                               | ✅     | Lowered to the engine's `\|\|` operator (not `concat()`): like MySQL, the result is NULL if any argument is NULL. Requires at least one argument. |
| `FIELD`                                | ✅     | Lowered to `CASE x WHEN a THEN 1 WHEN b THEN 2 ... ELSE 0 END` — the 1-based index of the first argument among the rest, or 0 if absent/NULL. WordPress uses it for `ORDER BY FIELD(...)` (e.g. `orderby=post__in`). |
| `LENGTH`                               | ✅     | Byte count. Lowered to `length(CAST(x AS BLOB))` (the engine's `length()` of a blob counts bytes); matches MySQL's byte semantics, distinct from `CHAR_LENGTH`. |
| `ROUND`                                | ❌     | **Excluded** — MySQL pads to the requested decimals / returns DECIMAL; SQLite returns a bare float. |
| `IF`                                   | ✅     | Renamed on emit to the engine's `IIF`; semantics are identical (a NULL/zero condition is false). |
| `SUBSTRING` / `MID`                    | ✅     | Renamed on emit to `substr` (same behaviour). |
| `LCASE` / `UCASE`                      | ✅     | Renamed on emit to `lower` / `upper`. |
| `CHAR_LENGTH` / `CHARACTER_LENGTH`     | ✅     | Renamed on emit to `length` (a character count). Distinct from `LENGTH`, which counts bytes and stays excluded. |
| `YEAR` / `MONTH` / `DAY` / `DAYOFMONTH` / `HOUR` / `MINUTE` / `SECOND` | ✅ | Date-part extractors, lowered to `CAST(strftime(fmt, x) AS INTEGER)`; return the integer component (no zero-padding) like MySQL for the standard `YYYY-MM-DD HH:MM:SS` format. |
| `DATE_ADD` / `DATE_SUB` (`INTERVAL n unit`) | 🚧 | Lowered to the engine's `datetime(x, '±n unit')` modifier. `unit` ∈ `DAY`/`WEEK`/`MONTH`/`YEAR`/`HOUR`/`MINUTE`/`SECOND`; the interval value must be an integer literal. Matches MySQL for DATETIME arguments. Diverges on a bare DATE (the engine keeps the `00:00:00` time) and on `MONTH`/`YEAR` arithmetic that overflows a month end (MySQL clamps, the engine rolls over). |
| `DATE_FORMAT(x, fmt)`                   | 🚧     | Lowered to `strftime()` with the format translated: `%Y %m %d %H` pass through, `%i`→`%M` (minutes), `%s`→`%S` (seconds), `%%` literal, other characters copied. The format must be a string literal. Specifiers without a strftime equivalent (`%M` month name, `%h` 12-hour, `%p`, `%W`, `%j`, …) are rejected rather than silently mistranslated. |
| `NOW` / `CURRENT_TIMESTAMP` / `UTC_TIMESTAMP` / `LOCALTIME` / `SYSDATE` | 🚧 | Current datetime, lowered to `datetime('now')`. **Always UTC** — the engine has no session time zone, so this diverges from MySQL's session-local `NOW()`. The no-argument form only (a fractional-seconds precision arg is not supported). |
| `CURDATE` / `CURRENT_DATE` / `CURTIME` / `CURRENT_TIME` / `UTC_DATE` / `UTC_TIME` | 🚧 | Lowered to `date('now')` / `time('now')`; UTC, as above. |
| `UNIX_TIMESTAMP([d])` / `FROM_UNIXTIME(n)` | 🚧 | Lowered to `unixepoch(d)` (or `unixepoch('now')`) and `datetime(n, 'unixepoch')`. Absolute conversions are UTC (the engine has no session time zone), so they diverge from MySQL's session-local values, but the two are inverses on both targets, so a round trip is zone-independent. `FROM_UNIXTIME`'s two-argument formatting form is not supported. |
| Other date/time functions (`STR_TO_DATE`, `DATEDIFF`, `TIMESTAMPDIFF`, …) | ❌ | **Excluded** — format/type or timezone differences. |
| `VERSION` / `DATABASE` / `SCHEMA` / `CONNECTION_ID` / `USER` / `CURRENT_USER` / `SESSION_USER` / `SYSTEM_USER` | 🚧 | Introspection functions, usable in any expression (not just as a standalone `SELECT`). Fold to fixed placeholder values matching the server's standalone-query answers (`VERSION()`→`8.0.0-turso`, `USER()`→`root@localhost`, `CONNECTION_ID()`→`1`); `DATABASE()`/`SCHEMA()`→`NULL` (no current schema). Not the real per-connection values. |
| any other function                     | ❌     | **Not supported** — not in the clean allow-list. |

### Transactional and Locking Statements

| Statement                                       | Status | Comment |
|-------------------------------------------------|--------|---------|
| START TRANSACTION / BEGIN [WORK]                | ✅     | Mapped to the engine's `BEGIN` (deferred). `READ ONLY`/`READ WRITE`/`WITH CONSISTENT SNAPSHOT` characteristics are rejected. |
| COMMIT [WORK]                                    | ✅     | `AND CHAIN`/`RELEASE` rejected. |
| ROLLBACK [WORK]                                  | ✅     | `AND CHAIN`/`RELEASE` rejected. |
| SAVEPOINT                                        | ❌     |         |
| ROLLBACK TO SAVEPOINT                            | ❌     | Rejected as unsupported. |
| RELEASE SAVEPOINT                                | ❌     |         |
| SELECT ... FOR UPDATE / FOR SHARE / LOCK IN SHARE MODE | 🚧 | The trailing locking-read clause is accepted and ignored. The engine is a single writer, so the locked query returns the same rows as the unlocked one; no real row locking is performed. `OF tbl` / `NOWAIT` / `SKIP LOCKED` refinements are rejected. |
| LOCK INSTANCE FOR BACKUP / UNLOCK INSTANCE       | ❌     |         |
| LOCK TABLES / UNLOCK TABLES                      | ❌     |         |
| SET TRANSACTION                                  | ❌     |         |
| XA transactions (XA START/END/PREPARE/COMMIT...) | ❌     |         |

### Replication Statements

| Statement                                       | Status | Comment |
|-------------------------------------------------|--------|---------|
| PURGE BINARY LOGS                               | ❌     |         |
| RESET MASTER / RESET BINARY LOGS AND GTIDS      | ❌     |         |
| SET sql_log_bin                                 | ❌     |         |
| CHANGE MASTER TO / CHANGE REPLICATION SOURCE TO | ❌     |         |
| CHANGE REPLICATION FILTER                       | ❌     |         |
| RESET REPLICA / RESET SLAVE                     | ❌     |         |
| START REPLICA / START SLAVE                     | ❌     |         |
| STOP REPLICA / STOP SLAVE                       | ❌     |         |
| START GROUP_REPLICATION                         | ❌     |         |
| STOP GROUP_REPLICATION                          | ❌     |         |

### Prepared SQL Statements

| Statement            | Status | Comment |
|----------------------|--------|---------|
| PREPARE              | ❌     |         |
| EXECUTE              | ❌     |         |
| DEALLOCATE PREPARE   | ❌     |         |

### Compound Statement Syntax

| Statement                                   | Status | Comment |
|---------------------------------------------|--------|---------|
| BEGIN ... END                               | ❌     |         |
| Statement labels                            | ❌     |         |
| DECLARE                                     | ❌     |         |
| Variables in stored programs                | ❌     |         |
| CASE                                        | ❌     |         |
| IF                                          | ❌     |         |
| ITERATE                                     | ❌     |         |
| LEAVE                                       | ❌     |         |
| LOOP                                        | ❌     |         |
| REPEAT                                      | ❌     |         |
| RETURN                                      | ❌     |         |
| WHILE                                       | ❌     |         |
| Cursors (OPEN / FETCH / CLOSE / DECLARE)    | ❌     |         |
| DECLARE ... CONDITION                       | ❌     |         |
| DECLARE ... HANDLER                         | ❌     |         |
| GET DIAGNOSTICS                             | ❌     |         |
| SIGNAL / RESIGNAL                           | ❌     |         |

### Database Administration Statements

#### Account Management

| Statement              | Status | Comment |
|------------------------|--------|---------|
| ALTER USER             | ❌     |         |
| CREATE ROLE            | ❌     |         |
| CREATE USER            | ❌     |         |
| DROP ROLE              | ❌     |         |
| DROP USER              | ❌     |         |
| GRANT                  | ❌     |         |
| RENAME USER            | ❌     |         |
| REVOKE                 | ❌     |         |
| SET DEFAULT ROLE       | ❌     |         |
| SET PASSWORD           | ❌     |         |
| SET ROLE               | ❌     |         |

#### Resource Group Management

| Statement                | Status | Comment |
|--------------------------|--------|---------|
| ALTER RESOURCE GROUP     | ❌     |         |
| CREATE RESOURCE GROUP    | ❌     |         |
| DROP RESOURCE GROUP      | ❌     |         |
| SET RESOURCE GROUP       | ❌     |         |

#### Table Maintenance

| Statement          | Status | Comment |
|--------------------|--------|---------|
| ANALYZE TABLE      | ❌     |         |
| CHECK TABLE        | ❌     |         |
| CHECKSUM TABLE     | ❌     |         |
| OPTIMIZE TABLE     | ❌     |         |
| REPAIR TABLE       | ❌     |         |

#### Component, Plugin, and Loadable Function

| Statement                                     | Status | Comment |
|-----------------------------------------------|--------|---------|
| CREATE FUNCTION (loadable)                    | ❌     |         |
| DROP FUNCTION (loadable)                       | ❌     |         |
| INSTALL COMPONENT                              | ❌     |         |
| INSTALL PLUGIN                                 | ❌     |         |
| UNINSTALL COMPONENT                            | ❌     |         |
| UNINSTALL PLUGIN                               | ❌     |         |
| CLONE                                          | ❌     |         |

#### SET Statements

| Statement                          | Status | Comment |
|------------------------------------|--------|---------|
| SET (variable assignment)          | 🚧     | Accepted as a no-op, except `SET [SESSION\|GLOBAL] sql_mode = '...'`, whose value is stored per session and returned by `SELECT @@[SESSION.\|GLOBAL.]sql_mode`. MySQL's default mode and mode normalization/reordering are not modeled (the value is stored verbatim). |
| SET CHARACTER SET                  | 🚧     | Accepted as a no-op. |
| SET NAMES                          | 🚧     | Accepted as a no-op (commonly sent by clients on connect). |

#### SHOW Statements

| Statement                  | Status | Comment |
|----------------------------|--------|---------|
| SHOW BINARY LOGS           | ❌     |         |
| SHOW BINLOG EVENTS         | ❌     |         |
| SHOW CHARACTER SET         | ❌     |         |
| SHOW COLLATION             | ❌     |         |
| SHOW [FULL] COLUMNS / FIELDS FROM *tbl* | ✅ | Answered from the schema (`PRAGMA table_info` plus the stored `CREATE TABLE` text). `Field`, `Null`, `Key`, `Default`, `Extra`, the declared column size (`varchar(60)`, `decimal(10,2)`), and the result-set shape match MySQL; `Type` is lowercased but otherwise not normalized (`unsigned` and integer display-width stripping are not applied), and text `Collation` is reported as a fixed `utf8mb4_general_ci`. The `LIKE`/`WHERE` filter forms are not handled (rejected as unsupported). A missing table errors with 1146. |
| SHOW CREATE DATABASE       | ❌     |         |
| SHOW CREATE EVENT          | ❌     |         |
| SHOW CREATE FUNCTION       | ❌     |         |
| SHOW CREATE PROCEDURE      | ❌     |         |
| SHOW CREATE TABLE          | ❌     |         |
| SHOW CREATE TRIGGER        | ❌     |         |
| SHOW CREATE USER           | ❌     |         |
| SHOW CREATE VIEW           | ❌     |         |
| SHOW DATABASES             | ❌     |         |
| SHOW ENGINE                | ❌     |         |
| SHOW ENGINES               | ❌     |         |
| SHOW ERRORS                | ❌     |         |
| SHOW EVENTS                | ❌     |         |
| SHOW FUNCTION CODE         | ❌     |         |
| SHOW FUNCTION STATUS       | ❌     |         |
| SHOW GRANTS                | ❌     |         |
| SHOW INDEX                 | ❌     |         |
| SHOW MASTER STATUS / BINARY LOG STATUS | ❌ |   |
| SHOW OPEN TABLES           | ❌     |         |
| SHOW PLUGINS               | ❌     |         |
| SHOW PRIVILEGES            | ❌     |         |
| SHOW PROCEDURE CODE        | ❌     |         |
| SHOW PROCEDURE STATUS      | ❌     |         |
| SHOW PROCESSLIST           | ❌     |         |
| SHOW PROFILE / PROFILES    | ❌     |         |
| SHOW RELAYLOG EVENTS       | ❌     |         |
| SHOW REPLICAS / SLAVE HOSTS| ❌     |         |
| SHOW REPLICA / SLAVE STATUS| ❌     |         |
| SHOW STATUS                | ❌     |         |
| SHOW TABLE STATUS          | ❌     |         |
| SHOW [FULL] TABLES [LIKE]  | ✅     | Base table names synthesized from the schema, optionally filtered by a `LIKE` pattern; `FULL` adds a `Table_type` column (`BASE TABLE`). The result column header is a fixed `Tables_in_database`, not MySQL's `Tables_in_<db>` (clients read it positionally). The `WHERE` filter form is rejected. |
| SHOW TRIGGERS              | ❌     |         |
| SHOW VARIABLES             | ❌     | Frequently probed by clients/ORMs; currently errors. |
| SHOW WARNINGS              | ❌     |         |

#### Other Administrative Statements

| Statement                  | Status | Comment |
|----------------------------|--------|---------|
| BINLOG                     | ❌     |         |
| CACHE INDEX                | ❌     |         |
| FLUSH                      | ❌     |         |
| KILL                       | ❌     |         |
| LOAD INDEX INTO CACHE      | ❌     |         |
| RESET                      | ❌     |         |
| RESET PERSIST              | ❌     |         |
| RESTART                    | ❌     |         |
| SHUTDOWN                   | ❌     |         |

### Utility Statements

| Statement   | Status | Comment |
|-------------|--------|---------|
| DESCRIBE / DESC | ✅ | Synonym for `SHOW COLUMNS FROM tbl` (non-FULL form). The `DESCRIBE tbl col_name` column-filter form is not supported. |
| EXPLAIN     | ❌     | MySQL `EXPLAIN` output format not produced. |
| HELP        | ❌     |         |
| USE         | ❌     | Maps conceptually to `COM_INIT_DB`; single-schema no-op. |
