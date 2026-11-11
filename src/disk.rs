//! Capacity of the filesystem holding a path.

use std::path::Path;

/// Allocated blocks on one filesystem, in the same units as `df`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskUsage {
    /// Every block on the filesystem.
    pub total: u64,
    /// Blocks in use, including space reserved for root. This is `df`'s `Used`
    /// column.
    pub used: u64,
    /// Blocks an unprivileged process can still allocate. This is `df`'s
    /// `Avail` column, which can be smaller than `total - used` when the
    /// filesystem reserves blocks for root.
    pub free: u64,
}

impl DiskUsage {
    /// Whole percent of the filesystem's total size that is in use. This is the
    /// share of the disk that reads as occupied; blocks reserved for root count
    /// as used, so on Linux it can differ slightly from `df`'s `Use%`, which
    /// measures against the space a user could actually fill.
    pub fn used_percent(&self) -> u64 {
        if self.total == 0 {
            return 0;
        }
        (self.used as f64 * 100.0 / self.total as f64).round() as u64
    }
}

/// Usage of the filesystem that holds `path`, or `None` when the platform has
/// no `statvfs` or the call fails.
#[cfg(unix)]
pub fn usage(path: &Path) -> Option<DiskUsage> {
    let stat = rustix::fs::statvfs(path).ok()?;
    // `f_frsize` is the block size that block counts are expressed in; some
    // filesystems leave it zero and only fill `f_bsize`.
    let unit = if stat.f_frsize != 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    if unit == 0 {
        return None;
    }
    let total = stat.f_blocks.saturating_mul(unit);
    let used = total.saturating_sub(stat.f_bfree.saturating_mul(unit));
    let free = stat.f_bavail.saturating_mul(unit);
    Some(DiskUsage { total, used, free })
}

#[cfg(not(unix))]
pub fn usage(_path: &Path) -> Option<DiskUsage> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_reports_a_plausible_slice_of_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(usage) = usage(tmp.path()) else {
            return; // filesystem without statvfs in this environment
        };
        assert!(usage.total > 0);
        assert!(usage.used <= usage.total);
        assert!(usage.free <= usage.total);
        assert!(usage.used_percent() <= 100);
    }

    #[test]
    fn percent_is_used_over_the_whole_disk() {
        let usage = DiskUsage {
            total: 1000,
            used: 400,
            free: 550,
        };
        assert_eq!(usage.used_percent(), 40);
    }

    #[test]
    fn an_empty_filesystem_does_not_divide_by_zero() {
        let empty = DiskUsage {
            total: 0,
            used: 0,
            free: 0,
        };
        assert_eq!(empty.used_percent(), 0);
    }
}
