//! Port of `com.healthmarketscience.jackcess.crypt.impl.DbFileRewriter`.
//!
//! Adding or removing encryption touches every page and cannot be undone half
//! way, so the work always builds a separate file and the original is replaced
//! only once that file is complete.

use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Port of `DbFileRewriter`.
pub struct DbFileRewriter;

impl DbFileRewriter {
    /// `DbFileRewriter.rewriteTo`: rewrites the given database into the given
    /// destination, which must not be the source and is overwritten if it
    /// exists.
    pub fn rewrite_to<F>(src_file: &Path, dst_file: &Path, op: F) -> Result<()>
    where
        F: FnOnce(&Path, &Path) -> Result<()>,
    {
        let dst_file = if dst_file.exists() {
            // a symbolic link is followed to the database it names, the same
            // as an in place rewrite does
            let real = std::fs::canonicalize(dst_file)?;
            if is_same_file(src_file, &real) {
                return Err(Error::IllegalArgument(
                    "Destination is the source database, use the in place form instead".into(),
                ));
            }
            real
        } else {
            dst_file.to_path_buf()
        };
        DbFileRewriter::rewrite(src_file, &dst_file, op)
    }

    /// `DbFileRewriter.rewriteInPlace`: rewrites the given database in place,
    /// replacing it with the result.
    pub fn rewrite_in_place<F>(db_file: &Path, op: F) -> Result<()>
    where
        F: FnOnce(&Path, &Path) -> Result<()>,
    {
        // a symbolic link is followed to the database it names.  Moving over
        // the link itself would replace it with a regular file and leave the
        // database it pointed at holding what the rewrite started with
        let db_file = std::fs::canonicalize(db_file)?;
        DbFileRewriter::rewrite(&db_file, &db_file, op)
    }

