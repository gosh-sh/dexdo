// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Backing of `GET /api/v1/inference/stats`: the two aggregate statements, and the pure
// assembly that turns their per-day groups into the response blocks. SQL sums raw
// integers and returns them as text; Rust adds them as `BigUint`. No floats.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use chrono::DateTime;
use chrono::Days;
use chrono::NaiveDate;
use chrono::NaiveTime;
use chrono::Utc;
use dodex_domain::DomainError;
use dodex_domain::InferenceStatsDay;
use dodex_domain::InferenceStatsHistory;
use dodex_domain::InferenceStatsPeriod;
use dodex_domain::InferenceStatsPreviousPeriod;
use dodex_domain::InferenceStatsTotals;
use dodex_domain::InferenceStatsVolume24h;
use num_bigint::BigUint;
use sqlx::postgres::PgArguments;
use sqlx::postgres::PgRow;
use sqlx::query::Query;
use sqlx::Postgres;
use sqlx::Row;
use tracing::warn;

use crate::inference_reconciler::QUANTITY_PRECISION;
use crate::inference_reconciler::QUOTE_TOKEN_TYPE_SHELL;
use crate::postgres_repo::validate_decimal_scale;
use crate::postgres_repo::PostgresReadModelRepository;
use crate::postgres_repo::MAX_DECIMAL_PRECISION;

/// Database-side bound on each statistics statement: a LOCAL `statement_timeout`, so
/// Postgres cancels a stalled statement (a lock wait included) and its connection goes
/// back to the pool. Deliberately below the API cache's 30 s `FLIGHT_TIMEOUT`, which
/// stays as the backstop; cutting only there would drop the future but leave the
/// statement running and its connection held.
pub(crate) const STATS_STATEMENT_TIMEOUT: Duration = Duration::from_secs(25);

/// SQLSTATE `query_canceled`: what a statement cut by `statement_timeout` fails with.
const QUERY_CANCELED: &str = "57014";

/// One SQL group: raw sums for one day at one pair of scales.
pub(crate) struct StatsGroup {
    pub day: NaiveDate,
    pub qty_scale: i32,
    pub quote_scale: Option<i32>,
    pub ticks_raw: String,
    pub quote_raw: String,
    pub fills: i64,
}

/// What the coverage probes found about the previous interval.
pub(crate) struct CoverageEvidence {
    pub execution_before_previous: bool,
    pub unprojected_in_previous: bool,
}

pub(crate) struct HistoryWindow {
    pub as_of: DateTime<Utc>,
    /// UTC date of `as_of`.
    pub last_day: NaiveDate,
    /// `None` for `All`.
    pub selected_from: Option<NaiveDate>,
    /// `None` for `All`.
    pub previous_from: Option<NaiveDate>,
}

/// Running sums at the fixed scale `MAX_DECIMAL_PRECISION`, so groups of any
/// valid scale add exactly; `canonical_decimal` trims the padding back off.
#[derive(Default)]
struct Acc {
    ticks: BigUint,
    quote: BigUint,
    fills: u64,
}

impl Acc {
    fn add(&mut self, other: &Acc) {
        self.ticks += &other.ticks;
        self.quote += &other.quote;
        self.fills += other.fills;
    }

    fn totals(&self) -> InferenceStatsTotals {
        InferenceStatsTotals {
            ticks: canonical_decimal(&self.ticks, MAX_DECIMAL_PRECISION),
            quote: canonical_decimal(&self.quote, MAX_DECIMAL_PRECISION),
            fills: self.fills,
        }
    }
}

