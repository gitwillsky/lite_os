//! Bounded read/write filesystem operations exposed to the file-manager
//! application: `list`/`read` plus `mkdir`/`remove`/`rename`/`copy`. Every path
//! argument must be absolute and structured (multi-path) methods take a JSON
//! payload, because paths legitimately contain `:` and spaces.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

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

/// One mutation result. `error` is absent on success (mirroring the read-side
/// listings), so the JS wrapper can branch on a single optional field.
#[derive(Serialize)]
struct Outcome {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

/// Copy-recursion depth ceiling. Matches the React host tree bound in `tree.rs`
/// so a symlink loop or pathological tree cannot exhaust the stack.
const MAX_COPY_DEPTH: usize = 64;

fn ok(path: &str) -> String {
    serialize(path, None)
}

fn fail(path: &str, code: &'static str) -> String {
    serialize(path, Some(code))
}

fn serialize(path: &str, error: Option<&'static str>) -> String {
    serde_json::to_string(&Outcome {
        path: path.to_owned(),
        error,
    })
    .unwrap_or_default()
}

/// Creates one directory at an absolute path (non-recursive: the parent must
/// already exist, matching Explorer's "New Folder"). Returns `{path}` on
/// success or `{path, error}` — `EEXIST` when the name is taken, `ENOENT` when
/// the parent is missing.
pub(super) fn mkdir(payload: &str) -> String {
    if !payload.starts_with('/') {
        return fail(payload, "EINVAL");
    }
    match fs::create_dir(payload) {
        Ok(()) => ok(payload),
        Err(error) => fail(payload, io_error_code(&error)),
    }
}

/// Removes an absolute path. Payload is JSON `{"path","recursive"}`; a file is
/// unlinked, an empty directory is removed, and a non-empty directory needs
/// `recursive:true` (otherwise `ENOTEMPTY`). The caller decides whether to send
/// the whole subtree, so no directory is ever silently deleted here.
pub(super) fn remove(payload: &str) -> String {
    #[derive(Deserialize)]
    struct Request {
        path: String,
        #[serde(default)]
        recursive: bool,
    }
    let request: Request = match serde_json::from_str(payload) {
        Ok(request) => request,
        Err(_) => return fail(payload, "EINVAL"),
    };
    if !request.path.starts_with('/') {
        return fail(&request.path, "EINVAL");
    }
    let metadata = match fs::symlink_metadata(&request.path) {
        Ok(metadata) => metadata,
        Err(error) => return fail(&request.path, io_error_code(&error)),
    };
    let result = if metadata.is_dir() {
        if request.recursive {
            fs::remove_dir_all(&request.path)
        } else {
            fs::remove_dir(&request.path)
        }
    } else {
        fs::remove_file(&request.path)
    };
    match result {
        Ok(()) => ok(&request.path),
        Err(error) => fail(&request.path, io_error_code(&error)),
    }
}

/// Renames (moves) one absolute path to another. Payload is JSON
/// `{"from","to"}`; this also backs the file-manager's cut/paste. Returns the
/// destination path on success.
pub(super) fn rename(payload: &str) -> String {
    let (from, to) = match parse_pair(payload) {
        Ok(pair) => pair,
        Err(code) => return fail(payload, code),
    };
    match fs::rename(&from, &to) {
        Ok(()) => ok(&to),
        Err(error) => fail(&to, io_error_code(&error)),
    }
}

/// Copies one absolute path to another. Payload is JSON `{"from","to"}`. A file
/// is copied whole (no 64 KiB read cap, unlike `read`); a directory is copied
/// recursively up to `MAX_COPY_DEPTH`. Returns the destination path.
pub(super) fn copy(payload: &str) -> String {
    let (from, to) = match parse_pair(payload) {
        Ok(pair) => pair,
        Err(code) => return fail(payload, code),
    };
    match copy_path(Path::new(&from), Path::new(&to), 0) {
        Ok(()) => ok(&to),
        Err(error) => fail(&to, io_error_code(&error)),
    }
}

/// Parses a `{"from","to"}` payload and validates both paths are absolute.
fn parse_pair(payload: &str) -> Result<(String, String), &'static str> {
    #[derive(Deserialize)]
    struct Request {
        from: String,
        to: String,
    }
    let request: Request = serde_json::from_str(payload).map_err(|_| "EINVAL")?;
    if !request.from.starts_with('/') || !request.to.starts_with('/') {
        return Err("EINVAL");
    }
    Ok((request.from, request.to))
}

