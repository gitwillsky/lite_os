//! 混音引擎：period 记账、预送深度、欠载判定、在途归属、主音量与设备失败。

use alloc::sync::Arc;
use core::cell::RefCell;
use std::sync::Mutex;

use super::engine::{Endpoint, Engine, Events, Master, STREAMS, Stream, StreamIo, StreamState};
use crate::devices::model::{
    PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_PERIODS,
    PcmCapabilities, PcmCompletionObserver, PcmConfiguration, PcmOutput, PcmOutputError,
};

mod transfer_tests;

const PERIOD: u64 = PCM_PERIOD_FRAMES as u64;

#[derive(Default)]
struct Shared {
    ring: Vec<u8>,
    written: u64,
    played: u64,
    state: Option<StreamState>,
}

/// 一个流的数据面：环形缓冲与已写入帧数由测试扮演的使用者写，位置与状态由引擎发布。
#[derive(Clone)]
struct FakeStream(Arc<Mutex<Shared>>);

impl FakeStream {
    fn with_frames(frames: u64) -> Self {
        Self(Arc::new(Mutex::new(Shared {
            ring: vec![0; frames as usize * PCM_FRAME_BYTES],
            ..Shared::default()
        })))
    }

    fn new() -> Self {
        Self::with_frames(PCM_BUFFER_FRAMES as u64)
    }

    /// 把第 `slot` 个 period 位置填成恒定电平。
    fn fill(&self, slot: usize, left: f32, right: f32) {
        let mut shared = self.0.lock().unwrap();
        for frame in 0..PCM_PERIOD_FRAMES {
            let at = (slot * PCM_PERIOD_FRAMES + frame) * PCM_FRAME_BYTES;
            shared.ring[at..at + 4].copy_from_slice(&left.to_le_bytes());
            shared.ring[at + 4..at + 8].copy_from_slice(&right.to_le_bytes());
        }
    }

    /// 使用者发布又写入了 `frames` 帧（数据已在环形缓冲里）。
    fn commit(&self, frames: u64) {
        self.0.lock().unwrap().written += frames;
    }

    fn written(&self) -> u64 {
        self.0.lock().unwrap().written
    }

    fn played(&self) -> u64 {
        self.0.lock().unwrap().played
    }

    fn state(&self) -> Option<StreamState> {
        self.0.lock().unwrap().state
    }
}

impl StreamIo for FakeStream {
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<(), ()> {
        let shared = self.0.lock().unwrap();
        let offset = offset as usize;
        let slice = shared.ring.get(offset..offset + output.len()).ok_or(())?;
        output.copy_from_slice(slice);
        Ok(())
    }

    fn written(&self) -> u64 {
        self.0.lock().unwrap().written
    }

    fn reset_written(&self) {
        self.0.lock().unwrap().written = 0;
    }

