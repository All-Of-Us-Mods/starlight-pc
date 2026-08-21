use crate::backend::error::AppResult;
use crate::backend::services::http_download::{download_file, extract_zip};
use log::{debug, info, warn};
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BepInExTargetType {
    Profile,
    Cache,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BepInExProgress {
    pub stage: String,
    pub progress: f64,
    pub message: String,
    pub target_type: BepInExTargetType,
    pub target_id: String,
}

fn emit(
    stage: &str,
    progress: f64,
    message: &str,
    target_type: BepInExTargetType,
    target_id: &str,
) {
    crate::backend::events::publish(crate::backend::events::BackendEvent::BepInExProgress(
        BepInExProgress {
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
            target_type,
            target_id: target_id.to_string(),
        },
    ));
}

/// `download_file` progress callback: emit a "downloading" event with the
/// percent computed once. No-op until the total size is known.
fn emit_download_progress(
    downloaded: u64,
    total: Option<u64>,
    target_type: BepInExTargetType,
    target_id: &str,
) {
    if let Some(total) = total {
        let pct = downloaded as f64 / total as f64 * 100.0;
        emit(
            "downloading",
            pct,
            &format!("Downloading... {pct:.0}%"),
            target_type,
            target_id,
        );
    }
}

/// `extract_zip` progress callback: emit an "extracting" event for entry
/// `current` of `total`.
fn emit_extract_progress(
    current: usize,
    total: usize,
    target_type: BepInExTargetType,
    target_id: &str,
) {
    emit(
        "extracting",
        current as f64 / total as f64 * 100.0,
        &format!("Extracting {current}/{total}"),
        target_type,
        target_id,
    );
}

pub fn install_bepinex(
    url: String,
    destination: String,
    cache_path: Option<String>,
    target_type: BepInExTargetType,
    target_id: &str,
) -> AppResult<()> {
    info!("install_bepinex: {} -> {}", url, destination);
    let dest = Path::new(&destination);

    if let Some(ref cache) = cache_path {
        let cache_file = Path::new(cache);
        if cache_file.exists() {
            info!("Using cached BepInEx");
            emit(
                "extracting",
                0.0,
                "Using cached BepInEx...",
                target_type,
                target_id,
            );
            extract_zip(cache_file, dest, |cur, total| {
                emit_extract_progress(cur, total, target_type, target_id)
            })?;
            emit("complete", 100.0, "Complete!", target_type, target_id);
            return Ok(());
        }
    }

    let temp = dest.with_extension("zip.tmp");
    emit("downloading", 0.0, "Downloading...", target_type, target_id);
    download_file(&url, &temp, None, None, |dl, total| {
        emit_download_progress(dl, total, target_type, target_id)
    })?;

    if let Some(ref cache) = cache_path {
        let cache_file = Path::new(cache);
        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Err(e) = fs::copy(&temp, cache_file) {
            warn!("Failed to cache: {}", e);
        } else {
            debug!("Cached to {:?}", cache_file);
        }
    }

    emit("extracting", 0.0, "Extracting...", target_type, target_id);
    extract_zip(&temp, dest, |cur, total| {
        emit_extract_progress(cur, total, target_type, target_id)
    })?;

    fs::remove_file(&temp).ok();
    emit("complete", 100.0, "Complete!", target_type, target_id);
    Ok(())
}

pub fn download_bepinex_to_cache(
    url: String,
    cache_path: String,
    architecture: String,
) -> AppResult<()> {
    let cache_file = Path::new(&cache_path);

    emit(
        "downloading",
        0.0,
        "Downloading...",
        BepInExTargetType::Cache,
        &architecture,
    );
    download_file(&url, cache_file, None, None, |dl, total| {
        emit_download_progress(dl, total, BepInExTargetType::Cache, &architecture)
    })?;

    emit(
        "complete",
        100.0,
        "Complete!",
        BepInExTargetType::Cache,
        &architecture,
    );
    Ok(())
}

pub fn clear_cache(cache_path: String, architecture: String) -> AppResult<()> {
    let cache_file = Path::new(&cache_path);
    if cache_file.exists() {
        fs::remove_file(cache_file)?;
    }
    emit(
        "cleared",
        0.0,
        "Cache cleared",
        BepInExTargetType::Cache,
        &architecture,
    );
    Ok(())
}

pub fn cache_size(cache_path: &str) -> Option<u64> {
    fs::metadata(cache_path).ok().map(|m| m.len())
}
