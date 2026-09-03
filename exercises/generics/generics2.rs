// Generic stack with From<Vec<T>> and order-preserving merge
//
// Build a tiny generic stack `Stack<T>` storing values of any type `T`.
// No bounds on `T` are allowed anywhere (push/pop must work for non-Clone
// types).
//
// API:
//   * `Stack::new() -> Self`
//   * `push(&mut self, value: T)`
//   * `pop(&mut self) -> Option<T>`            (LIFO)
//   * `top(&self) -> Option<&T>`
//   * `is_empty(&self) -> bool`
//   * `size(&self) -> usize`
//   * `merge_into(&mut self, other: Stack<T>)`  — moves all of `other`'s
//     elements into `self` so that the TOP of `other` becomes the new
//     TOP of `self`.
//   * `From<Vec<T>>` for `Stack<T>` — the last element of the vec becomes
//     the top.
//
// The tricky part is `merge_into`'s ordering guarantee: after merging
// `b` into `a`, popping `a` must yield b's top first, then the rest of
// b, then a's old elements.
//
// I AM NOT DONE

pub struct Stack<T> {
    items: Vec<T>,
}

impl<T> Stack<T> {
    pub fn new() -> Self {
        todo!()
    }

    pub fn push(&mut self, value: T) {
        todo!()
    }

    pub fn pop(&mut self) -> Option<T> {
        todo!()
    }

    pub fn top(&self) -> Option<&T> {
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        todo!()
    }

    pub fn size(&self) -> usize {
        todo!()
    }

    /// Move all elements from `other` into `self`. `other`'s top must
    /// become `self`'s new top.
    pub fn merge_into(&mut self, other: Stack<T>) {
        todo!()
    }
}

impl<T> From<Vec<T>> for Stack<T> {
    fn from(v: Vec<T>) -> Self {
        todo!()
    }
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ops() {
        let mut s: Stack<i32> = Stack::new();
        assert!(s.is_empty());
        s.push(1);
        s.push(2);
        s.push(3);
        assert_eq!(s.size(), 3);
        assert_eq!(s.top(), Some(&3));
        assert_eq!(s.pop(), Some(3));
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), Some(1));
        assert_eq!(s.pop(), None);
        assert!(s.is_empty());
    }

    #[test]
    fn merge_preserves_order() {
        let mut a: Stack<i32> = Stack::new();
        a.push(1);
        a.push(2); // a top is 2
        let mut b: Stack<i32> = Stack::new();
        b.push(3);
        b.push(4); // b top is 4
        a.merge_into(b);
        // Popping a must yield: 4 (b's top), 3, 2 (a's old top), 1
        assert_eq!(a.pop(), Some(4));
        assert_eq!(a.pop(), Some(3));
        assert_eq!(a.pop(), Some(2));
        assert_eq!(a.pop(), Some(1));
        assert_eq!(a.pop(), None);
    }

    #[test]
    fn from_vec_makes_last_top() {
        let s: Stack<i32> = Stack::from(vec![1, 2, 3]);
        assert_eq!(s.top(), Some(&3));
        assert_eq!(s.size(), 3);
    }

    #[test]
    fn works_without_clone() {
        // String does impl Clone, but this test never clones — the point
        // is that nothing in the API requires it.
        let mut s: Stack<String> = Stack::new();
        s.push("a".into());
        s.push("b".into());
        assert_eq!(s.pop().as_deref(), Some("b"));
    }
}
