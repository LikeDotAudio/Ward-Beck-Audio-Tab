// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Python-compatible helpers (json.dumps, str(), truthiness, round, int(), float()) shared by
//! every application: the HTTP API answers byte-for-byte like the Python daemon did.
//!
//!   (root)  json.dumps / str / repr / truthiness / round / int / float
//!   json    json.dumps(indent=...) and float repr, as the MXL engine writes its JSON files
//!   text    str.strip / str.splitlines / str.isspace
//!   sock    errno / exception names, if_nameindex, inet_ntop (as the AoIP listener reported them)

pub mod json;
pub mod sock;
pub mod text;
use serde_json::Value;
use std::fmt::Write as _;

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `a or b` on optional JSON values.
pub fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(v) if truthy(v) => Some(v),
        _ => b,
    }
}

/// Shortest round-trip digits and decimal point position: value = 0.DIGITS x 10^decpt.
fn digits_decpt(x: f64) -> (String, i32) {
    let s = format!("{:e}", x.abs()); // e.g. "1.7953980435718203e9"
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0').to_string();
    let digits = if digits.is_empty() { "0".to_string() } else { digits };
    (digits, exp + 1)
}

/// Python repr(float) (also what json.dumps writes for finite floats).
pub fn repr_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (d, decpt) = digits_decpt(x);
    let n = d.len() as i32;
    if decpt > -4 && decpt <= 16 {
        if decpt <= 0 {
            format!("{sign}0.{}{}", "0".repeat((-decpt) as usize), d)
        } else if decpt >= n {
            format!("{sign}{}{}.0", d, "0".repeat((decpt - n) as usize))
        } else {
            format!("{sign}{}.{}", &d[..decpt as usize], &d[decpt as usize..])
        }
    } else {
        let e = decpt - 1;
        let mant = if n > 1 { format!("{}.{}", &d[..1], &d[1..]) } else { d.clone() };
        let es = if e < 0 { '-' } else { '+' };
        format!("{sign}{mant}e{es}{:02}", e.abs())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", u);
                }
            }
        }
    }
    out.push('"');
}

#[cfg_attr(not(test), allow(dead_code))]
fn dump_into(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_nan() {
                    out.push_str("NaN");
                } else if f.is_infinite() {
                    out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
                } else {
                    out.push_str(&repr_float(f));
                }
            }
        }
        Value::String(s) => json_str(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_into(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                json_str(out, k);
                out.push_str(": ");
                dump_into(out, x);
            }
            out.push('}');
        }
    }
}

/// Python json.dumps(v) with default settings (", " / ": " separators, ensure_ascii).
/// The crate-wide implementation lives in crate::mxl::py_json_dumps.
pub fn dumps(v: &Value) -> String {
    crate::json::py_json_dumps(v, None)
}

/// Independent reference serializer, to cross-check json::py_json_dumps in tests.
#[doc(hidden)]
pub fn dumps_local(v: &Value) -> String {
    let mut out = String::new();
    dump_into(&mut out, v);
    out
}

fn repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Python repr() of a JSON-shaped value (dict/list/str/...).
pub fn repr(v: &Value) -> String {
    match v {
        Value::String(s) => repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, x)| format!("{}: {}", repr_str(k), repr(x))).collect::<Vec<_>>().join(", ")
        ),
        _ => str_of(v),
    }
}

/// Python str() of a JSON-shaped value (what an f-string interpolates).
pub fn str_of(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                repr_float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => s.clone(),
        _ => repr(v),
    }
}

/// Python str of an optional dict lookup with a default.
pub fn str_or(v: Option<&Value>, default: &str) -> String {
    v.map(str_of).unwrap_or_else(|| default.to_string())
}

/// Python round(x, n) for floats (correctly rounded, ties to even on the exact value).
pub fn round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// Python int(str): surrounding whitespace, optional sign, digits with single underscores.
pub fn parse_int(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let bad = || format!("invalid literal for int() with base 10: {}", repr_str(s));
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return Err(bad());
    }
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let v: i64 = digits.parse().map_err(|_| bad())?;
    Ok(if neg { -v } else { v })
}

/// Python int(v) of a JSON value.
pub fn int_of(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(u) = n.as_u64() {
                Ok(u as i64)
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_finite() { Ok(f.trunc() as i64) } else { Err("cannot convert float to integer".into()) }
            }
        }
        Value::String(s) => parse_int(s),
        _ => Err(format!("int() argument must be a string or a number, not {}", type_name(v))),
    }
}

/// Python float(v) of a JSON value.
pub fn float_of(v: &Value) -> Result<f64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => {
            let t: String = s.trim().chars().filter(|c| *c != '_').collect();
            let low = t.to_ascii_lowercase();
            let low = low.trim_start_matches(['+', '-']);
            if matches!(low, "inf" | "infinity" | "nan") || t.parse::<f64>().is_ok() {
                t.parse::<f64>().map_err(|_| String::new()).or_else(|_| {
                    let neg = t.starts_with('-');
                    Ok(match low {
                        "nan" => f64::NAN,
                        _ if neg => f64::NEG_INFINITY,
                        _ => f64::INFINITY,
                    })
                })
            } else {
                Err(format!("could not convert string to float: {}", repr_str(s)))
            }
        }
        _ => Err(format!("float() argument must be a string or a real number, not '{}'", type_name(v))),
    }
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => if n.is_f64() { "float" } else { "int" },
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// First `n` characters (Python s[:n]).
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python str.isdigit() for the ASCII cases pactl/ps print.
pub fn isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}
