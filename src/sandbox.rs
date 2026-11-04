use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Build,
    Run,
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub cpu_seconds: u64,
    pub file_bytes: u64,
    pub open_files: u64,
    pub wall: Duration,
}

impl Limits {
    pub fn for_phase(phase: Phase) -> Limits {
        match phase {
            Phase::Build => Limits {
                cpu_seconds: 1200,
                file_bytes: 4 << 30,
                open_files: 4096,
                wall: Duration::from_secs(900),
            },
            Phase::Run => Limits {
                cpu_seconds: 120,
                file_bytes: 256 << 20,
                open_files: 1024,
                wall: Duration::from_secs(120),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub workspace: PathBuf,
    pub protected: PathBuf,
    pub denied_reads: Vec<PathBuf>,
    pub nari_exe: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Isolation {
    Seatbelt(Sandbox),
    Unavailable(String),
}

fn quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl Sandbox {
    pub fn profile(&self, phase: Phase) -> String {
        let ws = quote(&real(&self.workspace));
        let mut p = String::from("(version 1)\n(deny default)\n");
        p.push_str(match phase {
            Phase::Build => "(allow process-fork)\n(allow process-exec)\n",
            Phase::Run => "(allow process-exec)\n",
        });
        p.push_str("(allow process-info*)\n(allow signal (target same-sandbox))\n(allow sysctl-read)\n(allow mach-lookup)\n(allow ipc-posix-shm)\n(allow file-ioctl)\n(allow file-read*)\n");
        p.push_str(&format!(
            "(allow file-write* (subpath {ws}) (literal \"/dev/null\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\"))\n"
        ));
        p.push_str(&format!(
            "(deny file-write* (subpath {}))\n",
            quote(&real(&self.protected))
        ));
        p.push_str("(deny network*)\n");
        if phase == Phase::Run {
            p.push_str("(allow network-bind (local ip \"localhost:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n");
        }
        for d in &self.denied_reads {
            p.push_str(&format!(
                "(deny file-read* (subpath {}))\n",
                quote(&real(d))
            ));
        }
        p
    }

    pub fn wrapper(&self, phase: Phase) -> Vec<String> {
        let l = Limits::for_phase(phase);
        vec![
            SANDBOX_EXEC.into(),
            "-p".into(),
            self.profile(phase),
            self.nari_exe.to_string_lossy().into_owned(),
            "__limit".into(),
            "--cpu".into(),
            l.cpu_seconds.to_string(),
            "--fsize".into(),
            l.file_bytes.to_string(),
            "--nofile".into(),
            l.open_files.to_string(),
            "--".into(),
        ]
    }

    pub fn command(
        &self,
        phase: Phase,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
    ) -> Command {
        let w = self.wrapper(phase);
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]).arg(program).args(args).env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }
}

pub fn default_denied_reads(project_dir: &Path, database: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".docker",
            ".kube",
            ".netrc",
            ".config/gcloud",
            ".config/gh",
            ".cargo/credentials",
            ".cargo/credentials.toml",
            "Library/Keychains",
        ] {
            v.push(home.join(rel));
        }
    }
    for rel in [
        ".nari/control.db",
        ".nari/control.db-wal",
        ".nari/control.db-shm",
        ".nari/run",
    ] {
        v.push(project_dir.join(rel));
    }
    for suffix in ["", "-wal", "-shm"] {
        v.push(project_dir.join(format!("{database}{suffix}")));
    }
    v
}

pub fn run_with_timeout(cmd: &mut Command, wall: Duration) -> Result<std::process::Output, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start sandboxed command: {e}"))?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let read = |s: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut s) = s {
                let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            }
            buf
        })
    };
    let out_t = read(out);
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = err {
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
        }
        buf
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if start.elapsed() > wall {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "sandboxed command exceeded its {}s wall-clock limit and was killed",
                wall.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Ok(std::process::Output {
        status,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    })
}

