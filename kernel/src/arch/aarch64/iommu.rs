//! Arm SMMUv3（Arm IHI 0070）stage-1 翻译后端。
//!
//! # 结构
//!
//! - 线性 stream table：每个 stream 一个 64 字节 STE，Config = stage-1 translate、stage-2 bypass；
//! - 每个 stream 一个上下文描述符（CD）：TTB0 指向该 stream 的设备页表，ASID 取 domain 编号，
//!   MAIR/T0SZ 与 CPU 一致，因此页表项格式与 CPU 完全相同；
//! - 命令队列：配置失效（CFGI_STE）、TLB 失效（TLBI_NH_ASID/VA）与 CMD_SYNC 完成等待；
//! - 事件队列：CD 的 R 位让翻译故障写入事件记录，[`Translation::drain_faults`] 由软件排空（不接中断）。
//!
//! # 限制
//!
//! stream table 采用线性格式，覆盖 `min(SIDSIZE, 12)` 位 stream ID（4096 个 stream）；
//! 更大的 stream ID 返回 `ERR_OUT_OF_RANGE`。QEMU `virt` 的 PCI RID 都位于总线 0，满足该范围。
//! 事件队列的生产者寄存器位于寄存器页 1；窗口不含页 1 时不启用事件队列，故障只由硬件中止事务。

use native_abi::Status;
use spin::Mutex;

use super::{DmaFrame, Translation};
const PAGE_SIZE: usize = native_abi::PAGE_SIZE;
use mmio::MmioBus;

const IDR0: usize = 0x00;
const IDR1: usize = 0x04;
const IDR5: usize = 0x14;
const CR0: usize = 0x20;
const GBPA: usize = 0x44;
const GBPA_UPDATE: u32 = 1 << 31;
const GBPA_ABORT: u32 = 1 << 20;
const CR0ACK: usize = 0x24;
const CR1: usize = 0x28;
const CR2: usize = 0x2c;
const STRTAB_BASE: usize = 0x80;
const STRTAB_BASE_CFG: usize = 0x88;
const CMDQ_BASE: usize = 0x90;
const CMDQ_PROD: usize = 0x98;
const CMDQ_CONS: usize = 0x9c;
const EVENTQ_BASE: usize = 0xa0;
const EVENTQ_PROD: usize = 0x1_00a8;
const EVENTQ_CONS: usize = 0x1_00ac;
/// 事件队列溢出标志（PROD）与其确认位（CONS）。
const EVENTQ_OVERFLOW: u32 = 1 << 31;

const CR0_SMMUEN: u32 = 1 << 0;
const CR0_EVTQEN: u32 = 1 << 2;
const CR0_CMDQEN: u32 = 1 << 3;
/// 表与队列访问：内部/外部 WB 可缓存、内部可共享（与 CPU 的一致性域相同）。
const CR1_ATTRIBUTES: u32 = 1 | (1 << 2) | (3 << 4) | (1 << 6) | (1 << 8) | (3 << 10);
const CR2_RECINVSID: u32 = 1 << 1;
const READ_ALLOCATE: u64 = 1 << 62;

const COMMAND_LOG2: u32 = 8;
/// 128 条 32 字节事件记录，恰好一页。
const EVENT_LOG2: u32 = 7;
const EVENT_BYTES: usize = 32;
const MAX_STREAM_BITS: u32 = 12;
const STE_BYTES: usize = 64;
const POLL_LIMIT: usize = 10_000_000;

const CMD_CFGI_STE: u64 = 0x03;
const CMD_CFGI_ALL: u64 = 0x04;
const CMD_TLBI_NH_ASID: u64 = 0x11;
const CMD_TLBI_NH_VA: u64 = 0x12;
const CMD_TLBI_NSNH_ALL: u64 = 0x30;
const CMD_SYNC: u64 = 0x46;
/// 一次失效超过该页数时改为整个 ASID 失效。
const RANGE_INVALIDATE_LIMIT: usize = 32;

struct CommandQueue<F> {
    frames: F,
    producer: u32,
}

struct EventQueue<F> {
    frames: F,
    log2: u32,
    consumer: u32,
    // 最近确认的溢出标志：硬件每次溢出翻转 PROD.OVFLG，与 CONS.OVACKFLG 不等才是新的溢出。
    acknowledged: u32,
}

