use crate::{Rgb, SystemColor, Theme};

/// `DrawEdge` 的立体边框样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Raised,
    Sunken,
    Etched,
    /// `BDR_RAISEDINNER`：只有内层的单层凸起。
    RaisedInner,
    /// `BDR_SUNKENOUTER`：只有外层的单层凹陷。
    SunkenOuter,
}

/// 一层 1 像素线的取色：`(上边与左边, 下边与右边)`。
pub type Bevel = (Rgb, Rgb);

/// 一个边框样式的外、内两层取色；`None` 表示该层不画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeColors {
    pub outer: Option<Bevel>,
    pub inner: Option<Bevel>,
}

impl Theme {
    /// 取立体边框的两层颜色，与 Win32 `DrawEdge` 一致。
    pub fn edge(&self, edge: Edge) -> EdgeColors {
        use SystemColor::{ButtonDkShadow, ButtonHilight, ButtonLight, ButtonShadow};
        let pair = |top_left, bottom_right| Some((self.color(top_left), self.color(bottom_right)));
        match edge {
            Edge::Raised => EdgeColors {
                outer: pair(ButtonLight, ButtonDkShadow),
                inner: pair(ButtonHilight, ButtonShadow),
            },
            Edge::Sunken => EdgeColors {
                outer: pair(ButtonShadow, ButtonHilight),
                inner: pair(ButtonDkShadow, ButtonLight),
            },
            Edge::Etched => EdgeColors {
                outer: pair(ButtonShadow, ButtonHilight),
                inner: pair(ButtonHilight, ButtonShadow),
            },
            Edge::RaisedInner => EdgeColors {
                outer: None,
                inner: pair(ButtonHilight, ButtonShadow),
            },
            Edge::SunkenOuter => EdgeColors {
                outer: pair(ButtonShadow, ButtonHilight),
                inner: None,
            },
        }
    }
}
