//! 两个有序集合的惰性合并；查询命中不分配，只有新的不同集合才捕获精确容量。
use super::{CommunityEdit, OutOfMemory, Pool, Shared, Vec};
use core::cmp::Ordering;

#[derive(Clone)]
struct Values<'a, T> {
    base: &'a [T],
    remove: &'a [T],
    add: &'a [T],
}
impl<T: Ord + Copy> Iterator for Values<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        loop {
            // 1. 两路取最小值，同值只交出一次；added 保存添加动作优先级，不能被后续删除误吞。
            let (value, added) = match (self.base.first(), self.add.first()) {
                (None, None) => return None,
                (None, Some(&value)) => {
                    self.add = &self.add[1..];
                    (value, true)
                }
                (Some(&value), None) => {
                    self.base = &self.base[1..];
                    (value, false)
                }
                (Some(&base), Some(&added)) => match base.cmp(&added) {
                    Ordering::Less => {
                        self.base = &self.base[1..];
                        (base, false)
                    }
                    Ordering::Greater => {
                        self.add = &self.add[1..];
                        (added, true)
                    }
                    Ordering::Equal => {
                        self.base = &self.base[1..];
                        self.add = &self.add[1..];
                        (base, true)
                    }
                },
            };
            // 2. 删除 cursor 只前进，整批合并不会为每个元素重新查找删除列表。
            while self.remove.first().is_some_and(|item| *item < value) {
                self.remove = &self.remove[1..];
            }
            // 3. 顺序是先删除再添加，同值新增因此始终保留。
            if added || self.remove.first() != Some(&value) {
                return Some(value);
            }
        }
    }
}
pub(super) fn project<T: Ord + Copy>(
    pool: &mut Pool<Vec<T>>,
    current: &Option<Shared<Vec<T>>>,
    edit: Option<&CommunityEdit<T>>,
) -> Result<Option<Shared<Vec<T>>>, OutOfMemory> {
    let Some(edit) = edit else {
        return Ok(current.clone());
    };
    if edit.replace.is_none() && edit.remove.is_empty() && edit.add.is_empty() {
        return Ok(current.clone());
    }
    let original = current
        .as_ref()
        .map_or(&[][..], |value| value.get().as_slice());
    let values = Values {
        base: edit.replace.as_deref().unwrap_or(original),
        remove: &edit.remove,
        add: &edit.add,
    };
    if values.clone().eq(original.iter().copied()) {
        return Ok(current.clone());
    }
    if values.clone().next().is_none() {
        return Ok(None);
    }
    if let Some(shared) = pool.find_by(|stored| values.clone().cmp(stored.iter().copied())) {
        return Ok(Some(shared));
    }
    let mut captured = Vec::new();
    captured
        .try_reserve_exact(values.clone().count())
        .map_err(|_| OutOfMemory)?;
    captured.extend(values);
    pool.intern(captured).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;

    fn values(bits: u8) -> Vec<u32> {
        (0..3).filter(|&value| bits & (1u8 << value) != 0).collect()
    }
    #[test]
    fn edits_match_set_algebra_and_release_every_unreferenced_projection() {
        // 指针推进/同值删除再添加及 replace 边界不能只靠几个报文示例覆盖；小全集对照独立集合模型。
        for base in 0..8 {
            let original = values(base);
            let mut pool = Pool::try_new().unwrap();
            let current = if original.is_empty() {
                None
            } else {
                Some(pool.intern(original.clone()).unwrap())
            };
            for replacement in core::iter::once(None).chain((0..8).map(Some)) {
                for remove in 0..8 {
                    for add in 0..8 {
                        let edit = CommunityEdit {
                            replace: replacement.map(values),
                            remove: values(remove),
                            add: values(add),
                        };
                        let mut expected: BTreeSet<_> = edit
                            .replace
                            .as_ref()
                            .unwrap_or(&original)
                            .iter()
                            .copied()
                            .collect();
                        for value in &edit.remove {
                            expected.remove(value);
                        }
                        expected.extend(edit.add.iter().copied());
                        let actual = project(&mut pool, &current, Some(&edit)).unwrap();
                        assert!(
                            actual
                                .as_ref()
                                .map_or(&[][..], |value| value.get().as_slice())
                                .iter()
                                .copied()
                                .eq(expected)
                        );
                        assert_eq!(
                            current
                                .as_ref()
                                .map_or(&[][..], |value| value.get().as_slice()),
                            original
                        );
                        drop(actual);
                        assert_eq!(pool.len(), usize::from(current.is_some()));
                    }
                }
            }
        }
    }
}
