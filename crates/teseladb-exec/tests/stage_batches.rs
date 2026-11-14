//! The algebra and the batches of a pipeline (010 FR-028, FR-029): stages that pass batches of
//! 1,024 documents from one to the next give what each stage gives over the whole input, run
//! one after the other; and `$sort` followed by `$limit` gives the first documents of the whole
//! sort, ties in their order.

use teseladb_bson::{Document, Value};
use teseladb_exec::reference::Reference;

fn d() -> Document {
    Document::new()
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> i32 {
        i32::try_from(self.next() % n as u64).expect("small")
    }
}

fn corpus(n: i32, seed: u64) -> Vec<Document> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|i| {
            let mut doc = d().with("_id", i).with("k", rng.below(7)).with("pad", "x".repeat(40));
            if rng.below(4) != 0 {
                doc.insert("v", Value::Double(f64::from(rng.below(1000)) / 8.0));
            }
            let tags: Vec<Value> = (0..rng.below(3)).map(Value::Int32).collect();
            doc.insert("tags", tags);
            doc.insert("sub", d().with("a", rng.below(5)).with("b", "s"));
            doc
        })
        .collect()
}

fn stored(docs: &[Document]) -> Vec<Document> {
    docs.iter()
        .map(|x| Document::from_stored(x.to_bytes().expect("small")).expect("canonical"))
        .collect()
}

/// `stages` run one at a time, each over the whole output of the one before.
fn one_by_one(docs: Vec<Document>, stages: &[Document]) -> Vec<Document> {
    stages.iter().fold(docs, |docs, st| {
        Reference::new(1)
            .with_collection("c", docs)
            .evaluate("c", std::slice::from_ref(st))
            .expect("runs")
    })
}

#[test]
fn stages_in_batches_give_what_each_stage_gives_over_the_whole_input() {
    let docs = stored(&corpus(5_000, 0x0BA7_C4E5));
    let stages = [
        d().with("$match", d().with("k", d().with("$ne", 3))),
        d().with("$unwind", "$tags"),
        d().with(
            "$addFields",
            d().with("w", d().with("$multiply", vec![Value::from("$k"), Value::from("$tags")])),
        ),
        d().with("$match", d().with("w", d().with("$lt", 6))),
        d().with("$project", d().with("pad", 0)),
    ];
    let batched =
        Reference::new(1).with_collection("c", docs.clone()).evaluate("c", &stages).expect("runs");
    assert!(batched.len() > 2 * 1024, "{}", batched.len());
    assert_eq!(batched, one_by_one(docs, &stages));
}

#[test]
fn a_sort_followed_by_a_limit_keeps_the_first_documents_of_the_whole_sort() {
    let docs = stored(&corpus(3_000, 0x50_27));
    for (key, dir) in [("k", 1), ("k", -1), ("v", 1), ("v", -1), ("sub.a", 1)] {
        for limit in [1_i64, 5, 100, 2_999, 3_000, 10_000] {
            let sort = d().with("$sort", d().with(key, dir));
            let top = [sort.clone(), d().with("$limit", limit)];
            let got = Reference::new(1)
                .with_collection("c", docs.clone())
                .evaluate("c", &top)
                .expect("top");
            let mut want = one_by_one(docs.clone(), &[sort]);
            want.truncate(usize::try_from(limit).expect("small"));
            assert_eq!(got, want, "{key} {dir} {limit}");
        }
    }
}
