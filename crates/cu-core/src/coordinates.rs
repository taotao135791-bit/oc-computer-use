//! All coordinate transformations live here and nowhere else.
//!
//! The runtime deals in four coordinate spaces:
//!
//! 1. **model / `normalized_1000`** — what the upper-layer model sees. The
//!    captured image is treated as a `1000 x 1000` canvas, top-left `(0,0)`.
//! 2. **`image_pixels`** — raw pixel coordinates inside a captured image.
//! 3. **global logical points** — macOS display coordinates as returned by
//!    `CGDisplayBounds`. Origin is the top-left of the *primary* display,
//!    `y` grows downward, values are in logical points (not pixels).
//! 4. **CGEvent coordinates** — identical to global logical points, so the
//!    mouse and keyboard drivers consume global points directly.
//!
//! The one rule that keeps this bug-free: **no module outside this one may
//! multiply or divide by a scale factor.** Drivers and the runtime call the
//! transform methods here and pass the result straight to CoreGraphics.

use serde::{Deserialize, Serialize};

use crate::errors::{BoundsDetail, CuError};

/// A 2D point in some coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Coordinate space a caller used when describing a location or region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    /// Image treated as a 1000x1000 canvas; top-left is (0,0).
    #[serde(rename = "normalized_1000")]
    Normalized1000,
    /// Raw pixels inside the captured image.
    ImagePixels,
}

impl CoordinateSpace {
    pub fn as_str(&self) -> &'static str {
        match self {
            CoordinateSpace::Normalized1000 => "normalized_1000",
            CoordinateSpace::ImagePixels => "image_pixels",
        }
    }
}

impl std::str::FromStr for CoordinateSpace {
    type Err = CuError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "normalized_1000" => Ok(CoordinateSpace::Normalized1000),
            "image_pixels" => Ok(CoordinateSpace::ImagePixels),
            other => Err(CuError::InvalidParams(format!(
                "unknown coordinate space `{other}` (expected normalized_1000 or image_pixels)"
            ))),
        }
    }
}

/// Logical bounds of a display in global desktop coordinates.
///
/// On macOS the origin can be negative (a secondary display to the left/above
/// the primary one). Width/height are logical points; the backing store is
/// `width * scale_factor` pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplayBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl DisplayBounds {
    pub fn contains_global(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.x + self.width && p.y >= self.y && p.y <= self.y + self.height
    }
}

/// Describes the geometry needed to convert between image and desktop coords
/// for a single captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImageGeometry {
    /// Pixel width of the captured image.
    pub image_width_px: u32,
    /// Pixel height of the captured image.
    pub image_height_px: u32,
    /// Logical bounds of the display the image was captured from.
    pub display_bounds: DisplayBounds,
}

impl ImageGeometry {
    /// `image_width_px / display_bounds.width`, i.e. the Retina scale factor.
    /// Falls back to the pixel/point ratio if the width is zero.
    pub fn scale_factor(&self) -> f64 {
        if self.display_bounds.width > 0.0 {
            self.image_width_px as f64 / self.display_bounds.width
        } else {
            1.0
        }
    }

    /// Normalized-1000 coordinates (model space) → raw image pixel.
    /// `1000,1000` maps to the far edge (`width - 1`), so rounding keeps the
    /// coordinate inside the image.
    pub fn normalized_1000_to_image_pixel(&self, x: f64, y: f64) -> Result<(u32, u32), CuError> {
        self.check_normalized_1000_bounds(x, y)?;
        let px = (x / 1000.0 * self.image_width_px as f64)
            .round()
            .clamp(0.0, self.image_width_px.saturating_sub(1) as f64);
        let py = (y / 1000.0 * self.image_height_px as f64)
            .round()
            .clamp(0.0, self.image_height_px.saturating_sub(1) as f64);
        Ok((px as u32, py as u32))
    }

    /// Normalized-1000 coordinates → global logical desktop point.
    pub fn normalized_1000_to_global(&self, x: f64, y: f64) -> Result<Point, CuError> {
        let (px, py) = self.normalized_1000_to_image_pixel(x, y)?;
        Ok(self.image_pixel_to_global(px, py))
    }

