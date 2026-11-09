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
use core::future::{Future, poll_fn};
use core::task::Poll;

use device_protocol::directory::DirectoryClient;
use launch_protocol::driver_host::{NODE_BYTES, Node};
use launch_protocol::driver_update;
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::task::{CRASH_LIMIT, CrashLoop};
use native_runtime::{Bti, Channel, Event, Job, Message, Process, Resource, Vmo};

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
    directory: DirectoryClient,
    open_directory: DirectoryClient,
}

fn prepare(
    bound: &BoundNode,
    image: Option<&Vmo>,
) -> Result<(Instance, native_runtime::launch::Prepared, Event), Status> {
    let name = b"driver-host";
    let job = Job::create(crate::handles::job())?;
    let result = (|| {
        let (process, vmar) = Process::create(&job, name, 0)?;
        let runtime_vmar = vmar.duplicate(rights::SAME)?;
        let (client, server) = Channel::create()?;
        let (open, open_server) = Channel::create()?;
        let ready = Event::create()?;
        let mut handles = alloc::vec![
            server.into_handle(),
            bound.bti.duplicate(rights::SAME)?.into_handle(),
            ready.duplicate(rights::SAME)?.into_handle(),
            open_server.into_handle(),
        ];
        if let Some(irq) = &bound.irq {
            handles.push(irq.duplicate(rights::SAME)?.into_handle());
        }
        if let Some(msi) = &bound.msi {
            handles.push(msi.duplicate(rights::SAME)?.into_handle());
        }
        for mmio in &bound.mmio {
            handles.push(mmio.duplicate(rights::SAME)?.into_handle());
        }
        let updated = image
            .map(|image| {
                let bytes = crate::image_bytes(image)?;
                let note = driver_update::note(driver_update::HOST_NOTE);
                if !native_runtime::elf::has_note(&bytes, &note)? {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                native_runtime::elf::Image::parse(&bytes)
            })
            .transpose()?;
        let prepared = native_runtime::launch::prepare(native_runtime::launch::Launch {
            process: &process,
            image_vmar: &vmar,
            runtime_vmar,
            name,
            image: updated
                .as_ref()
                .unwrap_or_else(|| crate::handles::host_image()),
            arguments: &bound.node.encode(),
            handles,
            stack_size: HOST_STACK_SIZE,
        })?;
        Ok((process, client, open, prepared, ready))
    })();
    match result {
        Ok((process, client, open, prepared, ready)) => Ok((
            Instance {
                job,
                process,
                directory: DirectoryClient::new(client),
                open_directory: DirectoryClient::new(open),
            },
            prepared,
            ready,
        )),
        Err(status) => {
            let _ = native_runtime::task::kill(&job);
            Err(status)
        }
    }
}

impl Instance {
    async fn stop(&self, executor: &'static Executor) -> Result<(), Status> {
        native_runtime::task::kill(&self.job)?;
        executor.on_signals(&self.job, signals::TERMINATED).await?;
        Ok(())
    }
}

/// 把宿主目录中 ID 大于 `cursor` 的设备并入汇聚目录。
///
/// # Returns
///
/// 宿主报告启动枚举已完成且已无更多设备时为 `true`。
fn mirror(instance: &Instance, cursor: &mut u64, forwarded: &mut Vec<u64>) -> Result<bool, Status> {
    loop {
        let (enumerated, entries) = instance.directory.list(*cursor)?;
        if entries.is_empty() {
            return Ok(enumerated);
        }
        for entry in entries {
            forwarded.push(crate::handles::directory().add(&instance.open_directory, &entry)?);
            *cursor = entry.id;
        }
    }
}

fn retire(forwarded: &mut Vec<u64>) {
    for id in forwarded.drain(..) {
        if let Err(status) = crate::handles::directory().remove(id) {
            error!("device withdrawal failed: {status}");
        }
    }
}

async fn activate(
    executor: &'static Executor,
    instance: &Instance,
    prepared: native_runtime::launch::Prepared,
    ready: &Event,
) -> Result<(), Status> {
    prepared.start(&instance.process)?;
    let deadline =
        native_runtime::system::monotonic() + launch_protocol::driver_update::START_TIMEOUT;
    let mut bound = core::pin::pin!(executor.on_signals(ready, signals::USER_0));
    let mut exited = core::pin::pin!(executor.on_signals(&instance.process, signals::TERMINATED));
    let mut timeout = core::pin::pin!(executor.sleep_until(deadline));
    poll_fn(|cx| {
        if exited.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(native_abi::ERR_PEER_CLOSED));
        }
        if timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(native_abi::ERR_TIMED_OUT));
        }
        bound.as_mut().poll(cx).map(|result| result.map(drop))
    })
    .await
}

