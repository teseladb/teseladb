//! The record layer reads many records received at once in time linear in their size (found in
//! the e2e battery of 04-11-2026). Apart from `src/record.rs` because it measures time, which
//! `teseladb-tls`, a crate without I/O, does not.

use teseladb_tls::record::{ContentType, MAX_PLAINTEXT, RecordLayer, header};

/// Many records received at once are read in time linear in their size: a message of the
/// wire protocol of tens of MB arrives as thousands of records (found in the e2e battery of
/// 04-11-2026: a 22 MB write kept a core busy for minutes moving the rest of the input on each
/// record).
#[test]
fn many_records_at_once_are_read_in_linear_time() {
    const RECORDS: usize = 8_192;
    let mut w = RecordLayer::default();
    let mut out = Vec::new();
    w.write(ContentType::ApplicationData, &vec![3u8; RECORDS * MAX_PLAINTEXT], &mut out)
        .expect("write succeeds");
    let mut r = RecordLayer::default();
    let start = std::time::Instant::now();
    r.push_input(&out);
    let mut read = 0;
    while let Some(rec) = r.next_record().expect("next_record succeeds") {
        read += rec.data.len();
        // Input arriving while records are read joins the rest.
        if read == MAX_PLAINTEXT {
            r.push_input(&header(ContentType::ApplicationData, 1));
            r.push_input(&[9]);
        }
    }
    assert_eq!(read, RECORDS * MAX_PLAINTEXT + 1);
    assert!(!r.has_input());
    // 128 MiB moved once take a fraction of a second; moved again on each record, minutes.
    assert!(start.elapsed() < std::time::Duration::from_secs(10), "{:?}", start.elapsed());
}
