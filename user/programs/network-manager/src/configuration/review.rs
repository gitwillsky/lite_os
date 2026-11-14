//! 审阅窗口只把当前页和所选差异的片段送给控件。

use std::{borrow::Cow, fmt, io, ops::Range, sync::Arc};

use liteos_ui::{
    Background, Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind,
    Window, WindowOptions, state,
};
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use crate::{APP_ID, child};

const ROWS_PER_PAGE: usize = 32;
const VALUE_PAGE_BYTES: usize = 8192;

#[derive(Deserialize)]
struct Change<'a> {
    #[serde(borrow)]
    path: &'a RawValue,
    #[serde(borrow, default, deserialize_with = "present")]
    before: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    after: Option<&'a RawValue>,
}

fn present<'de, D: Deserializer<'de>>(decoder: D) -> Result<Option<&'de RawValue>, D::Error> {
    // 缺失字段与显式 JSON null 在审阅中必须区分。
    <&RawValue>::deserialize(decoder).map(Some)
}

struct Entry {
    path: Range<usize>,
    before: Option<Range<usize>>,
    after: Option<Range<usize>>,
}

/// 背景线程一次建立原始差异的字段索引。字段内容只存一份。
pub(super) struct Review {
    document: String,
    rows: Vec<Entry>,
}

struct Index<'a>(&'a [u8]);

impl<'de> DeserializeSeed<'de> for Index<'de> {
    type Value = Vec<Entry>;

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Index<'de> {
    type Value = Vec<Entry>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a list of configuration changes")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Self::Value, A::Error> {
        let mut rows = Vec::new();
        let range = |value: &RawValue| {
            let text = value.get();
            // RawValue 的 slice 直接借用同一份输入，反序列化不会重建或移动正文。
            let start = text.as_ptr() as usize - self.0.as_ptr() as usize;
            start..start + text.len()
        };
        while let Some(row) = items.next_element::<Change<'_>>()? {
            rows.try_reserve(1).map_err(de::Error::custom)?;
            rows.push(Entry {
                path: range(row.path),
                before: row.before.map(range),
                after: row.after.map(range),
            });
        }
        Ok(rows)
    }
}

impl Review {
    /// 校验差异外层并建立字段偏移。解析失败不打开确认窗口。
    pub(super) fn new(document: Vec<u8>) -> io::Result<Self> {
        let document = String::from_utf8(document).map_err(io::Error::other)?;
        let mut decoder = serde_json::Deserializer::from_str(&document);
        let rows = Index(document.as_bytes()).deserialize(&mut decoder)?;
        decoder.end()?;
        Ok(Self { document, rows })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn text(&self, range: &Range<usize>) -> &str {
        &self.document[range.clone()]
    }

    fn path(&self, row: usize) -> io::Result<Cow<'_, str>> {
        let raw = self.text(&self.rows[row].path);
        if let Some(text) = raw
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            && !text.contains('\\')
        {
            return Ok(Cow::Borrowed(text));
        }
        Ok(Cow::Owned(serde_json::from_str(raw)?))
    }

