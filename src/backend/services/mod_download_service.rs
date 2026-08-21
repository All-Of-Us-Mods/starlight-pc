use crate::backend::error::{AppError, AppResult};
use crate::backend::services::http_download::{download_file, finish_digest};
use log::info;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ModDownloadProgress {
    pub mod_id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub progress: f64,
    pub stage: String,
}

fn emit_progress(mod_id: &str, downloaded: u64, total: Option<u64>, stage: &str) {
    let progress = total
        .map(|t| downloaded as f64 / t as f64 * 100.0)
        .unwrap_or(0.0);
    crate::backend::events::publish(crate::backend::events::BackendEvent::ModDownloadProgress(
        ModDownloadProgress {
            mod_id: mod_id.to_string(),
            downloaded,
            total,
            progress,
            stage: stage.to_string(),
        },
    ));
}

pub fn download_mod(
    mod_id: String,
    url: String,
    destination: String,
    expected_checksum: Option<String>,
) -> AppResult<()> {
    let dest_path = Path::new(&destination);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tracking_id = get_tracking_id()?;

    emit_progress(&mod_id, 0, None, "connecting");

    // Download to a `.part` file so a failed/interrupted download never
    // leaves a truncated plugin in place of the real one.
    let part_path = dest_path.with_extension("part");
    let mut hasher = Sha256::new();
    let mut last_pct: i64 = -1;
    let result = download_file(
        &url,
        &part_path,
        Some(("X-Starlight-ID", tracking_id)),
        Some(&mut hasher),
        |downloaded, total| {
            // Throttle to whole-percent changes (or a single emit when the
            // size is unknown/zero) so a large download doesn't flood the
            // event bus.
            let pct = total
                .and_then(|t| (downloaded * 100).checked_div(t))
                .unwrap_or(0) as i64;
            if pct != last_pct {
                last_pct = pct;
                emit_progress(&mod_id, downloaded, total, "downloading");
            }
        },
    );

    if let Err(e) = result {
        let _ = fs::remove_file(&part_path);
        return Err(e);
    }

    emit_progress(&mod_id, 0, None, "verifying");
    let computed_checksum = finish_digest(&mut hasher);
    if let Some(expected) = expected_checksum.filter(|checksum| !checksum.is_empty())
        && computed_checksum != expected.to_lowercase()
    {
        let _ = fs::remove_file(&part_path);
        return Err(AppError::validation(format!(
            "Checksum mismatch: expected {expected}, got {computed_checksum}"
        )));
    }

    emit_progress(&mod_id, 0, None, "writing");
    fs::rename(&part_path, dest_path)?;

    emit_progress(&mod_id, 0, None, "complete");
    info!("Mod download completed: {} -> {:?}", mod_id, dest_path);
    Ok(())
}

fn get_tracking_id() -> AppResult<String> {
    let dir = crate::backend::directories::app_data_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("tracking_id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let new_id = Uuid::new_v4().to_string();
    fs::write(&path, &new_id)?;
    Ok(new_id)
}
