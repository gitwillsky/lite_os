use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::DeviceError;
use super::geometry::{Point, Rect, Color, Circle};

pub trait GraphicsRenderer {
    fn draw_pixel(&mut self, point: Point, color: Color) -> Result<(), DeviceError>;
    fn draw_line(&mut self, start: Point, end: Point, color: Color) -> Result<(), DeviceError>;
    fn draw_rect(&mut self, rect: Rect, color: Color) -> Result<(), DeviceError>;
    fn fill_rect(&mut self, rect: Rect, color: Color) -> Result<(), DeviceError>;
    fn draw_circle(&mut self, circle: Circle, color: Color) -> Result<(), DeviceError>;
    fn fill_circle(&mut self, circle: Circle, color: Color) -> Result<(), DeviceError>;
    fn clear(&mut self, color: Color) -> Result<(), DeviceError>;
    fn flush(&mut self) -> Result<(), DeviceError>;
}

impl<T: ?Sized + Framebuffer> GraphicsRenderer for T {
    fn draw_pixel(&mut self, point: Point, color: Color) -> Result<(), DeviceError> {
        if point.x >= 0 && point.y >= 0 {
            let info = self.info();
            if point.x < info.width as i32 && point.y < info.height as i32 {
                self.write_pixel(point.x as u32, point.y as u32, color.to_rgba8888())
            } else {
                Err(DeviceError::OperationFailed)
            }
        } else {
            Err(DeviceError::OperationFailed)
        }
    }

    fn draw_line(&mut self, start: Point, end: Point, color: Color) -> Result<(), DeviceError> {
        let dx = (end.x - start.x).abs();
        let dy = (end.y - start.y).abs();
        let sx = if start.x < end.x { 1 } else { -1 };
        let sy = if start.y < end.y { 1 } else { -1 };
        let mut err = dx - dy;

        let mut x = start.x;
        let mut y = start.y;

        loop {
            self.draw_pixel(Point::new(x, y), color)?;

            if x == end.x && y == end.y {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }

        Ok(())
    }

    fn draw_rect(&mut self, rect: Rect, color: Color) -> Result<(), DeviceError> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }

        let top_left = Point::new(rect.x, rect.y);
        let top_right = Point::new(rect.x + rect.width as i32 - 1, rect.y);
        let bottom_left = Point::new(rect.x, rect.y + rect.height as i32 - 1);
        let bottom_right = Point::new(rect.x + rect.width as i32 - 1, rect.y + rect.height as i32 - 1);

        // Draw four edges
        self.draw_line(top_left, top_right, color)?;
        self.draw_line(top_right, bottom_right, color)?;
        self.draw_line(bottom_right, bottom_left, color)?;
        self.draw_line(bottom_left, top_left, color)?;

        Ok(())
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) -> Result<(), DeviceError> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }

        let info = self.info();
        let clip_x = rect.x.max(0) as u32;
        let clip_y = rect.y.max(0) as u32;
        let clip_width = ((rect.x + rect.width as i32).min(info.width as i32) - clip_x as i32).max(0) as u32;
        let clip_height = ((rect.y + rect.height as i32).min(info.height as i32) - clip_y as i32).max(0) as u32;

        if clip_width == 0 || clip_height == 0 {
            return Ok(());
        }

        Framebuffer::fill_rect(self, clip_x, clip_y, clip_width, clip_height, color.to_rgba8888())
    }

    fn draw_circle(&mut self, circle: Circle, color: Color) -> Result<(), DeviceError> {
        let cx = circle.center.x;
        let cy = circle.center.y;
        let r = circle.radius as i32;

        let mut x = 0;
        let mut y = r;
        let mut d = 3 - 2 * r;

        while y >= x {
            // Draw the 8 octants of the circle
            self.draw_pixel(Point::new(cx + x, cy + y), color)?;
            self.draw_pixel(Point::new(cx - x, cy + y), color)?;
            self.draw_pixel(Point::new(cx + x, cy - y), color)?;
            self.draw_pixel(Point::new(cx - x, cy - y), color)?;
            self.draw_pixel(Point::new(cx + y, cy + x), color)?;
            self.draw_pixel(Point::new(cx - y, cy + x), color)?;
            self.draw_pixel(Point::new(cx + y, cy - x), color)?;
            self.draw_pixel(Point::new(cx - y, cy - x), color)?;

            x += 1;

            if d > 0 {
                y -= 1;
                d = d + 4 * (x - y) + 10;
            } else {
                d = d + 4 * x + 6;
            }
        }

        Ok(())
    }

    fn fill_circle(&mut self, circle: Circle, color: Color) -> Result<(), DeviceError> {
        let cx = circle.center.x;
        let cy = circle.center.y;
        let r = circle.radius as i32;

        for y in -r..=r {
            for x in -r..=r {
                if x * x + y * y <= r * r {
                    self.draw_pixel(Point::new(cx + x, cy + y), color)?;
                }
            }
        }

        Ok(())
    }

    fn clear(&mut self, color: Color) -> Result<(), DeviceError> {
        Framebuffer::clear(self, color.to_rgba8888())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        Framebuffer::flush(self)
    }
}

