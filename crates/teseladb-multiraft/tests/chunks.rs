//! A snapshot cut in chunks and put back together, with chunks out of order, restarted
//! transfers and declared sizes that do not fit.

use teseladb_multiraft::chunks::{
    CHUNK_LEN, Chunk, ChunkError, MAX_SNAPSHOT_LEN, Reassembly, split,
};
use teseladb_types::{Bytes, GroupId};

const G: GroupId = GroupId(0);

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn split_and_reassemble_round_trip() {
    for n in [0, 1, CHUNK_LEN - 1, CHUNK_LEN, 2 * CHUNK_LEN + 7] {
        let d = data(n);
        let chunks = split(G, 9, 3, &d);
        assert_eq!(chunks.len(), n.div_ceil(CHUNK_LEN).max(1), "{n}");
        assert!(chunks.iter().all(|c| c.data.len() <= CHUNK_LEN && c.total == n as u64));
        let mut r = Reassembly::default();
        let mut got = None;
        for (i, c) in chunks.into_iter().enumerate() {
            let out = r.push(c).expect("in order");
            assert_eq!(out.is_some(), i == n.div_ceil(CHUNK_LEN).max(1) - 1, "{n} {i}");
            got = got.or(out);
        }
        let got = got.expect("complete");
        assert_eq!((got.group, got.index, got.term), (G, 9, 3));
        assert_eq!(got.data.as_ref(), &d[..]);
        assert_eq!(r.in_progress(), 0);
    }
}

#[test]
fn groups_are_independent_and_a_new_start_replaces_the_transfer() {
    let a = split(G, 5, 1, &data(CHUNK_LEN + 1));
    let b = split(GroupId(7), 2, 1, &data(10));
    let mut r = Reassembly::default();
    assert_eq!(r.push(a[0].clone()), Ok(None));
    assert_eq!(r.push(b[0].clone()).expect("other group").map(|x| x.group), Some(GroupId(7)));
    assert_eq!(r.in_progress(), 1);
    // A newer snapshot starts over: the old transfer is replaced.
    let newer = split(G, 6, 1, &data(3));
    assert_eq!(r.push(newer[0].clone()).expect("new start").map(|x| x.index), Some(6));
    assert_eq!(r.in_progress(), 0);
}

#[test]
fn chunks_that_do_not_continue_the_transfer_are_refused() {
    let a = split(G, 5, 1, &data(2 * CHUNK_LEN + 1));
    let mut r = Reassembly::default();
    // A continuation without a start.
    assert_eq!(r.push(a[1].clone()), Err(ChunkError::OutOfOrder));
    // A gap: the transfer is dropped and the next continuation is refused too.
    assert_eq!(r.push(a[0].clone()), Ok(None));
    assert_eq!(r.push(a[2].clone()), Err(ChunkError::OutOfOrder));
    assert_eq!(r.in_progress(), 0);
    assert_eq!(r.push(a[1].clone()), Err(ChunkError::OutOfOrder));
    // Another snapshot's continuation in the middle of a transfer.
    assert_eq!(r.push(a[0].clone()), Ok(None));
    let other = Chunk { index: 6, ..a[1].clone() };
    assert_eq!(r.push(other), Err(ChunkError::OutOfOrder));
    assert_eq!(r.in_progress(), 0);
}

#[test]
fn declared_sizes_are_bounded() {
    let mut r = Reassembly::default();
    let c = |offset, total, len| Chunk {
        group: G,
        index: 1,
        term: 1,
        offset,
        total,
        data: Bytes::from_vec(vec![0; len]),
    };
    assert_eq!(r.push(c(0, MAX_SNAPSHOT_LEN + 1, 1)), Err(ChunkError::TooLarge));
    assert_eq!(r.push(c(0, 4, 5)), Err(ChunkError::TooLarge));
    // A chunk that overflows drops the transfer in progress.
    assert_eq!(r.push(c(0, 8, 4)), Ok(None));
    assert_eq!(r.push(c(4, 8, 5)), Err(ChunkError::TooLarge));
    assert_eq!(r.in_progress(), 0);
    // The largest size accepted only reserves a bounded buffer.
    assert_eq!(r.push(c(0, MAX_SNAPSHOT_LEN, 1)), Ok(None));
    assert_eq!(r.in_progress(), 1);
}
