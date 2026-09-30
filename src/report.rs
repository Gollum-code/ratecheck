//! 报告：HTML 趋势看板 + CLI/JSON 输出 + 折线图。

use crate::analyze::{MetricStatus, ServiceStatus};
use crate::config::Level;
use crate::store::UsagePoint;
use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct DailyCost {
    pub date: String,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceReport {
    pub name: String,
    pub kind: String,
    pub worst: String,
    pub total_cost_usd: f64,
    pub metrics: Vec<MetricStatus>,
    /// 近 N 天每日成本。
    pub daily_cost: Vec<DailyCost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub generated_at: String,
    pub now: String,
    pub days: i64,
    pub service_count: usize,
    pub alert_count: usize,
    pub total_cost_usd: f64,
    pub services: Vec<ServiceReport>,
}

impl Report {
    pub fn worst(&self) -> Level {
        self.services
            .iter()
            .flat_map(|s| s.metrics.iter().map(|m| m.level))
            .max()
            .unwrap_or(Level::NoData)
    }
}

/// 数字格式化：大数用 K/M 后缀，钱保留两位。
pub fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if a >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if a >= 10_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else if (v.fract().abs() < 1e-9) && a < 1e15 {
        format!("{:.0}", v)
    } else {
        format!("{:.2}", v)
    }
}

pub fn is_money(unit: &str) -> bool {
    let u = unit.to_ascii_lowercase();
    u.contains("usd") || u.contains("$") || u == "cny" || u.contains("元")
}

/// 带单位的数值显示。
pub fn fmt_value(v: f64, unit: &str) -> String {
    let num = fmt_num(v);
    if is_money(unit) {
        let symbol = if unit.eq_ignore_ascii_case("cny") || unit.contains("元") {
            "¥"
        } else {
            "$"
        };
        format!("{symbol}{num}")
    } else if unit.is_empty() {
        num
    } else {
        format!("{num} {unit}")
    }
}

/// 相邻两日每日成本增量（按 Local 时区分组，取每日最后一个值做差）。
pub fn daily_cost(
    rate_per_unit: Option<f64>,
    invert: bool,
    limit: Option<f64>,
    points: &[UsagePoint],
    now: DateTime<Utc>,
    days: i64,
) -> Vec<DailyCost> {
    let Some(rate) = rate_per_unit else {
        return Vec::new();
    };
    if points.is_empty() {
        return Vec::new();
    }

    let first_of_window = (now - chrono::Duration::days(days)).date_naive();
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

    // 按日分组，取每日最后一个用量。
    let mut by_day: BTreeMap<chrono::NaiveDate, f64> = BTreeMap::new();
    for p in points {
        let d = p.at.with_timezone(&Local).date_naive();
        if d >= first_of_window {
            by_day.insert(d, to_usage(p.value));
        }
    }

    let mut out = Vec::new();
    let mut prev: Option<f64> = None;
    for (d, cum) in by_day {
        let delta = match prev {
            None => cum, // 窗口起点无基线，把首日累计直接计入该日
            Some(p) => (cum - p).max(0.0),
        };
        prev = Some(cum);
        if delta > 0.0 || !out.is_empty() {
            out.push(DailyCost {
                date: d.format("%m-%d").to_string(),
                cost: delta * rate,
            });
        }
    }
    out
}

/// 由服务分析结果生成最终报告。
pub fn build(
    generated_at: DateTime<Local>,
    days: i64,
    services: Vec<(ServiceStatus, Vec<DailyCost>)>,
    alert_count: usize,
) -> Report {
    let mut total = 0.0;
    let service_reports: Vec<ServiceReport> = services
        .into_iter()
        .map(|(s, daily_cost)| {
            let cost = s
                .metrics
                .iter()
                .filter_map(|m| m.cost_usd)
                .sum::<f64>();
            total += cost;
            ServiceReport {
                worst: s.worst.as_str().to_string(),
                total_cost_usd: cost,
                daily_cost,
                metrics: s.metrics,
                name: s.name.clone(),
                kind: s.kind.clone(),
            }
        })
        .collect();
    Report {
        generated_at: generated_at
            .format("%Y-%m-%d %H:%M:%S %z")
            .to_string(),
        now: Utc::now().to_rfc3339(),
        days,
        service_count: service_reports.len(),
        alert_count,
        total_cost_usd: total,
        services: service_reports,
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn level_css(l: Level) -> &'static str {
    match l {
        Level::NoData => "nodata",
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Critical => "critical",
        Level::Exceeded => "exceeded",
    }
}

fn badge(l: Level) -> String {
    format!(
        "<span class=\"badge {css}\">{}</span>",
        esc(l.label()),
        css = level_css(l)
    )
}

/// SVG 折线图（mini sparkline）。
pub fn sparkline(values: &[f64], width: u32, height: u32) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(1e-9);
    let n = values.len();
    let mut pts = String::new();
    for (i, v) in values.iter().enumerate() {
        let x = i as f64 * width as f64 / (n - 1) as f64;
        let y = height as f64 - (v - min) / range * height as f64;
        pts.push_str(&format!("{x:.1},{y:.1} "));
    }
    format!(
        "<svg class=\"spark\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" preserveAspectRatio=\"none\"><polyline points=\"{pts}\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\"/></svg>"
    )
}

