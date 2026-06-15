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
| ALTER TABLE                            | ⚠️     | ADD/DROP COLUMN, ADD [UNIQUE] KEY/INDEX, RENAME [COLUMN] supported. ADD FULLTEXT degrades to a plain index (no MATCH...AGAINST). ADD PRIMARY KEY (cols) is emulated by a `CREATE UNIQUE INDEX` over the key columns (the engine cannot add a real in-place rowid primary key): the statement succeeds and the key's uniqueness is enforced, but the index is reported by `SHOW INDEX` under a `<table>_primary` name rather than MySQL's `PRIMARY`, the columns are not made implicitly NOT NULL, and a repeated `ADD PRIMARY KEY` errors on the duplicate index name. DROP PRIMARY KEY is the inverse — it drops that `<table>_primary` index, so an ADD/DROP cycle round-trips; it does not apply to a primary key declared in CREATE TABLE (the engine's rowid alias, which has no such index). The comma-separated multi-operation form (`ADD a, ADD KEY ..., DROP b`) is expanded into one statement per operation, run in sequence (not atomic — operations before a failing one still apply). ADD FOREIGN KEY, SPATIAL, DROP FOREIGN KEY, CHANGE/MODIFY, and clauses like ALGORITHM/partitioning are not translated. |
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
| CREATE TABLE ... LIKE                  | ❌     | No engine equivalent (schema-only copy); rejected. |
| CREATE TABLE [AS] SELECT               | ✅     | `CREATE TABLE name [AS] SELECT ...` (the `AS` is optional) builds the table from the query's result rows and columns, evaluated by the engine like SQLite; composes with `TEMPORARY` and `IF NOT EXISTS`. The form with an explicit leading column list before the select is not modeled. |
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
| DROP TABLE *t1, t2, ...* (multiple)    | ✅     | The front-end expands the list into one `DROP TABLE` per table, which the server runs in sequence (the engine has no multi-table drop). `IF EXISTS` applies to every table. Matches MySQL's non-atomic semantics — tables before a failing one are still dropped. |
| DROP TABLE ... RESTRICT / CASCADE      | ❌     | **Not supported** — rejected as unsupported (no-ops in MySQL). |
| DROP TABLESPACE                        | ❌     |         |
| DROP TRIGGER                           | ❌     |         |
| DROP VIEW                              | ❌     |         |
| RENAME TABLE                           | ❌     |         |
| TRUNCATE TABLE                         | 🚧     | Translated to an unfiltered `DELETE FROM tbl` (same empty-table result). `TRUNCATE`'s implicit commit, `AUTO_INCREMENT` reset, and zero affected-row count are not reproduced. |

#### CREATE TABLE column attributes

| Attribute | Status | Comment |
|-----------|--------|---------|
| `NOT NULL` / `NULL`         | ✅ | A `NOT NULL` column with no explicit `DEFAULT` is given MySQL's implicit type default (`0` for numeric types, `''` for string/binary types) so a row that omits it still inserts — matching MySQL's default non-strict `sql_mode`, the mode WordPress runs under (a strict `sql_mode` would instead reject the row). `AUTO_INCREMENT` and `PRIMARY KEY` columns are excluded (the engine generates / rowid-handles their values). Date/time, `ENUM`/`SET`, `JSON`, and unrecognized types stay strictly `NOT NULL` (their MySQL defaults — the zero date, the first enum member, … — don't map cleanly). The synthesized default surfaces as the column's `Default` in `SHOW COLUMNS`/`DESCRIBE` (`0`/`''`), whereas MySQL reports `NULL` there — a minor introspection divergence. |
| `DEFAULT <literal>`         | ✅ | Literal defaults; function/expression defaults are dropped to NULL. An explicit `DEFAULT` is kept as written (it suppresses the implicit `NOT NULL` default above). |
| `PRIMARY KEY` (inline / table-level, single column) | ✅ | |
| `PRIMARY KEY` (composite)   | ✅ | Parsed and forwarded; not valid with `AUTO_INCREMENT` (below). |
| `AUTO_INCREMENT`            | ✅ | Only on a single-column `PRIMARY KEY` (inline or table-level). The key column is retyped to `INTEGER` so the engine treats it as a rowid alias that auto-assigns sequential ids and never reuses them — identical to MySQL. MySQL's int width (`bigint(20)`, `int(11)`) is display-only and dropped. |
| `AUTO_INCREMENT` elsewhere  | ❌ | On a non-key column, a composite key, or more than one column: rejected as unsupported (MySQL would map differently). |

### Data Manipulation Statements

| Statement                              | Status | Comment |
|----------------------------------------|--------|---------|
| CALL                                   | ❌     |         |
| DELETE FROM tbl [WHERE] (single table) | ✅     |         |
| DELETE `t1[, t2, ...] FROM <refs> [WHERE]` (multi-table) | ✅ | Lowered to `DELETE FROM <table> WHERE rowid IN (SELECT t1.rowid FROM <refs> [WHERE] [UNION SELECT t2.rowid ...])`. The `rowid` subquery (including the `UNION` over every target) is materialized before any row is deleted, so it matches MySQL without a two-phase delete. Targets may be table names or `FROM` aliases; the join may be comma or `JOIN ... ON`. **All targets must resolve to the same table** (e.g. WordPress's transient-cleanup self-join); targets on different tables are rejected. |
| DELETE `... LIMIT n` (single table)    | ✅     | The count-only `LIMIT` caps the rows deleted (no `OFFSET`). Without an `ORDER BY` the affected rows are unspecified on both MySQL and the engine, so they match. |
| DELETE (`... USING` / `ORDER BY`)      | ❌     | **Not supported** — `ORDER BY` because the engine cannot order a `DELETE`. |
| DO                                     | ❌     |         |
| EXCEPT clause                          | ❌     |         |
| HANDLER                                | ❌     |         |
| IMPORT TABLE                           | ❌     |         |
| INSERT ... VALUES (basic)              | ✅     | Multi-row `VALUES` supported. The empty form `INSERT INTO t () VALUES ()` (and the column-list-less `INSERT INTO t VALUES ()`) inserts one all-defaults row, lowered to the engine's `DEFAULT VALUES`. |
| INSERT ... SET                         | ✅     | The `INSERT [INTO] t SET col = expr, ...` assignment form, built as the equivalent `(cols) VALUES (exprs)`. |
| INSERT ... SELECT                      | ✅     | `INSERT [INTO] t [(cols)] SELECT ...`; the query runs through the same SELECT subset, evaluated by the engine. |
| INSERT ... ON DUPLICATE KEY UPDATE     | ✅     | Lowered to the engine's target-less upsert (`ON CONFLICT DO UPDATE SET ...`), which fires on any unique/primary-key conflict like MySQL. The `VALUES(col)` pseudo-function (the would-be-inserted value) is mapped to `excluded.col` anywhere in the assignment expression (e.g. `c = c + VALUES(c)`, `GREATEST(c, VALUES(c))`); a bare column on the right refers to the existing row. |
| INSERT/UPDATE/DELETE/REPLACE modifiers | ✅ | The priority/scheduling hints `LOW_PRIORITY`, `DELAYED`, `HIGH_PRIORITY`, and `QUICK` are accepted and ignored (no result effect). `INSERT IGNORE` and `UPDATE IGNORE` lower to the engine's `INSERT OR IGNORE` / `UPDATE OR IGNORE` (a row whose change would violate a constraint is skipped instead of aborting the statement). `DELETE IGNORE` is a no-op (the engine raises no per-row delete errors here). |
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
| `[INNER] JOIN` / `LEFT [OUTER] JOIN` / `RIGHT [OUTER] JOIN` ... `ON`/`USING` | ✅ | Table aliases (`t`, `t AS a`) and chained joins supported. Map identically onto the engine. |
| `CROSS JOIN` / `STRAIGHT_JOIN` / `NATURAL [LEFT\|RIGHT] JOIN` | ✅ | `CROSS JOIN` is the Cartesian product, `STRAIGHT_JOIN` lowers to a plain inner join (the join-order hint is dropped), and `NATURAL` joins on the common columns. Evaluated identically to MySQL. |
| Inner / plain `JOIN` without `ON`/`USING` | ✅ | A `JOIN` / `INNER JOIN` / `STRAIGHT_JOIN` with no condition is a cross join (MySQL treats these as equivalent to `CROSS JOIN`), typically with the predicate in `WHERE`. Only a non-NATURAL OUTER (`LEFT`/`RIGHT`) join still requires an explicit condition. |
| Comma join (`FROM a, b WHERE ...`)     | ✅     | Implicit cross join with the condition in `WHERE`; the engine evaluates it identically to MySQL. Used by WordPress term/post-count queries. |
| `FULL [OUTER] JOIN`                     | ❌     | MySQL has no `FULL JOIN`, so it is rejected (not accepted as an extension). |
| Index hints (`{USE\|FORCE\|IGNORE} {INDEX\|KEY} [FOR ...] (...)`) | ✅ | Parsed and **ignored** on any table reference (base or joined): they only steer MySQL's optimizer, and the engine plans its own access path, so the result set is unchanged. The empty `USE INDEX ()` list, the `FOR {JOIN\|ORDER BY\|GROUP BY}` scope, and `PRIMARY` as a name are all accepted. |
| UNION / UNION ALL                      | ✅     | `UNION` deduplicates, `UNION ALL` does not; a trailing `ORDER BY`/`LIMIT` applies to the whole result. Identical to MySQL 8.x. Branches may be parenthesized — `(SELECT ...) UNION (SELECT ...)`, including a leading parenthesis — and the grouping parens are stripped; a per-branch `ORDER BY`/`LIMIT` inside the parentheses is rejected (not representable in the flat compound model). |
| INTERSECT / EXCEPT set operations      | ✅     | Deduplicating set operations, identical to MySQL 8.x. Mixed-operator precedence is not exercised. |
| Subqueries                             | ✅     | `IN (SELECT ...)`, `[NOT] EXISTS (SELECT ...)`, scalar `(SELECT ...)` in expressions, and derived tables in `FROM` — including correlated forms. See the Expressions section. |
| `WITH ... SELECT` (CTEs)               | 🚧     | A `WITH` clause of one or more named CTEs (each with an optional `(col, ...)` rename list) feeding a `SELECT`; evaluated like SQLite, matching MySQL for non-recursive CTEs. `WITH RECURSIVE` parses but the engine does not yet execute recursive CTEs; the SQLite `MATERIALIZED` hint is accepted but is not MySQL syntax; `WITH` before `UPDATE`/`DELETE` is not supported. |
| Derived / lateral derived tables       | ❌     |         |
| TABLE statement                        | ❌     |         |
| UPDATE tbl SET ... [WHERE] (single table) | ✅  |         |
| UPDATE `... LIMIT n` (single table)    | ✅     | The count-only `LIMIT` caps the rows updated (no `OFFSET`); the affected rows are unspecified without an `ORDER BY`, matching MySQL. |
| UPDATE (multi-table / ORDER BY)        | ❌     | **Not supported** — `ORDER BY` because the engine cannot order an `UPDATE`. |
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
| `<=>` (NULL-safe equality)             | ✅     | Lowered to `CASE WHEN a IS NULL AND b IS NULL THEN 1 WHEN a IS NULL OR b IS NULL THEN 0 ELSE a = b END` — 1 if both NULL, 0 if exactly one, the ordinary equality otherwise; never NULL, as in MySQL. |
| Row values `(a, b) = (1, 2)` / `IN`    | ✅     | A parenthesized comma-separated list is a row-value tuple; row comparisons (`=`, `<>`, `IN`, `NOT IN`) compare element-wise, evaluated by the engine's row-value support. Used for compound-key lookups. A single `(expr)` is still an ordinary parenthesized expression. |
| `AND` / `OR` / `NOT`, parentheses      | ✅     |         |
| `&&` (logical AND)                      | ✅     | A MySQL synonym for the `AND` keyword, at the same precedence. A single `&` remains the bitwise operator. (`\|\|` is **not** treated as logical OR — it stays excluded, since the engine uses it for string concatenation.) |
| `XOR` (logical)                         | 🚧     | Lowered to `(a <> 0) <> (b <> 0)` — 1 when exactly one operand is truthy, NULL if either is NULL; between `OR` and `AND` in precedence. Matches MySQL for numeric / boolean operands; a non-numeric string's truthiness diverges (the engine does not coerce it to 0). |
| `!` (logical NOT prefix)                | ✅     | The high-precedence prefix form of NOT (binds tighter than the comparison operators, unlike the `NOT` keyword). Maps to the engine's unary `NOT`, whose truthiness matches MySQL (`!0`=1, non-zero→0, `!NULL`=NULL). `!=` is unaffected. |
| `IS [NOT] NULL`                        | ✅     |         |
| Arithmetic `+` `-` `*`                 | ✅     |         |
| `[NOT] IN (value list)`                | ✅     | Includes the empty list: `x IN ()` folds to `0` and `x NOT IN ()` to `1` (MySQL semantics), since the engine has no empty-list `IN`. |
| `[NOT] BETWEEN a AND b`                | ✅     |         |
| `[NOT] LIKE` (ASCII patterns)          | ✅     | Backslash is the default escape character (so `\%` / `\_` match literally), as in MySQL — the front-end supplies `ESCAPE '\'` when the query gives no explicit `ESCAPE` clause. An explicit `LIKE ... ESCAPE 'c'` is honored. This is what `$wpdb->esc_like()` relies on. |
| `[NOT] REGEXP` / `RLIKE`               | 🚧     | Mapped to the engine's `REGEXP` operator (Rust `regex` crate). Case-insensitive like MySQL's default (the pattern is prefixed with the regex crate's `(?i)` flag), but the regex dialect still differs from MySQL's for advanced constructs. |
| `CASE` (searched and simple forms)     | ✅     | `CASE WHEN ... THEN ... [ELSE ...] END` and `CASE expr WHEN ... END`; standard SQL, identical. |
| `expr COLLATE collation_name`          | 🚧     | The `COLLATE` postfix is parsed and **discarded** — the engine is effectively single-collation (binary), so the named collation is not honored (comparisons/sorts use the engine default). The collation name must be an identifier (`COLLATE 'string'` is rejected). |
| `CAST(expr AS type)`                   | 🚧     | Real cast syntax (not a function). Targets map onto engine affinity: `CHAR`→text, `SIGNED`/`UNSIGNED`→integer, `DECIMAL`→numeric, `DOUBLE`/`FLOAT`/`REAL`→real, `BINARY`→blob. Length/precision (`CHAR(n)`, `DECIMAL(m,d)`) parses but is **not enforced**, integer rounding of fractional values differs from MySQL (truncates vs rounds), and `UNSIGNED` is not distinguished from `SIGNED`. Date/time and `JSON` targets are rejected. |
| `CONVERT(expr USING charset)` / `CONVERT(expr, type)` | 🚧 | `USING charset` is charset coercion: the engine is single-charset (UTF-8), so the charset is dropped and the value passes through unchanged. `CONVERT(expr, type)` is identical to `CAST(expr AS type)` (same mapping and divergences). |
| `/` (division)                         | ✅     | Lowered to `CAST(a AS REAL) / b`, forcing MySQL's float division (`5 / 2` = `2.5`, not the engine's truncating integer division) and yielding NULL on division by zero. Two display/precision edges vs MySQL: the engine renders the quotient as a plain double where MySQL prints a fixed-scale DECIMAL (`2.5` vs `2.5000`), and a non-terminating quotient (`10 / 3`) carries full double precision rather than MySQL's default 4-decimal scale. The numeric value matches for terminating quotients. |
| `a % b` / `a MOD b` / `MOD(a, b)` / `a DIV b` | ✅ | The `%`/`MOD` modulo operators, the `MOD(a, b)` function, and the `DIV` integer-division operator are all lowered to integer arithmetic (`a - b * CAST(a / b AS INTEGER)` and `CAST(a / b AS INTEGER)`), which matches MySQL for both integer and float operands — including the sign-of-dividend rule and the exact float remainder (`5.5 % 2` = `1.5`). The symbolic `%` is lowered this way rather than passed to the engine's own `%`, which would truncate float operands. |
| `\|\|`                                 | ❌     | **Excluded** — MySQL `\|\|` is logical OR; SQLite `\|\|` is string concat. |
| `&` / `\|` / `<<` / `>>` / `~` (bitwise) | 🚧     | Bitwise AND / OR, left / right shift, and the unary NOT `~` (a tight prefix, like the other unary operators), mapped to the engine's equivalents. Precedence (tight → loose): `~`, `<<`/`>>`, `&`, `\|`; all tighter than comparison and looser than `+`/`-`, as in MySQL. MySQL evaluates on unsigned 64-bit integers and the engine on signed, so a result with bit 63 set prints differently — notably a bare `~x`, which always sets bit 63 (`~5` is `-6` here vs MySQL's `18446744073709551610`, the same bits) — but masked/combined results (`5 & ~1`, `(~x) & 0xFF`) and small non-negative operands match. `^` (XOR) is not parsed (the engine has no `^` operator). |
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
| `ISNULL`                               | ✅     | The single-argument test: lowered to the `x IS NULL` predicate, returning 1 if x is NULL else 0. Distinct from the two-argument `IFNULL(x, y)`. |
| `ABS`                                  | ✅     |         |
| `ROUND` / `FLOOR` / `CEIL` / `CEILING` / `POW` / `POWER` / `SQRT` / `EXP` / `LN` | ✅ | Numeric functions backed by the engine's identically-named ones (`CEILING`→`ceil`, `POWER`→`pow`); `ROUND(x[, d])` takes the optional decimal-places argument, and NULL propagates. One display difference: the engine renders an integer-valued result as a float (`FLOOR(1.8)` → `1.0`, `POW(2,10)` → `1024.0`) where MySQL prints `1` / `1024`; the numeric value is the same (`FLOOR(1.8) = 1` holds on both). |
| `TRUNCATE(x, d)`                       | ✅     | Truncates `x` to `d` decimal places toward zero (distinct from `ROUND`), synthesized as `trunc(x * pow(10, d)) / pow(10, d)`. A negative `d` truncates left of the decimal point (`TRUNCATE(1234.5678, -2)` = `1200`), and NULL in either argument propagates. The result renders as a plain double rather than MySQL's fixed-scale DECIMAL; the value matches. |
| `LOG` / `LOG2` / `LOG10` / `PI`        | ✅     | `LOG(x)` is the natural log (lowered to the engine's `ln`, since the engine's own one-arg `log` is base-10); `LOG(b, x)` is the base-`b` log; `LOG2`/`LOG10` and `PI()` map onto the engine's same-named ones. The engine evaluates the base-10/base-2 logs through natural logs, so an exact power lands a hair off (`LOG10(1000)` is `2.9999…` rather than MySQL's exact `3`) — equal after rounding to a few places. |
| `SIN` / `COS` / `TAN` / `ASIN` / `ACOS` / `ATAN` / `ATAN2` / `DEGREES` / `RADIANS` | ✅ | Trigonometric functions (in radians) and angle conversions, mapped onto the engine's same-named ones. MySQL's two-argument `ATAN(y, x)` is lowered to the engine's `atan2(y, x)`. Results are floating point, so they match MySQL after rounding. |
| `LOWER` / `UPPER`                      | ✅     | ASCII case folding. |
| `REPLACE`                              | ✅     | Replaces every occurrence, case-sensitively. |
| `SUBSTR`                               | ✅     | 1-indexed, optional length, negative position from the end. Both the comma form `SUBSTR(str, pos[, len])` and the SQL-standard `SUBSTR(str FROM pos [FOR len])` are accepted. |
| `INSTR` / `LOCATE` / `POSITION`        | 🚧     | 1-indexed position of the first match, or 0. `LOCATE(substr, str)` reverses `INSTR`'s operands, and `POSITION(substr IN str)` is the SQL-standard spelling of `LOCATE`. Lowered to `instr(lower(str), lower(substr))` so the match is case-insensitive like MySQL's default collation — exact for ASCII, non-ASCII case folding not modeled. The 3-arg `LOCATE(substr, str, pos)` searches from `pos` (lowered to an offset `instr` over `substr(str, pos)`); only `pos >= 1` matches MySQL. `INSTR` stays two-argument. |
| `TRIM`                                 | 🚧     | `TRIM(str)` and the `TRIM([{BOTH\|LEADING\|TRAILING}] [remstr] FROM str)` forms, lowered to the engine's `trim`/`ltrim`/`rtrim` (with `remstr` as the second argument). The two-argument engine trim removes any of the *characters* in `remstr`, so it matches MySQL for the default space or a single-character `remstr`; a multi-character `remstr` (which MySQL strips as a whole substring) diverges. |
| `INSERT(str, pos, len, newstr)`        | 🚧     | The string function (distinct from the `INSERT` statement; recognized in expression position). Replaces `len` characters of `str` from the 1-based `pos` with `newstr`, lowered to `CASE WHEN pos < 1 OR pos > length(str) THEN str ELSE substr(str, 1, pos-1) \|\| newstr \|\| substr(str, pos+len) END`. Out-of-range `pos` returns `str`, positions are per-character, and NULL propagates. A negative `len` is a documented edge. |
| `COUNT(*)` / `COUNT(expr)`             | ✅     | aggregate |
| `SUM` / `MIN` / `MAX` / `AVG`          | ✅     | aggregate. `AVG(expr)` is the mean of the non-NULL values (NULL over an empty/all-NULL group), backed by the engine's `avg`. The mean renders as a plain double where MySQL prints a DECIMAL padded to 4 places (`22.5` vs `22.5000`); the numeric value is the same. |
| `COUNT/SUM/MIN/MAX/AVG(DISTINCT expr)` | ✅     | The `DISTINCT` quantifier on an aggregate; `ALL` is the default and ignored. `DISTINCT` on a scalar function or with `*` is rejected. |
| `GROUP_CONCAT`                         | 🚧     | `GROUP_CONCAT([DISTINCT] expr [SEPARATOR 's'])`, lowered to the engine's `group_concat([DISTINCT] expr[, 's'])` (same default `,` separator). `DISTINCT` keeps the distinct values. Without an inner `ORDER BY` the concatenation order is unspecified in both (in practice the group's scan order). The inner `ORDER BY`, the multi-expression form, and `DISTINCT` together with a custom `SEPARATOR` (a `DISTINCT` engine aggregate takes only one argument) are rejected. |
| `CONCAT`                               | ✅     | Lowered to the engine's `\|\|` operator (not `concat()`): like MySQL, the result is NULL if any argument is NULL. Requires at least one argument. |
| `CHAR`                                 | 🚧     | Builds a string from integer character codes, mapped to the engine's `char()`. Exact for the common ASCII / control-character codes (`CHAR(10)`, `CHAR(72, 73)`→`HI`). Two divergences: MySQL skips NULL arguments while the engine stops at the first NULL, and for codes above 127 MySQL emits raw bytes (a number can span several) while the engine emits one UTF-8 code point. The `CHAR(... USING charset)` form is rejected. |
| `FIELD`                                | ✅     | Lowered to `CASE x WHEN a THEN 1 WHEN b THEN 2 ... ELSE 0 END` — the 1-based index of the first argument among the rest, or 0 if absent/NULL. WordPress uses it for `ORDER BY FIELD(...)` (e.g. `orderby=post__in`). |
| `ELT`                                  | ✅     | The inverse of `FIELD`: lowered to `CASE n WHEN 1 THEN a WHEN 2 THEN b ... END` (no `ELSE`) — the n-th string argument (1-based), or NULL if n is out of range or NULL. Requires the index plus at least one string. |
| `FIND_IN_SET`                          | 🚧     | The 1-based index of `str` in the comma-separated `strlist`, or 0; synthesized by comma-wrapping and counting commas in the matched prefix. Matches whole elements (not substrings) and is case-insensitive (ASCII, via `lower`), like MySQL's default collation. NULL propagates. A `str` that itself contains a comma returns 0 in MySQL but may match here — a minor documented edge. |
| `REPEAT`                               | ✅     | `REPEAT(s, n)` returns n copies of s. The engine has no `repeat()`, so it is synthesized as `replace(hex(zeroblob(n)), '00', s)`. A non-positive n gives the empty string; a NULL count is guarded to NULL (since `zeroblob(NULL)` is an empty blob, not NULL), and a NULL string propagates through `replace`. |
| `SPACE`                                | ✅     | `SPACE(n)` is `REPEAT(' ', n)` — a run of n spaces, the empty string for a non-positive n, and NULL for a NULL n. Same synthesized lowering as `REPEAT`. |
| `LPAD` / `RPAD`                         | 🚧     | Pad str to len characters with pad on the left / right; a too-long str is truncated to its left len chars. Synthesized from `REPEAT`/`substr`/`\|\|` (`RPAD` = `substr(str \|\| REPEAT(pad, len), 1, len)`, `LPAD` prepends the fill). Padding cycles pad and NULL propagates. Edges that diverge: a negative len gives the empty string (MySQL: NULL) and an empty pad needing fill gives str unchanged. |
| `RAND`                                 | 🚧     | Lowered to `abs(random() % 1000000000) / 1000000000.0`, a pseudo-random float in `[0, 1)` like MySQL — enough for `ORDER BY RAND()`. A seed argument (`RAND(n)`) is accepted but **not** honored: the engine's RNG is not seedable, so it does not reproduce MySQL's deterministic seeded sequence. |
| `LENGTH`                               | ✅     | Byte count. Lowered to `length(CAST(x AS BLOB))` (the engine's `length()` of a blob counts bytes); matches MySQL's byte semantics, distinct from `CHAR_LENGTH`. |
| `OCTET_LENGTH`                          | ✅     | A MySQL synonym for `LENGTH` (byte count); shares the exact `length(CAST(x AS BLOB))` lowering. |
| `BIT_LENGTH`                            | ✅     | The byte length times eight; lowered to `8 * length(CAST(x AS BLOB))`. NULL propagates. |
| `GET_LOCK` / `RELEASE_LOCK`            | 🚧     | Advisory locks fold to the constant `1` ("acquired" / "released"); the name and timeout are ignored. This single-node engine has no cross-session lock table, so it matches MySQL only for the uncontended acquire/release flow — the contended (`GET_LOCK` times out → `0`) and not-held (`RELEASE_LOCK` → `0`) cases, and the `NULL`-on-error case, are **not** modeled, and no real mutual exclusion is provided. |
| `ROUND`                                | ❌     | **Excluded** — MySQL pads to the requested decimals / returns DECIMAL; SQLite returns a bare float. |
| `IF`                                   | ✅     | Renamed on emit to the engine's `IIF`; semantics are identical (a NULL/zero condition is false). |
| `SUBSTRING` / `MID`                    | ✅     | Renamed on emit to `substr` (same behaviour). `SUBSTRING` also accepts the SQL-standard `SUBSTRING(str FROM pos [FOR len])` syntax; `MID` is comma-form only. |
| `LCASE` / `UCASE`                      | ✅     | Renamed on emit to `lower` / `upper`. |
| `CHAR_LENGTH` / `CHARACTER_LENGTH`     | ✅     | Renamed on emit to `length` (a character count). Distinct from `LENGTH`, which counts bytes and stays excluded. |
| `YEAR` / `MONTH` / `DAY` / `DAYOFMONTH` / `HOUR` / `MINUTE` / `SECOND` | ✅ | Date-part extractors, lowered to `CAST(strftime(fmt, x) AS INTEGER)`; return the integer component (no zero-padding) like MySQL for the standard `YYYY-MM-DD HH:MM:SS` format. |
| `EXTRACT(unit FROM x)`                  | 🚧     | The SQL-standard extractor. `YEAR`/`MONTH`/`DAY`/`HOUR`/`MINUTE`/`SECOND` use the strftime lowering, `WEEK` the default Sunday-first mode (like `WEEK(x)`), and `QUARTER` is `(month + 2) / 3` (like `QUARTER(x)`). `MICROSECOND` and the compound units (`YEAR_MONTH`, `DAY_HOUR`, …) are rejected. |
| `DATE_ADD` / `DATE_SUB` (`INTERVAL n unit`) | 🚧 | Lowered to the engine's `datetime(x, '±n unit')` modifier. `unit` ∈ `DAY`/`WEEK`/`MONTH`/`QUARTER`/`YEAR`/`HOUR`/`MINUTE`/`SECOND` (`WEEK`→7 days, `QUARTER`→3 months); the interval value must be an integer literal. Matches MySQL for DATETIME arguments. Diverges on a bare DATE (the engine keeps the `00:00:00` time) and on `MONTH`/`QUARTER`/`YEAR` arithmetic that overflows a month end (MySQL clamps, the engine rolls over). |
| `TIMESTAMPADD(unit, n, dt)`             | 🚧     | Shifts `dt` by `n` units — the same lowering as `DATE_ADD(dt, INTERVAL n unit)` (same units, same integer-literal requirement, same DATE/month-end divergences). The counterpart to `TIMESTAMPDIFF`. |
| `ADDDATE` / `SUBDATE`                   | 🚧     | The `INTERVAL` form is identical to `DATE_ADD`/`DATE_SUB`. The integer-days form (`ADDDATE(d, n)`) shifts by `n` whole days, lowered to `datetime(d, printf('%+d days', ±n))` with a NULL-day guard; `n` may be any expression. Same DATETIME-vs-DATE divergence as `DATE_ADD`. |
| `DATE_FORMAT(x, fmt)`                   | 🚧     | Lowered to `strftime()` for the directly-translatable specifiers (`%Y %m %d %H` pass through; `%i`→`%M`, `%s`→`%S`; `%j` day-of-year, `%w` weekday-number, `%U` Sunday-first week pass through; `%v`→`%V` ISO week; `%T`→`%H:%M:%S`; `%%` literal), and the name specifiers `%M` (month name), `%b` (abbreviated month), `%W` (weekday name), `%a` (abbreviated weekday) are expanded to `CASE` lookups and concatenated (English names only), the no-leading-zero numeric specifiers `%e` (day), `%c` (month), `%k` (hour) become integer casts of the strftime code, and the 12-hour clock `%l` (no pad) / `%h`/`%I` (padded), meridiem `%p` (AM/PM), and the day-with-ordinal-suffix `%D` (`1st`, `2nd`, …) become `CASE` expressions. The format must be a string literal. Specifiers with none of these forms (`%r` 12-hour time, `%f` microseconds, `%X`/`%x` week-year, `%u`/`%V` other week modes, …) are rejected rather than silently mistranslated. |
| `NOW` / `CURRENT_TIMESTAMP` / `UTC_TIMESTAMP` / `LOCALTIME` / `SYSDATE` | 🚧 | Current datetime, lowered to `datetime('now')`. **Always UTC** — the engine has no session time zone, so this diverges from MySQL's session-local `NOW()`. The no-argument form only (a fractional-seconds precision arg is not supported). |
| `CURDATE` / `CURRENT_DATE` / `CURTIME` / `CURRENT_TIME` / `UTC_DATE` / `UTC_TIME` | 🚧 | Lowered to `date('now')` / `time('now')`; UTC, as above. |
| `UNIX_TIMESTAMP([d])` / `FROM_UNIXTIME(n)` | 🚧 | Lowered to `unixepoch(d)` (or `unixepoch('now')`) and `datetime(n, 'unixepoch')`. Absolute conversions are UTC (the engine has no session time zone), so they diverge from MySQL's session-local values, but the two are inverses on both targets, so a round trip is zone-independent. `FROM_UNIXTIME`'s two-argument formatting form is not supported. |
| `TIME_TO_SEC` / `SEC_TO_TIME`          | 🚧     | Seconds since midnight and its inverse. `TIME_TO_SEC(t)` lowers to `H*3600 + M*60 + S` (each `CAST(strftime(code, t) AS INTEGER)`); `SEC_TO_TIME(s)` to `time(s, 'unixepoch')`. Matches MySQL for the normal time-of-day range; MySQL's out-of-range `TIME` values (up to 838 h, negatives) and `SEC_TO_TIME` outputs past `24:00:00` wrap in the engine. |
| `DAYNAME` / `MONTHNAME`                 | ✅     | The English weekday / month name, mapped from `strftime('%w', d)` (0=Sunday..6=Saturday) / `strftime('%m', d)` (1..12) by a `CASE` (no `ELSE`, so NULL propagates). Fills the gap left by `DATE_FORMAT`'s unsupported `%W`/`%M` specifiers. English names only (the engine has no locale). |
| `TO_DAYS` / `FROM_DAYS`                 | ✅     | Day number (days since year 0) and its inverse. `TO_DAYS(d)` is `CAST(julianday(date(d)) AS INTEGER) - 1721059` (the `date()` drops the time part); `FROM_DAYS(n)` is `date(n + 1721059.5)`. The offset shifts the engine's Julian day onto MySQL's proleptic-Gregorian count (meaningful for modern dates, as in MySQL). |
| `MAKEDATE(year, dayofyear)`             | ✅     | Builds a date from a year and a 1-based day of year, lowered to `date(printf('%04d-01-01', year), printf('%+d days', dayofyear - 1))`. Day 1 is Jan 1, a `dayofyear` past the year length rolls over, and a NULL argument or `dayofyear < 1` yields NULL (guarded by a `CASE`). |
| `MAKETIME(hour, minute, second)`       | 🚧     | Builds a time string, lowered to `printf('%02d:%02d:%02d', …)` guarded by a `CASE` that returns NULL for a NULL argument or `minute`/`second` outside `0..59` (as MySQL does). The hour may exceed 23. A negative hour (`-1:..` vs MySQL `-01:..`) and an hour past 838 (MySQL clamps) diverge. |
| Other date/time functions (`STR_TO_DATE`, `CONVERT_TZ`, `PERIOD_ADD`, …) | ❌ | **Excluded** — format/type or timezone differences. (`DATEDIFF`/`TIMESTAMPDIFF` *are* supported; see their own tests.) |
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
| SHOW TABLE STATUS [LIKE *pat*] | ✅ | One row per table in MySQL's 18-column shape (`Name`, `Engine`, …, `Collation`, …), optionally filtered by a `LIKE` pattern; an unknown table yields an empty result set. `Rows` is the table's real `COUNT(*)` and `Collation` is the fixed `utf8mb4_general_ci`; the columns the engine does not track — sizes (`Data_length`, `Index_length`, …), timestamps, `Auto_increment` — are reported as `0` or NULL, and `Engine`/`Row_format` are fixed values, so those differ from a real mysqld. Used by WordPress (`maybe_convert_table_to_utf8mb4`, Site Health). The `WHERE` filter form is not handled (rejected). |
| SHOW [FULL] TABLES [LIKE]  | ✅     | Base table names synthesized from the schema, optionally filtered by a `LIKE` pattern; `FULL` adds a `Table_type` column (`BASE TABLE`). The result column header is a fixed `Tables_in_database`, not MySQL's `Tables_in_<db>` (clients read it positionally). The `WHERE` filter form is rejected. |
| SHOW TRIGGERS              | ❌     |         |
| SHOW [GLOBAL\|SESSION] VARIABLES [LIKE *pat*] | ✅ | Answered from a fixed table of plausible system-variable values (the same table that backs `SELECT @@var`), returned as MySQL's `Variable_name` / `Value` result set ordered by name. The optional `GLOBAL`/`SESSION` scope is accepted and ignored, the `LIKE` pattern uses case-insensitive SQL wildcards (`%`, `_`), and an unknown variable yields an empty result set. Used by WordPress Site Health (`WP_Debug_Data::get_mysql_var`). Only the listed variables are reported (a real mysqld exposes hundreds more), and values are front-end constants, so the bare `SHOW VARIABLES` row set and individual values may differ from a given server. The `WHERE` filter form is not handled (rejected). |
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
