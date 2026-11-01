#![no_std]
#![no_main]

//! 根服务：内核启动的唯一进程。
//!
//! 1. 从启动消息取得根 Job、根 resource、bootfs 与 DTB；
//! 2. 为根 Job 建立异常 channel：任何系统服务未处理的异常都在这里记录，随后交还内核终止该进程；
//! 3. 创建设备目录 channel 与 ext4d、netd 的控制 channel，启动 devmgr（持有 DTB 与目录写端）、ext4d 与 netd
//!    （各持控制服务端）以及 linuxd（共享进程 + 首个成员进程，持有目录读端与两个控制客户端）；
//! 4. 任一服务终止即系统无法继续服务：记录退出状态并关机。

extern crate alloc;

use alloc::vec;

use launch_protocol::{devmgr, ext4d, linuxd, netd};
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, ExceptionReport, rights, signals, startup};
use native_runtime::{Channel, Handle, Job, Process, Resource, Startup, Vmar, Vmo, error, info};

native_runtime::entry!(main);

const LINUXD_STACK_SIZE: usize = 512 * 1024;
const DEVMGR_STACK_SIZE: usize = 256 * 1024;
const EXT4D_STACK_SIZE: usize = 256 * 1024;
const NETD_STACK_SIZE: usize = 256 * 1024;

fn main(mut startup: Startup) -> i64 {
    let job = Job::from_handle(startup.take(startup::root::JOB));
    let resource = ROOT_RESOURCE.call_once(|| Resource::from_handle(startup.take(startup::root::RESOURCE)));
    let bootfs_vmo = Vmo::from_handle(startup.take(startup::root::BOOTFS));
    let device_tree = startup.take(startup::root::DEVICE_TREE);
    native_runtime::set_fatal_scope(
        job.duplicate(rights::SAME)
            .expect("root job duplicate")
            .into_handle(),
    );

    let exceptions = native_runtime::task::create_exception_channel(&job).expect("root exception channel");
    let bootfs = native_runtime::bootfs::Bootfs::map(&bootfs_vmo).expect("bootfs mapping");
    let (directory_reader, directory_writer) = Channel::create().expect("device directory channel");
    let (ext4_client, ext4_server) = Channel::create().expect("ext4d control channel");
    let (network_client, network_server) = Channel::create().expect("netd control channel");
    let devices = start_devmgr(&job, resource, &bootfs, device_tree, directory_writer);
    let filesystems = start_service(&job, &bootfs, ext4d::IMAGE, b"ext4d", ext4_server, EXT4D_STACK_SIZE);
    let network = start_service(&job, &bootfs, netd::IMAGE, b"netd", network_server, NETD_STACK_SIZE);
    let linux = start_linuxd(
        &job,
        resource,
        &bootfs,
        directory_reader,
        [ext4_client, network_client],
        &startup.arguments,
    );

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
    executor.spawn(async move {
        let _ = executor.on_signals(&devices, signals::TERMINATED).await;
        native_runtime::task::exit_process(power_off(&devices, "devmgr"));
    });
    executor.spawn(async move {
        let _ = executor.on_signals(&filesystems, signals::TERMINATED).await;
        native_runtime::task::exit_process(power_off(&filesystems, "ext4d"));
    });
    executor.spawn(async move {
        let _ = executor.on_signals(&network, signals::TERMINATED).await;
        native_runtime::task::exit_process(power_off(&network, "netd"));
    });
    executor.block_on(async move {
        let _ = executor.on_signals(&linux, signals::TERMINATED).await;
        power_off(&linux, "linuxd")
    })
}

/// 系统服务终止后无法继续：记录退出状态并关机；关机请求被拒绝时返回根服务的退出码。
fn power_off(process: &Process, name: &str) -> i64 {
    let status = process.basic().map_or(-1, |basic| basic.return_code);
    error!("{name} terminated with status {status}; powering off");
    let resource = ROOT_RESOURCE.get().expect("root resource installed");
    let _ = native_runtime::system::power(resource, native_abi::system::POWER_OFF);
    -1
}

// OWNER: 根 resource；关机路径从任一服务的终止观察者使用它。
static ROOT_RESOURCE: native_runtime::sync::Once<Resource> = native_runtime::sync::Once::new();

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
    directory: Channel,
    [ext4, network]: [Channel; 2],
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
            directory.into_handle(),
            ext4.into_handle(),
            network.into_handle(),
        ],
        stack_size: LINUXD_STACK_SIZE,
    })
    .expect("linuxd launch");
    info!("linuxd started");
    process
}

/// 启动 devmgr：普通进程，持有根 resource、DTB 与设备目录写端。
fn start_devmgr(
    root: &Job,
    resource: &Resource,
    bootfs: &native_runtime::bootfs::Bootfs,
    device_tree: Handle,
    directory: Channel,
) -> Process {
    let image = bootfs
        .find(devmgr::IMAGE)
        .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(devmgr::IMAGE).unwrap_or("")))
        .data;
    let job = Job::create(root).expect("devmgr job");
    let (process, vmar) = Process::create(&job, b"devmgr", 0).expect("devmgr process");
    let runtime_vmar: Vmar = vmar.duplicate(rights::SAME).expect("devmgr vmar duplicate");
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &vmar,
        runtime_vmar,
        name: b"devmgr",
        image,
        arguments: &[],
        handles: vec![
            resource.duplicate(rights::SAME).expect("resource duplicate").into_handle(),
            device_tree,
            directory.into_handle(),
        ],
        stack_size: DEVMGR_STACK_SIZE,
    })
    .expect("devmgr launch");
    info!("devmgr started");
    process
}

/// 启动只持有一个控制 channel 服务端的系统服务（ext4d、netd）：普通进程，其余资源随使用者的请求交来。
fn start_service(
    root: &Job,
    bootfs: &native_runtime::bootfs::Bootfs,
    image: &[u8],
    name: &[u8],
    control: Channel,
    stack_size: usize,
) -> Process {
    let label = core::str::from_utf8(name).unwrap_or("");
    let image = bootfs
        .find(image)
        .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(image).unwrap_or("")))
        .data;
    let job = Job::create(root).unwrap_or_else(|status| panic!("{label} job: {status}"));
    let (process, vmar) = Process::create(&job, name, 0).unwrap_or_else(|status| panic!("{label} process: {status}"));
    let runtime_vmar: Vmar = vmar
        .duplicate(rights::SAME)
        .unwrap_or_else(|status| panic!("{label} vmar duplicate: {status}"));
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &vmar,
        runtime_vmar,
        name,
        image,
        arguments: &[],
        handles: vec![control.into_handle()],
        stack_size,
    })
    .unwrap_or_else(|status| panic!("{label} launch: {status}"));
    info!("{label} started");
    process
}
