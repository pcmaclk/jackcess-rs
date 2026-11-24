# Changelog

All notable changes to the Rust port are recorded here.  The version scheme is
`<jackcess version>-rs.<port revision>`; see the "Versioning" section of
README.md.

## 5.0.4-rs.1 — 2026-11-24

First Rust release, targeting **Jackcess 5.0.4** (and **jackcess-encrypt 5.0.2**
for encryption).

### Added

- Pure-Rust read/write support for Jet 4 (Access 2000/2003) and ACE
  (2007/2010/2016/2019) databases: database/table/column/index creation, row
  insert/update/delete (including MEMO/OLE long values, overflow rows,
  `KEEP_VALUE` partial updates), property maps, relationships and foreign-key
  enforcement, complex multi-value/attachment columns, version-history columns,
  calculated columns, validation rules, and index maintenance.
- Jet 3 (Access 97) read-only support, including the jump-table row format,
  indexes, overflow rows and long values.
- MSISAM (`.mny`, Microsoft Money) reading, old and modern encodings.
- Encryption: Jet 4 encoding and database passwords, Jet 3 encoded reads,
  Office/ACE agile (2010/2013), ECMA standard, RC4 CryptoAPI, binary RC4 and
  non-standard AES — verified against jackcess-encrypt and real encrypted
  files.  Deterministic-key encryption output is byte-identical.
- Query model and SQL rendering, name maps, OLE compound documents (v3/v4),
  delimited import/export and `exportAll`, cursors (including ranges,
  savepoints and iterables), expression engine (3,515 Java-oracle cases),
  GUID auto-numbers, `DateTimeType`/`ColumnOrder`, IANA time zones with DST,
  the full JDK code-page set (including IBM EBCDIC DBCS), in-memory databases,
  and the Java public-API compatibility surface (`Row`, builders, iterators,
  validators, `OleBlob.Builder`, etc.).

### Verification

- 41 Java-vs-Rust harnesses (byte, text, metadata and behavioural comparisons).
- Byte-identical scenario matrix: V2003 47, V2007 52, V2010 56, V2016 56
  (the skipped scenarios are format-specific: ACE-only complex/calculated
  columns or V2010+-only calculated columns).
- 517 unit tests pass (1 ignored large sweep).
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass.

### Notes / known gaps

- Java-side-only APIs (VBA/`MSysAccessObjects`, deleting relationships, removing
  columns/indexes, JDBC/ODBC remote data) are out of scope; ODBC linked tables
  are metadata-only, as in Jackcess.
- Writing Jet 3 and MSISAM, and the old Office encryption writers, are not
  supported because the reference libraries do not provide them.
- Rust API-shape differences (overload naming, `Result`-returning iterators and
  typed getters, `EvalConfig` not yet consulted by the evaluator) are documented
  in README.md.
- OLE v4 has no Windows-authored sample (validated against POI-generated
  fixtures).
