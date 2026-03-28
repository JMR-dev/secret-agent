use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub db_path: PathBuf,
    pub role_dirs: Vec<PathBuf>,
}

impl AppPaths {
    pub fn resolve(db_override: Option<PathBuf>, role_overrides: &[PathBuf]) -> Result<Self> {
        let cwd = env::current_dir().context("failed to resolve current working directory")?;
        let config_dir = cwd.join(".secret-agent");
        fs::create_dir_all(&config_dir).context("failed to create config directory")?;

        let db_path = db_override.unwrap_or_else(|| config_dir.join("history.sqlite3"));
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).context("failed to create database parent directory")?;
        }

        let role_dirs = if role_overrides.is_empty() {
            vec![cwd.join("roles"), config_dir.join("roles")]
        } else {
            role_overrides.to_vec()
        };

        Ok(Self {
            cwd,
            config_dir,
            db_path,
            role_dirs,
        })
    }
}
