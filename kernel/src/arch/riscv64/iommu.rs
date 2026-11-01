//! RISC-V IOMMU（v1.0）first-stage 翻译后端。
//!
//! # 结构
//!
//! - 两级设备目录（`ddtp.MODE = 2LVL`）：根页 512 个非叶项，叶页按需分配，每个设备上下文（DC）
//!   为 32 字节基本格式，或在 `capabilities.MSI_FLAT` 时为 64 字节扩展格式；
//! - DC 使用 first-stage Sv39（与 CPU 页表同格式），second-stage 为 Bare，PSCID 取 domain 编号；
//! - 命令队列：`IODIR.INVAL_DDT` 使 DC 生效，`IOTINVAL.VMA` 失效翻译，`IOFENCE.C` 后轮询队头完成。
//!
//! # 限制
//!
//! 设备 ID 覆盖两级目录可寻址的范围（基本格式 16 位，扩展格式 15 位）；更大的 ID 返回
//! `ERR_OUT_OF_RANGE`。故障队列未启用：翻译故障由硬件中止事务，不向软件报告。

use native_abi::Status;
use spin::Mutex;

use super::{DmaFrame, Translation};
const PAGE_SIZE: usize = native_abi::PAGE_SIZE;
use fallible_tree::FallibleMap;
use mmio::MmioBus;

const CAPABILITIES: usize = 0x00;
const DDTP: usize = 0x10;
const CQB: usize = 0x18;
const CQH: usize = 0x20;
const CQT: usize = 0x24;
const CQCSR: usize = 0x48;

const CAPABILITY_SV39: u64 = 1 << 9;
const CAPABILITY_MSI_FLAT: u64 = 1 << 22;
const DDTP_MODE_2LVL: u64 = 3;
const DDTP_BUSY: u64 = 1 << 4;
const CQCSR_CQEN: u32 = 1 << 0;
const CQCSR_CQON: u32 = 1 << 16;
const FSC_MODE_SV39: u64 = 8 << 60;

const COMMAND_LOG2: u32 = 8;
const POLL_LIMIT: usize = 10_000_000;
const RANGE_INVALIDATE_LIMIT: usize = 32;

const OPCODE_IOTINVAL: u64 = 1;
const OPCODE_IOFENCE: u64 = 2;
const OPCODE_IODIR: u64 = 3;

fn ppn_field(page: usize) -> u64 {
    (page as u64) << 10
}

fn write_words(page: usize, offset: usize, words: &[u64]) {
    let base = crate::arch::mmu::physical_to_virtual(page * PAGE_SIZE) as *mut u8;
    for (index, word) in words.iter().enumerate() {
        // SAFETY: 页属于本驱动独占的 DMA 表，偏移在页内且 8 字节对齐。
        unsafe { core::ptr::write_volatile(base.add(offset + index * 8).cast::<u64>(), *word) };
    }
}

struct CommandQueue<F> {
    frames: F,
    tail: u32,
}

