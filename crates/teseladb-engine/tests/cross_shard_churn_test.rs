//! Small blocks allocated on one shard and freed on another, at a high rate and from every
//! shard at once, as a cache shared by the shards does with its maps of maps: each shard inserts
//! into and removes from a map whose inner maps and values were allocated by any shard, so most
//! frees go through the owner's cross-shard list. Every block keeps its bytes until it is freed,
//! and the allocator's lists stay sound (a corrupt list faults the next allocation).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

/// A value whose bytes say which key it was made for.
fn value(key: u64, len: usize) -> Arc<[u8]> {
    (0..len).map(|i| (key as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

fn check(key: u64, v: &Arc<[u8]>) {
    let want = value(key, v.len());
    assert_eq!(&v[..], &want[..], "the bytes of {key} changed while it was held");
}

type Shared = Mutex<HashMap<u64, HashMap<u32, Arc<[u8]>>>>;

/// `ops` random inserts and removes on `map`, seeded by the shard.
fn churn(map: &Shared, seed: u64, ops: usize) {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..ops {
        let r = next();
        let file = r % 64;
        let block = ((r >> 8) % 16) as u32;
        let key = file * 1000 + u64::from(block);
        let mut m = map.lock().expect("the map");
        match (r >> 20) % 8 {
            // Drop a whole inner map, allocated by whichever shard made it.
            0 => {
                for (b, v) in m.remove(&file).into_iter().flatten() {
                    check(file * 1000 + u64::from(b), &v);
                }
            }
            1..=3 => {
                if let Some(inner) = m.get_mut(&file)
                    && let Some(v) = inner.remove(&block)
                {
                    check(key, &v);
                    if inner.is_empty() {
                        m.remove(&file);
                    }
                }
            }
            _ => {
                let len = 8 + (r >> 32) as usize % 200;
                if let Some(old) = m.entry(file).or_default().insert(block, value(key, len)) {
                    check(key, &old);
                }
            }
        }
    }
}

engine_test_case!(blocks_freed_by_other_shards_keep_the_allocator_sound, {
    assert!(Smp::count() >= 2, "the frees must cross shards: run with at least two");
    thread_async(|| {
        let map: Arc<Shared> = Arc::new(Mutex::new(HashMap::new()));
        for round in 0..4u64 {
            let m = map.clone();
            Smp::invoke_on_all(move || {
                churn(&m, 0x9E37_79B9 ^ ((this_shard_id() as u64) << 32) ^ round, 200_000);
                make_ready_future(())
            })
            .get()?;
        }
        let all = std::mem::take(&mut *map.lock().expect("the map"));
        for (file, inner) in all {
            for (b, v) in inner {
                check(file * 1000 + u64::from(b), &v);
            }
        }
        Ok(())
    })
});
