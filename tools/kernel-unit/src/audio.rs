//! 音频纯逻辑的宿主测试入口：ALSA 编解码、控件与状态来自 `system/linuxd/src/devices/audio/`，混音数学来自
//! `system/audio-core/src/mix.rs`。

#[path = "../../../system/linuxd/src/devices/audio/codec.rs"]
pub(crate) mod codec;
#[path = "../../../system/linuxd/src/devices/audio/control.rs"]
pub(crate) mod control;
#[path = "../../../system/linuxd/src/devices/audio/endpoint.rs"]
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "宿主测试只覆盖共享端点的 ABI 编码；独占端点与端点名称由 linuxd 的 ALSA 文件使用"
    )
)]
pub(crate) mod endpoint;
#[path = "../../../system/audio-core/src/mix.rs"]
pub(crate) mod mixer;
pub(crate) use endpoint::{PcmEndpoint, SUBSTREAMS};
use mixer::VOLUME_MAX;
#[path = "../../../system/linuxd/src/devices/audio/state.rs"]
pub(crate) mod state;

#[cfg(test)]
mod abi_tests;

#[cfg(test)]
mod mixing_tests {
    use super::mixer::{Accumulator, VOLUME_MAX, volume_gain};
    use crate::devices::model::{PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES};

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
            card: 0,
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
