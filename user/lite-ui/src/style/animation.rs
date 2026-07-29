//! CSS Animations/Transitions timeline and interpolation for computed values.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

use super::split_css_tokens;

#[derive(Clone)]
pub(super) struct Keyframes {
    pub(super) frames: Vec<Keyframe>,
}

#[derive(Clone)]
pub(super) struct Keyframe {
    pub(super) offset: f32,
    pub(super) declarations: Vec<(String, String)>,
}

#[derive(Clone)]
struct RunningAnimation {
    declaration: String,
    started_ms: f64,
}

#[derive(Clone)]
struct RunningTransition {
    from: String,
    to: String,
    started_ms: f64,
    duration_ms: f64,
    timing: Timing,
}

#[derive(Clone, Default)]
struct TransitionNode {
    targets: BTreeMap<String, String>,
    running: HashMap<String, RunningTransition>,
}

/// Per-document CSS timeline. State is keyed by stable host-node identity so
/// React tree rebuilds preserve animation and transition progress.
#[derive(Clone)]
pub(crate) struct Timeline {
    epoch: Instant,
    presentation_origin_ns: Option<u64>,
    presentation_ns: u64,
    presentation_origin_ms: f64,
    now_ms: f64,
    active: bool,
    animations: HashMap<(u64, String), RunningAnimation>,
    transitions: HashMap<u64, TransitionNode>,
    visited_animations: HashSet<(u64, String)>,
    visited_transitions: HashSet<u64>,
}

impl Timeline {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            presentation_origin_ns: None,
            presentation_ns: 0,
            presentation_origin_ms: 0.0,
            now_ms: 0.0,
            active: false,
            animations: HashMap::new(),
            transitions: HashMap::new(),
            visited_animations: HashSet::new(),
            visited_transitions: HashSet::new(),
        }
    }

    /// Advances the document clock to one compositor-confirmed page flip.
    pub(crate) fn presented(&mut self, monotonic_ns: u64) {
        if self.presentation_origin_ns.is_none() {
            self.presentation_origin_ms = self.epoch.elapsed().as_secs_f64() * 1000.0;
            self.presentation_origin_ns = Some(monotonic_ns);
        }
        let origin = self
            .presentation_origin_ns
            .expect("presentation origin was initialized");
        self.presentation_ns = monotonic_ns.saturating_sub(origin);
    }

    /// Starts one style sampling pass on the monotonic document timeline.
    pub(crate) fn begin_frame(&mut self) {
        self.now_ms = if self.presentation_origin_ns.is_some() {
            self.presentation_origin_ms + self.presentation_ns as f64 / 1_000_000.0
        } else {
            self.epoch.elapsed().as_secs_f64() * 1000.0
        };
        self.active = false;
        self.visited_animations.clear();
        self.visited_transitions.clear();
    }

    /// Drops state belonging to DOM nodes or declarations absent from this pass.
    pub(crate) fn finish_frame(&mut self) {
        self.animations
            .retain(|key, _| self.visited_animations.contains(key));
        self.transitions
            .retain(|node, _| self.visited_transitions.contains(node));
    }

    /// Whether the sampled document needs another presentation-driven frame.
    pub(crate) fn active(&self) -> bool {
        self.active
    }

    pub(super) fn apply_transitions(&mut self, node: u64, values: &mut BTreeMap<String, String>) {
        let Some(spec) = Transition::parse(values) else {
            self.transitions.remove(&node);
            return;
        };
        self.visited_transitions.insert(node);
        let state = self.transitions.entry(node).or_default();
        let target = values.get(&spec.property).cloned();
        let previous_target = state.targets.get(&spec.property).cloned();
        if let Some(target) = target {
            if let Some(previous) = previous_target
                && previous != target
            {
                let from = state
                    .running
                    .get(&spec.property)
                    .map(|running| {
                        sample_value(
                            &spec.property,
                            &running.from,
                            &running.to,
                            running.timing.sample(
                                ((self.now_ms - running.started_ms) / running.duration_ms) as f32,
                            ),
                        )
                    })
                    .unwrap_or(previous);
                state.running.insert(
                    spec.property.clone(),
                    RunningTransition {
                        from,
                        to: target.clone(),
                        started_ms: self.now_ms,
                        duration_ms: spec.duration_ms,
                        timing: spec.timing,
                    },
                );
            }
            state.targets.insert(spec.property.clone(), target);
        }
        let Some(running) = state.running.get(&spec.property).cloned() else {
            return;
        };
        let progress = (self.now_ms - running.started_ms) / running.duration_ms;
        if progress >= 1.0 {
            values.insert(spec.property.clone(), running.to);
            state.running.remove(&spec.property);
        } else {
            values.insert(
                spec.property.clone(),
                sample_value(
                    &spec.property,
                    &running.from,
                    &running.to,
                    running.timing.sample(progress as f32),
                ),
            );
            self.active = true;
        }
    }

    pub(super) fn apply_animation(
        &mut self,
        node: u64,
        values: &mut BTreeMap<String, String>,
        keyframes: &BTreeMap<String, Keyframes>,
    ) {
        let Some(declaration) = values.get("animation").cloned() else {
            return;
        };
        let Some(spec) = Animation::parse(&declaration) else {
            return;
        };
        let Some(frames) = keyframes.get(&spec.name) else {
            return;
        };
        let key = (node, spec.name.clone());
        self.visited_animations.insert(key.clone());
        let running = self
            .animations
            .entry(key)
            .or_insert_with(|| RunningAnimation {
                declaration: declaration.clone(),
                started_ms: self.now_ms,
            });
        if running.declaration != declaration {
            running.declaration = declaration;
            running.started_ms = self.now_ms;
        }
        let elapsed = self.now_ms - running.started_ms - spec.delay_ms;
        if elapsed < 0.0 {
            if matches!(spec.fill, FillMode::Backwards | FillMode::Both) {
                apply_keyframes(values, frames, 0.0, spec.timing);
            }
            self.active = true;
            return;
        }
        let total = spec.duration_ms * spec.iterations;
        if elapsed >= total {
            if matches!(spec.fill, FillMode::Forwards | FillMode::Both) {
                apply_keyframes(values, frames, 1.0, spec.timing);
            }
            return;
        }
        let iteration = if spec.duration_ms == 0.0 {
            1.0
        } else {
            (elapsed % spec.duration_ms) / spec.duration_ms
        };
        apply_keyframes(values, frames, iteration as f32, spec.timing);
        self.active = true;
    }
}

