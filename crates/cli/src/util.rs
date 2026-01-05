//! Shared utilities for CLI commands

use anyhow::{Context, Result};
use journal::{Checkpoint, Journal, PinManager};
use owo_colors::OwoColorize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use ulid::Ulid;

/// Find repository root by walking up from cwd to find .tl/
pub fn find_repo_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir()
        .context("Failed to get current directory")?;

    loop {
        let tl_dir = current.join(".tl");
        if tl_dir.exists() && tl_dir.is_dir() {
            return Ok(current);
        }

        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => anyhow::bail!("Not a Timelapse repository (no .tl directory found)"),
        }
    }
}

/// Resolve checkpoint reference to ULID
/// Supports:
/// - Full ULID: "01HN8XYZ..."
/// - Short ULID prefix: "01HN8" (must be unique)
/// - Pin name: "my-pin"
pub fn resolve_checkpoint_ref(
    reference: &str,
    journal: &Journal,
    pin_manager: &PinManager,
) -> Result<Ulid> {
    // Try parsing as ULID first
    if let Ok(ulid) = Ulid::from_string(reference) {
        // Verify it exists
        if journal.get(&ulid)?.is_some() {
            return Ok(ulid);
        } else {
            anyhow::bail!("Checkpoint not found: {}", reference);
        }
    }

    // Try as ULID prefix
    if reference.len() >= 4 {
        let all_checkpoints = journal.all_checkpoint_ids()?;
        let matching: Vec<_> = all_checkpoints
            .iter()
            .filter(|id| id.to_string().starts_with(reference))
            .collect();

        if matching.len() == 1 {
            return Ok(*matching[0]);
        } else if matching.len() > 1 {
            anyhow::bail!(
                "Ambiguous checkpoint prefix '{}': matches {} checkpoints",
                reference,
                matching.len()
            );
        }
    }

    // Try as pin name
    let pins = pin_manager.list_pins()?;
    for (name, ulid) in pins {
        if name == reference {
            return Ok(ulid);
        }
    }

    anyhow::bail!("Unknown checkpoint reference: '{}'", reference)
}

/// Format timestamp as relative time ("2 hours ago")
pub fn format_relative_time(ts_ms: u64) -> String {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let duration = Duration::from_millis(ts_ms);
    let datetime = UNIX_EPOCH + duration;

    if let Ok(elapsed) = SystemTime::now().duration_since(datetime) {
        let seconds = elapsed.as_secs();

        if seconds < 60 {
            format!("{} seconds ago", seconds)
        } else if seconds < 3600 {
            format!("{} minutes ago", seconds / 60)
        } else if seconds < 86400 {
            format!("{} hours ago", seconds / 3600)
        } else if seconds < 604800 {
            format!("{} days ago", seconds / 86400)
        } else {
            format!("{} weeks ago", seconds / 604800)
        }
    } else {
        "in the future".to_string()
    }
}

/// Format timestamp as absolute time ("2024-01-03 14:30:00")
pub fn format_absolute_time(ts_ms: u64) -> String {
    use std::time::Duration;

    let duration = Duration::from_millis(ts_ms);

    // Format as UTC time string (simplified civil calendar calculation)
    let secs = duration.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    // Calculate approximate date from Unix epoch
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let epoch_days = days + 719468; // Days from 0000-01-01 to 1970-01-01
    let era = epoch_days / 146097;
    let doe = epoch_days - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, m, d, hours, minutes, seconds
    )
}

/// Format file size in human-readable format
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format duration in user-friendly format
/// Shows the most significant unit without being overly granular
pub fn format_duration(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = MINUTE * 60;
    const DAY: u64 = HOUR * 24;
    const WEEK: u64 = DAY * 7;
    const MONTH: u64 = DAY * 30;
    const YEAR: u64 = DAY * 365;

    if secs < 5 {
        "just now".to_string()
    } else if secs < MINUTE {
        format!("{} seconds", secs)
    } else if secs < HOUR {
        let mins = secs / MINUTE;
        if mins == 1 { "1 minute".to_string() } else { format!("{} minutes", mins) }
    } else if secs < DAY {
        let hours = secs / HOUR;
        if hours == 1 { "1 hour".to_string() } else { format!("{} hours", hours) }
    } else if secs < WEEK {
        let days = secs / DAY;
        if days == 1 { "1 day".to_string() } else { format!("{} days", days) }
    } else if secs < MONTH {
        let weeks = secs / WEEK;
        if weeks == 1 { "1 week".to_string() } else { format!("{} weeks", weeks) }
    } else if secs < YEAR {
        let months = secs / MONTH;
        if months == 1 { "1 month".to_string() } else { format!("{} months", months) }
    } else {
        let years = secs / YEAR;
        if years == 1 { "1 year".to_string() } else { format!("{} years", years) }
    }
}