    fn publish(&self, played: u64, state: StreamState) {
        let mut shared = self.0.lock().unwrap();
        shared.played = played;
        shared.state = Some(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Unconfigured,
    Configured,
    Prepared,
    Running,
    Stopped,
}

/// 假物理流：严格按 VirtIO Sound 的生命周期校验调用顺序，记录提交的 period。
struct FakeOutput {
    phase: RefCell<Phase>,
    queued: RefCell<usize>,
    submitted: RefCell<Vec<[u8; PCM_PERIOD_BYTES]>>,
    history: RefCell<Vec<&'static str>>,
    fail_submit: RefCell<Option<PcmOutputError>>,
}

impl FakeOutput {
    fn new() -> Self {
        Self {
            phase: RefCell::new(Phase::Prepared),
            queued: RefCell::new(0),
            submitted: RefCell::new(Vec::new()),
            history: RefCell::new(Vec::new()),
            fail_submit: RefCell::new(None),
        }
    }

    /// 设备消费掉最早的一个在途 period，返回应交给引擎的完成事件。
    fn complete_one(&self) -> Events {
        let mut queued = self.queued.borrow_mut();
        assert!(
            *queued > 0,
            "device completed a period that was never submitted"
        );
        *queued -= 1;
        Events {
            completed: 1,
            disconnected: false,
        }
    }
}

// SAFETY: 引擎只会在单线程里被调用；RefCell 记录状态，Send+Sync 只为满足 trait 约束。
unsafe impl Send for FakeOutput {}
// SAFETY: 同上。
unsafe impl Sync for FakeOutput {}

impl PcmOutput for FakeOutput {
    fn name(&self) -> &str {
        "Fixture PCM"
    }

    fn set_observer(&self, _: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError> {
        Ok(())
    }

    fn capabilities(&self) -> PcmCapabilities {
        PcmCapabilities::SHARED
    }

    fn release(&self) -> Result<(), PcmOutputError> {
        assert!(matches!(
            *self.phase.borrow(),
            Phase::Prepared | Phase::Stopped | Phase::Configured
        ));
        *self.queued.borrow_mut() = 0;
        *self.phase.borrow_mut() = Phase::Unconfigured;
        self.history.borrow_mut().push("release");
        Ok(())
    }

    fn configure(&self, _configuration: PcmConfiguration) -> Result<(), PcmOutputError> {
        assert_eq!(*self.phase.borrow(), Phase::Unconfigured);
        *self.phase.borrow_mut() = Phase::Configured;
        self.history.borrow_mut().push("configure");
        Ok(())
    }

    fn prepare(&self) -> Result<(), PcmOutputError> {
        assert!(matches!(
            *self.phase.borrow(),
            Phase::Stopped | Phase::Configured
        ));
        self.history.borrow_mut().push("prepare");
        *self.phase.borrow_mut() = Phase::Prepared;
        Ok(())
    }

    fn start(&self) -> Result<(), PcmOutputError> {
        assert_eq!(
            *self.phase.borrow(),
            Phase::Prepared,
            "start only when prepared"
        );
        self.history.borrow_mut().push("start");
        *self.phase.borrow_mut() = Phase::Running;
        Ok(())
    }

    fn stop(&self) -> Result<(), PcmOutputError> {
        assert_eq!(
            *self.phase.borrow(),
            Phase::Running,
            "stop only while running"
        );
        self.history.borrow_mut().push("stop");
        *self.phase.borrow_mut() = Phase::Stopped;
        Ok(())
    }

    fn submit_period(&self, bytes: &[u8]) -> Result<(), PcmOutputError> {
        if let Some(error) = *self.fail_submit.borrow() {
            return Err(error);
        }
        assert!(
            matches!(*self.phase.borrow(), Phase::Prepared | Phase::Running),
            "submitting while stopped"
        );
        let mut queued = self.queued.borrow_mut();
        assert!(*queued < PCM_PERIODS, "engine overran the device slots");
        *queued += 1;
        self.submitted.borrow_mut().push(bytes.try_into().unwrap());
        Ok(())
    }
}

fn level(period: &[u8; PCM_PERIOD_BYTES]) -> (f32, f32) {
    let read = |at: usize| f32::from_le_bytes(period[at..at + 4].try_into().unwrap());
    (read(0), read(4))
}

fn shared_engine() -> Engine<FakeStream> {
    Engine::new(Some(PcmConfiguration::SHARED))
}

/// 在共享端点上建立一个流。
fn open(engine: &mut Engine<FakeStream>, stream: &FakeStream) -> usize {
    let frames = (stream.0.lock().unwrap().ring.len() / PCM_FRAME_BYTES) as u64;
    engine
        .allocate(Endpoint::Shared, Stream::new(stream.clone(), frames))
        .unwrap_or_else(|_| panic!("no free stream"))
}

/// 使用者写入 `periods` 个 period：先填数据再发布帧数。
fn write(stream: &FakeStream, left: f32, right: f32, periods: usize) {
    for _ in 0..periods {
        let slot = (stream.written() / PERIOD) as usize % PCM_PERIODS;
        stream.fill(slot, left, right);
        stream.commit(PERIOD);
    }
}

fn master(volume: [u32; 2], enabled: [bool; 2]) -> Master {
    Master { volume, enabled }
}

const NONE: Events = Events {
    completed: 0,
    disconnected: false,
};

#[test]
fn each_stream_gain_changes_only_its_contribution() {
    let (mut engine, output, first, second) = (
        shared_engine(),
        FakeOutput::new(),
        FakeStream::new(),
        FakeStream::new(),
    );
    let a = open(&mut engine, &first);
    let b = open(&mut engine, &second);
    engine.set_gain(a, [50, 100]).unwrap();
    write(&first, 0.25, 0.25, 4);
    write(&second, 0.25, 0.25, 4);
    engine.start(a).unwrap();
    engine.start(b).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(level(&output.submitted.borrow()[0]), (0.3125, 0.5));
}

#[test]
fn one_stream_is_mixed_started_and_credited_on_completion() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.25, -0.25, 4);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    // 预送时保留一段使用者数据，尚未收到完成事件。
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS - 1);
    assert_eq!(*output.phase.borrow(), Phase::Running);
    assert_eq!(level(&output.submitted.borrow()[0]), (0.25, -0.25));
    assert_eq!(stream.played(), 0, "nothing played yet");
    // 一个 period 完成：位置前进；使用者用腾出的空间写下一段，再补回物理队列。
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), PERIOD);
    write(&stream, 0.5, -0.5, 1);
    engine.tick(output.complete_one(), &output);
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS + 1);
    assert_eq!(level(&output.submitted.borrow()[PCM_PERIODS]), (0.5, -0.5));
    assert_eq!(stream.state(), Some(StreamState::Playing));
}

