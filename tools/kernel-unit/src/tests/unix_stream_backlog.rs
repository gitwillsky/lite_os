use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;
use std::{
    sync::{Barrier, mpsc},
    thread,
};

use crate::unix_stream_backlog::{Full, StagedConnection, StreamBacklog};

fn prepare_with<E>(
    backlog: &Arc<Mutex<StreamBacklog<usize>>>,
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
    let backlog = Arc::new(Mutex::new(StreamBacklog::new(1)));
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
    assert_eq!(backlog.lock().pop(), Some(7));
    assert!(backlog.lock().is_empty());
}

#[test]
fn resource_oom_and_abandoned_publication_rollback_capacity() {
    let backlog = Arc::new(Mutex::new(StreamBacklog::new(1)));
    let failed = prepare_with(&backlog, || Err::<usize, _>("oom"));
    assert!(matches!(failed, Err(Err("oom"))));

    let abandoned = prepare_with(&backlog, || Ok::<_, ()>(1)).unwrap();
    let reservation = abandoned.into_reservation();
    backlog.lock().rollback(reservation);

    let published = prepare_with(&backlog, || Ok::<_, ()>(2)).unwrap();
    backlog.lock().commit(published);
    assert_eq!(backlog.lock().pop(), Some(2));
    assert!(backlog.lock().is_empty());
}

#[test]
fn concurrent_completion_uses_publication_fifo_and_never_overcommits() {
    let backlog = Arc::new(Mutex::new(StreamBacklog::new(2)));
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
    assert_eq!(backlog.lock().pop(), Some(2));
    assert_eq!(backlog.lock().pop(), Some(1));
    assert_eq!(backlog.lock().pop(), None);
}
