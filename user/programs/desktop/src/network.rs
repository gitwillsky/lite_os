//! 通知区域的网络链路采样；Linux 状态由 network-config 解释，UI 线程只接收不可变快照。
use network_config::{Link, LinkState};
use std::{io, sync::mpsc, thread, time::Duration};

pub(super) struct Network {
    links: Vec<Link>,
    updates: mpsc::Receiver<io::Result<Vec<Link>>>,
    // OWNER: 本对象独占停止发送端与线程；Drop 断开通道唤醒 recv_timeout，再 join 回收线程。
    // 一项容量限制 UI 暂停时的积压；缺少停止通道会让退出后采样线程继续访问文件系统。
    stop: Option<mpsc::SyncSender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Network {
    pub fn new() -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("network-tray".into())
            .spawn(move || {
                loop {
                    match sender.try_send(network_config::ethernet_links()) {
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    }
                    match stopped.recv_timeout(Duration::from_millis(500)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })?;
        Ok(Self {
            links: Vec::new(),
            updates,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub fn poll(&mut self) -> bool {
        let Ok(result) = self.updates.try_recv() else {
            return false;
        };
        let links = result.unwrap_or_default();
        let changed = self.links != links;
        self.links = links;
        changed
    }

    /// 汇总优先显示已连接接口，其次是断开接口；全禁用时仍保留入口。
    pub fn current(&self) -> Option<&Link> {
        self.links
            .iter()
            .find(|link| link.state == LinkState::Connected)
            .or_else(|| {
                self.links
                    .iter()
                    .find(|link| link.state == LinkState::Disconnected)
            })
            .or_else(|| self.links.first())
    }

    pub fn tooltip(&self) -> Option<String> {
        let link = self.current()?;
        let index = self
            .links
            .iter()
            .position(|candidate| candidate.index == link.index)?;
        let name = if index == 0 {
            "本地连接".to_owned()
        } else {
            format!("本地连接 {}", index + 1)
        };
        let status = match link.state {
            LinkState::Connected => "已连接",
            LinkState::Disconnected => "网络电缆被拔出",
            LinkState::Disabled => "禁用",
        };
        Some(format!("{name}：{status}"))
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
