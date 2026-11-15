//! 标准网络探测命令及子进程监督。窗口只通过可读事件消费输出。

use linux_uapi::{
    process::{PidFd, SessionChild, SessionCommand, SessionIo},
    unix::{PollEvents, PollFd, poll, set_nonblocking},
};
use liteos_ui::{Application, AsyncFd, Background, Blocking, Widget};
use serde_json::Value;
use std::{
    cell::RefCell,
    ffi::OsString,
    io::{self, PipeReader, PipeWriter, Read},
    num::NonZeroU16,
    os::fd::AsFd,
    process::ExitStatus,
    rc::Rc,
    time::{Duration, Instant},
};

type Error = Box<dyn std::error::Error>;
const PROBE_OUTPUT_LIMIT: usize = 16 * 1024;
// 10 秒带宽测试每秒最多一个区间，JSON 的 TCP 统计比文本探测行大。
const BANDWIDTH_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Ping,
    Trace,
    Bandwidth { port: NonZeroU16, download: bool },
}

impl Kind {
    fn command(self, target: &str) -> (&'static str, Vec<OsString>, Duration) {
        let (program, options, timeout): (_, &[&str], _) = match self {
            Self::Ping => ("/bin/ping", &["-c", "4", "-W", "2", "-w", "8"], 12),
            Self::Trace => (
                "/bin/traceroute",
                &["-n", "-m", "30", "-q", "1", "-w", "1"],
                40,
            ),
            Self::Bandwidth { port, download } => {
                let mut args = vec![
                    format!("--client={target}").into(),
                    format!("--port={port}").into(),
                    "--time=10".into(),
                    "--interval=1".into(),
                    "--connect-timeout=3000".into(),
                    "--json-stream".into(),
                ];
                if download {
                    args.push("--reverse".into());
                }
                return ("/usr/bin/iperf3", args, Duration::from_secs(20));
            }
        };
        let args = options
            .iter()
            .copied()
            .chain(["--", target])
            .map(Into::into)
            .collect();
        (program, args, Duration::from_secs(timeout))
    }

    fn activity(self) -> &'static str {
        if matches!(self, Self::Bandwidth { .. }) {
            "带宽测试"
        } else {
            "探测"
        }
    }
}

enum Outcome {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

/// 关闭唯一取消写端会唤醒后台 poll，包括工作尚未开始的情形。
#[derive(Clone)]
pub(super) struct Cancel(Rc<RefCell<Option<PipeWriter>>>);

impl Cancel {
    pub(super) fn stop(&self) {
        self.0.borrow_mut().take();
    }
}

pub(super) struct Probe {
    reader: PipeReader,
    readiness: AsyncFd,
    completion: Blocking<io::Result<Outcome>>,
    cancel: Cancel,
    kind: Kind,
}

impl Probe {
    pub(super) fn start(
        app: &mut Application,
        background: &Background,
        kind: Kind,
        target: &str,
        domain: Option<&str>,
    ) -> Result<Self, Error> {
        let target = target.trim();
        if target.is_empty()
            || target.len() > 254
            || target
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err("请输入一个主机名或 IP 地址。".into());
        }
        let (mut program, mut arguments, timeout) = kind.command(target);
        if let Some(domain) = domain {
            // 只让受监督的子进程加入具名域。授权与 exec 沿用 ipconfig 的标准入口。
            arguments.splice(
                0..0,
                ["/domain", domain, "/exec", program].map(OsString::from),
            );
            program = "/bin/ipconfig";
        }
        let (reader, writer) = io::pipe()?;
        let (cancelled, cancel) = io::pipe()?;
        set_nonblocking(reader.as_fd(), true)?;
        let readiness = app.watch(reader.as_fd().try_clone_to_owned()?)?;
        let command = SessionCommand::new(program, arguments, SessionIo::Captured(writer.into()));
        let completion = background.run(move || supervise(command, cancelled, timeout))?;
        Ok(Self {
            reader,
            readiness,
            completion,
            cancel: Cancel(Rc::new(RefCell::new(Some(cancel)))),
            kind,
        })
    }

    pub(super) fn cancellation(&self) -> Cancel {
        self.cancel.clone()
    }

    pub(super) async fn display(mut self, output: &Widget) -> Result<String, Error> {
        let result = read_output(&mut self.reader, &mut self.readiness, output, self.kind).await;
        if result.is_err() {
            self.cancel.stop();
        }
        let outcome = self.completion.await;
        let (complete, summary) = result?;
        let activity = self.kind.activity();
        let state = match outcome? {
            Outcome::Exited(status) if status.success() && complete => format!("{activity}已结束"),
            Outcome::Exited(status) if status.success() => format!("{activity}未返回完整结果"),
            Outcome::Exited(status) => status.code().map_or_else(
                || format!("{activity}进程已终止"),
                |code| format!("{activity}失败（退出码 {code}）"),
            ),
            Outcome::Cancelled => format!("已停止{activity}"),
            Outcome::TimedOut => format!("{activity}超时"),
        };
        Ok(if summary.is_empty() {
            state
        } else {
            format!("{state}　{}", summary.join("；"))
        })
    }
}

