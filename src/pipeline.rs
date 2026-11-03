use crate::Project;
use crate::artifacts::{self, write_file};
use crate::codegen::{self, Region};
use crate::diag::{Diagnostic, Span};
use crate::ir::{self, Compiled};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const WORKSPACE_LOCK: &str = include_str!("../Cargo.lock");
pub const TOOLCHAIN_PIN: &str = include_str!("../rust-toolchain.toml");

pub struct Built {
    pub binary: PathBuf,
    pub manifest: Json,
}

pub fn package_name(p: &Project, app: &str) -> String {
    package_name_for(&p.dir, app)
}

pub fn package_name_for(dir: &Path, app: &str) -> String {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let tag = &ir::sha256_hex(dir.to_string_lossy().as_bytes())[..10];
    format!("frame-app-{}-{tag}", app.replace('_', "-"))
}

pub fn target_dir(p: &Project) -> PathBuf {
    std::env::var_os("FRAME_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| p.artifacts().join("build").join("target"))
}

pub fn cargo() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

pub fn policy_hash(c: &Compiled) -> String {
    let policy = json!({
        "identity": ir::to_json(&c.app.identity),
        "recovery": ir::to_json(&c.app.recovery),
        "repair": ir::to_json(&c.app.repair),
        "runtime": ir::to_json(&c.app.runtime),
    });
    ir::sha256_hex(ir::canonical_compact(&policy).as_bytes())
}

pub fn lock_hash() -> String {
    ir::sha256_hex(WORKSPACE_LOCK.as_bytes())
}

pub fn toolchain_hash() -> String {
    ir::sha256_hex(TOOLCHAIN_PIN.as_bytes())
}

pub fn generate(out: &Path, c: &Compiled, package: &str) -> Result<codegen::Generated, String> {
    artifacts::write_ir(out, c)?;
    let generated = codegen::generate(c, package);
    artifacts::ensure_real_dir(&out.join("generated"))?;
    for (rel, contents) in &generated.files {
        write_file(&out.join("generated"), rel, contents)?;
    }
    let lock = artifacts::safe_join(&out.join("generated"), "app/Cargo.lock")?;
    if !lock.exists() {
        fs::write(&lock, WORKSPACE_LOCK).map_err(|e| e.to_string())?;
    }
    Ok(generated)
}

fn region_for(regions: &[Region], line: usize) -> Option<&Region> {
    regions
        .iter()
        .find(|r| r.start_line <= line && line <= r.end_line)
}

pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub bin: PathBuf,
}

pub fn resolve_toolchain(app_dir: &Path) -> Result<Toolchain, String> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(app_dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run rustc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rustc --print sysroot failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let bin = sysroot.join("bin");
    Ok(Toolchain {
        cargo: bin.join("cargo"),
        rustc: bin.join("rustc"),
        bin,
    })
}