struct Running {
    instance: Instance,
    cursor: u64,
    forwarded: Vec<u64>,
    watch: Option<Event>,
}

impl Running {
    fn new(instance: Instance) -> Self {
        Self {
            instance,
            cursor: 0,
            forwarded: Vec::new(),
            watch: None,
        }
    }

    fn mirror(&mut self) -> Result<bool, Status> {
        mirror(&self.instance, &mut self.cursor, &mut self.forwarded)
    }

    async fn activate(
        &mut self,
        executor: &'static Executor,
        prepared: native_runtime::launch::Prepared,
        ready: &Event,
    ) -> Result<(), Status> {
        let result = activate(executor, &self.instance, prepared, ready)
            .await
            .and_then(|()| self.mirror().map(drop));
        if result.is_err() && self.stop(executor).await.is_err() {
            // 必须确认候选宿主已失去硬件访问权，才能复用节点 BTI。
            native_runtime::task::exit_process(-1);
        }
        result
    }

    async fn stop(&mut self, executor: &'static Executor) -> Result<(), Status> {
        self.instance.stop(executor).await?;
        retire(&mut self.forwarded);
        Ok(())
    }
}

/// 已冻结节点的可移交状态。移除的节点仍保有 BTI，直到系统重启。
pub(crate) struct Checkpoint {
    pub(crate) bound: BoundNode,
    active: Option<(Running, CrashLoop, Option<Vmo>)>,
}