fn bar_html(m: &MetricStatus) -> String {
    let pct = m.percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let css = level_css(m.level);
    format!(
        "<div class=\"bar\"><span class=\"fill {css}\" style=\"width:{pct:.1}%\"></span></div><span class=\"pct\">{pct:.1}%</span>"
    )
}

pub fn render_html(report: &Report) -> String {
    let worst = report.worst();
    let mut body = String::new();
    for s in &report.services {
        let rows: String = s
            .metrics
            .iter()
            .map(|m| {
                let limit = m
                    .limit
                    .map(|l| fmt_value(l, &m.unit))
                    .unwrap_or_else(|| "—".to_string());
                let overage = m
                    .overage_date
                    .map(|d| {
                        d.with_timezone(&Local)
                            .format("%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|| "—".to_string());
                let projected = m
                    .projected
                    .map(|p| fmt_value(p, &m.unit))
                    .unwrap_or_else(|| "—".to_string());
                let rate = m
                    .daily_rate
                    .map(|r| fmt_value(r, &m.unit))
                    .unwrap_or_else(|| "—".to_string());
                format!(
                    r#"<tr>
                        <td class="metric"><strong>{metric}</strong>{spark}</td>
                        <td>{current}</td>
                        <td>{limit}</td>
                        <td class="barcell">{bar}</td>
                        <td>{rate}</td>
                        <td>{projected}</td>
                        <td>{overage}</td>
                        <td>{cost}</td>
                        <td>{badge}</td>
                    </tr>"#,
                    metric = esc(&m.metric),
                    spark = sparkline(&m.usage_series, 120, 24),
                    current = fmt_value(m.current, &m.unit),
                    limit = limit,
                    bar = bar_html(m),
                    rate = rate,
                    projected = projected,
                    overage = overage,
                    cost = m
                        .cost_usd
                        .map(|c| format!("${}", fmt_num(c)))
                        .unwrap_or_else(|| "—".to_string()),
                    badge = badge(m.level),
                )
            })
            .collect::<String>();

        let cost_chart = sparkline(
            &s.daily_cost.iter().map(|d| d.cost).collect::<Vec<_>>(),
            320,
            60,
        );
        body.push_str(&format!(
            r#"<section class="svc">
                <h2>{name} <span class="kind">{kind}</span> {worst_badge} <span class="cost">本月成本 ${cost}</span></h2>
                <table><thead><tr>
                    <th>指标 / 趋势</th><th>当前</th><th>限额</th><th>用量</th>
                    <th>日均</th><th>月底预测</th><th>预计超限</th><th>成本</th><th>状态</th>
                </tr></thead><tbody>
                {rows}
                </tbody></table>
                <div class="costline"><div class="costlabel">近 {days} 天每日成本</div>{cost_chart}</div>
            </section>"#,
            name = esc(&s.name),
            kind = esc(&s.kind),
            worst_badge = badge(level_from_str(&s.worst)),
            cost = fmt_num(s.total_cost_usd),
            rows = rows,
            days = report.days,
            cost_chart = cost_chart,
        ));
    }

    format!(
        r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ratecheck 用量报告</title>
<style>
:root {{ --ok:#22c55e; --warn:#f59e0b; --critical:#ef4444; --exceeded:#7f1d1d; --nodata:#94a3b8; --bg:#0f172a; --card:#1e293b; --line:#334155; --fg:#e2e8f0; }}
* {{ box-sizing:border-box; margin:0; padding:0; }}
body {{ background:var(--bg); color:var(--fg); font:14px/1.5 "Segoe UI",system-ui,sans-serif; padding:24px; }}
header {{ display:flex; align-items:baseline; gap:16px; margin-bottom:20px; }}
h1 {{ font-size:22px; letter-spacing:.5px; }}
.sub {{ color:#94a3b8; font-size:12px; }}
.cards {{ display:flex; gap:12px; margin-bottom:20px; }}
.card {{ background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px 18px; min-width:130px; }}
.card .label {{ color:#94a3b8; font-size:11px; text-transform:uppercase; letter-spacing:1px; }}
.card .val {{ font-size:22px; font-weight:600; margin-top:4px; }}
.svc {{ background:var(--card); border:1px solid var(--line); border-radius:12px; padding:18px; margin-bottom:16px; }}
.svc h2 {{ font-size:16px; margin-bottom:12px; display:flex; align-items:center; gap:10px; }}
.kind {{ font-size:11px; color:#94a3b8; font-weight:400; }}
.cost {{ font-size:12px; color:#94a3b8; margin-left:auto; }}
table {{ width:100%; border-collapse:collapse; font-size:13px; }}
th {{ text-align:left; color:#94a3b8; font-weight:500; font-size:11px; text-transform:uppercase; letter-spacing:.5px; padding:6px 8px; border-bottom:1px solid var(--line); }}
td {{ padding:8px; border-bottom:1px solid var(--line); white-space:nowrap; }}
td.metric {{ white-space:normal; }}
td.metric .spark {{ display:block; margin-top:4px; color:#60a5fa; }}
.barcell {{ min-width:180px; }}
.bar {{ display:inline-block; vertical-align:middle; width:120px; height:10px; background:#0f172a; border-radius:5px; overflow:hidden; margin-right:8px; }}
.fill {{ display:block; height:100%; border-radius:5px; }}
.fill.ok {{ background:var(--ok); }} .fill.warn {{ background:var(--warn); }} .fill.critical {{ background:var(--critical); }} .fill.exceeded {{ background:var(--exceeded); }}
.pct {{ font-size:12px; color:#cbd5e1; }}
.badge {{ padding:2px 10px; border-radius:99px; font-size:11px; font-weight:600; }}
.badge.ok {{ background:rgba(34,197,94,.15); color:var(--ok); }}
.badge.warn {{ background:rgba(245,158,11,.15); color:var(--warn); }}
.badge.critical {{ background:rgba(239,68,68,.15); color:var(--critical); }}
.badge.exceeded {{ background:rgba(127,29,29,.6); color:#fff; }}
.badge.nodata {{ background:rgba(148,163,184,.15); color:var(--nodata); }}
.costline {{ margin-top:12px; color:#60a5fa; }}
.costlabel {{ font-size:12px; color:#94a3b8; margin-bottom:4px; }}
.costline svg {{ max-width:100%; height:auto; }}
footer {{ margin-top:24px; color:#64748b; font-size:12px; }}
</style>
</head>
<body>
<header>
  <h1>ratecheck</h1>
  <span class="sub">生成于 {generated}</span>
</header>
<div class="cards">
  <div class="card"><div class="label">服务</div><div class="val">{sc}</div></div>
  <div class="card"><div class="label">本月成本(估)</div><div class="val">${cost}</div></div>
  <div class="card"><div class="label">总状态</div><div class="val"><span class="badge {worst_css}">{worst_label}</span></div></div>
  <div class="card"><div class="label">告警</div><div class="val">{ac}</div></div>
</div>
{body}
<footer>ratecheck v0.1 — 成本为「单价 × 用量」估算，非账单。数据源：{src}</footer>
</body>
</html>"#,
        generated = esc(&report.generated_at),
        sc = report.service_count,
        cost = fmt_num(report.total_cost_usd),
        worst_css = level_css(worst),
        worst_label = esc(worst.label()),
        ac = report.alert_count,
        body = body,
        src = "services.yaml + 各服务用量 API / 本地记录",
    )
}

fn level_from_str(s: &str) -> Level {
    match s {
        "exceeded" => Level::Exceeded,
        "critical" => Level::Critical,
        "warn" => Level::Warn,
        "ok" => Level::Ok,
        _ => Level::NoData,
    }
}

/// CLI 表格文本。
pub fn render_cli(report: &Report) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let header = vec![
        "服务",
        "指标",
        "当前",
        "限额",
        "用量%",
        "级别",
        "日均",
        "月底预测",
        "预计超限",
        "成本$",
    ];
    for s in &report.services {
        for m in &s.metrics {
            rows.push(vec![
                s.name.clone(),
                m.metric.clone(),
                fmt_value(m.current, &m.unit),
                m.limit
                    .map(|l| fmt_value(l, &m.unit))
                    .unwrap_or_else(|| "—".to_string()),
                m.percent
                    .map(|p| format!("{p:.1}%"))
                    .unwrap_or_else(|| "—".to_string()),
                m.level.label().to_string(),
                m.daily_rate
                    .map(|r| fmt_value(r, &m.unit))
                    .unwrap_or_else(|| "—".to_string()),
                m.projected
                    .map(|p| fmt_value(p, &m.unit))
                    .unwrap_or_else(|| "—".to_string()),
                m.overage_date
                    .map(|d| d.with_timezone(&Local).format("%m-%d").to_string())
                    .unwrap_or_else(|| "—".to_string()),
                m.cost_usd.map(fmt_num).unwrap_or_else(|| "—".to_string()),
            ]);
        }
    }

    // 列宽对齐
    let cols = header.len();
    let mut widths = vec![0usize; cols];
    for col in 0..cols {
        widths[col] = header[col].chars().count();
    }
    for r in &rows {
        for (col, cell) in r.iter().enumerate() {
            widths[col] = widths[col].max(cell.chars().count());
        }
    }
    let pad = |s: &str, w: usize| {
        let n = s.chars().count();
        if n >= w {
            s.to_string()
        } else {
            format!("{s}{}", " ".repeat(w - n))
        }
    };

    let mut out = String::new();
    out.push_str(&header
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, widths[i]))
        .collect::<Vec<_>>()
        .join("  "));
    out.push('\n');
    out.push_str(&"-".repeat(widths.iter().sum::<usize>() + 2 * (cols - 1)));
    out.push('\n');
    for r in &rows {
        out.push_str(
            &r.iter()
                .enumerate()
                .map(|(i, c)| pad(c, widths[i]))
                .collect::<Vec<_>>()
                .join("  "),
        );
        out.push('\n');
    }
    out
}

pub fn to_json(report: &Report) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_formatting() {
        assert_eq!(fmt_num(12345.6), "12.3K");
        assert_eq!(fmt_num(100.0), "100");
        assert_eq!(fmt_num(123.456), "123.46");
        assert_eq!(fmt_num(2_500_000.0), "2.50M");
    }

    #[test]
    fn money_detection_and_value() {
        assert!(is_money("USD"));
        assert!(!is_money("tokens"));
        assert_eq!(fmt_value(12.3, "USD"), "$12.30");
        assert_eq!(fmt_value(12.3, "tokens"), "12.30 tokens");
    }

    #[test]
    fn sparkline_render() {
        let s = sparkline(&[1.0, 2.0, 3.0], 100, 30);
        assert!(s.contains("<svg"));
        assert!(s.contains("polyline"));
        assert!(sparkline(&[1.0], 100, 30).is_empty());
    }

    #[test]
    fn html_escapes() {
        let r = esc("<script>alert('x')</script> & \"q\"");
        assert!(!r.contains('<'));
        assert!(r.contains("&lt;script&gt;"));
    }

    #[test]
    fn daily_cost_increments() {
        use chrono::{Duration, TimeZone, Utc};
        let now = Utc::now();
        let start = now - Duration::days(3);
        let mk = |v: f64, t: DateTime<Utc>| UsagePoint {
            service: "s".into(),
            metric: "m".into(),
            value: v,
            at: t,
        };
        let pts = vec![
            mk(50.0, start),
            mk(80.0, start + Duration::days(1)),
            mk(120.0, start + Duration::days(2)),
            mk(170.0, now),
        ];
        let dc = daily_cost(Some(0.01), false, None, &pts, now, 4);
        assert_eq!(dc.len(), 4);
        // 首日 50，第二日 30，第三日 40，第四日 50 → 成本 = 值差 × 0.01
        assert!((dc[0].cost - 0.5).abs() < 1e-9);
        assert!((dc[1].cost - 0.3).abs() < 1e-9);
    }
}