use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    thread,
};

const SHARDS: usize = 16;
const LIVE: u8 = 0;

type SourceIndex = Mutex<BTreeMap<(u64, u64), Arc<Registration>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Winner {
    Wake = 1,
    Timeout = 2,
    Signal = 3,
    Cancel = 4,
}

struct Registration {
    id: u64,
    sources: Mutex<Vec<u64>>,
    state: AtomicU8,
}

struct Registry {
    shards: [SourceIndex; SHARDS],
}

impl Registry {
    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(BTreeMap::new())),
        }
    }

    fn shard(source: u64) -> usize {
        source as usize & (SHARDS - 1)
    }

    fn publish(
        &self,
        id: u64,
        sources: &[u64],
        fail_at: Option<usize>,
    ) -> Result<Arc<Registration>, ()> {
        let mut prepared = Vec::new();
        prepared.try_reserve_exact(sources.len()).map_err(|_| ())?;
        for (index, source) in sources.iter().copied().enumerate() {
            if prepared.contains(&source) {
                continue;
            }
            if fail_at == Some(index) {
                return Err(());
            }
            prepared.push(source);
        }
        let registration = Arc::new(Registration {
            id,
            sources: Mutex::new(prepared),
            state: AtomicU8::new(LIVE),
        });
        let mut shards = registration
            .sources
            .lock()
            .unwrap()
            .iter()
            .map(|source| Self::shard(*source))
            .collect::<Vec<_>>();
        shards.sort_unstable();
        shards.dedup();
        let mut guards = shards
            .iter()
            .map(|shard| (*shard, self.shards[*shard].lock().unwrap()))
            .collect::<Vec<_>>();
        for source in registration.sources.lock().unwrap().iter() {
            let guard = guards
                .iter_mut()
                .find(|(shard, _)| *shard == Self::shard(*source))
                .unwrap();
            assert!(
                guard
                    .1
                    .insert((*source, id), registration.clone())
                    .is_none()
            );
        }
        Ok(registration)
    }

    fn claim(&self, registration: &Arc<Registration>, winner: Winner) -> bool {
        if registration
            .state
            .compare_exchange(LIVE, winner as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let sources = registration.sources.lock().unwrap().clone();
        for source in sources {
            let removed = self.shards[Self::shard(source)]
                .lock()
                .unwrap()
                .remove(&(source, registration.id));
            assert!(removed.is_some());
        }
        true
    }

    fn requeue(&self, registration: &Arc<Registration>, source: u64, target: u64) -> bool {
        if registration.state.load(Ordering::Acquire) != LIVE {
            return false;
        }
        let mut sources = registration.sources.lock().unwrap();
        let Some(slot) = sources.iter_mut().find(|candidate| **candidate == source) else {
            return false;
        };
        let node = self.shards[Self::shard(source)]
            .lock()
            .unwrap()
            .remove(&(source, registration.id))
            .unwrap();
        assert!(
            self.shards[Self::shard(target)]
                .lock()
                .unwrap()
                .insert((target, registration.id), node)
                .is_none()
        );
        *slot = target;
        true
    }

    fn nodes(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().unwrap().len())
            .sum()
    }
}

#[test]
fn multi_shard_publication_oom_has_no_partial_membership() {
    let registry = Registry::new();
    let sources = [1, 18, 35, 52, 69];
    for fail_at in 0..sources.len() {
        assert!(
            registry
                .publish(fail_at as u64 + 1, &sources, Some(fail_at))
                .is_err()
        );
        assert_eq!(registry.nodes(), 0);
    }
}

#[test]
fn completion_and_requeue_linearizations_have_one_winner_and_no_stale_nodes() {
    fn permutations(values: &mut [Winner], start: usize, output: &mut Vec<Vec<Winner>>) {
        if start == values.len() {
            output.push(values.to_vec());
            return;
        }
        for index in start..values.len() {
            values.swap(start, index);
            permutations(values, start + 1, output);
            values.swap(start, index);
        }
    }

    let mut orders = Vec::new();
    permutations(
        &mut [
            Winner::Wake,
            Winner::Timeout,
            Winner::Signal,
            Winner::Cancel,
        ],
        0,
        &mut orders,
    );
    for (id, order) in orders.into_iter().enumerate() {
        let registry = Registry::new();
        let registration = registry.publish(id as u64 + 1, &[3, 20, 37], None).unwrap();
        let winners = order
            .into_iter()
            .filter(|winner| registry.claim(&registration, *winner))
            .count();
        assert_eq!(winners, 1);
        assert_eq!(registry.nodes(), 0);
    }

    let registry = Registry::new();
    let registration = registry.publish(99, &[5, 22], None).unwrap();
    assert!(registry.requeue(&registration, 5, 41));
    assert!(
        !registry.shards[Registry::shard(5)]
            .lock()
            .unwrap()
            .contains_key(&(5, 99))
    );
    assert!(registry.claim(&registration, Winner::Timeout));
    assert!(!registry.requeue(&registration, 41, 57));
    assert_eq!(registry.nodes(), 0);

    let registry = Registry::new();
    let registration = registry.publish(100, &[9, 9, 25], None).unwrap();
    assert_eq!(registry.nodes(), 2, "duplicate source has one exact node");
    assert!(registry.claim(&registration, Winner::Wake));
    assert_eq!(registry.nodes(), 0);
}

#[test]
fn eight_threads_randomized_independent_sources_leave_no_membership() {
    let registry = Arc::new(Registry::new());
    let mut workers = Vec::new();
    for cpu in 0..8u64 {
        let registry = registry.clone();
        workers.push(thread::spawn(move || {
            let mut random = 0x9e37_79b9_u64 ^ cpu;
            for sequence in 0..2_000u64 {
                random = random
                    .wrapping_mul(2_862_933_555_777_941_757)
                    .wrapping_add(3_037_000_493);
                let id = (cpu << 32) | (sequence + 1);
                let source = cpu;
                let registration = registry.publish(id, &[source], None).unwrap();
                let winner = match random & 3 {
                    0 => Winner::Wake,
                    1 => Winner::Timeout,
                    2 => Winner::Signal,
                    _ => Winner::Cancel,
                };
                assert!(registry.claim(&registration, winner));
                assert!(!registry.claim(&registration, Winner::Wake));
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(registry.nodes(), 0);
}