#[test]
fn prefetch_preserves_one_ready_period_across_batched_completions() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.25, -0.25, PCM_PERIODS);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS - 1);
    // 整批 TX 一次归还后，保留的数据让设备继续播放；不能提前判空或停启物理流。
    let mut events = NONE;
    for _ in 0..PCM_PERIODS - 1 {
        events.completed += output.complete_one().completed;
    }
    engine.tick(events, &output);
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS);
    assert_eq!(*output.queued.borrow(), 1);
    assert_eq!(stream.state(), Some(StreamState::Playing));
    assert_eq!(*output.history.borrow(), ["start"]);
}

#[test]
fn concurrent_streams_are_summed_into_one_hardware_period() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let (first, second) = (FakeStream::new(), FakeStream::new());
    let (a, b) = (open(&mut engine, &first), open(&mut engine, &second));
    write(&first, 0.25, 0.0, 2);
    write(&second, 0.125, -0.5, 2);
    engine.start(a).unwrap();
    engine.start(b).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(level(&output.submitted.borrow()[0]), (0.375, -0.5));
    engine.tick(output.complete_one(), &output);
    assert_eq!(first.played(), PERIOD);
    assert_eq!(second.played(), PERIOD);
}

#[test]
fn joining_stream_waits_for_prefilled_audio_without_premature_underrun() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let (first, second) = (FakeStream::new(), FakeStream::new());
    let (a, b) = (open(&mut engine, &first), open(&mut engine, &second));
    write(&first, 0.25, 0.25, PCM_PERIODS - 2);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    write(&second, 0.5, 0.5, PCM_PERIODS - 1);
    engine.start(b).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(first.state(), Some(StreamState::Playing));
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS - 2);
    // 第一路在设备中的数据仍有效，使用者可补充下一段；随后新流加入同一个物理 period。
    write(&first, 0.25, 0.25, 1);
    engine.tick(NONE, &output);
    assert_eq!(
        level(&output.submitted.borrow()[PCM_PERIODS - 2]),
        (0.75, 0.75)
    );
    assert_eq!(first.state(), Some(StreamState::Playing));
}

