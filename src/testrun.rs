use crate::ir::{self, ActionKind, App, Compiled, Type};
use crate::launch::{self, Session, call};
use serde::Serialize;
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const PROTECTED_DIR: &str = "acceptance";

#[derive(Clone, Debug, Serialize)]
pub struct Mismatch {
    pub path: String,
    pub expected: Json,
    pub actual: Json,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub kind: String,
    pub passed: bool,
    pub covers: Vec<String>,
    pub detail: String,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub step: usize,
    pub action: String,
    pub status: u16,
    pub error_code: Option<String>,
    pub mismatches: Vec<Mismatch>,
}

#[derive(Clone, Debug)]
pub struct ProtectedTest {
    pub file: String,
    pub sha256: String,
    pub spec: Json,
}

pub fn load_protected(project_dir: &Path) -> Result<Vec<ProtectedTest>, String> {
    let dir = project_dir.join(PROTECTED_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !crate::is_hidden(p))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        if fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "{} is a symlink; protected tests must be regular files",
                f.display()
            ));
        }
        let bytes = fs::read(&f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
        let spec: Json = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{}: invalid JSON: {e}", f.display()))?;
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        out.push(ProtectedTest {
            file: format!("{PROTECTED_DIR}/{name}"),
            sha256: ir::sha256_hex(&bytes),
            spec,
        });
    }
    Ok(out)
}

pub fn protected_hash(tests: &[ProtectedTest]) -> String {
    let joined: Vec<String> = tests
        .iter()
        .map(|t| format!("{} {}", t.file, t.sha256))
        .collect();
    ir::sha256_hex(joined.join("\n").as_bytes())
}

fn subset(expected: &Json, actual: &Json, path: &str, out: &mut Vec<Mismatch>) {
    match (expected, actual) {
        (Json::Object(e), Json::Object(a)) => {
            for (k, ev) in e {
                let p = format!("{path}.{k}");
                match a.get(k) {
                    Some(av) => subset(ev, av, &p, out),
                    None => out.push(Mismatch {
                        path: p,
                        expected: ev.clone(),
                        actual: Json::Null,
                    }),
                }
            }
        }
        (Json::Array(e), Json::Array(a)) if e.len() == a.len() => {
            for (i, (ev, av)) in e.iter().zip(a).enumerate() {
                subset(ev, av, &format!("{path}[{i}]"), out);
            }
        }
        _ if expected == actual => {}
        _ => out.push(Mismatch {
            path: path.to_string(),
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

fn substitute(v: &Json, saved: &BTreeMap<String, Json>) -> Json {
    match v {
        Json::String(s) if s.starts_with('$') => {
            let mut parts = s[1..].split('.');
            let root = parts.next().and_then(|r| saved.get(r));
            parts
                .try_fold(root.cloned().unwrap_or(Json::Null), |cur, p| {
                    cur.get(p).cloned()
                })
                .unwrap_or(Json::Null)
        }
        Json::Object(m) => Json::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), substitute(v, saved)))
                .collect(),
        ),
        Json::Array(a) => Json::Array(a.iter().map(|v| substitute(v, saved)).collect()),
        other => other.clone(),
    }
}

