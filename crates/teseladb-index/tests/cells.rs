//! The cell hierarchy and bounded coverings (009 T020, FR-011).

use std::f64::consts::PI;

use teseladb_bson::{Value, extjson};

use teseladb_index::geo::cells::{
    Cell, CellId, MAX_LEVEL, face_uv, face_uv_to_xyz, st_to_uv, uv_to_st,
};
use teseladb_index::geo::cover::{CoverOptions, covering, level_for_min_width, normalize};
use teseladb_index::geo::geojson::Geometry;
use teseladb_index::geo::region::GeometryRegion;
use teseladb_index::geo::sphere::{Cap, Loop, Point, Polygon};

/// Leaf cells of measured points and the measured cell boundaries of face 0.
const LEAVES: &str = include_str!("data/geo-leaves.json");

fn measured(field: &str) -> Vec<Value> {
    let file = extjson::parse(LEAVES).unwrap();
    let Some(Value::Array(v)) = file.get(field) else { panic!("no {field}") };
    v.clone()
}

fn number(v: &Value) -> f64 {
    match v {
        Value::Double(d) => *d,
        Value::Int32(n) => f64::from(*n),
        other => panic!("not a number: {other:?}"),
    }
}

#[test]
fn every_measured_point_lies_in_the_measured_leaf() {
    let mut wrong = Vec::new();
    let all = measured("points");
    for p in &all {
        let Value::Array(p) = p else { panic!("a point is an array") };
        let (lng, lat) = (number(&p[0]), number(&p[1]));
        let Value::String(id) = &p[2] else { panic!("an id is a string") };
        let want: u64 = id.parse().unwrap();
        let got = CellId::from_point(Point::from_degrees(lng, lat)).0;
        if got != want {
            wrong.push(format!("[{lng}, {lat}] want {want:016x} got {got:016x}"));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), all.len(), wrong.join("\n"));
}

#[test]
fn measured_cell_boundaries_follow_the_quadratic_projection() {
    let all = measured("boundaries");
    assert_eq!(all.len(), 31);
    for (k, u) in all.iter().enumerate() {
        let s = (k as f64 + 1.0) / 32.0;
        assert!((st_to_uv(s) - number(u)).abs() < 1e-14, "boundary {k}");
    }
}

/// A deterministic sequence of points over the whole sphere.
fn points(n: usize) -> Vec<Point> {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..n).map(|_| Point::from_degrees(360.0 * next() - 180.0, 180.0 * next() - 90.0)).collect()
}

#[test]
fn projection_round_trips() {
    for s in [0.0, 0.1, 0.5, 0.75, 1.0] {
        assert!((uv_to_st(st_to_uv(s)) - s).abs() < 1e-15, "{s}");
    }
    for p in points(200) {
        let (face, u, v) = face_uv(p);
        let q = face_uv_to_xyz(face, u, v).normalize();
        assert!(p.angle(q) < 1e-15);
        assert!(u.abs() <= 1.0 && v.abs() <= 1.0);
    }
}

#[test]
fn leaf_cells_contain_their_points_and_ancestors_their_leaves() {
    for p in points(300) {
        let leaf = CellId::from_point(p);
        assert!(leaf.is_valid());
        assert_eq!(leaf.level(), MAX_LEVEL);
        assert!(Cell::new(leaf).contains(p));
        for level in 0..MAX_LEVEL {
            let parent = leaf.parent(level);
            assert_eq!(parent.level(), level);
            assert!(parent.contains(leaf));
            assert!(Cell::new(parent).contains(p));
            let (face, i, j) = parent.face_ij();
            assert_eq!(face, leaf.face());
            assert_eq!(CellId::from_face_ij(face, i, j).parent(level), parent);
        }
    }
}

#[test]
fn children_split_their_parent_along_the_curve() {
    for face in 0..6 {
        let f = CellId::from_face(face);
        assert_eq!((f.level(), f.face()), (0, face));
        let kids: Vec<_> = (0..4).map(|k| f.child(k)).collect();
        assert_eq!(kids[0].range_min(), f.range_min());
        assert_eq!(kids[3].range_max(), f.range_max());
        for w in kids.windows(2) {
            assert_eq!(w[0].range_max().0 + 2, w[1].range_min().0);
        }
        // Consecutive leaves along the curve are neighbours: their cells share an edge.
        let a = Cell::new(kids[1].range_max());
        let b = Cell::new(kids[2].range_min());
        assert!(
            a.vertices()
                .iter()
                .filter(|v| b.vertices().iter().any(|w| v.angle(*w) < 1e-12))
                .count()
                >= 2
        );
    }
    assert!(!CellId(0).is_valid());
    assert!(!CellId(7 << 61 | 1).is_valid());
}

#[test]
fn cells_tile_the_sphere() {
    let total: f64 = (0..6).map(|f| Cell::new(CellId::from_face(f)).outline().area()).sum();
    assert!((total - 4.0 * PI).abs() < 1e-9, "{total}");
    let cell = Cell::new(CellId::from_point(Point::from_degrees(10.0, 20.0)).parent(8));
    let parts: f64 = cell.children().iter().map(|c| c.outline().area()).sum();
    assert!((parts - cell.outline().area()).abs() < 1e-12);
    assert!(cell.contains(cell.center()) && cell.contains_strictly(cell.center()));
    assert!(cell.contains(cell.vertex(2)) && !cell.contains_strictly(cell.vertex(2)));
    assert!(!cell.contains(-cell.center()));
}

