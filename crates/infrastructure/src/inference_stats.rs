// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Pure assembly of `GET /api/v1/inference/stats`: turns the per-day SQL
// aggregate groups into the response blocks. No I/O; exact `BigUint`
// arithmetic only.

use std::collections::BTreeMap;

use anyhow::anyhow;
use anyhow::Context;
use chrono::DateTime;
use chrono::Days;
use chrono::NaiveDate;
use chrono::Utc;
use dodex_domain::DomainError;
use dodex_domain::InferenceStatsDay;
use dodex_domain::InferenceStatsHistory;
use dodex_domain::InferenceStatsPeriod;
use dodex_domain::InferenceStatsPreviousPeriod;
use dodex_domain::InferenceStatsTotals;
use dodex_domain::InferenceStatsVolume24h;
use num_bigint::BigUint;
use tracing::warn;

use crate::postgres_repo::validate_decimal_scale;
use crate::postgres_repo::MAX_DECIMAL_PRECISION;

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