pub fn draw_horizontal_gradient<T: Framebuffer>(
    fb: &mut T,
    rect: Rect,
    start_color: Color,
    end_color: Color,
) -> Result<(), DeviceError> {
    if rect.width == 0 || rect.height == 0 {
        return Ok(());
    }

    for x in 0..rect.width {
        let t = x as f32 / (rect.width - 1) as f32;
        let color = start_color.interpolate(&end_color, t);
        
        let line_rect = Rect::new(rect.x + x as i32, rect.y, 1, rect.height);
        fb.fill_rect_geom(line_rect, color)?;
    }

    Ok(())
}

pub fn draw_vertical_gradient<T: Framebuffer>(
    fb: &mut T,
    rect: Rect,
    start_color: Color,
    end_color: Color,
) -> Result<(), DeviceError> {
    if rect.width == 0 || rect.height == 0 {
        return Ok(());
    }

    for y in 0..rect.height {
        let t = y as f32 / (rect.height - 1) as f32;
        let color = start_color.interpolate(&end_color, t);
        
        let line_rect = Rect::new(rect.x, rect.y + y as i32, rect.width, 1);
        fb.fill_rect_geom(line_rect, color)?;
    }

    Ok(())
}

pub fn draw_radial_gradient<T: Framebuffer>(
    fb: &mut T,
    center: Point,
    radius: u32,
    center_color: Color,
    edge_color: Color,
) -> Result<(), DeviceError> {
    let rect = Rect::new(
        center.x - radius as i32,
        center.y - radius as i32,
        radius * 2,
        radius * 2,
    );

    let height = fb.info().height;
    let width = fb.info().width;
    for y in rect.y.max(0)..((rect.y + rect.height as i32).min(height as i32)) {
        for x in rect.x.max(0)..((rect.x + rect.width as i32).min(width as i32)) {
            let dx = (x - center.x) as f32;
            let dy = (y - center.y) as f32;
            let distance = {
                let d = dx * dx + dy * dy;
                if d > 0.0 { 
                    // Simple Newton's method for sqrt in no_std environment
                    let mut x = d;
                    for _ in 0..10 { // 10 iterations should be enough for reasonable precision
                        x = (x + d / x) * 0.5;
                    }
                    x
                } else { 
                    0.0 
                }
            };
            
            if distance <= radius as f32 {
                let t = distance / radius as f32;
                let color = center_color.interpolate(&edge_color, t);
                fb.draw_pixel(Point::new(x, y), color)?;
            }
        }
    }

    Ok(())
}

pub fn draw_rounded_rect<T: Framebuffer>(
    fb: &mut T,
    rect: Rect,
    radius: u32,
    color: Color,
    fill: bool,
) -> Result<(), DeviceError> {
    if rect.width < radius * 2 || rect.height < radius * 2 {
        return if fill {
            fb.fill_rect_geom(rect, color)
        } else {
            fb.draw_rect(rect, color)
        };
    }

    let r = radius as i32;
    
    // Draw straight edges
    let top_rect = Rect::new(rect.x + r, rect.y, rect.width - radius * 2, 1);
    let bottom_rect = Rect::new(rect.x + r, rect.y + rect.height as i32 - 1, rect.width - radius * 2, 1);
    let left_rect = Rect::new(rect.x, rect.y + r, 1, rect.height - radius * 2);
    let right_rect = Rect::new(rect.x + rect.width as i32 - 1, rect.y + r, 1, rect.height - radius * 2);

    if fill {
        // Fill the main rectangle
        let main_rect = Rect::new(rect.x, rect.y + r, rect.width, rect.height - radius * 2);
        fb.fill_rect_geom(main_rect, color)?;
        
        let top_extension = Rect::new(rect.x + r, rect.y, rect.width - radius * 2, radius);
        fb.fill_rect_geom(top_extension, color)?;
        
        let bottom_extension = Rect::new(rect.x + r, rect.y + rect.height as i32 - r, rect.width - radius * 2, radius);
        fb.fill_rect_geom(bottom_extension, color)?;
    } else {
        fb.fill_rect_geom(top_rect, color)?;
        fb.fill_rect_geom(bottom_rect, color)?;
        fb.fill_rect_geom(left_rect, color)?;
        fb.fill_rect_geom(right_rect, color)?;
    }

    // Draw rounded corners
    let corner_centers = [
        Point::new(rect.x + r, rect.y + r),                                    // Top-left
        Point::new(rect.x + rect.width as i32 - r - 1, rect.y + r),          // Top-right
        Point::new(rect.x + r, rect.y + rect.height as i32 - r - 1),         // Bottom-left
        Point::new(rect.x + rect.width as i32 - r - 1, rect.y + rect.height as i32 - r - 1), // Bottom-right
    ];

    for &center in &corner_centers {
        let circle = Circle::new(center, radius);
        if fill {
            fb.fill_circle(circle, color)?;
        } else {
            fb.draw_circle(circle, color)?;
        }
    }

    Ok(())
}