    /// Image pixel → global logical desktop point.
    pub fn image_pixel_to_global(&self, px: u32, py: u32) -> Point {
        let s = self.scale_factor().max(f64::EPSILON);
        Point::new(
            self.display_bounds.x + px as f64 / s,
            self.display_bounds.y + py as f64 / s,
        )
    }

    /// Image pixel → normalized-1000 model coordinates.
    pub fn image_pixel_to_normalized_1000(&self, px: u32, py: u32) -> Point {
        let w = self.image_width_px.max(1) as f64;
        let h = self.image_height_px.max(1) as f64;
        Point::new(px as f64 * 1000.0 / w, py as f64 * 1000.0 / h)
    }

    /// Global logical desktop point → image pixel, if the point is inside the
    /// captured display. Returns `None` when outside (caller decides whether
    /// to clamp or reject).
    pub fn global_to_image_pixel(&self, g: Point) -> Option<(u32, u32)> {
        if !self.display_bounds.contains_global(g) {
            return None;
        }
        let s = self.scale_factor().max(f64::EPSILON);
        let px = ((g.x - self.display_bounds.x) * s).round();
        let py = ((g.y - self.display_bounds.y) * s).round();
        if px < 0.0
            || py < 0.0
            || px >= self.image_width_px as f64
            || py >= self.image_height_px as f64
        {
            return None;
        }
        Some((px as u32, py as u32))
    }

    /// A point expressed in `space` → global logical desktop point.
    pub fn to_global(&self, space: CoordinateSpace, p: Point) -> Result<Point, CuError> {
        match space {
            CoordinateSpace::Normalized1000 => self.normalized_1000_to_global(p.x, p.y),
            CoordinateSpace::ImagePixels => {
                let px = p.x.round() as i64;
                let py = p.y.round() as i64;
                if px < 0
                    || py < 0
                    || px >= self.image_width_px as i64
                    || py >= self.image_height_px as i64
                {
                    return Err(CuError::OutOfBounds(BoundsDetail {
                        coordinate_space: space.as_str().into(),
                        x: p.x,
                        y: p.y,
                        image_width: self.image_width_px,
                        image_height: self.image_height_px,
                    }));
                }
                Ok(self.image_pixel_to_global(px as u32, py as u32))
            }
        }
    }

    /// True when a normalized-1000 point lies within `[0,1000]²` (with a small
    /// tolerance so `1000` counts as on the far edge).
    pub fn normalized_1000_in_bounds(&self, x: f64, y: f64) -> bool {
        (0.0..=1000.0).contains(&x) && (0.0..=1000.0).contains(&y)
    }

    fn check_normalized_1000_bounds(&self, x: f64, y: f64) -> Result<(), CuError> {
        if !self.normalized_1000_in_bounds(x, y) {
            return Err(CuError::OutOfBounds(BoundsDetail {
                coordinate_space: CoordinateSpace::Normalized1000.as_str().into(),
                x,
                y,
                image_width: self.image_width_px,
                image_height: self.image_height_px,
            }));
        }
        Ok(())
    }
}

/// A rectangle expressed in one of the image-relative coordinate spaces.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub coordinate_space: CoordinateSpace,
}