/// Display a checkpoint in compact format
pub fn display_checkpoint_compact(cp: &Checkpoint, show_ulid: bool) {
    let id_short = cp.id.to_string()[..8].to_string();
    let time_str = format_relative_time(cp.ts_unix_ms);
    let reason = match cp.reason {
        journal::CheckpointReason::FsBatch => "fs",
        journal::CheckpointReason::Manual => "manual",
        journal::CheckpointReason::Restore => "restore",
        journal::CheckpointReason::Publish => "publish",
        journal::CheckpointReason::GcCompact => "gc",
        journal::CheckpointReason::WorkspaceSave => "workspace",
    };

    if show_ulid {
        println!(
            "{} {} {} - {} files",
            id_short.yellow(),
            time_str.dimmed(),
            reason.cyan(),
            cp.meta.files_changed
        );
    } else {
        println!(
            "{} {} - {} files",
            time_str.dimmed(),
            reason.cyan(),
            cp.meta.files_changed
        );
    }
}

/// Build a map from checkpoint ULID to pin names
pub fn build_pin_map(pin_manager: &PinManager) -> Result<HashMap<Ulid, Vec<String>>> {
    let pins = pin_manager.list_pins()?;
    let mut map: HashMap<Ulid, Vec<String>> = HashMap::new();
    for (name, ulid) in pins {
        map.entry(ulid).or_insert_with(Vec::new).push(name);
    }
    Ok(map)
}

/// Calculate directory size recursively
pub fn calculate_dir_size(dir: &Path) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut total = 0u64;

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            total += entry.metadata()?.len();
        } else if path.is_dir() {
            total += calculate_dir_size(&path)?;
        }
    }

    Ok(total)
}

// ============================================================================
// Git Integration Utilities
// ============================================================================

/// Detect if a .git directory exists at the given path
pub fn detect_git_repo(path: &Path) -> Result<bool> {
    let git_dir = path.join(".git");
    Ok(git_dir.exists() && git_dir.is_dir())
}

/// Parse git config for user.name and user.email
///
/// Returns Some((name, email)) if both are found, None otherwise.
/// Handles malformed configs gracefully by returning None.
pub fn parse_git_user_config(repo_root: &Path) -> Result<Option<(String, String)>> {
    // Check global configs first, then local (local overrides global)
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);

    let mut config_paths = Vec::new();

    // Global configs (checked first, can be overridden)
    if let Some(ref h) = home {
        config_paths.push(h.join(".config/git/config")); // XDG location
        config_paths.push(h.join(".gitconfig"));          // Traditional location
    }

    // Local repo config (checked last, overrides global)
    config_paths.push(repo_root.join(".git/config"));

    let mut name = None;
    let mut email = None;

    for config_path in &config_paths {
        if !config_path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let (parsed_name, parsed_email) = parse_git_config_user_section(&content);
        if parsed_name.is_some() {
            name = parsed_name;
        }
        if parsed_email.is_some() {
            email = parsed_email;
        }
    }

    match (name, email) {
        (Some(n), Some(e)) => Ok(Some((n, e))),
        _ => Ok(None),
    }
}

fn parse_git_config_user_section(content: &str) -> (Option<String>, Option<String>) {
    let mut in_user_section = false;
    let mut name = None;
    let mut email = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Check for [user] section
        if trimmed == "[user]" {
            in_user_section = true;
            continue;
        }

        // Check if we're leaving the [user] section
        if in_user_section && trimmed.starts_with('[') {
            break;
        }

        if in_user_section {
            // Parse name = value
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');

                match key {
                    "name" => name = Some(value.to_string()),
                    "email" => email = Some(value.to_string()),
                    _ => {}
                }
            }
        }
    }

    (name, email)
}

