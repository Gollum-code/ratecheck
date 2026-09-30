//! ratecheck — API 限额与用量监控 CLI。
//!
//! 用法见 README.md：`ratecheck collect && ratecheck status && ratecheck report`。

mod alert;
mod analyze;
mod collect;
mod config;
mod report;
mod store;

use crate::analyze::{month_bounds, MetricStatus, ServiceStatus};
use crate::config::{Config, Service, ServiceKind};
use crate::report::{DailyCost, Report};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "ratecheck",
    version,
    about = "API 限额/用量监控：采集 → 记录 → 阈值告警 → 用量/成本趋势报告"
)]
struct Cli {
    /// services.yaml 路径
    #[arg(long, global = true, default_value = "services.yaml")]
    config: PathBuf,
    /// SQLite 数据库路径
    #[arg(long, global = true, default_value = "ratecheck.db")]
    db: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 生成一份 services.yaml 示例配置
    Init {
        /// 输出路径
        #[arg(long, default_value = "services.yaml")]
        out: PathBuf,
        /// 已存在时覆盖
        #[arg(long)]
        force: bool,
    },
    /// 拉取各服务用量并写入时间序列
    Collect {
        /// 只采集指定服务
        #[arg(long)]
        service: Option<String>,
    },
    /// 手动/本地记录一条用量（代理侧统计兜底）
    Record {
        service: String,
        metric: String,
        value: f64,
        /// 该值为余额/剩余量，用量 = limit - value
        #[arg(long)]
        invert: bool,
    },
    /// 当前状态：接近限额一览（CLI 表格 / JSON）
    Status {
        /// 只看指定服务
        #[arg(long)]
        service: Option<String>,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 阈值判定并推送 webhook 告警（防抖）
    Check {
        /// 忽略防抖窗口强制发送
        #[arg(long)]
        force: bool,
        /// 只打印会告警的事件，不发送
        #[arg(long)]
        dry_run: bool,
    },
    /// 生成 HTML / JSON 趋势报告
    Report {
        /// 输出 HTML 文件路径（默认 report.html）
        #[arg(long)]
        out: Option<PathBuf>,
        /// 趋势窗口天数
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// 只输出 JSON 到 stdout
        #[arg(long)]
        json: bool,
    },
    /// 生成演示用合成数据（覆盖已有数据）
    Seed {
        /// 回填天数
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// 守护进程：按间隔循环 collect + check（+ 可选 report）
    Daemon {
        /// 循环间隔（秒）
        #[arg(long, default_value_t = 300)]
        interval: u64,
        /// 每轮生成 HTML 报告的路径
        #[arg(long)]
        report_out: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::Init { out, force } => cmd_init(out, *force),
        Cmd::Collect { service } => cmd_collect(&cli, service.as_deref()),
        Cmd::Record {
            service,
            metric,
            value,
            invert,
        } => cmd_record(&cli, service, metric, *value, *invert),
        Cmd::Status { service, json } => cmd_status(&cli, service.as_deref(), *json),
        Cmd::Check { force, dry_run } => cmd_check(&cli, *force, *dry_run),
        Cmd::Report { out, days, json } => cmd_report(&cli, out.as_deref(), *days, *json),
        Cmd::Seed { days } => cmd_seed(&cli, *days),
        Cmd::Daemon {
            interval,
            report_out,
        } => cmd_daemon(&cli, *interval, report_out.as_ref()),
    }
}

fn cmd_init(out: &PathBuf, force: bool) -> Result<()> {
    if out.exists() && !force {
        bail!("{} 已存在，用 --force 覆盖", out.display());
    }
    std::fs::write(out, include_str!("../services.example.yaml"))
        .with_context(|| format!("写入 {} 失败", out.display()))?;
    println!("已生成示例配置: {}", out.display());
    println!("编辑后用 `ratecheck collect && ratecheck status && ratecheck report` 开始监控。");
    Ok(())
}

fn load_store(cfg_path: &Path, db_path: &Path) -> Result<(Config, store::Store)> {
    let cfg = Config::load(cfg_path)?;
    let store = store::Store::open(db_path)?;
    Ok((cfg, store))
}

fn cmd_collect(cli: &Cli, only: Option<&str>) -> Result<()> {
    let (cfg, mut store) = load_store(&cli.config, &cli.db)?;
    let mut total = 0usize;
    for svc in &cfg.services {
        if let Some(name) = only {
            if svc.name != name {
                continue;
            }
        }
        if svc.kind == ServiceKind::Manual {
            println!("[skip] {} (manual，用 ratecheck record 记录)", svc.name);
            continue;
        }
        match collect::collect_service(svc, cfg.global.alert.timeout_secs) {
            Ok(samples) => {
                let sid = store.upsert_service(&svc.name)?;
                for s in &samples {
                    store.insert_point(sid, &s.metric, s.value, s.at)?;
                    println!(
                        "[ok] {} / {} = {} @ {}",
                        svc.name,
                        s.metric,
                        report::fmt_num(s.value),
                        s.at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S")
                    );
                    total += 1;
                }
                if samples.is_empty() {
                    println!("[ok] {} 无新样本", svc.name);
                }
            }
            Err(e) => eprintln!("[err] {}: {:#}", svc.name, e),
        }
    }
    println!("完成：写入 {total} 条用量点。");
    Ok(())
}

fn cmd_record(
    cli: &Cli,
    service: &str,
    metric: &str,
    value: f64,
    invert: bool,
) -> Result<()> {
    let (cfg, mut store) = load_store(&cli.config, &cli.db)?;
    if !cfg.services.iter().any(|s| s.name == service) {
        bail!("服务 {service:?} 未在 services.yaml 中配置");
    }
    let sid = store.upsert_service(service)?;
    store.insert_point(sid, metric, value, Utc::now())?;
    println!("已记录 {service} / {metric} = {value}（invert={invert}）");
    Ok(())
}

/// 对全部（或指定）服务做分析，返回 (服务配置, 状态) 列表。
fn statuses_for(
    cfg: &Config,
    store: &store::Store,
    only: Option<&str>,
) -> Result<Vec<(Service, Vec<MetricStatus>)>> {
    let now = Utc::now();
    let (start, _) = month_bounds(now);
    let mut out = Vec::new();
    for svc in &cfg.services {
        if let Some(name) = only {
            if svc.name != name {
                continue;
            }
        }
        let sid = match store.service_id(&svc.name)? {
            Some(id) => id,
            None => {
                eprintln!("[warn] {} 还没有数据，先 `ratecheck collect` 或 `ratecheck record`", svc.name);
                continue;
            }
        };
        let latest = store.latest_per_metric(sid)?;
        let mut series = Vec::new();
        for m in svc.metrics() {
            series.extend(store.series(sid, &m, start)?);
        }
        let metrics = analyze::analyze(svc, &latest, &series, now);
        out.push((svc.clone(), metrics));
    }
    Ok(out)
}

fn make_service_status(svc: &Service, metrics: Vec<MetricStatus>) -> ServiceStatus {
    let worst = metrics
        .iter()
        .map(|m| m.level)
        .max()
        .unwrap_or(config::Level::NoData);
    let total_cost = metrics.iter().filter_map(|m| m.cost_usd).sum::<f64>();
    ServiceStatus {
        name: svc.name.clone(),
        kind: svc.kind_label().to_string(),
        metrics,
        worst,
        total_cost_usd: total_cost,
    }
}

/// 按服务配置里的单价算出每个成本指标的每日成本，再汇总成该服务的每日成本。
fn daily_costs_for(
    svc: &Service,
    metrics: &[MetricStatus],
    store: &store::Store,
    sid: i64,
    days: i64,
    now: DateTime<Utc>,
) -> Vec<DailyCost> {
    let since = now - Duration::days(days);
    let mut merged: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for m in metrics {
        let rate = svc.cost_rate(&m.metric).map(|c| c.per_unit);
        if rate.is_none() {
            continue;
        }
        let Ok(pts) = store.series(sid, &m.metric, since) else {
            continue;
        };
        for dc in report::daily_cost(rate, m.invert, m.limit, &pts, now, days) {
            *merged.entry(dc.date).or_insert(0.0) += dc.cost;
        }
    }
    merged
        .into_iter()
        .map(|(date, cost)| DailyCost { date, cost })
        .collect()
}

fn cmd_status(cli: &Cli, only: Option<&str>, as_json: bool) -> Result<()> {
    let (cfg, store) = load_store(&cli.config, &cli.db)?;
    let pairs = statuses_for(&cfg, &store, only)?;
    let mut services = Vec::new();
    for (svc, metrics) in &pairs {
        services.push(make_service_status(svc, metrics.clone()));
    }
    let service_reports: Vec<report::ServiceReport> = services
        .into_iter()
        .map(|s| report::ServiceReport {
            name: s.name.clone(),
            kind: s.kind.clone(),
            worst: s.worst.as_str().to_string(),
            total_cost_usd: s.total_cost_usd,
            metrics: s.metrics.clone(),
            daily_cost: Vec::new(),
        })
        .collect();
    let report = Report {
        generated_at: Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string(),
        now: Utc::now().to_rfc3339(),
        days: cfg.global.report_days,
        service_count: service_reports.len(),
        alert_count: 0,
        total_cost_usd: service_reports.iter().map(|s| s.total_cost_usd).sum(),
        services: service_reports,
    };
    if as_json {
        println!("{}", report::to_json(&report)?);
    } else {
        print!("{}", report::render_cli(&report));
        if cfg.global.alert.webhook.is_some() {
            println!("\n提示：运行 `ratecheck check` 会推送阈值告警到已配置的 webhook。");
        }
    }
    Ok(())
}

fn cmd_check(cli: &Cli, force: bool, dry_run: bool) -> Result<()> {
    let (cfg, mut store) = load_store(&cli.config, &cli.db)?;
    let now = Utc::now();
    let pairs = statuses_for(&cfg, &store, None)?;
    if pairs.is_empty() {
        println!("没有可评估的服务（先 collect / record 写入数据）。");
        return Ok(());
    }
    let events = alert::evaluate(&store, &cfg, &pairs, now, force)?;
    if events.is_empty() {
        println!("当前无告警（全部在阈值以下或无数据）。");
        return Ok(());
    }
    if dry_run {
        for e in &events {
            println!(
                "[{}] {} / {} {:.1}% → {}",
                e.level.as_str(),
                e.service,
                e.metric,
                e.percent,
                if e.suppressed { "(防抖抑制)" } else { "(将发送)" }
            );
        }
        return Ok(());
    }
    let (sent, suppressed) = alert::dispatch(&mut store, &cfg, &events, false)?;
    for e in &events {
        if e.suppressed {
            println!(
                "[skip] {} / {} {:.1}% (防抖窗口内，未发送)",
                e.service, e.metric, e.percent
            );
        } else {
            println!(
                "[alert] {} / {} {:.1}% → {}",
                e.service,
                e.metric,
                e.percent,
                e.webhook.as_deref().unwrap_or("(未配置 webhook，仅记录)")
            );
        }
    }
    println!("共 {sent} 条告警，{suppressed} 条被防抖抑制。");
    Ok(())
}

fn cmd_report(cli: &Cli, out: Option<&Path>, days: i64, as_json: bool) -> Result<()> {
    let (cfg, store) = load_store(&cli.config, &cli.db)?;
    let now = Utc::now();
    let pairs = statuses_for(&cfg, &store, None)?;
    let mut services = Vec::new();
    for (svc, metrics) in &pairs {
        let status = make_service_status(svc, metrics.clone());
        let sid = store.service_id(&svc.name)?.unwrap_or(0);
        let dc = if sid > 0 {
            daily_costs_for(svc, metrics, &store, sid, days, now)
        } else {
            Vec::new()
        };
        services.push((status, dc));
    }
    let alert_count = store.recent_alerts(1000)?.len();
    let report = report::build(Local::now(), days, services, alert_count);

    if as_json {
        println!("{}", report::to_json(&report)?);
        return Ok(());
    }
    let path = out
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("report.html"));
    std::fs::write(&path, report::render_html(&report))
        .with_context(|| format!("写入报告失败: {}", path.display()))?;
    println!("已生成 HTML 报告: {}", path.display());
    println!("{}", report::render_cli(&report));
    Ok(())
}

fn cmd_seed(cli: &Cli, days: i64) -> Result<()> {
    let (cfg, mut store) = load_store(&cli.config, &cli.db)?;
    let now = Utc::now();
    let mut inserted = 0usize;
    let targets = [45.0, 78.0, 92.0, 118.0]; // 正常/警告/严重/超限 各色样本
    for (idx, svc) in cfg.services.iter().enumerate() {
        let sid = store.upsert_service(&svc.name)?;
        store.clear_service(sid)?;
        let target_pct = targets[idx % targets.len()];
        for limit in &svc.limits {
            for i in 0..days {
                let day = now - Duration::days(days - 1 - i);
                let t = i as f64 / days.max(1) as f64;
                let mut usage = limit.limit * (0.01 + 0.99 * t) * target_pct / 100.0;
                // 轻微波动
                let wobble = (i as f64 * 1.7).sin() * limit.limit * 0.01;
                usage = (usage + wobble).max(0.0);
                let value = if svc.invert { limit.limit - usage } else { usage };
                store.insert_point(sid, &limit.metric, value, day)?;
                inserted += 1;
            }
            println!(
                "[seed] {} / {} 回填 {days} 天，终点约为限额的 {target_pct:.0}%",
                svc.name, limit.metric
            );
        }
    }
    println!("合成数据 {inserted} 条已写入（仅供演示，非真实用量）。");
    println!("下一步：`ratecheck status` / `ratecheck report` / `ratecheck check --dry-run`");
    Ok(())
}

fn cmd_daemon(cli: &Cli, interval: u64, report_out: Option<&PathBuf>) -> Result<()> {
    let _ = cli;
    eprintln!(
        "ratecheck daemon 启动，每 {interval}s 循环 collect + check{}（Ctrl+C 退出）",
        if report_out.is_some() { " + report" } else { "" }
    );
    loop {
        // 子命令逻辑按 interval 循环
        let cli2 = Cli {
            config: cli.config.clone(),
            db: cli.db.clone(),
            command: Cmd::Collect { service: None },
        };
        let _ = cmd_collect(&cli2, None);
        let cli3 = Cli {
            config: cli.config.clone(),
            db: cli.db.clone(),
            command: Cmd::Check {
                force: false,
                dry_run: false,
            },
        };
        let _ = cmd_check(&cli3, false, false);
        if let Some(path) = report_out {
            let _ = cmd_report(cli, Some(path.as_path()), 30, false);
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}
