//! Payload translation between the wire and the file link.
//!
//! On the wire a payload is JSON (`docs/PROTOCOL.md`). In the file link it is one line of Lua literal syntax,
//! because that is what the mod writes (`C.serialize`) and reads (`C.parse`, a `load` in an empty environment).
//! The launcher translates in both directions so neither side learns the other's format:
//!
//! - [`json_to_lua`]: objects become `{["key"]=value,...}` with sorted keys, arrays `{[1]=a,[2]=b}`, integers are
//!   written as integers, other numbers in Rust's shortest round-trip form, strings with `\n \r \t \" \\` and
//!   `\ddd` for the other control characters (the mod's own escapes), `null` fields are left out.
//! - [`lua_to_json`]: a small recursive-descent parser for the subset the mod's serialiser produces (tables with
//!   `[key]=value` or `name=value` or positional fields, strings, numbers, booleans, nil). A table whose keys are
//!   exactly `1..n` is an array, any other table an object whose keys are the keys' text. Anything else, `(0/0)`
//!   included, is refused: `None`. Depth is limited to [`MAX_DEPTH`], size to [`MAX_INPUT`] and [`MAX_NODES`].
//! - [`is_literal`]: the same grammar as a yes or no, for text that is passed on to the mod inside a string (an
//!   action's body) and that the mod will `load`: a `load` runs whatever the text says, function calls and endless
//!   loops included, the empty environment only takes away the globals. Only text that is a literal gets through.
//!
//! Neither direction runs any code: the parser is not Lua, only its literal syntax. The text comes from a file the
//! game writes and from other players' games, so nothing here may panic or grow without bound on any input.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

/// Deepest nesting either direction accepts.
pub const MAX_DEPTH: usize = 48;

/// Longest text either direction looks at, in bytes. The wire carries at most 1 MiB of JSON per message and JSON
/// grows by at most a factor of four as Lua text (`\ddd` for a control character), so nothing real is longer.
pub const MAX_INPUT: usize = 4 << 20;

/// Most values (numbers, strings, booleans, nil and tables, table keys included) one text may hold. A real
/// payload has a few hundred. A parsed value takes about a hundred bytes per node, so this bounds the memory one
/// line can ask for to tens of megabytes however it is written.
pub const MAX_NODES: usize = 250_000;

/// JSON bytes to one line of Lua literal syntax. `None` when the bytes are not JSON or nest too deep.
pub fn json_to_lua(json: &[u8]) -> Option<String> {
    if json.len() > MAX_INPUT {
        return None;
    }
    let value: Value = serde_json::from_slice(json).ok()?;
    let mut out = String::with_capacity(json.len() + 16);
    write_lua(&value, 0, &mut out)?;
    Some(out)
}

