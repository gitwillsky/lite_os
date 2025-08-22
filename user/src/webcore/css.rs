use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{cmp::Ordering, fmt};

//==============================================================================
// 核心接口定义 (Core Interfaces)
//==============================================================================

/// CSS值解析器接口
pub trait CSSValueParser<T> {
    /// 解析CSS值
    fn parse(&self, input: &str, context: &ComputationContext) -> Result<T, ParseError>;

    /// 验证值是否有效
    fn validate(&self, value: &T) -> bool;
}

/// CSS值计算器接口
pub trait CSSValueComputer<T> {
    /// 计算相对值到绝对值
    fn compute(&self, value: &T, context: &ComputationContext) -> T;
}

/// 选择器匹配器接口
pub trait SelectorMatcher {
    /// 检查选择器是否匹配元素
    fn matches(&self, selector: &Selector, element: &dyn Element) -> bool;

    /// 计算选择器特异性
    fn specificity(&self, selector: &Selector) -> Specificity;
}

/// CSS解析器接口
pub trait CSSParser {
    /// 解析样式表
    fn parse_stylesheet(&self, input: &str) -> Result<StyleSheet, ParseError>;

    /// 解析选择器
    fn parse_selector(&self, input: &str) -> Result<Selector, ParseError>;

    /// 解析声明
    fn parse_declaration(&self, property: &str, value: &str) -> Result<Declaration, ParseError>;
}

/// 层叠计算器接口
pub trait CascadeCalculator {
    /// 计算层叠后的样式
    fn cascade(&self, rules: &[&Rule], element: &dyn Element) -> Vec<Declaration>;
}

/// 元素接口（由DOM提供）
pub trait Element {
    fn tag_name(&self) -> Option<&str>;
    fn id(&self) -> Option<&str>;
    fn classes(&self) -> &[String];
    fn parent(&self) -> Option<&dyn Element>;
    fn index(&self) -> usize;

    // 扩展：属性访问
    fn get_attribute(&self, name: &str) -> Option<&str>;
    fn has_attribute(&self, name: &str) -> bool;
    fn attributes(&self) -> &[(String, String)];

    // 扩展：兄弟元素访问
    fn previous_sibling(&self) -> Option<&dyn Element>;
    fn next_sibling(&self) -> Option<&dyn Element>;
    fn first_child(&self) -> Option<&dyn Element>;
    fn last_child(&self) -> Option<&dyn Element>;
    fn children(&self) -> Vec<&dyn Element>;

    // 扩展：状态访问（伪类支持）
    fn is_hover(&self) -> bool {
        false
    }
    fn is_active(&self) -> bool {
        false
    }
    fn is_focus(&self) -> bool {
        false
    }
    fn is_visited(&self) -> bool {
        false
    }
    fn is_link(&self) -> bool {
        false
    }
    fn is_checked(&self) -> bool {
        false
    }
    fn is_disabled(&self) -> bool {
        false
    }
    fn is_enabled(&self) -> bool {
        !self.is_disabled()
    }
    fn is_first_child(&self) -> bool {
        if let Some(_parent) = self.parent() {
            // 简化实现：使用索引比较而不是指针比较
            self.index() == 0
        } else {
            true // 根元素是第一个子元素
        }
    }
    fn is_last_child(&self) -> bool {
        if let Some(_parent) = self.parent() {
            let siblings = _parent.children();
            if !siblings.is_empty() {
                self.index() == siblings.len() - 1
            } else {
                true
            }
        } else {
            true // 根元素是最后一个子元素
        }
    }
    fn is_only_child(&self) -> bool {
        if let Some(parent) = self.parent() {
            parent.children().len() == 1
        } else {
            true
        }
    }
    fn is_nth_child(&self, n: usize) -> bool {
        if let Some(_parent) = self.parent() {
            // 使用索引比较（从1开始计数）
            self.index() + 1 == n
        } else {
            n == 1
        }
    }
}

//==============================================================================
// 基础数据类型 (Basic Data Types)
//==============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: &'static str,
    pub position: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CSS Parse Error at {}: {}", self.position, self.message)
    }
}

/// CSS颜色值 - 符合CSS 2.1标准
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }

    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub fn from_u32(value: u32) -> Self {
        Self {
            a: ((value >> 24) & 0xFF) as u8,
            r: ((value >> 16) & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: (value & 0xFF) as u8,
        }
    }
}

/// CSS长度值
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// 像素值
    Px(f32),
    /// em单位 (相对于字体大小)
    Em(f32),
    /// ex单位 (相对于字符x高度)
    Ex(f32),
    /// 英寸
    In(f32),
    /// 厘米
    Cm(f32),
    /// 毫米
    Mm(f32),
    /// 点 (1/72英寸)
    Pt(f32),
    /// pica (12点)
    Pc(f32),
    /// 百分比
    Percent(f32),
}

impl Default for Length {
    fn default() -> Self {
        Length::Px(0.0)
    }
}

/// CSS显示类型 - 符合CSS 2.1
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Display {
    None,
    Inline,
    Block,
    Flex,
    ListItem,
    InlineBlock,
    Table,
    InlineTable,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableColumnGroup,
    TableColumn,
    TableCell,
    TableCaption,
}

impl Default for Display {
    fn default() -> Self {
        Display::Inline
    }
}

/// CSS定位类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
}

impl Default for Position {
    fn default() -> Self {
        Position::Static
    }
}

/// CSS浮动
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Float {
    None,
    Left,
    Right,
}

impl Default for Float {
    fn default() -> Self {
        Float::None
    }
}

/// CSS清除
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

impl Default for Clear {
    fn default() -> Self {
        Clear::None
    }
}

/// CSS可见性
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    Visible,
    Hidden,
    Collapse,
}

impl Default for Visibility {
    fn default() -> Self {
        Visibility::Visible
    }
}

/// CSS溢出处理
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overflow {
    Visible,
    Hidden,
    Scroll,
    Auto,
}

impl Default for Overflow {
    fn default() -> Self {
        Overflow::Visible
    }
}

/// 字体样式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FontStyle {
    Normal,
    Italic,
    Oblique,
}

impl Default for FontStyle {
    fn default() -> Self {
        FontStyle::Normal
    }
}

/// 字体粗细
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FontWeight {
    Normal, // 400
    Bold,   // 700
    Bolder,
    Lighter,
    Weight(u16), // 100-900
}

impl Default for FontWeight {
    fn default() -> Self {
        FontWeight::Normal
    }
}

impl FontWeight {
    pub fn to_numeric(&self) -> u16 {
        match self {
            FontWeight::Normal => 400,
            FontWeight::Bold => 700,
            FontWeight::Weight(w) => *w,
            FontWeight::Bolder => 700,  // 简化处理
            FontWeight::Lighter => 300, // 简化处理
        }
    }
}

/// 文本装饰
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextDecoration {
    None,
    Underline,
    Overline,
    LineThrough,
    Blink,
}

impl Default for TextDecoration {
    fn default() -> Self {
        TextDecoration::None
    }
}

/// 文本对齐
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justify,
}

impl Default for TextAlign {
    fn default() -> Self {
        TextAlign::Left
    }
}

/// 垂直对齐
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VerticalAlign {
    Baseline,
    Sub,
    Super,
    Top,
    TextTop,
    Middle,
    Bottom,
    TextBottom,
    Length(Length),
    Percent(f32),
}

impl Default for VerticalAlign {
    fn default() -> Self {
        VerticalAlign::Baseline
    }
}

/// 边框样式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderStyle {
    None,
    Hidden,
    Dotted,
    Dashed,
    Solid,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl Default for BorderStyle {
    fn default() -> Self {
        BorderStyle::None
    }
}

/// 盒模型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxSizing {
    ContentBox,
    BorderBox,
}

impl Default for BoxSizing {
    fn default() -> Self {
        BoxSizing::ContentBox
    }
}

/// 背景重复
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackgroundRepeat {
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

impl Default for BackgroundRepeat {
    fn default() -> Self {
        BackgroundRepeat::Repeat
    }
}

/// 背景附着
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackgroundAttachment {
    Scroll,
    Fixed,
}

impl Default for BackgroundAttachment {
    fn default() -> Self {
        BackgroundAttachment::Scroll
    }
}

/// 背景位置
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundPosition {
    pub x: Length,
    pub y: Length,
}

impl Default for BackgroundPosition {
    fn default() -> Self {
        Self {
            x: Length::Percent(0.0),
            y: Length::Percent(0.0),
        }
    }
}

/// 文本转换
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextTransform {
    None,
    Capitalize,
    Uppercase,
    Lowercase,
}

impl Default for TextTransform {
    fn default() -> Self {
        TextTransform::None
    }
}

/// 表格布局
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableLayout {
    Auto,
    Fixed,
}

impl Default for TableLayout {
    fn default() -> Self {
        TableLayout::Auto
    }
}

/// 边框合并
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BorderCollapse {
    Separate,
    Collapse,
}

impl Default for BorderCollapse {
    fn default() -> Self {
        BorderCollapse::Separate
    }
}

/// 空单元格
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmptyCells {
    Show,
    Hide,
}

impl Default for EmptyCells {
    fn default() -> Self {
        EmptyCells::Show
    }
}

/// 标题位置
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptionSide {
    Top,
    Bottom,
    Left,
    Right,
}

impl Default for CaptionSide {
    fn default() -> Self {
        CaptionSide::Top
    }
}

/// 列表样式类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListStyleType {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerRoman,
    UpperRoman,
    LowerGreek,
    LowerAlpha,
    UpperAlpha,
    LowerLatin,
    UpperLatin,
}

