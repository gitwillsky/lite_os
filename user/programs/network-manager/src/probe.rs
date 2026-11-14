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
        let (program, arguments, timeout) = kind.command(target);
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
        let complete = result?;
        let activity = self.kind.activity();
        Ok(match outcome? {
            Outcome::Exited(status) if status.success() && complete => format!("{activity}已结束"),
            Outcome::Exited(status) if status.success() => format!("{activity}未返回完整结果"),
            Outcome::Exited(status) => status.code().map_or_else(
                || format!("{activity}进程已终止"),
                |code| format!("{activity}失败（退出码 {code}）"),
            ),
            Outcome::Cancelled => format!("已停止{activity}"),
            Outcome::TimedOut => format!("{activity}超时"),
        })
    }
}

async fn read_output(
    reader: &mut PipeReader,
    readiness: &mut AsyncFd,
    output: &Widget,
    kind: Kind,
) -> Result<bool, Error> {
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
            return Ok(!matches!(kind, Kind::Bandwidth { .. }) || report.finished);
        }
    }
}

#[derive(Default)]
struct Report {
    rows: Vec<String>,
    finished: bool,
}

impl Report {
    fn line(&mut self, kind: Kind, text: &str) {
        let Kind::Bandwidth { download, .. } = kind else {
            self.rows.push(text.into());
            return;
        };
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            // 加载器和进程在 JSON 流开启前也可能向 stderr 输出错误。
            self.rows.push(text.into());
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
                        "{start:.1}–{end:.1} 秒　{direction} {}",
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
                    self.rows.push(format!(
                        "{direction}平均速率　{}（接收端）",
                        bitrate(received)
                    ));
                    self.rows.push(format!("发送端平均速率　{}", bitrate(sent)));
                }
            }
            Some("error") => {
                if let Some(message) = data.as_str() {
                    self.rows.push(format!("带宽测试错误　{message}"));
                }
            }
            _ => {}
        }
    }
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