    /// Builds the result in a temp file and moves it over the destination,
    /// which is the source itself for a rewrite in place.
    fn rewrite<F>(src_file: &Path, dst_file: &Path, op: F) -> Result<()>
    where
        F: FnOnce(&Path, &Path) -> Result<()>,
    {
        let (tmp_file, file) = create_temp_file(dst_file)?;
        drop(file);
        let result = (|| -> Result<()> {
            op(src_file, &tmp_file)?;
            copy_permissions(src_file, &tmp_file)?;
            force(&tmp_file)?;
            move_over(&tmp_file, dst_file)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_file);
        }
        result
    }
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn create_temp_file(near_file: &Path) -> Result<(PathBuf, File)> {
    let dir = near_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    for attempt in 0..1000u32 {
        let name = format!(
            "jackcess{}-{}-{}.tmp",
            std::process::id(),
            attempt,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Jackcess(format!(
        "Cannot create a temporary file beside {}",
        near_file.display()
    )))
}

/// Gives the new file the permissions of the one it replaces.  The Java code
/// only does this on a file store which supports POSIX attributes; a fresh
/// temp file already inherits the directory's ACL on Windows.
fn copy_permissions(src_file: &Path, dst_file: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(src_file)?.permissions();
        std::fs::set_permissions(dst_file, std::fs::Permissions::from_mode(perms.mode()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (src_file, dst_file);
    }
    Ok(())
}

/// Puts the new file on the disk before it is moved into place.
fn force(file: &Path) -> Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(file)?;
    f.sync_all()?;
    Ok(())
}

fn move_over(tmp_file: &Path, dst_file: &Path) -> Result<()> {
    // `std::fs::rename` replaces an existing destination on both unix and
    // windows (the latter through `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`)
    std::fs::rename(tmp_file, dst_file)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// the page level file work the rewrites share
// ---------------------------------------------------------------------------

/// `DbFileRewriter.readPage`: fills the given buffer with the given page, and
/// fails if the file ends first.
pub fn read_page(
    channel: &mut File,
    page: &mut [u8],
    page_number: i32,
    page_size: usize,
) -> Result<()> {
    let page = &mut page[..page_size];
    let offset = page_number as u64 * page_size as u64;
    let mut bytes_read = 0usize;
    while bytes_read < page_size {
        channel.seek(SeekFrom::Start(offset + bytes_read as u64))?;
        let read = channel.read(&mut page[bytes_read..])?;
        if read == 0 {
            return Err(Error::Jackcess(format!(
                "Failed to read page {}, got {} of {} bytes",
                page_number, bytes_read, page_size
            )));
        }
        bytes_read += read;
    }
    Ok(())
}

/// `DbFileRewriter.writePage`: writes the given buffer as the given page.
pub fn write_page(
    channel: &mut File,
    page: &[u8],
    page_number: i32,
    page_size: usize,
) -> Result<()> {
    let page = &page[..page_size];
    let offset = page_number as u64 * page_size as u64;
    let mut bytes_written = 0usize;
    while bytes_written < page_size {
        channel.seek(SeekFrom::Start(offset + bytes_written as u64))?;
        let written = channel.write(&page[bytes_written..])?;
        if written == 0 {
            return Err(Error::Jackcess(format!(
                "Failed to write page {}, wrote {} of {} bytes",
                page_number, bytes_written, page_size
            )));
        }
        bytes_written += written;
    }
    Ok(())
}

/// `DbFileRewriter.countPages`: how many whole pages the given database holds.
pub fn count_pages(db_file: &Path, page_size: usize) -> Result<usize> {
    let size = std::fs::metadata(db_file)?.len();
    if size % page_size as u64 != 0 {
        return Err(Error::Jackcess(format!(
            "Database length {} is not a whole number of {} byte pages",
            size, page_size
        )));
    }
    Ok((size / page_size as u64) as usize)
}

/// Reads `JetFormat.getFormat` from the first bytes of a file, like the
/// `FileChannel` based Java helper the crypt handlers use.
pub fn read_file_format(file: &mut File) -> Result<crate::format::JetFormat> {
    let mut header = [0u8; 21];
    file.seek(SeekFrom::Start(0))?;
    let mut read = 0usize;
    while read < header.len() {
        let n = file.read(&mut header[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    crate::format::JetFormat::get_format(&header[..read])
}

#[cfg(test)]
mod tests {
    //! `DbFileRewriter` behaviour: the destination is replaced only once the
    //! rewrite has completed, the source is never written, and a failed
    //! rewrite leaves no temporary file behind.

    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rewrite_to_rejects_the_source_as_destination() {
        let dir = scratch_dir("rewriter_same");
        let file = dir.join("db.mdb");
        std::fs::write(&file, b"contents").unwrap();
        let err = DbFileRewriter::rewrite_to(&file, &file, |_, _| Ok(())).unwrap_err();
        assert!(
            format!("{}", err).contains("Destination is the source database"),
            "unexpected error: {}",
            err
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"contents");
    }

    #[test]
    fn a_failed_rewrite_leaves_the_destination_and_no_temp_file() {
        let dir = scratch_dir("rewriter_failed");
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(&dst, b"before").unwrap();

        let err =
            DbFileRewriter::rewrite_to(&src, &dst, |_, _| Err(Error::Jackcess("boom".into())))
                .unwrap_err();
        assert!(format!("{}", err).contains("boom"));
        // the destination is untouched and the temporary file is gone
        assert_eq!(std::fs::read(&dst).unwrap(), b"before");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("jackcess"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {:?}", leftovers);
        assert_eq!(std::fs::read(&src).unwrap(), b"source");
    }

    #[test]
    fn a_rewrite_replaces_the_destination_with_the_new_contents() {
        let dir = scratch_dir("rewriter_replace");
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(&dst, b"before").unwrap();

        DbFileRewriter::rewrite_to(&src, &dst, |src, dst| {
            let mut contents = std::fs::read(src)?;
            contents.extend_from_slice(b"!");
            std::fs::write(dst, &contents)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"source!");
        assert_eq!(std::fs::read(&src).unwrap(), b"source");
    }

    #[test]
    fn rewrite_in_place_replaces_the_file() {
        let dir = scratch_dir("rewriter_in_place");
        let file = dir.join("db.mdb");
        std::fs::write(&file, b"abc").unwrap();
        DbFileRewriter::rewrite_in_place(&file, |src, dst| {
            let mut contents = std::fs::read(src)?;
            contents.reverse();
            std::fs::write(dst, &contents)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"cba");
    }

    #[test]
    fn count_pages_requires_whole_pages() {
        let dir = scratch_dir("rewriter_pages");
        let file = dir.join("db.bin");
        std::fs::write(&file, vec![0u8; 4096 * 3]).unwrap();
        assert_eq!(count_pages(&file, 4096).unwrap(), 3);
        std::fs::write(&file, vec![0u8; 4096 * 2 + 7]).unwrap();
        let err = count_pages(&file, 4096).unwrap_err();
        assert!(
            format!("{}", err).contains("is not a whole number of 4096 byte pages"),
            "unexpected error: {}",
            err
        );
    }
}
