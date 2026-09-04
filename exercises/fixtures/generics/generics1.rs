// Generic fold: reduce a slice into a single accumulated value
//
// Implement a generic `fold` that reduces a slice of any type `T` into a
// single value of a (possibly different) type `U`, using a closure `f`
// and an initial value `init`.
//
// The closure receives `(accumulator, &item)` and returns the new
// accumulator. An empty slice must return `init` unchanged.
//
// Why this matters: `fold` is the prototypical higher-order generic
// function — once you can write it, you can express sum, product, length,
// string-join, and many other reductions generically.
//
// I AM NOT DONE

pub fn fold<T, U, F>(slice: &[T], mut init: U, f: F) -> U
where
    F: Fn(U, &T) -> U,
{
    // TODO
    for x in slice.iter() {
        init = f(init, x);
    }
    init
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_ints() {
        let xs = [1, 2, 3, 4, 5];
        assert_eq!(fold(&xs, 0, |acc, x| acc + x), 15);
    }

    #[test]
    fn builds_string() {
        let words = ["hello", "cruel", "world"];
        let res = fold(&words, String::new(), |acc, w| {
            if acc.is_empty() {
                w.to_string()
            } else {
                format!("{acc} {w}")
            }
        });
        assert_eq!(res, "hello cruel world");
    }

    #[test]
    fn counts_elements() {
        let xs = vec![1i32, 2, 3];
        assert_eq!(fold(&xs, 0, |acc, _| acc + 1), 3);
    }

    #[test]
    fn empty_returns_init() {
        let xs: [i32; 0] = [];
        assert_eq!(fold(&xs, 42i32, |acc, x| acc + x), 42);
    }

    #[test]
    fn finds_max() {
        let xs = [3, 7, 2, 9, 1];
        assert_eq!(fold(&xs, i32::MIN, |acc, &x| acc.max(x)), 9);
    }
}
