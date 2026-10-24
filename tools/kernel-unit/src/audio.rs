//! 内核 `audio` 领域里纯逻辑部分的宿主测试入口；子模块层级与 `kernel/src/audio/` 一致。

#[path = "../../../kernel/src/audio/codec.rs"]
pub(crate) mod codec;
#[path = "../../../kernel/src/audio/control.rs"]
pub(crate) mod control;
#[path = "../../../kernel/src/audio/engine.rs"]
pub(crate) mod engine;
#[path = "../../../kernel/src/audio/mixer.rs"]
pub(crate) mod mixer;
#[path = "../../../kernel/src/audio/state.rs"]
pub(crate) mod state;

#[cfg(test)]
mod abi_tests;

#[cfg(test)]
mod mixing_tests {
    use super::mixer::{Accumulator, VOLUME_MAX, volume_gain};
    use crate::drivers::{PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES};

    /// 一个 period：左声道恒为 `left`，右声道恒为 `right`。
    fn period(left: f32, right: f32) -> [u8; PCM_PERIOD_BYTES] {
        let mut bytes = [0u8; PCM_PERIOD_BYTES];
        for frame in 0..PCM_PERIOD_FRAMES {
            bytes[frame * 8..frame * 8 + 4].copy_from_slice(&left.to_le_bytes());
            bytes[frame * 8 + 4..frame * 8 + 8].copy_from_slice(&right.to_le_bytes());
        }
        bytes
    }

    fn sample(bytes: &[u8; PCM_PERIOD_BYTES], index: usize) -> f32 {
        f32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
    }

    fn mix(sources: &[[u8; PCM_PERIOD_BYTES]], gain: [f32; 2]) -> [u8; PCM_PERIOD_BYTES] {
        let mut accumulator = Accumulator::new();
        for source in sources {
            accumulator.add(source, [1.0, 1.0]);
        }
        let mut output = [0u8; PCM_PERIOD_BYTES];
        accumulator.finish(gain, &mut output);
        output
    }

    #[test]
    fn a_single_source_at_unity_gain_is_bit_exact() {
        let source = period(0.25, -0.5);
        assert_eq!(mix(&[source], [1.0, 1.0]), source);
    }

    #[test]
    fn independent_sources_add_per_channel() {
        let out = mix(&[period(0.25, 0.0), period(0.125, -0.25)], [1.0, 1.0]);
        assert_eq!(sample(&out, 0), 0.375);
        assert_eq!(sample(&out, 1), -0.25);
        assert_eq!(
            sample(&out, 2 * (PCM_PERIOD_FRAMES - 1)),
            0.375,
            "last frame mixes too"
        );
    }

    #[test]
    fn gain_applies_per_channel_and_zero_silences() {
        let out = mix(&[period(0.5, 0.5)], [1.0, 0.5]);
        assert_eq!((sample(&out, 0), sample(&out, 1)), (0.5, 0.25));
        let silent = mix(&[period(0.5, 0.5)], [0.0, 0.0]);
        assert!(
            silent.iter().all(|&byte| byte == 0),
            "silence is all-zero bytes"
        );
    }

    #[test]
    fn overload_hard_clips_to_full_scale_instead_of_wrapping() {
        let out = mix(&[period(0.75, -0.75), period(0.75, -0.75)], [1.0, 1.0]);
        assert_eq!((sample(&out, 0), sample(&out, 1)), (1.0, -1.0));
        // 峰值恰为满幅时不削波：未过载的混音完全透明。
        let exact = mix(&[period(0.5, 0.5), period(0.5, 0.5)], [1.0, 1.0]);
        assert_eq!(sample(&exact, 0), 1.0);
    }

    #[test]
    fn muting_a_finite_input_sum_that_overflowed_cannot_produce_nan() {
        let sources = [period(f32::MAX, -f32::MAX); 2];
        let muted = mix(&sources, [0.0, 0.0]);
        assert!(muted.iter().all(|byte| *byte == 0));
        let audible = mix(&sources, [1.0, 1.0]);
        assert_eq!((sample(&audible, 0), sample(&audible, 1)), (1.0, -1.0));
    }

    #[test]
    fn non_finite_samples_are_silenced_and_cannot_poison_other_sources() {
        let hostile = period(f32::NAN, f32::INFINITY);
        let out = mix(&[hostile, period(0.25, 0.5)], [1.0, 1.0]);
        assert_eq!((sample(&out, 0), sample(&out, 1)), (0.25, 0.5));
        let negative = mix(&[period(f32::NEG_INFINITY, 0.0)], [1.0, 1.0]);
        assert_eq!(sample(&negative, 0), 0.0);
    }

