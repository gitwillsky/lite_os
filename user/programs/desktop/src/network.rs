//! 通知区域的网络链路采样；Linux 状态由 network-config 解释，UI 线程只接收不可变快照。
use network_config::{Link, LinkState};
use std::{io, sync::mpsc, thread, time::Duration};

struct Disable {
    index: u32,
    result: mpsc::SyncSender<io::Result<()>>,
}

pub(super) struct Network {
    links: Vec<Link>,
    updates: mpsc::Receiver<io::Result<Vec<Link>>>,
    // OWNER: 本对象独占请求发送端与线程；Drop 断开通道唤醒 recv_timeout，再 join 回收线程。
    // 只接受一个禁用请求；无界排队会重复操作接口并拖延退出。
    requests: Option<mpsc::SyncSender<Disable>>,
    disabling: Option<mpsc::Receiver<io::Result<()>>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Network {
    pub fn new() -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(1);
        let (requests, pending) = mpsc::sync_channel::<Disable>(1);
        let worker = thread::Builder::new()
            .name("network-tray".into())
            .spawn(move || {
                loop {
                    match sender.try_send(network_config::ethernet_links()) {
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    }
                    match pending.recv_timeout(Duration::from_millis(500)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Ok(request) => {
                            let result = network_config::ethernet_links().and_then(|links| {
                                let link = links
                                    .iter()
                                    .find(|link| link.index == request.index)
                                    .ok_or_else(|| io::Error::other("网络连接已移除"))?;
                                network_config::set_enabled(&link.interface, false)
                            });
                            let _ = request.result.try_send(result);
                        }
                    }
                }
            })?;
        Ok(Self {
            links: Vec::new(),
            updates,
            requests: Some(requests),
            disabling: None,
            worker: Some(worker),
        })
    }

    pub fn poll(&mut self) -> io::Result<bool> {
        if let Some(result) = &self.disabling {
            match result.try_recv() {
                Ok(result) => {
                    self.disabling = None;
                    result?;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disabling = None;
                    return Err(io::Error::other("网络配置任务已停止"));
                }
            }
        }
        let Ok(result) = self.updates.try_recv() else {
            return Ok(false);
        };
        let links = result.unwrap_or_default();
        let changed = self.links != links;
        self.links = links;
        Ok(changed)
    }

    pub fn link(&self, index: u32) -> Option<&Link> {
        self.links.iter().find(|link| link.index == index)
    }

    pub fn disabling(&self) -> bool {
        self.disabling.is_some()
    }

    pub fn disable(&mut self, index: u32) -> io::Result<()> {
        if self.disabling() {
            return Ok(());
        }
        let (result, completion) = mpsc::sync_channel(1);
        self.requests
            .as_ref()
            .expect("network worker requests")
            .try_send(Disable { index, result })
            .map_err(|_| io::Error::other("网络配置任务不可用"))?;
        self.disabling = Some(completion);
        Ok(())
    }

    /// 汇总允许显示的接口，优先选择已连接链路，其次为断开、禁用。
    pub fn current(&self, settings: &desktop_settings::Settings) -> Option<&Link> {
        self.links
            .iter()
            .filter(|link| settings.network_icon_visible(&link.interface))
            .min_by_key(|link| match link.state {
                LinkState::Connected => 0,
                LinkState::Disconnected => 1,
                LinkState::Disabled => 2,
            })
    }

    pub fn tooltip(&self, settings: &desktop_settings::Settings) -> Option<String> {
        let link = self.current(settings)?;
        let status = match link.state {
            LinkState::Connected => "已连接",
            LinkState::Disconnected => "网络电缆被拔出",
            LinkState::Disabled => "禁用",
        };
        Some(format!("{}：{status}", link.name))
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
