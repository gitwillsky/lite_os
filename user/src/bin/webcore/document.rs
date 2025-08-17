use super::{html, css, style, layout, loader};

pub struct PreparedPage {
	pub dom: html::DomNode,
	pub stylesheet: css::StyleSheet,
}

impl PreparedPage {
	pub fn layout(self, viewport_w: i32, viewport_h: i32) -> layout::LayoutBox {
		let mut styled = style::build_style_tree(&self.dom, &self.stylesheet, None);
		layout::layout_tree(&mut styled, viewport_w, viewport_h)
	}
}

pub fn load_and_prepare(html_path: &str, fallback_html: &[u8]) -> PreparedPage {
	let html_bytes = loader::read_all(html_path).unwrap_or_else(|| fallback_html.to_vec());
	let dom = html::parse_document(core::str::from_utf8(&html_bytes).unwrap_or(""));
	// base stylesheet from same folder default name
	let mut stylesheet = css::StyleSheet::default();
	// 默认同目录 style.css
	if let Some(css_bytes) = loader::read_all("/usr/share/desktop/style.css") {
		let extra = css::parse_stylesheet(core::str::from_utf8(&css_bytes).unwrap_or(""));
		let mut rules = extra.rules;
		stylesheet.rules.append(&mut rules);
	}
	// 扫描顶层<link rel=stylesheet>
	for node in &dom.children {
		if node.tag.as_str() == "link" {
			if node.rel.as_ref().map(|s| s.as_str()=="stylesheet").unwrap_or(false) {
				if let Some(href) = node.src.as_ref() {
					if let Some(bytes) = loader::read_all(href) {
						let extra = css::parse_stylesheet(core::str::from_utf8(&bytes).unwrap_or(""));
						let mut rules = extra.rules;
						stylesheet.rules.append(&mut rules);
					}
				}
			}
		}
	}
	PreparedPage { dom, stylesheet }
}
