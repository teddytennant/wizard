## Summary

I read `src/lib.rs`, changed `add` to **wrap on overflow**, and ran the tests. The _short_ version: nothing else moved.

### What changed

1. `add` now calls `wrapping_add`.
2. A new `sub` uses `saturating_sub`, because a wrapping subtract hid a bug in `main.rs`.
   - nested bullet with `inline code`
   - another one with a [link to the docs](https://doc.rust-lang.org/std/primitive.i32.html)
     that wraps onto a second line when the terminal is narrow enough to need it
3. ~~Drop the old helper~~ kept, since `main.rs` still uses it.

- [x] tests pass
- [ ] benchmarks not run

> Overflow in release builds wraps silently; in debug builds it panics. That difference is the whole reason for this change.

```rust
pub fn add(a: i32, b: i32) -> i32 {
    // wrap instead of panicking in debug builds
    a.wrapping_add(b)
}

#[test]
fn wraps() {
    assert_eq!(add(i32::MAX, 1), i32::MIN);
}
```

```python
def add(a: int, b: int) -> int:
    return (a + b + 2**31) % 2**32 - 2**31
```

```json
{"name": "demo", "version": "0.1.0", "private": true}
```

```diff
-    a + b
+    a.wrapping_add(b)
```

```
plain fence with no language
```

| step | result | exit |
|------|--------|------|
| read | ok | 0 |
| patch | ok | 0 |
| cargo test | failed on purpose, with a long note that makes this cell wider than the others | 1 |

---

Final paragraph with *emphasis*, **strong**, ***both***, and a very long unbroken-looking sentence that keeps going so the wrapper has to break it at a word boundary somewhere near the right edge of the terminal, whatever width that happens to be.
