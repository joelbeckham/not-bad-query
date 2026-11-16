//! Render Arrow cells as display text, matching how the database itself
//! would print them (Postgres conventions for floats/booleans/bytea).

use std::fmt::Write;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int16Type, Int32Type, Int64Type};
use arrow_array::{Array, ArrayRef};
use arrow_schema::DataType;

/// Append the text of `array[row]` to `out`. Returns `false` for NULL
/// (nothing is appended).
pub fn cell_text(array: &ArrayRef, row: usize, out: &mut String) -> bool {
    if array.is_null(row) {
        return false;
    }
    match array.data_type() {
        DataType::Boolean => out.push_str(if array.as_boolean().value(row) { "true" } else { "false" }),
        DataType::Int16 => {
            let _ = write!(out, "{}", array.as_primitive::<Int16Type>().value(row));
        }
        DataType::Int32 => {
            let _ = write!(out, "{}", array.as_primitive::<Int32Type>().value(row));
        }
        DataType::Int64 => {
            let _ = write!(out, "{}", array.as_primitive::<Int64Type>().value(row));
        }
        DataType::Float32 => push_f32(array.as_primitive::<Float32Type>().value(row), out),
        DataType::Float64 => push_f64(array.as_primitive::<Float64Type>().value(row), out),
        DataType::Utf8 => out.push_str(array.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => out.push_str(array.as_string::<i64>().value(row)),
        DataType::Binary => push_hex(array.as_binary::<i32>().value(row), out),
        other => {
            if let Ok(formatter) = arrow_cast::display::ArrayFormatter::try_new(array.as_ref(), &Default::default()) {
                let _ = write!(out, "{}", formatter.value(row));
            } else {
                let _ = write!(out, "<{other}>");
            }
        }
    }
    true
}

pub fn push_hex(bytes: &[u8], out: &mut String) {
    out.push_str("\\x");
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
}

pub fn push_f64(v: f64, out: &mut String) {
    push_float(v, format!("{v:e}"), 15, out)
}

pub fn push_f32(v: f32, out: &mut String) {
    push_float(v as f64, format!("{v:e}"), 6, out)
}

/// Shortest round-trip digits, switching to exponent notation outside
/// `1e-4 <= |v| < 10^max_exp` like Postgres' `float8out`/`float4out`.
fn push_float(v: f64, sci: String, max_exp: i32, out: &mut String) {
    if v.is_nan() {
        out.push_str("NaN");
        return;
    }
    if v.is_infinite() {
        out.push_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    // `sci` is Rust's shortest representation, e.g. "2.5e100", "-1e-5".
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if v != 0.0 && (exp < -4 || exp >= max_exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        let _ = write!(out, "{mant}e{sign}{:02}", exp.abs());
        return;
    }
    // Positional: place the decimal point in the shortest digits.
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    if neg {
        out.push('-');
    }
    let point = exp + 1; // digits before the decimal point
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(&digits);
    } else if point as usize >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(point as usize - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f64s(v: f64) -> String {
        let mut s = String::new();
        push_f64(v, &mut s);
        s
    }
    fn f32s(v: f32) -> String {
        let mut s = String::new();
        push_f32(v, &mut s);
        s
    }

    #[test]
    fn floats_match_postgres() {
        assert_eq!(f64s(0.0), "0");
        assert_eq!(f64s(-0.0), "-0");
        assert_eq!(f64s(1.5), "1.5");
        assert_eq!(f64s(-2.25), "-2.25");
        assert_eq!(f64s(100.0), "100");
        assert_eq!(f64s(0.001), "0.001");
        assert_eq!(f64s(0.0001), "0.0001");
        assert_eq!(f64s(0.00001), "1e-05");
        assert_eq!(f64s(2.5e100), "2.5e+100");
        assert_eq!(f64s(1e15), "1e+15");
        assert_eq!(f64s(123456789012345.0), "123456789012345");
        assert_eq!(f64s(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(f64s(f64::NAN), "NaN");
        assert_eq!(f32s(0.1), "0.1");
        assert_eq!(f32s(1e10), "1e+10");
        assert_eq!(f32s(123456.0), "123456");
        assert_eq!(f32s(1234567.0), "1.234567e+06");
    }
}
