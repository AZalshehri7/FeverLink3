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
//!   included, is refused: `None`. Depth is limited to [`MAX_DEPTH`].
//!
//! Neither direction runs any code: the parser is not Lua, only its literal syntax.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

/// Deepest nesting either direction accepts.
pub const MAX_DEPTH: usize = 48;

/// JSON bytes to one line of Lua literal syntax. `None` when the bytes are not JSON or nest too deep.
pub fn json_to_lua(json: &[u8]) -> Option<String> {
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
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let value = p.value(0)?;
    p.skip_ws();
    if p.i != p.s.len() {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
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
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'{' => self.table(depth),
            b'"' | b'\'' => self.string().map(Value::String),
            b'-' | b'0'..=b'9' => self.number(),
            _ => match self.word() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                "nil" => Some(Value::Null),
                _ => None,
            },
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
            self.i += 1;
        }
        let token = std::str::from_utf8(&self.s[start..self.i]).ok()?;
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
                if !name.is_empty() && !name.as_bytes()[0].is_ascii_digit() && self.eat(b'=') {
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

fn to_json_table(entries: Vec<(Value, Value)>) -> Value {
    let n = entries.len();
    let mut slots: Vec<Option<Value>> = vec![None; n];
    let mut is_array = n > 0;
    for (k, v) in &entries {
        match k.as_u64() {
            Some(i) if i >= 1 && (i as usize) <= n && slots[i as usize - 1].is_none() => {
                slots[i as usize - 1] = Some(v.clone());
            }
            _ => {
                is_array = false;
                break;
            }
        }
    }
    if is_array {
        return Value::Array(slots.into_iter().flatten().collect());
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
}
