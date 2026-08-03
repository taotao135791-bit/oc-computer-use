//! Coordinate bounds enforcement: every action with a location is validated
//! against the geometry of the frame it references *before* it is dispatched.
//! Rejection here produces a structured `OUT_OF_BOUNDS` error instead of a
//! mis-aimed click.

use cu_core::{errors::CuError, ComputerAction, ImageGeometry, Point};

/// Resolve an action's coordinates to global desktop points, rejecting any
/// that fall outside the referenced image.
pub fn resolve_action_points(
    action: &ComputerAction,
    geometry: &ImageGeometry,
) -> Result<Vec<Point>, CuError> {
    let mut out = Vec::new();
    match action {
        ComputerAction::Click {
            x,
            y,
            coordinate_space,
            ..
        }
        | ComputerAction::DoubleClick {
            x,
            y,
            coordinate_space,
            ..
        } => {
            out.push(geometry.to_global(*coordinate_space, Point::new(*x, *y))?);
        }
        ComputerAction::Move {
            x,
            y,
            coordinate_space,
            ..
        } => {
            out.push(geometry.to_global(*coordinate_space, Point::new(*x, *y))?);
        }
        ComputerAction::Scroll {
            x,
            y,
            coordinate_space,
            ..
        } => {
            let space = *coordinate_space;
            if let (Some(x), Some(y)) = (x, y) {
                out.push(geometry.to_global(space, Point::new(*x, *y))?);
            }
        }
        ComputerAction::Drag {
            from,
            to,
            coordinate_space,
            ..
        } => {
            let space = *coordinate_space;
            out.push(geometry.to_global(space, *from)?);
            out.push(geometry.to_global(space, *to)?);
        }
        ComputerAction::TypeText { .. }
        | ComputerAction::Key { .. }
        | ComputerAction::Wait { .. } => {}
    }
    Ok(out)
}

/// True if every location-bearing action in the batch is inside the image.
pub fn batch_in_bounds(actions: &[ComputerAction], geometry: &ImageGeometry) -> bool {
    actions
        .iter()
        .all(|a| resolve_action_points(a, geometry).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::{CoordinateSpace, DisplayBounds, MouseButton};

    fn geom() -> ImageGeometry {
        ImageGeometry {
            image_width_px: 1000,
            image_height_px: 1000,
            display_bounds: DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1000.0,
                height: 1000.0,
            },
        }
    }

    #[test]
    fn valid_points_resolve() {
        let a = ComputerAction::Click {
            x: 500.0,
            y: 500.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        };
        let pts = resolve_action_points(&a, &geom()).unwrap();
        assert_eq!(pts, vec![Point::new(500.0, 500.0)]);
    }

    #[test]
    fn out_of_bounds_rejected() {
        let a = ComputerAction::Move {
            x: 1100.0,
            y: 500.0,
            coordinate_space: CoordinateSpace::Normalized1000,
            duration_ms: None,
        };
        assert!(resolve_action_points(&a, &geom()).is_err());
    }

    #[test]
    fn batch_in_bounds_detects_off_screen() {
        let ok = ComputerAction::Move {
            x: 10.0,
            y: 10.0,
            coordinate_space: CoordinateSpace::Normalized1000,
            duration_ms: None,
        };
        let bad = ComputerAction::Click {
            x: -50.0,
            y: 10.0,
            button: MouseButton::Right,
            coordinate_space: CoordinateSpace::Normalized1000,
        };
        assert!(batch_in_bounds(std::slice::from_ref(&ok), &geom()));
        assert!(!batch_in_bounds(&[bad], &geom()));
    }
}
