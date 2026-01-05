//! Unified data access layer for all commands
//! Provides consistent IPC-first approach with direct-access fallback

use anyhow::{Context, Result};
use journal::{Checkpoint, Journal, PinManager};
use std::path::Path;
use ulid::Ulid;

/// Unified checkpoint resolver - uses IPC first, falls back to direct access
///
/// Resolves checkpoint references which can be:
/// - Full ULID strings (26 characters)
/// - Short prefixes (4+ characters)
/// - Pin names
///
/// Returns Ulid for each reference. Returns None if not found or ambiguous.
pub async fn resolve_checkpoint_refs(
    refs: &[String],
    tl_dir: &Path,
) -> Result<Vec<Option<Ulid>>> {
    // Try IPC first (daemon running)
    if let Ok(Some(ids)) = try_resolve_via_ipc(refs, tl_dir).await {
        return Ok(ids);
    }

    // Fallback: direct journal access (daemon not running)
    resolve_via_journal(refs, tl_dir)
}

/// Get checkpoint data - uses IPC first, falls back to direct access
pub async fn get_checkpoints(
    ids: &[Ulid],
    tl_dir: &Path,
) -> Result<Vec<Option<Checkpoint>>> {
    // Try IPC first
    if let Ok(Some(checkpoints)) = try_get_via_ipc(ids, tl_dir).await {
        return Ok(checkpoints);
    }

    // Fallback: direct journal access
    get_via_journal(ids, tl_dir)
}

/// Get repository info - uses IPC first, falls back to direct access
///
/// Returns (total_checkpoints, checkpoint_ids, store_size_bytes)
pub async fn get_info_data(
    tl_dir: &Path,
) -> Result<(usize, Vec<String>, u64)> {
    // Try IPC first
    if let Ok(Some(data)) = try_get_info_via_ipc(tl_dir).await {
        return Ok(data);
    }

    // Fallback: direct journal access
    get_info_via_journal(tl_dir)
}

// ============================================================================
// Private helper functions - IPC access
// ============================================================================

/// Try to resolve checkpoint references via IPC
async fn try_resolve_via_ipc(
    refs: &[String],
    tl_dir: &Path,
) -> Result<Option<Vec<Option<Ulid>>>> {
    let socket_path = tl_dir.join("state/daemon.sock");

    if !socket_path.exists() {
        return Ok(None); // Daemon not running
    }

    match crate::ipc::IpcClient::connect(&socket_path).await {
        Ok(mut client) => {
            // Resolve via IPC
            let checkpoints = client.resolve_checkpoint_refs(refs.to_vec()).await?;

            // Extract ULIDs from checkpoints
            let ids: Vec<Option<Ulid>> = checkpoints.into_iter()
                .map(|opt_cp| opt_cp.map(|cp| cp.id))
                .collect();

            Ok(Some(ids))
        }
        Err(_) => Ok(None), // Can't connect to daemon
    }
}

/// Try to get checkpoints via IPC
async fn try_get_via_ipc(
    ids: &[Ulid],
    tl_dir: &Path,
) -> Result<Option<Vec<Option<Checkpoint>>>> {
    let socket_path = tl_dir.join("state/daemon.sock");

    if !socket_path.exists() {
        return Ok(None);
    }

    match crate::ipc::IpcClient::connect(&socket_path).await {
        Ok(mut client) => {
            let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            let checkpoints = client.get_checkpoint_batch(id_strings).await?;
            Ok(Some(checkpoints))
        }
        Err(_) => Ok(None),
    }
}

/// Try to get info data via IPC
async fn try_get_info_via_ipc(
    tl_dir: &Path,
) -> Result<Option<(usize, Vec<String>, u64)>> {
    let socket_path = tl_dir.join("state/daemon.sock");

    if !socket_path.exists() {
        return Ok(None);
    }

    match crate::ipc::IpcClient::connect(&socket_path).await {
        Ok(mut client) => {
            let data = client.get_info_data().await?;
            Ok(Some(data))
        }
        Err(_) => Ok(None),
    }
}

// ============================================================================
// Private helper functions - Direct journal access
// ============================================================================

/// Resolve checkpoint references via direct journal access
fn resolve_via_journal(
    refs: &[String],
    tl_dir: &Path,
) -> Result<Vec<Option<Ulid>>> {
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let pin_manager = PinManager::new(tl_dir);
    let mut results = Vec::new();

    for checkpoint_ref in refs {
        // Handle HEAD alias
        if checkpoint_ref == "HEAD" {
            if let Ok(Some(cp)) = journal.latest() {
                results.push(Some(cp.id));
                continue;
            }
            results.push(None);
            continue;
        }

        // Try full ULID first
        if let Ok(ulid) = Ulid::from_string(checkpoint_ref) {
            match journal.get(&ulid) {
                Ok(Some(_)) => {
                    results.push(Some(ulid));
                    continue;
                }
                _ => {}
            }
        }

        // Try short prefix (4+ chars)
        if checkpoint_ref.len() >= 4 {
            let all_ids = journal.all_checkpoint_ids()?;
            let matching: Vec<_> = all_ids
                .iter()
                .filter(|id| id.to_string().starts_with(checkpoint_ref))
                .collect();

            if matching.len() == 1 {
                results.push(Some(*matching[0]));
                continue;
            } else if matching.len() > 1 {
                results.push(None); // Ambiguous
                continue;
            }
        }

        // Try pin name
        match pin_manager.list_pins() {
            Ok(pins) => {
                if let Some((_, ulid)) = pins.iter().find(|(name, _)| name == checkpoint_ref) {
                    results.push(Some(*ulid));
                    continue;
                }
            }
            _ => {}
        }

        // Not found
        results.push(None);
    }

    Ok(results)
}

