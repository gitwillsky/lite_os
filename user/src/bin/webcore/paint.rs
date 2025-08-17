use alloc::vec::Vec;
use super::layout::LayoutBox;
use super::css::{Color, BackgroundType, Gradient};
use user_lib::gfx;
use alloc::vec;
use alloc::string::String;

#[allow(unused_imports)]
use libm::{cosf, sinf};

// Fallback三角函数，如果libm不可用
#[allow(dead_code)]
fn simple_cos(x: f32) -> f32 {
    // 简化余弦近似，仅用于0-360度
    let x = x % (2.0 * 3.14159265359);
    if x.abs() < 0.1 { 1.0 }
    else if (x - 1.5708).abs() < 0.1 { 0.0 }  // 90度
    else if (x - 3.14159).abs() < 0.1 { -1.0 } // 180度
    else if (x - 4.7124).abs() < 0.1 { 0.0 }   // 270度
    else { (1.0 - x*x/2.0 + x*x*x*x/24.0).max(-1.0).min(1.0) }
}

#[allow(dead_code)]
fn simple_sin(x: f32) -> f32 {
    simple_cos(x - 1.5708) // sin(x) = cos(x - π/2)
}

// 简易 PNG 解码（no_std，依赖 miniz_oxide/CRC 省略校验），支持 RGBA8888/非隔行
pub fn decode_png_rgba(data: &[u8]) -> Option<(u32,u32,Vec<u8>)> {
	if data.len() < 8 || &data[0..8] != b"\x89PNG\r\n\x1a\n" { return None; }
	let mut i = 8usize; let mut width = 0u32; let mut height = 0u32; let mut idat: Vec<u8> = Vec::new(); let mut color_type = 6u8; let mut _bit_depth = 8u8; let mut _interlace = 0u8;
	while i + 8 <= data.len() {
		if i + 8 > data.len() { break; }
		let len = u32::from_be_bytes([data[i],data[i+1],data[i+2],data[i+3]]) as usize; i += 4;
		let typ = &data[i..i+4]; i += 4;
		if i + len + 4 > data.len() { return None; }
		let payload = &data[i..i+len]; i += len; let _crc = &data[i..i+4]; i += 4;
		match typ {
			b"IHDR" => {
				if payload.len() < 13 { return None; }
				width = u32::from_be_bytes([payload[0],payload[1],payload[2],payload[3]]);
				height = u32::from_be_bytes([payload[4],payload[5],payload[6],payload[7]]);
				let bd = payload[8]; let ct = payload[9]; let il = payload[12];
				_bit_depth = bd; color_type = ct; _interlace = il;
				if bd != 8 || !(ct==6 || ct==2) || il!=0 { return None; }
			}
			b"IDAT" => { idat.extend_from_slice(payload); }
			b"IEND" => { break; }
			_ => {}
		}
	}
	// 解压 zlib（IDAT 拼接后一次解压）
	let cap = (width as usize * (if color_type==6 {4} else {3}) + 1) * (height as usize);
	let mut d = miniz_oxide::inflate::decompress_to_vec_zlib(idat.as_slice()).ok()?;
	if d.len() != cap { /* 允许更大，取前 cap */ if d.len() < cap { return None; } d.truncate(cap); }
	// 去滤波（每行前 1 字节滤波类型）
	let bpp = if color_type==6 {4} else {3};
	let stride = width as usize * bpp;
	let mut rgba = vec![0u8; (width as usize)*(height as usize)*4];
	let mut prev: Vec<u8> = vec![0u8; stride];
	let mut cur: Vec<u8> = vec![0u8; stride];
	let mut di = 0usize;
	for y in 0..(height as usize) {
		if di >= d.len() { return None; }
		let filter = d[di]; di+=1;
		if di + stride > d.len() { return None; }
		for x in 0..stride { cur[x] = d[di+x]; }
		// 仅实现 filter 0..4（标准）
		match filter { 0 => {}, 1 => { for x in 0..stride { let a = if x>=bpp { cur[x-bpp] } else { 0 }; cur[x] = cur[x].wrapping_add(a); } }, 2 => { for x in 0..stride { cur[x] = cur[x].wrapping_add(prev[x]); } }, 3 => { for x in 0..stride { let a = if x>=bpp { cur[x-bpp] } else { 0 }; let b = prev[x]; cur[x] = cur[x].wrapping_add(((a as u16 + b as u16)/2) as u8); } }, 4 => { for x in 0..stride { let a = if x>=bpp { cur[x-bpp] } else { 0 }; let b = prev[x]; let c = if x>=bpp { prev[x-bpp] } else { 0 }; let p = a as i32 + b as i32 - c as i32; let pa=(p - a as i32).abs(); let pb=(p - b as i32).abs(); let pc=(p - c as i32).abs(); let pr = if pa<=pb && pa<=pc { a } else if pb<=pc { b } else { c }; cur[x] = cur[x].wrapping_add(pr); } }, _ => return None }
		// 写 RGBA
		for x in 0..(width as usize) {
			let s = x*bpp; let dptr = (y*width as usize + x)*4;
			let r = cur[s+0]; let g = cur[s+1]; let b = cur[s+2]; let a = if bpp==4 { cur[s+3] } else { 0xFF };
			rgba[dptr+0]=r; rgba[dptr+1]=g; rgba[dptr+2]=b; rgba[dptr+3]=a;
		}
		prev.clone_from_slice(&cur); di += stride;
	}
	Some((width, height, rgba))
}

