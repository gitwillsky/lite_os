//! 系统采样只在后台执行；UI 线程保存只读结果，刷新请求与单调期限唤醒同一个 Future。
use crate::{
    gpu,
    menus::Page,
    processes::{self, Process},
    services,
    system::Monitor,
};
use liteos_ui::{Clock, Result};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
    task::{Poll, Waker},
    time::Duration,
};
pub struct Sampler {
    pub monitor: Monitor,
    previous_services: HashMap<u64, u64>,
    previous: HashMap<(u32, u64), u64>,
}
/// 一轮后台读取的进程和服务快照，以及 GPU 页请求的设备快照。
pub struct Sample {
    pub rows: Vec<Process>,
    pub gpu: Option<gpu::Snapshot>,
    pub services: services::Snapshot,
}
impl Sampler {
    pub fn new() -> Self {
        Self {
            monitor: Monitor::new(),
            previous_services: HashMap::new(),
            previous: HashMap::new(),
        }
    }
    /// 为 `page` 采样，返回继续持有差分基线的采样器和本轮只读结果。
    pub fn sample(mut self, page: Page) -> (Self, Sample) {
        self.monitor.sample();
        let rows = processes::list(&self.previous, &self.monitor);
        let gpu = (page == Page::Gpu).then(|| gpu::Snapshot::read(&rows));
        let services =
            services::Snapshot::read(&self.previous_services, self.monitor.interval_ticks);
        self.previous_services = services
            .rows
            .iter()
            .map(|service| (service.record.koid, service.record.cpu_ns))
            .collect();
        self.previous = rows
            .iter()
            .map(|row| ((row.pid, row.start_ticks), row.cpu_ticks))
            .collect();
        (
            self,
            Sample {
                rows,
                gpu,
                services,
            },
        )
    }
}
#[derive(Default)]
pub struct Refresh {
    epoch: Cell<u64>,
    waiter: RefCell<Option<Waker>>,
}
impl Refresh {
    pub fn request(&self) {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        let wake = self.waiter.borrow_mut().take();
        if let Some(wake) = wake {
            wake.wake();
        }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.get()
    }
    pub async fn wait(
        self: &Rc<Self>,
        clock: &Clock,
        seen: u64,
        interval: Option<u64>,
    ) -> Result<()> {
        let mut timer = interval
            .map(|ms| clock.sleep(Duration::from_millis(ms)))
            .transpose()?;
        poll_fn(|cx| {
            if self.epoch.get() != seen {
                return Poll::Ready(());
            }
            if timer
                .as_mut()
                .is_some_and(|timer| Pin::new(timer).poll(cx).is_ready())
            {
                return Poll::Ready(());
            }
            *self.waiter.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        self.waiter.borrow_mut().take();
        Ok(())
    }
}