impl Default for ListStyleType {
    fn default() -> Self {
        ListStyleType::Disc
    }
}

/// 列表样式位置
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListStylePosition {
    Inside,
    Outside,
}

impl Default for ListStylePosition {
    fn default() -> Self {
        ListStylePosition::Outside
    }
}

/// 光标样式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cursor {
    Auto,
    Default,
    None,
    ContextMenu,
    Help,
    Pointer,
    Progress,
    Wait,
    Cell,
    Crosshair,
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    EResize,
    NResize,
    NeResize,
    NwResize,
    SResize,
    SeResize,
    SwResize,
    WResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
    ColResize,
    RowResize,
    AllScroll,
    ZoomIn,
    ZoomOut,
}

//==============================================================================
// Flexbox数据类型 (Flexbox Data Types)
//==============================================================================

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlexWrap {
    NoWrap,
    Wrap,
    WrapReverse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JustifyContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlignItems {
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlignContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlignSelf {
    Auto,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    Stretch,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FlexBasis {
    Auto,
    Content,
    Length(Length),
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor::Auto
    }
}

impl Default for FlexDirection {
    fn default() -> Self {
        FlexDirection::Row
    }
}

impl Default for FlexWrap {
    fn default() -> Self {
        FlexWrap::NoWrap
    }
}

impl Default for JustifyContent {
    fn default() -> Self {
        JustifyContent::FlexStart
    }
}

impl Default for AlignItems {
    fn default() -> Self {
        AlignItems::Stretch
    }
}

impl Default for AlignContent {
    fn default() -> Self {
        AlignContent::Stretch
    }
}

impl Default for AlignSelf {
    fn default() -> Self {
        AlignSelf::Auto
    }
}

impl Default for FlexBasis {
    fn default() -> Self {
        FlexBasis::Auto
    }
}

//==============================================================================
// 选择器系统 (Selector System)
//==============================================================================

/// 选择器特异性
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Specificity {
    pub a: u32, // inline styles
    pub b: u32, // IDs
    pub c: u32, // classes, attributes, pseudo-classes
    pub d: u32, // elements, pseudo-elements
}

impl Specificity {
    pub fn new() -> Self {
        Self {
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        }
    }
}

impl Ord for Specificity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.a
            .cmp(&other.a)
            .then_with(|| self.b.cmp(&other.b))
            .then_with(|| self.c.cmp(&other.c))
            .then_with(|| self.d.cmp(&other.d))
    }
}

impl PartialOrd for Specificity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 组合符
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combinator {
    /// 后代选择器 (空格)
    Descendant,
    /// 子元素选择器 (>)
    Child,
    /// 相邻兄弟选择器 (+)
    AdjacentSibling,
    /// 通用兄弟选择器 (~)
    GeneralSibling,
}

/// 简单选择器
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimpleSelector {
    pub element_name: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attributes: Vec<AttributeSelector>,
    pub pseudo_classes: Vec<PseudoClass>,
    pub pseudo_elements: Vec<PseudoElement>,
}

/// 属性选择器
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeSelector {
    pub name: String,
    pub operator: AttributeOperator,
    pub value: Option<String>,
    pub case_insensitive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeOperator {
    /// [attr]
    Exists,
    /// [attr=value]
    Equals,
    /// [attr~=value]
    Contains,
    /// [attr|=value]
    DashMatch,
    /// [attr^=value]
    PrefixMatch,
    /// [attr$=value]
    SuffixMatch,
    /// [attr*=value]
    SubstringMatch,
}

/// 伪类
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoClass {
    Link,
    Visited,
    Hover,
    Active,
    Focus,
    FirstChild,
    LastChild,
    NthChild(i32, i32), // an + b
    NthLastChild(i32, i32),
    FirstOfType,
    LastOfType,
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
    OnlyChild,
    OnlyOfType,
    Root,
    Empty,
    Lang(String),
    // 表单相关伪类
    Checked,
    Disabled,
    Enabled,
    // 简化的选择器否定
    Not(Box<SimpleSelector>),
}

/// 伪元素
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoElement {
    FirstLine,
    FirstLetter,
    Before,
    After,
}

/// 复合选择器
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComplexSelector {
    pub simple: SimpleSelector,
    pub combinator: Option<Combinator>,
    pub next: Option<Box<ComplexSelector>>,
}

/// 选择器
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub complex: ComplexSelector,
}

impl Selector {
    pub fn specificity(&self) -> Specificity {
        let mut spec = Specificity::new();
        self.calculate_specificity(&self.complex, &mut spec);
        spec
    }

    fn calculate_specificity(&self, complex: &ComplexSelector, spec: &mut Specificity) {
        let simple = &complex.simple;

        // ID选择器
        if simple.id.is_some() {
            spec.b += 1;
        }

        // 类选择器、属性选择器、伪类
        spec.c += simple.classes.len() as u32;
        spec.c += simple.attributes.len() as u32;
        spec.c += simple.pseudo_classes.len() as u32;

        // 元素选择器、伪元素
        if simple.element_name.is_some() {
            spec.d += 1;
        }
        spec.d += simple.pseudo_elements.len() as u32;

        // 递归处理复合选择器
        if let Some(ref next) = complex.next {
            self.calculate_specificity(next, spec);
        }
    }
}

//==============================================================================
// CSS规则和样式表 (Rules and Stylesheets)
//==============================================================================

/// CSS声明
#[derive(Clone, Debug, PartialEq)]
pub struct Declaration {
    pub property: String,
    pub value: CSSValue,
    pub important: bool,
}

/// CSS值的统一表示
#[derive(Clone, Debug, PartialEq)]
pub enum CSSValue {
    Color(Color),
    Length(Length),
    Number(f32),
    Integer(i32),
    String(String),
    Keyword(String),
    Function(String, Vec<CSSValue>),
    List(Vec<CSSValue>),
}

/// CSS规则
#[derive(Clone, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

/// 样式表
#[derive(Clone, Default, Debug)]
pub struct StyleSheet {
    pub rules: Vec<Rule>,
    pub origin: Origin,
}

/// 样式来源
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    UserAgent,
    User,
    Author,
}

impl Default for Origin {
    fn default() -> Self {
        Origin::Author
    }
}

//==============================================================================
// 计算上下文 (Computation Context)
//==============================================================================

/// 计算上下文 - 提供计算CSS值所需的环境信息
#[derive(Clone, Debug)]
pub struct ComputationContext {
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub font_size: f32,
    pub parent_font_size: f32,
    pub root_font_size: f32,
    pub dpi: f32,
}

impl Default for ComputationContext {
    fn default() -> Self {
        Self {
            viewport_width: 1024.0,
            viewport_height: 768.0,
            font_size: 16.0,
            parent_font_size: 16.0,
            root_font_size: 16.0,
            dpi: 96.0,
        }
    }
}

//==============================================================================
// 计算样式 (Computed Style)
//==============================================================================

/// 完整的计算样式 - 符合CSS 2.1所有属性
#[derive(Clone, Debug)]
pub struct ComputedStyle {
    // 显示和可见性
    pub display: Display,
    pub visibility: Visibility,
    pub overflow: Overflow,

    // 定位
    pub position: Position,
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
    pub top_specified: bool,
    pub right_specified: bool,
    pub bottom_specified: bool,
    pub left_specified: bool,
    pub z_index: i32,

    // 浮动和清除
    pub float: Float,
    pub clear: Clear,

    // 盒模型
    pub width: Length,
    pub height: Length,
    pub min_width: Length,
    pub max_width: Length,
    pub min_height: Length,
    pub max_height: Length,

    // 外边距
    pub margin_top: Length,
    pub margin_right: Length,
    pub margin_bottom: Length,
    pub margin_left: Length,

    // 内边距
    pub padding_top: Length,
    pub padding_right: Length,
    pub padding_bottom: Length,
    pub padding_left: Length,

    // 边框
    pub border_top_width: Length,
    pub border_right_width: Length,
    pub border_bottom_width: Length,
    pub border_left_width: Length,
    pub border_top_style: BorderStyle,
    pub border_right_style: BorderStyle,
    pub border_bottom_style: BorderStyle,
    pub border_left_style: BorderStyle,
    pub border_top_color: Color,
    pub border_right_color: Color,
    pub border_bottom_color: Color,
    pub border_left_color: Color,

    // 背景
    pub background_color: Color,
    pub background_image: Option<String>,
    pub background_repeat: BackgroundRepeat,
    pub background_attachment: BackgroundAttachment,
    pub background_position: BackgroundPosition,

    // 字体和文本
    pub font_family: Vec<String>,
    pub font_style: FontStyle,
    pub font_weight: FontWeight,
    pub font_size: Length,
    pub line_height: Length,
    pub color: Color,
    pub text_decoration: TextDecoration,
    pub text_align: TextAlign,
    pub text_indent: Length,
    pub text_transform: TextTransform,
    pub vertical_align: VerticalAlign,
    pub letter_spacing: Length,
    pub word_spacing: Length,

