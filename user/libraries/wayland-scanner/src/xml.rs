//! Wayland 协议 XML 所需的最小 XML 子集：元素、属性、注释、声明；文本节点被忽略。

/// 一个 XML 元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |child| child.name == name)
    }
}

struct Parser<'a> {
    text: &'a str,
    position: usize,
}

/// 解析一个 XML 文档并返回根元素。
///
/// # Errors
///
/// 标签不匹配、属性未加引号、未知实体或文档不以单个根元素构成时返回带字节偏移的描述。
pub fn parse(text: &str) -> Result<Element, String> {
    let mut parser = Parser { text, position: 0 };
    parser.skip_misc()?;
    let root = parser.element()?;
    parser.skip_misc()?;
    if parser.position != text.len() {
        return Err(parser.error("content after the root element"));
    }
    Ok(root)
}

impl Parser<'_> {
    fn error(&self, message: &str) -> String {
        format!("xml offset {}: {message}", self.position)
    }

    fn rest(&self) -> &str {
        &self.text[self.position..]
    }

    fn skip_whitespace(&mut self) {
        let trimmed = self.rest().trim_start();
        self.position = self.text.len() - trimmed.len();
    }

    fn skip_until(&mut self, terminator: &str) -> Result<(), String> {
        let end = self
            .rest()
            .find(terminator)
            .ok_or_else(|| self.error(&format!("missing `{terminator}`")))?;
        self.position += end + terminator.len();
        Ok(())
    }

    /// 跳过空白、`<?...?>` 声明与 `<!-- -->` 注释。
    fn skip_misc(&mut self) -> Result<(), String> {
        loop {
            self.skip_whitespace();
            if self.rest().starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.rest().starts_with("<!--") {
                self.skip_until("-->")?;
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let length = self
            .rest()
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')))
            .unwrap_or_else(|| self.rest().len());
        if length == 0 {
            return Err(self.error("expected a name"));
        }
        let name = self.rest()[..length].to_owned();
        self.position += length;
        Ok(name)
    }

    fn element(&mut self) -> Result<Element, String> {
        if !self.rest().starts_with('<') {
            return Err(self.error("expected `<`"));
        }
        self.position += 1;
        let name = self.name()?;
        let mut attributes = Vec::new();
        loop {
            self.skip_whitespace();
            if self.rest().starts_with("/>") {
                self.position += 2;
                return Ok(Element {
                    name,
                    attributes,
                    children: Vec::new(),
                });
            }
            if self.rest().starts_with('>') {
                self.position += 1;
                break;
            }
            let key = self.name()?;
            self.skip_whitespace();
            if !self.rest().starts_with('=') {
                return Err(self.error("expected `=` after attribute name"));
            }
            self.position += 1;
            self.skip_whitespace();
            let quote = self
                .rest()
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''));
            let quote = quote.ok_or_else(|| self.error("attribute value must be quoted"))?;
            self.position += 1;
            let end = self
                .rest()
                .find(quote)
                .ok_or_else(|| self.error("unterminated attribute"))?;
            let value = self.decode_entities(&self.rest()[..end])?;
            self.position += end + 1;
            attributes.push((key, value));
        }
        let mut children = Vec::new();
        loop {
            // 文本节点（description 正文等）不携带协议语义，直接跳到下一个标记。
            let next = self
                .rest()
                .find('<')
                .ok_or_else(|| self.error(&format!("unclosed <{name}>")))?;
            self.position += next;
            if self.rest().starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest().starts_with("</") {
                self.position += 2;
                let closing = self.name()?;
                self.skip_whitespace();
                if closing != name || !self.rest().starts_with('>') {
                    return Err(self.error(&format!("expected </{name}>")));
                }
                self.position += 1;
                return Ok(Element {
                    name,
                    attributes,
                    children,
                });
            } else {
                children.push(self.element()?);
            }
        }
    }

    fn decode_entities(&self, raw: &str) -> Result<String, String> {
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(start) = rest.find('&') {
            out.push_str(&rest[..start]);
            let end = rest[start..]
                .find(';')
                .ok_or_else(|| self.error("unterminated entity"))?;
            out.push(match &rest[start + 1..start + end] {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                other => return Err(self.error(&format!("unknown entity &{other};"))),
            });
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }
}
