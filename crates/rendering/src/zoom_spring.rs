//! Deterministic spring-simulated zoom transform timeline.
//!
//! Replaces both the duration-based easing state machine that used to live in
//! `zoom.rs` (spring_ease / spring_ease_out / instant_ease over
//! `ZOOM_DURATION`) and the cursor-following `ZoomFocusInterpolator` layer.
//!
//! Three channels — `amount` (zoom scale), `center` (2D framing center in
//! `SegmentBounds::from_amount_center` travel space) and `activity` (the 0/1
//! "any zoom active" step that drives camera scale-during-zoom) — are each
//! integrated by an analytic [`SpringMassDamperSimulation`] chasing
//! step-function targets, retargeted every 8 ms step with velocity always
//! carried across retargets. There are no fixed animation durations and no
//! boundary special-cases: segment starts/ends/re-aims are just target changes
//! the spring smooths through for continuous motion.
//!
//! The timeline lives in TIMELINE time. Cursor events are in RECORDING time,
//! so click-cluster construction and the "active cluster at time t" lookup map
//! through the project's [`TimelineConfiguration`] (identity when absent).
//!
//! Precompute is lazy (`ensure_precomputed_until`) and must happen in the same
//! mutable phase render loops already used for the focus interpolator; the
//! render hot path only calls [`ZoomTransformTimeline::sample`], which is an
//! index + lerp over the cached samples with no allocation and no locking.
//! The result is a pure function of (segments, cursor events, spring config),
//! so sequential playback and seeking produce bit-identical transforms, and
//! export matches playback by construction.

use cap_project::{
    Crop, CursorEvents, CursorMoveEvent, ProjectConfiguration, ScreenMovementSpring,
    TimelineConfiguration, XY, ZoomMode, ZoomSegment,
};

use crate::{
    spring_mass_damper::{SpringMassDamperSimulation, SpringMassDamperSimulationConfig},
    zoom::{InterpolatedZoom, SegmentBounds},
};

/// Fixed precompute step (125 Hz), matching the sampling density the old
/// focus interpolator used.
const STEP_MS: f64 = 8.0;

/// Instant-animation segments snap all channels (no spring) while inside the
/// segment and within this window of its boundaries.
const INSTANT_SNAP_WINDOW_SECS: f64 = 0.1;

/// While the zoom amount is at (or within a hair of) identity the viewport
/// covers the whole frame regardless of the center, so the center channel is
/// free to track its target instantly — this pre-aims upcoming zooms so they
/// scale straight toward their focus. Any center jump while amount <= this
/// bound moves the viewport by at most (bound - 1) of the card: sub-pixel.
const CENTER_PREAIM_MAX_AMOUNT: f32 = 1.0005;
const CURSOR_VIEWPORT_MARGIN_RATIO: f32 = 0.02;
const CURSOR_FOLLOW_RESPONSE_MULTIPLIER: f32 = 3.0;

/// Fallback focus when a segment has no usable cursor data.
const FALLBACK_FOCUS: (f64, f64) = (0.5, 0.5);

/// Maps raw display-UV cursor coordinates into cropped-content UV space.
///
/// Cursor events are normalized to the FULL recorded display, but zoom
/// centers ([`SegmentBounds::from_amount_center`]) are proportions of the
/// rendered (cropped) content. Without this remap a cropped recording aims
/// its auto zoom at the wrong spot — and clustering dead-zone distances are
/// measured in the wrong scale. Identity when the recording is uncropped.
#[derive(Clone, Copy, Debug)]
pub struct CursorCropMap {
    /// Crop top-left in raw display UV.
    offset: XY<f64>,
    /// Crop size in raw display UV.
    scale: XY<f64>,
}

impl CursorCropMap {
    /// `None` when the crop covers the whole screen (identity) or when the
    /// inputs are degenerate.
    pub fn from_crop(crop: &Crop, screen_size: XY<u32>) -> Option<Self> {
        if screen_size.x == 0 || screen_size.y == 0 || crop.size.x == 0 || crop.size.y == 0 {
            return None;
        }
        if crop.position.x == 0
            && crop.position.y == 0
            && crop.size.x >= screen_size.x
            && crop.size.y >= screen_size.y
        {
            return None;
        }
        let screen = XY::new(f64::from(screen_size.x), f64::from(screen_size.y));
        Some(Self {
            offset: XY::new(
                f64::from(crop.position.x) / screen.x,
                f64::from(crop.position.y) / screen.y,
            ),
            scale: XY::new(
                f64::from(crop.size.x) / screen.x,
                f64::from(crop.size.y) / screen.y,
            ),
        })
    }

    /// Raw display UV -> content UV. Positions outside the crop map outside
    /// [0, 1]; consumers clamp at the point of use so movement into a
    /// cropped-away strip still aims the camera at that content edge.
    fn map(&self, x: f64, y: f64) -> (f64, f64) {
        (
            (x - self.offset.x) / self.scale.x,
            (y - self.offset.y) / self.scale.y,
        )
    }
}

#[derive(Debug)]
pub(crate) struct ClickCluster {
    focus_x: f64,
    focus_y: f64,
    target_x: f64,
    target_y: f64,
    follows_cursor: bool,
    start_time_ms: f64,
}

impl ClickCluster {
    fn new(x: f64, y: f64, time_ms: f64, zoom_amount: f64, follows_cursor: bool) -> Self {
        let focus_x = clamp_viewport_focus(x, zoom_amount);
        let focus_y = clamp_viewport_focus(y, zoom_amount);
        Self {
            focus_x,
            focus_y,
            target_x: focus_x,
            target_y: focus_y,
            follows_cursor,
            start_time_ms: time_ms,
        }
    }

    fn update_target(&mut self, x: f64, y: f64, zoom_amount: f64) {
        if self.follows_cursor {
            self.target_x = clamp_viewport_focus(x, zoom_amount);
            self.target_y = clamp_viewport_focus(y, zoom_amount);
        }
    }

    fn recentered(
        &self,
        x: f64,
        y: f64,
        time_ms: f64,
        zoom_amount: f64,
        safe_zone_inset_ratio: f64,
    ) -> Option<Self> {
        let safe_half_span = (0.5 - safe_zone_inset_ratio.clamp(0.0, 0.49)) / zoom_amount.max(1.0);
        let mut focus_x = self.focus_x;
        let mut focus_y = self.focus_y;

        if x < self.focus_x - safe_half_span || x > self.focus_x + safe_half_span {
            focus_x = clamp_viewport_focus(x, zoom_amount);
        }
        if y < self.focus_y - safe_half_span || y > self.focus_y + safe_half_span {
            focus_y = clamp_viewport_focus(y, zoom_amount);
        }

        if focus_x == self.focus_x && focus_y == self.focus_y {
            None
        } else {
            Some(Self {
                focus_x,
                focus_y,
                target_x: focus_x,
                target_y: focus_y,
                follows_cursor: true,
                start_time_ms: time_ms,
            })
        }
    }

    fn center(&self) -> (f64, f64) {
        (self.target_x, self.target_y)
    }
}

fn clamp_viewport_focus(focus: f64, zoom_amount: f64) -> f64 {
    let half_span = 0.5 / zoom_amount.max(1.0);
    focus.clamp(half_span, 1.0 - half_span)
}

fn focus_to_travel_center(focus: (f64, f64), zoom_amount: f64) -> (f64, f64) {
    if zoom_amount <= 1.0 + f64::EPSILON {
        return FALLBACK_FOCUS;
    }

    let travel = zoom_amount - 1.0;
    let convert = |value: f64| ((value * zoom_amount - 0.5) / travel).clamp(0.0, 1.0);
    (convert(focus.0), convert(focus.1))
}

/// Builds persistent camera targets from all cursor movement inside a
/// segment. The target stays fixed inside the safe zone; crossing it recenters
/// only the escaped axis on the cursor.
pub(crate) fn build_clusters(
    cursor_events: &CursorEvents,
    segment_start_secs: f64,
    segment_end_secs: f64,
    zoom_amount: f64,
    safe_zone_inset_ratio: f64,
    crop: Option<CursorCropMap>,
) -> Vec<ClickCluster> {
    let start_ms = segment_start_secs * 1000.0;
    let end_ms = segment_end_secs * 1000.0;
    // Clustering happens in CONTENT UV space: dead-zone box limits are
    // fractions of the visible (cropped) viewport, so raw display UVs must be
    // remapped before distances mean what the constants say they mean.
    let map_uv = |x: f64, y: f64| crop.map_or((x, y), |c| c.map(x, y));

    // Non-finite coordinates (corrupted files, synthetic event generators)
    // must never reach the cluster math: NaN propagates through min/max and
    // clamp, which would poison the spring targets for the whole timeline.
    let finite = |m: &&cap_project::CursorMoveEvent| {
        m.x.is_finite() && m.y.is_finite() && m.time_ms.is_finite()
    };

    let events_in_range: Vec<&cap_project::CursorMoveEvent> = cursor_events
        .moves
        .iter()
        .filter(finite)
        .filter(|m| m.time_ms >= start_ms && m.time_ms <= end_ms)
        .collect();

    if events_in_range.is_empty() {
        let fallback = cursor_events
            .moves
            .iter()
            .filter(finite)
            .rev()
            .find(|m| m.time_ms <= start_ms)
            .or_else(|| {
                cursor_events
                    .moves
                    .iter()
                    .filter(finite)
                    .find(|m| m.time_ms >= start_ms)
            });

        if let Some(evt) = fallback {
            let (x, y) = map_uv(evt.x, evt.y);
            return vec![ClickCluster::new(x, y, evt.time_ms, zoom_amount, false)];
        }
        return vec![];
    }

    let mut clusters = Vec::new();
    let first = events_in_range[0];
    let (first_x, first_y) = map_uv(first.x, first.y);
    let mut current = ClickCluster::new(first_x, first_y, first.time_ms, zoom_amount, false);

    for evt in &events_in_range[1..] {
        let (x, y) = map_uv(evt.x, evt.y);
        if let Some(next) =
            current.recentered(x, y, evt.time_ms, zoom_amount, safe_zone_inset_ratio)
        {
            clusters.push(current);
            current = next;
        } else {
            current.update_target(x, y, zoom_amount);
        }
    }
    clusters.push(current);

    clusters
}