pub fn paint_tree(root: &LayoutBox) {
	// 先清屏为背景色，再逐个绘制子块背景与文本
	let (sw, sh) = user_lib::gfx::screen_size();
	println!("[webcore::paint] viewport={}x{} root children={}", sw, sh, root.children.len());
	// 背景色取 root 的 background_color
	let bg = root.style.background_color.0;
	if bg != 0 { gfx::gui_clear(bg); }
	for child in &root.children { paint_block(child); }
}

fn paint_block(lb: &LayoutBox) {
	// 背景色/渐变：仅当背景不透明时绘制
	match lb.style.background_type {
		BackgroundType::Solid => {
			if lb.style.background_color.0 != 0 {
				gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, lb.rect.w as u32, lb.rect.h as u32, lb.style.background_color.0);
			}
		},
		BackgroundType::LinearGradient => {
			if let Some(ref gradient) = lb.style.background_gradient {
				// 安全检查：如果区域太大，降级为纯色
				if lb.rect.w * lb.rect.h > 1000000 {
					// 使用第一个渐变颜色作为fallback
					gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, lb.rect.w as u32, lb.rect.h as u32, gradient.stops[0].1.0);
				} else {
					draw_linear_gradient(lb.rect.x, lb.rect.y, lb.rect.w, lb.rect.h, gradient);
				}
			} else {
				// fallback到纯色背景
				if lb.style.background_color.0 != 0 {
					gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, lb.rect.w as u32, lb.rect.h as u32, lb.style.background_color.0);
				}
			}
		}
	}
	println!("[webcore::paint] rect x={} y={} w={} h={} bg={:#x}", lb.rect.x, lb.rect.y, lb.rect.w, lb.rect.h, lb.style.background_color.0);
	// 边框（统一宽度/颜色）
	let bw = lb.style.border_width;
	let bc = lb.style.border_color.0;
	if bw[0] > 0 { gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, lb.rect.w as u32, bw[0] as u32, bc); }
	if bw[2] > 0 { gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y + lb.rect.h - bw[2], lb.rect.w as u32, bw[2] as u32, bc); }
	if bw[3] > 0 { gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, bw[3] as u32, lb.rect.h as u32, bc); }
	if bw[1] > 0 { gfx::gui_fill_rect_xywh(lb.rect.x + lb.rect.w - bw[1], lb.rect.y, bw[1] as u32, lb.rect.h as u32, bc); }
	// 图像渲染：<img src>
	if let Some(src) = lb.image_src.as_ref() {
		// 读取文件
		// 使用堆缓冲，避免大栈帧导致的用户栈溢出
		let mut buf = vec![0u8; 8192];
		let fd = user_lib::open(src, user_lib::open_flags::O_RDONLY) as i32;
		if fd >= 0 {
			let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
			loop {
				let n = user_lib::read(fd as usize, &mut buf);
				if n > 0 { data.extend_from_slice(&buf[..(n as usize)]); if (n as usize) < buf.len() { break; } }
				else { break; }
			}
			let _ = user_lib::close(fd as usize);
			if let Some((w,h,rgba)) = decode_png_rgba(&data) {
				let stride = (w as usize) * 4;
				// 保护：不越界绘制
				if w > 0 && h > 0 && (lb.rect.x >= 0 || lb.rect.y >= 0) {
					gfx::blit_rgba(lb.rect.x, lb.rect.y, w, h, rgba.as_ptr(), stride);
				}
			}
		}
	}
	// 文本渲染：从布局盒上的文本内容绘制（支持阴影和字间距）
	if let Some(text) = lb.text.as_ref() {
		let color = lb.style.color.0;
		let font_px = lb.style.font_size_px as u32;
		let shadow_color = lb.style.text_shadow_color.0;
		let shadow_x = lb.style.text_shadow_offset_x;
		let shadow_y = lb.style.text_shadow_offset_y;
		let letter_spacing = lb.style.letter_spacing_px;
		let has_shadow = shadow_color & 0xFF000000 != 0;

		// 分行绘制：与布局相同的宽度限制
		let max_width = lb.rect.w.max(0) as i32;
		let mut line = String::new();
		let mut cur_w = 0i32;
		let mut pen_y = lb.rect.y + (font_px as i32);

		for ch in text.chars() {
			let cw = user_lib::gfx::measure_char(ch, font_px) + letter_spacing;
			if ch == '\n' || (cur_w + cw > max_width && !line.is_empty()) {
				// 绘制当前行（先阴影后正文）
				if has_shadow && !line.is_empty() {
					let _ = gfx::draw_text(lb.rect.x + shadow_x, pen_y + shadow_y, &line, font_px, shadow_color);
				}
				if !line.is_empty() {
					let _ = gfx::draw_text(lb.rect.x, pen_y, &line, font_px, color);
				}
				line.clear();
				cur_w = 0;
				pen_y += lb.style.font_size_px as i32; // 行高近似等于字号
				if ch == '\n' { continue; }
			}
			line.push(ch);
			cur_w += cw;
		}
		if !line.is_empty() {
			if has_shadow {
				let _ = gfx::draw_text(lb.rect.x + shadow_x, pen_y + shadow_y, &line, font_px, shadow_color);
			}
			let _ = gfx::draw_text(lb.rect.x, pen_y, &line, font_px, color);
		}
	}
	for c in &lb.children { paint_block(c); }
}

