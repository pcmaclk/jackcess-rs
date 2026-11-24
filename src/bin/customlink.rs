//! Rust side of the custom-link-resolver scenario.
//!
//! Mirrors `java-src/CustomLinkGen.java`:
//!
//! * creates `cl_target.<ext>` and `cl_loader_target.<ext>` in the output
//!   directory, plus `cl_linker.<ext>` with links which store a relative, an
//!   absolute, an upper-case, a missing, a loader and two custom file names;
//! * moves both targets to `moved/` so the stored names no longer resolve;
//! * reads through the link with the default resolver (denied), with
//!   [`CustomLinkResolver::from_map_with_base_dir`] (relative value, absolute
//!   key/value, different-case name), with
//!   [`CustomLinkResolver::from_loader`] (loader callback) and with
//!   [`CustomLinkResolver::from_base`] (Java's abstract
//!   `CustomLinkResolver` loader form, in-memory temporary database);
//! * prints the same deterministic `DUMP ...` lines as the Java program and
//!   the linker MD5 before/after (`CHECK ...`), proving the linker was not
//!   modified.
//!
//! Usage: `customlink <outDir> <V2003|V2007|V2010|V2016>`

use std::collections::BTreeMap;
fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}
use std::path::{Path, PathBuf};

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::linked::{self, CustomLinkResolver, CustomLinkResolverBase};
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;
use md5::{Digest, Md5};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: customlink <outDir> <V2003|V2007|V2010|V2016>");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let format = match FileFormat::from_name(&args[2]) {
        Some(format) => format,
        None => {
            eprintln!("unknown file format '{}'", args[2]);
            std::process::exit(2);
        }
    };
    if let Err(error) = run(&dir, format) {
        eprintln!("error: {}", error);
        std::process::exit(1);
    }
}