    #[test]
    fn volume_curve_is_quadratic_monotonic_and_unity_at_full_scale() {
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(VOLUME_MAX), 1.0);
        assert_eq!(volume_gain(50), 0.25);
        assert_eq!(
            volume_gain(VOLUME_MAX + 400),
            1.0,
            "values above the maximum saturate"
        );
        let mut previous = 0.0;
        for volume in 0..=VOLUME_MAX {
            let gain = volume_gain(volume);
            assert!(
                gain >= previous,
                "volume curve must not decrease at {volume}"
            );
            previous = gain;
        }
    }
}

#[cfg(test)]
mod engine_tests {
    use alloc::sync::Arc;
    use core::cell::RefCell;
    use std::sync::Mutex;

    use super::control::Element;
    use super::engine::{Engine, Events, Master, Ring, SUBSTREAMS};
    use super::state::PcmState;
    use crate::drivers::{
        PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_PERIODS,
        PcmCompletionObserver, PcmOutput, PcmOutputError,
    };

    const PERIOD: u64 = PCM_PERIOD_FRAMES as u64;

    /// 一个子流的环形缓冲：整块内存，能按 period 填入恒定电平，读取时与引擎的偏移计算一一对应。
    #[derive(Clone)]
    struct FakeRing(Arc<Mutex<Vec<u8>>>);

