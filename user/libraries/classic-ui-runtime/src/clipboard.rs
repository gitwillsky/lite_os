//! 标准编辑控件的剪切/复制/异步粘贴，接收与取消仍进入 Context 的唯一 FD 循环。
use super::context::{Context, Watch};
use classic_ui::Key;
use classic_ui_abi::{self as abi, event};
use std::{
    io::{self, Read},
    os::{fd::AsFd, unix::net::UnixStream},
    time::{Duration, Instant},
};
pub struct Paste {
    pub window: u64,
    pub node: u64,
    pub reader: UnixStream,
    pub deadline: Instant,
    pub bytes: Vec<u8>,
}
impl Context {
    pub fn edit_clipboard(&mut self, window: u64, key: Key) -> io::Result<bool> {
        if !key.pressed
            || !key.modifiers.control
            || key.modifiers.alt
            || !matches!(key.code, 45..=47)
        {
            return Ok(false);
        }
        let tree = self.host(window)?.controls()?;
        let Some(id) = tree
            .focus
            .filter(|id| tree.active(*id) && tree.nodes[id].editing_text())
        else {
            return Ok(false);
        };
        let command = match key.code {
            45 => 1,
            46 => 2,
            _ => 3,
        };
        self.clipboard_command(window, id, command)?;
        Ok(true)
    }
    pub(super) fn clipboard_command(
        &mut self,
        window: u64,
        id: u64,
        command: u32,
    ) -> io::Result<()> {
        let tree = self.host(window)?.controls()?;
        let node = tree
            .nodes
            .get(&id)
            .filter(|_| tree.active(id))
            .ok_or_else(|| io::Error::other("editor is unavailable"))?;
        if !node.editing_text() {
            return Err(io::Error::other("not an editor"));
        }
        if command == 3 {
            if self.pastes.len() >= 8 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let Some(reader) = self.display.receive_clipboard()? else {
                return Ok(());
            };
            self.cancel_pastes(|paste| paste.window == window && paste.node == id);
            let fd = reader.as_fd().try_clone_to_owned()?;
            let operation = self.next_watch;
            self.next_watch = operation
                .checked_add(1)
                .ok_or_else(|| io::Error::other("FD identities exhausted"))?;
            self.watches.insert(operation, Watch { fd, interest: 1 });
            self.pastes.insert(
                operation,
                Paste {
                    window,
                    node: id,
                    reader,
                    deadline: Instant::now() + Duration::from_secs(3),
                    bytes: Vec::new(),
                },
            );
        } else {
            if node.state & abi::state::PASSWORD != 0 {
                return Ok(());
            }
            let copied = if let Some(editor) = &node.editor {
                editor.document.selected_text().unwrap_or_default()
            } else {
                let edit = node.edit.as_ref().expect("edit model");
                edit.text()[edit.selection()].to_owned()
            };
            if copied.is_empty() {
                return Ok(());
            }
            self.display.set_clipboard(&copied)?;
            if command == 1 {
                self.change_edit(window, id, 1, "")?;
            }
        }
        Ok(())
    }
    pub fn cancel_pastes(&mut self, mut select: impl FnMut(&Paste) -> bool) {
        let ids: Vec<_> = self
            .pastes
            .iter()
            .filter_map(|(id, paste)| select(paste).then_some(*id))
            .collect();
        for id in ids {
            self.pastes.remove(&id);
            self.watches.remove(&id);
        }
    }
    pub fn progress_paste(&mut self, id: u64) -> bool {
        let Some(mut paste) = self.pastes.remove(&id) else {
            return false;
        };
        let live = self
            .windows
            .get(&paste.window)
            .and_then(|host| host.controls().ok())
            .and_then(|tree| tree.nodes.get(&paste.node))
            .is_some_and(|node| node.editing_text());
        if !live {
            self.watches.remove(&id);
            return true;
        }
        let mut complete = None;
        let mut budget = 65536usize;
        if Instant::now() >= paste.deadline {
            complete = Some(Err(1u32));
        }
        while complete.is_none() {
            let mut bytes = [0; 4096];
            match paste.reader.read(&mut bytes) {
                Ok(0) => {
                    complete =
                        Some(String::from_utf8(std::mem::take(&mut paste.bytes)).map_err(|_| 3));
                }
                Ok(count) => {
                    if paste.bytes.len() + count > 1 << 20 {
                        complete = Some(Err(2));
                    } else {
                        paste.bytes.extend_from_slice(&bytes[..count]);
                        budget = budget.saturating_sub(count);
                        if budget == 0 {
                            break;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    budget = budget.saturating_sub(1);
                    if budget == 0 {
                        break;
                    }
                }
                Err(_) => complete = Some(Err(4)),
            }
        }
        match complete {
            None => {
                self.pastes.insert(id, paste);
                if let Some(watch) = self.watches.get_mut(&id) {
                    watch.interest = 1;
                }
            }
            Some(result) => {
                self.watches.remove(&id);
                match result {
                    Ok(text) => {
                        if let Err(error) = self.change_edit(paste.window, paste.node, 4, &text)
                            && self.windows.contains_key(&paste.window)
                        {
                            self.error = error.to_string();
                            self.publish(Some(abi::Event {
                                window: paste.window,
                                node: paste.node,
                                kind: event::CLIPBOARD_ERROR,
                                code: 4,
                                ..Default::default()
                            }));
                        }
                    }
                    Err(code) => self.publish(Some(abi::Event {
                        window: paste.window,
                        node: paste.node,
                        kind: event::CLIPBOARD_ERROR,
                        code,
                        ..Default::default()
                    })),
                }
            }
        }
        true
    }
}
