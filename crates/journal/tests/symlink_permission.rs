//! Tests for symlink and permission change detection
//!
//! Verifies that timelapse correctly detects:
//! - Symlink target changes
//! - Permission-only changes (Unix)
//! - Mode preservation across checkpoints

use anyhow::Result;
use journal::PathMap;
use journal::incremental::incremental_update;
use core::{EntryKind, Store, hash::hash_bytes};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

/// Helper to create a test store and repository
fn setup_test_repo() -> Result<(TempDir, Store)> {
    let temp_dir = TempDir::new()?;
    let repo_root = temp_dir.path();

    // Initialize timelapse repository
    let store = Store::init(repo_root)?;

    Ok((temp_dir, store))
}

#[test]
#[cfg(unix)]
fn test_symlink_target_change_detection() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create two target files
    let target1 = repo_root.join("target1.txt");
    let target2 = repo_root.join("target2.txt");
    fs::write(&target1, b"target 1 content")?;
    fs::write(&target2, b"target 2 content")?;

    // Create symlink pointing to target1
    let link_path = repo_root.join("link.txt");
    symlink(&target1, &link_path)?;

    // Create initial checkpoint with symlink
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("link.txt")];
    let (map1, _tree1, hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    // Verify symlink was captured
    let entry1 = map1.get(Path::new("link.txt")).expect("Symlink should be in map");
    assert_eq!(entry1.kind, EntryKind::Symlink);

    // Change symlink target to target2
    fs::remove_file(&link_path)?;
    symlink(&target2, &link_path)?;

    // Create second checkpoint
    let dirty_paths = vec![Path::new("link.txt")];
    let (map2, _tree2, hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    // Verify symlink target change was detected
    let entry2 = map2.get(Path::new("link.txt")).expect("Symlink should be in map");
    assert_eq!(entry2.kind, EntryKind::Symlink);
    assert_ne!(entry1.blob_hash, entry2.blob_hash, "Symlink hash should change when target changes");
    assert_ne!(hash1, hash2, "Tree hash should change when symlink target changes");

    Ok(())
}

#[test]
#[cfg(unix)]
fn test_permission_only_change_detection() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create file with initial permissions (644)
    let file_path = repo_root.join("test_file.txt");
    fs::write(&file_path, b"unchanging content")?;
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&file_path, perms)?;

    // Create initial checkpoint
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("test_file.txt")];
    let (map1, _tree1, hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    // Verify initial mode
    let entry1 = map1.get(Path::new("test_file.txt")).expect("File should be in map");
    assert_eq!(entry1.kind, EntryKind::File);
    assert_eq!(entry1.mode & 0o777, 0o644);

    // Change permissions only (content unchanged) - make executable
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&file_path, perms)?;

    // Create second checkpoint
    let dirty_paths = vec![Path::new("test_file.txt")];
    let (map2, _tree2, hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    // Verify permission change was detected
    let entry2 = map2.get(Path::new("test_file.txt")).expect("File should be in map");
    assert_eq!(entry2.kind, EntryKind::ExecutableFile, "File should now be executable");
    assert_eq!(entry2.mode & 0o777, 0o755, "Mode should be updated to 755");
    assert_eq!(entry1.blob_hash, entry2.blob_hash, "Content hash should be unchanged");
    assert_ne!(hash1, hash2, "Tree hash should change when permissions change");

    Ok(())
}

