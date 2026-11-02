//! 原生映射驻留统计；只观察 backing，不触发 fault 或 pager。
use super::*;

impl Vmo {
    /// 只观察指定映射区间已有的 backing；包含 COW 祖先，不触发分配或 pager 请求。
    pub(crate) fn resident_pages(
        self: &Arc<Self>,
        first: usize,
        pages: usize,
    ) -> Result<usize, VmError> {
        let _tree = self.lock_tree()?;
        let mut ranges: alloc::vec::Vec<core::ops::Range<usize>> = alloc::vec::Vec::new();
        let mut current = self.clone();
        let (mut first, mut end) = (first, first.checked_add(pages).ok_or(VmError::OutOfRange)?);
        loop {
            let state = current.state.lock();
            end = end.min(state.pages);
            if first >= end {
                break;
            }
            match current.kind {
                Kind::Physical { .. } => break,
                Kind::Contiguous(_) => {
                    ranges.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                    ranges.push(0..end - first);
                    break;
                }
                _ => {
                    let visible = state
                        .committed
                        .iter_from(&first)
                        .take_while(|(index, _)| **index < end)
                        .count();
                    ranges.try_reserve(visible).map_err(|_| VmError::NoMemory)?;
                    for (&index, _) in state
                        .committed
                        .iter_from(&first)
                        .take_while(|(index, _)| **index < end)
                    {
                        ranges.push(index - first..index - first + 1);
                    }
                }
            }
            let next = match &state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) => {
                    end = end.min(*limit);
                    (first < end).then(|| (parent.clone(), *offset))
                }
                Some(Relation::Slice { parent, offset }) => Some((parent.clone(), *offset)),
                None => None,
            };
            drop(state);
            let Some((parent, offset)) = next else { break };
            first += offset;
            end += offset;
            current = parent;
        }
        ranges.sort_unstable_by_key(|range| range.start);
        let (mut count, mut covered) = (0, 0);
        for range in ranges {
            count += range.end.saturating_sub(covered.max(range.start));
            covered = covered.max(range.end);
        }
        Ok(count)
    }
}

impl Vmo {
    /// 本对象自己持有的已提交字节（不含从父对象共享的页）。
    ///
    /// # Errors
    /// 等待者分配失败返回 `NoMemory`。
    pub(crate) fn committed_bytes(&self) -> Result<u64, VmError> {
        let _tree = self.lock_tree()?;
        let state = self.state.lock();
        let pages = match &self.kind {
            Kind::Paged { .. } | Kind::Hidden => state.committed.len(),
            Kind::Contiguous(_) | Kind::Physical { .. } => state.pages,
            Kind::Slice => 0,
        };
        Ok((pages * PAGE_SIZE) as u64)
    }
}
