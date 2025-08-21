use alloc::{
    boxed::Box, string::{String, ToString}, vec::Vec, format
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

    /// 获取下一个token
    pub fn next_token(&mut self) -> Option<Token> {
        while self.pos < self.input.len() {
            let current_char = self.input[self.pos];

            match self.state {
                TokenizerState::Data => {
                    if current_char == '<' {
                        self.state = TokenizerState::TagOpen;
                        self.pos += 1;
                    } else {
                        return self.consume_character_token();
                    }
                },

                TokenizerState::TagOpen => {
                    if current_char == '!' {
                        self.pos += 1;
                        return self.handle_markup_declaration();
                    } else if current_char == '/' {
                        self.state = TokenizerState::EndTagOpen;
                        self.pos += 1;
                    } else if current_char.is_ascii_alphabetic() {
                        self.state = TokenizerState::TagName;
                        return self.consume_start_tag();
                    } else {
                        // 无效字符，返回到Data状态
                        self.state = TokenizerState::Data;
                        return Some(Token::Character { data: '<' });
                    }
                },

                TokenizerState::EndTagOpen => {
                    if current_char.is_ascii_alphabetic() {
                        self.state = TokenizerState::TagName;
                        return self.consume_end_tag();
                    } else {
                        // 错误恢复
                        self.state = TokenizerState::Data;
                        return Some(Token::Character { data: '<' });
                    }
                },

                TokenizerState::TagName => {
                    // 这个状态在consume_start_tag/consume_end_tag中处理
                    break;
                },

                TokenizerState::BeforeAttributeName => {
                    if current_char.is_whitespace() {
                        self.pos += 1;
                    } else if current_char == '>' {
                        self.state = TokenizerState::Data;
                        self.pos += 1;
                        return self.emit_current_tag();
                    } else if current_char == '/' {
                        self.state = TokenizerState::SelfClosingStartTag;
                        self.pos += 1;
                    } else {
                        self.state = TokenizerState::AttributeName;
                        return self.consume_attribute();
                    }
                },

                TokenizerState::SelfClosingStartTag => {
                    if current_char == '>' {
                        self.state = TokenizerState::Data;
                        self.pos += 1;
                        return self.emit_self_closing_tag();
                    } else {
                        // 错误，但继续处理属性
                        self.state = TokenizerState::BeforeAttributeName;
                    }
                },

                _ => {
                    // 其他状态的简化处理
                    self.pos += 1;
                }
            }
        }

        // 输入结束
        if self.pos >= self.input.len() {
            Some(Token::EndOfFile)
        } else {
            None
        }
    }

    fn consume_character_token(&mut self) -> Option<Token> {
        let start = self.pos;
        while self.pos < self.input.len() && self.input[self.pos] != '<' {
            self.pos += 1;
        }

        if start < self.pos {
            let text: String = self.input[start..self.pos].iter().collect();
            if !text.trim().is_empty() {
                return Some(Token::Character { data: text.chars().next().unwrap() });
            }
        }

        None
    }

    fn consume_start_tag(&mut self) -> Option<Token> {
        let mut tag_name = String::new();
        let mut attributes = Vec::new();

        // 消费标签名
        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            if ch.is_ascii_alphabetic() || ch.is_ascii_digit() {
                tag_name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            } else if ch.is_whitespace() {
                self.state = TokenizerState::BeforeAttributeName;
                self.pos += 1;
                break;
            } else if ch == '>' {
                self.state = TokenizerState::Data;
                self.pos += 1;
                return Some(Token::StartTag {
                    name: tag_name,
                    attributes,
                    self_closing: false,
                });
            } else if ch == '/' {
                self.state = TokenizerState::SelfClosingStartTag;
                self.pos += 1;
                break;
            } else {
                self.pos += 1;
            }
        }

        // 处理属性
        while self.pos < self.input.len() && self.state != TokenizerState::Data {
            match self.state {
                TokenizerState::BeforeAttributeName => {
                    let ch = self.input[self.pos];
                    if ch.is_whitespace() {
                        self.pos += 1;
                    } else if ch == '>' {
                        self.state = TokenizerState::Data;
                        self.pos += 1;
                        break;
                    } else if ch == '/' {
                        self.state = TokenizerState::SelfClosingStartTag;
                        self.pos += 1;
                    } else {
                        if let Some(attr) = self.consume_attribute_name_value() {
                            attributes.push(attr);
                        }
                    }
                },
                TokenizerState::SelfClosingStartTag => {
                    if self.pos < self.input.len() && self.input[self.pos] == '>' {
                        self.state = TokenizerState::Data;
                        self.pos += 1;
                        return Some(Token::StartTag {
                            name: tag_name,
                            attributes,
                            self_closing: true,
                        });
                    } else {
                        self.state = TokenizerState::BeforeAttributeName;
                    }
                },
                _ => break,
            }
        }

        Some(Token::StartTag {
            name: tag_name,
            attributes,
            self_closing: false,
        })
    }

    fn consume_end_tag(&mut self) -> Option<Token> {
        let mut tag_name = String::new();

        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            if ch.is_ascii_alphabetic() || ch.is_ascii_digit() {
                tag_name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            } else if ch == '>' {
                self.state = TokenizerState::Data;
                self.pos += 1;
                return Some(Token::EndTag {
                    name: tag_name,
                    attributes: Vec::new(),
                });
            } else {
                self.pos += 1;
            }
        }

        Some(Token::EndTag {
            name: tag_name,
            attributes: Vec::new(),
        })
    }

    fn consume_attribute_name_value(&mut self) -> Option<(String, String)> {
        let mut name = String::new();
        let mut value = String::new();

        // 读取属性名
        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            if ch.is_ascii_alphabetic() || ch.is_ascii_digit() || ch == '-' || ch == '_' {
                name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            } else if ch == '=' {
                self.pos += 1;
                break;
            } else if ch.is_whitespace() || ch == '>' || ch == '/' {
                // 属性没有值
                return Some((name, String::new()));
            } else {
                self.pos += 1;
            }
        }

        // 跳过空白
        while self.pos < self.input.len() && self.input[self.pos].is_whitespace() {
            self.pos += 1;
        }

        // 读取属性值
        if self.pos < self.input.len() {
            let quote_char = self.input[self.pos];
            if quote_char == '"' || quote_char == '\'' {
                self.pos += 1; // 跳过开始引号
                while self.pos < self.input.len() {
                    let ch = self.input[self.pos];
                    if ch == quote_char {
                        self.pos += 1; // 跳过结束引号
                        break;
                    } else {
                        value.push(ch);
                        self.pos += 1;
                    }
                }
            } else {
                // 无引号属性值
                while self.pos < self.input.len() {
                    let ch = self.input[self.pos];
                    if ch.is_whitespace() || ch == '>' || ch == '/' {
                        break;
                    } else {
                        value.push(ch);
                        self.pos += 1;
                    }
                }
            }
        }

        Some((name, value))
    }

    fn handle_markup_declaration(&mut self) -> Option<Token> {
        // 检查DOCTYPE
        if self.check_sequence("DOCTYPE") {
            self.pos += 7; // 跳过"DOCTYPE"
            return self.consume_doctype();
        }

        // 检查注释
        if self.check_sequence("--") {
            self.pos += 2; // 跳过"--"
            return self.consume_comment();
        }

        // 其他标记声明的简化处理
        self.state = TokenizerState::Data;
        Some(Token::Character { data: '<' })
    }

    fn check_sequence(&self, sequence: &str) -> bool {
        let seq_chars: Vec<char> = sequence.chars().collect();
        if self.pos + seq_chars.len() > self.input.len() {
            return false;
        }

        for (i, &expected) in seq_chars.iter().enumerate() {
            if self.input[self.pos + i].to_ascii_uppercase() != expected.to_ascii_uppercase() {
                return false;
            }
        }

        true
    }

    fn consume_doctype(&mut self) -> Option<Token> {
        // 跳过空白
        while self.pos < self.input.len() && self.input[self.pos].is_whitespace() {
            self.pos += 1;
        }

        let mut name = String::new();
        while self.pos < self.input.len() {
            let ch = self.input[self.pos];
            if ch == '>' {
                self.pos += 1;
                self.state = TokenizerState::Data;
                break;
            } else if ch.is_whitespace() {
                self.pos += 1;
                break;
            } else {
                name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            }
        }

        // 跳过到>
        while self.pos < self.input.len() && self.input[self.pos] != '>' {
            self.pos += 1;
        }
        if self.pos < self.input.len() {
            self.pos += 1; // 跳过>
        }

        self.state = TokenizerState::Data;
        Some(Token::Doctype {
            name: if name.is_empty() { None } else { Some(name) },
            public_id: None,
            system_id: None,
            force_quirks: false,
        })
    }

    fn consume_comment(&mut self) -> Option<Token> {
        let mut comment_data = String::new();

        while self.pos + 1 < self.input.len() {
            if self.input[self.pos] == '-' && self.input[self.pos + 1] == '-' {
                self.pos += 2;
                // 检查是否是结束
                if self.pos < self.input.len() && self.input[self.pos] == '>' {
                    self.pos += 1;
                    self.state = TokenizerState::Data;
                    return Some(Token::Comment { data: comment_data });
                }
            } else {
                comment_data.push(self.input[self.pos]);
                self.pos += 1;
            }
        }

        // 没有找到结束，但输入结束了
        self.state = TokenizerState::Data;
        Some(Token::Comment { data: comment_data })
    }

    fn emit_current_tag(&mut self) -> Option<Token> {
        // 这个方法在实际实现中需要保存当前标签状态
        // 这里简化处理
        None
    }

    fn emit_self_closing_tag(&mut self) -> Option<Token> {
        // 这个方法在实际实现中需要保存当前标签状态
        // 这里简化处理
        None
    }

    fn consume_attribute(&mut self) -> Option<Token> {
        // 这个方法在实际实现中需要处理属性状态
        // 这里简化处理
        None
    }
}

