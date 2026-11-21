//! Portable directory disk usage measurement with macOS APFS clone awareness
//! (`ATTR_CMNEXT_PRIVATESIZE`, `ATTR_CMNEXT_CLONEID`, and `ATTR_CMNEXT_CLONE_REFCNT`).

use std::collections::BTreeSet;
use std::path::Path;

use walkdir::WalkDir;

/// Disk usage summary for a directory tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirDiskUsage {
    /// Total allocated/apparent bytes across unique inodes inside the directory (similar to `du`).
    pub logical_bytes: u64,
    /// Estimated physical bytes uniquely owned by this directory tree that will be reclaimed on deletion.
    ///
    /// On macOS APFS, unmodified Copy-on-Write clones shared with files outside the directory
    /// contribute `0` to `unique_bytes` while diverged files and clone groups wholly contained
    /// inside the directory contribute their full allocated size. On other platforms, equals
    /// `logical_bytes`.
    pub unique_bytes: u64,
    /// Total number of regular files counted inside the directory.
    pub file_count: u64,
}

/// Formats a byte count into a concise human-readable binary unit string (`B`, `KiB`, `MiB`, `GiB`).
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;

    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// APFS extent metadata returned by `getattrlist(2)` on macOS.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
struct ApfsFileInfo {
    alloc_size: u64,
    private_size: u64,
    clone_id: u64,
    clone_refcnt: u32,
}

