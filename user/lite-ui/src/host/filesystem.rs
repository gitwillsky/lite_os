//! Read-only filesystem operations exposed to the file-manager application.

use std::{fs, io};

use serde::Serialize;

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
        _ => "IO",
    }
}
