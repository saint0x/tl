//! Test project generators for integration tests
//!
//! This module provides utilities for generating realistic test projects
//! of various sizes and structures. Projects are generated at runtime
//! with deterministic content for reproducible tests.

use anyhow::{Context, Result};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Test project sizes for different test scenarios
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSize {
    /// 5 files, 1 directory, ~5KB (quick sanity checks)
    Tiny,
    /// 10 files, 3 directories, ~50KB (unit tests)
    Small,
    /// 100 files, 10 directories, ~500KB (realistic development)
    Medium,
    /// 1000 files, 50 directories, ~5MB (performance testing)
    Large,
}

impl ProjectSize {
    /// Get file count for this size
    pub fn file_count(&self) -> usize {
        match self {
            ProjectSize::Tiny => 5,
            ProjectSize::Small => 10,
            ProjectSize::Medium => 100,
            ProjectSize::Large => 1000,
        }
    }

    /// Get directory count for this size
    pub fn dir_count(&self) -> usize {
        match self {
            ProjectSize::Tiny => 1,
            ProjectSize::Small => 3,
            ProjectSize::Medium => 10,
            ProjectSize::Large => 50,
        }
    }

    /// Get average file size in bytes
    pub fn avg_file_size(&self) -> usize {
        match self {
            ProjectSize::Tiny => 1024,      // 1KB
            ProjectSize::Small => 5120,     // 5KB
            ProjectSize::Medium => 5120,    // 5KB
            ProjectSize::Large => 5120,     // 5KB
        }
    }

    /// Get max nesting depth
    pub fn max_depth(&self) -> usize {
        match self {
            ProjectSize::Tiny => 1,
            ProjectSize::Small => 2,
            ProjectSize::Medium => 3,
            ProjectSize::Large => 5,
        }
    }
}

/// File type distribution for project generation
#[derive(Debug, Clone)]
pub struct FileTypeDistribution {
    /// Ratio of code files (.rs, .py, .js, .go)
    pub code_ratio: f32,
    /// Ratio of text files (.txt, .md, .json)
    pub text_ratio: f32,
    /// Ratio of config files (.toml, .yaml, .env)
    pub config_ratio: f32,
    /// Ratio of binary files (.png, .jpg, .bin)
    pub binary_ratio: f32,
}

impl Default for FileTypeDistribution {
    fn default() -> Self {
        Self {
            code_ratio: 0.5,    // 50% code
            text_ratio: 0.3,    // 30% text
            config_ratio: 0.1,  // 10% config
            binary_ratio: 0.1,  // 10% binary
        }
    }
}

/// Project template configuration
#[derive(Debug, Clone)]
pub struct ProjectTemplate {
    pub size: ProjectSize,
    pub file_types: FileTypeDistribution,
    pub nested_depth: usize,
    pub include_binary: bool,
    pub include_symlinks: bool,
    pub include_large_files: bool,
    pub gitignore_patterns: Vec<String>,
    pub seed: u64,
}

impl ProjectTemplate {
    /// Create a Rust project template
    pub fn rust_project(size: ProjectSize) -> Self {
        Self {
            size,
            file_types: FileTypeDistribution {
                code_ratio: 0.7,    // More code files
                text_ratio: 0.2,
                config_ratio: 0.08,
                binary_ratio: 0.02,
            },
            nested_depth: size.max_depth(),
            include_binary: false,
            include_symlinks: false,
            include_large_files: false,
            gitignore_patterns: vec![
                "target/".to_string(),
                "Cargo.lock".to_string(),
            ],
            seed: 12345,
        }
    }

    /// Create a Python project template
    pub fn python_project(size: ProjectSize) -> Self {
        Self {
            size,
            file_types: FileTypeDistribution {
                code_ratio: 0.6,
                text_ratio: 0.25,
                config_ratio: 0.1,
                binary_ratio: 0.05,
            },
            nested_depth: size.max_depth(),
            include_binary: false,
            include_symlinks: false,
            include_large_files: false,
            gitignore_patterns: vec![
                "__pycache__/".to_string(),
                "*.pyc".to_string(),
                ".venv/".to_string(),
            ],
            seed: 12345,
        }
    }

    /// Create a mixed/polyglot project template
    pub fn mixed_project(size: ProjectSize) -> Self {
        Self {
            size,
            file_types: FileTypeDistribution::default(),
            nested_depth: size.max_depth(),
            include_binary: true,
            include_symlinks: false,
            include_large_files: false,
            gitignore_patterns: vec![
                "node_modules/".to_string(),
                "build/".to_string(),
                "dist/".to_string(),
            ],
            seed: 12345,
        }
    }

