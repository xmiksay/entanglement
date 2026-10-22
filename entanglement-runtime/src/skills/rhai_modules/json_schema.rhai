/** Built-in `json_schema` module (ADR-0248): validate a value against a
    JSON Schema subset. */

/// Validate `value` (e.g. from `parse_json`) against `schema`, a map using
/// the subset: type (object/array/string/number/integer/boolean/null, or an
/// array of them), required, properties, additionalProperties (bool), items,
/// enum, const, minimum/maximum, minLength/maxLength, minItems/maxItems.
/// `true`/`false` work as schemas. No pattern, $ref or format; other keys
/// are ignored, and a supported keyword of the wrong shape is reported.
/// Returns an array of "<path>: <problem>" strings, the path JSON-pointer
/// style ("/" is the root, "/items/0/name" a nested one); an empty array
/// means valid.
fn validate(value, schema) {
    check(value, schema, "")
}

private fn check(v, s, path) {
    if type_of(s) == "bool" {
        if s { return []; }
        return [problem(path, "no value is allowed here")];
    }
    if type_of(s) != "map" {
        return [problem(path, "schema must be a map or a bool, got " + kind(s))];
    }
    let bad = malformed(s);
    if bad != "" { return [problem(path, bad)]; }
    if "type" in s {
        let types = s["type"];
        if type_of(types) != "array" { types = [types]; }
        let matched = false;
        for t in types {
            if is_type(v, t) { matched = true; }
        }
        if !matched {
            let msg = `expected type ${join(types)}, got ${kind(v)}`;
            return [problem(path, msg)];
        }
    }
    let errs = [];
    if "enum" in s {
        let found = false;
        for option in s["enum"] {
            if same(v, option) { found = true; }
        }
        if !found { errs.push(problem(path, "value is not one of the enum values")); }
    }
    if "const" in s && !same(v, s["const"]) {
        errs.push(problem(path, "value does not equal the const value"));
    }
    let k = kind(v);
    if k == "integer" || k == "number" {
        if "minimum" in s && v < s.minimum {
            errs.push(problem(path, `${v} is less than the minimum ${s.minimum}`));
        }
        if "maximum" in s && v > s.maximum {
            errs.push(problem(path, `${v} is greater than the maximum ${s.maximum}`));
        }
    }
    if k == "string" {
        let n = v.len();
        if "minLength" in s && n < s.minLength {
            errs.push(problem(path, `length ${n} is shorter than minLength ${s.minLength}`));
        }
        if "maxLength" in s && n > s.maxLength {
            errs.push(problem(path, `length ${n} is longer than maxLength ${s.maxLength}`));
        }
    }
    if k == "array" {
        let n = v.len();
        if "minItems" in s && n < s.minItems {
            errs.push(problem(path, `${n} items, fewer than minItems ${s.minItems}`));
        }
        if "maxItems" in s && n > s.maxItems {
            errs.push(problem(path, `${n} items, more than maxItems ${s.maxItems}`));
        }
        if "items" in s {
            for (item, i) in v {
                errs += check(item, s.items, path + "/" + i);
            }
        }
    }
    if k == "object" {
        if "required" in s {
            for key in s.required {
                if !(key in v) { errs.push(problem(path, "missing required property `" + key + "`")); }
            }
        }
        let props = if "properties" in s { s.properties } else { #{} };
        for key in props.keys() {
            if key in v { errs += check(v[key], props[key], path + "/" + escape(key)); }
        }
        if "additionalProperties" in s && s.additionalProperties == false {
            for key in v.keys() {
                if !(key in props) {
                    errs.push(problem(path + "/" + escape(key), "additional property is not allowed"));
                }
            }
        }
    }
    errs
}

/// Why a keyword of `s` has the wrong shape, or "" — a malformed schema is
/// reported, never silently ignored.
private fn malformed(s) {
    let shapes = #{
        "minimum": "number", "maximum": "number", "minLength": "number",
        "maxLength": "number", "minItems": "number", "maxItems": "number",
        "required": "array", "enum": "array", "properties": "object",
        "additionalProperties": "boolean"
    };
    for kw in shapes.keys() {
        if kw in s {
            let want = shapes[kw];
            let got = kind(s[kw]);
            let ok = got == want || (want == "number" && got == "integer");
            let article = if want == "array" || want == "object" { "an " } else { "a " };
            if !ok { return "schema keyword `" + kw + "` must be " + article + want; }
        }
    }
    if "items" in s {
        let k = kind(s.items);
        if k != "object" && k != "boolean" {
            return "schema keyword `items` must be an object or a boolean";
        }
    }
    if "required" in s {
        for key in s.required {
            if type_of(key) != "string" {
                return "schema keyword `required` must list property names (strings)";
            }
        }
    }
    if "type" in s {
        let types = s["type"];
        if type_of(types) != "array" { types = [types]; }
        if types.len() == 0 {
            return "schema keyword `type` must be a type name or an array of them";
        }
        let known = ["object", "array", "string", "number", "integer", "boolean", "null"];
        for t in types {
            if type_of(t) != "string" || !(t in known) {
                return "schema keyword `type` must be a type name or an array of them";
            }
        }
    }
    ""
}

/// The JSON type name of a Rhai value.
private fn kind(v) {
    switch type_of(v) {
        "map" => "object",
        "array" => "array",
        "string" => "string",
        "i64" => "integer",
        "f64" => "number",
        "bool" => "boolean",
        "()" => "null",
        _ => type_of(v)
    }
}

private fn is_type(v, t) {
    let k = kind(v);
    if t == "number" { return k == "integer" || k == "number"; }
    if t == "integer" { return k == "integer" || (k == "number" && v.fraction() == 0.0); }
    k == t
}

/// Deep JSON equality: integers equal floats of the same value.
private fn same(a, b) {
    let ka = kind(a);
    let kb = kind(b);
    let numeric = ["integer", "number"];
    if ka in numeric && kb in numeric { return a == b; }
    if ka != kb { return false; }
    if ka == "array" {
        if a.len() != b.len() { return false; }
        for (x, i) in a {
            if !same(x, b[i]) { return false; }
        }
        return true;
    }
    if ka == "object" {
        if a.len() != b.len() { return false; }
        for key in a.keys() {
            if !(key in b) || !same(a[key], b[key]) { return false; }
        }
        return true;
    }
    if ka == "null" { return true; }
    a == b
}

private fn problem(path, msg) {
    let at = if path == "" { "/" } else { path };
    at + ": " + msg
}

/// A JSON-pointer segment: `~` and `/` escaped.
private fn escape(key) {
    let k = key;
    k.replace("~", "~0");
    k.replace("/", "~1");
    k
}

private fn join(items) {
    let out = "";
    for (t, i) in items {
        if i > 0 { out += " or "; }
        out += `${t}`;
    }
    out
}
