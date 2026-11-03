//! 非整周期与跨环形缓冲末尾的传输、独占端点。

use super::*;

#[test]
fn negotiated_non_power_of_two_ring_wraps_at_its_own_end() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let buffer_frames = 19 * PERIOD;
    let stream = FakeStream::with_frames(buffer_frames);
    let a = open(&mut engine, &stream);
    for slot in 0..19 {
        stream.fill(slot, 0.25, -0.25);
    }
    let initial = buffer_frames - 64;
    stream.commit(initial);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    for _ in 0..18 {
        engine.tick(output.complete_one(), &output);
    }
    stream.fill(18, 0.5, -0.5);
    stream.commit(80);
    engine.tick(output.complete_one(), &output);
    let wrapped = output.submitted.borrow()[19];
    assert_eq!(level(&wrapped), (0.5, -0.5));
    assert_eq!(
        f32::from_le_bytes(wrapped[64 * PCM_FRAME_BYTES..][..4].try_into().unwrap()),
        0.25
    );
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), initial + 80);
}

#[test]
fn short_frames_wait_for_device_capacity_without_false_underrun() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    stream.fill(0, 0.25, -0.25);
    stream.commit(17);
    engine.start(a).unwrap();
    *output.fail_submit.borrow_mut() = Some(PcmOutputError::WouldBlock);
    engine.tick(NONE, &output);
    assert_eq!(stream.state(), Some(StreamState::Playing));
    *output.fail_submit.borrow_mut() = None;
    engine.tick(NONE, &output);
    assert_eq!(output.submitted.borrow().len(), 1);
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), 17);
}

#[test]
fn short_tail_and_wrapped_frames_are_mixed_and_credited_exactly() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    for slot in 0..PCM_PERIODS {
        stream.fill(slot, 0.25, -0.25);
    }
    let initial = PCM_BUFFER_FRAMES as u64 - 64;
    stream.commit(initial);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    // 短尾仍有在途周期时不提前补零；三个完整周期归还后才交付剩余 192 帧。
    assert_eq!(output.submitted.borrow().len(), 3);
    for _ in 0..3 {
        engine.tick(output.complete_one(), &output);
    }
    let tail = output.submitted.borrow()[3];
    assert_eq!(level(&tail), (0.25, -0.25));
    assert!(tail[192 * PCM_FRAME_BYTES..].iter().all(|byte| *byte == 0));
    // 新数据横跨环形缓冲末尾：64 帧来自末段，另 16 帧来自开头。
    stream.fill(PCM_PERIODS - 1, 0.5, -0.5);
    stream.commit(80);
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), initial);
    let wrapped = output.submitted.borrow()[4];
    assert_eq!(level(&wrapped), (0.5, -0.5));
    assert_eq!(
        f32::from_le_bytes(wrapped[64 * PCM_FRAME_BYTES..][..4].try_into().unwrap()),
        0.25
    );
    assert!(
        wrapped[80 * PCM_FRAME_BYTES..]
            .iter()
            .all(|byte| *byte == 0)
    );
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), initial + 80);
    assert_eq!(stream.written(), initial + 80);
}

#[test]
fn exclusive_stream_excludes_shared_streams_and_bypasses_software_gain() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let index = engine
        .allocate(Endpoint::Exclusive, Stream::new(stream.clone(), 1024))
        .unwrap_or_else(|_| panic!("exclusive stream"));
    assert!(!engine.admits(Endpoint::Shared));
    assert!(!engine.admits(Endpoint::Exclusive));
    stream.fill(0, 0.25, -0.25);
    engine.set_controls(master([0, 0], [true, true]));
    stream.commit(PERIOD);
    engine.start(index).unwrap();
    engine.tick(NONE, &output);
    assert!(engine.running());
    assert_eq!(level(&output.submitted.borrow()[0]), (0.25, -0.25));
    assert!(engine.begin_transition().unwrap());
    output.stop().unwrap();
    output.release().unwrap();
    engine.finish_transition(None).unwrap();
    assert_eq!(engine.configuration(), None);
    assert_eq!(*output.queued.borrow(), 0);
    assert!(!engine.begin_transition().unwrap());
    output.configure(PcmConfiguration::SHARED).unwrap();
    output.prepare().unwrap();
    engine
        .finish_transition(Some(PcmConfiguration::SHARED))
        .unwrap();
    assert_eq!(engine.configuration(), Some(PcmConfiguration::SHARED));
    drop(engine.release(index));
    assert!(engine.admits(Endpoint::Shared));
}
