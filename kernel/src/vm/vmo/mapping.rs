//! 映射登记与 COW/slice 别名投影；撤销和访问采集共用同一显式工作栈。
//! 调用者持 VMO 树锁，软件遍历不按别名深度递归，不持 state spin guard 等待地址空间或分配。
use super::*;
use core::ops::Range;

type AliasProjection = (Arc<Vmo>, usize, Range<usize>);

impl Vmo {
    /// 登记一个映射本对象的映射。
    ///
    /// # Errors
    /// 分配失败返回 `NoMemory`。
    pub(in crate::vm) fn register_mapping(&self, mapping: &Arc<Mapping>) -> Result<(), VmError> {
        let _tree = self.lock_tree()?;
        // 树锁保有整个登记事务；临时移出 Vec 后才准备容量，不在 state spin guard 内分配。
        // 任何失败都归还原 owner，其他合法映射遍历也必须持同一树锁。
        let mut mappings = core::mem::take(&mut self.state.lock().mappings);
        mappings.retain(|mapping| mapping.strong_count() != 0);
        let result = mappings.try_reserve(1);
        if result.is_ok() {
            mappings.push(Arc::downgrade(mapping));
        }
        self.state.lock().mappings = mappings;
        result.map_err(|_| VmError::NoMemory)
    }

    /// 树锁内遍历映射，升级 weak 引用后立即释放 state guard；回调可以阻塞等待空间锁。
    fn for_each_mapping(
        &self,
        mut visit: impl FnMut(&Mapping) -> Result<(), VmError>,
    ) -> Result<(), VmError> {
        let count = self.state.lock().mappings.len();
        for index in 0..count {
            let mapping = self
                .state
                .lock()
                .mappings
                .get(index)
                .and_then(Weak::upgrade);
            if let Some(mapping) = mapping {
                visit(&mapping)?;
            }
        }
        Ok(())
    }

    fn queue_aliases(
        &self,
        origin: usize,
        visible: Range<usize>,
        pending: &mut Vec<AliasProjection>,
    ) -> Result<(), VmError> {
        let count = self.state.lock().children.len();
        for index in 0..count {
            let child = self
                .state
                .lock()
                .children
                .get(index)
                .and_then(Weak::upgrade);
            let Some(child) = child else { continue };
            let projection = {
                let state = child.state.lock();
                match &state.relation {
                    Some(Relation::Cow { offset, limit, .. }) => {
                        Some((*offset, (*limit).min(state.pages)))
                    }
                    Some(Relation::Slice { offset, .. }) => Some((*offset, state.pages)),
                    None => None,
                }
            };
            let Some((offset, length)) = projection else {
                continue;
            };
            if length == 0 {
                continue;
            }
            let start = origin.checked_add(offset).ok_or(VmError::OutOfRange)?;
            if start >= visible.end {
                continue;
            }
            let end = start.checked_add(length).ok_or(VmError::OutOfRange)?;
            let range = visible.start.max(start)..visible.end.min(end);
            if !range.is_empty() {
                pending.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                pending.push((child, start, range));
            }
        }
        Ok(())
    }

    /// origin 把当前对象下标投影到起始 owner；descendant 区分根写保护与子孙重新解析。
    /// 根范围不按当前 size 裁剪：truncate 在更新 size 后还须撤销旧窗口的翻译。
    fn visit_aliases(
        &self,
        pages: Range<usize>,
        mut visit: impl FnMut(&Vmo, usize, Range<usize>, bool) -> Result<(), VmError>,
    ) -> Result<(), VmError> {
        visit(self, 0, pages.clone(), false)?;
        let mut pending = Vec::new();
        self.queue_aliases(0, pages, &mut pending)?;
        while let Some((object, origin, visible)) = pending.pop() {
            visit(&object, origin, visible.clone(), true)?;
            object.queue_aliases(origin, visible, &mut pending)?;
        }
        Ok(())
    }

    /// 树锁内撤销翻译并完成 fence；后代重新缺页，不能沿用旧的 COW 解析结果。
    pub(super) fn zap(&self, pages: Range<usize>, mode: ZapMode) -> Result<(), VmError> {
        self.visit_aliases(pages, |object, origin, visible, descendant| {
            let mode = if descendant { ZapMode::Unmap } else { mode };
            object.for_each_mapping(|mapping| {
                mapping.zap_vmo_pages(visible.start - origin..visible.end - origin, mode)
            })
        })
    }

    /// 树锁内按物理身份采集别名访问；样本之外及 COW 窗口外的子树不参与。
    pub(super) fn harvest_accessed(&self, samples: &mut [AccessSample]) -> Result<(), VmError> {
        let Some(first) = samples.first() else {
            return Ok(());
        };
        let end = samples
            .last()
            .expect("nonempty access samples")
            .index
            .checked_add(1)
            .ok_or(VmError::OutOfRange)?;
        let pages = first.index..end;
        self.visit_aliases(pages, |object, origin, visible, _| {
            object.for_each_mapping(|mapping| {
                mapping.harvest_accessed(samples, origin, visible.clone())
            })
        })
    }
}
