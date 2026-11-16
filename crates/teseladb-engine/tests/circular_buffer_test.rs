//! Tests of `CircularBuffer`: erasing ranges at the ends and in the middle, iterator
//! stability when erasing at the ends, and index comparisons around wrap-around.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;

use minstd::{DefaultRandomEngine, UniformIntDistribution};
use teseladb::engine::core::circular_buffer::CircularBuffer;

// The buffer iterates.
fn _static_checks(b: &CircularBuffer<i32>) {
    for _ in b.iter() {}
}

#[test]
fn test_erasing() {
    let mut buf = CircularBuffer::<i32>::new();

    buf.push_back(3);
    buf.erase(buf.begin(), buf.end());

    assert!(buf.size() == 0);
    assert!(buf.empty());

    buf.push_back(1);
    buf.push_back(2);
    buf.push_back(3);
    buf.push_back(4);
    buf.push_back(5);

    // Remove the even elements, keeping the order of the others.
    let mut w = 0usize;
    for r in 0..buf.size() {
        if (buf[r] & 1) != 0 {
            buf[w] = buf[r];
            w += 1;
        }
    }
    buf.erase(buf.begin() + w as isize, buf.end());

    assert!(buf.size() == 3);
    assert!(!buf.empty());
    {
        let mut i = buf.begin();
        assert_eq!(buf[i], 1);
        i += 1;
        assert_eq!(buf[i], 3);
        i += 1;
        assert_eq!(buf[i], 5);
        i += 1;
        assert!(i == buf.end());
    }
}

#[test]
fn test_erasing_at_beginning_or_end_does_not_invalidate_iterators() {
    // Erasing at either end must not move the remaining elements, as in a double-ended queue.

    let mut buf = CircularBuffer::<i32>::new();

    buf.push_back(1);
    buf.push_back(2);
    buf.push_back(3);
    buf.push_back(4);
    buf.push_back(5);

    let ptr_to_3: *const i32 = &buf[2];
    let iterator_to_3 = buf.begin() + 2;
    // SAFETY: erasing at either end does not move the other elements.
    assert!(unsafe { *ptr_to_3 } == 3);
    assert!(buf[iterator_to_3] == 3);

    buf.erase(buf.begin(), buf.begin() + 2);

    // SAFETY: erasing at the front does not move the remaining elements, so `ptr_to_3` still points at a live element.
    assert!(unsafe { *ptr_to_3 } == 3);
    assert!(buf[iterator_to_3] == 3);

    buf.erase(buf.begin() + 1, buf.end());

    // SAFETY: erasing at the back does not move the remaining elements, so `ptr_to_3` still points at a live element.
    assert!(unsafe { *ptr_to_3 } == 3);
    assert!(buf[iterator_to_3] == 3);

    assert!(buf.size() == 1);
}

#[test]
fn test_erasing_in_the_middle() {
    let mut buf = CircularBuffer::<i32>::new();

    for i in 0..10 {
        buf.push_back(i);
    }

    let mut i = buf.erase(buf.begin() + 3, buf.begin() + 6);
    assert_eq!(buf[i], 6);

    i = buf.begin();
    for expected in [0, 1, 2, 6, 7, 8, 9] {
        assert_eq!(buf[i], expected);
        i += 1;
    }
    assert!(i == buf.end());
}

#[test]
fn test_underflow_index_iterator_comparison() {
    let mut buf = CircularBuffer::<u32>::new();

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    println!("seed={}", seed);
    let mut rnd_engine = DefaultRandomEngine::new(seed);
    let count_dist = UniformIntDistribution::new(0, 20);
    let bool_dist = UniformIntDistribution::new(0, 1);

    let push_back = |buf: &mut CircularBuffer<u32>, n: u32| {
        for i in 0..n {
            buf.push_back(i);
        }
    };

    let push_front = |buf: &mut CircularBuffer<u32>, n: u32| {
        for i in 0..n {
            buf.push_front(i);
        }
    };

    for i in 0..16u32 {
        let push_back_count = count_dist.sample(&mut rnd_engine) as u32;
        let push_front_count = count_dist.sample(&mut rnd_engine) as u32;
        println!(
            "round[{}]: {} front: {} back: {}",
            i,
            buf.size(),
            push_front_count,
            push_back_count
        );
        if bool_dist.sample(&mut rnd_engine) != 0 {
            push_back(&mut buf, push_back_count);
            push_front(&mut buf, (20 - push_back_count).max(push_front_count));
        } else {
            push_front(&mut buf, push_front_count);
            push_back(&mut buf, (20 - push_front_count).max(push_back_count));
        }

        if buf.empty() {
            continue;
        }

        let mut it1 = buf.begin();
        while it1 != buf.end() {
            let mut bypass = false;
            let mut it2 = buf.end();
            while it2 != buf.begin() {
                let mut itl = it1;
                let mut ith = it2;
                if bypass {
                    std::mem::swap(&mut itl, &mut ith);
                }
                if itl == ith {
                    bypass = true;
                } else {
                    assert!(itl < ith);
                    assert!(ith > itl);
                    // Check the negated comparisons too.
                    #[allow(clippy::neg_cmp_op_on_partial_ord)]
                    let (not_lt, not_gt) = (!(ith < itl), !(itl > ith));
                    assert!(not_lt);
                    assert!(not_gt);
                }
                assert!(itl <= ith);
                assert!(itl <= itl);
                assert!(ith <= ith);
                assert!(ith >= itl);
                assert!(itl >= itl);
                assert!(ith >= ith);
                it2 -= 1;
            }
            it1 += 1;
        }

        let erase_count = count_dist.sample(&mut rnd_engine) as usize;
        let offset = count_dist.sample(&mut rnd_engine) as usize;
        println!("round[{}]: {} @ {}", i, erase_count, offset);
        let last = (offset + erase_count).min(buf.size());
        buf.erase(buf.begin() + offset as isize, buf.begin() + last as isize);
    }
}