    // 表格
    pub table_layout: TableLayout,
    pub border_collapse: BorderCollapse,
    pub border_spacing: (Length, Length),
    pub empty_cells: EmptyCells,
    pub caption_side: CaptionSide,

    // 列表
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,
    pub list_style_image: Option<String>,

    // Flexbox容器属性
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub align_content: AlignContent,
    pub gap: Length,
    pub row_gap: Length,
    pub column_gap: Length,

    // Flexbox项目属性
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: FlexBasis,
    pub align_self: AlignSelf,
    pub order: i32,

    // 其他
    pub box_sizing: BoxSizing,
    pub cursor: Cursor,
    pub outline_width: Length,
    pub outline_style: BorderStyle,
    pub outline_color: Color,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::default(),
            visibility: Visibility::default(),
            overflow: Overflow::default(),
            position: Position::default(),
            top: Length::default(),
            right: Length::default(),
            bottom: Length::default(),
            left: Length::default(),
            top_specified: false,
            right_specified: false,
            bottom_specified: false,
            left_specified: false,
            z_index: 0,
            float: Float::default(),
            clear: Clear::default(),
            width: Length::default(),
            height: Length::default(),
            min_width: Length::default(),
            max_width: Length::default(),
            min_height: Length::default(),
            max_height: Length::default(),
            margin_top: Length::default(),
            margin_right: Length::default(),
            margin_bottom: Length::default(),
            margin_left: Length::default(),
            padding_top: Length::default(),
            padding_right: Length::default(),
            padding_bottom: Length::default(),
            padding_left: Length::default(),
            border_top_width: Length::default(),
            border_right_width: Length::default(),
            border_bottom_width: Length::default(),
            border_left_width: Length::default(),
            border_top_style: BorderStyle::default(),
            border_right_style: BorderStyle::default(),
            border_bottom_style: BorderStyle::default(),
            border_left_style: BorderStyle::default(),
            border_top_color: Color::default(),
            border_right_color: Color::default(),
            border_bottom_color: Color::default(),
            border_left_color: Color::default(),
            background_color: Color::default(),
            background_image: None,
            background_repeat: BackgroundRepeat::default(),
            background_attachment: BackgroundAttachment::default(),
            background_position: BackgroundPosition::default(),
            font_family: vec!["serif".to_string()],
            font_style: FontStyle::default(),
            font_weight: FontWeight::default(),
            font_size: Length::Px(16.0),
            line_height: Length::Px(18.0),
            color: Color::rgb(0, 0, 0),
            text_decoration: TextDecoration::default(),
            text_align: TextAlign::default(),
            text_indent: Length::default(),
            text_transform: TextTransform::default(),
            vertical_align: VerticalAlign::default(),
            letter_spacing: Length::default(),
            word_spacing: Length::default(),
            table_layout: TableLayout::default(),
            border_collapse: BorderCollapse::default(),
            border_spacing: (Length::Px(2.0), Length::Px(2.0)),
            empty_cells: EmptyCells::default(),
            caption_side: CaptionSide::default(),
            list_style_type: ListStyleType::default(),
            list_style_position: ListStylePosition::default(),
            list_style_image: None,

            // Flexbox容器属性默认值
            flex_direction: FlexDirection::default(),
            flex_wrap: FlexWrap::default(),
            justify_content: JustifyContent::default(),
            align_items: AlignItems::default(),
            align_content: AlignContent::default(),
            gap: Length::Px(0.0),
            row_gap: Length::Px(0.0),
            column_gap: Length::Px(0.0),

            // Flexbox项目属性默认值
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::default(),
            align_self: AlignSelf::default(),
            order: 0,

            box_sizing: BoxSizing::default(),
            cursor: Cursor::default(),
            outline_width: Length::default(),
            outline_style: BorderStyle::default(),
            outline_color: Color::default(),
        }
    }
}

//==============================================================================
// CSS值解析器实现 (Value Parsers Implementation)
//==============================================================================

/// 颜色解析器
pub struct ColorParser;

impl CSSValueParser<Color> for ColorParser {
    fn parse(&self, input: &str, _context: &ComputationContext) -> Result<Color, ParseError> {
        parse_color(input).ok_or(ParseError {
            message: "Invalid color value",
            position: 0,
        })
    }

    fn validate(&self, _value: &Color) -> bool {
        true // 所有Color实例都是有效的
    }
}

/// 长度解析器
pub struct LengthParser;

impl CSSValueParser<Length> for LengthParser {
    fn parse(&self, input: &str, _context: &ComputationContext) -> Result<Length, ParseError> {
        parse_length(input).ok_or(ParseError {
            message: "Invalid length value",
            position: 0,
        })
    }

    fn validate(&self, value: &Length) -> bool {
        match value {
            Length::Px(v)
            | Length::Em(v)
            | Length::Ex(v)
            | Length::In(v)
            | Length::Cm(v)
            | Length::Mm(v)
            | Length::Pt(v)
            | Length::Pc(v)
            | Length::Percent(v) => v.is_finite(),
        }
    }
}

/// 长度计算器
pub struct LengthComputer;

impl CSSValueComputer<Length> for LengthComputer {
    fn compute(&self, value: &Length, context: &ComputationContext) -> Length {
        match *value {
            Length::Px(v) => Length::Px(v),
            Length::Em(v) => Length::Px(v * context.font_size),
            Length::Ex(v) => Length::Px(v * context.font_size * 0.5), // 近似值
            Length::In(v) => Length::Px(v * context.dpi),
            Length::Cm(v) => Length::Px(v * context.dpi / 2.54),
            Length::Mm(v) => Length::Px(v * context.dpi / 25.4),
            Length::Pt(v) => Length::Px(v * context.dpi / 72.0),
            Length::Pc(v) => Length::Px(v * context.dpi / 6.0),
            Length::Percent(v) => Length::Percent(v), // 百分比需要上下文处理
        }
    }
}

//==============================================================================
// CSS解析器实现 (Parser Implementation)
//==============================================================================

/// 标准CSS解析器
pub struct StandardCSSParser {
    color_parser: ColorParser,
    length_parser: LengthParser,
}

impl Default for StandardCSSParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StandardCSSParser {
    pub fn new() -> Self {
        Self {
            color_parser: ColorParser,
            length_parser: LengthParser,
        }
    }
}

impl CSSParser for StandardCSSParser {
    fn parse_stylesheet(&self, input: &str) -> Result<StyleSheet, ParseError> {
        parse_stylesheet(input)
    }

    fn parse_selector(&self, input: &str) -> Result<Selector, ParseError> {
        parse_selector(input)
    }

    fn parse_declaration(&self, property: &str, value: &str) -> Result<Declaration, ParseError> {
        parse_declaration(property, value)
    }
}

//==============================================================================
// 选择器匹配器实现 (Selector Matcher Implementation)
//==============================================================================

pub struct StandardSelectorMatcher;

impl SelectorMatcher for StandardSelectorMatcher {
    fn matches(&self, selector: &Selector, element: &dyn Element) -> bool {
        let matches = match_complex_selector(&selector.complex, element);
        let tag = element.tag_name().unwrap_or("");
        if matches && tag != "" && tag != "style" && tag != "head" {
            println!(
                "[css] Selector MATCHED element '{}' (id={:?}, classes={:?})",
                tag,
                element.id(),
                element.classes()
            );
        }
        matches
    }

    fn specificity(&self, selector: &Selector) -> Specificity {
        selector.specificity()
    }
}

fn match_complex_selector(complex: &ComplexSelector, element: &dyn Element) -> bool {
    // 首先匹配当前简单选择器
    if !match_simple_selector(&complex.simple, element) {
        return false;
    }

    // 如果没有下一个选择器，匹配成功
    let Some(ref next) = complex.next else {
        return true;
    };

    let Some(combinator) = complex.combinator else {
        return false;
    };

    match combinator {
        Combinator::Descendant => {
            // 查找任意祖先元素
            let mut current = element.parent();
            while let Some(parent) = current {
                if match_complex_selector(next, parent) {
                    return true;
                }
                current = parent.parent();
            }
            false
        }
        Combinator::Child => {
            // 查找直接父元素
            if let Some(parent) = element.parent() {
                match_complex_selector(next, parent)
            } else {
                false
            }
        }
        // 简化实现，暂不支持兄弟选择器
        Combinator::AdjacentSibling | Combinator::GeneralSibling => false,
    }
}

fn match_simple_selector(simple: &SimpleSelector, element: &dyn Element) -> bool {
    // 匹配元素名
    if let Some(ref name) = simple.element_name {
        if let Some(tag) = element.tag_name() {
            if name != tag {
                return false;
            }
        } else {
            return false;
        }
    }

    // 匹配ID
    if let Some(ref id) = simple.id {
        if let Some(element_id) = element.id() {
            if id != element_id {
                return false;
            }
        } else {
            return false;
        }
    }

    // 匹配类名
    for class in &simple.classes {
        if !element.classes().contains(class) {
            return false;
        }
    }

    // TODO: 实现属性选择器和伪类匹配

    true
}

//==============================================================================
// 层叠计算器实现 (Cascade Calculator Implementation)
//==============================================================================

