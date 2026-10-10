use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;
use std::{
    sync::{Barrier, mpsc},
    thread,
};

use crate::unix_connection_backlog::{ConnectionBacklog, Full, StagedConnection};

fn prepare_with<E>(
    backlog: &Arc<Mutex<ConnectionBacklog<usize>>>,
    prepare: impl FnOnce() -> Result<usize, E>,
) -> Result<StagedConnection<usize>, Result<Full, E>> {
    let reservation = backlog.lock().reserve().map_err(Ok)?;
    let item = match prepare() {
        Ok(item) => item,
        Err(error) => {
            backlog.lock().rollback(reservation);
            return Err(Err(error));
        }
    };
    match reservation.try_stage(item) {
        Ok(staged) => Ok(staged),
        Err((_, reservation)) => {
            backlog.lock().rollback(reservation);
            panic!("host allocator unexpectedly failed to allocate backlog node")
        }
    }
}

#[test]
fn full_backlog_rejects_before_resource_factory() {
    let backlog = Arc::new(Mutex::new(ConnectionBacklog::new(0)));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker_backlog = backlog.clone();
    let worker_entered = entered.clone();
    let worker_release = release.clone();
    let worker = thread::spawn(move || {
        let pending = prepare_with(&worker_backlog, || {
            worker_entered.wait();
            worker_release.wait();
            Ok::<_, ()>(7)
        })
        .unwrap();
        worker_backlog.lock().commit(pending);
    });

    entered.wait();
    let allocations = AtomicUsize::new(0);
    let second = prepare_with(&backlog, || {
        allocations.fetch_add(1, Ordering::Relaxed);
        Ok::<_, ()>(9)
    });
    assert!(matches!(second, Err(Ok(Full))));
    assert_eq!(allocations.load(Ordering::Relaxed), 0);

    release.wait();
    worker.join().unwrap();
    assert_eq!(backlog.lock().pop().map(|node| node.into_value()), Some(7));
    assert!(backlog.lock().is_empty());
}

#[test]
fn resource_oom_and_abandoned_publication_rollback_capacity() {
    let backlog = Arc::new(Mutex::new(ConnectionBacklog::new(0)));
    let failed = prepare_with(&backlog, || Err::<usize, _>("oom"));
    assert!(matches!(failed, Err(Err("oom"))));

    let abandoned = prepare_with(&backlog, || Ok::<_, ()>(1)).unwrap();
    let reservation = abandoned.into_reservation();
    backlog.lock().rollback(reservation);

    let published = prepare_with(&backlog, || Ok::<_, ()>(2)).unwrap();
    backlog.lock().commit(published);
    assert_eq!(backlog.lock().pop().map(|node| node.into_value()), Some(2));
    assert!(backlog.lock().is_empty());
}

#[test]
fn concurrent_completion_uses_publication_fifo_and_never_overcommits() {
    let backlog = Arc::new(Mutex::new(ConnectionBacklog::new(1)));
    let (first_ready_tx, first_ready_rx) = mpsc::channel();
    let (release_first_tx, release_first_rx) = mpsc::channel();
    let first_backlog = backlog.clone();
    let first = thread::spawn(move || {
        let pending = prepare_with(&first_backlog, || {
            first_ready_tx.send(()).unwrap();
            release_first_rx.recv().unwrap();
            Ok::<_, ()>(1)
        })
        .unwrap();
        first_backlog.lock().commit(pending);
    });

    first_ready_rx.recv().unwrap();
    let second = prepare_with(&backlog, || Ok::<_, ()>(2)).unwrap();
    backlog.lock().commit(second);
    assert!(matches!(
        prepare_with(&backlog, || Ok::<_, ()>(3)),
        Err(Ok(Full))
    ));

    release_first_tx.send(()).unwrap();
    first.join().unwrap();
    assert_eq!(backlog.lock().pop().map(|node| node.into_value()), Some(2));
    assert_eq!(backlog.lock().pop().map(|node| node.into_value()), Some(1));
    assert!(backlog.lock().pop().is_none());
}

#[test]
fn relisten_changes_actual_admission_without_discarding_pending_or_reservations() {
    let mut backlog = ConnectionBacklog::new(3);
    let Ok(first) = backlog.reserve().unwrap().try_stage(11) else {
        panic!("host allocation");
    };
    let Ok(second) = backlog.reserve().unwrap().try_stage(22) else {
        panic!("host allocation");
    };
    backlog.commit(first);
    backlog.set_limit(0);
    assert!(backlog.reserve().is_err());
    backlog.commit(second); // 已领取的 capability 不被新的 limit 撤销。
    assert_eq!(backlog.pop().map(|node| node.into_value()), Some(11));
    assert!(backlog.reserve().is_err());
    assert_eq!(backlog.pop().map(|node| node.into_value()), Some(22));
    backlog.set_limit(1);
    let one = backlog.reserve().unwrap();
    let two = backlog.reserve().unwrap();
    assert!(backlog.reserve().is_err());
    backlog.rollback(one);
    backlog.rollback(two);
    backlog.set_limit(0);
    let normalized = backlog.reserve().unwrap();
    assert!(backlog.reserve().is_err());
    backlog.rollback(normalized);
}

#[test]
fn native_backlog_plus_one_capacity_and_reserved_nodes_share_one_admission_limit() {
    for limit in [0, 1, 4] {
        let mut backlog = ConnectionBacklog::<usize>::new(limit);
        let mut reserved = Vec::new();
        for _ in 0..=limit {
            assert!(backlog.capacity_available());
            reserved.push(backlog.reserve().unwrap());
        }
        assert!(!backlog.capacity_available());
        assert!(backlog.reserve().is_err());
        while let Some(token) = reserved.pop() {
            backlog.rollback(token);
        }
        assert!(backlog.capacity_available());
    }
}
