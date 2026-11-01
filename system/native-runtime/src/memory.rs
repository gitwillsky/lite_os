//! VMO、VMAR 与 pager。

use native_abi::{Status, VmarRegion, call};

use crate::handle::object;
use crate::syscall::{check, invoke};
use crate::{Handle, Port};

object!(
    /// 内存对象。
    Vmo
);
object!(
    /// 地址区间（虚拟内存区域）。
    Vmar
);
object!(
    /// 为 VMO 提供页内容的用户态分页者。
    Pager
);

fn new_handle(status: Status, out: native_abi::Handle) -> Result<Handle, Status> {
    check(status)?;
    // SAFETY: 调用成功时内核写入的新句柄由调用者独占。
    Ok(unsafe { Handle::from_raw(out) })
}

impl Vmo {
    /// 创建零内容 VMO。
    ///
    /// # Errors
    ///
    /// 内存不足或选项非法。
    pub fn create(size: u64, options: u32) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe { invoke(call::VMO_CREATE, &[size as usize, options as usize, (&raw mut out) as usize]) };
        new_handle(status, out).map(Self)
    }

    /// 创建物理连续、常驻的 VMO。
    ///
    /// # Errors
    ///
    /// BTI 无效或连续内存不足。
    pub fn create_contiguous(bti: &Handle, size: u64) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::VMO_CREATE_CONTIGUOUS,
                &[bti.raw() as usize, size as usize, (&raw mut out) as usize],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 创建引用 MMIO 区间的 VMO。
    ///
    /// # Errors
    ///
    /// resource 不覆盖该区间。
    pub fn create_physical(resource: &Handle, physical: u64, size: u64) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::VMO_CREATE_PHYSICAL,
                &[resource.raw() as usize, physical as usize, size as usize, (&raw mut out) as usize],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 从 `offset` 读取。
    ///
    /// # Errors
    ///
    /// `ERR_OUT_OF_RANGE`、`ERR_IO`。
    pub fn read(&self, buffer: &mut [u8], offset: u64) -> Result<(), Status> {
        // SAFETY: 缓冲区有效。
        check(unsafe {
            invoke(
                call::VMO_READ,
                &[self.raw() as usize, buffer.as_mut_ptr() as usize, offset as usize, buffer.len()],
            )
        })
    }

    /// 写入到 `offset`。
    ///
    /// # Errors
    ///
    /// `ERR_OUT_OF_RANGE`、`ERR_IO`。
    pub fn write(&self, buffer: &[u8], offset: u64) -> Result<(), Status> {
        // SAFETY: 缓冲区有效。
        check(unsafe {
            invoke(
                call::VMO_WRITE,
                &[self.raw() as usize, buffer.as_ptr() as usize, offset as usize, buffer.len()],
            )
        })
    }

    /// 字节大小。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn size(&self) -> Result<u64, Status> {
        let mut size = 0u64;
        // SAFETY: 输出位置有效。
        check(unsafe { invoke(call::VMO_GET_SIZE, &[self.raw() as usize, (&raw mut size) as usize]) })?;
        Ok(size)
    }

    /// 大小与本对象自己持有的已提交字节。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn info(&self) -> Result<native_abi::VmoInfo, Status> {
        Handle::info(self, native_abi::info::VMO)
    }

    /// 改变大小（需 `RESIZABLE`）。
    ///
    /// # Errors
    ///
    /// `ERR_NOT_SUPPORTED`、`ERR_BAD_STATE`。
    pub fn set_size(&self, size: u64) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::VMO_SET_SIZE, &[self.raw() as usize, size as usize]) })
    }

    /// 对页区间执行 `OP_*`。
    ///
    /// # Errors
    ///
    /// 区间越界或操作不支持。
    pub fn op_range(&self, op: u32, offset: u64, length: u64) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::VMO_OP_RANGE,
                &[self.raw() as usize, op as usize, offset as usize, length as usize],
            )
        })
    }

    /// 创建子 VMO（`CHILD_*`）。
    ///
    /// # Errors
    ///
    /// 对象类型不支持该子对象。
    pub fn create_child(&self, options: u32, offset: u64, size: u64) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::VMO_CREATE_CHILD,
                &[self.raw() as usize, options as usize, offset as usize, size as usize, (&raw mut out) as usize],
            )
        };
        new_handle(status, out).map(Self)
    }
}

