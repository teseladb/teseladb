//! 019 T026, FR-021: the tools against those of the RefDb MCP Server fixed in the contract
//! (`reference_tools.json`, from `scripts/mcp-reference-tools.sh`): the same names, the arguments
//! each tool reads are the properties and required ones of its `inputSchema`, the same hints, and
//! a `structuredContent` with the fields of its `outputSchema`.

use teseladb_json::Value;
use teseladb_mcp::tools::{TOOLS, input_schema, listing, own_input_schema, reference};

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Object(o)) => {
            let mut k: Vec<String> = o.iter().map(|(k, _)| k.to_owned()).collect();
            k.sort();
            k
        }
        Some(Value::Array(a)) => {
            let mut k: Vec<String> =
                a.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            k.sort();
            k
        }
        _ => Vec::new(),
    }
}

#[test]
fn every_tool_matches_the_pinned_refdb_mcp_server() {
    let reference_names = {
        let Ok(Value::Array(all)) = teseladb_json::parse(teseladb_mcp::tools::REFERENCE.as_bytes())
        else {
            panic!("reference_tools.json does not read")
        };
        let mut n: Vec<String> = all
            .iter()
            .filter_map(|t| t.as_object()?.get("name")?.as_str().map(str::to_owned))
            .collect();
        n.sort();
        n
    };
    let mut ours: Vec<String> = TOOLS.iter().map(|t| t.name.to_owned()).collect();
    ours.sort();
    assert_eq!(ours, reference_names);
    assert_eq!(ours.len(), 25);
    for t in TOOLS {
        let r = reference(t.name).unwrap();
        let schema = input_schema(t);
        let own = own_input_schema(t);
        assert_eq!(
            names(schema.get("properties")),
            names(own.get("properties")),
            "arguments of {}",
            t.name
        );
        assert_eq!(names(schema.get("required")), names(own.get("required")), "{}", t.name);
        let hints = r.get("annotations").and_then(Value::as_object).unwrap();
        let listed = listing(t);
        let ann = listed.get("annotations").and_then(Value::as_object).unwrap();
        for h in ["readOnlyHint", "destructiveHint", "openWorldHint"] {
            assert_eq!(ann.get(h), hints.get(h), "{h} of {}", t.name);
        }
        let out = r.get("outputSchema").and_then(Value::as_object);
        let fields = out.map(|o| names(o.get("properties"))).unwrap_or_default();
        let mut own_fields: Vec<String> = t.output.iter().map(|f| (*f).to_owned()).collect();
        own_fields.sort();
        assert_eq!(fields, own_fields, "structuredContent of {}", t.name);
        assert_eq!(listed.get("inputSchema"), Some(&Value::Object(schema)));
    }
}