pub fn run_protected(t: &ProtectedTest, port: u16, a: &Session, b: &Session) -> CheckResult {
    let name = t.spec["name"].as_str().unwrap_or(&t.file).to_string();
    let covers: Vec<String> = t.spec["covers"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut saved: BTreeMap<String, Json> = BTreeMap::new();
    let steps = t.spec["steps"].as_array().cloned().unwrap_or_default();
    let fail = |detail: String, failure: Option<Failure>| CheckResult {
        name: name.clone(),
        kind: "protected".into(),
        passed: false,
        covers: covers.clone(),
        detail,
        failure,
    };
    if steps.is_empty() {
        return fail(format!("{} has no steps", t.file), None);
    }
    for (i, step) in steps.iter().enumerate() {
        let action = step["action"].as_str().unwrap_or_default();
        let session = match step["as"].as_str() {
            Some("a") => Some(a),
            Some("b") => Some(b),
            _ => None,
        };
        let input = substitute(&step["input"], &saved);
        let input = if input.is_null() { json!({}) } else { input };
        let key = step["key"].as_str().map(String::from).unwrap_or_else(|| {
            format!(
                "{}-{i}-{}",
                t.sha256.get(..12).unwrap_or_default(),
                crate::random_hex(8)
            )
        });
        let resp = match call(port, session, action, &input, Some(&key)) {
            Ok(r) => r,
            Err(e) => return fail(format!("step {i} ({action}): request failed: {e}"), None),
        };
        let body = resp.json();
        let expect = &step["expect"];
        let mut mismatches = Vec::new();
        let want_status = expect["status"].as_u64().unwrap_or(200) as u16;
        if resp.status != want_status {
            mismatches.push(Mismatch {
                path: "status".into(),
                expected: json!(want_status),
                actual: json!(resp.status),
            });
        }
        if let Some(code) = expect["error"].as_str()
            && body["error"]["code"].as_str() != Some(code)
        {
            mismatches.push(Mismatch {
                path: "error.code".into(),
                expected: json!(code),
                actual: body["error"]["code"].clone(),
            });
        }
        if !expect["data"].is_null() {
            subset(
                &substitute(&expect["data"], &saved),
                &body["data"],
                "data",
                &mut mismatches,
            );
        }
        if !mismatches.is_empty() {
            let summary: Vec<String> = mismatches
                .iter()
                .map(|m| format!("{}: expected {}, got {}", m.path, m.expected, m.actual))
                .collect();
            return fail(
                format!("step {i} ({action}) failed: {}", summary.join("; ")),
                Some(Failure {
                    step: i,
                    action: action.into(),
                    status: resp.status,
                    error_code: body["error"]["code"].as_str().map(String::from),
                    mismatches,
                }),
            );
        }
        if let Some(name) = step["save"].as_str() {
            saved.insert(name.into(), body["data"].clone());
        }
    }
    CheckResult {
        name,
        kind: "protected".into(),
        passed: true,
        covers,
        detail: format!("{} steps passed", steps.len()),
        failure: None,
    }
}

fn sample(t: &Type) -> Json {
    match t {
        Type::Text {
            min_length,
            max_length,
        } => Json::String("t".repeat((*min_length).max(1).min(*max_length) as usize)),
        Type::Bool => Json::Bool(true),
        Type::Uuid => Json::String("00000000-0000-4000-8000-0000000000ff".into()),
        Type::Int => json!(1),
    }
}

fn sample_input(app: &App, action: &str, record: Option<&Json>) -> Json {
    let a = app.action(action).expect("known action");
    let mut m = Map::new();
    if a.kind == ActionKind::List {
        return json!({});
    }
    for i in &a.inputs {
        let v = match (record, a.id_input.as_deref(), a.revision_input.as_deref()) {
            (Some(r), Some(id), _) if id == i.name => r["id"].clone(),
            (Some(r), _, Some(rev)) if rev == i.name => r["revision"].clone(),
            _ => sample(&i.ty),
        };
        m.insert(i.name.clone(), v);
    }
    Json::Object(m)
}

struct Checks {
    out: Vec<CheckResult>,
}

impl Checks {
    fn check(&mut self, name: String, covers: &str, passed: bool, detail: String) {
        self.out.push(CheckResult {
            name,
            kind: "contract".into(),
            passed,
            covers: vec![covers.to_string()],
            detail,
            failure: None,
        });
    }
}

fn status(r: &std::io::Result<crate::http::Response>) -> u16 {
    r.as_ref().map(|r| r.status).unwrap_or(0)
}

pub fn contract_checks(c: &Compiled, port: u16, a: &Session, b: &Session) -> Vec<CheckResult> {
    let app = &c.app;
    let mut k = Checks { out: Vec::new() };
    let key = || crate::random_hex(16);
    for action in &app.actions {
        let id = &action.id;
        let n = &action.name;
        let input = sample_input(app, n, None);
        let r = call(port, None, n, &input, Some(&key()));
        k.check(
            format!("{n}: unauthenticated request is rejected"),
            id,
            status(&r) == 401,
            format!("status {}", status(&r)),
        );
        let no_csrf = Session {
            csrf: String::new(),
            ..a.clone()
        };
        let r = call(port, Some(&no_csrf), n, &input, Some(&key()));
        k.check(
            format!("{n}: missing CSRF token is rejected"),
            id,
            status(&r) == 403,
            format!("status {}", status(&r)),
        );
        let forged = Session {
            cookie: "nari_session=00".into(),
            ..a.clone()
        };
        let r = call(port, Some(&forged), n, &input, Some(&key()));
        k.check(
            format!("{n}: forged session is rejected"),
            id,
            status(&r) == 401,
            format!("status {}", status(&r)),
        );
        let mut extra = input.clone();
        extra["__nari_unknown"] = json!(1);
        let r = call(port, Some(a), n, &extra, Some(&key()));
        let field_ok = r
            .as_ref()
            .is_ok_and(|r| r.json()["error"]["field"] == "__nari_unknown");
        k.check(
            format!("{n}: unknown input field is rejected"),
            id,
            status(&r) == 400 && field_ok,
            format!("status {}", status(&r)),
        );
        let o = "http://evil.example";
        let body = input.to_string();
        let r = crate::http::request(
            port,
            "POST",
            &format!("/api/actions/{n}"),
            &[
                ("Content-Type", "application/json"),
                ("Origin", o),
                ("Cookie", &a.cookie),
                ("X-Nari-CSRF", &a.csrf),
                ("Idempotency-Key", &key()),
            ],
            body.as_bytes(),
            launch::TIMEOUT,
        );
        k.check(
            format!("{n}: cross-origin request is rejected"),
            id,
            status(&r) == 403,
            format!("status {}", status(&r)),
        );
        if action.kind.mutates() {
            let r = call(port, Some(a), n, &input, None);
            k.check(
                format!("{n}: Idempotency-Key is required"),
                id,
                status(&r) == 400,
                format!("status {}", status(&r)),
            );
        }
        for i in &action.inputs {
            if let Type::Text { max_length, .. } = i.ty {
                let mut long = input.clone();
                long[&i.name] = Json::String("x".repeat(max_length as usize + 1));
                let r = call(port, Some(a), n, &long, Some(&key()));
                let field_ok = r
                    .as_ref()
                    .is_ok_and(|r| r.json()["error"]["field"] == i.name.as_str());
                k.check(
                    format!(
                        "{n}: input '{}' longer than {max_length} is rejected",
                        i.name
                    ),
                    &i.id,
                    status(&r) == 400 && field_ok,
                    format!("status {}", status(&r)),
                );
            }
        }
    }
    for ent in &app.entities {
        let Some(insert) = app
            .actions
            .iter()
            .find(|x| x.kind == ActionKind::Insert && x.entity == ent.name)
        else {
            continue;
        };
        let lists: Vec<_> = app
            .actions
            .iter()
            .filter(|x| x.kind == ActionKind::List && x.entity == ent.name)
            .collect();
        let created = call(
            port,
            Some(a),
            &insert.name,
            &sample_input(app, &insert.name, None),
            Some(&key()),
        );
        let record = created
            .as_ref()
            .map(|r| r.json()["data"].clone())
            .unwrap_or(Json::Null);
        let owned = record[&ent.owner_field] == json!(a.actor);
        k.check(
            format!(
                "{}: insert assigns the record to the acting user",
                insert.name
            ),
            &insert.id,
            status(&created) == 200 && owned,
            format!("status {}", status(&created)),
        );
        let rid = record["id"].clone();
        for l in &lists {
            let mut seen_a = false;
            let mut seen_b = false;
            for (s, seen) in [(a, &mut seen_a), (b, &mut seen_b)] {
                let mut after = Json::Null;
                loop {
                    let r = call(
                        port,
                        Some(s),
                        &l.name,
                        &json!({ "limit": 100, "after": after }),
                        None,
                    );
                    let page = r.map(|r| r.json()["data"].clone()).unwrap_or(Json::Null);
                    if page["items"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|i| i["id"] == rid))
                    {
                        *seen = true;
                    }
                    after = page["next_cursor"].clone();
                    if after.is_null() {
                        break;
                    }
                }
            }
            k.check(
                format!("{}: owner sees own record; another user does not", l.name),
                &l.id,
                seen_a && !seen_b,
                format!("owner sees: {seen_a}, other user sees: {seen_b}"),
            );
        }
        for up in app
            .actions
            .iter()
            .filter(|x| x.kind == ActionKind::Update && x.entity == ent.name)
        {
            let input = sample_input(app, &up.name, Some(&record));
            let r = call(port, Some(b), &up.name, &input, Some(&key()));
            k.check(
                format!("{}: another user cannot update the record", up.name),
                &up.id,
                status(&r) == 404,
                format!("status {}", status(&r)),
            );
            let mut stale = input.clone();
            if let Some(rev) = &up.revision_input {
                stale[rev] = json!(record["revision"].as_i64().unwrap_or(1) + 7);
            }
            let r = call(port, Some(a), &up.name, &stale, Some(&key()));
            let code_ok = r
                .as_ref()
                .is_ok_and(|r| r.json()["error"]["code"] == "REVISION_CONFLICT");
            k.check(
                format!("{}: stale revision is rejected with a conflict", up.name),
                &up.id,
                status(&r) == 409 && code_ok,
                format!("status {}", status(&r)),
            );
        }
    }
    k.out
}