impl Region {
    /// Convert the region to image pixel space, clamping so it stays inside
    /// the image. Returns the pixel rectangle.
    pub fn to_image_pixels(
        &self,
        geometry: &ImageGeometry,
    ) -> Result<(u32, u32, u32, u32), CuError> {
        let (x, y, w, h) = match self.coordinate_space {
            CoordinateSpace::Normalized1000 => {
                if !geometry.normalized_1000_in_bounds(self.x, self.y)
                    || !geometry
                        .normalized_1000_in_bounds(self.x + self.width, self.y + self.height)
                {
                    return Err(CuError::OutOfBounds(BoundsDetail {
                        coordinate_space: self.coordinate_space.as_str().into(),
                        x: self.x,
                        y: self.y,
                        image_width: geometry.image_width_px,
                        image_height: geometry.image_height_px,
                    }));
                }
                let (px, py) = geometry.normalized_1000_to_image_pixel(self.x, self.y)?;
                let (px2, py2) = geometry
                    .normalized_1000_to_image_pixel(self.x + self.width, self.y + self.height)?;
                let w = px2.saturating_sub(px).max(1);
                let h = py2.saturating_sub(py).max(1);
                (px, py, w, h)
            }
            CoordinateSpace::ImagePixels => {
                let x = self.x.round() as i64;
                let y = self.y.round() as i64;
                let w = self.width.round().max(1.0) as i64;
                let h = self.height.round().max(1.0) as i64;
                if x < 0
                    || y < 0
                    || x + w > geometry.image_width_px as i64
                    || y + h > geometry.image_height_px as i64
                {
                    return Err(CuError::OutOfBounds(BoundsDetail {
                        coordinate_space: self.coordinate_space.as_str().into(),
                        x: self.x,
                        y: self.y,
                        image_width: geometry.image_width_px,
                        image_height: geometry.image_height_px,
                    }));
                }
                (x as u32, y as u32, w as u32, h as u32)
            }
        };
        Ok((x, y, w, h))
    }
}

