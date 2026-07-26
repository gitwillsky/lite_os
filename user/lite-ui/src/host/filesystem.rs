//! Bounded filesystem capabilities exposed only to app sessions.

mod mutations;

pub(super) use mutations::{copy, mkdir, remove, rename};

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Seek},
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

/// Lists one absolute directory without following reported symlinks.
pub(super) fn list(path: &str) -> String {
    #[derive(Serialize)]
    struct Entry {
        name: String,
        kind: &'static str,
        size: u64,
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
    use std::{fs, path::PathBuf};

    use super::Files;

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