pub fn compile_app(
    c: &Compiled,
    out: &Path,
    generated: &codegen::Generated,
    package: &str,
    target: &Path,
    sandbox: Option<&crate::sandbox::Sandbox>,
) -> Result<PathBuf, (u8, Vec<Diagnostic>)> {
    let app_dir = out.join("generated").join("app");
    let tool_err = |e: String| {
        (
            3u8,
            vec![Diagnostic::error(
                "E_TOOLCHAIN",
                e,
                &c.file,
                Span::default(),
            )],
        )
    };
    let mut args: Vec<String> = [
        "build",
        "--release",
        "--message-format=json",
        "--manifest-path",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(app_dir.join("Cargo.toml").to_string_lossy().into_owned());
    let output = match sandbox {
        Some(sb) => {
            let tc = resolve_toolchain(&app_dir).map_err(tool_err)?;
            args.push("--offline".into());
            let tmp = sb.workspace.join("tmp");
            fs::create_dir_all(&tmp).map_err(|e| tool_err(e.to_string()))?;
            let home = std::env::var("HOME").unwrap_or_default();
            let cargo_home =
                std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{home}/.cargo"));
            let env: Vec<(String, String)> = vec![
                ("PATH".into(), format!("{}:/usr/bin:/bin", tc.bin.display())),
                ("HOME".into(), home),
                ("CARGO_HOME".into(), cargo_home),
                ("RUSTC".into(), tc.rustc.to_string_lossy().into_owned()),
                (
                    "CARGO_TARGET_DIR".into(),
                    target.to_string_lossy().into_owned(),
                ),
                ("TMPDIR".into(), tmp.to_string_lossy().into_owned()),
                ("CARGO_NET_OFFLINE".into(), "true".into()),
            ];
            let mut cmd = sb.command(crate::sandbox::Phase::Build, &tc.cargo, &args, &env);
            cmd.current_dir(&app_dir);
            crate::sandbox::run_with_timeout(
                &mut cmd,
                crate::sandbox::Limits::for_phase(crate::sandbox::Phase::Build).wall,
            )
            .map_err(tool_err)?
        }
        None => Command::new(cargo())
            .args(&args)
            .current_dir(&app_dir)
            .env("CARGO_TARGET_DIR", target)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| tool_err(format!("cannot run cargo: {e}")))?,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut binary = None;
    let mut diags = Vec::new();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Json>(line) else {
            continue;
        };
        match msg["reason"].as_str() {
            Some("compiler-artifact") if msg["target"]["name"].as_str() == Some(package) => {
                if let Some(exe) = msg["executable"].as_str() {
                    binary = Some(PathBuf::from(exe));
                }
            }
            Some("compiler-message") if msg["message"]["level"] == "error" => {
                let rendered = msg["message"]["rendered"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let primary = msg["message"]["spans"]
                    .as_array()
                    .and_then(|s| s.iter().find(|s| s["is_primary"] == true));
                let line = primary.and_then(|s| s["line_start"].as_u64()).unwrap_or(0) as usize;
                let in_main = primary
                    .and_then(|s| s["file_name"].as_str())
                    .is_some_and(|f| f.ends_with("src/main.rs"));
                let d = match region_for(&generated.regions, line).filter(|_| in_main) {
                    Some(r) => Diagnostic::error(
                        "E_BACKEND_COMPILE",
                        format!("generated backend for {} failed to compile", r.node),
                        &c.file,
                        c.spans.get(&r.node).copied().unwrap_or_default(),
                    )
                    .node(&r.node),
                    None => Diagnostic::error(
                        "E_BACKEND_COMPILE",
                        "generated backend failed to compile",
                        &c.file,
                        Span::default(),
                    ),
                };
                diags.push(d.details(rendered));
            }
            _ => {}
        }
    }
    if !output.status.success() {
        if diags.is_empty() {
            let dependency = [
                "failed to get",
                "download",
                "registry",
                "network",
                "failed to load source",
                "offline",
            ]
            .iter()
            .any(|k| stderr.contains(k));
            let (code, msg) = if dependency {
                (
                    "E_DEPENDENCIES",
                    "dependency installation failed (this is not a source validation error); run `frame build` once with network access",
                )
            } else {
                (
                    "E_BACKEND_COMPILE",
                    "cargo failed to build the generated backend",
                )
            };
            diags.push(
                Diagnostic::error(code, msg, &c.file, Span::default())
                    .details(stderr.trim().to_string()),
            );
        }
        return Err((3, diags));
    }
    let binary = binary.ok_or_else(|| {
        (
            3,
            vec![Diagnostic::error(
                "E_BACKEND_COMPILE",
                "cargo reported no executable",
                &c.file,
                Span::default(),
            )],
        )
    })?;
    let build_dir = out.join("build");
    artifacts::ensure_real_dir(&build_dir).map_err(|e| {
        (
            3,
            vec![Diagnostic::error("E_IO", e, &c.file, Span::default())],
        )
    })?;
    let dest = build_dir.join(format!("{}-app", c.app.name));
    let tmp = build_dir.join(format!(".{}-app.tmp", c.app.name));
    fs::copy(&binary, &tmp)
        .and_then(|_| fs::rename(&tmp, &dest))
        .map_err(|e| {
            (
                3,
                vec![Diagnostic::error(
                    "E_IO",
                    format!("cannot install app binary: {e}"),
                    &c.file,
                    Span::default(),
                )],
            )
        })?;
    artifacts::restrict(&dest, 0o700);
    Ok(dest)
}

pub fn file_hashes(files: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(rel, contents)| (rel.clone(), ir::sha256_hex(contents.as_bytes())))
        .collect()
}

pub fn build(p: &Project, c: &Compiled) -> Result<Built, (u8, Vec<Diagnostic>)> {
    build_in(p, c, &target_dir(p))
}

pub fn build_in(p: &Project, c: &Compiled, target: &Path) -> Result<Built, (u8, Vec<Diagnostic>)> {
    let io = |e: String| {
        (
            3u8,
            vec![Diagnostic::error("E_IO", e, &c.file, Span::default())],
        )
    };
    let out = artifacts::prepare(p).map_err(io)?;
    let package = package_name(p, &c.app.name);
    let generated = generate(&out, c, &package).map_err(io)?;
    let binary = compile_app(c, &out, &generated, &package, target, None)?;
    let binary_sha = fs::read(&binary)
        .map(|b| ir::sha256_hex(&b))
        .unwrap_or_default();
    let manifest = json!({
        "schema": "frame-artifacts/0.1",
        "compiler": format!("frame {}", ir::COMPILER_VERSION),
        "runtime_interface": "frame-runtime/0.1",
        "app": c.app.name,
        "source_file": c.file,
        "source_sha256": c.source_sha256,
        "semantic_sha256": c.semantic_sha256,
        "graph_sha256": c.graph_sha256,
        "policy_sha256": policy_hash(c),
        "schema_fingerprint": generated.schema_fingerprint,
        "dependency_lock_sha256": lock_hash(),
        "toolchain_sha256": toolchain_hash(),
        "generated": file_hashes(&generated.files),
        "binary": format!("build/{}-app", c.app.name),
        "binary_sha256": binary_sha,
    });
    write_file(&out, "manifest.json", ir::canonical(&manifest) + "\n").map_err(io)?;
    Ok(Built { binary, manifest })
}