/// Extract git remotes from .git/config
///
/// Returns Vec<(remote_name, url)> for all remotes found.
/// Handles malformed configs by skipping problematic entries.
pub fn parse_git_remotes(repo_root: &Path) -> Result<Vec<(String, String)>> {
    let config_path = repo_root.join(".git/config");

    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };

    let mut remotes = Vec::new();
    let mut current_remote: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Match [remote "name"] sections
        if trimmed.starts_with("[remote \"") && trimmed.ends_with("\"]") {
            // Extract remote name from [remote "origin"]
            let start = "[remote \"".len();
            let end = trimmed.len() - "\"]".len();
            current_remote = Some(trimmed[start..end].to_string());
            continue;
        }

        // Leaving remote section
        if current_remote.is_some() && trimmed.starts_with('[') && !trimmed.starts_with("[remote") {
            current_remote = None;
        }

        // Parse url in remote section
        if let Some(ref remote_name) = current_remote {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "url" {
                    let url = value.trim().trim_matches('"');
                    remotes.push((remote_name.clone(), url.to_string()));
                }
            }
        }
    }

    Ok(remotes)
}

/// Ensure .gitignore includes specified patterns (idempotent)
///
/// Creates .gitignore if it doesn't exist. Appends patterns only if they're not already present.
/// Uses atomic writes to prevent corruption.
pub fn ensure_gitignore_patterns(repo_root: &Path, patterns: &[&str]) -> Result<()> {
    use std::fs;
    use std::io::Write;

    let gitignore_path = repo_root.join(".gitignore");

    // Read existing content or start with empty
    let existing_content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)
            .context("Failed to read .gitignore")?
    } else {
        String::new()
    };

    let existing_lines: Vec<&str> = existing_content.lines().collect();

    // Filter patterns to only those not already present
    let mut missing_patterns = Vec::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        if !existing_lines.iter().any(|line| line.trim() == trimmed) {
            missing_patterns.push(trimmed);
        }
    }

    // If all patterns exist, nothing to do
    if missing_patterns.is_empty() {
        return Ok(());
    }

    // Build new content
    let mut new_content = existing_content.clone();

    // Ensure trailing newline if content exists
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    // Add header comment if we're adding patterns
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str("# Timelapse (added by tl init)\n");

    for pattern in missing_patterns {
        new_content.push_str(pattern);
        new_content.push('\n');
    }

    // Atomic write using temp file + rename
    let temp_path = gitignore_path.with_extension("gitignore.tmp");
    let mut temp_file = fs::File::create(&temp_path)
        .context("Failed to create temporary .gitignore")?;

    temp_file.write_all(new_content.as_bytes())
        .context("Failed to write to temporary .gitignore")?;

    temp_file.sync_all()
        .context("Failed to sync temporary .gitignore")?;

    drop(temp_file);

    fs::rename(&temp_path, &gitignore_path)
        .context("Failed to rename temporary .gitignore")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_size(1536), "1.50 KB");
    }

    #[test]
    fn test_format_relative_time() {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Test current time
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let result = format_relative_time(now_ms);
        assert!(result.contains("seconds ago") || result == "0 seconds ago");

        // Test 1 hour ago
        let one_hour_ago = now_ms - (3600 * 1000);
        let result = format_relative_time(one_hour_ago);
        assert!(result.contains("hour"));

        // Test 1 day ago
        let one_day_ago = now_ms - (86400 * 1000);
        let result = format_relative_time(one_day_ago);
        assert!(result.contains("day"));
    }

    // ========================================================================
    // Git utilities tests
    // ========================================================================

    #[test]
    fn test_detect_git_repo() -> Result<()> {
        use tempfile::TempDir;

        let temp = TempDir::new()?;
        assert!(!detect_git_repo(temp.path())?, "Should not detect git in empty dir");

        // Create .git directory
        std::fs::create_dir(temp.path().join(".git"))?;
        assert!(detect_git_repo(temp.path())?, "Should detect git directory");

        Ok(())
    }

    #[test]
    fn test_parse_git_user_config() -> Result<()> {
        use tempfile::TempDir;

        let temp = TempDir::new()?;
        std::fs::create_dir(temp.path().join(".git"))?;

        // Isolate test from real global git config by setting HOME to temp dir
        // This prevents the test from finding ~/.gitconfig
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp.path());

        // Test with no config (and no global config due to HOME override)
        let result = parse_git_user_config(temp.path())?;
        assert!(result.is_none(), "Should return None when no config exists (local or global)");

        // Test with valid local config
        let config = r#"
[core]
    repositoryformatversion = 0

[user]
    name = John Doe
    email = john@example.com