#[test]
fn completion_credits_only_the_streams_that_contributed() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let (first, second) = (FakeStream::new(), FakeStream::new());
    let (a, b) = (open(&mut engine, &first), open(&mut engine, &second));
    write(&first, 0.25, 0.25, 4);
    write(&second, 0.5, 0.5, 1);
    engine.start(a).unwrap();
    engine.start(b).unwrap();
    // 第 1 个 period 含 a+b；b 尚有在途数据时，不能因下一段未交付就提前报欠载。
    engine.tick(NONE, &output);
    assert_eq!(level(&output.submitted.borrow()[0]), (0.75, 0.75));
    assert_eq!(second.state(), Some(StreamState::Playing));
    engine.tick(output.complete_one(), &output);
    assert_eq!(level(&output.submitted.borrow()[1]), (0.25, 0.25));
    assert_eq!(
        second.state(),
        Some(StreamState::Underrun),
        "b's timeline has a gap"
    );
    assert_eq!(first.state(), Some(StreamState::Playing), "a is unaffected");
    assert_eq!(first.played(), PERIOD);
    assert_eq!(
        second.played(),
        PERIOD,
        "an underrun stream's position stays frozen"
    );
    engine.tick(output.complete_one(), &output);
    assert_eq!(first.played(), 2 * PERIOD);
    assert_eq!(
        second.played(),
        PERIOD,
        "b never advances until it is restarted"
    );
}

#[test]
fn stopping_a_stream_stops_crediting_it_for_periods_already_in_flight() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.25, 0.25, 4);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(output.submitted.borrow().len(), PCM_PERIODS - 1);
    // 1. 旧 period 还在设备里时停止，并用同一个流开始新一轮播放。
    engine.stop(a).unwrap();
    assert_eq!(stream.written(), 0, "stop clears the written count");
    write(&stream, 0.5, 0.5, 2);
    engine.start(a).unwrap();
    // 2. 逐个归还整批旧 period；空出的槽位可以补入新数据，但旧完成不能推进新一轮位置。
    for _ in 0..PCM_PERIODS - 1 {
        engine.tick(output.complete_one(), &output);
        assert_eq!(stream.played(), 0, "stale completion");
    }
    // 3. 新一轮自己的 period 完成后才记账。
    engine.tick(output.complete_one(), &output);
    assert_eq!(stream.played(), PERIOD);
    assert_eq!(stream.written(), 2 * PERIOD);
}

#[test]
fn a_dry_stream_underruns_and_the_physical_stream_stops_then_restarts_on_new_data() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.25, 0.25, 2);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    engine.tick(output.complete_one(), &output);
    assert_eq!(
        stream.state(),
        Some(StreamState::Playing),
        "one period in flight"
    );
    engine.tick(output.complete_one(), &output);
    // 数据耗尽且没有在途数据：流欠载，物理流停下并重新 prepare。
    assert_eq!(stream.state(), Some(StreamState::Underrun));
    assert_eq!(*output.phase.borrow(), Phase::Prepared);
    assert_eq!(*output.history.borrow(), ["start", "stop", "prepare"]);
    // 使用者 recover：停止后重新写入并开始，物理流再次启动。
    engine.stop(a).unwrap();
    write(&stream, 0.5, 0.5, 2);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(*output.phase.borrow(), Phase::Running);
    assert_eq!(
        *output.history.borrow(),
        ["start", "stop", "prepare", "start"]
    );
}

#[test]
fn starting_with_no_data_is_an_immediate_underrun() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    assert_eq!(stream.state(), Some(StreamState::Underrun));
    assert!(output.submitted.borrow().is_empty());
    assert_eq!(
        *output.phase.borrow(),
        Phase::Prepared,
        "hardware never started"
    );
}

