//! 采集：按服务类型把「用量 API」拉回来，统一成 `Sample`。
//!
//! 适配器策略（对应文档「各服务用量 API 差异 → 适配器 + 手动记录兜底」）：
//! - `openai_billing`：OpenAI dashboard billing（本月花费，分 → 美元）
//! - `generic_json`：任意 GET + JSON 路径取值（云 billing、DeepSeek 余额等）
//! - `manual`：不拉取，由 `ratecheck record` 写入本地记录

use crate::config::{Service, ServiceKind};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};

/// 一次拉取得到的原始样本（尚未入库）。
#[derive(Debug, Clone)]
pub struct Sample {
    pub metric: String,
    pub value: f64,
    pub at: DateTime<Utc>,
}

pub fn collect_service(svc: &Service, timeout_secs: u64) -> Result<Vec<Sample>> {
    match svc.kind {
        ServiceKind::Manual => Ok(vec![]),
        ServiceKind::OpenaiBilling => pull_openai_billing(svc, timeout_secs),
        ServiceKind::GenericJson => pull_generic_json(svc, timeout_secs),
    }
}

/// OpenAI 本月已花费（dashbaord billing usage 接口）。
/// 需要组织 admin key；返回 `total_usage`（分）。
pub fn pull_openai_billing(svc: &Service, timeout_secs: u64) -> Result<Vec<Sample>> {
    let key = svc.api_key().context("缺少 api_key / api_key_env")?;
    let base = svc
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let now = Local::now();
    let start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
        .context("构造月初日期失败")?
        .format("%Y-%m-%d")
        .to_string();
    let end = now.date_naive().format("%Y-%m-%d").to_string();
    let url = format!(
        "{base}/dashboard/billing/usage?start_date={start}&end_date={end}"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("构造 HTTP client 失败")?;
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .with_context(|| format!("请求失败: {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP 错误: {url}"))?;
    let json: serde_json::Value = resp.json().context("解析 JSON 失败")?;

    let total_used = json
        .get("total_used")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !total_used {
        bail!(
            "OpenAI billing 返回 total_used=false（该 key 可能不是组织 admin key）: {}",
            json
        );
    }
    // total_usage 单位是分（cent），转成美元。
    let cents = json
        .get("total_usage")
        .and_then(|v| v.as_f64())
        .context("响应缺少 total_usage")?;
    Ok(vec![Sample {
        metric: "cost_usd".to_string(),
        value: cents / 100.0,
        at: Utc::now(),
    }])
}

/// 通用 JSON 采集：GET `url`，从 `field`（支持 `a.b[0].c`）取值，
/// 乘以 `scale` 后作为指标 `metric`。
pub fn pull_generic_json(svc: &Service, timeout_secs: u64) -> Result<Vec<Sample>> {
    let url = svc.url.clone().context("generic_json 需要 url")?;
    let metric = svc.metric.clone().context("generic_json 需要 metric")?;
    let field = svc.field.clone().context("generic_json 需要 field")?;

    let mut req = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("构造 HTTP client 失败")?
        .get(&url);

    if let Some(key) = svc.api_key() {
        let header = svc
            .header_name()
            .unwrap_or_else(|| "Authorization".to_string());
        if header.eq_ignore_ascii_case("Authorization") {
            req = req.bearer_auth(key);
        } else {
            req = req.header(header, key);
        }
    }

    let resp = req
        .send()
        .with_context(|| format!("请求失败: {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP 错误: {url}"))?;
    let json: serde_json::Value = resp.json().context("解析 JSON 失败")?;

    let raw = json_path_get(&json, &field)
        .with_context(|| format!("JSON 路径 {field:?} 不存在: {}", json))?;
    let value = as_f64(raw)
        .with_context(|| format!("路径 {field:?} 的值不是数字: {raw}"))?;

    Ok(vec![Sample {
        metric,
        value: value * svc.scale,
        at: Utc::now(),
    }])
}

/// 从 JSON 里按 `a.b[0].c` 路径取值。
pub fn json_path_get<'a>(value: &'a serde_json::Value, path: &str) -> Result<&'a serde_json::Value> {
    let mut cur = value;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        let (name, idx) = match seg.find('[') {
            Some(p) => {
                let name = &seg[..p];
                let idx = &seg[p + 1..];
                if !idx.ends_with(']') {
                    bail!("非法路径段: {seg}");
                }
                let idx = &idx[..idx.len() - 1];
                (name, Some(idx))
            }
            None => (seg, None),
        };
        if !name.is_empty() {
            cur = cur
                .get(name)
                .with_context(|| format!("缺少字段 {name:?}（在 {path:?} 中）"))?;
        }
        if let Some(i) = idx {
            let i: usize = i.parse().with_context(|| format!("非法数组下标: {i}"))?;
            cur = cur.get(i).with_context(|| format!("缺少下标 [{i}]"))?;
        }
    }
    Ok(cur)
}

fn as_f64(v: &serde_json::Value) -> Result<f64> {
    match v {
        serde_json::Value::Number(n) => n
            .as_f64()
            .context("数字无法转成 f64"),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<f64>()
            .with_context(|| format!("字符串 {s:?} 不是数字")),
        serde_json::Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        other => bail!("值不是数字: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_path_navigation() {
        let v: serde_json::Value = serde_json::json!({
            "data": { "usage": { "total_tokens": 123 } },
            "balance_infos": [ { "currency": "CNY", "total_balance": "110.00" } ]
        });
        assert_eq!(
            json_path_get(&v, "data.usage.total_tokens").unwrap().as_i64(),
            Some(123)
        );
        let bal = json_path_get(&v, "balance_infos[0].total_balance").unwrap();
        assert_eq!(as_f64(bal).unwrap(), 110.0);
        assert!(json_path_get(&v, "data.missing").is_err());
        assert!(json_path_get(&v, "balance_infos[5]").is_err());
    }

    #[test]
    fn converts_values() {
        let v = serde_json::json!({"a": "12.5", "b": true, "c": 7});
        assert_eq!(as_f64(&v["a"]).unwrap(), 12.5);
        assert_eq!(as_f64(&v["b"]).unwrap(), 1.0);
        assert_eq!(as_f64(&v["c"]).unwrap(), 7.0);
        assert!(as_f64(&serde_json::json!({"x": {}})).is_err());
    }

    #[test]
    fn deepseek_sample_parses() {
        let v: serde_json::Value = serde_json::json!({
            "is_available": true,
            "balance_infos": [
                {
                    "currency": "CNY",
                    "total_balance": "110.00",
                    "granted_balance": "10.00",
                    "topped_up_balance": "100.00"
                }
            ]
        });
        let got = json_path_get(&v, "balance_infos[0].total_balance").unwrap();
        assert_eq!(as_f64(got).unwrap(), 110.0);
    }
}