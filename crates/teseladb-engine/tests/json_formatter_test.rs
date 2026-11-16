//! Tests of the JSON formatter: values, ranges, maps and objects formatted to a string and
//! written to an output stream.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, HashMap};

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{
    DataSink, OutputStream, OutputStreamOptions, OutputStreamState,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::vector_data_sink::VectorDataSink;
use teseladb::engine::http::api_docs::{ApiDocs20, DocEntry};
use teseladb::engine::http::request::Request;
use teseladb::engine::json::formatter::{Formatter, JsonPair, JsonWrite, State, ToJson};
use teseladb::engine::json::json_elements::{
    JsonBase, JsonBaseElement, JsonElement, JsonList, Jsonable, json_base_add,
    stream_range_as_array,
};
use teseladb::engine::net::packet::Packet;

fn to_json<T: ToJson + ?Sized>(v: &T) -> Sstring {
    v.to_json().expect("to_json")
}

teseladb::engine_test_case!(test_simple_values, {
    assert_eq!(to_json(&3i32), "3");
    assert_eq!(to_json(&3.0f64), "3");
    assert_eq!(to_json(&3.5f64), "3.5");
    assert_eq!(to_json(&true), "true");
    assert_eq!(to_json(&false), "false");
    assert_eq!(to_json("apa"), "\"apa\""); // to_json(const char*)
    assert_eq!(to_json(&Sstring::from("apa")), "\"apa\""); // to_json(const sstring&)
    assert_eq!(to_json(&b"apa"[..3]), "\"apa\""); // to_json(const char*, size_t)

    let str = Sstring::from_bytes(b"\0 COWA\x08U\nGA [{\r}]\x1a");
    let expected = Sstring::from("\"\\u0000 COWA\\bU\\nGA [{\\r}]\\u001A\"");
    assert_eq!(expected, to_json(&str)); // to_json(const sstring&)
    assert_eq!(expected, to_json(str.as_bytes())); // to_json(const char*, size_t)

    make_ready_future(())
});

teseladb::engine_test_case!(test_collections, {
    assert_eq!(to_json(&BTreeMap::from([(1, 2), (3, 4)])), "{1:2,3:4}");
    assert_eq!(to_json(&vec![1, 2, 3, 4]), "[1,2,3,4]");
    // A vector of pairs: the range form, which formats
    // each element in the array state (`ToJson for Vec` takes only `ToJson` elements).
    assert_eq!(
        Formatter::to_json_range(State::Array, [JsonPair(1, 2), JsonPair(3, 4)].iter())
            .expect("to_json"),
        "[{1:2},{3:4}]"
    );
    assert_eq!(to_json(&vec![BTreeMap::from([(1, 2)]), BTreeMap::from([(3, 4)])]), "[{1:2},{3:4}]");
    assert_eq!(to_json(&vec![vec![1, 2], vec![3, 4]]), "[[1,2],[3,4]]");

    make_ready_future(())
});

teseladb::engine_test_case!(test_ranges, {
    // A range: the formatter takes ranges of references, so the
    // view is materialized first.
    let iota: Vec<i32> = (1..5).collect();
    assert_eq!(Formatter::to_json_range(State::Array, iota.iter()).expect("to_json"), "[1,2,3,4]");
    // An enumerated range: each element is an index-value pair.
    assert_eq!(
        Formatter::to_json_range(
            State::Array,
            (5..9).enumerate().map(|(i, v)| JsonPair(i as u64, v)).collect::<Vec<_>>().iter()
        )
        .expect("to_json"),
        "[{0:5},{1:6},{2:7},{3:8}]"
    );

    make_ready_future(())
});

/// An object with registered members. The base lists the members on demand
/// (`JsonBase::element`) instead of keeping their addresses, so the object may move.
struct ObjectJson {
    subject: JsonElement<Sstring>,
    values: JsonList<i64>,
}

impl ObjectJson {
    fn register_params(&mut self) {
        json_base_add(&mut self.subject, "subject", false);
        json_base_add(&mut self.values, "values", false);
    }

    fn new() -> Self {
        let mut o = ObjectJson { subject: JsonElement::default(), values: JsonList::default() };
        o.register_params();
        o
    }

    /// A copy of `e`, with its members registered again.
    fn copy(e: &ObjectJson) -> Self {
        let mut o = Self::new();
        o.subject.set(e.subject.get().clone());
        o.values.assign(e.values._elements.iter().copied());
        o
    }
}

impl JsonBase for ObjectJson {
    fn element_count(&self) -> usize {
        2
    }

    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        match i {
            0 => Some(&self.subject),
            1 => Some(&self.values),
            _ => None,
        }
    }
}

