// Blanket implementations: implementing a trait for ALL qualifying types
//
// The std does this famously: `impl<T: Display> ToString for T`. You'll
// write your own blanket impl.
//
// The `Len` trait is given. Implement it twice:
//
//   1. A BLANKET impl: every `T: IntoIterator + Clone` gets `Len`, where
//      `len()` clones `self`, consumes it via `into_iter()`, and counts.
//      Don't override the default `is_empty`.
//
//   2. A manual impl for `StrBuf(String)` that returns the CHARACTER
//      count (not the byte count!). e.g. `"héllo"` has 5 chars.
//
// IMPORTANT: this must NOT conflict with the blanket impl. `StrBuf` must
// therefore NOT satisfy `IntoIterator + Clone`. As long as you DON'T
// implement `IntoIterator` for `StrBuf`, the manual impl is the only one
// that applies — no E0119 overlap error.
//
// Hint: to count chars, use `.chars().count()`.
//
// Why the tests use a generic helper: concrete types like `Vec` and slices
// have their OWN inherent `len` methods, which would shadow `Len::len` in
// a direct `v.len()` call. The generic `count<L: Len>` helper forces trait
// dispatch, so the blanket impl is genuinely required.
//
// I AM NOT DONE

pub trait Len {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct StrBuf(pub String);

// TODO: blanket impl `Len` for all `T: IntoIterator + Clone`.
// TODO: manual impl `Len` for `StrBuf` returning char count.

#[cfg(test)]
mod tests {
    use super::*;

    /// Forces `Len::len` to be called (no inherent method is visible on a
    /// generic type parameter), so the blanket impl is genuinely required.
    fn count<L: Len>(x: &L) -> usize {
        x.len()
    }

    #[test]
    fn blanket_works_for_vec() {
        let v = vec![1, 2, 3];
        assert_eq!(count(&v), 3);
        assert!(!v.is_empty());
    }

    #[test]
    fn blanket_works_for_array() {
        let a = [1, 2, 3, 4];
        assert_eq!(count(&a), 4);
    }

    #[test]
    fn blanket_works_for_slice() {
        let s = &[10, 20];
        assert_eq!(count(&s), 2);
    }

    #[test]
    fn blanket_works_for_empty() {
        let v: Vec<i32> = vec![];
        assert_eq!(count(&v), 0);
        assert!(v.is_empty());
    }

    #[test]
    fn strbuf_counts_chars_not_bytes() {
        // "héllo" = 5 code points, 6 bytes (é is two bytes in UTF-8).
        let b = StrBuf("héllo".into());
        assert_eq!(count(&b), 5);
        assert!(!b.is_empty());
    }

    #[test]
    fn strbuf_empty() {
        let b = StrBuf(String::new());
        assert_eq!(count(&b), 0);
        assert!(b.is_empty());
    }

    #[test]
    fn strbuf_not_covered_by_blanket() {
        // The blanket impl is NOT used for StrBuf (it doesn't impl
        // IntoIterator). The manual impl returns char count; "héllo" is
        // 5 chars but 6 bytes, which distinguishes the two impls.
        let b = StrBuf("héllo".into());
        assert_eq!(count(&b), 5);
    }
}