fn source_spec(table_name: &str) -> TableSpec {
    TableSpec::new(
        table_name,
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Num", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
}

fn create_source_database(
    format: FileFormat,
    path: &Path,
    table_name: &str,
    rows: &[(i32, &str, i32)],
) -> Result<()> {
    let mut db = Database::create(format, path)?;
    db.create_table(&source_spec(table_name))?;
    for (id, name, num) in rows {
        db.add_rows(
            table_name,
            &[vec![
                Value::Long(*id),
                Value::Text((*name).to_string()),
                Value::Long(*num),
            ]],
        )?;
    }
    db.flush()?;
    Ok(())
}

fn run(dir: &Path, format: FileFormat) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;

    let ext = format.extension();
    let target_rel = format!("cl_target{}", ext);
    let loader_rel = format!("cl_loader_target{}", ext);
    let target_path = dir.join(&target_rel);
    let loader_path = dir.join(&loader_rel);
    let linker_path = dir.join(format!("cl_linker{}", ext));

    create_source_database(
        format,
        &target_path,
        "Source",
        &[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)],
    )?;
    create_source_database(
        format,
        &loader_path,
        "Source",
        &[(7, "delta", 70), (8, "epsilon", 80)],
    )?;

    // ---- linker: relative, absolute, different-case, missing, loader ----
    let absolute_target_name = target_path.to_string_lossy().into_owned();
    {
        let mut linker = Database::create(format, &linker_path)?;
        linker.create_linked_table("RelLink", &target_rel, "Source")?;
        linker.create_linked_table("AbsLink", &absolute_target_name, "Source")?;
        linker.create_linked_table("CaseLink", &target_rel.to_ascii_uppercase(), "Source")?;
        linker.create_linked_table("MissingLink", &format!("cl_missing{}", ext), "Source")?;
        linker.create_linked_table("LoaderLink", &loader_rel, "Source")?;
        linker.create_linked_table("CustomLink", "custom_source_1.txt", "Custom1")?;
        linker.create_linked_table("CustomMissingTable", "custom_source_2.txt", "NoSuchTable")?;
        linker.flush()?;
    }

    // ---- move the targets: both stored names now point at nothing ------
    let moved_dir = dir.join("moved");
    std::fs::create_dir_all(&moved_dir)?;
    let moved_target = moved_dir.join(target_path.file_name().unwrap());
    let moved_loader = moved_dir.join(loader_path.file_name().unwrap());
    std::fs::rename(&target_path, &moved_target)?;
    std::fs::rename(&loader_path, &moved_loader)?;
    if target_path.exists() || loader_path.exists() {
        return Err(Error::IllegalState("targets were not moved".to_string()));
    }

    let md5_before = md5_file(&linker_path)?;

    // ---- 1. the default resolver denies every link ---------------------
    {
        let mut db = Database::open_path_read_only(&linker_path)?;
        match db.get_table("RelLink") {
            Err(error) => println!("DUMP DEFAULT_ERROR {}", error),
            Ok(_) => {
                return Err(Error::IllegalState(
                    "default resolver resolved RelLink".to_string(),
                ))
            }
        }
    }

    // ---- 1b. the moved target no longer resolves from the stored names --
    // (the unrestricted resolver uses the stored path, which the move broke)
    {
        let mut db = Database::open_path_read_only(&linker_path)?;
        db.set_link_resolver(linked::UNRESTRICTED);
        if db.get_table("RelLink").is_ok() {
            return Err(Error::IllegalState(
                "unrestricted resolver found the moved relative target".to_string(),
            ));
        }
        println!("DUMP UNRESTRICTED_MISSING_REL true");
        if db.get_table("AbsLink").is_ok() {
            return Err(Error::IllegalState(
                "unrestricted resolver found the moved absolute target".to_string(),
            ));
        }
        println!("DUMP UNRESTRICTED_MISSING_ABS true");
    }

    // ---- 2. map resolver: relative, absolute and different-case names ---
    let mut mapping: BTreeMap<String, PathBuf> = BTreeMap::new();
    // relative value, resolved against the output directory by the resolver
    mapping.insert(
        target_rel.to_ascii_lowercase(),
        PathBuf::from("moved").join(&target_rel),
    );
    // absolute key (the stored pre-move name) and absolute value
    mapping.insert(absolute_target_name, moved_target);
    let map_resolver =
        CustomLinkResolver::from_map_with_base_dir(dir.to_path_buf(), mapping.clone());
    {
        let mut db = Database::open_path_read_only(&linker_path)?;
        db.set_link_resolver(map_resolver);
        dump(&mut db, "RelLink", "REL")?;
        dump(&mut db, "AbsLink", "ABS")?;
        dump(&mut db, "CaseLink", "CASE")?;
    }

    // ---- 3. loader callback form ---------------------------------------
    let loader_rel_for_closure = loader_rel.clone();
    let moved_loader_for_closure = moved_loader.clone();
    let loader_resolver = CustomLinkResolver::from_loader(move |_linker, name| {
        if name.eq_ignore_ascii_case(&loader_rel_for_closure) {
            Ok(Some(moved_loader_for_closure.clone()))
        } else {
            Ok(None)
        }
    });
    {
        let mut db = Database::open_path_read_only(&linker_path)?;
        db.set_link_resolver(loader_resolver);
        dump(&mut db, "LoaderLink", "LOADER")?;
    }

    // ---- 4. a missing map entry uses the same denial message -----------
    {
        let mut db = Database::open_path_read_only(&linker_path)?;
        db.set_link_resolver(CustomLinkResolver::from_map_with_base_dir(
            dir.to_path_buf(),
            mapping,
        ));
        match db.get_table("MissingLink") {
            Err(error) => println!("DUMP MISSING_ERROR {}", error),
            Ok(_) => {
                return Err(Error::IllegalState(
                    "map resolver resolved MissingLink".to_string(),
                ))
            }
        }
    }

    // ---- 5. the abstract CustomLinkResolver loader form ----------------
    let custom_temp_dir = dir.join("custom_temp");
    std::fs::create_dir_all(&custom_temp_dir)?;
    {
        // read-write linker: the temporary database must be writable while
        // its tables are loaded
        let mut db = Database::open_path(&linker_path)?;
        db.set_link_resolver(CustomLinkResolver::from_base(TestCustomResolver {
            temp_dir: custom_temp_dir,
        }));
        dump(&mut db, "CustomLink", "CUSTOM")?;
        let missing = db.find_table("CustomMissingTable")?;
        println!("DUMP CUSTOM_MISSING {}", missing.is_none());
        match db.get_table("MissingLink") {
            Err(error) => println!("DUMP CUSTOM_FALLBACK_ERROR {}", error),
            Ok(_) => {
                return Err(Error::IllegalState(
                    "custom resolver resolved the non-custom MissingLink".to_string(),
                ))
            }
        }
    }

    // ---- 6. the linker file must not have changed ----------------------
    let md5_after = md5_file(&linker_path)?;
    println!("CHECK linker.md5.before={}", md5_before);
    println!("CHECK linker.md5.after={}", md5_after);
    println!("CHECK linker.unchanged={}", md5_before == md5_after);
    println!("DUMP DONE");
    if md5_before != md5_after {
        return Err(Error::IllegalState(
            "the linker file was modified".to_string(),
        ));
    }
    Ok(())
}