impl Jsonable for ObjectJson {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        self.base_to_json()
    }

    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        (*self).base_write(s)
    }
}

teseladb::engine_test_case!(test_jsonable, {
    let mut obj = ObjectJson::new();
    obj.subject.set("foo");
    obj.values.push(1);
    obj.values.push(2);
    obj.values.push(3);

    assert_eq!(
        Jsonable::to_json(&obj).expect("to_json"),
        "{\"subject\": \"foo\", \"values\": [1,2,3]}"
    );
    let _copy = ObjectJson::copy(&obj);
    make_ready_future(())
});

fn formatter_check_expected<F: FnOnce(&mut OutputStream<u8>)>(expected: &str, f: F, close: bool) {
    let mut vec: Vec<Packet> = Vec::new();
    {
        // SAFETY: `vec` outlives the stream (dropped at the end of this block, after its
        // close) and is not touched until then.
        let sink = unsafe { VectorDataSink::new(&mut vec) };
        let mut out = OutputStream::<u8>::with_options(
            DataSink::new(Box::new(sink)),
            8,
            OutputStreamOptions::default(),
        );
        f(&mut out);
        if close {
            out.close().get().expect("close");
        }
    }
    let mut packets = Packet::default();
    for p in vec.drain(..) {
        packets.append(p);
    }
    packets.linearize_all();
    let buf = packets.release();
    let result = Sstring::from_bytes(buf[0].as_slice());
    assert_eq!(expected, result.as_str());
}

teseladb::engine_test_case!(test_stream_range_as_array, {
    teseladb::engine::core::thread::async_(|| {
        let expected = r#"[{"subject":"1","values":[1]}, {"subject":"2","values":[2]}, {"subject":"3","values":[3]}]"#;
        formatter_check_expected(
            expected,
            |out| {
                let mut mapper =
                    stream_range_as_array(vec![1i64, 2, 3], |i: &i64| -> Box<dyn Jsonable> {
                        let mut obj = ObjectJson::new();
                        obj.subject.set(Sstring::from(i.to_string().as_str()));
                        obj.values.push(*i);
                        Box::new(obj)
                    });
                let stream = std::mem::take(out);
                mapper(stream).get().expect("mapper");
            },
            false,
        );
        Ok(())
    })
});

fn write<T: JsonWrite>(out: &mut OutputStream<u8>, v: T) -> Future<()> {
    v.write_json(out)
}