    /// Create edge cases template (large files, deep nesting, symlinks)
    pub fn edge_cases() -> Self {
        Self {
            size: ProjectSize::Medium,
            file_types: FileTypeDistribution::default(),
            nested_depth: 10,  // Deep nesting
            include_binary: true,
            include_symlinks: true,
            include_large_files: true,
            gitignore_patterns: vec![],
            seed: 12345,
        }
    }
}

/// File information
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: usize,
    pub extension: String,
}

/// Test project with automatic cleanup
pub struct TestProject {
    root: PathBuf,
    template: ProjectTemplate,
    pub file_manifest: Vec<FileInfo>,
    temp_dir: TempDir,
}

impl TestProject {
    /// Create a new test project from template
    pub fn new(template: ProjectTemplate) -> Result<Self> {
        let temp_dir = TempDir::new()
            .context("Failed to create temp directory")?;
        let root = temp_dir.path().to_path_buf();

        let file_manifest = generate_project(&root, &template)?;

        Ok(Self {
            root,
            template,
            file_manifest,
            temp_dir,
        })
    }

    /// Get project root path
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get number of files
    pub fn file_count(&self) -> usize {
        self.file_manifest.len()
    }

    /// Get total size in bytes
    pub fn total_size(&self) -> u64 {
        self.file_manifest.iter()
            .map(|f| f.size as u64)
            .sum()
    }

    /// Modify existing files with new content
    pub fn modify_files(&mut self, paths: &[&str], content: &str) -> Result<()> {
        for path in paths {
            let full_path = self.root.join(path);
            fs::write(&full_path, content)
                .with_context(|| format!("Failed to modify file: {}", path))?;
        }
        Ok(())
    }

    /// Add new files to the project
    pub fn add_files(&mut self, count: usize) -> Result<Vec<PathBuf>> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.template.seed);
        let mut new_files = Vec::new();

        for i in 0..count {
            let filename = format!("added_file_{}.rs", i);
            let file_path = self.root.join(&filename);
            let content = generate_rust_code(1024, &mut rng);

            fs::write(&file_path, content)?;

            new_files.push(file_path.clone());
            self.file_manifest.push(FileInfo {
                path: file_path,
                size: 1024,
                extension: "rs".to_string(),
            });
        }

        Ok(new_files)
    }

    /// Delete files from the project
    pub fn delete_files(&mut self, paths: &[&str]) -> Result<()> {
        for path in paths {
            let full_path = self.root.join(path);
            if full_path.exists() {
                fs::remove_file(&full_path)
                    .with_context(|| format!("Failed to delete file: {}", path))?;

                // Remove from manifest
                self.file_manifest.retain(|f| f.path != full_path);
            }
        }
        Ok(())
    }
}

/// Generate a project from template
fn generate_project(root: &Path, template: &ProjectTemplate) -> Result<Vec<FileInfo>> {
    let mut rng = ChaCha8Rng::seed_from_u64(template.seed);
    let mut file_manifest = Vec::new();

    // Create directory structure
    let directories = generate_directories(root, template, &mut rng)?;

    // Calculate files per type
    let total_files = template.size.file_count();
    let code_files = (total_files as f32 * template.file_types.code_ratio) as usize;
    let text_files = (total_files as f32 * template.file_types.text_ratio) as usize;
    let config_files = (total_files as f32 * template.file_types.config_ratio) as usize;
    let binary_files = if template.include_binary {
        total_files - code_files - text_files - config_files
    } else {
        0
    };
    let remaining = total_files - code_files - text_files - config_files - binary_files;

    // Generate files
    file_manifest.extend(generate_code_files(root, &directories, code_files, &mut rng)?);
    file_manifest.extend(generate_text_files(root, &directories, text_files, &mut rng)?);
    file_manifest.extend(generate_config_files(root, &directories, config_files, &mut rng)?);
    if template.include_binary {
        file_manifest.extend(generate_binary_files(root, &directories, binary_files, &mut rng)?);
    }

    // Fill remaining with code files
    if remaining > 0 {
        file_manifest.extend(generate_code_files(root, &directories, remaining, &mut rng)?);
    }

    // Create .gitignore if patterns specified
    if !template.gitignore_patterns.is_empty() {
        let gitignore_path = root.join(".gitignore");
        fs::write(&gitignore_path, template.gitignore_patterns.join("\n"))?;
    }

    Ok(file_manifest)
}

/// Generate directory structure
fn generate_directories(root: &Path, template: &ProjectTemplate, rng: &mut ChaCha8Rng) -> Result<Vec<PathBuf>> {
    let mut directories = vec![root.to_path_buf()];

    let dir_names = vec!["src", "tests", "benches", "examples", "docs", "data", "scripts", "lib"];

    for i in 0..template.size.dir_count() {
        let depth = rng.gen_range(0..=template.nested_depth);
        let dir_name = if i < dir_names.len() {
            dir_names[i]
        } else {
            &format!("dir_{}", i)
        };

        let parent = if depth == 0 || directories.len() == 1 {
            root
        } else {
            &directories[rng.gen_range(1..directories.len())]
        };

        let dir_path = parent.join(dir_name);
        fs::create_dir_all(&dir_path)?;
        directories.push(dir_path);
    }

    Ok(directories)
}

