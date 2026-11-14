//! `2d` keys (009 T021, FR-011), measured on refnode 7.0.45 (image `ref:7.0`,
//! `sha256:8bb4442761a63a8dda452744082cdac9644b429807eb977b42324a63cef59ca4`) and again on 9.0.4
//! (image `ref:9.0`, `sha256:452bdaf2127b8ff851e31410a267809b9829eded219979f2210e7c7efb85a058`,
//! same keys and errors), 30-10-2026: a
//! document `{_id: 1, loc: <value>}` inserted into a collection with `{loc: "2d"}` and the options
//! shown, and its key read with `find` + `hint` + `returnKey`.

use teseladb_bson::{RawDoc, extjson};
use teseladb_index::geo::planar::{DEFAULT_GRID, Grid};
use teseladb_index::keygen::{
    LOCATION_NOT_PAIR, NOT_LEGACY_PAIR, ONE_ELEMENT, OUT_OF_GRID, geo2d_keys,
};

fn keys(value: &str, grid: &Grid) -> Result<Vec<u64>, (i32, String, bool)> {
    let bytes = extjson::parse(&format!("{{\"v\": {value}}}")).unwrap().to_bytes().unwrap();
    let doc = RawDoc::from_bytes(&bytes).unwrap();
    geo2d_keys(doc.get("v").unwrap(), grid)
        .map(|ks| ks.into_iter().map(u64::from_be_bytes).collect())
        .map_err(|e| (e.code, e.reason, e.shows_document))
}

#[test]
fn geohashes_as_measured() {
    let measured: [(&str, Grid, u64); 13] = [
        ("[0, 0]", DEFAULT_GRID, 0xc000_0000_0000_0000),
        ("[1, 2]", DEFAULT_GRID, 0xc001_9e78_019e_7000),
        ("[-180, -180]", DEFAULT_GRID, 0),
        ("[179.999, 179.999]", DEFAULT_GRID, 0xffff_ffff_f303_3000),
        ("[180, 0]", DEFAULT_GRID, 0xeaaa_aaaa_aaaa_a000),
        ("[-73.97, 40.77]", DEFAULT_GRID, 0x61da_7d79_5da5_4000),
        ("[1, 2]", Grid { bits: 5, ..DEFAULT_GRID }, 0xc000_0000_0000_0000),
        ("[1, 2]", Grid { bits: 26, min: 0.0, max: 10.0 }, 0x0787_8787_8787_8000),
        ("[10, 10]", Grid { bits: 26, min: 0.0, max: 10.0 }, 0xffff_ffff_ffff_f000),
        ("{\"a\": 1, \"b\": 2}", DEFAULT_GRID, 0xc001_9e78_019e_7000),
        ("[1, 2, 3]", DEFAULT_GRID, 0xc001_9e78_019e_7000),
        ("[0, 0]", Grid { bits: 32, ..DEFAULT_GRID }, 0xc000_0000_0000_0000),
        ("[1e-9, 0]", Grid { bits: 32, ..DEFAULT_GRID }, 0xc000_0000_0000_0000),
    ];
    for (value, grid, hash) in measured {
        assert_eq!(keys(value, &grid), Ok(vec![hash]), "{value}");
    }
}

#[test]
fn several_locations_and_values_without_keys() {
    let two = keys("[[1, 2], [3, 4]]", &DEFAULT_GRID).unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[0], 0xc001_9e78_019e_7000, "RefDb's returnKey shows the first location");
    for none in ["\"x\"", "null", "[]", "{}"] {
        assert_eq!(keys(none, &DEFAULT_GRID), Ok(vec![]), "{none}");
    }
    assert_eq!(keys("[[1, 2], [1, 2]]", &DEFAULT_GRID).unwrap().len(), 1);
}

#[test]
fn rejections_as_measured() {
    let not_pair = (
        LOCATION_NOT_PAIR,
        "location object expected, location array not in correct format".to_string(),
        false,
    );
    let out = |range: &str| (OUT_OF_GRID, format!("point not in interval of [ {range} ]"), true);
    let cases = [
        ("[11, 2]", Grid { bits: 26, min: 0.0, max: 10.0 }, out("0, 10")),
        ("[21.5, -0.5]", Grid { bits: 26, min: -0.5, max: 10.25 }, out("-0.5, 10.25")),
        ("[0.05, 0.2]", Grid { bits: 26, min: 0.1, max: 1.0 / 3.0 }, out("0.1, 0.333333")),
        ("[1]", DEFAULT_GRID, (ONE_ELEMENT, "geo field only has 1 element".to_string(), true)),
        (
            "[1, \"a\"]",
            DEFAULT_GRID,
            (
                NOT_LEGACY_PAIR,
                "geo values must be 'legacy coordinate pairs' for 2d indexes".to_string(),
                true,
            ),
        ),
        ("[\"a\", 1]", DEFAULT_GRID, not_pair.clone()),
        ("{\"a\": \"x\"}", DEFAULT_GRID, not_pair.clone()),
        ("{\"type\": \"Point\", \"coordinates\": [1, 2]}", DEFAULT_GRID, not_pair.clone()),
        ("[{\"$numberDouble\": \"NaN\"}, 1]", DEFAULT_GRID, out("-180, 180")),
        ("[-180.000001, 0]", DEFAULT_GRID, out("-180, 180")),
        ("[0, 180.5]", DEFAULT_GRID, out("-180, 180")),
        ("[[1, 2], \"x\"]", DEFAULT_GRID, not_pair),
        (
            "[[1, 2], [1]]",
            DEFAULT_GRID,
            (ONE_ELEMENT, "geo field only has 1 element".to_string(), true),
        ),
    ];
    for (value, grid, err) in cases {
        assert_eq!(keys(value, &grid), Err(err), "{value}");
    }
}
