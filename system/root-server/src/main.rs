#![no_std]
#![no_main]

//! 根服务：内核启动的唯一进程。
//!
//! 1. 从启动消息取得根 Job、根 resource、bootfs 与 DTB；
//! 2. 为根 Job 建立异常 channel：任何系统服务未处理的异常都在这里记录，随后交还内核终止该进程；
//! 3. 启动 linuxd（共享进程 + 首个成员进程，见 `launch_protocol::linuxd`）；
//! 4. linuxd 终止即系统无法继续服务：记录退出状态并关机。

extern crate alloc;

use alloc::vec;

use launch_protocol::linuxd;
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, ExceptionReport, rights, signals, startup};
use native_runtime::{Handle, Job, Process, Resource, Startup, Vmar, Vmo, error, info};

native_runtime::entry!(main);

const LINUXD_STACK_SIZE: usize = 512 * 1024;

fn main(mut startup: Startup) -> i64 {
    let job = Job::from_handle(startup.take(startup::root::JOB));
    let resource = Resource::from_handle(startup.take(startup::root::RESOURCE));
    let bootfs_vmo = Vmo::from_handle(startup.take(startup::root::BOOTFS));
    let device_tree = startup.take(startup::root::DEVICE_TREE);
    native_runtime::set_fatal_scope(
        job.duplicate(rights::SAME)
            .expect("root job duplicate")
            .into_handle(),
    );

    let exceptions = native_runtime::task::create_exception_channel(&job).expect("root exception channel");
    let bootfs = native_runtime::bootfs::Bootfs::map(&bootfs_vmo).expect("bootfs mapping");
    let linux = start_linuxd(&job, &resource, &bootfs, device_tree, &startup.arguments);

    let executor = Executor::create().expect("root executor");
    executor.spawn(async move {
        loop {
            if executor
                .on_signals(&exceptions, signals::READABLE | signals::PEER_CLOSED)
                .await
                .is_err()
            {
                return;
            }
            while let Ok(message) = exceptions.read() {
                report_exception(&message.bytes);
                // 丢弃异常令牌：交给内核的默认处理（终止出错进程）。
                drop(message.handles);
            }
        }
    });
    let linux = executor.block_on(async move {
        let _ = executor.on_signals(&linux, signals::TERMINATED).await;
        linux
    });
    let status = linux.basic().map_or(-1, |basic| basic.return_code);
    error!("linuxd terminated with status {status}; powering off");
    let _ = native_runtime::system::power(&resource, native_abi::system::POWER_OFF);
    -1
}

fn report_exception(bytes: &[u8]) {
    let mut report = ExceptionReport::default();
    let size = core::mem::size_of::<ExceptionReport>();
    if bytes.len() < size {
        error!("malformed exception report ({} bytes)", bytes.len());
        return;
    }
    native_runtime::bytes_of_mut(&mut report).copy_from_slice(&bytes[..size]);
    error!(
        "unhandled exception in thread {}: kind {} access {} address {:#x} syndrome {:#x} pc {:#x}",
        report.thread_koid,
        report.kind,
        report.access,
        report.address,
        report.syndrome,
        program_counter(&report.state),
    );
}

#[cfg(target_arch = "aarch64")]
fn program_counter(state: &native_abi::register::RegisterState) -> u64 {
    state.pc
}

#[cfg(target_arch = "riscv64")]
fn program_counter(state: &native_abi::register::RegisterState) -> u64 {
    state.pc
}

/// 启动 linuxd：共享进程承载映像，首个成员进程运行首个线程。
fn start_linuxd(
    root: &Job,
    resource: &Resource,
    bootfs: &native_runtime::bootfs::Bootfs,
    device_tree: Handle,
    command_line: &[u8],
) -> Process {
    let image = bootfs
        .find(linuxd::IMAGE)
        .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(linuxd::IMAGE).unwrap_or("")))
        .data;
    let job = Job::create(root).expect("linuxd job");
    let (shared, shared_vmar) = Process::create(&job, b"linuxd-image", native_abi::process::SHARED)
        .expect("linuxd shared process");
    let (process, _private) = shared.create_shared(b"linuxd").expect("linuxd process");
    let runtime_vmar: Vmar = shared_vmar.duplicate(rights::SAME).expect("linuxd vmar duplicate");
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &shared_vmar,
        runtime_vmar,
        name: b"linuxd",
        image,
        arguments: command_line,
        handles: vec![
            shared.into_handle(),
            job.duplicate(rights::SAME).expect("job duplicate").into_handle(),
            resource.duplicate(rights::SAME).expect("resource duplicate").into_handle(),
            device_tree,
        ],
        stack_size: LINUXD_STACK_SIZE,
    })
    .expect("linuxd launch");
    info!("linuxd started");
    process
}