/// A linear drag path: `from` → `to` sampled into `steps + 1` points.
/// Steps are chosen from the travel distance so the motion looks smooth.
pub fn drag_path(
    from: Point,
    to: Point,
    duration_ms: u64,
    steps_per_second: f64,
) -> Vec<(Point, u64)> {
    let dist = ((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt();
    let n = ((dist / 8.0).ceil() as u64).clamp(4, 200);
    let n = n.max((duration_ms as f64 * steps_per_second / 1000.0).ceil() as u64);
    let mut out = Vec::with_capacity(n as usize + 1);
    for i in 0..=n {
        let t = i as f64 / n as f64;
        // Ease-in-out cubic for a natural drag start/stop.
        let ease = if t < 0.5 {
            2.0 * t * t
        } else {
            1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
        };
        let p = Point::new(
            from.x + (to.x - from.x) * ease,
            from.y + (to.y - from.y) * ease,
        );
        out.push((p, duration_ms / n));
    }
    out
}

/// A smooth move path for pointer motion (identical to drag, no button held).
pub fn move_path(from: Point, to: Point, duration_ms: u64) -> Vec<(Point, u64)> {
    drag_path(from, to, duration_ms, 90.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina_geometry() -> ImageGeometry {
        ImageGeometry {
            image_width_px: 2560,
            image_height_px: 1600,
            display_bounds: DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        }
    }

    fn negative_origin_geometry() -> ImageGeometry {
        ImageGeometry {
            image_width_px: 1920,
            image_height_px: 1080,
            display_bounds: DisplayBounds {
                x: -1920.0,
                y: -120.0,
                width: 1920.0,
                height: 1080.0,
            },
        }
    }

    #[test]
    fn normalized_1000_to_image_pixel_center() {
        let g = retina_geometry();
        let (px, py) = g.normalized_1000_to_image_pixel(500.0, 500.0).unwrap();
        assert_eq!((px, py), (1280, 800));
    }

    #[test]
    fn normalized_1000_to_image_pixel_edge_is_clamped_inside() {
        let g = retina_geometry();
        let (px, py) = g.normalized_1000_to_image_pixel(1000.0, 1000.0).unwrap();
        assert!(px < g.image_width_px);
        assert!(py < g.image_height_px);
        assert_eq!((px, py), (2559, 1599));
    }

    #[test]
    fn normalized_1000_to_global_retina_2x() {
        let g = retina_geometry();
        // Pixel (1280,800) at 2x → global (640,400).
        let p = g.normalized_1000_to_global(500.0, 500.0).unwrap();
        assert!((p.x - 640.0).abs() < 0.001 && (p.y - 400.0).abs() < 0.001);
    }

    #[test]
    fn non_2x_scale() {
        let g = ImageGeometry {
            image_width_px: 1920,
            image_height_px: 1080,
            display_bounds: DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
        };
        let p = g.normalized_1000_to_global(500.0, 500.0).unwrap();
        assert!((p.x - 960.0).abs() < 0.001);
        assert_eq!(g.scale_factor(), 1.0);
    }

    #[test]
    fn image_pixel_to_global_negative_origin() {
        let g = negative_origin_geometry();
        // Pixel (0,0) on a display at x=-1920 → global (-1920, -120).
        let p = g.image_pixel_to_global(0, 0);
        assert!((p.x - -1920.0).abs() < 0.001 && (p.y - -120.0).abs() < 0.001);
        // Pixel (1920,1080) → global (0, 960).
        let p = g.image_pixel_to_global(1920, 1080);
        assert!((p.x - 0.0).abs() < 0.001 && (p.y - 960.0).abs() < 0.001);
    }

    #[test]
    fn negative_origin_normalized_maps_to_global() {
        let g = negative_origin_geometry();
        let p = g.normalized_1000_to_global(1000.0, 1000.0).unwrap();
        // Normalized (1000,1000) → pixel (1919,1079) → global (-1, 959).
        assert!((p.x - -1.0).abs() < 1.0 && (p.y - 959.0).abs() < 1.0);
    }

    #[test]
    fn global_to_image_pixel_round_trip() {
        let g = retina_geometry();
        let world = g.normalized_1000_to_global(250.0, 750.0).unwrap();
        let (px, py) = g.global_to_image_pixel(world).unwrap();
        let back = g.image_pixel_to_global(px, py);
        assert!((back.x - world.x).abs() < 1.0 && (back.y - world.y).abs() < 1.0);
    }

    #[test]
    fn global_to_image_pixel_outside_returns_none() {
        let g = retina_geometry();
        assert!(g.global_to_image_pixel(Point::new(-5.0, 0.0)).is_none());
        assert!(g.global_to_image_pixel(Point::new(0.0, 10000.0)).is_none());
    }

    #[test]
    fn out_of_bounds_normalized_rejected() {
        let g = retina_geometry();
        assert!(g.normalized_1000_to_global(-0.1, 500.0).is_err());
        assert!(g.normalized_1000_to_global(1000.1, 500.0).is_err());
        let err = g.normalized_1000_to_global(500.0, 1200.0).unwrap_err();
        assert_eq!(err.code(), crate::errors::ErrorCode::OutOfBounds);
    }

    #[test]
    fn image_pixels_out_of_bounds_rejected() {
        let g = retina_geometry();
        assert!(g
            .to_global(CoordinateSpace::ImagePixels, Point::new(3000.0, 500.0))
            .is_err());
        assert!(g
            .to_global(CoordinateSpace::ImagePixels, Point::new(-1.0, 500.0))
            .is_err());
    }

    #[test]
    fn to_global_image_pixels_ok() {
        let g = retina_geometry();
        let p = g
            .to_global(CoordinateSpace::ImagePixels, Point::new(1280.0, 800.0))
            .unwrap();
        assert!((p.x - 640.0).abs() < 0.001);
    }

    #[test]
    fn region_normalized_clamps() {
        let g = retina_geometry();
        let r = Region {
            x: 100.0,
            y: 100.0,
            width: 300.0,
            height: 300.0,
            coordinate_space: CoordinateSpace::Normalized1000,
        };
        let (px, py, w, h) = r.to_image_pixels(&g).unwrap();
        assert!(w > 0 && h > 0 && px + w <= g.image_width_px && py + h <= g.image_height_px);
    }

    #[test]
    fn drag_path_has_endpoints() {
        let path = drag_path(Point::new(0.0, 0.0), Point::new(100.0, 100.0), 500, 90.0);
        assert!(path.len() >= 2);
        let (first, _) = path[0];
        let (last, _) = path[path.len() - 1];
        assert!((first.x - 0.0).abs() < 0.001);
        assert!((last.x - 100.0).abs() < 0.001 && (last.y - 100.0).abs() < 0.001);
    }

    #[test]
    fn coordinate_space_from_str() {
        assert_eq!(
            "normalized_1000".parse::<CoordinateSpace>().unwrap(),
            CoordinateSpace::Normalized1000
        );
        assert!("bogus".parse::<CoordinateSpace>().is_err());
    }

    #[test]
    fn scale_factor_for_retina() {
        assert!((retina_geometry().scale_factor() - 2.0).abs() < 1e-9);
    }
}