pub fn detect(sandbox: Sandbox) -> Isolation {
    if std::env::var("NARI_SANDBOX").is_ok_and(|v| v == "none") {
        return Isolation::Unavailable("disabled by NARI_SANDBOX=none".into());
    }
    if !cfg!(target_os = "macos") {
        return Isolation::Unavailable(
            "no supported OS sandbox on this platform (Imanari 0.1 uses macOS Seatbelt)".into(),
        );
    }
    if !Path::new(SANDBOX_EXEC).exists() {
        return Isolation::Unavailable(format!("{SANDBOX_EXEC} not found"));
    }
    match self_test(&sandbox) {
        Ok(()) => Isolation::Seatbelt(sandbox),
        Err(e) => Isolation::Unavailable(format!("sandbox self-test failed: {e}")),
    }
}

fn probe(sb: &Sandbox, phase: Phase, args: &[String]) -> Result<bool, String> {
    let mut cmd = sb.command(
        phase,
        &sb.nari_exe,
        &["__probe".to_string()]
            .into_iter()
            .chain(args.iter().cloned())
            .collect::<Vec<_>>(),
        &[],
    );
    let out = run_with_timeout(&mut cmd, Duration::from_secs(20))?;
    Ok(out.status.success())
}

pub fn self_test(sb: &Sandbox) -> Result<(), String> {
    std::fs::create_dir_all(&sb.workspace).map_err(|e| e.to_string())?;
    let inside = sb.workspace.join(".probe-ok");
    if !probe(
        sb,
        Phase::Build,
        &["write".into(), inside.to_string_lossy().into_owned()],
    )? {
        return Err("sandboxed process cannot write its own workspace".into());
    }
    let outside = sb
        .workspace
        .parent()
        .unwrap_or(&sb.workspace)
        .join(".probe-escape");
    if probe(
        sb,
        Phase::Build,
        &["write".into(), outside.to_string_lossy().into_owned()],
    )? {
        let _ = std::fs::remove_file(&outside);
        return Err("sandboxed process wrote outside its workspace".into());
    }
    let protected = sb.protected.join(".probe-protected");
    if probe(
        sb,
        Phase::Build,
        &["write".into(), protected.to_string_lossy().into_owned()],
    )? {
        let _ = std::fs::remove_file(&protected);
        return Err("sandboxed process wrote into read-only protected fixtures".into());
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .to_string();
    for phase in [Phase::Build, Phase::Run] {
        if probe(sb, phase, &["connect".into(), addr.clone()])? {
            return Err(format!(
                "{phase:?} phase opened an outbound network connection"
            ));
        }
    }
    Ok(())
}

pub fn limit_and_exec(args: &[String]) -> Result<std::convert::Infallible, String> {
    let mut it = args.iter();
    let mut program: Option<&String> = None;
    let mut rest: Vec<&String> = Vec::new();
    while let Some(a) = it.next() {
        let value = |it: &mut std::slice::Iter<String>| -> Result<u64, String> {
            it.next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{a} needs a number"))
        };
        match a.as_str() {
            "--cpu" => {
                let v = value(&mut it)?;
                rlimit::setrlimit(rlimit::Resource::CPU, v, v)
                    .map_err(|e| format!("RLIMIT_CPU: {e}"))?;
            }
            "--fsize" => {
                let v = value(&mut it)?;
                rlimit::setrlimit(rlimit::Resource::FSIZE, v, v)
                    .map_err(|e| format!("RLIMIT_FSIZE: {e}"))?;
            }
            "--nofile" => {
                let v = value(&mut it)?;
                rlimit::setrlimit(rlimit::Resource::NOFILE, v, v)
                    .map_err(|e| format!("RLIMIT_NOFILE: {e}"))?;
            }
            "--" => {
                program = it.next();
                rest.extend(it.by_ref());
            }
            other => return Err(format!("unknown limit option {other}")),
        }
    }
    let program = program.ok_or("missing program after --")?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(program).args(rest).exec();
        Err(format!("exec {program}: {err}"))
    }
    #[cfg(not(unix))]
    {
        let _ = rest;
        Err(format!("cannot exec {program} on this platform"))
    }
}

pub fn run_probe(args: &[String]) -> bool {
    match args {
        [kind, path] if kind == "write" => std::fs::write(path, b"probe").is_ok(),
        [kind, addr] if kind == "connect" => std::net::TcpStream::connect_timeout(
            &match addr.parse() {
                Ok(a) => a,
                Err(_) => return false,
            },
            Duration::from_secs(2),
        )
        .is_ok(),
        _ => false,
    }
}