/// Generate code files
fn generate_code_files(root: &Path, directories: &[PathBuf], count: usize, rng: &mut ChaCha8Rng) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();
    let extensions = vec!["rs", "py", "js", "go"];

    for i in 0..count {
        let ext = extensions[rng.gen_range(0..extensions.len())];
        let dir = &directories[rng.gen_range(0..directories.len())];
        let file_path = dir.join(format!("file_{}.{}", i, ext));

        let size = rng.gen_range(512..4096);
        let content = match ext {
            "rs" => generate_rust_code(size, rng),
            "py" => generate_python_code(size, rng),
            "js" => generate_javascript_code(size, rng),
            "go" => generate_go_code(size, rng),
            _ => vec![b'x'; size],
        };

        fs::write(&file_path, content)?;

        files.push(FileInfo {
            path: file_path,
            size,
            extension: ext.to_string(),
        });
    }

    Ok(files)
}

/// Generate text files
fn generate_text_files(root: &Path, directories: &[PathBuf], count: usize, rng: &mut ChaCha8Rng) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();
    let extensions = vec!["md", "txt", "json"];

    for i in 0..count {
        let ext = extensions[rng.gen_range(0..extensions.len())];
        let dir = &directories[rng.gen_range(0..directories.len())];
        let file_path = dir.join(format!("doc_{}.{}", i, ext));

        let size = rng.gen_range(512..2048);
        let content = match ext {
            "md" => generate_markdown(size, rng),
            "json" => generate_json(size, rng),
            _ => generate_lorem_ipsum(size, rng),
        };

        fs::write(&file_path, content)?;

        files.push(FileInfo {
            path: file_path,
            size,
            extension: ext.to_string(),
        });
    }

    Ok(files)
}

/// Generate config files
fn generate_config_files(root: &Path, directories: &[PathBuf], count: usize, rng: &mut ChaCha8Rng) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();
    let extensions = vec!["toml", "yaml", "env"];

    for i in 0..count {
        let ext = extensions[rng.gen_range(0..extensions.len())];
        let dir = &directories[rng.gen_range(0..directories.len())];
        let file_path = dir.join(format!("config_{}.{}", i, ext));

        let size = rng.gen_range(256..1024);
        let content = generate_config(ext, size, rng);

        fs::write(&file_path, content)?;

        files.push(FileInfo {
            path: file_path,
            size,
            extension: ext.to_string(),
        });
    }

    Ok(files)
}

/// Generate binary files
fn generate_binary_files(root: &Path, directories: &[PathBuf], count: usize, rng: &mut ChaCha8Rng) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();

    for i in 0..count {
        let dir = &directories[rng.gen_range(0..directories.len())];
        let file_path = dir.join(format!("binary_{}.bin", i));

        let size = rng.gen_range(1024..10240);
        let content: Vec<u8> = (0..size).map(|_| rng.gen()).collect();

        fs::write(&file_path, content)?;

        files.push(FileInfo {
            path: file_path,
            size,
            extension: "bin".to_string(),
        });
    }

    Ok(files)
}

// Content generators

fn generate_rust_code(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let templates = vec![
        "fn example() -> i32 { 42 }\n",
        "pub struct Data { field: String }\n",
        "impl Default for Data { fn default() -> Self { Self { field: String::new() } } }\n",
        "// Comment line\n",
        "use std::collections::HashMap;\n",
    ];

    let mut content = String::from("// Generated Rust file\n\n");
    while content.len() < target_size {
        let template = templates[rng.gen_range(0..templates.len())];
        content.push_str(template);
    }

    content.into_bytes()
}

fn generate_python_code(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let templates = vec![
        "def example():\n    return 42\n\n",
        "class Data:\n    def __init__(self):\n        self.field = ''\n\n",
        "# Comment line\n",
        "import os\n",
    ];

    let mut content = String::from("# Generated Python file\n\n");
    while content.len() < target_size {
        let template = templates[rng.gen_range(0..templates.len())];
        content.push_str(template);
    }

    content.into_bytes()
}

fn generate_javascript_code(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let templates = vec![
        "function example() { return 42; }\n",
        "const data = { field: '' };\n",
        "// Comment line\n",
        "import * as fs from 'fs';\n",
    ];

    let mut content = String::from("// Generated JavaScript file\n\n");
    while content.len() < target_size {
        let template = templates[rng.gen_range(0..templates.len())];
        content.push_str(template);
    }

    content.into_bytes()
}