fn write_lua(value: &Value, depth: usize, out: &mut String) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value {
        Value::Null => out.push_str("nil"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('{');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!("[{}]=", i + 1));
                write_lua(item, depth + 1, out)?;
            }
            out.push('}');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().filter(|(_, v)| !v.is_null()).collect();
            out.push('{');
            for (i, (key, item)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                write_string(key, out);
                out.push_str("]=");
                write_lua(item, depth + 1, out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// One line of Lua literal syntax to JSON bytes. `None` when the text is not in the accepted subset.
pub fn lua_to_json(text: &str) -> Option<Vec<u8>> {
    let value = parse(text, false)?;
    serde_json::to_vec(&value).ok()
}

/// Whether `text` is a Lua literal in the sense of [`lua_to_json`], with one addition: the three spellings the
/// mod's serialiser gives NaN and the infinities (`(0/0)`, `(1/0)`, `(-1/0)`), which JSON cannot hold but a `load`
/// of the text evaluates harmlessly. Nothing else that is not a plain literal passes: no call, no operator, no
/// function, no name.
pub fn is_literal(text: &str) -> bool {
    parse(text, true).is_some()
}

fn parse(text: &str, lenient: bool) -> Option<Value> {
    if text.len() > MAX_INPUT {
        return None;
    }
    let mut p = Parser { s: text.as_bytes(), i: 0, nodes: 0, lenient };
    let value = p.value(0)?;
    p.skip_ws();
    (p.i == p.s.len()).then_some(value)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// Values read so far, against [`MAX_NODES`].
    nodes: usize,
    /// Accept `(0/0)`, `(1/0)` and `(-1/0)` (as nil): see [`is_literal`].
    lenient: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_ws();
        if self.peek() == Some(b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self) -> &str {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
            self.i += 1;
        }
        self.s.get(start..self.i).and_then(|w| std::str::from_utf8(w).ok()).unwrap_or("")
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'{' => self.table(depth),
            b'"' | b'\'' => self.string().map(Value::String),
            b'-' | b'0'..=b'9' => self.number(),
            b'(' if self.lenient => self.not_a_number(),
            _ => match self.word() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                "nil" => Some(Value::Null),
                _ => None,
            },
        }
    }

    /// `(0/0)`, `(1/0)` or `(-1/0)`, as the mod writes NaN and the infinities.
    fn not_a_number(&mut self) -> Option<Value> {
        let rest = self.s.get(self.i..)?;
        let spelling = [&b"(0/0)"[..], b"(1/0)", b"(-1/0)"].into_iter().find(|s| rest.starts_with(s))?;
        self.i += spelling.len();
        Some(Value::Null)
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while self.peek().is_some_and(|b| matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) {
            self.i += 1;
        }
        let token = std::str::from_utf8(self.s.get(start..self.i)?).ok()?;
        if let Ok(n) = token.parse::<i64>() {
            return Some(Value::Number(n.into()));
        }
        let f: f64 = token.parse().ok()?;
        Number::from_f64(f).map(Value::Number)
    }

    fn string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.i += 1;
        let mut bytes = Vec::new();
        loop {
            let b = self.peek()?;
            self.i += 1;
            if b == quote {
                break;
            }
            if b != b'\\' {
                bytes.push(b);
                continue;
            }
            let e = self.peek()?;
            self.i += 1;
            match e {
                b'n' => bytes.push(b'\n'),
                b'r' => bytes.push(b'\r'),
                b't' => bytes.push(b'\t'),
                b'0'..=b'9' => {
                    let mut n = u32::from(e - b'0');
                    for _ in 0..2 {
                        match self.peek() {
                            Some(d @ b'0'..=b'9') => {
                                n = n * 10 + u32::from(d - b'0');
                                self.i += 1;
                            }
                            _ => break,
                        }
                    }
                    bytes.push(u8::try_from(n).ok()?);
                }
                other => bytes.push(other),
            }
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn table(&mut self, depth: usize) -> Option<Value> {
        self.i += 1; // {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        let mut positional = 0i64;
        loop {
            self.skip_ws();
            if self.eat(b'}') {
                break;
            }
            let (key, val) = if self.peek() == Some(b'[') {
                self.i += 1;
                let k = self.value(depth + 1)?;
                if !self.eat(b']') || !self.eat(b'=') {
                    return None;
                }
                (k, self.value(depth + 1)?)
            } else {
                let save = self.i;
                let name = self.word().to_string();
                if name.bytes().next().is_some_and(|b| !b.is_ascii_digit()) && self.eat(b'=') {
                    (Value::String(name), self.value(depth + 1)?)
                } else {
                    self.i = save;
                    positional += 1;
                    (Value::Number(positional.into()), self.value(depth + 1)?)
                }
            };
            if !val.is_null() {
                entries.push((key, val));
            }
            self.skip_ws();
            if !(self.eat(b',') || self.eat(b';')) {
                self.skip_ws();
                if self.eat(b'}') {
                    break;
                }
                return None;
            }
        }
        Some(to_json_table(entries))
    }
}

/// The slot (0-based) a table key names in an array of `n` entries: a whole number from 1 to `n`.
fn array_slot(key: &Value, n: usize) -> Option<usize> {
    let i = usize::try_from(key.as_u64()?).ok()?;
    (1..=n).contains(&i).then(|| i - 1)
}

