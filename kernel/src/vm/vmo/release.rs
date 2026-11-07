//! 回收 COW 祖先，折叠单子节点的隐藏父对象，并逐层释放失去最后引用的祖先。
use super::*;

impl Vmo {
    /// 调用者持有树锁。只折叠唯一存活子对象是自己的隐藏父节点，不扫描仍有兄弟的祖先链。
    pub(super) fn collapse_hidden_parents(&self) {
        loop {
            let mut state = self.state.lock();
            let (parent, offset, limit) = match &state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) if matches!(parent.kind, Kind::Hidden) => (parent.clone(), *offset, *limit),
                _ => return,
            };
            let mut hidden = parent.state.lock();
            hidden.children.retain(|child| child.strong_count() != 0);
            if hidden.children.len() != 1 || !core::ptr::eq(hidden.children[0].as_ptr(), self) {
                return;
            }
            let visible = limit
                .min(state.pages)
                .min(hidden.pages.saturating_sub(offset));
            // 1. 完整继承时复用较大的索引。幸存对象通常只覆盖少量页，避免持树锁重排全部祖先页。
            // 页块的建立（可能分配）全部前移到任何改动之前：预留失败就放弃本次折叠，隐藏节点保持原状，
            // 只是少回收一次；移交阶段只做无分配的插入并保留仍可见页的物理地址。覆盖页的旧翻译已先完成 zap/fence。
            let full = offset == 0
                && visible == hidden.pages
                && state.committed.len() < hidden.committed.len();
            let reserved = if full {
                // swap 之后较大的列表（现为 hidden 的）成为 state 的，较小列表的页并入它。
                hidden
                    .committed
                    .reserve_for_shifted(&state.committed, 0, usize::MAX)
            } else {
                state
                    .committed
                    .reserve_for_shifted(&hidden.committed, offset, visible)
            };
            if reserved.is_err() {
                return;
            }
            let child = hidden.children.pop().expect("sole COW child disappeared");
            if full {
                core::mem::swap(&mut state.committed, &mut hidden.committed);
                while let Some((index, _)) = hidden.committed.first_key_value() {
                    let page = hidden
                        .committed
                        .remove(&index)
                        .expect("surviving page disappeared");
                    if let Some(previous) = state.committed.get_mut(&index) {
                        *previous = page;
                    } else {
                        state.committed.insert_reserved(index, page);
                    }
                }
            } else {
                // 部分继承必须按窗口求交，并跳过已有私有页。死亡兄弟的映射已退休。
                while let Some((index, _)) = hidden.committed.first_key_value() {
                    let page = hidden
                        .committed
                        .remove(&index)
                        .expect("hidden page disappeared");
                    if index >= offset
                        && index - offset < visible
                        && !state.committed.contains_key(&(index - offset))
                    {
                        state.committed.insert_reserved(index - offset, page);
                    }
                }
            }
            // 2. 替换祖父的直接子对象，并把两段可见窗口求交；不可达祖先不再保活。
            state.relation = match hidden.relation.take() {
                Some(Relation::Cow {
                    parent: ancestor,
                    offset: ancestor_offset,
                    limit: ancestor_limit,
                }) => {
                    let inherited = visible.min(ancestor_limit.saturating_sub(offset));
                    let mut ancestor_state = ancestor.state.lock();
                    let position = ancestor_state
                        .children
                        .iter()
                        .position(|entry| core::ptr::eq(entry.as_ptr(), Arc::as_ptr(&parent)))
                        .expect("hidden parent absent from COW tree");
                    if inherited == 0 {
                        ancestor_state.children.swap_remove(position);
                        None
                    } else {
                        ancestor_state.children[position] = child;
                        drop(ancestor_state);
                        Some(Relation::Cow {
                            parent: ancestor,
                            offset: ancestor_offset + offset,
                            limit: inherited,
                        })
                    }
                }
                None => None,
                Some(Relation::Slice { .. }) => {
                    unreachable!("hidden node cannot have a slice relation")
                }
            };
            // 3. hidden 已无资源或祖先引用，释放它后继续折叠下一层。
        }
    }

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
        if let Kind::Contiguous(frames) = &self.kind {
            residency::remove_contiguous(frames.pages);
        }
        let mut parent = Self::take_parent(self.state.get_mut());
        while let Some(vmo) = parent {
            parent = match Arc::try_unwrap(vmo) {
                Ok(mut ancestor) => Self::take_parent(ancestor.state.get_mut()),
                Err(_) => None,
            };
        }
    }
}
