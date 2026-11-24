//! `guidauto` - behavioural verification of the GUID auto-number columns.
//!
//! A GUID auto-number value is random, so two writers can never produce the
//! same file.  This harness therefore checks the observable behaviour instead:
//! a generated value is a fresh version 4 / RFC 4122 variant UUID, values are
//! unique, explicit insertion follows `Database::set_allow_auto_number_insert`
//! (and the per-table override) exactly like `TableImpl
//! .handleAutoNumbersForAdd`/`handleAutoNumbersForUpdate`, bad explicit values
//! are rejected like `ColumnImpl.writeGUIDValue`, and the values survive a
//! close/reopen.
//!
//! Java allows at most one auto number column of a given type per table
//! (`TableMutator.validateAutoNumberColumn`), so the GUID primary-key table
//! (`G`) and the GUID non-key auto-number table (`N`) are separate.
//!
//! Usage: guidauto [out.mdb] [format]
//!
//! It exits non-zero when any check fails, so `tools/verify-guid.ps1` can run it
//! next to the Java oracle (`java-src/GuidAutoGen.java`).

use std::collections::HashSet;
use std::path::Path;

use jackcess::bytebuf::ByteOrder;
use jackcess::column::{self, ColumnSpec};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

const G: &str = "G";
const G_ID: usize = 0;
const G_REF: usize = 1;
const G_PLAIN: usize = 2;
const G_LABEL: usize = 3;

const N: &str = "N";
const N_NUM: usize = 0;
const N_LABEL: usize = 1;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "out/guidauto.mdb".to_string());
    let format = args
        .get(2)
        .and_then(|name| FileFormat::from_name(name))
        .unwrap_or(FileFormat::V2003);
    match run(Path::new(&out), format) {
        Ok(()) => {
            println!(
                "GUID AUTO-NUMBER RUST HARNESS: ALL CHECKS PASSED (format={:?})",
                format
            );
        }
        Err(e) => {
            eprintln!("GUID AUTO-NUMBER RUST HARNESS FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn fail(msg: impl Into<String>) -> Error {
    Error::Jackcess(msg.into())
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            return Err(fail(format!($($arg)*)));
        }
    };
}

