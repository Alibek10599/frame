use crate::Project;
use crate::artifacts;
use crate::codegen;
use crate::context;
use crate::ir::{self, Compiled};
use crate::patch::{self, Edit, Patch, Rejected};
use crate::pipeline;
use crate::progress::TaskRecord;
use crate::sandbox::{self, Isolation, Phase, Sandbox};
use crate::supervisor::{Control, now_ms};
use crate::syntax::{self, Entry, ValueKind};
use crate::testrun::{self, CheckResult, ProtectedTest};
use serde_json::{Value as Json, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub enum Proposer {
    Fixture,
    File(PathBuf),
}

impl Proposer {
    pub fn parse(s: &str) -> Result<Proposer, String> {
        match s {
            "fixture" => Ok(Proposer::Fixture),
            _ => match s.strip_prefix("file:") {
                Some(p) => Ok(Proposer::File(PathBuf::from(p))),
                None => Err(format!(
                    "unknown proposer '{s}' (expected fixture or file:PATH; live proposers are not part of 0.1)"
                )),
            },
        }
    }

    fn label(&self) -> String {
        match self {
            Proposer::Fixture => "fixture (deterministic)".into(),
            Proposer::File(p) => format!("file:{} (untrusted proposal data)", p.display()),
        }
    }
}

pub struct Options {
    pub nari_exe: PathBuf,
    pub proposer: Proposer,
}

pub struct Outcome {
    pub exit: u8,
    pub state: String,
    pub report: Json,
}

pub fn signature(file: &str, f: &testrun::Failure) -> String {
    let mut paths: Vec<&str> = f.mismatches.iter().map(|m| m.path.as_str()).collect();
    paths.sort();
    format!("test:{file}:step{}:{}", f.step, paths.join(","))
}

pub fn compat_key(c: &Compiled) -> String {
    let (_, schema) = codegen::schema(&c.app);
    ir::sha256_hex(
        format!(
            "{}|{}|{}|imanari-runtime/0.1",
            ir::COMPILER_VERSION,
            pipeline::lock_hash(),
            schema
        )
        .as_bytes(),
    )
}

pub async fn record_test_failures(
    p: &Project,
    c: &Compiled,
    tests: &[ProtectedTest],
    results: &[CheckResult],
) -> Result<Vec<String>, String> {
    let ctl = Control::open(p).await?;
    let mut ids = Vec::new();
    for r in results
        .iter()
        .filter(|r| !r.passed && r.kind == "protected")
    {
        let Some(f) = &r.failure else { continue };
        let Some(t) = tests
            .iter()
            .find(|t| r.name.ends_with(&format!("({})", t.file)))
        else {
            continue;
        };
        let sig = signature(&t.file, f);
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM incidents WHERE signature = ?1 AND outcome = 'open' AND record LIKE ?2",
        )
        .bind(&sig)
        .bind(format!("%{}%", c.source_sha256))
        .fetch_optional(&ctl.db)
        .await
        .map_err(|e| e.to_string())?;
        if let Some(id) = existing {
            ids.push(id);
            continue;
        }
        let mut rec = crate::supervisor::incident(
            c,
            "test_failure",
            &sig,
            json!({
                "test_file": t.file,
                "test_sha256": t.sha256,
                "test_name": t.spec["name"],
                "step": f.step,
                "action": f.action,
                "status": f.status,
                "error_code": f.error_code,
                "mismatches": f.mismatches,
            }),
            Json::Null,
            "open",
        );
        rec["target_id"] = json!(format!("{}/action/{}", c.app.name, f.action));
        rec["reproduction"] =
            json!({ "protected_test": t.file, "sha256": t.sha256, "step": f.step });
        rec["hypotheses"] = json!([{ "kind": "inferred", "text": format!("behavior of {} diverges from a protected expectation", f.action) }]);
        ctl.record_incident(&rec, false).await?;
        ids.push(rec["incident_id"].as_str().unwrap_or_default().to_string());
    }
    Ok(ids)
}

fn nari_literal(v: &Json) -> Option<String> {
    match v {
        Json::Bool(b) => Some(b.to_string()),
        Json::Number(n) => n.as_i64().map(|n| n.to_string()),
        Json::String(s) => serde_json::to_string(s).ok(),
        _ => None,
    }
}