    fn find(&self, query: &str, after: usize) -> io::Result<Option<usize>> {
        for row in (after + 1..self.rows.len()).chain(0..=after) {
            let entry = &self.rows[row];
            if self.path(row)?.contains(query)
                || entry
                    .before
                    .iter()
                    .chain(&entry.after)
                    .any(|range| self.text(range).contains(query))
            {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn detail(&self, index: usize, page: usize) -> io::Result<(String, usize, usize)> {
        let row = &self.rows[index];
        let before = row.before.as_ref().map(|range| self.text(range));
        let after = row.after.as_ref().map(|range| self.text(range));
        let pages = before
            .map_or(0, str::len)
            .max(after.map_or(0, str::len))
            .div_ceil(VALUE_PAGE_BYTES)
            .max(1);
        let page = page.min(pages - 1);
        let excerpt = |value: Option<&str>| -> String {
            match value {
                None => "（不存在）".into(),
                Some(value) => {
                    let start =
                        value.floor_char_boundary((page * VALUE_PAGE_BYTES).min(value.len()));
                    let end =
                        value.floor_char_boundary(((page + 1) * VALUE_PAGE_BYTES).min(value.len()));
                    value[start..end].into()
                }
            }
        };
        Ok((
            format!(
                "{}\n\n修改前\n{}\n\n修改后\n{}",
                self.path(index)?,
                excerpt(before),
                excerpt(after)
            ),
            page,
            pages,
        ))
    }
}

struct Controls {
    list: Widget,
    detail: Widget,
    position: Widget,
    previous: Widget,
    next: Widget,
    previous_value: Widget,
    next_value: Widget,
}

impl Controls {
    fn page(&self, review: &Review, selected: usize) -> Result<(), super::Error> {
        let start = selected / ROWS_PER_PAGE * ROWS_PER_PAGE;
        let end = (start + ROWS_PER_PAGE).min(review.rows.len());
        let text: Vec<_> = (start..end)
            .map(|index| review.path(index))
            .collect::<io::Result<_>>()?;
        let rows: Vec<_> = (start..end)
            .zip(&text)
            .map(|(index, path)| (index as u64 + 1, path.as_ref()))
            .collect();
        self.list.set_rows(&rows)?;
        self.list.select(Some(selected as u64 + 1))?;
        self.previous
            .set_state(if start == 0 { state::DISABLED } else { 0 })?;
        self.next.set_state(if end == review.rows.len() {
            state::DISABLED
        } else {
            0
        })?;
        self.detail(review, selected, 0).map(|_| ())
    }

    fn detail(&self, review: &Review, selected: usize, page: usize) -> Result<usize, super::Error> {
        let (text, page, pages) = review.detail(selected, page)?;
        self.detail.set_text(&text)?;
        self.position.set_text(&format!(
            "第 {} / {} 项 · 值第 {} / {} 页",
            selected + 1,
            review.rows.len(),
            page + 1,
            pages
        ))?;
        self.previous_value
            .set_state(if page == 0 { state::DISABLED } else { 0 })?;
        self.next_value.set_state(if page + 1 == pages {
            state::DISABLED
        } else {
            0
        })?;
        Ok(page)
    }
}

/// 分页审阅一个代际的差异。确认窗口关闭或取消时不提交配置。
pub(super) async fn confirm(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    review: Arc<Review>,
    recovery: bool,
) -> Result<bool, super::Error> {
    let mut options = WindowOptions::new("审阅路由配置", APP_ID, 740, 500);
    options.parent = Some(parent);
    options.modal = true;
    let (window, mut events) = dialogs.window(options)?;
    let root = window.widget(liteos_ui::WidgetSpec {
        layout: Layout {
            grow: 1,
            padding: 10,
            gap: 6,
            ..Layout::default()
        },
        ..liteos_ui::WidgetSpec::new(WidgetKind::Column, "")
    })?;
    let explanation = if recovery {
        "当前设置无法加载，提交将恢复以下配置。"
    } else {
        "核对改动后提交。设置变更可能影响网络连接。"
    };
    let _explanation = child(&root, WidgetKind::Label, explanation, Layout::default())?;
    let grow = Layout {
        grow: 1,
        ..Layout::default()
    };
    let row = Layout {
        gap: 6,
        ..Layout::default()
    };
    let button = Layout {
        width: 82,
        height: 23,
        ..Layout::default()
    };
    let search = child(&root, WidgetKind::Row, "", row)?;
    let _search_label = child(&search, WidgetKind::Label, "路径或值", Layout::default())?;
    let query = child(&search, WidgetKind::Edit, "", grow)?;
    let find = child(&search, WidgetKind::Button, "查找下一个", button)?;
    let list = child(&root, WidgetKind::List, "", grow)?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(&[Column {
        id: 1,
        title: "改动位置",
        width: 650,
    }])?;
    let detail = child(&root, WidgetKind::TextEditor, "", grow)?;
    detail.set_state(state::READ_ONLY)?;
    let bar = child(&root, WidgetKind::Row, "", row)?;
    let controls = Controls {
        list,
        detail,
        previous: child(&bar, WidgetKind::Button, "上一页", button)?,
        next: child(&bar, WidgetKind::Button, "下一页", button)?,
        previous_value: child(&bar, WidgetKind::Button, "值上一页", button)?,
        next_value: child(&bar, WidgetKind::Button, "值下一页", button)?,
        position: child(&bar, WidgetKind::Label, "", grow)?,
    };
    let submit = child(&bar, WidgetKind::Button, "提交", button)?;
    let cancel = child(&bar, WidgetKind::Button, "取消", button)?;
    cancel.set_state(state::DEFAULT)?;
    let mut selected = 0;
    let mut detail_page = 0;
    controls.page(&review, selected)?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(false),
            EventKind::Select { .. } if event.node == controls.list.id() => {
                if let Some(index) = controls.list.selection()?.map(|id| id as usize - 1)
                    && index != selected
                {
                    selected = index;
                    detail_page = 0;
                    controls.detail(&review, selected, detail_page)?;
                }
            }
            EventKind::Activate { .. } if event.node == submit.id() => return Ok(true),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(false),
            EventKind::Activate { .. }
                if event.node == controls.previous_value.id()
                    || event.node == controls.next_value.id() =>
            {
                detail_page = if event.node == controls.previous_value.id() {
                    detail_page.saturating_sub(1)
                } else {
                    detail_page + 1
                };
                detail_page = controls.detail(&review, selected, detail_page)?;
            }
            EventKind::Activate { .. } => {
                if event.node == controls.previous.id() {
                    selected = (selected / ROWS_PER_PAGE).saturating_sub(1) * ROWS_PER_PAGE;
                } else if event.node == controls.next.id() {
                    let next = (selected / ROWS_PER_PAGE + 1) * ROWS_PER_PAGE;
                    if next >= review.rows.len() {
                        continue;
                    }
                    selected = next;
                } else if event.node == find.id() {
                    let query = query.text()?;
                    if query.is_empty() {
                        continue;
                    }
                    let source = review.clone();
                    let Some(index) = background
                        .run(move || source.find(&query, selected))?
                        .await?
                    else {
                        controls.position.set_text("未找到匹配项")?;
                        continue;
                    };
                    selected = index;
                } else {
                    continue;
                }
                detail_page = 0;
                controls.page(&review, selected)?;
            }
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_preserves_null_and_pages_multibyte_values() {
        let value = "文".repeat(VALUE_PAGE_BYTES / 3 + 4);
        let document = serde_json::to_vec(&serde_json::json!([
            { "path": "/routes/0", "after": null },
            { "path": "/interfaces/\"uplink\"", "after": value }
        ]))
        .unwrap();
        let review = Review::new(document).unwrap();
        let (detail, _, _) = review.detail(0, 0).unwrap();
        assert!(detail.contains("修改前\n（不存在）\n\n修改后\nnull"));
        assert_eq!(review.find("\"uplink\"", 0).unwrap(), Some(1));
        let (first, _, pages) = review.detail(1, 0).unwrap();
        let (last, page, _) = review.detail(1, 2).unwrap();
        assert_eq!((page, pages), (1, 2));
        assert_eq!(
            first.matches('文').count() + last.matches('文').count(),
            value.chars().count()
        );
    }
}
