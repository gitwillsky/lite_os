use alloc::{vec::Vec, vec, string::String, boxed::Box};
use super::{html::DomNode, css::StyleSheet, layout::{LayoutBox, Rect}, paint::DrawCommand, document};

/// 输入事件类型
#[derive(Clone, Debug)]
pub enum InputEvent {
    MouseMove { x: i32, y: i32 },
    MouseDown { x: i32, y: i32, button: MouseButton },
    MouseUp { x: i32, y: i32, button: MouseButton },
    KeyDown { key: Key, modifiers: KeyModifiers },
    KeyUp { key: Key, modifiers: KeyModifiers },
}

#[derive(Clone, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug)]
pub struct Key {
    pub code: u32,
    pub char: Option<char>,
}

#[derive(Clone, Debug)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// 渲染结果
pub struct RenderResult {
    pub commands: Vec<DrawCommand>,
    pub dirty_regions: Vec<Rect>,
}

/// 事件处理结果
pub struct EventResult {
    pub handled: bool,
    pub redraw_needed: bool,
}

/// 更新结果
pub struct UpdateResult {
    pub redraw_needed: bool,
    pub dirty_regions: Vec<Rect>,
}

/// 渲染引擎主接口
pub trait RenderEngine {
    /// 加载 HTML 内容
    fn load_html(&mut self, html: &str);
    
    /// 从文件加载 HTML（会自动处理CSS和字体引用）
    fn load_html_from_file(&mut self, path: &str) -> bool;
    
    /// 加载 CSS 样式
    fn load_css(&mut self, css: &str);
    
    /// 添加外部样式表
    fn add_stylesheet(&mut self, stylesheet: StyleSheet);
    
    /// 设置视口大小
    fn set_viewport(&mut self, width: u32, height: u32);
    
    /// 执行渲染
    fn render(&mut self) -> RenderResult;
    
    /// 处理输入事件
    fn handle_event(&mut self, event: InputEvent) -> EventResult;
    
    /// 更新引擎状态（动画、定时器等）
    fn update(&mut self, delta_ms: u32) -> UpdateResult;
    
    /// 获取 DOM 树（用于调试）
    fn get_dom(&self) -> Option<&DomNode>;
    
    /// 获取布局树（用于调试）
    fn get_layout(&self) -> Option<&LayoutBox>;
    
    /// 获取所有加载的字体文件路径
    fn get_font_paths(&self) -> Vec<String>;
}

/// 标准渲染引擎实现
pub struct StandardRenderEngine {
    dom: Option<DomNode>,
    stylesheet: StyleSheet,
    layout: Option<LayoutBox>,
    viewport_width: u32,
    viewport_height: u32,
    dirty: bool,
    font_paths: Vec<String>,
}

impl StandardRenderEngine {
    pub fn new() -> Self {
        Self {
            dom: None,
            stylesheet: StyleSheet::default(),
            layout: None,
            viewport_width: 800,
            viewport_height: 600,
            dirty: true,
            font_paths: Vec::new(),
        }
    }
    
    /// 处理DOM中的资源引用（字体、CSS等）
    fn process_dom_resources(&mut self, dom: &DomNode) {
        // 处理顶级元素
        self.process_node_resources(dom);
        
        // 递归处理子元素
        for child in &dom.children {
            self.process_dom_resources(child);
        }
    }
    
    fn process_node_resources(&mut self, node: &DomNode) {
        match node.tag.as_str() {
            "link" => {
                // 处理CSS链接
                if node.rel.as_ref().map(|s| s.as_str() == "stylesheet").unwrap_or(false) {
                    if let Some(href) = node.src.as_ref() {
                        println!("[webcore] Loading stylesheet: {}", href);
                        if let Some(bytes) = super::loader::read_all(href) {
                            if let Ok(css_str) = core::str::from_utf8(&bytes) {
                                if let Ok(stylesheet) = super::css::parse_stylesheet(css_str) {
                                    let mut rules = stylesheet.rules;
                                    self.stylesheet.rules.append(&mut rules);
                                    println!("[webcore] Loaded {} rules from {}", rules.len(), href);
                                }
                            }
                        }
                    }
                }
            }
            "style" => {
                // 处理内联CSS
                if let Some(ref text) = node.text {
                    if let Ok(stylesheet) = super::css::parse_stylesheet(text) {
                        let mut rules = stylesheet.rules;
                        self.stylesheet.rules.append(&mut rules);
                        println!("[webcore] Loaded {} rules from inline style", rules.len());
                    }
                }
            }
            _ => {}
        }
        
        // 处理子元素
        for child in &node.children {
            self.process_node_resources(child);
        }
    }
}

impl RenderEngine for StandardRenderEngine {
    fn load_html(&mut self, html: &str) {
        let dom = super::html::parse_document(html);
        
        // 处理DOM中的资源引用
        self.process_dom_resources(&dom);
        
        self.dom = Some(dom);
        self.dirty = true;
    }
    
    fn load_html_from_file(&mut self, path: &str) -> bool {
        println!("[webcore] Loading HTML from file: {}", path);
        if let Some(bytes) = super::loader::read_all(path) {
            if let Ok(html) = String::from_utf8(bytes) {
                self.load_html(&html);
                return true;
            }
        }
        false
    }
    
    fn load_css(&mut self, css: &str) {
        if let Ok(stylesheet) = super::css::parse_stylesheet(css) {
            let mut rules = stylesheet.rules;
            self.stylesheet.rules.append(&mut rules);
            self.dirty = true;
        }
    }
    
    fn add_stylesheet(&mut self, mut stylesheet: StyleSheet) {
        self.stylesheet.rules.append(&mut stylesheet.rules);
        self.dirty = true;
    }
    
    fn set_viewport(&mut self, width: u32, height: u32) {
        self.viewport_width = width;
        self.viewport_height = height;
        self.dirty = true;
    }
    
    fn render(&mut self) -> RenderResult {
        if self.dirty {
            if let Some(ref dom) = self.dom {
                let styled = super::style::style_tree(dom, &self.stylesheet);
                let containing_block = Rect::new(0, 0, self.viewport_width as i32, self.viewport_height as i32);
                self.layout = Some(super::layout::layout_tree(&styled, containing_block));
            }
            self.dirty = false;
        }
        
        let mut commands = Vec::new();
        if let Some(ref layout) = self.layout {
            super::paint::collect_draw_commands(layout, &mut commands);
        }
        
        RenderResult {
            commands,
            dirty_regions: vec![Rect::new(0, 0, self.viewport_width as i32, self.viewport_height as i32)],
        }
    }
    
    fn handle_event(&mut self, _event: InputEvent) -> EventResult {
        // TODO: 实现事件处理
        EventResult {
            handled: false,
            redraw_needed: false,
        }
    }
    
    fn update(&mut self, _delta_ms: u32) -> UpdateResult {
        // TODO: 实现动画和定时器更新
        UpdateResult {
            redraw_needed: false,
            dirty_regions: Vec::new(),
        }
    }
    
    fn get_dom(&self) -> Option<&DomNode> {
        self.dom.as_ref()
    }
    
    fn get_layout(&self) -> Option<&LayoutBox> {
        self.layout.as_ref()
    }
    
    fn get_font_paths(&self) -> Vec<String> {
        self.font_paths.clone()
    }
}