pub fn fixture_propose(
    incident: &Json,
    bundle: &context::Bundle,
    precedents: &[Json],
) -> Result<Patch, String> {
    let target = incident["target_id"]
        .as_str()
        .ok_or("incident has no target")?;
    let mismatch = incident["observed"]["mismatches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["path"].as_str().is_some_and(|p| p.starts_with("data.") && !p[5..].contains(['.', '['])))
        .ok_or("no observed field mismatch to act on; the fixture proposer only repairs literal set bindings")?;
    let field = &mismatch["path"].as_str().unwrap_or_default()[5..];
    let src = &bundle.manifest["target_source"];
    let (file, start, text) = (
        src["file"].as_str().ok_or("context has no target source")?,
        src["start"].as_u64().ok_or("context has no target span")? as usize,
        src["text"].as_str().ok_or("context has no target text")?,
    );
    let prefix = "imanari \"0.1\";\napp snippet {\n";
    let doc = syntax::parse(&format!("{prefix}{text}\n}}\n"), "snippet")
        .map_err(|d| format!("cannot parse target snippet: {}", d.message))?;
    let Some(Entry::Block(block)) = doc.entries.first() else {
        return Err("target snippet is not a block".into());
    };
    let value = block
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::Property(p) if p.key.name == "set" => match &p.value.kind {
                ValueKind::Object(members) => members
                    .iter()
                    .find(|(k, _)| k.name == field)
                    .map(|(_, v)| v.clone()),
                _ => None,
            },
            _ => None,
        })
        .ok_or_else(|| format!("{target} has no set binding for '{field}'"))?;
    if matches!(value.kind, ValueKind::Reference(_)) {
        return Err(format!(
            "binding for '{field}' is a reference, not a literal; no deterministic proposal"
        ));
    }
    let precedent = precedents.iter().find(|p| {
        p["reusable"] == true && p["intervention"]["field"] == field && p["target_id"] == target
    });
    let replacement = match precedent.and_then(|p| p["intervention"]["replacement"].as_str()) {
        Some(r) => r.to_string(),
        None => {
            nari_literal(&mismatch["expected"]).ok_or("expected value is not a scalar literal")?
        }
    };
    let base = &bundle.manifest["patch_preconditions"];
    Ok(Patch {
        schema: patch::SCHEMA.into(),
        base_source_sha256: base["base_source_sha256"]
            .as_str()
            .unwrap_or_default()
            .into(),
        base_graph_sha256: base["base_graph_sha256"]
            .as_str()
            .unwrap_or_default()
            .into(),
        affected_nodes: vec![target.into()],
        allowed_scope: vec![target.into()],
        edits: vec![Edit {
            file: file.into(),
            start: start + value.span.start - prefix.len(),
            end: start + value.span.end - prefix.len(),
            replacement: replacement.clone(),
        }],
        rationale: format!(
            "observed {} = {} where protected test expects {}; replace the literal set binding for '{field}' with {replacement}{}",
            mismatch["path"].as_str().unwrap_or_default(),
            mismatch["actual"],
            mismatch["expected"],
            precedent
                .map(|p| format!(
                    " (compatible validated precedent {})",
                    p["id"].as_str().unwrap_or_default()
                ))
                .unwrap_or_default()
        ),
    })
}

pub fn validate_static(
    base: &Compiled,
    base_src: &str,
    project_file: &str,
    p: &Patch,
) -> Result<(String, Compiled, Vec<String>), Rejected> {
    if p.affected_nodes.is_empty() || p.allowed_scope.is_empty() {
        return Err(Rejected {
            code: "PATCH_INVALID",
            message: "patch must declare affected nodes and an allowed scope".into(),
        });
    }
    let text = patch::apply_text(p, project_file, base_src)?;
    let cand = crate::compile(&text, project_file).map_err(|ds| Rejected {
        code: "PATCH_CHECK_FAILED",
        message: ds
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.render())
            .collect::<Vec<_>>()
            .join("; "),
    })?;
    let changed = patch::check_scope(base, &cand.0, &p.allowed_scope)?;
    if pipeline::policy_hash(base) != pipeline::policy_hash(&cand.0) {
        return Err(Rejected {
            code: "PATCH_SCOPE_VIOLATION",
            message: "policy hash changed".into(),
        });
    }
    Ok((text, cand.0, changed))
}

struct Store<'a> {
    rt: &'a tokio::runtime::Runtime,
    ctl: &'a Control,
    repair_id: String,
    incident_id: String,
    signature: String,
    target_id: String,
    attempt: i64,
    policy: String,
    source: String,
    compat: String,
}