fn cluster_at_time(clusters: &[ClickCluster], time_ms: f64) -> Option<&ClickCluster> {
    clusters
        .iter()
        .rev()
        .find(|c| c.start_time_ms <= time_ms)
        .or_else(|| clusters.first())
}

fn cursor_position_at(moves: &[CursorMoveEvent], time_ms: f64) -> Option<XY<f64>> {
    let first = moves.first()?;
    if time_ms <= first.time_ms {
        return Some(XY::new(first.x, first.y));
    }

    let index = moves.partition_point(|event| event.time_ms <= time_ms);
    if index >= moves.len() {
        let last = moves.last()?;
        return Some(XY::new(last.x, last.y));
    }

    let previous = &moves[index - 1];
    let next = &moves[index];
    let duration = next.time_ms - previous.time_ms;
    if duration <= f64::EPSILON {
        return Some(XY::new(previous.x, previous.y));
    }

    let t = ((time_ms - previous.time_ms) / duration).clamp(0.0, 1.0);
    Some(XY::new(
        previous.x + (next.x - previous.x) * t,
        previous.y + (next.y - previous.y) * t,
    ))
}

fn constrain_center_to_cursor(center: XY<f32>, cursor: XY<f32>, amount: f32) -> XY<f32> {
    if amount <= 1.0 + f32::EPSILON {
        return center;
    }

    let travel = amount - 1.0;
    let constrain_axis = |center: f32, cursor: f32| {
        let min_center =
            ((cursor * amount - 1.0 + CURSOR_VIEWPORT_MARGIN_RATIO) / travel).clamp(0.0, 1.0);
        let max_center =
            ((cursor * amount - CURSOR_VIEWPORT_MARGIN_RATIO) / travel).clamp(0.0, 1.0);
        center.clamp(min_center, max_center)
    };

    XY::new(
        constrain_axis(center.x, cursor.x),
        constrain_axis(center.y, cursor.y),
    )
}

fn recenter_on_cursor_outside_safe_zone(
    center: XY<f32>,
    cursor: XY<f32>,
    amount: f32,
    safe_zone_inset_ratio: f32,
) -> XY<f32> {
    if amount <= 1.0 + f32::EPSILON {
        return center;
    }

    let travel = amount - 1.0;
    let safe_half_span = (0.5 - safe_zone_inset_ratio.clamp(0.0, 0.49)) / amount;
    let viewport_center = XY::new(
        (center.x * travel + 0.5) / amount,
        (center.y * travel + 0.5) / amount,
    );
    let cursor_outside = cursor.x < viewport_center.x - safe_half_span
        || cursor.x > viewport_center.x + safe_half_span
        || cursor.y < viewport_center.y - safe_half_span
        || cursor.y > viewport_center.y + safe_half_span;

    if cursor_outside {
        XY::new(
            ((cursor.x * amount - 0.5) / travel).clamp(0.0, 1.0),
            ((cursor.y * amount - 0.5) / travel).clamp(0.0, 1.0),
        )
    } else {
        center
    }
}

/// One entry of the timeline-time -> recording-time mapping derived from
/// [`TimelineConfiguration::get_segment_time`]'s accumulation logic.
#[derive(Clone, Copy)]
struct TimeMapSegment {
    timeline_start: f64,
    timeline_end: f64,
    recording_start: f64,
    timescale: f64,
    recording_clip: u32,
}

fn build_time_map(timeline: Option<&TimelineConfiguration>) -> Vec<TimeMapSegment> {
    let Some(timeline) = timeline else {
        return Vec::new();
    };

    let mut map = Vec::with_capacity(timeline.segments.len());
    let mut accum = 0.0;
    for (segment_index, segment) in timeline.segments.iter().enumerate() {
        if !timeline.transitions.is_empty() {
            accum -= timeline
                .effective_transition(segment_index)
                .map_or(0.0, |transition| transition.duration);
        }
        let duration = segment.duration();
        if !duration.is_finite() || duration <= 0.0 {
            continue;
        }
        map.push(TimeMapSegment {
            timeline_start: accum,
            timeline_end: accum + duration,
            recording_start: segment.start,
            timescale: segment.timescale,
            recording_clip: segment.recording_clip,
        });
        accum += duration;
    }
    map
}

/// Maps a timeline timestamp to recording seconds. Identity when no timeline
/// is configured; clamps to the nearest edit boundary outside the timeline.
fn map_timeline_to_recording_secs(
    map: &[TimeMapSegment],
    timeline_secs: f64,
    recording_clip: Option<u32>,
    prefer_outgoing: bool,
) -> f64 {
    let Some(first) = map.first() else {
        return timeline_secs;
    };

    if timeline_secs <= first.timeline_start {
        return first.recording_start;
    }

    let contains = |segment: &&TimeMapSegment| {
        timeline_secs >= segment.timeline_start && timeline_secs < segment.timeline_end
    };
    let segment = recording_clip
        .and_then(|recording_clip| {
            if prefer_outgoing {
                map.iter()
                    .filter(contains)
                    .find(|segment| segment.recording_clip == recording_clip)
            } else {
                map.iter()
                    .rev()
                    .filter(contains)
                    .find(|segment| segment.recording_clip == recording_clip)
            }
        })
        .or_else(|| map.iter().rev().find(contains));
    if let Some(segment) = segment {
        return segment.recording_start
            + (timeline_secs - segment.timeline_start) * segment.timescale;
    }

    let last = map.last().expect("map is non-empty");
    last.recording_start + (last.timeline_end - last.timeline_start) * last.timescale
}

/// One precomputed step. Sample times are implicit: `samples[i]` is the state
/// at `i * STEP_MS`, so lookup is pure index math.
#[derive(Clone, Copy, Debug)]
struct TimelineSample {
    amount: f32,
    center: XY<f32>,
    activity: f32,
    snapped: bool,
}

struct PrecomputeState {
    /// 2D framing center in `from_amount_center` travel space.
    center_sim: SpringMassDamperSimulation,
    /// x = zoom amount, y = zoom activity (0/1 step -> smooth camera driver).
    aux_sim: SpringMassDamperSimulation,
    /// Last center target while a segment was active. Held during zoom-out so
    /// the outgoing framing stays anchored instead of re-aiming mid-flight.
    held_center_target: XY<f32>,
}

struct StepTargets {
    amount: f32,
    center: XY<f32>,
    activity: f32,
    segment_active: bool,
    snap: bool,
    cursor: Option<XY<f32>>,
    fast_cursor_follow: bool,
}

/// Deterministic, lazily precomputed zoom transform timeline.
///
/// Construction is cheap (clusters only); integration happens on demand via
/// [`Self::ensure_precomputed_until`] which must be called from a mutable
/// phase (exactly where the old focus interpolator's precompute ran) before
/// [`Self::sample`] is used for the corresponding frame times.
pub struct ZoomTransformTimeline {
    samples: Vec<TimelineSample>,
    state: Option<PrecomputeState>,
    zoom_segments: Vec<ZoomSegment>,
    /// Parallel to `zoom_segments`: prebuilt clusters (RECORDING-time ms) for
    /// Auto segments, `None` for Manual ones.
    clusters: Vec<Option<Vec<ClickCluster>>>,
    cursor_moves: Vec<CursorMoveEvent>,
    cursor_crop: Option<CursorCropMap>,
    time_map: Vec<TimeMapSegment>,
    recording_clip: Option<u32>,
    prefer_outgoing: bool,
    /// Total number of samples covering [0, duration] plus one lerp partner.
    total_samples: usize,
}

struct RecordingClipSelection {
    recording_clip: Option<u32>,
    prefer_outgoing: bool,
}