pub struct StandardCascadeCalculator {
    matcher: StandardSelectorMatcher,
}

impl Default for StandardCascadeCalculator {
    fn default() -> Self {
        Self::new()
    }
}

impl StandardCascadeCalculator {
    pub fn new() -> Self {
        Self {
            matcher: StandardSelectorMatcher,
        }
    }
}

impl CascadeCalculator for StandardCascadeCalculator {
    fn cascade(&self, rules: &[&Rule], element: &dyn Element) -> Vec<Declaration> {
        let mut matched_declarations = Vec::new();

        // 收集所有匹配的声明
        for (rule_index, rule) in rules.iter().enumerate() {
            for selector in &rule.selectors {
                if self.matcher.matches(selector, element) {
                    let specificity = self.matcher.specificity(selector);
                    println!(
                        "[css] Rule matched element with {} declarations",
                        rule.declarations.len()
                    );
                    for declaration in &rule.declarations {
                        println!(
                            "[css]   Adding declaration: {} = {:?}",
                            declaration.property, declaration.value
                        );
                        matched_declarations.push((declaration.clone(), specificity, rule_index));
                    }
                }
            }
        }

        // 按特异性和源顺序排序
        matched_declarations.sort_by(|a, b| {
            // 首先比较!important
            b.0.important
                .cmp(&a.0.important)
                .then_with(|| b.1.cmp(&a.1)) // 特异性降序
                .then_with(|| b.2.cmp(&a.2)) // 源顺序降序
        });

        // 去重，保留最高优先级的声明
        let mut final_declarations = Vec::new();
        let mut seen_properties = Vec::new();

        for (declaration, _, _) in matched_declarations {
            if !seen_properties.contains(&declaration.property) {
                seen_properties.push(declaration.property.clone());
                final_declarations.push(declaration);
            }
        }

        final_declarations
    }
}

//==============================================================================
// 样式计算器实现 (Style Computation Implementation)
//==============================================================================

/// 计算样式生成器
pub struct StyleComputer {
    cascade_calculator: StandardCascadeCalculator,
    length_computer: LengthComputer,
}

impl Default for StyleComputer {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleComputer {
    pub fn new() -> Self {
        Self {
            cascade_calculator: StandardCascadeCalculator::new(),
            length_computer: LengthComputer,
        }
    }

    /// 计算元素的完整样式
    pub fn compute_style(
        &self,
        element: &dyn Element,
        stylesheets: &[&StyleSheet],
        context: &ComputationContext,
        parent_style: Option<&ComputedStyle>,
    ) -> ComputedStyle {
        let mut computed = ComputedStyle::default();

        if let Some(parent) = parent_style {
            self.cascade_calculator
                .apply_inheritance(&mut computed, Some(parent));
        }

        let mut all_rules = Vec::new();
        for stylesheet in stylesheets {
            for rule in &stylesheet.rules {
                all_rules.push(rule);
            }
        }

        let declarations = self.cascade_calculator.cascade(&all_rules, element);

        for declaration in declarations {
            self.apply_declaration(&mut computed, &declaration, context);
        }

        computed
    }

