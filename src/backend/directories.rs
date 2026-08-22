use crate::backend::error::{AppError, AppResult};
use directories::ProjectDirs;
use std::path::PathBuf;

pub fn app_data_dir() -> AppResult<PathBuf> {
    if let Some(proj_dirs) = ProjectDirs::from("dev", "allofus", "Starlight") {
        Ok(proj_dirs.data_dir().to_path_buf())
    } else {
        Err(AppError::state("Could not determine app data directory"))
    }
}