impl ZoomTransformTimeline {
    pub fn new(
        zoom_segments: &[ZoomSegment],
        timeline: Option<&TimelineConfiguration>,
        cursor_events: &CursorEvents,
        spring: ScreenMovementSpring,
        duration_secs: f64,
        crop: Option<CursorCropMap>,
    ) -> Self {
        Self::new_for_recording_clip(
            zoom_segments,
            timeline,
            cursor_events,
            spring,
            duration_secs,
            crop,
            RecordingClipSelection {
                recording_clip: None,
                prefer_outgoing: false,
            },
        )
    }

    fn new_for_recording_clip(
        zoom_segments: &[ZoomSegment],
        timeline: Option<&TimelineConfiguration>,
        cursor_events: &CursorEvents,
        spring: ScreenMovementSpring,
        duration_secs: f64,
        crop: Option<CursorCropMap>,
        selection: RecordingClipSelection,
    ) -> Self {
        let RecordingClipSelection {
            recording_clip,
            prefer_outgoing,
        } = selection;
        let mut zoom_segments = zoom_segments.to_vec();
        zoom_segments.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));

        let time_map = build_time_map(timeline);
        let mut cursor_moves: Vec<_> = cursor_events
            .moves
            .iter()
            .filter(|event| event.x.is_finite() && event.y.is_finite() && event.time_ms.is_finite())
            .cloned()
            .collect();
        cursor_moves.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
        let clusters = zoom_segments
            .iter()
            .map(|segment| match segment.mode {
                ZoomMode::Auto => {
                    let recording_start = map_timeline_to_recording_secs(
                        &time_map,
                        segment.start,
                        recording_clip,
                        prefer_outgoing,
                    );
                    let recording_end = map_timeline_to_recording_secs(
                        &time_map,
                        segment.end,
                        recording_clip,
                        prefer_outgoing,
                    )
                    .max(recording_start);
                    Some(build_clusters(
                        cursor_events,
                        recording_start,
                        recording_end,
                        segment.amount,
                        segment.edge_snap_ratio,
                        crop,
                    ))
                }
                ZoomMode::Manual { .. } => None,
            })
            .collect();

        let duration_secs = if duration_secs.is_finite() {
            duration_secs.max(0.0)
        } else {
            0.0
        };
        // One sample per step across the duration, plus one trailing sample so
        // a lookup right at the end always has a lerp partner.
        let total_samples = (duration_secs * 1000.0 / STEP_MS).ceil() as usize + 2;
        if zoom_segments.is_empty() {
            return Self {
                samples: vec![TimelineSample {
                    amount: 1.0,
                    center: XY::new(0.5, 0.5),
                    activity: 0.0,
                    snapped: false,
                }],
                state: None,
                zoom_segments,
                clusters,
                cursor_moves,
                cursor_crop: crop,
                time_map,
                recording_clip,
                prefer_outgoing,
                total_samples: 1,
            };
        }

        let spring_config = SpringMassDamperSimulationConfig {
            tension: spring.stiffness,
            mass: spring.mass,
            friction: spring.damping,
        };

        let mut timeline = Self {
            samples: Vec::new(),
            state: None,
            zoom_segments,
            clusters,
            cursor_moves,
            cursor_crop: crop,
            time_map,
            recording_clip,
            prefer_outgoing,
            total_samples,
        };

        // Seed the simulations at rest on the t=0 target so the very first
        // frame is already correct and `samples` is never empty.
        let initial = timeline.targets_at(0.0, XY::new(0.5, 0.5));
        let mut center_sim = SpringMassDamperSimulation::new(spring_config);
        center_sim.set_position(initial.center);
        center_sim.set_velocity(XY::new(0.0, 0.0));
        center_sim.set_target_position(initial.center);

        let mut aux_sim = SpringMassDamperSimulation::new(spring_config);
        aux_sim.set_position(XY::new(initial.amount, initial.activity));
        aux_sim.set_velocity(XY::new(0.0, 0.0));
        aux_sim.set_target_position(XY::new(initial.amount, initial.activity));

        timeline.samples.push(TimelineSample {
            amount: initial.amount.max(1.0),
            center: XY::new(
                initial.center.x.clamp(0.0, 1.0),
                initial.center.y.clamp(0.0, 1.0),
            ),
            activity: initial.activity.clamp(0.0, 1.0),
            snapped: false,
        });
        timeline.state = Some(PrecomputeState {
            center_sim,
            aux_sim,
            held_center_target: initial.center,
        });
        timeline
    }

    /// Convenience constructor pulling zoom segments, edit mapping, spring
    /// config and crop mapping out of a [`ProjectConfiguration`].
    /// `screen_size` is the raw recorded display size in px
    /// (`RenderOptions::screen_size`), needed to normalize the project's
    /// pixel-space crop into cursor UV space.
    pub fn from_project(
        project: &ProjectConfiguration,
        cursor_events: &CursorEvents,
        duration_secs: f64,
        screen_size: XY<u32>,
    ) -> Self {
        Self::from_project_for_recording_clip(
            project,
            cursor_events,
            duration_secs,
            screen_size,
            None,
            false,
        )
    }

    pub fn from_project_for_clip(
        project: &ProjectConfiguration,
        cursor_events: &CursorEvents,
        duration_secs: f64,
        screen_size: XY<u32>,
        recording_clip: u32,
    ) -> Self {
        Self::from_project_for_recording_clip(
            project,
            cursor_events,
            duration_secs,
            screen_size,
            Some(recording_clip),
            false,
        )
    }

    pub fn from_project_for_outgoing_clip(
        project: &ProjectConfiguration,
        cursor_events: &CursorEvents,
        duration_secs: f64,
        screen_size: XY<u32>,
        recording_clip: u32,
    ) -> Self {
        Self::from_project_for_recording_clip(
            project,
            cursor_events,
            duration_secs,
            screen_size,
            Some(recording_clip),
            true,
        )
    }

    fn from_project_for_recording_clip(
        project: &ProjectConfiguration,
        cursor_events: &CursorEvents,
        duration_secs: f64,
        screen_size: XY<u32>,
        recording_clip: Option<u32>,
        prefer_outgoing: bool,
    ) -> Self {
        let crop = project
            .background
            .crop
            .as_ref()
            .and_then(|crop| CursorCropMap::from_crop(crop, screen_size));
        Self::new_for_recording_clip(
            project
                .timeline
                .as_ref()
                .map(|t| t.zoom_segments.as_slice())
                .unwrap_or(&[]),
            project.timeline.as_ref(),
            cursor_events,
            project.screen_movement_spring,
            duration_secs,
            crop,
            RecordingClipSelection {
                recording_clip,
                prefer_outgoing,
            },
        )
    }

    /// Extends the precomputed cache to cover `timeline_secs`. Amortized and
    /// cheap (125 trivial steps per second of content); a no-op once the
    /// requested range — or the whole duration — is covered.
    pub fn ensure_precomputed_until(&mut self, timeline_secs: f32) {
        if self.state.is_none() {
            return;
        }
        let need_ms = (f64::from(timeline_secs).max(0.0)) * 1000.0;
        // +2: floor index plus its lerp partner.
        let need_samples = ((need_ms / STEP_MS).ceil() as usize + 2).min(self.total_samples);
        while self.samples.len() < need_samples && self.state.is_some() {
            self.advance_one_step();
        }
    }

    /// Precomputes the full duration.
    pub fn precompute(&mut self) {
        while self.state.is_some() {
            self.advance_one_step();
        }
    }

    /// Samples the transform at a TIMELINE timestamp: binary index + lerp over
    /// the precomputed steps. No allocation, no locks, no simulation work —
    /// safe for the render hot path. Times outside the precomputed range clamp
    /// to the nearest cached sample.
    pub fn sample(&self, timeline_secs: f32) -> InterpolatedZoom {
        self.sample_inner(timeline_secs, None)
    }

    pub fn sample_with_cursor(
        &self,
        timeline_secs: f32,
        rendered_cursor: XY<f64>,
    ) -> InterpolatedZoom {
        let cursor = self
            .active_auto_segment(f64::from(timeline_secs))
            .map(|segment| {
                (
                    self.map_cursor(rendered_cursor),
                    segment.edge_snap_ratio as f32,
                )
            });
        self.sample_inner(timeline_secs, cursor)
    }

    fn sample_inner(
        &self,
        timeline_secs: f32,
        rendered_cursor: Option<(XY<f32>, f32)>,
    ) -> InterpolatedZoom {
        let Some(last) = self.samples.len().checked_sub(1) else {
            return InterpolatedZoom {
                t: 0.0,
                bounds: SegmentBounds::default(),
            };
        };

        let pos = (f64::from(timeline_secs).max(0.0)) * 1000.0 / STEP_MS;
        let idx = (pos as usize).min(last);
        let next = (idx + 1).min(last);
        let frac = (pos - idx as f64).clamp(0.0, 1.0) as f32;

        let a = self.samples[idx];
        let b = self.samples[next];

        let amount = a.amount + (b.amount - a.amount) * frac;
        let center_x = a.center.x + (b.center.x - a.center.x) * frac;
        let center_y = a.center.y + (b.center.y - a.center.y) * frac;
        let activity = a.activity + (b.activity - a.activity) * frac;
        let center = if let Some((cursor, safe_zone_inset_ratio)) = rendered_cursor {
            recenter_on_cursor_outside_safe_zone(
                XY::new(center_x, center_y),
                cursor,
                amount,
                safe_zone_inset_ratio,
            )
        } else {
            self.active_auto_cursor_at(f64::from(timeline_secs))
                .map_or(XY::new(center_x, center_y), |cursor| {
                    constrain_center_to_cursor(XY::new(center_x, center_y), cursor, amount)
                })
        };

        InterpolatedZoom {
            t: f64::from(activity).clamp(0.0, 1.0),
            bounds: SegmentBounds::from_amount_center(
                f64::from(amount),
                XY::new(f64::from(center.x), f64::from(center.y)),
            ),
        }
    }

    /// Whether any precomputed step in `[from_secs, to_secs]` was an instant
    /// snap (no spring). Motion-effect consumers can use this to suppress
    /// velocity-derived effects across intentional discontinuities.
    pub fn snapped_within(&self, from_secs: f32, to_secs: f32) -> bool {
        if self.samples.is_empty() {
            return false;
        }
        let last = self.samples.len() - 1;
        let lo_ms = f64::from(from_secs.min(to_secs)).max(0.0) * 1000.0;
        let hi_ms = f64::from(from_secs.max(to_secs)).max(0.0) * 1000.0;
        let lo = ((lo_ms / STEP_MS) as usize).min(last);
        let hi = ((hi_ms / STEP_MS).ceil() as usize).min(last);
        self.samples[lo..=hi].iter().any(|s| s.snapped)
    }

    fn advance_one_step(&mut self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let held_center = state.held_center_target;

        let step_index = self.samples.len();
        let step_secs = step_index as f64 * STEP_MS / 1000.0;
        let targets = self.targets_at(step_secs, held_center);

        let Some(state) = self.state.as_mut() else {
            return;
        };

        if targets.segment_active {
            state.held_center_target = targets.center;
        }

        state.center_sim.set_target_position(targets.center);
        state
            .aux_sim
            .set_target_position(XY::new(targets.amount, targets.activity));

        if targets.snap {
            // Instant animation: park the springs on the target with zero
            // velocity so no motion (or motion-derived effect) leaks through.
            state.center_sim.set_position(targets.center);
            state.center_sim.set_velocity(XY::new(0.0, 0.0));
            state
                .aux_sim
                .set_position(XY::new(targets.amount, targets.activity));
            state.aux_sim.set_velocity(XY::new(0.0, 0.0));
        } else {
            // While the amount spring sits at identity the viewport shows the
            // whole frame no matter where the center is — the center channel
            // is unobservable, so track its target instantly (free pre-aim).
            // An incoming zoom then launches already aimed at its focus and
            // scales straight toward it, instead of zooming about the stale
            // center and dragging over at high magnification (a huge late pan
            // that also detonated the motion blur). The epsilon bounds any
            // theoretical pop to sub-pixel: (amount - 1) * |center jump| of
            // the card, i.e. < 0.05% of the card size.
            if state.aux_sim.position.x <= CENTER_PREAIM_MAX_AMOUNT {
                state.center_sim.set_position(targets.center);
                state.center_sim.set_velocity(XY::new(0.0, 0.0));
            }
            let center_step_ms = if targets.fast_cursor_follow {
                STEP_MS as f32 * CURSOR_FOLLOW_RESPONSE_MULTIPLIER
            } else {
                STEP_MS as f32
            };
            state.center_sim.run(center_step_ms);
            state.aux_sim.run(STEP_MS as f32);
        }

        // Geometric safety: a sprung amount below 1 would show out-of-bounds
        // background ("bounce-out"), so clamp and kill velocity on that axis
        // only. Centers are clamped per-sample instead (the spring target is
        // always in-band, so overshoot is transient and tiny).
        if state.aux_sim.position.x < 1.0 {
            state.aux_sim.position.x = 1.0;
            state.aux_sim.velocity.x = 0.0;
        }

        if let Some(cursor) = targets.cursor {
            let constrained = constrain_center_to_cursor(
                state.center_sim.position,
                cursor,
                state.aux_sim.position.x,
            );
            if constrained.x != state.center_sim.position.x {
                state.center_sim.position.x = constrained.x;
                state.center_sim.velocity.x = 0.0;
            }
            if constrained.y != state.center_sim.position.y {
                state.center_sim.position.y = constrained.y;
                state.center_sim.velocity.y = 0.0;
            }
        }

        self.samples.push(TimelineSample {
            amount: state.aux_sim.position.x,
            center: XY::new(
                state.center_sim.position.x.clamp(0.0, 1.0),
                state.center_sim.position.y.clamp(0.0, 1.0),
            ),
            activity: state.aux_sim.position.y.clamp(0.0, 1.0),
            snapped: targets.snap,
        });

        if self.samples.len() >= self.total_samples {
            self.state = None;
        }
    }

    fn targets_at(&self, timeline_secs: f64, held_center: XY<f32>) -> StepTargets {
        // Same active predicate `SegmentsCursor` used: (start, end].
        let active = self
            .zoom_segments
            .iter()
            .position(|s| timeline_secs > s.start && timeline_secs <= s.end);

        let snap = self.zoom_segments.iter().any(|s| {
            s.instant_animation
                && timeline_secs >= s.start - INSTANT_SNAP_WINDOW_SECS
                && timeline_secs <= s.end + INSTANT_SNAP_WINDOW_SECS
        });

        match active {
            Some(index) => {
                let segment = &self.zoom_segments[index];
                let amount = if segment.amount.is_finite() {
                    segment.amount.max(1.0)
                } else {
                    1.0
                };
                let (center, cursor, fast_cursor_follow) = match segment.mode {
                    ZoomMode::Manual { x, y } => (
                        (f64::from(x).clamp(0.0, 1.0), f64::from(y).clamp(0.0, 1.0)),
                        None,
                        false,
                    ),
                    ZoomMode::Auto => {
                        let recording_ms = map_timeline_to_recording_secs(
                            &self.time_map,
                            timeline_secs,
                            self.recording_clip,
                            self.prefer_outgoing,
                        ) * 1000.0;
                        let cluster = self.clusters[index]
                            .as_deref()
                            .and_then(|clusters| cluster_at_time(clusters, recording_ms));
                        let focus = cluster.map(ClickCluster::center).unwrap_or(FALLBACK_FOCUS);
                        (
                            focus_to_travel_center(focus, amount),
                            self.cursor_at_recording_ms(recording_ms),
                            cluster.is_some_and(|cluster| cluster.follows_cursor),
                        )
                    }
                };

                StepTargets {
                    amount: amount as f32,
                    center: XY::new(center.0 as f32, center.1 as f32),
                    activity: 1.0,
                    segment_active: true,
                    snap,
                    cursor,
                    fast_cursor_follow,
                }
            }
            None => StepTargets {
                amount: 1.0,
                // Hold the last active framing while zooming out so the
                // outgoing shot stays anchored (irrelevant once amount = 1).
                center: held_center,
                activity: 0.0,
                segment_active: false,
                snap,
                cursor: None,
                fast_cursor_follow: false,
            },
        }
    }

    fn cursor_at_recording_ms(&self, recording_ms: f64) -> Option<XY<f32>> {
        let cursor = cursor_position_at(&self.cursor_moves, recording_ms)?;
        Some(self.map_cursor(cursor))
    }

    fn map_cursor(&self, cursor: XY<f64>) -> XY<f32> {
        let (x, y) = self
            .cursor_crop
            .map_or((cursor.x, cursor.y), |crop| crop.map(cursor.x, cursor.y));
        XY::new(x.clamp(0.0, 1.0) as f32, y.clamp(0.0, 1.0) as f32)
    }

    fn active_auto_segment(&self, timeline_secs: f64) -> Option<&ZoomSegment> {
        self.zoom_segments
            .iter()
            .find(|segment| timeline_secs > segment.start && timeline_secs <= segment.end)
            .filter(|segment| matches!(segment.mode, ZoomMode::Auto))
    }

    fn active_auto_cursor_at(&self, timeline_secs: f64) -> Option<XY<f32>> {
        self.active_auto_segment(timeline_secs).and_then(|_| {
            let recording_ms = map_timeline_to_recording_secs(
                &self.time_map,
                timeline_secs,
                self.recording_clip,
                self.prefer_outgoing,
            ) * 1000.0;
            self.cursor_at_recording_ms(recording_ms)
        })
    }
}

