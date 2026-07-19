use super::*;

impl PublishedWait {
    fn registration(&self) -> &Arc<WaitRegistration> {
        self.registration
            .as_ref()
            .expect("published wait already consumed")
    }

    pub(in crate::task::task_manager) fn cancel(mut self) -> CancelOutcome {
        loop {
            match self.registration().state() {
                ARMING => {
                    if self
                        .registration()
                        .state
                        .compare_exchange(ARMING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        drop(WAIT_REGISTRY.detach(self.registration()));
                        self.registration.take();
                        return CancelOutcome::Cancelled;
                    }
                }
                state if WaitRegistration::notified_result(state).is_some() => {
                    let result = WaitRegistration::notified_result(state).unwrap();
                    self.registration.take();
                    return CancelOutcome::Notified(result);
                }
                ARMING_LOCKED | CLAIMING | REQUEUEING => spin_loop(),
                state => panic!("cannot cancel wait registration in state {state}"),
            }
        }
    }

    pub(in crate::task::task_manager) fn prepare_arm(
        mut self,
    ) -> Result<WaitArmGuard<'static>, WaitResult> {
        loop {
            match self.registration().state() {
                ARMING => {
                    if self
                        .registration()
                        .state
                        .compare_exchange(
                            ARMING,
                            ARMING_LOCKED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        let keys = self.registration().keys.lock();
                        let shards = LockedWaitShards::lock(&WAIT_REGISTRY.shards, &keys);
                        drop(keys);
                        let registration = self
                            .registration
                            .take()
                            .expect("published wait already consumed");
                        return Ok(WaitArmGuard {
                            registration,
                            shards,
                            armed: false,
                        });
                    }
                }
                state if WaitRegistration::notified_result(state).is_some() => {
                    let result = WaitRegistration::notified_result(state).unwrap();
                    self.registration.take();
                    return Err(result);
                }
                state => panic!("cannot arm wait registration in state {state}"),
            }
        }
    }
}

impl Drop for PublishedWait {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        loop {
            match registration.state() {
                ARMING => {
                    if registration
                        .state
                        .compare_exchange(ARMING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        drop(WAIT_REGISTRY.detach(&registration));
                        return;
                    }
                }
                state if WaitRegistration::notified_result(state).is_some() => return,
                ARMING_LOCKED | CLAIMING | REQUEUEING => spin_loop(),
                state => panic!("abandoned published wait in state {state}"),
            }
        }
    }
}
