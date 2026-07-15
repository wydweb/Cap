use std::{
    collections::HashMap,
    sync::mpsc::{self, Sender},
    thread,
};

use scap_targets::{
    Display, Window,
    bounds::{LogicalBounds, LogicalPosition, LogicalSize},
};
use tokio::sync::oneshot;
use uiautomation::{
    UIAutomation, UIElement,
    core::{UICacheRequest, UICondition},
    types::{Handle, TreeScope, UIProperty},
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

#[derive(Clone, Copy, Debug, PartialEq)]
struct PhysicalRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl PhysicalRect {
    fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self {
            left: left.min(right),
            top: top.min(bottom),
            right: left.max(right),
            bottom: top.max(bottom),
        }
    }

    fn width(self) -> f64 {
        self.right - self.left
    }

    fn height(self) -> f64 {
        self.bottom - self.top
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let rect = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };

        (rect.width() > 0.0 && rect.height() > 0.0).then_some(rect)
    }
}

pub struct DetectionQuery {
    hwnd: isize,
    display_bounds: PhysicalRect,
    display_logical_size: LogicalSize,
    window_bounds: PhysicalRect,
}

struct DetectionRequest {
    query: DetectionQuery,
    response: oneshot::Sender<Vec<LogicalBounds>>,
}

pub struct UiElementDetector {
    sender: Sender<DetectionRequest>,
}

impl UiElementDetector {
    pub fn new() -> Option<Self> {
        let (sender, receiver) = mpsc::channel::<DetectionRequest>();
        thread::Builder::new()
            .name("cap-ui-automation".to_string())
            .spawn(move || {
                let mut automation = UiAutomationState::new().ok();
                while let Ok(request) = receiver.recv() {
                    let bounds = automation
                        .as_mut()
                        .and_then(|state| state.detect(&request).ok())
                        .unwrap_or_default();
                    let _ = request.response.send(bounds);
                }
            })
            .ok()?;

        Some(Self { sender })
    }

    pub fn query(window: Window, display: Display) -> Option<DetectionQuery> {
        let Some(display_physical_bounds) = display.raw_handle().physical_bounds() else {
            return None;
        };
        let Some(display_logical_size) = display.logical_size() else {
            return None;
        };
        let Some(window_physical_bounds) = window.raw_handle().physical_bounds() else {
            return None;
        };

        Some(DetectionQuery {
            hwnd: window.raw_handle().inner().0 as isize,
            display_bounds: PhysicalRect::new(
                display_physical_bounds.position().x(),
                display_physical_bounds.position().y(),
                display_physical_bounds.position().x() + display_physical_bounds.size().width(),
                display_physical_bounds.position().y() + display_physical_bounds.size().height(),
            ),
            display_logical_size,
            window_bounds: PhysicalRect::new(
                window_physical_bounds.position().x(),
                window_physical_bounds.position().y(),
                window_physical_bounds.position().x() + window_physical_bounds.size().width(),
                window_physical_bounds.position().y() + window_physical_bounds.size().height(),
            ),
        })
    }

    pub async fn detect(&self, query: DetectionQuery) -> Vec<LogicalBounds> {
        let (response, receiver) = oneshot::channel();
        let request = DetectionRequest { query, response };

        if self.sender.send(request).is_err() {
            return vec![];
        }

        receiver.await.unwrap_or_default()
    }
}

struct UiAutomationState {
    automation: UIAutomation,
    true_condition: UICondition,
    cache_request: UICacheRequest,
    window_caches: HashMap<isize, WindowElementCache>,
}

struct CachedElement {
    element: UIElement,
    bounds: Option<PhysicalRect>,
    children: Option<Vec<usize>>,
}

struct WindowElementCache {
    window_bounds: PhysicalRect,
    elements: Vec<CachedElement>,
}

