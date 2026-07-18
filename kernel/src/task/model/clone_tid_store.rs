/// @description 按 Linux clone best-effort 语义尝试全部 parent/child TID stores。
/// @param addresses flags 选中的用户地址；Some(0) 仍执行一次会失败但被忽略的 store。
/// @param store 单个用户地址写入动作。
/// @return 无返回值；任一 store failure 不阻止后续 store，也不回滚已创建 Thread。
pub(super) fn store_clone_tid_values<E>(
    addresses: [Option<usize>; 2],
    mut store: impl FnMut(usize) -> Result<(), E>,
) {
    for address in addresses.into_iter().flatten() {
        let _ = store(address);
    }
}
