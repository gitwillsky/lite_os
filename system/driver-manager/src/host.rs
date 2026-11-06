//! 驱动宿主的启动、设备镜像与监督（DFv2 driver host）。
//!
//! 每个节点一个监督任务，全部任务在 driver-manager 的单线程 executor 上推进（多来源监督者用 async，
//! 设备与文件系统的阻塞服务保持每连接一个线程）：
//! 1. 在独立子 Job 中启动 `driver-host`，交付编码后的节点、本宿主目录服务端与节点资源的副本；
//! 2. 把宿主目录中的设备以转发条目并入汇聚目录。初始枚举按节点顺序进行：等前一个节点完成后才并入，
//!    使汇聚目录的登记顺序与节点顺序一致；之后到达的设备（USB 热插拔）随到随并入；
//! 3. 宿主终止时撤下它的全部条目（已开会话随宿主 `PEER_CLOSED`）。驱动声明 `restart_on_crash`
//!    时重建宿主（崩溃循环时放弃），否则节点从拓扑移除。

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_protocol::directory::{DirectoryClient, Entry};
use native_runtime::executor::Executor;
use native_runtime::native_abi::{rights, signals};
use native_runtime::task::CrashLoop;
use native_runtime::{Channel, Event, Handle, Job, Process, Vmar};

use crate::nodes::BoundNode;

const HOST_STACK_SIZE: usize = 256 * 1024;

/// 初始枚举的顺序门：节点 `i` 的初始设备并入后置位 `admitted[i]`。
pub(crate) struct Admission {
    admitted: Vec<Event>,
}

impl Admission {
    /// 为 `count` 个节点建立顺序门。
    ///
    /// # Panics
    ///
    /// Event 创建失败。
    pub(crate) fn new(count: usize) -> Arc<Self> {
        Arc::new(Self {
            admitted: (0..count)
                .map(|_| Event::create().expect("admission event"))
                .collect(),
        })
    }

    async fn wait_turn(&self, executor: &'static Executor, index: usize) {
        if let Some(previous) = index.checked_sub(1) {
            let _ = executor
                .on_signals(&self.admitted[previous], signals::USER_0)
                .await;
        }
    }

    fn admit(&self, index: usize) {
        let _ = self.admitted[index].signal(0, signals::USER_0);
    }

    /// 等全部节点完成初始枚举。
    pub(crate) async fn wait_all(&self, executor: &'static Executor) {
        if let Some(last) = self.admitted.last() {
            let _ = executor.on_signals(last, signals::USER_0).await;
        }
    }
}

/// 一个运行中的宿主实例。
struct Instance {
    job: Job,
    process: Process,
    directory: Arc<DirectoryClient>,
}

fn launch(bound: &BoundNode) -> Instance {
    let driver = bound.node.driver;
    // 启动时只标识宿主角色，驱动绑定后由宿主进程设置设备名称。
    let name = b"driver-host";
    let label = alloc::format!("{driver:?} host");
    let job =
        Job::create(crate::handles::job()).unwrap_or_else(|status| panic!("{label} job: {status}"));
    let (process, vmar) =
        Process::create(&job, name, 0).unwrap_or_else(|status| panic!("{label} process: {status}"));
    let runtime_vmar: Vmar = vmar
        .duplicate(rights::SAME)
        .unwrap_or_else(|status| panic!("{label} vmar duplicate: {status}"));
    let (client, server) = Channel::create().expect("host directory channel");
    let duplicate = |handle: &Handle| {
        handle
            .duplicate(rights::SAME)
            .unwrap_or_else(|status| panic!("{label} resource duplicate: {status}"))
    };
    let mut handles = alloc::vec![
        server.into_handle(),
        duplicate(&bound.bti),
        bound
            .irq
            .as_ref()
            .map_or(Handle::invalid(), |irq| duplicate(irq)),
    ];
    handles.extend(bound.mmio.iter().map(|mmio| duplicate(mmio)));
    let arguments = bound.node.encode();
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &vmar,
        runtime_vmar,
        name,
        image: crate::handles::host_image(),
        arguments: &arguments,
        handles,
        stack_size: HOST_STACK_SIZE,
    })
    .unwrap_or_else(|status| panic!("{label} launch: {status}"));
    Instance {
        job,
        process,
        directory: Arc::new(DirectoryClient::new(client)),
    }
}