fn to_json_table(entries: Vec<(Value, Value)>) -> Value {
    let n = entries.len();
    // an array when the keys are exactly 1..n, each once
    let slots: Option<Vec<usize>> = entries.iter().map(|(k, _)| array_slot(k, n)).collect();
    if let Some(slots) =
        slots.filter(|s| n > 0 && s.iter().copied().collect::<std::collections::BTreeSet<_>>().len() == n)
    {
        let mut items: Vec<Value> = vec![Value::Null; n];
        for (slot, (_, v)) in slots.into_iter().zip(entries) {
            if let Some(item) = items.get_mut(slot) {
                *item = v;
            }
        }
        return Value::Array(items);
    }
    let mut map = Map::new();
    for (k, v) in entries {
        let key = match k {
            Value::String(s) => s,
            other => other.to_string(),
        };
        map.insert(key, v);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua(json: &str) -> String {
        json_to_lua(json.as_bytes()).unwrap()
    }

    fn json(lua_text: &str) -> Value {
        serde_json::from_slice(&lua_to_json(lua_text).unwrap()).unwrap()
    }

    #[test]
    fn objects_sort_keys_and_drop_nulls() {
        assert_eq!(
            lua(r#"{"started":true,"speed":0,"lead":1200,"pauseAt":null}"#),
            r#"{["lead"]=1200,["speed"]=0,["started"]=true}"#
        );
        assert_eq!(lua(r#"{"values":{"b":1.5,"a":-2}}"#), r#"{["values"]={["a"]=-2,["b"]=1.5}}"#);
    }

    #[test]
    fn arrays_are_one_based_and_come_back() {
        let text = lua(r#"{"players":[{"id":0,"name":"A"},{"id":1,"name":"B"}]}"#);
        assert_eq!(text, r#"{["players"]={[1]={["id"]=0,["name"]="A"},[2]={["id"]=1,["name"]="B"}}}"#);
        assert_eq!(
            json(&text),
            serde_json::from_str::<Value>(r#"{"players":[{"id":0,"name":"A"},{"id":1,"name":"B"}]}"#).unwrap()
        );
    }

    #[test]
    fn strings_use_the_mods_escapes_and_round_trip() {
        let original = "a\tb\nc\"d\\e\u{1}f é";
        let value = serde_json::json!({ "body": original });
        let text = json_to_lua(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(text, "{[\"body\"]=\"a\\tb\\nc\\\"d\\\\e\\001f é\"}");
        assert!(!text.contains(['\n', '\t', '\r']), "stays on one line");
        assert_eq!(json(&text), value);
    }

    #[test]
    fn parses_what_the_mods_serialiser_writes() {
        let v = json(r#"{[1]=5,[2]="x"}"#);
        assert_eq!(v, serde_json::json!([5, "x"]));
        let v = json(r#"{["body"]="{[\"fn\"]=\"x\"}",["from"]=1,["seq"]=2,["stamp"]=400,["t"]=1.5e+3}"#);
        assert_eq!(v["body"], "{[\"fn\"]=\"x\"}");
        assert_eq!(v["stamp"], 400);
        assert_eq!(v["t"], 1500.0);
        assert_eq!(json("{}"), serde_json::json!({}));
        assert_eq!(json(r#"{a=1, b={true,false}; c="z"}"#), serde_json::json!({"a":1,"b":[true,false],"c":"z"}));
        assert_eq!(json(r#"{["x"]=nil,["y"]=1}"#), serde_json::json!({"y":1}));
        assert_eq!(json(r#"{[10]="a"}"#), serde_json::json!({"10":"a"}));
    }

    #[test]
    fn refuses_what_is_not_literal_syntax() {
        for bad in [
            "",
            "{",
            "{[1]=}",
            "os.exit()",
            "{[\"a\"]=(0/0)}",
            "{} extra",
            "\"open",
            "{[\"a\"]=1 [\"b\"]=2}",
            "function() end",
        ] {
            assert!(lua_to_json(bad).is_none(), "{bad:?}");
        }
        assert!(json_to_lua(b"not json").is_none());
        let deep = format!("{}{}", "{[1]=".repeat(60), "}".repeat(60));
        assert!(lua_to_json(&deep).is_none());
    }

    #[test]
    fn a_literal_is_what_the_mods_serialiser_writes_and_nothing_that_runs() {
        // the shapes C.serialize produces, NaN and infinities included
        for good in [
            "{}",
            r#"{[1]=5,[2]="x",[3]=true,[4]=false}"#,
            r#"{["a"]={["b"]={}},["n"]=-1.5e-07,["z"]=(0/0),["i"]=(1/0),["j"]=(-1/0)}"#,
            r#"{[true]=1,[false]=2,["s"]="\"quoted\" \\ \010 \000 é"}"#,
            "  nil ",
            "42",
        ] {
            assert!(is_literal(good), "{good:?}");
        }
        // the point: a load of any of these would run code, an endless loop or an allocation bomb
        for bad in [
            "(function() while true do end end)()",
            r#"{["a"]=("x"):rep(1e9)}"#,
            r#"("x"):rep(1e9)"#,
            "{} or (function() end)()",
            "{[1]=print('hi')}",
            "1 + 1",
            "{[1]=2} --[[x]]",
            "{1,2,3} end function f() end",
            "(0/0)x",
            "((0/0))",
            "(1/0",
            "(2/0)",
            "setmetatable({}, {__index=function() end})",
            "[[long string]]",
            "{a=1}.a",
        ] {
            assert!(!is_literal(bad), "{bad:?}");
        }
        // and the strict reader still refuses the three spellings
        assert!(lua_to_json("(0/0)").is_none() && lua_to_json("(1/0)").is_none() && lua_to_json("(-1/0)").is_none());
    }

    #[test]
    fn size_depth_and_value_count_are_bounded() {
        // too long to look at
        let long = format!("\"{}\"", "a".repeat(MAX_INPUT));
        assert!(lua_to_json(&long).is_none() && !is_literal(&long));
        assert!(json_to_lua(long.as_bytes()).is_none());
        // too many values for a line of reasonable length: two bytes each
        let many = format!("{{{}}}", "1,".repeat(MAX_NODES));
        assert!(lua_to_json(&many).is_none() && !is_literal(&many));
        let fits = format!("{{{}}}", "1,".repeat(MAX_NODES / 2));
        assert!(lua_to_json(&fits).is_some());
        // too deep: the depth limit holds in both directions and in the validator
        let deep = format!("{}1{}", "{[1]=".repeat(MAX_DEPTH + 2), "}".repeat(MAX_DEPTH + 2));
        assert!(lua_to_json(&deep).is_none() && !is_literal(&deep));
        let json_deep = format!("{}1{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert!(json_to_lua(json_deep.as_bytes()).is_none());
        let ok = format!("{}1{}", "[".repeat(MAX_DEPTH - 1), "]".repeat(MAX_DEPTH - 1));
        assert!(json_to_lua(ok.as_bytes()).is_some());
    }

    #[test]
    fn numbers_keep_their_value_both_ways() {
        for text in [
            "0",
            "-1",
            "9007199254740993",
            "-9223372036854775808",
            "1.5",
            "-0.25",
            "1e+21",
            "1.7976931348623157e308",
            "5e-324",
        ] {
            let v = json(&format!("{{[1]={text}}}"));
            let back = lua(&v.to_string());
            assert_eq!(json(&back), v, "{text}");
        }
        assert!(lua_to_json("{[1]=1e999}").is_none(), "an infinity is not JSON");
        assert!(
            lua_to_json("{[1]=--1}").is_none() && lua_to_json("{[1]=1e}").is_none() && lua_to_json("{[1]=-}").is_none()
        );
        // a double the default parser rounds wrongly arrives as it was printed
        let tricky = 1.1847772900119483e215_f64;
        let wire = serde_json::to_vec(&serde_json::json!({ "v": tricky })).unwrap();
        let through = json(&json_to_lua(&wire).unwrap());
        assert_eq!(through["v"].as_f64(), Some(tricky));
    }
}
