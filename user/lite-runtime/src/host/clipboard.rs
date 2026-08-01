//! Native clipboard and terminal-paste action publication.

use quickjs_runtime::EngineError;

use super::{Action, Host};

impl Host {
    /// Queues trusted clipboard text for the terminal helper.
    pub(super) fn terminal_paste(&self, text: &str) -> Result<String, EngineError> {
        if text.len() > display_proto::MAX_CLIPBOARD_TEXT {
            return Err(EngineError::from_host(
                "terminal paste exceeds clipboard limit",
            ));
        }
        self.state
            .actions
            .borrow_mut()
            .push(Action::TerminalPaste(text.to_owned()));
        Ok(String::new())
    }

    /// Allocates and queues one asynchronous Clipboard API request identity.
    pub(super) fn clipboard_read(&self) -> Result<String, EngineError> {
        let request = self.next_clipboard_request.get();
        self.next_clipboard_request.set(
            request
                .checked_add(1)
                .ok_or_else(|| EngineError::from_host("clipboard identity exhausted"))?,
        );
        self.state
            .actions
            .borrow_mut()
            .push(Action::ClipboardRead(request));
        Ok(request.to_string())
    }

    /// Queues one bounded UTF-8 Clipboard API publication.
    pub(super) fn clipboard_write(&self, text: &str) -> Result<String, EngineError> {
        if text.len() > display_proto::MAX_CLIPBOARD_TEXT {
            return Err(EngineError::from_host("clipboard text exceeds limit"));
        }
        self.state
            .actions
            .borrow_mut()
            .push(Action::ClipboardWrite(text.to_owned()));
        Ok(String::new())
    }
}