pub(crate) fn history_window(
    period: InferenceStatsPeriod,
    as_of_ms: i64,
) -> anyhow::Result<HistoryWindow> {
    let as_of = DateTime::<Utc>::from_timestamp_millis(as_of_ms)
        .with_context(|| format!("as_of_ms {as_of_ms} is out of range"))?;
    let last_day = as_of.date_naive();
    let back = |days: u32| {
        last_day
            .checked_sub_days(Days::new(u64::from(days)))
            .with_context(|| format!("{days} days before {last_day} is out of range"))
    };
    let (selected_from, previous_from) = match period.days() {
        Some(n) => (Some(back(n - 1)?), Some(back(2 * n - 1)?)),
        None => (None, None),
    };
    Ok(HistoryWindow { as_of, last_day, selected_from, previous_from })
}

pub(crate) fn assemble_history(
    period: InferenceStatsPeriod,
    window: &HistoryWindow,
    groups: Vec<StatsGroup>,
    evidence: CoverageEvidence,
) -> anyhow::Result<InferenceStatsHistory> {
    let mut by_day: BTreeMap<NaiveDate, Acc> = BTreeMap::new();
    let mut previous = Acc::default();
    for group in &groups {
        let acc = group_acc(group)?;
        match window.selected_from {
            Some(from) if group.day < from => previous.add(&acc),
            _ => by_day.entry(group.day).or_default().add(&acc),
        }
    }

    let first_day = match window.selected_from {
        Some(from) => Some(from),
        None => by_day.keys().next().copied(),
    };
    let mut per_day = Vec::new();
    let mut selected = Acc::default();
    if let Some(first_day) = first_day {
        for day in first_day.iter_days().take_while(|d| *d <= window.last_day) {
            let acc = by_day.remove(&day).unwrap_or_default();
            selected.add(&acc);
            per_day.push(InferenceStatsDay { day: day.to_string(), totals: acc.totals() });
        }
    }

    let previous_period = match (window.selected_from, window.previous_from) {
        (Some(to), Some(from))
            if period != InferenceStatsPeriod::All
                && evidence.execution_before_previous
                && selected.fills > 0
                && !evidence.unprojected_in_previous =>
        {
            Some(InferenceStatsPreviousPeriod {
                from: from.to_string(),
                to: to.to_string(),
                totals: previous.totals(),
            })
        }
        _ => None,
    };

    Ok(InferenceStatsHistory {
        as_of_ms: window.as_of.timestamp_millis(),
        per_day,
        totals: selected.totals(),
        previous_period,
    })
}

pub(crate) fn assemble_volume(
    as_of_ms: i64,
    groups: Vec<StatsGroup>,
) -> anyhow::Result<InferenceStatsVolume24h> {
    let mut sum = Acc::default();
    for group in &groups {
        sum.add(&group_acc(group)?);
    }
    Ok(InferenceStatsVolume24h { as_of_ms, totals: sum.totals() })
}

/// Render `value / 10^scale` as a canonical decimal: no trailing fractional
/// zeros, no trailing dot, zero is `"0"`.
pub(crate) fn canonical_decimal(value: &BigUint, scale: u32) -> String {
    let digits = value.to_string();
    let scale = scale as usize;
    if scale == 0 {
        return digits;
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, frac) = padded.split_at(padded.len() - scale);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    }
}

fn inconsistent() -> anyhow::Error {
    anyhow!(DomainError::MarketInconsistent)
}

/// Validate one group and bring its sums to the common scale.
fn group_acc(group: &StatsGroup) -> anyhow::Result<Acc> {
    let qty_scale = validate_decimal_scale(group.qty_scale).map_err(|_| {
        warn!(day = %group.day, raw = group.qty_scale, "inference stats qty scale out of range");
        inconsistent()
    })?;
    let quote_scale = match group.quote_scale.map(validate_decimal_scale) {
        Some(Ok(scale)) => scale,
        Some(Err(_)) => {
            warn!(day = %group.day, raw = group.quote_scale, "inference stats quote scale out of range");
            return Err(inconsistent());
        }
        None => {
            warn!(day = %group.day, "inference stats quote scale is null");
            return Err(inconsistent());
        }
    };
    let fills = u64::try_from(group.fills).map_err(|_| {
        warn!(day = %group.day, fills = group.fills, "inference stats fill count is negative");
        inconsistent()
    })?;
    Ok(Acc {
        ticks: rescale(&group.ticks_raw, qty_scale, group.day)?,
        quote: rescale(&group.quote_raw, quote_scale, group.day)?,
        fills,
    })
}

