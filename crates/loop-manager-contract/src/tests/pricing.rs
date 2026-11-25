//! Prices of models (docs/design/formula-budgets.md, "Pricing a request"): the user's `[[pricing]]`, the catalog,
//! exact arithmetic on disjoint quantities.

use super::config::{BASE, fails, load_with};
use crate::{
    config::{PriceSource, Rates, catalog},
    driver::Tokens,
    usd::Usd,
};
use serde_json::json;

fn orchestrator(pricing: &str) -> String {
    let base = BASE.iter().find(|(p, _)| *p == "config.toml").unwrap().1;
    format!("{base}{pricing}")
}

const ENTRY: &str = "[[pricing]]\nprovider = \"proxy\"\nmodel = \"m\"\n";

#[test]
fn a_cost_is_exact_to_the_pico_usd_on_disjoint_quantities() {
    // USD per 1M tokens: 0.0125 input, 0.000001 read, 1.5625 write, 2.5 write 1h, 10 output.
    let rates = Rates {
        input: 12_500,
        cache_read: 1,
        cache_write: 1_562_500,
        cache_write_1h: 2_500_000,
        output: 10_000_000,
    };
    let tokens = Tokens {
        input: 3,
        cache_read: 7,
        cache_write_5m: 11,
        cache_write_1h: 13,
        output: 17,
    };
    let pico = 3 * 12_500 + 7 + 11 * 1_562_500 + 13 * 2_500_000 + 17 * 10_000_000;
    assert_eq!(rates.cost(&tokens), Usd(pico));
}

#[test]
fn a_users_entry_defaults_its_cache_rates_and_switches_at_its_long_context() {
    let text = format!(
        "{ENTRY}input = 1.25\noutput = 10\ncache_write = 1.5\nlong_context_over = 100\n\
         long_context = {{ input = 2.5, output = 15 }}\n"
    );
    let config = load_with(&[("config.toml", orchestrator(&text).as_str())]).unwrap();
    let (source, price) = config.pricing.lookup("proxy", "m", None).unwrap();
    assert_eq!(source, PriceSource::User);
    let short = Rates {
        input: 1_250_000,
        cache_read: 1_250_000,
        cache_write: 1_500_000,
        cache_write_1h: 1_500_000,
        output: 10_000_000,
    };
    assert_eq!(price.rates, short);
    let at = |input| Tokens {
        input,
        ..Tokens::default()
    };
    assert_eq!(price.rates(&at(100)), &short, "at the threshold: the short rates");
    assert_eq!(price.rates(&at(101)).input, 2_500_000);
    assert_eq!(
        price.rates(&at(101)).cache_read,
        2_500_000,
        "defaults within the long rates"
    );
    let record = config.pricing.record(source, price.rates(&at(1)));
    assert_eq!(record["source"], "user");
    assert_eq!(record["cache_write"], "1.5");
}

#[test]
fn the_lookup_takes_the_users_entry_for_its_provider_before_the_catalog() {
    let model = catalog().catalog[0].model.clone();
    let text = format!(
        "[[pricing]]\nprovider = \"proxy\"\nmodel = \"{model}\"\ninput = 1\noutput = 2\n\
         [[pricing]]\nprovider = \"proxy\"\nmodel = \"{model}\"\nservice_tier = \"priority\"\ninput = 3\noutput = 6\n"
    );
    let config = load_with(&[("config.toml", orchestrator(&text).as_str())]).unwrap();
    let p = &config.pricing;
    let input = |provider, tier| {
        p.lookup(provider, &model, tier)
            .map(|(s, price)| (s, price.rates.input))
    };
    assert_eq!(input("proxy", Some("priority")), Some((PriceSource::User, 3_000_000)));
    assert_eq!(input("proxy", Some("flex")), Some((PriceSource::User, 1_000_000)));
    assert_eq!(input("proxy", None), Some((PriceSource::User, 1_000_000)));
    let official = catalog().catalog[0].rates.input;
    assert_eq!(input("openai", None), Some((PriceSource::Catalog, official)));
    assert_eq!(p.lookup("proxy", "a-private-model", None), None);
    let record = p.record(PriceSource::Catalog, &catalog().catalog[0].rates);
    assert_eq!(record["date"], json!(catalog().date));
}

#[test]
fn a_wrong_pricing_entry_is_a_load_error_that_names_it() {
    for (entry, error) in [
        ("input = 1\noutput = 2\ncolour = 3\n", "unknown field `colour`"),
        ("input = -1\noutput = 2\n", "`-1` is not a rate"),
        ("input = 0.0000001\noutput = 2\n", "more than six decimals"),
        ("input = 1\noutput = 2\nlong_context_over = 5\n", "go together"),
    ] {
        let text = orchestrator(&format!("{ENTRY}{entry}"));
        fails(&[("config.toml", text.as_str())], error);
    }
    let twice = orchestrator(&format!("{ENTRY}input = 1\noutput = 2\n{ENTRY}input = 1\noutput = 2\n"));
    fails(
        &[("config.toml", twice.as_str())],
        "two entries for one provider, model and service tier",
    );
    let anonymous = orchestrator("[[pricing]]\nmodel = \"m\"\ninput = 1\noutput = 2\n");
    fails(&[("config.toml", anonymous.as_str())], "needs `provider`");
}

#[test]
fn the_shipped_catalog_is_dated_and_prices_every_entry() {
    let catalog = catalog();
    assert!(
        jiff::civil::Date::strptime("%Y-%m-%d", &catalog.date).is_ok(),
        "{}",
        catalog.date
    );
    assert!(catalog.catalog.iter().any(|p| p.model.starts_with("claude-")));
    assert!(catalog.catalog.iter().any(|p| p.model.starts_with("gpt-")));
    assert!(
        catalog
            .catalog
            .iter()
            .all(|p| p.rates.output > 0 && p.provider.is_none())
    );
}
