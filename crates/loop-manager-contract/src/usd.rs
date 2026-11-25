//! Money in USD as an integer of pico-USD (10⁻¹² USD), so sums keep full precision and only a display rounds
//! (docs/design/formula-budgets.md, "Pricing a request").

use std::fmt;

/// Pico-USD per USD.
const PICO: i64 = 1_000_000_000_000;

/// An amount in pico-USD; `i64` holds about 9.2 million USD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Usd(pub i64);

impl Usd {
    /// A harness's float amount in USD, to the nearest pico-USD.
    pub fn from_f64(usd: f64) -> Self {
        // `as` saturates: a non-finite or huge amount ends at a bound, never wraps.
        Self((usd * PICO as f64).round() as i64)
    }
}

/// The exact decimal in USD with no trailing zeros: `0.00143244`, `5`, `-0.5`.
impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let (whole, frac) = (abs / PICO as u64, abs % PICO as u64);
        if frac == 0 {
            return write!(f, "{sign}{whole}");
        }
        let frac = format!("{frac:012}");
        write!(f, "{sign}{whole}.{}", frac.trim_end_matches('0'))
    }
}

/// A decimal amount in USD of at least zero and at most twelve decimals: `10`, `2.5`, `0.000001`.
impl std::str::FromStr for Usd {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) || !(frac.is_empty() || digits(frac)) || text.ends_with('.') {
            return Err(format!("`{text}` is not an amount of USD of at least zero"));
        }
        if frac.len() > 12 {
            return Err(format!("`{text}` has more than twelve decimals"));
        }
        let scaled = format!("{whole}{frac:0<12}");
        scaled.parse().map(Self).map_err(|_| format!("`{text}` is too large"))
    }
}

#[cfg(test)]
mod tests {
    use super::Usd;

    #[test]
    fn an_amount_reads_back_as_it_prints() {
        for text in ["10", "2.5", "0.000000000001", "0"] {
            let usd: Usd = text.parse().unwrap();
            assert_eq!(usd.to_string(), text);
        }
        assert_eq!("1.50".parse::<Usd>(), Ok(Usd(1_500_000_000_000)));
        for bad in ["-1", "1.", ".5", "1e3", "0.0000000000001", "", "99999999"] {
            assert!(bad.parse::<Usd>().is_err(), "{bad}");
        }
    }
}
