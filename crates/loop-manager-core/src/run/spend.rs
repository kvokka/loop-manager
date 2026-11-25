//! The spend ledger (docs/design/formula-budgets.md): what each attempt spent, by agent, provider and model, charged
//! from the running totals the harness reports over a checkpoint kept per session and counter.

use super::SpendDetail;
use crate::{
    config::Pricing,
    driver::{Owner, Tokens},
    usd::Usd,
    writer::{Error, Tx},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The token fields of [`Tokens`] in the column order of `spend` and `spend_counters`.
fn fields(t: &Tokens) -> [u64; 5] {
    [t.input, t.cache_read, t.cache_write_5m, t.cache_write_1h, t.output]
}

fn sql_int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// One running total of a usage counter, reported by the harness of the attempt `(run, attempt)`.
pub(super) struct Total<'a> {
    pub(super) session: &'a str,
    pub(super) counter: &'a str,
    pub(super) owner: &'a Owner,
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) service_tier: Option<&'a str>,
    pub(super) tokens: &'a Tokens,
    pub(super) cost: Option<Usd>,
}

/// The ledger row a part of a charge is added to.
struct Key<'a> {
    run: &'a str,
    attempt: u32,
    agent: &'a str,
    provider: &'a str,
    model: &'a str,
    estimate: bool,
    price: &'a str,
}

/// Adds `tokens` and `cost` to the ledger row of `key`.
fn add(tx: &Tx<'_>, key: &Key<'_>, tokens: [i64; 5], cost: i64) -> Result<(), Error> {
    tx.sql().execute(
        "INSERT INTO spend (run, attempt, agent, provider, model, estimate, price, input, cache_read, cache_write_5m, \
         cache_write_1h, output, cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT (run, attempt, agent, provider, model, estimate, price) DO UPDATE SET \
         input = input + excluded.input, cache_read = cache_read + excluded.cache_read, \
         cache_write_5m = cache_write_5m + excluded.cache_write_5m, \
         cache_write_1h = cache_write_1h + excluded.cache_write_1h, output = output + excluded.output, \
         cost = cost + excluded.cost",
        params![
            key.run,
            key.attempt,
            key.agent,
            key.provider,
            key.model,
            key.estimate,
            key.price,
            tokens[0],
            tokens[1],
            tokens[2],
            tokens[3],
            tokens[4],
            cost
        ],
    )?;
    Ok(())
}

/// The agent a part of the spend of attempt `(run, attempt)` is charged to: `''` for the unattributed part.
fn agent(tx: &Tx<'_>, run: &str, attempt: u32, owner: &Owner) -> Result<String, Error> {
    Ok(match owner {
        Owner::Session => tx
            .sql()
            .query_row(
                "SELECT COALESCE(agent, '') FROM attempts WHERE run = ?1 AND n = ?2",
                params![run, attempt],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default(),
        Owner::Subagent(name) => name.clone(),
        Owner::Unknown => String::new(),
    })
}

fn tokens_of(t: [i64; 5]) -> Tokens {
    let [input, cache_read, cache_write_5m, cache_write_1h, output] = t.map(|n| u64::try_from(n).unwrap_or(0));
    Tokens {
        input,
        cache_read,
        cache_write_5m,
        cache_write_1h,
        output,
    }
}

/// The table price of `tokens` (the user's entry, else the catalog's): the record of the price used and the cost;
/// a model in neither costs zero.
fn table_price(pricing: &Pricing, provider: &str, model: &str, tier: Option<&str>, tokens: [i64; 5]) -> (Value, i64) {
    let tokens = tokens_of(tokens);
    match pricing.lookup(provider, model, tier) {
        Some((source, price)) => {
            let rates = price.rates(&tokens);
            (pricing.record(source, rates), rates.cost(&tokens).0)
        }
        None => (json!({ "source": "none" }), 0),
    }
}

/// Charges to `(run, attempt)` the tokens of one request a later running total will include (contracts 4.3), at the
/// table price, as an estimate of `owner`'s spend until [`charge`] settles it.
pub(super) fn estimate(
    tx: &Tx<'_>,
    pricing: &Pricing,
    (run, attempt): (&str, u32),
    owner: &Owner,
    (provider, model): (&str, &str),
    tokens: &Tokens,
) -> Result<(), Error> {
    let tokens = fields(tokens).map(sql_int);
    let (price, cost) = table_price(pricing, provider, model, None, tokens);
    let agent = agent(tx, run, attempt, owner)?;
    let key = Key {
        run,
        attempt,
        agent: &agent,
        provider,
        model,
        estimate: true,
        price: &price.to_string(),
    };
    add(tx, &key, tokens, cost)
}

/// Charges to `(run, attempt)` what `total` adds over the checkpoint of its counter, and moves the checkpoint to it.
/// Each request is thereby charged once, however often a total that includes it is repeated, across a resume and a
/// restart. A field below its checkpoint adds nothing: a harness process killed in a turn loses what it added since its
/// session last saved its totals, so the next total can fall back (docs/design/experiments.md, experiment 11);
/// charging it whole would charge the saved part twice.
///
/// The increment settles the estimates of its attempt, provider and model: their tokens stay their agents', and the
/// amount is split between them and the rest of the increment by their table costs (by tokens when nothing has a
/// price), to the pico-USD; the rest is the total's owner's.
pub(super) fn charge(tx: &Tx<'_>, pricing: &Pricing, run: &str, attempt: u32, total: &Total<'_>) -> Result<(), Error> {
    let now = fields(total.tokens).map(sql_int);
    let cost = total.cost.map(|c| c.0);
    let last: Option<([i64; 5], Option<i64>)> = tx
        .sql()
        .query_row(
            "SELECT input, cache_read, cache_write_5m, cache_write_1h, output, cost FROM spend_counters \
             WHERE session = ?1 AND counter = ?2",
            params![total.session, total.counter],
            |r| Ok(([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?], r.get(5)?)),
        )
        .optional()?;
    let (base, base_cost) = last.map_or(([0; 5], 0), |(t, c)| (t, c.unwrap_or_default()));
    let delta: [i64; 5] = std::array::from_fn(|i| (now[i] - base[i]).max(0));
    let delta_cost = cost.map(|c| (c - base_cost).max(0));
    tx.sql().execute(
        "INSERT OR REPLACE INTO spend_counters \
         (session, counter, input, cache_read, cache_write_5m, cache_write_1h, output, cost) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            total.session,
            total.counter,
            now[0],
            now[1],
            now[2],
            now[3],
            now[4],
            cost
        ],
    )?;
    if delta.iter().all(|d| *d == 0) && delta_cost.unwrap_or_default() == 0 {
        return Ok(());
    }
    let (provider, model, tier) = (total.provider, total.model, total.service_tier);
    // The harness's cost first, an explicit zero included; else the user's price or the catalog's; else zero.
    let (price, amount) = match delta_cost {
        Some(cost) => (json!({ "source": "harness" }), cost),
        None => table_price(pricing, provider, model, tier, delta),
    };
    let settled = Settled {
        run,
        attempt,
        provider,
        model,
        tier,
        price: price.to_string(),
    };
    settle(tx, pricing, &settled, delta, amount, total.owner)
}

