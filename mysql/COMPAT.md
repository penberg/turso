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
| `ERR_Packet`                   | 🚧     | Encoded with the mapped error code and SQLSTATE (below), defaulting to `1105`/`HY000`. |
| `EOF_Packet`                   | ❌     | Encoded; used to terminate column lists and result sets.    |
| MySQL error code mapping       | 🚧     | The common cases carry their real code: duplicate key → `1062` (`ER_DUP_ENTRY`), NULL into a `NOT NULL` column → `1048`, missing table → `1146`, missing column → `1054`, `CREATE` of an existing table → `1050`, syntax error → `1064`, unsupported statement → `1235`. Other engine errors still collapse to the generic `1105`. WordPress's `$wpdb` branches on `mysql_errno()` (e.g. duplicate-key on insert). |
| SQLSTATE mapping               | 🚧     | Set for the mapped codes (`23000` for `1062`/`1048`, `42S02` for `1146`, `42S22` for `1054`, `42S01` for `1050`, `42000` for syntax); otherwise `HY000`. |
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
| ALTER TABLE                            | ⚠️     | ADD/DROP COLUMN, ADD [UNIQUE] KEY/INDEX, RENAME [COLUMN] supported. `ADD COLUMN` accepts a trailing `FIRST` / `AFTER col` position, but the engine always appends, so the position is ignored (column access is by name; `SHOW COLUMNS` order differs from MySQL). ADD FULLTEXT degrades to a plain index (no MATCH...AGAINST). ADD PRIMARY KEY (cols) is emulated by a `CREATE UNIQUE INDEX` over the key columns (the engine cannot add a real in-place rowid primary key): the statement succeeds and the key's uniqueness is enforced, but the index is reported by `SHOW INDEX` under a `<table>_primary` name rather than MySQL's `PRIMARY`, the columns are not made implicitly NOT NULL, and a repeated `ADD PRIMARY KEY` errors on the duplicate index name. DROP PRIMARY KEY is the inverse — it drops that `<table>_primary` index, so an ADD/DROP cycle round-trips; it does not apply to a primary key declared in CREATE TABLE (the engine's rowid alias, which has no such index). The comma-separated multi-operation form (`ADD a, ADD KEY ..., DROP b`) is expanded into one statement per operation, run in sequence (not atomic — operations before a failing one still apply). A pure table-option ALTER (`ENGINE=`, `CONVERT TO CHARACTER SET ...`, `DEFAULT CHARSET=`, `ROW_FORMAT=`, `AUTO_INCREMENT=`, `COMMENT=`, …) is accepted as a **no-op** — these have no effect on the engine's fixed storage / single charset, as on `CREATE TABLE` — so WordPress's `CONVERT TO CHARACTER SET utf8mb4` and plugin `ENGINE=` succeed without changing the table (`AUTO_INCREMENT=` does not reseat the counter, a documented divergence). `ADD CONSTRAINT [symbol] {UNIQUE\|PRIMARY KEY} (cols)` reuses the index lowering (the symbol name is dropped). `CHANGE [COLUMN] old new <def>` that **renames** (`old` ≠ `new`) lowers to `RENAME COLUMN old TO new`, discarding the redeclared type — which is advisory, since the engine's columns are affinity-typed (a same-affinity retype is a no-op; a fundamental type change is not applied). A same-name `CHANGE` and the `MODIFY` form are pure in-place type changes the engine cannot do and are rejected. ADD FOREIGN KEY / CHECK / SPATIAL, DROP FOREIGN KEY, and `ALGORITHM`/partitioning operations are not translated. |
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
| DO *expr* [, *expr*]...                | 🚧     | Accepted and replied to with OK and no result set, matching MySQL. The expressions are parsed for validation but **not evaluated** -- MySQL's usual `DO` targets (locking functions, `SLEEP`, user-variable assignments) have no engine equivalent, so there is nothing to run. Not observable through the OK response. |

#### CREATE TABLE column attributes

| Attribute | Status | Comment |
|-----------|--------|---------|
| `NOT NULL` / `NULL`         | ✅ | A `NOT NULL` column with no explicit `DEFAULT` is given MySQL's implicit type default (`0` for numeric types, `''` for string/binary types) so a row that omits it still inserts — matching MySQL's default non-strict `sql_mode`, the mode WordPress runs under (a strict `sql_mode` would instead reject the row). `AUTO_INCREMENT` and `PRIMARY KEY` columns are excluded (the engine generates / rowid-handles their values). Date/time, `ENUM`/`SET`, `JSON`, and unrecognized types stay strictly `NOT NULL` (their MySQL defaults — the zero date, the first enum member, … — don't map cleanly). The synthesized default surfaces as the column's `Default` in `SHOW COLUMNS`/`DESCRIBE` (`0`/`''`), whereas MySQL reports `NULL` there — a minor introspection divergence. |
| `DEFAULT <literal>`         | ✅ | Literal defaults; function/expression defaults are dropped to NULL. An explicit `DEFAULT` is kept as written (it suppresses the implicit `NOT NULL` default above). |
| `PRIMARY KEY` (inline / table-level, single column) | ✅ | |
| `PRIMARY KEY` (composite)   | ✅ | Parsed and forwarded; not valid with `AUTO_INCREMENT` (below). |
| `AUTO_INCREMENT`            | ✅ | Only on a single-column `PRIMARY KEY` (inline or table-level). The key column is retyped to `INTEGER` so the engine treats it as a rowid alias that auto-assigns sequential ids and never reuses them — identical to MySQL. MySQL's int width (`bigint(20)`, `int(11)`) is display-only and dropped. |
| `AUTO_INCREMENT` elsewhere  | ❌ | On a non-key column, a composite key, or more than one column: rejected as unsupported (MySQL would map differently). |
| Inline secondary `KEY` / `INDEX` (table-level) | 🚧 | An inline `[FULLTEXT\|SPATIAL] {KEY\|INDEX} [name] (cols)` in `CREATE TABLE` has no engine form, so each is stripped and re-emitted as a `CREATE INDEX` run right after the table (so a later `SHOW INDEX` reports it, as WordPress's `dbDelta` expects). An unnamed key is named `<table>_<first-col>`; `FULLTEXT`/`SPATIAL` degrade to a plain index; a column prefix length (`col(191)`) is dropped (the whole column is indexed). A **named** inline `UNIQUE KEY name (cols)` likewise becomes a deferred `CREATE UNIQUE INDEX` under that name (so `SHOW INDEX` reports it by name, as `dbDelta` looks up), still enforcing uniqueness; an *unnamed* `UNIQUE (cols)` stays a `UNIQUE` table constraint whose index the engine auto-names. `FOREIGN KEY` is dropped (no engine equivalent). Index names share a per-database namespace, so the same name on two tables collides. |
| `ENUM(...)` / `SET(...)`     | 🚧 | Both store a string in MySQL, so both lower to `TEXT` (the engine has no such types, and `SET` is a reserved keyword there that cannot be a type name). The column accepts and round-trips any string value; the **allowed-values list is not enforced**, MySQL's `SET` reordering/deduplication of multi-element values is not applied, and `SHOW COLUMNS` reports the type as `text` rather than `enum(...)`/`set(...)`. |
| `CHECK (expr)` (column / table) | 🚧 | A `CHECK` constraint is passed through to the engine, which **enforces** it like MySQL 8.0.16+ (a violating `INSERT`/`UPDATE` is rejected). Both the inline column form and the table-level `[CONSTRAINT name] CHECK (...)` form are supported, including over multiple columns. If the `CHECK` expression uses something the front-end cannot translate (e.g. an unsupported function), it is dropped rather than failing `CREATE TABLE`, so that constraint is not enforced — a documented fallback. `ALTER TABLE ... ADD CHECK` is still not translated (see ALTER TABLE). |
| `COLLATE` / `CHARACTER SET` | 🚧 | A character column (`CHAR`/`VARCHAR`/`TEXT` family, and `ENUM`/`SET`) is declared `COLLATE NOCASE` so its comparisons, `ORDER BY`, `DISTINCT`, `GROUP BY`, `LIKE`, and `UNIQUE`/index lookups fold ASCII case — matching MySQL's default case-insensitive `utf8mb4_general_ci`. A column given an explicit case-sensitive collation (`COLLATE utf8mb4_bin`, any `_bin`/`_cs`) keeps the engine's case-sensitive `BINARY` comparison; `BLOB`/`BINARY`/`VARBINARY` are always case-sensitive. Only ASCII A–Z folds, not the full Unicode set MySQL's `_ci` collations fold; the per-column charset is otherwise ignored (single UTF-8 charset). A literal-to-literal comparison (`'a' = 'A'`) still uses the engine default and stays case-sensitive, unlike MySQL — but a column-to-value comparison, the common case, folds correctly. |

### Data Manipulation Statements

| Statement                              | Status | Comment |
|----------------------------------------|--------|---------|
| CALL                                   | ❌     |         |
| DELETE FROM tbl [WHERE] (single table) | ✅     |         |
| DELETE `t1[, t2, ...] FROM <refs> [WHERE]` (multi-table) | ✅ | Lowered to `DELETE FROM <table> WHERE rowid IN (SELECT t1.rowid FROM <refs> [WHERE] [UNION SELECT t2.rowid ...])`. The `rowid` subquery (including the `UNION` over every target) is materialized before any row is deleted, so it matches MySQL without a two-phase delete. Targets may be table names or `FROM` aliases; the join may be comma or `JOIN ... ON`. **All targets must resolve to the same table** (e.g. WordPress's transient-cleanup self-join); targets on different tables are rejected. |
| DELETE `... LIMIT n` (single table)    | ✅     | The count-only `LIMIT` caps the rows deleted (no `OFFSET`). Without an `ORDER BY` the affected rows are unspecified on both MySQL and the engine, so they match. |
| DELETE `... ORDER BY ... LIMIT n` (single table) | ✅ | The engine cannot order a `DELETE` in place, so the ordering and row cap are folded into a `WHERE rowid IN (SELECT rowid FROM tbl [WHERE ...] ORDER BY ... LIMIT n)` subquery, which selects exactly the rows MySQL would delete (the subquery is materialized first). |
| DELETE `... USING`                     | ❌     | **Not supported.** |
| DO                                     | ❌     |         |
| EXCEPT clause                          | ❌     |         |
| HANDLER                                | ❌     |         |
| IMPORT TABLE                           | ❌     |         |
| INSERT ... VALUES (basic)              | ✅     | Multi-row `VALUES` supported. The `DEFAULT` keyword may stand in for any value (`VALUES (1, DEFAULT)`), inserting that column's declared default. The empty form `INSERT INTO t () VALUES ()` (and the column-list-less `INSERT INTO t VALUES ()`) inserts one all-defaults row, lowered to the engine's `DEFAULT VALUES`. (`DEFAULT` is honored only in `INSERT ... VALUES`, not in `UPDATE ... SET`, and the `DEFAULT(col)` function form is unsupported.) |
| INSERT ... SET                         | ✅     | The `INSERT [INTO] t SET col = expr, ...` assignment form, built as the equivalent `(cols) VALUES (exprs)`. |
| INSERT ... SELECT                      | ✅     | `INSERT [INTO] t [(cols)] SELECT ...`; the query runs through the same SELECT subset, evaluated by the engine. |
| INSERT ... ON DUPLICATE KEY UPDATE     | ✅     | Lowered to the engine's target-less upsert (`ON CONFLICT DO UPDATE SET ...`), which fires on any unique/primary-key conflict like MySQL. The `VALUES(col)` pseudo-function (the would-be-inserted value) is mapped to `excluded.col` anywhere in the assignment expression (e.g. `c = c + VALUES(c)`, `GREATEST(c, VALUES(c))`); a bare column on the right refers to the existing row. The MySQL 8.0.19+ **row-alias** form `VALUES (...) AS alias [(col1, ...)]` is also supported: in the `UPDATE`, `alias.col` (or a bare column alias from the optional list) is the new value and lowers to the same `excluded.col`, replacing the now-deprecated `VALUES()`. |
| INSERT/UPDATE/DELETE/REPLACE modifiers | ✅ | The priority/scheduling hints `LOW_PRIORITY`, `DELAYED`, `HIGH_PRIORITY`, and `QUICK` are accepted and ignored (no result effect). `INSERT IGNORE` and `UPDATE IGNORE` lower to the engine's `INSERT OR IGNORE` / `UPDATE OR IGNORE` (a row whose change would violate a constraint is skipped instead of aborting the statement). `DELETE IGNORE` is a no-op (the engine raises no per-row delete errors here). |
| INTERSECT clause                       | ❌     |         |
| LOAD DATA                              | ❌     |         |
| LOAD XML                               | ❌     |         |
| Parenthesized Query Expressions        | ❌     |         |
| REPLACE ... VALUES                     | ✅     | `REPLACE [INTO] tbl ... VALUES ...` lowers to the engine's `INSERT OR REPLACE`: a row conflicting on a primary/unique key is deleted before the new row is inserted, like MySQL. The `REPLACE ... SET` and `REPLACE ... SELECT` forms are not supported. |
| SELECT (single table, WHERE/ORDER BY/LIMIT) | ✅ |         |
| `SELECT ... FROM DUAL`                 | ✅     | MySQL's dummy single-row table: a lone unaliased `DUAL` in the `FROM` clause is dropped, leaving a `FROM`-less select (`SELECT 1 FROM DUAL` ≡ `SELECT 1`). Supports the conditional-insert idiom `INSERT ... SELECT ... FROM DUAL WHERE NOT EXISTS (...)`. As in MySQL, an unquoted `dual` is always the dummy (a real table actually named `dual`, referenced without an alias, would be shadowed). |
| `TABLE tbl [ORDER BY] [LIMIT]`         | ✅     | MySQL 8's `TABLE` statement, shorthand for `SELECT * FROM tbl [ORDER BY ...] [LIMIT ...]`; lowered to exactly that. (`TABLE` as a derived table or `UNION` branch — `(TABLE t)`, `... UNION TABLE u` — is not modeled.) |
| `LIMIT count` / `LIMIT offset, count` / `LIMIT count OFFSET offset` | ✅ | All three spellings. A `LIMIT`/`OFFSET` literal above `i64::MAX` (MySQL allows up to `2^64-1`, and `LIMIT 18446744073709551615` is the "all remaining rows" idiom used after an `OFFSET`) is clamped to `i64::MAX`, which the engine's signed 64-bit bound represents and which still returns every remaining row. |
| SELECT ... GROUP BY [HAVING]           | ✅     | GROUP BY column expressions (not integer ordinals — those diverge). A standalone `HAVING` (no `GROUP BY`) works both ways: an aggregate condition (`HAVING COUNT(*) > 2`, or one that references an aggregate through its SELECT-list alias, `SELECT COUNT(*) c ... HAVING c > 2`) is a whole-table aggregate the engine evaluates directly, and a non-aggregate condition (a post-`WHERE` row filter, e.g. WordPress's custom-fields `SELECT DISTINCT meta_key ... HAVING meta_key NOT LIKE '_%'`) is folded into the `WHERE` clause, where the filtering is equivalent. |
| SELECT DISTINCT / DISTINCTROW          | ✅     | Both forms remove duplicate result rows; `DISTINCTROW` is MySQL's synonym for `DISTINCT` and is treated identically. |
| `SELECT SQL_CALC_FOUND_ROWS ...` + `SELECT FOUND_ROWS()` | 🚧 | The modifier is honored: the query returns its limited rows, and a following `FOUND_ROWS()` on the same connection returns the count the query would return without its `LIMIT` (computed by re-running it without the limit). Drives `WP_Query` pagination. `FOUND_ROWS()` is only meaningful right after a `SQL_CALC_FOUND_ROWS` query — it is not updated after ordinary `SELECT`s. |
| Column aliases (`expr AS a` / `expr a`) | ✅    | Both the `AS` and bare forms; resolvable in `ORDER BY`/`GROUP BY`. A string-literal alias (`expr AS 'name'`) is also accepted; the elided string form (`expr 'name'`) is not (ambiguous with literal concatenation). |
| Default column labels                  | ✅     | An unaliased select-list expression is labelled with the **verbatim source text** of its expression, matching MySQL (`UPPER('abc')`, `COUNT(*)`, `a+b`, with the spacing as written), rather than the engine's re-rendered form (which would print stray spaces and the lowered function bodies, e.g. `UPPER ('abc')`, `length (CAST ('x' AS BLOB))`). A bare or qualified column reference is labelled by its column name (`t.a` → `a`); a string literal by its decoded value (`'it''s'` → `it's`); a hex literal by its verbatim source (`0x41`, `X'41'`); and a numeric/NULL literal by its value — all matching MySQL. |
| SELECT ... INTO                        | ❌     | **Not supported.** |
| `[INNER] JOIN` / `LEFT [OUTER] JOIN` / `RIGHT [OUTER] JOIN` ... `ON`/`USING` | ✅ | Table aliases (`t`, `t AS a`) and chained joins supported. Map identically onto the engine. |
| `CROSS JOIN` / `STRAIGHT_JOIN` / `NATURAL [LEFT\|RIGHT] JOIN` | ✅ | `CROSS JOIN` is the Cartesian product, `STRAIGHT_JOIN` lowers to a plain inner join (the join-order hint is dropped), and `NATURAL` joins on the common columns. Evaluated identically to MySQL. |
| Inner / plain `JOIN` without `ON`/`USING` | ✅ | A `JOIN` / `INNER JOIN` / `STRAIGHT_JOIN` with no condition is a cross join (MySQL treats these as equivalent to `CROSS JOIN`), typically with the predicate in `WHERE`. Only a non-NATURAL OUTER (`LEFT`/`RIGHT`) join still requires an explicit condition. |
| Comma join (`FROM a, b WHERE ...`)     | ✅     | Implicit cross join with the condition in `WHERE`; the engine evaluates it identically to MySQL. Used by WordPress term/post-count queries. |
| `FULL [OUTER] JOIN`                     | ❌     | MySQL has no `FULL JOIN`, so it is rejected (not accepted as an extension). |
| Index hints (`{USE\|FORCE\|IGNORE} {INDEX\|KEY} [FOR ...] (...)`) | ✅ | Parsed and **ignored** on any table reference (base or joined): they only steer MySQL's optimizer, and the engine plans its own access path, so the result set is unchanged. The empty `USE INDEX ()` list, the `FOR {JOIN\|ORDER BY\|GROUP BY}` scope, and `PRIMARY` as a name are all accepted. |
| UNION / UNION ALL / UNION DISTINCT     | ✅     | `UNION` deduplicates, `UNION ALL` does not; the explicit `UNION DISTINCT` is the same as `UNION` (DISTINCT is the default). A trailing `ORDER BY`/`LIMIT` applies to the whole result. Identical to MySQL 8.x. Branches may be parenthesized — `(SELECT ...) UNION (SELECT ...)`, including a leading parenthesis — and the grouping parens are stripped; a per-branch `ORDER BY`/`LIMIT` inside the parentheses is rejected (not representable in the flat compound model). |
| INTERSECT / EXCEPT set operations      | ✅     | Deduplicating set operations, identical to MySQL 8.x; the explicit `DISTINCT` quantifier is accepted (the default). Mixed-operator precedence is not exercised. |
| Subqueries                             | ✅     | `IN (SELECT ...)`, `[NOT] EXISTS (SELECT ...)`, scalar `(SELECT ...)` in expressions, and derived tables in `FROM` — including correlated forms. See the Expressions section. |
| Quantified comparison (`= ANY` / `<> ALL`) | 🚧 | The two quantified subquery comparisons that are exactly equivalent to `IN` / `NOT IN`: `x = ANY (subquery)` (and its synonym `= SOME`) lowers to `x IN (subquery)`, and `x <> ALL (subquery)` (and `!= ALL`) to `x NOT IN (subquery)` — including the empty-subquery and NULL semantics, which match. The other operator/quantifier pairs (`> ANY`, `>= ALL`, `= ALL`, `<> ANY`, …) need MIN/MAX or EXISTS rewrites with subtler NULL/empty-set behaviour and are rejected rather than mistranslated. |
| `WITH ... SELECT` (CTEs)               | 🚧     | A `WITH` clause of one or more named CTEs (each with an optional `(col, ...)` rename list) feeding a `SELECT`; evaluated like SQLite, matching MySQL for non-recursive CTEs. `WITH RECURSIVE` parses but the engine does not yet execute recursive CTEs; the SQLite `MATERIALIZED` hint is accepted but is not MySQL syntax; `WITH` before `UPDATE`/`DELETE` is not supported. |
| Derived / lateral derived tables       | ❌     |         |
| TABLE statement                        | ❌     |         |
| UPDATE tbl SET ... [WHERE] (single table) | ✅  |         |
| UPDATE `... LIMIT n` (single table)    | ✅     | The count-only `LIMIT` caps the rows updated (no `OFFSET`); the affected rows are unspecified without an `ORDER BY`, matching MySQL. |
| UPDATE `... ORDER BY ... LIMIT n` (single table) | ✅ | Rewritten the same way as the `DELETE` form: `... WHERE rowid IN (SELECT rowid FROM tbl [WHERE ...] ORDER BY ... LIMIT n)`, so the n rows updated are the ones MySQL would pick by sort order. |
| UPDATE `t1, t2, ... SET t1.col = expr [WHERE]` (multi-table) | 🚧 | The comma form is lowered to the engine's `UPDATE t1 SET col = expr FROM <the other tables> WHERE ...`, which joins the sources to the target and updates only the matching `t1` rows (rows with no join match are unchanged), exactly as MySQL does. The target is the first-listed table and may be aliased (`UPDATE a x, b y SET x.v = y.v`); `SET` columns may be bare or qualified with the target. **Only the first-listed table can be updated** — a `SET` assigning to another table is rejected — and the explicit-`JOIN` spelling (`UPDATE a JOIN b ON ... SET ...`) is not yet translated (use the comma form). `ORDER BY`/`LIMIT` (which MySQL disallows on a multi-table update) are rejected. |
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
| Arithmetic `+` `-` `*`                 | ✅     | The binary operators, plus **unary** `-` / `+` on any expression (`-a`, `-ABS(x)`, `-(a + 1)`, `ABS(-a)`, `ORDER BY -a`). Unary minus binds tightly (`-a * b` is `(-a) * b`); a signed numeric literal (`-5`) is folded into the literal. |
| `[NOT] IN (value list)`                | ✅     | Includes the empty list: `x IN ()` folds to `0` and `x NOT IN ()` to `1` (MySQL semantics), since the engine has no empty-list `IN`. |
| `[NOT] BETWEEN a AND b`                | ✅     |         |
| `[NOT] LIKE` (ASCII patterns)          | ✅     | Backslash is the default escape character (so `\%` / `\_` match literally), as in MySQL — the front-end supplies `ESCAPE '\'` when the query gives no explicit `ESCAPE` clause. An explicit `LIKE ... ESCAPE 'c'` is honored. This is what `$wpdb->esc_like()` relies on. |
| `[NOT] REGEXP` / `RLIKE`               | 🚧     | Mapped to the engine's `REGEXP` operator (Rust `regex` crate). Case-insensitive like MySQL's default (the pattern is prefixed with the regex crate's `(?i)` flag), but the regex dialect still differs from MySQL's for advanced constructs. |
| `REGEXP_LIKE(str, pat[, match_type])`  | 🚧     | The functional form of `REGEXP`, lowered to the same engine `REGEXP`. Defaults to case-insensitive like the operator; the optional `match_type` string literal sets the inline flags — `c` case-sensitive, `i` case-insensitive, `m` multi-line, `n` dot-matches-newline (`u` accepted and ignored), an unknown flag rejected. The `match_type` must be a literal so the flags are known at translation. NULL `str`/`pat` yields NULL. Same regex-dialect caveat as `REGEXP`. |
| `CASE` (searched and simple forms)     | ✅     | `CASE WHEN ... THEN ... [ELSE ...] END` and `CASE expr WHEN ... END`; standard SQL, identical. |
| `expr COLLATE collation_name`          | 🚧     | The `COLLATE` postfix maps the MySQL collation onto the engine collation that compares the same way: a case-sensitive `_bin`/`_cs` collation (or `binary`) → `COLLATE BINARY`, any other (the `_ci` default) → `COLLATE NOCASE`. So `ORDER BY x COLLATE utf8mb4_bin` sorts case-sensitively and `... COLLATE utf8mb4_general_ci` case-insensitively, overriding the column's collation. ASCII case folding only (not the full Unicode set MySQL folds). The collation name must be an identifier (`COLLATE 'string'` is rejected). |
| `doc -> path` / `doc ->> path` (JSON)  | ✅     | The JSON extract operators, mapped onto the engine's identical `->` / `->>`: `->` returns the JSON value at `path` keeping its quoting (a string comes back as `"x"`), `->>` returns the unquoted scalar (`x`). The path is a quoted path literal (`'$.a'`, `'$.c[1]'`); a missing path yields NULL. They bind tightly (so `doc ->> '$.a' = 'x'` groups as `(doc ->> '$.a') = 'x'`) and chain left-to-right. |
| `JSON_VALID(x)`                         | ✅     | Returns 1 if `x` is a valid JSON document, 0 if not, and NULL if `x` is NULL — the engine's identical `json_valid` (renamed on emit). The JSON *builders* (`JSON_OBJECT`, `JSON_ARRAY`) stay unsupported: the engine serializes them compactly (`{"k":1}`) whereas MySQL inserts spaces (`{"k": 1}`), so their text output would diverge. |
| `CAST(expr AS type)`                   | 🚧     | Real cast syntax (not a function). Targets map onto engine affinity: `CHAR`→text, `SIGNED`/`UNSIGNED`→integer, `DECIMAL`→numeric, `DOUBLE`/`FLOAT`/`REAL`→real, `BINARY`→blob. Length/precision (`CHAR(n)`, `DECIMAL(m,d)`) parses but is **not enforced**, integer rounding of fractional values differs from MySQL (truncates vs rounds), and `UNSIGNED` is not distinguished from `SIGNED`. Date/time and `JSON` targets are rejected. |
| Temporal literals (`DATE '...'`, `TIME '...'`, `TIMESTAMP '...'`) | ✅ | The typed temporal literal (a `DATE`/`TIME`/`TIMESTAMP` keyword directly before a quoted string) is lowered to `date`/`time`/`datetime` of the string, which normalizes it as MySQL does (`TIMESTAMP '2026-03-01'` → `2026-03-01 00:00:00`) and compares equal to the plain string. The keyword followed by `(` is instead the date/time *function*, and the keyword not before a string is an ordinary identifier, so a column named `date` is unaffected. MySQL has no `DATETIME '...'` literal, so that form is not added. |
| `CONVERT(expr USING charset)` / `CONVERT(expr, type)` | 🚧 | `USING charset` is charset coercion: the engine is single-charset (UTF-8), so the charset is dropped and the value passes through unchanged. `CONVERT(expr, type)` is identical to `CAST(expr AS type)` (same mapping and divergences). |
| `/` (division)                         | ✅     | Lowered to `CAST(a AS REAL) / b`, forcing MySQL's float division (`5 / 2` = `2.5`, not the engine's truncating integer division) and yielding NULL on division by zero. Two display/precision edges vs MySQL: the engine renders the quotient as a plain double where MySQL prints a fixed-scale DECIMAL (`2.5` vs `2.5000`), and a non-terminating quotient (`10 / 3`) carries full double precision rather than MySQL's default 4-decimal scale. The numeric value matches for terminating quotients. |
| `a % b` / `a MOD b` / `MOD(a, b)` / `a DIV b` | ✅ | The `%`/`MOD` modulo operators, the `MOD(a, b)` function, and the `DIV` integer-division operator are all lowered to integer arithmetic (`a - b * CAST(a / b AS INTEGER)` and `CAST(a / b AS INTEGER)`), which matches MySQL for both integer and float operands — including the sign-of-dividend rule and the exact float remainder (`5.5 % 2` = `1.5`). The symbolic `%` is lowered this way rather than passed to the engine's own `%`, which would truncate float operands. |
| `\|\|`                                 | ❌     | **Excluded** — MySQL `\|\|` is logical OR; SQLite `\|\|` is string concat. |
| `&` / `\|` / `^` / `<<` / `>>` / `~` (bitwise) | 🚧 | Bitwise AND / OR / XOR, left / right shift, and the unary NOT `~` (a tight prefix, like the other unary operators), mapped to the engine's equivalents — except `^` (XOR), which the engine has no operator for and is lowered to `(a & ~b) \| (~a & b)`. Precedence (tight → loose): `~`, `^`, `<<`/`>>`, `&`, `\|`; all tighter than comparison and looser than `+`/`-` (`^` is also tighter than `*`), as in MySQL. MySQL evaluates on unsigned 64-bit integers and the engine on signed, so a result with bit 63 set prints differently — notably a bare `~x`, which always sets bit 63 (`~5` is `-6` here vs MySQL's `18446744073709551610`, the same bits) — but masked/combined results (`5 & ~1`, `(~x) & 255`, `5 ^ 3`) and small non-negative operands match. (A `0x..` mask is a *binary string*, not an integer — see the hex-literal row — so `& 0xFF` does not mask; use the decimal `255`.) |
| Hex literals (`0x41`, `X'41'`)         | 🚧     | Lexed into the engine's blob literal (`0x41` and `X'41'`/`x'41'` both hold the same hex digits); an odd-length `0x..` is left-padded to even as MySQL does, and an odd-length `X'..'` is rejected. They evaluate as the **bytes** they encode (`0x48656C6C6F` is `Hello`), matching MySQL in a string/binary context — display, `HEX()`, `LENGTH()`, storing into a column, and blob-to-blob comparison. In a numeric / bitwise context MySQL coerces a hex literal to its integer value while the engine's blob coerces to `0` (`0x41 + 0` is `65` on MySQL, `0` here; likewise `& 0xFF`, `CAST(0x41 AS UNSIGNED)`), and a hex literal does not compare equal to an equivalent *string* literal (`0x41 = 'A'` is `1` on MySQL, `0` here) — documented divergences of the blob representation. |
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
| `ANY_VALUE`                            | ✅     | `ANY_VALUE(x)` returns `x` — in MySQL it marks a non-aggregated column as intentionally unconstrained so `ONLY_FULL_GROUP_BY` does not reject it (returning a value from some row of each group). The engine already permits a bare column in a `GROUP BY` query with the same "any value from the group" semantics, so the wrapper is dropped. |
| `ABS`                                  | ✅     |         |
| `CEIL` / `CEILING` / `FLOOR` / `ROUND` | ✅ | Whole-number rounding, backed by the engine's `ceil`/`floor`/`round` (`CEILING`→`ceil`). The engine returns a real, but these results are integral and MySQL types them as integers, so the front-end wraps them in `CAST(... AS INTEGER)` to print `6` rather than `6.0` (the integer also composes cleanly in arithmetic). `ROUND(x)` and `ROUND(x, 0)` are integers; `ROUND(x, d)` with `d > 0` keeps `d` decimal places (a real). NULL propagates. Two edges: a magnitude above 2^63 saturates the cast (MySQL keeps it as a double), and a negative `d` in `ROUND(x, -d)` is not rounded to tens (the engine has no negative-scale round). |
| `POW` / `POWER` / `SQRT` / `EXP` / `LN` | ✅ | Numeric functions backed by the engine's identically-named ones (`POWER`→`pow`); NULL propagates. One display difference: the engine renders an integer-valued result as a float (`POW(2,10)` → `1024.0`) where MySQL prints `1024`; the numeric value is the same (`POW(2,10) = 1024` holds on both). |
| `TRUNCATE(x, d)`                       | ✅     | Truncates `x` to `d` decimal places toward zero (distinct from `ROUND`), synthesized as `trunc(x * pow(10, d)) / pow(10, d)`. A literal `d <= 0` gives a whole number, which is cast to an integer to match MySQL (`TRUNCATE(3.7, 0)` is `3`, `TRUNCATE(1234.5678, -2)` is `1200`, and the integer composes in arithmetic); a positive `d` keeps the fractional part as a real, which renders as a plain double rather than MySQL's fixed-scale DECIMAL (the value matches). A non-literal `d` is always treated as the real case. NULL in either argument propagates. |
| `LOG` / `LOG2` / `LOG10` / `PI`        | ✅     | `LOG(x)` is the natural log (lowered to the engine's `ln`, since the engine's own one-arg `log` is base-10); `LOG(b, x)` is the base-`b` log; `LOG2`/`LOG10` and `PI()` map onto the engine's same-named ones. The engine evaluates the base-10/base-2 logs through natural logs, so an exact power lands a hair off (`LOG10(1000)` is `2.9999…` rather than MySQL's exact `3`) — equal after rounding to a few places. |
| `SIN` / `COS` / `TAN` / `ASIN` / `ACOS` / `ATAN` / `ATAN2` / `COT` / `DEGREES` / `RADIANS` | ✅ | Trigonometric functions (in radians) and angle conversions, mapped onto the engine's same-named ones. MySQL's two-argument `ATAN(y, x)` is lowered to the engine's `atan2(y, x)`. `COT(x)` (no engine builtin) is lowered to `1 / tan(x)`; `COT(0)` divides by zero (MySQL raises an out-of-range error there). Results are floating point, so they match MySQL after rounding. |
| `LOWER` / `UPPER`                      | ✅     | ASCII case folding. |
| `REPLACE`                              | ✅     | Replaces every occurrence, case-sensitively. |
| `REVERSE`                              | 🚧     | Reverses the characters of the string, mapped onto the engine's `string_reverse` (renamed on emit). NULL propagates and a number is reversed as its decimal string, as in MySQL. Exact for single-byte (ASCII) strings; diverges for a multi-byte character — MySQL reverses raw bytes (corrupting it) while the engine reverses whole characters. |
| `STRCMP(a, b)`                         | 🚧     | Returns -1 / 0 / 1 for `a < b` / `a = b` / `a > b`, and NULL if either argument is NULL — lowered to a `CASE` over the comparison operators, taken under `COLLATE NOCASE` so it folds ASCII case like MySQL's default collation (`STRCMP('a', 'A')` is `0`, matching MySQL). Non-ASCII case folding is not modeled, and numeric arguments compare numerically rather than as strings (`STRCMP(10, 9)` differs from MySQL — a rare edge). |
| `SUBSTR`                               | ✅     | 1-indexed, optional length, negative position from the end. Both the comma form `SUBSTR(str, pos[, len])` and the SQL-standard `SUBSTR(str FROM pos [FOR len])` are accepted. The out-of-range cases match MySQL (not SQLite): a position of `0`, a position more than `length(str)` before the start (`pos < -length`), or a negative length yields `''` (where SQLite's `substr` would return the whole string or a backward slice) — the lowering wraps the engine's `substr` in a `CASE` guard. |
| `INSTR` / `LOCATE` / `POSITION`        | 🚧     | 1-indexed position of the first match, or 0. `LOCATE(substr, str)` reverses `INSTR`'s operands, and `POSITION(substr IN str)` is the SQL-standard spelling of `LOCATE`. Lowered to `instr(lower(str), lower(substr))` so the match is case-insensitive like MySQL's default collation — exact for ASCII, non-ASCII case folding not modeled. The 3-arg `LOCATE(substr, str, pos)` searches from `pos` (lowered to an offset `instr` over `substr(str, pos)`); only `pos >= 1` matches MySQL. `INSTR` stays two-argument. |
| `TRIM`                                 | 🚧     | `TRIM(str)` and the `TRIM([{BOTH\|LEADING\|TRAILING}] [remstr] FROM str)` forms, lowered to the engine's `trim`/`ltrim`/`rtrim` (with `remstr` as the second argument). The two-argument engine trim removes any of the *characters* in `remstr`, so it matches MySQL for the default space or a single-character `remstr`; a multi-character `remstr` (which MySQL strips as a whole substring) diverges. |
| `INSERT(str, pos, len, newstr)`        | 🚧     | The string function (distinct from the `INSERT` statement; recognized in expression position). Replaces `len` characters of `str` from the 1-based `pos` with `newstr`, lowered to `CASE WHEN pos < 1 OR pos > length(str) THEN str ELSE substr(str, 1, pos-1) \|\| newstr \|\| substr(str, pos+len) END`. Out-of-range `pos` returns `str`, positions are per-character, and NULL propagates. A negative `len` is a documented edge. |
| `COUNT(*)` / `COUNT(expr)`             | ✅     | aggregate |
| `SUM` / `MIN` / `MAX` / `AVG`          | ✅     | aggregate. `AVG(expr)` is the mean of the non-NULL values (NULL over an empty/all-NULL group), backed by the engine's `avg`. The mean renders as a plain double where MySQL prints a DECIMAL padded to 4 places (`22.5` vs `22.5000`); the numeric value is the same. |
| `COUNT/SUM/MIN/MAX/AVG(DISTINCT expr)` | ✅     | The `DISTINCT` quantifier on an aggregate; `ALL` is the default and ignored. `DISTINCT` on a scalar function or with `*` is rejected. |
| `agg(...) OVER (...)` (window aggregate) | 🚧 | An aggregate (`SUM`/`COUNT`/`MIN`/`MAX`/`AVG`, including `COUNT(*)`) with an `OVER ( [PARTITION BY ...] [ORDER BY ...] )` clause is evaluated by the engine: `OVER ()` aggregates the whole result, `PARTITION BY` aggregates per group, and `ORDER BY` gives a running aggregate under MySQL's default frame. An explicit frame (`ROWS`/`RANGE`/`GROUPS ...`) and a named window (`OVER w` with a `WINDOW` clause) are rejected. Of the dedicated window functions only `ROW_NUMBER` is available (see its row); `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `NTILE`, `FIRST_VALUE`, … have no engine equivalent. |
| `ROW_NUMBER() OVER (...)`               | 🚧     | The row-numbering window function, passed through to the engine's `row_number`, which numbers each partition's rows 1, 2, 3, … in the window's `ORDER BY` order — identically to MySQL (verified against MySQL 8.4 with and without `PARTITION BY`, including the "first row per group" derived-table pattern). The `OVER ( [PARTITION BY ...] [ORDER BY ...] )` clause is parsed as for a window aggregate, so the same restrictions apply (no explicit frame, no named `WINDOW`). |
| `GROUP_CONCAT`                         | 🚧     | `GROUP_CONCAT([DISTINCT] expr [SEPARATOR 's'])`, lowered to the engine's `group_concat([DISTINCT] expr[, 's'])` (same default `,` separator). `DISTINCT` keeps the distinct values. Without an inner `ORDER BY` the concatenation order is unspecified in both (in practice the group's scan order). The inner `ORDER BY`, the multi-expression form, and `DISTINCT` together with a custom `SEPARATOR` (a `DISTINCT` engine aggregate takes only one argument) are rejected. |
| `CONCAT`                               | ✅     | Lowered to the engine's `\|\|` operator (not `concat()`): like MySQL, the result is NULL if any argument is NULL. Requires at least one argument. |
| `CHAR`                                 | 🚧     | Builds a string from integer character codes, mapped to the engine's `char()`. Exact for the common ASCII / control-character codes (`CHAR(10)`, `CHAR(72, 73)`→`HI`). Two divergences: MySQL skips NULL arguments while the engine stops at the first NULL, and for codes above 127 MySQL emits raw bytes (a number can span several) while the engine emits one UTF-8 code point. An optional trailing `USING charset` clause is parsed and ignored (the engine always builds from Unicode code points, matching MySQL's default `utf8mb4`). |
| `FIELD`                                | ✅     | Lowered to `CASE x COLLATE NOCASE WHEN a THEN 1 WHEN b THEN 2 ... ELSE 0 END` — the 1-based index of the first argument among the rest, or 0 if absent/NULL. The `COLLATE NOCASE` base folds ASCII case like MySQL's default collation (`FIELD('a', 'A', 'b')` is `1`); it is harmless for a numeric `x`. WordPress uses it for `ORDER BY FIELD(...)` (e.g. `orderby=post__in`). |
| `ELT`                                  | ✅     | The inverse of `FIELD`: lowered to `CASE n WHEN 1 THEN a WHEN 2 THEN b ... END` (no `ELSE`) — the n-th string argument (1-based), or NULL if n is out of range or NULL. Requires the index plus at least one string. |
| `MAKE_SET`                             | ✅     | The comma-separated set of the strings whose corresponding bit in the bitmask is set (`s1` for bit 0, `s2` for bit 1, …). Lowered to `CONCAT_WS(',', CASE WHEN bits & 1 THEN s1 END, CASE WHEN bits & 2 THEN s2 END, …)`: each string appears only when its bit is set, `CONCAT_WS` skips the unset slots and any NULL string, and an outer guard returns NULL for a NULL bitmask — all matching MySQL. Requires at least one string; strings past the 64th (unaddressable by the 64-bit mask) are dropped. |
| `INET_NTOA`                            | ✅     | Renders a 32-bit number as a dotted-quad IPv4 address, lowered to the per-octet `((n >> 24) & 255) \|\| '.' \|\| ((n >> 16) & 255) \|\| '.' \|\| ((n >> 8) & 255) \|\| '.' \|\| (n & 255)`. The `\|\|` concatenation propagates NULL, so `INET_NTOA(NULL)` is NULL, as in MySQL; values outside `0..2^32-1` are not meaningful (as in MySQL). The inverse `INET_ATON` is not supported (the front-end cannot split the dotted string without the engine's array functions). |
| `EXPORT_SET`                           | ✅     | `EXPORT_SET(bits, on, off[, sep[, n]])` writes `on`/`off` per low bit of `bits` (`sep` default `,`, `n` default 64, clamped to `0..64`), lowered to `CONCAT_WS(sep, CASE WHEN (bits >> 0) & 1 THEN on ELSE off END, …)` — each bit tested by `(bits >> i) & 1` so the 64th mask does not overflow the signed 64-bit integer. An outer guard returns NULL for a NULL `bits`/`on`/`off` (a NULL `sep` makes `CONCAT_WS` NULL), matching MySQL. `number_of_bits` must be an integer literal so the entry count is fixed at translation. |
| `BIT_COUNT`                            | ✅     | The number of set bits of `n`, taken as an unsigned 64-bit value, lowered to the sum of `(n >> i) & 1` over the 64 bits (the arithmetic shift reads the sign bit, so `BIT_COUNT(-1)` is 64). The terms are summed in a *balanced* tree of additions so the expression stays shallow (a left-nested 64-deep sum overflows the engine's evaluator). NULL propagates. |
| `BIN`                                  | ✅     | The base-2 string of `n` (unsigned 64-bit, no leading zeros). Builds the 64 bit characters most-significant first — each `CASE WHEN (n >> i) & 1 THEN '1' ELSE '0' END` (the arithmetic shift reads the sign bit, so `BIN(-1)` is 64 ones) — joins them with the engine's *flat* `concat` (the front-end `CONCAT` would nest `\|\|` 64 deep and overflow the evaluator) and strips leading zeros via `ltrim(…, '0')`, with a guard returning `'0'` for `n = 0` and NULL for a NULL argument. |
| `FIND_IN_SET`                          | 🚧     | The 1-based index of `str` in the comma-separated `strlist`, or 0; synthesized by comma-wrapping and counting commas in the matched prefix. Matches whole elements (not substrings) and is case-insensitive (ASCII, via `lower`), like MySQL's default collation. NULL propagates. A `str` that itself contains a comma returns 0 in MySQL but may match here — a minor documented edge. |
| `REPEAT`                               | ✅     | `REPEAT(s, n)` returns n copies of s. The engine has no `repeat()`, so it is synthesized as `replace(hex(zeroblob(n)), '00', s)`. A non-positive n gives the empty string; a NULL count is guarded to NULL (since `zeroblob(NULL)` is an empty blob, not NULL), and a NULL string propagates through `replace`. |
| `HEX`                                  | ✅     | Overloaded as in MySQL: the uppercase hexadecimal of a number (`HEX(255)` → `FF`) or the hex of a string's bytes (`HEX('A')` → `41`; a numeric *string* like `'255'` is still hexed as bytes). The two are told apart at runtime by the value's type — `printf('%X', x)` for an integer/real, else the engine's `hex(x)` — with a NULL guard. A non-integer numeric truncates toward zero rather than rounding, a minor edge. |
| `OCT`                                  | ✅     | `OCT(n)` is the octal string of `n`, synthesized as `printf('%o', n)` with a NULL guard. `n` is treated as an unsigned 64-bit value, so a negative `n` matches MySQL (`OCT(-8)` → `1777777777777777777770`). |
| `INTERVAL(n, n1, ...)`                 | ✅     | The bucketing function (distinct from the date-arithmetic `INTERVAL` keyword): returns how many of the ascending bounds `n` reaches or exceeds. Lowered to `(n>=n1)+(n>=n2)+...` (a comparison yields 1/0), with a NULL guard returning `-1` as MySQL does. At least one bound is required. |
| `UNHEX`                                | 🚧     | The inverse of `HEX` for the string case — decodes a hex string to the bytes it represents (`UNHEX('48656C6C6F')` → `Hello`), mapped onto the engine's `unhex`; a NULL or invalid/odd-length hex string yields NULL, as in MySQL. The result is a binary string (a BLOB), which renders like MySQL's binary string and round-trips through `HEX`, but compares against a plain text value differently (`UNHEX('41') = 'A'` is false here vs true on MySQL — a BLOB-vs-text divergence). |
| `SPACE`                                | ✅     | `SPACE(n)` is `REPEAT(' ', n)` — a run of n spaces, the empty string for a non-positive n, and NULL for a NULL n. Same synthesized lowering as `REPEAT`. |
| `MD5` / `SHA1` / `SHA` / `SHA2`        | 🚧     | The lowercase hex digest of a string. The engine has no builtin hashing, so the server loads the crypto extension and these lower to `lower(hex(crypto_md5(…)))` / `crypto_sha1` / `crypto_sha256`·`crypto_sha384`·`crypto_sha512` (selected by `SHA2`'s bit-length argument — 256, 384, 512, or `0` for 256; 224 is rejected). The argument is cast to text first, so a numeric argument hashes as its string form (`MD5(123)` = `MD5('123')`), and NULL propagates. Verified against MySQL 8.4. |
| `TO_BASE64` / `FROM_BASE64`            | 🚧     | base64 encode / decode, lowered to the crypto extension's `crypto_encode` / `crypto_decode` with a `'base64'` format. The encode argument is cast to text, so a numeric argument encodes as its string form (`TO_BASE64(255)` is the base64 of `'255'`), and NULL propagates. Two divergences: `TO_BASE64` does not insert MySQL's 76-character line breaks (so long output is a single line), and `FROM_BASE64` returns text rather than a binary string — so it round-trips text and through `HEX()`, but errors on base64 that decodes to non-UTF-8 bytes (where MySQL returns the bytes). Verified against MySQL 8.4 for short text. |
| `LPAD` / `RPAD`                         | ✅     | Pad str to len characters with pad on the left / right; a too-long str is truncated to its left len chars. Synthesized from `REPEAT`/`substr`/`\|\|` (`RPAD` = `substr(str \|\| REPEAT(pad, len), 1, len)`, `LPAD` prepends the fill) wrapped in a guard `CASE`. Padding cycles pad and NULL propagates. The guard matches MySQL's edges: a negative len yields NULL, and an empty pad when fill is needed (len > length(str)) yields the empty string. |
| `RAND`                                 | 🚧     | Lowered to `abs(random() % 1000000000) / 1000000000.0`, a pseudo-random float in `[0, 1)` like MySQL — enough for `ORDER BY RAND()`. A seed argument (`RAND(n)`) is accepted but **not** honored: the engine's RNG is not seedable, so it does not reproduce MySQL's deterministic seeded sequence. |
| `UUID`                                 | 🚧     | A 36-character hyphenated UUID, lowered to the engine's `uuid4_str`. MySQL returns a time-based version-1 UUID and the engine a random version-4 one, so the value and the version nibble differ (the value is non-deterministic either way); the format is identical. Evaluated per row (so `ORDER BY UUID()` and one-per-row inserts work), but two `UUID()` calls *within a single scalar expression* fold to one value (`UUID() = UUID()` is `1` here vs `0` on MySQL — an engine common-subexpression edge). |
| `LENGTH`                               | ✅     | Byte count. Lowered to `length(CAST(x AS BLOB))` (the engine's `length()` of a blob counts bytes); matches MySQL's byte semantics, distinct from `CHAR_LENGTH`. |
| `OCTET_LENGTH`                          | ✅     | A MySQL synonym for `LENGTH` (byte count); shares the exact `length(CAST(x AS BLOB))` lowering. |
| `BIT_LENGTH`                            | ✅     | The byte length times eight; lowered to `8 * length(CAST(x AS BLOB))`. NULL propagates. |
| `GET_LOCK` / `RELEASE_LOCK`            | 🚧     | Advisory locks fold to the constant `1` ("acquired" / "released"); the name and timeout are ignored. This single-node engine has no cross-session lock table, so it matches MySQL only for the uncontended acquire/release flow — the contended (`GET_LOCK` times out → `0`) and not-held (`RELEASE_LOCK` → `0`) cases, and the `NULL`-on-error case, are **not** modeled, and no real mutual exclusion is provided. |
| `ROUND`                                | ❌     | **Excluded** — MySQL pads to the requested decimals / returns DECIMAL; SQLite returns a bare float. |
| `IF`                                   | ✅     | Renamed on emit to the engine's `IIF`; semantics are identical (a NULL/zero condition is false). |
| `SUBSTRING` / `MID`                    | ✅     | Lowered to the same `CASE`-guarded `substr` as `SUBSTR` (above), so the out-of-range positions/lengths match MySQL. `SUBSTRING` also accepts the SQL-standard `SUBSTRING(str FROM pos [FOR len])` syntax; `MID` is comma-form only. |
| `LCASE` / `UCASE`                      | ✅     | Renamed on emit to `lower` / `upper`. |
| `CHAR_LENGTH` / `CHARACTER_LENGTH`     | ✅     | Renamed on emit to `length` (a character count). Distinct from `LENGTH`, which counts bytes and stays excluded. |
| `ASCII` / `ORD`                         | 🚧     | Code of the first character, lowered to `CASE WHEN str = '' THEN 0 ELSE unicode(str) END` — the engine's `unicode()` gives the first code point, and the guard restores MySQL's `ASCII('')` = 0 (the engine's `unicode('')` is NULL); a NULL argument stays NULL. Exact for an ASCII first character (where `ASCII` = `ORD` = code point). Diverges for a non-ASCII first character — MySQL's `ASCII` returns the leading byte (0-255) and `ORD` a byte-weighted value, while this returns the Unicode code point — and for a string whose first byte is NUL (MySQL 0, here NULL). |
| `YEAR` / `MONTH` / `DAY` / `DAYOFMONTH` / `HOUR` / `MINUTE` / `SECOND` | ✅ | Date-part extractors, lowered to `CAST(strftime(fmt, x) AS INTEGER)`; return the integer component (no zero-padding) like MySQL for the standard `YYYY-MM-DD HH:MM:SS` format. |
| `EXTRACT(unit FROM x)`                  | 🚧     | The SQL-standard extractor. `YEAR`/`MONTH`/`DAY`/`HOUR`/`MINUTE`/`SECOND` use the strftime lowering, `WEEK` the default Sunday-first mode (like `WEEK(x)`), and `QUARTER` is `(month + 2) / 3` (like `QUARTER(x)`). The compound units `YEAR_MONTH`, `DAY_HOUR`, `DAY_MINUTE`, `DAY_SECOND`, `HOUR_MINUTE`, `HOUR_SECOND`, `MINUTE_SECOND` combine their fields into one integer (e.g. `YEAR_MONTH` → `year*100 + month`, `DAY_SECOND` → `day*1000000 + hour*10000 + minute*100 + second`), matching MySQL. `MICROSECOND` and the `*_MICROSECOND` compound units are rejected (the engine's strftime has only millisecond precision). |
| `DATE_ADD` / `DATE_SUB` (`INTERVAL n unit`) | 🚧 | Lowered to the engine's `datetime(x, '±n unit')` modifier. A simple `unit` ∈ `DAY`/`WEEK`/`MONTH`/`QUARTER`/`YEAR`/`HOUR`/`MINUTE`/`SECOND` (`WEEK`→7 days, `QUARTER`→3 months) takes an integer-literal value. A **compound** unit — `YEAR_MONTH`, `DAY_HOUR`, `DAY_MINUTE`, `DAY_SECOND`, `HOUR_MINUTE`, `HOUR_SECOND`, `MINUTE_SECOND` — takes a multi-field string literal (`'1:30'`, `'2-3'`, `'1 2:3:4'`; WordPress's GMT-offset upgrade uses `INTERVAL '<h>:<m>' HOUR_MINUTE`), split into one field per engine unit and lowered to a multi-modifier `datetime(...)`; a leading `-` (and `DATE_SUB`) negates every field. Matches MySQL for DATETIME arguments. Diverges on a bare DATE (the engine keeps the `00:00:00` time) and on `MONTH`/`QUARTER`/`YEAR`(`_MONTH`) arithmetic that overflows a month end (MySQL clamps, the engine rolls over). |
| `TIMESTAMPADD(unit, n, dt)`             | 🚧     | Shifts `dt` by `n` units — the same lowering as `DATE_ADD(dt, INTERVAL n unit)` (same units, same integer-literal requirement, same DATE/month-end divergences). The counterpart to `TIMESTAMPDIFF`. |
| `TIMESTAMPDIFF(unit, a, b)`             | 🚧     | Whole `unit`s in `b - a` (operand order is the reverse of `DATEDIFF`). The fixed-duration units `SECOND`/`MINUTE`/`HOUR`/`DAY`/`WEEK` divide `unixepoch(b) - unixepoch(a)` by the unit length (integer division truncates toward zero, matching MySQL's complete-units result for both signs). The calendar units `MONTH`/`QUARTER`/`YEAR` count whole months — `(year*12 + month)` of `b` minus that of `a`, less one when the trailing month is incomplete (the day-and-time of `b`, compared as `DDhhmmss`, has not reached that of `a`) — then divide by 1/3/12; verified against MySQL 8.4 over month-end and leap-day boundaries. `MICROSECOND` is rejected (the engine's datetimes carry only millisecond precision). NULL propagates. |
| `ADDDATE` / `SUBDATE`                   | 🚧     | The `INTERVAL` form is identical to `DATE_ADD`/`DATE_SUB`. The integer-days form (`ADDDATE(d, n)`) shifts by `n` whole days, lowered to `datetime(d, printf('%+d days', ±n))` with a NULL-day guard; `n` may be any expression. Same DATETIME-vs-DATE divergence as `DATE_ADD`. |
| `DATE_FORMAT(x, fmt)`                   | 🚧     | Lowered to `strftime()` for the directly-translatable specifiers (`%Y %m %d %H` pass through; `%i`→`%M`, `%s`→`%S`; `%j` day-of-year, `%w` weekday-number, `%U` Sunday-first week pass through; `%v`→`%V` ISO week; `%T`→`%H:%M:%S`; `%%` literal), and the name specifiers `%M` (month name), `%b` (abbreviated month), `%W` (weekday name), `%a` (abbreviated weekday) are expanded to `CASE` lookups and concatenated (English names only), the no-leading-zero numeric specifiers `%e` (day), `%c` (month), `%k` (hour) become integer casts of the strftime code, and the 12-hour clock `%l` (no pad) / `%h`/`%I` (padded), meridiem `%p` (AM/PM), and the day-with-ordinal-suffix `%D` (`1st`, `2nd`, …) become `CASE` expressions. The format must be a string literal. Specifiers with none of these forms (`%r` 12-hour time, `%f` microseconds, `%X`/`%x` week-year, `%u`/`%V` other week modes, …) are rejected rather than silently mistranslated. |
| `TIME_FORMAT(x, fmt)`                   | 🚧     | Shares the `DATE_FORMAT` lowering. For a time-only format (`%H %i %s %h %I %l %p %k %T`, literal text, …) it matches MySQL exactly, since those specifiers read only the time part — of either a `TIME` or a `DATETIME` argument. Two divergences from MySQL's stricter `TIME_FORMAT`: a *date* specifier (`%Y`, `%m`, `%W`, …) is evaluated here rather than returning NULL, and a `TIME` value outside `00:00:00..23:59:59` (MySQL allows up to 838 hours) is not represented. The format must be a string literal, and the specifiers `DATE_FORMAT` rejects (`%r`, `%f`, …) are rejected here too. |
| `NOW` / `CURRENT_TIMESTAMP` / `UTC_TIMESTAMP` / `LOCALTIME` / `LOCALTIMESTAMP` / `SYSDATE` | 🚧 | Current datetime, lowered to `datetime('now')`. **Always UTC** — the engine has no session time zone, so this diverges from MySQL's session-local `NOW()`. The no-argument form only (a fractional-seconds precision arg is not supported). The SQL-standard keywords (`CURRENT_TIMESTAMP`, `LOCALTIME`, `LOCALTIMESTAMP`, `UTC_TIMESTAMP`) are accepted both with and without parentheses; `NOW`/`SYSDATE` require parentheses, as in MySQL. |
| `CURDATE` / `CURRENT_DATE` / `CURTIME` / `CURRENT_TIME` / `UTC_DATE` / `UTC_TIME` | 🚧 | Lowered to `date('now')` / `time('now')`; UTC, as above. The standard keywords (`CURRENT_DATE`, `CURRENT_TIME`, `UTC_DATE`, `UTC_TIME`) work with or without parentheses; `CURDATE`/`CURTIME` require parentheses. |
| `UNIX_TIMESTAMP([d])` / `FROM_UNIXTIME(n)` | 🚧 | Lowered to `unixepoch(d)` (or `unixepoch('now')`) and `datetime(n, 'unixepoch')`. Absolute conversions are UTC (the engine has no session time zone), so they diverge from MySQL's session-local values, but the two are inverses on both targets, so a round trip is zone-independent. `FROM_UNIXTIME`'s two-argument formatting form is not supported. |
| `TIME_TO_SEC` / `SEC_TO_TIME`          | 🚧     | Seconds since midnight and its inverse. `TIME_TO_SEC(t)` lowers to `H*3600 + M*60 + S` (each `CAST(strftime(code, t) AS INTEGER)`); `SEC_TO_TIME(s)` to `time(s, 'unixepoch')`. Matches MySQL for the normal time-of-day range; MySQL's out-of-range `TIME` values (up to 838 h, negatives) and `SEC_TO_TIME` outputs past `24:00:00` wrap in the engine. |
| `ADDTIME` / `SUBTIME`                   | 🚧     | Add / subtract a time-of-day to a datetime or time, lowered to `CASE WHEN expr LIKE '%-%' THEN datetime(expr, t) ELSE time(expr, t) END` (SUBTIME uses `'-' \|\| t`) — the engine accepts a `'HH:MM:SS'` argument as a signed time offset, and the `LIKE '%-%'` test keeps `expr`'s datetime-vs-time type as MySQL does. Matches MySQL for the normal range, including midnight rollover. Edges diverge: a time-of-day **result** past `24:00:00` wraps (MySQL's `TIME` runs to 838 h), a `t` **argument** beyond 24 h yields NULL, a negative-`TIME` `expr` (it contains `-`) takes the datetime branch, and a bare `DATE` `expr` is treated as midnight rather than MySQL's odd coercion. NULL propagates. |
| `DAYNAME` / `MONTHNAME`                 | ✅     | The English weekday / month name, mapped from `strftime('%w', d)` (0=Sunday..6=Saturday) / `strftime('%m', d)` (1..12) by a `CASE` (no `ELSE`, so NULL propagates). Fills the gap left by `DATE_FORMAT`'s unsupported `%W`/`%M` specifiers. English names only (the engine has no locale). |
| `TO_DAYS` / `FROM_DAYS`                 | ✅     | Day number (days since year 0) and its inverse. `TO_DAYS(d)` is `CAST(julianday(date(d)) AS INTEGER) - 1721059` (the `date()` drops the time part); `FROM_DAYS(n)` is `date(n + 1721059.5)`. The offset shifts the engine's Julian day onto MySQL's proleptic-Gregorian count (meaningful for modern dates, as in MySQL). |
| `TO_SECONDS`                            | ✅     | Seconds from year 0 to a date/time, lowered to `TO_DAYS(d) * 86400 + TIME_TO_SEC(d)` — the day number scaled to seconds plus the seconds since midnight. Reuses the `TO_DAYS` and `TIME_TO_SEC` lowerings (so it shares their modern-Gregorian and normal-time-of-day ranges), and NULL propagates. Verified against MySQL 8.4. |
| `PERIOD_DIFF` / `PERIOD_ADD`            | ✅     | Month arithmetic on `YYYYMM`/`YYMM` periods. Each period becomes an absolute month count `normalized_year * 12 + month` (month `p % 100`, year `p / 100`, a two-digit year normalized as MySQL does: `< 70` → `20YY`, `< 100` → `19YY`). `PERIOD_DIFF(p1, p2)` is `months(p1) - months(p2)`; `PERIOD_ADD(p, n)` adds `n` months and converts back via `((total - 1) / 12) * 100 + ((total - 1) % 12 + 1)`. NULL propagates; verified against MySQL 8.4 (including the two-digit-year normalization and negative shifts). |
| `MAKEDATE(year, dayofyear)`             | ✅     | Builds a date from a year and a 1-based day of year, lowered to `date(printf('%04d-01-01', year), printf('%+d days', dayofyear - 1))`. Day 1 is Jan 1, a `dayofyear` past the year length rolls over, and a NULL argument or `dayofyear < 1` yields NULL (guarded by a `CASE`). |
| `MAKETIME(hour, minute, second)`       | 🚧     | Builds a time string, lowered to `printf('%02d:%02d:%02d', …)` guarded by a `CASE` that returns NULL for a NULL argument or `minute`/`second` outside `0..59` (as MySQL does). The hour may exceed 23. A negative hour (`-1:..` vs MySQL `-01:..`) and an hour past 838 (MySQL clamps) diverge. |
| `WEEK(d[, mode])`                       | 🚧     | The week number, lowered to a `strftime` week. The `mode` (default 0) selects the numbering scheme; seven of the eight are supported: 0→`%U`, 3→`%V` (ISO), 5→`%W`; 1 (Monday-first, 0–53, week 1 = first week with >3 days) → the ISO week with a year-boundary correction (0 in a previous-year partial week, 53 in a next-year partial week); 2 and 7 (the 1–53 siblings of 0 and 5) → that code with the leading partial week renumbered as the previous year's last week (`%U`/`%W` of `date(d, 'start of year', '-1 day')`); and 4 (Sunday-first 0–53, "4 or more days" rule) → `%U` plus a per-year offset of 1 when January 1 is a Mon/Tue/Wed. WordPress's `WP_Date_Query` emits modes 2/4/5 via `WEEK(col, 7 - start_of_week)`. All verified against MySQL 8.4 (every mode for all year-boundary weeks of 2000–2040; mode 4 also for every day of 2018–2027). Only mode 6 (the Sunday-first 1–53 "4 or more days" rule) has no clean strftime form and is rejected. `WEEKOFYEAR(d)` is `WEEK(d, 3)`. |
| `YEARWEEK(d[, mode])`                   | 🚧     | `year * 100 + week`, where the year owns the week (so it differs from the calendar year for a boundary-straddling week — YEARWEEK has no week 0). Modes 1 and 3 are the ISO year-week, `strftime('%G', d) * 100 + strftime('%V', d)` (they coincide, since `%G` already attributes a straddling week and YEARWEEK has no week 0). Modes 0 (`%U`) and 5 (`%W`) number within the calendar year and push a "week 0" date into the previous year's last week (a `CASE` on `week = 0` using the week number of `date(d, 'start of year', '-1 day')`). Modes 2/4/6/7 are rejected, mirroring `WEEK`. Verified against MySQL 8.4 for all four modes over 2014–2031. NULL propagates. |
| Other date/time functions (`STR_TO_DATE`, `CONVERT_TZ`, `PERIOD_ADD`, `YEARWEEK`, …) | ❌ | **Excluded** — format/type or timezone differences. (`DATEDIFF`/`TIMESTAMPDIFF` *are* supported; see their own tests.) |
| `VERSION` / `DATABASE` / `SCHEMA` / `CONNECTION_ID` / `USER` / `CURRENT_USER` / `SESSION_USER` / `SYSTEM_USER` | 🚧 | Introspection functions, usable in any expression (not just as a standalone `SELECT`). Fold to fixed placeholder values matching the server's standalone-query answers (`VERSION()`→`8.0.0-turso`, `USER()`→`root@localhost`, `CONNECTION_ID()`→`1`); `DATABASE()`/`SCHEMA()`→`NULL` (no current schema). Not the real per-connection values. `CURRENT_USER` is also accepted without parentheses (the SQL-standard niladic form); the others require parentheses, as in MySQL. |
| any other function                     | ❌     | **Not supported** — not in the clean allow-list. |

### Transactional and Locking Statements

| Statement                                       | Status | Comment |
|-------------------------------------------------|--------|---------|
| START TRANSACTION / BEGIN [WORK]                | ✅     | Mapped to the engine's `BEGIN` (deferred). `READ ONLY`/`READ WRITE`/`WITH CONSISTENT SNAPSHOT` characteristics are rejected. |
| COMMIT [WORK]                                    | ✅     | `AND CHAIN`/`RELEASE` rejected. |
| ROLLBACK [WORK]                                  | ✅     | `AND CHAIN`/`RELEASE` rejected. |
| SAVEPOINT name                                   | ✅     | Passed through to the engine's native savepoint. |
| ROLLBACK [WORK] TO [SAVEPOINT] name              | ✅     | Undoes the statements since the savepoint, via the engine's `ROLLBACK TO`. The `SAVEPOINT` keyword is optional, as in MySQL. |
| RELEASE SAVEPOINT name                           | ✅     | Discards a savepoint without rolling back, via the engine's `RELEASE`. The `SAVEPOINT` keyword is required (MySQL syntax). |
| SELECT ... FOR UPDATE / FOR SHARE / LOCK IN SHARE MODE | 🚧 | The trailing locking-read clause is accepted and ignored, including the `OF tbl [, tbl] ...` table list and the `NOWAIT` / `SKIP LOCKED` lock-acquisition options. The engine is a single writer, so the locked query returns the same rows as the unlocked one; no real row locking is performed, and `NOWAIT` / `SKIP LOCKED` (which only matter under contention) are no-ops. |
| LOCK INSTANCE FOR BACKUP / UNLOCK INSTANCE       | ❌     |         |
| LOCK TABLES / UNLOCK TABLES                      | 🚧     | `LOCK TABLE[S] ... {READ\|WRITE}` (any number of tables) and `UNLOCK TABLE[S]` are accepted as **no-ops** that report success — the engine is a single writer, so the table locks MySQL uses to serialize access are unnecessary. The statements between them run normally. MySQL's side effects are not reproduced: `LOCK TABLES` does not commit an active transaction and does not confine the session to the locked tables (accessing an unlocked table still works). |
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
| ANALYZE / CHECK / OPTIMIZE / REPAIR TABLE | 🚧 | Accepted as a no-op that reports success (WordPress's database-repair admin page runs them). The engine has no fragmentation, optimizer statistics, or MySQL-style corruption, so each returns MySQL's `Table`/`Op`/`Msg_type`/`Msg_text` columns with one `status`/`OK` row per named table (comma lists give one row each; the `LOCAL` modifier and trailing options like `QUICK`/`FOR UPGRADE` are ignored). Not byte-identical to mysqld: `Table` is the bare table name (no `db.` prefix), and `OPTIMIZE` returns one status row rather than InnoDB's note + status pair. |
| CHECKSUM TABLE     | ❌     |         |

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
| SHOW [FULL] COLUMNS / FIELDS FROM *tbl* | ✅ | Answered from the schema (`PRAGMA table_info` plus the stored `CREATE TABLE` text). `Field`, `Null`, `Key`, `Default`, `Extra`, the declared column size (`varchar(60)`, `decimal(10,2)`), and the result-set shape match MySQL (a string `Default` is reported as its bare value — `DEFAULT 'hi'` → `hi`, `DEFAULT ''` → the empty string — not the stored SQL literal); a primary-key column reports `Null = NO` (a PK is implicitly NOT NULL in MySQL even for an `INT PRIMARY KEY` rowid alias the engine does not flag); the `Key` flag is `PRI` for a primary-key column, `UNI`/`MUL` for the leading column of a unique / non-unique index (derived from the engine's index list), and empty otherwise — matching MySQL; `Type` is lowercased and normalized like MySQL 8.0 for integers — `integer` renders as `int` and the display width is stripped (`int(11)` → `int`, `bigint(20) unsigned` → `bigint unsigned`), except `tinyint(1)` and `zerofill` columns (width kept) — while other sizes (`varchar(60)`, `decimal(10,2)`) pass through; `Collation` is reported as a fixed `utf8mb4_general_ci`. (An `AUTO_INCREMENT` primary key is the exception: the engine retypes it to the rowid-alias `INTEGER`, so its declared type — e.g. `bigint unsigned` — and the `auto_increment` `Extra` are lost.) A trailing `LIKE 'pat'` (matched against the `Field` column name, as WordPress uses to test whether a column exists) and a single `WHERE col {= \| LIKE} value` predicate over one known output column are applied as row filters; a compound `WHERE`, an unknown column, or any other predicate falls through as unsupported. A missing table errors with 1146. |
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
| SHOW ERRORS                | 🚧     | Always returns an empty result set (MySQL's `Level`/`Code`/`Message` columns, no rows) — the engine keeps no diagnostics area, so this matches a real mysqld only when the last statement produced no warnings/errors. A trailing `LIMIT` is accepted; the `SHOW COUNT(*) WARNINGS` form is not. |
| SHOW EVENTS                | ❌     |         |
| SHOW FUNCTION CODE         | ❌     |         |
| SHOW FUNCTION STATUS       | ❌     |         |
| SHOW GRANTS                | ❌     |         |
| SHOW {INDEX\|INDEXES\|KEYS} FROM *tbl* | ✅ | Reshapes `PRAGMA index_list`/`index_info` into MySQL 8's 15-column result (`Table`, `Non_unique`, `Key_name`, `Seq_in_index`, `Column_name`, …). `dbDelta()` reads it to learn which indexes exist; the values it keys on (`Key_name`, `Column_name`, `Non_unique`, `Seq_in_index`) match. A primary key is reported under the `PRIMARY` name. Not byte-identical to a real mysqld: the engine keeps no index statistics, so `Cardinality`/`Sub_part`/`Packed`/`Expression` are NULL, `Collation` is `A`, `Index_type` `BTREE`, `Visible` `YES`, and the row order follows PRAGMA order, not MySQL's PRIMARY-first. An optional trailing `WHERE col {= \| LIKE} value` filters the rows over a single known output column (e.g. `WHERE Key_name='a_key'`, as dbDelta emits); a compound `WHERE`, an unknown column, or any other predicate falls through as unsupported. A missing table errors with 1146. |
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
| SHOW TABLE STATUS [LIKE *pat* \| WHERE Name …] | ✅ | One row per table in MySQL's 18-column shape (`Name`, `Engine`, …, `Collation`, …), optionally filtered by a `LIKE` pattern or a `WHERE Name {= \| LIKE} value` predicate (WordPress's `wpdb` issues `SHOW TABLE STATUS WHERE Name = '<table>'` to route a query to its table); a `LIKE` honors MySQL's default `\` escape (so `wp\_%` matches a literal underscore). An unknown table yields an empty result set. `Rows` is the table's real `COUNT(*)` and `Collation` is the fixed `utf8mb4_general_ci`; the columns the engine does not track — sizes (`Data_length`, `Index_length`, …), timestamps, `Auto_increment` — are reported as `0` or NULL, and `Engine`/`Row_format` are fixed values, so those differ from a real mysqld. Used by WordPress (`maybe_convert_table_to_utf8mb4`, table routing, Site Health). A `WHERE` on a column other than `Name` is rejected (as MySQL also rejects a non-existent column). |
| SHOW [FULL] TABLES [LIKE]  | ✅     | Base table names synthesized from the schema, optionally filtered by a `LIKE` pattern; `FULL` adds a `Table_type` column (`BASE TABLE`). SQLite's `sqlite_%` tables and turso's internal `__turso_internal_*` bookkeeping tables (the AUTO_INCREMENT sequence and CREATE TYPE tables) are hidden, as a real MySQL server has none — `SHOW TABLE STATUS` excludes them too. The result column header is a fixed `Tables_in_database`, not MySQL's `Tables_in_<db>` (clients read it positionally). The `WHERE` filter form is rejected. |
| `SELECT ... FROM information_schema.TABLES` | 🚧 | A reference to `information_schema.TABLES` in a `FROM` clause is rewritten into a derived table synthesized from the engine catalog, exposing `TABLE_NAME`, `ENGINE` (the fixed `InnoDB`), `TABLE_TYPE` (`BASE TABLE`), and the zeroed `TABLE_ROWS` / `DATA_LENGTH` / `INDEX_LENGTH` (the engine keeps no statistics). SQLite's `sqlite_%` and turso's `__turso_internal_*` tables are excluded. WordPress's upgrade routine (`SELECT TABLE_NAME ... WHERE ... ENGINE = 'MyISAM'`) correctly gets an empty result (no MyISAM tables). **`TABLE_SCHEMA` is a placeholder** (the front-end does not track the connection's database name), so a query that filters on it — as WordPress's Site Health and the `... AND TABLE_SCHEMA = <db>` form do — matches nothing. Other `information_schema` tables (`COLUMNS`, `STATISTICS`, …) remain unsupported. |
| SHOW TRIGGERS              | ❌     |         |
| SHOW [GLOBAL\|SESSION] VARIABLES [LIKE *pat*] | ✅ | Answered from a fixed table of plausible system-variable values (the same table that backs `SELECT @@var`), returned as MySQL's `Variable_name` / `Value` result set ordered by name. The optional `GLOBAL`/`SESSION` scope is accepted and ignored, the `LIKE` pattern uses case-insensitive SQL wildcards (`%`, `_`), and an unknown variable yields an empty result set. Used by WordPress Site Health (`WP_Debug_Data::get_mysql_var`). Only the listed variables are reported (a real mysqld exposes hundreds more), and values are front-end constants, so the bare `SHOW VARIABLES` row set and individual values may differ from a given server. The `WHERE` filter form is not handled (rejected). |
| SHOW WARNINGS              | 🚧     | Always returns an empty result set (MySQL's `Level`/`Code`/`Message` columns, no rows) — the engine keeps no diagnostics area, so this matches a real mysqld only when the last statement produced no warnings/errors. A trailing `LIMIT` is accepted; the `SHOW COUNT(*) WARNINGS` form is not. |

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