/// Parse a raw non-negative integer sum and rescale it from `scale` to
/// `MAX_DECIMAL_PRECISION`.
fn rescale(raw: &str, scale: u32, day: NaiveDate) -> anyhow::Result<BigUint> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        warn!(%day, raw, "inference stats raw sum is not a non-negative integer");
        return Err(inconsistent());
    }
    let value: BigUint = raw.parse().map_err(|_| inconsistent())?;
    Ok(value * BigUint::from(10u32).pow(MAX_DECIMAL_PRECISION - scale))
}

/// Which statement shape to build. The text is assembled per shape, never a NULL bind
/// inside an `$n is null or ...` disjunction: that can cost the `chain_time` index under
/// a generic plan.
#[derive(Clone, Copy)]
enum StatsKind {
    /// Bounded period: per-day groups from the previous period's start, plus the two
    /// coverage probes. Binds `$1` cutoff, `$2` previous start, `$3` selected start.
    History,
    /// `all`: per-day groups up to the cutoff, no probes. Binds `$1` cutoff.
    AllTime,
    /// Rolling 24 hours: groups without a day. Binds `$1` cutoff, `$2` window start.
    Volume,
}

/// The aggregate statement for `kind`. Binds follow the kind's own, then the quantity
/// precision fallback, the SHELL token type, and (when `scoped`) the book addresses.
fn stats_sql(kind: StatsKind, scoped: bool) -> String {
    let (qty_fallback, quote_fallback) = match kind {
        StatsKind::History => (4, 5),
        StatsKind::AllTime => (2, 3),
        StatsKind::Volume => (3, 4),
    };
    let scope = quote_fallback + 1;
    let mut sql = String::new();
    if let StatsKind::History = kind {
        let probe_book = if scoped {
            format!(" and b.orderbook_address = any(${scope})")
        } else {
            String::new()
        };
        let probe_events =
            if scoped { format!(" and r.src_address = any(${scope})") } else { String::new() };
        sql.push_str(&format!(
            "with ev as (
    select exists(select 1 from inference_trades b
                   where b.chain_time < $2{probe_book}) as execution_before_previous,
           exists(select 1 from raw_events r
                   where r.processed_at is null
                     and (r.created_at_chain is null
                          or (r.created_at_chain >= $2 and r.created_at_chain < $3))
                     and (r.event_type = 'InferenceOrderBook.InferenceFilled'
                          or exists(select 1 from inference_markets k
                                     where k.orderbook_address = r.src_address)){probe_events}
                 ) as unprojected_in_previous
)
"
        ));
    }
    if matches!(kind, StatsKind::Volume) {
        sql.push_str("select ");
    } else {
        sql.push_str("select (t.chain_time at time zone 'UTC')::date as day,\n       ");
    }
    sql.push_str(&format!(
        "coalesce(m.quantity_precision, ${qty_fallback})      as qty_scale,
       rt.decimals                             as quote_scale,
       sum(t.qty)::text                        as ticks_raw,
       sum(t.price * t.qty)::text              as quote_raw,
       count(*)                                as fills"
    ));
    if let StatsKind::History = kind {
        sql.push_str(",\n       ev.execution_before_previous,\n       ev.unprojected_in_previous");
    }
    sql.push_str("\n  from inference_trades t\n");
    if let StatsKind::History = kind {
        sql.push_str(" cross join ev\n");
    }
    sql.push_str(&format!(
        "  left join inference_markets m on m.orderbook_address = t.orderbook_address
  left join ref_tokens rt on rt.token_type = coalesce(m.quote_token_type, ${quote_fallback})
 where "
    ));
    if !matches!(kind, StatsKind::AllTime) {
        sql.push_str("t.chain_time >= $2\n   and ");
    }
    sql.push_str("t.chain_time < $1");
    if scoped {
        sql.push_str(&format!("\n   and t.orderbook_address = any(${scope})"));
    }
    sql.push_str(match kind {
        StatsKind::History => {
            "\n group by 1, 2, 3, ev.execution_before_previous, ev.unprojected_in_previous"
        }
        StatsKind::AllTime => "\n group by 1, 2, 3",
        StatsKind::Volume => "\n group by 1, 2",
    });
    sql
}

