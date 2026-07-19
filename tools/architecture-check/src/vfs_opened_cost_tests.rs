use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PathKey {
    parent: u8,
    name: u8,
    inode: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    path: PathKey,
    deleted: bool,
}

#[derive(Clone, Default)]
struct ExactIndex {
    entries: BTreeMap<(PathKey, u16), bool>,
    memberships: BTreeMap<u16, PathKey>,
}

impl ExactIndex {
    fn register(&mut self, id: u16, path: PathKey) {
        if self.memberships.contains_key(&id) {
            return;
        }
        self.memberships.insert(id, path);
        assert!(self.entries.insert((path, id), false).is_none());
    }

    fn unregister(&mut self, id: u16) {
        let Some(path) = self.memberships.remove(&id) else {
            return;
        };
        assert!(self.entries.remove(&(path, id)).is_some());
    }

    fn unlink(&mut self, path: PathKey) {
        let keys = self
            .entries
            .range((path, 0)..=(path, u16::MAX))
            .map(|(&key, _)| key)
            .collect::<Vec<_>>();
        for key in keys {
            *self.entries.get_mut(&key).unwrap() = true;
        }
    }

    fn rename(&mut self, old: PathKey, new: PathKey) {
        let keys = self
            .entries
            .range((old, 0)..=(old, u16::MAX))
            .filter_map(|(&key, &deleted)| (!deleted).then_some(key))
            .collect::<Vec<_>>();
        for (_, id) in keys {
            let deleted = self.entries.remove(&(old, id)).unwrap();
            assert!(!deleted);
            assert!(self.entries.insert((new, id), false).is_none());
            self.memberships.insert(id, new);
        }
    }

    fn projection(&self) -> BTreeMap<u16, Entry> {
        self.memberships
            .iter()
            .map(|(&id, &path)| {
                (
                    id,
                    Entry {
                        path,
                        deleted: self.entries[&(path, id)],
                    },
                )
            })
            .collect()
    }
}

#[derive(Clone, Default)]
struct ScanReference(BTreeMap<u16, Entry>);

impl ScanReference {
    fn register(&mut self, id: u16, path: PathKey) {
        self.0.entry(id).or_insert(Entry {
            path,
            deleted: false,
        });
    }

    fn unregister(&mut self, id: u16) {
        self.0.remove(&id);
    }

    fn unlink(&mut self, path: PathKey) {
        for entry in self.0.values_mut().filter(|entry| entry.path == path) {
            entry.deleted = true;
        }
    }

    fn rename(&mut self, old: PathKey, new: PathKey) {
        for entry in self
            .0
            .values_mut()
            .filter(|entry| entry.path == old && !entry.deleted)
        {
            entry.path = new;
        }
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(2_862_933_555_777_941_757)
        .wrapping_add(3_037_000_493);
    *state
}

fn path(value: u64) -> PathKey {
    PathKey {
        parent: ((value >> 8) & 7) as u8,
        name: ((value >> 12) & 15) as u8,
        inode: ((value >> 20) & 31) as u8,
    }
}

#[test]
fn randomized_exact_index_matches_full_scan_semantics() {
    let mut exact = ExactIndex::default();
    let mut reference = ScanReference::default();
    let mut seed = 0x51a7_1f1e_u64;
    for _ in 0..20_000 {
        let value = random(&mut seed);
        let id = ((value >> 32) & 255) as u16;
        let current = path(value);
        match value & 3 {
            0 => {
                exact.register(id, current);
                reference.register(id, current);
            }
            1 => {
                exact.unregister(id);
                reference.unregister(id);
            }
            2 => {
                exact.unlink(current);
                reference.unlink(current);
            }
            _ => {
                let new = path(value.rotate_left(17));
                exact.rename(current, new);
                reference.rename(current, new);
            }
        }
        assert_eq!(exact.projection(), reference.0);
    }
}

#[derive(Clone, Copy)]
enum RacingOperation {
    Register,
    Rename,
    Unlink,
    FinalDrop,
}

#[test]
fn register_rename_unlink_and_final_drop_linearizations_match_scan() {
    fn explore(exact: ExactIndex, reference: ScanReference, remaining: &mut Vec<RacingOperation>) {
        if remaining.is_empty() {
            return;
        }
        let old = PathKey {
            parent: 1,
            name: 2,
            inode: 3,
        };
        let new = PathKey {
            parent: 4,
            name: 5,
            inode: 3,
        };
        for index in 0..remaining.len() {
            let operation = remaining.remove(index);
            let (mut next_exact, mut next_reference) = (exact.clone(), reference.clone());
            match operation {
                RacingOperation::Register => {
                    next_exact.register(9, old);
                    next_reference.register(9, old);
                }
                RacingOperation::Rename => {
                    next_exact.rename(old, new);
                    next_reference.rename(old, new);
                }
                RacingOperation::Unlink => {
                    next_exact.unlink(old);
                    next_reference.unlink(old);
                }
                RacingOperation::FinalDrop => {
                    next_exact.unregister(9);
                    next_reference.unregister(9);
                }
            }
            assert_eq!(next_exact.projection(), next_reference.0);
            explore(next_exact, next_reference, remaining);
            remaining.insert(index, operation);
        }
    }

    explore(
        ExactIndex::default(),
        ScanReference::default(),
        &mut vec![
            RacingOperation::Register,
            RacingOperation::Rename,
            RacingOperation::Unlink,
            RacingOperation::FinalDrop,
        ],
    );
}