fn checkpoint(
    bound: &BoundNode,
    active: Option<(&Running, &CrashLoop, Option<&Vmo>)>,
    output: &Channel,
) -> Result<(), Status> {
    let mut bytes = bound.node.encode().to_vec();
    let mut handles = alloc::vec![bound.bti.duplicate(rights::SAME)?.into_handle()];
    if let Some(irq) = &bound.irq {
        handles.push(irq.duplicate(rights::SAME)?.into_handle());
    }
    if let Some(msi) = &bound.msi {
        handles.push(msi.duplicate(rights::SAME)?.into_handle());
    }
    for mmio in &bound.mmio {
        handles.push(mmio.duplicate(rights::SAME)?.into_handle());
    }
    if let Some((running, crashes, image)) = active {
        bytes.extend_from_slice(&running.cursor.to_le_bytes());
        for instant in crashes.snapshot() {
            bytes.extend_from_slice(&instant.to_le_bytes());
        }
        for id in &running.forwarded {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        handles.extend([
            running.instance.job.duplicate(rights::SAME)?.into_handle(),
            running
                .instance
                .process
                .duplicate(rights::SAME)?
                .into_handle(),
            running.instance.directory.duplicate()?.into_handle(),
            running.instance.open_directory.duplicate()?.into_handle(),
            running
                .watch
                .as_ref()
                .ok_or(native_abi::ERR_BAD_STATE)?
                .duplicate(rights::SAME)?
                .into_handle(),
        ]);
        if let Some(image) = image {
            handles.push(image.duplicate(rights::SAME)?.into_handle());
        }
    }
    output.write(&bytes, handles)
}

impl Checkpoint {
    /// 解码节点状态和对应的原生能力，供新监督者接管。
    pub(crate) fn decode(message: Message) -> Result<Self, Status> {
        let invalid = native_abi::ERR_INVALID_ARGS;
        let (node_bytes, state) = message.bytes.split_at_checked(NODE_BYTES).ok_or(invalid)?;
        let node = Node::decode(node_bytes).ok_or(invalid)?;
        let mut handles = message.handles.into_iter();
        let bti = Bti::from_handle(handles.next().ok_or(invalid)?);
        let irq = if node.has_irq() {
            Some(Resource::from_handle(handles.next().ok_or(invalid)?))
        } else {
            None
        };
        let msi = if node.has_msi() {
            Some(Resource::from_handle(handles.next().ok_or(invalid)?))
        } else {
            None
        };
        let mmio_count = match node.bus {
            launch_protocol::driver_host::Bus::VirtioMmio { .. } => 1,
            launch_protocol::driver_host::Bus::Pci { bars, .. } => {
                1 + bars.iter().flatten().count()
            }
        };
        let mmio = handles
            .by_ref()
            .take(mmio_count)
            .map(Resource::from_handle)
            .collect::<Vec<_>>();
        if mmio.len() != mmio_count {
            return Err(invalid);
        }
        let active = if state.is_empty() {
            None
        } else {
            let (words, remainder) = state.as_chunks::<8>();
            if !remainder.is_empty() || words.len() <= CRASH_LIMIT {
                return Err(invalid);
            }
            let (fixed, published) = words.split_at(CRASH_LIMIT + 1);
            let cursor = u64::from_le_bytes(fixed[0]);
            let crashes = CrashLoop::restore(core::array::from_fn(|index| {
                i64::from_le_bytes(fixed[index + 1])
            }));
            let forwarded = published.iter().copied().map(u64::from_le_bytes).collect();
            let job = Job::from_handle(handles.next().ok_or(invalid)?);
            let process = Process::from_handle(handles.next().ok_or(invalid)?);
            let directory =
                DirectoryClient::new(Channel::from_handle(handles.next().ok_or(invalid)?));
            let open_directory =
                DirectoryClient::new(Channel::from_handle(handles.next().ok_or(invalid)?));
            let watch = Some(Event::from_handle(handles.next().ok_or(invalid)?));
            let image = handles.next().map(Vmo::from_handle);
            let instance = Instance {
                job,
                process,
                directory,
                open_directory,
            };
            Some((
                Running {
                    instance,
                    cursor,
                    forwarded,
                    watch,
                },
                crashes,
                image,
            ))
        };
        if handles.next().is_some() {
            return Err(invalid);
        }
        Ok(Self {
            bound: BoundNode {
                node,
                bti,
                irq,
                msi,
                mmio,
            },
            active,
        })
    }

    /// 在根服务提交后继续监督现有宿主，或保留已撤下节点的 DMA 隔离。
    pub(crate) async fn resume(self, executor: &'static Executor, control: Channel) {
        let (running, crashes, image) = self.active.map_or_else(
            || (None, CrashLoop::new(), None),
            |(running, crashes, image)| (Some(running), crashes, image),
        );
        supervise_inner(executor, self.bound, None, control, running, crashes, image).await;
    }
}

enum Command {
    Update(Vmo),
    Freeze {
        output: Channel,
        decision: Event,
        reply: Channel,
    },
}

impl Command {
    fn decode(message: Message) -> Option<Self> {
        let mut handles = message.handles;
        match handles.len() {
            1 => Some(Self::Update(Vmo::from_handle(handles.pop()?))),
            3 => {
                let reply = Channel::from_handle(handles.pop()?);
                let decision = Event::from_handle(handles.pop()?);
                let output = Channel::from_handle(handles.pop()?);
                Some(Self::Freeze {
                    output,
                    decision,
                    reply,
                })
            }
            _ => None,
        }
    }
}

async fn freeze(
    executor: &'static Executor,
    bound: &BoundNode,
    active: Option<(&Running, &CrashLoop, Option<&Vmo>)>,
    output: Channel,
    decision: Event,
    reply: Channel,
) -> bool {
    let status = checkpoint(bound, active, &output)
        .err()
        .unwrap_or(native_abi::OK);
    drop(output);
    let _ = reply.write(
        &launch_protocol::driver_update::reply(status, native_abi::OK),
        Vec::new(),
    );
    status == native_abi::OK
        && executor
            .on_signals(&decision, signals::USER_0 | signals::USER_1)
            .await
            .is_ok_and(|decision| decision & signals::USER_0 != 0)
}

async fn retain_removed(
    executor: &'static Executor,
    bound: &BoundNode,
    control: &Channel,
) -> Option<(Running, Vmo)> {
    // 没有新宿主完成硬件复位时，BTI 仍要保活隔离页；丢弃它会让失控的 DMA 触及已复用的内存。
    while let Ok(message) = executor
        .read_until(control, native_abi::DEADLINE_INFINITE)
        .await
    {
        match Command::decode(message) {
            Some(Command::Freeze {
                output,
                decision,
                reply,
            }) => {
                if freeze(executor, bound, None, output, decision, reply).await {
                    return None;
                }
            }
            Some(Command::Update(image)) => {
                let result = async {
                    if !bound.node.driver.restart_on_crash() {
                        return Err(native_abi::ERR_NOT_SUPPORTED);
                    }
                    let (candidate, prepared, ready) = prepare(bound, Some(&image))?;
                    let mut running = Running::new(candidate);
                    running.activate(executor, prepared, &ready).await?;
                    Ok(running)
                }
                .await;
                let status = result.as_ref().err().copied().unwrap_or(native_abi::OK);
                let _ = control.write(&driver_update::reply(status, native_abi::OK), Vec::new());
                if let Ok(running) = result {
                    return Some((running, image));
                }
            }
            None => {}
        }
    }
    native_runtime::task::exit_process(-1);
}

async fn replace(
    executor: &'static Executor,
    bound: &BoundNode,
    active: &mut Running,
    previous: &mut Option<Vmo>,
    image: Vmo,
) -> (Status, Status) {
    let (candidate, prepared, ready) = match prepare(bound, Some(&image)) {
        Ok(candidate) => candidate,
        Err(status) => return (status, native_abi::OK),
    };
    let mut candidate = Running::new(candidate);
    if let Err(status) = active.stop(executor).await {
        error!("cannot retire driver host before replacement: {status}");
        native_runtime::task::exit_process(-1);
    }
    let Err(status) = candidate.activate(executor, prepared, &ready).await else {
        *active = candidate;
        *previous = Some(image);
        return (native_abi::OK, native_abi::OK);
    };
    // 候选 Job 已完全终止。DMA 隔离保留到回退驱动重置设备。
    let (fallback, prepared, ready) = match prepare(bound, previous.as_ref()) {
        Ok(fallback) => fallback,
        Err(failure) => return (status, failure),
    };
    let mut fallback = Running::new(fallback);
    match fallback.activate(executor, prepared, &ready).await {
        Ok(()) => {
            *active = fallback;
            (status, native_abi::OK)
        }
        Err(failure) => (status, failure),
    }
}

/// 监督单节点。只有本任务持有设备生命周期的写权限。
pub(crate) async fn supervise(
    executor: &'static Executor,
    bound: BoundNode,
    index: usize,
    admission: Arc<Admission>,
    control: Channel,
) {
    supervise_inner(
        executor,
        bound,
        Some((admission, index)),
        control,
        None,
        CrashLoop::new(),
        None,
    )
    .await;
}

async fn supervise_inner(
    executor: &'static Executor,
    bound: BoundNode,
    pending: Option<(Arc<Admission>, usize)>,
    control: Channel,
    existing: Option<Running>,
    crashes: CrashLoop,
    image: Option<Vmo>,
) {
    if (pending.is_some() || existing.is_some())
        && supervise_active(
            executor, &bound, pending, &control, existing, crashes, image,
        )
        .await
    {
        return;
    }
    while let Some((running, image)) = retain_removed(executor, &bound, &control).await {
        if supervise_active(
            executor,
            &bound,
            None,
            &control,
            Some(running),
            CrashLoop::new(),
            Some(image),
        )
        .await
        {
            return;
        }
    }
}

// 返回 true 表示已提交管理器交接。其他正常返回保留节点能力，等待修复映像。
async fn supervise_active(
    executor: &'static Executor,
    bound: &BoundNode,
    mut pending: Option<(Arc<Admission>, usize)>,
    control: &Channel,
    mut existing: Option<Running>,
    mut crashes: CrashLoop,
    mut image: Option<Vmo>,
) -> bool {
    let driver = bound.node.driver;
    let controls = executor.packets();
    let control_wait = control
        .duplicate(rights::SAME)
        .expect("host control duplicate");
    control_wait
        .wait_async(
            executor.port(),
            controls.key(),
            signals::READABLE | signals::PEER_CLOSED,
        )
        .expect("host control wait");
    'supervision: loop {
        let result = existing.take().map_or_else(
            || {
                prepare(bound, image.as_ref()).and_then(|(instance, prepared, _)| {
                    match prepared.start(&instance.process) {
                        Ok(()) => Ok(Running::new(instance)),
                        Err(status) => {
                            let _ = native_runtime::task::kill(&instance.job);
                            Err(status)
                        }
                    }
                })
            },
            Ok,
        );
        let mut running = match result {
            Ok(running) => running,
            Err(status) => {
                if pending.is_some() {
                    panic!("{driver:?} host launch failed: {status}");
                }
                error!(
                    "{driver:?}@{:x} host launch failed: {status}",
                    bound.node.address()
                );
                break;
            }
        };
        if let Some((admission, index)) = &pending {
            admission.wait_turn(executor, *index).await;
        }
        'instance: loop {
            let events = executor.packets();
            if running.watch.is_none() {
                running.watch = running.instance.directory.watch().ok();
            }
            running
                .instance
                .process
                .wait_async(executor.port(), events.key(), signals::TERMINATED)
                .expect("host termination wait");
            if let Some(watch) = &running.watch {
                let _ = watch.signal(signals::USER_0, 0);
                let _ = watch.wait_async(executor.port(), events.key(), signals::USER_0);
            }
            loop {
                if running.mirror() == Ok(true)
                    && let Some((admission, index)) = pending.take()
                {
                    admission.admit(index);
                }
                let mut event = core::pin::pin!(events.next());
                let mut command = core::pin::pin!(controls.next());
                let packet = poll_fn(|cx| {
                    if let Poll::Ready(packet) = event.as_mut().poll(cx) {
                        return Poll::Ready(packet);
                    }
                    command.as_mut().poll(cx)
                })
                .await;
                let trigger = packet.trigger & packet.observed;
                if trigger & signals::TERMINATED != 0 {
                    break 'instance;
                }
                if trigger & signals::USER_0 != 0 {
                    if let Some(watch) = &running.watch {
                        let _ = watch.signal(signals::USER_0, 0);
                        let _ = watch.wait_async(executor.port(), events.key(), signals::USER_0);
                    }
                    continue;
                }
                if trigger & signals::PEER_CLOSED != 0 {
                    native_runtime::task::exit_process(-1);
                }
                if trigger & signals::READABLE != 0 {
                    let Ok(message) = control.read() else {
                        continue;
                    };
                    control_wait
                        .wait_async(
                            executor.port(),
                            controls.key(),
                            signals::READABLE | signals::PEER_CLOSED,
                        )
                        .expect("host control wait");
                    let image_candidate = match Command::decode(message) {
                        Some(Command::Update(image)) => image,
                        Some(Command::Freeze {
                            output,
                            decision,
                            reply,
                        }) => {
                            let active = Some((&running, &crashes, image.as_ref()));
                            if freeze(executor, bound, active, output, decision, reply).await {
                                return true;
                            }
                            continue;
                        }
                        None => continue,
                    };
                    let process = running.instance.process.raw();
                    let (status, rollback) = if !driver.restart_on_crash() {
                        (native_abi::ERR_NOT_SUPPORTED, native_abi::OK)
                    } else {
                        replace(executor, bound, &mut running, &mut image, image_candidate).await
                    };
                    let _ = control.write(
                        &launch_protocol::driver_update::reply(status, rollback),
                        Vec::new(),
                    );
                    if rollback != native_abi::OK {
                        error!(
                            "{driver:?}@{:x} host rollback failed: {rollback}",
                            bound.node.address()
                        );
                        if running.stop(executor).await.is_err() {
                            native_runtime::task::exit_process(-1);
                        }
                        break 'supervision;
                    }
                    if process != running.instance.process.raw() {
                        break;
                    }
                }
            }
        }
        if let Some((admission, index)) = pending.take() {
            admission.admit(index);
        }
        let status = running
            .instance
            .process
            .basic()
            .map_or(-1, |basic| basic.return_code);
        if let Err(error) = running.stop(executor).await {
            error!("{driver:?} host cleanup failed: {error}");
            native_runtime::task::exit_process(-1);
        }
        if let Ok(bti) = bound.bti.info::<native_abi::BtiInfo>(native_abi::info::BTI)
            && bti.faults != 0
        {
            error!(
                "{driver:?} host: {} DMA accesses aborted by the IOMMU",
                bti.faults
            );
        }
        if !driver.restart_on_crash() || crashes.record() {
            error!(
                "{driver:?}@{:x} host terminated with status {status}; node withdrawn",
                bound.node.address()
            );
            break;
        }
        error!("{driver:?} host terminated with status {status}; restarting");
    }
    false
}