async fn read_output(
    reader: &mut PipeReader,
    readiness: &mut AsyncFd,
    output: &Widget,
    kind: Kind,
) -> Result<(bool, Vec<String>), Error> {
    let limit = if matches!(kind, Kind::Bandwidth { .. }) {
        BANDWIDTH_OUTPUT_LIMIT
    } else {
        PROBE_OUTPUT_LIMIT
    };
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut consumed = 0;
    let mut report = Report::default();
    loop {
        readiness.readable().await?;
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if bytes.len() + count > limit {
            return Err("诊断输出过长，已停止。".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        let complete = if count == 0 {
            bytes.len()
        } else {
            bytes[consumed..]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(consumed, |newline| consumed + newline + 1)
        };
        if complete != consumed {
            let before = report.rows.len();
            let text = String::from_utf8_lossy(&bytes[consumed..complete]);
            for line in text.lines() {
                report.line(kind, line);
            }
            consumed = complete;
            if report.rows.len() != before {
                let rows: Vec<_> = report
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(index, line)| (index as u64 + 1, line.as_str()))
                    .collect();
                output.set_rows(&rows)?;
            }
        }
        if count == 0 {
            let complete = !matches!(kind, Kind::Bandwidth { .. }) || report.finished;
            return Ok((complete, report.summary));
        }
    }
}

/// 诊断窗口的分列结果：每行以制表符分隔，列与 [`Kind::columns`] 对应；
/// 统计行和无法归入表格的提示（如解析失败）进入状态栏摘要。
#[derive(Default)]
struct Report {
    rows: Vec<String>,
    summary: Vec<String>,
    finished: bool,
}

impl Kind {
    /// 结果表的列标题与宽度：Ping 按应答分列，逐跳探测按跳分列，带宽测试按时段分列。
    pub(super) fn columns(self) -> &'static [(&'static str, i32)] {
        match self {
            Self::Ping => &[
                ("序号", 56),
                ("主机", 200),
                ("时间", 96),
                ("大小", 64),
                ("TTL", 56),
            ],
            Self::Trace => &[("跳", 48), ("地址", 280), ("往返时间", 240)],
            Self::Bandwidth { .. } => &[("时段", 160), ("速率", 320)],
        }
    }
}

impl Report {
    fn line(&mut self, kind: Kind, text: &str) {
        match kind {
            Kind::Ping => self.ping(text),
            Kind::Trace => self.trace(text),
            Kind::Bandwidth { download, .. } => self.bandwidth(download, text),
        }
    }

    /// BusyBox ping：`64 bytes from 10.0.2.2: seq=0 ttl=255 time=0.476 ms`。
    /// 开头的 `PING …` 与分隔行省略，统计两行进入摘要。
    fn ping(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || text.starts_with("PING ") || text.starts_with("---") {
            return;
        }
        let reply = text.split_once(" bytes from ").and_then(|(size, rest)| {
            let (host, fields) = rest.split_once(": ")?;
            let field = |name: &str| {
                fields
                    .split_whitespace()
                    .find_map(|item| item.strip_prefix(name))
            };
            let time = fields.split_once("time=").map(|(_, time)| time.trim())?;
            Some(format!(
                "{}\t{host}\t{time}\t{size}\t{}",
                field("seq=").or_else(|| field("icmp_seq=")).unwrap_or(""),
                field("ttl=").unwrap_or(""),
            ))
        });
        match reply {
            Some(row) => self.rows.push(row),
            None => self
                .summary
                .push(ping_statistics(text).unwrap_or_else(|| text.into())),
        }
    }

    /// BusyBox traceroute：` 1  10.0.2.2 (10.0.2.2)  0.512 ms  0.3 ms  *`，首行标题进入摘要。
    fn trace(&mut self, text: &str) {
        let mut tokens = text.split_whitespace();
        let Some(hop) = tokens.next().filter(|hop| hop.parse::<u8>().is_ok()) else {
            if !text.trim().is_empty() {
                self.summary.push(text.trim().into());
            }
            return;
        };
        let rest: Vec<_> = tokens.collect();
        let mut address = Vec::new();
        let mut times = Vec::new();
        let mut index = 0;
        while index < rest.len() {
            match (rest[index], rest.get(index + 1)) {
                ("*", _) => times.push("*".to_owned()),
                (value, Some(&"ms")) if value.parse::<f64>().is_ok() => {
                    times.push(format!("{value} ms"));
                    index += 1;
                }
                (value, _) => address.push(value),
            }
            index += 1;
        }
        self.rows.push(format!(
            "{hop}\t{}\t{}",
            address.join(" "),
            times.join("  ")
        ));
    }

