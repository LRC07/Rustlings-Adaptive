// Trait objects: dynamic dispatch with `dyn Shape`
//
// Given the `Shape` trait below with a required `area` method and a
// default `name` returning `"shape"`:
//
//   * Implement `Shape` for `Circle`, `Rectangle`, `Triangle`, OVERRIDING
//     `name` to return `"circle"` / `"rectangle"` / `"triangle"`.
//   * Complete `total_area(shapes: &[Box<dyn Shape>]) -> f64` — sum of
//     areas of owned boxes.
//   * Complete `largest(shapes: &[&dyn Shape]) -> Option<&dyn Shape>` —
//     a reference to the shape with the largest area, or `None` if empty.
//
// Lessons:
//   * A `Box<dyn Trait>` is an owned, heap-allocated trait object.
//   * A `&dyn Trait` is a borrowed "fat pointer" (data ptr + vtable ptr).
//   * Both enable heterogeneous collections, but the dispatch is dynamic.
//
// I AM NOT DONE

pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> &'static str {
        "shape"
    }
}

pub struct Circle {
    pub radius: f64,
}

pub struct Rectangle {
    pub width: f64,
    pub height: f64,
}

pub struct Triangle {
    pub base: f64,
    pub height: f64,
}

// TODO: implement Shape for Circle, Rectangle, Triangle.

/// Sum the areas of owned trait objects.
pub fn total_area(shapes: &[Box<dyn Shape>]) -> f64 {
    todo!()
}

/// Return the shape with the largest area, or `None` if the slice is empty.
/// Ties resolve to the first one found.
pub fn largest<'a>(shapes: &'a [&'a dyn Shape]) -> Option<&'a dyn Shape> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_are_correct() {
        assert!((Circle { radius: 1.0 }.area() - std::f64::consts::PI).abs() < 1e-9);
        assert_eq!(Rectangle { width: 2.0, height: 3.0 }.area(), 6.0);
        assert_eq!(Triangle { base: 4.0, height: 5.0 }.area(), 10.0);
    }

    #[test]
    fn names_overridden() {
        assert_eq!(Circle { radius: 1.0 }.name(), "circle");
        assert_eq!(Rectangle { width: 1.0, height: 1.0 }.name(), "rectangle");
        assert_eq!(Triangle { base: 1.0, height: 1.0 }.name(), "triangle");
    }

    #[test]
    fn total_with_heterogeneous_collection() {
        let shapes: Vec<Box<dyn Shape>> = vec![
            Box::new(Circle { radius: 1.0 }),
            Box::new(Rectangle { width: 2.0, height: 3.0 }),
            Box::new(Triangle { base: 4.0, height: 5.0 }),
        ];
        let expected = std::f64::consts::PI + 6.0 + 10.0;
        assert!((total_area(&shapes) - expected).abs() < 1e-9);
    }

    #[test]
    fn total_empty_is_zero() {
        let shapes: Vec<Box<dyn Shape>> = vec![];
        assert_eq!(total_area(&shapes), 0.0);
    }

    #[test]
    fn largest_picks_biggest_area() {
        let c = Circle { radius: 1.0 }; // ~3.14
        let r = Rectangle { width: 2.0, height: 3.0 }; // 6.0
        let t = Triangle { base: 4.0, height: 5.0 }; // 10.0
        let slice: [&dyn Shape; 3] = [&c, &r, &t];
        let l = largest(&slice).unwrap();
        assert_eq!(l.name(), "triangle");
    }

    #[test]
    fn largest_empty_is_none() {
        let slice: [&dyn Shape; 0] = [];
        assert!(largest(&slice).is_none());
    }

    #[test]
    fn largest_breaks_ties_earliest() {
        let a = Rectangle { width: 2.0, height: 3.0 }; // 6.0
        let b = Rectangle { width: 1.0, height: 6.0 }; // 6.0 (tie)
        let slice: [&dyn Shape; 2] = [&a, &b];
        let l = largest(&slice).unwrap();
        assert!(std::ptr::eq(l as *const dyn Shape, &a as *const dyn Shape));
    }
}
