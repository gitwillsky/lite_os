use alloc::{
    boxed::Box, string::{String, ToString}, vec::Vec
};

//======================================================================
// HTML5解析器标准接口定义
//======================================================================

/// HTML5解析器核心接口 - 将HTML字符串解析为DOM树
pub trait HtmlParser {
    fn parse(&mut self, input: &str) -> ParseResult<DomNode>;
    fn parse_fragment(&mut self, input: &str, context: &str) -> ParseResult<Vec<DomNode>>;
}

/// 标记化器接口 - 将HTML字符串转换为Token流
pub trait HtmlTokenizer {
    fn tokenize(&mut self, input: &str) -> TokenStream;
    fn next_token(&mut self) -> Option<Token>;
    fn reset(&mut self);
}

/// 树构建器接口 - 从Token流构建DOM树
pub trait HtmlTreeBuilder {
    fn build(&mut self, tokens: TokenStream) -> ParseResult<DomNode>;
    fn process_token(&mut self, token: Token) -> Result<(), ParseError>;
    fn finish(&mut self) -> DomNode;
}

/// DOM节点操作接口
pub trait DomNodeBuilder {
    fn create_element(&self, tag_name: &str, attributes: Vec<(String, String)>) -> DomNode;
    fn create_text_node(&self, text: &str) -> DomNode;
    fn create_comment_node(&self, comment: &str) -> DomNode;
}

/// 解析结果类型
pub type ParseResult<T> = Result<T, ParseError>;

/// Token流类型
pub type TokenStream = Vec<Token>;

/// HTML解析错误
#[derive(Debug, Clone)]
pub enum ParseError {
    UnexpectedToken { expected: String, found: String },
    UnexpectedEndOfInput,
    InvalidCharacter { character: char, position: usize },
    InvalidAttribute { name: String, value: String },
    InvalidTagName { name: String },
    NestedError { source: Box<ParseError> },
}

//======================================================================
// HTML5标准实现
//======================================================================

// HTML5标准的Token类型
#[derive(Clone, Debug)]
pub enum Token {
    Doctype {
        name: Option<String>,
        public_id: Option<String>,
        system_id: Option<String>,
        force_quirks: bool,
    },
    StartTag {
        name: String,
        attributes: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
        attributes: Vec<(String, String)>,
    },
    Comment {
        data: String,
    },
    Character {
        data: char,
    },
    EndOfFile,
}

// HTML5解析状态
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenizerState {
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentEndDash,
    CommentEnd,
    MarkupDeclarationOpen,
    Doctype,
    DoctypeName,
    AfterDoctypeName,
}

#[derive(Clone)]
pub struct DomNode {
    pub tag: String, // 空字符串表示文本节点
    pub id: Option<String>,
    pub class_list: Vec<String>,
    pub inline_style: Option<String>,
    pub src: Option<String>,      // for <img> 或 <link href>
    pub rel: Option<String>,      // for <link rel>
    pub attr_width: Option<i32>,  // width attribute in px
    pub attr_height: Option<i32>, // height attribute in px
    pub text: Option<String>,
    pub children: Vec<DomNode>,
    pub attributes: Vec<(String, String)>, // 完整属性支持
}

impl DomNode {
    pub fn text(text: &str) -> Self {
        Self {
            tag: String::new(),
            id: None,
            class_list: Vec::new(),
            inline_style: None,
            src: None,
            rel: None,
            attr_width: None,
            attr_height: None,
            text: Some(text.to_string()),
            children: Vec::new(),
            attributes: Vec::new(),
        }
    }

    pub fn elem(tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            id: None,
            class_list: Vec::new(),
            inline_style: None,
            src: None,
            rel: None,
            attr_width: None,
            attr_height: None,
            text: None,
            children: Vec::new(),
            attributes: Vec::new(),
        }
    }

    fn set_attributes(&mut self, attributes: Vec<(String, String)>) {
        self.attributes = attributes.clone();

        // 设置常用属性的快速访问
        for (name, value) in &attributes {
            match name.as_str() {
                "id" => self.id = Some(value.clone()),
                "class" => {
                    self.class_list = value.split_whitespace().map(|s| s.to_string()).collect()
                }
                "style" => self.inline_style = Some(value.clone()),
                "src" | "href" => self.src = Some(value.clone()),
                "rel" => self.rel = Some(value.clone()),
                "width" => self.attr_width = value.parse().ok(),
                "height" => self.attr_height = value.parse().ok(),
                _ => {}
            }
        }
    }
}

// HTML5标准标记化器和树构建器（预留接口，待完整实现）
pub struct Tokenizer {
    input: Vec<char>,
    pos: usize,
    state: TokenizerState,
}

impl Tokenizer {
    pub fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            pos: 0,
            state: TokenizerState::Data,
        }
    }
}

pub struct TreeBuilder {
    stack: Vec<DomNode>,
}