impl Store<'_> {
    fn transition(
        &self,
        state: &str,
        candidate: Option<&str>,
        outcome: &str,
        evidence: &Json,
    ) -> Result<(), String> {
        let ev = ir::canonical_compact(&crate::supervisor::redact(evidence));
        self.rt.block_on(async {
            let mut tx = self.ctl.db.begin().await.map_err(|e| e.to_string())?;
            sqlx::query(
                "INSERT INTO repairs (id, incident_id, signature, target_id, attempt, state, candidate_hash, policy_sha256, source_sha256, compat, outcome, evidence, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET state = excluded.state, candidate_hash = COALESCE(excluded.candidate_hash, repairs.candidate_hash), outcome = excluded.outcome, evidence = excluded.evidence, updated_ms = excluded.updated_ms",
            )
            .bind(&self.repair_id)
            .bind(&self.incident_id)
            .bind(&self.signature)
            .bind(&self.target_id)
            .bind(self.attempt)
            .bind(state)
            .bind(candidate)
            .bind(&self.policy)
            .bind(&self.source)
            .bind(&self.compat)
            .bind(outcome)
            .bind(&ev)
            .bind(now_ms())
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            sqlx::query("INSERT INTO repair_transitions (repair_id, incident_id, state, candidate_hash, attempt, policy_sha256, evidence, at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")
                .bind(&self.repair_id)
                .bind(&self.incident_id)
                .bind(state)
                .bind(candidate)
                .bind(self.attempt)
                .bind(&self.policy)
                .bind(&ev)
                .bind(now_ms())
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            tx.commit().await.map_err(|e| e.to_string())
        })
    }
}

pub async fn precedents(
    ctl: &Control,
    signature: &str,
    compat: &str,
    exclude: &str,
) -> Result<Vec<Json>, String> {
    let rows: Vec<(String, String, String, String, Option<String>, String, String, i64)> = sqlx::query_as(
        "SELECT id, target_id, state, compat, candidate_hash, outcome, evidence, attempt FROM repairs WHERE signature = ?1 AND id != ?2 ORDER BY updated_ms",
    )
    .bind(signature)
    .bind(exclude)
    .fetch_all(&ctl.db)
    .await
    .map_err(|e| e.to_string())?;
    let mut out: Vec<Json> = rows
        .into_iter()
        .map(|(id, target, state, c, cand, outcome, ev, attempt)| {
            let compatible = c == compat;
            let validated = state == "Validated" || state == "Applied";
            json!({
                "id": id, "target_id": target, "state": state, "candidate_hash": cand, "outcome": outcome, "attempt": attempt,
                "compatible": compatible,
                "reusable": compatible && validated,
                "note": if !compatible { "excluded: incompatible compiler/dependency/schema versions" } else if validated { "validated precedent; ranked first, still revalidated" } else { "failed or inconclusive attempt; visible, not reused" },
                "intervention": serde_json::from_str::<Json>(&ev).map(|e| e["evidence"].clone()).unwrap_or(Json::Null),
            })
        })
        .collect();
    out.sort_by_key(|p| (p["reusable"] != true, p["compatible"] != true));
    Ok(out)
}

fn snapshot(p: &Project, src: &str, ws: &Path, tests: &[ProtectedTest]) -> Result<PathBuf, String> {
    let source_dir = ws.join("source");
    fs::create_dir_all(&source_dir).map_err(|e| e.to_string())?;
    fs::write(source_dir.join(&p.file_name), src).map_err(|e| e.to_string())?;
    let protected = ws.join("protected");
    let acc = protected.join(testrun::PROTECTED_DIR);
    fs::create_dir_all(&acc).map_err(|e| e.to_string())?;
    for t in tests {
        let name = Path::new(&t.file).file_name().ok_or("bad test path")?;
        let dest = acc.join(name);
        fs::copy(p.dir.join(&t.file), &dest).map_err(|e| e.to_string())?;
        artifacts::restrict(&dest, 0o444);
    }
    artifacts::restrict(&acc, 0o555);
    artifacts::restrict(&protected, 0o555);
    Ok(protected)
}

fn clone_cache(from: &Path, to: &Path) {
    if from.is_dir() && !to.exists() {
        let _ = Command::new("/bin/cp")
            .arg("-cR")
            .arg(from)
            .arg(to)
            .stdin(Stdio::null())
            .output();
    }
}

