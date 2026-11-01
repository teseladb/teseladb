//! A cursor's read of an interval, range by range (014 FR-004, FR-005, FR-010): the ranges in key
//! order without gaps or overlaps whatever splits or merges happen between two reads, and the rows
//! up to the lowest resolved mark of the ranges.

use teseladb_changestream::read::IntervalRead;
use teseladb_hlc::Hlc;

fn ts(p: u64) -> Hlc {
    Hlc { physical: p, logical: 0 }
}

/// A row: its key and its mark.
type Row = (u8, u64);

fn rows(r: &[Row]) -> Vec<Row> {
    r.to_vec()
}

fn finish(read: IntervalRead<Row>) -> (Vec<Row>, Hlc) {
    read.finish(|r| ts(r.1))
}

#[test]
fn every_range_is_read_once_and_rows_stop_at_the_lowest_mark() {
    let mut read = IntervalRead::new(&[1], &[9], ts(10));
    assert_eq!(read.next(), Some(&[1][..]));
    // The first range ends at 4: its read stops there.
    let stop = read.stop(&[4]);
    assert_eq!(stop, vec![4]);
    read.answered(stop, rows(&[(1, 20), (2, 28)]), ts(30), None);
    assert_eq!(read.next(), Some(&[4][..]));
    // The last range goes past the interval (or to the end of the space): it stops at its end.
    assert_eq!(read.stop(&[]), vec![9]);
    assert_eq!(read.stop(&[12]), vec![9]);
    read.answered(vec![9], rows(&[(5, 22), (6, 40)]), ts(50), Some(ts(25)));
    assert_eq!(read.next(), None);
    let (got, resolved) = finish(read);
    assert_eq!(resolved, Hlc { physical: 24, logical: u32::MAX });
    assert_eq!(got, [(1, 20), (5, 22)], "nothing above the resolved mark");
}

#[test]
fn a_split_between_two_reads_changes_only_where_the_next_one_stops() {
    // The table named one range [1, 9) when the first read was planned; it split at 5 before the
    // range answered, so the read of [1, 5) is followed by one of [5, 9): no key read twice.
    let mut read = IntervalRead::new(&[1], &[9], ts(10));
    let first = read.stop(&[5]);
    read.answered(first, rows(&[(2, 12)]), ts(40), None);
    let second = read.stop(&[]);
    assert_eq!(second, vec![9]);
    read.answered(second, rows(&[(7, 14)]), ts(20), None);
    let (got, resolved) = finish(read);
    assert_eq!((got, resolved), (vec![(2, 12), (7, 14)], ts(20)));
}

#[test]
fn a_range_with_nothing_past_the_start_keeps_the_read_at_the_start() {
    let mut read = IntervalRead::new(&[1], &[9], ts(10));
    read.answered(vec![4], rows(&[]), ts(5), None);
    read.answered(vec![9], rows(&[(6, 30)]), ts(60), None);
    assert_eq!(finish(read), (vec![], ts(10)));
    // An empty interval reads nothing and stays at its start.
    let read = IntervalRead::<Row>::new(&[3], &[3], ts(10));
    assert_eq!(read.next(), None);
    assert_eq!(finish(read), (vec![], ts(10)));
}
