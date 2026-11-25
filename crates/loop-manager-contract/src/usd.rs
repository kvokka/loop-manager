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
