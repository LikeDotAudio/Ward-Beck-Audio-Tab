// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Python json.dumps(indent=...) and float repr, as the MXL engine writes domain_def.json /
//! flow_def.json (byte-identical to the Python files).

use serde_json::Value;

/// Python repr(float).
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{:e}", f);
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let e = exp as usize + 1;
            let mut d = digits.clone();
            while d.len() < e {
                d.push('0');
            }
            let (ip, fp) = d.split_at(e);
            out.push_str(ip);
            out.push('.');
            out.push_str(if fp.is_empty() { "0" } else { fp });
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    }
    out
}

fn py_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    out.push('"');
}

fn py_json_write(out: &mut String, v: &Value, indent: Option<usize>, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                out.push_str(&n.to_string());
            } else {
                out.push_str(&py_float_repr(n.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(s) => py_json_str(out, s),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                match indent {
                    Some(w) => {
                        out.push_str(if i == 0 { "\n" } else { ",\n" });
                        out.push_str(&" ".repeat(w * (level + 1)));
                    }
                    None => {
                        if i > 0 {
                            out.push_str(", ");
                        }
                    }
                }
                py_json_write(out, item, indent, level + 1);
            }
            if let Some(w) = indent {
                out.push('\n');
                out.push_str(&" ".repeat(w * level));
            }
            out.push(']');
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in o.iter().enumerate() {
                match indent {
                    Some(w) => {
                        out.push_str(if i == 0 { "\n" } else { ",\n" });
                        out.push_str(&" ".repeat(w * (level + 1)));
                    }
                    None => {
                        if i > 0 {
                            out.push_str(", ");
                        }
                    }
                }
                py_json_str(out, k);
                out.push_str(": ");
                py_json_write(out, item, indent, level + 1);
            }
            if let Some(w) = indent {
                out.push('\n');
                out.push_str(&" ".repeat(w * level));
            }
            out.push('}');
        }
    }
}

/// Byte-exact Python `json.dumps(v)` (indent None: ", " / ": " separators) or
/// `json.dump(v, f, indent=n)`; ensure_ascii escaping, Python float repr.
pub fn py_json_dumps(v: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    py_json_write(&mut out, v, indent, 0);
    out
}

