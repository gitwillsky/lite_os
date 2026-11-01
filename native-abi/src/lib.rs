#![no_std]

//! LiteOS 内核原生对象接口（native ABI）。
//!
//! 这是内核唯一的系统调用接口：进程、线程、内存对象、IPC 与等待都经它访问。
//!
//! # 调用约定
//!
//! 1. 编号与最多 7 个参数使用各架构的 syscall 寄存器（AArch64 `svc #0` 的 `x8`/`x0..x6`，
//!    RISC-V `ecall` 的 `a7`/`a0..a6`）。
//! 2. 返回寄存器是 [`Status`]：0 成功，负值错误；输出参数一律写入调用者提供的用户指针。
//! 3. 句柄是进程内不透明的 32 位值，携带 [`rights`]；不能伪造，只能经 channel 传递、
//!    `handle_duplicate` 降权或 `handle_close` 关闭。
//! 4. 时间是单调时钟纳秒，`deadline` 为绝对时刻；[`DEADLINE_INFINITE`] 表示不超时。

pub mod call;
pub mod register;

/// 调用结果；数值与 Zircon `zx_status_t` 一致，便于对照成熟实现的错误语义。
pub type Status = i32;

/// 成功。
pub const OK: Status = 0;
/// 内核内部不变量失败；不应由合法输入触发。
pub const ERR_INTERNAL: Status = -1;
/// 操作未实现或对象不支持。
pub const ERR_NOT_SUPPORTED: Status = -2;
/// 句柄表或对象数量等非内存资源耗尽。
pub const ERR_NO_RESOURCES: Status = -3;
/// 内存分配失败。
pub const ERR_NO_MEMORY: Status = -4;
/// 参数非法（标志位、对齐、长度）。
pub const ERR_INVALID_ARGS: Status = -10;
/// 句柄值不在调用者句柄表中。
pub const ERR_BAD_HANDLE: Status = -11;
/// 句柄指向的对象类型不支持该操作。
pub const ERR_WRONG_TYPE: Status = -12;
/// 编号不在 native 调用表中。
pub const ERR_BAD_SYSCALL: Status = -13;
/// 偏移或长度越过对象边界。
pub const ERR_OUT_OF_RANGE: Status = -14;
/// 输出缓冲区不足；需要的大小已写回，消息仍留在队列中。
pub const ERR_BUFFER_TOO_SMALL: Status = -15;
/// 对象当前状态不允许该操作。
pub const ERR_BAD_STATE: Status = -20;
/// 截止时间已到。
pub const ERR_TIMED_OUT: Status = -21;
/// 非阻塞操作当前无法完成。
pub const ERR_SHOULD_WAIT: Status = -22;
/// 等待因句柄关闭而取消。
pub const ERR_CANCELED: Status = -23;
/// channel 对端已关闭。
pub const ERR_PEER_CLOSED: Status = -24;
/// 目标不存在（地址未映射、名称不存在）。
pub const ERR_NOT_FOUND: Status = -25;
/// 句柄缺少操作所需 rights。
pub const ERR_ACCESS_DENIED: Status = -30;
/// 用户指针不可访问。
pub const ERR_INVALID_POINTER: Status = -31;
/// 对象已绑定，不能重复绑定。
pub const ERR_ALREADY_BOUND: Status = -33;
/// I/O 失败（pager 无法提供页内容）。
pub const ERR_IO: Status = -40;

/// 句柄值；0 永远不是有效句柄。
pub type Handle = u32;

/// 无效句柄。
pub const HANDLE_INVALID: Handle = 0;

/// 不超时的截止时间。
pub const DEADLINE_INFINITE: i64 = i64::MAX;

