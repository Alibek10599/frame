mod common;

use common::*;
use std::path::Path;
use std::process::Command;

fn chromium() -> Option<String> {
    let mut candidates = vec![std::env::var("NARI_CHROMIUM").unwrap_or_default()];
    if let Some(home) = std::env::var_os("HOME") {
        let cache = Path::new(&home).join("Library/Caches/ms-playwright");
        let mut shells: Vec<_> = std::fs::read_dir(&cache)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| {
                e.path()
                    .join("chrome-headless-shell-mac-arm64/chrome-headless-shell")
            })
            .collect();
        shells.sort();
        candidates.extend(
            shells
                .into_iter()
                .rev()
                .map(|p| p.to_string_lossy().into_owned()),
        );
    }
    candidates.extend([
        "/opt/homebrew/bin/chromium".into(),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        "/usr/bin/chromium".into(),
        "/usr/bin/google-chrome".into(),
    ]);
    candidates.into_iter().find(|c| {
        !c.is_empty()
            && Path::new(c).exists()
            && Command::new(c)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
    })
}

#[test]
fn browser_journey_create_reload_complete_isolate() {
    let node_ok = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let Some(chrome) = chromium().filter(|_| node_ok) else {
        assert!(
            std::env::var("NARI_REQUIRE_BROWSER").is_err(),
            "NARI_REQUIRE_BROWSER is set but node or chromium is unavailable"
        );
        eprintln!("BROWSER_UNAVAILABLE: node or chromium not found; browser journey not run");
        return;
    };
    let b = fieldwork();
    let dir = scratch("browser");
    let app = start(b, &dir.join("app.db"), &[]).unwrap();
    let url = format!("http://127.0.0.1:{}/#bootstrap={}", app.port, app.bootstrap);
    let out = Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/browser/journey.mjs"
        ))
        .arg(&url)
        .arg(&chrome)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&out.stdout);
    std::fs::write(dir.join("journey.json"), report.as_bytes()).unwrap();
    assert!(
        out.status.success(),
        "browser journey failed:\n{report}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
