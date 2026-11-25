//! The spend ledger (docs/design/formula-budgets.md): what each attempt spent, by agent, provider and model, charged
//! from the running totals the harness reports over a checkpoint kept per session and counter.

use crate::{
    config::Pricing,
    driver::Tokens,
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
    pub(super) provider: &'a str,
    pub(super) model: &'a str,
    pub(super) service_tier: Option<&'a str>,
    pub(super) tokens: &'a Tokens,
    pub(super) cost: Option<Usd>,
}

/// Charges to `(run, attempt)` what `total` adds over the checkpoint of its counter, and moves the checkpoint to it.
/// Each request is thereby charged once, however often a total that includes it is repeated, across a resume and a
/// restart. A field below its checkpoint adds nothing: a claude process killed in a turn loses what it added since its
/// session last saved its totals, so the next total can fall back (docs/design/experiments.md, experiment 11);
/// charging it whole would charge the saved part twice.
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
    // The harness's cost first, an explicit zero included; else the user's price or the catalog's; else zero.
    let [input, cache_read, cache_write_5m, cache_write_1h, output] = delta.map(|d| u64::try_from(d).unwrap_or(0));
    let tokens = Tokens {
        input,
        cache_read,
        cache_write_5m,
        cache_write_1h,
        output,
    };
    let (price, amount) = match (
        delta_cost,
        pricing.lookup(total.provider, total.model, total.service_tier),
    ) {
        (Some(cost), _) => (json!({ "source": "harness" }), cost),
        (None, Some((source, price))) => {
            let rates = price.rates(&tokens);
            (pricing.record(source, rates), rates.cost(&tokens).0)
        }
        (None, None) => (json!({ "source": "none" }), 0),
    };
    tx.sql().execute(
        "INSERT INTO spend (run, attempt, agent, provider, model, price, input, cache_read, cache_write_5m, \
         cache_write_1h, output, cost) \
         SELECT ?1, ?2, COALESCE(agent, ''), ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11 FROM attempts \
         WHERE run = ?1 AND n = ?2 \
         ON CONFLICT (run, attempt, agent, provider, model, price) DO UPDATE SET \
         input = input + excluded.input, cache_read = cache_read + excluded.cache_read, \
         cache_write_5m = cache_write_5m + excluded.cache_write_5m, \
         cache_write_1h = cache_write_1h + excluded.cache_write_1h, output = output + excluded.output, \
         cost = cost + excluded.cost",
        params![
            run,
            attempt,
            total.provider,
            total.model,
            price.to_string(),
            delta[0],
            delta[1],
            delta[2],
            delta[3],
            delta[4],
            amount
        ],
    )?;
    Ok(())
}

/// `usage` of contracts 1.3 from the ledger rows a `WHERE` clause over `spend` selects, its `?1` bound to `key`.
fn sums(conn: &Connection, filter: &str, key: impl rusqlite::Params) -> Result<Value, Error> {
    let sql = format!(
        "SELECT COALESCE(SUM(input), 0), COALESCE(SUM(cache_read), 0), \
         COALESCE(SUM(cache_write_5m + cache_write_1h), 0), COALESCE(SUM(output), 0), COALESCE(SUM(cost), 0) \
         FROM spend WHERE {filter}"
    );
    Ok(conn.query_row(&sql, key, |r| {
        Ok(json!({
            "input_tokens": r.get::<_, i64>(0)?, "cache_read_tokens": r.get::<_, i64>(1)?,
            "cache_write_tokens": r.get::<_, i64>(2)?, "output_tokens": r.get::<_, i64>(3)?,
            "cost_usd": Usd(r.get(4)?).to_string(),
        }))
    })?)
}

/// The ledger rows of a run (`?1`) and its subtree (docs/design/formula-budgets.md): the runs its sessions and steps
/// started, down the chain; a run a trigger started is an invocation of its own.
const SUBTREE: &str = "run IN (WITH RECURSIVE tree(id) AS (SELECT ?1 UNION ALL \
    SELECT r.id FROM runs r JOIN tree t ON r.parent_run = t.id WHERE r.trigger IS NULL) SELECT id FROM tree)";

/// What a run and its subtree spent.
pub(super) fn usage(conn: &Connection, run: &str) -> Result<Value, Error> {
    sums(conn, SUBTREE, [run])
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
