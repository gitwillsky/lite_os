//! The shell's shared view of freedesktop Trash 1.0 storage locations.

use linux_uapi::filesystem::mount_path;
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

/// A private Trash location on the home or a mounted local filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    root: PathBuf,
    top: Option<PathBuf>,
}

impl Volume {
    /// Home Trash at the effective XDG data location.
    pub fn home(home: &Path) -> Self {
        Self {
            root: super::user_data_directory(Some(home))
                .unwrap_or_else(|| home.join(".local/share"))
                .join("Trash"),
            top: None,
        }
    }

    /// Directory containing trashed items.
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    /// Directory containing the corresponding .trashinfo records.
    pub fn info(&self) -> PathBuf {
        self.root.join("info")
    }

    /// Mount point for a volume Trash, or None for the home Trash.
    pub fn top(&self) -> Option<&Path> {
        self.top.as_deref()
    }

    /// Base for a relative Path field in a Trash 1.0 record.
    pub fn base(&self) -> &Path {
        self.top()
            .unwrap_or_else(|| self.root.parent().expect("Trash has a data directory"))
    }

    /// Create missing directories. Invalid ownership, permissions or mount placement returns an error.
    pub fn prepare(&self) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        let Some(top) = &self.top else {
            builder.recursive(true);
            builder.create(self.files())?;
            return builder.create(self.info());
        };
        let uid = current_uid().ok_or(io::ErrorKind::Unsupported)?;
        let dev = fs::metadata(top)?.dev();
        if !candidates(top, uid).contains(&self.root) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        for path in [&self.root, &self.files(), &self.info()] {
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            if !private(path, uid, dev) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        Ok(())
    }
}

struct Mount {
    path: PathBuf,
    local: bool,
    writable: bool,
}

fn mounts() -> io::Result<Vec<Mount>> {
    let text = fs::read("/proc/self/mounts")?;
    Ok(text
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|&byte| byte == b' ');
            let source = fields.next()?;
            let path = mount_path(fields.next()?);
            let kind = fields.next()?;
            let options = fields.next()?;
            Some(Mount {
                path,
                local: source.starts_with(b"/dev/")
                    && matches!(kind, b"ext2" | b"ext3" | b"ext4" | b"xfs" | b"btrfs"),
                writable: options
                    .split(|&byte| byte == b',')
                    .any(|option| option == b"rw"),
            })
        })
        .collect())
}

fn current_uid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        Some(linux_uapi::process::effective_uid())
    }
    #[cfg(not(target_os = "linux"))]
    None
}

fn private(path: &Path, uid: u32, dev: u64) -> bool {
    fs::symlink_metadata(path).is_ok_and(|entry| {
        entry.is_dir() && entry.uid() == uid && entry.dev() == dev && entry.mode() & 0o077 == 0
    })
}

fn candidates(top: &Path, uid: u32) -> Vec<PathBuf> {
    let Ok(entry) = fs::symlink_metadata(top) else {
        return Vec::new();
    };
    // Other users must not be able to replace the private directory through its parent.
    if !entry.is_dir()
        || (entry.uid() != 0 && entry.uid() != uid)
        || (entry.mode() & 0o022 != 0 && entry.mode() & 0o1000 == 0)
    {
        return Vec::new();
    }
    let shared = top.join(".Trash");
    let mut roots = Vec::new();
    if fs::symlink_metadata(&shared).is_ok_and(|dir| {
        dir.is_dir()
            && dir.dev() == entry.dev()
            && (dir.uid() == 0 || dir.uid() == uid)
            && dir.mode() & 0o1000 != 0
    }) {
        roots.push(shared.join(uid.to_string()));
    }
    roots.push(top.join(format!(".Trash-{uid}")));
    roots
}

/// Visible Trash locations for the current identity. The home location is always included.
pub fn volumes(home: &Path) -> Vec<Volume> {
    let mut result = vec![Volume::home(home)];
    let (Some(uid), Ok(mounts)) = (current_uid(), mounts()) else {
        return result;
    };
    for mount in mounts.into_iter().filter(|mount| mount.local) {
        let Ok(top) = fs::metadata(&mount.path) else {
            continue;
        };
        for root in candidates(&mount.path, uid) {
            let volume = Volume {
                root,
                top: Some(mount.path.clone()),
            };
            if private(&volume.root, uid, top.dev())
                && private(&volume.files(), uid, top.dev())
                && (private(&volume.info(), uid, top.dev())
                    || fs::symlink_metadata(volume.info())
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound))
                && !result.contains(&volume)
            {
                result.push(volume);
            }
        }
    }
    result
}

/// Candidate Trash locations for the canonical source parent, in preference order.
/// Mount boundaries matter even when a bind mount has the same st_dev.
pub fn destinations(home: &Path, parent: &Path) -> io::Result<Vec<Volume>> {
    let home = Volume::home(home);
    let source = fs::metadata(parent)?;
    let mut ancestor = home.files();
    let target = loop {
        match fs::canonicalize(&ancestor) {
            Ok(path) => break path,
            Err(error) if error.kind() == io::ErrorKind::NotFound && ancestor.pop() => {}
            Err(error) => return Err(error),
        }
    };
    let mounts = mounts().unwrap_or_default();
    let containing = |path: &Path| {
        mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.path))
            .max_by_key(|mount| mount.path.as_os_str().len())
    };
    let source_mount = containing(parent);
    if source.dev() == fs::metadata(&target)?.dev()
        && source_mount.map(|mount| &mount.path) == containing(&target).map(|mount| &mount.path)
    {
        return Ok(vec![home]);
    }
    let (Some(uid), Some(mount)) = (current_uid(), source_mount) else {
        return Ok(Vec::new());
    };
    if !mount.local || !mount.writable {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for root in candidates(&mount.path, uid) {
        let allowed = match fs::symlink_metadata(&root) {
            Ok(_) => private(&root, uid, source.dev()) && can_create(&root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                root.parent().is_some_and(can_create)
            }
            Err(_) => false,
        };
        if allowed {
            result.push(Volume {
                root,
                top: Some(mount.path.clone()),
            });
        }
    }
    Ok(result)
}

fn can_create(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_uapi::filesystem::can_create_in(path).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Identities and change times of the directories contributing to the aggregate view.
pub fn stamp(home: &Path) -> Vec<(PathBuf, u64, u64, std::time::SystemTime)> {
    volumes(home)
        .into_iter()
        .filter_map(|volume| {
            let files = volume.files();
            let metadata = fs::metadata(&files).ok()?;
            let modified = metadata.modified().ok()?;
            Some((files, metadata.dev(), metadata.ino(), modified))
        })
        .collect()
}

/// Whether any visible Trash contains an item.
pub fn has_items(home: &Path) -> io::Result<bool> {
    for volume in volumes(home) {
        match fs::read_dir(volume.files()) {
            Ok(mut entries) => {
                if entries.next().transpose()?.is_some() {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound && volume.top().is_none() => {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    Ok(false)
}
