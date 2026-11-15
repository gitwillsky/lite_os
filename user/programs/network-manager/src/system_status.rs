//! WinBox 主窗口底部的系统状态栏：主机与系统版本、CPU 占用、内存、运行时间和本地时间。
//! 数据来自 Linux procfs 与 uname，每秒在后台读取一次；本地时间按 `/etc/localtime` 换算。
use crate::child;
use liteos_ui::{Application, Background, Layout, Task, Widget, WidgetKind, Window};
use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct SystemStatus {
    _task: Task<()>,
}

/// 一次采样；CPU 占用需要与上一次的累计时间求差，首次采样不显示百分比。
struct Sample {
    uptime: Option<u64>,
    /// （总量, 可用）KiB。
    memory: Option<(u64, u64)>,
    cpu: Option<(u64, u64)>,
    unix: i64,
}

impl SystemStatus {
    /// 在 `parent` 末尾建立状态栏并开始周期采样；窗口关闭时任务随之结束。
    ///
    /// 错误：窗口已关闭或无法建立后台任务。
    pub(super) fn new(
        app: &mut Application,
        window: &Window,
        parent: &Widget,
        background: Background,
    ) -> liteos_ui::Result<Self> {
        let status = child(parent, WidgetKind::Status, "", Layout::default())?;
        status.set_status_parts(&[320, 90, 260, 150, -1])?;
        let host = linux_uapi::uts::read().map_or_else(
            |_| "LiteOS".to_owned(),
            |identity| {
                format!(
                    "{}　LiteOS / {} / {}",
                    identity.hostname.to_string_lossy(),
                    std::env::consts::ARCH,
                    identity.release.to_string_lossy(),
                )
            },
        );
        let clock = app.clock();
        let task = app.spawn_local(window, async move {
            let mut previous = None;
            let mut minute = None;
            loop {
                let Ok(load) = background.run(sample) else {
                    return;
                };
                let sample = load.await;
                // 控制面板修改时区后，长驻程序每分钟重读一次 /etc/localtime。
                if minute != Some(sample.unix / 60) {
                    minute = Some(sample.unix / 60);
                    let _ = calendar::zone::reload();
                }
                if status.set_text(&line(&host, &sample, previous)).is_err() {
                    return;
                }
                previous = sample.cpu.or(previous);
                let Ok(sleep) = clock.sleep(Duration::from_secs(1)) else {
                    return;
                };
                sleep.await;
            }
        })?;
        Ok(Self { _task: task })
    }
}

/// 状态栏各格文字，以制表符分隔。
fn line(host: &str, sample: &Sample, previous: Option<(u64, u64)>) -> String {
    let usage = sample
        .cpu
        .zip(previous)
        .and_then(|((busy, total), (was_busy, was_total))| {
            let elapsed = total.checked_sub(was_total).filter(|value| *value > 0)?;
            Some(busy.saturating_sub(was_busy) * 100 / elapsed)
        });
    let cpu = usage.map_or_else(|| "CPU —".to_owned(), |usage| format!("CPU {usage}%"));
    let memory = sample.memory.map_or_else(
        || "内存 —".to_owned(),
        |(total, available)| {
            format!(
                "内存 {} / {} / {}",
                mebibytes(total.saturating_sub(available)),
                mebibytes(available),
                mebibytes(total)
            )
        },
    );
    let uptime = sample.uptime.map_or_else(
        || "运行 —".to_owned(),
        |seconds| {
            let (days, rest) = (seconds / 86_400, seconds % 86_400);
            let clock = format!("{:02}:{:02}:{:02}", rest / 3600, rest / 60 % 60, rest % 60);
            if days == 0 {
                format!("运行 {clock}")
            } else {
                format!("运行 {days}天 {clock}")
            }
        },
    );
    let local = calendar::zone::to_local(sample.unix);
    let date = calendar::civil_from_days(local.div_euclid(86_400));
    let second = local.rem_euclid(86_400);
    format!(
        "{host}\t{cpu}\t{memory}\t{uptime}\t{}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.year,
        date.month,
        date.day,
        second / 3600,
        second / 60 % 60,
        second % 60
    )
}

fn mebibytes(kib: u64) -> String {
    format!("{:.1} MiB", kib as f64 / 1024.0)
}

fn meminfo(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn sample() -> Sample {
    let uptime = fs::read_to_string("/proc/uptime").ok().and_then(|text| {
        let seconds = text.split_whitespace().next()?;
        seconds.split('.').next()?.parse().ok()
    });
    let memory = fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
        Some((
            meminfo(&text, "MemTotal:")?,
            meminfo(&text, "MemAvailable:")?,
        ))
    });
    // /proc/stat 首行累计 user nice system idle iowait irq softirq steal；空闲含 idle 与 iowait。
    let cpu = fs::read_to_string("/proc/stat").ok().and_then(|text| {
        let fields: Vec<u64> = text
            .lines()
            .next()?
            .strip_prefix("cpu ")?
            .split_whitespace()
            .take(8)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let total: u64 = fields.iter().sum();
        let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
        Some((total - idle, total))
    });
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    Sample {
        uptime,
        memory,
        cpu,
        unix,
    }
}