fn is_dark(c: u32) -> bool {
	let r = ((c >> 16) & 0xFF) as i32;
	let g = ((c >> 8) & 0xFF) as i32;
	let b = (c & 0xFF) as i32;
	(r * 299 + g * 587 + b * 114) / 1000 < 128
}

fn interpolate_color(c1: Color, c2: Color, t: f32) -> u32 {
	let t = t.clamp(0.0, 1.0);
	let c1 = c1.0;
	let c2 = c2.0;

	let a1 = ((c1 >> 24) & 0xFF) as f32;
	let r1 = ((c1 >> 16) & 0xFF) as f32;
	let g1 = ((c1 >> 8) & 0xFF) as f32;
	let b1 = (c1 & 0xFF) as f32;

	let a2 = ((c2 >> 24) & 0xFF) as f32;
	let r2 = ((c2 >> 16) & 0xFF) as f32;
	let g2 = ((c2 >> 8) & 0xFF) as f32;
	let b2 = (c2 & 0xFF) as f32;

	let a = (a1 + (a2 - a1) * t) as u32;
	let r = (r1 + (r2 - r1) * t) as u32;
	let g = (g1 + (g2 - g1) * t) as u32;
	let b = (b1 + (b2 - b1) * t) as u32;

	(a << 24) | (r << 16) | (g << 8) | b
}

