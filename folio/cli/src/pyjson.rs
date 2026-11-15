//! JSON as folio's Python wrote it, json.dumps(..., indent=2), so a file folio writes reads the same
//! whichever wrote it: two spaces a level, and every character past ASCII escaped (\uXXXX, a
//! surrogate pair past the BMP), as Python's ensure_ascii does.

use serde::Serialize;

/// `value` as json.dumps(value, indent=2) has it: its keys in its own order (a struct's fields',
/// say), or, with `sort_keys`, sorted.
pub fn dumps(value: &impl Serialize, sort_keys: bool) -> String {
    let text = if sort_keys {
        // serde_json's maps are sorted: through a Value, a struct's fields are too.
        serde_json::to_value(value).and_then(|v| serde_json::to_string_pretty(&v))
    } else {
        serde_json::to_string_pretty(value)
    }
    .expect("folio's records are JSON");
    ascii(&text)
}

/// `text`, its characters past ASCII escaped as JSON escapes them: they're only ever in strings.
pub fn ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn as_python_writes_it() {
        let value = json!({"b": [1, 2.5, "é😀"], "a": {}, "c": []});
        assert_eq!(
            dumps(&value, true),
            "{\n  \"a\": {},\n  \"b\": [\n    1,\n    2.5,\n    \"\\u00e9\\ud83d\\ude00\"\n  ],\n  \"c\": []\n}"
        );
    }

    #[test]
    fn a_structs_fields_in_its_order_unless_sorted() {
        #[derive(Serialize)]
        struct S {
            z: u8,
            a: f64,
        }
        assert_eq!(dumps(&S { z: 1, a: 0.0 }, false), "{\n  \"z\": 1,\n  \"a\": 0.0\n}");
        assert_eq!(dumps(&S { z: 1, a: 0.0 }, true), "{\n  \"a\": 0.0,\n  \"z\": 1\n}");
    }
}
