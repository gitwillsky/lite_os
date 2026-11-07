#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! driver-manager：驱动框架的管理者（Fuchsia DFv2 driver_manager）。
//!
//! 拥有根 resource、平台设备事实（DTB、PCI 枚举与 BAR 分配）与节点拓扑；为每个节点匹配驱动，
//! 在独立的 `driver-host` 进程中运行它，只交付该节点的 MMIO/IRQ resource 与 BTI；把各宿主的设备汇聚为
//! 一个由根服务持有的设备目录（devfs）。宿主终止时按驱动声明重启或移除节点；根服务提交后可移交监督权。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod handles;
mod host;
mod nodes;
mod pci;
mod platform;

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

use alloc::sync::Arc;
use alloc::vec::Vec;
use launch_protocol::{driver_manager, driver_update};
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::{Channel, Event, Startup, Vmo};

// OWNER: driver-manager 入口发布本映像的换版角色和协议代际，component-manager 在装载前校验。
// SAFETY: 链接脚本将独立 ELF note 保留于只读段，装载前可校验角色与协议代际。
#[used]
#[unsafe(link_section = ".note.liteos.lifecycle")]
static UPDATE_ABI: [u8; 24] =
    launch_protocol::driver_update::note(launch_protocol::driver_update::MANAGER_NOTE);

native_runtime::entry!(main);

fn main(mut startup: Startup) -> i64 {
    let device_tree = handles::install(&mut startup);
    let control = Channel::from_handle(startup.take(driver_manager::CONTROL));
    let executor = Executor::create().expect("driver-manager executor");
    let nodes =
        if startup.handles.len() > driver_manager::HANDOVER - native_abi::startup::ROLE_HANDLES {
            let transfer = Channel::from_handle(startup.take(driver_manager::HANDOVER));
            let decision = Event::from_handle(startup.take(driver_manager::DECISION));
            match executor.block_on(adopt(
                executor,
                transfer,
                decision,
                control.duplicate(rights::SAME).expect("control duplicate"),
            )) {
                Ok(nodes) => nodes,
                Err(status) => return i64::from(status),
            }
        } else {
            platform::initialize(&device_tree);
            let nodes = nodes::enumerate();
            let admission = host::Admission::new(nodes.len());
            let mut supervisors = Vec::with_capacity(nodes.len());
            for (index, bound) in nodes.into_iter().enumerate() {
                let (client, server) = Channel::create().expect("driver control");
                supervisors.push((bound.node.address(), client));
                executor.spawn(host::supervise(
                    executor,
                    bound,
                    index,
                    admission.clone(),
                    server,
                ));
            }
            executor.block_on(async move { admission.wait_all(executor).await });
            handles::directory()
                .enumerated()
                .expect("device publication");
            info!("driver-manager: devices published");
            supervisors
        };
    drop(device_tree);
    executor.spawn(serve(executor, control, nodes));
    executor.run()
}

pub(crate) fn image_bytes(image: &Vmo) -> Result<Vec<u8>, Status> {
    let size = image.size()?;
    if size == 0 || size > driver_update::IMAGE_LIMIT {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    let size = size as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    bytes.resize(size, 0);
    image.read(&mut bytes, 0)?;
    Ok(bytes)
}

async fn adopt(
    executor: &'static Executor,
    transfer: Channel,
    decision: Event,
    control: Channel,
) -> Result<Vec<(u64, Channel)>, Status> {
    let deadline = native_runtime::system::monotonic() + driver_update::START_TIMEOUT;
    let mut supervisors = Vec::new();
    let decision = Arc::new(decision);
    loop {
        let message = executor.read_until(&transfer, deadline).await?;
        if message.bytes == driver_update::VERSION && message.handles.is_empty() {
            break;
        }
        let checkpoint = host::Checkpoint::decode(message)?;
        let (client, server) = Channel::create()?;
        supervisors.push((checkpoint.bound.node.address(), client));
        let decision = decision.clone();
        executor.spawn(async move {
            if executor
                .on_signals(&decision, signals::USER_0 | signals::USER_1)
                .await
                .is_ok_and(|decision| decision & signals::USER_0 != 0)
            {
                checkpoint.resume(executor, server).await;
            }
        });
    }
    // 确认所有节点和后续控制连接均已接收，再等待根服务移交唯一监督权。
    control.write(driver_update::VERSION, Vec::new())?;
    let signals = executor
        .on_signals(&decision, signals::USER_0 | signals::USER_1)
        .await?;
    if signals & native_abi::signals::USER_0 == 0 {
        return Err(native_abi::ERR_CANCELED);
    }
    Ok(supervisors)
}

async fn serve(executor: &'static Executor, control: Channel, nodes: Vec<(u64, Channel)>) {
    while let Ok(message) = executor
        .read_until(&control, native_abi::DEADLINE_INFINITE)
        .await
    {
        let mut handles = message.handles.into_iter();
        let Some(reply) = handles.next().map(Channel::from_handle) else {
            continue;
        };
        let result = async {
            let address = u64::from_le_bytes(
                message
                    .bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            );
            let handle = handles.next().ok_or(native_abi::ERR_INVALID_ARGS)?;
            if address == 0 {
                let decision =
                    Event::from_handle(handles.next().ok_or(native_abi::ERR_INVALID_ARGS)?);
                let transfer = Channel::from_handle(handle);
                let deadline = native_runtime::system::monotonic() + driver_update::START_TIMEOUT;
                for (_, node) in &nodes {
                    let (ack, responder) = Channel::create()?;
                    match node.write(
                        &[],
                        alloc::vec![
                            transfer.duplicate(rights::SAME)?.into_handle(),
                            decision.duplicate(rights::SAME)?.into_handle(),
                            responder.into_handle(),
                        ],
                    ) {
                        Ok(()) => {}
                        // 已被移除的设备不再拥有活动宿主。
                        Err(native_abi::ERR_PEER_CLOSED) => continue,
                        Err(status) => return Err(status),
                    }
                    let response = match executor.read_until(&ack, deadline).await {
                        Ok(response) => response,
                        Err(status) => return Err(status),
                    };
                    let status = i32::from_le_bytes(
                        response
                            .bytes
                            .get(..4)
                            .and_then(|bytes| bytes.try_into().ok())
                            .ok_or(native_abi::ERR_INVALID_ARGS)?,
                    );
                    if status != native_abi::OK {
                        return Err(status);
                    }
                }
                transfer.write(driver_update::VERSION, Vec::new())?;
                return Ok(driver_update::reply(native_abi::OK, native_abi::OK));
            }
            let (_, node) = nodes
                .iter()
                .find(|(id, _)| *id == address)
                .ok_or(native_abi::ERR_NOT_FOUND)?;
            node.write(&[], alloc::vec![handle])?;
            executor
                .read_until(node, native_abi::DEADLINE_INFINITE)
                .await?
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)
        }
        .await;
        let response = result.unwrap_or_else(|status| driver_update::reply(status, native_abi::OK));
        let _ = reply.write(&response, Vec::new());
    }
    native_runtime::task::exit_process(-1);
}
