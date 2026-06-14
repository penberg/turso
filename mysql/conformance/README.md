# MySQL conformance tests

A wire-protocol conformance suite for the Turso MySQL front-end. The runner is a
plain MySQL client (the [`mysql`](https://docs.rs/mysql) crate), so it drives any
MySQL-speaking endpoint — the Turso server **or** a real `mysqld` — through the
exact same path a normal client uses.

## Test format

Files end in `.test` and use a small, sqllogictest-inspired DSL. Records are
separated by blank lines; `#` begins a comment.

```text
statement ok
CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50))

statement error
CREATE TABLE                 # malformed: must be rejected

query
SELECT id, name FROM t ORDER BY id
----
1	alice
2	bob
```

- `statement ok` — the SQL must succeed.
- `statement error` — the SQL must fail.
- `query` — the SQL must succeed and return exactly the rows after `----`, in
  order, columns separated by a single tab, SQL `NULL` rendered as `NULL`.
- `query types` — like `query`, but the single expected line lists the MySQL
  column *type* of each result column (`LONG`, `VAR_STRING`, …), checking the
  result-set metadata rather than the rows.
- `exec ok` / `exec error` — like `statement`, but run over the binary
  (prepared-statement) protocol. An optional `params` line binds the `?`
  placeholders, tab-separated, with `NULL` for SQL NULL.
- `exec query` — like `query`, run as a prepared statement, with an optional
  `params` line before the `----` separator.

```text
query types
SELECT id, name FROM t
----
LONG	VAR_STRING

exec query
SELECT name FROM t WHERE id = ?
params 1
----
alice
```

## Running

```bash
# Against the Turso MySQL front-end (start it first):
turso-mysql-server --listen 127.0.0.1:3306 --database :memory: &
cargo run -p turso-mysql-conformance -- --url mysql://root@127.0.0.1:3306/

# Against a real MySQL (the same files should pass):
cargo run -p turso-mysql-conformance -- \
    --url mysql://root@127.0.0.1:3306/ mysql/conformance/tests
```

The endpoint may also be set via `MYSQL_TEST_URL`. The runner exits non-zero if
any record fails.

## Running against real MySQL with Docker

The runner is a normal MySQL client, so the simplest way to check the suite
against "MySQL proper" is to start a throwaway `mysql` container and forward its
port to the host.

```bash
# 1. Start MySQL 8.4 with an empty root password, listening on host port 3307.
#    (Empty password keeps the client URL simple; the container is disposable.)
docker run --rm -d \
  --name turso-mysql-conformance \
  -e MYSQL_ALLOW_EMPTY_PASSWORD=yes \
  -e MYSQL_DATABASE=conformance \
  -p 127.0.0.1:3307:3306 \
  mysql:8.4

# 2. Wait until the server is accepting connections (first boot initializes the
#    data directory and takes a few seconds).
until docker exec turso-mysql-conformance \
        mysqladmin ping -h 127.0.0.1 --silent 2>/dev/null; do
  sleep 1
done

# 3. Run the suite against it.
cargo run -p turso-mysql-conformance -- \
  --url 'mysql://root@127.0.0.1:3307/conformance' \
  mysql/conformance/tests

# 4. Tear the container down.
docker rm -f turso-mysql-conformance
```

Notes:

- Port `3307` on the host is forwarded to `3306` in the container, so it does
  not clash with a local MySQL or the Turso front-end on `3306`.
- The `conformance` database created by `MYSQL_DATABASE` is referenced in the
  connection URL so the test tables land in a dedicated schema.
- `mysql:8.4` defaults to the `caching_sha2_password` auth plugin; the `mysql`
  client crate handles it for an empty-password account over plaintext, so no
  TLS setup is needed.

## Current state

`tests/create_table.test` passes against both the Turso front-end and real
MySQL. As the front-end grows beyond `CREATE TABLE` (INSERT, SELECT, ...), add
test files written to real MySQL semantics — keep them green against a real
`mysqld` first, then bring the front-end up to match. See
[`../COMPAT.md`](../COMPAT.md).
