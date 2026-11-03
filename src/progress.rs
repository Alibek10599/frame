use crate::ir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const SCHEMA: &str = "frame-task/0.1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub path: String,
    pub sha256: String,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub schema: String,
    pub task_id: String,
    pub goal: String,
    pub base_source_sha256: String,
    pub base_graph_sha256: String,
    pub state: String,
    pub completed: Vec<String>,
    pub outstanding_checks: Vec<String>,
    pub evidence: Vec<Evidence>,
}

impl TaskRecord {
    pub fn new(task_id: &str, goal: &str, source: &str, graph: &str) -> Self {
        TaskRecord {
            schema: SCHEMA.into(),
            task_id: task_id.into(),
            goal: goal.into(),
            base_source_sha256: source.into(),
            base_graph_sha256: graph.into(),
            state: "open".into(),
            completed: Vec::new(),
            outstanding_checks: Vec::new(),
            evidence: Vec::new(),
        }
    }

    pub fn add_evidence(
        &mut self,
        project_dir: &Path,
        rel: &str,
        kind: &str,
    ) -> Result<(), String> {
        let bytes = fs::read(project_dir.join(rel))
            .map_err(|e| format!("cannot read evidence {rel}: {e}"))?;
        self.evidence.retain(|e| e.path != rel);
        self.evidence.push(Evidence {
            path: rel.into(),
            sha256: ir::sha256_hex(&bytes),
            kind: kind.into(),
        });
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = ir::canonical(&ir::to_json(self)) + "\n";
        fs::write(path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let rec: TaskRecord =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid task record: {e}"))?;
        if rec.schema != SCHEMA {
            return Err(format!("unsupported task record schema '{}'", rec.schema));
        }
        Ok(rec)
    }

    pub fn verify(
        &self,
        project_dir: &Path,
        source_sha256: &str,
        graph_sha256: &str,
    ) -> Vec<String> {
        let mut drift = Vec::new();
        if self.base_source_sha256 != source_sha256 {
            drift.push(
                "source changed since the task was recorded; recapture context before continuing"
                    .to_string(),
            );
        }
        if self.base_graph_sha256 != graph_sha256 {
            drift.push("dependency graph changed; affected checks must be recomputed".to_string());
        }
        for e in &self.evidence {
            match fs::read(project_dir.join(&e.path)) {
                Ok(b) if ir::sha256_hex(&b) == e.sha256 => {}
                Ok(_) => drift.push(format!("evidence {} changed since it was recorded", e.path)),
                Err(_) => drift.push(format!("evidence {} is missing", e.path)),
            }
        }
        drift
    }
}