/// 句柄 rights 位。
///
/// rights 只能在 `handle_duplicate` 与 channel 传递时缩小，永远不能扩大。
pub mod rights {
    /// 可复制句柄。
    pub const DUPLICATE: u32 = 1 << 0;
    /// 可经 channel 转移给其他进程。
    pub const TRANSFER: u32 = 1 << 1;
    /// 可读取（channel 读消息、VMO 读）。
    pub const READ: u32 = 1 << 2;
    /// 可写入（channel 写消息、VMO 写）。
    pub const WRITE: u32 = 1 << 3;
    /// 可映射为可执行。
    pub const EXECUTE: u32 = 1 << 4;
    /// 可映射到地址空间。
    pub const MAP: u32 = 1 << 5;
    /// 可修改用户 signal。
    pub const SIGNAL: u32 = 1 << 6;
    /// 可等待 signal。
    pub const WAIT: u32 = 1 << 7;
    /// 可管理任务（创建线程、启动、终止、读取状态）。
    pub const MANAGE: u32 = 1 << 8;
    /// 可读取对象信息。
    pub const INSPECT: u32 = 1 << 9;
    /// 显式请求“与原句柄相同”的 rights。
    pub const SAME: u32 = 1 << 31;
}

/// 对象 signal 位；等待与 port packet 都只观察这些位。
pub mod signals {
    /// channel 有可读消息。
    pub const READABLE: u32 = 1 << 0;
    /// channel 可写（对端仍存在）。
    pub const WRITABLE: u32 = 1 << 1;
    /// channel 对端端点已关闭。
    pub const PEER_CLOSED: u32 = 1 << 2;
    /// event 被置位。
    pub const SIGNALED: u32 = 1 << 3;
    /// 进程、线程或 Job 已终止。
    pub const TERMINATED: u32 = 1 << 4;
    /// 中断对象被销毁。
    pub const CANCELED: u32 = 1 << 5;
    /// 用户可置位的 8 个 signal（`object_signal`）。
    pub const USER_ALL: u32 = 0xff << 24;
    /// 第一个用户 signal。
    pub const USER_0: u32 = 1 << 24;
}

/// 句柄对应的对象类型。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Process = 1,
    Thread = 2,
    Vmo = 3,
    Channel = 4,
    Event = 5,
    Port = 6,
    Vmar = 7,
    Job = 8,
    Resource = 9,
    Interrupt = 10,
    Iommu = 11,
    Bti = 12,
    Pmt = 13,
    Pager = 14,
    Exception = 15,
}

/// 单条 channel 消息的最大字节数。
pub const CHANNEL_MAX_BYTES: usize = 65536;
/// 单条 channel 消息的最大句柄数。
pub const CHANNEL_MAX_HANDLES: usize = 64;

/// `channel_call` 消息首 4 字节保存的事务号；内核分配并据此匹配回复。
pub const CHANNEL_TXID_BYTES: usize = 4;

/// `channel_read` 实际读出的大小。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelActual {
    /// 消息字节数。
    pub bytes: u32,
    /// 消息句柄数。
    pub handles: u32,
}

/// `channel_call` 的写入与读回缓冲区描述。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelCallArgs {
    /// 请求消息字节；首 [`CHANNEL_TXID_BYTES`] 字节由内核覆盖为事务号。
    pub write_bytes: u64,
    /// 请求消息句柄数组。
    pub write_handles: u64,
    /// 回复字节缓冲区。
    pub read_bytes: u64,
    /// 回复句柄缓冲区。
    pub read_handles: u64,
    /// 请求字节数。
    pub write_num_bytes: u32,
    /// 请求句柄数。
    pub write_num_handles: u32,
    /// 回复字节缓冲区容量。
    pub read_num_bytes: u32,
    /// 回复句柄缓冲区容量。
    pub read_num_handles: u32,
}

/// port packet 的类型。
pub mod packet {
    /// `port_queue` 投递的用户 packet。
    pub const USER: u32 = 0;
    /// `object_wait_async` 注册的 signal 已满足。
    pub const SIGNAL_ONE: u32 = 1;
    /// pager VMO 需要补页：`payload` 为字节偏移，`observed` 为页数。
    pub const PAGE_REQUEST: u32 = 2;
    /// 中断触发：`payload` 为单调时钟时间戳。
    pub const INTERRUPT: u32 = 3;
}

