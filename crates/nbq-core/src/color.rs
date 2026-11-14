//! Connection colors. A small curated palette (so colors stay readable on
//! both themes) plus arbitrary `#rrggbb`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnColor {
    pub rgb: u32,
}

/// Name → RGB. Chosen to read clearly on dark and light backgrounds.
pub const PALETTE: &[(&str, u32)] = &[
    ("red", 0xef4444),
    ("orange", 0xf97316),
    ("amber", 0xf59e0b),
    ("green", 0x22c55e),
    ("teal", 0x14b8a6),
    ("blue", 0x3b82f6),
    ("violet", 0x8b5cf6),
    ("pink", 0xec4899),
    ("gray", 0x8b8b96),
];

impl ConnColor {
    pub fn parse(s: &str) -> Option<ConnColor> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            if hex.len() == 6 {
                return u32::from_str_radix(hex, 16).ok().map(|rgb| ConnColor { rgb });
            }
            return None;
        }
        PALETTE.iter().find(|(n, _)| n.eq_ignore_ascii_case(s)).map(|(_, rgb)| ConnColor { rgb: *rgb })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(ConnColor::parse("red"), Some(ConnColor { rgb: 0xef4444 }));
        assert_eq!(ConnColor::parse(" Blue "), Some(ConnColor { rgb: 0x3b82f6 }));
        assert_eq!(ConnColor::parse("#00ff7f"), Some(ConnColor { rgb: 0x00ff7f }));
        assert_eq!(ConnColor::parse("#fff"), None);
        assert_eq!(ConnColor::parse("chartreuse"), None);
    }
}
