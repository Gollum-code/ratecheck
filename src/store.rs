//! SQLite 时间序列存储：用量点 + 已发告警记录。
//!
//! 一张 `points` 表存 (服务, 指标, 值, 时间) 的时间序列；
//! 一张 `alerts` 表用于告警防抖。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::Path;

#[derive(Debug, Clone)]
#[allow(dead_code)] // service 字段保留给后续 JSON 导出使用
pub struct UsagePoint {
    pub service: String,
    pub metric: String,
    pub value: f64,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // AlertRecord 字段保留给后续告警历史查询使用
pub struct AlertRecord {
    pub service: String,
    pub metric: String,
    pub level: String,
    pub pct: f64,
    pub message: String,
    pub at: DateTime<Utc>,
}

pub struct Store {
    conn: Connection,
}

fn ts(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("创建数据库目录失败: {}", dir.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS services (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS points (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                service_id INTEGER NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                metric     TEXT NOT NULL,
                value      REAL NOT NULL,
                at         TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_points
                ON points(service_id, metric, at);
            CREATE TABLE IF NOT EXISTS alerts (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                service_id INTEGER NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                metric     TEXT NOT NULL,
                level      TEXT NOT NULL,
                pct        REAL NOT NULL,
                message    TEXT NOT NULL,
                at         TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_alerts
                ON alerts(service_id, metric, level, at);
            "#,
        )?;
        Ok(Store { conn })
    }

    /// 登记服务并返回其 id（不存在则创建）。
    pub fn upsert_service(&mut self, name: &str) -> Result<i64> {
        let id = self.conn.query_row(
            "SELECT id FROM services WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        );
        match id {
            Ok(id) => Ok(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute(
                    "INSERT INTO services (name) VALUES (?1)",
                    params![name],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
            Err(e) => Err(e).context("查询服务失败"),
        }
    }

    pub fn service_id(&self, name: &str) -> Result<Option<i64>> {
        let id = self.conn.query_row(
            "SELECT id FROM services WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        );
        match id {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("查询服务失败"),
        }
    }

    #[allow(dead_code)] // 仅供调试 / 后续 CLI 子命令使用
    pub fn list_services(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM services ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn insert_point(
        &mut self,
        service_id: i64,
        metric: &str,
        value: f64,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO points (service_id, metric, value, at) VALUES (?1, ?2, ?3, ?4)",
            params![service_id, metric, value, ts(at)],
        )?;
        Ok(())
    }

    /// 每个指标最新一条。
    pub fn latest_per_metric(&self, service_id: i64) -> Result<Vec<UsagePoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, p.metric, p.value, p.at
             FROM points p JOIN services s ON s.id = p.service_id
             WHERE p.service_id = ?1 AND p.id IN (
                 SELECT MAX(id) FROM points WHERE service_id = ?1 GROUP BY metric
             )",
        )?;
        let rows = stmt.query_map(params![service_id], |r| {
            Ok(UsagePoint {
                service: r.get(0)?,
                metric: r.get(1)?,
                value: r.get(2)?,
                at: parse_ts(&r.get::<_, String>(3)?),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 某指标在时间窗内的全部点（按时间升序）。
    pub fn series(
        &self,
        service_id: i64,
        metric: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<UsagePoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, p.metric, p.value, p.at
             FROM points p JOIN services s ON s.id = p.service_id
             WHERE p.service_id = ?1 AND p.metric = ?2 AND p.at >= ?3
             ORDER BY p.at ASC",
        )?;
        let rows = stmt.query_map(params![service_id, metric, ts(since)], |r| {
            Ok(UsagePoint {
                service: r.get(0)?,
                metric: r.get(1)?,
                value: r.get(2)?,
                at: parse_ts(&r.get::<_, String>(3)?),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn record_alert(
        &mut self,
        service_id: i64,
        metric: &str,
        level: &str,
        pct: f64,
        message: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO alerts (service_id, metric, level, pct, message, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![service_id, metric, level, pct, message, ts(at)],
        )?;
        Ok(())
    }

    /// 同一 (服务, 指标, 级别) 上次告警时间。
    pub fn last_alert_at(
        &self,
        service_id: i64,
        metric: &str,
        level: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        let mut stmt = self.conn.prepare(
            "SELECT MAX(at) FROM alerts
             WHERE service_id = ?1 AND metric = ?2 AND level = ?3",
        )?;
        let max: Option<String> = stmt.query_row(
            params![service_id, metric, level],
            |r| r.get::<_, Option<String>>(0),
        )?;
        Ok(max.map(|s| parse_ts(&s)))
    }

    pub fn recent_alerts(&self, limit: usize) -> Result<Vec<AlertRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, a.metric, a.level, a.pct, a.message, a.at
             FROM alerts a JOIN services s ON s.id = a.service_id
             ORDER BY a.at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(AlertRecord {
                service: r.get(0)?,
                metric: r.get(1)?,
                level: r.get(2)?,
                pct: r.get(3)?,
                message: r.get(4)?,
                at: parse_ts(&r.get::<_, String>(5)?),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 删掉某服务全部数据（seed/清理用）。
    pub fn clear_service(&mut self, service_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM points WHERE service_id = ?1", params![service_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (store, dir)
    }

    #[test]
    fn roundtrip_points_and_latest() {
        let (mut store, _d) = temp_store();
        let id = store.upsert_service("svc-a").unwrap();
        let now = Utc::now();
        store
            .insert_point(id, "tokens", 100.0, now - Duration::hours(2))
            .unwrap();
        store
            .insert_point(id, "tokens", 250.0, now - Duration::hours(1))
            .unwrap();
        store
            .insert_point(id, "cost_usd", 5.5, now - Duration::hours(1))
            .unwrap();

        let latest = store.latest_per_metric(id).unwrap();
        assert_eq!(latest.len(), 2);
        let tokens = latest.iter().find(|p| p.metric == "tokens").unwrap();
        assert_eq!(tokens.value, 250.0);

        let series = store
            .series(id, "tokens", now - Duration::hours(3))
            .unwrap();
        assert_eq!(series.len(), 2);
        assert!(series[0].value <= series[1].value);
    }

    #[test]
    fn upsert_is_idempotent() {
        let (mut store, _d) = temp_store();
        let a = store.upsert_service("x").unwrap();
        let b = store.upsert_service("x").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.list_services().unwrap().len(), 1);
    }

    #[test]
    fn alert_dedup_helpers() {
        let (mut store, _d) = temp_store();
        let id = store.upsert_service("x").unwrap();
        let now = Utc::now();
        assert!(store.last_alert_at(id, "tokens", "warn").unwrap().is_none());
        store
            .record_alert(id, "tokens", "warn", 81.0, "hi", now)
            .unwrap();
        let last = store.last_alert_at(id, "tokens", "warn").unwrap();
        assert!(last.is_some());
        assert_eq!(store.recent_alerts(10).unwrap().len(), 1);
    }
}