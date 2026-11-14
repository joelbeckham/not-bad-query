//! Postgres binary wire format → display text.
//!
//! Every function here is total: malformed input falls back to a hex dump
//! rather than panicking.

use std::fmt::Write;
use std::net::{Ipv4Addr, Ipv6Addr};

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use tokio_postgres::types::{Kind, Type};

use crate::ColumnKind;

/// How a column is stored in Arrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Bool,
    I16,
    I32,
    I64,
    F32,
    F64,
    Bytes,
    /// Rendered to text with [`format_value`].
    Text(Type),
}

pub fn plan(ty: &Type) -> Plan {
    match base(ty) {
        t if *t == Type::BOOL => Plan::Bool,
        t if *t == Type::INT2 => Plan::I16,
        t if *t == Type::INT4 => Plan::I32,
        t if *t == Type::INT8 => Plan::I64,
        t if *t == Type::FLOAT4 => Plan::F32,
        t if *t == Type::FLOAT8 => Plan::F64,
        t if *t == Type::BYTEA => Plan::Bytes,
        t => Plan::Text(t.clone()),
    }
}

/// Domains behave like their base type.
fn base(ty: &Type) -> &Type {
    match ty.kind() {
        Kind::Domain(b) => base(b),
        _ => ty,
    }
}

pub fn kind_of(ty: &Type) -> ColumnKind {
    let t = base(ty);
    if let Kind::Array(_) = t.kind() {
        return ColumnKind::Other;
    }
    match *t {
        Type::BOOL => ColumnKind::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC | Type::OID
        | Type::MONEY | Type::XID | Type::XID8 | Type::CID => ColumnKind::Number,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::CHAR | Type::UNKNOWN => ColumnKind::Text,
        Type::DATE | Type::TIME | Type::TIMETZ | Type::TIMESTAMP | Type::TIMESTAMPTZ | Type::INTERVAL => {
            ColumnKind::Temporal
        }
        Type::JSON | Type::JSONB => ColumnKind::Json,
        Type::UUID => ColumnKind::Uuid,
        Type::BYTEA => ColumnKind::Binary,
        _ => match t.kind() {
            Kind::Enum(_) => ColumnKind::Text,
            _ => ColumnKind::Other,
        },
    }
}

/// Append the display text of a non-null value of type `ty` to `out`.
pub fn format_value(ty: &Type, raw: &[u8], out: &mut String) {
    let mark = out.len();
    if fmt(ty, raw, out).is_none() {
        out.truncate(mark);
        hex(raw, out);
    }
}