/// The GUID primary-key table: one GUID auto number (the key) and two plain
/// GUID columns (one named `Ref`, covering a GUID non-key column).
fn guid_spec() -> TableSpec {
    TableSpec::new(
        G,
        vec![
            ColumnSpec::new("ID", DataType::Guid).with_auto_number(true),
            ColumnSpec::new("Ref", DataType::Guid),
            ColumnSpec::new("Plain", DataType::Guid),
            ColumnSpec::new("Label", DataType::Text).with_length(50),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
}

/// A GUID auto number in a non-key, un-indexed column.
fn nokey_spec() -> TableSpec {
    TableSpec::new(
        N,
        vec![
            ColumnSpec::new("Num", DataType::Guid).with_auto_number(true),
            ColumnSpec::new("Label", DataType::Text).with_length(50),
        ],
    )
}

fn grow(id: Value, reference: Value, plain: Value, label: &str) -> Vec<Value> {
    vec![id, reference, plain, Value::Text(label.to_string())]
}

fn nrow(num: Value, label: &str) -> Vec<Value> {
    vec![num, Value::Text(label.to_string())]
}

/// The stored bytes of a canonical GUID literal (the Jet4/ACE page layout).
fn stored(literal: &str) -> [u8; 16] {
    let canonical = column::parse_guid(literal).expect("a canonical GUID literal");
    column::guid_stored_bytes(&canonical, ByteOrder::LittleEndian)
}

fn raw_as_uuid(value: &Value, what: &str) -> Result<[u8; 16]> {
    match value {
        Value::Uuid(bytes) => Ok(*bytes),
        other => Err(fail(format!("{what} is not a GUID value: {other:?}"))),
    }
}

/// (version, variant) of the canonical form of a stored GUID value.
fn version_variant(raw: &[u8; 16]) -> (u8, u8) {
    let canonical = column::guid_canonical_bytes(raw, ByteOrder::LittleEndian);
    (canonical[6] >> 4, (canonical[8] >> 6) & 0x03)
}

fn require_v4(raw: &[u8; 16], what: &str) -> Result<()> {
    let (version, variant) = version_variant(raw);
    check!(
        version == 4 && variant == 2,
        "{what} is not version 4 / RFC 4122 variant: version={version} variant={variant}"
    );
    Ok(())
}

fn canonical_text(raw: &[u8; 16]) -> String {
    let canonical = column::guid_canonical_bytes(raw, ByteOrder::LittleEndian);
    format!(
        "{{{}-{}-{}-{}-{}}}",
        hex(&canonical[0..4]),
        hex(&canonical[4..6]),
        hex(&canonical[6..8]),
        hex(&canonical[8..10]),
        hex(&canonical[10..16])
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Reads the live rows of a table as `(row_id, values)`.
fn read_rows(db: &mut Database, table: &str) -> Result<Vec<(jackcess::table::RowId, Vec<Value>)>> {
    Ok(db
        .read_rows_with_ids(table)?
        .into_iter()
        .filter_map(|(row_id, values)| values.map(|values| (row_id, values)))
        .collect())
}

fn find_label(
    db: &mut Database,
    table: &str,
    label_index: usize,
    label: &str,
) -> Result<(jackcess::table::RowId, Vec<Value>)> {
    read_rows(db, table)?
        .into_iter()
        .find(|(_, values)| {
            matches!(values.get(label_index), Some(Value::Text(text)) if text == label)
        })
        .ok_or_else(|| fail(format!("row {table}/{label} not found")))
}

fn row_count(db: &mut Database, table: &str) -> Result<usize> {
    Ok(read_rows(db, table)?.len())
}

fn run(path: &Path, format: FileFormat) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(format, path)?;
    db.now_ms = Some(1_700_000_000_000);
    db.create_table(&guid_spec())?;
    db.create_table(&nokey_spec())?;

    // the created column definitions carry the GUID auto number flag
    {
        let table = db.get_table(G)?;
        check!(
            table.columns[G_ID].auto_number && table.columns[G_ID].data_type == DataType::Guid,
            "ID is not a GUID auto number column"
        );
        check!(
            !table.columns[G_REF].auto_number && !table.columns[G_PLAIN].auto_number,
            "Ref/Plain must not be auto number columns"
        );
    }
    {
        let table = db.get_table(N)?;
        check!(
            table.columns[N_NUM].auto_number && table.columns[N_NUM].data_type == DataType::Guid,
            "Num is not a GUID auto number column"
        );
    }

    // ------------------------------------------------------------------
    // generated values (null / absent input) in the GUID primary-key table
    // ------------------------------------------------------------------
    let mut generated_rows = Vec::new();
    for i in 0..6 {
        generated_rows.push(grow(
            Value::Null,
            // a plain GUID is written as given, whatever the auto number policy
            Value::Text("{12345678-90AB-CDEF-1234-567890ABCDEF}".to_string()),
            if i == 5 {
                Value::Text("{A1B2C3D4-E5F6-7890-ABCD-EF0123456789}".to_string())
            } else {
                Value::Null
            },
            &format!("gen{i}"),
        ));
    }
    db.add_rows(G, &generated_rows)?;

    let rows = read_rows(&mut db, G)?;
    check!(
        rows.len() == 6,
        "expected 6 generated rows, got {}",
        rows.len()
    );
    let mut ids = HashSet::new();
    for (_, values) in &rows {
        let id = raw_as_uuid(&values[G_ID], "generated ID")?;
        require_v4(&id, "generated ID")?;
        check!(ids.insert(id), "generated ID value is not unique");
        check!(
            raw_as_uuid(&values[G_REF], "plain Ref")?
                == stored("12345678-90AB-CDEF-1234-567890ABCDEF"),
            "the plain GUID column did not keep the value it was given"
        );
    }
    let plain_null = rows
        .iter()
        .filter(|(_, values)| values[G_PLAIN].is_null())
        .count();
    check!(
        plain_null == 5,
        "expected 5 null Plain values, got {plain_null}"
    );
    println!(
        "[rust] format={:?} generated rows={} ids={} v4=true variant=true unique=true",
        format,
        rows.len(),
        ids.len()
    );

    // the same for the GUID auto number in an un-indexed, non-key column
    db.add_rows(
        N,
        &[
            nrow(Value::Null, "n0"),
            nrow(Value::Null, "n1"),
            nrow(Value::Null, "n2"),
        ],
    )?;
    let nrows = read_rows(&mut db, N)?;
    check!(
        nrows.len() == 3,
        "expected 3 NoKey rows, got {}",
        nrows.len()
    );
    let mut nums = HashSet::new();
    for (_, values) in &nrows {
        let num = raw_as_uuid(&values[N_NUM], "generated Num")?;
        require_v4(&num, "generated Num")?;
        check!(nums.insert(num), "generated Num value is not unique");
    }
    println!(
        "[rust] nonkey_generation rows={} v4=true unique=true",
        nrows.len()
    );

    // ------------------------------------------------------------------
    // read back after close / reopen
    // ------------------------------------------------------------------
    let before: Vec<([u8; 16], [u8; 16])> = rows
        .iter()
        .map(|(_, values)| {
            (
                raw_as_uuid(&values[G_ID], "ID").unwrap(),
                raw_as_uuid(&values[G_REF], "Ref").unwrap(),
            )
        })
        .collect();
    let nums_before: Vec<[u8; 16]> = nrows
        .iter()
        .map(|(_, values)| raw_as_uuid(&values[N_NUM], "Num").unwrap())
        .collect();
    db.flush()?;
    drop(db);
    let mut db = Database::open_path(path)?;
    let reopened = read_rows(&mut db, G)?;
    check!(
        reopened.len() == before.len(),
        "reopen changed the row count: {} != {}",
        reopened.len(),
        before.len()
    );
    for ((_, values), (id, reference)) in reopened.iter().zip(before.iter()) {
        check!(
            raw_as_uuid(&values[G_ID], "reopened ID")? == *id,
            "reopened ID differs from the value read before the close"
        );
        check!(
            raw_as_uuid(&values[G_REF], "reopened Ref")? == *reference,
            "reopened Ref differs from the value read before the close"
        );
    }
    let nreopened = read_rows(&mut db, N)?;
    check!(
        nreopened.len() == nums_before.len(),
        "reopen changed the NoKey row count"
    );
    for ((_, values), num) in nreopened.iter().zip(nums_before.iter()) {
        check!(
            raw_as_uuid(&values[N_NUM], "reopened Num")? == *num,
            "reopened Num differs from the value read before the close"
        );
    }
    // the auto number flags survive the reopen
    check!(
        db.get_table(G)?.columns[G_ID].auto_number,
        "the GUID auto number flag was lost on reopen"
    );
    // the policy itself is not stored in the file: jackcess defaults to false
    check!(
        !db.is_allow_auto_number_insert(),
        "the auto number insert policy must default to false after reopen"
    );
    println!(
        "[rust] reopen rows={} equal=true flags=true policy_default_off=true",
        reopened.len()
    );

    // ------------------------------------------------------------------
    // policy off: an explicit GUID is ignored and a fresh one is generated
    // ------------------------------------------------------------------
    let explicit_off = "DEADBEEF-DEAD-BEEF-DEAD-BEEFDEADBEEF";
    let off_stored = stored(explicit_off);
    db.add_rows(
        G,
        &[grow(
            Value::Text(explicit_off.to_string()),
            Value::Text(explicit_off.to_string()),
            Value::Null,
            "policy-off",
        )],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "policy-off")?;
    let off_id = raw_as_uuid(&values[G_ID], "policy-off ID")?;
    require_v4(&off_id, "policy-off ID")?;
    check!(
        off_id != off_stored,
        "policy off must replace the explicit GUID with a generated one"
    );
    check!(
        raw_as_uuid(&values[G_REF], "policy-off Ref")? == off_stored,
        "the policy must not affect a plain GUID column"
    );
    println!("[rust] policy_off explicit_ignored=true generated_v4=true plain_kept=true");

    // ------------------------------------------------------------------
    // policy on: explicit values are used (string, raw bytes, null)
    // ------------------------------------------------------------------
    db.set_allow_auto_number_insert(true);
    check!(
        db.is_allow_auto_number_insert(),
        "the policy did not turn on"
    );
    let string_guid = "00112233-4455-6677-8899-AABBCCDDEEFF";
    let uuid_guid = "ABCDEF01-2345-6789-ABCD-EF0123456789";
    let lowercase_guid = "aabbccdd-eeff-0011-2233-445566778899";
    let uuid_stored = stored(uuid_guid);
    db.add_rows(
        G,
        &[
            grow(
                Value::Text(format!("{{{string_guid}}}")),
                Value::Text(uuid_guid.to_string()),
                Value::Text(format!("{{{lowercase_guid}}}")),
                "policy-on",
            ),
            grow(Value::Null, Value::Null, Value::Null, "policy-on-null"),
            grow(
                Value::Text(uuid_guid.to_lowercase()),
                Value::Uuid(uuid_stored),
                Value::Null,
                "policy-on-raw",
            ),
        ],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "policy-on")?;
    check!(
        raw_as_uuid(&values[G_ID], "policy-on ID")? == stored(string_guid),
        "the explicit braced GUID was not stored"
    );
    check!(
        raw_as_uuid(&values[G_REF], "policy-on Ref")? == stored(uuid_guid),
        "the explicit unbraced GUID was not stored"
    );
    check!(
        raw_as_uuid(&values[G_PLAIN], "policy-on Plain")? == stored(lowercase_guid),
        "the explicit lowercase GUID was not stored"
    );
    let (_, values) = find_label(&mut db, G, G_LABEL, "policy-on-null")?;
    let null_id = raw_as_uuid(&values[G_ID], "policy-on-null ID")?;
    require_v4(&null_id, "policy-on-null ID")?;
    check!(
        null_id != stored(string_guid),
        "an explicit null must generate a fresh GUID"
    );
    let (_, values) = find_label(&mut db, G, G_LABEL, "policy-on-raw")?;
    check!(
        raw_as_uuid(&values[G_ID], "policy-on-raw ID")? == stored(uuid_guid),
        "the explicit lower case GUID was not stored"
    );
    // the non-key GUID auto number follows the same policy
    db.add_rows(N, &[nrow(Value::Text(uuid_guid.to_string()), "n-explicit")])?;
    let (_, values) = find_label(&mut db, N, N_LABEL, "n-explicit")?;
    check!(
        raw_as_uuid(&values[N_NUM], "n-explicit Num")? == uuid_stored,
        "the non-key GUID auto number did not accept the explicit value"
    );
    println!(
        "[rust] policy_on string_used=true raw_used=true null_generated=true nonkey_used=true"
    );

    // ------------------------------------------------------------------
    // per-table override
    // ------------------------------------------------------------------
    let override_guid = "11111111-2222-3333-4444-555555555555";
    db.set_allow_auto_number_insert(false);
    db.set_table_allow_auto_number_insert(G, Some(true))?;
    db.add_rows(
        G,
        &[grow(
            Value::Text(override_guid.to_string()),
            Value::Null,
            Value::Null,
            "table-on",
        )],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "table-on")?;
    check!(
        raw_as_uuid(&values[G_ID], "table-on ID")? == stored(override_guid),
        "the table override did not enable the explicit insert"
    );

    let ignored_guid = "22222222-3333-4444-5555-666666666666";
    db.set_allow_auto_number_insert(true);
    db.set_table_allow_auto_number_insert(G, Some(false))?;
    db.add_rows(
        G,
        &[grow(
            Value::Text(ignored_guid.to_string()),
            Value::Null,
            Value::Null,
            "table-off",
        )],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "table-off")?;
    let ignored_id = raw_as_uuid(&values[G_ID], "table-off ID")?;
    check!(
        ignored_id != stored(ignored_guid),
        "the table override did not disable the explicit insert"
    );
    require_v4(&ignored_id, "table-off ID")?;

    let restored_guid = "33333333-4444-5555-6666-777777777777";
    db.set_table_allow_auto_number_insert(G, None)?;
    db.add_rows(
        G,
        &[grow(
            Value::Text(restored_guid.to_string()),
            Value::Null,
            Value::Null,
            "table-default",
        )],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "table-default")?;
    check!(
        raw_as_uuid(&values[G_ID], "table-default ID")? == stored(restored_guid),
        "removing the table override did not restore the database policy"
    );
    println!("[rust] table_override on_used=true off_ignored=true fallback_used=true");

    // ------------------------------------------------------------------
    // rejected explicit values (policy on)
    // ------------------------------------------------------------------
    db.set_allow_auto_number_insert(true);
    let count_before = row_count(&mut db, G)?;
    for (bad, expected_text) in [
        (Value::Text("not-a-guid".to_string()), "not-a-guid"),
        (Value::Long(123), "123"),
        // java's GUID_PATTERN demands the hyphens in the canonical positions
        (
            Value::Text("00112233445566778899AABBCCDDEEFF".to_string()),
            "00112233445566778899AABBCCDDEEFF",
        ),
    ] {
        let err = db
            .add_rows(G, &[grow(bad, Value::Null, Value::Null, "rejected")])
            .expect_err("a bad explicit GUID must be rejected");
        let message = err.to_string();
        check!(
            message.contains(&format!("Invalid GUID: {expected_text}")),
            "bad explicit GUID was not reported as an invalid GUID: {message}"
        );
        check!(
            message.contains(&format!("Table={G}"))
                && message.contains("Column=ID")
                && message.contains("(Db="),
            "the invalid GUID error lacks the column context: {message}"
        );
        check!(
            row_count(&mut db, G)? == count_before,
            "a rejected explicit GUID changed the row count"
        );
    }
    // a rejected explicit value consumes nothing: the next generated value works
    db.add_rows(
        G,
        &[grow(Value::Null, Value::Null, Value::Null, "after-reject")],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, "after-reject")?;
    require_v4(
        &raw_as_uuid(&values[G_ID], "after-reject ID")?,
        "after-reject ID",
    )?;
    check!(
        row_count(&mut db, G)? == count_before + 1,
        "the generated row after a rejection was not written"
    );
    println!("[rust] reject invalid_message=true context=true rows_unchanged=true next_valid=true");

    // ------------------------------------------------------------------
    // update semantics: an auto number is not regenerated, KEEP_VALUE keeps
    // the value, and an explicit update needs the policy
    // ------------------------------------------------------------------
    let keep_label = "policy-on";
    let (row_id, _) = find_label(&mut db, G, G_LABEL, keep_label)?;
    let old_id = stored(string_guid);

    // policy off: an explicit GUID is ignored on update as well
    db.set_allow_auto_number_insert(false);
    let update_ignored = "44444444-5555-6666-7777-888888888888";
    db.update_row(
        G,
        row_id,
        &[
            Value::Text(update_ignored.to_string()),
            Value::Keep,
            Value::Keep,
            Value::Keep,
        ],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "updated ID (policy off)")? == old_id,
        "an update regenerated or replaced a GUID auto number with the policy off"
    );
    // every column kept: the auto number is the value the row already holds
    db.update_row(
        G,
        row_id,
        &[Value::Keep, Value::Keep, Value::Keep, Value::Keep],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "kept ID")? == old_id,
        "KEEP_VALUE changed the GUID auto number"
    );

    // policy on: an explicit GUID replaces the old value
    db.set_allow_auto_number_insert(true);
    let update_used = "55555555-6666-7777-8888-999999999999";
    db.update_row(
        G,
        row_id,
        &[
            Value::Text(update_used.to_string()),
            Value::Keep,
            Value::Keep,
            Value::Keep,
        ],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "updated ID (policy on)")? == stored(update_used),
        "an explicit GUID update with the policy on was not stored"
    );
    let new_id = stored(update_used);

    // KEEP_VALUE and an explicit null keep the current value even with the
    // policy on (neither regenerates it)
    db.update_row(
        G,
        row_id,
        &[Value::Keep, Value::Keep, Value::Keep, Value::Keep],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "kept ID (policy on)")? == new_id,
        "KEEP_VALUE regenerated the GUID auto number"
    );
    db.update_row(
        G,
        row_id,
        &[Value::Null, Value::Keep, Value::Keep, Value::Keep],
    )?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "null ID (policy on)")? == new_id,
        "an explicit null on update must keep the GUID auto number"
    );

    // a bad explicit update is rejected and leaves the stored value alone
    let err = db
        .update_row(
            G,
            row_id,
            &[
                Value::Text("bad-update".to_string()),
                Value::Keep,
                Value::Keep,
                Value::Keep,
            ],
        )
        .expect_err("a bad explicit GUID update must be rejected");
    check!(
        err.to_string().contains("Invalid GUID: bad-update"),
        "the rejected update did not report the bad GUID: {err}"
    );
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "ID after a rejected update")? == new_id,
        "a rejected explicit update changed the GUID auto number"
    );

    // the non-key GUID auto number has the same update semantics
    let (n_row_id, _) = find_label(&mut db, N, N_LABEL, "n0")?;
    let (_, values) = find_label(&mut db, N, N_LABEL, "n0")?;
    let n_old = raw_as_uuid(&values[N_NUM], "n0 Num")?;
    db.set_allow_auto_number_insert(false);
    db.update_row(
        N,
        n_row_id,
        &[Value::Text(update_ignored.to_string()), Value::Keep],
    )?;
    let (_, values) = find_label(&mut db, N, N_LABEL, "n0")?;
    check!(
        raw_as_uuid(&values[N_NUM], "n0 Num (policy off)")? == n_old,
        "the non-key GUID auto number changed with the policy off"
    );
    db.set_allow_auto_number_insert(true);
    let n_update = "66666666-7777-8888-9999-AAAAAAAAAAAA";
    db.update_row(
        N,
        n_row_id,
        &[Value::Text(n_update.to_string()), Value::Keep],
    )?;
    let (_, values) = find_label(&mut db, N, N_LABEL, "n0")?;
    check!(
        raw_as_uuid(&values[N_NUM], "n0 Num (policy on)")? == stored(n_update),
        "the non-key GUID auto number did not accept the explicit update"
    );
    println!(
        "[rust] update keep_off=true keep_on=true explicit_on=true explicit_off_ignored=true null_keeps=true invalid_rejected=true nonkey=true"
    );

    // ------------------------------------------------------------------
    // the updated values survive a second close / reopen
    // ------------------------------------------------------------------
    db.flush()?;
    drop(db);
    let mut db = Database::open_path(path)?;
    let (_, values) = find_label(&mut db, G, G_LABEL, keep_label)?;
    check!(
        raw_as_uuid(&values[G_ID], "reopened updated ID")? == new_id,
        "the updated GUID auto number did not survive the reopen"
    );
    let (_, values) = find_label(&mut db, N, N_LABEL, "n0")?;
    check!(
        raw_as_uuid(&values[N_NUM], "reopened updated Num")? == stored(n_update),
        "the updated non-key GUID auto number did not survive the reopen"
    );
    check!(
        row_count(&mut db, G)? == count_before + 1,
        "unexpected row count after the reopen"
    );
    println!(
        "[rust] final_reopen updated_value=true rows={}",
        row_count(&mut db, G)?
    );

    // a final value dump is useful evidence when a later failure needs context
    let rows = read_rows(&mut db, G)?;
    let mut sample: Vec<String> = rows
        .iter()
        .take(3)
        .map(|(_, values)| canonical_text(&raw_as_uuid(&values[G_ID], "sample ID").unwrap()))
        .collect();
    sample.sort();
    println!("[rust] sample_ids={}", sample.join(","));
    Ok(())
}
