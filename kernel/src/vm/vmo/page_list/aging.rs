//! 以页块为节点的八代队列：每代一个槽位图和双向链接，不逐页分配节点。
//! 页状态的变更都经 PageList 事务更新；链表只定位非空位图，淘汰不扫描脏页/固定页。
use super::*;
use core::num::NonZeroUsize;

type Link = Option<NonZeroUsize>;
#[derive(Clone, Copy, Default)]
struct Links {
    previous: Link,
    next: Link,
}
#[derive(Clone, Copy, Default)]
struct Ends {
    first: Link,
    last: Link,
}

pub(super) struct GenerationIndex {
    queues: [Ends; 8],
}
impl GenerationIndex {
    fn new() -> Self {
        Self {
            queues: [Ends::default(); 8],
        }
    }
}
pub(super) struct ChunkAges {
    pub(super) masks: [u64; 8],
    links: [Links; 8],
}
impl ChunkAges {
    pub(super) fn new() -> Self {
        Self {
            masks: [0; 8],
            links: [Links::default(); 8],
        }
    }
}
fn encode(key: usize) -> NonZeroUsize {
    NonZeroUsize::new(key.checked_add(1).expect("page chunk key overflow"))
        .expect("encoded chunk key is nonzero")
}
fn decode(key: NonZeroUsize) -> usize {
    key.get() - 1
}

impl<T: Slot> PageList<T> {
    fn ages_mut(&mut self, key: usize) -> &mut ChunkAges {
        self.chunks
            .get_mut(&key)
            .expect("generation chunk disappeared")
            .ages
            .as_deref_mut()
            .expect("untracked generation chunk")
    }

    pub(super) fn link_generation(&mut self, key: usize, generation: usize) {
        let tail = self
            .generations
            .as_ref()
            .expect("untracked generation index")
            .queues[generation]
            .last;
        self.ages_mut(key).links[generation] = Links {
            previous: tail,
            next: None,
        };
        if let Some(tail) = tail {
            self.ages_mut(decode(tail)).links[generation].next = Some(encode(key));
        }
        let queue = &mut self.generations.as_mut().expect("untracked index").queues[generation];
        if queue.first.is_none() {
            queue.first = Some(encode(key));
        }
        queue.last = Some(encode(key));
    }

    pub(super) fn unlink_generation(&mut self, key: usize, generation: usize) {
        let links = self.ages_mut(key).links[generation];
        if let Some(previous) = links.previous {
            self.ages_mut(decode(previous)).links[generation].next = links.next;
        }
        if let Some(next) = links.next {
            self.ages_mut(decode(next)).links[generation].previous = links.previous;
        }
        let queue = &mut self.generations.as_mut().expect("untracked index").queues[generation];
        if links.previous.is_none() {
            queue.first = links.next;
        }
        if links.next.is_none() {
            queue.last = links.previous;
        }
        self.ages_mut(key).links[generation] = Links::default();
    }

    pub(super) fn refresh_generation(
        &mut self,
        index: usize,
        before: Option<u8>,
        after: Option<u8>,
    ) {
        if self.generations.is_none() || before == after {
            return;
        }
        let key = index / CHUNK_PAGES;
        let bit = 1u64 << (index % CHUNK_PAGES);
        if let Some(generation) = before {
            let mask = &mut self.ages_mut(key).masks[generation as usize];
            *mask &= !bit;
            if *mask == 0 {
                self.unlink_generation(key, generation as usize);
            }
        }
        if let Some(generation) = after {
            let mask = &mut self.ages_mut(key).masks[generation as usize];
            let empty = *mask == 0;
            *mask |= bit;
            if empty {
                self.link_generation(key, generation as usize);
            }
        }
    }

    /// 一代是否已没有可回收成员；不扫描块或页。
    pub(in crate::vm::vmo) fn generation_empty(&self, generation: u8) -> bool {
        self.generations
            .as_ref()
            .is_none_or(|index| index.queues[generation as usize].first.is_none())
    }

    /// 只遍历这一代的成员；选过的块轮换到队尾，年轻热点不能长期挡住其他块。
    pub(in crate::vm::vmo) fn visit_generation(
        &mut self,
        generation: u8,
        mut visit: impl FnMut(usize, &T),
    ) -> usize {
        let Some(index) = self.generations.as_ref() else {
            return 0;
        };
        let generation = generation as usize;
        let ends = index.queues[generation];
        let mut cursor = ends.first;
        let mut visited = 0;
        while let Some(encoded) = cursor {
            if visited == CHUNK_PAGES {
                break;
            }
            let key = decode(encoded);
            let chunk = self
                .chunks
                .get(&key)
                .expect("queued page chunk disappeared");
            let ages = chunk.ages.as_ref().expect("queued chunk lost ages");
            let next = ages.links[generation].next;
            let mut mask = ages.masks[generation];
            assert_ne!(mask, 0, "empty generation chunk was linked");
            // 不拆分一个块的候选：否则较小热点块占掉预算时，后一块末端会反复被跳过。
            // 未访问的块仍是队首，下批拥有完整的 64 槽预算，保证每个成员最终都能进入样本。
            if mask.count_ones() as usize > CHUNK_PAGES - visited {
                break;
            }

            while mask != 0 {
                let slot = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                visit(
                    key * CHUNK_PAGES + slot,
                    chunk.slots[slot]
                        .as_ref()
                        .expect("generation bit points at empty slot"),
                );
                visited += 1;
            }
            self.unlink_generation(key, generation);
            self.link_generation(key, generation);
            if Some(encoded) == ends.last {
                break;
            }
            cursor = next;
        }
        visited
    }
}

impl PageList<Page> {
    /// pager 后备列表的分代头在 VMO 发表前准备；匿名、bootfs 和请求列表沿用无索引存储。
    /// # Errors
    /// 头部存储分配失败，列表尚未发表且没有页 owner。
    pub(in crate::vm::vmo) fn try_reclaimable() -> Result<Self, ()> {
        Ok(Self {
            generations: Some(Box::try_new(GenerationIndex::new()).map_err(|_| ())?),
            ..Self::new()
        })
    }
}
