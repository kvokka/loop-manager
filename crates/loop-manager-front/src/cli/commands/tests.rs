use super::{metrics_table, spend_table};
use serde_json::json;

fn usage(input: u64, output: u64, cost: &str) -> serde_json::Value {
    json!({ "input_tokens": input, "cache_read_tokens": 0, "cache_write_tokens": 0, "output_tokens": output,
        "cost_usd": cost })
}

#[test]
fn a_spend_table_aligns_its_rows_names_the_unattributed_part_and_rounds_costs() {
    let mut unattributed = usage(5, 1, "0.00004999");
    unattributed["agent"] = json!(null);
    unattributed["provider"] = json!("anthropic");
    unattributed["model"] = json!("haiku");
    let mut priced = usage(1200, 30, "1.23456");
    priced["agent"] = json!("shop.dev");
    priced["provider"] = json!("anthropic");
    priced["model"] = json!("opus");
    let report = json!({ "run": "r1", "budget_usd": "10", "detail": "model", "total": usage(1205, 31, "1.23460999"),
        "rows": [unattributed, priced] });
    let want = [
        "agent         provider   model  input  cache read  cache write  output  cost USD",
        "unattributed  anthropic  haiku      5           0            0       1    0.0000",
        "shop.dev      anthropic  opus    1200           0            0      30    1.2346",
        "total                            1205           0            0      31    1.2346",
        "budget                                                                   10.0000",
    ];
    assert_eq!(spend_table(&report), want.join("\n"));
}

#[test]
fn a_metrics_table_heads_its_key_column_by_the_grouping_and_names_the_unattributed_part() {
    let row = |key: serde_json::Value, runs: u64, cost: &str| {
        let mut row = usage(1200, 30, cost);
        for (field, n) in [("runs", runs), ("attempts", 2), ("turns", 7), ("session_s", 95)] {
            row[field] = json!(n);
        }
        row["key"] = key;
        row
    };
    let report = json!({ "by": "agent", "project": null,
        "rows": [row(json!("shop.dev"), 12, "1.23456"), row(json!(null), 1, "0.00004999")] });
    let want = [
        "agent         runs  attempts  turns  session s  input  cache read  cache write  output  cost USD",
        "shop.dev        12         2      7         95   1200           0            0      30    1.2346",
        "unattributed     1         2      7         95   1200           0            0      30    0.0000",
    ];
    assert_eq!(metrics_table(&report), want.join("\n"));
}
