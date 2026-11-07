//! 未登记 CPU 字节缓冲的 VMO 适配；并发调用不在 I/O 期间占用 pool 锁。
use super::*;
use alloc::vec::Vec;
use native_runtime::native_abi;
use native_runtime::sync::Mutex;

/// 可复用的元数据/字节 I/O 缓冲。缓存最多一个协商传输窗口，且条目数不超过 queue depth。
///
/// pool 只持空闲登记；借出的缓冲由当前调用独占，失败/展开时其 Arc 仍正常释放登记。
#[derive(Default)]
pub struct BlockBufferPool {
    idle: Mutex<Vec<(usize, Arc<dyn BlockBuffer>)>>,
}

/// 一组批量读共用的暂存缓冲上限；更大的总量分组顺序处理。
const STAGING_LIMIT: usize = 1 << 20;

fn error(status: native_abi::Status) -> BlockError {
    match status {
        native_abi::ERR_NO_MEMORY | native_abi::ERR_NO_RESOURCES => BlockError::OutOfMemory,
        _ => BlockError::DeviceError,
    }
}

impl BlockBufferPool {
    fn with_buffer<T>(
        &self,
        device: &dyn BlockDevice,
        capacity: usize,
        operation: impl FnOnce(&dyn BlockBuffer) -> Result<T, BlockError>,
    ) -> Result<T, BlockError> {
        let budget = device
            .max_transfer_blocks()
            .checked_mul(device.block_size())
            .ok_or(BlockError::InvalidBlock)?;
        let cached = {
            let mut idle = self.idle.lock();
            let index = idle
                .iter()
                .enumerate()
                .filter(|(_, (bytes, _))| *bytes >= capacity)
                .min_by_key(|(_, (bytes, _))| *bytes)
                .map(|(index, _)| index);
            index.map(|index| idle.swap_remove(index))
        };
        let (capacity, buffer) = match cached {
            Some(cached) => cached,
            None => (
                capacity,
                device.attach_buffer(Vmo::create(capacity as u64, 0).map_err(error)?)?,
            ),
        };
        let result = operation(&*buffer);
        let mut buffer = Some(buffer);
        {
            let mut idle = self.idle.lock();
            let retained: usize = idle.iter().map(|(bytes, _)| *bytes).sum();
            // 失联可能先于宿主复位；错误缓冲退出复用池，实际页由 PMT quarantine 保活到复位，
            // 不能把同一页交给下一次 CPU I/O。成功缓冲才进入空闲缓存。
            if result.is_ok()
                && idle.len() < device.queue_depth()
                && retained
                    .checked_add(capacity)
                    .is_some_and(|bytes| bytes <= budget)
                && idle.try_reserve(1).is_ok()
            {
                idle.push((capacity, buffer.take().expect("buffer not yet returned")));
            }
        }
        // 不在 pool guard 内销毁远端登记；最后 Drop 可能向会话发送 detach。
        drop(buffer);
        result
    }

    fn window(device: &dyn BlockDevice, first: usize, length: usize) -> Result<usize, BlockError> {
        let block_size = device.block_size();
        if block_size == 0
            || length == 0
            || !length.is_multiple_of(block_size)
            || (first as u64)
                .checked_add((length / block_size) as u64)
                .is_none_or(|end| end > device.block_count())
        {
            return Err(BlockError::InvalidBlock);
        }
        device
            .max_transfer_blocks()
            .checked_mul(block_size)
            .filter(|bytes| *bytes != 0)
            .map(|bytes| bytes.min(length))
            .ok_or(BlockError::InvalidBlock)
    }

    /// 读完整逻辑块区间；失败不承诺目标缓冲保持原值。
    ///
    /// # Errors
    ///
    /// 区间无效、缓冲登记/分配或设备传输失败。
    pub fn read(
        &self,
        device: &dyn BlockDevice,
        first: usize,
        output: &mut [u8],
    ) -> Result<usize, BlockError> {
        let window = Self::window(device, first, output.len())?;
        self.with_buffer(device, window, |buffer| {
            let mut block = first as u64;
            for chunk in output.chunks_mut(window) {
                let count = chunk.len() / device.block_size();
                device.transfer(BlockOperation::Read, block, count, buffer, 0)?;
                buffer.vmo().read(chunk, 0).map_err(error)?;
                block += count as u64;
            }
            Ok(output.len())
        })
    }

    /// 读多个不相邻的完整块区间：同一组暂存缓冲登记一次，整组请求并发提交，再分别复制回各目标。
    ///
    /// 一组的暂存上限为 [`STAGING_LIMIT`]，更大的总量分组顺序处理；单个区间超过协商窗口时拆成多个请求。
    ///
    /// # Errors
    ///
    /// 区间无效、缓冲登记/分配或设备传输失败。
    pub fn read_many(
        &self,
        device: &dyn BlockDevice,
        ranges: &mut [(usize, &mut [u8])],
    ) -> Result<(), BlockError> {
        let block_size = device.block_size();
        let (mut index, mut offset) = (0, 0);
        while index < ranges.len() {
            // (区间下标, 区间内字节偏移, 设备起始块, 字节数)
            let mut pieces: Vec<(usize, usize, u64, usize)> = Vec::new();
            let mut staged = 0;
            while index < ranges.len() && staged < STAGING_LIMIT {
                let (first, output) = &ranges[index];
                let window = Self::window(device, *first, output.len())?;
                let take = (output.len() - offset)
                    .min(window)
                    .min(STAGING_LIMIT - staged)
                    / block_size
                    * block_size;
                if take == 0 {
                    break;
                }
                pieces.try_reserve(1).map_err(|_| BlockError::OutOfMemory)?;
                pieces.push((index, offset, (*first + offset / block_size) as u64, take));
                staged += take;
                offset += take;
                if offset == output.len() {
                    index += 1;
                    offset = 0;
                }
            }
            if pieces.is_empty() {
                return Err(BlockError::InvalidBlock);
            }
            self.with_buffer(device, staged, |buffer| {
                let mut requests: Vec<BlockRequest<'_>> = Vec::new();
                requests
                    .try_reserve_exact(pieces.len())
                    .map_err(|_| BlockError::OutOfMemory)?;
                let mut position = 0u64;
                for &(_, _, first, length) in &pieces {
                    requests.push(BlockRequest {
                        operation: BlockOperation::Read,
                        first_block: first,
                        blocks: length / block_size,
                        buffer,
                        buffer_offset: position,
                    });
                    position += length as u64;
                }
                device.transfer_batch(&requests)?;
                let mut position = 0u64;
                for &(range, within, _, length) in &pieces {
                    buffer
                        .vmo()
                        .read(&mut ranges[range].1[within..within + length], position)
                        .map_err(error)?;
                    position += length as u64;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// 写完整逻辑块区间；返回前设备已消费缓冲，持久化另用 flush。
    ///
    /// # Errors
    ///
    /// 区间无效、缓冲登记/分配或设备传输失败。
    pub fn write(
        &self,
        device: &dyn BlockDevice,
        first: usize,
        input: &[u8],
    ) -> Result<usize, BlockError> {
        let window = Self::window(device, first, input.len())?;
        self.with_buffer(device, window, |buffer| {
            let mut block = first as u64;
            for chunk in input.chunks(window) {
                let count = chunk.len() / device.block_size();
                buffer.vmo().write(chunk, 0).map_err(error)?;
                device.transfer(BlockOperation::Write, block, count, buffer, 0)?;
                block += count as u64;
            }
            Ok(input.len())
        })
    }
}
