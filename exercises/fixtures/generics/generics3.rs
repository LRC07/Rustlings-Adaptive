// Method-level trait bounds: never bound the struct
//
// `Pair<T>` holds two values of the same type. The exercise is about
// WHERE you put bounds:
//
//   * `Pair::new` must NOT require any bound on `T`. You must be able to
//     build a `Pair<NonOrdType>`.
//   * `swap(&mut self)` needs no bounds.
//   * `into_sorted(self) -> (T, T)` requires `T: Ord` — put that bound on
//     the METHOD, not the impl block.
//   * `map<U, F>(self, f: F) -> Pair<U>` transforms both members using
//     `f: Fn(T) -> U` and returns a `Pair<U>`. Needs no bounds on `T`/`U`
//     beyond the function bound.
//
// The key lesson: writing `impl<T: Ord> Pair<T> { ... }` would force every
// user of `Pair<T>` to supply `T: Ord`, even for operations that don't
// need ordering. Bounds belong on the items that need them.
//
// I AM NOT DONE

pub struct Pair<T> {
    pub first: T,
    pub second: T,
}

impl<T> Pair<T> {
    pub fn new(first: T, second: T) -> Self {
        todo!()
    }

    pub fn swap(&mut self) {
        todo!()
    }

    /// Consume the pair and return `(min, max)`.
    pub fn into_sorted(self) -> (T, T)
    where
        T: Ord,
    {
        todo!()
    }

    /// Apply `f` to both elements, yielding a `Pair<U>`.
    pub fn map<U, F>(self, f: F) -> Pair<U>
    where
        F: Fn(T) -> U,
    {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_works_for_non_ord_type() {
        // A type that does NOT implement Ord (only Eq, intentionally).
        #[derive(Debug, PartialEq, Eq)]
        struct Tag(u32);

        let p = Pair::new(Tag(1), Tag(2));
        assert_eq!(p.first, Tag(1));
        assert_eq!(p.second, Tag(2));
    }

    #[test]
    fn swap() {
        let mut p = Pair::new(1, 2);
        p.swap();
        assert_eq!((p.first, p.second), (2, 1));
    }

    #[test]
    fn sorted_when_smaller_first() {
        let p = Pair::new(5, 2);
        assert_eq!(p.into_sorted(), (2, 5));
    }

    #[test]
    fn sorted_when_already_sorted() {
        let p = Pair::new(1, 9);
        assert_eq!(p.into_sorted(), (1, 9));
    }

    #[test]
    fn map_changes_type() {
        let p = Pair::new(1, 2);
        let q: Pair<String> = p.map(|x| x.to_string());
        assert_eq!(q.first, "1");
        assert_eq!(q.second, "2");
    }

    #[test]
    fn map_can_swap_meaning() {
        let p = Pair::new(10, 20);
        let q = p.map(|x| x * 2);
        assert_eq!(q.first, 20);
        assert_eq!(q.second, 40);
    }
}