/// port 中的一个事件。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortPacket {
    /// 注册或投递时给出的 key。
    pub key: u64,
    /// [`packet`] 类型。
    pub kind: u32,
    /// 事件状态（`SIGNAL_ONE` 为 [`OK`]）。
    pub status: Status,
    /// `SIGNAL_ONE`：触发等待的 signal 掩码；`USER`：未使用。
    pub trigger: u32,
    /// `SIGNAL_ONE`：投递时对象的全部 signal；`USER`：未使用。
    pub observed: u32,
    /// `USER`：投递者给出的载荷。
    pub payload: u64,
}

/// `vmar_map`/`vmar_allocate`/`vmar_protect` 的选项。
pub mod vm {
    /// 可读。
    pub const READ: u32 = 1 << 0;
    /// 可写。
    pub const WRITE: u32 = 1 << 1;
    /// 可执行。
    pub const EXECUTE: u32 = 1 << 2;
    /// 放在 `vmar_offset` 处，区间必须空闲。
    pub const SPECIFIC: u32 = 1 << 3;
    /// 放在 `vmar_offset` 处，先解除区间内已有映射。
    pub const SPECIFIC_OVERWRITE: u32 = 1 << 4;
    /// 允许映射超出 VMO 当前大小的区间；未获后备的页在访问时产生可交付的缺页。
    /// 未设置时 map 拒绝越界区间，服务可依赖已验证的 backing 窗口。
    pub const ALLOW_FAULTS: u32 = 1 << 5;
}

/// VMO 选项与操作。
pub mod vmo {
    /// `vmo_create`：允许 `vmo_set_size`。
    pub const RESIZABLE: u32 = 1 << 0;
    /// `vmo_create_child`：写时复制快照，父子都看不到对方之后的写入。
    pub const CHILD_SNAPSHOT: u32 = 1 << 0;
    /// `vmo_create_child`：未写入页读到父对象当前内容，写入时复制。
    pub const CHILD_SNAPSHOT_AT_LEAST_ON_WRITE: u32 = 1 << 1;
    /// `vmo_create_child`：父对象区间的别名。
    pub const CHILD_SLICE: u32 = 1 << 2;
    /// `vmo_op_range`：提交页。
    pub const OP_COMMIT: u32 = 1;
    /// `vmo_op_range`：释放页（之后读为零或重新向 pager 请求）。
    pub const OP_DECOMMIT: u32 = 2;
    /// `vmo_op_range`：把字节区间清零（不要求页对齐）。
    pub const OP_ZERO: u32 = 3;
}

/// pager 操作。
pub mod pager {
    /// 区间无法提供：撤销区间内已发出的请求，当前等待者以 `ERR_IO` 失败；之后的访问重新请求。
    pub const OP_FAIL: u32 = 1;
    /// 回写开始：清除脏标记并写保护映射。
    pub const OP_WRITEBACK_BEGIN: u32 = 2;
    /// 查询区间内第一段脏页。
    pub const OP_QUERY_DIRTY: u32 = 3;
}

/// 进程创建选项。
pub mod process {
    /// 创建只管理共享区的共享进程，供 `process_create_shared` 引用。
    pub const SHARED: u32 = 1 << 0;
}

/// futex 选项。
pub mod futex {
    /// 以地址解析出的 (VMO, 偏移) 为 key，跨地址空间共享同一内存时互相可见。
    pub const SHARED: u32 = 1 << 0;
}

/// restricted mode 退出原因（`vector` 的第二个参数）。
pub mod restricted {
    /// restricted 代码执行了 syscall 指令；状态中的 PC 指向该指令之后。
    pub const REASON_SYSCALL: u64 = 0;
    /// 发生地址空间无法解决的异常；状态中的 `exception` 描述它。
    pub const REASON_EXCEPTION: u64 = 1;
    /// 线程被 kick。
    pub const REASON_KICK: u64 = 2;
}

/// resource 类型。
pub mod resource {
    /// 全部权限（只交给根服务）。
    pub const ROOT: u32 = 0;
    /// 一段 MMIO 物理区间。
    pub const MMIO: u32 = 1;
    /// 一段中断向量。
    pub const IRQ: u32 = 2;
}

/// 中断选项。
pub mod interrupt {
    /// 电平触发：触发后保持屏蔽直到 `interrupt_ack`。
    pub const LEVEL: u32 = 1 << 0;
}