#[derive(Clone, Copy)]
enum Timing {
    Linear,
    CubicBezier(f32, f32, f32, f32),
}

impl Timing {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "linear" => Self::Linear,
            "ease" => Self::CubicBezier(0.25, 0.1, 0.25, 1.0),
            "ease-in" => Self::CubicBezier(0.42, 0.0, 1.0, 1.0),
            "ease-out" => Self::CubicBezier(0.0, 0.0, 0.58, 1.0),
            "ease-in-out" => Self::CubicBezier(0.42, 0.0, 0.58, 1.0),
            _ => return None,
        })
    }

    fn sample(self, progress: f32) -> f32 {
        let progress = progress.clamp(0.0, 1.0);
        if progress == 0.0 || progress == 1.0 {
            return progress;
        }
        let Self::CubicBezier(x1, y1, x2, y2) = self else {
            return progress;
        };
        // CSS easing solves x(t)=progress, then returns y(t). Bisection is
        // deterministic and bounded; unlike treating y as the input directly,
        // it preserves the specified cubic-bezier timing curve.
        let bezier = |t: f32, first: f32, second: f32| {
            let inverse = 1.0 - t;
            3.0 * inverse * inverse * t * first + 3.0 * inverse * t * t * second + t * t * t
        };
        let mut low = 0.0;
        let mut high = 1.0;
        for _ in 0..14 {
            let middle = (low + high) * 0.5;
            if bezier(middle, x1, x2) < progress {
                low = middle;
            } else {
                high = middle;
            }
        }
        bezier((low + high) * 0.5, y1, y2)
    }
}

struct Transition {
    property: String,
    duration_ms: f64,
    timing: Timing,
}

impl Transition {
    fn parse(values: &BTreeMap<String, String>) -> Option<Self> {
        let tokens = split_css_tokens(values.get("transition")?);
        let property = tokens
            .iter()
            .find(|token| !is_time(token) && Timing::parse(token).is_none())?
            .to_string();
        let duration_ms = tokens.iter().find_map(|token| parse_time(token))?;
        (duration_ms > 0.0).then_some(Self {
            property,
            duration_ms,
            timing: tokens
                .iter()
                .find_map(|token| Timing::parse(token))
                .unwrap_or(Timing::CubicBezier(0.25, 0.1, 0.25, 1.0)),
        })
    }
}

struct Animation {
    name: String,
    duration_ms: f64,
    delay_ms: f64,
    timing: Timing,
    iterations: f64,
    fill: FillMode,
}

#[derive(Clone, Copy)]
enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
}

impl Animation {
    fn parse(value: &str) -> Option<Self> {
        let tokens = split_css_tokens(value);
        let times: Vec<f64> = tokens
            .iter()
            .filter_map(|token| parse_time(token))
            .collect();
        let duration_ms = *times.first()?;
        let delay_ms = times.get(1).copied().unwrap_or(0.0);
        let timing = tokens
            .iter()
            .find_map(|token| Timing::parse(token))
            .unwrap_or(Timing::CubicBezier(0.25, 0.1, 0.25, 1.0));
        let fill = tokens
            .iter()
            .find_map(|token| match *token {
                "forwards" => Some(FillMode::Forwards),
                "backwards" => Some(FillMode::Backwards),
                "both" => Some(FillMode::Both),
                "none" => Some(FillMode::None),
                _ => None,
            })
            .unwrap_or(FillMode::None);
        let iterations = tokens
            .iter()
            .find_map(|token| {
                if *token == "infinite" {
                    Some(f64::INFINITY)
                } else if is_time(token) {
                    None
                } else {
                    token.parse::<f64>().ok()
                }
            })
            .unwrap_or(1.0);
        let name = tokens
            .iter()
            .find(|token| {
                !is_time(token)
                    && Timing::parse(token).is_none()
                    && !matches!(
                        **token,
                        "infinite"
                            | "normal"
                            | "reverse"
                            | "alternate"
                            | "alternate-reverse"
                            | "none"
                            | "forwards"
                            | "backwards"
                            | "both"
                            | "running"
                            | "paused"
                    )
                    && token.parse::<f64>().is_err()
            })?
            .to_string();
        Some(Self {
            name,
            duration_ms: duration_ms.max(0.0),
            delay_ms,
            timing,
            iterations: iterations.max(0.0),
            fill,
        })
    }
}

