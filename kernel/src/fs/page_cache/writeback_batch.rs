use core::cell::Cell;

/// 单次 page-cache storage transaction 的最大 logical page 数。
pub(super) const WRITEBACK_BATCH_PAGES: usize = 32;
/// 单次 regular write storage transaction 的最大 logical page 数。
pub(super) const REGULAR_WRITE_BATCH_PAGES: usize = 256;

/// regular byte batch 已持久提交并可向 cache/syscall 发布的连续 prefix。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CommittedPrefix {
    pub(super) offset: u64,
    pub(super) bytes: usize,
}

enum ContiguousAttemptError<Error> {
    Backend(Error),
    Short,
}

/// @description 按成功提交的最大已知 chunk 顺序处理固定 writeback batch。
///
/// capacity error 只在 chunk 大于一项时触发二分退避；成功后才调用 `publish`，
/// 因此后续失败不会把尚未提交的 suffix 标 clean。
#[inline(always)]
pub(super) fn commit_with_backoff<T, Error>(
    entries: &[T],
    mut commit: impl FnMut(&[T]) -> Result<(), Error>,
    mut publish: impl FnMut(&[T]),
    mut capacity_error: impl FnMut(&Error) -> bool,
) -> Result<(), Error> {
    let mut first = 0;
    let mut chunk_limit = entries.len();
    while first < entries.len() {
        let count = chunk_limit.min(entries.len() - first);
        let chunk = &entries[first..first + count];
        match commit(chunk) {
            Ok(()) => {
                publish(chunk);
                first += count;
            }
            Err(error) if count > 1 && capacity_error(&error) => {
                chunk_limit = count.div_ceil(2);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// @description 以固定 logical-unit 上限提交连续 byte prefix，并复用 capacity 二分退避。
/// @param byte_count 非零 byte 数，最多 `REGULAR_WRITE_BATCH_PAGES * unit_bytes`。
/// @param unit_bytes 退避的最小 logical unit；通常为 page size。
/// @param commit 接收当前已提交 byte 数与本次连续 byte 数，返回实际 storage offset/bytes。
/// @param publish 每个成功 durable transaction 后发布对应 byte range。
/// @param capacity_error 标识可在未 publication 时缩小 transaction 重试的 backend error。
/// @return 全部提交、storage short 或后续错误前已提交的连续 prefix；首笔 backend error 原样返回。
pub(super) fn commit_contiguous_prefix_with_backoff<Error>(
    byte_count: usize,
    unit_bytes: usize,
    mut commit: impl FnMut(usize, usize) -> Result<(u64, usize), Error>,
    mut publish: impl FnMut(u64, usize, usize),
    mut capacity_error: impl FnMut(&Error) -> bool,
) -> Result<CommittedPrefix, Error> {
    assert!(byte_count != 0);
    assert!(unit_bytes != 0);
    assert!(byte_count <= REGULAR_WRITE_BATCH_PAGES * unit_bytes);
    let units = [(); REGULAR_WRITE_BATCH_PAGES];
    let unit_count = byte_count.div_ceil(unit_bytes);
    let committed = Cell::new(0usize);
    let first_offset = Cell::new(None::<u64>);
    let transaction = Cell::new(None::<(u64, usize)>);

    let result = commit_with_backoff(
        &units[..unit_count],
        |chunk| {
            let start = committed.get();
            let count = (chunk.len() * unit_bytes).min(byte_count - start);
            transaction.set(None);
            let (offset, written) =
                commit(start, count).map_err(ContiguousAttemptError::Backend)?;
            assert!(
                written <= count,
                "storage write exceeded requested byte range"
            );
            if let Some(first) = first_offset.get() {
                assert_eq!(
                    offset,
                    first
                        .checked_add(start as u64)
                        .expect("contiguous storage offset overflow"),
                    "storage batch returned a non-contiguous byte range"
                );
            } else {
                first_offset.set(Some(offset));
            }
            transaction.set(Some((offset, written)));
            if written == count {
                Ok(())
            } else {
                Err(ContiguousAttemptError::Short)
            }
        },
        |_| {
            let start = committed.get();
            let (offset, written) = transaction
                .take()
                .expect("successful storage batch lost publication range");
            publish(offset, start, written);
            committed.set(start + written);
        },
        |error| match error {
            ContiguousAttemptError::Backend(error) => capacity_error(error),
            ContiguousAttemptError::Short => false,
        },
    );

    match result {
        Ok(()) => Ok(CommittedPrefix {
            offset: first_offset
                .get()
                .expect("non-empty successful batch lost first offset"),
            bytes: committed.get(),
        }),
        Err(ContiguousAttemptError::Short) => {
            let start = committed.get();
            let (offset, written) = transaction
                .take()
                .expect("short storage batch lost committed range");
            publish(offset, start, written);
            Ok(CommittedPrefix {
                offset: first_offset
                    .get()
                    .expect("short storage batch lost first offset"),
                bytes: start + written,
            })
        }
        Err(ContiguousAttemptError::Backend(_)) if committed.get() != 0 => Ok(CommittedPrefix {
            offset: first_offset
                .get()
                .expect("partial storage batch lost first offset"),
            bytes: committed.get(),
        }),
        Err(ContiguousAttemptError::Backend(error)) => Err(error),
    }
}