/// Get checkpoints via direct journal access
fn get_via_journal(
    ids: &[Ulid],
    tl_dir: &Path,
) -> Result<Vec<Option<Checkpoint>>> {
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let mut results = Vec::new();
    for id in ids {
        let checkpoint = journal.get(id)?;
        results.push(checkpoint);
    }

    Ok(results)
}

/// Get info data via direct journal access
fn get_info_via_journal(
    tl_dir: &Path,
) -> Result<(usize, Vec<String>, u64)> {
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    // Get total checkpoints
    let total_checkpoints = journal.count();

    // Get all checkpoint IDs
    let checkpoint_ids: Vec<String> = journal
        .all_checkpoint_ids()?
        .iter()
        .map(|id| id.to_string())
        .collect();

    // Calculate store size
    let store_path = tl_dir.join("store");
    let store_size_bytes = calculate_dir_size(&store_path).unwrap_or(0);

    Ok((total_checkpoints, checkpoint_ids, store_size_bytes))
}

/// Calculate total size of a directory recursively
fn calculate_dir_size(path: &Path) -> Result<u64> {
    let mut total_size = 0u64;

    if !path.exists() {
        return Ok(0);
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total_size += metadata.len();
        } else if metadata.is_dir() {
            total_size += calculate_dir_size(&entry.path())?;
        }
    }
    Ok(total_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use journal::{CheckpointMeta, CheckpointReason};
    use tempfile::TempDir;
    use tl_core::Sha1Hash;

    fn create_test_checkpoint(parent: Option<Ulid>) -> Checkpoint {
        let root_tree = Sha1Hash::from_bytes([0u8; 20]);
        let meta = CheckpointMeta {
            files_changed: 1,
            bytes_added: 100,
            bytes_removed: 0,
        };
        Checkpoint::new(parent, root_tree, CheckpointReason::Manual, vec![], meta)
    }

    #[test]
    fn test_head_alias_resolves_to_latest() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path().to_path_buf();
        let journal_path = tl_dir.join("journal");
        std::fs::create_dir_all(&journal_path).unwrap();

        // Create checkpoints and store IDs before dropping journal
        let cp3_id;
        {
            let journal = Journal::open(&journal_path).unwrap();

            // Create 3 checkpoints
            let cp1 = create_test_checkpoint(None);
            let cp2 = create_test_checkpoint(Some(cp1.id));
            let cp3 = create_test_checkpoint(Some(cp2.id));
            cp3_id = cp3.id;

            journal.append(&cp1).unwrap();
            journal.append(&cp2).unwrap();
            journal.append(&cp3).unwrap();
        } // journal lock released here

        // Test HEAD resolves to latest (cp3)
        let results = resolve_via_journal(&["HEAD".to_string()], &tl_dir).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], Some(cp3_id), "HEAD should resolve to latest checkpoint");
    }

    #[test]
    fn test_head_alias_empty_journal() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path().to_path_buf();
        let journal_path = tl_dir.join("journal");
        std::fs::create_dir_all(&journal_path).unwrap();

        {
            let _journal = Journal::open(&journal_path).unwrap();
        } // journal lock released here

        // Test HEAD on empty journal returns None
        let results = resolve_via_journal(&["HEAD".to_string()], &tl_dir).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], None, "HEAD should return None for empty journal");
    }

    #[test]
    fn test_head_case_sensitive() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path().to_path_buf();
        let journal_path = tl_dir.join("journal");
        std::fs::create_dir_all(&journal_path).unwrap();

        let cp1_id;
        {
            let journal = Journal::open(&journal_path).unwrap();

            let cp1 = create_test_checkpoint(None);
            cp1_id = cp1.id;
            journal.append(&cp1).unwrap();
        } // journal lock released here

        // Test that only uppercase HEAD works (lowercase should not match)
        let results = resolve_via_journal(&["HEAD".to_string()], &tl_dir).unwrap();
        assert_eq!(results[0], Some(cp1_id), "HEAD should work");

        // lowercase "head" should not resolve (it's not a valid ULID or pin)
        let results = resolve_via_journal(&["head".to_string()], &tl_dir).unwrap();
        assert_eq!(results[0], None, "lowercase 'head' should not resolve");
    }

    #[test]
    fn test_mixed_refs_with_head() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path().to_path_buf();
        let journal_path = tl_dir.join("journal");
        std::fs::create_dir_all(&journal_path).unwrap();

        let (cp1_id, cp2_id);
        {
            let journal = Journal::open(&journal_path).unwrap();

            let cp1 = create_test_checkpoint(None);
            let cp2 = create_test_checkpoint(Some(cp1.id));
            cp1_id = cp1.id;
            cp2_id = cp2.id;
            journal.append(&cp1).unwrap();
            journal.append(&cp2).unwrap();
        } // journal lock released here

        // Test resolving multiple refs including HEAD
        let refs = vec![
            "HEAD".to_string(),
            cp1_id.to_string(),
            "nonexistent".to_string(),
        ];
        let results = resolve_via_journal(&refs, &tl_dir).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], Some(cp2_id), "HEAD should resolve to latest (cp2)");
        assert_eq!(results[1], Some(cp1_id), "Full ULID should resolve");
        assert_eq!(results[2], None, "nonexistent should return None");
    }
}