pub(crate) struct Backend<F> {
    registers: MmioBus,
    extended: bool,
    root: F,
    // OWNER: 按需分配的目录叶页（根索引 → 页）；绑定后由硬件读取，生命周期等同 IOMMU。
    leaves: Mutex<FallibleMap<usize, F>>,
    // OWNER: 命令队列的唯一写者；队尾索引与队列内容同锁发布。
    queue: Mutex<CommandQueue<F>>,
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

impl<F: DmaFrame> Backend<F> {
    pub(crate) fn create(kind: u32, virtual_base: usize, size: usize) -> Result<Self, Status> {
        if kind != native_abi::iommu::RISCV { return Err(native_abi::ERR_NOT_SUPPORTED); }
        let registers =
            MmioBus::new(virtual_base, size).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let capabilities = registers
            .read_u64(CAPABILITIES)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        if capabilities & CAPABILITY_SV39 == 0 {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let root = F::allocate_zeroed(1)
            .ok_or(native_abi::ERR_NO_MEMORY)?;
        let queue_pages = ((1usize << COMMAND_LOG2) * 16).div_ceil(PAGE_SIZE);
        let queue = F::allocate_zeroed(queue_pages)
            .ok_or(native_abi::ERR_NO_MEMORY)?;
        let error = |_| native_abi::ERR_INVALID_ARGS;
        registers
            .write_u64(CQB, ppn_field(queue.physical_page()) | u64::from(COMMAND_LOG2 - 1))
            .map_err(error)?;
        registers.write_u32(CQT, 0).map_err(error)?;
        registers.write_u32(CQCSR, CQCSR_CQEN).map_err(error)?;
        poll(|| {
            registers
                .read_u32(CQCSR)
                .is_ok_and(|csr| csr & CQCSR_CQON != 0)
        })?;
        mmio::before_write();
        registers
            .write_u64(DDTP, ppn_field(root.physical_page()) | DDTP_MODE_2LVL)
            .map_err(error)?;
        poll(|| {
            registers
                .read_u64(DDTP)
                .is_ok_and(|ddtp| ddtp & DDTP_BUSY == 0)
        })?;
        Ok(Self {
            registers,
            extended: capabilities & CAPABILITY_MSI_FLAT != 0,
            root,
            leaves: Mutex::new(FallibleMap::new()),
            queue: Mutex::new(CommandQueue {
                frames: queue,
                tail: 0,
            }),
        })
    }

    /// 写入命令并以 `IOFENCE.C` 等待全部完成。
    fn submit(&self, commands: &[[u64; 2]]) -> Result<(), Status> {
        let mut queue = self.queue.lock();
        let mask = (1u32 << COMMAND_LOG2) - 1;
        for command in commands
            .iter()
            .chain(core::iter::once(&[OPCODE_IOFENCE, 0]))
        {
            poll(|| {
                self.registers
                    .read_u32(CQH)
                    .is_ok_and(|head| (queue.tail + 1) & mask != head & mask)
            })?;
            write_words(queue.frames.physical_page(), (queue.tail & mask) as usize * 16, command);
            queue.tail = (queue.tail + 1) & mask;
        }
        mmio::before_write();
        self.registers
            .write_u32(CQT, queue.tail)
            .map_err(|_| native_abi::ERR_INTERNAL)?;
        let tail = queue.tail;
        poll(|| {
            self.registers
                .read_u32(CQH)
                .is_ok_and(|head| head & mask == tail)
        })
    }

    /// 返回设备上下文所在叶页与页内偏移；叶页按需分配并挂入根目录。
    fn context_slot(&self, device: u32) -> Result<(usize, usize), Status> {
        let (leaf_bits, context_bytes) = if self.extended { (6, 64) } else { (7, 32) };
        let root_index = (device >> leaf_bits) as usize;
        if root_index >= 512 {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let offset = (device as usize & ((1 << leaf_bits) - 1)) * context_bytes;
        let mut leaves = self.leaves.lock();
        if let Some(leaf) = leaves.get(&root_index) {
            return Ok((leaf.physical_page(), offset));
        }
        let leaf = F::allocate_zeroed(1)
            .ok_or(native_abi::ERR_NO_MEMORY)?;
        let page = leaf.physical_page();
        leaves
            .try_insert(root_index, leaf)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        mmio::before_write();
        write_words(self.root.physical_page(), root_index * 8, &[ppn_field(page) | 1]);
        Ok((page, offset))
    }
}

impl<F: DmaFrame> Translation for Backend<F> {
    fn attach(&self, stream: u32, domain: u16, root: usize) -> Result<(), Status> {
        let (leaf, offset) = self.context_slot(stream)?;
        let translation = u64::from(domain) << 12; // ta.PSCID[31:12]
        let first_stage = FSC_MODE_SV39 | (root as u64);
        // 先写 iohgatp（Bare）、ta 与 fsc，最后置 tc.V。
        write_words(leaf, offset + 8, &[0, translation, first_stage]);
        mmio::before_write();
        write_words(leaf, offset, &[1]);
        let invalidate_directory = OPCODE_IODIR | (1 << 33) | (u64::from(stream) << 40);
        self.submit(&[[invalidate_directory, 0]])
    }

    fn invalidate(&self, domain: u16, first_page: usize, pages: usize) {
        let base = OPCODE_IOTINVAL | (u64::from(domain) << 12) | (1 << 32);
        let result = if pages > RANGE_INVALIDATE_LIMIT {
            self.submit(&[[base, 0]])
        } else {
            let mut commands = [[0u64; 2]; RANGE_INVALIDATE_LIMIT];
            for (index, command) in commands.iter_mut().take(pages).enumerate() {
                *command = [base | (1 << 10), ((first_page + index) as u64) << 10];
            }
            self.submit(&commands[..pages])
        };
        // 失效未完成就释放页会让设备访问已复用的内存：只能停机。
        result.unwrap_or_else(|status| panic!("RISC-V IOMMU invalidation failed: {status}"));
    }
}