#[cfg(test)]
mod tests {
    use cap_project::{
        ClipTransition, ClipTransitionType, CursorClickEvent, CursorMoveEvent, GlideDirection,
        TimelineSegment, ZoomMode,
    };

    use super::*;

    fn manual_segment(start: f64, end: f64, amount: f64, x: f64, y: f64) -> ZoomSegment {
        ZoomSegment {
            start,
            end,
            amount,
            mode: ZoomMode::Manual {
                x: x as f32,
                y: y as f32,
            },
            glide_direction: GlideDirection::default(),
            glide_speed: 0.5,
            instant_animation: false,
            edge_snap_ratio: 0.25,
        }
    }

    fn auto_segment(start: f64, end: f64, amount: f64) -> ZoomSegment {
        ZoomSegment {
            mode: ZoomMode::Auto,
            ..manual_segment(start, end, amount, 0.5, 0.5)
        }
    }

    fn move_event(time_ms: f64, x: f64, y: f64) -> CursorMoveEvent {
        CursorMoveEvent {
            active_modifiers: vec![],
            cursor_id: "default".to_string(),
            time_ms,
            x,
            y,
        }
    }

    fn click_event(time_ms: f64) -> CursorClickEvent {
        CursorClickEvent {
            active_modifiers: vec![],
            cursor_num: 0,
            cursor_id: "default".to_string(),
            time_ms,
            down: true,
        }
    }