pub(crate) struct Backend<F> {
    registers: MmioBus,
    stream_bits: u32,
    command_log2: u32,
    output_size: u64,
    // OWNER: stream table 与每个 stream 的 CD 页；绑定后由硬件读取，生命周期等同 IOMMU。
    stream_table: F,
    contexts: Mutex<fallible_tree::FallibleMap<u32, F>>,
    // OWNER: 命令队列的唯一写者；生产者索引与队列内容同锁发布。
    queue: Mutex<CommandQueue<F>>,
    // OWNER: 事件队列的唯一消费者；None 表示寄存器窗口不含页 1、未启用。缺失消费者时队列写满后
    // 硬件置溢出并丢弃后续故障，软件看不到任何 DMA 越界。
    events: Option<Mutex<EventQueue<F>>>,
}

/// 读取设备写入的 DMA 记录前的屏障：保证先观察到生产者寄存器，再读记录内容。
fn dma_read_barrier() {
    // SAFETY: 只排序 outer-shareable 域内的读取，不访问内存。
    unsafe { core::arch::asm!("dmb oshld", options(nostack, preserves_flags)) };
}

fn read_word(page: usize, offset: usize) -> u64 {
    let base = crate::arch::mmu::physical_to_virtual(page * PAGE_SIZE) as *const u8;
    // SAFETY: 页属于本驱动独占的事件队列，偏移在队列范围内且 8 字节对齐。
    unsafe { core::ptr::read_volatile(base.add(offset).cast::<u64>()) }
}

fn write_words(page: usize, offset: usize, words: &[u64]) {
    let base = crate::arch::mmu::physical_to_virtual(page * PAGE_SIZE) as *mut u8;
    for (index, word) in words.iter().enumerate() {
        // SAFETY: 页属于本驱动独占的 DMA 表，偏移在已分配范围内且 8 字节对齐。
        unsafe { core::ptr::write_volatile(base.add(offset + index * 8).cast::<u64>(), *word) };
    }
}

impl<F: DmaFrame> Backend<F> {
    pub(crate) fn create(kind: u32, virtual_base: usize, size: usize) -> Result<Self, Status> {
        if kind != native_abi::iommu::ARM_SMMU_V3 {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let registers =
            MmioBus::new(virtual_base, size).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let read = |offset| {
            registers
                .read_u32(offset)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)
        };
        let idr0 = read(IDR0)?;
        if idr0 & (1 << 1) == 0 {
            // 不支持 stage-1。
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let idr1 = read(IDR1)?;
        let stream_bits = (idr1 & 0x3f).min(MAX_STREAM_BITS);
        let command_log2 = ((idr1 >> 21) & 0x1f).min(COMMAND_LOG2);
        let output_size = u64::from(read(IDR5)? & 0x7);

        // 配置期间拒绝未翻译事务，关闭并确认后才发布 owned 表。
        disable(registers)?;

        let stream_pages = ((1usize << stream_bits) * STE_BYTES).div_ceil(PAGE_SIZE);
        let stream_table = F::allocate_zeroed(stream_pages).ok_or(native_abi::ERR_NO_MEMORY)?;
        let queue_pages = ((1usize << command_log2) * 16).div_ceil(PAGE_SIZE);
        let queue_frames = F::allocate_zeroed(queue_pages).ok_or(native_abi::ERR_NO_MEMORY)?;
        let stream_address = (stream_table.physical_page() * PAGE_SIZE) as u64;
        let queue_address = (queue_frames.physical_page() * PAGE_SIZE) as u64;

        let write32 = |offset, value| {
            registers
                .write_u32(offset, value)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)
        };
        let write64 = |offset, value| {
            registers
                .write_u64(offset, value)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)
        };
        write32(CR1, CR1_ATTRIBUTES)?;
        write32(CR2, CR2_RECINVSID)?;
        write64(STRTAB_BASE, READ_ALLOCATE | stream_address)?;
        write32(STRTAB_BASE_CFG, stream_bits)?;
        write64(
            CMDQ_BASE,
            READ_ALLOCATE | queue_address | u64::from(command_log2),
        )?;
        write32(CMDQ_PROD, 0)?;
        write32(CMDQ_CONS, 0)?;
        let events = if size > EVENTQ_CONS {
            let log2 = ((idr1 >> 16) & 0x1f).min(EVENT_LOG2);
            let pages = ((1usize << log2) * EVENT_BYTES).div_ceil(PAGE_SIZE);
            let frames = F::allocate_zeroed(pages).ok_or(native_abi::ERR_NO_MEMORY)?;
            let address = (frames.physical_page() * PAGE_SIZE) as u64;
            write64(EVENTQ_BASE, READ_ALLOCATE | address | u64::from(log2))?;
            write32(EVENTQ_PROD, 0)?;
            write32(EVENTQ_CONS, 0)?;
            Some(Mutex::new(EventQueue {
                frames,
                log2,
                consumer: 0,
                acknowledged: 0,
            }))
        } else {
            None
        };
        let queues = CR0_CMDQEN | if events.is_some() { CR0_EVTQEN } else { 0 };
        let smmu = Self {
            registers,
            stream_bits,
            command_log2,
            output_size,
            stream_table,
            contexts: Mutex::new(fallible_tree::FallibleMap::new()),
            queue: Mutex::new(CommandQueue {
                frames: queue_frames,
                producer: 0,
            }),
            events,
        };
        mmio::before_write();
        write32(CR0, queues)?;
        poll(|| read(CR0ACK).is_ok_and(|ack| ack == queues))?;

