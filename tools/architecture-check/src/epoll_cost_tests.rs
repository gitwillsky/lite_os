use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
struct Interest {
    ofd: u8,
    edge: bool,
    oneshot: bool,
    generation: u64,
    delivered_generation: Option<u64>,
    level_ready: bool,
    disabled: bool,
}

#[derive(Default)]
struct Model {
    interests: BTreeMap<u8, Interest>,
    ready: BTreeSet<u8>,
    reverse: BTreeMap<u8, BTreeSet<u8>>,
}

impl Model {
    fn add(&mut self, key: u8, ofd: u8, edge: bool, oneshot: bool, level_ready: bool) {
        if self.interests.contains_key(&key) {
            return;
        }
        self.interests.insert(
            key,
            Interest {
                ofd,
                edge,
                oneshot,
                generation: 0,
                delivered_generation: None,
                level_ready,
                disabled: false,
            },
        );
        self.reverse.entry(ofd).or_default().insert(key);
        self.refresh(key);
    }

    fn modify(&mut self, key: u8, edge: bool, oneshot: bool) {
        let Some(interest) = self.interests.get_mut(&key) else {
            return;
        };
        interest.edge = edge;
        interest.oneshot = oneshot;
        interest.disabled = false;
        interest.delivered_generation = None;
        self.refresh(key);
    }

    fn source_change(&mut self, ofd: u8, level_ready: bool) {
        let keys = self.reverse.get(&ofd).cloned().unwrap_or_default();
        for key in keys {
            let interest = self.interests.get_mut(&key).unwrap();
            interest.generation += 1;
            interest.level_ready = level_ready;
            self.refresh(key);
        }
    }

    fn deliver(&mut self, key: u8, copyout_succeeds: bool) {
        if !copyout_succeeds || !self.ready.remove(&key) {
            return;
        }
        let interest = self.interests.get_mut(&key).unwrap();
        if interest.edge {
            interest.delivered_generation = Some(interest.generation);
        }
        if interest.oneshot {
            interest.disabled = true;
        }
        if !interest.edge && !interest.oneshot {
            self.refresh(key);
        }
    }

    fn close_ofd(&mut self, ofd: u8) -> usize {
        let keys = self.reverse.remove(&ofd).unwrap_or_default();
        let visits = keys.len();
        for key in keys {
            self.interests.remove(&key);
            self.ready.remove(&key);
        }
        visits
    }

    fn refresh(&mut self, key: u8) {
        let Some(interest) = self.interests.get(&key) else {
            return;
        };
        let should_be_ready = !interest.disabled
            && interest.level_ready
            && (!interest.edge
                || interest
                    .delivered_generation
                    .is_none_or(|delivered| interest.generation != delivered));
        if should_be_ready {
            self.ready.insert(key);
        } else {
            self.ready.remove(&key);
        }
    }

    fn assert_matches_full_scan(&self) {
        let expected = self
            .interests
            .iter()
            .filter_map(|(&key, interest)| {
                (!interest.disabled
                    && interest.level_ready
                    && (!interest.edge
                        || interest
                            .delivered_generation
                            .is_none_or(|delivered| interest.generation != delivered)))
                .then_some(key)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(self.ready, expected);
        for (&ofd, keys) in &self.reverse {
            assert!(keys.iter().all(|key| self.interests[key].ofd == ofd));
        }
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

#[test]
fn randomized_incremental_ready_matches_full_scan() {
    let mut model = Model::default();
    let mut seed = 0x5eed_cafe_f00d_u64;
    for _ in 0..20_000 {
        let value = random(&mut seed);
        let key = ((value >> 8) & 31) as u8;
        let ofd = ((value >> 16) & 7) as u8;
        match value % 6 {
            0 => model.add(key, ofd, value & 1 != 0, value & 2 != 0, value & 4 != 0),
            1 => model.modify(key, value & 1 != 0, value & 2 != 0),
            2 => model.source_change(ofd, value & 4 != 0),
            3 => model.deliver(key, true),
            4 => model.deliver(key, false),
            _ => {
                model.close_ofd(ofd);
            }
        }
        model.assert_matches_full_scan();
    }
}

#[test]
fn final_close_visits_only_exact_reverse_memberships() {
    let mut model = Model::default();
    for key in 0..64 {
        model.add(key, key / 2, false, false, key & 1 != 0);
    }
    assert_eq!(model.close_ofd(17), 2);
    assert_eq!(model.interests.len(), 62);
    assert!(model.interests.values().all(|interest| interest.ofd != 17));
    model.assert_matches_full_scan();
}

#[derive(Clone, Copy)]
enum RacingOperation {
    SourceReady,
    ModifyEdge,
    Deliver,
    FinalClose,
}

#[test]
fn ctl_source_delivery_and_close_linearizations_preserve_owners() {
    fn explore(model: Model, remaining: &mut Vec<RacingOperation>) {
        if remaining.is_empty() {
            model.assert_matches_full_scan();
            return;
        }
        for index in 0..remaining.len() {
            let operation = remaining.remove(index);
            let mut next = Model {
                interests: model.interests.clone(),
                ready: model.ready.clone(),
                reverse: model.reverse.clone(),
            };
            match operation {
                RacingOperation::SourceReady => next.source_change(3, true),
                RacingOperation::ModifyEdge => next.modify(9, true, false),
                RacingOperation::Deliver => next.deliver(9, true),
                RacingOperation::FinalClose => {
                    next.close_ofd(3);
                }
            }
            next.assert_matches_full_scan();
            explore(next, remaining);
            remaining.insert(index, operation);
        }
    }

    let mut model = Model::default();
    model.add(9, 3, false, false, false);
    explore(
        model,
        &mut vec![
            RacingOperation::SourceReady,
            RacingOperation::ModifyEdge,
            RacingOperation::Deliver,
            RacingOperation::FinalClose,
        ],
    );
}