/// Recursively copies `from` to `to`, honoring the depth ceiling. Files use
/// `fs::copy`; directories are created then their entries copied.
fn copy_path(from: &Path, to: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy depth exceeded",
        ));
    }
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_path(&entry.path(), &to.join(entry.file_name()), depth + 1)?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::{copy, mkdir, remove, rename};

    /// A unique scratch directory under the system temp dir, cleaned on drop so
    /// a failing assertion never leaks files (mirrors the std-only approach the
    /// host tests take — no extra dev-dependency).
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "lite-fs-{tag}-{}",
                std::process::id() as u64 * 1000 + line!() as u64,
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).expect("scratch root");
            Self(base)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn error_of(json: &str) -> Option<String> {
        serde_json::from_str::<Value>(json)
            .ok()
            .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_owned))
    }

    #[test]
    fn mkdir_creates_then_reports_eexist() {
        let scratch = Scratch::new("mkdir");
        let dir = scratch.path("New Folder");
        assert_eq!(error_of(&mkdir(&dir)), None);
        assert!(fs::metadata(&dir).expect("dir exists").is_dir());
        assert_eq!(error_of(&mkdir(&dir)).as_deref(), Some("EEXIST"));
    }

    #[test]
    fn remove_file_empty_dir_and_gated_recursive() {
        let scratch = Scratch::new("remove");
        let file = scratch.path("note.txt");
        fs::write(&file, b"hi").expect("seed file");
        assert_eq!(error_of(&remove(&format!(r#"{{"path":{file:?}}}"#))), None);
        assert!(fs::metadata(&file).is_err());

        let empty = scratch.path("empty");
        fs::create_dir(&empty).expect("seed empty dir");
        assert_eq!(error_of(&remove(&format!(r#"{{"path":{empty:?}}}"#))), None);

        let full = scratch.path("full");
        fs::create_dir(&full).expect("seed full dir");
        fs::write(scratch.path("full/child"), b"x").expect("seed child");
        assert_eq!(
            error_of(&remove(&format!(r#"{{"path":{full:?}}}"#))).as_deref(),
            Some("ENOTEMPTY"),
        );
        assert_eq!(
            error_of(&remove(&format!(r#"{{"path":{full:?},"recursive":true}}"#))),
            None,
        );
        assert!(fs::metadata(&full).is_err());
    }

    #[test]
    fn rename_moves_the_entry() {
        let scratch = Scratch::new("rename");
        let from = scratch.path("a.txt");
        let to = scratch.path("b.txt");
        fs::write(&from, b"data").expect("seed");
        assert_eq!(
            error_of(&rename(&format!(r#"{{"from":{from:?},"to":{to:?}}}"#))),
            None,
        );
        assert!(fs::metadata(&from).is_err());
        assert_eq!(fs::read(&to).expect("moved"), b"data");
    }

    #[test]
    fn copy_file_and_directory_recursively() {
        let scratch = Scratch::new("copy");
        let file = scratch.path("src.txt");
        let file_copy = scratch.path("dst.txt");
        fs::write(&file, b"payload").expect("seed file");
        assert_eq!(
            error_of(&copy(&format!(r#"{{"from":{file:?},"to":{file_copy:?}}}"#))),
            None,
        );
        assert_eq!(fs::read(&file_copy).expect("copied file"), b"payload");

        let tree = scratch.path("tree");
        fs::create_dir(&tree).expect("seed tree");
        fs::write(scratch.path("tree/leaf"), b"leaf").expect("seed leaf");
        let tree_copy = scratch.path("tree-copy");
        assert_eq!(
            error_of(&copy(&format!(r#"{{"from":{tree:?},"to":{tree_copy:?}}}"#))),
            None,
        );
        assert_eq!(
            fs::read(scratch.path("tree-copy/leaf")).expect("copied leaf"),
            b"leaf",
        );
    }

    #[test]
    fn relative_paths_and_bad_json_are_rejected() {
        assert_eq!(error_of(&mkdir("relative/path")).as_deref(), Some("EINVAL"));
        assert_eq!(
            error_of(&remove(r#"{"path":"relative"}"#)).as_deref(),
            Some("EINVAL"),
        );
        assert_eq!(error_of(&remove("not json")).as_deref(), Some("EINVAL"));
        assert_eq!(
            error_of(&rename(r#"{"from":"/a","to":"relative"}"#)).as_deref(),
            Some("EINVAL"),
        );
        assert_eq!(error_of(&copy("{}")).as_deref(), Some("EINVAL"));
    }
}
