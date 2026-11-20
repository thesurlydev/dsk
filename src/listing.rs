//! One directory's entries, with everything a scan needs to know about each.
//!
//! The portable way to get this is `read_dir` plus a `stat` per entry. On
//! macOS that is slow because it costs a system call per entry, and threads
//! making them contend with each other inside the kernel.
//! `getattrlistbulk(2)` instead returns the attributes of around a thousand
//! entries of an open directory per call. Scanning 3M files went from 39s to
//! under 8s.
//!
//! The same call reports whether a file shares blocks with APFS clones.
//! Copy-on-write clones (`cp -c`, `clonefile`) share extents until one side is
//! written, so a cloned tree reports its full size in its allocation while
//! deleting it frees only the extents it no longer shares. Tools that seed
//! worktrees with a clone of a warm `target/` directory make this the norm: a
//! worktree can hold 170G of blocks of which only 20G would be freed. Each
//! entry therefore carries its private bytes as well as its allocation.

use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::scan::NodeKind;

/// When an entry was created and last modified, in seconds since the Unix
/// epoch. Either is `None` where the filesystem does not report it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamps {
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

impl Stamps {
    pub fn of(metadata: &Metadata) -> Self {
        Self {
            created: metadata.created().ok().map(unix_secs),
            modified: metadata.modified().ok().map(unix_secs),
        }
    }
}

fn unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// One directory entry. Symlinks are described, never followed.
#[derive(Debug)]
pub struct Entry {
    /// The name for display, with invalid UTF-8 replaced.
    pub name: Box<str>,
    /// The exact name, kept only when `name` had to be altered.
    raw_name: Option<OsString>,
    pub kind: NodeKind,
    /// Allocated bytes (`st_blocks * 512`). For a directory, its own blocks.
    pub size: u64,
    /// The part of `size` that deleting the entry would free: all of it, unless
    /// the file shares blocks with APFS clones.
    pub private: u64,
    /// The device the entry is on, as `st_dev`. For a mount point this is the
    /// mounted filesystem, not the one holding the directory.
    pub device: u64,
    /// The `(device, inode)` key of a regular file with more than one link.
    pub hardlink: Option<(u64, u64)>,
    /// The entry's own creation and modification times.
    pub stamps: Stamps,
}

impl Entry {
    /// The exact name, for building a path.
    pub fn os_name(&self) -> &OsStr {
        self.raw_name
            .as_deref()
            .unwrap_or_else(|| OsStr::new(&*self.name))
    }
}

/// The entries of one directory that could be read, and the ones that could
/// not, each with the path the error belongs to.
#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub errors: Vec<(PathBuf, io::Error)>,
}

/// List `dir`. Fails only when the directory itself cannot be read.
pub fn read(dir: &Path) -> io::Result<Listing> {
    // A filesystem without the bulk call, or any failure of it, falls through
    // to the portable listing, which then reports whatever is really wrong.
    #[cfg(target_os = "macos")]
    if let Some(listing) = bulk::read(dir) {
        return Ok(listing);
    }
    portable(dir)
}

fn portable(dir: &Path) -> io::Result<Listing> {
    let mut listing = Listing::default();
    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                listing.errors.push((dir.to_path_buf(), error));
                continue;
            }
        };
        // DirEntry::metadata does not follow symlinks.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                listing.errors.push((entry.path(), error));
                continue;
            }
        };
        let (name, raw_name) = display_name(entry.file_name());
        let size = disk_usage(&metadata);
        listing.entries.push(Entry {
            name,
            raw_name,
            kind: kind_of(&metadata),
            size,
            private: size,
            device: device(&metadata),
            hardlink: inode_key(&metadata),
            stamps: Stamps::of(&metadata),
        });
    }
    Ok(listing)
}

fn display_name(name: OsString) -> (Box<str>, Option<OsString>) {
    match name.into_string() {
        Ok(name) => (name.into_boxed_str(), None),
        Err(raw) => (raw.to_string_lossy().into(), Some(raw)),
    }
}

