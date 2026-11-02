//! 祖先链的释放：析构深度恒定，不随 COW/切片链长递归。
use super::*;

impl Vmo {
    /// 取走本对象对父对象（COW 或切片）的强引用。
    fn take_parent(state: &mut State) -> Option<Arc<Vmo>> {
        match state.relation.take() {
            Some(Relation::Cow { parent, .. } | Relation::Slice { parent, .. }) => Some(parent),
            None => None,
        }
    }
}

impl Drop for Vmo {
    /// 逐层解开祖先链。
    ///
    /// 1. 每个仍存活的 fork 子进程在 COW 树中保留一层隐藏节点，祖先链长度随常驻子进程数增长；默认析构经
    ///    `Relation` 的 `Arc` 递归释放父对象，深度与链长成正比，会耗尽内核栈（常驻子进程较多时的批量退出）。
    /// 2. 这里先取走父引用；本对象是父对象的最后一个强引用时，把父对象移出 `Arc`、同样取走它的父引用后再析构它
    ///    （它的 `drop` 不再有父引用可递归），循环直到某个祖先仍被其他对象引用。析构深度因此恒定。
    fn drop(&mut self) {
        let mut parent = Self::take_parent(self.state.get_mut());
        while let Some(vmo) = parent {
            parent = match Arc::try_unwrap(vmo) {
                Ok(mut ancestor) => Self::take_parent(ancestor.state.get_mut()),
                Err(_) => None,
            };
        }
    }
}