[remote "origin"]
    url = https://github.com/user/repo.git
"#;
        std::fs::write(temp.path().join(".git/config"), config)?;

        let result = parse_git_user_config(temp.path())?;
        assert_eq!(
            result,
            Some(("John Doe".to_string(), "john@example.com".to_string())),
            "Should parse user name and email from local config"
        );

        // Test with missing email (no global fallback)
        let config_no_email = r#"
[user]
    name = Jane Doe
"#;
        std::fs::write(temp.path().join(".git/config"), config_no_email)?;

        let result = parse_git_user_config(temp.path())?;
        assert!(result.is_none(), "Should return None when email is missing and no global config");

        // Test global config fallback: create a global .gitconfig
        let global_config = r#"
[user]
    name = Global User
    email = global@example.com
"#;
        std::fs::write(temp.path().join(".gitconfig"), global_config)?;

        // Clear local config
        std::fs::write(temp.path().join(".git/config"), "[core]\n")?;

        let result = parse_git_user_config(temp.path())?;
        assert_eq!(
            result,
            Some(("Global User".to_string(), "global@example.com".to_string())),
            "Should fall back to global config when local config has no user section"
        );

        // Test local override of global: local should take precedence
        let local_override = r#"
[user]
    name = Local Override
    email = local@example.com
"#;
        std::fs::write(temp.path().join(".git/config"), local_override)?;

        let result = parse_git_user_config(temp.path())?;
        assert_eq!(
            result,
            Some(("Local Override".to_string(), "local@example.com".to_string())),
            "Local config should override global config"
        );

        // Restore original HOME
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        }

        Ok(())
    }

    #[test]
    fn test_parse_git_remotes() -> Result<()> {
        use tempfile::TempDir;

        let temp = TempDir::new()?;
        std::fs::create_dir(temp.path().join(".git"))?;

        // Test with no remotes
        let config_no_remotes = r#"
[core]
    repositoryformatversion = 0
"#;
        std::fs::write(temp.path().join(".git/config"), config_no_remotes)?;

        let result = parse_git_remotes(temp.path())?;
        assert!(result.is_empty(), "Should return empty vec with no remotes");

        // Test with single remote
        let config_single_remote = r#"
[remote "origin"]
    url = https://github.com/user/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
"#;
        std::fs::write(temp.path().join(".git/config"), config_single_remote)?;

        let result = parse_git_remotes(temp.path())?;
        assert_eq!(result.len(), 1, "Should find one remote");
        assert_eq!(result[0].0, "origin", "Remote name should be origin");
        assert_eq!(
            result[0].1,
            "https://github.com/user/repo.git",
            "Remote URL should match"
        );

        // Test with multiple remotes
        let config_multiple_remotes = r#"
[remote "origin"]
    url = https://github.com/user/repo.git

[remote "upstream"]
    url = https://github.com/upstream/repo.git
"#;
        std::fs::write(temp.path().join(".git/config"), config_multiple_remotes)?;

        let result = parse_git_remotes(temp.path())?;
        assert_eq!(result.len(), 2, "Should find two remotes");

        Ok(())
    }

    #[test]
    fn test_ensure_gitignore_patterns() -> Result<()> {
        use tempfile::TempDir;

        let temp = TempDir::new()?;

        // Test creating new .gitignore
        ensure_gitignore_patterns(temp.path(), &[".tl/", "*.log"])?;

        let content = std::fs::read_to_string(temp.path().join(".gitignore"))?;
        assert!(content.contains(".tl/"), "Should contain .tl/ pattern");
        assert!(content.contains("*.log"), "Should contain *.log pattern");
        assert!(content.contains("# Timelapse"), "Should contain header comment");

        // Test idempotency - adding same patterns again
        ensure_gitignore_patterns(temp.path(), &[".tl/"])?;

        let content2 = std::fs::read_to_string(temp.path().join(".gitignore"))?;
        assert_eq!(
            content2.matches(".tl/").count(),
            1,
            "Should not duplicate patterns"
        );

        // Test appending new pattern to existing file
        ensure_gitignore_patterns(temp.path(), &["*.tmp"])?;

        let content3 = std::fs::read_to_string(temp.path().join(".gitignore"))?;
        assert!(content3.contains("*.tmp"), "Should append new pattern");
        assert!(content3.contains(".tl/"), "Should preserve existing patterns");

        Ok(())
    }
}