fn generate_go_code(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let templates = vec![
        "func example() int { return 42 }\n",
        "type Data struct { Field string }\n",
        "// Comment line\n",
        "import \"fmt\"\n",
    ];

    let mut content = String::from("// Generated Go file\n\n");
    while content.len() < target_size {
        let template = templates[rng.gen_range(0..templates.len())];
        content.push_str(template);
    }

    content.into_bytes()
}

fn generate_markdown(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let paragraphs = vec![
        "# Heading\n\n",
        "This is a paragraph of text.\n\n",
        "## Subheading\n\n",
        "- List item\n",
        "- Another item\n\n",
    ];

    let mut content = String::from("# Documentation\n\n");
    while content.len() < target_size {
        let para = paragraphs[rng.gen_range(0..paragraphs.len())];
        content.push_str(para);
    }

    content.into_bytes()
}

fn generate_json(target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    let mut content = String::from("{\n  \"data\": [\n");

    let item_count = target_size / 50;
    for i in 0..item_count {
        content.push_str(&format!("    {{\"id\": {}, \"value\": \"item_{}\"}}", i, i));
        if i < item_count - 1 {
            content.push_str(",\n");
        } else {
            content.push('\n');
        }
    }

    content.push_str("  ]\n}\n");
    content.into_bytes()
}

fn generate_lorem_ipsum(target_size: usize, _rng: &mut ChaCha8Rng) -> Vec<u8> {
    let lorem = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
    let mut content = String::new();

    while content.len() < target_size {
        content.push_str(lorem);
    }

    content.into_bytes()
}

fn generate_config(extension: &str, target_size: usize, rng: &mut ChaCha8Rng) -> Vec<u8> {
    match extension {
        "toml" => {
            let mut content = String::from("[settings]\n");
            while content.len() < target_size {
                let key = format!("key_{}", rng.gen_range(0..100));
                let value = format!("\"value_{}\"", rng.gen_range(0..100));
                content.push_str(&format!("{} = {}\n", key, value));
            }
            content.into_bytes()
        },
        "yaml" => {
            let mut content = String::from("settings:\n");
            while content.len() < target_size {
                let key = format!("  key_{}", rng.gen_range(0..100));
                let value = format!("value_{}", rng.gen_range(0..100));
                content.push_str(&format!("{}: {}\n", key, value));
            }
            content.into_bytes()
        },
        _ => {
            let mut content = String::new();
            while content.len() < target_size {
                let key = format!("KEY_{}", rng.gen_range(0..100));
                let value = format!("value_{}", rng.gen_range(0..100));
                content.push_str(&format!("{}={}\n", key, value));
            }
            content.into_bytes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tiny_project_generation() -> Result<()> {
        let template = ProjectTemplate::rust_project(ProjectSize::Tiny);
        let project = TestProject::new(template)?;

        assert!(project.file_count() > 0);
        assert!(project.file_count() <= 10);  // Should be around 5 but allow some variance
        assert!(project.root().exists());

        Ok(())
    }

    #[test]
    fn test_small_project_generation() -> Result<()> {
        let template = ProjectTemplate::rust_project(ProjectSize::Small);
        let project = TestProject::new(template)?;

        assert!(project.file_count() >= 5);
        assert!(project.file_count() <= 15);  // Should be around 10

        Ok(())
    }

    #[test]
    fn test_modify_files() -> Result<()> {
        let template = ProjectTemplate::rust_project(ProjectSize::Tiny);
        let mut project = TestProject::new(template)?;

        // Add a test file
        let test_file = project.root().join("test.txt");
        fs::write(&test_file, "original content")?;

        // Modify it
        project.modify_files(&["test.txt"], "modified content")?;

        // Verify
        let content = fs::read_to_string(&test_file)?;
        assert_eq!(content, "modified content");

        Ok(())
    }

    #[test]
    fn test_generation_produces_valid_project() -> Result<()> {
        // Test that project generation works correctly (timing benchmarks are in benches/)
        let template = ProjectTemplate::rust_project(ProjectSize::Medium);
        let project = TestProject::new(template)?;

        // Verify the project was actually created with expected structure
        assert!(project.root().exists(), "Project root should exist");

        // For Medium size, we should have a reasonable number of files
        let file_count = project.file_count();
        assert!(file_count > 0, "Project should have files");
        assert!(file_count >= 50, "Medium project should have at least 50 files, got {}", file_count);

        // Verify we have some directories (src, tests, etc. are generated)
        let entries: Vec<_> = std::fs::read_dir(project.root())?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        assert!(!entries.is_empty(), "Project should have subdirectories");

        // Verify total size is reasonable
        let total_size = project.total_size();
        assert!(total_size > 0, "Project should have non-zero size");

        // Verify file manifest is populated
        assert!(!project.file_manifest.is_empty(), "File manifest should not be empty");

        Ok(())
    }
}
