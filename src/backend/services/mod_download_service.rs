use crate::backend::error::{AppError, AppResult};
use log::{debug, info};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

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

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    emit_progress(&mod_id, 0, None, "connecting");

    let mut response = client
        .get(&url)
        .header("X-Starlight-ID", &tracking_id)
        .send()?
        .error_for_status()?;

    let total_size: Option<u64> = response.content_length();
    debug!("Download size: {:?}", total_size);

    let part_path = dest_path.with_extension("part");
    let downloaded = match stream_and_verify(
        &mod_id,
        &mut response,
        &part_path,
        total_size,
        expected_checksum,
    ) {
        Ok(downloaded) => downloaded,
        Err(e) => {
            let _ = fs::remove_file(&part_path);
            return Err(e);
        }
    };

    emit_progress(&mod_id, downloaded, total_size, "writing");
    fs::rename(&part_path, dest_path)?;

    emit_progress(&mod_id, downloaded, total_size, "complete");
    info!("Mod download completed: {} -> {:?}", mod_id, dest_path);
    Ok(())
}

/// Streams the response body into `part_path`, hashing as it goes, and
/// verifies the checksum once the body is exhausted. Returns the total bytes
/// downloaded. The caller is responsible for removing `part_path` on error.
fn stream_and_verify(
    mod_id: &str,
    response: &mut reqwest::blocking::Response,
    part_path: &Path,
    total_size: Option<u64>,
    expected_checksum: Option<String>,
) -> AppResult<u64> {
    let mut file = File::create(part_path)?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut last_pct: i64 = -1;

    emit_progress(mod_id, 0, total_size, "downloading");

    loop {
        let n = response.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        file.write_all(&chunk[..n])?;
        downloaded += n as u64;
        // Throttle to whole-percent changes (or a single emit when the size is
        // unknown/zero) so a large download doesn't flood the event bus.
        let pct = total_size
            .and_then(|t| (downloaded * 100).checked_div(t))
            .unwrap_or(0) as i64;
        if pct != last_pct {
            last_pct = pct;
            emit_progress(mod_id, downloaded, total_size, "downloading");
        }
    }
    drop(file);

    emit_progress(mod_id, downloaded, total_size, "verifying");
    let computed_checksum = crate::backend::services::hex_digest(hasher.finalize());
    if let Some(expected_checksum) = expected_checksum.filter(|checksum| !checksum.is_empty())
        && computed_checksum != expected_checksum.to_lowercase()
    {
        return Err(AppError::validation(format!(
            "Checksum mismatch: expected {}, got {}",
            expected_checksum, computed_checksum
        )));
    }

    Ok(downloaded)
}

fn get_tracking_id() -> AppResult<String> {
    use std::fs;
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