// 工具函数
fn is_void_element(tag_name: &str) -> bool {
    matches!(
        tag_name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

// 增强版HTML解析器，支持更多HTML5特性
pub fn parse_document(input: &str) -> DomNode {
    println!("[webcore::html] Starting HTML parse, input length: {}", input.len());
    println!("[webcore::html] Input preview: {:?}", &input[..input.len().min(100)]);

    let mut pos = 0;
    let chars: Vec<char> = input.chars().collect();

    fn skip_whitespace(chars: &[char], pos: &mut usize) {
        while *pos < chars.len() && chars[*pos].is_whitespace() {
            *pos += 1;
        }
    }

    fn read_until_char(chars: &[char], pos: &mut usize, delimiter: char) -> String {
        let start = *pos;
        while *pos < chars.len() && chars[*pos] != delimiter {
            *pos += 1;
        }
        let result: String = chars[start..*pos].iter().collect();
        if *pos < chars.len() && chars[*pos] == delimiter {
            *pos += 1;
        }
        result
    }

    fn read_tag_name(chars: &[char], pos: &mut usize) -> String {
        let start = *pos;
        while *pos < chars.len()
            && (chars[*pos].is_alphanumeric() || chars[*pos] == '-' || chars[*pos] == '_')
        {
            *pos += 1;
        }
        chars[start..*pos].iter().collect::<String>().to_lowercase()
    }

    fn parse_attributes(chars: &[char], pos: &mut usize) -> Vec<(String, String)> {
        let mut attributes = Vec::new();

        loop {
            skip_whitespace(chars, pos);
            if *pos >= chars.len() || chars[*pos] == '>' || chars[*pos] == '/' {
                break;
            }

            // 读取属性名
            let attr_name = read_tag_name(chars, pos);
            if attr_name.is_empty() {
                break;
            }

            let mut attr_value = String::new();
            skip_whitespace(chars, pos);

            // 检查是否有属性值
            if *pos < chars.len() && chars[*pos] == '=' {
                *pos += 1; // 跳过 '='
                skip_whitespace(chars, pos);

                if *pos < chars.len() {
                    match chars[*pos] {
                        '"' => {
                            *pos += 1;
                            attr_value = read_until_char(chars, pos, '"');
                        }
                        '\'' => {
                            *pos += 1;
                            attr_value = read_until_char(chars, pos, '\'');
                        }
                        _ => {
                            // 无引号属性值
                            let start = *pos;
                            while *pos < chars.len()
                                && !chars[*pos].is_whitespace()
                                && chars[*pos] != '>'
                            {
                                *pos += 1;
                            }
                            attr_value = chars[start..*pos].iter().collect();
                        }
                    }
                }
            }

            attributes.push((attr_name, attr_value));
        }

        attributes
    }

    fn parse_element(chars: &[char], pos: &mut usize) -> Option<DomNode> {
        skip_whitespace(chars, pos);

        if *pos >= chars.len() {
            return None;
        }

        // 处理文本内容
        if chars[*pos] != '<' {
            let start = *pos;
            while *pos < chars.len() && chars[*pos] != '<' {
                *pos += 1;
            }
            let text = chars[start..*pos].iter().collect::<String>();
            let text = text.trim();
            if !text.is_empty() {
                println!("[webcore::html] Found text content: '{}'", text);
                return Some(DomNode::text(text));
            } else {
                return None;
            }
        }

        // 处理标签
        if *pos + 1 < chars.len() && chars[*pos] == '<' {
            *pos += 1; // 跳过 '<'

            // 处理结束标签
            if *pos < chars.len() && chars[*pos] == '/' {
                return None; // 结束标签由上层处理
            }

            // 处理注释
            if *pos + 3 < chars.len()
                && chars[*pos] == '!'
                && chars[*pos + 1] == '-'
                && chars[*pos + 2] == '-'
            {
                *pos += 3;
                // 跳过注释内容直到 -->
                while *pos + 2 < chars.len() {
                    if chars[*pos] == '-' && chars[*pos + 1] == '-' && chars[*pos + 2] == '>' {
                        *pos += 3;
                        break;
                    }
                    *pos += 1;
                }
                return None; // 忽略注释
            }

            // 处理DOCTYPE
            if *pos + 7 <= chars.len()
                && chars[*pos..*pos + 7]
                    .iter()
                    .collect::<String>()
                    .to_lowercase()
                    == "doctype"
            {
                println!("[webcore::html] Found DOCTYPE declaration");
                // 回退到 '<' 位置
                *pos -= 1;
                // 跳过整个DOCTYPE声明
                while *pos < chars.len() && chars[*pos] != '>' {
                    *pos += 1;
                }
                if *pos < chars.len() {
                    *pos += 1; // 跳过 '>'
                }
                println!("[webcore::html] Skipped DOCTYPE, position now: {}", *pos);
                return None; // 忽略DOCTYPE，但继续解析
            }

            // 读取标签名
            let tag_name = read_tag_name(chars, pos);
            println!("[webcore::html] Found tag: '{}'", tag_name);
            if tag_name.is_empty() {
                println!("[webcore::html] Empty tag name, skipping");
                return None;
            }

            // 解析属性
            let attributes = parse_attributes(chars, pos);

            // 检查自闭合标签
            let mut self_closing = false;
            skip_whitespace(chars, pos);
            if *pos < chars.len() && chars[*pos] == '/' {
                self_closing = true;
                *pos += 1;
            }

            // 跳过 '>'
            if *pos < chars.len() && chars[*pos] == '>' {
                *pos += 1;
            }

            // 创建元素节点
            let mut element = DomNode::elem(&tag_name);
            element.set_attributes(attributes);

            // 如果是void元素或自闭合，直接返回
            if self_closing || is_void_element(&tag_name) {
                return Some(element);
            }

            // 解析子元素
            loop {
                skip_whitespace(chars, pos);
                if *pos >= chars.len() {
                    break;
                }

                // 检查结束标签
                if *pos + 1 < chars.len() && chars[*pos] == '<' && chars[*pos + 1] == '/' {
                    let temp_pos = *pos + 2; // 跳过 '</'
                    let mut end_pos = temp_pos;
                    let end_tag = read_tag_name(chars, &mut end_pos);

                    println!("[webcore::html] Found end tag: '{}' for current '{}'", end_tag, tag_name);

                    if end_tag == tag_name {
                        // 找到匹配的结束标签，更新位置并结束
                        *pos = end_pos;
                        // 跳过到 '>'
                        while *pos < chars.len() && chars[*pos] != '>' {
                            *pos += 1;
                        }
                        if *pos < chars.len() {
                            *pos += 1; // 跳过 '>'
                        }
                        println!("[webcore::html] Closed tag '{}' at position {}", tag_name, *pos);
                        break;
                    }
                }

                if let Some(child) = parse_element(chars, pos) {
                    if child.tag.is_empty() && child.text.is_some() {
                        println!("[webcore::html] Adding text node '{}' to '{}'", child.text.as_ref().unwrap(), tag_name);
                    } else {
                        println!("[webcore::html] Adding child element '{}' to '{}'", child.tag, tag_name);
                    }
                    element.children.push(child);
                } else {
                    // 如果无法解析子元素，检查是否有文本内容
                    let start = *pos;
                    while *pos < chars.len() && chars[*pos] != '<' {
                        *pos += 1;
                    }
                    if start < *pos {
                        let text = chars[start..*pos].iter().collect::<String>();
                        let text = text.trim();
                        if !text.is_empty() {
                            println!("[webcore::html] Adding text '{}' to '{}'", text, tag_name);
                            let text_node = DomNode::text(text);
                            element.children.push(text_node);
                        }
                    }
                    if start == *pos {
                        break; // 避免无限循环
                    }
                }
            }

            Some(element)
        } else {
            None
        }
    }

    // 寻找根HTML元素，如果没有就创建
    let mut document = DomNode::elem("html");
    let mut found_html_root = false;

    // 解析所有顶级元素
    while pos < chars.len() {
        skip_whitespace(&chars, &mut pos);
        if pos >= chars.len() {
            break;
        }

        if let Some(element) = parse_element(&chars, &mut pos) {
            println!("[webcore::html] Parsed top-level element: '{}' with {} children", element.tag, element.children.len());

            // 如果找到html根元素，使用它作为文档根
            if element.tag == "html" && !found_html_root {
                document = element;
                found_html_root = true;
                println!("[webcore::html] Using parsed html element as document root");
            } else if !element.tag.is_empty() {
                // 其他非空元素添加到文档
                document.children.push(element);
            }
        } else {
            // 如果没有解析到元素，手动前进位置避免无限循环
            let old_pos = pos;
            skip_whitespace(&chars, &mut pos);
            if pos == old_pos && pos < chars.len() {
                pos += 1; // 强制前进
            }
        }
    }

    println!(
        "[webcore::html] Enhanced HTML parsing completed, {} children",
        document.children.len()
    );

    // 调试输出DOM结构
    print_dom_tree(&document, 0);

    document
}

// 调试工具：打印DOM树结构
fn print_dom_tree(node: &DomNode, depth: usize) {
    let indent = "  ".repeat(depth);
    if node.tag.is_empty() && node.text.is_some() {
        // 文本节点
        if let Some(ref text) = node.text {
            println!("{}[TEXT]: \"{}\"", indent, text.trim());
        }
    } else if node.tag.is_empty() {
        // 空节点（应该避免）
        println!("{}[EMPTY_NODE] children={}", indent, node.children.len());
        for child in &node.children {
            print_dom_tree(child, depth + 1);
        }
    } else {
        // 元素节点
        println!("{}[{}] id={:?} class={:?} children={}",
                 indent, node.tag, node.id, node.class_list, node.children.len());
        for child in &node.children {
            print_dom_tree(child, depth + 1);
        }
    }
}