    impl FakeRing {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(vec![
                0;
                PCM_BUFFER_FRAMES
                    * PCM_FRAME_BYTES
            ])))
        }

        /// 把第 `slot`（0..4）个 period 位置填成恒定电平。
        fn fill(&self, slot: usize, left: f32, right: f32) {
            let mut bytes = self.0.lock().unwrap();
            for frame in 0..PCM_PERIOD_FRAMES {
                let at = (slot * PCM_PERIOD_FRAMES + frame) * PCM_FRAME_BYTES;
                bytes[at..at + 4].copy_from_slice(&left.to_le_bytes());
                bytes[at + 4..at + 8].copy_from_slice(&right.to_le_bytes());
            }
        }
    }

    impl Ring for FakeRing {
        fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), ()> {
            let bytes = self.0.lock().unwrap();
            let slice = bytes.get(offset..offset + output.len()).ok_or(())?;
            output.copy_from_slice(slice);
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Phase {
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

        /// 设备消费掉最早的一个在途 period，返回应交给引擎的完成事件数。
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

    // 引擎只会在单线程里被调用；FakeOutput 用 RefCell 记录状态，满足 trait 的 Send+Sync 约束只为通过类型检查。
    unsafe impl Send for FakeOutput {}
    unsafe impl Sync for FakeOutput {}

    impl PcmOutput for FakeOutput {
        fn set_observer(&self, _: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError> {
            Ok(())
        }

        fn configure(&self) -> Result<(), PcmOutputError> {
            panic!("the engine must never reconfigure the physical stream");
        }

        fn prepare(&self) -> Result<(), PcmOutputError> {
            assert_eq!(
                *self.phase.borrow(),
                Phase::Stopped,
                "prepare is only legal after stop"
            );
            self.history.borrow_mut().push("prepare");
            *self.phase.borrow_mut() = Phase::Prepared;
            Ok(())
        }

        fn start(&self) -> Result<(), PcmOutputError> {
            assert_eq!(
                *self.phase.borrow(),
                Phase::Prepared,
                "start is only legal when prepared"
            );
            self.history.borrow_mut().push("start");
            *self.phase.borrow_mut() = Phase::Running;
            Ok(())
        }

        fn stop(&self) -> Result<(), PcmOutputError> {
            assert_eq!(
                *self.phase.borrow(),
                Phase::Running,
                "stop is only legal while running"
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

        fn writable(&self) -> bool {
            *self.queued.borrow() < PCM_PERIODS
        }
    }

    fn level(period: &[u8; PCM_PERIOD_BYTES]) -> (f32, f32) {
        let read = |at: usize| f32::from_le_bytes(period[at..at + 4].try_into().unwrap());
        (read(0), read(4))
    }

    /// 打开一个子流并走完 Open → Setup → Prepared 的前置步骤。
    fn open(engine: &mut Engine<FakeRing>, ring: &FakeRing) -> usize {
        let index = engine
            .allocate(ring.clone(), -1)
            .unwrap_or_else(|_| panic!("no free substream"));
        engine.configure(index).unwrap();
        engine
            .set_software(
                index,
                PERIOD,
                PCM_BUFFER_FRAMES as u64,
                PCM_BUFFER_FRAMES as u64,
                1 << 20,
            )
            .unwrap();
        engine.prepare(index).unwrap();
        index
    }

    /// 应用写入 `periods` 个 period：先填数据再推进应用指针，与 `PcmFile::write_period` 的顺序一致。
    fn write(
        engine: &mut Engine<FakeRing>,
        index: usize,
        ring: &FakeRing,
        left: f32,
        right: f32,
        periods: usize,
    ) {
        for _ in 0..periods {
            let slot = (engine.state(index).application_frames / PERIOD) as usize % 4;
            ring.fill(slot, left, right);
            engine.application_period(index).unwrap();
        }
    }

    const NONE: Events = Events {
        completed: 0,
        disconnected: false,
    };

    #[test]
    fn hardware_free_rejects_running_and_preserves_card_master() {
        let mut engine = Engine::new();
        let ring = FakeRing::new();
        let index = open(&mut engine, &ring);
        let master = Master {
            volume: [25, 50],
            enabled: [true, false],
        };
        engine.set_control(Element::MasterVolume, master.volume);
        engine.set_control(Element::MasterSwitch, master.enabled.map(u32::from));
        engine.start(index).unwrap();
        assert!(engine.free_hardware(index).is_err());
        engine.drop_stream(index).unwrap();
        engine.free_hardware(index).unwrap();
        assert_eq!(engine.state(index).state, PcmState::Open);
        assert_eq!(engine.control_values(Element::MasterVolume), master.volume);
        assert_eq!(
            engine.control_values(Element::MasterSwitch),
            master.enabled.map(u32::from)
        );
        assert_eq!(
            engine.available(),
            SUBSTREAMS - 1,
            "HW_FREE does not close the OFD"
        );
    }

    #[test]
    fn preferred_substream_never_falls_back_to_a_different_slot() {
        let mut engine = Engine::new();
        assert_eq!(engine.allocate(FakeRing::new(), 5).ok(), Some(5));
        assert_eq!(engine.allocate(FakeRing::new(), -1).ok(), Some(0));
        assert!(engine.allocate(FakeRing::new(), 5).is_err());
        assert!(engine.allocate(FakeRing::new(), SUBSTREAMS as i32).is_err());
        assert!(engine.allocate(FakeRing::new(), -2).is_err());
        engine.release(5);
        assert_eq!(engine.allocate(FakeRing::new(), 5).ok(), Some(5));
    }

    #[test]
    fn each_pcm_volume_changes_only_its_contribution_and_survives_close() {
        let (mut engine, output, first, second) = (
            Engine::new(),
            FakeOutput::new(),
            FakeRing::new(),
            FakeRing::new(),
        );
        let a = open(&mut engine, &first);
        let b = open(&mut engine, &second);
        assert!(engine.set_control(Element::PcmVolume(a), [50, 100]));
        assert!(!engine.set_control(Element::PcmVolume(a), [50, 100]));
        write(&mut engine, a, &first, 0.25, 0.25, 4);
        write(&mut engine, b, &second, 0.25, 0.25, 4);
        engine.start(a).unwrap();
        engine.start(b).unwrap();
        engine.tick(NONE, &output);
        assert_eq!(level(&output.submitted.borrow()[0]), (0.3125, 0.5));
        engine.release(a);
        assert_eq!(engine.allocate(FakeRing::new(), a as i32).ok(), Some(a));
        assert_eq!(engine.control_values(Element::PcmVolume(a)), [50, 100]);
    }

    #[test]
    fn one_substream_is_mixed_started_and_credited_on_completion() {
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        write(&mut engine, a, &ring, 0.25, -0.25, 4);
        engine.start(a).unwrap();
        assert!(engine.tick(NONE, &output));
        // 队列深度被限制在 2 个 period，物理流已经启动。
        assert_eq!(output.submitted.borrow().len(), 2);
        assert_eq!(*output.phase.borrow(), Phase::Running);
        assert_eq!(level(&output.submitted.borrow()[0]), (0.25, -0.25));
        assert_eq!(engine.state(a).hardware_frames, 0, "nothing played yet");
        // 一个 period 完成：hw_ptr 前进，并补填一个新的。
        let events = output.complete_one();
        assert!(engine.tick(events, &output));
        assert_eq!(engine.state(a).hardware_frames, PERIOD);
        assert_eq!(output.submitted.borrow().len(), 3);
        assert_eq!(engine.state(a).state, PcmState::Running);
    }

    #[test]
    fn concurrent_substreams_are_summed_into_one_hardware_period() {
        let (mut engine, output) = (Engine::new(), FakeOutput::new());
        let (ring_a, ring_b) = (FakeRing::new(), FakeRing::new());
        let (a, b) = (open(&mut engine, &ring_a), open(&mut engine, &ring_b));
        write(&mut engine, a, &ring_a, 0.25, 0.0, 2);
        write(&mut engine, b, &ring_b, 0.125, -0.5, 2);
        engine.start(a).unwrap();
        engine.start(b).unwrap();
        engine.tick(NONE, &output);
        assert_eq!(level(&output.submitted.borrow()[0]), (0.375, -0.5));
        // 一个硬件 period 同时完成两个子流的一个 period。
        let events = output.complete_one();
        engine.tick(events, &output);
        assert_eq!(engine.state(a).hardware_frames, PERIOD);
        assert_eq!(engine.state(b).hardware_frames, PERIOD);
    }

    #[test]
    fn completion_credits_only_the_substreams_that_contributed() {
        let (mut engine, output) = (Engine::new(), FakeOutput::new());
        let (ring_a, ring_b) = (FakeRing::new(), FakeRing::new());
        let (a, b) = (open(&mut engine, &ring_a), open(&mut engine, &ring_b));
        write(&mut engine, a, &ring_a, 0.25, 0.25, 4);
        write(&mut engine, b, &ring_b, 0.5, 0.5, 1);
        engine.start(a).unwrap();
        engine.start(b).unwrap();
        // 第 1 个 period 含 a+b；b 只写了一个 period，第 2 个 period 里 b 缺席，所以 b 进入 Xrun。
        engine.tick(NONE, &output);
        assert_eq!(level(&output.submitted.borrow()[0]), (0.75, 0.75));
        assert_eq!(level(&output.submitted.borrow()[1]), (0.25, 0.25));
        assert_eq!(
            engine.state(b).state,
            PcmState::Xrun,
            "b's timeline has a gap"
        );
        assert_eq!(
            engine.state(a).state,
            PcmState::Running,
            "a is unaffected by b's underrun"
        );
        engine.tick(output.complete_one(), &output);
        assert_eq!(engine.state(a).hardware_frames, PERIOD);
        // Xrun 之后 hw_ptr 冻结：客户端必须 prepare 重来（指针清零），不会依赖 xrun 之后的位置。
        assert_eq!(
            engine.state(b).hardware_frames,
            0,
            "an xrun substream's hw_ptr stays frozen"
        );
        engine.tick(output.complete_one(), &output);
        assert_eq!(engine.state(a).hardware_frames, 2 * PERIOD);
        assert_eq!(
            engine.state(b).hardware_frames,
            0,
            "b never advances again until it is re-prepared"
        );
    }

    #[test]
    fn dropping_a_substream_stops_crediting_it_for_periods_already_in_flight() {
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        write(&mut engine, a, &ring, 0.25, 0.25, 4);
        engine.start(a).unwrap();
        engine.tick(NONE, &output);
        assert_eq!(output.submitted.borrow().len(), 2);
        // 在途 period 还没完成就 drop 并重新 prepare：旧 period 的完成不能推进新一轮的 hw_ptr。
        engine.drop_stream(a).unwrap();
        engine.prepare(a).unwrap();
        write(&mut engine, a, &ring, 0.5, 0.5, 2);
        engine.tick(output.complete_one(), &output);
        assert_eq!(
            engine.state(a).hardware_frames,
            0,
            "stale completion must not be credited"
        );
        assert_eq!(engine.state(a).application_frames, 2 * PERIOD);
    }

    #[test]
    fn a_dry_substream_xruns_and_the_physical_stream_stops_then_restarts_on_new_data() {
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        write(&mut engine, a, &ring, 0.25, 0.25, 2);
        engine.start(a).unwrap();
        engine.tick(NONE, &output);
        engine.tick(output.complete_one(), &output);
        assert_eq!(
            engine.state(a).state,
            PcmState::Running,
            "one period is still in flight"
        );
        engine.tick(output.complete_one(), &output);
        // 数据耗尽且没有在途数据：子流 Xrun，物理流停下并重新 prepare。
        assert_eq!(engine.state(a).state, PcmState::Xrun);
        assert_eq!(*output.phase.borrow(), Phase::Prepared);
        assert_eq!(*output.history.borrow(), ["start", "stop", "prepare"]);
        // 应用 recover：prepare 后重新写入并 start，物理流再次启动。
        engine.prepare(a).unwrap();
        write(&mut engine, a, &ring, 0.5, 0.5, 2);
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
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        engine.start(a).unwrap();
        assert!(engine.tick(NONE, &output));
        assert_eq!(engine.state(a).state, PcmState::Xrun);
        assert!(output.submitted.borrow().is_empty());
        assert_eq!(
            *output.phase.borrow(),
            Phase::Prepared,
            "hardware never started"
        );
    }

    #[test]
    fn master_volume_and_mute_shape_the_mixed_output() {
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        write(&mut engine, a, &ring, 0.5, 0.5, 4);
        engine.set_control(Element::MasterVolume, [50, 100]);
        engine.set_control(Element::MasterSwitch, [true, false].map(u32::from));
        engine.start(a).unwrap();
        engine.tick(NONE, &output);
        // 左声道 50% → 增益 0.25；右声道静音。
        assert_eq!(level(&output.submitted.borrow()[0]), (0.125, 0.0));
        engine.set_control(Element::MasterVolume, [100, 100]);
        engine.set_control(Element::MasterSwitch, [true, true].map(u32::from));
        engine.tick(output.complete_one(), &output);
        assert_eq!(
            level(&output.submitted.borrow()[2]),
            (0.5, 0.5),
            "next period uses the new gain"
        );
    }

    #[test]
    fn substreams_are_bounded_released_and_reusable() {
        let mut engine = Engine::new();
        let mut indices = Vec::new();
        for _ in 0..SUBSTREAMS {
            indices.push(
                engine
                    .allocate(FakeRing::new(), -1)
                    .unwrap_or_else(|_| panic!("free")),
            );
        }
        assert_eq!(engine.available(), 0);
        assert!(
            engine.allocate(FakeRing::new(), -1).is_err(),
            "all substreams are busy"
        );
        assert!(engine.release(indices[3]).is_some());
        assert_eq!(engine.available(), 1);
        assert_eq!(
            engine.allocate(FakeRing::new(), -1).ok(),
            Some(indices[3]),
            "freed slot is reused"
        );
        assert_eq!(
            engine.state(indices[3]).state,
            PcmState::Open,
            "reused substream starts clean"
        );
    }

    #[test]
    fn a_failed_device_disconnects_every_substream_and_is_permanent() {
        let (mut engine, output) = (Engine::new(), FakeOutput::new());
        let (ring_a, ring_b) = (FakeRing::new(), FakeRing::new());
        let (a, b) = (open(&mut engine, &ring_a), open(&mut engine, &ring_b));
        write(&mut engine, a, &ring_a, 0.25, 0.25, 2);
        engine.start(a).unwrap();
        engine.tick(
            Events {
                completed: 0,
                disconnected: true,
            },
            &output,
        );
        assert_eq!(engine.state(a).state, PcmState::Disconnected);
        assert_eq!(engine.state(b).state, PcmState::Disconnected);
        assert!(output.submitted.borrow().is_empty());
        // 之后新打开的子流也直接是 Disconnected，引擎不再触碰设备。
        let late = engine
            .allocate(FakeRing::new(), -1)
            .unwrap_or_else(|_| panic!("free"));
        assert_eq!(engine.state(late).state, PcmState::Disconnected);
        assert!(!engine.tick(NONE, &output));
    }

    #[test]
    fn a_submit_error_fails_the_whole_card_but_would_block_just_waits() {
        let (mut engine, output, ring) = (Engine::new(), FakeOutput::new(), FakeRing::new());
        let a = open(&mut engine, &ring);
        write(&mut engine, a, &ring, 0.25, 0.25, 4);
        engine.start(a).unwrap();
        *output.fail_submit.borrow_mut() = Some(PcmOutputError::WouldBlock);
        engine.tick(NONE, &output);
        assert_eq!(
            engine.state(a).state,
            PcmState::Running,
            "a full device queue is not a failure"
        );
        assert!(output.submitted.borrow().is_empty());
        *output.fail_submit.borrow_mut() = Some(PcmOutputError::Device);
        engine.tick(NONE, &output);
        assert_eq!(engine.state(a).state, PcmState::Disconnected);
    }

    #[test]
    fn the_queue_never_exceeds_the_lead_even_with_many_ready_substreams() {
        let (mut engine, output) = (Engine::new(), FakeOutput::new());
        let rings: Vec<_> = (0..SUBSTREAMS).map(|_| FakeRing::new()).collect();
        let indices: Vec<_> = rings.iter().map(|ring| open(&mut engine, ring)).collect();
        for (index, ring) in indices.iter().zip(&rings) {
            write(&mut engine, *index, ring, 0.0625, 0.0625, 4);
            engine.start(*index).unwrap();
        }
        engine.tick(NONE, &output);
        assert_eq!(
            output.submitted.borrow().len(),
            2,
            "lead-bounded regardless of substream count"
        );
        // 8 路 0.0625 相加恰为 0.5。
        assert_eq!(level(&output.submitted.borrow()[0]), (0.5, 0.5));
    }
}

#[cfg(test)]
mod control_tests {
    use super::control::{
        CARD_INFO_BYTES, CTL_PROTOCOL_VERSION, CardText, ControlError, ControlOpen, ELEM_ID_BYTES,
        ELEM_INFO_BYTES, ELEM_LIST_BYTES, ELEM_VALUE_BYTES, ELEMENT_COUNT, EVENT_BYTES, Element,
        SNDRV_CTL_IOCTL_CARD_INFO, SNDRV_CTL_IOCTL_ELEM_INFO, SNDRV_CTL_IOCTL_ELEM_LIST,
        SNDRV_CTL_IOCTL_ELEM_READ, SNDRV_CTL_IOCTL_ELEM_WRITE, SNDRV_CTL_IOCTL_PCM_INFO,
        SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE, SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE,
        SNDRV_CTL_IOCTL_PVERSION, SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, describe_element,
        encode_card_info, encode_event, list_elements, read_element, write_element,
    };
    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_ne_bytes());
    }

    /// 按 `numid` 定位元素的请求 id。
    fn by_numid(numid: u32) -> [u8; ELEM_VALUE_BYTES] {
        let mut bytes = [0u8; ELEM_VALUE_BYTES];
        put_u32(&mut bytes, 0, numid);
        bytes
    }

    /// 按名字定位（numid = 0），alsa-lib 打开混音器时就是这样找元素的。
    fn by_name(name: &[u8], index: u32) -> [u8; ELEM_VALUE_BYTES] {
        let mut bytes = [0u8; ELEM_VALUE_BYTES];
        put_u32(&mut bytes, 4, 2); // iface = MIXER
        bytes[16..16 + name.len()].copy_from_slice(name);
        put_u32(&mut bytes, 60, index);
        bytes
    }

    #[test]
    fn ioctl_numbers_and_sizes_match_linux_7_1_uapi() {
        // 取自用 Linux 7.1 `sound/asound.h` 编译的探针，不是凭记忆。
        assert_eq!(SNDRV_CTL_IOCTL_PVERSION, 0x8004_5500);
        assert_eq!(SNDRV_CTL_IOCTL_CARD_INFO, 0x8178_5501);
        assert_eq!(SNDRV_CTL_IOCTL_ELEM_LIST, 0xc050_5510);
        assert_eq!(SNDRV_CTL_IOCTL_ELEM_INFO, 0xc110_5511);
        assert_eq!(SNDRV_CTL_IOCTL_ELEM_READ, 0xc4c8_5512);
        assert_eq!(SNDRV_CTL_IOCTL_ELEM_WRITE, 0xc4c8_5513);
        assert_eq!(SNDRV_CTL_IOCTL_SUBSCRIBE_EVENTS, 0xc004_5516);
        assert_eq!(SNDRV_CTL_IOCTL_PCM_NEXT_DEVICE, 0x8004_5530);
        assert_eq!(SNDRV_CTL_IOCTL_PCM_INFO, 0xc120_5531);
        assert_eq!(SNDRV_CTL_IOCTL_PCM_PREFER_SUBDEVICE, 0x4004_5532);
        assert_eq!(CTL_PROTOCOL_VERSION, 131_081);
        assert_eq!(
            (
                CARD_INFO_BYTES,
                ELEM_ID_BYTES,
                ELEM_LIST_BYTES,
                ELEM_INFO_BYTES,
                ELEM_VALUE_BYTES
            ),
            (376, 64, 80, 272, 1224)
        );
    }

    #[test]
    fn card_info_places_every_text_field_at_its_linux_offset() {
        let bytes = encode_card_info(&CardText {
            id: b"LiteOS",
            name: b"LiteOS Sound",
            long_name: b"LiteOS VirtIO Sound",
            mixer_name: b"LiteOS Mixer",
        });
        let text = |at: usize| {
            let field = &bytes[at..];
            core::str::from_utf8(&field[..field.iter().position(|&b| b == 0).unwrap()]).unwrap()
        };
        assert_eq!(u32_at(&bytes, 0), 0, "card number");
        assert_eq!(text(8), "LiteOS");
        assert_eq!(text(24), "LiteOS", "driver");
        assert_eq!(text(40), "LiteOS Sound");
        assert_eq!(text(72), "LiteOS VirtIO Sound");
        assert_eq!(text(168), "LiteOS Mixer");
        assert!(
            bytes[248..].iter().all(|&b| b == 0),
            "components stay empty"
        );
    }

    #[test]
    fn listing_returns_all_controls_and_honours_offset_and_space() {
        let mut list = [0u8; ELEM_LIST_BYTES];
        put_u32(&mut list, 4, 16);
        let (ids, used) = list_elements(&mut list).unwrap();
        assert_eq!(
            (used, u32_at(&list, 8), u32_at(&list, 12)),
            (ELEMENT_COUNT, ELEMENT_COUNT as u32, ELEMENT_COUNT as u32)
        );
        assert_eq!(u32_at(&ids, 0), 1);
        assert_eq!(&ids[16..38], b"Master Playback Volume");
        assert_eq!(u32_at(&ids, ELEM_ID_BYTES), 2);
        assert_eq!(
            &ids[ELEM_ID_BYTES + 16..ELEM_ID_BYTES + 38],
            b"Master Playback Switch"
        );
        // 只给 1 个空位：只返回第一个，但 count 仍报告总数，调用方据此知道还有剩余。
        let mut small = [0u8; ELEM_LIST_BYTES];
        put_u32(&mut small, 4, 1);
        assert_eq!(list_elements(&mut small).unwrap().1, 1);
        assert_eq!(u32_at(&small, 12), ELEMENT_COUNT as u32);
        // 从 offset 1 开始：略过第一项。
        let mut shifted = [0u8; ELEM_LIST_BYTES];
        put_u32(&mut shifted, 0, 1);
        put_u32(&mut shifted, 4, 16);
        let (ids, used) = list_elements(&mut shifted).unwrap();
        assert_eq!((used, u32_at(&ids, 0)), (ELEMENT_COUNT - 1, 2));
    }

    #[test]
    fn info_describes_the_integer_range_and_the_boolean() {
        let mut volume = [0u8; ELEM_INFO_BYTES];
        put_u32(&mut volume, 0, 1);
        describe_element(&mut volume).unwrap();
        assert_eq!(
            (
                u32_at(&volume, 64),
                u32_at(&volume, 68),
                u32_at(&volume, 72)
            ),
            (2, 3, 2)
        );
        assert_eq!(
            (
                u64_at(&volume, 80),
                u64_at(&volume, 88),
                u64_at(&volume, 96)
            ),
            (0, 100, 1)
        );
        let mut switch = [0u8; ELEM_INFO_BYTES];
        put_u32(&mut switch, 0, 2);
        describe_element(&mut switch).unwrap();
        assert_eq!(
            (u32_at(&switch, 64), u32_at(&switch, 72)),
            (1, 2),
            "boolean, two channels"
        );
        assert!(
            switch[80..].iter().all(|&b| b == 0),
            "booleans have no range"
        );
        // 按名字查询时 id 被补全（numid、iface 都填上），alsa-lib 依赖它拿到 numid。
        let mut named = [0u8; ELEM_INFO_BYTES];
        named[..ELEM_ID_BYTES]
            .copy_from_slice(&by_name(b"Master Playback Switch", 0)[..ELEM_ID_BYTES]);
        describe_element(&mut named).unwrap();
        assert_eq!(u32_at(&named, 0), 2);
    }

    #[test]
    fn lookup_rejects_unknown_elements_and_wrong_indexes() {
        let mut info = [0u8; ELEM_INFO_BYTES];
        put_u32(&mut info, 0, 99);
        assert_eq!(
            describe_element(&mut info),
            Err(ControlError::NoSuchElement)
        );
        let mut missing = [0u8; ELEM_INFO_BYTES];
        missing[..ELEM_ID_BYTES].copy_from_slice(&by_name(b"Nope", 0)[..ELEM_ID_BYTES]);
        assert_eq!(
            describe_element(&mut missing),
            Err(ControlError::NoSuchElement)
        );
        let mut indexed = [0u8; ELEM_INFO_BYTES];
        indexed[..ELEM_ID_BYTES]
            .copy_from_slice(&by_name(b"Master Playback Volume", 1)[..ELEM_ID_BYTES]);
        assert_eq!(
            describe_element(&mut indexed),
            Err(ControlError::NoSuchElement)
        );
    }

    #[test]
    fn read_reports_per_channel_volume_and_switch() {
        let mut volume = by_numid(1);
        read_element(&mut volume, [30, 70]).unwrap();
        assert_eq!((u64_at(&volume, 72), u64_at(&volume, 80)), (30, 70));
        let mut switch = by_numid(2);
        read_element(&mut switch, [1, 0]).unwrap();
        assert_eq!((u64_at(&switch, 72), u64_at(&switch, 80)), (1, 0));
    }

    #[test]
    fn write_decodes_validated_stereo_values() {
        let mut request = by_numid(1);
        put_u64(&mut request, 72, 40);
        put_u64(&mut request, 80, 60);
        assert_eq!(
            write_element(&request).unwrap(),
            (Element::MasterVolume, [40, 60])
        );
        let mut mute = by_numid(2);
        put_u64(&mut mute, 72, 1);
        put_u64(&mut mute, 80, 0);
        assert_eq!(
            write_element(&mute).unwrap(),
            (Element::MasterSwitch, [1, 0])
        );
    }

    #[test]
    fn write_rejects_out_of_range_values_without_partial_application() {
        let mut request = by_numid(1);
        put_u64(&mut request, 72, 50);
        put_u64(&mut request, 80, 101);
        assert_eq!(write_element(&request), Err(ControlError::Invalid));
        let mut huge = by_numid(1);
        put_u64(&mut huge, 72, u64::MAX);
        assert_eq!(write_element(&huge), Err(ControlError::Invalid));
        let mut switch = by_numid(2);
        put_u64(&mut switch, 72, 1);
        put_u64(&mut switch, 80, 2);
        assert_eq!(write_element(&switch), Err(ControlError::Invalid));
        assert_eq!(
            write_element(&by_numid(99)),
            Err(ControlError::NoSuchElement)
        );
    }

    #[test]
    fn control_events_coalesce_in_first_change_order_and_unsubscribe_discards() {
        let mut file = ControlOpen::new();
        file.notify(Element::MasterVolume);
        file.subscribe(true);
        assert!(!file.readable(), "no events from before subscription");
        file.notify(Element::MasterSwitch);
        file.notify(Element::MasterVolume);
        file.notify(Element::MasterSwitch);
        file.subscribe(true);
        assert_eq!(file.pop(), Some(Element::MasterSwitch));
        assert_eq!(file.pop(), Some(Element::MasterVolume));
        assert_eq!(file.pop(), None);
        for index in 0..ELEMENT_COUNT {
            file.notify(Element::at(index).unwrap());
        }
        for index in 0..ELEMENT_COUNT {
            assert_eq!(file.pop(), Element::at(index));
        }
        file.notify(Element::MasterVolume);
        file.subscribe(false);
        file.subscribe(true);
        assert!(!file.readable());
    }

    #[test]
    fn actual_close_revokes_preference_and_pending_control_events() {
        let mut file = ControlOpen::new();
        file.prefer(5);
        file.subscribe(true);
        file.notify(Element::PcmVolume(5));
        assert_eq!(file.preferred(), 5);
        file.close();
        file.close();
        file.subscribe(true);
        file.notify(Element::MasterVolume);
        assert!(!file.active());
        assert!(!file.subscribed());
        assert_eq!(file.preferred(), -1);
        assert_eq!(file.pop(), None);
    }

    #[test]
    fn pcm_control_name_index_and_event_id_use_the_native_layout() {
        let mut info = [0; ELEM_INFO_BYTES];
        info[..ELEM_ID_BYTES].copy_from_slice(&by_name(b"PCM Playback Volume", 7)[..ELEM_ID_BYTES]);
        describe_element(&mut info).unwrap();
        assert_eq!(u32_at(&info, 0), 10);
        assert_eq!(u32_at(&info, 60), 7);
        let event = encode_event(Element::PcmVolume(7));
        assert_eq!(event.len(), EVENT_BYTES);
        assert_eq!(u32_at(&event, 0), 0);
        assert_eq!(u32_at(&event, 4), 1);
        assert_eq!(&event[8..], &info[..ELEM_ID_BYTES]);
    }
}
