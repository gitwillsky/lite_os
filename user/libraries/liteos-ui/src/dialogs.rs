//! 可跨 await 使用的 UI 线程对话框入口；与 Application 共用唯一窗口路由和订阅 owner。
use crate::{
    Error, FileDialogFuture, FileDialogMode, FileFilter, MessageBoxFuture, MessageButtons, Result,
    WeakWindow, Window,
    connection::{Connection, text},
    events::Subscriptions,
};
use classic_ui_abi as abi;
use std::{cell::RefCell, collections::BTreeMap, os::unix::ffi::OsStrExt, path::Path, rc::Rc};
/// 对话框工厂，不实现 Send/Sync；允许异步业务顺序询问、选择文件和报告错误。
#[derive(Clone)]
pub struct Dialogs {
    pub(crate) connection: Rc<Connection>,
    pub(crate) windows: Rc<RefCell<BTreeMap<u64, WeakWindow>>>,
    pub(crate) subscriptions: Rc<RefCell<Subscriptions>>,
}
impl Dialogs {
    pub(super) fn check(&self, parent: &Window) -> Result<()> {
        parent.inner.check()?;
        if !Rc::ptr_eq(&self.connection, &parent.inner.connection) {
            return Err(Error {
                code: abi::INVALID,
                message: "parent belongs to another application".into(),
            });
        }
        Ok(())
    }
    fn events(&self, window: &Window) -> Result<crate::EventStream> {
        self.windows
            .borrow_mut()
            .insert(window.id(), window.downgrade());
        self.subscriptions
            .borrow_mut()
            .subscribe(window.id(), None, &window.inner.scope)
    }
    /// 打开共享消息框；父窗口关闭、Escape 或标题栏关闭返回 Cancel。
    pub fn message(
        &self,
        parent: &Window,
        title: &str,
        message: &str,
        buttons: MessageButtons,
    ) -> Result<MessageBoxFuture> {
        self.check(parent)?;
        let mut handles = abi::MessageBox::default();
        // SAFETY: 本次同步调用内文本有效、输出独占，运行库只保留副本。
        self.connection.check(unsafe {
            (self.connection.api.message_box)(
                self.connection.context.as_ptr(),
                parent.id(),
                text(title),
                text(message),
                buttons as u32,
                &mut handles,
            )
        })?;
        let window = Window::adopt(self.connection.clone(), handles.window, Some(parent));
        let events = self.events(&window)?;
        Ok(MessageBoxFuture::new(
            window,
            events,
            handles.buttons,
            buttons,
        ))
    }
    /// 打开共享文件对话框；原生路径字节不强制转成 UTF-8，取消返回 None。
    pub fn file(
        &self,
        parent: &Window,
        mode: FileDialogMode,
        directory: &Path,
        name: &str,
    ) -> Result<FileDialogFuture> {
        self.check(parent)?;
        let bytes = directory.as_os_str().as_bytes();
        let mut id = 0;
        // SAFETY: 路径、文本和输出在同步调用内存活，运行库复制参数。
        self.connection.check(unsafe {
            (self.connection.api.file_dialog)(
                self.connection.context.as_ptr(),
                parent.id(),
                mode as u32,
                abi::Bytes {
                    data: bytes.as_ptr(),
                    len: bytes.len(),
                },
                text(name),
                &mut id,
            )
        })?;
        let window = Window::adopt(self.connection.clone(), id, Some(parent));
        let events = self.events(&window)?;
        Ok(FileDialogFuture::new(window, events))
    }
    /// 按指定扩展名筛选共享文件对话框，参数仅借用本次调用。
    /// 扩展名不含前导点；非法筛选、连接或创建失败返回 Err，取消结果仍为 None。
    pub fn file_filtered(
        &self,
        parent: &Window,
        mode: FileDialogMode,
        directory: &Path,
        name: &str,
        filter: FileFilter<'_>,
    ) -> Result<FileDialogFuture> {
        self.check(parent)?;
        let bytes = directory.as_os_str().as_bytes();
        let extensions: Vec<_> = filter
            .extensions
            .iter()
            .map(|extension| text(extension))
            .collect();
        let filter = abi::FileFilter {
            label: text(filter.label),
            extensions: extensions.as_ptr(),
            count: extensions.len(),
        };
        let mut id = 0;
        // SAFETY: 筛选描述符、文本、路径和输出只在本次同步调用内借用，运行库复制所有输入。
        self.connection.check(unsafe {
            (self.connection.api.file_dialog_filtered)(
                self.connection.context.as_ptr(),
                parent.id(),
                mode as u32,
                abi::Bytes {
                    data: bytes.as_ptr(),
                    len: bytes.len(),
                },
                text(name),
                &filter,
                &mut id,
            )
        })?;
        let window = Window::adopt(self.connection.clone(), id, Some(parent));
        let events = self.events(&window)?;
        Ok(FileDialogFuture::new(window, events))
    }
}