/// IOMMU 类型。
pub mod iommu {
    /// 无转换：设备地址等于物理地址（平台没有 IOMMU 时）。
    pub const IDENTITY: u32 = 0;
    /// Arm SMMUv3；描述符为 [`crate::SmmuDescriptor`]。
    pub const ARM_SMMU_V3: u32 = 1;
    /// RISC-V IOMMU；描述符为 [`crate::RiscvIommuDescriptor`]。
    pub const RISCV: u32 = 2;
}

/// `bti_pin` 选项。
pub mod bti {
    /// 设备可读。
    pub const READ: u32 = 1 << 0;
    /// 设备可写。
    pub const WRITE: u32 = 1 << 1;
}

/// `object_get_info` 主题。
pub mod info {
    /// [`crate::HandleBasic`]。
    pub const HANDLE_BASIC: u32 = 1;
    /// [`crate::TaskRuntime`]（线程或进程）。
    pub const TASK_RUNTIME: u32 = 2;
    /// [`crate::VmarRegion`]。
    pub const VMAR: u32 = 3;
    /// [`crate::ProcessBasic`]。
    pub const PROCESS: u32 = 4;
    /// 进程或线程名称（UTF-8 字节，≤ 32）。
    pub const NAME: u32 = 5;
    /// [`crate::VmoInfo`]。
    pub const VMO: u32 = 6;
}

/// `system_get_info` 主题与电源操作。
pub mod system {
    /// [`crate::SystemBasic`]。
    pub const BASIC: u32 = 1;
    /// 每个 CPU 的累计忙碌纳秒，`u64` 数组。
    pub const CPU_BUSY: u32 = 2;
    /// 上一次启动的内核崩溃记录文本；上次正常结束时为空。
    pub const CRASH_LOG: u32 = 3;
    /// 物理页分配器每个 order 的空闲块数，`u64` 数组（order N 的块为 2^N 页）。
    pub const FREE_BLOCKS: u32 = 4;
    /// 关机。
    pub const POWER_OFF: u32 = 1;
    /// 重启。
    pub const POWER_REBOOT: u32 = 2;
}

/// 进程启动消息。
///
/// 每个经启动协议创建的进程，其首个线程的第一个参数是 startup channel 句柄；channel 中恰有一条消息：
/// 字节为 [`StartupHeader`] 后接参数字节，句柄前 [`startup::ROLE_HANDLES`] 个为通用句柄，其后由
/// 启动者与被启动者的角色约定。内核启动根服务时同样遵循该协议，参数字节是内核启动参数文本。
pub mod startup {
    /// 消息头魔数（`"LTSTART1"`）。
    pub const MAGIC: u64 = 0x3154_5241_5453_544c;
    /// 进程自身。
    pub const SELF_PROCESS: usize = 0;
    /// 运行时分配（堆、线程栈）所用的 VMAR。
    pub const SELF_VMAR: usize = 1;
    /// 首个线程自身。
    pub const SELF_THREAD: usize = 2;
    /// 角色句柄的起始下标。
    pub const ROLE_HANDLES: usize = 3;

    /// 根服务的角色句柄。
    pub mod root {
        /// 根 Job。
        pub const JOB: usize = super::ROLE_HANDLES;
        /// 根 resource（全部硬件访问权）。
        pub const RESOURCE: usize = super::ROLE_HANDLES + 1;
        /// bootfs（cpio `newc`）VMO。
        pub const BOOTFS: usize = super::ROLE_HANDLES + 2;
        /// 平台 DTB 的只读副本 VMO。
        pub const DEVICE_TREE: usize = super::ROLE_HANDLES + 3;
        /// 句柄总数。
        pub const HANDLE_COUNT: usize = super::ROLE_HANDLES + 4;
    }
}

/// 启动消息头。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StartupHeader {
    /// [`startup::MAGIC`]。
    pub magic: u64,
    /// 紧随头部的参数字节数。
    pub arguments_length: u64,
}

/// 句柄的对象类型、rights 与对象全局编号。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HandleBasic {
    pub koid: u64,
    pub object_type: u32,
    pub rights: u32,
}

