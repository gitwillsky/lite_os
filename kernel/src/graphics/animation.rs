use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::DeviceError;
use super::geometry::{Point, Rect, Color};
use super::primitives::GraphicsRenderer;
use super::font::FontRenderer;
use alloc::{vec::Vec, string::String};

pub trait Animator {
    fn update(&mut self, delta_time: f32) -> bool; // returns true if animation continues
    fn render<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError>;
    fn is_finished(&self) -> bool;
    fn reset(&mut self);
}

pub struct ProgressBarAnimation {
    rect: Rect,
    progress: f32,
    target_progress: f32,
    speed: f32,
    bg_color: Color,
    fg_color: Color,
    border_color: Color,
    finished: bool,
}

impl ProgressBarAnimation {
    pub fn new(rect: Rect, speed: f32) -> Self {
        ProgressBarAnimation {
            rect,
            progress: 0.0,
            target_progress: 1.0,
            speed,
            bg_color: Color::DARK_GRAY,
            fg_color: Color::XP_BLUE,
            border_color: Color::GRAY,
            finished: false,
        }
    }

    pub fn with_colors(mut self, bg_color: Color, fg_color: Color, border_color: Color) -> Self {
        self.bg_color = bg_color;
        self.fg_color = fg_color;
        self.border_color = border_color;
        self
    }

    pub fn set_target_progress(&mut self, target: f32) {
        self.target_progress = target.clamp(0.0, 1.0);
    }
}

impl Animator for ProgressBarAnimation {
    fn update(&mut self, delta_time: f32) -> bool {
        if self.progress < self.target_progress {
            self.progress += self.speed * delta_time;
            if self.progress >= self.target_progress {
                self.progress = self.target_progress;
                if self.target_progress >= 1.0 {
                    self.finished = true;
                }
            }
            true
        } else {
            false
        }
    }

    fn render<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        // Draw border
        fb.draw_rect(self.rect, self.border_color)?;
        
        // Draw background
        let inner_rect = Rect::new(
            self.rect.x + 1,
            self.rect.y + 1,
            self.rect.width - 2,
            self.rect.height - 2,
        );
        fb.fill_rect_geom(inner_rect, self.bg_color)?;
        
        // Draw progress
        if self.progress > 0.0 {
            let progress_width = ((inner_rect.width as f32) * self.progress) as u32;
            let progress_rect = Rect::new(
                inner_rect.x,
                inner_rect.y,
                progress_width,
                inner_rect.height,
            );
            fb.fill_rect_geom(progress_rect, self.fg_color)?;
        }
        
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn reset(&mut self) {
        self.progress = 0.0;
        self.finished = false;
    }
}

pub struct FadeAnimation {
    rect: Rect,
    color: Color,
    alpha: f32,
    target_alpha: f32,
    speed: f32,
    finished: bool,
}

impl FadeAnimation {
    pub fn new(rect: Rect, color: Color, speed: f32) -> Self {
        FadeAnimation {
            rect,
            color,
            alpha: 0.0,
            target_alpha: 1.0,
            speed,
            finished: false,
        }
    }

    pub fn fade_in(mut self) -> Self {
        self.alpha = 0.0;
        self.target_alpha = 1.0;
        self
    }

    pub fn fade_out(mut self) -> Self {
        self.alpha = 1.0;
        self.target_alpha = 0.0;
        self
    }
}

impl Animator for FadeAnimation {
    fn update(&mut self, delta_time: f32) -> bool {
        let diff = self.target_alpha - self.alpha;
        if diff.abs() > 0.01 {
            let step = self.speed * delta_time;
            if diff > 0.0 {
                self.alpha += step;
                if self.alpha >= self.target_alpha {
                    self.alpha = self.target_alpha;
                }
            } else {
                self.alpha -= step;
                if self.alpha <= self.target_alpha {
                    self.alpha = self.target_alpha;
                }
            }
            true
        } else {
            self.alpha = self.target_alpha;
            self.finished = true;
            false
        }
    }

