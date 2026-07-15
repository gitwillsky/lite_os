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
