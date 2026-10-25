//! 固定 xdg-dialog-v1：对象唯一性、模态提示与 toplevel 销毁后的 inert 生命周期。
use super::client::{Client, Data, Entry, Fail};
use wayland_wire::{
    protocol::{Interface, xdg_dialog_v1, xdg_wm_dialog_v1},
    wire::ObjectId,
};
/// 提示唯一归属于协议对象；窗口列表只投影它，不保存第二份可写状态。
pub(super) struct Dialog {
    pub toplevel: Option<ObjectId>,
    pub modal: bool,
}
impl Client {
    pub(super) fn dialog_manager(
        &mut self,
        object: ObjectId,
        version: u32,
        request: xdg_wm_dialog_v1::Request,
    ) -> Result<(), Fail> {
        match request {
            xdg_wm_dialog_v1::Request::Destroy => Ok(()),
            xdg_wm_dialog_v1::Request::GetXdgDialog { id, toplevel } => {
                if !matches!(
                    self.objects.get(&toplevel.get()),
                    Some(Entry {
                        data: Data::Toplevel(_),
                        ..
                    })
                ) {
                    return Err(Fail::invalid_object("dialog target is not an xdg_toplevel"));
                }
                if self.objects.values().any(|entry| matches!(&entry.data, Data::Dialog(dialog) if dialog.toplevel == Some(toplevel))) {
                    return Err(Fail::new(object, xdg_wm_dialog_v1::Error::AlreadyUsed as u32, "toplevel already has an xdg_dialog_v1"));
                }
                self.create(
                    id,
                    Interface::XdgDialogV1,
                    version,
                    Data::Dialog(Dialog {
                        toplevel: Some(toplevel),
                        modal: false,
                    }),
                )
            }
        }
    }
    pub(super) fn dialog(
        &mut self,
        object: ObjectId,
        request: xdg_dialog_v1::Request,
    ) -> Result<(), Fail> {
        let Some(Entry {
            data: Data::Dialog(dialog),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::invalid_object("unknown dialog"));
        };
        match request {
            xdg_dialog_v1::Request::Destroy => {}
            xdg_dialog_v1::Request::SetModal => dialog.modal = true,
            xdg_dialog_v1::Request::UnsetModal => dialog.modal = false,
        }
        Ok(())
    }
    pub(super) fn dialog_modal(&self, toplevel: ObjectId) -> bool {
        self.objects.values().any(|entry| matches!(&entry.data, Data::Dialog(dialog) if dialog.toplevel == Some(toplevel) && dialog.modal))
    }
    pub(super) fn inert_dialogs(&mut self, toplevel: ObjectId) {
        for entry in self.objects.values_mut() {
            if let Data::Dialog(dialog) = &mut entry.data {
                if dialog.toplevel == Some(toplevel) {
                    dialog.toplevel = None;
                }
            }
        }
    }
}
