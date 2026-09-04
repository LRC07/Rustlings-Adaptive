// Generic ring buffer with a custom iterator
//
// Implement a fixed-capacity ring buffer `Ring<T>`:
//
//   * `Ring::new(capacity: usize) -> Self`
//   * `capacity(&self) -> usize`
//   * `len(&self) -> usize`
//   * `is_empty(&self) -> bool`
//   * `push(&mut self, value: T)` — when full, OVERWRITES the oldest entry
//   * `iter(&self) -> impl Iterator<Item = &T>` — yields elements in
//     insertion order (oldest first)
//
// Constraints:
//   * NO bound on `T` is allowed on the struct or on `push`.
//   * `iter` must walk the live region with wraparound.
//   * `Ring::new(0)` must work: pushing does nothing, iter yields nothing,
//     `len` is always 0.
//
// The hard part: getting `iter`'s ordering right when the buffer has
// wrapped around (i.e. `head` points into the middle of the backing
// storage). Test `fill_and_overwrite` pins this down precisely.
//
// I AM NOT DONE

pub struct Ring<T> {
    buf: Vec<Option<T>>,
    head: usize, // index of the OLDEST live element
    len: usize,  // number of live elements
}

impl<T> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        todo!()
    }

    pub fn capacity(&self) -> usize {
        todo!()
    }

    pub fn len(&self) -> usize {
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn push(&mut self, value: T) {
        todo!()
    }

    /// Yield live elements oldest-first, including after wraparound.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_and_overwrite() {
        let mut r: Ring<i32> = Ring::new(3);
        r.push(1);
        r.push(2);
        r.push(3);
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);

        r.push(4); // overwrites the oldest (1)
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);

        r.push(5); // overwrites 2
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);

        r.push(6); // overwrites 3
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![4, 5, 6]);
    }

    #[test]
    fn partial_fill_preserves_order() {
        let mut r: Ring<i32> = Ring::new(5);
        r.push(10);
        r.push(20);
        assert_eq!(r.len(), 2);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![10, 20]);
    }

    #[test]
    fn zero_capacity_is_noop() {
        let mut r: Ring<i32> = Ring::new(0);
        r.push(1);
        r.push(2);
        assert_eq!(r.len(), 0);
        assert_eq!(r.capacity(), 0);
        assert!(r.is_empty());
        assert!(r.iter().next().is_none());
    }

    #[test]
    fn wraps_many_times() {
        let mut r: Ring<i32> = Ring::new(2);
        for i in 0..10 {
            r.push(i);
        }
        // only last two survive: 8, 9
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![8, 9]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn works_for_owned_nonclone_type() {
        let mut r: Ring<String> = Ring::new(2);
        r.push("a".into());
        r.push("b".into());
        r.push("c".into());
        let v: Vec<&str> = r.iter().map(|s| s.as_str()).collect();
        assert_eq!(v, vec!["b", "c"]);
    }
}
