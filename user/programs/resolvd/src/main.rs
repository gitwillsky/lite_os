//! resolvd：把原生 router_config 正在生效的 DNS 写入 Linux 的 `/etc/resolv.conf`（init 监督，root 运行）。
//!
//! 1. 经 router-api 取设置快照，按接口顺序收集 DNS（去重，至多 3 个，与 musl 的 `MAXNS` 一致）；
//! 2. 内容变化时以“同目录临时文件 → fsync → rename”原子替换，读者只会看到完整的旧内容或新内容；
//! 3. 等待设置代际变化后重复；管理网关或原生设置服务暂不可用时退避重试。
use std::{
    fs,
    io::{self, Write},
    path::Path,
    thread,
    time::Duration,
};

/// musl 1.2.6 的消费者容量，不反向限制 LiteOS 原生配置或 DHCP 租约。
const MUSL_MAXNS: usize = 3;
/// 256-byte 读取缓冲，扣除 search 前缀、换行和结尾 NUL。
const MUSL_SEARCH_BYTES: usize = 247;
const RESOLV_CONF: &str = "/etc/resolv.conf";
/// 设置端点暂不可用时的重试间隔。
const RETRY: Duration = Duration::from_secs(2);

fn render(snapshot: &network_config::Snapshot) -> String {
    let mut text =
        String::from("# 由 resolvd 依据网络设置生成；修改请使用“Internet 协议 (TCP/IP) 属性”。\n");
    for server in snapshot.nameservers().into_iter().take(MUSL_MAXNS) {
        match server {
            std::net::SocketAddr::V6(server) if server.scope_id() != 0 => {
                text.push_str(&format!(
                    "nameserver {}%{}\n",
                    server.ip(),
                    server.scope_id()
                ));
            }
            _ => text.push_str(&format!("nameserver {}\n", server.ip())),
        }
    }
    let mut length = 0;
    let search: Vec<_> = snapshot
        .search_domains()
        .into_iter()
        .take_while(|suffix| {
            let next = length + suffix.len() + usize::from(length != 0);
            if next > MUSL_SEARCH_BYTES {
                return false;
            }
            length = next;
            true
        })
        .collect();
    if !search.is_empty() {
        text.push_str("search ");
        text.push_str(&search.join(" "));
        text.push('\n');
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
    loop {
        let result = network_config::snapshot().and_then(|snapshot| {
            store(&render(&snapshot))?;
            network_config::wait(snapshot.generation)
        });
        match result {
            Ok(_) => {}
            Err(error) => {
                eprintln!("resolvd: {error}");
                thread::sleep(RETRY);
            }
        }
    }
}