/// 任务已消耗的 CPU 时间。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskRuntime {
    /// 全部 CPU 时间。
    pub cpu_time_ns: u64,
    /// 其中在 restricted mode 中消耗的部分。
    pub restricted_time_ns: u64,
    /// 因阻塞离开 CPU 的次数。
    pub voluntary_switches: u64,
    /// 仍可运行时被抢占或让出 CPU 的次数。
    pub involuntary_switches: u64,
    /// restricted mode 中成功解决的缺页次数。
    pub restricted_faults: u64,
    /// 其中本次解析等待了 pager 响应的次数。
    pub restricted_pager_waits: u64,
}

/// VMO 的大小与本对象自己持有的已提交字节（不含从父对象共享的页）。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmoInfo {
    pub size: u64,
    pub committed_bytes: u64,
}

/// VMAR 覆盖的地址区间。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmarRegion {
    pub base: u64,
    pub size: u64,
}

/// 进程状态。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessBasic {
    /// 进程已终止时为 1。
    pub exited: u32,
    /// `process_exit` 给出的退出码；被终止时为 -1。
    pub return_code: i32,
}

/// 系统概况。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBasic {
    pub cpu_count: u32,
    pub page_size: u32,
    pub total_pages: u64,
    pub free_pages: u64,
    pub boot_utc_ns: i64,
    /// 单调时钟的最小可观察粒度（纳秒，向上取整）。
    pub clock_resolution_ns: u64,
    /// 调度器基础时间片（纳秒）。
    pub scheduler_quantum_ns: u64,
}

/// 一条内核日志记录。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DebuglogRecord {
    pub sequence: u64,
    pub timestamp_ns: u64,
    /// 0 debug..3 error。
    pub severity: u32,
    pub length: u32,
    pub message: [u8; DEBUGLOG_MESSAGE_CAPACITY],
}

impl Default for DebuglogRecord {
    fn default() -> Self {
        Self {
            sequence: 0,
            timestamp_ns: 0,
            severity: 0,
            length: 0,
            message: [0; DEBUGLOG_MESSAGE_CAPACITY],
        }
    }
}

/// 单条日志记录的消息上限。
pub const DEBUGLOG_MESSAGE_CAPACITY: usize = 1024;

/// 异常类型。
pub mod exception {
    /// 缺页未能解决。
    pub const PAGE_FAULT: u32 = 1;
    /// 权限或总线访问错误。
    pub const ACCESS_FAULT: u32 = 2;
    /// 未对齐访问。
    pub const ALIGNMENT: u32 = 3;
    /// 非法指令。
    pub const ILLEGAL_INSTRUCTION: u32 = 4;
    /// 断点。
    pub const BREAKPOINT: u32 = 5;
    /// 其他未解码的异常。
    pub const UNSUPPORTED: u32 = 6;
}

/// 异常报告：异常 channel 消息与 restricted 状态共用。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ExceptionReport {
    /// [`exception`] 类型。
    pub kind: u32,
    /// 缺页访问类型：0 读、1 写、2 执行。
    pub access: u32,
    /// 异常地址（数据异常）或 PC（指令异常）。
    pub address: u64,
    /// 架构原始综合码（AArch64 `ESR_EL1`，RISC-V `scause`）。
    pub syndrome: u64,
    /// 线程全局编号。
    pub thread_koid: u64,
    /// 异常时的寄存器。
    pub state: register::RegisterState,
}

/// restricted 状态 VMO 的布局。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RestrictedState {
    /// 进入时加载、退出时保存的 restricted 寄存器。
    pub registers: register::RegisterState,
    /// 退出原因为 `REASON_EXCEPTION` 时的异常描述。
    pub exception: ExceptionReport,
}

/// SMMUv3 描述符：寄存器窗口的物理地址与大小。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SmmuDescriptor {
    pub base: u64,
    pub size: u64,
}

/// RISC-V IOMMU 描述符：寄存器窗口的物理地址与大小。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RiscvIommuDescriptor {
    pub base: u64,
    pub size: u64,
}

/// 原生进程用户地址空间的页大小。
pub const PAGE_SIZE: usize = 4096;