/// Queries macOS APFS `ATTR_FILE_ALLOCSIZE`, `ATTR_CMNEXT_PRIVATESIZE`, `ATTR_CMNEXT_CLONEID`,
/// and `ATTR_CMNEXT_CLONE_REFCNT` for `path` without following symbolic links.
#[cfg(target_os = "macos")]
fn query_apfs_file_info(path: &Path) -> Option<ApfsFileInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
    const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;
    const FSOPT_NOFOLLOW: libc::c_uint = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: libc::c_uint = 0x0000_0020;

    let path_c = CString::new(path.as_os_str().as_bytes()).ok()?;

    let mut full_list = libc::attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_ALLOCSIZE,
        forkattr: ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT,
    };
    let mut buf = [0u8; 64];

    // SAFETY: `path_c` is a valid NUL-terminated C string, `full_list` is a properly initialized
    // `libc::attrlist`, and `buf` is a 64-byte writable buffer larger than the 32-byte packed reply.
    let rc = unsafe {
        libc::getattrlist(
            path_c.as_ptr(),
            (&raw mut full_list).cast::<libc::c_void>(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };

    if rc == 0 {
        let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        if len >= 32 {
            let alloc_size = i64::from_ne_bytes([
                buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
            ])
            .max(0) as u64;
            let private_size = i64::from_ne_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ])
            .max(0) as u64;
            let clone_id = u64::from_ne_bytes([
                buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
            ]);
            let clone_refcnt = u32::from_ne_bytes([buf[28], buf[29], buf[30], buf[31]]);

            return Some(ApfsFileInfo {
                alloc_size,
                private_size,
                clone_id,
                clone_refcnt,
            });
        }
    }

    // Fallback for older macOS releases where `ATTR_CMNEXT_CLONE_REFCNT` may not be supported.
    let mut basic_list = libc::attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_ALLOCSIZE,
        forkattr: ATTR_CMNEXT_PRIVATESIZE,
    };
    let mut basic_buf = [0u8; 32];

    // SAFETY: `path_c` is a valid NUL-terminated C string, `basic_list` is initialized, and
    // `basic_buf` is 32 bytes (larger than the 20-byte packed reply).
    let rc_basic = unsafe {
        libc::getattrlist(
            path_c.as_ptr(),
            (&raw mut basic_list).cast::<libc::c_void>(),
            basic_buf.as_mut_ptr().cast::<libc::c_void>(),
            basic_buf.len(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };

    if rc_basic == 0 {
        let len =
            u32::from_ne_bytes([basic_buf[0], basic_buf[1], basic_buf[2], basic_buf[3]]) as usize;

        if len >= 20 {
            let alloc_size = i64::from_ne_bytes([
                basic_buf[4],
                basic_buf[5],
                basic_buf[6],
                basic_buf[7],
                basic_buf[8],
                basic_buf[9],
                basic_buf[10],
                basic_buf[11],
            ])
            .max(0) as u64;
            let private_size = i64::from_ne_bytes([
                basic_buf[12],
                basic_buf[13],
                basic_buf[14],
                basic_buf[15],
                basic_buf[16],
                basic_buf[17],
                basic_buf[18],
                basic_buf[19],
            ])
            .max(0) as u64;

            return Some(ApfsFileInfo {
                alloc_size,
                private_size,
                clone_id: 0,
                clone_refcnt: 0,
            });
        }
    }

    None
}

/// Measures disk usage for `dir`, deduplicating hardlinks on Unix and accounting for APFS
/// Copy-on-Write clone sharing on macOS.
///
/// Returns zeroed [`DirDiskUsage`] if `dir` does not exist.
#[must_use]
pub fn measure_dir_usage(dir: &Path) -> DirDiskUsage {
    if !dir.exists() {
        return DirDiskUsage::default();
    }

    let mut usage = DirDiskUsage::default();
    #[cfg(unix)]
    let mut seen_inodes: BTreeSet<(u64, u64)> = BTreeSet::new();
    #[cfg(not(unix))]
    let _seen_placeholder: BTreeSet<u64> = BTreeSet::new();
    #[cfg(target_os = "macos")]
    let mut shared_clones: std::collections::BTreeMap<u64, (u64, u32, u32)> =
        std::collections::BTreeMap::new();

    for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            if !seen_inodes.insert((meta.dev(), meta.ino())) {
                continue;
            }
        }

        usage.file_count = usage.file_count.saturating_add(1);

        #[cfg(target_os = "macos")]
        {
            if let Some(info) = query_apfs_file_info(entry.path()) {
                usage.logical_bytes = usage.logical_bytes.saturating_add(info.alloc_size);

                if info.private_size > 0 {
                    usage.unique_bytes = usage.unique_bytes.saturating_add(info.private_size);
                } else if info.clone_id != 0 && info.clone_refcnt > 1 {
                    let slot = shared_clones.entry(info.clone_id).or_insert((
                        info.alloc_size,
                        0,
                        info.clone_refcnt,
                    ));
                    slot.1 = slot.1.saturating_add(1);
                } else if info.clone_refcnt <= 1 {
                    usage.unique_bytes = usage.unique_bytes.saturating_add(info.alloc_size);
                }

                continue;
            }
        }

        #[cfg(unix)]
        let file_bytes = {
            use std::os::unix::fs::MetadataExt;

            meta.blocks().saturating_mul(512).max(meta.len())
        };

        #[cfg(not(unix))]
        let file_bytes = meta.len();

        usage.logical_bytes = usage.logical_bytes.saturating_add(file_bytes);
        usage.unique_bytes = usage.unique_bytes.saturating_add(file_bytes);
    }

    #[cfg(target_os = "macos")]
    {
        for (_cid, (alloc_size, seen_in_dir, total_refcnt)) in shared_clones {
            if seen_in_dir >= total_refcnt {
                usage.unique_bytes = usage.unique_bytes.saturating_add(alloc_size);
            }
        }
    }

    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn format_bytes_renders_expected_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn measure_dir_usage_counts_files_and_recognizes_apfs_clones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_dir = tmp.path().join("src");
        let clone_dir = tmp.path().join("clone");
        fs::create_dir_all(&src_dir).expect("mkdir src");

        let payload = vec![b'A'; 128 * 1024];
        fs::write(src_dir.join("big.bin"), &payload).expect("write big.bin");

        let src_usage = measure_dir_usage(&src_dir);
        assert_eq!(src_usage.file_count, 1);
        assert!(src_usage.logical_bytes >= 128 * 1024);
        assert!(src_usage.unique_bytes >= 128 * 1024);

        let strategy = crate::clone_path_cow(&src_dir, &clone_dir).expect("clone_path_cow");
        let clone_usage = measure_dir_usage(&clone_dir);
        assert_eq!(clone_usage.file_count, 1);
        assert!(clone_usage.logical_bytes >= 128 * 1024);

        #[cfg(target_os = "macos")]
        if strategy == crate::CowStrategy::ApfsClonefile {
            assert_eq!(
                clone_usage.unique_bytes, 0,
                "unmodified APFS CoW clone shared with src must report 0 unique bytes"
            );
        }

        let _ = strategy;
    }
}
