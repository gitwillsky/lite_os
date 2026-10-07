//! 把字节范围读写映射到整块读写：块设备节点（`/dev/vda`）的唯一字节寻址实现。
//!
//! 与具体设备解耦：`BlockStore` 只提供整块读写。对齐的整块直接进出调用者缓冲，不对齐的头尾经
//! 一块 scratch 做 read-modify-write；超出设备末尾的写入在末尾处截断。

/// 按整块读写的存储。
pub(super) trait BlockStore {
    type Error;

    fn block_size(&self) -> usize;
    fn block_count(&self) -> u64;
    fn read_block(&self, block: usize, buffer: &mut [u8]) -> Result<(), Self::Error>;
    fn write_block(&self, block: usize, buffer: &[u8]) -> Result<(), Self::Error>;
}

/// 范围 I/O 失败原因。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RangeError<E> {
    /// 写入起点已在设备末尾或之后（`ENOSPC`）。
    Full,
    /// 底层块读写失败。
    Store(E),
}

/// 在设备末尾处截断后实际可访问的字节数。
fn clipped<S: BlockStore>(store: &S, offset: u64, length: usize) -> usize {
    let capacity = store
        .block_count()
        .saturating_mul(store.block_size() as u64);
    usize::try_from(capacity.saturating_sub(offset)).map_or(length, |left| left.min(length))
}

/// 从 `offset` 读入 `output`；到设备末尾为止，返回读到的字节数（越过末尾为 0）。
///
/// # Parameters
///
/// - `scratch`: 恰好一块大小的缓冲，供不对齐的头尾使用。
pub(super) fn read_range<S: BlockStore>(
    store: &S,
    offset: u64,
    output: &mut [u8],
    scratch: &mut [u8],
) -> Result<usize, S::Error> {
    let block = store.block_size();
    assert_eq!(scratch.len(), block, "scratch must be exactly one block");
    let count = clipped(store, offset, output.len());
    let mut done = 0;
    while done < count {
        let position = offset + done as u64;
        let index = (position / block as u64) as usize;
        let inside = (position % block as u64) as usize;
        let part = (block - inside).min(count - done);
        if inside == 0 && part == block {
            store.read_block(index, &mut output[done..done + block])?;
        } else {
            store.read_block(index, scratch)?;
            output[done..done + part].copy_from_slice(&scratch[inside..inside + part]);
        }
        done += part;
    }
    Ok(count)
}

/// 把 `input` 写到 `offset`；到设备末尾为止，返回写入的字节数。
///
/// 不对齐的头尾先读出整块、合并后写回，保证同一块内不被改动的字节不变。已写入的前缀在后续块失败时
/// 保留（POSIX 短写）：失败发生在第一块之后返回已写字节数，由调用者的下一次写入重新遇到该错误。
///
/// # Errors
///
/// 起点不在设备范围内返回 [`RangeError::Full`]；第一块就失败返回 [`RangeError::Store`]。
pub(super) fn write_range<S: BlockStore>(
    store: &S,
    offset: u64,
    input: &[u8],
    scratch: &mut [u8],
) -> Result<usize, RangeError<S::Error>> {
    let block = store.block_size();
    assert_eq!(scratch.len(), block, "scratch must be exactly one block");
    let count = clipped(store, offset, input.len());
    if count == 0 && !input.is_empty() {
        return Err(RangeError::Full);
    }
    let mut done = 0;
    while done < count {
        let position = offset + done as u64;
        let index = (position / block as u64) as usize;
        let inside = (position % block as u64) as usize;
        let part = (block - inside).min(count - done);
        let step = if inside == 0 && part == block {
            store.write_block(index, &input[done..done + block])
        } else {
            store.read_block(index, scratch).and_then(|()| {
                scratch[inside..inside + part].copy_from_slice(&input[done..done + part]);
                store.write_block(index, scratch)
            })
        };
        match step {
            Ok(()) => done += part,
            Err(error) if done == 0 => return Err(RangeError::Store(error)),
            Err(_) => break,
        }
    }
    Ok(done)
}
