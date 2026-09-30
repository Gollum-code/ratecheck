# ratecheck

**API 限额与用量监控 CLI** —— 一条命令回答三个问题：**用了多少？还剩多少？会不会超？**

跟踪各服务的 API 用量（LLM 的 token/credits/配额、云服务的 RPS/账单），定期拉取或本地记录 → 时间序列入库（SQLite）→ 阈值告警（webhook，防抖）→ 用量/成本趋势报告（HTML/CLI/JSON）。

> 缺口：各服务自带看板分散、代理日志只记不警；缺「统一限额监控 + 告警 + 成本趋势」的本地 CLI。

## 特性

- **统一监控多服务**：OpenAI 账单、DeepSeek 余额、任意 JSON 用量 API、本地代理统计
- **阈值告警**：接近限额（默认 warn 80% / critical 95%）→ webhook，带防抖窗口不刷屏
- **用量/成本趋势**：HTML 看板（限额条 + SVG 趋势图 + 每日成本）+ CLI 表格 + JSON
- **成本预测**：最小二乘拟合 → 日均速率 → 月底预测用量 → 预计超限日期
- **余额适配**：`invert: true` 把「剩余余额」换算成「已用量」
- 纯本地 SQLite，零部署，一条命令可跑

## 快速开始

```bash
# 1. 生成示例配置并编辑
ratecheck init
$EDITOR services.yaml

# 2. 采集一次（或本地记录）
ratecheck collect                 # 拉取各服务用量 API
ratecheck record my-proxy tokens 523001   # 本地/代理侧手动记录

# 3. 看状态 + 告警 + 报告
ratecheck status
ratecheck check                   # 超阈值则推送 webhook（防抖）
ratecheck report                  # 生成 report.html

# 4. 定时跑（Windows 任务计划 / crontab）
#    * * * * *  ratecheck collect && ratecheck check
```

### 30 秒看效果（不接真实 API）

```bash
ratecheck init
ratecheck seed --days 30          # 生成演示用合成数据
ratecheck status
ratecheck check --dry-run         # 预览告警
ratecheck report                  # 打开 report.html
```

## 配置（services.yaml）

```yaml
global:
  report_days: 30
  alert:
    webhook: "https://hooks.example.com/xxxx"   # Slack/Discord/自建
    min_interval_minutes: 60                    # 告警防抖窗口
    timeout_secs: 10
services:
  # 1) OpenAI：本月已花费（需组织 admin key）
  - name: openai
    type: openai_billing
    api_key_env: OPENAI_ADMIN_KEY
    limits: [{ metric: cost_usd, limit: 200, unit: usd }]
    thresholds: [{ metric: cost_usd, warn: 70, critical: 90 }]

  # 2) 任意 JSON 用量 API（余额/配额/账单）
  - name: deepseek
    type: generic_json
    url: https://api.deepseek.com/user/balance
    api_key_env: DEEPSEEK_API_KEY
    field: balance_infos[0].total_balance     # 支持 a.b[0].c
    metric: balance_cny
    scale: 1.0
    invert: true                              # 值=剩余量 → 用量 = limit - value
    limits: [{ metric: balance_cny, limit: 110, unit: CNY }]

  # 3) 本地/代理侧记录
  - name: my-proxy
    type: manual
    limits: [{ metric: tokens, limit: 1000000, unit: tokens }]
    cost_rates: [{ metric: tokens, per_unit: 0.000002 }]
```

配置里支持 `${ENV_VAR}` 插值。未配置阈值时默认 warn=80%、critical=95%。

### 适配器

| type | 说明 | 采集方式 |
|---|---|---|
| `openai_billing` | OpenAI dashboard billing | 自动 |
| `generic_json` | 任意 GET + JSON 路径取值 | 自动 |
| `manual` | 本地代理/应用侧统计 | `ratecheck record` |

### 成本估算

`cost_rates.metric.per_unit`（每单位用量折合美元）× 用量 = 估算成本。
报告页明确标注「单价 × 用量」为估算值、非账单。

## 命令

| 命令 | 作用 |
|---|---|
| `ratecheck init [--out x.yaml] [--force]` | 生成示例配置 |
| `ratecheck collect [--service NAME]` | 拉取用量并入库 |
| `ratecheck record <svc> <metric> <value> [--invert]` | 手动记录一条 |
| `ratecheck status [--service NAME] [--json]` | 状态一览（表格/JSON） |
| `ratecheck check [--force] [--dry-run]` | 阈值判定 + webhook 告警 |
| `ratecheck report [--out f.html] [--days N] [--json]` | HTML/JSON 趋势报告 |
| `ratecheck seed [--days N]` | 生成演示数据 |
| `ratecheck daemon [--interval S] [--report-out f.html]` | 守护循环采集+告警 |

## 告警负载（webhook）

```json
{
  "text": "[ratecheck] openai / cost_usd 接近限额(严重): $182.3 (91%)",
  "service": "openai",
  "metric": "cost_usd",
  "level": "critical",
  "percent": 91.0
}
```

## 架构

```
ratecheck CLI/守护 (Rust)
  ├─ 配置  services.yaml（服务 + 用量 API + 限额 + 阈值 + 成本单价）
  ├─ 采集  适配器（openai_billing / generic_json / manual）
  ├─ 记录  SQLite 时间序列（points / alerts）
  ├─ 分析  接近限额、速率突增、成本趋势/预测（最小二乘）
  ├─ 告警  阈值 → webhook（防抖窗口）
  └─ 报告  HTML（限额条+趋势图+每日成本）+ CLI/JSON
```

## Roadmap

- M1（单服务采集 + 记录 + CLI 报告）✅ 本版本
- M2（多服务 + 阈值告警 + HTML 趋势）✅ 本版本
- M3（成本预测 + 本地代理统计）✅ 本版本
- 后续：RPS/限流实时监控、预算上限硬切断、多云账单导入、Prometheus 导出

## 已知限制

- 成本为估算（单价 × 用量），非官方账单；OpenAI 需组织 admin key
- 各服务 API 变动时需调整适配器（generic_json 已覆盖大多数场景）
- `report.html` 数据来自 SQLite 本地点，不含实时拉取

## 开发

```bash
cargo build --release
cargo test          # 单元测试（config/store/analyze/collect/report）
```
