//! Actual IPC endpoint allocation/admission/retirement with a thread-local allocator fault.
use crate::{
    allocation_failure,
    ipc::{Pipe, PipeDirection, PipeOpenAccess, PipeOpenError, PipeWrite},
};
#[test]
fn rw_endpoint_oom_does_not_publish_half_an_open_or_retire_the_live_session() {
    let (reader, writer) = Pipe::pair().unwrap();
    let pipe = reader.pipe();
    assert_eq!(writer.write(b"kept"), PipeWrite::Bytes(4));
    let generations = (
        pipe.readiness_generation(PipeDirection::Read),
        pipe.readiness_generation(PipeDirection::Write),
    );
    for stage in 0..2 {
        allocation_failure::fail_after(stage);
        let result = pipe.open(PipeOpenAccess::ReadWrite);
        assert!(allocation_failure::was_consumed());
        assert!(matches!(result, Err(PipeOpenError::OutOfMemory)));
        assert_eq!(pipe.peer_snapshot(PipeDirection::Read), (1, 1));
        assert_eq!(pipe.peer_snapshot(PipeDirection::Write), (1, 1));
        assert_eq!(
            (
                pipe.readiness_generation(PipeDirection::Read),
                pipe.readiness_generation(PipeDirection::Write)
            ),
            generations
        );
        assert!(!pipe.retired());
        assert_eq!(pipe.buffered_bytes(), 4);
    }
    drop(reader);
    drop(writer);
    assert!(pipe.retired());
    assert_eq!(pipe.capacity(), 0);
}
#[test]
fn rw_open_commits_both_endpoints_and_their_last_close_retires_the_session() {
    let (reader, writer) = Pipe::pair().unwrap();
    let pipe = reader.pipe();
    let opened = pipe.open(PipeOpenAccess::ReadWrite).unwrap();
    let (next_reader, next_writer) = (opened.reader.unwrap(), opened.writer.unwrap());
    assert_eq!(pipe.peer_snapshot(PipeDirection::Read), (2, 2));
    assert_eq!(pipe.peer_snapshot(PipeDirection::Write), (2, 2));
    drop(reader);
    drop(writer);
    assert!(!pipe.retired());
    drop(next_reader);
    assert!(!pipe.retired());
    drop(next_writer);
    assert!(pipe.retired());
}

#[test]
fn named_no_reader_and_prior_peer_counters_share_the_admission_transaction() {
    let pipe = Pipe::new_named().unwrap();
    let before = pipe.peer_snapshot(PipeDirection::Write);
    assert!(matches!(
        pipe.open(PipeOpenAccess::Write {
            require_reader: true
        }),
        Err(PipeOpenError::NoReader)
    ));
    assert_eq!(pipe.peer_snapshot(PipeDirection::Write), before);
    assert!(!pipe.retired());
    let writer = pipe
        .open(PipeOpenAccess::Write {
            require_reader: false,
        })
        .unwrap();
    let reader = pipe.open(PipeOpenAccess::Read).unwrap();
    assert_eq!(
        (
            reader.prior.readers,
            reader.prior.writers,
            reader.prior.writer_opens
        ),
        (0, 1, 1)
    );
    drop(writer);
    assert_eq!(pipe.peer_snapshot(PipeDirection::Read), (0, 1));
    drop(reader);
    assert!(pipe.retired());
}
