use std::fmt;

use crate::{Rgb, SystemColor, Theme};

/// 主题文本的语法或取值错误，行号从 1 开始。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    UnknownSection(String),
    UnknownKey(String),
    MissingEquals,
    KeyOutsideSection,
    InvalidColor(String),
    InvalidMetric(String),
    InvalidFont(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: ", self.line)?;
        match &self.kind {
            ParseErrorKind::UnknownSection(name) => write!(f, "unknown section [{name}]"),
            ParseErrorKind::UnknownKey(key) => write!(f, "unknown key `{key}`"),
            ParseErrorKind::MissingEquals => write!(f, "expected `key=value`"),
            ParseErrorKind::KeyOutsideSection => write!(f, "key before the first section"),
            ParseErrorKind::InvalidColor(value) => {
                write!(f, "invalid color `{value}`, expected `R G B`")
            }
            ParseErrorKind::InvalidMetric(value) => write!(f, "invalid metric `{value}`"),
            ParseErrorKind::InvalidFont(value) => {
                write!(f, "invalid font `{value}`, expected `face,points,weight`")
            }
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Copy)]
enum Section {
    Colors,
    WindowMetrics,
    Shell,
    Fonts,
    InternetExplorer,
}

/// `face,points,weight`：字体名非空，磅值 1..=72，粗细 1..=1000（LOGFONT 的 lfWeight）。
fn parse_font(value: &str) -> Option<crate::LogicalFont> {
    let mut parts = value.rsplitn(3, ',').map(str::trim);
    let weight = parts
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|w| (1..=1000).contains(w))?;
    let points = parts
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|p| (1..=72).contains(p))?;
    let face = parts.next().filter(|face| !face.is_empty())?;
    Some(crate::LogicalFont {
        face: face.to_owned(),
        points,
        weight,
    })
}

fn parse_color(value: &str) -> Option<Rgb> {
    let mut parts = value.split_whitespace().map(|part| part.parse::<u8>().ok());
    let color = Rgb {
        red: parts.next()??,
        green: parts.next()??,
        blue: parts.next()??,
    };
    parts.next().is_none().then_some(color)
}

pub(crate) fn overlay(mut theme: Theme, text: &str) -> Result<Theme, ParseError> {
    let mut section = None;
    for (index, raw) in text.lines().enumerate() {
        let error = |kind| ParseError {
            line: index + 1,
            kind,
        };
        let content = raw.trim();
        if content.is_empty() || content.starts_with(';') {
            continue;
        }
        if let Some(name) = content
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = Some(match name {
                "Colors" => Section::Colors,
                "WindowMetrics" => Section::WindowMetrics,
                "Shell" => Section::Shell,
                "Fonts" => Section::Fonts,
                "Internet Explorer" => Section::InternetExplorer,
                other => return Err(error(ParseErrorKind::UnknownSection(other.to_owned()))),
            });
            continue;
        }
        let (key, value) = content
            .split_once('=')
            .ok_or_else(|| error(ParseErrorKind::MissingEquals))?;
        let (key, value) = (key.trim(), value.trim());
        let section = section.ok_or_else(|| error(ParseErrorKind::KeyOutsideSection))?;
        let unknown = || error(ParseErrorKind::UnknownKey(key.to_owned()));
        match section {
            Section::Colors => {
                let color = SystemColor::from_name(key).ok_or_else(unknown)?;
                theme.colors[color as usize] = parse_color(value)
                    .ok_or_else(|| error(ParseErrorKind::InvalidColor(value.to_owned())))?;
            }
            Section::InternetExplorer => {
                let slot = match key {
                    "Anchor Color" => &mut theme.anchors.link,
                    "Anchor Color Visited" => &mut theme.anchors.visited,
                    _ => return Err(unknown()),
                };
                // IE 注册表值为 "R,G,B"；与 [Colors] 一样也接受空格分隔。
                *slot = parse_color(&value.replace(',', " "))
                    .ok_or_else(|| error(ParseErrorKind::InvalidColor(value.to_owned())))?;
            }
            Section::Fonts if key == "FontSmoothing" => {
                // 注册表 FontSmoothing 为 "0" 关闭、"2" 开启。
                theme.fonts.smoothing = match value {
                    "0" => false,
                    "2" => true,
                    _ => return Err(error(ParseErrorKind::InvalidFont(value.to_owned()))),
                };
            }
            Section::Fonts => {
                *theme.fonts.slot(key).ok_or_else(unknown)? = parse_font(value)
                    .ok_or_else(|| error(ParseErrorKind::InvalidFont(value.to_owned())))?;
            }
            Section::WindowMetrics | Section::Shell => {
                let slot = match section {
                    Section::WindowMetrics => theme.window.slot(key),
                    _ => theme.shell.slot(key),
                }
                .ok_or_else(unknown)?;
                *slot = value
                    .parse::<i32>()
                    .ok()
                    .filter(|metric| *metric >= 0)
                    .ok_or_else(|| error(ParseErrorKind::InvalidMetric(value.to_owned())))?;
            }
        }
    }
    Ok(theme)
}
