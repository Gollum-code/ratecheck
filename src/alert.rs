//! 告警：阈值触发 → webhook 推送，带防抖。
//!
//! 防抖：同一 (服务, 指标, 级别) 在 `min_interval_minutes` 内只推一次，
//! 避免同一个限额反复刷屏（对应文档「告警防抖 → 阈值 + 频控」）。

use crate::analyze::MetricStatus;
use crate::config::{Config, Level, Service};
use crate::store::Store;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub service: String,
    pub metric: String,
    pub level: Level,
    pub percent: f64,
    pub message: String,
    pub webhook: Option<String>,
    /// 是否被防抖窗口抑制（未真正发送）。
    pub suppressed: bool,
}

/// 对一组指标做阈值判定，返回需要告警的事件（已应用防抖窗口）。
pub fn evaluate(
    store: &Store,
    cfg: &Config,
    services: &[(Service, Vec<MetricStatus>)],
    now: DateTime<Utc>,
    force: bool,
) -> Result<Vec<AlertEvent>> {
    let min_interval = Duration::minutes(cfg.global.alert.min_interval_minutes as i64);
    let mut out = Vec::new();

    for (svc, statuses) in services {
        for m in statuses {
            if m.level < Level::Warn {
                continue;
            }
            let Some(sid) = store.service_id(&svc.name)? else {
                continue;
            };
            let level = m.level.as_str();
            let suppressed = !force
                && store
                    .last_alert_at(sid, &m.metric, level)?
                    .is_some_and(|last| now - last < min_interval);
            out.push(AlertEvent {
                service: svc.name.clone(),
                metric: m.metric.clone(),
                level: m.level,
                percent: m.percent.unwrap_or(0.0),
                message: message(m),
                webhook: resolve_webhook(cfg, svc),
                suppressed,
            });
        }
    }
    Ok(out)
}

fn resolve_webhook(cfg: &Config, svc: &Service) -> Option<String> {
    svc.webhook
        .clone()
        .or_else(|| cfg.global.alert.webhook.clone())
}

fn message(m: &MetricStatus) -> String {
    let cur = crate::report::fmt_num(m.current);
    let pct = crate::report::fmt_num(m.percent.unwrap_or(0.0));
    match m.level {
        Level::Exceeded => format!(
            "[ratecheck] {} / {} 已超限: {} ({}%)",
            m.service,
            m.metric,
            cur,
            pct
        ),
        Level::Critical => format!(
            "[ratecheck] {} / {} 接近限额(严重): {} ({}%)",
            m.service,
            m.metric,
            cur,
            pct
        ),
        Level::Warn => format!(
            "[ratecheck] {} / {} 接近限额: {} ({}%)",
            m.service,
            m.metric,
            cur,
            pct
        ),
        _ => String::new(),
    }
}

/// 推送 webhook（Slack/Discord/自建均可：统一 JSON，带 `text` 字段）。无 webhook 则跳过。
pub fn send(webhook: &str, ev: &AlertEvent, timeout_secs: u64) -> Result<()> {
    let payload = json!({
        "text": ev.message,
        "service": ev.service,
        "metric": ev.metric,
        "level": ev.level.as_str(),
        "percent": ev.percent,
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("构造 HTTP client 失败")?;
    client
        .post(webhook)
        .json(&payload)
        .send()
        .with_context(|| format!("推送 webhook 失败: {webhook}"))?
        .error_for_status()
        .with_context(|| format!("webhook 返回错误状态: {webhook}"))?;
    Ok(())
}

/// 真正把事件发出去，并把「已发送」的记录写入 DB（用于下次防抖）。
pub fn dispatch(
    store: &mut Store,
    cfg: &Config,
    events: &[AlertEvent],
    dry_run: bool,
) -> Result<(usize, usize)> {
    let now = Utc::now();
    let mut sent = 0usize;
    let mut suppressed = 0usize;
    for ev in events {
        if ev.suppressed {
            suppressed += 1;
            if !dry_run {
                if let Some(sid) = store.service_id(&ev.service)? {
                    store.record_alert(sid, &ev.metric, ev.level.as_str(), ev.percent, &ev.message, now)?;
                }
            }
            continue;
        }
        if !dry_run {
            if let Some(url) = &ev.webhook {
                send(url, ev, cfg.global.alert.timeout_secs)?;
            }
            if let Some(sid) = store.service_id(&ev.service)? {
                store.record_alert(
                    sid,
                    &ev.metric,
                    ev.level.as_str(),
                    ev.percent,
                    &ev.message,
                    now,
                )?;
            }
            sent += 1;
        }
    }
    Ok((sent, suppressed))
}