#[test]
fn vertex_neighbors_share_the_nearest_vertex() {
    for p in points(100) {
        for level in [1, 5, 12, 20] {
            let leaf = CellId::from_point(p);
            let n: Vec<CellId> = leaf.vertex_neighbors(level).into_iter().flatten().collect();
            assert_eq!(n[0], leaf.parent(level), "the leaf's own cell comes first");
            let cells: Vec<_> = n.iter().map(|&id| Cell::new(id)).collect();
            let shared = cells[0]
                .vertices()
                .into_iter()
                .find(|v| cells.iter().all(|c| c.vertices().iter().any(|w| v.angle(*w) < 1e-12)));
            let Some(shared) = shared else { panic!("no shared vertex at level {level}") };
            let [x, y, z] = shared.0.map(f64::abs);
            let cube_corner = (x - y).abs() < 1e-12 && (y - z).abs() < 1e-12;
            assert_eq!(n.len(), if cube_corner { 3 } else { 4 }, "{level}");
        }
    }
    // At a corner of the cube three faces meet.
    let corner = face_uv_to_xyz(0, 1.0 - 1e-12, 1.0 - 1e-12).normalize();
    let around = CellId::from_point(corner).vertex_neighbors(3);
    assert_eq!(around.iter().flatten().count(), 3);
    assert_eq!(around[3], None, "no diagonal cell at a cube corner");
}

#[test]
fn level_for_width() {
    assert_eq!(level_for_min_width(0.0), MAX_LEVEL);
    assert_eq!(level_for_min_width(10.0), 0);
    let w = 2.0 * 2f64.sqrt() / 3.0;
    assert_eq!(level_for_min_width(w / 8.0), 3);
    assert_eq!(level_for_min_width(w / 8.0 * 1.01), 2);
}

fn square(lng: f64, lat: f64, side: f64) -> Geometry {
    let v = [(0.0, 0.0), (side, 0.0), (side, side), (0.0, side)]
        .map(|(x, y)| Point::from_degrees(lng + x, lat + y));
    Geometry::Polygon(Polygon { loops: vec![Loop::smaller(v.to_vec())] })
}

#[test]
fn coverings_cover_and_respect_their_limits() {
    let options = CoverOptions { min_level: 2, max_level: 12, max_cells: 8 };
    for (k, p) in points(40).into_iter().enumerate() {
        let (lng, lat) = p.to_degrees();
        let g =
            square(lng.clamp(-170.0, 160.0), lat.clamp(-80.0, 70.0), [0.01, 0.5, 4.0, 9.0][k % 4]);
        let cells = covering(&GeometryRegion::new(&g), options);
        assert!(!cells.is_empty());
        assert!(cells.windows(2).all(|w| w[0].range_max() < w[1].range_min()), "sorted, disjoint");
        assert!(cells.iter().all(|c| (2..=12).contains(&c.level())));
        let Geometry::Polygon(poly) = &g else { panic!() };
        for v in poly.vertices() {
            let leaf = CellId::from_point(v);
            assert!(cells.iter().any(|c| c.contains(leaf)), "vertex covered");
        }
    }
}

#[test]
fn coverings_of_caps_and_lines() {
    let cap = Cap { center: Point::from_degrees(20.0, 30.0), radius: 0.01 };
    let cells = covering(&cap, CoverOptions { min_level: 0, max_level: 20, max_cells: 10 });
    assert!(cells.len() <= 10);
    assert!(cells.iter().any(|c| c.contains(CellId::from_point(cap.center))));
    let line =
        Geometry::LineString(vec![Point::from_degrees(0.5, 0.5), Point::from_degrees(1.5, 0.7)]);
    let cells = covering(
        &GeometryRegion::new(&line),
        CoverOptions { min_level: 0, max_level: 10, max_cells: 4 },
    );
    assert!(!cells.is_empty() && cells.len() <= 6);
    // A whole hemisphere starts from the faces.
    let big = Cap { center: Point([0.0, 0.0, 1.0]), radius: PI / 2.0 };
    let cells = covering(&big, CoverOptions { min_level: 1, max_level: 3, max_cells: 30 });
    assert!(cells.iter().all(|c| c.level() >= 1));
}

#[test]
fn normalize_merges_complete_siblings() {
    let parent = CellId::from_face(3).child(2);
    let kids: Vec<_> = (0..4).map(|k| parent.child(k)).collect();
    assert_eq!(normalize(kids.clone()), vec![parent]);
    assert_eq!(normalize(vec![kids[0], kids[1], kids[3]]).len(), 3);
    assert_eq!(normalize(vec![kids[2], parent, kids[2].child(1)]), vec![parent]);
    let all: Vec<_> = (0..4).flat_map(|k| (0..4).map(move |j| parent.child(k).child(j))).collect();
    assert_eq!(normalize(all), vec![parent]);
}