    fn render<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        if self.alpha > 0.0 {
            let alpha = (self.alpha * 255.0) as u8;
            let fade_color = Color::new_rgba(self.color.r, self.color.g, self.color.b, alpha);
            fb.fill_rect_geom(self.rect, fade_color)?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn reset(&mut self) {
        self.alpha = 0.0;
        self.finished = false;
    }
}

pub struct TextAnimation {
    text: &'static str,
    position: Point,
    color: Color,
    current_length: usize,
    target_length: usize,
    speed: f32, // characters per second
    time_accumulator: f32,
    finished: bool,
}

impl TextAnimation {
    pub fn new(text: &'static str, position: Point, color: Color, speed: f32) -> Self {
        TextAnimation {
            text,
            position,
            color,
            current_length: 0,
            target_length: text.len(),
            speed,
            time_accumulator: 0.0,
            finished: false,
        }
    }
}

impl Animator for TextAnimation {
    fn update(&mut self, delta_time: f32) -> bool {
        if self.current_length < self.target_length {
            self.time_accumulator += delta_time;
            let chars_to_add = (self.time_accumulator * self.speed) as usize;
            if chars_to_add > 0 {
                self.current_length = (self.current_length + chars_to_add).min(self.target_length);
                self.time_accumulator = 0.0;
                if self.current_length >= self.target_length {
                    self.finished = true;
                }
            }
            true
        } else {
            false
        }
    }

    fn render<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        if self.current_length > 0 {
            let visible_text = &self.text[..self.current_length];
            fb.draw_string(visible_text, self.position, self.color)?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn reset(&mut self) {
        self.current_length = 0;
        self.time_accumulator = 0.0;
        self.finished = false;
    }
}

pub struct WindowsXpBootAnimation {
    screen_rect: Rect,
    logo_rect: Rect,
    progress_rect: Rect,
    text_position: Point,
    
    // Animation phases
    phase: XpBootPhase,
    phase_timer: f32,
    
    // Components
    fade_in: FadeAnimation,
    progress_bar: ProgressBarAnimation,
    text_anim: TextAnimation,
    
    // Progress animation
    progress_dots: usize,
    dot_timer: f32,
    
    finished: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum XpBootPhase {
    FadeIn,
    ShowLogo,
    ShowProgress,
    Loading,
    Complete,
}

impl WindowsXpBootAnimation {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        let screen_rect = Rect::new(0, 0, screen_width, screen_height);
        
        // Calculate positions for 1024x768 or similar resolutions
        let logo_width = 200u32;
        let logo_height = 100u32;
        let logo_rect = Rect::new(
            (screen_width as i32 - logo_width as i32) / 2,
            (screen_height as i32) / 3,
            logo_width,
            logo_height,
        );
        
        let progress_width = 300u32;
        let progress_height = 20u32;
        let progress_rect = Rect::new(
            (screen_width as i32 - progress_width as i32) / 2,
            logo_rect.y + logo_rect.height as i32 + 80,
            progress_width,
            progress_height,
        );
        
        let text_position = Point::new(
            (screen_width as i32) / 2 - 50,
            progress_rect.y + progress_rect.height as i32 + 40,
        );
        
        let fade_in = FadeAnimation::new(screen_rect, Color::BLACK, 2.0).fade_in();
        let progress_bar = ProgressBarAnimation::new(progress_rect, 0.1)
            .with_colors(Color::DARK_GRAY, Color::XP_BLUE, Color::GRAY);
        let text_anim = TextAnimation::new("Microsoft Windows XP", 
                                         Point::new(text_position.x - 80, text_position.y), 
                                         Color::WHITE, 10.0);
        
        WindowsXpBootAnimation {
            screen_rect,
            logo_rect,
            progress_rect,
            text_position,
            phase: XpBootPhase::FadeIn,
            phase_timer: 0.0,
            fade_in,
            progress_bar,
            text_anim,
            progress_dots: 0,
            dot_timer: 0.0,
            finished: false,
        }
    }

    fn render_windows_logo<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        // Draw a simplified Windows logo (4 colored rectangles)
        let quarter_width = self.logo_rect.width / 2 - 2;
        let quarter_height = self.logo_rect.height / 2 - 2;
        
        // Top-left (red)
        let tl_rect = Rect::new(
            self.logo_rect.x,
            self.logo_rect.y,
            quarter_width,
            quarter_height,
        );
        fb.fill_rect_geom(tl_rect, Color::RED)?;
        
