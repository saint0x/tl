//! System-wide configuration for Timelapse
//!
//! System config is stored at `~/.config/tl/config.toml` (Linux/macOS)
//! or `%APPDATA%\tl\config.toml` (Windows).
//!
//! This config is separate from project-level `.tl/config.toml` and contains
//! user preferences that apply across all repositories.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// System-wide Timelapse configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    /// Daemon configuration
    pub daemon: DaemonConfig,

    /// Garbage collection configuration
    pub gc: GcConfig,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            gc: GcConfig::default(),
        }
    }
}

/// Daemon configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Checkpoint creation interval in seconds (default: 5)
    pub checkpoint_interval_secs: u64,

    /// Whether to enable auto-GC (default: true)
    pub auto_gc_enabled: bool,

    /// Auto-GC interval in seconds (default: 3600 = 1 hour)
    /// GC runs when checkpoint count exceeds threshold OR interval has passed
    pub auto_gc_interval_secs: u64,

    /// Auto-GC checkpoint threshold (default: 5000)
    /// Triggers GC when checkpoint count exceeds this
    pub auto_gc_checkpoint_threshold: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            checkpoint_interval_secs: 5,
            auto_gc_enabled: true,
            auto_gc_interval_secs: 3600, // 1 hour
            auto_gc_checkpoint_threshold: 5000,
        }
    }
}

/// Garbage collection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GcConfig {
    /// Number of checkpoints to retain (default: 2000)
    pub retain_count: usize,

    /// Time window to keep all checkpoints (in hours, default: 24)
    pub retain_hours: u64,

    /// Always retain pinned checkpoints (default: true)
    pub retain_pins: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            retain_count: 2000,
            retain_hours: 24,
            retain_pins: true,
        }
    }
}

impl GcConfig {
    /// Convert to journal::RetentionPolicy
    pub fn to_retention_policy(&self) -> journal::RetentionPolicy {
        journal::RetentionPolicy {
            retain_dense_count: self.retain_count,
            retain_dense_window_ms: self.retain_hours * 60 * 60 * 1000, // hours to ms
            retain_pins: self.retain_pins,
        }
    }
}

/// Get the system config directory path
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join(".config/tl"))
    }

    #[cfg(target_os = "linux")]
    {
        dirs::config_dir().map(|c| c.join("tl"))
    }

    #[cfg(target_os = "windows")]
    {
        dirs::config_dir().map(|c| c.join("tl"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        dirs::home_dir().map(|h| h.join(".config/tl"))
    }
}

/// Get the system config file path
pub fn config_file_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Load system configuration
///
/// Returns default config if file doesn't exist.
/// Creates default config file if directory exists but file doesn't.
pub fn load() -> Result<SystemConfig> {
    let config_path = match config_file_path() {
        Some(p) => p,
        None => {
            tracing::debug!("Could not determine config directory, using defaults");
            return Ok(SystemConfig::default());
        }
    };

    if !config_path.exists() {
        tracing::debug!("System config not found at {}, using defaults", config_path.display());
        return Ok(SystemConfig::default());
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read system config at {}", config_path.display()))?;

    let config: SystemConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse system config at {}", config_path.display()))?;

    tracing::debug!("Loaded system config from {}", config_path.display());
    Ok(config)
}

/// Save system configuration
pub fn save(config: &SystemConfig) -> Result<()> {
    let config_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine system config directory"))?;

    // Ensure config directory exists
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("Failed to create config directory at {}", config_dir.display()))?;

    let config_path = config_dir.join("config.toml");
    let content = toml::to_string_pretty(config)
        .context("Failed to serialize system config")?;

    fs::write(&config_path, &content)
        .with_context(|| format!("Failed to write system config to {}", config_path.display()))?;

    tracing::info!("Saved system config to {}", config_path.display());
    Ok(())
}

/// Initialize system config with defaults if it doesn't exist
pub fn init_if_missing() -> Result<()> {
    let config_path = match config_file_path() {
        Some(p) => p,
        None => return Ok(()), // Can't determine path, skip
    };

    if config_path.exists() {
        return Ok(()); // Already exists
    }

    // Create default config
    let config = SystemConfig::default();
    save(&config)?;

    Ok(())
}

/// Generate example config content for display
pub fn example_config() -> String {
    let config = SystemConfig::default();
    let mut content = String::from("# Timelapse System Configuration\n");
    content.push_str("# Location: ~/.config/tl/config.toml\n");
    content.push_str("#\n");
    content.push_str("# This file configures system-wide Timelapse behavior.\n");
    content.push_str("# Project-specific settings are in .tl/config.toml\n\n");

    content.push_str(&toml::to_string_pretty(&config).unwrap_or_default());
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SystemConfig::default();

        assert_eq!(config.daemon.checkpoint_interval_secs, 5);
        assert!(config.daemon.auto_gc_enabled);
        assert_eq!(config.daemon.auto_gc_interval_secs, 3600);
        assert_eq!(config.daemon.auto_gc_checkpoint_threshold, 5000);

        assert_eq!(config.gc.retain_count, 2000);
        assert_eq!(config.gc.retain_hours, 24);
        assert!(config.gc.retain_pins);
    }

    #[test]
    fn test_retention_policy_conversion() {
        let gc_config = GcConfig {
            retain_count: 1000,
            retain_hours: 12,
            retain_pins: false,
        };

        let policy = gc_config.to_retention_policy();

        assert_eq!(policy.retain_dense_count, 1000);
        assert_eq!(policy.retain_dense_window_ms, 12 * 60 * 60 * 1000);
        assert!(!policy.retain_pins);
    }

    #[test]
    fn test_config_serialization() {
        let config = SystemConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: SystemConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.daemon.auto_gc_interval_secs, parsed.daemon.auto_gc_interval_secs);
        assert_eq!(config.gc.retain_count, parsed.gc.retain_count);
    }
}
