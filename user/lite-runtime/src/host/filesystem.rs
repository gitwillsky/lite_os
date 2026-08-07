//! Bounded filesystem capabilities exposed only to app sessions.

mod mutations;

pub(super) use mutations::{copy, mkdir, remove, rename};

use std::{
    collections::BTreeMap,
    ffi::CString,
    fs,
    io::{self, Read, Seek},
    mem::MaybeUninit,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::Serialize;

#[derive(Default)]
pub(super) struct Files {
    next_object_url: u64,
    object_urls: BTreeMap<u64, FileEntry>,
}

struct FileEntry {
    path: PathBuf,
    offset: u64,
    length: u64,
}

pub(super) struct FileRange {
    pub(super) path: PathBuf,
    pub(super) offset: u64,
    pub(super) length: u64,
}

impl Files {
    /// Opens one regular file as a filesystem-backed Web `File`.
    pub(super) fn open(&self, path: &str) -> Result<String, &'static str> {
        if !path.starts_with('/') {
            return Err("EINVAL");
        }
        let metadata = fs::metadata(path).map_err(|error| io_error_code(&error))?;
        if !metadata.is_file() {
            return Err("EISDIR");
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Descriptor<'a> {
            path: &'a str,
            name: &'a str,
            size: u64,
            r#type: &'static str,
            last_modified: u64,
        }
        serde_json::to_string(&Descriptor {
            path,
            name: Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("media"),
            size: metadata.len(),
            r#type: media_type(path),
            last_modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis() as u64),
        })
        .map_err(|_| "IO")
    }

    /// Creates one opaque URL whose lifetime is independent from the Web `File` object.
    pub(super) fn create_object_url(
        &mut self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<String, &'static str> {
        let metadata = file_metadata(path)?;
        let end = offset.checked_add(length).ok_or("EINVAL")?;
        if end > metadata.len() {
            return Err("EINVAL");
        }
        self.next_object_url = self.next_object_url.checked_add(1).ok_or("EMFILE")?;
        let object = self.next_object_url;
        self.object_urls.insert(
            object,
            FileEntry {
                path: PathBuf::from(path),
                offset,
                length,
            },
        );
        Ok(format!("blob:lite/{object}"))
    }

    /// Revokes exactly one opaque URL without invalidating its source `File`.
    pub(super) fn revoke_object_url(&mut self, source: &str) -> Result<(), &'static str> {
        let object = object_url_identity(source)?;
        self.object_urls.remove(&object).map(|_| ()).ok_or("ENOENT")
    }

    /// Resolves only a live opaque URL; a revoked sibling URL cannot alias another.
    pub(super) fn resolve_blob(&self, source: &str) -> Result<FileRange, &'static str> {
        let object = object_url_identity(source)?;
        let entry = self.object_urls.get(&object).ok_or("ENOENT")?;
        Ok(FileRange {
            path: entry.path.clone(),
            offset: entry.offset,
            length: entry.length,
        })
    }

    /// Reads an explicit Blob range for the standard `arrayBuffer`/`bytes` surface.
    pub(super) fn read_range(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, &'static str> {
        let metadata = file_metadata(path)?;
        let available = metadata.len().saturating_sub(offset);
        let length = length.min(available as usize);
        let mut file = fs::File::open(path).map_err(|error| io_error_code(&error))?;
        file.seek(io::SeekFrom::Start(offset)).map_err(|_| "IO")?;
        let mut bytes = Vec::with_capacity(length);
        file.take(length as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "IO")?;
        Ok(bytes)
    }
}

fn file_metadata(path: &str) -> Result<fs::Metadata, &'static str> {
    if !path.starts_with('/') {
        return Err("EINVAL");
    }
    let metadata = fs::metadata(path).map_err(|error| io_error_code(&error))?;
    metadata.is_file().then_some(metadata).ok_or("EISDIR")
}

fn object_url_identity(source: &str) -> Result<u64, &'static str> {
    source
        .strip_prefix("blob:lite/")
        .and_then(|identity| identity.parse::<u64>().ok())
        .filter(|identity| *identity > 0)
        .ok_or("EINVAL")
}

fn media_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wav") => "audio/wav",
        Some("aiff" | "aif") => "audio/aiff",
        Some("caf") => "audio/x-caf",
        Some("flac") => "audio/flac",
        Some("mp1") => "audio/mpeg",
        Some("mp2") => "audio/mpeg",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "oga") => "audio/ogg",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("mka" | "webm") => "audio/webm",
        _ => "application/octet-stream",
    }
}