#[test]
#[cfg(unix)]
fn test_no_change_when_content_and_mode_unchanged() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create file
    let file_path = repo_root.join("stable_file.txt");
    fs::write(&file_path, b"stable content")?;
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&file_path, perms)?;

    // Create initial checkpoint
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("stable_file.txt")];
    let (map1, _tree1, hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    // Trigger update again without any changes
    let dirty_paths = vec![Path::new("stable_file.txt")];
    let (map2, _tree2, hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    // Verify no change detected
    let entry1 = map1.get(Path::new("stable_file.txt")).unwrap();
    let entry2 = map2.get(Path::new("stable_file.txt")).unwrap();

    assert_eq!(entry1.blob_hash, entry2.blob_hash);
    assert_eq!(entry1.mode, entry2.mode);
    assert_eq!(hash1, hash2, "Tree hash should be unchanged when nothing changes");

    Ok(())
}

#[test]
#[cfg(unix)]
fn test_symlink_to_regular_file_conversion() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create target file and symlink
    let target = repo_root.join("target.txt");
    fs::write(&target, b"target content")?;

    let path = repo_root.join("convertible.txt");
    symlink(&target, &path)?;

    // Create checkpoint with symlink
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("convertible.txt")];
    let (map1, _tree1, hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    // Verify it's a symlink
    let entry1 = map1.get(Path::new("convertible.txt")).unwrap();
    assert_eq!(entry1.kind, EntryKind::Symlink);

    // Convert to regular file
    fs::remove_file(&path)?;
    fs::write(&path, b"now a regular file")?;

    // Create checkpoint after conversion
    let dirty_paths = vec![Path::new("convertible.txt")];
    let (map2, _tree2, hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    // Verify it's now a regular file
    let entry2 = map2.get(Path::new("convertible.txt")).unwrap();
    assert_eq!(entry2.kind, EntryKind::File);
    assert_ne!(entry1.blob_hash, entry2.blob_hash);
    assert_ne!(hash1, hash2);

    Ok(())
}

#[test]
#[cfg(unix)]
fn test_readonly_to_writable_permission_change() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create readonly file
    let file_path = repo_root.join("readonly.txt");
    fs::write(&file_path, b"read only content")?;
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_mode(0o444); // readonly
    fs::set_permissions(&file_path, perms)?;

    // Create initial checkpoint
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("readonly.txt")];
    let (map1, _tree1, _hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    let entry1 = map1.get(Path::new("readonly.txt")).unwrap();
    // Git format normalizes 0o444 to 0o644 (regular file)
    assert_eq!(entry1.mode & 0o777, 0o644, "Git normalizes readonly to regular file");

    // Make executable (to test actual Git mode change)
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&file_path, perms)?;

    // Create second checkpoint
    let dirty_paths = vec![Path::new("readonly.txt")];
    let (map2, _tree2, _hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    let entry2 = map2.get(Path::new("readonly.txt")).unwrap();
    assert_eq!(entry2.kind, EntryKind::ExecutableFile);
    assert_eq!(entry2.mode & 0o777, 0o755, "Should now be executable");
    assert_eq!(entry1.blob_hash, entry2.blob_hash, "Content unchanged");

    Ok(())
}

#[test]
#[cfg(not(unix))]
fn test_windows_readonly_detection() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    // Create writable file
    let file_path = repo_root.join("test.txt");
    fs::write(&file_path, b"content")?;

    // Create initial checkpoint (writable)
    let base_map = PathMap::new(hash_bytes(b"initial"));
    let dirty_paths = vec![Path::new("test.txt")];
    let (map1, _tree1, _hash1) = incremental_update(&base_map, dirty_paths, repo_root, &store)?;

    let entry1 = map1.get(Path::new("test.txt")).unwrap();
    assert_eq!(entry1.mode, 0o644); // writable

    // Make readonly
    let mut perms = fs::metadata(&file_path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(&file_path, perms)?;

    // Create second checkpoint
    let dirty_paths = vec![Path::new("test.txt")];
    let (map2, _tree2, _hash2) = incremental_update(&map1, dirty_paths, repo_root, &store)?;

    let entry2 = map2.get(Path::new("test.txt")).unwrap();
    assert_eq!(entry2.mode, 0o444); // readonly

    Ok(())
}

#[test]
#[cfg(unix)]
fn test_multiple_permission_changes_sequence() -> Result<()> {
    let (temp_dir, store) = setup_test_repo()?;
    let repo_root = temp_dir.path();

    let file_path = repo_root.join("changing.txt");
    fs::write(&file_path, b"content")?;

    // Test sequence: Git only supports 644 (regular) and 755 (executable)
    // Test: 644 -> 755 -> 644 -> 755
    let test_cases = vec![
        (0o644, 0o644, EntryKind::File),           // Regular file
        (0o755, 0o755, EntryKind::ExecutableFile), // Executable
        (0o644, 0o644, EntryKind::File),           // Back to regular
        (0o777, 0o755, EntryKind::ExecutableFile), // 0o777 normalized to 0o755
    ];
    let mut prev_map = PathMap::new(hash_bytes(b"initial"));

    for (set_mode, expected_git_mode, expected_kind) in test_cases {
        let mut perms = fs::metadata(&file_path)?.permissions();
        perms.set_mode(set_mode);
        fs::set_permissions(&file_path, perms)?;

        let dirty_paths = vec![Path::new("changing.txt")];
        let (new_map, _tree, _hash) = incremental_update(&prev_map, dirty_paths, repo_root, &store)?;

        let entry = new_map.get(Path::new("changing.txt")).unwrap();
        assert_eq!(entry.kind, expected_kind, "Kind should match for mode {:#o}", set_mode);
        assert_eq!(entry.mode & 0o777, expected_git_mode, "Git mode should be {:#o} (set {:#o})", expected_git_mode, set_mode);

        prev_map = new_map;
    }

    Ok(())
}
