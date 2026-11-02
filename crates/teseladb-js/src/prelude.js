// TeselaDB's code for the globals that the JavaScript engine of RefDb 9.0 defines in JavaScript
// (tojson, assert, hex_md5, ISODate and the additions to Object, Array, String, Number, Date, Map
// and Set). Written from what those functions do on a refrouter 9.0.4, measured on 02-11 by calling
// them with many arguments and recording results, errors and the properties they read; nothing
// here comes from RefDb's code. The script runs once, in sloppy mode as the originals do, when
// the globals are built; `__teseladbShellHost` carries the native helpers and is removed after.
(function (host) {
    // The built-ins are captured now: a program that replaces a global must not change how these
    // functions behave, as on the refrouter, where they live in a realm of their own.
    var global = globalThis;
    var nativePrint = global.print;
    var stringify = JSON.stringify;
    var isArray = Array.isArray;
    var arrayFrom = Array.from;
    var ownKeys = Object.keys;
    var sqrt = Math.sqrt;
    var round = Math.round;
    var floor = Math.floor;
    var DateCtor = Date;
    var ErrorCtor = Error;
    var ReferenceErrorCtor = ReferenceError;
    var SetCtor = Set;
    var ProxyCtor = Proxy;
    var isNotANumber = isNaN;
    var toNumber = Number;
    var classOf = Object.prototype.toString;

    // Depth from which objects print as `[Object]` and arrays as `[Array]`.
    var MAX_DEPTH = 100;
    // `tojson` puts an object on one line when its text is shorter than this.
    var ONE_LINE_BELOW = 80;
    var ASSERT_PREFIX = "assert failed";
    var NOT_THROWN = "did not throw exception";
    var THREW = "threw unexpected exception: ";
    var THIS_TOUCHED = "Attempted to access 'this' during function call in assert.throws/doesNotThrow." +
        " Instead, wrap the function argument in another function.";
    var NAN_DATE = 'ISODate("0NaN-NaN-NaNTNaN:NaN:NaNZ")';
    var ISO_PATTERN = /^(\d{4})-?(\d{2})-?(\d{2})(?:[T ](\d{2})(?::?(\d{2})(?::?(\d{2})(?:\.(\d+))?)?)?(Z|([+-])(\d{2})(?::?(\d{2}))?)?)?$/;
    var MAX_YEAR = 9999;

    function missing(name) {
        // Names the shell's code uses and the engine does not define.
        return new ReferenceErrorCtor(name + " is not defined");
    }

    var defineProperty = Object.defineProperty;
    var WeakSetCtor = WeakSet;
    var ObjectIdCtor = ObjectId;

    // `fixed`: the refrouter declares the global (it cannot be deleted) instead of assigning it.
    function shellGlobal(name, value, fixed) {
        defineProperty(global, name, { value: value, writable: true, enumerable: true, configurable: !fixed });
    }

    // Value printing.

    function oneLine(nolint) {
        return nolint == null || nolint == true;
    }

    function printValue(x, indent, nolint, depth, sortKeys) {
        if (x === null) return "null";
        if (x === undefined) return "undefined";
        indent = indent || "";
        if (sortKeys === undefined) sortKeys = false;
        var s;
        switch (typeof x) {
            case "string":
                return stringify(x);
            case "number":
            case "boolean":
                return "" + x;
            case "function":
                return x.toString();
            case "object":
                s = printObject(x, indent, nolint, depth, sortKeys);
                break;
            default:
                throw new ErrorCtor("tojson can't handle type " + typeof x);
        }
        if (oneLine(nolint) && s.length < ONE_LINE_BELOW && !indent) {
            s = s.replace(/[\t\r\n]+/g, " ");
        }
        return s;
    }

    function block(open, parts, close, indent, nolint, depth) {
        var pad = indent.repeat(depth);
        if (parts.length == 0) return open + " " + close;
        if (nolint) return open + " " + parts.join(", ") + " " + close;
        var lines = [];
        for (var i = 0; i < parts.length; i++) lines.push(pad + indent + parts[i]);
        return open + "\n" + lines.join(",\n") + "\n" + pad + close;
    }

    function unit(indent, nolint) {
        return nolint ? "" : indent || "\t";
    }

    function printObject(x, indent, nolint, depth, sortKeys) {
        if (typeof x !== "object") {
            // The shell checks this with `assert.eq`, whose failure message needs `patchDiff`.
            throw missing("patchDiff");
        }
        if (typeof x.tojson == "function" && x.tojson != tojson) {
            return x.tojson(indent, nolint, depth, sortKeys);
        }
        if (x.constructor && typeof x.constructor.tojson == "function" &&
            x.constructor.tojson != tojson) {
            return x.constructor.tojson(x, indent, nolint, depth, sortKeys);
        }
        try {
            x.toString();
        } catch (e) {
            return "[Object]";
        }
        x._simpleKeys;
        if (depth > MAX_DEPTH) return "[Object]";
        indent = unit(indent, nolint);
        var keys = [];
        for (var k in x) keys.push(k);
        if (sortKeys) keys.sort();
        var parts = [];
        for (var j = 0; j < keys.length; j++) {
            var value = x[keys[j]];
            // Fields that are undefined or null are left out (measured: `{ "b" : 1 }` for {a: null, b: 1}).
            if (value == null) continue;
            parts.push('"' + keys[j] + '" : ' + printValue(value, indent, nolint, depth + 1, sortKeys));
        }
        return block("{", parts, "}", indent, nolint, depth);
    }

    function printArray(a, indent, nolint, depth, sortKeys) {
        indent = unit(indent, nolint);
        if (depth > MAX_DEPTH) return "[Array]";
        if (a.length == 0) return "[ ]";
        var parts = [];
        for (var i = 0; i < a.length; i++) {
            parts.push(printValue(a[i], indent, nolint, depth + 1, sortKeys));
        }
        return block("[", parts, "]", indent, nolint, depth);
    }

    var tojson = function tojson(x, indent, nolint, depth = 0, sortKeys) {
        return printValue(x, indent, nolint, depth, sortKeys);
    };
    tojson.MAX_DEPTH = MAX_DEPTH;

    var tojsonObject = function tojsonObject(x, indent, nolint, depth = 0, sortKeys) {
        return printObject(x, indent, nolint, depth, sortKeys);
    };

    var tojsononeline = function tojsononeline(x) {
        return printValue(x, " ", true, 0);
    };

    var printjson = function printjson(x) {
        nativePrint(printValue(x, undefined, undefined, 0));
    };

    var toJsonForLog = function toJsonForLog(x) {
        var ancestors = [];
        return stringify(x, function (key, value) {
            if (value === undefined) return { "$undefined": true };
            if (typeof value !== "object" || value === null) return value;
            while (ancestors.length && ancestors[ancestors.length - 1] !== this) ancestors.pop();
            if (ancestors.includes(value)) return "[recursive]";
            ancestors.push(value);
            return value;
        });
    };

    Array.tojson = function (a, indent, nolint, depth = 0, sortKeys) {
        requireArray(a, "tojson");
        return printArray(a, indent, nolint, depth, sortKeys);
    };

    Map.tojson = function (m, indent, nolint, depth) {
        var entries = arrayFrom(m.entries());
        return "new Map(" + printArray(entries, indent, nolint, depth || 0) + ")";
    };

    Set.tojson = function (s, indent, nolint, depth) {
        return "new Set(" + printArray(arrayFrom(s), indent, nolint, depth || 0) + ")";
    };

    Date.prototype.tojson = function () {
        if (isNotANumber(this.getTime())) return NAN_DATE;
        var str = this.toISOString();
        return 'ISODate("' + str.replace(".000Z", "Z") + '")';
    };

    // Type tests.

    var isString = function isString(x) {
        return typeof x === "string";
    };
    var isNumber = function isNumber(x) {
        return typeof x === "number";
    };
    var isObject = function isObject(x) {
        return typeof x === "object";
    };

    // Assertions.

    var _processMsg = function _processMsg(msg) {
        if (typeof msg === "function") msg = msg();
        return typeof msg === "object" ? printValue(msg, undefined, undefined, 0) : msg;
    };

    var _buildAssertionMessage = function _buildAssertionMessage(msg, prefix) {
        var text = prefix || "";
        if (msg) {
            var detail = _processMsg(msg);
            text = text ? text + " : " + detail : detail;
        }
        return text;
    };

    var formatErrorMsg = function formatErrorMsg(msg, attr = {}, serializeFn = tojson) {
        var keys = ownKeys(attr);
        for (var i = 0; i < keys.length; i++) {
            var text = serializeFn(attr[keys[i]]);
            msg = msg.replaceAll("{" + keys[i] + "}", text);
        }
        return msg;
    };

    var doassert = function doassert(msg) {
        throw new ErrorCtor(_processMsg(msg));
    };

    var _doassert = function _doassert(msg, prefix, attr) {
        if (attr !== undefined) prefix = formatErrorMsg(prefix, attr);
        doassert(_buildAssertionMessage(msg, prefix));
    };

    var assert = function assert(value, msg, attr) {
        if (arguments.length > 3) doassert("Too many parameters to assert().");
        if (value) return;
        _doassert(msg, ASSERT_PREFIX, attr);
    };

    function sameText(a, b) {
        if (a == b) return true;
        var left = printValue(a, "", true, 0).replace(/NumberInt\((-?\d+)\)/g, "$1");
        var right = printValue(b, "", true, 0).replace(/NumberInt\((-?\d+)\)/g, "$1");
        return left == right;
    }

    var friendlyEqual = function friendlyEqual(a, b) {
        return sameText(a, b);
    };
    var _isEq = function _isEq(a, b) {
        return sameText(a, b);
    };

    assert.eq = function (a, b, msg, attr) {
        if (sameText(a, b)) return;
        printValue(a, undefined, undefined, 0);
        printValue(b, undefined, undefined, 0);
        throw missing("patchDiff");
    };

    // Runs `func` with a `this` that records any read, write, `in` or `delete` on it.
    function runGuarded(func, hasParams, params) {
        if (typeof func !== "function") throw new ErrorCtor("1st argument must be a function");
        var isArgs = classOf.call(params) === "[object Arguments]";
        if (hasParams && !isArray(params) && !isArgs) {
            throw new ErrorCtor("2nd argument must be an Array or Arguments object");
        }
        var touched = false;
        var touch = function () {
            touched = true;
        };
        var guard = new ProxyCtor({}, {
            get: function () {
                touch();
                return undefined;
            },
            set: function () {
                touch();
                return true;
            },
            has: function () {
                touch();
                return false;
            },
            deleteProperty: function () {
                touch();
                return true;
            },
        });
        var outcome = { error: null, res: null };
        try {
            outcome.res = func.apply(guard, params);
        } catch (e) {
            outcome.error = e;
        }
        if (touched) throw new ErrorCtor(THIS_TOUCHED);
        return outcome;
    }

    var assertThrowsHelper = function assertThrowsHelper(func, params) {
        return runGuarded(func, arguments.length > 1, params);
    };

    assert.doesNotThrow = function (func, params, msg, attr) {
        var outcome = runGuarded(func, arguments.length > 1, params);
        var error = outcome.error;
        if (error) {
            var summary = printValue({ code: error.code, message: error.message }, undefined, undefined, 0);
            _doassert(msg, THREW + summary, attr);
        }
        return outcome.res;
    };

    assert.throws = function (func, params, msg, attr) {
        var outcome = runGuarded(func, arguments.length > 1, params);
        if (!outcome.error) _doassert(msg, NOT_THROWN, attr);
        return outcome.error;
    };

    // Hashes and dates.

    var hex_md5 = function hex_md5(s) {
        var units = [];
        for (var i = 0; i < s.length; i++) units.push(s.charCodeAt(i));
        return host.md5(units);
    };

    // The dates that `ISODate` made: `ObjectId.fromDate` only takes those, because on the refrouter
    // they are the only ones of the shell's realm (any other date fails its `instanceof Date`).
    var shellDates = new WeakSetCtor();

    var ISODate = function ISODate(isoDateStr) {
        if (!isoDateStr) return shellDate(new DateCtor());
        var m = ISO_PATTERN.exec(isoDateStr);
        if (!m) throw new ErrorCtor("invalid ISO date: " + isoDateStr);
        var d = new DateCtor(0);
        var ms = m[7] ? round(toNumber("0." + m[7]) * 1000) : 0;
        d.setUTCFullYear(toNumber(m[1]), toNumber(m[2]) - 1, toNumber(m[3]));
        d.setUTCHours(toNumber(m[4] || 0), toNumber(m[5] || 0), toNumber(m[6] || 0), ms);
        if (m[9]) {
            var minutes = toNumber(m[10]) * 60 + toNumber(m[11] || 0);
            var sign = m[9] == "-" ? -1 : 1;
            d = new DateCtor(d.getTime() - sign * minutes * 60000);
        }
        var year = d.getUTCFullYear();
        if (!(year >= 0 && year <= MAX_YEAR)) throw new ErrorCtor("invalid ISO date: " + isoDateStr);
        return shellDate(d);
    };

    function shellDate(d) {
        shellDates.add(d);
        return d;
    }

    ObjectId.fromDate = function (date) {
        if (date === undefined) throw new ErrorCtor("date missing or undefined");
        if (!shellDates.has(date)) {
            throw new ErrorCtor("Cannot create ObjectId from " + typeof date + ": " + printValue(date, undefined, undefined, 0));
        }
        var seconds = floor(date.getTime() / 1000);
        return ObjectIdCtor(seconds.toString(16).padStart(8, "0") + "0000000000000000");
    };

    Date.timeFunc = function (theFunc, numTimes = 1) {
        var start = new DateCtor();
        for (var i = 0; i < numTimes; i++) theFunc();
        return new DateCtor().getTime() - start.getTime();
    };

    // Objects.

    Object.extend = function (dst, src, deep) {
        for (var k in src) {
            var v = src[k];
            if (deep && typeof v == "object" && v !== null) {
                v = extend(typeof v.length == "number" ? [] : {}, v, true);
            }
            dst[k] = v;
        }
        return dst;
    };
    var extend = Object.extend;

    Object.merge = function (dst, src, deep) {
        var out = {};
        extend(out, dst, deep);
        extend(out, src, deep);
        return out;
    };

    function isObjectValue(v) {
        return typeof v === "object" && v !== null;
    }

    function mergeInto(target, source = {}) {
        var keys = ownKeys(source);
        for (var i = 0; i < keys.length; i++) {
            var k = keys[i];
            var incoming = source[k];
            var current = target[k];
            if (isArray(current) && isArray(incoming)) {
                target[k] = current.concat(incoming);
            } else if (isObjectValue(current) && isObjectValue(incoming)) {
                target[k] = deepMerge(current, incoming);
            } else {
                target[k] = incoming;
            }
        }
    }

    Object.deepMerge = function () {
        var out = {};
        for (var i = 0; i < arguments.length; i++) mergeInto(out, arguments[i]);
        return out;
    };
    var deepMerge = Object.deepMerge;

    Object.keySet = function (o) {
        var keys = [];
        for (var k in o) {
            if (!(k in o.__proto__) || o[k] !== o.__proto__[k]) keys.push(k);
        }
        return keys;
    };

    // Arrays.

    function requireArray(a, name) {
        if (!isArray(a)) throw new ErrorCtor("The first argument to Array." + name + " must be an array");
    }

    Array.sum = function (arr) {
        requireArray(arr, "sum");
        var n = arr.length;
        if (n == 0) return null;
        var total = arr[0];
        for (var i = 1; i < n; i++) total += arr[i];
        return total;
    };
    var sum = Array.sum;

    Array.avg = function (arr) {
        requireArray(arr, "avg");
        if (arr.length == 0) return null;
        return sum(arr) / arr.length;
    };
    var avg = Array.avg;

    Array.contains = function (arr, obj) {
        requireArray(arr, "contains");
        return arr.includes(obj);
    };

    Array.unique = function (arr) {
        requireArray(arr, "unique");
        return [...new SetCtor(arr)];
    };

    Array.shuffle = function (arr) {
        requireArray(arr, "shuffle");
        // The shell draws from `Random`, which the engine does not define.
        if (arr.length > 1) throw missing("Random");
        return arr;
    };

    Array.fetchRefs = function (arr, coll) {
        requireArray(arr, "fetchRefs");
        return arr.filter(function (z) {
            return !coll || z.getCollection() == coll;
        }).map(function (z) {
            return z.fetch();
        });
    };

    Array.stdDev = function (arr) {
        requireArray(arr, "stdDev");
        var mean = avg(arr);
        var squares = 0;
        for (var i = 0; i < arr.length; i++) {
            var d = arr[i] - mean;
            squares += d * d;
        }
        return sqrt(squares / arr.length);
    };

    // Strings and numbers.

    String.prototype.pad = function (length, right, chr) {
        return right ? this.padEnd(length, chr) : this.padStart(length, chr);
    };

    Number.prototype.toPercentStr = function () {
        return (this * 100).toFixed(2) + "%";
    };

    Number.prototype.zeroPad = function (width) {
        return ("" + this).pad(width, false, "0");
    };

    // The BSON types: what the shell writes in JavaScript after the natives of each prototype,
    // in the order in which the refrouter lists them (measured on a refrouter 9.0.4 on 02-11).

    // The shell's own global object, where `db` is never defined: `fetch` reads it there.
    var shellRealm = (function () {
        var globalThis = {};
        return function (ns, dbName) {
            return dbName ? globalThis.db.getSiblingDB(dbName) : globalThis.db[ns];
        };
    })();
    var parseInteger = parseInt;

    NumberLong.prototype.tojson = function () {
        return this.toString();
    };
    NumberInt.prototype.tojson = function () {
        return this.toString();
    };
    NumberDecimal.prototype.tojson = function () {
        return this.toString();
    };
    // `numberDecimalsEqual` is not defined on the refrouter: `equals` always fails there.
    NumberDecimal.prototype.equals = function (other) {
        return numberDecimalsEqual(this, other);
    };

    Timestamp.prototype.tojson = function () {
        return this.toStringIncomparable();
    };
    Timestamp.prototype.getTime = function () {
        return this.t;
    };
    Timestamp.prototype.getInc = function () {
        return this.i;
    };
    Timestamp.prototype.toString = function () {
        return this.toStringIncomparable();
    };
    Timestamp.prototype.toStringIncomparable = function () {
        return "Timestamp(" + this.t + ", " + this.i + ")";
    };

    ObjectId.prototype.tojson = function () {
        return this.toString();
    };
    ObjectId.prototype.valueOf = function () {
        return this.str;
    };
    ObjectId.prototype.isObjectId = true;
    ObjectId.prototype.getTimestamp = function () {
        return new DateCtor(parseInteger(this.valueOf().slice(0, 8), 16) * 1000);
    };
    ObjectId.prototype.equals = function (other) {
        return this.str === other.str;
    };

    BinData.prototype.tojson = function () {
        return this.toString();
    };
    BinData.prototype.subtype = function () {
        return this.type;
    };
    BinData.prototype.length = function () {
        return this.len;
    };

    DBRef.prototype.fetch = function () {
        assert(this.$ref, "need a ns");
        assert(this.$id, "need an id");
        return shellRealm(this.$ref, this.$db);
    };
    DBRef.prototype.tojson = function () {
        return this.toString();
    };
    DBRef.prototype.getDb = function () {
        return this.$db;
    };
    DBRef.prototype.getCollection = function () {
        return this.$ref;
    };
    DBRef.prototype.getRef = function () {
        return this.$ref;
    };
    DBRef.prototype.getId = function () {
        return this.$id;
    };
    DBRef.prototype.toString = function () {
        return "DBRef(" + tojson(this.$ref) + ", " + tojson(this.$id) + (this.$db ? ", " + tojson(this.$db) : "") + ")";
    };

    DBPointer.prototype.fetch = function () {
        assert(this.ns, "need a ns");
        assert(this.id, "need an id");
        return shellRealm(this.ns);
    };
    DBPointer.prototype.tojson = function () {
        return this.toString();
    };
    DBPointer.prototype.toJSON = function () {
        return { ns: this.ns, id: this.id };
    };
    DBPointer.prototype.getCollection = function () {
        return this.ns;
    };
    DBPointer.prototype.getId = function () {
        return this.id;
    };
    DBPointer.prototype.toString = function () {
        return "DBPointer(" + tojson(this.ns) + ", " + tojson(this.id) + ")";
    };

    // The global functions, in the order in which the refrouter lists them.
    shellGlobal("hex_md5", hex_md5);
    shellGlobal("ISODate", ISODate);
    shellGlobal("tojsononeline", tojsononeline);
    shellGlobal("tojson", tojson);
    shellGlobal("tojsonObject", tojsonObject);
    shellGlobal("toJsonForLog", toJsonForLog);
    shellGlobal("printjson", printjson);
    shellGlobal("isString", isString);
    shellGlobal("isNumber", isNumber);
    shellGlobal("isObject", isObject);
    shellGlobal("doassert", doassert, true);
    shellGlobal("_processMsg", _processMsg, true);
    shellGlobal("_buildAssertionMessage", _buildAssertionMessage, true);
    shellGlobal("formatErrorMsg", formatErrorMsg, true);
    shellGlobal("_doassert", _doassert, true);
    shellGlobal("assert", assert, true);
    shellGlobal("friendlyEqual", friendlyEqual, true);
    shellGlobal("_isEq", _isEq, true);
    shellGlobal("assertThrowsHelper", assertThrowsHelper, true);
})(__teseladbShellHost);
