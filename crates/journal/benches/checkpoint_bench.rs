//! Performance benchmarks for checkpoint operations
//!
//! Run with: cargo bench --package journal

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal, PathMap};
use core::{Sha1Hash, Entry, Tree};
use std::path::PathBuf;
use tempfile::TempDir;

fn bench_checkpoint_serialization(c: &mut Criterion) {
    let root_tree = Sha1Hash::from_bytes([1u8; 20]);
    let meta = CheckpointMeta {
        files_changed: 10,
        bytes_added: 1024,
        bytes_removed: 512,
    };

    let checkpoint = Checkpoint::new(
        None,
        root_tree,
        CheckpointReason::FsBatch,
        vec![PathBuf::from("test/file.txt")],
        meta,
    );

    c.bench_function("checkpoint_serialize", |b| {
        b.iter(|| {
            let bytes = checkpoint.serialize().unwrap();
            black_box(bytes);
        });
    });

    let serialized = checkpoint.serialize().unwrap();
    c.bench_function("checkpoint_deserialize", |b| {
        b.iter(|| {
            let cp = Checkpoint::deserialize(&serialized).unwrap();
            black_box(cp);
        });
    });
}

fn bench_journal_append(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let journal = Journal::open(temp_dir.path()).unwrap();

    let root_tree = Sha1Hash::from_bytes([2u8; 20]);
    let meta = CheckpointMeta {
        files_changed: 1,
        bytes_added: 100,
        bytes_removed: 0,
    };

    c.bench_function("journal_append", |b| {
        b.iter(|| {
            let checkpoint = Checkpoint::new(
                None,
                root_tree,
                CheckpointReason::FsBatch,
                vec![],
                meta.clone(),
            );
            let seq = journal.append(&checkpoint).unwrap();
            black_box(seq);
        });
    });
}

fn bench_journal_queries(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let journal = Journal::open(temp_dir.path()).unwrap();

    // Populate journal with checkpoints
    let mut checkpoint_ids = Vec::new();
    for i in 0..1000 {
        let root_tree = Sha1Hash::from_bytes([i as u8; 20]);
        let meta = CheckpointMeta {
            files_changed: 1,
            bytes_added: 100,
            bytes_removed: 0,
        };
        let checkpoint = Checkpoint::new(None, root_tree, CheckpointReason::FsBatch, vec![], meta);
        journal.append(&checkpoint).unwrap();
        checkpoint_ids.push(checkpoint.id);
    }

    c.bench_function("journal_get_latest", |b| {
        b.iter(|| {
            let latest = journal.latest().unwrap();
            black_box(latest);
        });
    });

    c.bench_function("journal_get_by_id", |b| {
        b.iter(|| {
            let cp = journal.get(&checkpoint_ids[500]).unwrap();
            black_box(cp);
        });
    });

    c.bench_function("journal_last_n_100", |b| {
        b.iter(|| {
            let last = journal.last_n(100).unwrap();
            black_box(last);
        });
    });
}

fn bench_pathmap_operations(c: &mut Criterion) {
    let root_tree = Sha1Hash::from_bytes([3u8; 20]);
    let mut pathmap = PathMap::new(root_tree);

    // Populate with entries
    for i in 0..100 {
        let path = PathBuf::from(format!("file{}.txt", i));
        let hash = Sha1Hash::from_bytes([(i % 256) as u8; 20]);
        let entry = Entry::file(0o644, hash);
        pathmap.update(&path, Some(entry));
    }

    let temp_dir = TempDir::new().unwrap();
    let save_path = temp_dir.path().join("pathmap.bin");

    c.bench_function("pathmap_save", |b| {
        b.iter(|| {
            pathmap.save(&save_path).unwrap();
            black_box(&pathmap);
        });
    });

    pathmap.save(&save_path).unwrap();
    c.bench_function("pathmap_load", |b| {
        b.iter(|| {
            let loaded = PathMap::load(&save_path).unwrap();
            black_box(loaded);
        });
    });
}

fn bench_tree_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_sizes");

    for size in [1, 10, 100, 1000] {
        let mut tree = Tree::new();

        for i in 0..size {
            let path = PathBuf::from(format!("file{}.txt", i));
            let hash = Sha1Hash::from_bytes([(i % 256) as u8; 20]);
            let entry = Entry::file(0o644, hash);
            tree.insert(&path, entry);
        }

        group.bench_with_input(BenchmarkId::new("serialize", size), &tree, |b, tree| {
            b.iter(|| {
                let bytes = tree.serialize();
                black_box(bytes);
            });
        });

        group.bench_with_input(BenchmarkId::new("hash", size), &tree, |b, tree| {
            b.iter(|| {
                let hash = tree.hash();
                black_box(hash);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_checkpoint_serialization,
    bench_journal_append,
    bench_journal_queries,
    bench_pathmap_operations,
    bench_tree_operations
);
criterion_main!(benches);