impl Vmar {
    /// 映射 `vmo` 的 `[vmo_offset, vmo_offset + length)`，返回地址。
    ///
    /// `options` 中含 `SPECIFIC`/`SPECIFIC_OVERWRITE` 时 `vmar_offset` 是相对本 VMAR 基址的偏移。
    ///
    /// # Errors
    ///
    /// 见 `VMAR_MAP`。
    pub fn map(&self, options: u32, vmar_offset: usize, vmo: &Vmo, vmo_offset: u64, length: usize) -> Result<usize, Status> {
        let mut address = 0u64;
        // SAFETY: 输出位置有效。
        check(unsafe {
            crate::syscall::call(
                call::VMAR_MAP,
                [
                    self.raw() as usize,
                    options as usize,
                    vmar_offset,
                    vmo.raw() as usize,
                    vmo_offset as usize,
                    length,
                    (&raw mut address) as usize,
                ],
            )
        })?;
        Ok(address as usize)
    }

    /// 解除 `[address, address + length)` 的映射。
    ///
    /// # Safety
    ///
    /// 区间内不得再有调用者仍在使用的引用。
    ///
    /// # Errors
    ///
    /// `ERR_BAD_STATE`（与子 VMAR 部分重叠）、`ERR_INVALID_ARGS`。
    pub unsafe fn unmap(&self, address: usize, length: usize) -> Result<(), Status> {
        // SAFETY: 由调用者保证区间不再被引用。
        check(unsafe { invoke(call::VMAR_UNMAP, &[self.raw() as usize, address, length]) })
    }

    /// 改变权限。
    ///
    /// # Errors
    ///
    /// 权限超出映射上限或区间非法。
    pub fn protect(&self, options: u32, address: usize, length: usize) -> Result<(), Status> {
        // SAFETY: 无指针参数（区间由内核校验）。
        check(unsafe { invoke(call::VMAR_PROTECT, &[self.raw() as usize, options as usize, address, length]) })
    }

    /// 分配子 VMAR，返回子 VMAR 与其基址。
    ///
    /// # Errors
    ///
    /// 没有足够空隙或区间冲突。
    pub fn allocate(&self, options: u32, offset: usize, size: usize) -> Result<(Self, usize), Status> {
        let mut out = native_abi::HANDLE_INVALID;
        let mut address = 0u64;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::VMAR_ALLOCATE,
                &[
                    self.raw() as usize,
                    options as usize,
                    offset,
                    size,
                    (&raw mut out) as usize,
                    (&raw mut address) as usize,
                ],
            )
        };
        let handle = new_handle(status, out)?;
        Ok((Self(handle), address as usize))
    }

    /// 销毁 VMAR 及其全部映射。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::VMAR_DESTROY, &[self.raw() as usize]) })
    }

    /// 覆盖的地址区间。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn region(&self) -> Result<VmarRegion, Status> {
        self.info(native_abi::info::VMAR)
    }
}

impl Pager {
    /// 创建 pager。
    ///
    /// # Errors
    ///
    /// 句柄表或内存不足。
    pub fn create() -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe { invoke(call::PAGER_CREATE, &[(&raw mut out) as usize]) };
        new_handle(status, out).map(Self)
    }

    /// 创建由本 pager 支撑的 VMO；缺页请求以 `key` 投递到 `port`。
    ///
    /// # Errors
    ///
    /// 选项非法或内存不足。
    pub fn create_vmo(&self, options: u32, port: &Port, key: u64, size: u64) -> Result<Vmo, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::PAGER_CREATE_VMO,
                &[
                    self.raw() as usize,
                    options as usize,
                    port.raw() as usize,
                    key as usize,
                    size as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Vmo)
    }

    /// 把 `aux` 中 `[aux_offset, aux_offset + length)` 的页移入 `vmo` 的 `offset` 处。
    ///
    /// # Errors
    ///
    /// 区间未对齐或越界。
    pub fn supply_pages(&self, vmo: &Vmo, offset: u64, length: u64, aux: &Vmo, aux_offset: u64) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::PAGER_SUPPLY_PAGES,
                &[
                    self.raw() as usize,
                    vmo.raw() as usize,
                    offset as usize,
                    length as usize,
                    aux.raw() as usize,
                    aux_offset as usize,
                ],
            )
        })
    }

    /// 执行 pager 操作；`OP_QUERY_DIRTY` 返回第一段脏区间 `(offset, length)`。
    ///
    /// # Errors
    ///
    /// 操作非法或区间越界。
    pub fn op(&self, op: u32, vmo: &Vmo, offset: u64, length: u64) -> Result<(u64, u64), Status> {
        let mut out = [0u64; 2];
        // SAFETY: 输出位置有效。
        check(unsafe {
            invoke(
                call::PAGER_OP,
                &[
                    self.raw() as usize,
                    op as usize,
                    vmo.raw() as usize,
                    offset as usize,
                    length as usize,
                    out.as_mut_ptr() as usize,
                ],
            )
        })?;
        Ok((out[0], out[1]))
    }
}