impl HtmlTokenizer for Tokenizer {
    fn tokenize(&mut self, input: &str) -> TokenStream {
        *self = Tokenizer::new(input);
        let mut tokens = Vec::new();

        while let Some(token) = self.next_token() {
            match token {
                Token::EndOfFile => break,
                _ => tokens.push(token),
            }
        }

        tokens
    }

    fn next_token(&mut self) -> Option<Token> {
        self.next_token()
    }

    fn reset(&mut self) {
        self.pos = 0;
        self.state = TokenizerState::Data;
    }
}

pub struct TreeBuilder {
    stack: Vec<DomNode>,
    insertion_mode: InsertionMode,
    document: Option<DomNode>,
    head_element: Option<DomNode>,
    form_element: Option<DomNode>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InsertionMode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InSelect,
    InSelectInTable,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            insertion_mode: InsertionMode::Initial,
            document: None,
            head_element: None,
            form_element: None,
        }
    }

    pub fn build_from_tokens(&mut self, tokens: TokenStream) -> Result<DomNode, ParseError> {
        // 创建文档根节点
        self.document = Some(DomNode::elem("document"));

        for token in tokens {
            self.process_token(token)?;
        }

        // 返回document或者html元素
        if let Some(doc) = &self.document {
            if !doc.children.is_empty() {
                Ok(doc.children[0].clone())
            } else {
                Ok(doc.clone())
            }
        } else {
            Err(ParseError::UnexpectedEndOfInput)
        }
    }

    fn process_token(&mut self, token: Token) -> Result<(), ParseError> {
        match self.insertion_mode {
            InsertionMode::Initial => self.handle_initial_mode(token),
            InsertionMode::BeforeHtml => self.handle_before_html_mode(token),
            InsertionMode::BeforeHead => self.handle_before_head_mode(token),
            InsertionMode::InHead => self.handle_in_head_mode(token),
            InsertionMode::AfterHead => self.handle_after_head_mode(token),
            InsertionMode::InBody => self.handle_in_body_mode(token),
            _ => self.handle_in_body_mode(token), // 简化：其他模式按body处理
        }
    }

    fn handle_initial_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::Doctype { .. } => {
                // DOCTYPE处理，设置quirks模式等
                self.insertion_mode = InsertionMode::BeforeHtml;
                Ok(())
            },
            Token::Character { data } if data.is_whitespace() => {
                // 忽略空白字符
                Ok(())
            },
            _ => {
                // 其他token，切换到BeforeHtml模式并重新处理
                self.insertion_mode = InsertionMode::BeforeHtml;
                self.process_token(token)
            }
        }
    }

    fn handle_before_html_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::StartTag { name, attributes, .. } if name == "html" => {
                let mut html_elem = DomNode::elem("html");
                html_elem.set_attributes(attributes);

                if let Some(ref mut doc) = self.document {
                    doc.children.push(html_elem.clone());
                }

                self.stack.push(html_elem);
                self.insertion_mode = InsertionMode::BeforeHead;
                Ok(())
            },
            Token::Character { data } if data.is_whitespace() => {
                // 忽略空白
                Ok(())
            },
            _ => {
                // 隐式创建html元素
                let html_elem = DomNode::elem("html");
                if let Some(ref mut doc) = self.document {
                    doc.children.push(html_elem.clone());
                }
                self.stack.push(html_elem);
                self.insertion_mode = InsertionMode::BeforeHead;
                self.process_token(token)
            }
        }
    }

    fn handle_before_head_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::StartTag { name, attributes, .. } if name == "head" => {
                let mut head_elem = DomNode::elem("head");
                head_elem.set_attributes(attributes);

                self.head_element = Some(head_elem.clone());
                self.insert_element(head_elem);
                self.insertion_mode = InsertionMode::InHead;
                Ok(())
            },
            Token::Character { data } if data.is_whitespace() => {
                Ok(())
            },
            _ => {
                // 隐式创建head元素
                let head_elem = DomNode::elem("head");
                self.head_element = Some(head_elem.clone());
                self.insert_element(head_elem);
                self.insertion_mode = InsertionMode::InHead;
                self.process_token(token)
            }
        }
    }

    fn handle_in_head_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::StartTag { name, attributes, self_closing } => {
                match name.as_str() {
                    "meta" | "link" | "base" | "title" | "style" | "script" => {
                        let mut elem = DomNode::elem(&name);
                        elem.set_attributes(attributes);
                        self.insert_element(elem);

                        if self_closing || matches!(name.as_str(), "meta" | "link" | "base") {
                            self.stack.pop(); // 立即关闭自闭合元素
                        }
                        Ok(())
                    },
                    _ => {
                        // 其他元素，离开head模式
                        self.insertion_mode = InsertionMode::AfterHead;
                        self.process_token(Token::StartTag { name, attributes, self_closing })
                    }
                }
            },
            Token::EndTag { name, .. } if name == "head" => {
                self.stack.pop(); // 弹出head元素
                self.insertion_mode = InsertionMode::AfterHead;
                Ok(())
            },
            Token::Character { data } if data.is_whitespace() => {
                Ok(())
            },
            _ => {
                // 其他token，隐式关闭head
                self.stack.pop();
                self.insertion_mode = InsertionMode::AfterHead;
                self.process_token(token)
            }
        }
    }

    fn handle_after_head_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::StartTag { name, attributes, .. } if name == "body" => {
                let mut body_elem = DomNode::elem("body");
                body_elem.set_attributes(attributes);
                self.insert_element(body_elem);
                self.insertion_mode = InsertionMode::InBody;
                Ok(())
            },
            Token::Character { data } if data.is_whitespace() => {
                Ok(())
            },
            _ => {
                // 隐式创建body元素
                let body_elem = DomNode::elem("body");
                self.insert_element(body_elem);
                self.insertion_mode = InsertionMode::InBody;
                self.process_token(token)
            }
        }
    }

    fn handle_in_body_mode(&mut self, token: Token) -> Result<(), ParseError> {
        match token {
            Token::StartTag { name, attributes, self_closing } => {
                let mut elem = DomNode::elem(&name);
                elem.set_attributes(attributes);

                // 检查是否是void元素
                let is_void = matches!(name.as_str(),
                    "area" | "base" | "br" | "col" | "embed" | "hr" | "img" |
                    "input" | "link" | "meta" | "param" | "source" | "track" | "wbr"
                );

                self.insert_element(elem);

                if self_closing || is_void {
                    self.stack.pop(); // 立即关闭
                }

                Ok(())
            },

            Token::EndTag { name, .. } => {
                // 查找匹配的开始标签并关闭
                let mut found_index = None;
                for (i, node) in self.stack.iter().enumerate().rev() {
                    if node.tag == name {
                        found_index = Some(i);
                        break;
                    }
                }

                if let Some(index) = found_index {
                    // 关闭从找到的元素到栈顶的所有元素
                    self.stack.truncate(index);
                }

                Ok(())
            },

            Token::Character { data } => {
                self.insert_text(&data.to_string());
                Ok(())
            },

            Token::Comment { data } => {
                // 注释节点处理（可选）
                println!("[TreeBuilder] Comment: {}", data);
                Ok(())
            },

            _ => Ok(())
        }
    }

    fn insert_element(&mut self, element: DomNode) {
        if let Some(current) = self.stack.last_mut() {
            current.children.push(element.clone());
        }
        self.stack.push(element);
    }

    fn insert_text(&mut self, text: &str) {
        if let Some(current) = self.stack.last_mut() {
            let text_node = DomNode::text(text);
            current.children.push(text_node);
        }
    }

    fn current_node(&self) -> Option<&DomNode> {
        self.stack.last()
    }

    fn current_node_mut(&mut self) -> Option<&mut DomNode> {
        self.stack.last_mut()
    }
}

