//! 声音事件只提供业务数据；列表选择、窗口、按钮和异步启动使用共同框架。
use crate::{Page, child, group, label, pair};
use liteos_ui::{Event, EventKind, Layout, PropertySheet, Widget, WidgetKind as Kind};
struct SoundEvent {
    name: &'static str,
    file: &'static str,
}

/// 与桌面、资源管理器实际播放的事件一致；"默认声音"系列都映射到同一个提示音。
const EVENTS: [SoundEvent; 8] = [
    SoundEvent {
        name: "默认声音",
        file: "chord.wav",
    },
    SoundEvent {
        name: "严重错误",
        file: "chord.wav",
    },
    SoundEvent {
        name: "感叹号",
        file: "chord.wav",
    },
    SoundEvent {
        name: "星号",
        file: "chord.wav",
    },
    SoundEvent {
        name: "询问",
        file: "chord.wav",
    },
    SoundEvent {
        name: "登录 LiteOS",
        file: "logon.wav",
    },
    SoundEvent {
        name: "注销 LiteOS",
        file: "logoff.wav",
    },
    SoundEvent {
        name: "清空回收站",
        file: "recycle.wav",
    },
];
const SOUND_DIRECTORY: &str = "/usr/share/liteos/sounds";

pub struct Sounds {
    _controls: Vec<Widget>,
    list: Widget,
    file: Widget,
    preview: Widget,
    volume: Widget,
}
impl Sounds {
    pub fn build(sheet: &PropertySheet, available: bool) -> liteos_ui::Result<Self> {
        let sounds = sheet.page("声音")?;
        let mut controls = vec![label(&sounds, "选择声音事件，可试听对应声音文件。")?];
        let list = child(
            &sounds,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_rows(
            &EVENTS
                .iter()
                .enumerate()
                .map(|(index, event)| (index as u64 + 1, event.name))
                .collect::<Vec<_>>(),
        )?;
        list.select(Some(1))?;
        let (a, file) = pair(&sounds, "声音文件：", EVENTS[0].file)?;
        controls.push(a);
        let preview = child(
            &sounds,
            Kind::Button,
            "试听(P)",
            Layout {
                width: 90,
                height: 26,
                ..Default::default()
            },
        )?;
        let audio = sheet.page("音频")?;
        let playback = group(&audio, "声音播放")?;
        controls.push(label(
            &playback,
            if available {
                "默认设备：LiteOS Playback"
            } else {
                "没有可用的音频输出"
            },
        )?);
        let volume = child(
            &playback,
            Kind::Button,
            "音量(V)...",
            Layout {
                width: 100,
                height: 26,
                ..Default::default()
            },
        )?;
        if !available {
            preview.set_state(liteos_ui::state::DISABLED)?;
            volume.set_state(liteos_ui::state::DISABLED)?;
        }
        controls.push(label(&audio, "录音：没有可用的录音设备。")?);
        controls.push(label(&audio, "MIDI 音乐播放：没有可用的 MIDI 设备。")?);
        controls.extend([sounds, audio, playback]);
        Ok(Self {
            _controls: controls,
            list,
            file,
            preview,
            volume,
        })
    }
}
impl Page for Sounds {
    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<crate::Response>> {
        if event.node == self.volume.id() && matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(Some(crate::Response::Launch(vec!["/bin/sndvol".into()])));
        }
        let selected = self.list.selection()?.unwrap_or(1).saturating_sub(1) as usize;
        let Some(sound) = EVENTS.get(selected) else {
            return Ok(None);
        };
        if event.node == self.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            self.file.set_text(sound.file)?;
        }
        if matches!(event.kind, EventKind::Activate { .. })
            && (event.node == self.preview.id() || event.node == self.list.id())
        {
            return Ok(Some(crate::Response::Launch(vec![
                "/bin/sndplay".into(),
                format!("{SOUND_DIRECTORY}/{}", sound.file),
            ])));
        }
        Ok(None)
    }
}