/// The charge a running total settles: whose, of which model, and the record of the price of its amount.
struct Settled<'a> {
    run: &'a str,
    attempt: u32,
    provider: &'a str,
    model: &'a str,
    tier: Option<&'a str>,
    price: String,
}

/// Charges `amount` for the increment `delta` of a total: the estimates of its attempt, provider and model take their
/// shares, the rest is `owner`'s (docs at [`charge`]).
fn settle(
    tx: &Tx<'_>,
    pricing: &Pricing,
    s: &Settled<'_>,
    delta: [i64; 5],
    amount: i64,
    owner: &Owner,
) -> Result<(), Error> {
    let Settled {
        run,
        attempt,
        provider,
        model,
        tier,
        ref price,
    } = *s;
    let estimates: Vec<(String, [i64; 5], i64)> = {
        let mut stmt = tx.sql().prepare(
            "SELECT agent, input, cache_read, cache_write_5m, cache_write_1h, output, cost FROM spend \
             WHERE run = ?1 AND attempt = ?2 AND provider = ?3 AND model = ?4 AND estimate = 1",
        )?;
        let rows = stmt.query_map(params![run, attempt, provider, model], |r| {
            Ok((
                r.get(0)?,
                [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
                r.get(6)?,
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let rest: [i64; 5] = std::array::from_fn(|i| (delta[i] - estimates.iter().map(|e| e.1[i]).sum::<i64>()).max(0));
    let (_, rest_weight) = table_price(pricing, provider, model, tier, rest);
    let mut weights: Vec<i64> = estimates.iter().map(|e| e.2).chain([rest_weight]).collect();
    if weights.iter().all(|w| *w == 0) {
        weights = estimates
            .iter()
            .map(|e| e.1)
            .chain([rest])
            .map(|t| t.iter().sum())
            .collect();
    }
    let sum: i128 = weights.iter().map(|w| i128::from(*w)).sum();
    let share = |w: i64| match sum {
        0 => 0,
        sum => i64::try_from(i128::from(amount) * i128::from(w) / sum).unwrap_or(i64::MAX),
    };
    tx.sql().execute(
        "DELETE FROM spend WHERE run = ?1 AND attempt = ?2 AND provider = ?3 AND model = ?4 AND estimate = 1",
        params![run, attempt, provider, model],
    )?;
    let mut settled = 0;
    for ((agent, tokens, _), weight) in estimates.iter().zip(&weights) {
        let cost = share(*weight);
        settled += cost;
        let key = Key {
            run,
            attempt,
            agent,
            provider,
            model,
            estimate: false,
            price,
        };
        add(tx, &key, *tokens, cost)?;
    }
    if rest.iter().all(|t| *t == 0) && amount == settled {
        return Ok(());
    }
    let owner = agent(tx, run, attempt, owner)?;
    let key = Key {
        run,
        attempt,
        agent: &owner,
        provider,
        model,
        estimate: false,
        price,
    };
    add(tx, &key, rest, amount - settled)
}

/// The sums of the `usage` of contracts 1.3 over ledger rows, in the column order [`usage_at`] reads.
const SUMS: &str = "COALESCE(SUM(input), 0), COALESCE(SUM(cache_read), 0), \
    COALESCE(SUM(cache_write_5m + cache_write_1h), 0), COALESCE(SUM(output), 0), COALESCE(SUM(cost), 0)";

/// The `usage` the [`SUMS`] give from column `at` of `r` on.
fn usage_at(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Value> {
    Ok(json!({
        "input_tokens": r.get::<_, i64>(at)?, "cache_read_tokens": r.get::<_, i64>(at + 1)?,
        "cache_write_tokens": r.get::<_, i64>(at + 2)?, "output_tokens": r.get::<_, i64>(at + 3)?,
        "cost_usd": Usd(r.get(at + 4)?).to_string(),
    }))
}

/// `usage` of contracts 1.3 from the ledger rows a `WHERE` clause over `spend` selects, its `?1` bound to `key`.
fn sums(conn: &Connection, filter: &str, key: impl rusqlite::Params) -> Result<Value, Error> {
    let sql = format!("SELECT {SUMS} FROM spend WHERE {filter}");
    Ok(conn.query_row(&sql, key, |r| usage_at(r, 0))?)
}

/// The ledger rows of a run (`?1`) and its subtree (docs/design/formula-budgets.md): the runs its sessions and steps
/// started, down the chain; a run a trigger started is an invocation of its own.
const SUBTREE: &str = "run IN (WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL \
    SELECT r.id FROM runs r JOIN tree t ON r.parent_run = t.id WHERE r.trigger IS NULL) SELECT id FROM tree)";

/// What a run and its subtree spent.
pub(super) fn usage(conn: &Connection, run: &str) -> Result<Value, Error> {
    sums(conn, SUBTREE, [run])
}

/// The rows of a spend report on the subtree of `run` (docs/api/frontends.md): its ledger grouped by agent, or by
/// agent, provider and model, in that order; none for [`SpendDetail::Total`]. The rows sum to [`usage`] exactly: the
/// same rows, only grouped. An unattributed part has a null `agent`.
pub(super) fn rows(conn: &Connection, run: &str, detail: SpendDetail) -> Result<Vec<Value>, Error> {
    let keys = match detail {
        SpendDetail::Total => return Ok(Vec::new()),
        SpendDetail::Agent => "agent",
        SpendDetail::Model => "agent, provider, model",
    };
    let sql = format!("SELECT {keys}, {SUMS} FROM spend WHERE {SUBTREE} GROUP BY {keys} ORDER BY {keys}");
    let mut stmt = conn.prepare(&sql)?;
    let by_model = matches!(detail, SpendDetail::Model);
    let rows = stmt.query_map([run], |r| {
        let mut row = usage_at(r, if by_model { 3 } else { 1 })?;
        let agent: String = r.get(0)?;
        row["agent"] = json!((!agent.is_empty()).then_some(agent));
        if by_model {
            row["provider"] = json!(r.get::<_, String>(1)?);
            row["model"] = json!(r.get::<_, String>(2)?);
        }
        Ok(row)
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The outermost live run whose budget the spend of its subtree has reached, from `run` up the runs whose subtree
/// it is in (docs/design/formula-budgets.md, "Scope and enforcement").
pub(super) fn exhausted(conn: &Connection, run: &str) -> Result<Option<String>, Error> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE up(id, parent, trigger, level) AS (SELECT id, parent_run, trigger, 0 FROM runs WHERE id = ?1 \
         UNION ALL SELECT r.id, r.parent_run, r.trigger, up.level + 1 FROM runs r JOIN up ON r.id = up.parent \
         WHERE up.trigger IS NULL) \
         SELECT r.id, r.budget FROM up JOIN runs r ON r.id = up.id \
         WHERE r.budget IS NOT NULL AND r.state NOT IN ('passed', 'failed', 'cancelled') ORDER BY up.level DESC",
    )?;
    let limited: Vec<(String, i64)> = stmt
        .query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, budget) in limited {
        let sql = format!("SELECT COALESCE(SUM(cost), 0) FROM spend WHERE {SUBTREE}");
        let spent: i64 = conn.query_row(&sql, [&id], |r| r.get(0))?;
        if spent >= budget {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// What one attempt spent, its session's subagents included.
pub(super) fn attempt_usage(conn: &Connection, run: &str, attempt: u32) -> Result<Value, Error> {
    sums(conn, "run = ?1 AND attempt = ?2", params![run, attempt])
}