fn dump(db: &mut Database, link_name: &str, label: &str) -> Result<()> {
    let columns: Vec<String> = {
        let table = db.get_table(link_name)?;
        table
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect()
    };
    let rows = db.read_rows_with_ids(link_name)?;
    println!("DUMP {} rows={}", label, rows.len());
    for (index, (_, values)) in rows.into_iter().enumerate() {
        let values = values.ok_or_else(|| {
            Error::IllegalState(format!("a row read through {} is deleted", link_name))
        })?;
        let line = (0..columns.len())
            .map(|i| format_value(values.get(i).unwrap_or(&Value::Null)))
            .collect::<Vec<String>>()
            .join("|");
        println!("DUMP {} row{} {}", label, index, line);
    }
    Ok(())
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "<null>".to_string(),
        Value::Long(value) => value.to_string(),
        Value::Text(value) => value.clone(),
        Value::Bytes(value) => format!("bytes:{}", hex_encode(value)),
        other => format!("{:?}", other),
    }
}

fn md5_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut digest = Md5::new();
    digest.update(&bytes);
    Ok(hex_encode(digest.finalize()))
}

/// The loader form: Java's `CustomLinkResolver` subclass from
/// `CustomLinkGen.TestCustomResolver`.
struct TestCustomResolver {
    temp_dir: PathBuf,
}

impl CustomLinkResolverBase for TestCustomResolver {
    type CustomFile = String;

    fn is_default_in_memory(&self) -> bool {
        true
    }

    fn default_temp_directory(&self) -> Option<PathBuf> {
        Some(self.temp_dir.clone())
    }

    fn load_custom_file(
        &self,
        _linker: &Database,
        linkee_file_name: &str,
    ) -> Result<Option<Self::CustomFile>> {
        if linkee_file_name == "custom_source_1.txt" || linkee_file_name == "custom_source_2.txt" {
            Ok(Some(linkee_file_name.to_string()))
        } else {
            Ok(None)
        }
    }

    fn load_custom_table(
        &self,
        temp_db: &mut Database,
        custom_file: &Self::CustomFile,
        table_name: &str,
    ) -> Result<bool> {
        if table_name != "Custom1" || custom_file != "custom_source_1.txt" {
            return Ok(false);
        }
        temp_db.create_table(&TableSpec::new(
            "Custom1",
            vec![
                ColumnSpec::new("id", DataType::Long),
                ColumnSpec::new("data1", DataType::Text).with_length(50),
            ],
        ))?;
        for i in 0..3i32 {
            temp_db.add_rows(
                "Custom1",
                &[vec![Value::Long(i), Value::Text(format!("row{}", i))]],
            )?;
        }
        Ok(true)
    }
}
