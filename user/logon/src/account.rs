//! 本机账户策略：不信任字段缺失、锁定密码、到期账户或不可登录 shell。
use linux_uapi::process::{Identity, SessionUser};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// 最短改密间隔仅限制 PasswordChange，不能因此阻止正常登录；UID 0 保留管理员改密语义。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Login,
    PasswordChange,
}

pub fn authenticate(
    name: &str,
    password: &str,
    purpose: Purpose,
) -> io::Result<Option<SessionUser>> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    let shadow = fs::read_to_string("/etc/shadow")?;
    let group = fs::read_to_string("/etc/group")?;
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs()
        / 86400;
    let account = parse(&passwd, &shadow, &group, name, today, purpose);
    let Some((user, hash)) = account else {
        return Ok(None);
    };
    // 空 shadow 字段表示未设密码，只接受空输入；锁定标记由 parse 拒绝，不与空字段混同。
    let matched = if hash.is_empty() {
        password.is_empty()
    } else {
        linux_uapi::credentials::verify(password, &hash)?
    };
    Ok(matched.then_some(user))
}

fn parse(
    passwd: &str,
    shadow: &str,
    group: &str,
    name: &str,
    today: u64,
    purpose: Purpose,
) -> Option<(SessionUser, String)> {
    if name.is_empty() || name.contains([':', '/', '\0']) {
        return None;
    }
    let rows: Vec<_> = passwd
        .lines()
        .filter(|row| row.split(':').next() == Some(name))
        .collect();
    if rows.len() != 1 {
        return None;
    }
    let p: Vec<_> = rows[0].split(':').collect();
    if p.len() != 7
        || p[1] != "x"
        || p[6].ends_with("/false")
        || p[6].ends_with("/nologin")
        || p[6].is_empty()
    {
        return None;
    }
    let uid = p[2].parse().ok()?;
    let gid = p[3].parse().ok()?;
    let home = PathBuf::from(p[5]);
    if !home.is_absolute() || uid == 65534 {
        return None;
    }
    let rows: Vec<_> = shadow
        .lines()
        .filter(|row| row.split(':').next() == Some(name))
        .collect();
    if rows.len() != 1 {
        return None;
    }
    let s: Vec<_> = rows[0].split(':').collect();
    if s.len() != 9 || !(s[1].is_empty() || s[1].starts_with("$5$") || s[1].starts_with("$6$")) {
        return None;
    }
    let day = |value: &str| -> Option<Option<u64>> {
        if value.is_empty() || value == "-1" {
            Some(None)
        } else {
            Some(Some(value.parse().ok()?))
        }
    };
    let changed = day(s[2])?;
    let minimum = day(s[3])?;
    let maximum = day(s[4])?;
    let expires = day(s[7])?;
    // 尚未提供首次登录/过期时的强制改密流程；不能用正常登录绕过 shadow 的到期策略。
    if changed == Some(0)
        || expires.is_some_and(|day| today >= day)
        || changed
            .zip(maximum)
            .is_some_and(|(changed, max)| today > changed.saturating_add(max))
    {
        return None;
    }
    if purpose == Purpose::PasswordChange
        && uid != 0
        && changed
            .zip(minimum)
            .is_some_and(|(changed, min)| today < changed.saturating_add(min))
    {
        return None;
    }
    // musl initgroups/getgrouplist 将主组列为第一个成员；setgroups 接收这一完整列表。
    let mut groups = vec![gid];
    for row in group
        .lines()
        .filter(|row| !row.is_empty() && !row.starts_with('#'))
    {
        let fields: Vec<_> = row.split(':').collect();
        if fields.len() != 4 {
            return None;
        }
        let id: u32 = fields[2].parse().ok()?;
        if fields[3].split(',').any(|member| member == name) && !groups.contains(&id) {
            groups.push(id);
        }
    }
    Some((
        SessionUser {
            identity: Identity { uid, gid },
            groups,
            name: name.into(),
            home,
            runtime: PathBuf::from(format!("/run/user/{uid}")),
        },
        s[1].into(),
    ))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    const PASSWD: &str = "alice:x:1000:100:Alice:/home/alice:/bin/sh\n";
    fn shadow(hash: &str, changed: &str, max: &str, expiry: &str) -> String {
        format!("alice:{hash}:{changed}:0:{max}:7::{expiry}:\n")
    }
    #[test]
    fn passwordless_account_is_distinct_from_locked_accounts() {
        let empty = shadow("", "100", "100", "");
        let (user, hash) = parse(PASSWD, &empty, "", "alice", 150, Purpose::Login).unwrap();
        assert_eq!(user.name, "alice");
        assert!(hash.is_empty());
        for hash in ["!", "*", "!!", "!$6$salt$hash"] {
            assert!(
                parse(
                    PASSWD,
                    &shadow(hash, "100", "100", ""),
                    "",
                    "alice",
                    150,
                    Purpose::Login
                )
                .is_none()
            );
        }
    }
    #[test]
    fn minimum_change_age_restricts_password_change_without_blocking_login() {
        let shadow =
            shadow("$6$salt$hash", "100", "100", "").replace(":100:0:100:", ":100:80:100:");
        assert!(parse(PASSWD, &shadow, "", "alice", 150, Purpose::Login).is_some());
        assert!(parse(PASSWD, &shadow, "", "alice", 150, Purpose::PasswordChange).is_none());
        assert!(parse(PASSWD, &shadow, "", "alice", 180, Purpose::PasswordChange).is_some());
        let root = PASSWD.replace(":1000:", ":0:");
        assert!(parse(&root, &shadow, "", "alice", 150, Purpose::PasswordChange).is_some());
    }
    #[test]
    fn account_policy_is_fail_closed_and_groups_are_unique() {
        let valid = shadow("$6$salt$hash", "100", "100", "");
        let (user, _) = parse(
            PASSWD,
            &valid,
            "users:x:100:alice\nvideo:x:44:alice\nagain:x:44:alice\n",
            "alice",
            150,
            Purpose::Login,
        )
        .unwrap();
        assert_eq!(user.identity.uid, 1000);
        assert_eq!(user.groups, [100, 44]);
        for s in [
            shadow("!$6$x", "100", "100", ""),
            shadow("$6$x", "0", "100", ""),
            shadow("$6$x", "100", "2", ""),
            shadow("$6$x", "100", "100", "150"),
            shadow("$6$x", "bad", "100", ""),
        ] {
            assert!(parse(PASSWD, &s, "", "alice", 150, Purpose::Login).is_none());
        }
        assert!(parse(PASSWD, &valid, "broken", "alice", 150, Purpose::Login).is_none());
        assert!(parse(&PASSWD.repeat(2), &valid, "", "alice", 150, Purpose::Login).is_none());
        assert!(parse(PASSWD, &valid, "", "unknown", 150, Purpose::Login).is_none());
    }
}
