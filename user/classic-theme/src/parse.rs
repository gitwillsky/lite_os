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
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Copy)]
enum Section {
    Colors,
    WindowMetrics,
    Shell,
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
