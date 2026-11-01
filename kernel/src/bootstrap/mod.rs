//! 启动根服务：内核唯一直接创建的用户进程。
//!
//! 1. 把 initrd（cpio `newc` bootfs）复制进 VMO，随后把 initrd 占用的物理页归还 frame allocator；
//! 2. 从 bootfs 取出 `bin/root-server`（静态 ELF）装进新进程，映射栈；
//! 3. 创建根 Job、根 resource 与 DTB 副本，按启动协议（`native_abi::startup`）经 startup channel
//!    交给根服务，参数字节为内核启动参数文本；
//! 4. 以 startup channel 句柄为第一个参数启动首个线程。
//!
//! 其余系统服务都由根服务从 bootfs 启动；内核不认识它们。

mod cpio;
mod elf;

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_abi::{StartupHeader, rights, startup};

use crate::object::{
    Channel, HandleEntry, Job, Message, Object, Process, ProcessKind, Resource, Thread,
};
use crate::thread::UserStart;
use crate::vm::page_table::PagePermissions;
use crate::vm::{MapOptions, Placement, Vmo, VmoOptions};

const ROOT_SERVER: &[u8] = b"bin/root-server";
const STACK_SIZE: usize = 256 * 1024;

/// 把一段物理内存复制进新 VMO。
fn copy_physical(range: core::ops::Range<usize>) -> Arc<Vmo> {
    let virtual_start = crate::arch::mmu::physical_to_virtual(range.start);
    // SAFETY: 区间是固件交付、启动期保留且在 direct map 内的 RAM；复制完成前没有其他所有者。
    let bytes = unsafe { core::slice::from_raw_parts(virtual_start as *const u8, range.len()) };
    let vmo =
        Vmo::create(range.len() as u64, VmoOptions::default()).expect("boot VMO allocation failed");
    vmo.write(0, bytes).expect("boot VMO copy failed");
    vmo
}

fn entry(object: Object, rights: u32) -> HandleEntry {
    HandleEntry { object, rights }
}

/// 启动根服务。
///
/// # Panics
///
/// 没有 initrd、bootfs 中缺少根服务或装载失败时 fail-stop：没有根服务系统无法继续。
pub(crate) fn start_root_server(command_line: &[u8]) {
    let initrd =
        crate::platform::initrd_range().expect("boot requires an initrd carrying the bootfs");
    let bootfs = copy_physical(initrd.clone());
    let device_tree = copy_physical(crate::platform::dtb_range());
    crate::memory::frame_allocator::release_boot_reserved();

    let mut image = Vec::new();
    image
        .try_reserve_exact(initrd.len())
        .expect("bootfs staging allocation failed");
    image.resize(initrd.len(), 0);
    bootfs.read(0, &mut image).expect("bootfs read failed");
    let server = cpio::find(&image, ROOT_SERVER)
        .unwrap_or_else(|error| panic!("bootfs is malformed: {error:?}"))
        .unwrap_or_else(|| {
            panic!(
                "bootfs lacks {}",
                core::str::from_utf8(ROOT_SERVER).unwrap_or("")
            )
        });

    let job = Job::create_root().expect("root job allocation failed");
    let process = Process::create(&job, b"root-server", ProcessKind::Plain)
        .expect("root process allocation failed");
    let vmar = process.root_vmar().clone();
    let entry_point = elf::load(server.data, &vmar).unwrap_or_else(|error| {
        panic!(
            "{} cannot be loaded: {error:?}",
            core::str::from_utf8(server.name).unwrap_or("")
        )
    });
    drop(image);

    let stack = Vmo::create(STACK_SIZE as u64, VmoOptions::default())
        .expect("root stack allocation failed");
    let stack_base = vmar
        .map(
            &stack,
            0,
            STACK_SIZE,
            MapOptions {
                placement: Placement::Any,
                permissions: PagePermissions::READ | PagePermissions::WRITE,
                ceiling: PagePermissions::READ | PagePermissions::WRITE,
                allow_faults: false,
            },
        )
        .expect("root stack mapping failed");

    let thread = Thread::create(&process, b"root-server").expect("root thread allocation failed");
    let (kernel_end, user_end) =
        Channel::create_pair().expect("bootstrap channel allocation failed");
    let kernel_end = Arc::try_new(kernel_end).expect("bootstrap channel allocation failed");
    let user_end = Arc::try_new(user_end).expect("bootstrap channel allocation failed");

    let header = StartupHeader {
        magic: startup::MAGIC,
        arguments_length: command_line.len() as u64,
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(core::mem::size_of::<StartupHeader>() + command_line.len())
        .expect("bootstrap message allocation failed");
    // SAFETY: StartupHeader 是 repr(C) 整数结构，按字节复制不违反任何不变量。
    bytes.extend_from_slice(unsafe {
        core::slice::from_raw_parts(
            (&header as *const StartupHeader).cast::<u8>(),
            core::mem::size_of::<StartupHeader>(),
        )
    });
    bytes.extend_from_slice(command_line);

    let mut handles = Vec::new();
    handles
        .try_reserve_exact(startup::root::HANDLE_COUNT)
        .expect("bootstrap message allocation failed");
    let full = |object: Object| {
        let rights = object.kernel_object().default_rights();
        entry(object, rights)
    };
    handles.push(full(Object::Process(process.clone())));
    handles.push(full(Object::Vmar(vmar)));
    handles.push(full(Object::Thread(thread.clone())));
    handles.push(full(Object::Job(job)));
    handles.push(full(Object::Resource(
        Arc::try_new(Resource::root()).expect("root resource allocation failed"),
    )));
    handles.push(entry(
        Object::Vmo(bootfs),
        rights::READ | rights::MAP | rights::DUPLICATE | rights::TRANSFER | rights::INSPECT,
    ));
    handles.push(entry(
        Object::Vmo(device_tree),
        rights::READ | rights::MAP | rights::DUPLICATE | rights::TRANSFER | rights::INSPECT,
    ));
    kernel_end
        .write(Message { bytes, handles })
        .unwrap_or_else(|_| panic!("bootstrap message delivery failed"));
    drop(kernel_end);

    let channel = process
        .handles()
        .insert(full(Object::Channel(user_end)))
        .expect("bootstrap handle allocation failed");
    process.mark_started().expect("root process started twice");
    thread
        .start(
            thread.clone(),
            UserStart {
                entry: entry_point,
                stack: stack_base + STACK_SIZE,
                arguments: [channel as usize, 0],
            },
        )
        .expect("root thread start failed");
    info!(
        "root server started: entry {:#x}, bootfs {} bytes",
        entry_point,
        initrd.len()
    );
}