fn kind_of(metadata: &Metadata) -> NodeKind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        NodeKind::Dir
    } else if file_type.is_file() {
        NodeKind::File
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

pub fn disk_usage(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

pub fn device(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

fn inode_key(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (metadata.is_file() && metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "macos")]
mod bulk {
    use std::cell::RefCell;
    use std::ffi::{CStr, CString, OsStr};
    use std::fs;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{Entry, Listing, Stamps, device, disk_usage, display_name};
    use crate::scan::NodeKind;

    /// Per-entry error slot; libc does not export it.
    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    /// `ATTR_CMNEXT_EXT_FLAGS` bits from `<sys/stat.h>`; libc does not export
    /// them. A file that may share blocks with another file, and one that
    /// shares every block it has.
    const EF_MAY_SHARE_BLOCKS: u64 = 0x01;
    const EF_SHARES_ALL_BLOCKS: u64 = 0x40;
    /// `vtype` values in `ATTR_CMN_OBJTYPE`.
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;

    const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_CRTIME
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FILEID
        | ATTR_CMN_ERROR;
    const DIR: u32 = libc::ATTR_DIR_MOUNTSTATUS | libc::ATTR_DIR_ALLOCSIZE;
    const FILE: u32 = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;
    /// With FSOPT_ATTR_CMN_EXTENDED, the fork group carries the extended
    /// common attributes (ATTR_CMNEXT_*).
    ///
    /// The flags, not ATTR_CMNEXT_PRIVATESIZE: asking every file for its
    /// private size doubles the time of a scan, while the flags settle all but
    /// the rare partly shared file, and only those are asked.
    const EXTENDED: u32 = libc::ATTR_CMNEXT_EXT_FLAGS;

    thread_local! {
        /// The reply buffer, one per scanning thread. Each call fills it with
        /// as many entries as fit, so a larger buffer means fewer calls.
        static BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; 256 * 1024]);
    }

    /// List `dir` with `getattrlistbulk`, or `None` if that is not possible
    /// here or the reply cannot be understood.
    pub fn read(dir: &Path) -> Option<Listing> {
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
        // SAFETY: `fd` was just opened and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // SAFETY: a plain C struct for which all-zero is valid.
        let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
        request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        request.commonattr = COMMON;
        request.dirattr = DIR;
        request.fileattr = FILE;
        request.forkattr = EXTENDED;

        let mut listing = Listing::default();
        BUFFER.with_borrow_mut(|buffer| {
            loop {
                // SAFETY: the buffer pointer and length describe `buffer`, and
                // `request` outlives the call.
                let count = unsafe {
                    libc::getattrlistbulk(
                        fd.as_raw_fd(),
                        (&raw mut request).cast(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        u64::from(libc::FSOPT_ATTR_CMN_EXTENDED),
                    )
                };
                if count < 0 {
                    return None;
                }
                if count == 0 {
                    return Some(());
                }
                let mut rest = &buffer[..];
                for _ in 0..count {
                    let length = Record::new(rest).u32()? as usize;
                    let record = rest.get(..length)?;
                    add(&mut listing, dir, &fd, record)?;
                    rest = rest.get(length..)?;
                }
            }
        })?;
        Some(listing)
    }

    /// Reads the fields of one packed record in order. Every field starts on a
    /// 4-byte boundary and none is padded, so reading is plain concatenation.
    struct Record<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl<'a> Record<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, at: 0 }
        }

        fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
            let field = self.bytes.get(self.at..self.at + N)?.try_into().ok()?;
            self.at += N;
            Some(field)
        }

        fn u32(&mut self) -> Option<u32> {
            self.take().map(u32::from_ne_bytes)
        }

        fn u64(&mut self) -> Option<u64> {
            self.take().map(u64::from_ne_bytes)
        }

        /// The seconds of a `timespec`; the nanoseconds are skipped.
        fn timespec_secs(&mut self) -> Option<i64> {
            let secs = i64::from_ne_bytes(self.take()?);
            self.take::<8>()?;
            Some(secs)
        }

        /// An `attrreference_t` to a NUL-terminated string: an offset from the
        /// reference's own address, and a length that counts the NUL.
        fn string(&mut self) -> Option<&'a CStr> {
            let base = self.at;
            let offset = i32::from_ne_bytes(self.take()?);
            let length = self.u32()? as usize;
            let start = base.checked_add_signed(offset as isize)?;
            CStr::from_bytes_until_nul(self.bytes.get(start..start.checked_add(length)?)?).ok()
        }
    }

    /// Parse one record and add its entry, or its error, to `listing`. `None`
    /// means the record is not in the expected shape.
    ///
    /// A record holds its length, the set of attributes actually returned,
    /// then those attributes in a fixed order: the error, the common group,
    /// the directory group, the file group, and the extended common group. An
    /// attribute that does not apply (a file attribute of a directory) or that
    /// the filesystem cannot supply is simply absent, so each read is paired
    /// with a test of its returned bit.
    fn add(listing: &mut Listing, dir: &Path, fd: &OwnedFd, record: &[u8]) -> Option<()> {
        let mut record = Record::new(record);
        let _length = record.u32()?;
        let common = record.u32()?;
        let _volume = record.u32()?;
        let directory = record.u32()?;
        let file = record.u32()?;
        let extended = record.u32()?;
        let has = |returned: u32, bit: u32| returned & bit != 0;

        let error = if has(common, ATTR_CMN_ERROR) {
            record.u32()?
        } else {
            0
        };
        if !has(common, libc::ATTR_CMN_NAME) {
            return None;
        }
        let name = record.string()?;
        let mut devid = None;
        if has(common, libc::ATTR_CMN_DEVID) {
            // dev_t is signed; widen it the way `st_dev` is widened.
            devid = Some(record.u32()? as i32 as u64);
        }
        let mut objtype = None;
        if has(common, libc::ATTR_CMN_OBJTYPE) {
            objtype = Some(record.u32()?);
        }
        let mut created = None;
        if has(common, libc::ATTR_CMN_CRTIME) {
            created = Some(record.timespec_secs()?);
        }
        let mut modified = None;
        if has(common, libc::ATTR_CMN_MODTIME) {
            modified = Some(record.timespec_secs()?);
        }
        let mut fileid = None;
        if has(common, libc::ATTR_CMN_FILEID) {
            fileid = Some(record.u64()?);
        }
        let mut mount_status = 0;
        if has(directory, libc::ATTR_DIR_MOUNTSTATUS) {
            mount_status = record.u32()?;
        }
        let mut dir_allocated = None;
        if has(directory, libc::ATTR_DIR_ALLOCSIZE) {
            dir_allocated = Some(record.u64()?);
        }
        let mut links = None;
        if has(file, libc::ATTR_FILE_LINKCOUNT) {
            links = Some(record.u32()?);
        }
        let mut file_allocated = None;
        if has(file, libc::ATTR_FILE_ALLOCSIZE) {
            file_allocated = Some(record.u64()?);
        }
        let mut flags = None;
        if has(extended, libc::ATTR_CMNEXT_EXT_FLAGS) {
            flags = Some(record.u64()?);
        }

        let os_name = OsStr::from_bytes(name.to_bytes());
        if error != 0 {
            let error = std::io::Error::from_raw_os_error(error as i32);
            listing.errors.push((dir.join(os_name), error));
            return Some(());
        }

        // What the listing did not supply comes from one `fstatat` on the
        // open directory, which still involves no path lookup.
        let stat = || stat_at(fd, name);
        let entry = match objtype {
            Some(VDIR) if mount_status & libc::DIR_MNTSTATUS_MNTPOINT != 0 => {
                // The listing describes the directory under a mount, and
                // `fstatat` does not cross onto it; only a path does.
                match fs::symlink_metadata(dir.join(os_name)) {
                    Ok(metadata) => Ok(Fields {
                        kind: NodeKind::Dir,
                        size: disk_usage(&metadata),
                        device: device(&metadata),
                        hardlink: None,
                        stamps: Stamps::of(&metadata),
                    }),
                    Err(error) => {
                        listing.errors.push((dir.join(os_name), error));
                        return Some(());
                    }
                }
            }
            Some(VDIR) => match (dir_allocated, devid) {
                (Some(size), Some(device)) => Ok(Fields {
                    kind: NodeKind::Dir,
                    size,
                    device,
                    hardlink: None,
                    stamps: Stamps::default(),
                }),
                _ => Fields::from_stat(stat()),
            },
            Some(VREG) => match (file_allocated, devid, fileid, links) {
                (Some(size), Some(device), Some(inode), Some(links)) => Ok(Fields {
                    kind: NodeKind::File,
                    size,
                    device,
                    hardlink: (links > 1).then_some((device, inode)),
                    stamps: Stamps::default(),
                }),
                _ => Fields::from_stat(stat()),
            },
            // Symlinks and special files have no file attributes.
            Some(VLNK) | Some(_) | None => Fields::from_stat(stat()),
        };
        let Fields {
            kind,
            size,
            device,
            hardlink,
            stamps,
        } = match entry {
            Ok(fields) => fields,
            Err(error) => {
                listing.errors.push((dir.join(os_name), error));
                return Some(());
            }
        };
        // The listing's times win; what it left out comes from the stat that
        // described the entry, or failing that one more `fstatat`.
        let mut stamps = Stamps {
            created: created.or(stamps.created),
            modified: modified.or(stamps.modified),
        };
        if (stamps.created.is_none() || stamps.modified.is_none())
            && let Ok(stat) = stat()
        {
            let from_stat = Fields::stamps_of(&stat);
            stamps.created = stamps.created.or(from_stat.created);
            stamps.modified = stamps.modified.or(from_stat.modified);
        }

        let private = match (kind, flags) {
            (NodeKind::File, Some(flags)) if flags & EF_MAY_SHARE_BLOCKS == 0 => size,
            (NodeKind::File, Some(flags)) if flags & EF_SHARES_ALL_BLOCKS != 0 => 0,
            // Shares some blocks but not all: only the file itself can say
            // how many.
            (NodeKind::File, Some(_)) => private_size(fd, name).map_or(size, |own| own.min(size)),
            _ => size,
        };

        let (name, raw_name) = display_name(os_name.to_os_string());
        listing.entries.push(Entry {
            name,
            raw_name,
            kind,
            size,
            private,
            device,
            hardlink,
            stamps,
        });
        Some(())
    }

    /// What an entry needs besides its name and private bytes.
    struct Fields {
        kind: NodeKind,
        size: u64,
        device: u64,
        hardlink: Option<(u64, u64)>,
        /// Times known from a stat; empty when only the listing has them.
        stamps: Stamps,
    }

    impl Fields {
        fn stamps_of(stat: &libc::stat) -> Stamps {
            Stamps {
                created: Some(stat.st_birthtime),
                modified: Some(stat.st_mtime),
            }
        }

        fn from_stat(stat: std::io::Result<libc::stat>) -> std::io::Result<Self> {
            let stat = stat?;
            let kind = match stat.st_mode & libc::S_IFMT {
                libc::S_IFDIR => NodeKind::Dir,
                libc::S_IFREG => NodeKind::File,
                libc::S_IFLNK => NodeKind::Symlink,
                _ => NodeKind::Other,
            };
            let device = stat.st_dev as u64;
            Ok(Self {
                kind,
                size: (stat.st_blocks as u64).saturating_mul(512),
                device,
                hardlink: (kind == NodeKind::File && stat.st_nlink > 1)
                    .then_some((device, stat.st_ino)),
                stamps: Self::stamps_of(&stat),
            })
        }
    }

    /// `lstat` of `name` inside the open directory.
    fn stat_at(fd: &OwnedFd, name: &CStr) -> std::io::Result<libc::stat> {
        // SAFETY: a plain C struct for which all-zero is valid.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is NUL-terminated and `stat` outlives the call.
        let status = unsafe {
            libc::fstatat(
                fd.as_raw_fd(),
                name.as_ptr(),
                &raw mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        match status {
            0 => Ok(stat),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// The reply to a request for `ATTR_CMN_RETURNED_ATTRS` and
    /// `ATTR_CMNEXT_PRIVATESIZE`, packed on 4-byte boundaries.
    #[repr(C, packed(4))]
    struct PrivateSize {
        length: u32,
        returned: libc::attribute_set_t,
        private: libc::off_t,
    }

    /// The bytes of `name`, inside the open directory, that no other file
    /// shares, if the filesystem reports them.
    fn private_size(fd: &OwnedFd, name: &CStr) -> Option<u64> {
        // SAFETY: `attrlist` is a plain C struct for which all-zero is valid.
        let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
        // SAFETY: `PrivateSize` is a packed struct of integers and plain C
        // structs, so all-zero is a valid value.
        let mut reply: PrivateSize = unsafe { std::mem::zeroed() };
        request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        request.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
        request.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE;
        // SAFETY: `name` is NUL-terminated, and the buffer and its size
        // describe `reply`, which outlives the call.
        let status = unsafe {
            libc::getattrlistat(
                fd.as_raw_fd(),
                name.as_ptr(),
                (&raw mut request).cast(),
                (&raw mut reply).cast(),
                size_of::<PrivateSize>(),
                libc::c_ulong::from(libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED),
            )
        };
        let returned = reply.returned;
        if status != 0 || returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE == 0 {
            return None;
        }
        u64::try_from(reply.private).ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn entry<'a>(listing: &'a Listing, name: &str) -> &'a Entry {
        listing
            .entries
            .iter()
            .find(|entry| &*entry.name == name)
            .unwrap_or_else(|| panic!("{name} listed"))
    }

    /// Every field of every entry, sorted by name, for comparing listings.
    fn summary(listing: &Listing) -> Vec<String> {
        let mut lines: Vec<String> = listing
            .entries
            .iter()
            .map(|entry| {
                format!(
                    "{} {:?} size={} device={} hardlink={:?} stamps={:?}",
                    entry.name, entry.kind, entry.size, entry.device, entry.hardlink, entry.stamps
                )
            })
            .collect();
        lines.sort();
        lines
    }

    /// Set the modification time of the file or directory at `path`, `ago`
    /// seconds before now.
    pub(crate) fn set_modified_ago(path: &Path, ago: u64) {
        let time = std::time::SystemTime::now() - std::time::Duration::from_secs(ago);
        fs::File::open(path).unwrap().set_modified(time).unwrap();
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

    /// A directory with one of everything a listing has to tell apart.
    fn assortment() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("file"), vec![1u8; 300 * 1024]).unwrap();
        fs::write(dir.join("empty"), b"").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();
        fs::write(dir.join("subdir/inner"), vec![2u8; 64 * 1024]).unwrap();
        fs::hard_link(dir.join("file"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink("file", dir.join("symlink")).unwrap();
        std::os::unix::fs::symlink("subdir", dir.join("dirlink")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
        tmp
    }

    #[test]
    fn describes_each_kind_of_entry_like_lstat() {
        let tmp = assortment();
        let listing = read(tmp.path()).unwrap();
        assert!(listing.errors.is_empty(), "{:?}", listing.errors);
        assert_eq!(listing.entries.len(), 8);

        for (name, kind) in [
            ("file", NodeKind::File),
            ("empty", NodeKind::File),
            ("link", NodeKind::File),
            ("subdir", NodeKind::Dir),
            ("symlink", NodeKind::Symlink),
            ("dirlink", NodeKind::Symlink),
            ("dangling", NodeKind::Symlink),
            ("socket", NodeKind::Other),
        ] {
            let entry = entry(&listing, name);
            let metadata = fs::symlink_metadata(tmp.path().join(name)).unwrap();
            assert_eq!(entry.kind, kind, "{name}");
            assert_eq!(entry.size, metadata.blocks() * 512, "{name}");
            assert_eq!(entry.device, metadata.dev(), "{name}");
            assert_eq!(entry.os_name(), OsStr::new(name));
            assert_eq!(entry.private, entry.size, "{name}: nothing is cloned");
            assert_eq!(entry.stamps, Stamps::of(&metadata), "{name}");
            assert!(entry.stamps.modified.is_some(), "{name}");
        }

        let inode = fs::metadata(tmp.path().join("file")).unwrap();
        let key = Some((inode.dev(), inode.ino()));
        assert_eq!(entry(&listing, "file").hardlink, key);
        assert_eq!(entry(&listing, "link").hardlink, key);
        assert_eq!(entry(&listing, "empty").hardlink, None);
        assert_eq!(entry(&listing, "symlink").hardlink, None);
    }

    #[test]
    fn the_fast_listing_agrees_with_the_portable_one() {
        let tmp = assortment();
        assert_eq!(
            summary(&read(tmp.path()).unwrap()),
            summary(&portable(tmp.path()).unwrap())
        );
    }

    #[test]
    fn an_entry_reports_the_modification_time_it_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("old"), b"x").unwrap();
        fs::write(tmp.path().join("new"), b"x").unwrap();
        let year = 365 * 24 * 3600;
        set_modified_ago(&tmp.path().join("old"), 3 * year);

        let listing = read(tmp.path()).unwrap();
        let now = crate::format::unix_now();
        let old = entry(&listing, "old").stamps.modified.unwrap();
        let new = entry(&listing, "new").stamps.modified.unwrap();
        assert!((now - old - 3 * year as i64).abs() < 60, "{now} {old}");
        assert!((now - new).abs() < 60, "{now} {new}");
    }

    #[test]
    fn a_large_directory_spans_several_calls() {
        let tmp = tempfile::tempdir().unwrap();
        // Long names fill the reply buffer in about a thousand entries.
        let names: Vec<String> = (0..5000).map(|i| format!("{i:0>200}")).collect();
        for name in &names {
            fs::write(tmp.path().join(name), b"x").unwrap();
        }
        let listing = read(tmp.path()).unwrap();
        assert_eq!(listing.entries.len(), names.len());
        assert_eq!(
            summary(&listing),
            summary(&portable(tmp.path()).unwrap()),
            "no entry lost or repeated between calls"
        );
    }

    #[test]
    fn an_empty_directory_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let listing = read(tmp.path()).unwrap();
        assert!(listing.entries.is_empty() && listing.errors.is_empty());
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = read(&tmp.path().join("missing")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_name_that_is_not_utf8_keeps_its_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let raw = OsStr::from_bytes(b"caf\xe9");
        if fs::write(tmp.path().join(raw), b"x").is_err() {
            return; // APFS refuses names that are not valid UTF-8
        }
        let listing = read(tmp.path()).unwrap();
        assert_eq!(listing.entries[0].os_name(), raw);
        assert_eq!(&*listing.entries[0].name, "caf\u{fffd}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_apfs_clone_shares_its_blocks_until_written() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        let clone = tmp.path().join("clone");
        fs::write(&original, vec![5u8; 1024 * 1024]).unwrap();
        fs::write(tmp.path().join("own"), vec![4u8; 256 * 1024]).unwrap();
        assert!(cloned(&original, &clone), "cp -c failed");

        let listing = read(tmp.path()).unwrap();
        let size = entry(&listing, "clone").size;
        assert_eq!(size, 1024 * 1024);
        assert_eq!(entry(&listing, "clone").private, 0);
        assert_eq!(entry(&listing, "original").private, 0);
        let own = entry(&listing, "own");
        assert_eq!(own.private, own.size, "an ordinary file is all private");

        // Rewriting the clone gives it extents of its own, once they are on
        // disk; until then APFS reports no private bytes for the new data.
        fs::write(&clone, vec![6u8; 1024 * 1024]).unwrap();
        fs::File::open(&clone).unwrap().sync_all().unwrap();
        let listing = read(tmp.path()).unwrap();
        assert_eq!(entry(&listing, "clone").private, size);
        assert_eq!(entry(&listing, "original").private, size);
    }

    /// A clone rewritten in part shares some blocks and owns the rest, which
    /// the flags alone cannot size.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_partly_rewritten_clone_owns_only_what_it_rewrote() {
        use std::io::{Seek, SeekFrom, Write};
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        let clone = tmp.path().join("clone");
        fs::write(&original, vec![5u8; 4 * 1024 * 1024]).unwrap();
        assert!(cloned(&original, &clone), "cp -c failed");

        let mut file = fs::OpenOptions::new().write(true).open(&clone).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&vec![9u8; 1024 * 1024]).unwrap();
        file.sync_all().unwrap();

        let listing = read(tmp.path()).unwrap();
        let clone = entry(&listing, "clone");
        assert_eq!(clone.size, 4 * 1024 * 1024);
        assert_eq!(clone.private, 1024 * 1024);
    }
}
