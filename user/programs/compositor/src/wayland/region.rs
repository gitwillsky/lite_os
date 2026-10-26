//! wl_region 的集合运算与不可变快照；后续修改原对象不会改变 surface 已引用的区域。
use super::client::{Client, Data, Entry, Fail};
use std::rc::Rc;
use wayland_wire::{
    protocol::{wl_display, wl_region},
    wire::ObjectId,
};

/// 按请求顺序应用的矩形并/差；对一个点，最后覆盖它的操作决定集合归属。
#[derive(Clone, Default)]
pub(super) struct Region {
    operations: Vec<(bool, canvas::Rect)>,
}

impl Region {
    pub(super) fn contains(&self, x: f64, y: f64) -> bool {
        self.operations
            .iter()
            .rev()
            .find_map(|(add, rect)| {
                (x >= f64::from(rect.x)
                    && y >= f64::from(rect.y)
                    && x < f64::from(rect.x) + f64::from(rect.width)
                    && y < f64::from(rect.y) + f64::from(rect.height))
                .then_some(*add)
            })
            .unwrap_or(false)
    }
}

impl Client {
    pub(super) fn region_snapshot(
        &self,
        object: Option<ObjectId>,
    ) -> Result<Option<Rc<Region>>, Fail> {
        object
            .map(|object| match self.objects.get(&object.get()) {
                Some(Entry {
                    data: Data::Region(region),
                    ..
                }) => Ok(region.clone()),
                _ => Err(Fail::invalid_object("object is not a wl_region")),
            })
            .transpose()
    }

    pub(super) fn region(
        &mut self,
        object: ObjectId,
        request: wl_region::Request,
    ) -> Result<(), Fail> {
        let (add, rect) = match request {
            wl_region::Request::Destroy => return Ok(()),
            wl_region::Request::Add {
                x,
                y,
                width,
                height,
            } => (true, canvas::Rect::new(x, y, width, height)),
            wl_region::Request::Subtract {
                x,
                y,
                width,
                height,
            } => (false, canvas::Rect::new(x, y, width, height)),
        };
        if rect.is_empty() {
            return Ok(());
        }
        let Some(Entry {
            data: Data::Region(region),
            ..
        }) = self.objects.get_mut(&object.get())
        else {
            return Err(Fail::invalid_object("object is not a wl_region"));
        };
        // OWNER: COW 保活已提交和待提交快照；操作数预算防止单对象绕过对象数量限制耗尽内存。
        if region.operations.len() >= 4096 {
            return Err(Fail::new(
                ObjectId::DISPLAY,
                wl_display::Error::NoMemory as u32,
                "region operation budget exhausted",
            ));
        }
        Rc::make_mut(region).operations.push((add, rect));
        Ok(())
    }

    pub(super) fn accepts_input(&self, surface: ObjectId, x: f64, y: f64) -> bool {
        matches!(self.objects.get(&surface.get()), Some(Entry { data: Data::Surface(state), .. })
            if state.current.input_region.as_ref().is_none_or(|region| region.contains(x, y)))
    }
}
