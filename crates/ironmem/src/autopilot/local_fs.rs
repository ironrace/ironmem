//! Autopilot runs on one host: its controllers must share a local disk.
//!
//! Everything a controller proves about another rests on that. The issue
//! lock is an `flock`, which an NFS mount with `nolock` keeps per host; the
//! process table and Claude's registry show this host's processes only; and
//! SQLite's write-ahead log needs shared memory that a network filesystem
//! does not give. Two hosts sharing a database or a repository over the
//! network would each see the other's live implementer as gone.
//!
//! So a controller refuses, at its start, a git common directory or an
//! ironmem database on a filesystem known to be remote: on macOS any mount
//! without `MNT_LOCAL`, and on Linux a filesystem whose `statfs` magic is one
//! of [`linux_network_type`]'s. Anything else is allowed: the list names
//! known network filesystems rather than trying to name every local one.

use std::path::{Path, PathBuf};

use crate::error::MemoryError;

/// What `statfs` says of the filesystem holding a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsKind {
    /// Whether it is local to this host.
    pub local: bool,
    /// Its type, for messages: `apfs`, `nfs`, or a Linux magic number.
    pub name: String,
}

/// Reads the filesystem kind of an existing path.
pub(crate) type FsProbe<'a> = &'a dyn Fn(&Path) -> std::io::Result<FsKind>;

/// Refuse when `path` (or, when it does not exist yet, its nearest existing
/// ancestor) is on a remote filesystem. `what` names it in the error.
pub fn require_local(path: &Path, what: &str) -> Result<(), MemoryError> {
    require_local_with(path, what, &statfs_kind)
}

/// [`require_local`], with `statfs` injected.
pub(crate) fn require_local_with(
    path: &Path,
    what: &str,
    probe: FsProbe<'_>,
) -> Result<(), MemoryError> {
    let existing = nearest_existing(path);
    let kind = probe(&existing).map_err(|e| {
        MemoryError::Validation(format!(
            "cannot tell whether {what} {} is on a local filesystem: {e}",
            existing.display()
        ))
    })?;
    if kind.local {
        return Ok(());
    }
    Err(MemoryError::Validation(format!(
        "{what} {} is on a {} filesystem, which is not local to this host. Autopilot's \
         controllers coordinate through a file lock, the process table and SQLite, none of \
         which holds across hosts: keep the repository and the ironmem database on a local \
         disk",
        existing.display(),
        kind.name
    )))
}

/// `path`, or its nearest ancestor that exists.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut at = path;
    loop {
        if at.exists() {
            return at.to_path_buf();
        }
        match at.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => at = parent,
            _ => return path.to_path_buf(),
        }
    }
}

/// The network filesystem a Linux `statfs` magic number names, if it names
/// one. Refused: NFS, SMB, CIFS and SMB2, FUSE (sshfs, s3fs and the like
/// share its one magic number, so every FUSE mount is refused), 9p, AFS,
/// Ceph, Coda, NCP, OCFS2, GFS2 and Lustre. Everything else, overlay and
/// virtiofs included, is allowed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_network_type(magic: u64) -> Option<&'static str> {
    Some(match magic & 0xFFFF_FFFF {
        0x6969 => "nfs",
        0x517B => "smb",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x6573_5546 => "fuse",
        0x0102_1997 => "9p",
        0x5346_414F => "afs",
        0x00C3_6400 => "ceph",
        0x7375_7245 => "coda",
        0x564C => "ncp",
        0x7461_636F => "ocfs2",
        0x0116_1970 => "gfs2",
        0x0BD0_0BD0 => "lustre",
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
fn statfs_kind(path: &Path) -> std::io::Result<FsKind> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid C string and `st` a writable statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel NUL-terminates f_fstypename.
    let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(FsKind {
        local: st.f_flags & (libc::MNT_LOCAL as u32) != 0,
        name,
    })
}

#[cfg(target_os = "linux")]
fn statfs_kind(path: &Path) -> std::io::Result<FsKind> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid C string and `st` a writable statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = st.f_type as u64;
    Ok(match linux_network_type(magic) {
        Some(name) => FsKind {
            local: false,
            name: name.to_string(),
        },
        None => FsKind {
            local: true,
            name: format!("{:#x}", magic & 0xFFFF_FFFF),
        },
    })
}

/// Elsewhere nothing is known to be remote.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn statfs_kind(_path: &Path) -> std::io::Result<FsKind> {
    Ok(FsKind {
        local: true,
        name: "unknown".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(local: bool, name: &str) -> impl Fn(&Path) -> std::io::Result<FsKind> + '_ {
        move |_| {
            Ok(FsKind {
                local,
                name: name.to_string(),
            })
        }
    }

    #[test]
    fn a_remote_filesystem_is_refused_naming_the_path_and_its_type() {
        let dir = tempfile::tempdir().unwrap();
        let err = require_local_with(dir.path(), "the ironmem database", &kind(false, "nfs"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("the ironmem database"), "{err}");
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        assert!(err.contains("nfs filesystem"), "{err}");
        require_local_with(dir.path(), "x", &kind(true, "apfs")).unwrap();
    }

    #[test]
    fn a_path_not_made_yet_is_judged_by_its_nearest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let seen = std::cell::RefCell::new(None);
        let probe = |p: &Path| {
            *seen.borrow_mut() = Some(p.to_path_buf());
            Ok(FsKind {
                local: true,
                name: "apfs".into(),
            })
        };
        require_local_with(&dir.path().join("a/b/ironmem.db"), "x", &probe).unwrap();
        assert_eq!(seen.borrow().as_deref(), Some(dir.path()));
    }

    #[test]
    fn a_filesystem_that_cannot_be_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let failing = |_: &Path| Err(std::io::Error::other("statfs failed"));
        let err = require_local_with(dir.path(), "x", &failing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("statfs failed"), "{err}");
    }

    #[test]
    fn known_network_magics_are_remote_and_local_ones_are_not() {
        for (magic, name) in [
            (0x6969, "nfs"),
            (0xFF53_4D42, "cifs"),
            (0xFE53_4D42, "smb2"),
            (0x517B, "smb"),
            (0x6573_5546, "fuse"),
            (0x0102_1997, "9p"),
        ] {
            assert_eq!(linux_network_type(magic), Some(name), "{magic:#x}");
        }
        // ext4, xfs, btrfs, tmpfs, overlay, virtiofs.
        for magic in [
            0xEF53,
            0x5846_5342,
            0x9123_683E,
            0x0102_1994,
            0x794C_7630,
            0x6A65_6A63,
        ] {
            assert_eq!(linux_network_type(magic), None, "{magic:#x}");
        }
    }

    #[test]
    fn this_hosts_temp_directory_is_local() {
        let dir = tempfile::tempdir().unwrap();
        require_local(dir.path(), "a temp directory").unwrap();
    }
}