    fn timeline_for(
        segments: &[ZoomSegment],
        cursor: &CursorEvents,
        duration: f64,
    ) -> ZoomTransformTimeline {
        ZoomTransformTimeline::new(
            segments,
            None,
            cursor,
            ScreenMovementSpring::default(),
            duration,
            None,
        )
    }

    #[test]
    fn empty_zoom_timeline_stays_constant_without_precompute_work() {
        let mut timeline = timeline_for(&[], &CursorEvents::default(), 60.0 * 60.0);
        timeline.ensure_precomputed_until(60.0 * 60.0);

        assert!(timeline.state.is_none());
        assert_eq!(timeline.samples.len(), 1);
        assert_eq!(timeline.sample(60.0 * 60.0).display_amount(), 1.0);
    }

    /// Max |value delta| and |slope delta| between adjacent 8ms sample
    /// intervals across the whole precomputed range, measured on the VISIBLE
    /// viewport rect (bounds corners + amount), not the latent channels: the
    /// center channel deliberately snaps to its target while the amount sits
    /// at identity (free pre-aim, geometrically invisible), and bounds are
    /// what the renderer — and the motion-blur velocity analysis — consume.
    fn max_step_discontinuities(timeline: &ZoomTransformTimeline) -> (f64, f64) {
        let step_secs = STEP_MS / 1000.0;
        let values: Vec<[f64; 5]> = timeline
            .samples
            .iter()
            .map(|s| {
                let bounds = SegmentBounds::from_amount_center(
                    f64::from(s.amount),
                    XY::new(f64::from(s.center.x), f64::from(s.center.y)),
                );
                [
                    f64::from(s.amount),
                    bounds.top_left.x,
                    bounds.top_left.y,
                    bounds.bottom_right.x,
                    bounds.bottom_right.y,
                ]
            })
            .collect();

        let mut max_value_jump = 0.0f64;
        let mut max_slope_jump = 0.0f64;
        for window in values.windows(3) {
            for channel in 0..5 {
                let v0 = window[0][channel];
                let v1 = window[1][channel];
                let v2 = window[2][channel];
                let slope_a = (v1 - v0) / step_secs;
                let slope_b = (v2 - v1) / step_secs;
                max_value_jump = max_value_jump.max((v1 - v0).abs()).max((v2 - v1).abs());
                max_slope_jump = max_slope_jump.max((slope_b - slope_a).abs());
            }
        }
        (max_value_jump, max_slope_jump)
    }

    fn assert_viewport_in_bounds(zoom: &InterpolatedZoom, context: &str) {
        // The display rect must cover the full output on both axes; anything
        // less shows out-of-bounds background.
        assert!(
            zoom.bounds.top_left.x <= 1e-6 && zoom.bounds.top_left.y <= 1e-6,
            "{context}: top_left out of bounds: {:?}",
            zoom.bounds
        );
        assert!(
            zoom.bounds.bottom_right.x >= 1.0 - 1e-6 && zoom.bounds.bottom_right.y >= 1.0 - 1e-6,
            "{context}: bottom_right out of bounds: {:?}",
            zoom.bounds
        );
        assert!(
            zoom.display_amount() >= 1.0 - 1e-6,
            "{context}: amount below 1: {}",
            zoom.display_amount()
        );
        assert!(
            (0.0..=1.0).contains(&zoom.t),
            "{context}: t out of [0,1]: {}",
            zoom.t
        );
    }

