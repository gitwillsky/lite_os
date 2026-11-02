//! resolvd：把原生 netcfg 正在生效的 DNS 写入 Linux 的 `/etc/resolv.conf`（init 监督，root 运行）。
//!
//! 1. 经 `/dev/network-settings` 取设置快照，按接口顺序收集 DNS（去重，至多 3 个，与 musl 的 `MAXNS` 一致）；
//! 2. 内容变化时以“同目录临时文件 → fsync → rename”原子替换，读者只会看到完整的旧内容或新内容；
//! 3. 阻塞等待设置代际变化后重复；端点暂不可用（netcfg 或 linuxd 正在重启）时退避重试。
use std::{
    fs,
    io::{self, Write},
    path::Path,
    thread,
    time::Duration,
};

const RESOLV_CONF: &str = "/etc/resolv.conf";
/// 设置端点暂不可用时的重试间隔。
const RETRY: Duration = Duration::from_secs(2);

fn render(snapshot: &network_config::Snapshot) -> String {
    let mut text =
        String::from("# 由 resolvd 依据网络设置生成；修改请使用“Internet 协议 (TCP/IP) 属性”。\n");
    for server in snapshot.nameservers() {
        text.push_str(&format!("nameserver {server}\n"));
    }
    text
}

fn store(text: &str) -> io::Result<()> {
    let path = Path::new(RESOLV_CONF);
    if fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    let temporary = path.with_file_name(format!(".resolv.conf.{}.new", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn main() {
    let mut generation = 0;
    loop {
        let result = network_config::snapshot().and_then(|snapshot| {
            store(&render(&snapshot))?;
            network_config::wait(snapshot.generation)
        });
        match result {
            Ok(next) => generation = next,
            Err(error) => {
                eprintln!("resolvd: {error} (generation {generation})");
                thread::sleep(RETRY);
            }
        }
    }
}
