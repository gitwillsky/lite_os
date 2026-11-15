//! 以同一地址空间观察指定路由域的 Linux 网络对象。

use std::io;

/// 在 `domain` 的网络命名空间执行查询。具名域使用独占线程，避免改变后台线程池的命名空间。
/// 错误：命名空间未授权、不可用，查询失败或独占线程异常退出。
pub(super) fn read<T: Send>(
    domain: Option<&str>,
    query: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    use std::os::fd::AsFd;

    let Some(name) = domain else { return query() };
    let namespace = network_config::open_network_namespace(name)?;
    std::thread::scope(|thread| {
        std::thread::Builder::new()
            .spawn_scoped(thread, move || {
                linux_uapi::process::enter_network_namespace(namespace.as_fd())?;
                query()
            })?
            .join()
            .map_err(|_| io::Error::other("网络观察线程异常退出。"))?
    })
}
