use crate::Project;
use crate::ir::{self, Compiled};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn ensure_real_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "{} is a symlink; refusing to write artifacts through it",
            path.display()
        )),
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("{} exists and is not a directory", path.display())),
        Err(_) => {
            fs::create_dir(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            restrict(path, 0o700);
            Ok(())
        }
    }
}

pub fn restrict(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

pub fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut out = base.to_path_buf();
    let parts: Vec<Component> = Path::new(rel).components().collect();
    for (i, c) in parts.iter().enumerate() {
        match c {
            Component::Normal(name) => {
                out.push(name);
                if i + 1 < parts.len() {
                    ensure_real_dir(&out)?;
                } else if fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(format!(
                        "{} is a symlink; refusing to use it",
                        out.display()
                    ));
                }
            }
            _ => return Err(format!("path '{rel}' escapes its base directory")),
        }
    }
    Ok(out)
}

pub fn prepare(p: &Project) -> Result<PathBuf, String> {
    let dir = p.artifacts();
    ensure_real_dir(&dir)?;
    Ok(dir)
}

pub fn write_file(base: &Path, rel: &str, contents: impl AsRef<[u8]>) -> Result<PathBuf, String> {
    let path = safe_join(base, rel)?;
    fs::write(&path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn write_ir(dir: &Path, c: &Compiled) -> Result<(), String> {
    write_file(dir, "ir.json", ir::canonical(&c.ir_json()) + "\n")?;
    write_file(
        dir,
        "graph.json",
        ir::canonical(&ir::to_json(&c.graph)) + "\n",
    )?;
    Ok(())
}
