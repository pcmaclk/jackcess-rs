# jackcess-rs

[![CI](https://github.com/pcmaclk/jackcess-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/pcmaclk/jackcess-rs/actions/workflows/ci.yml)

[English](README.en.md) | [简体中文](README.md)

A Rust port of [Jackcess](https://jackcess.sourceforge.io/) 5.0.4, the pure-Java
library for reading and writing Microsoft Access databases (Jet / ACE `.mdb` and
`.accdb` files).

The port is written to be faithful enough that files written by the Rust code
are **byte-for-byte identical** to files written by the original Java library
for the same operations.  An automated harness runs the Java reference
generator and the Rust generator against the same scenario and compares the
resulting files.

## Building from a clone

```
cargo build
cargo test --lib        # 327 tests; no test fixtures needed
```

This repository is the **minimal buildable crate**: it contains only the source
and the 14 files (3.6 MB) that the library embeds at compile time (the
`empty*.mdb/accdb` templates, the index collation tables and the generated
code-page table).  The verification harnesses, the Java reference tools and the
Java-vs-Rust test fixtures are development-only and are **not** part of this
repository.

The fixture-dependent tests are gated behind the `fixtures` feature, so they are
compiled and run only in a development tree that has the test data:

```
cargo test --lib --features fixtures
```

Development harnesses additionally use the Java reference libraries and the
real Access/encryption corpora.  `fetch-external.ps1` downloads those (it does
not restore the development test fixtures):

```
powershell -ExecutionPolicy Bypass -File fetch-external.ps1
# behind a local proxy:
powershell -ExecutionPolicy Bypass -File fetch-external.ps1 -Proxy http://127.0.0.1:10808
```

## Feature flags

The crate has two opt-out features; both are on by default, so a plain
`cargo build` gives the full, byte-identical library:

| feature | what it adds | approximate binary cost |
| --- | --- | --- |
| `create` | the embedded `empty*.mdb/accdb` templates and `Database::create*` | ~0.1 MB (compressed templates) |
| `crypto` | page encoding, Jet/Office passwords, jackcess-encrypt-compatible handlers, MSISAM (`.mny`) decoding, and the crypto dependencies | ~1.9 MB in `mdbgen.exe` |

A read-only application that only opens unencoded Jet/ACE databases can turn
both off:

```toml
[dependencies]
jackcess = { version = "5.0.4-rs.1", default-features = false }
```

With `--no-default-features`, creating a database returns
`Error::Unsupported` unless `create` is enabled, and opening an encoded or
MSISAM file returns `Unsupported` unless `crypto` is enabled; plain
unencrypted databases open normally.  Measured with `lto = "fat"`,
`codegen-units = 1`, `panic = "abort"` and `strip = "symbols"`:

| build | size |
| --- | --- |
| minimal consumer (open + table names), default features | 3.63 MB |
| minimal consumer, `default-features = false` | 3.37 MB |
| `mdbgen.exe`, default features | 5.62 MB |
| `mdbgen.exe`, `--no-default-features` | 5.30 MB |

`query`, `export`, `ole`, `complex`, `deflate` and `expr` are not separate
features: the table read/write paths use complex columns, OLE blobs, calculated
columns and expression validation directly, and query/export are small next to
those core dependencies.  They stay compiled in every configuration.

## Versioning

The version has two independent parts:

```
<jackcess version>-rs.<port revision>
       5.0.4        -rs.1
```

* **`<jackcess version>`** is the upstream Jackcess release whose file format,
  API and byte-level behaviour the port targets.  Moving to a newer upstream
  (`5.0.5`, `5.1.0`, …) starts a new `<jackcess version>` segment.
* **`<port revision>`** counts Rust-side releases of that baseline: a bug fix,
  an API-compatibility change, a dependency/security update or any other change
  to shipped code increments it (`-rs.1`, `-rs.2`, `-rs.3`, …).  It resets to
  `-rs.1` when the upstream segment changes.

Rules:

1. **First release:** `5.0.4-rs.1`, git tag `v5.0.4-rs.1`.
2. **Port bugfix / API change:** `5.0.4-rs.2`, `5.0.4-rs.3`, …; tag
   `v5.0.4-rs.2`, …  The upstream segment stays `5.0.4` so consumers can see
   that the file-format/API baseline did not change.
3. **Upstream port bump:** `5.0.5-rs.1`, tag `v5.0.5-rs.1`; the revision counter
   restarts.
4. **Never reuse a version or tag.**  crates.io versions are immutable; every
   published change gets a new `-rs.N`, and the Git tag always equals the crate
   version prefixed with `v`.
5. **SemVer semantics:** `-rs.N` is a pre-release identifier, so a plain
   `jackcess = "5.0.4"` requirement does **not** select it.  Consumers should
   pin the port explicitly, e.g. `jackcess = "=5.0.4-rs.2"`, or use a
   `5.0.4-rs.2` requirement to allow later `-rs.N` revisions of the same
   upstream baseline.  There is intentionally no plain `5.0.4` release, so the
   port revision is always visible.
6. **Release notes** list the upstream baseline, the port revision, and the
   changes since the previous `-rs.N`; `CHANGELOG.md` is the source of truth.

If a stable, non-pre-release line is ever wanted, that is a deliberate policy
change: publish a new upstream-segment version (e.g. `5.0.5`) rather than
silently dropping the suffix on an existing `5.0.4` release.

## Verified byte-identical output

`tools\verify.ps1` (V2003 / Jet 4 unless stated otherwise):

| scenario | what it does | result |
| --- | --- | --- |
| `empty` | create a new database from the embedded template | identical (204,800 bytes) |
| `t1` | create `Test(ID autonumber, Name text(50), Value double)` with a primary-key index | identical (217,088) |
| `rows` | `t1` + 5 rows | identical (225,280) |
| `rows50` | `t1` + 50 rows (forces index page splits, nested index pages, several data pages) | identical (225,280) |
| `types` | create a table with 14 columns covering the scalar types | identical (212,992) |
| `guidx` | GUID auto-number primary key plus a GUID column, with explicit braced/unbraced/upper/lower-case values, a raw-byte GUID and a null on a plain GUID column | identical (V2003 225,280; V2007 348,160; V2010 364,544; V2016 385,024) |
| `bigindex` | 3000 rows inserted one at a time with an autonumber primary key, a unique TEXT index, a descending compound index and a non-unique variable-text index (deep B-trees, many splits, prefix compression) | identical (V2003 983,040; V2007 1,105,920; V2010 1,212,416; V2016 1,232,896) |
| `indexchurn` | the `bigindex` table after 300 indexed key changes, ~215 non-key updates, 500 deletes and 500 re-inserts | identical (827,392 / 950,272 / 1,019,904 / 1,040,384) |
| `bigindex2` | 2000 rows with a 3-column compound index, two descending indexes, all-null keys and 120-char shared text prefixes | identical (1,875,968 / 1,998,848 / 2,514,944 / 2,535,424) |
| `types_rows` | `types` + 4 rows (unicode text, money, numeric, guid, binary, dates, memo, booleans, an all-null row) | identical (221,184) |
| `twoindexes` | table with a primary key, a unique text index and a descending multi-column index + 20 rows | identical (233,472) |
| `twotables` | two tables created in sequence (distinct catalog timestamps) | identical (229,376) |
| `append` | open the same jackcess-created database in Java and in Rust, append the same 3 rows (including long MEMO values written to long-value pages), compare | identical (229,376) |
| `unicode` | table with a primary key on a TEXT column, 9 rows with accented/CJK/emoji keys (exercises text collation end to end) | identical (225,280) |
| `bigmemo` | table with a MEMO column, rows of 10…12,000 characters (long values stored on other pages and across chained pages) | identical (274,432) |
| `delete` | table with a MEMO column + 150 rows, delete 6 rows (one holding a long MEMO value), then add 3 more rows; all inside one open of the file | identical (417,792) |
| `update` | table with text/double/long/boolean/date columns + 150 rows, update 7 rows in place and through relocations (shorter/longer text, changed numeric/date/boolean/null values) | identical (233,472) |
| `memoupdate` | MEMO column + 60 rows, update values inline→other page, other/multi-page→inline, unchanged, null, and across several long-value pages, then add 2 rows | identical (294,912) |
| `partialupdate` | `Test(ID autonumber pk, Name text(255) unique, Memo memo, N long)` with 5 rows (short inline memo, multi-page memos) + 60 filler rows; then partial row updates which pass `Column.KEEP_VALUE`/`Value::Keep` for `ID`/`Memo` and change `Name`/`N` (an all-keep update, an update through `updateValue`, one through `asUpdateRow`/`update_row_from_map`, a `Name` growth which relocates the row to another data page while the multi-page memo keeps its pages, and an update of the memo itself which writes new long-value pages), then 2 more rows | identical (266,240) |
| `diag200` | the `partialupdate` table shape with 200 `filler%03d` rows added one at a time (the per-row `addRowFromMap` path, which writes/splits the index pages after every row and crosses the unique text index page's prefix-compression/split thresholds) | identical (V2003 245,760; V2007 368,640; V2016 405,504) |
| `keeplong` | a multi-page MEMO kept through seven updates of the first row's other columns and two of the second row's (never allocating new long-value pages), including `updateValue` and `asUpdateRow`, then a row added | identical (249,856) |
| `rowops` | a table with two indexes (primary key + unique text index) and an index-less table: update rows twice (a relocated row in place, then grow again), delete a relocated row, delete from the index-less table, then add rows | identical (249,856) |
| `addcol` | `T` + 5 rows, then add a LONG column (`ColumnBuilder.addToTable`) and 3 more rows (two with values, one null) | identical (225,280) |
| `addindex` | table with no index + 30 rows, then add a unique text index, a second unique index sharing the same backing index data, a descending numeric index and a primary key (`IndexBuilder.addToTable`), then 2 more rows | identical (233,472) |
| `addboth` | add a LONG column, index it, add a MEMO column (with its usage maps) and 4 rows, all inside one open of the file | identical (233,472) |
| `addmany` | add 120 LONG columns so the table definition grows onto a second page (definition split and page re-use in the update path) | identical (229,376) |
| `relate` | `P(ID autonumber pk, Name text(40))` and `C(ID autonumber pk, P_ID long, Note text(50))` with rows, then a relationship `P.ID → C.P_ID` with referential integrity (creates the `.rB` unique foreign key index on `P`, reusing the primary key index data, and the `PC` foreign key index on `C` with new index data), then more rows | identical (258,048) |
| `samerel` | two relationships in one open: `P.ID → C.P_ID` with cascade updates and deletes, and the one-to-one `N.ID → D.N_ID` (both sides uniquely indexed, so the one-to-one flag is derived and both foreign key indexes reuse existing index data) | identical (303,104) |
| `selfrel` | a table related to itself (`E.ID → E.Parent_ID`): both supporting indexes live on `E`, one index number apart, and reference each other | identical (237,568) |
| `dbprops` | database properties (`Title` TEXT, `CustomNum` LONG), a summary property (`Title`) and a user defined property (`MyBool` BOOLEAN) applied while the database is created (`DatabaseBuilder.putDatabaseProperty` and friends; the Java builder collects each group in a `HashMap`, so the blob order is reproduced as well) | identical (204,800) |
| `tableprops` | a table with a table `Description`, a `RowSource` MEMO property large enough to span several long-value pages, column properties (`Description`/`Format` on `Name`, `DefaultValue` on `Value`, `Description` on `Note`), 3 rows, then a changed table property, a removed column property and a changed column property, saved again | identical (258,048) |
| `propupdate` | a plain table with rows, closed and reopened to set a table and a column `Description` (saved against the existing catalog row), then reopened again to change/remove them and save | identical (225,280) |
| `summaryprops` | changed/removed properties of the database, summary and user defined groups of an existing database (`MSysDb`, `SummaryInfo` and `UserDefined` rows), inside one open | identical (204,800) |
| `linked` | `P`/`C` plus a relationship (which takes the first synthetic object id), then two linked tables (`Database.createLinkedTable`: type 6 catalog rows with `Database`/`ForeignName`, ACEs and synthetic ids counted up after the relationship's) and a normal table | identical (249,856) |
| `reclaim` | an indexed table with 905 rows (the primary-key index grows over several leaf pages), 895 rows deleted through the primary key so the empty index pages are deallocated, then `Database.reclaimFreeSpace()` (drops the trailing free page) | identical (258,048) |
| `reuse` | the same table/deletions, then `Database.setReuseFreePages(true)` and 300 more rows, which the allocator places on the deallocated index pages instead of growing the file | identical (262,144) |
| `autonum` | a LONG auto-number primary key with `setAllowAutoNumberInsert(true)`: explicit ids 5/100/3/7 (the lower ones do not move the high water mark), generated 101-104, one deleted, another generated (105), plus an index-less auto-number table with explicit/generated mix | identical (245,760) |
| `fkadd` | `P(ID long pk, Name text(40))` and `C(ID autonumber pk, P_ID long, Note text(50))` with rows, then a relationship with referential integrity; valid rows before and after a rejected insert (a foreign key naming no parent), which leaves the file unchanged and consumes no auto number | identical (258,048) |
| `fkdel` | a cascade-delete relationship (`P.ID → C.P_ID`) and a restricted one (`N.ID → D.N_ID`): deleting a `P` row cascades to its children, deleting an `N` row a `D` row names is rejected, a parent without children and a child row are deleted freely, then more rows | identical (303,104) |
| `fkupd` | a cascade-update relationship (`P.ID → C.P_ID`) and a restricted one (`N.ID → D.N_ID`): a parent key change follows into the children, a child naming a missing parent and a restricted parent key change are rejected (file unchanged), then allowed updates and more rows | identical (303,104) |
| `defaults` | a table whose columns carry `DefaultValue` properties (`LONG` 7, `LONG` 1.5 rounded half even to 2, `DOUBLE` 1.5, quoted and unquoted `TEXT`, `MEMO`, `BOOLEAN` TRUE, `LONG` -1): rows which omit the columns (defaults applied), rows with explicit values (defaults ignored) and rows with explicit nulls (defaults applied on add), then an update which keeps most columns and never applies a default | identical (221,184) |
| `required` | a `Required` `LONG` column and a `TEXT` column with `AllowZeroLength` off: valid rows, then rejected inserts with a null required value and an empty string (the Java messages, no row added, no auto number consumed), then more valid rows | identical (221,184) |
| `validate` | field validation rules (`[Num] >= 0` with `ValidationText`, `Len([Code]) = 3`) and a table row rule (`[Start] <= [End]` with text): valid inserts, rejected inserts/updates with the exact Java messages (file unchanged, no auto number consumed), then more valid rows/updates | identical for V2003/V2007/V2010/V2016 |
| `validateupdate` | valid updates plus rejected field/row-rule updates (including a two-column row violation), then more valid updates and an insert | identical for V2003/V2007/V2010/V2016 |
| `multivalue` (ACE only) | `Lookup` + `Test(ID autonumber, Name, Tags multi-value TEXT with a value list, Codes multi-value TEXT with a `RowSource` over `Lookup`) and a primary key; 3 rows with 0, 1 and 3+2 values, then a value changed, one removed, one added, and a row deleted | identical (V2007 421,888; V2016 454,656) |
| `attachment` (ACE only) | `Test(ID autonumber, Name, Files attachment)` with 4 rows: 0, 1, 2 and 1 attachments (raw `png`/`jpg`/`zip` payloads, one row's attachment larger than a data page), then an attachment renamed and rewritten, one deleted and the row holding one deleted | identical (V2007 397,312; V2016 430,080) |
| `addcomplex` (ACE only) | `Test(ID autonumber, Name)` + 3 rows, then a multi-value column and an attachment column added to the existing table (`ColumnBuilder.addToTable`), values added to the pre-existing rows (which get the new complex ids) and one attachment added | identical (V2007 401,408; V2016 438,272) |
| `versionhistory` (ACE only) | `Test(ID autonumber, Name, Body MEMO with setAppendOnly(true))` + a primary key; one row with an initial memo value, three versions with explicit modified dates, the row updated (a new memo value records another version), the row deleted (jackcess leaves its versions behind) and a second row with one version | identical (V2007 385,024; V2016 421,888) |
| `vhadd` (ACE only) | an append-only MEMO column added to a table which already holds 5 rows (`ColumnBuilder.addToTable`; the existing rows get the next complex ids), a version of every existing row's memo value, then a new row with a body and its version | identical (V2007 385,024; V2016 421,888) |
| `calc` (V2010+ only) | `Test(ID autonumber, A LONG, B DOUBLE, S TEXT, D date)` with calculated columns `C=[A]+1`, `E=[B]*2`, `F=[S] & "x"`, `G=IIf([A]>0,"pos","neg")`, `H=[C]+[E]` (two calculated dependencies), `J="v=" & [A]`, `M=[S] & [S]` (MEMO, one value on a long-value page), `N=[A]*2` (NUMERIC), `K=[D]`, `P=[A]>0` (BOOLEAN result); six rows with nulls, negatives, zero and fractions | identical (V2010 364,544; V2016 385,024; V2019 454,656) |
| `calcupdate` (V2010+ only) | the same table, then a `KEEP_VALUE` update of `A`, a full-row update of `B`, an `asUpdateRow` change of `S`, a null-out update of `A`/`S`, and two more rows (calculated values recomputed each time) | identical (V2010 364,544; V2016 385,024; V2019 454,656) |
| `calcfloat` (V2010+ only) | calculated `MONEY`/`FLOAT`/`DOUBLE` result types (`[A]/4`, `[A]*2`, `[A]/3`, `[A]/8`) over rows `-2…3` plus a null | identical (V2016 380,928) |
| `noexpr` | `setEvaluateExpressions(false)`: a table with `DefaultValue`, `Required`, `AllowZeroLength` columns and rows with nulls/empty strings (defaults not applied, `Required`/`AllowZeroLength` still enforced) | identical (221,184 / 344,064 / 360,448 / 380,928 bytes for V2003/V2007/V2010/V2016) |
| `noexprcalc` (V2010+ only) | the same with a calculated column: when expressions are off Java neither evaluates it nor applies defaults, and an explicitly supplied calculated value is stored as given | identical (V2010 360,448; V2016 380,928) |
| `tzutc` | database time zone `UTC`: date columns written from fixed epoch instants and catalog timestamps | identical for V2003/V2007/V2010/V2016 |
| `tzneg` | database time zone `GMT-05:00`: the same with a negative offset | identical for V2003/V2007/V2010/V2016 |

`tools\verify.ps1 -Scenarios "t1,rows" -Format V2007` and `-Format V2016` are
also byte-identical (ACE files, e.g. `rows` = 348,160 / 385,024 bytes).  The
property persistence scenarios are byte-identical for `-Format V2007`
(`dbprops`/`summaryprops` = 327,680, `tableprops` = 380,928,
`propupdate` = 348,160 bytes) and `-Format V2016` (`dbprops`/`summaryprops` =
364,544, `tableprops` = 417,792, `propupdate` = 385,024 bytes), and spot checks
of all of them with `-Format V2000` are identical too.
The row-update/delete scenarios (`delete,update,memoupdate,partialupdate,keeplong,rowops`)
are byte-identical for `-Format V2007` and `-Format V2016` as well (540,672 /
356,352 / 417,792 / 389,120 / 372,736 / 368,640 bytes and 577,536 / 393,216 /
454,656 / 425,984 / 409,600 / 405,504 bytes).  The
table-mutation scenarios are byte-identical for `-Format V2007`
(`addcol,addindex,addboth,addmany` = 348,160 / 356,352 / 356,352 / 352,256
bytes) and for `-Format V2016` (`addboth` = 393,216 bytes).  The relationship
scenarios are byte-identical for `-Format V2007` too (`relate,samerel,selfrel`
= 376,832 / 421,888 / 356,352 bytes), and spot checks of `relate` with
`-Format V2000` (147,456 bytes) and of `selfrel` with `-Format V2016`
(393,216 bytes) are identical as well.
The linked table, free page and explicit auto-number scenarios are
byte-identical for `-Format V2007` (`linked,reclaim,reuse,autonum` =
368,640 / 380,928 / 385,024 / 364,544 bytes) and for `-Format V2016`
(409,600 / 417,792 / 421,888 / 401,408 bytes).
The foreign key enforcement scenarios are byte-identical for `-Format V2007`
as well (`fkadd,fkupd` = 376,832 / 421,888 bytes).
The column default value and validator scenarios are byte-identical for
`-Format V2007` (`defaults,required` = 344,064 bytes each) and
`-Format V2016` (380,928 bytes each).
The calculated column scenarios are V2010+ only, since jackcess itself rejects
calculated columns for V2003 and V2007; they are byte-identical for
`-Format V2010` (`calc,calcupdate` = 364,544 bytes each), `-Format V2016`
(385,024 bytes each) and `-Format V2019` (454,656 bytes each).

The complex column scenarios (`multivalue`, `attachment`, `addcomplex`,
`versionhistory`, `vhadd`) are ACE-only: Jet 4 has no
`MSysComplexColumns`/`MSysComplexType_*` supporting tables.  They are
byte-identical for `-Format V2007` and `-Format V2016` (see the table above).
Access puts a random GUID in the name of a flat table, so
`tools\verify.ps1` reads the flat table names back from the Java file
(`java-src/ComplexInfo.java --flat-names`) and passes them to the Rust
generator (`--complex-names`); a version history column also has a per-column
type table with a random GUID in its name, which `ComplexInfo --vh-names`
reads back and which is passed in the same list before the flat table name
(the type table is created first).  The timestamps come from `GetDates
--tables`.  Everything else about the file is generated independently by the
two writers.

The complex column scenarios (including `versionhistory` and `vhadd`) are
byte-identical for `-Format V2019` as well (`multivalue` = 524,288,
`attachment` = 503,808, `addcomplex` = 507,904, `versionhistory`/`vhadd` =
491,520 bytes).  (Other `-Format V2019` scenarios were not verified; the
relationship scenarios still differ there, in the `MSysRelationships`
`DateCreate` values.)

`tools\verify-props.ps1` additionally compares parsed property maps:

- database properties of `empty.mdb`, `empty2003.mdb`, `empty2007.accdb`,
  `empty2016.accdb` — identical values (2/10/12/34 properties)
- table properties of `MSysNavPaneGroups` — identical

Three further harnesses cover features which need their own fixtures:

- `tools\verify-attach.ps1` — attachment columns with raw file types
  (png/jpg/gif/zip/cab/docx/xlsx/xlsb/pptx) and deflate-compressed types
  (txt/csv/pdf/html/bin), upper-case extensions, empty and large payloads:
  BYTE-IDENTICAL on V2007 (450,560 bytes) and V2016 (483,328 bytes).
- `tools\verify-link.ps1` — a target database and a linker database with two
  linked tables; the rows are read through the links with
  `LinkResolver.UNRESTRICTED` and copied into a local table.  Both the linker
  and the target file are BYTE-IDENTICAL on V2003, V2007 and V2016.
- `tools\verify-expr.ps1` — the expression engine against Jackcess' own
  calculated-column evaluation: 3280/3280 oracle cases (including custom `Format` patterns and error cases)
  match, covering precedence, null/three-valued logic, integer division and
  `MOD` signs, decimals, dates, `LIKE`, the conversion functions and coercion
  to eight column types.
- `tools\verify-query.ps1` — the query model and SQL rendering against
  Jackcess' own `QueryImpl.toSQLString()`: 107/107 synthetic `MSysQueries`
  cases (SELECT/joins/parameters/crosstab/union/append/update/delete/
  make-table/passthrough/data-definition, plus the error paths).
- `tools\verify-linkwrite.ps1` — a target database and a linker database with a
  linked table; rows are inserted, updated (including an indexed key change) and
  deleted *through the link*, then read back and copied into a local table.
  Both files are BYTE-IDENTICAL on V2003, V2007 and V2016.
- `tools\verify-cursor.ps1` — the public table-scan and index cursor APIs
  (finds, iteration, update/delete of the current row, indexed key changes) on
  the same database in both implementations: identical operation trace and
  BYTE-IDENTICAL databases on V2003/V2007/V2016.
- `tools\verify-cursor-range.ps1` — the cursor ranges, savepoints and
  Iterable/EntryIterable builders (reversed/empty/prefix/null bounds,
  exclusive/inclusive ends, boundary positions, restore-savepoint semantics,
  forward/reverse iterables and column subsets, CursorBuilder index selection by columns/index, non-unique `findRow` semantics) with a 240-step identical
  operation trace and BYTE-IDENTICAL databases on V2003/V2007/V2016.
- `tools\verify-export.ps1` — `ExportUtil`/`ImportUtil`: 15 exported text files
  byte-identical (5 option sets × 3 formats) and 24 imported databases
  BYTE-IDENTICAL (7 import scenarios × 3 formats, plus the 3 source databases),
  covering quoted fields, embedded newlines/delimiters, nulls, CRLF and explicit
  column types.
- `tools\verify-crypt.ps1` — Jet 4 database encoding and database password
  against jackcess-encrypt 5.0.2: encoded, password and password+encoded files
  are BYTE-IDENTICAL to Java; encode/decode and set/remove-password round trips
  return the exact base bytes, rows read through the codec provider match,
  wrong-password rejection matches, and the in-place/path forms agree.
- `tools\verify-crypt-office.ps1` — ACE (`.accdb`) encryption: agile
  `OFFICE_2010`/`OFFICE_2013` read/write, ECMA standard and the older RC4
  providers for reading and removal.  In deterministic mode (fixed salts/key/
  verifier and encoding key, mirroring Java's `AgileEncryptionWriter` API) the
  encrypted files are BYTE-IDENTICAL; in random mode each side decrypts the
  other's file to the exact plain base and both read the same rows, and a wrong
  password is rejected with Java's `Incorrect password provided`.
- `tools\verify-querydb.ps1` — the query model integrated into `Database`
  (reading `MSysObjects` type-5 + `MSysQueries`): 14 queries (SELECT with
  parameters/WHERE/ORDER BY, GROUP BY/HAVING, joins, crosstab, union,
  append/update/delete/make-table, passthrough, hidden and system) produce
  IDENTICAL SQL and metadata dumps in V2003/V2007/V2016; the metadata dump also
  covers table/column/index metadata and crafted linked (type 6) and ODBC
  (type 4) tables, including the `TableDefinition has no data access` refusal.
- `tools\verify-namemap.ps1` — the `NameMap`/`TableNameMap`/`QueryNameMap`
  binary blobs: realistic maps (renames, unicode, deleted/mismatched records),
  the absent-property case and 11 malformed-blob error cases, parsed by both
  implementations: IDENTICAL dumps for V2003/V2007/V2016.
- `tools\verify-exportall.ps1` — `ExportUtil.exportAll` and the streaming /
  overwrite / append forms: 231 identical text files and 51 identical
  manifests (multi-table databases, filename escaping including `R&D #2 (100%)`
  and unicode names, empty databases, hostile names, option sets) for
  V2003/V2007/V2016.
- `tools\verify-realquery.ps1` — **real Access-created files** from the jackcess
  test corpus: `queryV1997/V2000/V2003/V2007/V2010` (9 queries each) and the
  56 queries of `linkedOdbcV2007.accdb` produce IDENTICAL name/type/flags/
  parameters/owner-access/SQL dumps, and that file's real ODBC linked table
  (`LINKED_ODBC`, connect string, 20 columns, 5 indexes, no-data error) matches.
- `tools\verify-jet3.ps1` — Jet 3 / Access 97 **reading** against 9 real V1997
  files plus 18 V2000/V2003 cross-checks: 27/27 canonical dumps (definitions,
  metadata, every row, long values, overflow, indexes/entries) are IDENTICAL.
- `tools\verify-ole.ps1` — `OleBlob`/OLE compound files: the real
  `blobV2007.accdb` (193 lines, 11 values incl. Word/PDF/Excel compound
  storages), 12 wrapped fixtures, 24 raw/truncated/corrupt compound files and
  synthetic V2000/V2003/V2007 databases all match (SHA-256 compared); the v4
  (4096-byte sector) path is validated with generated fixtures, DIFAT and
  malformed cases (11 sections).
- `tools\verify-errorhandler.ps1` — `ErrorHandler`/`DebugErrorHandler` on a
  corrupt TEXT row and a corrupt MEMO row of real files (no-handler failures,
  replacement values, table/database/cursor overrides, throwing handlers, the
  unsupported-column and `TableDefinition has no data access` cases): identical
  callback trace (96 lines).
- `tools\verify-queryprops.ps1` — query property maps and query name maps:
  10 databases / 121 queries, 113 with properties (276 records) and 53 name maps
  (206 records), plus the Java-generated value-class matrix and 4+4 malformed
  cases: identical dumps.
- `tools\verify-customlink.ps1` — `CustomLinkResolver` with a moved target:
  relative/absolute mappings, different-case names, the loader form, missing
  mappings and a temp-DB resolver; identical read dumps and unchanged linker
  files on V2003/V2007/V2010/V2016.
- `tools\verify-jet3-crypt.ps1` — Jet 3 **encoded** reads: 9 real V1997 files
  encoded with the Java page transform (via reflection) and the Rust encoder are
  byte-identical, Java/Rust read the same rows, both decode back to the original
  bytes, crafted Jet 3 password files are byte-identical, and both refuse the
  public encode/set-password APIs with Java's message.
- `tools\verify-datetime.ps1` — `DateTimeType` (DATE / LOCAL_DATE_TIME; 5.0.4
  has no STRING mode) and `ColumnOrder` on real files (`oldDatesV2007` with
  1392-1582 dates, `common1V2003/V2010`) and generated V2003-V2016 databases:
  value class/millis/stored bits and column order dumps identical, and writes in
  both modes byte-identical (the modes differ only for sub-millisecond values).
- `tools\verify-memdb.ps1` — the in-memory `MemFileChannel`/`MemDatabase`:
  Java memory-written bytes equal the disk file, and the Rust memory-written
  bytes are BYTE-IDENTICAL to the Java disk file for V2003/V2007/V2010/V2016
  (491,520 / 610,304 / 626,688 / 647,168 bytes), including reopening the bytes.
- `tools\verify-realcrypt.ps1` — **real** Access/jackcess-encrypt files:
  `db-enc.mdb`/`db97-enc.mdb` (encoded, keys recovered and re-encoded exactly),
  `pwd-set`/`pwd2-set` (password `testpassword`, SID sets and page-0 field
  identical), and `db2007-enc`/`db2007-oldenc`/`db2013-enc`/`db-nonstandard`
  (agile/RC4/AES, passwords `Test123`/`1234`/`password`): reads, decrypted
  bytes and wrong-password failures all identical.
- `tools\verify-msisam.ps1` — Microsoft Money `.mny`: both encodings (old
  file-derived key and modern MD5/SHA-1 hashed password), 5 real files
  (873/1691/3192/876/3192 rows, 402-504 index definitions), passwords
  (`Test12345`), wrong-password and create/write refusals: 47/47 checks
  identical.
- `tools\verify-codepage.ps1` — Jet 3 code pages: all 109 JDK pages (87
  single-byte + 22 multi-byte incl. the IBM EBCDIC DBCS state machines):
  10,886,038 exact decode/encode comparisons against the JVM, plus real-file
  text dumps with patched code pages (EBCDIC-header files are proven unopenable
  by Java too, so the charset is checked through the documented override).
- `tools\verify-apisurface.ps1` — 18 Database/Table read-API dumps (real files
  incl. a `.mny`, generated V2003-V2016, version-history/multivalue/attachment)
  identical, including exceptions.
- `tools\verify-jet3-depth.ps1` — Jet 3 deep read: 103 tables, 706 columns,
  162 indexes, 5,062 entries, 1,905 properties, 680 ACE rows, 2 relationships
  and 153 write-refusal probes (114 refused, files SHA-256-unchanged) across the
  9 V1997 files: both dumps identical.
- `tools\verify-miscdeg.ps1` — `Double.toString`/`Float.toString` on 237,945
  hard values (all subnormals, random bit patterns, powers of ten, ties) plus
  Java-serialized binary import, `ColumnFormatter` (835-line format matrix) and
  the no-scratch in-memory database checks: all text/byte identical.
- `tools\verify-timezone.ps1` — IANA zones with DST and historical offsets
  (UTC, GMT-05:00, America/New_York, Europe/Berlin, Asia/Shanghai,
  Australia/Lord_Howe, Pacific/Apia): wall-clock <-> instant conversions across
  DST gaps/overlaps, 1901/1970/2038 boundaries and pre-1901 LMT values, plus
  byte-identical writes/reads on all four formats (144 checks).
- `tools\verify-zonedates.ps1` — zoned DATE-mode values, properties, index
  lookups and reads for the same zones: 113 checks identical.
- `tools\verify-rowapi.ps1` — the Row/table/cursor API: Row contents, ids,
  typed getters (incl. null/wrong-type paths), getBlob, DATE mode, map
  equality, iteration order and a generated add/update/delete/iteration
  scenario: identical dumps and byte-identical databases on V2003/V2007.
- `tools\verify-dbapi.ps1` — DatabaseBuilder/static factories, builder-created
  databases (byte-identical on all four formats), open options (read-only,
  charset, named/fixed zones), encrypted open, table/meta iterables,
  TableMetaData.open/definition and close semantics (32 checks).
- `tools\verify-objapi.ps1` — Column accessors (row values, SQL type, version
  history, complex info, validators), Index/Index.Column accessors,
  Relationship table/column resolution and TableNameMap add/remove/save
  (saved files byte-identical, V2003/V2007/V2016): 43 checks.
- `tools\verify-utilapi.ps1` — DataType byte/SQL/unit conversions, PropertyMap
  put forms (blob bytes), OleBlob.Builder layouts, EvalConfig defaults and all
  validator results/messages: 3,738 identical lines.








The catalog timestamps are made deterministic: the harness reads the
`DateCreate` values the Java generator wrote and passes the same instants to the
Rust generator (`--now-ms`, one value per catalog object).

## Workspace layout

| path | contents |
| --- | --- |
| `src/` | the Rust port (`jackcess` crate) |
| `src/bin/mdbgen.rs` | generator for the verification scenarios |
| `src/bin/mdbdump.rs` | structural dump (including foreign key references), row re-encode checker, property dump, relationship dump |
| `java-src/` | Java reference generator and helper tools (`Gen`, `Append`, `GetDates`, `DbProps`, `TableProps`, `PropFixtures`, index oracle) |
| `jackcess-src/` | extracted sources of jackcess 5.0.4 (reference) |
| `resources/` | empty database templates, collation tables and property fixtures embedded by the crate |
| `tools/verify.ps1` | end-to-end byte comparison harness |
| `tools/verify-props.ps1` | property-map comparison harness |
| `tools/verify-attach.ps1` | attachment/deflate byte comparison harness |
| `tools/verify-link.ps1` | linked-database read/write comparison harness |
| `tools/verify-expr.ps1` | expression-engine Java-oracle harness |
| `tools/verify-query.ps1` | query-model/SQL Java-oracle harness |
| `tools/verify-linkwrite.ps1` | linked-table write byte comparison harness |
| `tools/verify-cursor.ps1` | cursor API byte comparison harness |
| `tools/verify-cursor-range.ps1` | cursor ranges/savepoints/iterables byte comparison harness |
| `tools/verify-export.ps1` | export/import text and database comparison harness |
| `tools/verify-crypt.ps1` | Jet encoding/password byte comparison harness |
| `tools/verify-crypt-office.ps1` | ACE agile/ECMA encryption comparison harness |
| `tools/verify-querydb.ps1` | query/metadata integration comparison harness |
| `tools/verify-namemap.ps1` | name-map blob comparison harness |
| `tools/verify-exportall.ps1` | exportAll/streaming comparison harness |
| `tools/verify-guid.ps1` | GUID auto-number behaviour + explicit-GUID byte harness |
| `tools/verify-realquery.ps1` | real Access query files + ODBC link metadata harness |
| `tools/verify-jet3.ps1` | Jet 3 / V1997 read dump harness (27 real files) |
| `tools/verify-ole.ps1` | OLE compound-document / OleBlob harness |
| `tools/verify-errorhandler.ps1` | ErrorHandler/DebugErrorHandler trace harness |
| `tools/verify-queryprops.ps1` | query property-map / query name-map harness |
| `tools/verify-customlink.ps1` | CustomLinkResolver moved-target harness |
| `tools/verify-jet3-crypt.ps1` | Jet 3 encoded/password read harness |
| `tools/verify-datetime.ps1` | DateTimeType / ColumnOrder / write-mode comparison harness |
| `tools/verify-memdb.ps1` | in-memory database byte comparison harness |
| `tools/verify-realcrypt.ps1` | real Access/jackcess-encrypt encrypted files harness |
| `tools/verify-msisam.ps1` | MSISAM (.mny) read/decryption harness |
| `tools/verify-codepage.ps1` | code-page decode/encode parity harness |
| `tools/verify-apisurface.ps1` | Database/Table read-API surface harness |
| `tools/verify-jet3-depth.ps1` | Jet 3 deep read/exception harness |
| `tools/verify-miscdeg.ps1` | Double/float text, binary import, ColumnFormatter harness |
| `tools/verify-timezone.ps1` | IANA/DST time-zone conversion harness |
| `tools/verify-zonedates.ps1` | zoned DATE-mode value/lookup harness |
| `tools/verify-rowapi.ps1` | Row/table-iteration/cursor API harness |
| `tools/verify-dbapi.ps1` | DatabaseBuilder/iterables/metadata API harness |
| `tools/verify-objapi.ps1` | Column/Index/Relationship/TableNameMap API harness |
| `tools/verify-utilapi.ps1` | DataType/PropertyMap/OleBlob/validators/EvalConfig API harness |
| `tools/verify-all.ps1` | runs every harness above and summarises |
| `src/bin/attachgen.rs`, `src/bin/linkread.rs`, `src/bin/linkwrite.rs`, `src/bin/cursorgen.rs`, `src/bin/cursorrange.rs`, `src/bin/exportgen.rs`, `src/bin/jetcrypt.rs`, `src/bin/officecrypt.rs`, `src/bin/querydb.rs` | generators/dumpers for the specialty harnesses |
| `tools/compare.py`, `tools/bytediff.py`, `tools/pagedump.py` | diff/debug helpers |
| `out/` | generated files (scratch) |

## Environment

- Rust: `C:\Tools\Rust` (`cargo` on `PATH` via `C:\Tools\Rust\cargo\bin`)
- JDK: `C:\Tools\JDK25`
- Java classpath for the reference tools: `jackcess-5.0.4.jar;java-classes`
  (compile with `javac -encoding UTF-8 -cp jackcess-5.0.4.jar -d java-classes java-src\*.java`);
  the encryption harnesses additionally use
  `external\jackcess-encrypt-5.0.2.jar;external\bcprov-jdk18on-1.86.jar`
- Cargo dependencies (fetched through the configured mirror): `rc4`, `sha1`,
  `aes`, `sha2`, `md-5`, `base64`, `getrandom`, `roxmltree`, `hex` - used only
  by the encryption codecs; the database engine itself remains std-only.

```
cargo build
cargo test --lib
powershell -ExecutionPolicy Bypass -File tools\verify-all.ps1
```

## Architecture (mirrors the Java classes)

| module | Java counterpart |
| --- | --- |
| `src/bytebuf.rs` | `java.nio.ByteBuffer` work-alike (absolute/relative access, byte order) |
| `src/byteutil.rs` | `ByteUtil` (3-byte ints, var ints) |
| `src/format.rs` | `JetFormat`, `Database.FileFormat`, `SortOrder` |
| `src/datatype.rs` | `DataType` |
| `src/datenums.rs` | Access OLE date doubles, `LocalDateTime`, time zone |
| `src/pagechannel.rs` | `PageChannel` (page I/O, allocation, header mask, global usage map) |
| `src/usagemap.rs` | `UsageMap` (inline/reference maps, promotion, global map) |
| `src/column.rs` | `ColumnImpl` (definitions, value codecs, text compression, long values) |
| `src/table.rs` | `TableImpl` (table definitions, data pages, row read/write, null masks) |
| `src/tablecreator.rs` | `TableCreator` / `TableBuilder` / `IndexBuilder` (including creation time table/column properties) |
| `src/tableupdater.rs` | `TableUpdater` + `TableImpl.mutateAddColumn` / `mutateAddIndexData` / `mutateAddIndex` / `addUsageMaps` (adding columns and indexes to an existing table) |
| `src/complex.rs` | `ComplexDataType` / `ComplexColumnDesc` / `ComplexColumnInfo` / `MultiValueColumnInfo` / `AttachmentColumnInfo` / `VersionHistoryColumnInfo`, `ComplexValue` / `SingleValue` / `Attachment` / `Version`, `ComplexValueForeignKey`, `ComplexColumnSupport` and the attachment data wrapper (`ComplexColumnCreator` lives in `tablecreator.rs`) |
| `src/index.rs` | `IndexData` / `IndexImpl` (entries, ordering, key encoding) |
| `src/indexpagecache.rs` | `IndexPageCache` (B-tree pages, splits, nesting, deferred writes) |
| `src/indexcodes.rs` | `GeneralLegacyIndexCodes` / `General97IndexCodes` / `GeneralIndexCodes` |
| `src/property.rs` | `PropertyMap` / `PropertyMaps` / `PropertyMapImpl` (MR2/KKD blobs, `save` and the builders' property order) |
| `src/relationship.rs` | `RelationshipBuilder` / `RelationshipImpl` / `RelationshipCreator` and the `MSysRelationships` handling of `DatabaseImpl` |
| `src/fkenforcer.rs` | `FKEnforcer` (foreign key checks, cascade update/delete/null, the shared update depth) and `util.Joiner` |
| `src/linked.rs` | `util.LinkResolver` (`DEFAULT`/`UNRESTRICTED`) and the linked-table reading of `DatabaseImpl` |
| `src/deflate.rs` | zlib 1.3.1 `deflate_fast` level 3 + zlib wrapper and an inflater (attachment compression) |
| `src/expr.rs` | `impl.expr.Expressionator` (tokenizer, parser, evaluator, value model and builtin functions) |
| `src/query.rs` | `query.Query` + `impl.query.QueryImpl` and the concrete query classes (SQL rendering) |
| `src/cursor.rs` | `Cursor`/`TableScanCursor`/`IndexCursor` and their lookups, updates and deletes |
| `src/export.rs` | `util.ExportUtil` / `util.ImportUtil` (delimited text) |
| `src/crypt/` | `CodecHandler`/`CodecProvider`, the Jet page codec/encoding/password and the Office agile/ECMA codecs (`jackcess-encrypt` 5.0.2) |
| `src/namemap.rs` | `NameMap`/`TableNameMap`/`QueryNameMap` (display-name blob) |
| `src/ole.rs` | `util.OleBlob` / `OleUtil` and the OLE compound-file reader |
| `src/mem.rs` | `util.MemFileChannel` and the in-memory database (`MemDatabase`) |
| `src/row.rs` | `Row` (ordered column map + typed getters) and the table/cursor row iterators |
| `src/builder.rs` | `DatabaseBuilder`, the static builder factories and `TableIterableBuilder` |
| `src/validator.rs` | `util.ColumnValidator` / `ColumnValidatorFactory` and the validator chain |
| `src/crypt/msisam.rs` | `MSISAMCryptCodecHandler` (Microsoft Money page keys) |
| `src/database.rs` | `DatabaseImpl` (open/create, system catalog, table lookup, database/summary/user defined properties) |

## Java API compatibility layer

The original Rust API is kept; the `com.healthmarketscience.jackcess` public
shape is added on top.  Java overloads cannot share one Rust name, so they map
to distinct, documented names (each module's docs list its table):

| Java | Rust |
| --- | --- |
| `Row` (Map + typed getters), `Row.getId()` | `row::Row` (`get_string` … `get_blob`), `get_id` |
| `Table.asRow/asUpdateRow` | `Table::as_row` / `as_update_row` |
| `Table.addRowFromMap/addRowsFromMaps` | `Database::add_row_from_map` / `add_rows_from_maps` (and `Table` forms) |
| `Table.updateRow(Row)` / `deleteRow(Row)` | `*_row_object` (the positional overloads keep their names) |
| `Table.iterator/stream` | `TableRowIter`, `Database::row_iter`, `Table::stream` |
| `Table.newCursor/getDefaultCursor` | `Table::new_cursor` / `default_cursor` |
| `Cursor.iterator/stream`, `getNextRow(Collection)` | `CursorRowIter`, `Cursor::stream`, `get_next_row_object(_columns)` |
| `DatabaseBuilder` + static factories | `builder::DatabaseBuilder`, `new_table/new_column/new_index/new_primary_key/new_relationship` |
| `Database.iterator/stream/newIterable/newTableMetaDataIterable` | `Database::tables`/`for_each_table`, `new_iterable`, `new_table_meta_data_iterable` |
| `TableMetaData.open/getTableDefinition` | `TableMetaData::open` / `get_table_definition` |
| `Column` object accessors | `get_table(&Table)`, `get_database`, `get_row_value`, `get_sql_type`, `get_version_history_column`, `get_complex_info`, validator accessors |
| `Index`/`Relationship` accessors | `get_table`, `get_properties`, `new_cursor`, `get_from/to_table/columns` |
| `TableNameMap.addColumn/removeColumn/save` | `add_column`, `remove_column`, `save_for_table` |
| `Database.close/getFile/getZoneInfo/getEvalConfig/validator factory` | snake_case equivalents |
| `Database.getRelationships/getSystemRelationships` | `get_relationships` / `get_system_relationships` |
| `ColumnValidator(Factory)` | `validator::ColumnValidator`, `ColumnValidatorFactory`, `DefaultColumnValidator` (run by insert/update) |
| `OleBlob.Builder` / `Table.newBlob` | `ole::OleBlobBuilder` / `Table::new_blob` |
| `Column.AUTO_NUMBER` | `column::AUTO_NUMBER` + `is_auto_number_value` (generates on add, keeps on update) |
| `DataType`/`PropertyMap` conveniences | `try_*`, `from_sql_type*`, `put_typed*`, `get_value_default`, `stream` |

Known Rust-shape differences: Java's `Row`/`Cursor` iterators throw
`UncheckedIOException`, the Rust iterators yield `Result<Row>`; `Row` typed
getters return `Result<Option<T>>`; `java.util.Date` is represented by
`Value::DateBits` plus a millis helper; `Table` has no database back-reference,
so Row-based add/update/delete live on `Database` and table-associated helpers
take the database explicitly; `EvalConfig` is exposed but the evaluator still
uses its built-in temporal/numeric/function defaults.

## Test evidence beyond the end-to-end comparisons

- `cargo test --lib` — 518 tests (517 passing, 1 large sweep ignored). They include:
  - index entry bytes and index pages against Java for 11 index shapes and 600
    rows (`indexpagecache::tests::many_rows_match_java_index_pages`: all 25
    index pages identical),
  - text collation: 387,204 generated cases (every UTF-16 code unit × 3
    collations × ascending/descending, plus random strings) match Java byte for
    byte,
  - property blobs: 24 Java-generated fixtures plus 35 real `LvProp` blobs from
    the templates round-trip byte-identically,
  - property persistence (`database::tests::*`, `property::tests::*`): the
    database/summary/user defined groups of the `dbprops` scenario and the
    creation time and updated table/column groups of the `tableprops` scenario
    (whose MEMO property makes the blob 12,312/12,288 bytes, stored across
    several long-value pages) match the blobs the Java library wrote (fixtures
    regenerated by `java-src/PropFixtures.java`), the Java builders' `HashMap`
    property order is reproduced (pinned for several name sets), the
    `LvProp` value read back from the file equals the blob, an emptied group is
    stored as a null `LvProp`, and a missing `SummaryInfo` catalog row is
    created on save,
  - column default values and property driven validators
    (`column::tests::*`, `table::tests::*`): the literal forms the Java
    `DefaultValue` expression engine takes (quoted/unquoted text, integers,
    decimals like `1.5` which round half even to 2 in a LONG column, boolean
    constants/numeric values, `Null`, and text that would otherwise look like
    a number) with the exact `generateDefaultValue`/`toInternalValue`
    coercions, the `Unsupported` reports for `=` expressions, date literals,
    date columns and `NUMERIC`/`MONEY` literals jackcess would have to round,
    the `Required`/`AllowZeroLength` messages including the
    `(Db=…;Table=…;Column=…)` context, default values applied on add but not on
    update, rejected inserts/updates leaving the row set and the auto number
    counter untouched, and a property save re-applying the validators to the
    cached table,
  - fresh index root page buffer byte-identical to Java,
  - row update/delete mechanics at the byte level: the delete/overflow flags in
    the row directory, the unchanged page free-space record, the table
    definition row count, the unchanged index unique-entry count on update, and
    the 4-byte overflow pointer with the rest of the header row cleared
    (`table::tests::*`),
  - `KEEP_VALUE` partial updates (`table::tests::*`): an update which keeps
    every column rewrites exactly the same row bytes (fixed values re-encoded
    from their decoded form, variable length values and the long-value
    definition re-used), a kept multi-page MEMO keeps its 12-byte long-value
    definition (the same pointer) and allocates no pages across five updates
    while the file size stays put, while replacing the value allocates new
    long-value pages, the kept value is the old value for the unique /
    non-unique index updates (the unique entry counts do not change, a
    duplicate unique key is rejected and the removed entry is restored), the
    three entry points (`update_row_partial`, `update_value`,
    `update_row_from_map` with absent columns kept and unknown keys ignored)
    agree, a boolean (null mask) value and a kept null stay untouched (an
    update never generates a column default), the foreign key enforcement sees
    a kept foreign key column as its unchanged old value (`fkenforcer`), and a
    `KEEP_VALUE` is rejected for a new row,
  - table mutation mechanics at the byte level (`tableupdater::tests::*`): the
    definition length and max/var/column counts after `add_column`, the new
    column's number/id/variable index/fixed offset/flags/length, the free space
    recomputed from the new length, the definition growth for `add_index`
    (index data block + logical index block + names), the new root page and its
    free space, the populated unique-entry count written into the definition,
    the reuse of existing backing index data for an equivalent index, and a
    definition large enough to spill onto a continuation page (readable after
    reopening the database),
  - row re-encoding: every non-long-value row of the Java-generated
    `MSysObjects` table re-encodes to the exact same bytes
    (`mdbdump <file> MSYSOBJECTS --reencode`),
  - relationship creation at the byte level (`relationship::tests::*`): the
    `MSysRelationships` row payload and its exact re-encoded row bytes, the
    foreign key reference of both supporting indexes (table type, other index
    number, other table page, cascade flags, index type) read straight out of
    the table definition pages, the index data flags (the primary side reusing
    the primary key index data keeps `0x89`; the secondary side gets a plain
    `0x80` index data), the exact definition lengths and the generated index
    names (`.rB`, walking to `.rE` / `.ra` when taken, and the relationship name
    as the secondary index name),
  - complex columns (`complex::tests::*`): the `Test` table definition page and
    the flat table definition page of a multi-value column match the bytes the
    Java library wrote (fixture `resources/complex_tdefs.bin`, regenerated by
    `java-src/ComplexFixtures.java`; the fixture's random flat table name is
    stored in `resources/complex_flat_name.txt` and reused by the test), the
    `COMPLEX_TYPE` type byte and the complex id in the four sort-order bytes,
    the flat table's `_Tags` foreign key column carries the complex foreign key
    ext flag, the catalog flags (`0x40000` on the table with the column,
    `0x800A0000` on the flat table), the `MSysComplexColumns` row values, the
    main row's complex id and the complex auto-number counter; adding a complex
    column to an existing table creates the supporting table, marks the catalog
    flags, gives the existing rows the next complex ids and leaves the column
    usable; the multi-value API (`add_multi_value`, `update_multi_value`,
    `delete_value`, `multi_values`) round trips through a reopen; attachment
    data wrappers encode/decode raw payloads and reject the deflate compressed
    kinds; an attachment column's flat table has the
    `_Files,FileURL,FileName,FileType,FileData,FileTimeStamp,FileFlags,Test_Files`
    shape; and a version history column
    (`version_history_definitions_match_java_fixture`) matches the Java table
    definition, the per-column `MSysComplexTypeVH_<guid>` type table definition
    and the flat table definition byte for byte (fixture
    `resources/complex_vh_tdefs.bin` + `resources/complex_vh_names.txt`, also
    regenerated by `java-src/ComplexFixtures.java`), with the type table's
    `Body,Modified_...` shape, the flat table's
    `_VersionHistory_...,Body,Modified_...,Test_VersionHistory_...` shape, the
    catalog flags (`0x80030000` on the type table), the `VERSION_HISTORY` ext
    flag and the `AppendOnly` properties of the memo column and of its table;
    the version API (`add_version`, `versions` newest first, the refused
    update/delete) round trips through a reopen, and a second append-only memo
    column (or an append-only non-memo column) is rejected on both the create
    and the add-column paths,
  - free space handling (`pagechannel::tests::*`): a deallocated page is not
    handed out again unless `setReuseFreePages` is enabled (then the lowest free
    page comes first and the file does not grow for the reused allocations), and
    `truncateFreeSpace` drops the run of free pages at the end of the file and
    reports how many,
  - linked table creation (`database::tests::*`): the synthetic object id
    continues after the template's and the relationship's ids, the type 6
    catalog row holds the external database/table names, the access control
    entries carry the parent object's SIDs, the lookup entry survives a reopen
    (so the duplicate name check applies to existing linked tables), and the
    name checks reject duplicates, blank linked database names, invalid
    characters and leading spaces,
  - explicit auto-number insertion (`database::tests::*`): the database and
    table level policies (including the table override in both directions), the
    high water mark adjustments of explicit values (lower values ignored),
    generated values continuing after the highest value used, deleted rows not
    lowering it, and the saved `OFFSET_NEXT_AUTO_NUMBER` after a reopen,
  - foreign key enforcement (`fkenforcer::tests::*`): the index key lookup
    (`IndexData::find_rows_by_entry`: an exact key finds the rows sharing the
    entry, including duplicate non-unique keys; a key which is not in the index
    lands on the next entry; a key past the end finds nothing), the rejection of
    an insert whose foreign key has no parent with the exact
    `ConstraintViolationException` message text (including the
    `Foreign Key from C[P_ID] (secondary) to P[ID] (primary) (Db=…)` description
    and the `Adding new row [3, 99, bad] violates constraint` preamble) and an
    unchanged file plus unconsumed auto number afterwards, the cascade and
    restricted behaviour of delete and update (with unchanged files after a
    rejection), null and case-insensitive text foreign keys, a self-relationship
    row which is its own parent (allowed on add and delete), and
    `setEnforceForeignKeys(false)`.
  - calculated columns (`table::tests::*`): the Java-written table definition
    page and first data page (fixture `resources/calc_fixture.bin`, regenerated
    with `Gen calcfixture`) match byte for byte, including the `ResultType`/
    `Expression` properties, the calculated ext flag, the length rules and the
    wrapped value encodings; the dependency order and a dependency cycle, the
    pre-V2010 rejection messages, a calculated column added to an existing
    table, and a `KEEP_VALUE` update which recomputes the calculated columns;
    `mdbdump --reencode` now follows overflow pointers and re-uses non-inline
    long-value definitions so calculated rows re-encode identically.
  - zlib/attachment compression (`deflate::tests::*`): 341/341 Java
    `Deflater(3)` streams byte-identical (3.2 MB of inputs across every block
    type, Huffman edge cases, long matches and window slides) and 94/94 encoded
    attachment payloads (raw and compressed, upper-case extensions, empty and
    large) identical and round-tripping.
  - expression engine (`expr::tests::*`): 434/434 cases of the Java oracle
    (397 values and 37 parse/eval/arithmetic errors) match, with the corpus in
    `resources/expr_ref/` regenerated by `java-src/ExprOracle.java`.
  - linked tables (`database::tests::*`): linked metadata after create/reopen,
    the default resolver's denial message naming the linkee file, the
    unrestricted resolver opening and reading the target, a read-only linker
    opening the linkee read-only, and reading through links not modifying the
    linker; linked *writes* inserting/updating (indexed key changes,
    `update_value`, map updates)/deleting through a link land in the linkee and
    leave the linker bytes untouched, linkee constraints/defaults/validators
    apply, schema writes (`add_column`/`add_index`/auto-number policy) change the
    linkee, and a read-only linkee rejects writes.
  - query model (`query::tests::*`): 107/107 synthetic `MSysQueries` cases
    match `QueryImpl.toSQLString()`, with the row fixtures and expected text in
    `resources/query_ref/` regenerated by `java-src/QueryOracle.java`.
  - calculated `MONEY`/`FLOAT` result types (`calcfloat`): the coercion to
    `Value.Type.DOUBLE` and the currency/float write path match Java
    byte-for-byte (V2010/V2016/V2019).
  - cursor APIs (`cursor::tests::*`): index-entry ordering/lookup on ascending,
    descending, unique and non-unique indexes, table-scan find semantics,
    current-row validity and updates/deletes through both cursor kinds.
  - delimited import/export (`export::tests::*`): escaping/quoting, delimiters,
    multi-line values, nulls, CRLF, the parser's stale-matcher quirk, Java
    number/date/binary formatting, batch writes and table creation.
  - database settings (`database::tests::*`, `datenums::tests::*`):
    `setEvaluateExpressions` gating exactly the Java parts (calculated columns,
    defaults, expression validators; `Required`/`AllowZeroLength` stay active),
    fixed-offset time zones (UTC/GMT-05:00) controlling catalog and date
    conversions, and the stored `ColumnOrder`/`DateTimeType` settings.
  - the OLE date decoding now carries a rounded whole second of nanoseconds into
    the seconds field (like `Duration.ofSeconds`), and `Value::Byte` is treated
    as Java's signed byte (0xFF writes as -1), both with regression tests.
  - Jet encryption (`crypt::tests::*`): the RC4 page transform, header mask and
    key handling, the password field/SID remask and the encoding key fixtures,
    all pinned against Java-derived values (`tools/crypt_fixtures.txt`).
  - Office encryption (`crypt::office::tests::*`): the agile XML descriptor,
    password key derivation, block chaining, verifier and the standard/RC4
    providers, pinned against Java-derived fixtures
    (`tools/office_crypt_fixtures.txt`), plus the deterministic-writer bytes.
  - query integration and metadata (`database::tests::*`, `table::tests::*`):
    grouping `MSysQueries` rows by object id, catalog type-5 lookup, the
    table/column/index metadata fields (including type-4/type-6 rows and the
    definition-only refusal).
  - validation rules (`column::tests::*`, `table::tests::*`): rule loading from
    the properties, exact `Invalid column value '<v>'` / `Invalid row` /
    `ValidationText` messages, the "condition evaluated to Null" error, the
    no-write/no-auto-number-consumed rejection, and the evaluateExpressions
    gate.
  - cursor ranges/savepoints/iterables (`cursor::tests::*`): reversed/empty/
    prefix bounds, boundary positions, inclusive flags, savepoint restore
    semantics and forward/reverse iterables with column subsets.
  - the expression engine's extended builtins (`expr::tests::*`) are covered by
    the 3280-case oracle corpus in `resources/expr_ref/`.
  - name maps (`namemap::tests::*`): blob parsing for table/query maps, records,
    ordering of mismatches, unicode names and the malformed-blob errors, all
    pinned to the same bytes Java parses (`resources/namemap_ref/`).
  - `exportAll`/streaming (`export::tests::*`): table filtering, filename
    escaping/collisions, overwrite/append and the empty-database case.
  - GUID auto numbers (`table::tests::*`): v4 shape/uniqueness, explicit policy
    and error messages, update/keep, explicit-insert page bytes and the
    duplicate-auto-column rejection.
  - Jet 3 (`format::tests::*`, `table::tests::*`): jump-table offsets (short and
    jumped), deleted/overflow rows, MEMO long values, entries, read-only
    refusal, windows-1252 decoding, VERSION_3 constants and real fixtures.
  - OLE (`ole::tests::*`): header/FAT/DIFAT/mini-FAT/directory parsing, big and
    mini streams, nested storages, loops/truncation/block errors, the content
    wrappers and Java/POI-compatible messages.
  - real query fixtures (`query::tests::*`): 6 Access-created databases (101
    queries) rebuilt from their stored rows and compared against Java.
  - error handlers (`table::tests::*`, `database::tests::*`): default rethrow,
    replacement values, database/table/cursor override precedence, callback
    context and messages, throwing handlers, unsupported-column raw values.
  - query properties (`query::tests::*`): catalog `LvProp` loading, empty and
    malformed maps, name-map parsing, save round trip and the real-file
    fixtures (121 queries).
  - custom link resolver (`linked::tests::*`): map/loader forms, relative and
    absolute resolution, case-insensitive names, missing mappings and the
    temp-DB loader.
  - Jet 3 crypt (`crypt::tests::*`): the Jet3 page transform and key layout,
    the password field without the creation-date mask, and real V1997
    encode/read/decode integration.
  - index stress (`indexpagecache::tests::*`): page-cache LRU purge order,
    stale page-tail bytes, and a 900-row multi-index tree whose pages are parsed
    back after splits and bulk deletes.
  - date types (`database::tests::*`, `datenums::tests::*`): DATE/LDT read and
    write conversions, the byte-identical whole-millisecond invariant (438,000
    values), old-date round trips and the column-order view.
  - in-memory database (`mem::tests::*`): grow/truncate/position/read-past-end,
    read-only writes, and a database round trip through memory.
  - real encrypted files (`crypt::real_file_tests::*`): Jet 3/4 encoding keys,
    password fields, SID sets, Office decrypted SHA-256s.
  - API/exception parity (`database::tests::*`, `table::tests::*`,
    `index::tests::*`, `bytebuf::tests::*`): batch-update wrapping, system-table
    catalog dates/name maps, referenced-index validation, ACE rows, Java NIO
    position wording and sort order ids.

- `java-src/idx/.../IdxOracle.java` generates the Java reference values used by
  the index tests.

## Scope and known gaps

**Scope policy:** the port implements only behaviour that jackcess 5.0.4 (and
jackcess-encrypt 5.0.2 for encryption) actually exposes.  Anything for which
the reference libraries provide no API (VBA/`MSysAccessObjects`, deleting
relationships, removing columns/indexes, JDBC/ODBC data access, MSISAM/Jet 3
writing, the old Office encryption writers, `Partition`/`D*` aggregates,
`importResultSet`) is **out of scope and is not a porting gap**; it is neither
implemented nor planned.  The lists below only cover behaviour the reference
library has.

Implemented and verified: Jet 4 (Access 2000/2003) and ACE (2007/2010/2016)
database creation, table creation with columns/indexes, adding columns and
indexes to an existing table (`ColumnBuilder.addToTable` /
`IndexBuilder.addToTable`: table definition growth, column id/offset assignment,
index data creation and population, definition splits across pages),
row insertion (including long MEMO/OLE values, compressed text, all scalar
column types), row update/delete (in-place rewrites and relocations through
overflow rows, long-value pages, index entry replacement, table-definition row
counts), `KEEP_VALUE` partial row updates (`Column.KEEP_VALUE` /
`Value::Keep`: a fixed-length column keeps its old decoded value, a
variable-length column also re-uses its raw on-page bytes so a kept MEMO/OLE
value keeps its existing long-value definition and pages instead of being
written again, the kept value is the old value for the index updates, the
foreign key checks and the validators, an update never generates a column
default, and the `updateValue`/`asUpdateRow`/`update_row_from_map` entry
points), index maintenance during insertion (splits, nesting, prefix
compression), system catalog/ACE updates, usage maps, database, table, column
and index property parsing and persistence (`PropertyMap`/`PropertyMaps.save`
writing the blob back as the `LvProp` of the owning catalog row, including
database creation time properties, the table builder's creation time table and
column properties, property updates/removals, and the Java builders' `HashMap`
property order), and table relationships (`RelationshipBuilder.toRelationship`
writing the `MSysRelationships` rows, the relationship catalog object and ACEs,
and the supporting foreign key indexes with their `ForeignKeyReference`
metadata; `Database.getRelationships` reading them back), foreign key
*enforcement* (`FKEnforcer`: every row added, updated or deleted in a table which
participates in a referential-integrity relationship is checked against the
other side through an index key lookup, cascading updates follow a parent key
change into the children, cascading deletes remove the children (or null their
foreign keys), and a violating operation throws a `ConstraintViolation` carrying
the Java `ConstraintViolationException` message and leaves the file unchanged;
`Database.setEnforceForeignKeys`, on by default like jackcess), linked table
*creation* (`Database.createLinkedTable`: the synthetic object id, the type 6
catalog row with its `Database`/`ForeignName` values and the ACEs), free page
handling (`Database.setReuseFreePages` handing deallocated pages out again and
`Database.reclaimFreeSpace` truncating the trailing free pages), explicit
auto-number insertion (`Database.setAllowAutoNumberInsert`, the per-table
override and the LONG `handleInsert`/high water mark bookkeeping), complex
columns (`ColumnBuilder.setMultiValue`/`setValueList`/`setRowSource`/
`setAttachment`: the `MSysComplexColumns` row, the per-column flat table with
its parent foreign key/order/value columns, its indexes and usage maps, the
`COMPLEX_TYPE` definition and complex id, the table's `0x00040000` catalog
flags, the row values which are complex foreign key ids and the complex
auto-number counter, `ColumnBuilder.addToTable` with a complex column, and the
`ComplexValueForeignKey` read/write API for multi-values and attachments), and
version history (append-only memo) columns (`ColumnBuilder.setAppendOnly` on a
memo column: the per-column `MSysComplexTypeVH_<guid>` type table with the memo
column and its `Modified_...` date column, the `VERSION_HISTORY` ext flag on
the added `COMPLEX_TYPE` column, the `AppendOnly` property on the memo column
and on its table, the flat table laid out as foreign key / memo value / modified
date / auto-number key, the `ColumnBuilder.addToTable` path, and the read back
of the column and date through `Database.version_history_info` plus the
`getVersions`/`addVersion` API, whose strongly typed update/delete methods are
refused exactly like jackcess does).  Calculated columns are ported for the
formats which support them (V2010 and later): the calculated definition flags
and length rules, the `ResultType`/`Expression` properties, dependency-ordered
evaluation on add and update through the expression engine, the wrapped value
encoding per result type and the read-back unwrap, with `calc`/`calcupdate`
byte-identical in V2010/V2016/V2019.

Column default values and the property driven column validators are
implemented: when a row value is null on insert, a non-autonumber column's
`DefaultValue` property is evaluated and coerced with the same expression
result type the Java library uses (`ColumnImpl.generateDefaultValue`), and the
`Required` (default false) and `AllowZeroLength` (default true, textual
columns only) properties are enforced on insert and update with the Java
`InvalidValueException` messages.  The supported `DefaultValue` forms are the
ones which can be reproduced byte for byte:

- numeric literals for the numeric and `BOOLEAN` columns (`0`, `-1`, `1.5`,
  scientific notation, `TRUE`/`FALSE`/`YES`/`NO`/`ON`/`OFF`/`NULL`), including
  the Java expression engine's conversions (a `LONG` column rounds a decimal
  half even, a boolean is any non-zero number, `BigInt` truncates),
- quoted text literals (`"abc"`, with the doubled-quote escape, and single
  quotes) for `TEXT`/`MEMO`,
- unquoted text defaults for `TEXT`/`MEMO`, which the Java library takes as the
  literal text (the whole trimmed property) unless it starts with `=`.

A default starting with `=` is full expression mode in the Java library; the
port returns a clear `Unsupported` error instead of writing a different value.
`#date#` literals, defaults on date/GUID/binary columns, defaults on textual
columns which start with `#` or `[`, numeric literals too large for the port's
exact decimal parser, and `NUMERIC`/`MONEY` literals which jackcess would have
to round (the `NUMERIC` column's scale, or the four decimal places of a
currency value whose exact binary `double` needs more, as with `0.1`) are also
reported as unsupported rather than approximated.  `NUMERIC`/`MONEY` literals
which jackcess stores exactly (`1.5`, `2.25`, ...) are supported.  As in
jackcess, an update never generates a default value (a null stays null, subject
to the validators), and a property save reloads the cached table's defaults
and validators.

Linked database *reading and writing* are ported (`util.LinkResolver`: `DEFAULT`
denied and `UNRESTRICTED`; linkee databases are owned children of the linker,
linked catalog rows resolve through `get_table`/`find_table`/`read_rows_with_ids`,
`isLinkedTable`/`getLinkedDatabases` are exposed, and every table-name write
entry point (rows, schema, complex/version-history values) delegates to the
linkee so the linker file is untouched while the linkee changes).
Attachment payloads of every file type are
byte-identical: file types Access stores raw are stored raw, and every other
extension is deflate compressed with a faithful zlib 1.3.1 level-3 port
(341/341 oracle streams byte-identical) whose inflater also reads compressed
attachments.  The expression engine (`impl.expr.Expressionator`: tokenizer,
parser, value model, operators, `LIKE`, dates, conversions and a large builtin
function subset) is ported standalone in `src/expr.rs` and verified against
Jackcess' own calculated-column evaluation for 434 oracle cases; it is the
foundation for calculated columns, `=...` defaults and validation rules.
The public cursor APIs are ported (`Cursor`, `TableScanCursor`,
`IndexCursor`: find/first/next/closest by map, column or entry, iteration,
current-row get/set, update and delete) including row ranges
(`setStartRow`/`setEndRow`/entry bounds, inclusive flags, boundary positions),
savepoints and the `IterableBuilder`/`EntryIterableBuilder` iterators.  The
delimited import/export
utilities (`ExportUtil`/`ImportUtil`: options, filters, quoting, headers,
table creation, and `exportAll` with directory/stream output, filename
escaping, overwrite/append) are ported and compared against Java.  Table,
query and column display-name maps (`NameMap`/`TableNameMap`/`QueryNameMap`,
the binary blob format, records and mismatches) are parsed from the owning
property and exposed through `Table`, matching Java's dumps and its errors for
malformed blobs.  Database behaviour
settings are ported where they affect written bytes: `setEvaluateExpressions`
(gates calculated columns, defaults and expression/row validators exactly like
Java, while `Required`/`AllowZeroLength` stay active), fixed-offset time zones
(UTC/GMT-05:00 and any offset) used by all date conversions and catalog
timestamps, and the stored `ColumnOrder`/`DateTimeType` settings.  Expression
based field and table validation rules (`ValidationRule`/`ValidationText`) are
enforced on insert and update with the exact Java messages and leave the file
and auto-number counters untouched on rejection, and the expression engine now
also covers date/time, `Format`, financial functions, `Rnd`/`GenGUID` and the
other builtins Jackcess implements, including custom `Format` patterns (3280 oracle cases).  GUID auto-number
columns generate a version-4 UUID on insert (honouring the explicit-insert
policy and its errors, and keeping the value on update/`KEEP_VALUE`).  The crate
now depends on the `rc4`/`sha1` crates for the encryption work (see below).
Jet 4 database *encoding* ("Encode/Decode Database") and the Jet database
*password* are ported from jackcess-encrypt 5.0.2 and byte-identical to it for
the same key/password: page coding is RC4 keyed by `encodingKey ^ pageNumber`
with page 0 unmasked and holding the key, the password field is the encoded
password under the creation-date mask and the ACE SIDs are remasked
(`SidRemasker`) so the file stays openable.  Encrypted/encoded databases can be
read through the ported `CodecProvider` (`Database::open_path_with_password`,
`open_path_with_provider`), and the file-level operations
(`crypt::encode_file`/`decode_file`, `PasswordUtil::set/remove_database_password`,
in-place and path forms) reproduce Java bytes exactly, including
wrong-password rejection (the Jet format stores no password verifier, so both
implementations detect it from the stored field, like jackcess-encrypt).
ACE (`.accdb`) encryption is ported from the same add-on: agile
`OFFICE_2010`/`OFFICE_2013` (AES-CBC, SHA-1/SHA-512, the password key
derivation and the XML `EncryptionInfo` descriptor) for reading *and* writing,
plus ECMA standard, non-standard AES, RC4 CryptoAPI and Office binary RC4 for
reading and removing encryption.  As in jackcess-encrypt, an encrypted `.accdb`
is not an OLE `EncryptedPackage` container: page 0 stays plain (encoding key,
masked password field, `EncryptionInfo`) and every page is a sector encrypted
whole with `IV = H(keyDataSalt ‖ (encodingKey XOR pageNumber))`, so encryption
moves no data.  A deterministic mode (fixed salts/key/verifier and encoding
key, matching Java's public `AgileEncryptionWriter` constructor) makes the
encrypted bytes identical; the default random mode is cross-read/decrypted
against Java instead.  Query definitions are integrated into `Database`
(`get_queries`, `get_system_queries`, `get_query`, parameters) by reading
`MSysObjects` type-5 rows and grouping `MSysQueries` by object id, and the
read-side metadata is ported: `TableMetaData` (LOCAL/LINKED/LINKED_ODBC, flags,
created/updated dates, linked db/table, connection name), table/column/index
metadata, row counts, SQL types and length-in-units.  Jet 3 / Access 97
**reading** is ported (2048-byte pages, the jump-table variable-column row
format, Jet 3 indexes, overflow rows, long values, windows-1252 code-page
text, forced read-only opening) and verified against 27 real V1997/V2000/V2003
files, and the OLE utilities (`OleBlob`/`OleUtil`: compound-file
header/FAT/mini-FAT/directory, streams, simple packages, links, compound
storages) are ported and verified on the real `blobV2007.accdb` plus
synthetic/corrupt fixtures.  The `ErrorHandler`/`DebugErrorHandler` row-error
semantics are ported (a bad column value is replaced - Java never skips the
row - while table-level failures still surface), with database/table/cursor
overrides and Java's callback context/messages.  Query property maps and query
name maps (`Query.getProperties`/`getNameMap`, read from the query's `LvProp`)
are ported and saveable, and `CustomLinkResolver` (map/loader forms, temp-DB
loader, relative/absolute paths) is ported for moved linked databases.  Jet 3
*encoded* databases can be read and decoded (verified against the Java page
transform on real V1997 files) and Jet 3 password fields are honoured on read;
the public encode/set-password APIs keep Java's refusal.  Index writing is
stress-verified at scale (deep multi-level B-trees, splits, churn, compound and
descending keys), including Java's index page-cache LRU and its stale
page-tail behaviour.  `DateTimeType` is implemented with jackcess' real
semantics (DATE reads as date bits with a millis view, LOCAL_DATE_TIME is the
default; 5.0.4 has no STRING mode) and written bytes are byte-identical in both
modes.  `ColumnOrder` is a real view over the loaded columns (data order for
rows/writes, display order for `get_columns`), and in-memory databases
(`MemFileChannel`/`MemDatabase`: create/open/take bytes) write the same bytes
as disk files.  Real Access/jackcess-encrypt encrypted files (encoded Jet 3/4,
database passwords, agile/RC4/AES Office files) are read and decrypted exactly
like Java.  MSISAM / Microsoft Money `.mny` databases are read (Jet 4 layout,
old file-derived and modern MD5/SHA-1 password keys, pages 1-14 encoded) with
exact Java parity, and all 109 JDK code pages (87 single-byte + 22 multi-byte,
including the IBM EBCDIC DBCS state machines) are decoded/encoded byte-exactly
for Jet 3 text, verified against the JVM with over ten million comparisons.
`DateTimeType` also drives property-map dates, catalog created/updated dates
and index search keys (`Value::DateBits` equality follows `java.util.Date`),
and the corrupt-null-mask `ErrorHandler` path retries the mask per column
exactly like Java (428-line identical callback trace).  Utility parity is
complete for `Double.toString`/`Float.toString` (including subnormals and
shortest-round-trip ties), Java-serialized binary import from text, the
`ColumnFormatter` utility (including its internal property save), and
in-memory databases now open directly over a channel without any scratch file.
Expression `Format`'s number path uses arbitrary-precision decimals
(`bigdecimal`), so extreme doubles and >38-digit coefficients match Java's
`BigDecimal` output.  The database time zone supports IANA zones through
`chrono-tz` (DST, gaps/overlaps and historical LMT offsets) in addition to
fixed offsets, with every date path (rows, catalog, properties, indexes)
using it.  The public read API is audited
against Java (`get_file_format`, charset, zone, sort order, table names,
metadata, index/ACE data, cursors) with 18 exact dumps.

Out of scope by reference (jackcess 5.0.4 / jackcess-encrypt expose no API or
writer, so there is nothing to port or compare): removing columns or indexes
after creation, deleting relationships (`Database.deleteRelationship`),
`MSysAccessObjects`/VBA, JDBC/ODBC remote data access, MSISAM and Jet 3
*writing*, and the older Office encryption schemes on the *write* side (reading
and removal are ported).  These are **not implemented and not planned**, and
are not counted as gaps.

Remaining parity gaps (small): complex columns of a kind jackcess itself cannot
read (`UnsupportedColumnInfo`) are surfaced as unsupported data (Jackcess only
exposes their metadata); Java-object serialization of non-text values supplied
by a custom `ImportFilter` cannot be reproduced in Rust (no Java object model);
and historic IANA-zone results can differ
if the JVM's tzdata version differs from `chrono-tz` (the tested matrix passes).
Two documented API details: `Value` has no `Hash` (not observable through the
public API), and `Table::set_date_time_type` alone does not re-key already
loaded indexes (use `Database::set_date_time_type`, like Java's per-database
factory).  API-shape caveats (overload name mapping, no database back-reference
on `Table`, `Result`-returning `Row` getters/iterators, and `EvalConfig` not yet
consulted by the evaluator) are listed in the Java API compatibility layer
section.
GUID auto-number generation is implemented (`UUID.randomUUID` v4 semantics,
explicit-insert policy, update keeps the value); generated values are random by
design, so they are verified structurally/behaviourally while explicit GUID
insertion is byte-identical.
Writing name maps is Access-only (jackcess reads them, which is ported).
Linked ODBC tables (type 4) are
supported at the *metadata/definition* level only (catalog row, columns/indexes,
and the Java `TableDefinition has no data access` refusal); their remote data
needs a JDBC/ODBC driver, which jackcess 5.0.4 does not provide either.
There are no linked queries or cross-link foreign-key checks (relationships are
per database, like Access' own linked-table behaviour).  Remaining smaller
gaps: `StrConv` non-US locales and malformed `Format` patterns Java itself
rejects (Jackcess' `Partition`/`D*` aggregates do not exist in 5.0.4 either),
and rules over BINARY/OLE/GUID values are reported as unsupported rather than
coerced.  Jet 3 is read-only like jackcess (writing it is unsupported); its
text uses all JDK code pages (87 single-byte + 22 multi-byte, including the
IBM EBCDIC DBCS pages, verified byte-for-byte against the JVM; a real Jet 3
file whose header names an EBCDIC page cannot be opened by Java either, so the
charset is verified through the documented charset override).  MSISAM is
read-only
(no creation/write API in jackcess either).  OLE v4-sector compounds are
verified against generated POI-validated
fixtures (no Windows-authored v4 sample exists).  `DateTimeType` also applies
to property-map dates and index search keys (verified with `DATE`/`LDT` mode
lookups and property dumps); `STRING` does not exist in
jackcess 5.0.4.  jackcess 5.0.4 itself
cannot read a BOOLEAN *input* column inside an expression (it throws a
`ClassCastException`), so only a BOOLEAN result type is exercised; calculated
`MONEY`/`FLOAT` result types are supported.  One diagnostic quirk is documented:
`Database.getDatabasePassword()` on an Office-encrypted `.accdb` returns
meaningless page-0 key material (not the password), and with *random* keys the
two implementations store different key material there; the harness compares
the full encrypted page 0 only in deterministic mode and ignores that one
diagnostic line otherwise.

Calculated columns are ported for the formats jackcess supports (V2010 and
later; V2003/V2007 reject them with the Java message): the calculated ext
flags, the `CALC_FIXED_FIELD_LEN`/`CALC_EXTRA_DATA_LEN` length rules, the
`ResultType`/`Expression` properties in the builder's order, dependency
ordered evaluation on add and update through the expression engine, the
per-type wrapped value encodings (generic fixed, boolean, text, memo through
the long-value path, numeric) and the read-back unwrap.  Append-only/version
history memo columns *are*
ported, including `ColumnBuilder.addToTable` (the memo column is added, the
version history complex column jackcess adds beside it is created and its
supporting tables exist, the existing rows get the next complex ids, and the
table's `AppendOnly` property is saved after the column properties), as are
complex multi-value and attachment columns (the supporting tables and
`MSysComplexColumns` row are created first, the table's catalog flags are
marked, and the rows written before the column get the next complex ids).  The
port rejects the unsupported parts (`Unsupported`/`IllegalArgument`) instead of
half-implementing them.

`KEEP_VALUE` on a *calculated* column recomputes it, like the Java library:
calculated columns are excluded from raw byte reuse
(`_calcColEval.willCalculate`) and the kept input is replaced by the recomputed
value; `calcupdate` and the calculated column unit tests pin this.

Known complex column gaps: a complex column of a shape neither
`MultiValueColumnInfo` nor `AttachmentColumnInfo` nor
`VersionHistoryColumnInfo` recognises (jackcess' `UnsupportedColumnInfo`) is
read as `ComplexDataType::Unsupported`; and the attachment OLE wrapper is only
understood for the two jackcess storage kinds (the wrapper header, content
header and file extension are written/read exactly, the raw payloads are stored
raw and every other file type is deflate compressed at level 3 with a
byte-identical zlib stream, but the "real" OLE compound document is not built,
matching jackcess which stores the raw payload in the OLE column).

Note: jackcess does not delete the versions of a row when the row is deleted
(no cascade), so the `versionhistory` scenario's deleted row leaves its version
rows in the flat table; the port mirrors that.  Likewise the strongly typed
`Version.update()`/`delete()` and `VersionHistoryColumnInfo.updateValue`/
`deleteValue`/`deleteAllValues` throw `UnsupportedOperationException` in
jackcess, and the port returns `Unsupported` for the corresponding methods.

Note: `src/property.rs` implements property blob parsing/serialization, the
object/column/index grouping model and `save()` (through the catalog row
machinery of `Database::write_property_maps`).  A property save rewrites the
whole catalog row, so other *long value* columns of the same row (for example
`Lv`, `LvModule` or `Connect` of a linked table) are decoded and written again
instead of having their raw bytes kept the way `TableImpl`'s `KEEP_VALUE` path
does; the property scenarios' rows only hold `LvProp`, so their bytes match.
`PropertyMap` values are limited to the scalar types `PropertyMapImpl` can
write (see the conventions at the top of `src/property.rs`); binary/`OLE`
values must be byte arrays and `NUMERIC` values must fit in an `i128`.

Note: the cascade updates/deletes of the foreign key enforcer rewrite a child
row from its column *values* (read back and re-encoded) instead of passing
`Column.KEEP_VALUE` for the unmodified columns the way `FKEnforcer` does.  For
fixed-length columns and ordinary variable-length values the re-encoding is
byte-identical (the `fkadd`/`fkdel`/`fkupd` scenarios have no long values), but a
cascaded update of a row holding a MEMO/OLE value takes new long-value pages
rather than keeping the existing ones.  The row set the cascade touches is
collected before the modifications instead of being iterated live through an
`IndexCursor`; the set is the same for the relationships the scenarios cover.

The former ACE/V2016 divergence of the `partialupdate` table shape (three
indexes, one of them unique on a TEXT column, 165+ rows) was **not an
allocator bug**: it was a call-granularity difference in the scenario
generators, and it is now fixed.  `Gen.java` adds every row with
`Table.addRowFromMap`, so jackcess runs the complete per-row write for every
row: `PageChannel.startWrite`, the data page write, then
`updateTableDefinition` -> `IndexData.update` -> `IndexPageCache.write`
(`handleEmptyPages`, `preparePagesForWriting` and its entry-prefix
compression/`splitDataPage`/`allocateNewPage`, then `writeDataPages`).
`mdbgen` used one `Database::add_rows` call for the whole row list; that is the
port of jackcess' *batch* `TableImpl.addRows`, which keeps the index changes in
memory and only writes/splits the index pages once, at the end of the batch.
Both paths are faithful (batch-vs-batch and per-row-vs-per-row are
byte-identical for V2003, V2007 and V2016 at the sampled row counts up to 500),
but they are different histories once an index page crosses
`maxPageEntrySize`.  For V2016 (the `GENERAL` sort order and general index
codes) the unique `ByName` page first crosses it at about 150 `fillerNNN` rows:
the per-row path recomputes the compressed entry prefix in
`preparePagesForWriting` after each row, so the page still fits, while the
batch path compresses only at the end and then splits.  At 500 rows the
per-row file has the `ByName` root nested on page 92 with leaf pages
102/103/107 and data pages 94,96-101,104-106 (442,368 bytes), while the batch
file has leaf pages 105/106 and data pages 94,96-104 (438,272 bytes); the index
usage map (page 95), the global usage map (page 1), the primary key entries'
row ids and every page after the first split differ.  A related effect is that
jackcess reuses the index page buffer for reads and writes
(`IndexData._indexBufferH`), so a page keeps the bytes of the previous, larger
write in its free space: at 165 rows the per-row page 92 and the batch page 92
have the same header, entries and free space and differ only in the 1,557
free-space bytes beyond the used area (the batch write leaves zeros there).
V2003 (Jet 4 index codes) and V2007 (`GENERAL_LEGACY`) cross the same
thresholds at larger row counts (about 300), which is why the same table looked
byte-identical there at 165 rows.

`mdbgen` now adds every row with a single-row call (`add_rows_one_by_one`),
mirroring `Gen.java`'s `addRowFromMap` loops, so the scenarios are
byte-identical for any row count; `Database::add_rows` keeps the batch
`TableImpl.addRows` semantics (the documentation says so, including when the
per-row history is needed).  The `diag200` scenario (the `partialupdate` table
shape plus 200 filler rows added one at a time) is part of the harness
defaults and pins this.
















































