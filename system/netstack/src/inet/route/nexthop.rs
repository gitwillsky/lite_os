//! 同一 FIB 的路径去重与具名下一跳；报文直接索引槽，配置身份索引只在冷路径访问。
use super::*;
use core::num::{NonZeroU32, NonZeroU64};
use fallible_tree::{FallibleMap, VacantEntry};
use network_protocol::routes::NextHopConfiguration;

// 槽号加一编码，零只表示无下一跳；Option 不为每个前缀增加额外判别字。
pub(super) type Id = NonZeroU32;
pub(super) type Name = NonZeroU64;
use super::group::Group;
pub(super) use network_protocol::routes::NextHopMember as NextHop;
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Path(NextHop),
    Named(Name),
}
enum Slot {
    Anonymous {
        hop: NextHop,
        references: u32,
        link_scope: u32,
        host_scope: u32,
    },
    Named {
        name: Name,
        group: Group,
        references: u32,
        link_scope: u32,
        host_scope: u32,
    },
    Free {
        next: Option<Id>,
    },
}
/// 预备不增加引用或占用槽；同一 NET guard 内消费，不能跨路径变更保留 token。
pub(super) struct Prepared(Reservation);
enum Reservation {
    Existing(Id),
    New {
        entry: VacantEntry<Key, Id>,
        hop: NextHop,
    },
}
/// 更新前像只包含一个槽的路径值；槽身份/引用数和全部前缀均保持不动。
pub(super) struct Change {
    index: Id,
    replacement: Group,
}
pub(super) struct NextHops {
    slots: Vec<Slot>,
    // OWNER: 身份/值索引与槽属于同一复合 owner；不复制设备在线状态，不进入每包查询。
    by_key: FallibleMap<Key, Id>,
    free: Option<Id>,
}
impl NextHops {
    pub(super) fn new() -> Self {
        Self {
            slots: Vec::new(),
            by_key: FallibleMap::new(),
            free: None,
        }
    }
    pub(super) fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
    pub(super) fn view(&self, index: Id) -> (&[NextHop], Option<Name>) {
        match &self.slots[index.get() as usize - 1] {
            Slot::Anonymous { hop, .. } => (core::slice::from_ref(hop), None),
            Slot::Named { name, group, .. } => (group.members(), Some(*name)),
            Slot::Free { .. } => unreachable!("FIB route references a retired next hop"),
        }
    }
    pub(super) fn members(&self, index: Id) -> &[NextHop] {
        self.view(index).0
    }
    pub(super) fn get(&self, index: Id) -> (NextHop, Option<Name>) {
        let (members, name) = self.view(index);
        (
            *members
                .first()
                .expect("FIB route references an empty group"),
            name,
        )
    }
    pub(super) fn named(&self, name: Name) -> Option<Id> {
        self.by_key.get(&Key::Named(name)).copied()
    }
    pub(super) fn named_group(&self, name: Name) -> Result<&Group, SocketError> {
        let index = self.named(name).ok_or(SocketError::Invalid)?;
        match &self.slots[index.get() as usize - 1] {
            Slot::Named { group, .. } => Ok(group),
            _ => unreachable!("named index is not a group"),
        }
    }
    pub(super) fn slot_count(&self) -> usize {
        self.slots.len()
    }
    pub(super) fn active_groups(&self) -> impl Clone + Iterator<Item = (Id, &[NextHop])> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(position, slot)| {
                let members = match slot {
                    Slot::Anonymous {
                        hop, references, ..
                    } if *references != 0 => core::slice::from_ref(hop),
                    Slot::Named {
                        group, references, ..
                    } if *references != 0 => group.members(),
                    _ => return None,
                };
                Some((
                    Id::new(position as u32 + 1).expect("next-hop slot identity is bounded"),
                    members,
                ))
            })
    }
    pub(super) fn definitions(&self) -> impl Iterator<Item = NextHopConfiguration<'_>> + '_ {
        self.slots.iter().filter_map(|slot| match slot {
            Slot::Named { name, group, .. } => {
                Some(NextHopConfiguration::new(name.get(), group.members()))
            }
            _ => None,
        })
    }
    pub(super) fn define(&mut self, name: Name, group: Group) -> Result<(), SocketError> {
        let key = Key::Named(name);
        if self.by_key.contains_key(&key) {
            return Err(SocketError::Invalid);
        }
        let index = self.reserve_slot()?;
        let entry = self
            .by_key
            .try_prepare_vacant(key, index)
            .map_err(|_| SocketError::NoMemory)?;
        self.publish(
            entry,
            Slot::Named {
                name,
                group,
                references: 0,
                link_scope: 0,
                host_scope: 0,
            },
        );
        Ok(())
    }
    fn reserve_slot(&mut self) -> Result<Id, SocketError> {
        if let Some(index) = self.free {
            return Ok(index);
        }
        let index = u32::try_from(self.slots.len())
            .ok()
            .and_then(|index| index.checked_add(1))
            .and_then(Id::new)
            .ok_or(SocketError::NoSpace)?;
        self.slots
            .try_reserve(1)
            .map_err(|_| SocketError::NoMemory)?;
        Ok(index)
    }
    fn publish(&mut self, entry: VacantEntry<Key, Id>, slot: Slot) -> Id {
        let index = *entry.value();
        if index.get() as usize - 1 == self.slots.len() {
            self.slots.push(slot);
        } else {
            let Slot::Free { next } = self.slots[index.get() as usize - 1] else {
                unreachable!("prepared next hop slot is occupied");
            };
            self.free = next;
            self.slots[index.get() as usize - 1] = slot;
        }
        self.by_key.commit_vacant(entry);
        index
    }
    fn prepare_new(
        &mut self,
        key: Key,
        hop: NextHop,
    ) -> Result<(VacantEntry<Key, Id>, NextHop), SocketError> {
        let index = self.reserve_slot()?;
        let entry = self
            .by_key
            .try_prepare_vacant(key, index)
            .map_err(|_| SocketError::NoMemory)?;
        Ok((entry, hop))
    }
    pub(super) fn prepare(
        &mut self,
        hop: NextHop,
        name: Option<Name>,
    ) -> Result<Prepared, SocketError> {
        let key = name.map_or(Key::Path(hop), Key::Named);
        if let Some(&index) = self.by_key.get(&key) {
            let references = match self.slots[index.get() as usize - 1] {
                Slot::Anonymous { references, .. } | Slot::Named { references, .. } => references,
                Slot::Free { .. } => unreachable!("next-hop index points at a free slot"),
            };
            if references == u32::MAX {
                return Err(SocketError::NoSpace);
            }
            return Ok(Prepared(Reservation::Existing(index)));
        }
        if name.is_some() {
            return Err(SocketError::Invalid);
        }
        let (entry, hop) = self.prepare_new(key, hop)?;
        Ok(Prepared(Reservation::New { entry, hop }))
    }
    fn commit_new(&mut self, entry: VacantEntry<Key, Id>, hop: NextHop, scope: u8) -> Id {
        self.publish(
            entry,
            Slot::Anonymous {
                hop,
                references: 1,
                link_scope: u32::from(scope >= 253),
                host_scope: u32::from(scope >= 254),
            },
        )
    }
    pub(super) fn acquire(&mut self, prepared: Prepared, scope: u8) -> Id {
        match prepared.0 {
            Reservation::Existing(index) => {
                match &mut self.slots[index.get() as usize - 1] {
                    Slot::Anonymous {
                        references,
                        link_scope,
                        host_scope,
                        ..
                    }
                    | Slot::Named {
                        references,
                        link_scope,
                        host_scope,
                        ..
                    } => {
                        *references += 1;
                        *link_scope += u32::from(scope >= 253);
                        *host_scope += u32::from(scope >= 254);
                    }
                    Slot::Free { .. } => unreachable!("prepared next hop was retired"),
                }
                index
            }
            Reservation::New { entry, hop } => self.commit_new(entry, hop, scope),
        }
    }
    pub(super) fn change_scope(&mut self, index: Id, previous: u8, next: u8) {
        match &mut self.slots[index.get() as usize - 1] {
            Slot::Anonymous {
                link_scope,
                host_scope,
                ..
            }
            | Slot::Named {
                link_scope,
                host_scope,
                ..
            } => {
                *link_scope -= u32::from(previous >= 253);
                *link_scope += u32::from(next >= 253);
                *host_scope -= u32::from(previous >= 254);
                *host_scope += u32::from(next >= 254);
            }
            Slot::Free { .. } => unreachable!("FIB scope change references a retired next hop"),
        }
    }
    pub(super) fn release(&mut self, index: Id, scope: u8) {
        let slot = &mut self.slots[index.get() as usize - 1];
        match slot {
            Slot::Anonymous {
                hop,
                references,
                link_scope,
                host_scope,
            } => {
                *references -= 1;
                *link_scope -= u32::from(scope >= 253);
                *host_scope -= u32::from(scope >= 254);
                if *references == 0 {
                    self.by_key.remove(&Key::Path(*hop));
                    *slot = Slot::Free { next: self.free };
                    self.free = Some(index);
                }
            }
            // 具名定义独立于路由引用寿命；全量受管集合替换才删除旧定义。
            Slot::Named {
                references,
                link_scope,
                host_scope,
                ..
            } => {
                *references -= 1;
                *link_scope -= u32::from(scope >= 253);
                *host_scope -= u32::from(scope >= 254);
            }
            Slot::Free { .. } => unreachable!("FIB released a retired next hop"),
        }
    }
    pub(super) fn retire(&mut self, index: u32, force: bool) {
        for slot in &mut self.slots {
            if let Slot::Named { group, .. } = slot {
                group.retire(index, force);
            }
        }
    }
    pub(super) fn remove_missing(&mut self, interfaces: &[NetworkInterface]) {
        for slot in &mut self.slots {
            if let Slot::Named { group, .. } = slot {
                group.remove_missing(interfaces);
            }
        }
    }
    pub(super) fn prepare_change(
        &self,
        name: Name,
        replacement: Group,
    ) -> Result<Change, SocketError> {
        let index = self.named(name).ok_or(SocketError::NoDevice)?;
        if let Slot::Named {
            link_scope,
            host_scope,
            ..
        } = self.slots[index.get() as usize - 1]
            && (link_scope != 0
                && replacement
                    .members()
                    .iter()
                    .any(|member| member.gateway.is_some())
                || host_scope != 0 && replacement.members().len() != 1)
        {
            return Err(SocketError::Invalid);
        }
        Ok(Change { index, replacement })
    }
    pub(super) fn exchange(&mut self, change: &mut Change) -> &Group {
        let Slot::Named { group, .. } = &mut self.slots[change.index.get() as usize - 1] else {
            unreachable!("configuration lease lost a named next hop");
        };
        core::mem::swap(group, &mut change.replacement);
        group
    }
}