teseladb::engine_test_case!(formatter_write, {
    teseladb::engine::core::thread::async_(|| {
        formatter_check_expected("3", |out| write(out, 3i32).get().expect("write"), true);
        formatter_check_expected("false", |out| write(out, false).get().expect("write"), true);
        formatter_check_expected("\"foo\"", |out| write(out, "foo").get().expect("write"), true);
        formatter_check_expected(
            "{1:2,3:4}",
            |out| write(out, BTreeMap::from([(1, 2), (3, 4)])).get().expect("write"),
            true,
        );
        // A map is written in the container's iteration order. A
        // `HashMap` has its own (seeded) order, so the expected text follows the order of this
        // map, which its copy keeps.
        let map = HashMap::from([(1, 2), (3, 4)]);
        let expected = format!(
            "{{{}}}",
            map.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
        );
        formatter_check_expected(
            &expected,
            |out| write(out, map.clone()).get().expect("write"),
            true,
        );
        formatter_check_expected(
            "[1,2,3,4]",
            |out| write(out, vec![1, 2, 3, 4]).get().expect("write"),
            true,
        );
        formatter_check_expected(
            "[{1:2},{3:4}]",
            |out| {
                // A vector of pairs: the range form in the array
                // state (`JsonWrite for Vec` takes only `JsonWrite` elements).
                Formatter::write_range(
                    out,
                    State::Array,
                    vec![JsonPair(1, 2), JsonPair(3, 4)].into_iter(),
                )
                .get()
                .expect("write")
            },
            true,
        );
        formatter_check_expected(
            "[{1:2},{3:4}]",
            |out| {
                write(out, vec![BTreeMap::from([(1, 2)]), BTreeMap::from([(3, 4)])])
                    .get()
                    .expect("write")
            },
            true,
        );
        formatter_check_expected(
            "[[1,2],[3,4]]",
            |out| write(out, vec![vec![1, 2], vec![3, 4]]).get().expect("write"),
            true,
        );
        formatter_check_expected(
            "[1,2,3,4]",
            |out| Formatter::write_range(out, State::Array, 1..5).get().expect("write"),
            true,
        );
        // An enumerated range: each element is an index-value pair.
        formatter_check_expected(
            "[{0:5},{1:6},{2:7},{3:8}]",
            |out| {
                Formatter::write_range(
                    out,
                    State::Array,
                    (5..9).enumerate().map(|(i, v)| JsonPair(i as u64, v)),
                )
                .get()
                .expect("write")
            },
            true,
        );
        Ok(())
    })
});

teseladb::engine_test_case!(formatter_write_outlives_stream_borrow, {
    teseladb::engine::core::thread::async_(|| {
        // The streaming writes keep the stream's state, not the borrow they were given: the
        // stream handle may move while they are pending, and the object written is owned by
        // the write (`base_write` and `write_json` take it by value), so no caller-side
        // object has to outlive the future.
        formatter_check_expected(
            "[{1:2},{3:4},{5:6}]",
            |out| {
                let pending = Formatter::write_range(
                    out,
                    State::Array,
                    vec![JsonPair(1, 2), JsonPair(3, 4), JsonPair(5, 6)].into_iter(),
                );
                let mut moved = Box::new(std::mem::take(out));
                pending.get().expect("write");
                moved.close().get().expect("close");
            },
            false,
        );
        formatter_check_expected(
            "{\"subject\":\"moved\",\"values\":[7,8]}",
            |out| {
                let mut obj = ObjectJson::new();
                obj.subject.set("moved");
                obj.values.push(7);
                obj.values.push(8);
                // The object moves into the write and the stream moves out of `out`.
                let pending = obj.base_write(out);
                let mut moved = std::mem::take(out);
                pending.get().expect("write");
                moved.close().get().expect("close");
            },
            false,
        );
        Ok(())
    })
});

teseladb::engine_test_case!(api_docs_write_outlives_docs, {
    teseladb::engine::core::thread::async_(|| {
        // ApiDocs20::write holds its entries: the document is written in full although the
        // docs are dropped before the write resolves.
        formatter_check_expected(
            "{\"paths\": {\"/a\": {}},\"definitions\": {\"d\": {}}}",
            |out| {
                let pending = {
                    let mut docs = ApiDocs20::default();
                    docs.add_api(DocEntry::new(|os: &mut OutputStreamState<u8>| {
                        os.write_str("{\"paths\": {")
                    }));
                    docs.add_api(DocEntry::new(|os: &mut OutputStreamState<u8>| {
                        os.write_str("\"/a\": {}")
                    }));
                    docs.add_definition(DocEntry::new(|os: &mut OutputStreamState<u8>| {
                        os.write_str("\"d\": {}")
                    }));
                    docs.write(std::mem::take(out), Box::new(Request::default()))
                };
                pending.get().expect("write");
            },
            false,
        );
        Ok(())
    })
});