impl HtmlTreeBuilder for TreeBuilder {
    fn build(&mut self, tokens: TokenStream) -> ParseResult<DomNode> {
        self.build_from_tokens(tokens)
    }

    fn process_token(&mut self, token: Token) -> Result<(), ParseError> {
        self.process_token(token)
    }

    fn finish(&mut self) -> DomNode {
        if let Some(doc) = &self.document {
            if !doc.children.is_empty() {
                doc.children[0].clone()
            } else {
                doc.clone()
            }
        } else {
            DomNode::elem("html")
        }
    }
}

/// 完整的HTML5解析器实现
pub struct Html5Parser {
    tokenizer: Tokenizer,
    tree_builder: TreeBuilder,
}

impl Html5Parser {
    pub fn new() -> Self {
        Self {
            tokenizer: Tokenizer::new(""),
            tree_builder: TreeBuilder::new(),
        }
    }
}

impl HtmlParser for Html5Parser {
    fn parse(&mut self, input: &str) -> ParseResult<DomNode> {
        let tokens = self.tokenizer.tokenize(input);
        self.tree_builder.build(tokens)
    }

    fn parse_fragment(&mut self, input: &str, _context: &str) -> ParseResult<Vec<DomNode>> {
        let tokens = self.tokenizer.tokenize(input);
        let document = self.tree_builder.build(tokens)?;
        Ok(document.children)
    }
}

impl DomNodeBuilder for Html5Parser {
    fn create_element(&self, tag_name: &str, attributes: Vec<(String, String)>) -> DomNode {
        let mut elem = DomNode::elem(tag_name);
        elem.set_attributes(attributes);
        elem
    }

    fn create_text_node(&self, text: &str) -> DomNode {
        DomNode::text(text)
    }

    fn create_comment_node(&self, comment: &str) -> DomNode {
        // 简化实现：将注释作为文本节点
        DomNode::text(&format!("<!-- {} -->", comment))
    }
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
