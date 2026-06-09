# The MySQL Query Language

This document is a specification of the MySQL-family dialect.

Its primary syntactic source is the ANTLR `grammars-v4` Oracle MySQL grammar
in `sql/mysql/Oracle/` (`MySQLParser.g4` and `MySQLLexer.g4`):

<https://github.com/antlr/grammars-v4/tree/master/sql/mysql/Oracle>

This document may also be refined using public MySQL and MariaDB reference
manuals and black-box observation of running servers. It must not be derived
from MySQL or MariaDB source code.

## Notation

Grammar rules use the form:

```
rule-name ::=
  alternative
  alternative
```

Each line under a rule is one alternative. Within an alternative:

- `'TEXT'` is a terminal. Keywords are case-insensitive; punctuation is exact.
- A lowercase name is a nonterminal defined elsewhere in this document.
- `x?` means zero or one `x`; `x*` zero or more; `x+` one or more.
- `( ... )` groups; `(a | b)` inside a group separates alternatives inline.
- `(one of)` introduces a flat list of single-token alternatives.

The grammar is given for the default `sql_mode` (see [SQL
modes](#sql-modes-change-the-grammar)); mode-dependent deviations are noted in
prose where they occur. Constructs marked **not yet specified** are part of
the dialect but not yet written out in this document.

## Source Text

The unit of parsing is a statement:

```
query ::=
  statement ';'?
```

A statement is terminated by `;` or by end of input; a trailing `;` after the
final statement is optional. Empty input (no statement) is accepted.

Multi-statement input (several statements in one request, separated by `;`) is
not a grammar feature. It is negotiated at the protocol level via the
`CLIENT_MULTI_STATEMENTS` capability flag, and statement splitting must respect
the full lexical grammar: a `;` inside a string literal, quoted identifier, or
comment does not terminate a statement.

## Lexical Structure

### Whitespace and Comments

Space, tab, newline, carriage return, and form feed separate tokens. Three
comment styles exist:

```
comment ::=
  '#' (any character except newline)*
  '--' (whitespace | control character) (any character except newline)*
  '/*' (any character sequence not containing '*/') '*/'
```

`--` starts a comment **only when followed by whitespace, a control character,
or end of input**. `--x` is not a comment; it parses as two unary minus
operators applied to `x`.

**Executable (versioned) comments.** Not all `/* ... */` comments are
discardable trivia:

```
versioned-comment ::=
  '/*!' version-number? sql-text '*/'

version-number ::=
  digit+
```

The contents of `/*! ... */` are parsed as live SQL tokens. With a version
number (`/*!50032 ... */`, `/*!100000 ... */`), the contents are parsed as
live tokens only when the server version is at least that number; otherwise
the whole comment is discarded.

### Keywords

The dialect distinguishes **reserved** and **non-reserved** keywords at the grammar
level. Reserved keywords cannot be used as unquoted identifiers; non-reserved
keywords can. The keyword inventory is part of the dialect and is not
reproduced in this document. A lexer that treats every keyword as a generic
identifier token cannot reproduce this distinction. Keywords are real tokens;
throughout this grammar, a quoted keyword terminal denotes that token, not a
generic identifier with matching text.

### Identifiers

```
identifier ::=
  unquoted-identifier
  quoted-identifier
  (any non-reserved keyword)

unquoted-identifier ::=
  ident-char+

ident-char ::=
  letter
  digit
  '_'
  '$'
  (any character with code point >= U+0080)

quoted-identifier ::=
  '`' (ident-quoted-char | '``')* '`'
```

An unquoted identifier may not consist entirely of digits. Inside a quoted
identifier, any character except a bare backtick may appear; a literal backtick
is written by doubling it. Under `ANSI_QUOTES` (see [SQL
modes](#sql-modes-change-the-grammar)), `"..."` is also a quoted identifier.

Names qualify with `.`, with whitespace permitted around the dot:

```
schema-ref ::=
  identifier

table-ref ::=
  identifier
  identifier '.' identifier

column-ref ::=
  identifier
  identifier '.' identifier
  identifier '.' identifier '.' identifier
```

### String Literals

```
string-literal ::=
  introducer? single-quoted-string
  introducer? double-quoted-string
  ('N' | 'n') single-quoted-string

single-quoted-string ::=
  "'" (string-char | escape-sequence | "''")* "'"

double-quoted-string ::=
  '"' (string-char | escape-sequence | '""')* '"'

introducer ::=
  '_' charset-name-token        (e.g. _utf8mb4, _latin1, _binary)
```

- `'...'` is always a string literal. An embedded quote is written by doubling
  (`''`) or, by default, by backslash escape (`\'`).
- `"..."` is a string literal **by default**, and a quoted identifier under
  `ANSI_QUOTES`. It is incorrect to treat `"` as always-string or
  always-identifier.
- Adjacent string literals separated only by whitespace concatenate:
  `'a' 'b'` is `'ab'`.
- String literals must be processed as byte/character sequences without
  mangling multi-byte UTF-8 content.

```
escape-sequence ::= (one of)
  \0 \' \" \b \n \r \t \Z \\ \% \_
```

An unrecognized escape yields the escaped character itself (`\x` is `x`).
Under `NO_BACKSLASH_ESCAPES`, backslash is an ordinary character and only
quote doubling escapes a quote; this changes both lexical acceptance and
string contents.

### Numeric Literals

```
numeric-literal ::=
  integer-literal
  decimal-literal
  float-literal

integer-literal ::=
  digit+

decimal-literal ::=
  digit+ '.' digit*
  '.' digit+

float-literal ::=
  (integer-literal | decimal-literal) ('e' | 'E') ('+' | '-')? digit+
```

Leading-dot forms (`.5`, `.5e-2`) are literals. Lexing is maximal-munch the
way a real server applies it: a number immediately followed by an identifier
character is not a number followed by an identifier (`1ex` is a single
identifier-like token, not `1` `ex`).

### Hexadecimal, Bit, Temporal, Boolean, and Null Literals

```
hexadecimal-literal ::=
  '0x' hex-digit+
  introducer? ('x' | 'X') "'" hex-digit* "'"

bit-literal ::=
  '0b' binary-digit+
  introducer? ('b' | 'B') "'" binary-digit* "'"

temporal-literal ::=
  'DATE' string-literal
  'TIME' string-literal
  'TIMESTAMP' string-literal

boolean-literal ::= (one of)
  TRUE FALSE

null-literal ::=
  'NULL'

literal ::=
  string-literal
  numeric-literal
  hexadecimal-literal
  bit-literal
  temporal-literal
  boolean-literal
  null-literal
```

### Variables and Placeholders

```
user-variable ::=
  '@' identifier
  '@' string-literal
  '@' quoted-identifier

system-variable ::=
  '@@' (('GLOBAL' | 'SESSION' | 'LOCAL') '.')? identifier ('.' identifier)?

placeholder ::=
  '?'
```

A placeholder may appear wherever this grammar permits it explicitly (notably
as a `simple-expression` and as a `limit-option`), in preparable statements
only.

### Function-Call Recognition

For a set of built-in function names, whether whitespace is permitted between
the function name and `(` while still parsing as a function call depends on
`IGNORE_SPACE`. By default `COUNT (*)` (with a space) is rejected for those
names while `COUNT(*)` is accepted; under `IGNORE_SPACE` both parse as calls.
This is a lexer-level decision, not an expression-grammar production.

## SQL Modes Change the Grammar

The accepted grammar is a function of the session's `sql_mode`. The flags that
change tokenization or parsing are:

- `ANSI_QUOTES` — `"` delimits identifiers instead of strings.
- `NO_BACKSLASH_ESCAPES` — disables backslash escapes in string literals.
- `IGNORE_SPACE` — permits whitespace between built-in function names and `(`.
- `PIPES_AS_CONCAT` — `||` is string concatenation instead of logical OR.
- `HIGH_NOT_PRECEDENCE` — raises the precedence of `NOT` to that of `!`.
- `REAL_AS_FLOAT` — `REAL` is a synonym for `FLOAT` instead of `DOUBLE`.

The combination mode `ANSI` expands to `REAL_AS_FLOAT`, `PIPES_AS_CONCAT`,
`ANSI_QUOTES`, and `IGNORE_SPACE`.

This document specifies the grammar under the **default**
`sql_mode`:

```
STRICT_TRANS_TABLES, ERROR_FOR_DIVISION_BY_ZERO, NO_ENGINE_SUBSTITUTION,
NO_AUTO_CREATE_USER
```

None of the default flags alter tokenization or parsing; they affect semantics
(grouping validation, strictness, zero-date handling).

## Statements

```
statement ::=
  select-statement
  insert-statement
  replace-statement
  update-statement
  delete-statement
  load-data-statement
  handler-statement
  import-statement
  call-statement
  do-statement
  transaction-statement
  savepoint-statement
  lock-statement
  xa-statement
  set-statement
  create-database-statement
  create-table-statement
  create-view-statement
  alter-view-statement
  create-index-statement
  alter-table-statement
  create-procedure-statement
  alter-procedure-statement
  create-function-statement
  alter-function-statement
  create-trigger-statement
  drop-trigger-statement
  create-event-statement
  alter-event-statement
  drop-event-statement
  drop-statement
  rename-table-statement
  truncate-statement
  prepare-statement
  execute-statement
  deallocate-statement
  create-user-statement
  alter-user-statement
  drop-user-statement
  rename-user-statement
  grant-statement
  revoke-statement
  set-role-statement
  replication-statement
  table-administration-statement
  administrative-statement
  install-statement
  uninstall-statement
  clone-statement
  resource-group-statement
  show-statement
  use-statement
  help-statement
  restart-statement
  explain-statement
  describe-statement
  analyze-statement
  get-diagnostics-statement
  signal-statement
  resignal-statement
```

## SELECT

```
select-statement ::=
  query-expression locking-clause*
  query-expression into-clause locking-clause*
  query-expression locking-clause+ into-clause
  '(' select-statement ')'

query-expression ::=
  with-clause? query-expression-body order-by-clause? limit-clause?

subquery ::=
  '(' query-expression locking-clause* ')'
```

### Set Operations

```
query-expression-body ::=
  query-term
  query-expression-body 'UNION' set-quantifier? query-term
  query-expression-body 'EXCEPT' set-quantifier? query-term

query-term ::=
  query-primary
  query-term 'INTERSECT' set-quantifier? query-primary

query-primary ::=
  query-specification
  values-statement
  subquery

set-quantifier ::= (one of)
  ALL DISTINCT
```

`UNION` and `EXCEPT` are left-associative and share a precedence level;
`INTERSECT` binds tighter. Without a quantifier, `DISTINCT` is implied.
All three operators accept both quantifiers.

### Query Specification

```
query-specification ::=
  'SELECT' select-option* select-item-list into-clause? from-clause?
      where-clause? group-by-clause? having-clause? window-clause?

select-option ::= (one of)
  ALL DISTINCT DISTINCTROW STRAIGHT_JOIN HIGH_PRIORITY
  SQL_SMALL_RESULT SQL_BIG_RESULT SQL_BUFFER_RESULT
  SQL_CALC_FOUND_ROWS SQL_NO_CACHE

select-item-list ::=
  ('*' | select-item) (',' select-item)*

select-item ::=
  table-wild
  expression select-alias?

table-wild ::=
  identifier '.' '*'
  identifier '.' identifier '.' '*'

select-alias ::=
  'AS'? identifier
  'AS'? string-literal
```

### FROM Clause and Table References

```
from-clause ::=
  'FROM' 'DUAL'
  'FROM' table-reference-list

table-reference-list ::=
  table-reference (',' table-reference)*

table-reference ::=
  table-factor joined-table*

joined-table ::=
  inner-join-type table-reference join-specification?
  outer-join-type table-reference join-specification
  natural-join-type table-factor

join-specification ::=
  'ON' expression
  'USING' '(' identifier-list ')'

inner-join-type ::=
  ('INNER' | 'CROSS')? 'JOIN'
  'STRAIGHT_JOIN'

outer-join-type ::=
  ('LEFT' | 'RIGHT') 'OUTER'? 'JOIN'

natural-join-type ::=
  'NATURAL' 'INNER'? 'JOIN'
  'NATURAL' ('LEFT' | 'RIGHT') 'OUTER'? 'JOIN'

table-factor ::=
  single-table
  '(' single-table ')'
  derived-table
  '(' table-reference-list ')'

single-table ::=
  table-ref use-partition? table-alias? index-hint-list?

derived-table ::=
  subquery table-alias? ('(' identifier-list ')')?

table-alias ::=
  'AS'? identifier

use-partition ::=
  'PARTITION' '(' identifier-list ')'

identifier-list ::=
  identifier (',' identifier)*
```

Every derived table requires an alias. A comma in a `table-reference-list` is
a cross join with lower precedence than explicit `JOIN` operators.

```
index-hint-list ::=
  index-hint (',' index-hint)*

index-hint ::=
  'USE' ('INDEX' | 'KEY') index-hint-scope? '(' index-list? ')'
  ('IGNORE' | 'FORCE') ('INDEX' | 'KEY') index-hint-scope? '(' index-list ')'

index-hint-scope ::=
  'FOR' 'JOIN'
  'FOR' 'ORDER' 'BY'
  'FOR' 'GROUP' 'BY'

index-list ::=
  index-name (',' index-name)*

index-name ::=
  identifier
  'PRIMARY'
```

### WHERE, GROUP BY, HAVING

```
where-clause ::=
  'WHERE' expression

group-by-clause ::=
  'GROUP' 'BY' order-list ('WITH' 'ROLLUP')?

having-clause ::=
  'HAVING' expression
```

### Window Clause

```
window-clause ::=
  'WINDOW' window-definition (',' window-definition)*

window-definition ::=
  identifier 'AS' window-specification

window-specification ::=
  '(' identifier? partition-clause? order-by-clause? frame-clause? ')'

partition-clause ::=
  'PARTITION' 'BY' order-list

frame-clause ::=
  frame-units frame-extent frame-exclusion?

frame-units ::= (one of)
  ROWS RANGE

frame-extent ::=
  frame-start
  'BETWEEN' frame-bound 'AND' frame-bound

frame-start ::=
  'UNBOUNDED' 'PRECEDING'
  unsigned-bound 'PRECEDING'
  'CURRENT' 'ROW'

frame-bound ::=
  frame-start
  'UNBOUNDED' 'FOLLOWING'
  unsigned-bound 'FOLLOWING'

unsigned-bound ::=
  integer-literal
  placeholder

frame-exclusion ::=
  'EXCLUDE' 'CURRENT' 'ROW'
  'EXCLUDE' 'GROUP'
  'EXCLUDE' 'TIES'
  'EXCLUDE' 'NO' 'OTHERS'
```

### ORDER BY and LIMIT

```
order-by-clause ::=
  'ORDER' 'BY' order-list

order-list ::=
  order-item (',' order-item)*

order-item ::=
  expression ('ASC' | 'DESC')?

limit-clause ::=
  'LIMIT' limit-options
  'LIMIT' limit-options 'ROWS' 'EXAMINED' limit-option
  'LIMIT' 'ROWS' 'EXAMINED' limit-option
  fetch-first-clause

limit-options ::=
  limit-option
  limit-option ',' limit-option
  limit-option 'OFFSET' limit-option

fetch-first-clause ::=
  offset-clause? 'FETCH' ('FIRST' | 'NEXT') limit-option?
      ('ROW' | 'ROWS') ('ONLY' | 'WITH' 'TIES')
  offset-clause

offset-clause ::=
  'OFFSET' limit-option ('ROW' | 'ROWS')

simple-limit-clause ::=
  'LIMIT' limit-option

limit-option ::=
  integer-literal
  placeholder
```

In `LIMIT a, b`, `a` is the offset and `b` the row count; in
`LIMIT b OFFSET a` the order is reversed. Expressions are not permitted as
limit options. The SQL-standard `OFFSET ... FETCH`
form omits the count to default it to one row.

### INTO and Locking Clauses

```
into-clause ::=
  'INTO' 'OUTFILE' string-literal charset-clause? fields-clause? lines-clause?
  'INTO' 'DUMPFILE' string-literal
  'INTO' into-target (',' into-target)*

into-target ::=
  user-variable
  identifier

locking-clause ::=
  'FOR' 'UPDATE' locked-row-action?
  'LOCK' 'IN' 'SHARE' 'MODE' locked-row-action?

locked-row-action ::=
  'WAIT' integer-literal
  'SKIP' 'LOCKED'
  'NOWAIT'
```

MariaDB does not support MySQL's `FOR SHARE` or the `OF table-list`
qualification; shared row locks are taken with `LOCK IN SHARE MODE`.

The `fields-clause` and `lines-clause` of `INTO OUTFILE` share the field- and
line-handling syntax of `LOAD DATA`:

```
fields-clause ::=
  ('FIELDS' | 'COLUMNS') field-term+

field-term ::=
  'TERMINATED' 'BY' string-literal
  'OPTIONALLY'? 'ENCLOSED' 'BY' string-literal
  'ESCAPED' 'BY' string-literal

lines-clause ::=
  'LINES' line-term+

line-term ::=
  'STARTING' 'BY' string-literal
  'TERMINATED' 'BY' string-literal
```

### Common Table Expressions

```
with-clause ::=
  'WITH' 'RECURSIVE'? common-table-expression (',' common-table-expression)*

common-table-expression ::=
  identifier ('(' identifier-list ')')? 'AS' subquery cycle-clause?

cycle-clause ::=
  'CYCLE' identifier-list 'RESTRICT'
```

The `CYCLE` clause is permitted only on recursive CTEs.

### VALUES Statements

A `VALUES` statement (table value constructor) is a `query-primary` and may
stand alone as a complete statement, combine in set operations, and take the
surrounding `query-expression`'s `ORDER BY` and `LIMIT`.

```
values-statement ::=
  'VALUES' row-constructor (',' row-constructor)*

row-constructor ::=
  '(' value-list? ')'

value-list ::=
  (expression | 'DEFAULT') (',' (expression | 'DEFAULT'))*
```

MariaDB does not support MySQL's `ROW(...)` row-constructor keyword in
`VALUES` statements, nor MySQL's standalone `TABLE t` statement.

## INSERT and REPLACE

```
insert-statement ::=
  'INSERT' insert-lock-option? 'IGNORE'? 'INTO'? table-ref use-partition?
      insert-body on-duplicate-key-update? returning-clause?

insert-lock-option ::= (one of)
  LOW_PRIORITY DELAYED HIGH_PRIORITY

insert-body ::=
  insert-column-list? insert-values
  'SET' assignment-list
  insert-column-list? query-expression
  insert-column-list? subquery

insert-column-list ::=
  '(' insert-columns? ')'

insert-columns ::=
  column-ref (',' column-ref)*

insert-values ::=
  ('VALUES' | 'VALUE') insert-row (',' insert-row)*

insert-row ::=
  '(' value-list? ')'

returning-clause ::=
  'RETURNING' select-item-list

on-duplicate-key-update ::=
  'ON' 'DUPLICATE' 'KEY' 'UPDATE' assignment-list

assignment-list ::=
  assignment (',' assignment)*

assignment ::=
  column-ref '=' (expression | 'DEFAULT')
```

The `VALUES(col)` form (see [Simple expressions](#simple-expressions))
refers to the would-be inserted value inside `ON DUPLICATE KEY UPDATE`.

```
replace-statement ::=
  'REPLACE' ('LOW_PRIORITY' | 'DELAYED')? 'INTO'? table-ref use-partition?
      replace-body returning-clause?

replace-body ::=
  insert-column-list? insert-values
  'SET' assignment-list
  insert-column-list? query-expression
  insert-column-list? subquery
```

## UPDATE

```
update-statement ::=
  with-clause? 'UPDATE' 'LOW_PRIORITY'? 'IGNORE'? table-reference-list
      'SET' assignment-list where-clause? order-by-clause? simple-limit-clause?
      returning-clause?
```

`ORDER BY` and `LIMIT` are permitted only in the single-table form (one table
reference, no join); a multi-table update with either is an error and must
be rejected.

## DELETE

```
delete-statement ::=
  with-clause? 'DELETE' delete-option* 'FROM' table-ref table-alias?
      use-partition? where-clause? order-by-clause? simple-limit-clause?
      returning-clause?
  with-clause? 'DELETE' delete-option* table-alias-ref-list
      'FROM' table-reference-list where-clause?
  with-clause? 'DELETE' delete-option* 'FROM' table-alias-ref-list
      'USING' table-reference-list where-clause?

delete-option ::= (one of)
  QUICK LOW_PRIORITY IGNORE

table-alias-ref-list ::=
  table-alias-ref (',' table-alias-ref)*

table-alias-ref ::=
  table-ref ('.' '*')?
```

`RETURNING` is permitted only in the single-table form.

## LOAD DATA and HANDLER

```
load-data-statement ::=
  'LOAD' ('DATA' | 'XML') ('LOW_PRIORITY' | 'CONCURRENT')? 'LOCAL'? 'INFILE'
      string-literal ('REPLACE' | 'IGNORE')? 'INTO' 'TABLE' table-ref
      use-partition? charset-clause? xml-rows-identified-by? fields-clause?
      lines-clause? load-data-tail

xml-rows-identified-by ::=
  'ROWS' 'IDENTIFIED' 'BY' string-literal

load-data-tail ::=
  ('IGNORE' integer-literal ('LINES' | 'ROWS'))? load-target-list?
      ('SET' assignment-list)?

load-target-list ::=
  '(' load-target (',' load-target)* ')'

load-target ::=
  column-ref
  user-variable
  '@@'

charset-clause ::=
  ('CHARACTER' 'SET' | 'CHARSET') charset-name

handler-statement ::=
  'HANDLER' table-ref 'OPEN' table-alias?
  'HANDLER' identifier 'CLOSE'
  'HANDLER' identifier 'READ' handler-read-or-scan where-clause? limit-clause?

handler-read-or-scan ::=
  'FIRST'
  'NEXT'
  identifier ('FIRST' | 'NEXT' | 'PREV' | 'LAST')
  identifier ('=' | '<' | '>' | '<=' | '>=') '(' value-list ')'

import-statement ::=
  'IMPORT' 'TABLE' 'FROM' string-literal (',' string-literal)*
```

`LOAD XML` uses the same file-handling surface as `LOAD DATA`, but treats the
payload as row-oriented XML and may add implementation-specific validation
after parsing. The common `INFILE` form above is the stable surface
shared with `INTO OUTFILE`.

## Transactions and Locking

```
transaction-statement ::=
  'START' 'TRANSACTION' transaction-characteristic*
  'BEGIN' 'WORK'?
  'COMMIT' 'WORK'? ('AND' 'NO'? 'CHAIN')? ('NO'? 'RELEASE')?
  'ROLLBACK' 'WORK'? ('AND' 'NO'? 'CHAIN')? ('NO'? 'RELEASE')?

transaction-characteristic ::=
  'WITH' 'CONSISTENT' 'SNAPSHOT'
  'READ' 'WRITE'
  'READ' 'ONLY'

savepoint-statement ::=
  'SAVEPOINT' identifier
  'ROLLBACK' 'WORK'? 'TO' 'SAVEPOINT'? identifier
  'RELEASE' 'SAVEPOINT' identifier

lock-statement ::=
  'LOCK' ('TABLES' | 'TABLE') lock-item (',' lock-item)*
  'UNLOCK' ('TABLES' | 'TABLE')

lock-item ::=
  table-ref table-alias? lock-type

lock-type ::=
  'READ' 'LOCAL'?
  'LOW_PRIORITY'? 'WRITE'

xa-statement ::=
  'XA' ('START' | 'BEGIN') xid ('JOIN' | 'RESUME')?
  'XA' 'END' xid ('SUSPEND' ('FOR' 'MIGRATE')?)?
  'XA' 'PREPARE' xid
  'XA' 'COMMIT' xid ('ONE' 'PHASE')?
  'XA' 'ROLLBACK' xid
  'XA' 'RECOVER' ('FORMAT' '=' format-name)?

xid ::=
  string-literal (',' string-literal (',' integer-literal)?)?

format-name ::=
  identifier
  string-literal
```

The `XA RECOVER` format name must be `SQL` or `RAW`.

## SET

```
set-statement ::=
  'SET' set-assignment (',' set-assignment)*
  'SET' set-scope? 'TRANSACTION' transaction-attribute (',' transaction-attribute)*
  'SET' 'NAMES' (charset-name ('COLLATE' collation-name)? | 'DEFAULT')
  'SET' ('CHARACTER' 'SET' | 'CHARSET') (charset-name | 'DEFAULT')
  'SET' 'STATEMENT' set-assignment (',' set-assignment)* 'FOR' statement

set-assignment ::=
  set-scope? identifier ('.' identifier)? '=' set-value
  system-variable '=' set-value
  user-variable '=' expression

set-scope ::= (one of)
  GLOBAL SESSION LOCAL

set-value ::=
  expression
  'DEFAULT'

transaction-attribute ::=
  'ISOLATION' 'LEVEL' isolation-level
  'READ' 'WRITE'
  'READ' 'ONLY'

isolation-level ::=
  'REPEATABLE' 'READ'
  'READ' 'COMMITTED'
  'READ' 'UNCOMMITTED'
  'SERIALIZABLE'

charset-name ::=
  identifier
  string-literal
  'BINARY'

collation-name ::=
  identifier
  string-literal
  'BINARY'
```

In a `set-value`, the bare keywords `ON`, `OFF`, `ALL`, and `BINARY` are also
accepted where an expression is expected (`SET autocommit = ON`).

## Data Definition

### CREATE DATABASE

```
create-database-statement ::=
  'CREATE' or-replace? ('DATABASE' | 'SCHEMA') if-not-exists? identifier
      create-database-option*

create-database-option ::=
  'DEFAULT'? ('CHARACTER' 'SET' | 'CHARSET') '='? charset-name
  'DEFAULT'? 'COLLATE' '='? collation-name
  'COMMENT' '='? string-literal

or-replace ::=
  'OR' 'REPLACE'

if-not-exists ::=
  'IF' 'NOT' 'EXISTS'

if-exists ::=
  'IF' 'EXISTS'
```

### CREATE TABLE

```
create-table-statement ::=
  'CREATE' or-replace? 'TEMPORARY'? 'TABLE' if-not-exists? table-ref
      ('(' table-element-list ')')? create-table-tail?
  'CREATE' or-replace? 'TEMPORARY'? 'TABLE' if-not-exists? table-ref
      'LIKE' table-ref
  'CREATE' or-replace? 'TEMPORARY'? 'TABLE' if-not-exists? table-ref
      '(' 'LIKE' table-ref ')'

create-table-tail ::=
  table-options? table-partition-clause? duplicate-as-query?

table-options ::=
  table-option (','? table-option)*

duplicate-as-query ::=
  ('REPLACE' | 'IGNORE')? 'AS'? query-expression

table-element-list ::=
  table-element (',' table-element)*

table-element ::=
  column-definition
  table-constraint

table-partition-clause ::=
  'PARTITION' 'BY' partition-type ('PARTITIONS' integer-literal)?
      subpartition-clause? partition-definition-list?

partition-type ::=
  'LINEAR'? 'KEY' partition-key-algorithm? '(' identifier-list? ')'
  'LINEAR'? 'HASH' '(' expression ')'
  ('RANGE' | 'LIST')
      ('(' expression ')' | 'COLUMNS' '(' identifier-list? ')')

partition-key-algorithm ::=
  'ALGORITHM' '='? integer-literal

subpartition-clause ::=
  'SUBPARTITION' 'BY' 'LINEAR'?
      ('HASH' '(' expression ')'
       | 'KEY' partition-key-algorithm? '(' identifier-list ')')
      ('SUBPARTITIONS' integer-literal)?

partition-definition-list ::=
  '(' partition-definition (',' partition-definition)* ')'

partition-definition ::=
  'PARTITION' identifier partition-bound? partition-option*
      ('(' subpartition-definition (',' subpartition-definition)* ')')?

partition-bound ::=
  'VALUES' 'LESS' 'THAN' (partition-value-list | 'MAXVALUE')
  'VALUES' 'IN' partition-in-list

partition-in-list ::=
  partition-value-list
  '(' partition-value-list (',' partition-value-list)* ')'

partition-value-list ::=
  '(' partition-value (',' partition-value)* ')'

partition-value ::=
  expression
  'MAXVALUE'

partition-option ::=
  'TABLESPACE' '='? identifier
  'STORAGE'? 'ENGINE' '='? identifier
  'NODEGROUP' '='? integer-literal
  ('MAX_ROWS' | 'MIN_ROWS') '='? integer-literal
  ('DATA' | 'INDEX') 'DIRECTORY' '='? string-literal
  'COMMENT' '='? string-literal

subpartition-definition ::=
  'SUBPARTITION' identifier partition-option*
```

### Column Definitions

```
column-definition ::=
  identifier data-type column-attribute* references-clause?
  identifier data-type collate-attribute? ('GENERATED' 'ALWAYS')? 'AS'
      '(' expression ')' ('VIRTUAL' | 'STORED' | 'PERSISTENT')?
      generated-column-attribute*

generated-column-attribute ::=
  'UNIQUE' 'KEY'?
  'COMMENT' string-literal
  'INVISIBLE'

column-attribute ::=
  'NOT'? 'NULL'
  'DEFAULT' default-value
  'ON' 'UPDATE' now-function
  'AUTO_INCREMENT'
  'SERIAL' 'DEFAULT' 'VALUE'
  'PRIMARY'? 'KEY'
  'UNIQUE' 'KEY'?
  'COMMENT' string-literal
  collate-attribute
  constraint-name? check-constraint
  'INVISIBLE'
  engine-option

engine-option ::=
  identifier '=' (string-literal | identifier | integer-literal)

default-value ::=
  signed-literal
  now-function
  '(' expression ')'

signed-literal ::=
  ('+' | '-')? numeric-literal
  literal

now-function ::=
  ('NOW' | 'CURRENT_TIMESTAMP' | 'LOCALTIME' | 'LOCALTIMESTAMP')
      ('(' integer-literal? ')')?

collate-attribute ::=
  'COLLATE' collation-name

check-constraint ::=
  'CHECK' '(' expression ')'

constraint-name ::=
  'CONSTRAINT' identifier?
```

An inline `references-clause` on a column is parsed but ignored (it creates
no foreign key); only a table-level `FOREIGN KEY` constraint does. An
`engine-option` (`name = value`) attaches an engine-defined attribute to the
column.

### Table Constraints

```
table-constraint ::=
  ('KEY' | 'INDEX') index-name? index-type-clause? key-list index-option*
  'FULLTEXT' ('KEY' | 'INDEX')? index-name? key-list index-option*
  ('SPATIAL' | 'VECTOR') ('KEY' | 'INDEX')? index-name? key-list index-option*
  constraint-name? 'PRIMARY' 'KEY' index-type-clause? key-list index-option*
  constraint-name? 'UNIQUE' ('KEY' | 'INDEX')? index-name? index-type-clause?
      key-list index-option*
  constraint-name? 'FOREIGN' 'KEY' index-name? key-list references-clause
  constraint-name? check-constraint

key-list ::=
  '(' key-part (',' key-part)* ')'

key-part ::=
  identifier ('(' integer-literal ')')? ('ASC' | 'DESC')?
  '(' expression ')' ('ASC' | 'DESC')?

index-type-clause ::=
  ('USING' | 'TYPE') ('BTREE' | 'RTREE' | 'HASH')

index-option ::=
  'KEY_BLOCK_SIZE' '='? integer-literal
  'COMMENT' string-literal
  'NOT'? 'IGNORED'
  index-type-clause
  'WITH' 'PARSER' identifier

references-clause ::=
  'REFERENCES' table-ref ('(' identifier-list ')')?
      ('MATCH' ('FULL' | 'PARTIAL' | 'SIMPLE'))? reference-actions?

reference-actions ::=
  'ON' 'UPDATE' reference-action ('ON' 'DELETE' reference-action)?
  'ON' 'DELETE' reference-action ('ON' 'UPDATE' reference-action)?

reference-action ::=
  'RESTRICT'
  'CASCADE'
  'SET' 'NULL'
  'SET' 'DEFAULT'
  'NO' 'ACTION'
```

### Table Options

```
table-option ::=
  'ENGINE' '='? identifier
  'SECONDARY_ENGINE' '='? (identifier | 'NULL')
  'AUTO_INCREMENT' '='? integer-literal
  'DEFAULT'? ('CHARACTER' 'SET' | 'CHARSET') '='? charset-name
  'DEFAULT'? 'COLLATE' '='? collation-name
  'COMMENT' '='? string-literal
  'PASSWORD' '='? string-literal
  'CONNECTION' '='? string-literal
  'COMPRESSION' '='? string-literal
  'ENCRYPTION' '='? string-literal
  'ROW_FORMAT' '='? ('DEFAULT' | 'DYNAMIC' | 'FIXED' | 'COMPRESSED'
                     | 'REDUNDANT' | 'COMPACT' | 'PAGE')
  'KEY_BLOCK_SIZE' '='? integer-literal
  'MAX_ROWS' '='? integer-literal
  'MIN_ROWS' '='? integer-literal
  'AVG_ROW_LENGTH' '='? integer-literal
  'CHECKSUM' '='? integer-literal
  'TABLE_CHECKSUM' '='? integer-literal
  'DELAY_KEY_WRITE' '='? integer-literal
  'PACK_KEYS' '='? (integer-literal | 'DEFAULT')
  'STATS_AUTO_RECALC' '='? (integer-literal | 'DEFAULT')
  'STATS_PERSISTENT' '='? (integer-literal | 'DEFAULT')
  'STATS_SAMPLE_PAGES' '='? (integer-literal | 'DEFAULT')
  'UNION' '='? '(' table-ref-list ')'
  'INSERT_METHOD' '='? ('NO' | 'FIRST' | 'LAST')
  'DATA' 'DIRECTORY' '='? string-literal
  'INDEX' 'DIRECTORY' '='? string-literal
  'TABLESPACE' '='? identifier
  'STORAGE' ('DISK' | 'MEMORY')
  'START' 'TRANSACTION'
  'ENGINE_ATTRIBUTE' '='? string-literal
  'SECONDARY_ENGINE_ATTRIBUTE' '='? string-literal
  'AUTOEXTEND_SIZE' '='? size-number
  engine-option

size-number ::=
  integer-literal
  identifier
```

Table options may be separated by whitespace or commas. Engine-specific options
such as InnoDB's `PAGE_COMPRESSED` or `ENCRYPTED` still parse through the
generic `engine-option` (`name = value`) production.

### Data Types

```
data-type ::=
  ('INT' | 'INTEGER' | 'TINYINT' | 'SMALLINT' | 'MEDIUMINT' | 'BIGINT')
      field-length? field-option*
  ('REAL' | 'DOUBLE' 'PRECISION'?) precision? field-option*
  ('FLOAT' | 'DECIMAL' | 'DEC' | 'NUMERIC' | 'FIXED') float-options? field-option*
  'BIT' field-length?
  ('BOOL' | 'BOOLEAN')
  'CHAR' field-length? charset-modifier?
  ('NCHAR' | 'NATIONAL' 'CHAR') field-length? 'BINARY'?
  'BINARY' field-length?
  ('VARCHAR' | 'CHAR' 'VARYING') field-length charset-modifier?
  ('NVARCHAR' | 'NCHAR' 'VARYING' | 'NATIONAL' 'VARCHAR') field-length 'BINARY'?
  'VARBINARY' field-length
  'YEAR' field-length?
  'DATE'
  'TIME' field-length?
  'TIMESTAMP' field-length?
  'DATETIME' field-length?
  'TINYBLOB'
  'BLOB' field-length?
  'MEDIUMBLOB'
  'LONGBLOB'
  'LONG' 'VARBINARY'
  'LONG' ('CHAR' 'VARYING' | 'VARCHAR')? charset-modifier?
  'TINYTEXT' charset-modifier?
  'TEXT' field-length? charset-modifier?
  'MEDIUMTEXT' charset-modifier?
  'LONGTEXT' charset-modifier?
  'ENUM' string-list charset-modifier?
  'SET' string-list charset-modifier?
  'SERIAL'
  'JSON'
  ('GEOMETRY' | 'GEOMETRYCOLLECTION' | 'POINT' | 'MULTIPOINT' | 'LINESTRING'
   | 'MULTILINESTRING' | 'POLYGON' | 'MULTIPOLYGON')
  identifier float-options?

field-length ::=
  '(' integer-literal ')'

precision ::=
  '(' integer-literal ',' integer-literal ')'

float-options ::=
  field-length
  precision

field-option ::= (one of)
  SIGNED UNSIGNED ZEROFILL

charset-modifier ::=
  'ASCII' 'BINARY'?
  'UNICODE' 'BINARY'?
  'BYTE'
  'BINARY' (('CHARACTER' 'SET' | 'CHARSET') charset-name)?
  ('CHARACTER' 'SET' | 'CHARSET') charset-name 'BINARY'?

string-list ::=
  '(' string-literal (',' string-literal)* ')'
```

`SERIAL` is shorthand for `BIGINT UNSIGNED NOT NULL AUTO_INCREMENT UNIQUE`.
Under `REAL_AS_FLOAT`, `REAL` denotes `FLOAT` rather than `DOUBLE`.

### CREATE INDEX

```
create-index-statement ::=
  'CREATE' or-replace? ('UNIQUE' | 'FULLTEXT' | 'SPATIAL' | 'VECTOR')?
      'INDEX' if-not-exists? identifier index-type-clause?
      'ON' table-ref key-list index-option* index-lock-and-algorithm*

index-lock-and-algorithm ::=
  'ALGORITHM' '='? ('DEFAULT' | identifier)
  'LOCK' '='? ('DEFAULT' | identifier)
```

The algorithm name must be one of `DEFAULT`, `COPY`, `INPLACE`, `NOCOPY`,
or `INSTANT`, and the lock name one of `DEFAULT`, `NONE`, `SHARED`, or
`EXCLUSIVE`; both parse as plain identifiers and are validated after
parsing.

### ALTER TABLE

```
alter-table-statement ::=
  'ALTER' 'TABLE' table-ref alter-action (',' alter-action)*

alter-action ::=
  'ADD' 'COLUMN'? if-not-exists? column-definition column-position?
  'ADD' 'COLUMN'? if-not-exists? '(' table-element-list ')'
  'ADD' table-constraint
  'CHANGE' 'COLUMN'? if-exists? identifier column-definition column-position?
  'MODIFY' 'COLUMN'? if-exists? column-definition column-position?
  'DROP' 'COLUMN'? if-exists? identifier ('RESTRICT' | 'CASCADE')?
  'DROP' ('KEY' | 'INDEX') if-exists? index-name
  'DROP' 'PRIMARY' 'KEY'
  'DROP' 'FOREIGN' 'KEY' if-exists? identifier
  'DROP' ('CHECK' | 'CONSTRAINT') if-exists? identifier
  'ALTER' 'COLUMN'? if-exists? identifier 'SET' 'DEFAULT' default-value
  'ALTER' 'COLUMN'? if-exists? identifier 'DROP' 'DEFAULT'
  'ALTER' ('KEY' | 'INDEX') if-exists? index-name 'NOT'? 'IGNORED'
  'RENAME' 'COLUMN' identifier 'TO' identifier
  'RENAME' ('KEY' | 'INDEX') index-name 'TO' index-name
  'RENAME' ('TO' | 'AS')? table-ref
  'CONVERT' 'TO' ('CHARACTER' 'SET' | 'CHARSET') charset-name
      ('COLLATE' collation-name)?
  'ORDER' 'BY' identifier-list
  alter-partition-action
  table-option+
  index-lock-and-algorithm

column-position ::=
  'FIRST'
  'AFTER' identifier

alter-partition-action ::=
  'ADD' 'PARTITION' (partition-definition-list | 'PARTITIONS' integer-literal)
  'DROP' 'PARTITION' identifier-list
  'COALESCE' 'PARTITION' integer-literal
  'REORGANIZE' 'PARTITION' identifier-list ('INTO' partition-definition-list)?
  'ANALYZE' 'PARTITION' ('ALL' | identifier-list)
  'CHECK' 'PARTITION' ('ALL' | identifier-list)
  'OPTIMIZE' 'PARTITION' ('ALL' | identifier-list)
  'REBUILD' 'PARTITION' ('ALL' | identifier-list)
  'REPAIR' 'PARTITION' ('ALL' | identifier-list)
  'TRUNCATE' 'PARTITION' ('ALL' | identifier-list)
  'EXCHANGE' 'PARTITION' identifier 'WITH' 'TABLE' table-ref
      ('WITH' | 'WITHOUT') 'VALIDATION'?
  'DISCARD' 'PARTITION' ('ALL' | identifier-list) 'TABLESPACE'
  'IMPORT' 'PARTITION' ('ALL' | identifier-list) 'TABLESPACE'
  'REMOVE' 'PARTITIONING'
```

### DROP, TRUNCATE, RENAME

```
drop-statement ::=
  'DROP' ('DATABASE' | 'SCHEMA') if-exists? identifier
  'DROP' 'TEMPORARY'? ('TABLE' | 'TABLES') if-exists? table-ref-list
      ('RESTRICT' | 'CASCADE')?
  'DROP' 'INDEX' if-exists? index-name 'ON' table-ref
      index-lock-and-algorithm*
  'DROP' 'VIEW' if-exists? table-ref-list ('RESTRICT' | 'CASCADE')?

table-ref-list ::=
  table-ref (',' table-ref)*

truncate-statement ::=
  'TRUNCATE' 'TABLE'? table-ref

rename-table-statement ::=
  'RENAME' ('TABLE' | 'TABLES') if-exists? rename-pair (',' rename-pair)*

rename-pair ::=
  table-ref 'TO' table-ref
```

### Views

```
create-view-statement ::=
  'CREATE' view-replace-or-algorithm? definer-clause? sql-security-clause?
      'VIEW' table-ref view-tail

alter-view-statement ::=
  'ALTER' view-algorithm? definer-clause? sql-security-clause? 'VIEW'
      table-ref view-tail

view-replace-or-algorithm ::=
  'OR' 'REPLACE' view-algorithm?
  view-algorithm

view-algorithm ::=
  'ALGORITHM' '=' ('UNDEFINED' | 'MERGE' | 'TEMPTABLE')

sql-security-clause ::=
  'SQL' 'SECURITY' ('DEFINER' | 'INVOKER')

view-tail ::=
  ('(' identifier-list ')')? 'AS' query-expression view-check-option?

view-check-option ::=
  'WITH' ('CASCADED' | 'LOCAL')? 'CHECK' 'OPTION'

definer-clause ::=
  'DEFINER' '=' user
```

### Stored Routines, Triggers, and Events

```
create-procedure-statement ::=
  'CREATE' definer-clause? 'PROCEDURE' if-not-exists? table-ref
      '(' procedure-parameter-list? ')' routine-option* stored-routine-body

alter-procedure-statement ::=
  'ALTER' 'PROCEDURE' table-ref routine-option*

create-function-statement ::=
  'CREATE' definer-clause? 'FUNCTION' if-not-exists? table-ref
      '(' function-parameter-list? ')' 'RETURNS' type-with-collation
      routine-option* stored-routine-body

alter-function-statement ::=
  'ALTER' 'FUNCTION' table-ref routine-option*

procedure-parameter-list ::=
  procedure-parameter (',' procedure-parameter)*

procedure-parameter ::=
  ('IN' | 'OUT' | 'INOUT')? identifier type-with-collation

function-parameter-list ::=
  function-parameter (',' function-parameter)*

function-parameter ::=
  identifier type-with-collation

type-with-collation ::=
  data-type collate-attribute?

routine-option ::=
  'COMMENT' string-literal
  'LANGUAGE' ('SQL' | identifier)
  'NO' 'SQL'
  'CONTAINS' 'SQL'
  'READS' 'SQL' 'DATA'
  'MODIFIES' 'SQL' 'DATA'
  'SQL' 'SECURITY' ('DEFINER' | 'INVOKER')
  'NOT'? 'DETERMINISTIC'

stored-routine-body ::=
  compound-statement
  'AS' string-literal

compound-statement ::=
  statement
  return-statement
  if-statement
  case-statement
  labeled-block
  unlabeled-block
  labeled-control
  unlabeled-control
  leave-statement
  iterate-statement
  cursor-open
  cursor-fetch
  cursor-close

return-statement ::=
  'RETURN' expression

if-statement ::=
  'IF' expression 'THEN' compound-statement-list
      ('ELSEIF' expression 'THEN' compound-statement-list)*
      ('ELSE' compound-statement-list)? 'END' 'IF'

compound-statement-list ::=
  (compound-statement ';')+

case-statement ::=
  'CASE' expression? ('WHEN' expression 'THEN' compound-statement-list)+
      ('ELSE' compound-statement-list)? 'END' 'CASE'

label ::=
  identifier

labeled-block ::=
  label ':' begin-end-block label-ref?

unlabeled-block ::=
  begin-end-block

begin-end-block ::=
  'BEGIN' stored-program-declaration* compound-statement-list? 'END'

stored-program-declaration ::=
  variable-declaration ';'
  condition-declaration ';'
  handler-declaration ';'
  cursor-declaration ';'

variable-declaration ::=
  'DECLARE' identifier-list data-type collate-attribute? ('DEFAULT' expression)?

condition-declaration ::=
  'DECLARE' identifier 'CONDITION' 'FOR' (integer-literal | sqlstate-literal)

handler-declaration ::=
  'DECLARE' ('CONTINUE' | 'EXIT' | 'UNDO') 'HANDLER' 'FOR'
      handler-condition (',' handler-condition)* compound-statement

handler-condition ::=
  integer-literal
  sqlstate-literal
  identifier
  'SQLWARNING'
  'NOT' 'FOUND'
  'SQLEXCEPTION'

cursor-declaration ::=
  'DECLARE' identifier 'CURSOR' 'FOR' select-statement

labeled-control ::=
  label ':' unlabeled-control label-ref?

unlabeled-control ::=
  loop-block
  while-do-block
  repeat-until-block

loop-block ::=
  'LOOP' compound-statement-list 'END' 'LOOP'

while-do-block ::=
  'WHILE' expression 'DO' compound-statement-list 'END' 'WHILE'

repeat-until-block ::=
  'REPEAT' compound-statement-list 'UNTIL' expression 'END' 'REPEAT'

leave-statement ::=
  'LEAVE' label-ref

iterate-statement ::=
  'ITERATE' label-ref

cursor-open ::=
  'OPEN' identifier

cursor-fetch ::=
  'FETCH' 'NEXT'? 'FROM'? identifier 'INTO' identifier-list

cursor-close ::=
  'CLOSE' identifier

label-ref ::=
  identifier

sqlstate-literal ::=
  'SQLSTATE' 'VALUE'? string-literal

create-trigger-statement ::=
  'CREATE' definer-clause? 'TRIGGER' if-not-exists? table-ref
      ('BEFORE' | 'AFTER') ('INSERT' | 'UPDATE' | 'DELETE') 'ON' table-ref
      'FOR' 'EACH' 'ROW' trigger-order-clause? compound-statement

trigger-order-clause ::=
  ('FOLLOWS' | 'PRECEDES') identifier

drop-trigger-statement ::=
  'DROP' 'TRIGGER' if-exists? table-ref

create-event-statement ::=
  'CREATE' definer-clause? 'EVENT' if-not-exists? table-ref 'ON' 'SCHEDULE'
      event-schedule event-completion-clause? event-status-clause?
      event-comment-clause? 'DO' compound-statement

alter-event-statement ::=
  'ALTER' definer-clause? 'EVENT' table-ref ('ON' 'SCHEDULE' event-schedule)?
      event-completion-clause? ('RENAME' 'TO' table-ref)? event-status-clause?
      event-comment-clause? ('DO' compound-statement)?

drop-event-statement ::=
  'DROP' 'EVENT' if-exists? table-ref

event-schedule ::=
  'AT' expression
  'EVERY' expression interval-unit ('STARTS' expression)? ('ENDS' expression)?

event-completion-clause ::=
  'ON' 'COMPLETION' 'NOT'? 'PRESERVE'

event-status-clause ::=
  'ENABLE'
  'DISABLE' ('ON' ('SLAVE' | 'REPLICA'))?

event-comment-clause ::=
  'COMMENT' string-literal
```

Inside stored-program bodies, the `statement` alternative in
`compound-statement` means an ordinary SQL statement that is legal in a stored
program body; `BEGIN WORK` is a transaction statement, not a block opener.

## Prepared Statements

```
prepare-statement ::=
  'PREPARE' identifier 'FROM' expression

execute-statement ::=
  'EXECUTE' identifier ('USING' expression (',' expression)*)?
  'EXECUTE' 'IMMEDIATE' expression ('USING' expression (',' expression)*)?

deallocate-statement ::=
  ('DEALLOCATE' | 'DROP') 'PREPARE' identifier
```

The `PREPARE ... FROM` source and `USING` arguments are general expressions
(subqueries excluded), not just string literals and user variables. A `USING`
argument may also be the bare keyword `DEFAULT` or `IGNORE`.

## Utility Statements

```
use-statement ::=
  'USE' identifier

call-statement ::=
  'CALL' table-ref ('(' expression-list? ')')?

do-statement ::=
  'DO' expression (',' expression)*
```

### EXPLAIN and DESCRIBE

`DESCRIBE`, `DESC`, and `EXPLAIN` are interchangeable keywords; the two
statement forms are distinguished by what follows.

```
describe-statement ::=
  ('DESCRIBE' | 'DESC' | 'EXPLAIN') table-ref (identifier | string-literal)?

explain-statement ::=
  ('DESCRIBE' | 'DESC' | 'EXPLAIN') explain-option? explainable-statement
  ('DESCRIBE' | 'DESC' | 'EXPLAIN') format-option? 'FOR' 'CONNECTION'
      expression

explain-option ::=
  'EXTENDED' 'ALL'?
  'PARTITIONS'
  format-option

format-option ::=
  'FORMAT' '=' ('TRADITIONAL' | 'JSON')

analyze-statement ::=
  'ANALYZE' format-option? explainable-statement

explainable-statement ::=
  select-statement
  insert-statement
  replace-statement
  update-statement
  delete-statement
```

### SHOW

```
show-statement ::=
  'SHOW' 'DATABASES' like-or-where?
  'SHOW' 'FULL'? 'TABLES' in-database? like-or-where?
  'SHOW' 'FULL'? 'TRIGGERS' in-database? like-or-where?
  'SHOW' 'EVENTS' in-database? like-or-where?
  'SHOW' 'OPEN' 'TABLES' in-database? like-or-where?
  'SHOW' 'FULL'? ('COLUMNS' | 'FIELDS') ('FROM' | 'IN') table-ref
      in-database? like-or-where?
  'SHOW' 'BINARY' 'LOGS'
  'SHOW' 'MASTER' 'LOGS'
  'SHOW' 'BINARY' 'LOG' 'STATUS'
  'SHOW' ('REPLICA' 'HOSTS' | 'REPLICAS')
  'SHOW' 'BINLOG' 'EVENTS' ('IN' string-literal)?
      ('FROM' integer-literal)? limit-clause? channel-clause?
  'SHOW' 'RELAYLOG' 'EVENTS' ('IN' string-literal)?
      ('FROM' integer-literal)? limit-clause? channel-clause?
  'SHOW' ('INDEX' | 'INDEXES' | 'KEYS') ('FROM' | 'IN') table-ref
      in-database? where-clause?
  'SHOW' 'PLUGINS'
  'SHOW' 'ENGINE' (identifier | 'ALL') ('LOGS' | 'MUTEX' | 'STATUS')
  'SHOW' 'CREATE' ('DATABASE' | 'SCHEMA') if-not-exists? identifier
  'SHOW' 'CREATE' 'TABLE' table-ref
  'SHOW' 'CREATE' 'VIEW' table-ref
  'SHOW' 'CREATE' 'PROCEDURE' table-ref
  'SHOW' 'CREATE' 'FUNCTION' table-ref
  'SHOW' 'CREATE' 'TRIGGER' table-ref
  'SHOW' 'CREATE' 'EVENT' table-ref
  'SHOW' 'CREATE' 'USER' user
  'SHOW' 'PROCEDURE' 'STATUS' like-or-where?
  'SHOW' 'FUNCTION' 'STATUS' like-or-where?
  'SHOW' 'PROCEDURE' 'CODE' table-ref
  'SHOW' 'FUNCTION' 'CODE' table-ref
  'SHOW' 'TABLE' 'STATUS' in-database? like-or-where?
  'SHOW' ('GLOBAL' | 'SESSION')? 'VARIABLES' like-or-where?
  'SHOW' ('GLOBAL' | 'SESSION')? 'STATUS' like-or-where?
  'SHOW' ('CHARACTER' 'SET' | 'CHARSET') like-or-where?
  'SHOW' 'COLLATION' like-or-where?
  'SHOW' 'PRIVILEGES'
  'SHOW' ('STORAGE'? 'ENGINES')
  'SHOW' 'FULL'? 'PROCESSLIST'
  'SHOW' 'PROFILE' profile-definition* ('FOR' 'QUERY' integer-literal)?
      limit-clause?
  'SHOW' 'PROFILES'
  'SHOW' 'WARNINGS' limit-clause?
  'SHOW' 'ERRORS' limit-clause?
  'SHOW' 'COUNT' '(' '*' ')' 'WARNINGS'
  'SHOW' 'COUNT' '(' '*' ')' 'ERRORS'
  'SHOW' 'GRANTS' ('FOR' user ('USING' user-list)?)?
  'SHOW' ('SLAVE' | 'REPLICA') 'STATUS' channel-clause?

in-database ::=
  ('FROM' | 'IN') identifier

like-or-where ::=
  'LIKE' string-literal
  'WHERE' expression

channel-clause ::=
  'FOR' 'CHANNEL' identifier

profile-definition ::= (one of)
  ALL BLOCK IO CONTEXT SWITCHES CPU IPC MEMORY PAGE FAULTS
  SOURCE SWAPS

user-list ::=
  user (',' user)*
```

### HELP and RESTART

```
help-statement ::=
  'HELP' identifier

restart-statement ::=
  'RESTART'
```

## Account Management and Roles

```
create-user-statement ::=
  'CREATE' 'USER' if-not-exists? user-spec (',' user-spec)*
      default-role-clause? user-tail?

alter-user-statement ::=
  'ALTER' 'USER' if-exists? alter-user-spec (',' alter-user-spec)* user-tail?
  'ALTER' 'USER' user 'DEFAULT' 'ROLE' ('ALL' | 'NONE' | role-list)
  'ALTER' 'USER' user user-registration?

drop-user-statement ::=
  'DROP' 'USER' if-exists? user-list

rename-user-statement ::=
  'RENAME' 'USER' user 'TO' user (',' user 'TO' user)*

grant-statement ::=
  'GRANT' role-or-privilege-list 'TO' user-list ('WITH' 'ADMIN' 'OPTION')?
  'GRANT' (role-or-privilege-list | 'ALL' 'PRIVILEGES'?) 'ON' acl-type?
      grant-identifier 'TO' grant-target-list require-clause? grant-options?
      grant-as?
  'GRANT' 'PROXY' 'ON' user 'TO' grant-target-list
      ('WITH' 'GRANT' 'OPTION')?

revoke-statement ::=
  'REVOKE' if-exists? role-or-privilege-list 'FROM' user-list
  'REVOKE' if-exists? role-or-privilege-list 'ON' acl-type? grant-identifier
      'FROM' user-list
  'REVOKE' if-exists? 'ALL' 'PRIVILEGES'?
      ('ON' acl-type? grant-identifier | ',' 'GRANT' 'OPTION') 'FROM' user-list
  'REVOKE' if-exists? 'PROXY' 'ON' user 'FROM' user-list

set-role-statement ::=
  'SET' 'ROLE' role-list
  'SET' 'ROLE' ('NONE' | 'DEFAULT')
  'SET' 'DEFAULT' 'ROLE' (role-list | 'NONE' | 'ALL') 'TO' role-list
  'SET' 'ROLE' 'ALL' ('EXCEPT' role-list)?

user-spec ::=
  user user-auth-chain?

alter-user-spec ::=
  user auth-spec?
  user 'DISCARD' 'OLD' 'PASSWORD'
  user 'ADD' factor-number auth-spec ('ADD' factor-number auth-spec)?
  user 'MODIFY' factor-number auth-spec ('MODIFY' factor-number auth-spec)?
  user 'DROP' factor-number ('DROP' factor-number)?

additional-auth-specs ::=
  'AND' auth-spec ('AND' auth-spec)?

user-auth-chain ::=
  auth-spec additional-auth-specs?

auth-spec ::=
  'IDENTIFIED' 'BY' string-literal
  'IDENTIFIED' 'BY' 'RANDOM' 'PASSWORD'
  'IDENTIFIED' 'WITH' identifier
  'IDENTIFIED' 'WITH' identifier 'AS' string-literal
  'IDENTIFIED' 'WITH' identifier 'BY' string-literal
  'IDENTIFIED' 'WITH' identifier 'BY' 'RANDOM' 'PASSWORD'

factor-number ::= (one of)
  '2' '3'

default-role-clause ::=
  'DEFAULT' 'ROLE' role-list

user-tail ::=
  require-clause? connect-options? account-option* user-attribute?

require-clause ::=
  'REQUIRE' ('SSL' | 'X509' | 'NONE' | require-element ('AND'? require-element)*)

require-element ::=
  'CIPHER' string-literal
  'ISSUER' string-literal
  'SUBJECT' string-literal

connect-options ::=
  'WITH' connect-option+

connect-option ::=
  'MAX_QUERIES_PER_HOUR' integer-literal
  'MAX_UPDATES_PER_HOUR' integer-literal
  'MAX_CONNECTIONS_PER_HOUR' integer-literal
  'MAX_USER_CONNECTIONS' integer-literal

account-option ::=
  'ACCOUNT' ('LOCK' | 'UNLOCK')
  'PASSWORD' 'EXPIRE' ('INTERVAL' integer-literal 'DAY' | 'NEVER' | 'DEFAULT')?
  'PASSWORD' 'HISTORY' (integer-literal | 'DEFAULT')
  'PASSWORD' 'REUSE' 'INTERVAL' (integer-literal 'DAY' | 'DEFAULT')
  'PASSWORD' 'REQUIRE' 'CURRENT' ('DEFAULT' | 'OPTIONAL')?
  'FAILED_LOGIN_ATTEMPTS' integer-literal
  'PASSWORD_LOCK_TIME' (integer-literal | 'UNBOUNDED')

user-attribute ::=
  'ATTRIBUTE' string-literal
  'COMMENT' string-literal

grant-options ::=
  'WITH' 'GRANT' 'OPTION'

grant-as ::=
  'AS' 'USER' with-roles?

with-roles ::=
  'WITH' 'ROLE' (role-list | 'ALL' except-role-list? | 'NONE' | 'DEFAULT')

except-role-list ::=
  'EXCEPT' role-list

grant-target-list ::=
  user-list

acl-type ::= (one of)
  TABLE FUNCTION PROCEDURE

grant-identifier ::=
  '*' ('.' '*')?
  identifier ('.' '*')?
  table-ref

role-or-privilege-list ::=
  role-or-privilege (',' role-or-privilege)*

role-or-privilege ::=
  role
  privilege-name ('(' identifier-list ')')?

privilege-name ::= (one of)
  SELECT INSERT UPDATE REFERENCES DELETE USAGE INDEX DROP EXECUTE
  RELOAD SHUTDOWN PROCESS FILE PROXY SUPER EVENT TRIGGER
  SHOW DATABASES SHOW VIEW LOCK TABLES ALTER ROUTINE
  CREATE CREATE TEMPORARY TABLES CREATE ROUTINE CREATE TABLESPACE
  CREATE USER CREATE VIEW CREATE ROLE DROP ROLE GRANT OPTION
  REPLICATION CLIENT REPLICATION SLAVE REPLICATION REPLICA

role-list ::=
  role (',' role)*

role ::=
  identifier ('@' (identifier | string-literal))?

user ::=
  identifier ('@' (identifier | string-literal))?
  'CURRENT_USER' ('(' ')')?

user-registration ::=
  user-attribute?
```

## Replication

```
replication-statement ::=
  'PURGE' ('BINARY' | 'MASTER') 'LOGS'
      ('TO' string-literal | 'BEFORE' expression)
  'CHANGE' ('MASTER' | 'REPLICATION' 'SOURCE') 'TO'
      source-definition (',' source-definition)* channel-clause?
  'RESET' reset-target (',' reset-target)*
  'RESET' 'PERSIST' identifier?
  'START' ('SLAVE' | 'REPLICA') replica-thread-options?
      ('UNTIL' replica-until)? user-option? password-option?
      default-auth-option? plugin-dir-option? channel-clause?
  'STOP' ('SLAVE' | 'REPLICA') replica-thread-options? channel-clause?
  'CHANGE' 'REPLICATION' 'FILTER' filter-definition (',' filter-definition)*
      channel-clause?
  'LOAD' ('DATA' | 'TABLE' table-ref) 'FROM' 'MASTER'
  ('START' group-replication-start-options? | 'STOP') 'GROUP_REPLICATION'

reset-target ::=
  'MASTER' source-reset-options?
  'BINARY' 'LOGS' 'AND' 'GTIDS' source-reset-options?
  ('SLAVE' | 'REPLICA') 'ALL'? channel-clause?

source-reset-options ::=
  'TO' integer-literal

source-definition ::=
  source-key '=' source-value

source-key ::= (one of)
  MASTER_HOST SOURCE_HOST NETWORK_NAMESPACE MASTER_BIND SOURCE_BIND
  MASTER_USER SOURCE_USER MASTER_PASSWORD SOURCE_PASSWORD
  MASTER_PORT SOURCE_PORT MASTER_CONNECT_RETRY SOURCE_CONNECT_RETRY
  MASTER_RETRY_COUNT SOURCE_RETRY_COUNT MASTER_DELAY SOURCE_DELAY
  MASTER_SSL SOURCE_SSL MASTER_SSL_CA SOURCE_SSL_CA
  MASTER_SSL_CAPATH SOURCE_SSL_CAPATH MASTER_TLS_VERSION SOURCE_TLS_VERSION
  MASTER_SSL_CERT SOURCE_SSL_CERT MASTER_TLS_CIPHERSUITES
  SOURCE_TLS_CIPHERSUITES MASTER_SSL_CIPHER SOURCE_SSL_CIPHER
  MASTER_SSL_KEY SOURCE_SSL_KEY MASTER_SSL_VERIFY_SERVER_CERT
  SOURCE_SSL_VERIFY_SERVER_CERT MASTER_SSL_CRL SOURCE_SSL_CRL
  MASTER_SSL_CRLPATH SOURCE_SSL_CRLPATH MASTER_PUBLIC_KEY_PATH
  SOURCE_PUBLIC_KEY_PATH GET_MASTER_PUBLIC_KEY GET_SOURCE_PUBLIC_KEY
  MASTER_HEARTBEAT_PERIOD SOURCE_HEARTBEAT_PERIOD
  MASTER_COMPRESSION_ALGORITHM SOURCE_COMPRESSION_ALGORITHM
  MASTER_ZSTD_COMPRESSION_LEVEL SOURCE_ZSTD_COMPRESSION_LEVEL
  MASTER_AUTO_POSITION SOURCE_AUTO_POSITION PRIVILEGE_CHECKS_USER
  REQUIRE_ROW_FORMAT REQUIRE_TABLE_PRIMARY_KEY_CHECK
  SOURCE_CONNECTION_AUTO_FAILOVER ASSIGN_GTIDS_TO_ANONYMOUS_TRANSACTIONS
  GTID_ONLY MASTER_LOG_FILE SOURCE_LOG_FILE MASTER_LOG_POS SOURCE_LOG_POS
  RELAY_LOG_FILE RELAY_LOG_POS IGNORE_SERVER_IDS

source-value ::=
  string-literal
  integer-literal
  'NULL'
  '(' (integer-literal (',' integer-literal)*)? ')'

replica-thread-options ::=
  replica-thread-option (',' replica-thread-option)*

replica-thread-option ::= (one of)
  SQL_THREAD RELAY_THREAD

replica-until ::=
  source-definition (',' source-definition)*
  ('SQL_BEFORE_GTIDS' | 'SQL_AFTER_GTIDS') '=' string-literal
  'SQL_AFTER_MTS_GAPS'

user-option ::=
  'USER' '=' string-literal

password-option ::=
  'PASSWORD' '=' string-literal

default-auth-option ::=
  'DEFAULT_AUTH' '=' string-literal

plugin-dir-option ::=
  'PLUGIN_DIR' '=' string-literal

filter-definition ::=
  filter-key '=' '(' filter-items? ')'

filter-key ::= (one of)
  REPLICATE_DO_DB REPLICATE_IGNORE_DB REPLICATE_DO_TABLE
  REPLICATE_IGNORE_TABLE REPLICATE_WILD_DO_TABLE
  REPLICATE_WILD_IGNORE_TABLE REPLICATE_REWRITE_DB

filter-items ::=
  filter-item (',' filter-item)*

filter-item ::=
  identifier
  table-ref
  string-literal
  '(' identifier ',' identifier ')'

group-replication-start-options ::=
  group-replication-start-option (',' group-replication-start-option)*

group-replication-start-option ::=
  'USER' '=' string-literal
  'PASSWORD' '=' string-literal
  'DEFAULT_AUTH' '=' string-literal
```

## Administrative Statements

```
table-administration-statement ::=
  'ANALYZE' no-write-to-binlog? 'TABLE' table-ref-list histogram-clause?
  'CHECK' 'TABLE' table-ref-list check-option*
  'CHECKSUM' 'TABLE' table-ref-list ('QUICK' | 'EXTENDED')?
  'OPTIMIZE' no-write-to-binlog? ('TABLE' | 'TABLES') table-ref-list
  'REPAIR' no-write-to-binlog? 'TABLE' table-ref-list repair-option*

histogram-clause ::=
  'UPDATE' 'HISTOGRAM' 'ON' identifier-list histogram-update-parameter?
  'DROP' 'HISTOGRAM' 'ON' identifier-list

histogram-update-parameter ::=
  ('WITH' integer-literal 'BUCKETS')? ('AUTO' | 'MANUAL')? 'UPDATE'?
  'USING' 'DATA' string-literal

check-option ::= (one of)
  FOR UPGRADE QUICK FAST MEDIUM EXTENDED CHANGED

repair-option ::= (one of)
  QUICK EXTENDED USE_FRM

administrative-statement ::=
  'BINLOG' string-literal
  'CACHE' 'INDEX' cache-assignment (',' cache-assignment)* 'IN'
      (identifier | 'DEFAULT')
  'FLUSH' no-write-to-binlog? (flush-tables | flush-option (',' flush-option)*)
  'KILL' ('CONNECTION' | 'QUERY')? expression
  'LOAD' 'INDEX' 'INTO' 'CACHE' preload-tail
  'SHUTDOWN'

cache-assignment ::=
  table-ref admin-partition? cache-key-list?

admin-partition ::=
  'PARTITION' '(' ('ALL' | identifier-list) ')'

cache-key-list ::=
  ('KEY' | 'INDEX') '(' cache-key-name-list? ')'

cache-key-name-list ::=
  cache-key-name (',' cache-key-name)*

cache-key-name ::=
  identifier
  'PRIMARY'

flush-option ::=
  'HOSTS'
  'PRIVILEGES'
  'STATUS'
  'USER_RESOURCES'
  ('BINARY' | 'ENGINE' | 'ERROR' | 'GENERAL' | 'SLOW')? 'LOGS'
  'RELAY' 'LOGS' channel-clause?
  'OPTIMIZER_COSTS'

flush-tables ::=
  ('TABLES' | 'TABLE') ('WITH' 'READ' 'LOCK'
      | identifier-list flush-tables-option?)?

flush-tables-option ::=
  'FOR' 'EXPORT'
  'WITH' 'READ' 'LOCK'

preload-tail ::=
  cache-assignment ('IGNORE' 'LEAVES')?
  preload-entry (',' preload-entry)*

preload-entry ::=
  table-ref cache-key-list? ('IGNORE' 'LEAVES')?

install-statement ::=
  'INSTALL' 'PLUGIN' identifier 'SONAME' string-literal
  'INSTALL' 'COMPONENT' string-literal (',' string-literal)*
      install-set-clause?

install-set-clause ::=
  'SET' install-set-value (',' install-set-value)*

install-set-value ::=
  ('GLOBAL' | 'PERSIST')? identifier '=' install-set-rvalue

install-set-rvalue ::=
  expression
  'ON'

uninstall-statement ::=
  'UNINSTALL' 'PLUGIN' identifier
  'UNINSTALL' 'COMPONENT' identifier (',' identifier)*

clone-statement ::=
  'CLONE' 'LOCAL' 'DATA' 'DIRECTORY' '='? string-literal
  'CLONE' 'INSTANCE' 'FROM' user ':' integer-literal 'IDENTIFIED' 'BY'
      string-literal clone-data-directory? clone-ssl-clause?

clone-data-directory ::=
  'DATA' 'DIRECTORY' '='? string-literal

clone-ssl-clause ::=
  'REQUIRE' 'NO'? 'SSL'

resource-group-statement ::=
  create-resource-group-statement
  alter-resource-group-statement
  set-resource-group-statement
  drop-resource-group-statement

create-resource-group-statement ::=
  'CREATE' 'RESOURCE' 'GROUP' identifier 'TYPE' '='? ('USER' | 'SYSTEM')
      resource-group-vcpu-list? resource-group-priority?
      resource-group-enable-disable?

alter-resource-group-statement ::=
  'ALTER' 'RESOURCE' 'GROUP' identifier resource-group-vcpu-list?
      resource-group-priority? resource-group-enable-disable? 'FORCE'?

set-resource-group-statement ::=
  'SET' 'RESOURCE' 'GROUP' identifier ('FOR' integer-literal
      (',' integer-literal)*)?

drop-resource-group-statement ::=
  'DROP' 'RESOURCE' 'GROUP' identifier 'FORCE'?

resource-group-vcpu-list ::=
  'VCPU' '='? vcpu-range (','? vcpu-range)*

vcpu-range ::=
  integer-literal ('-' integer-literal)?

resource-group-priority ::=
  'THREAD_PRIORITY' '='? integer-literal

resource-group-enable-disable ::=
  'ENABLE'
  'DISABLE'

no-write-to-binlog ::= (one of)
  LOCAL NO_WRITE_TO_BINLOG
```

## Diagnostics and Signals

```
get-diagnostics-statement ::=
  'GET' ('CURRENT' | 'STACKED')? 'DIAGNOSTICS'
      (statement-information-item (',' statement-information-item)*
       | 'CONDITION' signal-allowed-expression
         condition-information-item (',' condition-information-item)*)

statement-information-item ::=
  (user-variable | identifier) '=' ('NUMBER' | 'ROW_COUNT')

condition-information-item ::=
  (user-variable | identifier) '='
      (signal-information-item-name | 'RETURNED_SQLSTATE')

signal-statement ::=
  'SIGNAL' (identifier | sqlstate-literal)
      ('SET' signal-information-item (',' signal-information-item)*)?

resignal-statement ::=
  'RESIGNAL' (identifier | sqlstate-literal)?
      ('SET' signal-information-item (',' signal-information-item)*)?

signal-information-item ::=
  signal-information-item-name '=' signal-allowed-expression

signal-information-item-name ::= (one of)
  CLASS_ORIGIN SUBCLASS_ORIGIN CONSTRAINT_CATALOG CONSTRAINT_SCHEMA
  CONSTRAINT_NAME CATALOG_NAME SCHEMA_NAME TABLE_NAME COLUMN_NAME
  CURSOR_NAME MESSAGE_TEXT MYSQL_ERRNO

signal-allowed-expression ::=
  literal
  user-variable
  identifier
```

## Expressions

The expression grammar encodes operator precedence by layering nonterminals
from lowest precedence (loosest binding) at the top to highest at the bottom.

```
expression ::=
  or-expression

expression-list ::=
  expression (',' expression)*

or-expression ::=
  xor-expression
  or-expression ('OR' | '||') xor-expression

xor-expression ::=
  and-expression
  xor-expression 'XOR' and-expression

and-expression ::=
  not-expression
  and-expression ('AND' | '&&') not-expression

not-expression ::=
  'NOT' not-expression
  truth-expression

truth-expression ::=
  boolean-primary ('IS' 'NOT'? ('TRUE' | 'FALSE' | 'UNKNOWN'))?
```

`||` is logical OR by default; under `PIPES_AS_CONCAT` it is instead string
concatenation and appears as a `simple-expression` operator (see below).
Under `HIGH_NOT_PRECEDENCE`, `NOT` parses at the precedence of `!` instead of
its position here, so `NOT a BETWEEN b AND c` parses as
`(NOT a) BETWEEN b AND c`.

### Comparison

```
boolean-primary ::=
  predicate
  boolean-primary 'IS' 'NOT'? 'NULL'
  boolean-primary comparison-operator predicate
  boolean-primary comparison-operator ('ALL' | 'ANY' | 'SOME') subquery

comparison-operator ::= (one of)
  = <=> <> != < <= > >=
```

### Predicates

```
predicate ::=
  bit-expression
  predicate 'NOT'? 'IN' subquery
  predicate 'NOT'? 'IN' '(' expression-list ')'
  predicate 'NOT'? 'BETWEEN' predicate 'AND' predicate
  predicate 'NOT'? 'LIKE' predicate ('ESCAPE' predicate)?
  predicate 'NOT'? ('REGEXP' | 'RLIKE') predicate
  predicate 'SOUNDS' 'LIKE' predicate
```

These operators share one precedence level and do not associate; chains
like `a LIKE b LIKE c` resolve by operator precedence, not left-to-right
nesting. `RLIKE` is a lexer-level synonym for `REGEXP`.

### Arithmetic and Bit Operators

```
bit-expression ::=
  bit-or-expression

bit-or-expression ::=
  bit-and-expression
  bit-or-expression '|' bit-and-expression

bit-and-expression ::=
  shift-expression
  bit-and-expression '&' shift-expression

shift-expression ::=
  additive-expression
  shift-expression ('<<' | '>>') additive-expression

additive-expression ::=
  multiplicative-expression
  additive-expression ('+' | '-') multiplicative-expression
  additive-expression ('+' | '-') 'INTERVAL' expression interval-unit

multiplicative-expression ::=
  xor-bit-expression
  multiplicative-expression ('*' | '/' | '%' | 'DIV' | 'MOD') xor-bit-expression

xor-bit-expression ::=
  simple-expression
  xor-bit-expression '^' simple-expression
```

### Simple Expressions

```
simple-expression ::=
  literal
  column-ref
  function-call
  aggregate-function-call
  window-function-call
  placeholder
  user-variable
  user-variable ':=' expression
  system-variable
  ('+' | '-' | '~') simple-expression
  '!' simple-expression
  'BINARY' simple-expression
  simple-expression '||' simple-expression
  simple-expression 'COLLATE' collation-name
  'ROW'? '(' expression-list ')'
  'EXISTS'? subquery
  'MATCH' match-columns 'AGAINST' '(' bit-expression fulltext-option? ')'
  case-expression
  cast-expression
  'DEFAULT' '(' column-ref ')'
  'VALUES' '(' column-ref ')'
  'INTERVAL' expression interval-unit '+' expression
```

The `simple-expression '||' simple-expression` alternative exists only under
`PIPES_AS_CONCAT`. A parenthesized `expression-list` with more than one
element, or any list prefixed with `ROW`, is a row constructor; with exactly
one element and no `ROW` it is ordinary grouping.

```
match-columns ::=
  identifier-list
  '(' identifier-list ')'

fulltext-option ::=
  'IN' 'NATURAL' 'LANGUAGE' 'MODE' ('WITH' 'QUERY' 'EXPANSION')?
  'IN' 'BOOLEAN' 'MODE'
  'WITH' 'QUERY' 'EXPANSION'
```

### CASE, CAST, and CONVERT

```
case-expression ::=
  'CASE' expression? ('WHEN' expression 'THEN' expression)+
      ('ELSE' expression)? 'END'

cast-expression ::=
  'CAST' '(' expression 'AS' cast-type ')'
  'CONVERT' '(' expression ',' cast-type ')'
  'CONVERT' '(' expression 'USING' charset-name ')'

cast-type ::=
  'BINARY' field-length?
  'CHAR' field-length? (('CHARACTER' 'SET' | 'CHARSET') charset-name | 'BINARY')?
  'NCHAR' field-length?
  'VARCHAR' field-length
  'INT'
  'SIGNED' 'INT'?
  'UNSIGNED' 'INT'?
  'DATE'
  'TIME' field-length?
  'DATETIME' field-length?
  ('DECIMAL' | 'DEC') float-options?
  'FLOAT'
  'DOUBLE' precision?
  'INTERVAL' 'DAY_SECOND' field-length
  identifier
```

The trailing `identifier` alternative covers plugin and user-defined types
(`INET6`, `UUID`, ...).

### Function Calls

```
function-call ::=
  function-name '(' expression-list? ')'
  table-ref '(' expression-list? ')'
  special-function-call
  bare-function

function-name ::=
  identifier
```

A call whose name is qualified (`schema.func(...)`) invokes a stored function.
The functions below have argument syntax that ordinary call syntax cannot
express:

```
special-function-call ::=
  'TRIM' '(' expression ')'
  'TRIM' '(' ('BOTH' | 'LEADING' | 'TRAILING') expression? 'FROM' expression ')'
  'TRIM' '(' expression 'FROM' expression ')'
  ('SUBSTRING' | 'SUBSTR' | 'MID') '(' expression ',' expression
      (',' expression)? ')'
  ('SUBSTRING' | 'SUBSTR') '(' expression 'FROM' expression
      ('FOR' expression)? ')'
  'POSITION' '(' bit-expression 'IN' expression ')'
  'EXTRACT' '(' interval-unit 'FROM' expression ')'
  ('DATE_ADD' | 'DATE_SUB' | 'ADDDATE' | 'SUBDATE')
      '(' expression ',' 'INTERVAL' expression interval-unit ')'
  ('TIMESTAMPADD' | 'TIMESTAMPDIFF') '(' interval-unit ',' expression ','
      expression ')'
  'GET_FORMAT' '(' ('DATE' | 'TIME' | 'DATETIME' | 'TIMESTAMP') ','
      expression ')'
  'CHAR' '(' expression-list ('USING' charset-name)? ')'
  'WEIGHT_STRING' '(' expression ('AS' ('CHAR' | 'BINARY') field-length)? ')'

bare-function ::= (one of)
  CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP CURRENT_USER CURRENT_ROLE
  LOCALTIME LOCALTIMESTAMP UTC_DATE UTC_TIME UTC_TIMESTAMP
  NOW SYSDATE USER SESSION_USER SYSTEM_USER DATABASE SCHEMA
```

A `bare-function` keyword may be used with or without a trailing empty
parameter list (`CURRENT_TIMESTAMP` or `CURRENT_TIMESTAMP()`); the temporal
ones additionally accept a fractional-seconds precision argument
(`NOW(6)`).

```
interval-unit ::= (one of)
  MICROSECOND SECOND MINUTE HOUR DAY WEEK MONTH QUARTER YEAR
  SECOND_MICROSECOND MINUTE_MICROSECOND MINUTE_SECOND
  HOUR_MICROSECOND HOUR_SECOND HOUR_MINUTE
  DAY_MICROSECOND DAY_SECOND DAY_MINUTE DAY_HOUR YEAR_MONTH
```

### Aggregate Functions

```
aggregate-function-call ::=
  'COUNT' '(' 'ALL'? '*' ')' over-clause?
  'COUNT' '(' 'ALL'? expression ')' over-clause?
  'COUNT' '(' 'DISTINCT' expression-list ')' over-clause?
  ('AVG' | 'SUM' | 'MIN' | 'MAX') '(' ('DISTINCT' | 'ALL')? expression ')'
      over-clause?
  ('BIT_AND' | 'BIT_OR' | 'BIT_XOR' | 'STD' | 'STDDEV' | 'STDDEV_POP'
   | 'STDDEV_SAMP' | 'VARIANCE' | 'VAR_POP' | 'VAR_SAMP')
      '(' 'ALL'? expression ')' over-clause?
  'GROUP_CONCAT' '(' 'DISTINCT'? expression-list order-by-clause?
      ('SEPARATOR' string-literal)? group-limit-clause? ')' over-clause?
  'JSON_ARRAYAGG' '(' 'DISTINCT'? expression order-by-clause?
      group-limit-clause? ')' over-clause?
  'JSON_OBJECTAGG' '(' expression ',' expression ')' over-clause?

group-limit-clause ::=
  'LIMIT' limit-option (',' limit-option)?
  'LIMIT' limit-option 'OFFSET' limit-option
```

### Window Functions

```
window-function-call ::=
  ('ROW_NUMBER' | 'RANK' | 'DENSE_RANK' | 'CUME_DIST' | 'PERCENT_RANK')
      '(' ')' over-clause
  'NTILE' '(' expression ')' over-clause
  ('LEAD' | 'LAG') '(' expression (',' expression)? ')' over-clause
  ('FIRST_VALUE' | 'LAST_VALUE') '(' expression ')' over-clause
  'NTH_VALUE' '(' expression ',' expression ')' over-clause

over-clause ::=
  'OVER' identifier
  'OVER' window-specification
```

An `over-clause` after an aggregate function makes it a window aggregate;
`DISTINCT` aggregates and `GROUP_CONCAT` with `ORDER BY`/`SEPARATOR` cannot be
windowed.

### Operator Precedence Summary

For reference, the complete precedence ordering this expression grammar
encodes, from highest to lowest (operators on one line share a level):

```text
INTERVAL
BINARY, COLLATE
!
- (unary minus), ~ (unary bit inversion)
^
*, /, DIV, %, MOD
-, +
<<, >>
&
|
= (comparison), <=>, >=, >, <=, <, <>, !=, IS, LIKE, REGEXP, IN
BETWEEN, CASE, WHEN, THEN, ELSE
NOT
AND, &&
XOR
OR, ||
= (assignment), :=
```

`=` is comparison in expression context and assignment in `SET` clauses and
`SET` statements; `:=` is assignment in any context. Mode sensitivity: under
`PIPES_AS_CONCAT`, `||` is concatenation with precedence above `^`; under
`HIGH_NOT_PRECEDENCE`, `NOT` moves up to the precedence of `!`.
