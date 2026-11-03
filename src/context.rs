use crate::Project;
use crate::ir::{self, Compiled};
use crate::pipeline;
use crate::testrun::{self, ProtectedTest};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;

pub struct Bundle {
    pub markdown: String,
    pub manifest: Json,
    pub over_budget: bool,
}

struct Section {
    id: String,
    kind: String,
    reason: String,
    required: bool,
    body: String,
}

fn line_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let l1 = src[..start.min(src.len())].matches('\n').count() + 1;
    let l2 = src[..end.min(src.len())].matches('\n').count() + 1;
    (l1, l2)
}

fn source_block(c: &Compiled, src: &str, id: &str) -> Option<String> {
    let span = c.spans.get(id)?;
    let (l1, l2) = line_range(src, span.start, span.end);
    let text = src.get(span.start..span.end)?;
    Some(format!(
        "`{}` bytes {}..{} (lines {l1}-{l2}):\n\n```frame\n{text}\n```\n",
        c.file, span.start, span.end
    ))
}

fn node_ir(c: &Compiled, id: &str) -> Option<Json> {
    let app = &c.app;
    let found = app
        .entities
        .iter()
        .find(|e| e.id == id)
        .map(ir::to_json)
        .or_else(|| app.actions.iter().find(|a| a.id == id).map(ir::to_json))
        .or_else(|| app.screens.iter().find(|s| s.id == id).map(ir::to_json));
    if found.is_some() {
        return found;
    }
    for (single, value) in [
        (&app.identity.id, ir::to_json(&app.identity)),
        (&app.runtime.id, ir::to_json(&app.runtime)),
        (&app.recovery.id, ir::to_json(&app.recovery)),
        (&app.repair.id, ir::to_json(&app.repair)),
    ] {
        if single == id {
            return Some(value);
        }
    }
    None
}

fn block_owner(c: &Compiled, id: &str) -> String {
    let mut cur = id.to_string();
    loop {
        if cur == c.app.id {
            return id.to_string();
        }
        if c.spans.contains_key(&cur) && !cur.contains("/set/") {
            return cur;
        }
        match cur.rfind('/') {
            Some(i) if cur[..i].contains('/') => {
                cur.truncate(i);
                if let Some(j) = cur.rfind('/') {
                    cur.truncate(j);
                }
            }
            _ => return id.to_string(),
        }
    }
}

pub fn incidents_for(p: &Project, target: &str) -> Vec<Json> {
    let dir = p.artifacts().join("incidents");
    let mut out: Vec<Json> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Json>(&b).ok())
        .filter(|j| j["target_id"] == target)
        .collect();
    out.sort_by(|a, b| a["incident_id"].as_str().cmp(&b["incident_id"].as_str()));
    out
}

pub fn hashes(c: &Compiled, tests: &[ProtectedTest]) -> Json {
    json!({
        "source_sha256": c.source_sha256,
        "semantic_sha256": c.semantic_sha256,
        "graph_sha256": c.graph_sha256,
        "dependency_lock_sha256": pipeline::lock_hash(),
        "toolchain_sha256": pipeline::toolchain_hash(),
        "policy_sha256": pipeline::policy_hash(c),
        "protected_tests_sha256": testrun::protected_hash(tests),
    })
}