    #[test]
    fn sequential_and_seek_precompute_are_identical() {
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.2, 0.3),
                move_event(1500.0, 0.8, 0.7),
                move_event(4000.0, 0.4, 0.9),
            ],
            clicks: vec![click_event(1200.0), click_event(3600.0)],
        };
        let segments = vec![
            auto_segment(1.0, 4.0, 2.0),
            manual_segment(6.0, 8.0, 3.0, 0.2, 0.8),
        ];

        let mut sequential = timeline_for(&segments, &cursor, 10.0);
        let mut chunk_time = 0.0f32;
        while chunk_time < 10.5 {
            sequential.ensure_precomputed_until(chunk_time);
            chunk_time += 0.037; // deliberately not a multiple of the step
        }
        sequential.precompute();

        let mut seeked = timeline_for(&segments, &cursor, 10.0);
        seeked.precompute();

        assert_eq!(sequential.samples.len(), seeked.samples.len());
        for (index, (a, b)) in sequential
            .samples
            .iter()
            .zip(seeked.samples.iter())
            .enumerate()
        {
            assert_eq!(a.amount.to_bits(), b.amount.to_bits(), "amount @ {index}");
            assert_eq!(
                a.center.x.to_bits(),
                b.center.x.to_bits(),
                "center.x @ {index}"
            );
            assert_eq!(
                a.center.y.to_bits(),
                b.center.y.to_bits(),
                "center.y @ {index}"
            );
            assert_eq!(
                a.activity.to_bits(),
                b.activity.to_bits(),
                "activity @ {index}"
            );
        }

        // Sampling backwards after a forward precompute is pure cache lookup
        // and must equal a fresh sample of the same time.
        let early = sequential.sample(1.5);
        let late = sequential.sample(9.0);
        let early_again = sequential.sample(1.5);
        assert_eq!(early.bounds, early_again.bounds);
        assert!(late.display_amount().is_finite());
    }

    #[test]
    fn velocity_is_continuous_across_segment_boundaries() {
        // The old easing scheme had C1 breaks at segment start/end that
        // required `segment_end_focus` patches; the spring must not. Bounds
        // derived from the default spring: |accel| <= (k*disp + c*v)/m with
        // disp <= 2, v <= ~7/s => ~300/s^2, i.e. slope deltas of at most
        // ~2.4/s between adjacent 8ms intervals. A duration-eased jump would
        // show up as a slope delta of tens per second.
        let cursor = CursorEvents {
            moves: vec![move_event(0.0, 0.7, 0.4)],
            clicks: vec![],
        };
        let segments = vec![
            auto_segment(1.0, 3.0, 3.0),
            manual_segment(3.5, 5.0, 2.0, 0.1, 0.9), // retarget mid-flight of the zoom-out
        ];
        let mut timeline = timeline_for(&segments, &cursor, 7.0);
        timeline.precompute();

        let (max_value_jump, max_slope_jump) = max_step_discontinuities(&timeline);
        assert!(
            max_value_jump < 0.1,
            "C0 violated: sample-to-sample jump {max_value_jump}"
        );
        assert!(
            max_slope_jump < 4.0,
            "velocity discontinuity across retargets: slope jump {max_slope_jump}/s"
        );
    }

    #[test]
    fn viewport_stays_in_bounds_for_all_t() {
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.02, 0.02),
                move_event(1000.0, 0.98, 0.03),
                move_event(2500.0, 0.97, 0.96),
                move_event(4000.0, 0.01, 0.99),
            ],
            clicks: vec![click_event(900.0), click_event(2400.0), click_event(3900.0)],
        };
        // Edge-hugging focus + manual corners + amounts up to 4x.
        let segments = vec![
            auto_segment(0.5, 4.5, 4.0),
            manual_segment(5.0, 6.0, 2.0, 0.0, 0.0),
            manual_segment(6.0, 7.0, 3.0, 1.0, 1.0),
        ];
        let mut timeline = timeline_for(&segments, &cursor, 9.0);
        timeline.precompute();

        let mut t = 0.0f32;
        while t <= 9.0 {
            let zoom = timeline.sample(t);
            assert_viewport_in_bounds(&zoom, &format!("t={t}"));
            t += 0.003; // off-grid sampling exercises the lerp too
        }
    }

    #[test]
    fn instant_segments_snap_without_spring() {
        let cursor = CursorEvents::default();
        let mut segment = manual_segment(1.0, 2.0, 2.5, 0.5, 0.5);
        segment.instant_animation = true;
        let mut timeline = timeline_for(&[segment], &cursor, 4.0);
        timeline.precompute();

        // One step into the segment the amount is already at target.
        let inside = timeline.sample(1.016);
        assert!(
            (inside.display_amount() - 2.5).abs() < 1e-4,
            "instant zoom-in did not snap: {}",
            inside.display_amount()
        );

        // One step past the end (still inside the +-100ms snap window) it is
        // already back at identity.
        let after = timeline.sample(2.016);
        assert!(
            (after.display_amount() - 1.0).abs() < 1e-4,
            "instant zoom-out did not snap: {}",
            after.display_amount()
        );

        assert!(timeline.snapped_within(0.95, 1.05));
        assert!(timeline.snapped_within(1.95, 2.05));
        assert!(!timeline.snapped_within(3.0, 4.0));
    }

    #[test]
    fn cluster_re_aim_is_smooth() {
        // Two click clusters far apart inside one long auto segment: the
        // target re-aims discretely, the sprung center must move smoothly.
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.1, 0.1),
                move_event(2000.0, 0.12, 0.12),
                move_event(5000.0, 0.9, 0.9),
                move_event(8000.0, 0.88, 0.88),
            ],
            clicks: vec![click_event(1000.0), click_event(6000.0)],
        };
        let segments = vec![auto_segment(0.5, 9.0, 2.0)];
        let mut timeline = timeline_for(&segments, &cursor, 10.0);
        timeline.precompute();

        let (max_value_jump, max_slope_jump) = max_step_discontinuities(&timeline);
        assert!(max_value_jump < 0.06, "re-aim value jump {max_value_jump}");
        assert!(max_slope_jump < 4.0, "re-aim slope jump {max_slope_jump}");

        // And it actually re-aims: early framing differs from late framing.
        let early = timeline.sample(3.0);
        let late = timeline.sample(8.9);
        assert!(
            (early.bounds.top_left.x - late.bounds.top_left.x).abs() > 0.05,
            "cluster re-aim never moved the framing"
        );
    }

    /// Raw-UV viewport of a sampled zoom: (left, top, size).
    fn visible_viewport(zoom: &InterpolatedZoom) -> (f64, f64, f64) {
        let amount = zoom.display_amount();
        (
            -zoom.bounds.top_left.x / amount,
            -zoom.bounds.top_left.y / amount,
            1.0 / amount,
        )
    }

    #[test]
    fn fast_cursor_stays_inside_viewport_at_every_zoom_amount() {
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.5, 0.5),
                move_event(4000.0, 0.5, 0.5),
                move_event(4120.0, 0.95, 0.95),
                move_event(8000.0, 0.95, 0.95),
            ],
            clicks: vec![],
        };

        for amount in [1.5, 2.0, 4.0, 8.0] {
            let segments = vec![auto_segment(1.0, 7.0, amount)];
            let mut timeline = timeline_for(&segments, &cursor, 8.0);
            timeline.precompute();

            for time_ms in 4000..=4120 {
                let zoom = timeline.sample(time_ms as f32 / 1000.0);
                let cursor = cursor_position_at(&cursor.moves, time_ms as f64).unwrap();
                let (left, top, size) = visible_viewport(&zoom);
                assert!(
                    cursor.x >= left - 1e-6 && cursor.x <= left + size + 1e-6,
                    "cursor {} outside viewport [{left}, {}] at {amount}x and {time_ms}ms",
                    cursor.x,
                    left + size
                );
                assert!(
                    cursor.y >= top - 1e-6 && cursor.y <= top + size + 1e-6,
                    "cursor {} outside viewport [{top}, {}] at {amount}x and {time_ms}ms",
                    cursor.y,
                    top + size
                );
            }
        }
    }

    #[test]
    fn safe_zone_holds_until_exit_then_recenters_on_the_final_cursor() {
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.5, 0.5),
                move_event(100.0, 0.6, 0.58),
                move_event(200.0, 0.7, 0.58),
                move_event(300.0, 0.72, 0.6),
            ],
            clicks: vec![],
        };
        let clusters = build_clusters(&cursor, 0.0, 1.0, 2.0, 0.25, None);

        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].center(), (0.5, 0.5));
        assert_eq!(clusters[1].center(), (0.72, 0.6));
    }

    #[test]
    fn follow_target_places_the_cursor_at_the_viewport_center() {
        for amount in [2.0, 4.0, 8.0] {
            let cursor = (0.7, 0.6);
            let center = focus_to_travel_center(cursor, amount);
            let viewport_center = (
                (center.0 * (amount - 1.0) + 0.5) / amount,
                (center.1 * (amount - 1.0) + 0.5) / amount,
            );

            assert!((viewport_center.0 - cursor.0).abs() < 1e-9);
            assert!((viewport_center.1 - cursor.1).abs() < 1e-9);
        }
    }

    #[test]
    fn rendered_cursor_recenters_both_axes_after_leaving_the_safe_zone() {
        let cursor = CursorEvents {
            moves: vec![move_event(0.0, 0.5, 0.5)],
            clicks: vec![],
        };
        let segments = vec![auto_segment(0.5, 5.0, 2.0)];
        let mut timeline = timeline_for(&segments, &cursor, 6.0);
        timeline.precompute();

        let rendered_cursor = XY::new(0.7, 0.6);
        let zoom = timeline.sample_with_cursor(3.0, rendered_cursor);
        let (left, top, size) = visible_viewport(&zoom);

        assert!((left + size * 0.5 - rendered_cursor.x).abs() < 1e-6);
        assert!((top + size * 0.5 - rendered_cursor.y).abs() < 1e-6);
    }

    #[test]
    fn smoothed_cursor_stays_inside_viewport_between_precomputed_steps() {
        let cursor = CursorEvents {
            moves: vec![
                move_event(0.0, 0.15, 0.15),
                move_event(4000.0, 0.15, 0.15),
                move_event(4120.0, 0.95, 0.95),
                move_event(8000.0, 0.95, 0.95),
            ],
            clicks: vec![],
        };
        let rendered_cursor = crate::cursor_interpolation::PrecomputedCursorTimeline::new(
            &cursor,
            Some(SpringMassDamperSimulationConfig {
                tension: 470.0,
                mass: 3.0,
                friction: 70.0,
            }),
            None,
        );
        let segments = vec![auto_segment(1.0, 7.0, 2.0)];
        let mut timeline = ZoomTransformTimeline::new(
            &segments,
            None,
            &cursor,
            ScreenMovementSpring::default(),
            8.0,
            None,
        );
        timeline.precompute();

        for time_ms in 4000..=4300 {
            let time = time_ms as f32 / 1000.0;
            let position = rendered_cursor.interpolate(time).unwrap().position.coord;
            let zoom = timeline.sample_with_cursor(time, position);
            let (left, top, size) = visible_viewport(&zoom);
            assert!(
                position.x >= left - 1e-6
                    && position.x <= left + size + 1e-6
                    && position.y >= top - 1e-6
                    && position.y <= top + size + 1e-6,
                "smoothed cursor {position:?} outside viewport at {time_ms}ms"
            );
        }
    }

    #[test]
    fn hovering_into_a_corner_re_aims_without_a_click() {
        // Regression for the real-recording report: clicks early near the
        // center, then the cursor HOVERS (no click) into the bottom-right
        // corner mid-segment. All movement participates in clustering, so
        // leaving the dead-zone box must re-aim the camera and bring the
        // hovered corner into the settled viewport.
        let corner = (0.95, 0.9);
        let mut moves = Vec::new();
        for i in 0..50 {
            moves.push(move_event(i as f64 * 100.0, 0.5, 0.5));
        }
        for i in 0..=20 {
            let t = i as f64 / 20.0;
            moves.push(move_event(
                5000.0 + t * 1000.0,
                0.5 + (corner.0 - 0.5) * t,
                0.5 + (corner.1 - 0.5) * t,
            ));
        }
        for i in 1..=30 {
            moves.push(move_event(6000.0 + i as f64 * 200.0, corner.0, corner.1));
        }
        let cursor = CursorEvents {
            moves,
            clicks: vec![click_event(1500.0), click_event(2000.0)],
        };
        let segments = vec![auto_segment(1.0, 15.1, 2.0)];
        let mut timeline = timeline_for(&segments, &cursor, 16.0);
        timeline.precompute();

        // Two seconds after arriving in the corner the spring has settled.
        let settled = timeline.sample(8.0);
        let (left, top, size) = visible_viewport(&settled);
        assert!(
            corner.0 >= left && corner.0 <= left + size,
            "hovered corner x {} outside viewport [{left}, {}]",
            corner.0,
            left + size
        );
        assert!(
            corner.1 >= top && corner.1 <= top + size,
            "hovered corner y {} outside viewport [{top}, {}]",
            corner.1,
            top + size
        );

        // And the framing genuinely moved from the early click-cluster view.
        let early = timeline.sample(4.0);
        let (early_left, early_top, _) = visible_viewport(&early);
        assert!(
            (left - early_left).abs() > 0.1 || (top - early_top).abs() > 0.1,
            "camera never re-aimed toward the hovered corner"
        );
        assert_viewport_in_bounds(&settled, "corner hover settle");
    }

    #[test]
    fn hostile_cursor_data_stays_finite_and_in_bounds() {
        // Simulated/corrupted input: NaN and infinite coordinates, positions
        // far outside the crop (cursor on another monitor), and full-screen
        // teleports between consecutive events (automation tools). The
        // timeline must stay finite and in bounds throughout.
        let moves = vec![
            move_event(0.0, 0.5, 0.5),
            move_event(500.0, f64::NAN, 0.5),
            move_event(600.0, 0.5, f64::INFINITY),
            move_event(1000.0, -3.0, 7.5),
            move_event(1500.0, 0.02, 0.98),
            move_event(1550.0, 0.98, 0.02),
            move_event(1600.0, 0.02, 0.02),
            move_event(1650.0, 0.98, 0.98),
            move_event(4000.0, 1.4, -0.4),
            move_event(8000.0, 0.5, 0.5),
        ];
        let cursor = CursorEvents {
            moves,
            clicks: vec![click_event(f64::NAN), click_event(1500.0)],
        };
        let segments = vec![auto_segment(0.5, 9.0, 2.0)];
        let mut timeline = timeline_for(&segments, &cursor, 10.0);
        timeline.precompute();

        for sample in &timeline.samples {
            assert!(
                sample.amount.is_finite()
                    && sample.center.x.is_finite()
                    && sample.center.y.is_finite()
                    && sample.activity.is_finite(),
                "non-finite sample: {sample:?}"
            );
        }
        let mut t = 0.0f32;
        while t <= 10.0 {
            let zoom = timeline.sample(t);
            assert!(
                zoom.bounds.top_left.x.is_finite() && zoom.bounds.bottom_right.y.is_finite(),
                "non-finite bounds at t={t}"
            );
            assert_viewport_in_bounds(&zoom, &format!("hostile input at t={t}"));
            t += 0.037;
        }
    }

    #[test]
    fn empty_cursor_events_fall_back_to_centered_focus() {
        let cursor = CursorEvents::default();
        let segments = vec![auto_segment(0.5, 5.0, 2.0)];
        let mut timeline = timeline_for(&segments, &cursor, 6.0);
        timeline.precompute();

        // Well after the spring has settled (~1s), framing is centered.
        let settled = timeline.sample(4.5);
        let expected = SegmentBounds::from_amount_center(
            2.0,
            XY::new(
                SegmentBounds::calculate_follow_center(FALLBACK_FOCUS, 0.25).0,
                SegmentBounds::calculate_follow_center(FALLBACK_FOCUS, 0.25).1,
            ),
        );
        assert!((settled.bounds.top_left.x - expected.top_left.x).abs() < 1e-3);
        assert!((settled.bounds.bottom_right.y - expected.bottom_right.y).abs() < 1e-3);
        assert!((settled.t - 1.0).abs() < 1e-3);
    }

    #[test]
    fn steady_state_matches_manual_target_framing() {
        // Parity anchor with the retired easing implementation: a settled
        // manual zoom must land on exactly the same framing formula.
        let cursor = CursorEvents::default();
        let segments = vec![manual_segment(0.5, 6.0, 2.0, 0.3, 0.7)];
        let mut timeline = timeline_for(&segments, &cursor, 7.0);
        timeline.precompute();

        let settled = timeline.sample(5.5);
        let expected = SegmentBounds::from_amount_center(2.0, XY::new(0.3, 0.7));
        assert!((settled.bounds.top_left.x - expected.top_left.x).abs() < 1e-3);
        assert!((settled.bounds.top_left.y - expected.top_left.y).abs() < 1e-3);
        assert!((settled.bounds.bottom_right.x - expected.bottom_right.x).abs() < 1e-3);
        assert!((settled.bounds.bottom_right.y - expected.bottom_right.y).abs() < 1e-3);
    }

    #[test]
    fn manual_corner_zoom_launches_already_aimed() {
        // A manual zoom to the top-left corner must scale straight into the
        // corner from the first visible frame — not zoom about the stale
        // (centered) framing and pan over at high magnification. While the
        // amount is at identity the center is unobservable, so the timeline
        // pre-aims it; with a corner-flush target (0,0) the viewport's
        // top-left then stays pinned to the content's top-left for the whole
        // ramp.
        let cursor = CursorEvents::default();
        let segments = vec![manual_segment(1.0, 5.0, 2.862, 0.0, 0.0)];
        let mut timeline = timeline_for(&segments, &cursor, 6.0);
        timeline.precompute();

        for t in [1.05f32, 1.2, 1.5, 2.0, 3.0] {
            let z = timeline.sample(t);
            assert!(
                z.bounds.top_left.x.abs() < 5e-3 && z.bounds.top_left.y.abs() < 5e-3,
                "viewport must stay corner-anchored during the ramp at t={t}: {:?}",
                z.bounds
            );
        }

        // Guard against the assertion above passing trivially (identity
        // bounds also have top_left = 0): the zoom must actually engage and
        // settle on the full manual amount.
        let settled = timeline.sample(4.5);
        assert!(
            (settled.display_amount() - 2.862).abs() < 1e-2,
            "zoom failed to settle on the manual amount: {}",
            settled.display_amount()
        );

        // And the pre-aim must never cause a visible pop: the viewport stays
        // continuous across the segment start. Thresholds mirror the other
        // continuity tests — a 2.862x ramp legitimately sweeps ~0.054/step at
        // peak spring velocity, while a center pop at (say) amount 1.5 would
        // jump ~0.25 in one step.
        let (max_value_jump, max_slope_jump) = max_step_discontinuities(&timeline);
        assert!(
            max_value_jump < 0.1,
            "pre-aim introduced a step discontinuity: {max_value_jump}"
        );
        assert!(
            max_slope_jump < 4.0,
            "pre-aim introduced a velocity discontinuity: {max_slope_jump}/s"
        );
    }

    #[test]
    fn zoom_out_returns_to_identity_and_zero_activity() {
        let cursor = CursorEvents::default();
        let segments = vec![manual_segment(0.5, 2.0, 2.0, 0.5, 0.5)];
        let mut timeline = timeline_for(&segments, &cursor, 6.0);
        timeline.precompute();

        let rest = timeline.sample(5.5);
        assert!((rest.display_amount() - 1.0).abs() < 1e-4);
        assert!(rest.t < 1e-3);
        assert!((rest.bounds.top_left.x).abs() < 1e-4);
        assert!((rest.bounds.bottom_right.x - 1.0).abs() < 1e-4);
    }

    #[test]
    fn degenerate_segments_do_not_break_the_timeline() {
        let cursor = CursorEvents {
            moves: vec![move_event(0.0, 0.5, 0.5)],
            clicks: vec![],
        };
        let segments = vec![
            manual_segment(1.0, 1.0, 2.0, 0.5, 0.5), // zero duration
            manual_segment(3.0, 2.5, 2.0, 0.5, 0.5), // reversed
            auto_segment(100.0, 105.0, 2.0),         // beyond duration
        ];
        let mut timeline = timeline_for(&segments, &cursor, 5.0);
        timeline.precompute();

        let mut t = 0.0f32;
        while t <= 5.0 {
            let zoom = timeline.sample(t);
            assert_viewport_in_bounds(&zoom, &format!("degenerate t={t}"));
            // None of these segments can activate, so the timeline is identity.
            assert!(
                (zoom.display_amount() - 1.0).abs() < 1e-6,
                "degenerate segment activated at t={t}"
            );
            t += 0.05;
        }

        // Zero-duration timelines (screenshot paths) still sample safely.
        let mut zero = timeline_for(&[], &CursorEvents::default(), 0.0);
        zero.ensure_precomputed_until(1.0);
        let frame0 = zero.sample(0.0);
        assert!((frame0.display_amount() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sampling_before_precompute_clamps_to_seed_instead_of_diverging() {
        // The old focus interpolator silently fell back to a *different*
        // interpolation when precompute had not run (the scrub-path bug).
        // The timeline instead clamps to what is cached — callers must ensure
        // first, but an unensured sample can never disagree with an ensured
        // one at t=0.
        let cursor = CursorEvents::default();
        let segments = vec![manual_segment(0.5, 2.0, 2.0, 0.5, 0.5)];
        let unensured = timeline_for(&segments, &cursor, 6.0);
        let frame0 = unensured.sample(0.0);
        assert!((frame0.display_amount() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn precompute_cost_is_bounded_for_long_projects() {
        use std::time::{Duration, Instant};

        // Synthetic 30-minute project: 10 auto zoom segments and 50k cursor
        // move events with a click every 3 seconds.
        let duration_secs = 1800.0;
        let moves: Vec<CursorMoveEvent> = (0..50_000)
            .map(|i| {
                let t_ms = i as f64 * (duration_secs * 1000.0 / 50_000.0);
                move_event(
                    t_ms,
                    ((i as f64 * 0.0011).sin() + 1.0) / 2.0,
                    ((i as f64 * 0.0007).cos() + 1.0) / 2.0,
                )
            })
            .collect();
        let clicks: Vec<CursorClickEvent> =
            (0..600).map(|i| click_event(i as f64 * 3_000.0)).collect();
        let cursor = CursorEvents { moves, clicks };
        let segments: Vec<ZoomSegment> = (0..10)
            .map(|i| auto_segment(i as f64 * 170.0 + 10.0, i as f64 * 170.0 + 40.0, 2.0))
            .collect();

        // Amortization: ensuring an early time must NOT precompute the whole
        // 30-minute timeline. 1s of content is ~127 samples at the 8ms step;
        // the full timeline would be ~225k.
        let construct_start = Instant::now();
        let mut lazy = timeline_for(&segments, &cursor, duration_secs);
        let construct_elapsed = construct_start.elapsed();
        let ensure_start = Instant::now();
        lazy.ensure_precomputed_until(1.0);
        let ensure_elapsed = ensure_start.elapsed();
        let lazy_samples = lazy.samples.len();
        println!(
            "construct: {construct_elapsed:?}, ensure(1s): {ensure_elapsed:?}, samples after early ensure: {lazy_samples}"
        );
        assert!(
            lazy_samples < 1_000,
            "early ensure_precomputed_until precomputed {lazy_samples} samples; amortization is broken"
        );

        // Full precompute of the 30-minute timeline.
        let mut timeline = timeline_for(&segments, &cursor, duration_secs);
        let precompute_start = Instant::now();
        timeline.precompute();
        let precompute_elapsed = precompute_start.elapsed();
        let total_samples = timeline.samples.len();
        println!("precompute({total_samples} samples): {precompute_elapsed:?}");

        // 10k hot-path samples spread across the whole duration.
        let sample_start = Instant::now();
        let mut checksum = 0.0f64;
        for i in 0..10_000u32 {
            let t = (i as f32 * 0.18) % duration_secs as f32;
            checksum += timeline.sample(t).display_amount();
        }
        let sample_elapsed = sample_start.elapsed();
        println!("10k samples: {sample_elapsed:?} (checksum {checksum})");

        // Generous debug-mode bounds; release is far faster.
        assert!(
            precompute_elapsed < Duration::from_millis(500),
            "full precompute took {precompute_elapsed:?} (>500ms) for a 30-minute project"
        );
        assert!(
            sample_elapsed < Duration::from_millis(50),
            "10k samples took {sample_elapsed:?} (>50ms)"
        );
    }

    #[test]
    fn timeline_mapping_filters_clusters_in_recording_time() {
        // Timeline: single segment showing recording range [10s, 20s] at 1x,
        // so timeline t=1s corresponds to recording t=11s. A click at
        // recording 11s must aim the zoom; a click at recording 1s must not.
        let timeline_config = TimelineConfiguration {
            segments: vec![TimelineSegment {
                recording_clip: 0,
                timescale: 1.0,
                start: 10.0,
                end: 20.0,
                name: None,
                speed_audio_mode: None,
            }],
            transitions: vec![],
            zoom_segments: vec![],
            scene_segments: vec![],
            mask_segments: vec![],
            text_segments: vec![],
            caption_segments: vec![],
            keyboard_segments: vec![],
            audio_segments: vec![],
        };
        let cursor = CursorEvents {
            moves: vec![
                move_event(1_000.0, 0.05, 0.05), // recording 1s: outside edit
                move_event(11_000.0, 0.9, 0.9),  // recording 11s: inside edit
            ],
            clicks: vec![click_event(1_000.0), click_event(11_000.0)],
        };
        let segments = vec![auto_segment(0.5, 8.0, 2.0)];
        let mut timeline = ZoomTransformTimeline::new(
            &segments,
            Some(&timeline_config),
            &cursor,
            ScreenMovementSpring::default(),
            10.0,
            None,
        );
        timeline.precompute();

        // Settled framing aims at the in-edit click (0.9, 0.9), not (0.05, 0.05).
        let settled = timeline.sample(6.0);
        let toward_bottom_right = SegmentBounds::from_amount_center(
            2.0,
            XY::new(
                SegmentBounds::calculate_follow_center((0.9, 0.9), 0.25).0,
                SegmentBounds::calculate_follow_center((0.9, 0.9), 0.25).1,
            ),
        );
        assert!(
            (settled.bounds.top_left.x - toward_bottom_right.top_left.x).abs() < 1e-2,
            "expected framing near {:?}, got {:?}",
            toward_bottom_right,
            settled.bounds
        );
    }

    #[test]
    fn timeline_mapping_uses_incoming_source_during_transition() {
        let timeline = TimelineConfiguration {
            segments: vec![
                TimelineSegment {
                    recording_clip: 0,
                    timescale: 1.0,
                    start: 0.0,
                    end: 4.0,
                    name: None,
                    speed_audio_mode: None,
                },
                TimelineSegment {
                    recording_clip: 0,
                    timescale: 1.0,
                    start: 10.0,
                    end: 14.0,
                    name: None,
                    speed_audio_mode: None,
                },
            ],
            transitions: vec![ClipTransition {
                segment_index: 1,
                kind: ClipTransitionType::CrossFade,
                duration: 0.5,
            }],
            zoom_segments: Vec::new(),
            scene_segments: Vec::new(),
            mask_segments: Vec::new(),
            text_segments: Vec::new(),
            caption_segments: Vec::new(),
            keyboard_segments: Vec::new(),
            audio_segments: Vec::new(),
        };
        let map = build_time_map(Some(&timeline));

        assert_eq!(
            map_timeline_to_recording_secs(&map, 3.25, None, false),
            3.25
        );
        assert_eq!(
            map_timeline_to_recording_secs(&map, 3.75, None, false),
            10.25
        );
        assert_eq!(
            map_timeline_to_recording_secs(&map, 3.75, Some(0), false),
            10.25
        );
        assert_eq!(
            map_timeline_to_recording_secs(&map, 3.75, Some(0), true),
            3.75
        );
    }

    #[test]
    fn crop_map_identity_cases() {
        let screen = XY::new(1000u32, 1000u32);
        // Full-screen crop is the identity: no map.
        let full = Crop {
            position: XY::new(0, 0),
            size: XY::new(1000, 1000),
        };
        assert!(CursorCropMap::from_crop(&full, screen).is_none());
        // Degenerate inputs never produce a map.
        let degenerate = Crop {
            position: XY::new(0, 0),
            size: XY::new(0, 500),
        };
        assert!(CursorCropMap::from_crop(&degenerate, screen).is_none());
        assert!(CursorCropMap::from_crop(&full, XY::new(0, 0)).is_none());
    }

    #[test]
    fn crop_remaps_auto_zoom_focus_into_content_space() {
        // Screen 1000x1000 cropped to the bottom half: content = y 500..1000.
        let crop = Crop {
            position: XY::new(0, 500),
            size: XY::new(1000, 500),
        };
        let map = CursorCropMap::from_crop(&crop, XY::new(1000, 1000)).unwrap();

        // Cursor parked at raw (0.5, 0.75) = the exact CENTER of the visible
        // content. Uncropped this raw y would edge-snap to a bottom-flush
        // framing; content-space it must settle centered.
        let cursor = CursorEvents {
            moves: vec![move_event(0.0, 0.5, 0.75), move_event(8_000.0, 0.5, 0.75)],
            clicks: vec![],
        };
        let segments = vec![auto_segment(0.5, 8.0, 2.0)];
        let mut with_crop = ZoomTransformTimeline::new(
            &segments,
            None,
            &cursor,
            ScreenMovementSpring::default(),
            10.0,
            Some(map),
        );
        with_crop.precompute();

        let settled = with_crop.sample(6.0);
        let centered = SegmentBounds::from_amount_center(2.0, XY::new(0.5, 0.5));
        assert!(
            (settled.bounds.top_left.y - centered.top_left.y).abs() < 1e-2,
            "expected centered framing {:?}, got {:?}",
            centered,
            settled.bounds
        );

        let mut without_crop = timeline_for(&segments, &cursor, 10.0);
        without_crop.precompute();
        let raw_settled = without_crop.sample(6.0);
        assert!(
            (raw_settled.bounds.top_left.y - centered.top_left.y).abs() > 0.2,
            "control: uncropped framing should NOT be centered, got {:?}",
            raw_settled.bounds
        );
    }

    #[test]
    fn cursor_in_cropped_away_region_aims_at_content_edge() {
        // Bottom-half crop; the cursor hovers in the removed TOP strip
        // (raw y = 0.05 -> content y < 0). The framing must clamp to a
        // top-flush viewport, not wander or blow up.
        let crop = Crop {
            position: XY::new(0, 500),
            size: XY::new(1000, 500),
        };
        let map = CursorCropMap::from_crop(&crop, XY::new(1000, 1000)).unwrap();

        let cursor = CursorEvents {
            moves: vec![move_event(0.0, 0.5, 0.05), move_event(8_000.0, 0.5, 0.05)],
            clicks: vec![],
        };
        let segments = vec![auto_segment(0.5, 8.0, 2.0)];
        let mut timeline = ZoomTransformTimeline::new(
            &segments,
            None,
            &cursor,
            ScreenMovementSpring::default(),
            10.0,
            Some(map),
        );
        timeline.precompute();

        let settled = timeline.sample(6.0);
        let top_flush = SegmentBounds::from_amount_center(2.0, XY::new(0.5, 0.0));
        assert!(
            (settled.bounds.top_left.y - top_flush.top_left.y).abs() < 1e-2,
            "expected top-flush framing {:?}, got {:?}",
            top_flush,
            settled.bounds
        );
    }
}
