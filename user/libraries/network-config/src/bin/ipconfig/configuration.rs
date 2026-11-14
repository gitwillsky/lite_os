//! 配置审阅、整份导入与备份的命令行入口。

use network_config::{
    ConfigurationStatus, ManagementClient, Revision,
    configuration::{BackupId, CommitId, CommitResult, new_commit_id, save_new},
    format_lease_time,
};
use router_protocol::backup::Activity;
use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::Path,
    str::FromStr,
};

pub(super) fn run(
    client: &mut ManagementClient,
    option: &[u8],
    first: Option<&OsStr>,
    second: Option<&OsStr>,
) -> Option<io::Result<()>> {
    Some(
        match (option.to_ascii_lowercase().as_slice(), first, second) {
            (b"/schema", Some(output), None) => client
                .schema()
                .and_then(|document| save_new(Path::new(output), &document)),
            (b"/validate", Some(path), None) => (|| {
                let (revision, _) = client.validate(&fs::read(path)?)?;
                writeln!(io::stdout().lock(), "配置通过校验，代际 {revision}")
            })(),
            (b"/diff", Some(path), Some(output)) => (|| {
                let (revision, document) = client.difference(&fs::read(path)?)?;
                save_new(Path::new(output), &document)?;
                writeln!(io::stdout().lock(), "已导出差异，代际 {revision}")
            })(),
            (b"/export", Some(output), None) => (|| {
                let (revision, document) = client.export()?;
                save_new(Path::new(output), &document)?;
                writeln!(io::stdout().lock(), "已导出配置，代际 {revision}")
            })(),
            (b"/import", Some(path), Some(revision)) => (|| {
                let revision = parse(revision, "配置代际无效")?;
                let document = fs::read(path)?;
                submit(|commit| client.import(revision, commit, &document))
            })(),
            (b"/commit-status", Some(id), None) => (|| {
                let status = match client.commit_result(parse(id, "提交编号无效")?)? {
                    CommitResult::Saved => "该次提交已保存。可用 /watch-config 查看当前应用状态。",
                    CommitResult::Unknown => "当前记录无法确认该次提交。请重新导出并核对当前配置。",
                };
                writeln!(io::stdout().lock(), "{status}")
            })(),
            (b"/watch-config", None, None) => watch_configuration(client),
            (b"/backups", None, None) => list_backups(client),
            (b"/backup", None, None) => client
                .create_backup()
                .and_then(|id| writeln!(io::stdout().lock(), "已创建手动备份 {id}")),
            (b"/backup-export", Some(id), Some(output)) => (|| {
                let id: BackupId = parse(id, "备份编号无效")?;
                save_new(Path::new(output), &client.export_backup(id)?)?;
                writeln!(io::stdout().lock(), "已导出备份 {id}")
            })(),
            (b"/backup-delete", Some(id), None) => (|| {
                let id = parse(id, "备份编号无效")?;
                client.delete_backup(id)?;
                writeln!(io::stdout().lock(), "已删除备份 {id}")
            })(),
            (b"/backup-restore", Some(id), Some(revision)) => (|| {
                let id = parse(id, "备份编号无效")?;
                let revision = parse(revision, "配置代际无效")?;
                submit(|commit| client.restore_backup(revision, id, commit))
            })(),
            _ => return None,
        },
    )
}

fn parse<T: FromStr>(value: &OsStr, error: &'static str) -> io::Result<T> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn submit(send: impl FnOnce(CommitId) -> io::Result<Revision>) -> io::Result<()> {
    let commit = new_commit_id()?;
    {
        let mut output = io::stdout().lock();
        writeln!(output, "提交编号 {commit}")?;
        output.flush()?;
    }
    let revision = send(commit)?;
    writeln!(io::stdout().lock(), "配置已保存，代际 {revision}")
}

fn watch_configuration(client: &mut ManagementClient) -> io::Result<()> {
    loop {
        let snapshot = client.snapshot()?;
        let status = match snapshot.configuration_status {
            ConfigurationStatus::Ready => "已生效",
            ConfigurationStatus::RollingBack => "正在恢复原配置",
            ConfigurationStatus::Applying => "正在应用",
            ConfigurationStatus::Rejected => "无法加载已保存的配置",
        };
        {
            let mut output = io::stdout().lock();
            writeln!(output, "代际 {}  {status}", snapshot.generation)?;
            output.flush()?;
        }
        client.wait(snapshot.generation)?;
    }
}

fn list_backups(client: &mut ManagementClient) -> io::Result<()> {
    let listing = client.backups()?;
    let status = match listing.automatic.activity {
        Activity::Disabled => "已停用",
        Activity::Waiting => "等待中",
        Activity::Working => "正在执行",
        Activity::Retrying => "等待重试",
    };
    let mut output = io::stdout().lock();
    writeln!(output, "自动备份 {status}")?;
    if let Some(seconds) = listing.automatic.next_in_seconds {
        writeln!(output, "  下次运行将在 {seconds} 秒后")?;
    }
    if let Some(error) = listing.automatic.error {
        writeln!(output, "  上次失败的错误码为 {error}")?;
    }
    if listing.entries.is_empty() {
        return writeln!(output, "没有配置备份");
    }
    for entry in listing.entries {
        let kind = if entry.automatic { "自动" } else { "手动" };
        writeln!(
            output,
            "{}  {kind}  {} 字节  {}",
            entry.id,
            entry.size,
            format_lease_time(entry.modified_seconds)
        )?;
    }
    Ok(())
}
