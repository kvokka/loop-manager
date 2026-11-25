//! The prices of models (docs/design/formula-budgets.md, "Pricing a request"): the user's `[[pricing]]` entries of
//! the orchestrator config and the dated catalog of official API prices shipped in `pricing.toml`.

use crate::{driver::Tokens, usd::Usd};
use serde::Deserialize;
use serde_json::{Value, json};

/// The shipped catalog: its `date`, its `sources` and the official price of each model.
const CATALOG: &str = include_str!("pricing.toml");

/// Rates in pico-USD per token, which is USD per 1M tokens times 10⁶: an integer for a rate of up to six decimals,
/// so a cost is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rates {
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cache_write_1h: i64,
    pub output: i64,
}

impl Rates {
    /// The cost of disjoint token quantities.
    pub fn cost(&self, t: &Tokens) -> Usd {
        let parts = [
            (t.input, self.input),
            (t.cache_read, self.cache_read),
            (t.cache_write_5m, self.cache_write),
            (t.cache_write_1h, self.cache_write_1h),
            (t.output, self.output),
        ];
        let sum: i128 = parts.iter().map(|(n, r)| i128::from(*n) * i128::from(*r)).sum();
        Usd(i64::try_from(sum).unwrap_or(i64::MAX))
    }

    /// The rates as written, USD per 1M tokens.
    fn json(&self) -> Value {
        let usd = |r: i64| Usd(r * 1_000_000).to_string();
        json!({ "input": usd(self.input), "cache_read": usd(self.cache_read), "cache_write": usd(self.cache_write),
            "cache_write_1h": usd(self.cache_write_1h), "output": usd(self.output) })
    }
}

/// The price of one model: of one provider for a user's entry, of any for the catalog's.
#[derive(Clone, Debug, PartialEq)]
pub struct Price {
    pub provider: Option<String>,
    pub model: String,
    pub service_tier: Option<String>,
    pub rates: Rates,
    /// Over this many input tokens (ordinary, read and written) a request takes these rates.
    pub long_context: Option<(u64, Rates)>,
}

impl Price {
    /// The rates of a request of these tokens.
    pub fn rates(&self, t: &Tokens) -> &Rates {
        let input = t.input + t.cache_read + t.cache_write_5m + t.cache_write_1h;
        match &self.long_context {
            Some((over, long)) if input > *over => long,
            _ => &self.rates,
        }
    }
}

/// Where a price comes from, in the order of the lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    Catalog,
}

/// The user's entries and the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Pricing {
    pub user: Vec<Price>,
    pub catalog: Vec<Price>,
    /// The date of the catalog, kept with every charge priced by it.
    pub date: String,
}

impl Pricing {
    /// The price of `model` from `provider` on `tier`: the user's entry for the provider, model and tier, the user's
    /// with no tier, the catalog's with the tier, the catalog's with none; else none, and the request costs zero.
    pub fn lookup(&self, provider: &str, model: &str, tier: Option<&str>) -> Option<(Source, &Price)> {
        let user = |t: Option<&str>| {
            let mut user = self.user.iter();
            user.find(|p| p.provider.as_deref() == Some(provider) && p.model == model && p.service_tier.as_deref() == t)
        };
        let catalog = |t: Option<&str>| {
            let mut catalog = self.catalog.iter();
            catalog.find(|p| p.model == model && p.service_tier.as_deref() == t)
        };
        let user = tier.and_then(|t| user(Some(t))).or_else(|| user(None));
        let catalog = tier.and_then(|t| catalog(Some(t))).or_else(|| catalog(None));
        user.map(|p| (Source::User, p))
            .or_else(|| catalog.map(|p| (Source::Catalog, p)))
    }

