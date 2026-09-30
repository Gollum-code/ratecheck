//! 分析：接近限额、速率突增、成本趋势/预测。
//!
//! 模型：每个指标存的是「累计用量」（或余额，`invert=true`）。
//! - 用量 = value（invert 时为 limit - value）
//! - 用量% = 用量 / 限额
//! - 用最近点数做最小二乘线性拟合 → 日均速率 → 月底预测 → 预计超限日期

use crate::config::{Level, Service};
use crate::store::UsagePoint;
use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct MetricStatus {
    pub service: String,
    pub metric: String,
    pub unit: String,
    pub current: f64,
    pub limit: Option<f64>,
    pub percent: Option<f64>,
    pub remaining: Option<f64>,
    pub level: Level,
    pub invert: bool,
    pub has_data: bool,
    /// 日均用量（预测）。
    pub daily_rate: Option<f64>,
    /// 本月结束时的预测用量。
    pub projected: Option<f64>,
    /// 预计超限日期。
    pub overage_date: Option<DateTime<Utc>>,
    /// 成本（用量 × 单价）。
    pub cost_usd: Option<f64>,
    pub projected_cost_usd: Option<f64>,
    /// 供图表使用的用量序列（仅值）。
    #[serde(skip_serializing)]
    pub usage_series: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub kind: String,
    pub metrics: Vec<MetricStatus>,
    pub worst: Level,
    pub total_cost_usd: f64,
}

/// 当前自然月的起止时刻（按本机时区确定年月）。
pub fn month_bounds(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let local = now.with_timezone(&Local);
    let y = local.year();
    let m = local.month();
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let start = NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
        .unwrap_or(now);
    let end = NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
        .unwrap_or(now);
    (start, end)
}

/// 最小二乘线性拟合，返回 (slope 单位/秒, intercept)。
pub fn linear_fit(xy: &[(f64, f64)]) -> (f64, f64) {
    let n = xy.len() as f64;
    if n < 2.0 {
        return (0.0, xy.first().map(|p| p.1).unwrap_or(0.0));
    }
    let mx = xy.iter().map(|p| p.0).sum::<f64>() / n;
    let my = xy.iter().map(|p| p.1).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xy {
        num += (x - mx) * (y - my);
        den += (x - mx).powi(2);
    }
    if den.abs() < 1e-12 {
        return (0.0, my);
    }
    let slope = num / den;
    (slope, my - slope * mx)
}