fn hex(raw: &[u8], out: &mut String) {
    out.push_str("\\x");
    for b in raw {
        let _ = write!(out, "{b:02x}");
    }
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn i16(&mut self) -> Option<i16> {
        self.take(2).map(|b| i16::from_be_bytes([b[0], b[1]]))
    }
    fn u16(&mut self) -> Option<u16> {
        self.i16().map(|v| v as u16)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|b| i32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        self.i32().map(|v| v as u32)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|b| i64::from_be_bytes(b.try_into().unwrap()))
    }
    /// A length-prefixed value; `Ok(None)` is SQL NULL.
    fn value(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 { Some(None) } else { self.take(len as usize).map(Some) }
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

fn exact<const N: usize>(raw: &[u8]) -> Option<[u8; N]> {
    raw.try_into().ok()
}

fn utf8(raw: &[u8], out: &mut String) -> Option<()> {
    out.push_str(std::str::from_utf8(raw).ok()?);
    Some(())
}

fn fmt(ty: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    match ty.kind() {
        Kind::Domain(b) => return fmt(b, raw, out),
        Kind::Enum(_) => return utf8(raw, out),
        Kind::Array(elem) => return array(elem, raw, out),
        Kind::Range(elem) => return range(elem, raw, out),
        Kind::Composite(fields) => {
            let tys: Vec<Type> = fields.iter().map(|f| f.type_().clone()).collect();
            return record(Some(&tys), raw, out);
        }
        _ => {}
    }
    match *ty {
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML | Type::JSON
        | Type::REFCURSOR => utf8(raw, out),
        Type::JSONB => {
            let (&ver, rest) = raw.split_first()?;
            if ver != 1 {
                return None;
            }
            utf8(rest, out)
        }
        Type::CHAR => {
            out.push(*raw.first()? as char);
            Some(())
        }
        Type::BOOL => {
            out.push_str(if *raw.first()? != 0 { "true" } else { "false" });
            Some(())
        }
        Type::INT2 => w(out, i16::from_be_bytes(exact(raw)?)),
        Type::INT4 => w(out, i32::from_be_bytes(exact(raw)?)),
        Type::INT8 => w(out, i64::from_be_bytes(exact(raw)?)),
        Type::OID | Type::XID | Type::CID | Type::REGPROC | Type::REGCLASS | Type::REGTYPE => {
            w(out, u32::from_be_bytes(exact(raw)?))
        }
        Type::XID8 => w(out, u64::from_be_bytes(exact(raw)?)),
        Type::FLOAT4 => {
            crate::display::push_f32(f32::from_be_bytes(exact(raw)?), out);
            Some(())
        }
        Type::FLOAT8 => {
            crate::display::push_f64(f64::from_be_bytes(exact(raw)?), out);
            Some(())
        }
        Type::NUMERIC => numeric(raw, out),
        Type::MONEY => {
            let v = i64::from_be_bytes(exact(raw)?);
            let sign = if v < 0 { "-" } else { "" };
            let a = v.unsigned_abs();
            w(out, format_args!("{sign}{}.{:02}", a / 100, a % 100))
        }
        Type::UUID => w(out, uuid::Uuid::from_bytes(exact(raw)?)),
        Type::BYTEA => {
            crate::display::push_hex(raw, out);
            Some(())
        }
        Type::DATE => date(i32::from_be_bytes(exact(raw)?), out),
        Type::TIME => {
            time(i64::from_be_bytes(exact(raw)?), out);
            Some(())
        }
        Type::TIMETZ => {
            let mut r = Rd(raw);
            let us = r.i64()?;
            let zone = r.i32()?;
            time(us, out);
            offset(-zone, out);
            Some(())
        }
        Type::TIMESTAMP => timestamp(i64::from_be_bytes(exact(raw)?), false, out),
        Type::TIMESTAMPTZ => timestamp(i64::from_be_bytes(exact(raw)?), true, out),
        Type::INTERVAL => interval(raw, out),
        Type::INET | Type::CIDR => inet(raw, out),
        Type::MACADDR | Type::MACADDR8 => {
            if raw.len() != 6 && raw.len() != 8 {
                return None;
            }
            for (i, b) in raw.iter().enumerate() {
                if i > 0 {
                    out.push(':');
                }
                let _ = write!(out, "{b:02x}");
            }
            Some(())
        }
        Type::BIT | Type::VARBIT => {
            let mut r = Rd(raw);
            let len = r.i32()?.max(0) as usize;
            let bytes = r.take(len.div_ceil(8))?;
            for i in 0..len {
                out.push(if bytes[i / 8] >> (7 - i % 8) & 1 == 1 { '1' } else { '0' });
            }
            Some(())
        }
        Type::PG_LSN => {
            let v = u64::from_be_bytes(exact(raw)?);
            w(out, format_args!("{:X}/{:X}", v >> 32, v & 0xffff_ffff))
        }
        Type::RECORD => record(None, raw, out),
        // Unknown/extension types (citext, ltree, ...) usually send text.
        _ => {
            let s = std::str::from_utf8(raw).ok()?;
            if s.chars().any(|c| c.is_control() && !c.is_whitespace()) {
                return None;
            }
            out.push_str(s);
            Some(())
        }
    }
}

fn w(out: &mut String, v: impl std::fmt::Display) -> Option<()> {
    write!(out, "{v}").ok()
}

fn numeric(raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let ndigits = r.i16()?.max(0) as usize;
    let weight = r.i16()? as i32;
    let sign = r.u16()?;
    let dscale = r.u16()? as i32;
    let mut digits = Vec::with_capacity(ndigits);
    for _ in 0..ndigits {
        digits.push(r.i16()?);
    }
    match sign {
        0xC000 => {
            out.push_str("NaN");
            return Some(());
        }
        0xD000 => {
            out.push_str("Infinity");
            return Some(());
        }
        0xF000 => {
            out.push_str("-Infinity");
            return Some(());
        }
        0x4000 => out.push('-'),
        0x0000 => {}
        _ => return None,
    }
    let digit = |i: i32| if i >= 0 && (i as usize) < digits.len() { digits[i as usize] } else { 0 };
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                w(out, digit(i))?;
            } else {
                w(out, format_args!("{:04}", digit(i)))?;
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut written = 0;
        let mut i = weight + 1;
        while written < dscale {
            let group = format!("{:04}", digit(i));
            for ch in group.chars() {
                if written < dscale {
                    out.push(ch);
                    written += 1;
                }
            }
            i += 1;
        }
    }
    Some(())
}