/// Returns the capacity of the mounted filesystem containing one absolute path.
///
/// The byte counts share one `statvfs` snapshot so the UI cannot combine total
/// and free values from different filesystem states. Reserved blocks remain
/// unavailable but not used: `usedBytes` follows `f_blocks - f_bfree`, while
/// `availableBytes` exposes `f_bavail` for future write-availability displays.
pub(super) fn capacity(path: &str) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Capacity {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        used_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        available_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'static str>,
    }
    let error = |code: &'static str| Capacity {
        path: path.to_owned(),
        total_bytes: None,
        used_bytes: None,
        available_bytes: None,
        error: Some(code),
    };
    if !path.starts_with('/') {
        return serde_json::to_string(&error("EINVAL")).unwrap_or_default();
    }
    let native_path = match CString::new(path) {
        Ok(path) => path,
        Err(_) => return serde_json::to_string(&error("EINVAL")).unwrap_or_default(),
    };
    let mut statistics = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `native_path` is NUL-terminated and lives across the call;
    // `statistics` is aligned writable storage for exactly one `statvfs`.
    if unsafe { libc::statvfs(native_path.as_ptr(), statistics.as_mut_ptr()) } != 0 {
        return serde_json::to_string(&error(io_error_code(&io::Error::last_os_error())))
            .unwrap_or_default();
    }
    // SAFETY: POSIX requires a successful `statvfs` call to initialize the
    // complete output structure; the failure branch returned above.
    let statistics = unsafe { statistics.assume_init() };
    let fragment_size = if statistics.f_frsize == 0 {
        statistics.f_bsize
    } else {
        statistics.f_frsize
    } as u64;
    let total_bytes = (statistics.f_blocks as u64).saturating_mul(fragment_size);
    let free_bytes = (statistics.f_bfree as u64)
        .saturating_mul(fragment_size)
        .min(total_bytes);
    let available_bytes = (statistics.f_bavail as u64)
        .saturating_mul(fragment_size)
        .min(total_bytes);
    serde_json::to_string(&Capacity {
        path: path.to_owned(),
        total_bytes: Some(total_bytes),
        used_bytes: Some(total_bytes - free_bytes),
        available_bytes: Some(available_bytes),
        error: None,
    })
    .unwrap_or_default()
}

/// Lists one absolute directory without following reported symlinks.
///
/// `mtime` is the entry's own `st_mtime` in Unix seconds (stat(2) semantics):
/// `DirEntry::metadata` does not follow symlinks, so a link reports its own
/// timestamp exactly like `ls -l` and Explorer do. The metadata was already
/// fetched per entry for kind/size, so the new field adds no extra syscalls —
/// the per-entry cost gate is unchanged (no new hot-path syscall, hence no
/// benchmark impact to measure).
pub(super) fn list(path: &str) -> String {
    #[derive(Serialize)]
    struct Entry {
        name: String,
        kind: &'static str,
        size: u64,
        mtime: u64,
    }
    #[derive(Serialize)]
    struct Listing {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        entries: Option<Vec<Entry>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'static str>,
    }
    const MAX_ENTRIES: usize = 1000;
    let error = |code: &'static str| Listing {
        path: path.to_owned(),
        entries: None,
        truncated: None,
        error: Some(code),
    };
    if !path.starts_with('/') {
        return serde_json::to_string(&error("EINVAL")).unwrap_or_default();
    }
    let iterator = match fs::read_dir(path) {
        Ok(iterator) => iterator,
        Err(io_error) => {
            return serde_json::to_string(&error(io_error_code(&io_error))).unwrap_or_default();
        }
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in iterator.flatten() {
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "dir"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: if kind == "dir" { 0 } else { metadata.len() },
            // Pre-epoch or unsupported timestamps collapse to 0 (the UI shows
            // an empty cell), matching `open`'s last_modified fallback.
            mtime: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs()),
        });
    }
    serde_json::to_string(&Listing {
        path: path.to_owned(),
        entries: Some(entries),
        truncated: Some(truncated),
        error: None,
    })
    .unwrap_or_default()
}