fn build_isolated(
    ws_out: &Path,
    c: &Compiled,
    sb: &Sandbox,
    target: &Path,
) -> Result<PathBuf, String> {
    let package = pipeline::package_name_for(ws_out, &c.app.name);
    fs::create_dir_all(ws_out).map_err(|e| e.to_string())?;
    let generated = pipeline::generate(ws_out, c, &package)?;
    let manifest = ws_out.join("generated/app/Cargo.toml");
    let fetched = Command::new(pipeline::cargo())
        .args(["fetch", "--manifest-path"])
        .arg(&manifest)
        .current_dir(ws_out.join("generated/app"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot prefetch dependencies: {e}"))?;
    if !fetched.status.success() {
        return Err(format!(
            "trusted dependency prefetch failed: {}",
            String::from_utf8_lossy(&fetched.stderr).trim()
        ));
    }
    pipeline::compile_app(c, ws_out, &generated, &package, target, Some(sb))
        .map_err(|(_, ds)| ds.iter().map(|d| d.render()).collect::<Vec<_>>().join("\n"))
}

fn run_suite(
    c: &Compiled,
    binary: &Path,
    ws: &Path,
    label: &str,
    tests: &[ProtectedTest],
    sb: &Sandbox,
    contracts: bool,
) -> Result<Vec<CheckResult>, String> {
    let run = testrun::run_all(
        c,
        tests,
        &testrun::RunOptions {
            binary,
            cwd: ws,
            run_dir: &ws.join("run").join(label),
            wrapper: sb.wrapper(Phase::Run),
        },
        contracts,
    )?;
    Ok(run.results)
}

fn load_snapshot_tests(protected: &Path) -> Result<Vec<ProtectedTest>, String> {
    testrun::load_protected(protected)
}

pub fn run(
    p: &Project,
    c: &Compiled,
    src: &str,
    incident_id: &str,
    o: &Options,
) -> Result<Outcome, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let ctl = rt.block_on(Control::open(p))?;
    let incident = rt
        .block_on(ctl.incidents(Some(incident_id), 1))?
        .into_iter()
        .next()
        .ok_or_else(|| format!("E_UNKNOWN_INCIDENT: no incident {incident_id}"))?;
    if incident["kind"] != "test_failure" {
        return Err(format!(
            "E_REPAIR_UNSUPPORTED: incident kind '{}' has no code repair path in 0.1 (recovery handles process failures)",
            incident["kind"].as_str().unwrap_or("?")
        ));
    }
    let target = incident["target_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let sig = incident["failure_signature"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let action = target.rsplit('/').next().unwrap_or_default().to_string();
    let policy = &c.app.repair;
    let compat = compat_key(c);
    let used: i64 = rt
        .block_on(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repairs WHERE signature = ?1 AND state != 'Applied' AND updated_ms > COALESCE((SELECT max(updated_ms) FROM repairs WHERE signature = ?1 AND state = 'Applied'), 0)")
                .bind(&sig)
                .fetch_one(&ctl.db),
        )
        .map_err(|e| e.to_string())?;
    let attempt = used + 1;
    let prior_rows: i64 = rt
        .block_on(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repairs WHERE incident_id = ?1")
                .bind(incident_id)
                .fetch_one(&ctl.db),
        )
        .map_err(|e| e.to_string())?;
    let store = Store {
        rt: &rt,
        ctl: &ctl,
        repair_id: format!(
            "rep-{}-{}",
            incident_id.trim_start_matches("inc-"),
            prior_rows + 1
        ),
        incident_id: incident_id.into(),
        signature: sig.clone(),
        target_id: target.clone(),
        attempt,
        policy: pipeline::policy_hash(c),
        source: c.source_sha256.clone(),
        compat: compat.clone(),
    };
    let mut report = json!({
        "schema": "nari-repair/0.1",
        "repair_id": store.repair_id,
        "incident_id": incident_id,
        "target_id": target,
        "failure_signature": sig,
        "attempt": attempt,
        "max_attempts": policy.max_attempts,
        "policy_sha256": store.policy,
        "proposer": o.proposer.label(),
        "release": "never: Validated means eligible for explicit local apply only",
    });
    let finish = |state: &str,
                  outcome: &str,
                  exit: u8,
                  report: &mut Json,
                  candidate: Option<&str>|
     -> Result<Outcome, String> {
        report["state"] = json!(state);
        report["outcome"] = json!(outcome);
        store.transition(state, candidate, outcome, report)?;
        Ok(Outcome {
            exit,
            state: state.into(),
            report: report.clone(),
        })
    };
    if !policy.editable_actions.contains(&action) {
        report["reason"] = json!(format!(
            "{target} is not in repair.editable_actions; the repair controller cannot authorize this change"
        ));
        return finish("Rejected", "POLICY_NOT_EDITABLE", 4, &mut report, None);
    }
    if used >= i64::from(policy.max_attempts) {
        report["reason"] = json!(format!(
            "{used} attempts already recorded for this failure signature (max_attempts {})",
            policy.max_attempts
        ));
        report["attempt"] = json!(used);
        let exhausted = Store {
            repair_id: format!("{}-exhausted", store.repair_id),
            ..store
        };
        report["state"] = json!("Exhausted");
        report["outcome"] = json!("REPAIR_EXHAUSTED");
        exhausted.transition("Exhausted", None, "REPAIR_EXHAUSTED", &report)?;
        return Ok(Outcome {
            exit: 4,
            state: "Exhausted".into(),
            report,
        });
    }
    store.transition("Detected", None, "in_progress", &report)?;
    let repair_dir = artifacts::prepare(p)?.join("repair");
    artifacts::ensure_real_dir(&repair_dir)?;
    let inc_dir = artifacts::safe_join(&repair_dir, incident_id)?;
    artifacts::ensure_real_dir(&inc_dir)?;
    let ws = inc_dir.join(format!("attempt-{attempt}"));
    if ws.exists() {
        let _ = Command::new("/bin/chmod")
            .args(["-R", "u+w"])
            .arg(&ws)
            .output();
        fs::remove_dir_all(&ws).map_err(|e| format!("cannot reset workspace: {e}"))?;
    }
    fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
    let ws = fs::canonicalize(&ws).map_err(|e| e.to_string())?;
    let mut task = TaskRecord::new(
        &store.repair_id,
        &format!("repair {sig}"),
        &c.source_sha256,
        &c.graph_sha256,
    );
    task.outstanding_checks = vec![
        "reproduce".into(),
        "propose".into(),
        "static validation".into(),
        "isolated build".into(),
        "protected suite".into(),
    ];
    let task_path = ws.join("task.json");
    let tests = testrun::load_protected(&p.dir)?;
    let protected = snapshot(p, src, &ws, &tests)?;
    let snap_tests = load_snapshot_tests(&protected)?;
    let protected_sha = testrun::protected_hash(&snap_tests);
    report["snapshot"] = json!({
        "source_sha256": c.source_sha256,
        "protected_tests_sha256": protected_sha,
        "protected_tests": snap_tests.iter().map(|t| json!({ "file": t.file, "sha256": t.sha256 })).collect::<Vec<_>>(),
        "workspace": ws.strip_prefix(&p.dir).unwrap_or(&ws),
    });
    let rel = |path: &Path| {
        path.strip_prefix(&p.dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let incident_source = incident["versions"]["source_sha256"]
        .as_str()
        .unwrap_or_default();
    if incident_source != c.source_sha256 {
        report["reproduction"] = json!({ "result": "not attempted", "reason": "incident was recorded against a different source revision" });
        return finish(
            "Inconclusive",
            "SOURCE_CHANGED_SINCE_INCIDENT",
            1,
            &mut report,
            None,
        );
    }
    let sb = Sandbox {
        workspace: ws.clone(),
        protected: protected.clone(),
        denied_reads: sandbox::default_denied_reads(&p.dir, &c.app.runtime.database),
        nari_exe: o.nari_exe.clone(),
    };
    let isolation = sandbox::detect(sb);
    let failing_test: Vec<ProtectedTest> = snap_tests
        .iter()
        .filter(|t| Some(t.file.as_str()) == incident["reproduction"]["protected_test"].as_str())
        .cloned()
        .collect();
    if failing_test.first().map(|t| t.sha256.as_str())
        != incident["reproduction"]["sha256"].as_str()
    {
        report["reproduction"] = json!({ "result": "not attempted", "reason": "protected reproduction test is missing or changed" });
        return finish(
            "Inconclusive",
            "REPRODUCTION_FIXTURE_CHANGED",
            1,
            &mut report,
            None,
        );
    }
    let target_cache = pipeline::target_dir(p);
    match &isolation {
        Isolation::Seatbelt(sb) => {
            report["isolation"] = json!({
                "kind": "macOS Seatbelt (sandbox-exec)",
                "self_test": "passed: writes outside the workspace and to protected fixtures denied; outbound network denied",
                "build_phase": "no network, offline cargo, writes confined to the workspace",
                "run_phase": "loopback inbound only, no fork, no outbound network",
                "limits": { "build": format!("{:?}", sandbox::Limits::for_phase(Phase::Build)), "run": format!("{:?}", sandbox::Limits::for_phase(Phase::Run)) },
                "not_enforced": "memory and process-count rlimits are not enforced by macOS for this use; bounded by wall-clock and CPU limits",
                "env": "cleared; no credentials passed",
            });
            clone_cache(&target_cache, &ws.join("target"));
            let base_bin = build_isolated(&ws.join("base"), c, sb, &ws.join("target"))
                .map_err(|e| format!("E_REPAIR_BUILD: base build failed in sandbox: {e}"))?;
            let results = run_suite(c, &base_bin, &ws, "reproduce", &failing_test, sb, false)?;
            let r = results.first().ok_or("reproduction produced no result")?;
            let reproduced = !r.passed
                && r.failure
                    .as_ref()
                    .is_some_and(|f| signature(&failing_test[0].file, f) == sig);
            report["reproduction"] = json!({ "result": if reproduced { "reproduced" } else { "not reproduced" }, "check": r });
            if !reproduced {
                return finish("Inconclusive", "NOT_REPRODUCED", 1, &mut report, None);
            }
            task.completed.push("reproduced in sandbox".into());
            store.transition("Reproduced", None, "in_progress", &report)?;
        }
        Isolation::Unavailable(why) => {
            report["isolation"] = json!({ "kind": "unavailable", "reason": why });
            report["reproduction"] = json!({ "result": "SANDBOX_UNAVAILABLE", "reason": "dynamic reproduction requires isolation" });
        }
    }
    task.outstanding_checks.retain(|c| c != "reproduce");
    task.save(&task_path)?;

    let bundle = context::extract(p, c, src, &target, context::DEFAULT_MAX_BYTES)?;
    let ctx_dir = ws.join("context");
    fs::create_dir_all(&ctx_dir).map_err(|e| e.to_string())?;
    fs::write(ctx_dir.join("context.md"), &bundle.markdown).map_err(|e| e.to_string())?;
    fs::write(
        ctx_dir.join("manifest.json"),
        ir::canonical(&bundle.manifest),
    )
    .map_err(|e| e.to_string())?;
    let prior = rt.block_on(precedents(&ctl, &sig, &compat, &store.repair_id))?;
    report["precedents"] = json!(prior);
    let proposal = match &o.proposer {
        Proposer::Fixture => fixture_propose(&incident, &bundle, &prior),
        Proposer::File(path) => fs::read(path)
            .map_err(|e| format!("cannot read proposal: {e}"))
            .and_then(|b| {
                serde_json::from_slice::<Patch>(&b)
                    .map_err(|e| format!("proposal is not a valid patch document: {e}"))
            }),
    };
    let patch = match proposal {
        Ok(p) => p,
        Err(e) => {
            report["proposal"] = json!({ "error": e });
            return finish("Rejected", "NO_VALID_PROPOSAL", 4, &mut report, None);
        }
    };
    let candidate_hash = patch.hash();
    fs::write(ws.join("patch.json"), ir::canonical(&ir::to_json(&patch)))
        .map_err(|e| e.to_string())?;
    report["proposal"] = json!({ "candidate_hash": candidate_hash, "patch": ir::to_json(&patch) });
    store.transition("Proposed", Some(&candidate_hash), "in_progress", &report)?;
    task.completed
        .push(format!("proposed candidate {candidate_hash}"));
    task.outstanding_checks.retain(|c| c != "propose");
    store.transition("Validating", Some(&candidate_hash), "in_progress", &report)?;
    let snapshot_src =
        fs::read_to_string(ws.join("source").join(&p.file_name)).map_err(|e| e.to_string())?;
    let (cand_src, cand, changed) = match validate_static(c, &snapshot_src, &p.file_name, &patch) {
        Ok(v) => v,
        Err(rej) => {
            report["static_validation"] =
                json!({ "result": "rejected", "code": rej.code, "message": rej.message });
            return finish("Rejected", rej.code, 4, &mut report, Some(&candidate_hash));
        }
    };
    let field = patch
        .edits
        .first()
        .map(|e| e.replacement.clone())
        .unwrap_or_default();
    report["static_validation"] = json!({
        "result": "passed",
        "changed_nodes": changed,
        "candidate_source_sha256": cand.source_sha256,
        "candidate_semantic_sha256": cand.semantic_sha256,
        "checks": ["path policy", "stale precondition", "full recheck", "editable set-binding scope", "permissions/schemas/effects/policy unchanged", "dependency lock unchanged"],
    });
    report["evidence"] = json!({ "field": incident["observed"]["mismatches"][0]["path"].as_str().unwrap_or_default().trim_start_matches("data."), "replacement": field });
    task.completed.push("static validation passed".into());
    task.outstanding_checks.retain(|c| c != "static validation");
    task.save(&task_path)?;
    let Isolation::Seatbelt(sb) = &isolation else {
        report["dynamic_validation"] = json!({ "result": "SANDBOX_UNAVAILABLE" });
        return finish(
            "Validating",
            "SANDBOX_UNAVAILABLE",
            3,
            &mut report,
            Some(&candidate_hash),
        );
    };
    let cand_dir = ws.join("candidate");
    fs::create_dir_all(&cand_dir).map_err(|e| e.to_string())?;
    fs::write(cand_dir.join(&p.file_name), &cand_src).map_err(|e| e.to_string())?;
    let cand_bin = match build_isolated(&cand_dir, &cand, sb, &ws.join("target")) {
        Ok(b) => b,
        Err(e) => {
            report["dynamic_validation"] =
                json!({ "result": "rejected", "stage": "isolated build", "detail": e });
            return finish(
                "Rejected",
                "CANDIDATE_BUILD_FAILED",
                4,
                &mut report,
                Some(&candidate_hash),
            );
        }
    };
    task.completed.push("candidate built in sandbox".into());
    let results = run_suite(&cand, &cand_bin, &ws, "validate", &snap_tests, sb, true)?;
    let after = load_snapshot_tests(&protected)?;
    let fixtures_intact = testrun::protected_hash(&after) == protected_sha;
    let repro_fixed = results
        .iter()
        .any(|r| r.passed && r.name.ends_with(&format!("({})", failing_test[0].file)));
    let all_pass = results.iter().all(|r| r.passed);
    let running_unchanged = fs::read(&p.file)
        .map(|b| ir::sha256_hex(&b))
        .ok()
        .as_deref()
        == Some(c.source_sha256.as_str());
    report["dynamic_validation"] = json!({
        "result": if all_pass && repro_fixed && fixtures_intact { "passed" } else { "failed" },
        "reproduction_before": "failed",
        "reproduction_after": if repro_fixed { "passed" } else { "failed" },
        "protected_suite": results.iter().filter(|r| r.kind == "protected").map(|r| json!({ "name": r.name, "passed": r.passed })).collect::<Vec<_>>(),
        "contract_checks": { "passed": results.iter().filter(|r| r.kind == "contract" && r.passed).count(), "total": results.iter().filter(|r| r.kind == "contract").count() },
        "failures": results.iter().filter(|r| !r.passed).collect::<Vec<_>>(),
        "protected_fixtures_unchanged": fixtures_intact,
    });
    report["running_source_unchanged"] = json!(running_unchanged);
    if !(all_pass && repro_fixed && fixtures_intact && running_unchanged) {
        return finish(
            "Rejected",
            "CANDIDATE_FAILED_VALIDATION",
            4,
            &mut report,
            Some(&candidate_hash),
        );
    }
    task.completed
        .push("protected suite passed against candidate".into());
    task.outstanding_checks = vec![format!(
        "explicit local apply: nari apply-repair {incident_id} --candidate {candidate_hash}"
    )];
    task.state = "validated".into();
    let cands = inc_dir.join("candidates");
    artifacts::ensure_real_dir(&cands)?;
    let stored = json!({
        "schema": "nari-candidate/0.1",
        "incident_id": incident_id,
        "repair_id": store.repair_id,
        "candidate_hash": candidate_hash,
        "patch": ir::to_json(&patch),
        "base_source_sha256": c.source_sha256,
        "protected_tests_sha256": protected_sha,
        "compat": compat,
        "report": format!("{}/report.json", rel(&ws)),
    });
    artifacts::write_file(
        &cands,
        &format!("{candidate_hash}.json"),
        ir::canonical(&stored) + "\n",
    )?;
    let mut outcome = finish(
        "Validated",
        "VALIDATED",
        0,
        &mut report,
        Some(&candidate_hash),
    )?;
    fs::write(
        ws.join("report.json"),
        ir::canonical(&outcome.report) + "\n",
    )
    .map_err(|e| e.to_string())?;
    task.add_evidence(&p.dir, &rel(&ws.join("report.json")), "repair-report")?;
    task.add_evidence(
        &p.dir,
        &rel(&cands.join(format!("{candidate_hash}.json"))),
        "candidate",
    )?;
    task.save(&task_path)?;
    outcome.report["evidence_paths"] = json!({ "report": rel(&ws.join("report.json")), "candidate": rel(&cands.join(format!("{candidate_hash}.json"))), "task": rel(&task_path) });
    Ok(outcome)
}

pub fn apply(
    p: &Project,
    c: &Compiled,
    src: &str,
    incident_id: &str,
    candidate_hash: &str,
    o: &Options,
) -> Result<Outcome, String> {
    if !candidate_hash.chars().all(|ch| ch.is_ascii_hexdigit()) || candidate_hash.len() != 64 {
        return Err("E_USAGE: candidate must be a 64-character hex hash".into());
    }
    let inc_dir = artifacts::safe_join(&p.artifacts().join("repair"), incident_id)?;
    let path = inc_dir
        .join("candidates")
        .join(format!("{candidate_hash}.json"));
    let stored: Json = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            format!("E_UNKNOWN_CANDIDATE: no stored candidate {candidate_hash} for {incident_id}")
        })?;
    let patch: Patch = serde_json::from_value(stored["patch"].clone())
        .map_err(|e| format!("stored candidate is corrupt: {e}"))?;
    let reject = |code: &str, msg: String| {
        Ok(Outcome {
            exit: 4,
            state: "Rejected".into(),
            report: json!({ "code": code, "message": msg, "candidate_hash": candidate_hash }),
        })
    };
    if patch.hash() != candidate_hash {
        return reject(
            "CANDIDATE_TAMPERED",
            "stored patch does not match its hash".into(),
        );
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let ctl = rt.block_on(Control::open(p))?;
    let state: Option<String> = rt
        .block_on(sqlx::query_scalar("SELECT state FROM repairs WHERE incident_id = ?1 AND candidate_hash = ?2 ORDER BY updated_ms DESC LIMIT 1").bind(incident_id).bind(candidate_hash).fetch_optional(&ctl.db))
        .map_err(|e| e.to_string())?;
    if state.as_deref() != Some("Validated") {
        return reject(
            "CANDIDATE_NOT_VALIDATED",
            format!(
                "candidate state is {}",
                state.as_deref().unwrap_or("unknown")
            ),
        );
    }
    if c.source_sha256 != patch.base_source_sha256 {
        return reject(
            "PATCH_STALE",
            "source changed since the candidate was validated; re-run nari repair".into(),
        );
    }
    let tests = testrun::load_protected(&p.dir)?;
    if testrun::protected_hash(&tests)
        != stored["protected_tests_sha256"]
            .as_str()
            .unwrap_or_default()
    {
        return reject(
            "PROTECTED_TESTS_CHANGED",
            "protected tests changed since validation".into(),
        );
    }
    let (cand_src, cand, _) = match validate_static(c, src, &p.file_name, &patch) {
        Ok(v) => v,
        Err(r) => return reject(r.code, r.message),
    };
    let ws = inc_dir.join(format!("apply-{}", &candidate_hash[..12]));
    if ws.exists() {
        let _ = Command::new("/bin/chmod")
            .args(["-R", "u+w"])
            .arg(&ws)
            .output();
        fs::remove_dir_all(&ws).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
    let ws = fs::canonicalize(&ws).map_err(|e| e.to_string())?;
    let protected = snapshot(p, src, &ws, &tests)?;
    let sb = Sandbox {
        workspace: ws.clone(),
        protected,
        denied_reads: sandbox::default_denied_reads(&p.dir, &c.app.runtime.database),
        nari_exe: o.nari_exe.clone(),
    };
    let sb = match sandbox::detect(sb) {
        Isolation::Seatbelt(sb) => sb,
        Isolation::Unavailable(why) => {
            return Ok(Outcome {
                exit: 3,
                state: "Validated".into(),
                report: json!({ "code": "SANDBOX_UNAVAILABLE", "message": why }),
            });
        }
    };
    clone_cache(&pipeline::target_dir(p), &ws.join("target"));
    let bin = build_isolated(&ws.join("candidate"), &cand, &sb, &ws.join("target"))?;
    let results = run_suite(&cand, &bin, &ws, "apply", &tests, &sb, false)?;
    if !results.iter().all(|r| r.passed) {
        return reject(
            "PROTECTED_TESTS_FAILED",
            "candidate failed the protected suite during apply".into(),
        );
    }
    if fs::read(&p.file)
        .map(|b| ir::sha256_hex(&b))
        .ok()
        .as_deref()
        != Some(c.source_sha256.as_str())
    {
        return reject("PATCH_STALE", "source changed during apply".into());
    }
    if fs::symlink_metadata(&p.file).is_ok_and(|m| m.file_type().is_symlink()) {
        return reject("PATH_FORBIDDEN", "project source is a symlink".into());
    }
    let tmp = p.dir.join(format!(".{}.nari-apply", p.file_name));
    fs::write(&tmp, &cand_src)
        .and_then(|_| fs::rename(&tmp, &p.file))
        .map_err(|e| format!("cannot write source: {e}"))?;
    let repair_id = stored["repair_id"].as_str().unwrap_or_default().to_string();
    rt.block_on(async {
        sqlx::query("UPDATE repairs SET state = 'Applied', outcome = 'APPLIED', updated_ms = ?1 WHERE id = ?2")
            .bind(now_ms())
            .bind(&repair_id)
            .execute(&ctl.db)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("INSERT INTO repair_transitions (repair_id, incident_id, state, candidate_hash, attempt, policy_sha256, evidence, at_ms) VALUES (?1, ?2, 'Applied', ?3, 0, ?4, '{}', ?5)")
            .bind(&repair_id)
            .bind(incident_id)
            .bind(candidate_hash)
            .bind(pipeline::policy_hash(c))
            .bind(now_ms())
            .execute(&ctl.db)
            .await
            .map_err(|e| e.to_string())?;
        ctl.set_outcome(incident_id, "repaired_by_local_apply").await
    })?;
    Ok(Outcome {
        exit: 0,
        state: "Applied".into(),
        report: json!({ "applied": candidate_hash, "new_source_sha256": cand.source_sha256, "protected_tests": results.len() }),
    })
}
