//! The order of the own properties of the built-ins (016 R11), as `Object.getOwnPropertyNames`
//! lists them on a reference router 9.0.4 (measured on 02-11). The built-ins are made in the order
//! that suits the code; this puts each object's properties in the measured order before they are
//! frozen. A path is a global (`""` is the global object), `X.prototype`, `X^` (the prototype of
//! `X`) or `%Name` (an intrinsic prototype of the realm).

use crate::interp::Interp;
use crate::value::ObjRef;

#[rustfmt::skip]
const ORDER: &[(&str, &[&str])] = &[
    ("", &["undefined", "Function", "Object", "eval", "globalThis", "Array", "Boolean", "JSON", "Date", "Math", "isNaN", "isFinite", "parseInt", "parseFloat", "NaN", "Infinity", "Number", "escape", "unescape", "decodeURI", "encodeURI", "decodeURIComponent", "encodeURIComponent", "String", "RegExp", "Error", "InternalError", "AggregateError", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError", "ArrayBuffer", "Int8Array", "Uint8Array", "Int16Array", "Uint16Array", "Int32Array", "Uint32Array", "Float32Array", "Float64Array", "Uint8ClampedArray", "BigInt64Array", "BigUint64Array", "Float16Array", "BigInt", "Proxy", "WeakMap", "Map", "Set", "DataView", "Symbol", "WeakSet", "Promise", "Iterator", "FinalizationRegistry", "WeakRef", "sleep", "gc", "print", "version", "buildInfo", "getJSHeapLimitMB", "BinData", "HexData", "MD5", "UUID", "bsonWoCompare", "bsonUnorderedFieldsCompare", "bsonBinaryEqual", "bsonObjToArray", "bsonToBase64", "bsonGetImmutable", "Code", "DBPointer", "DBRef", "MaxKey", "MinKey", "NumberDecimal", "NumberInt", "NumberLong", "ObjectId", "Timestamp", "hex_md5", "ISODate", "tojsononeline", "tojson", "tojsonObject", "toJsonForLog", "printjson", "isString", "isNumber", "isObject", "doassert", "_processMsg", "_buildAssertionMessage", "formatErrorMsg", "_doassert", "assert", "friendlyEqual", "_isEq", "assertThrowsHelper"]),
    ("Function", &["prototype", "length", "name"]),
    ("Object", &["assign", "getPrototypeOf", "setPrototypeOf", "getOwnPropertyDescriptor", "getOwnPropertyDescriptors", "keys", "values", "entries", "is", "defineProperty", "defineProperties", "create", "getOwnPropertyNames", "getOwnPropertySymbols", "isExtensible", "preventExtensions", "freeze", "isFrozen", "seal", "isSealed", "fromEntries", "hasOwn", "groupBy", "prototype", "extend", "merge", "deepMerge", "keySet", "length", "name"]),
    ("Array", &["isArray", "from", "fromAsync", "of", "prototype", "sum", "avg", "contains", "unique", "shuffle", "tojson", "fetchRefs", "stdDev", "length", "name"]),
    ("Boolean", &["prototype", "length", "name"]),
    ("Date", &["UTC", "parse", "now", "prototype", "timeFunc", "length", "name"]),
    ("Number", &["isFinite", "isInteger", "isNaN", "isSafeInteger", "POSITIVE_INFINITY", "NEGATIVE_INFINITY", "MAX_VALUE", "MIN_VALUE", "MAX_SAFE_INTEGER", "MIN_SAFE_INTEGER", "EPSILON", "prototype", "parseInt", "parseFloat", "NaN", "length", "name"]),
    ("String", &["fromCharCode", "fromCodePoint", "raw", "prototype", "length", "name"]),
    ("RegExp", &["escape", "input", "lastMatch", "lastParen", "leftContext", "rightContext", "$1", "$2", "$3", "$4", "$5", "$6", "$7", "$8", "$9", "$_", "$&", "$+", "$`", "$'", "prototype", "length", "name"]),
    ("Error", &["isError", "captureStackTrace", "prototype", "length", "name"]),
    ("InternalError", &["prototype", "length", "name"]),
    ("AggregateError", &["prototype", "length", "name"]),
    ("EvalError", &["prototype", "length", "name"]),
    ("RangeError", &["prototype", "length", "name"]),
    ("ReferenceError", &["prototype", "length", "name"]),
    ("SyntaxError", &["prototype", "length", "name"]),
    ("TypeError", &["prototype", "length", "name"]),
    ("URIError", &["prototype", "length", "name"]),
    ("ArrayBuffer", &["isView", "prototype", "length", "name"]),
    ("Int8Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Uint8Array", &["fromBase64", "fromHex", "BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Int16Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Uint16Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Int32Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Uint32Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Float32Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Float64Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Uint8ClampedArray", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("BigInt64Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("BigUint64Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("Float16Array", &["BYTES_PER_ELEMENT", "prototype", "length", "name"]),
    ("BigInt", &["asUintN", "asIntN", "prototype", "length", "name"]),
    ("Proxy", &["revocable", "length", "name"]),
    ("WeakMap", &["prototype", "length", "name"]),
    ("Map", &["groupBy", "prototype", "tojson", "length", "name"]),
    ("Set", &["prototype", "tojson", "length", "name"]),
    ("DataView", &["prototype", "length", "name"]),
    ("Symbol", &["for", "keyFor", "prototype", "isConcatSpreadable", "iterator", "match", "replace", "search", "species", "hasInstance", "split", "toPrimitive", "toStringTag", "unscopables", "asyncIterator", "matchAll", "length", "name"]),
    ("WeakSet", &["prototype", "length", "name"]),
    ("Promise", &["all", "allSettled", "any", "race", "reject", "resolve", "withResolvers", "try", "prototype", "length", "name"]),
    ("Iterator", &["from", "prototype", "length", "name"]),
    ("FinalizationRegistry", &["prototype", "length", "name"]),
    ("WeakRef", &["prototype", "length", "name"]),
    ("BinData", &["prototype", "length", "name"]),
    ("HexData", &["length", "name"]),
    ("MD5", &["length", "name"]),
    ("UUID", &["length", "name"]),
    ("Code", &["prototype", "length", "name"]),
    ("DBPointer", &["prototype", "length", "name"]),
    ("DBRef", &["prototype", "length", "name"]),
    ("NumberDecimal", &["prototype", "length", "name"]),
    ("NumberInt", &["prototype", "length", "name"]),
    ("NumberLong", &["prototype", "length", "name"]),
    ("ObjectId", &["prototype", "fromDate", "length", "name"]),
    ("Timestamp", &["prototype", "length", "name"]),
    ("ISODate", &["prototype", "length", "name"]),
    ("Function.prototype", &["toString", "apply", "call", "bind", "arguments", "caller", "constructor", "length", "name"]),
    ("Object.prototype", &["toString", "toLocaleString", "valueOf", "hasOwnProperty", "isPrototypeOf", "propertyIsEnumerable", "__defineGetter__", "__defineSetter__", "__lookupGetter__", "__lookupSetter__", "__proto__", "constructor", "bsonsize"]),
    ("Array.prototype", &["length", "toString", "toLocaleString", "join", "reverse", "sort", "push", "pop", "shift", "unshift", "splice", "concat", "slice", "lastIndexOf", "indexOf", "forEach", "map", "filter", "reduce", "reduceRight", "some", "every", "find", "findIndex", "copyWithin", "fill", "entries", "keys", "values", "includes", "flatMap", "flat", "at", "findLast", "findLastIndex", "toReversed", "toSorted", "toSpliced", "with", "constructor"]),
    ("Boolean.prototype", &["toString", "valueOf", "constructor"]),
    ("Date.prototype", &["getTime", "getTimezoneOffset", "getYear", "getFullYear", "getUTCFullYear", "getMonth", "getUTCMonth", "getDate", "getUTCDate", "getDay", "getUTCDay", "getHours", "getUTCHours", "getMinutes", "getUTCMinutes", "getSeconds", "getUTCSeconds", "getMilliseconds", "getUTCMilliseconds", "setTime", "setYear", "setFullYear", "setUTCFullYear", "setMonth", "setUTCMonth", "setDate", "setUTCDate", "setHours", "setUTCHours", "setMinutes", "setUTCMinutes", "setSeconds", "setUTCSeconds", "setMilliseconds", "setUTCMilliseconds", "toUTCString", "toLocaleString", "toLocaleDateString", "toLocaleTimeString", "toDateString", "toTimeString", "toISOString", "toJSON", "toString", "valueOf", "constructor", "toGMTString", "tojson"]),
    ("Number.prototype", &["toString", "toLocaleString", "valueOf", "toFixed", "toExponential", "toPrecision", "constructor", "toPercentStr", "zeroPad"]),
    ("String.prototype", &["length", "toString", "valueOf", "toLowerCase", "toUpperCase", "charAt", "charCodeAt", "codePointAt", "at", "substring", "padStart", "padEnd", "includes", "indexOf", "lastIndexOf", "startsWith", "endsWith", "trim", "trimStart", "trimEnd", "toLocaleLowerCase", "toLocaleUpperCase", "localeCompare", "repeat", "match", "matchAll", "search", "replace", "replaceAll", "split", "substr", "concat", "slice", "bold", "italics", "fixed", "strike", "small", "big", "blink", "sup", "sub", "anchor", "link", "fontcolor", "fontsize", "isWellFormed", "toWellFormed", "constructor", "trimLeft", "trimRight", "ltrim", "rtrim", "pad"]),
    ("RegExp.prototype", &["toString", "compile", "exec", "test", "flags", "hasIndices", "global", "ignoreCase", "multiline", "dotAll", "source", "sticky", "unicode", "unicodeSets", "constructor", "toJSON", "tojson"]),
    ("Error.prototype", &["toString", "message", "name", "stack", "constructor"]),
    ("InternalError.prototype", &["message", "name", "constructor"]),
    ("AggregateError.prototype", &["message", "name", "constructor"]),
    ("EvalError.prototype", &["message", "name", "constructor"]),
    ("RangeError.prototype", &["message", "name", "constructor"]),
    ("ReferenceError.prototype", &["message", "name", "constructor"]),
    ("SyntaxError.prototype", &["message", "name", "constructor"]),
    ("TypeError.prototype", &["message", "name", "constructor"]),
    ("URIError.prototype", &["message", "name", "constructor"]),
    ("ArrayBuffer.prototype", &["slice", "resize", "transfer", "transferToFixedLength", "byteLength", "maxByteLength", "resizable", "detached", "constructor"]),
    ("Int8Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Uint8Array.prototype", &["setFromBase64", "setFromHex", "toBase64", "toHex", "BYTES_PER_ELEMENT", "constructor"]),
    ("Int16Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Uint16Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Int32Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Uint32Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Float32Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Float64Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Uint8ClampedArray.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("BigInt64Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("BigUint64Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("Float16Array.prototype", &["BYTES_PER_ELEMENT", "constructor"]),
    ("BigInt.prototype", &["valueOf", "toString", "toLocaleString", "constructor"]),
    ("WeakMap.prototype", &["has", "get", "delete", "set", "constructor"]),
    ("Map.prototype", &["get", "has", "set", "delete", "keys", "values", "clear", "forEach", "entries", "size", "constructor"]),
    ("Set.prototype", &["has", "add", "delete", "entries", "clear", "forEach", "union", "difference", "intersection", "symmetricDifference", "isSubsetOf", "isSupersetOf", "isDisjointFrom", "values", "keys", "size", "constructor"]),
    ("DataView.prototype", &["getInt8", "getUint8", "getInt16", "getUint16", "getInt32", "getUint32", "getFloat16", "getFloat32", "getFloat64", "getBigInt64", "getBigUint64", "setInt8", "setUint8", "setInt16", "setUint16", "setInt32", "setUint32", "setFloat16", "setFloat32", "setFloat64", "setBigInt64", "setBigUint64", "buffer", "byteLength", "byteOffset", "constructor"]),
    ("Symbol.prototype", &["toString", "valueOf", "description", "constructor"]),
    ("WeakSet.prototype", &["add", "delete", "has", "constructor"]),
    ("Promise.prototype", &["then", "catch", "finally", "constructor"]),
    ("Iterator.prototype", &["map", "filter", "take", "drop", "flatMap", "reduce", "toArray", "forEach", "some", "every", "find", "constructor"]),
    ("FinalizationRegistry.prototype", &["register", "unregister", "constructor"]),
    ("WeakRef.prototype", &["deref", "constructor"]),
    ("BinData.prototype", &["constructor", "base64", "hex", "toString", "toJSON", "tojson", "subtype", "length"]),
    ("Code.prototype", &["constructor", "toString"]),
    ("DBPointer.prototype", &["constructor", "fetch", "tojson", "toJSON", "getCollection", "getId", "toString"]),
    ("DBRef.prototype", &["constructor", "fetch", "tojson", "getDb", "getCollection", "getRef", "getId", "toString"]),
    ("NumberDecimal.prototype", &["constructor", "toString", "toJSON", "tojson", "equals"]),
    ("NumberInt.prototype", &["constructor", "toNumber", "toString", "toJSON", "valueOf", "tojson"]),
    ("NumberLong.prototype", &["constructor", "toNumber", "toString", "toJSON", "valueOf", "compare", "floatApprox", "top", "bottom", "exactValueString", "tojson"]),
    ("ObjectId.prototype", &["constructor", "toString", "toJSON", "str", "tojson", "valueOf", "isObjectId", "getTimestamp", "equals"]),
    ("Timestamp.prototype", &["constructor", "toJSON", "tojson", "getTime", "getInc", "toString", "toStringIncomparable"]),
    ("Math", &["abs", "acos", "asin", "atan", "atan2", "ceil", "clz32", "cos", "exp", "floor", "imul", "fround", "f16round", "log", "max", "min", "pow", "random", "round", "sin", "sqrt", "tan", "log10", "log2", "log1p", "expm1", "cosh", "sinh", "tanh", "acosh", "asinh", "atanh", "hypot", "trunc", "sign", "cbrt", "sumPrecise", "E", "LOG2E", "LOG10E", "LN2", "LN10", "PI", "SQRT2", "SQRT1_2"]),
    ("JSON", &["parse", "stringify", "isRawJSON", "rawJSON"]),
    ("%GeneratorFunction", &["constructor", "prototype"]),
    ("%Generator", &["next", "throw", "return", "constructor"]),
    ("Int8Array^", &["from", "of", "prototype", "length", "name"]),
    ("Int8Array^.prototype", &["subarray", "set", "copyWithin", "every", "fill", "filter", "find", "findIndex", "findLast", "findLastIndex", "forEach", "indexOf", "join", "lastIndexOf", "map", "reduce", "reduceRight", "reverse", "slice", "some", "sort", "entries", "keys", "values", "includes", "toString", "toLocaleString", "at", "toReversed", "toSorted", "with", "length", "buffer", "byteLength", "byteOffset", "constructor"]),
    ("%AsyncGeneratorFunction", &["constructor", "prototype"]),
    ("%AsyncGenerator", &["next", "throw", "return", "constructor"]),
];

/// Puts the properties of each built-in of [`ORDER`] in its order; the ones it does not name
/// (symbols, internal slots) keep theirs after them.
pub fn apply(it: &Interp) {
    for (path, names) in ORDER {
        if let Some(o) = resolve(it, path) {
            let mut b = o.borrow_mut();
            let rank = |k: &str| names.iter().position(|n| *n == k).unwrap_or(names.len());
            b.props.sort_by_key(|(k, _)| rank(k));
        }
    }
}

/// The object `path` names.
fn resolve(it: &Interp, path: &str) -> Option<ObjRef> {
    if path.is_empty() {
        return Some(it.realm.global.clone());
    }
    if let Some(name) = path.strip_prefix('%') {
        return it.realm.protos.iter().find(|(n, _)| *n == name).map(|(_, o)| o.clone());
    }
    let mut parts = path.split('.');
    let first = parts.next()?;
    let (global, up) = match first.strip_suffix('^') {
        Some(g) => (g, true),
        None => (first, false),
    };
    let mut o = it.realm.global.borrow().get_own(global)?.value.as_obj().cloned()?;
    if up {
        let parent = o.borrow().proto.clone()?;
        o = parent;
    }
    for part in parts {
        let next = o.borrow().get_own(part)?.value.as_obj().cloned()?;
        o = next;
    }
    Some(o)
}