/// Reads one absolute text file under the bounded QuickJS payload budget.
pub(super) fn read(path: &str) -> String {
    #[derive(Serialize)]
    struct FileContent {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'static str>,
    }
    const MAX_BYTES: usize = 64 * 1024;
    let error = |code: &'static str| FileContent {
        path: path.to_owned(),
        content: None,
        truncated: None,
        error: Some(code),
    };
    if !path.starts_with('/') {
        return serde_json::to_string(&error("EINVAL")).unwrap_or_default();
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return serde_json::to_string(&error("EISDIR")).unwrap_or_default();
        }
        Ok(_) => {}
        Err(io_error) => {
            return serde_json::to_string(&error(io_error_code(&io_error))).unwrap_or_default();
        }
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(io_error) => {
            return serde_json::to_string(&error(io_error_code(&io_error))).unwrap_or_default();
        }
    };
    let truncated = bytes.len() > MAX_BYTES;
    let slice = &bytes[..bytes.len().min(MAX_BYTES)];
    match std::str::from_utf8(slice) {
        Ok(text) => serde_json::to_string(&FileContent {
            path: path.to_owned(),
            content: Some(text.to_owned()),
            truncated: Some(truncated),
            error: None,
        })
        .unwrap_or_default(),
        // A cap can split a multi-byte codepoint. Only classify an untruncated
        // decode failure as binary; otherwise retry against the complete file.
        Err(_) if !truncated => serde_json::to_string(&error("not-text")).unwrap_or_default(),
        Err(_) => match std::str::from_utf8(&bytes) {
            Ok(text) => serde_json::to_string(&FileContent {
                path: path.to_owned(),
                content: Some(text.chars().take(MAX_BYTES).collect()),
                truncated: Some(true),
                error: None,
            })
            .unwrap_or_default(),
            Err(_) => serde_json::to_string(&error("not-text")).unwrap_or_default(),
        },
    }
}

fn io_error_code(error: &io::Error) -> &'static str {
    use io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        _ => "IO",
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::UNIX_EPOCH};

    use super::Files;

    #[test]
    fn capacity_uses_one_real_filesystem_snapshot_and_rejects_relative_paths() {
        let capacity = serde_json::from_str::<serde_json::Value>(&super::capacity("/tmp"))
            .expect("capacity json");
        let total = capacity
            .get("totalBytes")
            .and_then(serde_json::Value::as_u64)
            .expect("total bytes");
        let used = capacity
            .get("usedBytes")
            .and_then(serde_json::Value::as_u64)
            .expect("used bytes");
        let available = capacity
            .get("availableBytes")
            .and_then(serde_json::Value::as_u64)
            .expect("available bytes");
        assert!(total > 0);
        assert!(used <= total);
        assert!(available <= total);

        let relative = serde_json::from_str::<serde_json::Value>(&super::capacity("tmp"))
            .expect("relative-path error json");
        assert_eq!(
            relative.get("error").and_then(serde_json::Value::as_str),
            Some("EINVAL")
        );
    }

    #[test]
    fn directory_listing_reports_each_entry_mtime() {
        let directory = PathBuf::from(format!(
            "/tmp/lite-ui-list-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir(&directory).expect("fixture dir");
        fs::write(directory.join("note.txt"), b"hello").expect("fixture file");
        let listing = serde_json::from_str::<serde_json::Value>(&super::list(
            directory.to_str().expect("utf8 path"),
        ))
        .expect("listing json");
        let entries = listing
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .expect("entries");
        let entry = entries
            .iter()
            .find(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some("note.txt"))
            .expect("fixture entry");
        let mtime = entry
            .get("mtime")
            .and_then(serde_json::Value::as_u64)
            .expect("mtime field");
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_secs();
        assert!(mtime > 0 && mtime <= now && now - mtime < 60);
        fs::remove_dir_all(directory).expect("cleanup fixture");
    }

    #[test]
    fn filesystem_file_ranges_remain_lazy_and_exact() {
        let path = PathBuf::from(format!(
            "/tmp/lite-ui-file-{}-{}.bin",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, b"prefix-media-suffix").expect("fixture");
        let files = Files::default();
        let descriptor = files.open(path.to_str().expect("utf8 path")).expect("open");
        let descriptor =
            serde_json::from_str::<serde_json::Value>(&descriptor).expect("descriptor");
        let native_path = descriptor
            .get("path")
            .and_then(serde_json::Value::as_str)
            .expect("native path");
        assert_eq!(
            files.read_range(native_path, 7, 5).expect("range"),
            b"media"
        );
        fs::remove_file(path).expect("cleanup fixture");
    }

    #[test]
    fn repeated_source_replacement_reclaims_every_object_url() {
        let path = PathBuf::from(format!(
            "/tmp/lite-ui-object-url-{}-{}.bin",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, b"prefix-media-suffix").expect("fixture");
        let mut files = Files::default();
        for _ in 0..128 {
            let url = files
                .create_object_url(path.to_str().expect("utf8 path"), 7, 5)
                .expect("create object URL");
            let range = files.resolve_blob(&url).expect("resolve");
            assert_eq!(range.path, path);
            assert_eq!((range.offset, range.length), (7, 5));
            files.revoke_object_url(&url).expect("revoke");
            assert!(files.resolve_blob(&url).is_err());
        }
        assert!(files.object_urls.is_empty());
        assert_eq!(
            files
                .read_range(path.to_str().expect("utf8 path"), 7, 5)
                .expect("File remains usable"),
            b"media"
        );
        fs::remove_file(path).expect("cleanup fixture");
    }
}