    fn bandwidth(&mut self, download: bool, text: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            // 加载器和进程在 JSON 流开启前也可能向 stderr 输出错误。
            if !text.trim().is_empty() {
                self.summary.push(text.trim().into());
            }
            return;
        };
        let direction = if download { "下载" } else { "上传" };
        let data = &frame["data"];
        match frame["event"].as_str() {
            Some("interval") => {
                let sample = &data["sum"];
                if let (Some(start), Some(end), Some(rate)) = (
                    sample["start"].as_f64(),
                    sample["end"].as_f64(),
                    sample["bits_per_second"].as_f64(),
                ) {
                    self.rows.push(format!(
                        "{start:.1}–{end:.1} 秒\t{direction} {}",
                        bitrate(rate)
                    ));
                }
            }
            Some("end") => {
                if let (Some(sent), Some(received)) = (
                    data["sum_sent"]["bits_per_second"].as_f64(),
                    data["sum_received"]["bits_per_second"].as_f64(),
                ) {
                    self.finished = true;
                    self.summary.push(format!(
                        "{direction}平均速率 {}（接收端），发送端 {}",
                        bitrate(received),
                        bitrate(sent)
                    ));
                }
            }
            Some("error") => {
                if let Some(message) = data.as_str() {
                    self.summary.push(format!("带宽测试错误：{message}"));
                }
            }
            _ => {}
        }
    }
}

/// 把 BusyBox ping 的两行统计译成界面语言；其他行原样返回 None。
fn ping_statistics(text: &str) -> Option<String> {
    if let Some(values) = text.strip_prefix("round-trip min/avg/max = ") {
        return Some(format!("往返 最小/平均/最大 {values}"));
    }
    let mut parts = text.split(", ");
    let sent = parts.next()?.strip_suffix(" packets transmitted")?;
    let received = parts.next()?.strip_suffix(" packets received")?;
    let loss = parts.next()?.strip_suffix(" packet loss")?;
    Some(format!("发送 {sent}，接收 {received}，丢包 {loss}"))
}

fn bitrate(bits_per_second: f64) -> String {
    if bits_per_second >= 1_000_000_000.0 {
        format!("{:.2} Gbit/s", bits_per_second / 1_000_000_000.0)
    } else if bits_per_second >= 1_000_000.0 {
        format!("{:.2} Mbit/s", bits_per_second / 1_000_000.0)
    } else if bits_per_second >= 1_000.0 {
        format!("{:.2} kbit/s", bits_per_second / 1_000.0)
    } else {
        format!("{bits_per_second:.0} bit/s")
    }
}

fn supervise(command: SessionCommand, stop: PipeReader, timeout: Duration) -> io::Result<Outcome> {
    // 排队期间被取消时，关闭命令的输出端并直接返回，避免开始已撤销的探测。
    let mut cancellation = [PollFd::new(stop.as_fd(), PollEvents::READ)];
    if poll(&mut cancellation, Some(Duration::ZERO))? != 0 {
        return Ok(Outcome::Cancelled);
    }
    let mut child = SessionChild::spawn(command)?;
    let exit = PidFd::open(child.id())?;
    let deadline = Instant::now() + timeout;
    let mut descriptors = [
        PollFd::new(stop.as_fd(), PollEvents::READ),
        PollFd::new(exit.as_fd(), PollEvents::READ),
    ];
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            child.terminate()?;
            return Ok(Outcome::TimedOut);
        };
        match poll(&mut descriptors, Some(remaining)) {
            Ok(0) => {
                child.terminate()?;
                return Ok(Outcome::TimedOut);
            }
            Ok(_) if descriptors[0].returned() != PollEvents::EMPTY => {
                child.terminate()?;
                return Ok(Outcome::Cancelled);
            }
            Ok(_) if descriptors[1].returned().contains(PollEvents::READ) => {
                // pidfd 就绪时 leader 已退出但未回收。先清理拥有的进程组，再读缓存的退出状态。
                child.terminate()?;
                return child
                    .try_wait()?
                    .map(Outcome::Exited)
                    .ok_or_else(|| io::Error::other("探测进程未返回退出状态"));
            }
            Ok(_) => return Err(io::Error::other("无法等待探测进程退出")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
