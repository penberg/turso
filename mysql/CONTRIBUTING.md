# Contributing to the Turso MySQL Front-End

This is a proof-of-concept MySQL wire-protocol front-end for Turso: a
`turso-mysql-server` that speaks the MySQL protocol on the wire and runs queries
through the synchronous `turso_core` engine. The goal is to behave like a real
MySQL 8.x server, so most of the work is closing the gap between what real MySQL
does and what the front-end does today.

Three things keep that work honest:

- **[`GRAMMAR.md`](GRAMMAR.md)** — the exact grammar target.
- **[`COMPAT.md`](COMPAT.md)** — where Turso intentionally diverges from MySQL.
- **[`conformance/`](conformance/)** — executable tests run against both the
  front-end and a real `mysqld`, so divergence can't sneak in unnoticed.

## Verification workflow

When changing the parser or query behavior:

1. Check the relevant page of the MySQL 8.0 Reference Manual.
2. Confirm the behavior against a real MySQL 8.x server (the conformance runner
   makes this a one-liner — see below).
3. Encode the observed behavior as a test in [`conformance/`](conformance/) so
   it can't regress.

If Turso intentionally diverges from MySQL, document that in
[`COMPAT.md`](COMPAT.md), not in `GRAMMAR.md`. `GRAMMAR.md` is the exact grammar
target; deliberate gaps belong in `COMPAT.md`.

## Running the conformance suite

The conformance suite is the day-to-day test loop. Run it with:

```bash
mysql/conformance/run.sh
```

This builds `turso-mysql-server` and the conformance runner, then runs every
`.test` file under [`conformance/tests/`](conformance/tests/) against **two**
targets through the same MySQL client:

1. A throwaway **MySQL 8.4 in Docker** — the reference. It should always be
   green; if it isn't, the test file itself is wrong.
2. The **Turso front-end** — work in progress. Failures here are expected for
   not-yet-implemented statements and don't fail the script by default.

Useful flags (see `run.sh --help` for the full list):

```bash
mysql/conformance/run.sh --turso-only          # skip Docker; just run the front-end
mysql/conformance/run.sh --mysql-only          # only the reference MySQL
mysql/conformance/run.sh --strict              # also fail if the front-end fails
mysql/conformance/run.sh conformance/tests/insert.test   # run specific file(s)
mysql/conformance/run.sh --keep                # leave the container/server up
```

The Docker MySQL target needs `docker` or `podman` on `PATH`. Without a
container runtime, use `--turso-only`. Common environment overrides:
`MYSQL_IMAGE` (default `mysql:8.4`), `MYSQL_PORT` (`3307`), `TURSO_PORT`
(`3308`), `RUST_LOG` (server log level).

To drive an already-running server directly, the runner is just a MySQL client:

```bash
cargo run -p turso-mysql-conformance -- \
    --url 'mysql://root@127.0.0.1:3306/' conformance/tests
```

## Writing a new conformance test

Tests live in [`conformance/tests/`](conformance/tests/) and end in `.test`.
They use a small, sqllogictest-inspired DSL: records are separated by blank
lines, and `#` begins a comment.

```text
# A short header explaining what this file covers and why it must pass
# against both a real mysqld and the front-end.

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

Record types:

- **`statement ok`** — the SQL must succeed.
- **`statement error`** — the SQL must fail.
- **`query`** — the SQL must succeed and return exactly the rows listed after
  the `----` separator, in order. Columns are separated by a single tab; SQL
  `NULL` renders as `NULL`.

Guidelines for tests that stay green against real MySQL:

- **Write the test against real MySQL first.** A new test must pass against the
  Docker MySQL target before you bring the front-end up to match. Run
  `mysql/conformance/run.sh --mysql-only conformance/tests/your_file.test` while
  iterating.
- **Make results deterministic.** Always `ORDER BY` in `query` records; row
  order is otherwise unspecified.
- **Avoid output that diverges by formatting.** Prefer `INT` columns where the
  text rendering is identical across engines. For example `AVG` and some
  `DECIMAL` formatting differs between MySQL and the engine — keep those out of
  shared tests (or document the divergence in [`COMPAT.md`](COMPAT.md)).
- **Use `CREATE TABLE IF NOT EXISTS`** so a file can be re-run against a
  persistent server without manual cleanup.
- **Add a header comment** stating what the file covers and that it is expected
  to pass against both targets.

New files under `conformance/tests/` are picked up automatically — no
registration step.

## Running the WordPress test suite

The WordPress core PHPUnit suite is a large, real-world MySQL workload. Pointing
it at `turso-mysql-server` is a good way to surface compatibility gaps beyond the
curated conformance tests.

The clone lives at `mysql/wordpress-develop/` and is **not** checked in. Set it
up once:

```bash
cd mysql
git clone https://github.com/WordPress/wordpress-develop.git
cd wordpress-develop
composer install      # installs PHPUnit into vendor/bin (needs PHP + Composer)
```

The repo already ships a `wp-tests-config.php` pre-pointed at the front-end:

```php
define( 'DB_NAME', 'wordpress' );        // the front-end ignores schema selection for now
define( 'DB_USER', 'root' );
define( 'DB_PASSWORD', '' );             // empty password is accepted
define( 'DB_HOST', '127.0.0.1:3306' );   // host:port of turso-mysql-server
```

> **Warning:** the test suite **drops all tables** with the `wptests_` prefix on
> every run. Only ever point it at a disposable database.

Run it against the front-end:

```bash
# 1. Start turso-mysql-server on the port wp-tests-config.php expects (3306).
#    Use a file (not :memory:) so the schema survives across connections.
cargo run -p turso-mysql-server -- --listen 127.0.0.1:3306 --database /tmp/wp.db &

# 2. From the wordpress-develop checkout, run the PHPUnit suite.
cd mysql/wordpress-develop
vendor/bin/phpunit

# Narrow to a single test file while debugging a specific failure:
vendor/bin/phpunit tests/phpunit/tests/db.php
```

Most of the suite will fail today — that is the point. Pick a failing test,
reproduce the failing query against the front-end (and against real MySQL to
confirm the expected behavior), then either fix the front-end or, if Turso
intentionally diverges, record it in [`COMPAT.md`](COMPAT.md). When a behavior
is worth pinning down permanently, distill it into a focused conformance test so
it can't regress.
