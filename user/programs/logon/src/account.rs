//! 本机账户登录与桌面会话目录。
use linux_uapi::process::SessionUser;
use std::{fs, io, path::Path};

pub use linux_uapi::credentials::account::{Purpose, authenticate, passwordless};

pub fn runtime(user: &SessionUser) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
    fs::create_dir_all("/run/user")?;
    let path = &user.runtime;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let stat = fs::symlink_metadata(path)?;
            if !stat.is_dir() || stat.uid() != user.identity.uid {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    chown(path, Some(user.identity.uid), Some(user.identity.gid))?;
    if !Path::new(&user.home).is_dir() {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok(())
}