/// UTC midnight starting `day`.
fn start_of(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

impl PostgresReadModelRepository {
    /// `inference_stats_history` restricted to the given book addresses, so a test can
    /// assert exact figures over just the rows it seeded — immune to a concurrent writer
    /// elsewhere in the shared test DB. Same statement as production, plus the book filter.
    pub async fn inference_stats_history_for(
        &self,
        period: InferenceStatsPeriod,
        as_of_ms: i64,
        scope: &[String],
    ) -> anyhow::Result<InferenceStatsHistory> {
        self.stats_history(period, as_of_ms, Some(scope), STATS_STATEMENT_TIMEOUT).await
    }

    /// `inference_stats_history` with its database-side bound set to `statement_timeout`
    /// instead of `STATS_STATEMENT_TIMEOUT`, so a test can reach the bound in milliseconds.
    /// Unscoped: the real statement, for a private schema.
    #[doc(hidden)]
    pub async fn inference_stats_history_with_statement_timeout(
        &self,
        period: InferenceStatsPeriod,
        as_of_ms: i64,
        statement_timeout: Duration,
    ) -> anyhow::Result<InferenceStatsHistory> {
        self.stats_history(period, as_of_ms, None, statement_timeout).await
    }

    /// `inference_stats_volume_24h` restricted to the given book addresses.
    pub async fn inference_stats_volume_24h_for(
        &self,
        as_of_ms: i64,
        scope: &[String],
    ) -> anyhow::Result<InferenceStatsVolume24h> {
        self.stats_volume_24h(as_of_ms, Some(scope)).await
    }

    /// Runs one statistics statement in a transaction that first sets the LOCAL
    /// `statement_timeout`. A statement Postgres cancelled at that bound is
    /// `RequestTimeout`; any other error carries `context`.
    async fn fetch_stats_rows(
        &self,
        query: Query<'_, Postgres, PgArguments>,
        statement_timeout: Duration,
        block: &'static str,
        context: &'static str,
    ) -> anyhow::Result<Vec<PgRow>> {
        let mut tx = self.pool().begin().await.context(context)?;
        sqlx::query("select set_config('statement_timeout', $1, true)")
            .bind(statement_timeout.as_millis().to_string())
            .execute(&mut *tx)
            .await
            .context(context)?;
        let rows = match query.fetch_all(&mut *tx).await {
            Ok(rows) => rows,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(QUERY_CANCELED) => {
                warn!(
                    block,
                    bound_ms = statement_timeout.as_millis() as u64,
                    "inference stats statement ran past its database-side bound and was cancelled"
                );
                return Err(anyhow!(DomainError::RequestTimeout));
            }
            Err(err) => return Err(anyhow::Error::new(err).context(context)),
        };
        tx.commit().await.context(context)?;
        Ok(rows)
    }

    pub(crate) async fn stats_history(
        &self,
        period: InferenceStatsPeriod,
        as_of_ms: i64,
        scope: Option<&[String]>,
        statement_timeout: Duration,
    ) -> anyhow::Result<InferenceStatsHistory> {
        let window = history_window(period, as_of_ms)?;
        let bounded = window.selected_from.zip(window.previous_from);
        let kind = if bounded.is_some() { StatsKind::History } else { StatsKind::AllTime };
        let sql = stats_sql(kind, scope.is_some());
        let mut query = sqlx::query(&sql).bind(window.as_of);
        if let Some((selected_from, previous_from)) = bounded {
            query = query.bind(start_of(previous_from)).bind(start_of(selected_from));
        }
        query = query.bind(QUANTITY_PRECISION).bind(QUOTE_TOKEN_TYPE_SHELL);
        if let Some(scope) = scope {
            query = query.bind(scope);
        }
        let rows = self
            .fetch_stats_rows(query, statement_timeout, period.as_str(), "inference stats history")
            .await?;

        // No rows means no executions in range: the evidence is irrelevant then, because
        // `previous_period` is withheld while the selected period has no fills.
        let mut evidence =
            CoverageEvidence { execution_before_previous: false, unprojected_in_previous: false };
        if let (Some(row), true) = (rows.first(), bounded.is_some()) {
            evidence.execution_before_previous = row.try_get("execution_before_previous")?;
            evidence.unprojected_in_previous = row.try_get("unprojected_in_previous")?;
        }
        let groups = rows
            .iter()
            .map(|row| {
                Ok(StatsGroup {
                    day: row.try_get("day")?,
                    qty_scale: row.try_get("qty_scale")?,
                    quote_scale: row.try_get("quote_scale")?,
                    ticks_raw: row.try_get("ticks_raw")?,
                    quote_raw: row.try_get("quote_raw")?,
                    fills: row.try_get("fills")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .context("decode inference stats groups")?;
        assemble_history(period, &window, groups, evidence)
    }

    pub(crate) async fn stats_volume_24h(
        &self,
        as_of_ms: i64,
        scope: Option<&[String]>,
    ) -> anyhow::Result<InferenceStatsVolume24h> {
        let as_of = DateTime::<Utc>::from_timestamp_millis(as_of_ms)
            .with_context(|| format!("as_of_ms {as_of_ms} is out of range"))?;
        let sql = stats_sql(StatsKind::Volume, scope.is_some());
        let mut query = sqlx::query(&sql)
            .bind(as_of)
            .bind(as_of - chrono::Duration::hours(24))
            .bind(QUANTITY_PRECISION)
            .bind(QUOTE_TOKEN_TYPE_SHELL);
        if let Some(scope) = scope {
            query = query.bind(scope);
        }
        let rows = self
            .fetch_stats_rows(
                query,
                STATS_STATEMENT_TIMEOUT,
                "volume24h",
                "inference stats volume 24h",
            )
            .await?;
        // The day only labels a group in diagnostics; the 24 h block has no day axis.
        let day = as_of.date_naive();
        let groups = rows
            .iter()
            .map(|row| {
                Ok(StatsGroup {
                    day,
                    qty_scale: row.try_get("qty_scale")?,
                    quote_scale: row.try_get("quote_scale")?,
                    ticks_raw: row.try_get("ticks_raw")?,
                    quote_raw: row.try_get("quote_raw")?,
                    fills: row.try_get("fills")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .context("decode inference stats volume groups")?;
        assemble_volume(as_of_ms, groups)
    }
}

#[cfg(test)]
mod tests {
    use dodex_domain::DomainError;
    use dodex_domain::InferenceStatsPeriod;
    use dodex_domain::InferenceStatsTotals;
    use num_bigint::BigUint;

    use super::*;

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }
    fn d(s: &str) -> chrono::NaiveDate {
        s.parse().unwrap()
    }
    fn g(day: &str, ticks: &str, quote: &str, fills: i64) -> StatsGroup {
        StatsGroup {
            day: d(day),
            qty_scale: 0,
            quote_scale: Some(9),
            ticks_raw: ticks.into(),
            quote_raw: quote.into(),
            fills,
        }
    }
    fn ev(before: bool, unprojected: bool) -> CoverageEvidence {
        CoverageEvidence { execution_before_previous: before, unprojected_in_previous: unprojected }
    }
    fn big(s: &str) -> BigUint {
        s.parse().unwrap()
    }

    #[test]
    fn canonical_decimal_trims_trailing_zeros() {
        assert_eq!(canonical_decimal(&big("108000000000"), 9), "108");
        assert_eq!(canonical_decimal(&big("1500000000"), 9), "1.5");
        assert_eq!(canonical_decimal(&big("1"), 9), "0.000000001");
        assert_eq!(canonical_decimal(&big("0"), 9), "0");
        assert_eq!(canonical_decimal(&big("42"), 0), "42");
        assert_eq!(canonical_decimal(&big("1200"), 2), "12");
    }

    #[test]
    fn window_14d_and_30d_and_all() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T12:00:00Z")).unwrap();
        assert_eq!(
            (w.last_day, w.selected_from, w.previous_from),
            (d("2026-10-23"), Some(d("2026-10-10")), Some(d("2026-09-26")))
        );
        let w = history_window(InferenceStatsPeriod::Days30, ms("2026-10-23T12:00:00Z")).unwrap();
        assert_eq!(
            (w.selected_from, w.previous_from),
            (Some(d("2026-09-24")), Some(d("2026-08-25")))
        );
        let w = history_window(InferenceStatsPeriod::All, ms("2026-10-23T12:00:00Z")).unwrap();
        assert_eq!((w.selected_from, w.previous_from), (None, None));
    }

    #[test]
    fn history_14d_fills_zero_days_and_splits_previous() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T12:00:00Z")).unwrap();
        let h = assemble_history(
            InferenceStatsPeriod::Days14,
            &w,
            vec![
                g("2026-09-26", "2", "4000000000", 1),
                g("2026-10-09", "3", "3000000000", 1),
                g("2026-10-10", "3", "4500000003", 1),
                g("2026-10-23", "9", "108000000000", 3),
            ],
            ev(true, false),
        )
        .unwrap();
        assert_eq!(h.as_of_ms, ms("2026-10-23T12:00:00Z"));
        assert_eq!(h.per_day.len(), 14);
        assert_eq!(h.per_day[0].day, "2026-10-10");
        assert_eq!(
            h.per_day[0].totals,
            InferenceStatsTotals { ticks: "3".into(), quote: "4.500000003".into(), fills: 1 }
        );
        assert_eq!(
            h.per_day[1].totals,
            InferenceStatsTotals { ticks: "0".into(), quote: "0".into(), fills: 0 }
        );
        assert_eq!(h.per_day[13].day, "2026-10-23");
        assert_eq!(
            h.per_day[13].totals,
            InferenceStatsTotals { ticks: "9".into(), quote: "108".into(), fills: 3 }
        );
        assert_eq!(
            h.totals,
            InferenceStatsTotals { ticks: "12".into(), quote: "112.500000003".into(), fills: 4 }
        );
        let p = h.previous_period.unwrap();
        assert_eq!((p.from.as_str(), p.to.as_str()), ("2026-09-26", "2026-10-10"));
        assert_eq!(
            p.totals,
            InferenceStatsTotals { ticks: "5".into(), quote: "7".into(), fills: 2 }
        );
    }

    #[test]
    fn previous_period_needs_both_brackets_and_no_unprojected_event() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T12:00:00Z")).unwrap();
        let sel = || vec![g("2026-10-20", "1", "1000000000", 1)];
        let prev = |groups, e| {
            assemble_history(InferenceStatsPeriod::Days14, &w, groups, e).unwrap().previous_period
        };
        // covered and empty -> object with zero totals
        assert_eq!(
            prev(sel(), ev(true, false)).unwrap().totals,
            InferenceStatsTotals { ticks: "0".into(), quote: "0".into(), fills: 0 }
        );
        assert!(prev(sel(), ev(false, false)).is_none(), "no execution before the interval");
        assert!(
            prev(vec![g("2026-10-01", "1", "1000000000", 1)], ev(true, false)).is_none(),
            "no execution in the selected period"
        );
        assert!(prev(sel(), ev(true, true)).is_none(), "an unprojected event inside the interval");
    }

    #[test]
    fn all_spans_first_execution_to_today_and_has_no_previous() {
        let w = history_window(InferenceStatsPeriod::All, ms("2026-10-05T09:00:00Z")).unwrap();
        let h = assemble_history(
            InferenceStatsPeriod::All,
            &w,
            vec![g("2026-10-01", "1", "1000000000", 1), g("2026-10-03", "2", "2000000000", 2)],
            ev(true, false),
        )
        .unwrap();
        let days: Vec<&str> = h.per_day.iter().map(|x| x.day.as_str()).collect();
        assert_eq!(days, ["2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05"]);
        assert_eq!(h.per_day[4].totals.fills, 0);
        assert_eq!(
            h.totals,
            InferenceStatsTotals { ticks: "3".into(), quote: "3".into(), fills: 3 }
        );
        assert!(h.previous_period.is_none());

        let empty =
            assemble_history(InferenceStatsPeriod::All, &w, vec![], ev(false, false)).unwrap();
        assert!(empty.per_day.is_empty());
        assert_eq!(
            empty.totals,
            InferenceStatsTotals { ticks: "0".into(), quote: "0".into(), fills: 0 }
        );
    }

    #[test]
    fn as_of_at_utc_midnight_opens_a_zero_day() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T00:00:00Z")).unwrap();
        let h = assemble_history(
            InferenceStatsPeriod::Days14,
            &w,
            vec![g("2026-10-22", "1", "1000000000", 1)],
            ev(false, false),
        )
        .unwrap();
        assert_eq!(h.per_day[13].day, "2026-10-23");
        assert_eq!(h.per_day[13].totals.fills, 0);
        assert_eq!(h.per_day[12].totals.fills, 1);
    }

    #[test]
    fn groups_with_different_scales_add_exactly() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T12:00:00Z")).unwrap();
        let mut a = g("2026-10-20", "5", "1000000000", 1); // 5 ticks, 1 quote (scale 9)
        let mut b = g("2026-10-20", "250", "2500000", 1); // 2.5 ticks (scale 2), 2.5 quote (scale 6)
        a.qty_scale = 0;
        b.qty_scale = 2;
        b.quote_scale = Some(6);
        let h = assemble_history(InferenceStatsPeriod::Days14, &w, vec![a, b], ev(false, false))
            .unwrap();
        assert_eq!(
            h.per_day[10].totals,
            InferenceStatsTotals { ticks: "7.5".into(), quote: "3.5".into(), fills: 2 }
        );
    }

    #[test]
    fn bad_scale_or_raw_value_is_market_inconsistent() {
        let w = history_window(InferenceStatsPeriod::Days14, ms("2026-10-23T12:00:00Z")).unwrap();
        for bad in [
            StatsGroup { qty_scale: -1, ..g("2026-10-20", "1", "1", 1) },
            StatsGroup { qty_scale: 39, ..g("2026-10-20", "1", "1", 1) },
            StatsGroup { quote_scale: None, ..g("2026-10-20", "1", "1", 1) },
            g("2026-10-20", "-1", "1", 1),
        ] {
            let err =
                assemble_history(InferenceStatsPeriod::Days14, &w, vec![bad], ev(false, false))
                    .unwrap_err();
            assert_eq!(err.downcast_ref::<DomainError>(), Some(&DomainError::MarketInconsistent));
        }
    }

    #[test]
    fn volume_sums_all_groups() {
        let v = assemble_volume(
            ms("2026-10-23T12:00:00Z"),
            vec![g("2026-10-22", "1", "1000000000", 1), g("2026-10-23", "2", "2500000000", 2)],
        )
        .unwrap();
        assert_eq!(v.as_of_ms, ms("2026-10-23T12:00:00Z"));
        assert_eq!(
            v.totals,
            InferenceStatsTotals { ticks: "3".into(), quote: "3.5".into(), fills: 3 }
        );
    }
}
