# WebCore 模块架构文档

## 概述

WebCore 是一个完整的Web渲染引擎，实现了HTML解析、CSS样式计算、布局引擎和绘制系统。采用接口优先的设计模式，具有高度的模块化和可扩展性。

## 模块结构

### 1. HTML 解析器 (`html.rs`)

**核心接口：**
- `HtmlParser` - HTML解析器核心接口
- `HtmlTokenizer` - 标记化器接口
- `HtmlTreeBuilder` - 树构建器接口
- `DomNodeBuilder` - DOM节点构建器接口

**主要功能：**
- 符合HTML5标准的解析算法
- 完整的DOM树构建
- 支持属性解析和文本节点处理
- 错误恢复机制

**数据结构：**
- `DomNode` - DOM节点，支持元素和文本节点
- `Token` - HTML标记类型
- `TokenizerState` - 解析状态机

### 2. CSS 引擎 (`css.rs`)

**核心接口：**
- `CSSValueParser<T>` - CSS值解析器
- `CSSValueComputer<T>` - CSS值计算器
- `SelectorMatcher` - 选择器匹配器
- `CSSParser` - CSS解析器
- `CascadeCalculator` - 层叠计算器

**主要功能：**
- 完整的CSS 2.1支持 + Flexbox
- 选择器解析和匹配（元素、ID、类、属性、伪类）
- 特异性计算和层叠算法
- CSS值解析（颜色、长度、关键字、函数）
- CSS继承机制

**具体实现：**
- `StandardSelectorMatcher` - 选择器匹配实现
- `StandardCascadeCalculator` - 层叠计算实现
- `StyleComputer` - 完整样式计算器

**数据类型：**
- `Color` - RGBA颜色值
- `Length` - 长度值（px, em, %, 物理单位）
- `Display` - 显示类型（包括flex）
- `Position` - 定位类型
- `Specificity` - 选择器特异性

### 3. 样式计算 (`style.rs`)

**主要功能：**
- 构建样式树
- 将DOM与CSS关联
- 计算最终样式值

**数据结构：**
- `StyledNode` - 样式化的DOM节点

### 4. 布局引擎 (`layout.rs`)

**主要功能：**
- 完整的盒模型实现
- 块级布局（Block Layout）
- Flexbox布局（Flex Layout）
- 内联布局（Inline Layout）
- 绝对定位支持

**数据结构：**
- `LayoutBox` - 布局盒，包含位置、尺寸和盒模型
- `BoxModelDimensions` - 完整盒模型（margin、border、padding）
- `EdgeSizes` - 边缘尺寸
- `Rect` - 矩形位置和尺寸

**布局算法：**
- 精确的盒模型计算
- 支持百分比、em等相对单位
- 垂直margin合并
- 文本节点特殊处理

### 5. 绘制系统 (`paint.rs`)

**主要功能：**
- 背景色绘制（考虑盒模型）
- 边框绘制（支持四边独立）
- 文本渲染（TTF字体 + 后备位图字体）
- 图片占位符绘制

**绘制特性：**
- 精确的盒模型边界计算
- 文本在内容区域居中对齐
- 边框独立颜色和宽度支持
- 图片占位符（可扩展为真实图片加载）

### 6. 资源加载器 (`loader.rs`)

**主要功能：**
- 文件读取
- 资源缓存（TODO）

### 7. 文档管理 (`document.rs`)

**主要功能：**
- 协调HTML、CSS、样式、布局、绘制的完整流程
- 提供高层API

## 关键特性

### 接口优先设计
所有核心组件都定义了清晰的trait接口，具体实现可以替换和扩展。

### CSS 2.1 + 现代特性
- 完整的CSS 2.1支持
- Flexbox布局
- 现代选择器支持
- 精确的层叠算法

### 精确的布局计算
- 标准盒模型
- 相对单位计算（em, %, 物理单位）
- 绝对定位
- Flexbox布局

### 高性能文本渲染
- TTF字体渲染
- 精确文本测量
- 位图字体后备

### 完整的绘制管道
- 分层绘制（背景 -> 边框 -> 内容）
- 盒模型边界精确处理
- 可扩展的图片支持

## 使用示例

```rust
// 完整的渲染流程
let page = webcore::document::load_and_prepare(html_path, fallback_html);
let layout_root = page.layout(viewport_width, viewport_height);
webcore::paint::paint_layout_box(&layout_root);
```

## 待实现功能

1. **HTML5标准解析器** - 完整的HTML5标记化器和树构建器
2. **复杂选择器** - 属性选择器、伪类、组合选择器
3. **CSS完整属性解析** - 所有CSS属性的专用解析器
4. **真实图片加载** - PNG/JPEG解码和渲染
5. **文本测量优化** - 更精确的文本布局计算

## 架构优势

1. **高度模块化** - 每个组件独立，易于测试和维护
2. **接口导向** - 所有核心功能都有清晰的trait定义
3. **标准兼容** - 遵循CSS 2.1和HTML5标准
4. **可扩展性** - 容易添加新的CSS属性、布局算法和渲染特性
5. **性能优化** - 精确计算，避免不必要的重复工作

这个架构为后续的功能扩展和性能优化奠定了坚实的基础。