        smmu.submit(&[[CMD_CFGI_ALL, 31], [CMD_TLBI_NSNH_ALL, 0]])?;
        write32(CR0, queues | CR0_SMMUEN)?;
        poll(|| read(CR0ACK).is_ok_and(|ack| ack == queues | CR0_SMMUEN))?;
        Ok(smmu)
    }

    /// 写入命令并以 CMD_SYNC 等待全部完成。
    fn submit(&self, commands: &[[u64; 2]]) -> Result<(), Status> {
        let mut queue = self.queue.lock();
        let entries = 1u32 << self.command_log2;
        let mask = entries - 1;
        let wrap = entries;
        for command in commands.iter().chain(core::iter::once(&[CMD_SYNC, 0])) {
            // 等待空位：生产者与消费者索引相差一整圈时队列满。
            poll(|| {
                let consumer = self.registers.read_u32(CMDQ_CONS).unwrap_or(0) & (mask | wrap);
                (queue.producer & (mask | wrap)) != (consumer ^ wrap)
            })?;
            let index = (queue.producer & mask) as usize;
            write_words(queue.frames.physical_page(), index * 16, command);
            queue.producer = (queue.producer + 1) & (mask | wrap);
        }
        mmio::before_write();
        self.registers
            .write_u32(CMDQ_PROD, queue.producer)
            .map_err(|_| native_abi::ERR_INTERNAL)?;
        let producer = queue.producer;
        poll(|| {
            self.registers
                .read_u32(CMDQ_CONS)
                .is_ok_and(|consumer| consumer & (mask | wrap) == producer)
        })
    }
}

