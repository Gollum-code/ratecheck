//! `services.yaml` 的解析与校验。
//!
//! 支持 `${ENV_VAR}` 插值，便于把密钥写在配置里而不入库。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// 未配置阈值时的默认告警水位（占限额百分比）。
pub const DEFAULT_WARN: f64 = 80.0;
pub const DEFAULT_CRITICAL: f64 = 95.0;

fn default_report_days() -> i64 {
    30
}

fn default_min_interval() -> u64 {
    60
}

fn default_timeout() -> u64 {
    10
}

fn one() -> f64 {
    1.0
}

fn default_currency() -> String {
    "USD".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    #[serde(default)]
    pub services: Vec<Service>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Config = serde_yaml::from_str(text).context("解析 services.yaml 失败")?;
        cfg.expand_env();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置失败: {}", path.display()))?;
        Self::parse(&text)
    }

    /// 把配置里所有 `${VAR}` 替换成环境变量值。
    pub fn expand_env(&mut self) {
        fn walk(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::String(s) => *s = expand_str(s),
                serde_json::Value::Array(a) => a.iter_mut().for_each(walk),
                serde_json::Value::Object(o) => o.values_mut().for_each(walk),
                _ => {}
            }
        }
        if let Ok(value) = serde_json::to_value(&*self) {
            let mut value = value;
            walk(&mut value);
            if let Ok(cfg) = serde_json::from_value(value) {
                *self = cfg;
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for svc in &self.services {
            if svc.name.trim().is_empty() {
                bail!("存在未命名的服务");
            }
            if !seen.insert(svc.name.clone()) {
                bail!("服务名重复: {}", svc.name);
            }
            if svc.kind == ServiceKind::GenericJson {
                if svc.url.is_none() {
                    bail!("服务 {} (type=generic_json) 必须配置 url", svc.name);
                }
                if svc.metric.is_none() {
                    bail!("服务 {} (type=generic_json) 必须配置 metric", svc.name);
                }
            }
            for l in &svc.limits {
                if l.limit <= 0.0 {
                    bail!("服务 {} 的指标 {} 限额必须大于 0", svc.name, l.metric);
                }
            }
            for t in &svc.thresholds {
                if let (Some(w), Some(c)) = (t.warn, t.critical) {
                    if w > c {
                        bail!(
                            "服务 {} 的指标 {} warn({}) 不能大于 critical({})",
                            svc.name,
                            t.metric,
                            w,
                            c
                        );
                    }
                }
                if t.warn.is_none() && t.critical.is_none() {
                    bail!("服务 {} 的指标 {} 未设置任何阈值", svc.name, t.metric);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Global {
    #[serde(default = "default_report_days")]
    pub report_days: i64,
    #[serde(default)]
    pub alert: AlertSettings,
}

impl Default for Global {
    fn default() -> Self {
        Global {
            report_days: default_report_days(),
            alert: AlertSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSettings {
    #[serde(default)]
    pub webhook: Option<String>,
    /// 同一 (服务, 指标, 级别) 在此窗口内只告警一次，避免刷屏。
    #[serde(default = "default_min_interval")]
    pub min_interval_minutes: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for AlertSettings {
    fn default() -> Self {
        AlertSettings {
            webhook: None,
            min_interval_minutes: default_min_interval(),
            timeout_secs: default_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    /// OpenAI dashboard billing 接口（返回本月已花费，单位分）。
    OpenaiBilling,
    /// 通用：GET 一个 JSON 地址，按路径取值。
    GenericJson,
    /// 本地/代理侧记录，用 `ratecheck record` 写入。
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ServiceKind,

    // ---- openai_billing / generic_json ----
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 非 Authorization 头的名字（如 "X-API-Key"）。
    #[serde(default)]
    pub header_name: Option<String>,
    /// generic_json 放值的 JSON 路径，支持 `a.b[0].c`。
    #[serde(default)]
    pub field: Option<String>,
    /// 采集到的值写入哪个指标。
    #[serde(default)]
    pub metric: Option<String>,
    /// 值缩放：raw * scale（分→元等）。
    #[serde(default = "one")]
    pub scale: f64,
    /// true 表示这是「剩余量」（余额/配额余额），用量 = limit - value。
    #[serde(default)]
    pub invert: bool,
    #[serde(default)]
    pub unit: Option<String>,

    #[serde(default)]
    pub limits: Vec<Limit>,
    #[serde(default)]
    pub cost_rates: Vec<CostRate>,
    #[serde(default)]
    pub thresholds: Vec<Threshold>,
    /// 覆盖 global.alert.webhook，仅该服务用这个 webhook。
    #[serde(default)]
    pub webhook: Option<String>,
}

impl Service {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            ServiceKind::OpenaiBilling => "openai_billing",
            ServiceKind::GenericJson => "generic_json",
            ServiceKind::Manual => "manual",
        }
    }

    pub fn api_key(&self) -> Option<String> {
        if let Some(k) = self.api_key.as_ref().filter(|s| !s.is_empty()) {
            return Some(k.clone());
        }
        self.api_key_env
            .as_ref()
            .and_then(|e| std::env::var(e).ok())
            .filter(|s| !s.is_empty())
    }

    pub fn header_name(&self) -> Option<String> {
        self.header_name.clone()
    }

    pub fn limit(&self, metric: &str) -> Option<&Limit> {
        self.limits.iter().find(|l| l.metric == metric)
    }

    pub fn cost_rate(&self, metric: &str) -> Option<&CostRate> {
        self.cost_rates.iter().find(|c| c.metric == metric)
    }

    /// 未显式配置阈值时回落到 80% / 95%。
    pub fn threshold(&self, metric: &str) -> Threshold {
        self.thresholds
            .iter()
            .find(|t| t.metric == metric)
            .cloned()
            .unwrap_or(Threshold {
                metric: metric.to_string(),
                warn: Some(DEFAULT_WARN),
                critical: Some(DEFAULT_CRITICAL),
            })
    }

    /// 该服务需要展示/检查的全部指标名（去重排序）。
    pub fn metrics(&self) -> Vec<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for l in &self.limits {
            set.insert(l.metric.clone());
        }
        for c in &self.cost_rates {
            set.insert(c.metric.clone());
        }
        for t in &self.thresholds {
            set.insert(t.metric.clone());
        }
        match self.kind {
            ServiceKind::OpenaiBilling => {
                set.insert("cost_usd".to_string());
            }
            ServiceKind::GenericJson => {
                if let Some(m) = &self.metric {
                    set.insert(m.clone());
                }
            }
            ServiceKind::Manual => {}
        }
        set.into_iter().collect()
    }

    pub fn unit_of(&self, metric: &str) -> String {
        if let Some(l) = self.limit(metric) {
            if let Some(u) = &l.unit {
                return u.clone();
            }
        }
        if let Some(u) = &self.unit {
            return u.clone();
        }
        if metric.ends_with("_usd") {
            "USD".to_string()
        } else {
            metric.to_string()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limit {
    pub metric: String,
    pub limit: f64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRate {
    pub metric: String,
    /// 每 1 单位用量折合多少货币。
    pub per_unit: f64,
    #[serde(default = "default_currency")]
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Threshold {
    pub metric: String,
    #[serde(default)]
    pub warn: Option<f64>,
    #[serde(default)]
    pub critical: Option<f64>,
}

impl Threshold {
    pub fn classify(&self, percent: f64) -> Level {
        Level::from_percent(percent, self.warn, self.critical)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    NoData,
    Ok,
    Warn,
    Critical,
    Exceeded,
}

impl Level {
    pub fn from_percent(percent: f64, warn: Option<f64>, critical: Option<f64>) -> Level {
        if percent >= 100.0 {
            return Level::Exceeded;
        }
        if critical.map(|c| percent >= c).unwrap_or(false) {
            return Level::Critical;
        }
        if warn.map(|w| percent >= w).unwrap_or(false) {
            return Level::Warn;
        }
        Level::Ok
    }

    pub fn label(&self) -> &'static str {
        match self {
            Level::NoData => "无数据",
            Level::Ok => "正常",
            Level::Warn => "警告",
            Level::Critical => "严重",
            Level::Exceeded => "已超限",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Level::NoData => "nodata",
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Critical => "critical",
            Level::Exceeded => "exceeded",
        }
    }
}

/// `${VAR}` -> 环境变量值；找不到变量时替换为空串。
pub fn expand_str(input: &str) -> String {
    if !input.contains("${") {
        return input.to_string();
    }
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some(off) = chars[i + 2..].iter().position(|c| *c == '}') {
                let name: String = chars[i + 2..i + 2 + off].iter().collect();
                out.push_str(&std::env::var(&name).unwrap_or_default());
                i = i + 2 + off + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
global:
  report_days: 14
  alert:
    webhook: "https://example.com/hook"
services:
  - name: openai
    type: openai_billing
    api_key_env: OPENAI_API_KEY
    limits:
      - metric: cost_usd
        limit: 200
    thresholds:
      - metric: cost_usd
        warn: 70
        critical: 90
  - name: my-proxy
    type: manual
    limits:
      - metric: tokens
        limit: 1000000
    cost_rates:
      - metric: tokens
        per_unit: 0.000002
"#;

    #[test]
    fn parses_sample() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.global.report_days, 14);
        assert_eq!(cfg.services.len(), 2);
        assert_eq!(cfg.services[0].kind, ServiceKind::OpenaiBilling);
        assert_eq!(cfg.services[1].metrics(), vec!["tokens"]);
    }

    #[test]
    fn default_threshold_used_when_absent() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let my_proxy = cfg.services.iter().find(|s| s.name == "my-proxy").unwrap();
        let t = my_proxy.threshold("tokens");
        assert_eq!(t.warn, Some(DEFAULT_WARN));
        assert_eq!(t.classify(96.0), Level::Critical);
        assert_eq!(t.classify(82.0), Level::Warn);
        assert_eq!(t.classify(30.0), Level::Ok);
        assert_eq!(t.classify(120.0), Level::Exceeded);
    }

    #[test]
    fn rejects_duplicate_names() {
        let text = SAMPLE.replace("name: my-proxy", "name: openai");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn expands_env_placeholders() {
        std::env::set_var("RATECHECK_TEST_KEY", "sk-xyz");
        assert_eq!(expand_str("Bearer ${RATECHECK_TEST_KEY}"), "Bearer sk-xyz");
        assert_eq!(expand_str("${NOT_SET_VAR_XYZ}"), "");
        assert_eq!(expand_str("plain"), "plain");
    }
}
