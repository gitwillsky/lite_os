//! Bounded filesystem mutations exposed only to app sessions.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use super::io_error_code;

/// One mutation result. `error` is absent on success so the JavaScript wrapper
/// can branch on one optional field.
#[derive(Serialize)]
struct Outcome {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

/// Copy-recursion depth ceiling. Without it a pathological tree can exhaust
/// the stack while the host recursively materializes the destination.
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

/// Creates one directory at an absolute path.
///
/// # Parameters
///
/// - `payload`: Absolute directory path. The parent must already exist.
///
/// # Returns
///
/// A JSON outcome containing the path and, on failure, a stable errno name.
pub(in crate::host) fn mkdir(payload: &str) -> String {
    if !payload.starts_with('/') {
        return fail(payload, "EINVAL");
    }
    match fs::create_dir(payload) {
        Ok(()) => ok(payload),
        Err(error) => fail(payload, io_error_code(&error)),
    }
}

/// Removes one absolute filesystem path.
///
/// # Parameters
///
/// - `payload`: JSON object containing `path` and optional `recursive`.
///
/// # Returns
///
/// A JSON outcome. Non-empty directories require `recursive: true`.
pub(in crate::host) fn remove(payload: &str) -> String {
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

/// Renames one absolute path to another.
///
/// # Parameters
///
/// - `payload`: JSON object containing absolute `from` and `to` paths.
///
/// # Returns
///
/// A JSON outcome whose path is the destination on success.
pub(in crate::host) fn rename(payload: &str) -> String {
    let (from, to) = match parse_pair(payload) {
        Ok(pair) => pair,
        Err(code) => return fail(payload, code),
    };
    match fs::rename(&from, &to) {
        Ok(()) => ok(&to),
        Err(error) => fail(&to, io_error_code(&error)),
    }
}

/// Copies one absolute path to another.
///
/// # Parameters
///
/// - `payload`: JSON object containing absolute `from` and `to` paths.
///
/// # Returns
///
/// A JSON outcome whose path is the destination on success.
pub(in crate::host) fn copy(payload: &str) -> String {
    let (from, to) = match parse_pair(payload) {
        Ok(pair) => pair,
        Err(code) => return fail(payload, code),
    };
    match copy_path(Path::new(&from), Path::new(&to), 0) {
        Ok(()) => ok(&to),
        Err(error) => fail(&to, io_error_code(&error)),
    }
}

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
        serde_json::from_str::<Value>(json).ok().and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
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
