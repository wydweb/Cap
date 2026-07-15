use std::{
    sync::mpsc::{self, Sender},
    thread,
};

use scap_targets::{
    Display, Window,
    bounds::{LogicalBounds, LogicalPosition, LogicalSize},
};
use tokio::sync::oneshot;
use uiautomation::{
    UIAutomation, UIElement, UITreeWalker,
    core::UICacheRequest,
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
    walker: UITreeWalker,
    cache_request: UICacheRequest,
}

impl UiAutomationState {
    fn new() -> uiautomation::Result<Self> {
        let automation = UIAutomation::new()?;
        let walker = automation.get_content_view_walker()?;
        let cache_request = automation.create_cache_request()?;
        cache_request.add_property(UIProperty::BoundingRectangle)?;
        cache_request.add_property(UIProperty::IsOffscreen)?;
        cache_request.set_tree_scope(TreeScope::Element)?;

        Ok(Self {
            automation,
            walker,
            cache_request,
        })
    }

    fn detect(&mut self, request: &DetectionRequest) -> uiautomation::Result<Vec<LogicalBounds>> {
        let mut cursor = windows::Win32::Foundation::POINT::default();
        unsafe { GetCursorPos(&mut cursor) }.map_err(|error| error.to_string())?;

        let root = self
            .automation
            .element_from_handle(Handle::from(request.query.hwnd))?;
        let mut parent = root;
        let mut physical_bounds = Vec::new();

        for _ in 0..64 {
            let Some((element, bounds)) = self.child_at_point(
                &parent,
                cursor.x as f64,
                cursor.y as f64,
                request.query.window_bounds,
            )?
            else {
                break;
            };

            if physical_bounds.last() != Some(&bounds) {
                physical_bounds.push(bounds);
            }
            parent = element;
        }

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

    fn child_at_point(
        &self,
        parent: &UIElement,
        x: f64,
        y: f64,
        window_bounds: PhysicalRect,
    ) -> uiautomation::Result<Option<(UIElement, PhysicalRect)>> {
        let Ok(mut element) = self
            .walker
            .get_first_child_build_cache(parent, &self.cache_request)
        else {
            return Ok(None);
        };

        loop {
            if !element.is_cached_offscreen().unwrap_or(true)
                && let Ok(rect) = element.get_cached_bounding_rectangle()
                && let Some(bounds) = PhysicalRect::new(
                    rect.get_left() as f64,
                    rect.get_top() as f64,
                    rect.get_right() as f64,
                    rect.get_bottom() as f64,
                )
                .intersect(window_bounds)
                && bounds.contains(x, y)
            {
                return Ok(Some((element, bounds)));
            }

            match self
                .walker
                .get_next_sibling_build_cache(&element, &self.cache_request)
            {
                Ok(sibling) => element = sibling,
                Err(_) => return Ok(None),
            }
        }
    }
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
}
