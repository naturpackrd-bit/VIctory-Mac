//! The low 4 GiB on this host: free ranges, reserving them, carving them
//! out (docs/macos.md). An integration test, so that on x86-64 macOS it is
//! linked as the runtime is, with a one-page `__PAGEZERO` (build.rs): the
//! crate's unit test binary has the default 4 GiB one, below which nothing
//! is free.
#![cfg(all(unix, target_arch = "x86_64"))]

use victory_platform::low::{LOW_FLOOR, LOW_LIMIT, carve, free_ranges, reserve};
use victory_platform::{Host, Memory};

#[test]
fn free_ranges_are_sorted_pagegranular_and_below_4gib() {
    let free = free_ranges();
    if free.is_empty() {
        println!("nothing free below 4 GiB (a 4 GiB __PAGEZERO, or reserved)");
        return;
    }
    let page = Host::page_size();
    for r in &free {
        assert_eq!(r.start % page, 0, "{r:x?}");
        assert_eq!(r.len % page, 0, "{r:x?}");
        assert!(r.len > 0, "{r:x?}");
        assert!(r.start >= LOW_FLOOR, "{r:x?}");
        assert!(r.start + r.len <= LOW_LIMIT, "{r:x?}");
    }
    for w in free.windows(2) {
        assert!(w[0].start + w[0].len <= w[1].start, "{w:x?}");
    }
}

#[test]
fn reserving_takes_what_is_free_and_carving_hands_it_out() {
    let before = free_ranges();
    if before.is_empty() {
        println!("nothing free below 4 GiB (a 4 GiB __PAGEZERO)");
        return;
    }
    let page = Host::page_size();
    let taken = reserve();
    assert!(!taken.is_empty());
    assert_eq!(Host::allocate(page, Some(taken[0].start)), None, "taken");
    let again = reserve();
    assert!(
        again.iter().all(|r| !taken.contains(r)),
        "{again:x?} {taken:x?}"
    );

    let a = carve(2 * page, 64 << 10).expect("two pages");
    assert_eq!(a % (64 << 10), 0);
    // SAFETY: ours, read-write.
    unsafe {
        (a as *mut u8).write(0x5a);
        ((a + 2 * page - 1) as *mut u8).write(0xa5);
        assert_eq!((a as *const u8).read(), 0x5a);
        assert_eq!(((a + 2 * page - 1) as *const u8).read(), 0xa5);
    }
    let b = carve(page, page).expect("a page");
    assert_ne!(a, b);
    assert!(!(a..a + 2 * page).contains(&b), "{a:#x} {b:#x}");
    assert_eq!(carve(usize::MAX / 2, page), None);
}