/// 分析一个服务：输入该服务最新各指标值 + 本周期内的全部历史点。
pub fn analyze(
    svc: &Service,
    latest: &[UsagePoint],
    series: &[UsagePoint],
    now: DateTime<Utc>,
) -> Vec<MetricStatus> {
    let (start, end) = month_bounds(now);
    let mut by_metric: std::collections::BTreeMap<&str, Vec<&UsagePoint>> =
        std::collections::BTreeMap::new();
    for p in series {
        by_metric.entry(p.metric.as_str()).or_default().push(p);
    }

    svc.metrics()
        .into_iter()
        .map(|metric| {
            let lat = latest.iter().find(|p| p.metric == metric);
            let pts: Vec<&UsagePoint> = by_metric
                .get(metric.as_str())
                .cloned()
                .unwrap_or_default();
            analyze_metric(svc, &metric, lat, &pts, now, start, end)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn analyze_metric(
    svc: &Service,
    metric: &str,
    latest: Option<&UsagePoint>,
    pts: &[&UsagePoint],
    now: DateTime<Utc>,
    _start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> MetricStatus {
    let limit = svc.limit(metric).map(|l| l.limit);
    let invert = svc.invert;
    let unit = svc.unit_of(metric);
    let cost_rate = svc.cost_rate(metric).map(|c| c.per_unit);

    let to_usage = |v: f64| -> f64 {
        if invert {
            match limit {
                Some(l) => (l - v).max(0.0),
                None => v,
            }
        } else {
            v
        }
    };

    let current = latest.map(|p| to_usage(p.value));
    let has_data = latest.is_some() || !pts.is_empty();

    // 拟合：用周期内的用量序列（时间秒 - 首点）。
    let mut xy: Vec<(f64, f64)> = Vec::new();
    let mut usage_series: Vec<f64> = Vec::new();
    if let Some(first) = pts.first() {
        let t0 = first.at.timestamp() as f64;
        for p in pts {
            if p.at < now {
                let u = to_usage(p.value);
                usage_series.push(u);
                xy.push(((p.at.timestamp() as f64) - t0, u));
            }
        }
    } else if let Some(c) = current {
        usage_series.push(c);
        xy.push((0.0, c));
    }

    let (slope, _intercept) = linear_fit(&xy);
    let daily_rate = Some(slope * 86400.0);
    let seconds_remaining = (end - now).num_seconds().max(0) as f64;
    let projected = if !xy.is_empty() {
        let last_usage = xy.last().map(|p| p.1).unwrap_or(0.0);
        Some((last_usage + slope * seconds_remaining).max(0.0))
    } else {
        None
    };

    // 预计超限日期：以最新一次采集点为基准，按当前速率外推。
    let overage_date = match (limit, slope, pts.last(), current) {
        (Some(l), s, Some(last_p), Some(cu)) if s > 0.0 && cu < l => {
            let dt = ((l - cu) / s) as i64;
            Some(last_p.at + chrono::Duration::seconds(dt))
        }
        _ => None,
    };

    let percent = limit.map(|l| if l > 0.0 { current.unwrap_or(0.0) / l * 100.0 } else { 0.0 });
    let remaining = limit.map(|l| current.map(|c| l - c).unwrap_or(l));
    let level = match (has_data, percent, limit) {
        (true, Some(p), Some(_)) => svc.threshold(metric).classify(p),
        (true, _, _) => Level::Ok,
        (false, _, _) => Level::NoData,
    };

    let cost_usd = cost_rate.zip(current).map(|(r, c)| c * r);
    let projected_cost_usd = cost_rate
        .zip(projected)
        .map(|(r, p)| (p - current.unwrap_or(0.0)) * r + cost_usd.unwrap_or(0.0));

    MetricStatus {
        service: svc.name.clone(),
        metric: metric.to_string(),
        unit,
        current: current.unwrap_or(0.0),
        limit,
        percent,
        remaining,
        level,
        invert,
        has_data,
        daily_rate,
        projected,
        overage_date,
        cost_usd,
        projected_cost_usd,
        usage_series,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Limit, Service, ServiceKind, Threshold};
    use chrono::{Datelike, Duration};

    fn svc(name: &str) -> Service {
        Service {
            name: name.to_string(),
            kind: ServiceKind::Manual,
            base_url: None,
            url: None,
            api_key: None,
            api_key_env: None,
            header_name: None,
            field: None,
            metric: None,
            scale: 1.0,
            invert: false,
            unit: None,
            limits: vec![Limit {
                metric: "tokens".to_string(),
                limit: 1000.0,
                unit: Some("tokens".to_string()),
            }],
            cost_rates: vec![],
            thresholds: vec![Threshold {
                metric: "tokens".to_string(),
                warn: Some(80.0),
                critical: Some(95.0),
            }],
            webhook: None,
        }
    }

    fn pt(name: &str, metric: &str, value: f64, at: DateTime<Utc>) -> UsagePoint {
        UsagePoint {
            service: name.to_string(),
            metric: metric.to_string(),
            value,
            at,
        }
    }

    #[test]
    fn linear_fit_slope() {
        // y = 2x + 1
        let xy: Vec<(f64, f64)> = (0..5).map(|i| (i as f64, 2.0 * i as f64 + 1.0)).collect();
        let (slope, intercept) = linear_fit(&xy);
        assert!((slope - 2.0).abs() < 1e-9);
        assert!((intercept - 1.0).abs() < 1e-9);
    }

    #[test]
    fn flat_series_no_crash() {
        let xy: Vec<(f64, f64)> = vec![(0.0, 5.0), (10.0, 5.0)];
        let (slope, _) = linear_fit(&xy);
        assert!(slope.abs() < 1e-12);
    }

    #[test]
    fn analyze_computes_percent_level_projection() {
        let s = svc("openai");
        let now = Utc::now();
        let (start, _end) = month_bounds(now);
        let mut series = Vec::new();
        for i in 0..10 {
            series.push(pt(
                "openai",
                "tokens",
                50.0 * (i as f64) + 10.0,
                start + Duration::days(i),
            ));
        }
        let latest = vec![pt("openai", "tokens", 460.0, now - Duration::hours(1))];
        let res = analyze(&s, &latest, &series, now);
        assert_eq!(res.len(), 1);
        let m = &res[0];
        assert!(m.has_data);
        assert!((m.percent.unwrap() - 46.0).abs() < 1e-6);
        assert_eq!(m.level, Level::Ok);
        assert!(m.daily_rate.unwrap() > 0.0);
        assert!(m.projected.unwrap() > 460.0);
    }

    #[test]
    fn analyze_invert_balance() {
        let mut s = svc("deepseek");
        s.invert = true;
        let now = Utc::now();
        let latest = vec![pt("deepseek", "tokens", 800.0, now)];
        let res = analyze(&s, &latest, &[], now);
        let m = &res[0];
        // 余额剩 800，用了 200/1000 = 20%
        assert!((m.percent.unwrap() - 20.0).abs() < 1e-6);
        assert_eq!(m.current, 200.0);
        assert_eq!(m.level, Level::Ok);
    }

    #[test]
    fn analyze_exceeds_limit() {
        let s = svc("openai");
        let now = Utc::now();
        let latest = vec![pt("openai", "tokens", 1200.0, now)];
        let res = analyze(&s, &latest, &[], now);
        let m = &res[0];
        assert_eq!(m.level, Level::Exceeded);
        assert!(m.percent.unwrap() >= 100.0);
    }

    #[test]
    fn month_bounds_cover_now() {
        let now = Utc::now();
        let (start, end) = month_bounds(now);
        assert!(start <= now && now < end);
        assert!(start.day() <= 1);
    }
}