#[test]
fn master_volume_and_mute_shape_the_mixed_output() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.5, 0.5, 2);
    assert!(engine.set_controls(master([50, 100], [true, false])));
    assert!(!engine.set_controls(master([50, 100], [true, false])));
    engine.start(a).unwrap();
    engine.tick(NONE, &output);
    // 左声道 50% → 增益 0.25；右声道静音。
    assert_eq!(level(&output.submitted.borrow()[0]), (0.125, 0.0));
    engine.set_controls(master([100, 100], [true, true]));
    write(&stream, 0.5, 0.5, 2);
    engine.tick(output.complete_one(), &output);
    assert_eq!(
        level(&output.submitted.borrow()[2]),
        (0.5, 0.5),
        "next period uses the new gain"
    );
}

#[test]
fn streams_are_bounded_released_and_reusable() {
    let mut engine = shared_engine();
    let indices: Vec<_> = (0..STREAMS)
        .map(|_| open(&mut engine, &FakeStream::new()))
        .collect();
    assert!(!engine.admits(Endpoint::Shared), "all streams are busy");
    assert!(engine.release(indices[3]).is_some());
    assert!(engine.admits(Endpoint::Shared));
    let reused = FakeStream::new();
    assert_eq!(
        open(&mut engine, &reused),
        indices[3],
        "freed slot is reused"
    );
    assert_eq!(reused.state(), None, "reused stream starts clean");
}

#[test]
fn a_failed_device_disconnects_every_stream_and_is_permanent() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let (first, second) = (FakeStream::new(), FakeStream::new());
    let a = open(&mut engine, &first);
    open(&mut engine, &second);
    write(&first, 0.25, 0.25, 2);
    engine.start(a).unwrap();
    engine.tick(
        Events {
            completed: 0,
            disconnected: true,
        },
        &output,
    );
    assert_eq!(first.state(), Some(StreamState::Disconnected));
    assert_eq!(second.state(), Some(StreamState::Disconnected));
    assert!(output.submitted.borrow().is_empty());
    assert!(engine.failed());
    // 之后新建的流也直接断开，引擎不再触碰设备。
    let late = FakeStream::new();
    let index = open(&mut engine, &late);
    assert_eq!(late.state(), Some(StreamState::Disconnected));
    assert!(engine.start(index).is_err());
    engine.tick(NONE, &output);
    assert!(output.history.borrow().is_empty());
}

#[test]
fn a_submit_error_fails_the_whole_output_but_would_block_just_waits() {
    let (mut engine, output, stream) = (shared_engine(), FakeOutput::new(), FakeStream::new());
    let a = open(&mut engine, &stream);
    write(&stream, 0.25, 0.25, 4);
    engine.start(a).unwrap();
    *output.fail_submit.borrow_mut() = Some(PcmOutputError::WouldBlock);
    engine.tick(NONE, &output);
    assert_eq!(
        stream.state(),
        Some(StreamState::Playing),
        "a full device queue is not a failure"
    );
    assert!(output.submitted.borrow().is_empty());
    *output.fail_submit.borrow_mut() = Some(PcmOutputError::Device);
    engine.tick(NONE, &output);
    assert_eq!(stream.state(), Some(StreamState::Disconnected));
}

#[test]
fn the_queue_never_exceeds_prefetch_depth_even_with_many_ready_streams() {
    let (mut engine, output) = (shared_engine(), FakeOutput::new());
    let streams: Vec<_> = (0..STREAMS).map(|_| FakeStream::new()).collect();
    for stream in &streams {
        let index = open(&mut engine, stream);
        write(stream, 0.0625, 0.0625, 4);
        engine.start(index).unwrap();
    }
    engine.tick(NONE, &output);
    assert_eq!(
        output.submitted.borrow().len(),
        PCM_PERIODS - 1,
        "prefetch depth is independent of stream count"
    );
    // 8 路 0.0625 相加恰为 0.5。
    assert_eq!(level(&output.submitted.borrow()[0]), (0.5, 0.5));
}
