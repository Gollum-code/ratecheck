//! 端到端 CLI 测试：init → seed → status/report/check 全流程。

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ratecheck"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ratecheck-it-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn end_to_end_seed_status_report() {
    let dir = temp_dir("e2e");
    let cfg = dir.join("services.yaml");
    let db = dir.join("test.db");

    let out = bin()
        .args(["init", "--out"])
        .arg(&cfg)
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "init 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cfg.exists());

    let out = bin()
        .args(["seed", "--config"])
        .arg(&cfg)
        .arg("--db")
        .arg(&db)
        .arg("--days")
        .arg("14")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = bin()
        .args(["status", "--config"])
        .arg(&cfg)
        .arg("--db")
        .arg(&db)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("status 输出不是合法 JSON: {e}\n{stdout}"));
    assert_eq!(v["service_count"], 3, "应包含 3 个服务: {stdout}");
    assert!(v["services"].is_array());

    // 我的代理的 tokens 目标 118% → 应触发告警
    let out = bin()
        .args(["check", "--config"])
        .arg(&cfg)
        .arg("--db")
        .arg(&db)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("my-proxy"),
        "check --dry-run 应包含 my-proxy: {stdout}"
    );

    let html = dir.join("report.html");
    let out = bin()
        .args(["report", "--config"])
        .arg(&cfg)
        .arg("--db")
        .arg(&db)
        .arg("--out")
        .arg(&html)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(html.exists(), "report.html 应生成");
    let content = std::fs::read_to_string(&html).unwrap();
    assert!(content.contains("ratecheck"), "HTML 应有标题");
    assert!(content.contains("my-proxy"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_without_data_is_safe() {
    let dir = temp_dir("empty");
    let cfg = dir.join("services.yaml");
    let db = dir.join("empty.db");
    let out = bin()
        .args(["init", "--out"])
        .arg(&cfg)
        .arg("--force")
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = bin()
        .args(["status", "--config"])
        .arg(&cfg)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(out.status.success(), "空库 status 不应报错");
    let _ = std::fs::remove_dir_all(&dir);
}
