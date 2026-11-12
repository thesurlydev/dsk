//! Bytes of a file that no other file shares.
//!
//! APFS copy-on-write clones (`cp -c`, `clonefile`) share extents until one
//! side is written, so a cloned tree reports its full size in `st_blocks` while
//! deleting it frees only the extents it no longer shares. Tools that seed
//! worktrees with a clone of a warm `target/` directory make this the norm:
//! a worktree can hold 170G of blocks of which only 20G would be freed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

/// Private sizes of the regular files directly inside one directory, by name.
/// A file missing from the map has no reported private size, and every
/// allocated byte of it counts as private.
#[derive(Debug, Default)]
pub struct PrivateSizes(HashMap<OsString, u64>);

impl PrivateSizes {
    /// Read the private sizes of the regular files in `dir`. On APFS this is
    /// one `getattrlistbulk` listing, so it costs about as much as reading the
    /// directory, not a call per file. Elsewhere, or when the listing fails,
    /// the result is empty.
    pub fn read(dir: &Path) -> Self {
        Self(platform::private_sizes(dir).unwrap_or_default())
    }

    /// Bytes deleting the file `name` would free, given its `allocated` bytes.
    pub fn private(&self, name: &std::ffi::OsStr, allocated: u64) -> u64 {
        self.0
            .get(name)
            .map_or(allocated, |private| (*private).min(allocated))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `vtype` of a regular file in `ATTR_CMN_OBJTYPE`.
    const VREG: u32 = 1;
    /// `ATTR_CMNEXT_EXT_FLAGS` bit for a file that may share blocks with
    /// another file, from `<sys/stat.h>`; libc does not export it.
    const EF_MAY_SHARE_BLOCKS: u64 = 0x1;

    /// Closes the directory descriptor on every return path.
    struct Dir(libc::c_int);

    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: the descriptor came from `open` and is closed only here.
            unsafe { libc::close(self.0) };
        }
    }

    pub fn private_sizes(dir: &Path) -> Option<HashMap<OsString, u64>> {
        let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let dir = Dir(fd);

        // SAFETY: a plain C struct for which all-zero is valid.
        let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
        request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        request.commonattr =
            libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE;
        // With FSOPT_ATTR_CMN_EXTENDED, the fork group carries the extended
        // common attributes (ATTR_CMNEXT_*).
        request.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_EXT_FLAGS;
        // FSOPT_PACK_INVAL_ATTRS keeps a slot for every requested attribute
        // even where the filesystem cannot supply it, so each entry has the
        // same layout and the returned set says which values are real.
        let options = u64::from(libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_PACK_INVAL_ATTRS);

        let mut sizes = HashMap::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            // SAFETY: the buffer pointer and length describe `buffer`.
            let count = unsafe {
                libc::getattrlistbulk(
                    dir.0,
                    (&raw mut request).cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    options,
                )
            };
            if count < 0 {
                return None;
            }
            if count == 0 {
                return Some(sizes);
            }
            let mut rest = &buffer[..];
            for _ in 0..count {
                let length = read_u32(rest, 0)? as usize;
                let entry = rest.get(..length)?;
                if let Some((name, private)) = parse(entry) {
                    sizes.insert(name, private);
                }
                rest = rest.get(length..)?;
            }
        }
    }

    /// One packed entry: length, returned set, name reference, object type,
    /// then the private size and extended flags. Every attribute starts on a
    /// 4-byte boundary. Returns the name and private size of a regular file
    /// that may share blocks; a file that cannot is wholly private, and is
    /// left out so its allocation counts.
    ///
    /// The flag matters because APFS reports a private size only for data
    /// already placed on disk: a file written moments ago can read 0 private
    /// bytes while `st_blocks` counts its reservation.
    fn parse(entry: &[u8]) -> Option<(OsString, u64)> {
        // u32 length, then attribute_set_t: the common, volume, directory,
        // file, and fork groups, one u32 each.
        let returned_common = read_u32(entry, 4)?;
        let returned_fork = read_u32(entry, 4 + 4 * 4)?;
        let name_at = 24;
        let objtype_at = name_at + 8;
        let private_at = objtype_at + 4;
        let flags_at = private_at + 8;

        if returned_common & libc::ATTR_CMN_NAME == 0
            || returned_common & libc::ATTR_CMN_OBJTYPE == 0
            || returned_fork & libc::ATTR_CMNEXT_PRIVATESIZE == 0
            || returned_fork & libc::ATTR_CMNEXT_EXT_FLAGS == 0
            || read_u32(entry, objtype_at)? != VREG
        {
            return None;
        }
        let flags = u64::from_ne_bytes(entry.get(flags_at..flags_at + 8)?.try_into().ok()?);
        if flags & EF_MAY_SHARE_BLOCKS == 0 {
            return None;
        }

        // attrreference_t: an offset from the reference itself, and a length
        // that counts the trailing NUL.
        let offset = i32::from_ne_bytes(entry.get(name_at..name_at + 4)?.try_into().ok()?);
        let length = read_u32(entry, name_at + 4)? as usize;
        let start = name_at.checked_add_signed(offset as isize)?;
        let name = entry.get(start..start + length.checked_sub(1)?)?;

        let private = i64::from_ne_bytes(entry.get(private_at..private_at + 8)?.try_into().ok()?);
        Some((
            OsStr::from_bytes(name).to_os_string(),
            u64::try_from(private).ok()?,
        ))
    }

    fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;

    pub fn private_sizes(_dir: &Path) -> Option<HashMap<OsString, u64>> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn allocated(path: &Path) -> u64 {
        fs::metadata(path).unwrap().blocks() * 512
    }

    #[test]
    fn an_unshared_file_is_wholly_private() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("own");
        fs::write(&file, vec![3u8; 256 * 1024]).unwrap();
        let sizes = PrivateSizes::read(tmp.path());
        assert_eq!(
            sizes.private(OsStr::new("own"), allocated(&file)),
            allocated(&file)
        );
    }

    #[test]
    fn an_unknown_name_counts_as_private() {
        let tmp = tempfile::tempdir().unwrap();
        let sizes = PrivateSizes::read(tmp.path());
        assert_eq!(sizes.private(OsStr::new("gone"), 4096), 4096);
        assert_eq!(
            PrivateSizes::read(&tmp.path().join("missing")).private(OsStr::new("x"), 512),
            512
        );
    }

    #[test]
    fn many_entries_span_several_listing_batches() {
        let tmp = tempfile::tempdir().unwrap();
        // Long names fill the 64K buffer in a few hundred entries.
        let names: Vec<String> = (0..2000).map(|i| format!("{i:0>200}")).collect();
        for name in &names {
            fs::write(tmp.path().join(name), b"x").unwrap();
        }
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        // Clone every other file, so half the listing carries a private size.
        #[cfg(target_os = "macos")]
        for name in names.iter().step_by(2) {
            let from = tmp.path().join(name);
            let to = tmp.path().join(format!("{name}.clone"));
            assert!(cloned(&from, &to), "cp -c failed");
        }
        let sizes = PrivateSizes::read(tmp.path());
        #[cfg(target_os = "macos")]
        {
            // Only files that may share blocks are listed: the clones and
            // their originals, and no directory.
            assert_eq!(sizes.0.len(), names.len());
            for name in names.iter().step_by(2) {
                assert_eq!(sizes.private(OsStr::new(name), 4096), 0);
            }
        }
        for name in names.iter().skip(1).step_by(2) {
            let size = allocated(&tmp.path().join(name));
            assert_eq!(sizes.private(OsStr::new(name), size), size);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_apfs_clone_shares_its_blocks_until_written() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        let clone = tmp.path().join("clone");
        fs::write(&original, vec![5u8; 1024 * 1024]).unwrap();
        assert!(cloned(&original, &clone), "cp -c failed");
        let size = allocated(&clone);
        let sizes = PrivateSizes::read(tmp.path());
        assert_eq!(sizes.private(OsStr::new("clone"), size), 0);
        assert_eq!(sizes.private(OsStr::new("original"), size), 0);

        // Rewriting the clone gives it extents of its own, once they are on
        // disk; until then APFS reports no private bytes for the new data.
        fs::write(&clone, vec![6u8; 1024 * 1024]).unwrap();
        fs::File::open(&clone).unwrap().sync_all().unwrap();
        let sizes = PrivateSizes::read(tmp.path());
        assert_eq!(sizes.private(OsStr::new("clone"), size), size);
        assert_eq!(sizes.private(OsStr::new("original"), size), size);
    }

    /// Clone `from` to `to` with `cp -c`, as tools that seed worktrees do.
    #[cfg(target_os = "macos")]
    pub(crate) fn cloned(from: &Path, to: &Path) -> bool {
        std::process::Command::new("cp")
            .arg("-cRp")
            .arg(from)
            .arg(to)
            .status()
            .is_ok_and(|status| status.success())
    }
}
