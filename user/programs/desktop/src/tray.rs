//! 通知区采样与命令的唯一后台 owner；网络/USB 快照只在 UI 线程中应用。
use linux_uapi::usb::{self, Device};
use network_config::{Link, LinkState};
use std::{io, sync::mpsc, thread, time::Duration};

struct Disable {
    index: u32,
    result: mpsc::SyncSender<io::Result<()>>,
}
struct Snapshot {
    links: io::Result<Vec<Link>>,
    devices: io::Result<Vec<Device>>,
}
/// 同一通知区的采样共用一个线程/截止，避免为两个低频状态各保留一份栈与唤醒。
/// UI 不执行 sysfs/配置 I/O；结果容量为一，忙时保留已排队快照，不积压采样。
pub(super) struct Tray {
    pub network: Network,
    pub usb: Usb,
    updates: mpsc::Receiver<Snapshot>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Tray {
    pub fn new() -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(1);
        let (requests, pending) = mpsc::sync_channel::<Disable>(1);
        let worker = thread::Builder::new()
            .name("notification-area".into())
            .spawn(move || {
                loop {
                    if matches!(
                        sender.try_send(Snapshot {
                            links: network_config::ethernet_links(),
                            devices: usb::devices()
                        }),
                        Err(mpsc::TrySendError::Disconnected(_))
                    ) {
                        break;
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
            network: Network {
                links: Vec::new(),
                requests: Some(requests),
                disabling: None,
            },
            usb: Usb {
                devices: Vec::new(),
            },
            updates,
            worker: Some(worker),
        })
    }
    pub fn poll(&mut self) -> io::Result<bool> {
        // 禁用完成与采样分别消费；命令失败不能阻止同轮 USB 状态刷新。
        let completed = self.network.poll_disable();
        let mut changed = completed.as_ref().copied().unwrap_or(false);
        if let Ok(snapshot) = self.updates.try_recv() {
            let links = snapshot.links.unwrap_or_default();
            changed |= links != self.network.links;
            self.network.links = links;
            if let Ok(devices) = snapshot.devices {
                changed |= devices != self.usb.devices;
                self.usb.devices = devices;
            }
        }
        completed.map(|_| changed)
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        // 唯一请求发送端断开会唤醒等待；join 后才释放视图和结果接收者，不遗留桌面后台线程。
        self.network.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) struct Network {
    links: Vec<Link>,
    // OWNER: 一个通知区至多一个禁用请求；有界准入避免重复修改接口和拖延退出。
    requests: Option<mpsc::SyncSender<Disable>>,
    disabling: Option<mpsc::Receiver<io::Result<()>>>,
}
impl Network {
    fn poll_disable(&mut self) -> io::Result<bool> {
        if let Some(result) = &self.disabling {
            match result.try_recv() {
                Ok(result) => {
                    self.disabling = None;
                    result?;
                    return Ok(true);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disabling = None;
                    return Err(io::Error::other("网络配置任务已停止"));
                }
            }
        }
        Ok(false)
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

pub(super) struct Usb {
    devices: Vec<Device>,
}
impl Usb {
    pub fn connected(&self) -> bool {
        !self.devices.is_empty()
    }
    pub fn tooltip(&self) -> Option<String> {
        self.connected()
            .then(|| format!("拔出或弹出硬件（{} 个设备）", self.devices.len()))
    }
}