pub fn extract(
    p: &Project,
    c: &Compiled,
    src: &str,
    target: &str,
    max_bytes: usize,
) -> Result<Bundle, String> {
    let kind = c
        .graph
        .kind(target)
        .ok_or_else(|| {
            let near: Vec<&str> = c
                .graph
                .nodes
                .iter()
                .map(|n| n.id.as_str())
                .filter(|n| n.contains(target.rsplit('/').next().unwrap_or(target)))
                .take(5)
                .collect();
            format!("unknown node '{target}'; similar: {}", near.join(", "))
        })?
        .to_string();
    let tests = testrun::load_protected(&p.dir)?;
    let mut sections: Vec<Section> = Vec::new();
    let mut included: BTreeSet<String> = BTreeSet::new();
    let mut add = |sections: &mut Vec<Section>,
                   id: &str,
                   kind: &str,
                   reason: String,
                   required: bool,
                   body: String| {
        if !included.insert(id.to_string()) {
            if let Some(s) = sections
                .iter_mut()
                .find(|s| s.id == id && !s.reason.contains(&reason))
            {
                s.reason = format!("{}; {reason}", s.reason);
            }
        } else {
            sections.push(Section {
                id: id.into(),
                kind: kind.into(),
                reason,
                required,
                body,
            });
        }
    };

    let mut target_body = source_block(c, src, &block_owner(c, target)).unwrap_or_default();
    if let Some(a) = c.app.actions.iter().find(|a| a.id == target) {
        let ct = &a.contract;
        target_body.push_str(&format!(
            "\nInferred contract (compiler inference, not declared source):\n\n- transport: `{}`\n- inputs: {}\n- output: `{}`\n- errors: {}\n- effects: {}\n- idempotency key required: {}\n- authorization: signed_in, rows filtered by owner inside the SQL predicate\n",
            ct.transport,
            ct.inputs.iter().map(|(k, v)| format!("`{k}: {v}`")).collect::<Vec<_>>().join(", "),
            ct.output,
            ct.errors.join(", "),
            ct.effects.join(", "),
            ct.requires_idempotency_key
        ));
    } else if let Some(j) = node_ir(c, target) {
        target_body.push_str(&format!(
            "\nTyped IR:\n\n```json\n{}\n```\n",
            ir::canonical(&j)
        ));
    }
    add(
        &mut sections,
        target,
        &kind,
        "target".into(),
        true,
        target_body,
    );

    let inside = |id: &str| id.starts_with(&format!("{target}/"));
    for e in c.graph.forward(target) {
        if inside(&e.to) {
            continue;
        }
        let to_kind = c.graph.kind(&e.to).unwrap_or("unknown").to_string();
        let (id, kind) = match (to_kind.as_str(), e.to.find("/field/")) {
            ("field", Some(i)) => (e.to[..i].to_string(), "entity".to_string()),
            _ => (e.to.clone(), to_kind.clone()),
        };
        let body = match kind.as_str() {
            "artifact" => {
                format!("Generated artifact `{id}` (no source span; regenerated from source).\n")
            }
            _ => source_block(c, src, &block_owner(c, &id)).unwrap_or_else(|| format!("`{id}`\n")),
        };
        add(
            &mut sections,
            &id,
            &kind,
            format!("direct dependency ({})", e.role),
            true,
            body,
        );
    }
    for e in c.graph.reverse(target) {
        if inside(&e.from) {
            continue;
        }
        let from_kind = c.graph.kind(&e.from).unwrap_or("unknown").to_string();
        let reason = match from_kind.as_str() {
            "identity" => "permission: authenticates this node".to_string(),
            "repair" => "policy: repair authority over this node".to_string(),
            _ => format!("reverse dependent ({}) affected by changes", e.role),
        };
        let owner = block_owner(c, &e.from);
        let body = source_block(c, src, &owner).unwrap_or_else(|| format!("`{}`\n", e.from));
        add(&mut sections, &owner, &from_kind, reason, true, body);
    }
    let constraint = format!(
        "Runtime limits that bound every request (requirement):\n\n- bind {} port {}\n- request_limit_bytes {}\n- request_timeout_ms {}\n- 100 concurrent requests, 60 mutations per actor per minute (runtime default)\n- Host/Origin validation, CSRF header on cookie-authenticated requests, no-store responses\n",
        c.app.runtime.bind,
        c.app.runtime.port,
        c.app.runtime.request_limit_bytes,
        c.app.runtime.request_timeout_ms
    );
    add(
        &mut sections,
        &c.app.runtime.id,
        "runtime",
        "system constraint".into(),
        true,
        constraint,
    );

    let covering: Vec<&ProtectedTest> = tests
        .iter()
        .filter(|t| {
            t.spec["covers"]
                .as_array()
                .is_some_and(|c| c.iter().any(|v| v == target))
        })
        .collect();
    let mut tbody = String::new();
    for t in &covering {
        tbody.push_str(&format!(
            "- `{}` sha256 `{}` — {} (protected; repair cannot modify)\n",
            t.file,
            t.sha256,
            t.spec["name"].as_str().unwrap_or_default()
        ));
        let target_action = target.rsplit('/').next().unwrap_or_default();
        for (i, s) in t.spec["steps"].as_array().into_iter().flatten().enumerate() {
            if s["action"] == target_action {
                tbody.push_str(&format!(
                    "  - step {i}: as {} → expect {}\n",
                    s["as"].as_str().unwrap_or("?"),
                    ir::canonical_compact(&s["expect"])
                ));
            }
        }
    }
    if c.app.actions.iter().any(|a| a.id == target) {
        tbody.push_str("- generated contract checks: unauthenticated 401, missing CSRF 403, forged session 401, unknown field 400, cross-origin 403, idempotency key required, text bounds, cross-user isolation, stale revision 409\n");
    }
    if !tbody.is_empty() {
        add(
            &mut sections,
            &format!("{target}#tests"),
            "tests",
            "relevant protected tests (requirements)".into(),
            true,
            tbody,
        );
    }

    let report_path = p.artifacts().join("test").join("report.json");
    if let Some(report) = fs::read(&report_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Json>(&b).ok())
    {
        let fresh = report["source_sha256"] == c.source_sha256.as_str();
        let rows: Vec<String> = report["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                r["covers"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|v| v == target))
            })
            .map(|r| {
                format!(
                    "- {} `{}`: {}",
                    if r["passed"] == true { "pass" } else { "FAIL" },
                    r["name"].as_str().unwrap_or(""),
                    r["detail"].as_str().unwrap_or("")
                )
            })
            .collect();
        if !rows.is_empty() {
            let body = format!(
                "Observed in `.frame/test/report.json` ({}):\n\n{}\n",
                if fresh {
                    "matches current source"
                } else {
                    "STALE: recorded against a different source hash"
                },
                rows.join("\n")
            );
            add(
                &mut sections,
                &format!("{target}#observed"),
                "evidence",
                "diagnostic evidence (observed)".into(),
                false,
                body,
            );
        }
    }
    for inc in incidents_for(p, target) {
        let id = format!("incident/{}", inc["incident_id"].as_str().unwrap_or("?"));
        let body = format!(
            "Historical outcome (not a rule; compatibility must be rechecked):\n\n```json\n{}\n```\n",
            ir::canonical(&inc)
        );
        add(
            &mut sections,
            &id,
            "incident",
            "linked incident (historical outcome)".into(),
            false,
            body,
        );
    }

    let header = |hashes: &Json| {
        let mut h = format!(
            "# Context: {target}\n\nFrame context 0.1. A bounded snapshot for this node, not a claim of complete understanding. Labels: *requirement* = declared source or protected test; *inference* = compiler-derived; *observed* = recorded run evidence; *historical* = past incident outcome.\n\n## Hashes\n\n"
        );
        for (k, v) in hashes.as_object().into_iter().flatten() {
            h.push_str(&format!("- {k}: `{}`\n", v.as_str().unwrap_or_default()));
        }
        h
    };
    let hash_json = hashes(c, &tests);
    let mut md = header(&hash_json);
    let mut manifest_included = Vec::new();
    let mut omitted_optional = Vec::new();
    let mut required_bytes = md.len();
    for s in sections.iter().filter(|s| s.required) {
        required_bytes += s.body.len() + s.id.len() + 40;
    }
    let over_budget = required_bytes > max_bytes;
    for s in &sections {
        let chunk = format!("\n## {} — {} ({})\n\n{}", s.id, s.kind, s.reason, s.body);
        if !s.required && md.len() + chunk.len() + 2048 > max_bytes {
            omitted_optional.push(json!({ "id": s.id, "kind": s.kind, "reason": "optional content omitted to respect the byte budget", "expand": format!("frame context {target} --max-bytes {}", max_bytes * 2) }));
            continue;
        }
        if s.required || !over_budget {
            md.push_str(&chunk);
            manifest_included.push(json!({ "id": s.id, "kind": s.kind, "reason": s.reason, "required": s.required, "bytes": chunk.len() }));
        }
    }
    let mut omitted: Vec<Json> = c
        .graph
        .nodes
        .iter()
        .filter(|n| {
            !included.contains(&n.id)
                && !included.iter().any(|i| n.id.starts_with(&format!("{i}/")))
                && !n.id.starts_with(&format!("{target}/"))
        })
        .filter(|n| matches!(n.kind.as_str(), "action" | "entity" | "screen" | "identity" | "recovery" | "repair" | "runtime"))
        .map(|n| json!({ "id": n.id, "kind": n.kind, "reason": "not a direct dependency, consumer, or applicable policy", "expand": format!("frame context {}", n.id) }))
        .collect();
    omitted.extend(omitted_optional);
    md.push_str("\n## Omitted\n\n");
    if omitted.len() > 20 {
        md.push_str(&format!(
            "{} nodes omitted; the first 20 are listed, the full list with reasons is in manifest.json.\n\n",
            omitted.len()
        ));
    }
    for o in omitted.iter().take(20) {
        md.push_str(&format!(
            "- `{}` ({}) — {}; expand: `{}`\n",
            o["id"].as_str().unwrap_or_default(),
            o["kind"].as_str().unwrap_or_default(),
            o["reason"].as_str().unwrap_or_default(),
            o["expand"].as_str().unwrap_or_default()
        ));
    }
    let bytes = md.len();
    let required_expansion: Vec<Json> = if over_budget {
        sections
            .iter()
            .filter(|s| s.required)
            .map(|s| json!({ "id": s.id, "kind": s.kind, "bytes": s.body.len() }))
            .collect()
    } else {
        Vec::new()
    };
    let mut by_role: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in &sections {
        let role = match s.kind.as_str() {
            "evidence" => "observed",
            "incident" => "historical",
            _ if s.reason.starts_with("direct dependency") || s.reason.starts_with("reverse") => {
                "inferred"
            }
            _ => "requirement",
        };
        by_role.entry(role).or_default().push(&s.id);
    }
    let manifest = json!({
        "schema": "frame-context/0.1",
        "target": target,
        "kind": kind,
        "status": if over_budget { "CONTEXT_BUDGET_EXCEEDED" } else { "ok" },
        "hashes": hash_json,
        "max_bytes": max_bytes,
        "bytes": if over_budget { 0 } else { bytes },
        "required_bytes": required_bytes,
        "token_estimate": { "value": bytes / 4, "label": "estimate only: bytes/4, not a tokenizer count" },
        "included": manifest_included,
        "omitted": omitted,
        "classification": by_role,
        "required_expansion": required_expansion,
        "target_source": c.spans.get(&block_owner(c, target)).map(|s| json!({ "file": c.file, "start": s.start, "end": s.end, "text": src.get(s.start..s.end).unwrap_or_default() })),
        "patch_preconditions": {
            "base_source_sha256": c.source_sha256,
            "base_graph_sha256": c.graph_sha256,
            "affected_nodes": sections.iter().filter(|s| s.required).map(|s| s.id.clone()).collect::<Vec<_>>(),
            "rule": "any source change since capture rejects a patch as stale (0.1)",
        },
    });
    Ok(Bundle {
        markdown: if over_budget { String::new() } else { md },
        manifest,
        over_budget,
    })
}

pub fn write(p: &Project, target: &str, b: &Bundle) -> Result<std::path::PathBuf, String> {
    let base = crate::artifacts::prepare(p)?.join("context");
    crate::artifacts::ensure_real_dir(&base)?;
    let dir_name: String = target
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '.'
            }
        })
        .collect();
    let dir = crate::artifacts::safe_join(&base, &dir_name)?;
    crate::artifacts::ensure_real_dir(&dir)?;
    if !b.over_budget {
        fs::write(dir.join("context.md"), &b.markdown).map_err(|e| e.to_string())?;
    } else {
        let _ = fs::remove_file(dir.join("context.md"));
    }
    fs::write(dir.join("manifest.json"), ir::canonical(&b.manifest) + "\n")
        .map_err(|e| e.to_string())?;
    Ok(dir)
}
