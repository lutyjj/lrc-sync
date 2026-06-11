use std::{env, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanAction {
    Keep,
    Reconcile,
    Quarantine,
    Delete,
}

impl OrphanAction {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "keep" => Ok(Self::Keep),
            "reconcile" => Ok(Self::Reconcile),
            "quarantine" => Ok(Self::Quarantine),
            "delete" => Ok(Self::Delete),
            other => bail!("invalid LRCGET_ORPHAN_ACTION={other:?}; expected keep, reconcile, quarantine, or delete"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub music_dir: PathBuf,
    pub db_file: PathBuf,
    pub concurrency: usize,
    pub clean_fallback: bool,
    pub retry_not_found_days: u64,
    pub request_interval: Duration,
    pub orphan_action: OrphanAction,
    pub follow_symlinks: bool,
    pub fallback_interval: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            music_dir: PathBuf::from(
                env::var("LRCGET_MUSIC_PATH").unwrap_or_else(|_| "/music".to_string()),
            ),
            db_file: PathBuf::from(
                env::var("LRCGET_DB_PATH").unwrap_or_else(|_| "/config/lyrics.db".to_string()),
            ),
            concurrency: parse_usize("LRCGET_CONCURRENCY", 8, 1, 128)?,
            clean_fallback: parse_bool("LRCGET_CLEAN_FALLBACK", true)?,
            retry_not_found_days: parse_u64("LRCGET_RETRY_NOT_FOUND_DAYS", 7, 1, 3650)?,
            request_interval: Duration::from_millis(parse_u64(
                "LRCGET_REQUEST_INTERVAL_MS",
                750,
                0,
                60_000,
            )?),
            orphan_action: OrphanAction::parse(
                &env::var("LRCGET_ORPHAN_ACTION").unwrap_or_else(|_| "keep".to_string()),
            )?,
            follow_symlinks: parse_bool("LRCGET_FOLLOW_SYMLINKS", false)?,
            fallback_interval: Duration::from_secs(parse_u64(
                "LRCGET_FALLBACK_SCAN_SECONDS",
                43_200,
                60,
                604_800,
            )?),
        })
    }
}

fn parse_usize(name: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => {
            let value = raw
                .trim()
                .parse::<usize>()
                .with_context(|| format!("{name} must be an integer"))?;
            if !(min..=max).contains(&value) {
                bail!("{name} must be between {min} and {max}, got {value}");
            }
            Ok(value)
        }
        _ => Ok(default),
    }
}

fn parse_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => {
            let value = raw
                .trim()
                .parse::<u64>()
                .with_context(|| format!("{name} must be an integer"))?;
            if !(min..=max).contains(&value) {
                bail!("{name} must be between {min} and {max}, got {value}");
            }
            Ok(value)
        }
        _ => Ok(default),
    }
}

fn parse_bool(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" => Ok(true),
            "0" | "false" | "no" | "n" | "off" => Ok(false),
            _ => bail!("{name} must be a boolean"),
        },
        _ => Ok(default),
    }
}