        // Top-right (green)
        let tr_rect = Rect::new(
            self.logo_rect.x + quarter_width as i32 + 4,
            self.logo_rect.y,
            quarter_width,
            quarter_height,
        );
        fb.fill_rect_geom(tr_rect, Color::GREEN)?;
        
        // Bottom-left (blue)
        let bl_rect = Rect::new(
            self.logo_rect.x,
            self.logo_rect.y + quarter_height as i32 + 4,
            quarter_width,
            quarter_height,
        );
        fb.fill_rect_geom(bl_rect, Color::BLUE)?;
        
        // Bottom-right (yellow)
        let br_rect = Rect::new(
            self.logo_rect.x + quarter_width as i32 + 4,
            self.logo_rect.y + quarter_height as i32 + 4,
            quarter_width,
            quarter_height,
        );
        fb.fill_rect_geom(br_rect, Color::YELLOW)?;
        
        Ok(())
    }

    fn render_loading_text<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        let mut dots = String::from("Loading");
        for _ in 0..self.progress_dots {
            dots.push('.');
        }
        
        fb.draw_string(&dots, self.text_position, Color::WHITE)?;
        Ok(())
    }
}

impl Animator for WindowsXpBootAnimation {
    fn update(&mut self, delta_time: f32) -> bool {
        if self.finished {
            return false;
        }

        self.phase_timer += delta_time;
        self.dot_timer += delta_time;
        
        // Update dot animation
        if self.dot_timer >= 0.5 {
            self.progress_dots = (self.progress_dots + 1) % 4;
            self.dot_timer = 0.0;
        }

        match self.phase {
            XpBootPhase::FadeIn => {
                self.fade_in.update(delta_time);
                if self.phase_timer >= 1.0 {
                    self.phase = XpBootPhase::ShowLogo;
                    self.phase_timer = 0.0;
                }
            }
            XpBootPhase::ShowLogo => {
                if self.phase_timer >= 1.0 {
                    self.phase = XpBootPhase::ShowProgress;
                    self.phase_timer = 0.0;
                }
            }
            XpBootPhase::ShowProgress => {
                self.text_anim.update(delta_time);
                if self.phase_timer >= 2.0 {
                    self.phase = XpBootPhase::Loading;
                    self.phase_timer = 0.0;
                }
            }
            XpBootPhase::Loading => {
                self.progress_bar.update(delta_time);
                if self.progress_bar.is_finished() && self.phase_timer >= 1.0 {
                    self.phase = XpBootPhase::Complete;
                    self.finished = true;
                }
            }
            XpBootPhase::Complete => {
                self.finished = true;
                return false;
            }
        }

        true
    }

    fn render<T: Framebuffer>(&self, fb: &mut T) -> Result<(), DeviceError> {
        // Clear screen with black background
        fb.clear_geom(Color::BLACK)?;
        
        match self.phase {
            XpBootPhase::FadeIn => {
                self.fade_in.render(fb)?;
            }
            XpBootPhase::ShowLogo => {
                self.render_windows_logo(fb)?;
            }
            XpBootPhase::ShowProgress => {
                self.render_windows_logo(fb)?;
                self.text_anim.render(fb)?;
            }
            XpBootPhase::Loading => {
                self.render_windows_logo(fb)?;
                fb.draw_string("Microsoft Windows XP", 
                             Point::new(self.text_position.x - 80, self.text_position.y), 
                             Color::WHITE)?;
                self.progress_bar.render(fb)?;
                self.render_loading_text(fb)?;
            }
            XpBootPhase::Complete => {
                self.render_windows_logo(fb)?;
                fb.draw_string("Microsoft Windows XP", 
                             Point::new(self.text_position.x - 80, self.text_position.y), 
                             Color::WHITE)?;
                self.progress_bar.render(fb)?;
                fb.draw_string("Loading complete!", self.text_position, Color::GREEN)?;
            }
        }
        
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn reset(&mut self) {
        self.phase = XpBootPhase::FadeIn;
        self.phase_timer = 0.0;
        self.progress_dots = 0;
        self.dot_timer = 0.0;
        self.finished = false;
        self.fade_in.reset();
        self.progress_bar.reset();
        self.text_anim.reset();
    }
}