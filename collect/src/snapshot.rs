//! A raw snapshot: unredacted records, local only. `pack` reads it; `run` deletes it after packing.

use crate::collect::Outcome;
use crate::record::Record;
use crate::window::Window;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub tool_version: String,
    pub call_plan_version: String,
    pub window: Window,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub outcome: Outcome,
}

pub struct Snapshot {
    pub dir: PathBuf,
    pub meta: SnapshotMeta,
    pub records: Vec<Record>,
}

const META: &str = "snapshot.json";
const CALLS: &str = "calls.ndjson";

impl Snapshot {
    /// `<work_dir>/raw/<window label>`.
    pub fn dir_for(work_dir: &Path, window: &Window) -> PathBuf {
        work_dir.join("raw").join(window.label())
    }

    pub fn write(dir: PathBuf, meta: SnapshotMeta, records: Vec<Record>) -> Result<Snapshot> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut out = BufWriter::new(std::fs::File::create(dir.join(CALLS))?);
        for r in &records {
            serde_json::to_writer(&mut out, r)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        std::fs::write(dir.join(META), serde_json::to_vec_pretty(&meta)?)?;
        Ok(Snapshot { dir, meta, records })
    }

    pub fn read(dir: &Path) -> Result<Snapshot> {
        let meta: SnapshotMeta = serde_json::from_slice(
            &std::fs::read(dir.join(META)).with_context(|| format!("{} is not a raw snapshot", dir.display()))?,
        )?;
        let file = std::fs::File::open(dir.join(CALLS))?;
        let records = read_ndjson(std::io::BufReader::new(file))?;
        Ok(Snapshot {
            dir: dir.to_path_buf(),
            meta,
            records,
        })
    }
}

pub fn read_ndjson(reader: impl BufRead) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line).with_context(|| format!("record on line {}", i + 1))?);
    }
    Ok(records)
}