const PG_EPOCH_DAYS_FROM_CE: i32 = 730_120; // 2000-01-01 as days since 0001-01-01

fn pg_epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
}

fn push_year_date(d: NaiveDate, out: &mut String) -> bool {
    use chrono::Datelike;
    let y = d.year();
    if y <= 0 {
        let _ = write!(out, "{:04}-{:02}-{:02}", 1 - y, d.month(), d.day());
        true // BC
    } else {
        let _ = write!(out, "{:04}-{:02}-{:02}", y, d.month(), d.day());
        false
    }
}

fn date(days: i32, out: &mut String) -> Option<()> {
    match days {
        i32::MAX => out.push_str("infinity"),
        i32::MIN => out.push_str("-infinity"),
        _ => {
            let d = NaiveDate::from_num_days_from_ce_opt(days.checked_add(PG_EPOCH_DAYS_FROM_CE)?)?;
            if push_year_date(d, out) {
                out.push_str(" BC");
            }
        }
    }
    Some(())
}

fn frac(us: u32, out: &mut String) {
    if us != 0 {
        let s = format!("{us:06}");
        out.push('.');
        out.push_str(s.trim_end_matches('0'));
    }
}

fn time(us: i64, out: &mut String) {
    let secs = us.div_euclid(1_000_000);
    let _ = write!(out, "{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    frac(us.rem_euclid(1_000_000) as u32, out);
}

/// `secs` east of UTC.
fn offset(secs: i32, out: &mut String) {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    let _ = write!(out, "{sign}{:02}", a / 3600);
    if a % 3600 != 0 {
        let _ = write!(out, ":{:02}", a / 60 % 60);
        if a % 60 != 0 {
            let _ = write!(out, ":{:02}", a % 60);
        }
    }
}

fn timestamp(us: i64, tz: bool, out: &mut String) -> Option<()> {
    match us {
        i64::MAX => out.push_str("infinity"),
        i64::MIN => out.push_str("-infinity"),
        _ => {
            let utc = pg_epoch().checked_add_signed(Duration::microseconds(us))?;
            let (naive, off) = if tz {
                let l: DateTime<Local> = Local.from_utc_datetime(&utc);
                (l.naive_local(), Some(l.offset().local_minus_utc()))
            } else {
                (utc, None)
            };
            let bc = push_year_date(naive.date(), out);
            let t: NaiveTime = naive.time();
            let _ = write!(out, " {}", t.format("%H:%M:%S"));
            use chrono::Timelike;
            frac(t.nanosecond() / 1000, out);
            if let Some(o) = off {
                offset(o, out);
            }
            if bc {
                out.push_str(" BC");
            }
        }
    }
    Some(())
}

fn interval(raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let us = r.i64()?;
    let days = r.i32()?;
    let months = r.i32()?;
    let (years, mons) = (months / 12, months % 12);
    let mut parts: Vec<String> = Vec::new();
    let unit = |n: i32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if years != 0 {
        parts.push(unit(years, "year", "years"));
    }
    if mons != 0 {
        parts.push(unit(mons, "mon", "mons"));
    }
    if days != 0 {
        parts.push(unit(days, "day", "days"));
    }
    if us != 0 || parts.is_empty() {
        let mut s = String::new();
        if us < 0 {
            s.push('-');
        }
        time(us.unsigned_abs() as i64, &mut s);
        parts.push(s);
    }
    out.push_str(&parts.join(" "));
    Some(())
}

fn inet(raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let family = r.u8()?;
    let bits = r.u8()?;
    let is_cidr = r.u8()? != 0;
    let nb = r.u8()? as usize;
    let addr = r.take(nb)?;
    let max = match family {
        2 => {
            w(out, Ipv4Addr::from(<[u8; 4]>::try_from(addr).ok()?))?;
            32
        }
        3 => {
            w(out, Ipv6Addr::from(<[u8; 16]>::try_from(addr).ok()?))?;
            128
        }
        _ => return None,
    };
    if is_cidr || bits != max {
        w(out, format_args!("/{bits}"))?;
    }
    Some(())
}

fn elem_needs_quotes(s: &str, record: bool) -> bool {
    s.is_empty()
        || (!record && s.eq_ignore_ascii_case("NULL"))
        || s.chars().any(|c| {
            c.is_whitespace() || c == '"' || c == '\\' || c == ',' || if record { c == '(' || c == ')' } else { c == '{' || c == '}' }
        })
}

fn push_elem(s: &str, record: bool, out: &mut String) {
    if elem_needs_quotes(s, record) {
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

fn array(elem: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let ndim = r.i32()?;
    let _flags = r.i32()?;
    let _elem_oid = r.u32()?;
    if !(0..=6).contains(&ndim) {
        return None;
    }
    let mut dims = Vec::new();
    let mut custom_bounds = false;
    for _ in 0..ndim {
        let len = r.i32()?;
        let lb = r.i32()?;
        if len < 0 {
            return None;
        }
        custom_bounds |= lb != 1;
        dims.push((len, lb));
    }
    if ndim == 0 {
        out.push_str("{}");
        return Some(());
    }
    if custom_bounds {
        for (len, lb) in &dims {
            w(out, format_args!("[{}:{}]", lb, lb + len - 1))?;
        }
        out.push('=');
    }
    let lens: Vec<i32> = dims.iter().map(|d| d.0).collect();
    array_dim(elem, &lens, &mut r, out)?;
    r.done().then_some(())
}

fn array_dim(elem: &Type, dims: &[i32], r: &mut Rd, out: &mut String) -> Option<()> {
    out.push('{');
    for i in 0..dims[0] {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            array_dim(elem, &dims[1..], r, out)?;
        } else {
            match r.value()? {
                None => out.push_str("NULL"),
                Some(v) => {
                    let mut s = String::new();
                    format_value(elem, v, &mut s);
                    push_elem(&s, false, out);
                }
            }
        }
    }
    out.push('}');
    Some(())
}

fn range(elem: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let flags = r.u8()?;
    if flags & 0x01 != 0 {
        out.push_str("empty");
        return Some(());
    }
    out.push(if flags & 0x02 != 0 { '[' } else { '(' });
    let bound = |inf: bool, r: &mut Rd, out: &mut String| -> Option<()> {
        if !inf {
            let v = r.value()??;
            let mut s = String::new();
            format_value(elem, v, &mut s);
            push_elem(&s, true, out);
        }
        Some(())
    };
    bound(flags & 0x08 != 0, &mut r, out)?;
    out.push(',');
    bound(flags & 0x10 != 0, &mut r, out)?;
    out.push(if flags & 0x04 != 0 { ']' } else { ')' });
    Some(())
}

fn record(types: Option<&[Type]>, raw: &[u8], out: &mut String) -> Option<()> {
    let mut r = Rd(raw);
    let n = r.i32()?.max(0) as usize;
    out.push('(');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        let oid = r.u32()?;
        let ty = types.and_then(|t| t.get(i).cloned()).or_else(|| Type::from_oid(oid)).unwrap_or(Type::UNKNOWN);
        if let Some(v) = r.value()? {
            let mut s = String::new();
            format_value(&ty, v, &mut s);
            push_elem(&s, true, out);
        }
    }
    out.push(')');
    r.done().then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(ty: &Type, raw: &[u8]) -> String {
        let mut s = String::new();
        format_value(ty, raw, &mut s);
        s
    }

    fn num(ndigits: &[i16], weight: i16, sign: u16, dscale: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend((ndigits.len() as i16).to_be_bytes());
        v.extend(weight.to_be_bytes());
        v.extend(sign.to_be_bytes());
        v.extend(dscale.to_be_bytes());
        for d in ndigits {
            v.extend(d.to_be_bytes());
        }
        v
    }

    #[test]
    fn numerics() {
        assert_eq!(f(&Type::NUMERIC, &num(&[], 0, 0, 0)), "0");
        assert_eq!(f(&Type::NUMERIC, &num(&[1234, 5678], 1, 0, 0)), "12345678");
        assert_eq!(f(&Type::NUMERIC, &num(&[12, 3400], 0, 0x4000, 2)), "-12.34");
        assert_eq!(f(&Type::NUMERIC, &num(&[1], -1, 0, 4)), "0.0001");
        assert_eq!(f(&Type::NUMERIC, &num(&[1], -2, 0, 8)), "0.00000001");
        assert_eq!(f(&Type::NUMERIC, &num(&[1], 2, 0, 0)), "100000000");
        assert_eq!(f(&Type::NUMERIC, &num(&[5], 0, 0, 3)), "5.000");
        assert_eq!(f(&Type::NUMERIC, &num(&[], 0, 0xC000, 0)), "NaN");
    }

    #[test]
    fn temporal() {
        assert_eq!(f(&Type::DATE, &0i32.to_be_bytes()), "2000-01-01");
        assert_eq!(f(&Type::DATE, &(-1i32).to_be_bytes()), "1999-12-31");
        assert_eq!(f(&Type::DATE, &i32::MAX.to_be_bytes()), "infinity");
        assert_eq!(f(&Type::TIME, &3_723_500_000i64.to_be_bytes()), "01:02:03.5");
        let ts = 86_400_000_000i64 + 1;
        assert_eq!(f(&Type::TIMESTAMP, &ts.to_be_bytes()), "2000-01-02 00:00:00.000001");
        let mut tz = 0i64.to_be_bytes().to_vec();
        tz.extend((-19800i32).to_be_bytes()); // +05:30
        assert_eq!(f(&Type::TIMETZ, &tz), "00:00:00+05:30");
        let iv = |us: i64, d: i32, m: i32| {
            let mut v = us.to_be_bytes().to_vec();
            v.extend(d.to_be_bytes());
            v.extend(m.to_be_bytes());
            f(&Type::INTERVAL, &v)
        };
        assert_eq!(iv(0, 0, 0), "00:00:00");
        assert_eq!(iv(14_706_000_000, 3, 14), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(iv(-1_500_000, 1, 0), "1 day -00:00:01.5");
    }

    #[test]
    fn misc() {
        assert_eq!(f(&Type::JSONB, b"\x01{\"a\": 1}"), "{\"a\": 1}");
        assert_eq!(f(&Type::MONEY, &(-12345i64).to_be_bytes()), "-123.45");
        assert_eq!(f(&Type::INET, &[2, 32, 0, 4, 10, 0, 0, 1]), "10.0.0.1");
        assert_eq!(f(&Type::CIDR, &[2, 8, 1, 4, 10, 0, 0, 0]), "10.0.0.0/8");
        assert_eq!(f(&Type::VARBIT, &[0, 0, 0, 5, 0b1010_1000]), "10101");
        assert_eq!(f(&Type::BYTEA, &[0xde, 0xad]), "\\xdead");
        assert_eq!(f(&Type::FLOAT8, &f64::INFINITY.to_be_bytes()), "Infinity");
        // Malformed input falls back to hex instead of panicking.
        assert_eq!(f(&Type::INT4, &[1, 2]), "\\x0102");
        assert_eq!(f(&Type::NUMERIC, &[0, 5]), "\\x0005");
    }

    #[test]
    fn arrays() {
        let mut v = Vec::new();
        v.extend(1i32.to_be_bytes()); // ndim
        v.extend(1i32.to_be_bytes()); // has nulls
        v.extend(25u32.to_be_bytes()); // text
        v.extend(4i32.to_be_bytes()); // len
        v.extend(1i32.to_be_bytes()); // lbound
        for e in [Some("a"), None, Some("b c"), Some("")] {
            match e {
                None => v.extend((-1i32).to_be_bytes()),
                Some(s) => {
                    v.extend((s.len() as i32).to_be_bytes());
                    v.extend(s.as_bytes());
                }
            }
        }
        assert_eq!(f(&Type::TEXT_ARRAY, &v), r#"{a,NULL,"b c",""}"#);

        let mut e = Vec::new();
        e.extend(0i32.to_be_bytes());
        e.extend(0i32.to_be_bytes());
        e.extend(23u32.to_be_bytes());
        assert_eq!(f(&Type::INT4_ARRAY, &e), "{}");
    }
}