fn draw_linear_gradient(x: i32, y: i32, w: i32, h: i32, gradient: &Gradient) {
	if gradient.stops.len() < 2 { return; }

	// 对于很小的区域，直接用第一个颜色填充
	if w <= 0 || h <= 0 { return; }
	if w * h <= 100 {
		gfx::gui_fill_rect_xywh(x, y, w as u32, h as u32, gradient.stops[0].1.0);
		return;
	}

	// 优化：对于垂直渐变（180度），使用行绘制
	if (gradient.angle_deg - 180.0).abs() < 1.0 {
		// 垂直渐变，按行绘制
		for py in 0..h {
			let t = py as f32 / h as f32;
			let mut color = gradient.stops[0].1.0;

			// 在渐变停靠点中插值
			for i in 1..gradient.stops.len() {
				let (pos1, color1) = gradient.stops[i-1];
				let (pos2, color2) = gradient.stops[i];
				if t >= pos1 && t <= pos2 {
					let local_t = if pos2 > pos1 { (t - pos1) / (pos2 - pos1) } else { 0.0 };
					color = interpolate_color(color1, color2, local_t);
					break;
				}
			}

			gfx::gui_fill_rect_xywh(x, y + py, w as u32, 1, color);
		}
	} else if gradient.angle_deg.abs() < 1.0 || (gradient.angle_deg - 360.0).abs() < 1.0 {
		// 水平渐变，按列绘制
		for px in 0..w {
			let t = px as f32 / w as f32;
			let mut color = gradient.stops[0].1.0;

			// 在渐变停靠点中插值
			for i in 1..gradient.stops.len() {
				let (pos1, color1) = gradient.stops[i-1];
				let (pos2, color2) = gradient.stops[i];
				if t >= pos1 && t <= pos2 {
					let local_t = if pos2 > pos1 { (t - pos1) / (pos2 - pos1) } else { 0.0 };
					color = interpolate_color(color1, color2, local_t);
					break;
				}
			}

			gfx::gui_fill_rect_xywh(x + px, y, 1, h as u32, color);
		}
	} else {
		// 对角渐变，降采样绘制（每4个像素一个）
		let step = 4;
		let angle_rad = gradient.angle_deg * 3.14159265359 / 180.0;
		let cos_a = cosf(angle_rad);
		let sin_a = sinf(angle_rad);

		// 检查三角函数是否有效
		if !cos_a.is_finite() || !sin_a.is_finite() {
			// fallback到简单渐变
			for py in 0..h {
				let t = py as f32 / h as f32;
				let mut color = gradient.stops[0].1.0;
				for i in 1..gradient.stops.len() {
					let (pos1, color1) = gradient.stops[i-1];
					let (pos2, color2) = gradient.stops[i];
					if t >= pos1 && t <= pos2 {
						let local_t = if pos2 > pos1 { (t - pos1) / (pos2 - pos1) } else { 0.0 };
						color = interpolate_color(color1, color2, local_t);
						break;
					}
				}
				gfx::gui_fill_rect_xywh(x, y + py, w as u32, 1, color);
			}
			return;
		}

		for py in (0..h).step_by(step) {
			for px in (0..w).step_by(step) {
				let dx = px as f32 - w as f32 / 2.0;
				let dy = py as f32 - h as f32 / 2.0;
				let proj = dx * cos_a + dy * sin_a;
				let max_proj = (w as f32 * cos_a.abs() + h as f32 * sin_a.abs()) / 2.0;
				let t = if max_proj > 0.0 { (proj + max_proj) / (2.0 * max_proj) } else { 0.0 };

				let mut color = gradient.stops[0].1.0;
				for i in 1..gradient.stops.len() {
					let (pos1, color1) = gradient.stops[i-1];
					let (pos2, color2) = gradient.stops[i];
					if t >= pos1 && t <= pos2 {
						let local_t = if pos2 > pos1 { (t - pos1) / (pos2 - pos1) } else { 0.0 };
						color = interpolate_color(color1, color2, local_t);
						break;
					}
				}

				let block_w = (step as i32).min(w - px) as u32;
				let block_h = (step as i32).min(h - py) as u32;
				gfx::gui_fill_rect_xywh(x + px, y + py, block_w, block_h, color);
			}
		}
	}
}