impl WindowElementCache {
    fn new(window_bounds: PhysicalRect, root: UIElement) -> Self {
        Self {
            window_bounds,
            elements: vec![CachedElement {
                element: root,
                bounds: None,
                children: None,
            }],
        }
    }
}

impl UiAutomationState {
    fn new() -> uiautomation::Result<Self> {
        let automation = UIAutomation::new()?;
        let true_condition = automation.create_true_condition()?;
        let cache_request = automation.create_cache_request()?;
        cache_request.add_property(UIProperty::BoundingRectangle)?;
        cache_request.add_property(UIProperty::IsOffscreen)?;
        cache_request.set_tree_scope(TreeScope::Element)?;

        Ok(Self {
            automation,
            true_condition,
            cache_request,
            window_caches: HashMap::new(),
        })
    }

    fn detect(&mut self, request: &DetectionRequest) -> uiautomation::Result<Vec<LogicalBounds>> {
        let mut cursor = windows::Win32::Foundation::POINT::default();
        unsafe { GetCursorPos(&mut cursor) }.map_err(|error| error.to_string())?;

        let mut cache = match self.window_caches.remove(&request.query.hwnd) {
            Some(cache) if cache.window_bounds == request.query.window_bounds => cache,
            _ => WindowElementCache::new(
                request.query.window_bounds,
                self.automation
                    .element_from_handle(Handle::from(request.query.hwnd))?,
            ),
        };
        let physical_bounds = self.detect_from_cache(
            &mut cache,
            cursor.x as f64,
            cursor.y as f64,
            request.query.window_bounds,
        );
        self.window_caches.insert(request.query.hwnd, cache);
        let mut physical_bounds = physical_bounds?;

        physical_bounds.reverse();
        Ok(physical_bounds
            .into_iter()
            .filter_map(|bounds| {
                physical_to_display_logical(
                    bounds,
                    request.query.display_bounds,
                    request.query.display_logical_size,
                )
            })
            .collect())
    }

    fn detect_from_cache(
        &self,
        cache: &mut WindowElementCache,
        x: f64,
        y: f64,
        window_bounds: PhysicalRect,
    ) -> uiautomation::Result<Vec<PhysicalRect>> {
        let mut current_index = 0;
        let mut physical_bounds = Vec::new();

        for _ in 0..64 {
            let child_indexes = self.cached_children(cache, current_index, window_bounds)?;
            let Some(child_index) = child_indexes.into_iter().find(|index| {
                cache.elements[*index]
                    .bounds
                    .is_some_and(|bounds| is_candidate_at_point(false, bounds, x, y, window_bounds))
            }) else {
                break;
            };
            let bounds = cache.elements[child_index].bounds.unwrap();

            if physical_bounds.last() != Some(&bounds) {
                physical_bounds.push(bounds);
            }
            current_index = child_index;
        }

        Ok(physical_bounds)
    }

    fn cached_children(
        &self,
        cache: &mut WindowElementCache,
        parent_index: usize,
        window_bounds: PhysicalRect,
    ) -> uiautomation::Result<Vec<usize>> {
        if let Some(children) = &cache.elements[parent_index].children {
            return Ok(children.clone());
        }

        let parent = cache.elements[parent_index].element.clone();
        let elements = parent.find_all_build_cache(
            TreeScope::Children,
            &self.true_condition,
            &self.cache_request,
        )?;
        let mut child_indexes = Vec::with_capacity(elements.len());

        for element in elements {
            let is_offscreen = element.is_cached_offscreen().unwrap_or(true);
            if is_offscreen {
                continue;
            }
            let Ok(rect) = element.get_cached_bounding_rectangle() else {
                continue;
            };
            let bounds = PhysicalRect::new(
                rect.get_left() as f64,
                rect.get_top() as f64,
                rect.get_right() as f64,
                rect.get_bottom() as f64,
            );
            let Some(bounds) = bounds.intersect(window_bounds) else {
                continue;
            };
            let child_index = cache.elements.len();
            cache.elements.push(CachedElement {
                element,
                bounds: Some(bounds),
                children: None,
            });
            child_indexes.push(child_index);
        }

        cache.elements[parent_index].children = Some(child_indexes.clone());
        Ok(child_indexes)
    }
}

