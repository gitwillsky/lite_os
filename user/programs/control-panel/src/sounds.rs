//! 声音事件只提供业务数据；列表选择、窗口、按钮和异步启动使用共同框架。
use crate::{Apply, Page, child, group, label, pair};
use linux_uapi::alsa::{NativeOutput, preferred};
use liteos_ui::{
    Event, EventKind, Layout, PropertySheet, TreeRow, Widget, WidgetKind as Kind, state,
};
// 事件名与声音文件只由 `sound::Event` 的默认方案表给出，桌面与资源管理器播放同一张表。
use sound::Event as SoundEvent;

pub struct Sounds {
    _controls: Vec<Widget>,
    list: Widget,
    file: Widget,
    preview: Widget,
    volume: Widget,
    output: Widget,
    outputs: Vec<NativeOutput>,
    saved: Option<usize>,
}
impl Sounds {
    pub fn build(
        sheet: &PropertySheet,
        outputs: Vec<NativeOutput>,
        preferred: Option<String>,
    ) -> liteos_ui::Result<Self> {
        let saved = match preferred {
            Some(id) => outputs.iter().position(|output| output.id == id),
            None => (!outputs.is_empty()).then_some(0),
        };
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
            &SoundEvent::ALL
                .iter()
                .enumerate()
                .map(|(index, event)| (index as u64 + 1, event.label()))
                .collect::<Vec<_>>(),
        )?;
        list.select(Some(1))?;
        let (a, file) = pair(&sounds, "声音文件：", SoundEvent::ALL[0].file())?;
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
        controls.push(label(&playback, "首选设备(P)：")?);
        let output = child(
            &playback,
            Kind::Combo,
            saved.map_or("首选设备不可用", |index| &outputs[index].name),
            Layout {
                height: 24,
                ..Default::default()
            },
        )?;
        output.set_state(state::DROPDOWN_LIST)?;
        output.set_tree_rows(
            &outputs
                .iter()
                .enumerate()
                .map(|(index, output)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text: &output.name,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        output.select(saved.map(|index| index as u64 + 1))?;
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
        if outputs.is_empty() {
            output.set_state(state::DROPDOWN_LIST | state::DISABLED)?;
        }
        if saved.is_none() {
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
            output,
            outputs,
            saved,
        })
    }

    fn selected(&self) -> liteos_ui::Result<Option<usize>> {
        Ok(self
            .output
            .selection()?
            .and_then(|id| id.checked_sub(1))
            .map(|index| index as usize))
    }
}
impl Page for Sounds {
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.selected()? != self.saved)
    }

    fn apply(&self) -> liteos_ui::Result<Apply> {
        let selected = self.selected()?;
        let output = selected
            .filter(|_| selected != self.saved)
            .and_then(|index| self.outputs.get(index))
            .map(|output| output.id.clone());
        Ok(Box::new(move || {
            if let Some(id) = output {
                preferred::set(&id).map_err(|error| format!("无法设置首选设备：{error}"))?;
            }
            Ok(())
        }))
    }

    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.selected()?;
        if self.saved.is_some() {
            self.preview.set_state(0)?;
            self.volume.set_state(0)?;
        }
        Ok(())
    }

    fn event(&mut self, event: &Event) -> liteos_ui::Result<Option<crate::Response>> {
        if event.node == self.volume.id() && matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(Some(crate::Response::Launch(vec!["/bin/sndvol".into()])));
        }
        let selected = self.list.selection()?.unwrap_or(1).saturating_sub(1) as usize;
        let Some(sound) = SoundEvent::ALL.get(selected) else {
            return Ok(None);
        };
        if event.node == self.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            self.file.set_text(sound.file())?;
        }
        if matches!(event.kind, EventKind::Activate { .. })
            && (event.node == self.preview.id() || event.node == self.list.id())
        {
            return Ok(Some(crate::Response::Launch(vec![
                "/bin/sndplay".into(),
                format!("{}/{}", sound::DIRECTORY, sound.file()),
            ])));
        }
        Ok(None)
    }
}
