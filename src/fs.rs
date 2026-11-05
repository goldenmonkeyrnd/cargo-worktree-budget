//! Linux filesystem identity, checked directory opening and managed-tree accounting.
use crate::{Error, Result, reject};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    device: u64,
    inode: u64,
    birth_seconds: i64,
    birth_nanoseconds: u32,
}

fn stat(path: &Path, flags: i32, dirfd: RawFd) -> Result<libc::statx> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Policy("NUL in path".into()))?;
    let mut out = MaybeUninit::uninit();
    // SAFETY: name and output storage live through the syscall.
    if unsafe {
        libc::statx(
            dirfd,
            name.as_ptr(),
            flags,
            libc::STATX_BASIC_STATS | libc::STATX_BTIME | libc::STATX_MNT_ID,
            out.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: statx succeeded.
    Ok(unsafe { out.assume_init() })
}

fn fd_stat(file: &File) -> Result<libc::statx> {
    stat(Path::new(""), libc::AT_EMPTY_PATH, file.as_raw_fd())
}

pub(crate) fn identity(file: &File) -> Result<Identity> {
    let raw = fd_stat(file)?;
    if raw.stx_mask & libc::STATX_BTIME == 0 {
        return reject("filesystem does not provide creation time for safe directory identity");
    }
    Ok(Identity {
        device: file.metadata()?.dev(),
        inode: raw.stx_ino,
        birth_seconds: raw.stx_btime.tv_sec,
        birth_nanoseconds: raw.stx_btime.tv_nsec,
    })
}

pub(crate) fn directory_mount_id(file: &File) -> Result<u64> {
    mount_id(&fd_stat(file)?)
}

fn mount_id(raw: &libc::statx) -> Result<u64> {
    if raw.stx_mask & libc::STATX_MNT_ID == 0 {
        return reject("Linux mount identity is unavailable");
    }
    Ok(raw.stx_mnt_id)
}

pub(crate) fn require_mount(path: &Path) -> Result<u64> {
    let file = directory(path, false)?;
    let raw = fd_stat(&file)?;
    if raw.stx_attributes & libc::STATX_ATTR_MOUNT_ROOT as u64 == 0 {
        return reject(format!(
            "expected a mounted filesystem at {}",
            path.display()
        ));
    }
    mount_id(&raw)
}
pub(crate) fn check_mount(file: &File, expected: Option<u64>) -> Result<()> {
    if let Some(id) = expected
        && mount_id(&fd_stat(file)?)? != id
    {
        return reject("managed path is on a different mount");
    }
    Ok(())
}

pub(crate) fn link(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
pub(crate) fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

pub(crate) fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return reject(format!(
            "expected an absolute physical path without '..': {}",
            path.display()
        ));
    }
    Ok(())
}

// Each new component is resolved relative to an open directory handle. Refuse
// symlinks, including those in ancestors, before creating any missing child.
pub(crate) fn directory(path: &Path, create: bool) -> Result<File> {
    validate_path(path)?;
    let mut parent = open_dir(Path::new("/"))?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            let child = link(&parent).join(name);
            parent = match open_dir(&child) {
                Ok(file) => file,
                Err(err) if create && err.kind() == io::ErrorKind::NotFound => {
                    match DirBuilder::new().mode(0o700).create(&child) {
                        Ok(()) => parent.sync_all()?,
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(err) => return Err(err.into()),
                    }
                    open_dir(&child)?
                }
                Err(err) => return Err(err.into()),
            };
        }
    }
    Ok(parent)
}

pub(crate) fn nearest(path: &Path) -> Result<File> {
    let mut candidate = path;
    loop {
        match directory(candidate, false) {
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                candidate = candidate.parent().ok_or(err)?
            }
            other => return other,
        }
    }
}

pub(crate) fn private(root: &File) -> Result<()> {
    let meta = root.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return reject("pool root must be an owner-only directory (mode 0700) owned by this user");
    }
    Ok(())
}

pub(crate) fn free(file: &File) -> Result<u64> {
    let mut info = MaybeUninit::uninit();
    // SAFETY: valid fd and writable statvfs output.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the syscall succeeded.
    let info = unsafe { info.assume_init() };
    u64::try_from(u128::from(info.f_bavail) * u128::from(info.f_frsize))
        .map_err(|_| Error::Policy("available-byte count overflow".into()))
}

pub(crate) fn lock(file: &File, wait: bool) -> Result<bool> {
    let flags = libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB };
    loop {
        // SAFETY: flock borrows this live descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err.into());
        }
    }
}

pub(crate) fn tree(build: &File, purge: bool) -> Result<u64> {
    let id = mount_id(&fd_stat(build)?)?;
    walk(build, id, purge, &mut HashSet::new())
}

fn walk(
    dir: &File,
    expected_mount: u64,
    purge: bool,
    seen: &mut HashSet<(u32, u32, u64)>,
) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in fs::read_dir(link(dir))? {
        let path = entry?.path();
        let s = stat(&path, libc::AT_SYMLINK_NOFOLLOW, libc::AT_FDCWD)?;
        if mount_id(&s)? != expected_mount {
            return reject(format!(
                "refusing to cross nested mount at {}",
                path.display()
            ));
        }
        if seen.insert((s.stx_dev_major, s.stx_dev_minor, s.stx_ino)) {
            bytes = bytes
                .checked_add(
                    s.stx_blocks
                        .checked_mul(512)
                        .ok_or_else(|| Error::Policy("block count overflow".into()))?,
                )
                .ok_or_else(|| Error::Policy("tree size overflow".into()))?;
        }
        if s.stx_mode & libc::S_IFMT as u16 == libc::S_IFDIR as u16 {
            let child = open_dir(&path)?;
            check_mount(&child, Some(expected_mount))?;
            bytes = bytes
                .checked_add(walk(&child, expected_mount, purge, seen)?)
                .ok_or_else(|| Error::Policy("tree size overflow".into()))?;
            if purge {
                fs::remove_dir(path)?;
            }
        } else if purge {
            fs::remove_file(path)?;
        }
    }
    if purge {
        dir.sync_all()?;
    }
    Ok(bytes)
}