fn is_candidate_at_point(
    is_offscreen: bool,
    bounds: PhysicalRect,
    x: f64,
    y: f64,
    window_bounds: PhysicalRect,
) -> bool {
    !is_offscreen
        && bounds
            .intersect(window_bounds)
            .is_some_and(|bounds| bounds.contains(x, y))
}

fn physical_to_display_logical(
    bounds: PhysicalRect,
    display_bounds: PhysicalRect,
    display_logical_size: LogicalSize,
) -> Option<LogicalBounds> {
    let bounds = bounds.intersect(display_bounds)?;
    let scale_x = display_logical_size.width() / display_bounds.width();
    let scale_y = display_logical_size.height() / display_bounds.height();

    Some(LogicalBounds::new(
        LogicalPosition::new(
            (bounds.left - display_bounds.left) * scale_x,
            (bounds.top - display_bounds.top) * scale_y,
        ),
        LogicalSize::new(bounds.width() * scale_x, bounds.height() * scale_y),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_physical_bounds_with_display_scaling() {
        let result = physical_to_display_logical(
            PhysicalRect::new(150.0, 250.0, 650.0, 750.0),
            PhysicalRect::new(100.0, 200.0, 2100.0, 1200.0),
            LogicalSize::new(1600.0, 800.0),
        )
        .unwrap();

        assert_eq!(result.position(), LogicalPosition::new(40.0, 40.0));
        assert_eq!(result.size(), LogicalSize::new(400.0, 400.0));
    }

    #[test]
    fn handles_negative_display_origins_and_clips_bounds() {
        let result = physical_to_display_logical(
            PhysicalRect::new(-2200.0, -100.0, -1200.0, 700.0),
            PhysicalRect::new(-1920.0, 0.0, 0.0, 1080.0),
            LogicalSize::new(1536.0, 864.0),
        )
        .unwrap();

        assert_eq!(result.position(), LogicalPosition::new(0.0, 0.0));
        assert_eq!(result.size(), LogicalSize::new(576.0, 560.0));
    }

    #[test]
    fn rejects_rectangles_outside_the_display() {
        assert!(
            physical_to_display_logical(
                PhysicalRect::new(2000.0, 0.0, 2200.0, 200.0),
                PhysicalRect::new(0.0, 0.0, 1920.0, 1080.0),
                LogicalSize::new(1920.0, 1080.0),
            )
            .is_none()
        );
    }

    #[test]
    fn selects_visible_candidate_containing_the_cursor() {
        assert!(is_candidate_at_point(
            false,
            PhysicalRect::new(20.0, 20.0, 80.0, 80.0),
            40.0,
            40.0,
            PhysicalRect::new(0.0, 0.0, 100.0, 100.0),
        ));
    }

    #[test]
    fn rejects_offscreen_and_non_containing_candidates() {
        let bounds = PhysicalRect::new(20.0, 20.0, 80.0, 80.0);
        let window = PhysicalRect::new(0.0, 0.0, 100.0, 100.0);

        assert!(!is_candidate_at_point(true, bounds, 40.0, 40.0, window));
        assert!(!is_candidate_at_point(false, bounds, 90.0, 90.0, window));
    }

    #[test]
    fn uses_the_window_clipped_candidate_bounds() {
        let bounds = PhysicalRect::new(-20.0, -20.0, 80.0, 80.0);
        let window = PhysicalRect::new(0.0, 0.0, 100.0, 100.0);

        assert!(is_candidate_at_point(false, bounds, 20.0, 20.0, window));
        assert!(!is_candidate_at_point(false, bounds, -10.0, -10.0, window));
    }
}