/// 把宿主目录中 ID 大于 `cursor` 的设备并入汇聚目录。
///
/// # Returns
///
/// 宿主报告启动枚举已完成且已无更多设备时为 `true`。
fn mirror(instance: &Instance, cursor: &mut u64, forwarded: &mut Vec<u64>) -> bool {
    loop {
        let Ok((enumerated, entries)) = instance.directory.list(*cursor) else {
            return false;
        };
        if entries.is_empty() {
            return enumerated;
        }
        for entry in entries {
            *cursor = entry.id;
            let class = entry.class;
            let description = entry.description.clone();
            let directory = instance.directory.clone();
            let open = alloc::boxed::Box::new(move || {
                directory
                    .open(&Entry {
                        id: entry.id,
                        class: entry.class,
                        description: entry.description.clone(),
                    })
                    .map(|opened| opened.handles)
            });
            match crate::handles::directory().add_forwarded(class, &description, open) {
                Ok(id) => forwarded.push(id),
                Err(status) => error!("device forwarding failed: {status}"),
            }
        }
    }
}

/// 运行一个宿主实例直到它终止；返回前撤下它的全部条目。
///
/// 宿主终止与目录变化登记到同一个 packet 流，按 packet 的触发 signal 区分；目录 `LIST` 是对宿主
/// 目录服务线程的短同步调用，不经过驱动代码。
async fn run(
    executor: &'static Executor,
    bound: &BoundNode,
    admission: Option<(&Admission, usize)>,
) -> i32 {
    let instance = launch(bound);
    let events = executor.packets();
    let watch = instance.directory.watch();
    instance
        .process
        .wait_async(executor.port(), events.key(), signals::TERMINATED)
        .expect("host termination wait");
    let mut cursor = 0;
    let mut forwarded = Vec::new();
    let mut pending = admission;
    loop {
        // 先清除再登记，然后读取：读取之后到达的变化会重新置位并投递，不会丢失。
        if let Ok(watch) = &watch {
            let _ = watch.signal(signals::USER_0, 0);
            let _ = watch.wait_async(executor.port(), events.key(), signals::USER_0);
        }
        if let Some((gate, index)) = pending {
            gate.wait_turn(executor, index).await;
        }
        let enumerated = mirror(&instance, &mut cursor, &mut forwarded);
        if enumerated && let Some((gate, index)) = pending.take() {
            gate.admit(index);
        }
        if events.next().await.trigger & signals::TERMINATED != 0 {
            break;
        }
    }
    // 宿主在完成初始枚举前终止：仍要放行后续节点，否则启动会永远等待。
    if let Some((gate, index)) = pending {
        gate.admit(index);
    }
    for id in forwarded {
        crate::handles::directory().remove(id);
    }
    // 宿主终止的常见原因是设备越界 DMA：把节点 BTI 上被 IOMMU 中止的访问数一并记录。
    if let Ok(info) = bound
        .bti
        .info::<native_runtime::native_abi::BtiInfo>(native_runtime::native_abi::info::BTI)
        && info.faults != 0
    {
        error!(
            "{:?} host: {} DMA accesses aborted by the IOMMU",
            bound.node.driver, info.faults
        );
    }
    let _ = native_runtime::task::kill(&instance.job);
    instance
        .process
        .basic()
        .map_or(-1, |basic| basic.return_code)
}

/// 监督节点 `index` 的宿主；只在节点被移除时返回。
pub(crate) async fn supervise(
    executor: &'static Executor,
    bound: BoundNode,
    index: usize,
    admission: Arc<Admission>,
) {
    let driver = bound.node.driver;
    let label = alloc::format!("{driver:?} host");
    let mut crashes = CrashLoop::new();
    let mut gate = Some((&*admission, index));
    loop {
        let status = run(executor, &bound, gate.take()).await;
        if !driver.restart_on_crash() {
            error!("{label} terminated with status {status}; node removed");
            return;
        }
        if crashes.record() {
            error!("{label} crash loop (last status {status}); node removed");
            return;
        }
        error!("{label} terminated with status {status}; restarting");
    }
}