fn poll(mut done: impl FnMut() -> bool) -> Result<(), Status> {
    for _ in 0..POLL_LIMIT {
        if done() {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(native_abi::ERR_TIMED_OUT)
}

fn disable(registers: MmioBus) -> Result<(), Status> {
    poll(|| {
        registers
            .read_u32(GBPA)
            .is_ok_and(|value| value & GBPA_UPDATE == 0)
    })?;
    let attributes = registers
        .read_u32(GBPA)
        .map_err(|_| native_abi::ERR_INTERNAL)?;
    registers
        .write_u32(GBPA, attributes | GBPA_ABORT | GBPA_UPDATE)
        .map_err(|_| native_abi::ERR_INTERNAL)?;
    poll(|| {
        registers
            .read_u32(GBPA)
            .is_ok_and(|value| value & GBPA_UPDATE == 0 && value & GBPA_ABORT != 0)
    })?;
    registers
        .write_u32(CR0, 0)
        .map_err(|_| native_abi::ERR_INTERNAL)?;
    poll(|| registers.read_u32(CR0ACK).is_ok_and(|value| value == 0))
}

impl<F> Drop for Backend<F> {
    fn drop(&mut self) {
        // 先封闭 disabled bypass，再关闭表遍历和命令队列；确认后字段才可释放 DMA 表页。
        disable(self.registers).unwrap_or_else(|status| panic!("SMMUv3 shutdown failed: {status}"));
    }
}

impl<F: DmaFrame> Translation for Backend<F> {
    fn attach(&self, stream: u32, domain: u16, root: usize) -> Result<(), Status> {
        if stream >= 1 << self.stream_bits {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let context = F::allocate_zeroed(1).ok_or(native_abi::ERR_NO_MEMORY)?;
        let context_address = (context.physical_page() * PAGE_SIZE) as u64;
        let prepared = fallible_tree::FallibleMap::try_prepare(stream, context)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let context = prepared.value();
        let t0sz = 64 - crate::arch::mmu::TRANSLATION_BITS as u64;
        let cd0 = t0sz
            | (1 << 8) // IR0 WBWA
            | (1 << 10) // OR0 WBWA
            | (3 << 12) // SH0 inner shareable
            | (1 << 30) // EPD1：不使用 TTB1
            | (1 << 31) // V
            | (self.output_size << 32) // IPS
            | (1 << 35) // AFFD：页表项已置 AF
            | (1 << 41) // AA64
            | (1 << 45) // R：记录故障
            | (1 << 46) // A：故障中止事务
            | (1 << 47) // ASET：非共享 ASID 集
            | (u64::from(domain) << 48);
        let ttb0 = (root * PAGE_SIZE) as u64;
        write_words(
            context.physical_page(),
            0,
            &[
                cd0,
                ttb0,
                0,
                crate::arch::mmu::MEMORY_ATTRIBUTES,
                0,
                0,
                0,
                0,
            ],
        );
        let ste0 = 1 // V
            | (0b101 << 1) // Config：stage-1 translate，stage-2 bypass
            | (context_address & 0x000f_ffff_ffff_ffc0); // S1ContextPtr，S1CDMax = 0
        let ste1 = (1 << 2) // S1CIR WBRA
            | (1 << 4) // S1COR WBRA
            | (3 << 6) // S1CSH inner shareable
            | (1 << 44); // SHCFG：使用设备给出的共享属性
        let offset = stream as usize * STE_BYTES;
        // 先写其余字，最后置 V，硬件不会看到半个 STE。
        write_words(
            self.stream_table.physical_page(),
            offset + 8,
            &[ste1, 0, 0, 0, 0, 0, 0],
        );
        mmio::before_write();
        write_words(self.stream_table.physical_page(), offset, &[ste0]);
        self.contexts.lock().commit_vacant(prepared);
        // STE.V 已公布：失效失败不能返回并让 caller 释放根表。
        self.submit(&[[CMD_CFGI_STE | (u64::from(stream) << 32), 1]])
            .unwrap_or_else(|status| panic!("SMMUv3 stream publication failed: {status}"));
        Ok(())
    }

    fn drain_faults(&self, record: &mut dyn FnMut(u32, u64)) -> bool {
        let Some(events) = &self.events else {
            return false;
        };
        let mut queue = events.lock();
        let entries = 1u32 << queue.log2;
        let index_mask = (entries << 1) - 1;
        let Ok(producer) = self.registers.read_u32(EVENTQ_PROD) else {
            return false;
        };
        dma_read_barrier();
        while queue.consumer & index_mask != producer & index_mask {
            let offset = (queue.consumer & (entries - 1)) as usize * EVENT_BYTES;
            let page = queue.frames.physical_page();
            // 记录字 0 的 [63:32] 是 StreamID，字 2 是故障输入地址（翻译类事件）。
            let stream = (read_word(page, offset) >> 32) as u32;
            record(stream, read_word(page, offset + 16));
            queue.consumer = (queue.consumer + 1) & index_mask;
        }
        // 回写消费者索引，并以 OVACKFLG 跟随生产者的溢出标志确认已观察到溢出。
        let flag = producer & EVENTQ_OVERFLOW;
        let overflowed = flag != queue.acknowledged;
        queue.acknowledged = flag;
        let _ = self.registers.write_u32(EVENTQ_CONS, queue.consumer | flag);
        overflowed
    }

    fn invalidate(&self, domain: u16, first_page: usize, pages: usize) {
        let asid = u64::from(domain) << 48;
        let result = if pages > RANGE_INVALIDATE_LIMIT {
            self.submit(&[[CMD_TLBI_NH_ASID | asid, 0]])
        } else {
            let mut commands = [[0u64; 2]; RANGE_INVALIDATE_LIMIT];
            for (index, command) in commands.iter_mut().take(pages).enumerate() {
                *command = [
                    CMD_TLBI_NH_VA | asid,
                    (((first_page + index) * PAGE_SIZE) as u64) | 1,
                ];
            }
            self.submit(&commands[..pages])
        };
        // 失效未完成就释放页会让设备访问已复用的内存：只能停机。
        result.unwrap_or_else(|status| panic!("SMMUv3 invalidation failed: {status}"));
    }
}

/// 完成 coherent 设备表写入，再允许控制器消费失效命令；不能用仅排序的 DMB 替代。
pub(crate) fn complete_table_writes() {
    // SAFETY: DSB 只改变写入完成顺序，memory clobber 防止编译器跨 publication 重排。
    unsafe { core::arch::asm!("dsb oshst", options(nostack, preserves_flags)) };
}
