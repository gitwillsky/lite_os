use super::*;

#[test]
fn negotiated_non_power_of_two_ring_wraps_at_its_own_boundary() {
    let (mut engine, output, ring) = (
        Engine::new(Some(crate::devices::model::PcmConfiguration::SHARED)),
        FakeOutput::new(),
        FakeRing::new(),
    );
    let buffer_frames = 19 * PERIOD;
    ring.0
        .lock()
        .unwrap()
        .resize(buffer_frames as usize * PCM_FRAME_BYTES, 0);
    let a = engine
        .allocate(
            ring.clone(),
            -1,
            crate::devices::audio::engine::PcmEndpoint::Shared,
        )
        .unwrap_or_else(|_| panic!("no free substream"));
    engine
        .configure(
            a,
            buffer_frames,
            crate::devices::model::PcmConfiguration::SHARED,
        )
        .unwrap();
    assert_eq!(engine.state(a).boundary % buffer_frames, 0);
    assert!(engine.state(a).boundary >= 2 * buffer_frames);
    engine.prepare(a).unwrap();
    for slot in 0..19 {
        ring.fill(slot, 0.25, -0.25);
    }
    let initial = buffer_frames - 64;
    engine.application_frames(a, initial).unwrap();
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    for _ in 0..18 {
        engine.tick(output.complete_one(), &output);
    }
    ring.fill(18, 0.5, -0.5);
    engine.application_frames(a, 80).unwrap();
    engine.tick(output.complete_one(), &output);
    let wrapped = output.submitted.borrow()[19];
    assert_eq!(level(&wrapped), (0.5, -0.5));
    assert_eq!(
        f32::from_le_bytes(wrapped[64 * PCM_FRAME_BYTES..][..4].try_into().unwrap()),
        0.25
    );
    engine.tick(output.complete_one(), &output);
    assert_eq!(engine.state(a).hardware_frames, initial + 80);
}

#[test]
fn short_frames_wait_for_device_capacity_without_false_xrun() {
    let (mut engine, output, ring) = (
        Engine::new(Some(crate::devices::model::PcmConfiguration::SHARED)),
        FakeOutput::new(),
        FakeRing::new(),
    );
    let a = open(&mut engine, &ring);
    ring.fill(0, 0.25, -0.25);
    engine.application_frames(a, 17).unwrap();
    engine.start(a).unwrap();
    *output.fail_submit.borrow_mut() = Some(PcmOutputError::WouldBlock);
    engine.tick(NONE, &output);
    assert_eq!(engine.state(a).state, PcmState::Running);
    *output.fail_submit.borrow_mut() = None;
    engine.tick(NONE, &output);
    assert_eq!(output.submitted.borrow().len(), 1);
    engine.tick(output.complete_one(), &output);
    assert_eq!(engine.state(a).hardware_frames, 17);
}

#[test]
fn short_tail_and_wrapped_frames_are_mixed_and_credited_exactly() {
    let (mut engine, output, ring) = (
        Engine::new(Some(crate::devices::model::PcmConfiguration::SHARED)),
        FakeOutput::new(),
        FakeRing::new(),
    );
    let a = open(&mut engine, &ring);
    for slot in 0..PCM_PERIODS {
        ring.fill(slot, 0.25, -0.25);
    }
    let initial = PCM_BUFFER_FRAMES as u64 - 64;
    engine.application_frames(a, initial).unwrap();
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
    // 新数据横跨 ring 末尾：64 帧来自末页，另 16 帧来自 ring 开头。
    ring.fill(PCM_PERIODS - 1, 0.5, -0.5);
    engine.application_frames(a, 80).unwrap();
    engine.tick(output.complete_one(), &output);
    assert_eq!(engine.state(a).hardware_frames, initial);
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
    assert_eq!(engine.state(a).hardware_frames, initial + 80);
    assert_eq!(engine.state(a).application_frames, initial + 80);
}

#[test]
fn native_lease_excludes_shared_streams_and_bypasses_software_gain() {
    use crate::{devices::audio::engine::PcmEndpoint, devices::model::PcmConfiguration};
    let (mut engine, output, ring) = (
        Engine::new(Some(crate::devices::model::PcmConfiguration::SHARED)),
        FakeOutput::new(),
        FakeRing::new(),
    );
    let index = engine
        .allocate(ring.clone(), -1, PcmEndpoint::Direct)
        .unwrap_or_else(|_| panic!("native lease"));
    assert!(
        engine
            .allocate(ring.clone(), -1, PcmEndpoint::Shared)
            .is_err()
    );
    assert_eq!(engine.endpoint_available(PcmEndpoint::Shared), 0);
    assert!(
        core::str::from_utf8(PcmEndpoint::Direct.name())
            .unwrap()
            .contains("Exclusive")
    );
    engine
        .configure(index, 1024, PcmConfiguration::SHARED)
        .unwrap();
    engine.prepare(index).unwrap();
    ring.fill(0, 0.25, -0.25);
    engine.set_control(
        crate::devices::audio::control::Element::MasterVolume,
        [0, 0],
    );
    engine.application_frames(index, PERIOD).unwrap();
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
    assert_eq!(engine.endpoint_available(PcmEndpoint::Shared), SUBSTREAMS);
}