pub struct RunOptions<'a> {
    pub binary: &'a Path,
    pub cwd: &'a Path,
    pub run_dir: &'a Path,
    pub wrapper: Vec<String>,
}

pub struct TestRun {
    pub results: Vec<CheckResult>,
    pub protected_sha256: String,
}

pub fn run_all(
    c: &Compiled,
    tests: &[ProtectedTest],
    o: &RunOptions,
    contracts: bool,
) -> Result<TestRun, String> {
    fs::create_dir_all(o.run_dir).map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    if contracts {
        let mut app = start(o, "contracts")?;
        let (a, b) = launch::dev_sessions(app.port, &app.bootstrap)?;
        results.extend(contract_checks(c, app.port, &a, &b));
        app.stop();
    }
    for (i, t) in tests.iter().enumerate() {
        let mut app = start(o, &format!("protected-{i}"))?;
        let (a, b) = launch::dev_sessions(app.port, &app.bootstrap)?;
        let mut r = run_protected(t, app.port, &a, &b);
        r.name = format!("{} ({})", r.name, t.file);
        results.push(r);
        app.stop();
    }
    Ok(TestRun {
        results,
        protected_sha256: protected_hash(tests),
    })
}

fn start(o: &RunOptions, label: &str) -> Result<launch::AppProcess, String> {
    let db = o.run_dir.join(format!("{label}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    launch::AppProcess::start(&launch::LaunchOptions {
        binary: o.binary.to_path_buf(),
        cwd: o.cwd.to_path_buf(),
        database: db,
        port: 0,
        log: Some(o.run_dir.join(format!("{label}.log"))),
        env: Vec::new(),
        wrapper: o.wrapper.clone(),
    })
}
