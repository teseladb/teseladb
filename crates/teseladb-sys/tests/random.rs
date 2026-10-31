//! Operating-system randomness (013 FR-001).

#[test]
fn fill_random_fills_buffers_longer_than_one_getentropy_call() {
    let mut a = vec![0u8; 1000];
    let mut b = vec![0u8; 1000];
    teseladb_sys::fill_random(&mut a).expect("entropy");
    teseladb_sys::fill_random(&mut b).expect("entropy");
    assert_ne!(a, b);
    // 1000 random bytes all zero in their last 744 has probability 2^-5952.
    assert!(a[256..].iter().any(|&x| x != 0));
    teseladb_sys::fill_random(&mut []).expect("empty");
}