fn apply_keyframes(
    values: &mut BTreeMap<String, String>,
    keyframes: &Keyframes,
    progress: f32,
    timing: Timing,
) {
    let Some(first) = keyframes.frames.first() else {
        return;
    };
    let previous = keyframes
        .frames
        .iter()
        .rev()
        .find(|frame| frame.offset <= progress)
        .unwrap_or(first);
    let next = keyframes
        .frames
        .iter()
        .find(|frame| frame.offset >= progress)
        .unwrap_or_else(|| keyframes.frames.last().expect("non-empty keyframes"));
    let span = next.offset - previous.offset;
    let local = if span <= f32::EPSILON {
        1.0
    } else {
        ((progress - previous.offset) / span).clamp(0.0, 1.0)
    };
    // `animation-timing-function` applies independently to each keyframe
    // interval, not once to the whole animation progress.
    let local = timing.sample(local);
    let mut properties = HashSet::new();
    properties.extend(previous.declarations.iter().map(|(name, _)| name.as_str()));
    properties.extend(next.declarations.iter().map(|(name, _)| name.as_str()));
    for property in properties {
        let underlying = values.get(property).cloned().unwrap_or_default();
        let from = declaration(previous, property).unwrap_or(&underlying);
        let to = declaration(next, property).unwrap_or(&underlying);
        values.insert(
            property.to_owned(),
            sample_value(property, from, to, local),
        );
    }
}

fn declaration<'a>(frame: &'a Keyframe, property: &str) -> Option<&'a String> {
    frame
        .declarations
        .iter()
        .rev()
        .find(|(name, _)| name == property)
        .map(|(_, value)| value)
}

fn sample_value(property: &str, from: &str, to: &str, progress: f32) -> String {
    // CSS `display` uses the standardized discrete exception: transitions to
    // `none` keep the element visible through the complete effect, while
    // transitions from `none` make it visible at the effect's start. A generic
    // 50% discrete flip would tear down a fading overlay halfway through.
    if property == "display" {
        if from == "none" && to != "none" {
            return to.to_owned();
        }
        if to == "none" {
            return if progress < 1.0 {
                from.to_owned()
            } else {
                to.to_owned()
            };
        }
    }
    if let (Ok(from), Ok(to)) = (from.parse::<f32>(), to.parse::<f32>()) {
        return format_number(from + (to - from) * progress);
    }
    if let (Some(from), Some(to)) = (parse_px(from), parse_px(to)) {
        return format!("{}px", format_number(from + (to - from) * progress));
    }
    if let (Some(from), Some(to)) = (parse_translate(from), parse_translate(to)) {
        return format!(
            "translate({}px, {}px)",
            format_number(from.0 + (to.0 - from.0) * progress),
            format_number(from.1 + (to.1 - from.1) * progress)
        );
    }
    if progress < 0.5 {
        from.to_owned()
    } else {
        to.to_owned()
    }
}

fn parse_translate(value: &str) -> Option<(f32, f32)> {
    let value = value.trim();
    if value == "none" {
        return Some((0.0, 0.0));
    }
    if let Some(inner) = value
        .strip_prefix("translateX(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return Some((parse_px(inner)?, 0.0));
    }
    if let Some(inner) = value
        .strip_prefix("translateY(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return Some((0.0, parse_px(inner)?));
    }
    let inner = value
        .strip_prefix("translate(")
        .and_then(|value| value.strip_suffix(')'))?;
    let components: Vec<&str> = inner
        .split([',', ' '])
        .filter(|part| !part.trim().is_empty())
        .collect();
    Some((
        parse_px(components.first()?.trim())?,
        components
            .get(1)
            .and_then(|value| parse_px(value.trim()))
            .unwrap_or(0.0),
    ))
}

fn parse_px(value: &str) -> Option<f32> {
    value.trim().strip_suffix("px")?.trim().parse().ok()
}

fn parse_time(value: &str) -> Option<f64> {
    if let Some(milliseconds) = value.strip_suffix("ms") {
        return milliseconds.trim().parse().ok();
    }
    value
        .strip_suffix('s')?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|seconds| seconds * 1000.0)
}

fn is_time(value: &str) -> bool {
    parse_time(value).is_some()
}

fn format_number(value: f32) -> String {
    let mut value = format!("{value:.4}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

#[cfg(test)]
#[path = "animation_tests.rs"]
mod tests;
