//! 搜索选项的表单快照与查询级编译；元数据先筛选，再打开正文，遍历目录不受结果筛选影响。
use super::child;
use desktop_entry::Associations;
use liteos_ui::{Layout, TreeRow, Widget, WidgetKind as Kind, state};
use std::{
    collections::HashSet,
    fs::Metadata,
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Draft {
    pub date: Option<Date>,
    pub kind: Option<String>,
    pub size: Option<(u64, String)>,
    pub case_sensitive: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Date {
    field: u64,
    period: u64,
    amount: String,
    from: String,
    to: String,
}
enum FileType {
    Any,
    Folder,
    Registered(HashSet<String>),
}
pub(super) struct Compiled {
    date: Option<(u64, i64, i64)>,
    kind: FileType,
    size: Option<(u64, u64)>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn number(value: &str) -> io::Result<u64> {
    value
        .trim()
        .parse()
        .map_err(|_| invalid("必须指定有效的非负整数。"))
}
fn day(value: &str) -> io::Result<i64> {
    let parts: Vec<_> = value.trim().split('-').collect();
    let failure = || invalid("必须指定有效的日期或日期范围，格式为 年-月-日。");
    if parts.len() != 3 {
        return Err(failure());
    }
    let date = calendar::Date {
        year: parts[0].parse().map_err(|_| failure())?,
        month: parts[1].parse().map_err(|_| failure())?,
        day: parts[2].parse().map_err(|_| failure())?,
    };
    if !(1601..=9999).contains(&date.year)
        || !(1..=12).contains(&date.month)
        || !(1..=calendar::days_in_month(date.year, date.month)).contains(&date.day)
    {
        return Err(failure());
    }
    Ok(calendar::days_from_civil(date))
}
fn today() -> i64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    calendar::zone::to_local(unix.min(i64::MAX as u64) as i64).div_euclid(86400)
}
fn date_text(day: i64) -> String {
    let date = calendar::civil_from_days(day);
    format!("{}-{:02}-{:02}", date.year, date.month, date.day)
}
impl Draft {
    pub(super) fn compile(&self, associations: &Associations) -> io::Result<Compiled> {
        let date = self
            .date
            .as_ref()
            .map(|date| {
                let current = today();
                let (from, to) = match date.period {
                    1 => {
                        let months = number(&date.amount)?;
                        if months == 0 || months > 120000 {
                            return Err(invalid("月份数必须介于 1 和 120000。"));
                        }
                        let now = calendar::civil_from_days(current);
                        let index = now.year * 12 + i64::from(now.month) - 1 - months as i64;
                        let year = index.div_euclid(12);
                        let month = index.rem_euclid(12) as u32 + 1;
                        (
                            calendar::days_from_civil(calendar::Date {
                                year,
                                month,
                                day: now.day.min(calendar::days_in_month(year, month)),
                            }),
                            current,
                        )
                    }
                    2 => {
                        let days = number(&date.amount)?;
                        if days == 0 || days > 3652058 {
                            return Err(invalid("天数必须介于 1 和 3652058。"));
                        }
                        (current - days as i64, current)
                    }
                    _ => (day(&date.from)?, day(&date.to)?),
                };
                if from > to {
                    return Err(invalid("开始日期不能晚于结束日期。"));
                }
                Ok((date.field, from, to))
            })
            .transpose()?;
        let kind = match self.kind.as_deref().map(str::trim) {
            None | Some("") | Some("所有文件和文件夹") => FileType::Any,
            Some("文件夹") => FileType::Folder,
            Some(name) => {
                let types: HashSet<_> = associations
                    .registered_types()
                    .filter(|(mime, label)| *mime == name || *label == name)
                    .map(|(mime, _)| mime.to_owned())
                    .collect();
                if types.is_empty() {
                    return Err(invalid("此文件类型未注册，请重新选择类型。"));
                }
                FileType::Registered(types)
            }
        };
        let size = self
            .size
            .as_ref()
            .map(|(comparison, amount)| {
                let bytes = number(amount)?
                    .checked_mul(1024)
                    .ok_or_else(|| invalid("文件大小超出可表示范围。"))?;
                Ok::<_, io::Error>((*comparison, bytes))
            })
            .transpose()?;
        Ok(Compiled { date, kind, size })
    }
}
impl Compiled {
    pub(super) fn accepts(
        &self,
        path: &Path,
        metadata: &Metadata,
        associations: &Associations,
    ) -> bool {
        let kind = match &self.kind {
            FileType::Any => true,
            FileType::Folder => metadata.is_dir(),
            FileType::Registered(types) => {
                !metadata.is_dir()
                    && associations
                        .mime_for(path)
                        .is_some_and(|mime| types.contains(mime))
            }
        };
        if !kind {
            return false;
        }
        if let Some((comparison, bytes)) = self.size
            && (!metadata.is_file()
                || !match comparison {
                    2 => metadata.len() >= bytes,
                    3 => metadata.len() <= bytes,
                    _ => metadata.len() == bytes,
                })
        {
            return false;
        }
        self.date.is_none_or(|(field, from, to)| {
            let stamp = match field {
                2 => metadata.created(),
                3 => metadata.accessed(),
                _ => metadata.modified(),
            };
            stamp
                .ok()
                .and_then(|stamp| match stamp.duration_since(UNIX_EPOCH) {
                    Ok(duration) => i64::try_from(duration.as_secs()).ok(),
                    Err(error) => i64::try_from(error.duration().as_secs())
                        .ok()
                        .and_then(|seconds| seconds.checked_neg())
                        .and_then(|seconds| {
                            seconds.checked_sub(i64::from(error.duration().subsec_nanos() != 0))
                        }),
                })
                .is_some_and(|seconds| {
                    (from..=to).contains(&calendar::zone::to_local(seconds).div_euclid(86400))
                })
        })
    }
}

struct Section {
    check: Widget,
    body: Widget,
}
pub struct Controls {
    pub toggle: Widget,
    body: Widget,
    sections: [Section; 4],
    field: Widget,
    period: Widget,
    amount_area: Widget,
    dates: Widget,
    amount: Widget,
    from: Widget,
    to: Widget,
    kind: Widget,
    comparison: Widget,
    size: Widget,
    pub recursive: Widget,
    case_sensitive: Widget,
    _owned: Vec<Widget>,
}
fn combo(parent: &Widget, labels: &[&str]) -> liteos_ui::Result<Widget> {
    let combo = child(parent, Kind::Combo, "", Layout::default())?;
    combo.set_state(state::DROPDOWN_LIST)?;
    combo.set_tree_rows(
        &labels
            .iter()
            .enumerate()
            .map(|(index, text)| TreeRow {
                id: index as u64 + 1,
                parent: 0,
                text,
                expanded: false,
                has_children: false,
            })
            .collect::<Vec<_>>(),
    )?;
    combo.select(Some(1))?;
    Ok(combo)
}
impl Controls {
    pub fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let toggle = child(parent, Kind::Link, "搜索选项 >>", Layout::default())?;
        let body = child(
            parent,
            Kind::Column,
            "",
            Layout {
                gap: 3,
                ..Default::default()
            },
        )?;
        body.set_state(state::HIDDEN)?;
        let mut sections = Vec::new();
        for title in ["日期", "类型", "大小", "高级选项"] {
            let check = child(&body, Kind::CheckBox, title, Layout::default())?;
            let inner = child(
                &body,
                Kind::Column,
                "",
                Layout {
                    padding: 6,
                    gap: 3,
                    ..Default::default()
                },
            )?;
            inner.set_state(state::HIDDEN)?;
            sections.push(Section { check, body: inner });
        }
        let sections: [Section; 4] = sections.try_into().ok().expect("four search sections");
        let mut owned = Vec::new();
        let mut label = |parent: &Widget, text| -> liteos_ui::Result<()> {
            let label = child(parent, Kind::Label, text, Layout::default())?;
            label.set_state(state::WRAP)?;
            owned.push(label);
            Ok(())
        };
        let date = &sections[0].body;
        label(date, "搜索:")?;
        let field = combo(date, &["修改过的文件", "创建的文件", "最近访问过的文件"])?;
        let period = combo(date, &["在前几个月", "在前几天", "介于以下日期"])?;
        let amount_area = child(date, Kind::Column, "", Layout::default())?;
        label(&amount_area, "月数或天数:")?;
        let amount = child(&amount_area, Kind::Edit, "1", Layout::default())?;
        let dates = child(date, Kind::Column, "", Layout::default())?;
        dates.set_state(state::HIDDEN)?;
        label(&dates, "从 (年-月-日):")?;
        let from = child(&dates, Kind::Edit, &date_text(today()), Layout::default())?;
        label(&dates, "至 (年-月-日):")?;
        let to = child(&dates, Kind::Edit, &date_text(today()), Layout::default())?;
        let kind = combo(&sections[1].body, &["所有文件和文件夹", "文件夹"])?;
        let comparison = combo(&sections[2].body, &["等于", "至少", "至多"])?;
        label(&sections[2].body, "大小(以 KB 为单位):")?;
        let size = child(&sections[2].body, Kind::Edit, "0", Layout::default())?;
        let recursive = child(
            &sections[3].body,
            Kind::CheckBox,
            "搜索子文件夹",
            Layout::default(),
        )?;
        recursive.set_state(state::CHECKED)?;
        let case_sensitive = child(
            &sections[3].body,
            Kind::CheckBox,
            "区分大小写",
            Layout::default(),
        )?;
        sections[3].check.set_state(state::CHECKED)?;
        sections[3].body.set_state(0)?;
        Ok(Self {
            toggle,
            body,
            sections,
            field,
            period,
            amount_area,
            dates,
            amount,
            from,
            to,
            kind,
            comparison,
            size,
            recursive,
            case_sensitive,
            _owned: owned,
        })
    }
    pub fn editing(&self, node: u64) -> bool {
        [
            &self.field,
            &self.period,
            &self.amount,
            &self.from,
            &self.to,
            &self.kind,
            &self.comparison,
            &self.size,
        ]
        .iter()
        .any(|field| field.id() == node)
    }
    pub fn selected(&self, node: u64) -> liteos_ui::Result<()> {
        if node == self.period.id() {
            let between = self.period.selection()? == Some(3);
            self.amount_area
                .set_state(if between { state::HIDDEN } else { 0 })?;
            self.dates
                .set_state(if between { 0 } else { state::HIDDEN })?;
        }
        Ok(())
    }
    pub fn handles(&self, node: u64) -> bool {
        node == self.toggle.id()
            || self
                .sections
                .iter()
                .any(|section| section.check.id() == node)
    }
    /// 返回是否需要从后台读取已注册类型；控件中只保存原始名称，不复制 MIME 判型数据库。
    pub fn activate(&self, node: u64) -> liteos_ui::Result<bool> {
        if node == self.toggle.id() {
            let opening = self.body.state()? & state::HIDDEN != 0;
            self.body
                .set_state(if opening { 0 } else { state::HIDDEN })?;
            self.toggle.set_text(if opening {
                "搜索选项 <<"
            } else {
                "搜索选项 >>"
            })?;
        }
        for section in &self.sections {
            if section.check.id() == node {
                section
                    .body
                    .set_state(if section.check.state()? & state::CHECKED != 0 {
                        0
                    } else {
                        state::HIDDEN
                    })?;
            }
        }
        Ok(node == self.sections[1].check.id()
            && self.sections[1].check.state()? & state::CHECKED != 0)
    }
    pub fn set_types(&self, types: &[String]) -> liteos_ui::Result<()> {
        let current = self.kind.text()?;
        let mut labels = vec!["所有文件和文件夹", "文件夹"];
        labels.extend(types.iter().map(String::as_str));
        self.kind.set_tree_rows(
            &labels
                .iter()
                .enumerate()
                .map(|(index, text)| TreeRow {
                    id: index as u64 + 1,
                    parent: 0,
                    text,
                    expanded: false,
                    has_children: false,
                })
                .collect::<Vec<_>>(),
        )?;
        self.kind.set_text(&current)
    }
    pub fn draft(&self) -> liteos_ui::Result<Draft> {
        let active = |index: usize| {
            self.sections[index]
                .check
                .state()
                .map(|state| state & state::CHECKED != 0)
        };
        Ok(Draft {
            date: if active(0)? {
                Some(Date {
                    field: self.field.selection()?.unwrap_or(1),
                    period: self.period.selection()?.unwrap_or(1),
                    amount: self.amount.text()?,
                    from: self.from.text()?,
                    to: self.to.text()?,
                })
            } else {
                None
            },
            kind: if active(1)? {
                Some(self.kind.text()?)
            } else {
                None
            },
            size: if active(2)? {
                Some((self.comparison.selection()?.unwrap_or(1), self.size.text()?))
            } else {
                None
            },
            case_sensitive: self.case_sensitive.state()? & state::CHECKED != 0,
        })
    }
    pub fn restore(&self, draft: &Draft) -> liteos_ui::Result<()> {
        for (index, active) in [
            draft.date.is_some(),
            draft.kind.is_some(),
            draft.size.is_some(),
        ]
        .into_iter()
        .enumerate()
        {
            self.sections[index]
                .check
                .set_state(if active { state::CHECKED } else { 0 })?;
            self.sections[index]
                .body
                .set_state(if active { 0 } else { state::HIDDEN })?;
        }
        if draft.date.is_some()
            || draft.kind.is_some()
            || draft.size.is_some()
            || draft.case_sensitive
        {
            self.body.set_state(0)?;
            self.toggle.set_text("搜索选项 <<")?;
        }
        if let Some(date) = &draft.date {
            self.field.select(Some(date.field))?;
            self.period.select(Some(date.period))?;
            self.selected(self.period.id())?;
            self.amount.set_text(&date.amount)?;
            self.from.set_text(&date.from)?;
            self.to.set_text(&date.to)?;
        }
        if let Some(kind) = &draft.kind {
            self.kind.set_text(kind)?;
        }
        if let Some((comparison, size)) = &draft.size {
            self.comparison.select(Some(*comparison))?;
            self.size.set_text(size)?;
        }
        self.case_sensitive.set_state(if draft.case_sensitive {
            state::CHECKED
        } else {
            0
        })
    }
}