    /// The record of the price a charge used: its source, the catalog's date and the rates, USD per 1M tokens.
    pub fn record(&self, source: Source, rates: &Rates) -> Value {
        let mut record = rates.json();
        match source {
            Source::User => record["source"] = json!("user"),
            Source::Catalog => {
                record["source"] = json!("catalog");
                record["date"] = json!(self.date);
            }
        }
        record
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RatesFile {
    input: toml::Value,
    cache_read: Option<toml::Value>,
    cache_write: Option<toml::Value>,
    cache_write_1h: Option<toml::Value>,
    output: toml::Value,
}

/// A `[[pricing]]` entry of the orchestrator config or of the catalog.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PriceFile {
    provider: Option<String>,
    model: String,
    service_tier: Option<String>,
    // Not a flattened `RatesFile`: serde cannot deny unknown fields through `flatten`.
    input: toml::Value,
    cache_read: Option<toml::Value>,
    cache_write: Option<toml::Value>,
    cache_write_1h: Option<toml::Value>,
    output: toml::Value,
    long_context_over: Option<u64>,
    long_context: Option<RatesFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    date: String,
    /// The pages the prices come from, for whoever publishes the next catalog.
    #[serde(rename = "sources")]
    _sources: Vec<String>,
    pricing: Vec<PriceFile>,
}

/// A rate of USD per 1M tokens as pico-USD per token: at most six decimals, not negative.
fn rate(value: &toml::Value) -> Result<i64, String> {
    let text = match value {
        toml::Value::Integer(n) => n.to_string(),
        // The shortest decimal that reads back as the float: what the file says.
        toml::Value::Float(f) => f.to_string(),
        other => return Err(format!("`{other}` is not a number")),
    };
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !(frac.is_empty() || digits(frac)) {
        return Err(format!("`{text}` is not a rate of USD per 1M tokens of at least zero"));
    }
    if frac.len() > 6 {
        return Err(format!("`{text}` has more than six decimals"));
    }
    let scaled = format!("{whole}{frac:0<6}");
    scaled.parse().map_err(|_| format!("`{text}` is too large"))
}

fn rates(file: &RatesFile) -> Result<Rates, String> {
    let input = rate(&file.input)?;
    let opt = |v: &Option<toml::Value>, default: i64| v.as_ref().map_or(Ok(default), rate);
    let cache_write = opt(&file.cache_write, input)?;
    Ok(Rates {
        input,
        cache_read: opt(&file.cache_read, input)?,
        cache_write,
        cache_write_1h: opt(&file.cache_write_1h, cache_write)?,
        output: rate(&file.output)?,
    })
}

/// Checks the entries of one file: the user's need a `provider`, the catalog's have none; one entry per provider,
/// model and tier; `long_context` and `long_context_over` come together.
pub(super) fn prices(entries: Vec<PriceFile>, user: bool) -> Result<Vec<Price>, Vec<String>> {
    let mut out: Vec<Price> = Vec::new();
    let mut errors = Vec::new();
    for e in entries {
        let at = format!("`[[pricing]]` of model `{}`", e.model);
        if e.provider.is_some() != user {
            errors.push(format!(
                "{at}: {}",
                if user { "needs `provider`" } else { "has a `provider`" }
            ));
        }
        let long = match (e.long_context_over, &e.long_context) {
            (Some(over), Some(long)) => rates(long).map(|r| Some((over, r))),
            (None, None) => Ok(None),
            _ => Err("`long_context` and `long_context_over` go together".to_owned()),
        };
        let own = RatesFile {
            input: e.input,
            cache_read: e.cache_read,
            cache_write: e.cache_write,
            cache_write_1h: e.cache_write_1h,
            output: e.output,
        };
        let price = rates(&own).and_then(|r| long.map(|l| (r, l)));
        let Ok((rates, long_context)) = price.map_err(|m| errors.push(format!("{at}: {m}"))) else {
            continue;
        };
        let price = Price {
            provider: e.provider,
            model: e.model,
            service_tier: e.service_tier,
            rates,
            long_context,
        };
        if out
            .iter()
            .any(|p| (&p.provider, &p.model, &p.service_tier) == (&price.provider, &price.model, &price.service_tier))
        {
            errors.push(format!("{at}: two entries for one provider, model and service tier"));
        }
        out.push(price);
    }
    if errors.is_empty() { Ok(out) } else { Err(errors) }
}

/// The user's entries over the shipped catalog.
pub(super) fn pricing(user: Vec<PriceFile>) -> Result<Pricing, Vec<String>> {
    Ok(Pricing {
        user: prices(user, true)?,
        ..catalog()
    })
}

/// The shipped catalog; a unit test checks that it parses.
pub fn catalog() -> Pricing {
    let file: CatalogFile = toml::from_str(CATALOG).expect("the shipped price catalog parses");
    let catalog = prices(file.pricing, false).expect("the shipped price catalog is valid");
    Pricing {
        user: Vec::new(),
        catalog,
        date: file.date,
    }
}