    /// 应用单个声明到计算样式
    fn apply_declaration(
        &self,
        computed: &mut ComputedStyle,
        declaration: &Declaration,
        context: &ComputationContext,
    ) {
        println!(
            "[css] Applying declaration: {} = {:?}",
            declaration.property, declaration.value
        );

        match declaration.property.as_str() {
            "display" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    computed.display = parse_display_value(value);
                }
            }
            "color" => {
                if let CSSValue::Color(color) = &declaration.value {
                    computed.color = *color;
                }
            }
            "background-color" => {
                if let CSSValue::Color(color) = &declaration.value {
                    computed.background_color = *color;
                }
            }
            "background" => {
                // 处理background简写属性，提取颜色部分
                match &declaration.value {
                    CSSValue::Color(color) => {
                        computed.background_color = *color;
                        println!("[css] Applied solid background color: {:?}", color);
                    }
                    CSSValue::Function(name, args) if name == "linear-gradient" => {
                        // 从linear-gradient中提取第一个颜色作为fallback
                        println!("[css] Processing linear-gradient with {} args", args.len());
                        for (i, arg) in args.iter().enumerate() {
                            println!("[css]   Arg {}: {:?}", i, arg);
                            match arg {
                                CSSValue::Color(color) => {
                                    computed.background_color = *color;
                                    println!(
                                        "[css] Extracted color from linear-gradient: {:?}",
                                        color
                                    );
                                    break;
                                }
                                CSSValue::List(list) => {
                                    // 在列表中查找颜色（如 [Color, Percent] 的组合）
                                    for item in list {
                                        if let CSSValue::Color(color) = item {
                                            computed.background_color = *color;
                                            println!(
                                                "[css] Extracted color from gradient list: {:?}",
                                                color
                                            );
                                            return; // 直接返回，找到第一个颜色就够了
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    CSSValue::List(values) => {
                        // 在列表中查找颜色值
                        for value in values {
                            if let CSSValue::Color(color) = value {
                                computed.background_color = *color;
                                println!("[css] Applied background color from list: {:?}", color);
                                break;
                            }
                        }
                    }
                    CSSValue::Keyword(keyword) if keyword == "transparent" => {
                        computed.background_color = Color::new(0, 0, 0, 0);
                        println!("[css] Applied transparent background");
                    }
                    _ => {
                        println!("[css] Unhandled background value: {:?}", declaration.value);
                    }
                }
            }
            "font-size" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.font_size = self.length_computer.compute(length, context);
                }
            }
            "width" => {
                match &declaration.value {
                    CSSValue::Length(length) => {
                        computed.width = self.length_computer.compute(length, context);
                        println!("[css] Applied width: {:?} -> {:?}", length, computed.width);
                    }
                    CSSValue::Keyword(keyword) if keyword == "auto" => {
                        // auto width 保持默认值
                        println!("[css] Applied auto width");
                    }
                    _ => {
                        println!("[css] Unhandled width value: {:?}", declaration.value);
                    }
                }
            }
            "height" => {
                match &declaration.value {
                    CSSValue::Length(length) => {
                        computed.height = self.length_computer.compute(length, context);
                        println!(
                            "[css] Applied height: {:?} -> {:?}",
                            length, computed.height
                        );
                    }
                    CSSValue::Keyword(keyword) if keyword == "auto" => {
                        // auto height 保持默认值
                        println!("[css] Applied auto height");
                    }
                    CSSValue::Number(n) if *n == 100.0 => {
                        // 处理 100% (可能被解析为数字)
                        computed.height = Length::Percent(100.0);
                        println!("[css] Applied 100% height");
                    }
                    _ => {
                        println!("[css] Unhandled height value: {:?}", declaration.value);
                    }
                }
            }
            "margin" => {
                // 处理margin简写属性
                match &declaration.value {
                    CSSValue::Length(length) => {
                        let computed_length = self.length_computer.compute(length, context);
                        computed.margin_top = computed_length;
                        computed.margin_right = computed_length;
                        computed.margin_bottom = computed_length;
                        computed.margin_left = computed_length;
                    }
                    CSSValue::Number(n) if *n == 0.0 => {
                        computed.margin_top = Length::Px(0.0);
                        computed.margin_right = Length::Px(0.0);
                        computed.margin_bottom = Length::Px(0.0);
                        computed.margin_left = Length::Px(0.0);
                    }
                    _ => {
                        self.apply_box_property(
                            &mut computed.margin_top,
                            &mut computed.margin_right,
                            &mut computed.margin_bottom,
                            &mut computed.margin_left,
                            &declaration.value,
                            context,
                        );
                    }
                }
            }
            "margin-top" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.margin_top = self.length_computer.compute(length, context);
                }
            }
            "margin-right" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.margin_right = self.length_computer.compute(length, context);
                }
            }
            "margin-bottom" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.margin_bottom = self.length_computer.compute(length, context);
                }
            }
            "margin-left" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.margin_left = self.length_computer.compute(length, context);
                }
            }
            "padding" => {
                // 处理padding简写属性
                self.apply_box_property(
                    &mut computed.padding_top,
                    &mut computed.padding_right,
                    &mut computed.padding_bottom,
                    &mut computed.padding_left,
                    &declaration.value,
                    context,
                );
            }
            "padding-top" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.padding_top = self.length_computer.compute(length, context);
                }
            }
            "padding-right" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.padding_right = self.length_computer.compute(length, context);
                }
            }
            "padding-bottom" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.padding_bottom = self.length_computer.compute(length, context);
                }
            }
            "padding-left" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.padding_left = self.length_computer.compute(length, context);
                }
            }
            "border-width" => {
                // 处理border-width简写属性
                self.apply_box_property(
                    &mut computed.border_top_width,
                    &mut computed.border_right_width,
                    &mut computed.border_bottom_width,
                    &mut computed.border_left_width,
                    &declaration.value,
                    context,
                );
            }
            "position" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    computed.position = parse_position_value(value);
                    println!("[css] Applied position: {}", value);
                }
            }
            "left" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.left = self.length_computer.compute(length, context);
                    computed.left_specified = true;
                    println!("[css] Applied left: {:?} -> {:?}", length, computed.left);
                }
            }
            "top" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.top = self.length_computer.compute(length, context);
                    computed.top_specified = true;
                    println!("[css] Applied top: {:?} -> {:?}", length, computed.top);
                }
            }
            "right" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.right = self.length_computer.compute(length, context);
                    computed.right_specified = true;
                    println!("[css] Applied right: {:?} -> {:?}", length, computed.right);
                }
            }
            "bottom" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.bottom = self.length_computer.compute(length, context);
                    computed.bottom_specified = true;
                    println!(
                        "[css] Applied bottom: {:?} -> {:?}",
                        length, computed.bottom
                    );
                }
            }
            "font-weight" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    computed.font_weight = parse_font_weight_value(value);
                } else if let CSSValue::Integer(weight) = &declaration.value {
                    if *weight >= 100 && *weight <= 900 {
                        computed.font_weight = FontWeight::Weight(*weight as u16);
                    }
                }
            }
            "flex-direction" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    match value.trim().to_lowercase().as_str() {
                        "row" => computed.flex_direction = FlexDirection::Row,
                        "row-reverse" => computed.flex_direction = FlexDirection::RowReverse,
                        "column" => computed.flex_direction = FlexDirection::Column,
                        "column-reverse" => computed.flex_direction = FlexDirection::ColumnReverse,
                        _ => {}
                    }
                }
            }
            "justify-content" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    match value.trim().to_lowercase().as_str() {
                        "flex-start" => computed.justify_content = JustifyContent::FlexStart,
                        "flex-end" => computed.justify_content = JustifyContent::FlexEnd,
                        "center" => computed.justify_content = JustifyContent::Center,
                        "space-between" => computed.justify_content = JustifyContent::SpaceBetween,
                        "space-around" => computed.justify_content = JustifyContent::SpaceAround,
                        "space-evenly" => computed.justify_content = JustifyContent::SpaceEvenly,
                        _ => {}
                    }
                }
            }
            "align-items" => {
                if let CSSValue::Keyword(value) = &declaration.value {
                    match value.trim().to_lowercase().as_str() {
                        "flex-start" => computed.align_items = AlignItems::FlexStart,
                        "flex-end" => computed.align_items = AlignItems::FlexEnd,
                        "center" => computed.align_items = AlignItems::Center,
                        "baseline" => computed.align_items = AlignItems::Baseline,
                        "stretch" => computed.align_items = AlignItems::Stretch,
                        _ => {}
                    }
                }
            }
            "gap" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.gap = self.length_computer.compute(length, context);
                }
            }
            "row-gap" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.row_gap = self.length_computer.compute(length, context);
                }
            }
            "column-gap" => {
                if let CSSValue::Length(length) = &declaration.value {
                    computed.column_gap = self.length_computer.compute(length, context);
                }
            }
            "flex-grow" => match &declaration.value {
                CSSValue::Number(n) => {
                    computed.flex_grow = *n;
                }
                CSSValue::Integer(i) => {
                    computed.flex_grow = *i as f32;
                }
                CSSValue::Length(Length::Px(v)) => {
                    computed.flex_grow = *v;
                }
                _ => {}
            },
            "flex-basis" => match &declaration.value {
                CSSValue::Length(len) => {
                    computed.flex_basis =
                        FlexBasis::Length(self.length_computer.compute(len, context));
                }
                CSSValue::Keyword(k) => match k.trim().to_lowercase().as_str() {
                    "auto" => computed.flex_basis = FlexBasis::Auto,
                    "content" => computed.flex_basis = FlexBasis::Content,
                    _ => {}
                },
                _ => {}
            },
            "flex-shrink" => match &declaration.value {
                CSSValue::Number(n) => {}
                CSSValue::Integer(_i) => {}
                _ => {}
            },
            "flex" => match &declaration.value {
                CSSValue::Number(n) => {
                    computed.flex_grow = *n;
                }
                CSSValue::Integer(i) => {
                    computed.flex_grow = *i as f32;
                }
                CSSValue::Length(Length::Px(v)) => {
                    computed.flex_grow = *v;
                }
                CSSValue::Keyword(k) => match k.trim().to_lowercase().as_str() {
                    "none" => {
                        computed.flex_grow = 0.0;
                        computed.flex_basis = FlexBasis::Auto;
                    }
                    "auto" => {
                        computed.flex_grow = 1.0;
                    }
                    _ => {}
                },
                CSSValue::List(list) => {
                    let mut grow: Option<f32> = None;
                    let mut basis: Option<FlexBasis> = None;
                    for v in list {
                        match v {
                            CSSValue::Number(n) => {
                                if grow.is_none() {
                                    grow = Some(*n);
                                }
                            }
                            CSSValue::Integer(i) => {
                                if grow.is_none() {
                                    grow = Some(*i as f32);
                                }
                            }
                            CSSValue::Length(len) => {
                                basis = Some(FlexBasis::Length(
                                    self.length_computer.compute(len, context),
                                ));
                            }
                            CSSValue::Keyword(k) => match k.trim().to_lowercase().as_str() {
                                "auto" => basis = Some(FlexBasis::Auto),
                                "content" => basis = Some(FlexBasis::Content),
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                    if let Some(g) = grow {
                        computed.flex_grow = g;
                    }
                    if let Some(b) = basis {
                        computed.flex_basis = b;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// 应用盒模型属性（margin, padding, border-width等）
    fn apply_box_property(
        &self,
        top: &mut Length,
        right: &mut Length,
        bottom: &mut Length,
        left: &mut Length,
        value: &CSSValue,
        context: &ComputationContext,
    ) {
        match value {
            CSSValue::Length(length) => {
                // 单个值，应用到所有四边
                let computed_length = self.length_computer.compute(length, context);
                *top = computed_length;
                *right = computed_length;
                *bottom = computed_length;
                *left = computed_length;
            }
            CSSValue::List(values) => {
                // 多个值，按CSS规则展开
                let lengths: Vec<Length> = values
                    .iter()
                    .filter_map(|v| {
                        if let CSSValue::Length(length) = v {
                            Some(self.length_computer.compute(length, context))
                        } else {
                            None
                        }
                    })
                    .collect();

                match lengths.len() {
                    1 => {
                        *top = lengths[0];
                        *right = lengths[0];
                        *bottom = lengths[0];
                        *left = lengths[0];
                    }
                    2 => {
                        *top = lengths[0];
                        *bottom = lengths[0];
                        *right = lengths[1];
                        *left = lengths[1];
                    }
                    3 => {
                        *top = lengths[0];
                        *right = lengths[1];
                        *bottom = lengths[2];
                        *left = lengths[1];
                    }
                    4 => {
                        *top = lengths[0];
                        *right = lengths[1];
                        *bottom = lengths[2];
                        *left = lengths[3];
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

//==============================================================================
// 解析函数实现 (Parsing Functions Implementation)
//==============================================================================

pub fn parse_color(s: &str) -> Option<Color> {
    let t = s.trim();

    // #RRGGBB 或 #AARRGGBB
    if t.starts_with('#') {
        let hex = &t[1..];
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            3 => {
                let r = ((v >> 8) & 0xF) as u8;
                let g = ((v >> 4) & 0xF) as u8;
                let b = (v & 0xF) as u8;
                Color::new(r * 17, g * 17, b * 17, 255) // 将4位扩展到8位
            }
            6 => {
                let r = ((v >> 16) & 0xFF) as u8;
                let g = ((v >> 8) & 0xFF) as u8;
                let b = (v & 0xFF) as u8;
                Color::new(r, g, b, 255)
            }
            8 => {
                let a = ((v >> 24) & 0xFF) as u8;
                let r = ((v >> 16) & 0xFF) as u8;
                let g = ((v >> 8) & 0xFF) as u8;
                let b = (v & 0xFF) as u8;
                Color::new(r, g, b, a)
            }
            _ => return None,
        });
    }

    // rgb() 函数
    if t.starts_with("rgb(") && t.ends_with(')') {
        let inner = &t[4..t.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 3 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            return Some(Color::new(r, g, b, 255));
        }
    }

    // rgba() 函数
    if t.starts_with("rgba(") && t.ends_with(')') {
        let inner = &t[5..t.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 4 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            let a = (parts[3].parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0) as u8;
            return Some(Color::new(r, g, b, a));
        }
    }

    // 颜色关键字 - 符合CSS 2.1标准
    match t.to_lowercase().as_str() {
        "black" => Some(Color::rgb(0, 0, 0)),
        "silver" => Some(Color::rgb(192, 192, 192)),
        "gray" => Some(Color::rgb(128, 128, 128)),
        "white" => Some(Color::rgb(255, 255, 255)),
        "maroon" => Some(Color::rgb(128, 0, 0)),
        "red" => Some(Color::rgb(255, 0, 0)),
        "purple" => Some(Color::rgb(128, 0, 128)),
        "fuchsia" => Some(Color::rgb(255, 0, 255)),
        "green" => Some(Color::rgb(0, 128, 0)),
        "lime" => Some(Color::rgb(0, 255, 0)),
        "olive" => Some(Color::rgb(128, 128, 0)),
        "yellow" => Some(Color::rgb(255, 255, 0)),
        "navy" => Some(Color::rgb(0, 0, 128)),
        "blue" => Some(Color::rgb(0, 0, 255)),
        "teal" => Some(Color::rgb(0, 128, 128)),
        "aqua" => Some(Color::rgb(0, 255, 255)),
        "transparent" => Some(Color::new(0, 0, 0, 0)),
        _ => None,
    }
}

pub fn parse_length(s: &str) -> Option<Length> {
    let t = s.trim();

    if t == "0" {
        return Some(Length::Px(0.0));
    }

    // 像素
    if let Some(value) = t.strip_suffix("px") {
        return value.parse::<f32>().ok().map(Length::Px);
    }

    // em单位
    if let Some(value) = t.strip_suffix("em") {
        return value.parse::<f32>().ok().map(Length::Em);
    }

    // ex单位
    if let Some(value) = t.strip_suffix("ex") {
        return value.parse::<f32>().ok().map(Length::Ex);
    }

    // 英寸
    if let Some(value) = t.strip_suffix("in") {
        return value.parse::<f32>().ok().map(Length::In);
    }

    // 厘米
    if let Some(value) = t.strip_suffix("cm") {
        return value.parse::<f32>().ok().map(Length::Cm);
    }

    // 毫米
    if let Some(value) = t.strip_suffix("mm") {
        return value.parse::<f32>().ok().map(Length::Mm);
    }

    // 点
    if let Some(value) = t.strip_suffix("pt") {
        return value.parse::<f32>().ok().map(Length::Pt);
    }

    // pica
    if let Some(value) = t.strip_suffix("pc") {
        return value.parse::<f32>().ok().map(Length::Pc);
    }

    // 百分比
    if let Some(value) = t.strip_suffix('%') {
        return value.parse::<f32>().ok().map(Length::Percent);
    }

    // 纯数字（当作像素）
    t.parse::<f32>().ok().map(Length::Px)
}

/// 解析CSS声明
pub fn parse_declaration(property: &str, value: &str) -> Result<Declaration, ParseError> {
    let css_value = parse_css_value(value)?;
    Ok(Declaration {
        property: property.to_string(),
        value: css_value,
        important: value.contains("!important"),
    })
}

/// 解析CSS值
pub fn parse_css_value(value: &str) -> Result<CSSValue, ParseError> {
    let trimmed = value.trim().replace("!important", "").trim().to_string();

    // 尝试解析为颜色
    if let Some(color) = parse_color(&trimmed) {
        return Ok(CSSValue::Color(color));
    }

    // 尝试解析为长度
    if let Some(length) = parse_length(&trimmed) {
        return Ok(CSSValue::Length(length));
    }

    // 尝试解析为数字
    if let Ok(number) = trimmed.parse::<f32>() {
        return Ok(CSSValue::Number(number));
    }

    // 尝试解析为整数
    if let Ok(integer) = trimmed.parse::<i32>() {
        return Ok(CSSValue::Integer(integer));
    }

    // 函数值
    if trimmed.contains('(') && trimmed.ends_with(')') {
        let parts: Vec<&str> = trimmed.splitn(2, '(').collect();
        if parts.len() == 2 {
            let func_name = parts[0].trim();
            let func_args = &parts[1][..parts[1].len() - 1];

            let args = parse_function_args(func_args)?;
            return Ok(CSSValue::Function(func_name.to_string(), args));
        }
    }

    // 列表值（用空格或逗号分隔）
    if trimmed.contains(' ') || trimmed.contains(',') {
        let separator = if trimmed.contains(',') { ',' } else { ' ' };
        let parts: Vec<&str> = trimmed
            .split(separator)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if parts.len() > 1 {
            let mut values = Vec::new();
            for part in parts {
                values.push(parse_css_value(part)?);
            }
            return Ok(CSSValue::List(values));
        }
    }

    // 字符串值（引号包围）
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        let content = &trimmed[1..trimmed.len() - 1];
        return Ok(CSSValue::String(content.to_string()));
    }

    // 关键字
    Ok(CSSValue::Keyword(trimmed))
}

/// 解析函数参数
fn parse_function_args(args: &str) -> Result<Vec<CSSValue>, ParseError> {
    let mut values = Vec::new();
    let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();

    for part in parts {
        if !part.is_empty() {
            // 对于linear-gradient，尝试解析每个部分
            // 如果解析失败，跳过这个参数
            match parse_css_value(part) {
                Ok(value) => values.push(value),
                Err(_) => {
                    println!("[css] Skipping unparseable function arg: '{}'", part);
                    // 如果包含百分比或度数，尝试解析为关键字
                    if part.contains('%') || part.contains("deg") {
                        values.push(CSSValue::Keyword(part.to_string()));
                    }
                }
            }
        }
    }

    Ok(values)
}

/// 解析选择器
pub fn parse_selector(input: &str) -> Result<Selector, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError {
            message: "Empty selector",
            position: 0,
        });
    }

    let complex = parse_complex_selector(trimmed)?;
    Ok(Selector { complex })
}

/// 解析复合选择器
fn parse_complex_selector(input: &str) -> Result<ComplexSelector, ParseError> {
    // 简化实现：先解析简单选择器，不支持组合符
    let simple = parse_simple_selector(input)?;

    Ok(ComplexSelector {
        simple,
        combinator: None,
        next: None,
    })
}

/// 解析简单选择器
fn parse_simple_selector(input: &str) -> Result<SimpleSelector, ParseError> {
    let mut selector = SimpleSelector::default();
    let mut i = 0;
    let bytes = input.trim().as_bytes();

    // 跳过开头空白
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                // ID选择器
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
                {
                    i += 1;
                }
                if i > start {
                    let id = String::from_utf8_lossy(&bytes[start..i]).to_string();
                    println!("[css] Parsed ID selector: #{}", id);
                    selector.id = Some(id);
                }
            }
            b'.' => {
                // 类选择器
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
                {
                    i += 1;
                }
                if i > start {
                    let class = String::from_utf8_lossy(&bytes[start..i]).to_string();
                    println!("[css] Parsed class selector: .{}", class);
                    selector.classes.push(class);
                }
            }
            b if b.is_ascii_alphabetic() => {
                // 元素选择器
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                    i += 1;
                }
                if i > start {
                    let element = String::from_utf8_lossy(&bytes[start..i]).to_string();
                    println!("[css] Parsed element selector: {}", element);
                    selector.element_name = Some(element);
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                // 跳过空白字符
                i += 1;
            }
            _ => {
                // 跳过其他字符（如伪类等暂不支持）
                i += 1;
            }
        }
    }

    println!(
        "[css] Final simple selector: element={:?} id={:?} classes={:?}",
        selector.element_name, selector.id, selector.classes
    );

    Ok(selector)
}

/// 解析样式表
pub fn parse_stylesheet(input: &str) -> Result<StyleSheet, ParseError> {
    println!(
        "[css] Starting stylesheet parse, input length: {}",
        input.len()
    );
    let mut stylesheet = StyleSheet::default();
    let mut rules = Vec::new();

    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        skip_whitespace(bytes, &mut i);
        if i >= bytes.len() {
            break;
        }

        // 解析规则
        match parse_rule(bytes, &mut i) {
            Ok(rule) => {
                println!(
                    "[css] Parsed rule with {} selectors and {} declarations",
                    rule.selectors.len(),
                    rule.declarations.len()
                );
                // 只显示第一个选择器
                if let Some(selector) = rule.selectors.first() {
                    println!("[css]   First selector: {:?}", selector);
                }
                // 只显示前几个声明
                for (idx, decl) in rule.declarations.iter().take(3).enumerate() {
                    println!(
                        "[css]   Declaration {}: {} = {:?}",
                        idx, decl.property, decl.value
                    );
                }
                if rule.declarations.len() > 3 {
                    println!(
                        "[css]   ... and {} more declarations",
                        rule.declarations.len() - 3
                    );
                }
                rules.push(rule);
            }
            Err(e) => {
                println!("[css] Failed to parse rule: {:?}", e);
                // 跳过错误的规则
                skip_to_next_rule(bytes, &mut i);
            }
        }
    }

    stylesheet.rules = rules;
    Ok(stylesheet)
}

/// 解析CSS规则
fn parse_rule(bytes: &[u8], i: &mut usize) -> Result<Rule, ParseError> {
    // 解析选择器列表
    let selector_start = *i;
    let mut selector_end = *i;

    // 查找开括号
    while selector_end < bytes.len() && bytes[selector_end] != b'{' {
        selector_end += 1;
    }

    if selector_end >= bytes.len() {
        return Err(ParseError {
            message: "Missing opening brace",
            position: *i,
        });
    }

    let selector_str = String::from_utf8_lossy(&bytes[selector_start..selector_end]);
    let selectors = parse_selector_list(&selector_str)?;

    *i = selector_end + 1; // 跳过开括号

    // 解析声明块
    let mut declarations = Vec::new();

    while *i < bytes.len() {
        skip_whitespace(bytes, i);
        if *i >= bytes.len() || bytes[*i] == b'}' {
            break;
        }

        match parse_declaration_from_bytes(bytes, i) {
            Ok(declaration) => declarations.push(declaration),
            Err(_) => {
                // 跳过错误的声明
                skip_to_next_declaration(bytes, i);
            }
        }
    }

    if *i < bytes.len() && bytes[*i] == b'}' {
        *i += 1; // 跳过闭括号
    }

    Ok(Rule {
        selectors,
        declarations,
    })
}

/// 解析选择器列表
fn parse_selector_list(input: &str) -> Result<Vec<Selector>, ParseError> {
    let parts: Vec<&str> = input
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let mut selectors = Vec::new();

    for part in parts {
        selectors.push(parse_selector(part)?);
    }

    if selectors.is_empty() {
        return Err(ParseError {
            message: "No valid selectors found",
            position: 0,
        });
    }

    Ok(selectors)
}

/// 从字节流解析声明
fn parse_declaration_from_bytes(bytes: &[u8], i: &mut usize) -> Result<Declaration, ParseError> {
    // 解析属性名
    let prop_start = *i;
    while *i < bytes.len() && bytes[*i] != b':' && bytes[*i] != b';' && bytes[*i] != b'}' {
        *i += 1;
    }

    if *i >= bytes.len() || bytes[*i] != b':' {
        return Err(ParseError {
            message: "Expected colon after property name",
            position: *i,
        });
    }

    let property = String::from_utf8_lossy(&bytes[prop_start..*i])
        .trim()
        .to_string();
    *i += 1; // 跳过冒号

    // 解析属性值
    skip_whitespace(bytes, i);
    let value_start = *i;
    while *i < bytes.len() && bytes[*i] != b';' && bytes[*i] != b'}' {
        *i += 1;
    }

    let value = String::from_utf8_lossy(&bytes[value_start..*i])
        .trim()
        .to_string();

    if *i < bytes.len() && bytes[*i] == b';' {
        *i += 1; // 跳过分号
    }

    parse_declaration(&property, &value)
}

/// 跳过空白字符
fn skip_whitespace(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len()
        && (bytes[*i] == b' ' || bytes[*i] == b'\n' || bytes[*i] == b'\t' || bytes[*i] == b'\r')
    {
        *i += 1;
    }
}

/// 跳到下一个规则
fn skip_to_next_rule(bytes: &[u8], i: &mut usize) {
    let mut brace_count = 0;
    while *i < bytes.len() {
        match bytes[*i] {
            b'{' => brace_count += 1,
            b'}' => {
                brace_count -= 1;
                if brace_count <= 0 {
                    *i += 1;
                    break;
                }
            }
            _ => {}
        }
        *i += 1;
    }
}

/// 跳到下一个声明
fn skip_to_next_declaration(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && bytes[*i] != b';' && bytes[*i] != b'}' {
        *i += 1;
    }
    if *i < bytes.len() && bytes[*i] == b';' {
        *i += 1;
    }
}

//==============================================================================
// 属性值解析辅助函数 (Property Value Parsing Helpers)
//==============================================================================

fn parse_display_value(value: &str) -> Display {
    match value.trim().to_lowercase().as_str() {
        "none" => Display::None,
        "inline" => Display::Inline,
        "block" => Display::Block,
        "flex" => Display::Flex,
        "list-item" => Display::ListItem,
        "inline-block" => Display::InlineBlock,
        "table" => Display::Table,
        "inline-table" => Display::InlineTable,
        "table-row-group" => Display::TableRowGroup,
        "table-header-group" => Display::TableHeaderGroup,
        "table-footer-group" => Display::TableFooterGroup,
        "table-row" => Display::TableRow,
        "table-column-group" => Display::TableColumnGroup,
        "table-column" => Display::TableColumn,
        "table-cell" => Display::TableCell,
        "table-caption" => Display::TableCaption,
        _ => Display::Inline,
    }
}

fn parse_position_value(value: &str) -> Position {
    match value.trim().to_lowercase().as_str() {
        "static" => Position::Static,
        "relative" => Position::Relative,
        "absolute" => Position::Absolute,
        "fixed" => Position::Fixed,
        _ => Position::Static,
    }
}

fn parse_font_weight_value(value: &str) -> FontWeight {
    match value.trim().to_lowercase().as_str() {
        "normal" => FontWeight::Normal,
        "bold" => FontWeight::Bold,
        "bolder" => FontWeight::Bolder,
        "lighter" => FontWeight::Lighter,
        _ => FontWeight::Normal,
    }
}

//==============================================================================
// 接口的具体实现增强 (Enhanced Interface Implementations)
//==============================================================================

// 为现有的StandardSelectorMatcher扩展功能
impl StandardSelectorMatcher {
    /// 增强的选择器匹配，支持复杂选择器
    pub fn matches_enhanced(&self, selector: &Selector, element: &dyn Element) -> bool {
        self.matches_complex_enhanced(&selector.complex, element)
    }

    fn matches_complex_enhanced(&self, complex: &ComplexSelector, element: &dyn Element) -> bool {
        // 检查基础选择器
        if !self.matches_simple_enhanced(&complex.simple, element) {
            return false;
        }

        // 检查组合符
        if let Some(ref next_complex) = complex.next {
            if let Some(ref combinator) = complex.combinator {
                return self.matches_combinator_enhanced(combinator, next_complex, element);
            }
        }

        true
    }

    fn matches_simple_enhanced(&self, simple: &SimpleSelector, element: &dyn Element) -> bool {
        // 元素名匹配
        if let Some(ref element_name) = simple.element_name {
            if let Some(tag) = element.tag_name() {
                if tag != element_name {
                    return false;
                }
            } else {
                return false;
            }
        }

        // ID匹配
        if let Some(ref id) = simple.id {
            if let Some(element_id) = element.id() {
                if element_id != id {
                    return false;
                }
            } else {
                return false;
            }
        }

        // 类匹配
        for class in &simple.classes {
            if !element.classes().iter().any(|c| c == class) {
                return false;
            }
        }

        // 属性选择器匹配
        for attribute in &simple.attributes {
            if !self.matches_attribute(attribute, element) {
                return false;
            }
        }

        // 伪类匹配
        for pseudo_class in &simple.pseudo_classes {
            if !self.matches_pseudo_class(pseudo_class, element) {
                return false;
            }
        }

        // 伪元素暂时不支持，需要在渲染时处理
        // 如果有伪元素选择器，当前简化为匹配父元素
        if !simple.pseudo_elements.is_empty() {
            // 简化：伪元素选择器总是匹配（在实际浏览器中需要特殊处理）
            println!("[CSS] Pseudo-element selectors detected, simplified matching");
        }

        true
    }

    fn matches_attribute(&self, attribute: &AttributeSelector, element: &dyn Element) -> bool {
        match attribute.operator {
            AttributeOperator::Equals => {
                // [attr=value]
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    attr_value == attribute.value.as_deref().unwrap_or("")
                } else {
                    false
                }
            }
            AttributeOperator::Contains => {
                // [attr~=value] - 属性值包含指定词
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    if let Some(target) = &attribute.value {
                        attr_value.split_whitespace().any(|word| word == target)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            AttributeOperator::DashMatch => {
                // [attr|=value] - 属性值等于value或以value-开头
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    if let Some(target) = &attribute.value {
                        attr_value == target || attr_value.starts_with(&format!("{}-", target))
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            AttributeOperator::PrefixMatch => {
                // [attr^=value] - 属性值以value开头
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    if let Some(target) = &attribute.value {
                        attr_value.starts_with(target)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            AttributeOperator::SuffixMatch => {
                // [attr$=value] - 属性值以value结尾
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    if let Some(target) = &attribute.value {
                        attr_value.ends_with(target)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            AttributeOperator::SubstringMatch => {
                // [attr*=value] - 属性值包含value
                if let Some(attr_value) = element.get_attribute(&attribute.name) {
                    if let Some(target) = &attribute.value {
                        attr_value.contains(target)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            AttributeOperator::Exists => {
                // [attr] - 仅检查属性是否存在
                element.has_attribute(&attribute.name)
            }
        }
    }

    /// 匹配伪类选择器
    fn matches_pseudo_class(&self, pseudo_class: &PseudoClass, element: &dyn Element) -> bool {
        match pseudo_class {
            PseudoClass::Hover => element.is_hover(),
            PseudoClass::Active => element.is_active(),
            PseudoClass::Focus => element.is_focus(),
            PseudoClass::Visited => element.is_visited(),
            PseudoClass::Link => element.is_link(),
            PseudoClass::Checked => element.is_checked(),
            PseudoClass::Disabled => element.is_disabled(),
            PseudoClass::Enabled => element.is_enabled(),
            PseudoClass::FirstChild => element.is_first_child(),
            PseudoClass::LastChild => element.is_last_child(),
            PseudoClass::OnlyChild => element.is_only_child(),
            PseudoClass::FirstOfType => self.is_first_of_type(element),
            PseudoClass::LastOfType => self.is_last_of_type(element),
            PseudoClass::OnlyOfType => self.is_only_of_type(element),
            PseudoClass::Empty => self.is_empty(element),
            PseudoClass::Root => element.parent().is_none(),
            PseudoClass::NthChild(_a, b) => element.is_nth_child(*b as usize), // 简化：只使用b值
            PseudoClass::NthLastChild(_a, b) => {
                // 简化实现：从最后开始的第n个子元素
                if let Some(parent) = element.parent() {
                    let siblings = parent.children();
                    let from_end = siblings.len() - element.index();
                    from_end == *b as usize
                } else {
                    *b == 1
                }
            }
            PseudoClass::NthOfType(_a, b) => self.is_nth_of_type(element, *b as usize), // 简化：只使用b值
            PseudoClass::NthLastOfType(_a, b) => {
                // 简化实现：从最后开始的第n个同类型元素
                self.is_nth_last_of_type(element, *b as usize)
            }
            PseudoClass::Lang(lang) => {
                // 简化实现：检查lang属性
                element.get_attribute("lang").map_or(false, |l| l == lang)
            }
            PseudoClass::Not(simple_selector) => !self.matches_simple(simple_selector, element),
        }
    }

    /// 检查是否是第一个同类型元素
    fn is_first_of_type(&self, element: &dyn Element) -> bool {
        if let Some(parent) = element.parent() {
            let tag_name = element.tag_name();
            let element_index = element.index();
            for (i, child) in parent.children().iter().enumerate() {
                if child.tag_name() == tag_name {
                    return i == element_index;
                }
            }
        }
        true
    }

    /// 检查是否是最后一个同类型元素
    fn is_last_of_type(&self, element: &dyn Element) -> bool {
        if let Some(parent) = element.parent() {
            let tag_name = element.tag_name();
            let element_index = element.index();
            for (i, child) in parent.children().iter().enumerate().rev() {
                if child.tag_name() == tag_name {
                    return i == element_index;
                }
            }
        }
        true
    }

    /// 检查是否是唯一的同类型元素
    fn is_only_of_type(&self, element: &dyn Element) -> bool {
        if let Some(parent) = element.parent() {
            let tag_name = element.tag_name();
            let count = parent
                .children()
                .iter()
                .filter(|child| child.tag_name() == tag_name)
                .count();
            count == 1
        } else {
            true
        }
    }

    /// 检查是否是第n个同类型元素
    fn is_nth_of_type(&self, element: &dyn Element, n: usize) -> bool {
        if let Some(parent) = element.parent() {
            let tag_name = element.tag_name();
            let element_index = element.index();
            let mut count = 0;
            for (i, child) in parent.children().iter().enumerate() {
                if child.tag_name() == tag_name {
                    count += 1;
                    if count == n {
                        return i == element_index;
                    }
                }
            }
        }
        false
    }

    /// 检查元素是否为空
    fn is_empty(&self, element: &dyn Element) -> bool {
        element.children().is_empty()
    }

    /// 检查是否是从最后开始的第n个同类型元素
    fn is_nth_last_of_type(&self, element: &dyn Element, n: usize) -> bool {
        if let Some(parent) = element.parent() {
            let tag_name = element.tag_name();
            let element_index = element.index();
            let mut count = 0;
            for (i, child) in parent.children().iter().enumerate().rev() {
                if child.tag_name() == tag_name {
                    count += 1;
                    if count == n {
                        return i == element_index;
                    }
                }
            }
        }
        false
    }

    /// 匹配简单选择器（为:not()伪类提供支持）
    fn matches_simple(&self, simple: &SimpleSelector, element: &dyn Element) -> bool {
        self.matches_simple_enhanced(simple, element)
    }

    fn matches_combinator_enhanced(
        &self,
        combinator: &Combinator,
        next: &ComplexSelector,
        element: &dyn Element,
    ) -> bool {
        match combinator {
            Combinator::Descendant => {
                // 后代选择器
                let mut current = element.parent();
                while let Some(parent) = current {
                    if self.matches_complex_enhanced(next, parent) {
                        return true;
                    }
                    current = parent.parent();
                }
                false
            }
            Combinator::Child => {
                // 直接子选择器
                if let Some(parent) = element.parent() {
                    self.matches_complex_enhanced(next, parent)
                } else {
                    false
                }
            }
            _ => false, // 其他组合符需要更多DOM支持
        }
    }

    /// 增强的特异性计算
    pub fn calculate_specificity_enhanced(&self, complex: &ComplexSelector) -> Specificity {
        let mut specificity = self.simple_specificity_enhanced(&complex.simple);

        if let Some(ref next) = complex.next {
            let next_spec = self.calculate_specificity_enhanced(next);
            specificity.a += next_spec.a;
            specificity.b += next_spec.b;
            specificity.c += next_spec.c;
            specificity.d += next_spec.d;
        }

        specificity
    }

    fn simple_specificity_enhanced(&self, simple: &SimpleSelector) -> Specificity {
        let mut spec = Specificity {
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        };

        // ID选择器
        if simple.id.is_some() {
            spec.b += 1;
        }

        // 类选择器、属性选择器、伪类
        spec.c += simple.classes.len() as u32;
        spec.c += simple.attributes.len() as u32;
        spec.c += simple.pseudo_classes.len() as u32;

        // 元素选择器和伪元素
        if simple.element_name.is_some() {
            spec.d += 1;
        }
        spec.d += simple.pseudo_elements.len() as u32;

        spec
    }
}

// 为现有的StandardCascadeCalculator扩展CSS继承功能
impl StandardCascadeCalculator {
    /// 检查属性是否可继承
    pub fn is_inherited_property(property: &str) -> bool {
        match property {
            // 字体相关属性
            "color" | "font-family" | "font-size" | "font-style" | "font-weight"
            | "line-height" | "text-align" | "text-indent" | "text-transform"
            | "letter-spacing" | "word-spacing" | "text-decoration" => true,

            // 列表相关属性
            "list-style-type" | "list-style-position" | "list-style-image" => true,

            // 表格相关属性
            "border-collapse" | "border-spacing" | "caption-side" | "empty-cells"
            | "table-layout" => true,

            // 其他继承属性
            "visibility" | "cursor" => true,

            // 大多数属性不继承
            _ => false,
        }
    }

    /// 应用CSS继承
    pub fn apply_inheritance(
        &self,
        computed: &mut ComputedStyle,
        parent_style: Option<&ComputedStyle>,
    ) {
        if let Some(parent) = parent_style {
            // 应用可继承的属性
            computed.color = parent.color;
            computed.font_family = parent.font_family.clone();
            computed.font_size = parent.font_size;
            computed.font_style = parent.font_style;
            computed.font_weight = parent.font_weight;
            computed.line_height = parent.line_height;
            computed.text_align = parent.text_align;
            computed.text_decoration = parent.text_decoration;
            computed.text_transform = parent.text_transform;
            computed.letter_spacing = parent.letter_spacing;
            computed.word_spacing = parent.word_spacing;
            computed.visibility = parent.visibility;
            computed.cursor = parent.cursor;

            // 列表样式继承
            computed.list_style_type = parent.list_style_type;
            computed.list_style_position = parent.list_style_position;
            computed.list_style_image = parent.list_style_image.clone();

            // 表格样式继承
            computed.border_collapse = parent.border_collapse;
            computed.border_spacing = parent.border_spacing;
            computed.caption_side = parent.caption_side;
            computed.empty_cells = parent.empty_cells;
            computed.table_layout = parent.table_layout;
        }
    }
}

//==============================================================================
// CSS值解析器扩展 (Extended CSS Value Parsers)
//==============================================================================

/// 扩展CSS值解析功能
pub fn parse_css_value_extended(property: &str, value: &str) -> Result<CSSValue, ParseError> {
    let trimmed = value.trim();

    // 根据属性类型进行特定解析
    match property {
        "color"
        | "background-color"
        | "border-color"
        | "border-top-color"
        | "border-right-color"
        | "border-bottom-color"
        | "border-left-color" => {
            if let Some(color) = parse_color(trimmed) {
                Ok(CSSValue::Color(color))
            } else {
                parse_css_value(value)
            }
        }

        "font-size"
        | "line-height"
        | "margin"
        | "margin-top"
        | "margin-right"
        | "margin-bottom"
        | "margin-left"
        | "padding"
        | "padding-top"
        | "padding-right"
        | "padding-bottom"
        | "padding-left"
        | "width"
        | "height"
        | "border-width"
        | "border-top-width"
        | "border-right-width"
        | "border-bottom-width"
        | "border-left-width" => {
            if let Some(length) = parse_length(trimmed) {
                Ok(CSSValue::Length(length))
            } else {
                parse_css_value(value)
            }
        }

        "display" => Ok(CSSValue::Keyword(trimmed.to_string())),

        _ => parse_css_value(value),
    }
}

//==============================================================================
// 布局相关的辅助函数 (Layout Helper Functions)
//==============================================================================

/// 计算百分比值
pub fn resolve_percentage(percentage: f32, base_value: f32) -> f32 {
    (percentage / 100.0) * base_value
}

/// 计算em值到px
pub fn em_to_px(em_value: f32, font_size: f32) -> f32 {
    em_value * font_size
}

/// 计算ex值到px
pub fn ex_to_px(ex_value: f32, font_size: f32) -> f32 {
    ex_value * font_size * 0.5 // ex大约是字体高度的一半
}

/// 物理单位转换为像素
pub fn physical_to_px(value: f32, unit: &str) -> f32 {
    match unit {
        "in" => value * 96.0, // 1 inch = 96px (96 DPI)
        "cm" => value * 37.8, // 1 cm = 37.8px
        "mm" => value * 3.78, // 1 mm = 3.78px
        "pt" => value * 1.33, // 1 point = 1.33px
        "pc" => value * 16.0, // 1 pica = 16px
        _ => value,           // 默认返回原值
    }
}
