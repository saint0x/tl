#!/bin/bash
#
# Timelapse (tl) Test Runner
# Comprehensive testing script for development and CI
#

set -e

# =============================================================================
# Environment Configuration
# =============================================================================

# OpenSSL and libssh2 paths (required for jj-lib compilation on macOS)
export OPENSSL_DIR="/opt/homebrew/opt/openssl@3"
export PKG_CONFIG_PATH="/opt/homebrew/opt/openssl@3/lib/pkgconfig:/opt/homebrew/opt/libssh2/lib/pkgconfig"
export RUSTFLAGS="-L /opt/homebrew/opt/openssl@3/lib -L /opt/homebrew/opt/libssh2/lib"

# Optional: Increase backtrace verbosity for debugging
# export RUST_BACKTRACE=1

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# =============================================================================
# Helper Functions
# =============================================================================

print_header() {
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}  $1${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}! $1${NC}"
}

print_info() {
    echo -e "${CYAN}→ $1${NC}"
}

# Timer functions
start_timer() {
    START_TIME=$(date +%s)
}

end_timer() {
    END_TIME=$(date +%s)
    ELAPSED=$((END_TIME - START_TIME))
    echo ""
    print_info "Completed in ${ELAPSED}s"
}

# =============================================================================
# Test Commands
# =============================================================================

cmd_check() {
    print_header "Running cargo check (fast compilation check)"
    start_timer
    cargo check --all-targets
    end_timer
    print_success "Check passed"
}

cmd_build() {
    print_header "Building all targets"
    start_timer
    cargo build
    end_timer
    print_success "Build completed"
}

cmd_build_release() {
    print_header "Building release binary"
    start_timer
    cargo build --release
    end_timer
    print_success "Release build completed"
    echo ""
    print_info "Binary at: target/aarch64-apple-darwin/release/tl"
}

cmd_test_unit() {
    print_header "Running unit tests"
    start_timer
    cargo test --lib
    end_timer
    print_success "Unit tests passed"
}

cmd_test_core() {
    print_header "Running core crate tests"
    start_timer
    cargo test -p core
    end_timer
    print_success "Core tests passed"
}

cmd_test_journal() {
    print_header "Running journal crate tests"
    start_timer
    cargo test -p journal
    end_timer
    print_success "Journal tests passed"
}

cmd_test_watcher() {
    print_header "Running watcher crate tests"
    start_timer
    cargo test -p watcher
    end_timer
    print_success "Watcher tests passed"
}

cmd_test_jj() {
    print_header "Running JJ integration crate tests"
    start_timer
    cargo test -p jj
    end_timer
    print_success "JJ tests passed"
}

cmd_test_cli() {
    print_header "Running CLI tests"
    start_timer
    cargo test -p cli
    end_timer
    print_success "CLI tests passed"
}

cmd_test_integration() {
    print_header "Running integration tests"
    start_timer
    cargo test -p cli --test integration_test
    end_timer
    print_success "Integration tests passed"
}

cmd_test_workflows() {
    print_header "Running workflow integration tests"
    print_warning "This may take 2+ minutes..."
    start_timer
    cargo test -p cli --test workflows_integration
    end_timer
    print_success "Workflow tests passed"
}

cmd_test_publish() {
    print_header "Running publish/pull tests"
    start_timer
    cargo test -p cli --test workflows_integration publish
    end_timer
    print_success "Publish tests passed"
}

cmd_test_restore() {
    print_header "Running restore/rewind tests"
    start_timer
    cargo test -p cli --test workflows_integration restore_rewind
    end_timer
    print_success "Restore tests passed"
}

cmd_test_gc() {
    print_header "Running GC and pin tests"
    start_timer
    cargo test -p cli --test workflows_integration pin_unpin_gc
    end_timer
    print_success "GC tests passed"
}

cmd_test_edge() {
    print_header "Running edge case tests"
    start_timer
    cargo test -p cli --test workflows_integration edge_cases
    end_timer
    print_success "Edge case tests passed"
}

cmd_test_all() {
    print_header "Running ALL tests"
    print_warning "This will take several minutes..."
    start_timer
    cargo test
    end_timer
    print_success "All tests passed"
}

cmd_test_quick() {
    print_header "Running quick test suite (unit + basic integration)"
    start_timer

    print_info "Unit tests..."
    cargo test --lib --quiet

    print_info "Core tests..."
    cargo test -p core --quiet

    print_info "Journal tests..."
    cargo test -p journal --quiet

    print_info "CLI basic tests..."
    cargo test -p cli --test integration_test --quiet

    end_timer
    print_success "Quick tests passed"
}

cmd_test_specific() {
    if [ -z "$1" ]; then
        print_error "Please provide a test name pattern"
        echo "Usage: ./test.sh test <pattern>"
        echo "Example: ./test.sh test test_publish_head"
        exit 1
    fi

    print_header "Running tests matching: $1"
    start_timer
    cargo test "$1"
    end_timer
}

cmd_clean() {
    print_header "Cleaning build artifacts"
    cargo clean
    print_success "Clean completed"
}

cmd_lint() {
    print_header "Running clippy lints"
    start_timer
    cargo clippy --all-targets -- -D warnings
    end_timer
    print_success "Lint passed"
}

cmd_fmt() {
    print_header "Checking code formatting"
    cargo fmt --check
    print_success "Format check passed"
}

cmd_fmt_fix() {
    print_header "Fixing code formatting"
    cargo fmt
    print_success "Format fixed"
}

cmd_doc() {
    print_header "Building documentation"
    start_timer
    cargo doc --no-deps
    end_timer
    print_success "Docs built at: target/doc/"
}

cmd_ci() {
    print_header "Running full CI pipeline"
    print_warning "This runs: check, fmt, lint, test-all"
    start_timer

    echo ""
    print_info "Step 1/4: Check..."
    cargo check --all-targets --quiet
    print_success "Check passed"

    echo ""
    print_info "Step 2/4: Format..."
    cargo fmt --check
    print_success "Format passed"

    echo ""
    print_info "Step 3/4: Lint..."
    cargo clippy --all-targets --quiet -- -D warnings
    print_success "Lint passed"

    echo ""
    print_info "Step 4/4: Tests..."
    cargo test --quiet
    print_success "Tests passed"

    end_timer
    echo ""
    print_success "CI pipeline completed successfully!"
}

cmd_bench() {
    print_header "Running benchmark tests"
    start_timer
    cargo test -p cli --test workflows_integration -- bench_ --ignored
    end_timer
}

cmd_install() {
    print_header "Installing tl to ~/.cargo/bin"
    start_timer
    cargo install --path crates/cli
    end_timer
    print_success "Installed! Run 'tl --version' to verify"
}

# =============================================================================
# Usage Information
# =============================================================================

show_usage() {
    echo ""
    echo -e "${CYAN}Timelapse Test Runner${NC}"
    echo ""
    echo "Usage: ./test.sh <command> [args]"
    echo ""
    echo -e "${YELLOW}Build Commands:${NC}"
    echo "  check           Fast compilation check (no codegen)"
    echo "  build           Build debug binary"
    echo "  build-release   Build optimized release binary"
    echo "  clean           Remove build artifacts"
    echo "  install         Install tl to ~/.cargo/bin"
    echo ""
    echo -e "${YELLOW}Test Commands:${NC}"
    echo "  test-all        Run ALL tests (slow, ~2 min)"
    echo "  test-quick      Run quick test suite (fast, ~10s)"
    echo "  test <pattern>  Run tests matching pattern"
    echo ""
    echo -e "${YELLOW}Crate-Specific Tests:${NC}"
    echo "  test-unit       Run unit tests only"
    echo "  test-core       Run core crate tests"
    echo "  test-journal    Run journal crate tests"
    echo "  test-watcher    Run watcher crate tests"
    echo "  test-jj         Run JJ integration tests"
    echo "  test-cli        Run CLI tests"
    echo ""
    echo -e "${YELLOW}Integration Tests:${NC}"
    echo "  test-integration  Basic CLI integration tests"
    echo "  test-workflows    Full workflow tests (slow)"
    echo "  test-publish      Publish/pull tests"
    echo "  test-restore      Restore/rewind tests"
    echo "  test-gc           GC and pin tests"
    echo "  test-edge         Edge case tests"
    echo ""
    echo -e "${YELLOW}Code Quality:${NC}"
    echo "  lint            Run clippy lints"
    echo "  fmt             Check code formatting"
    echo "  fmt-fix         Fix code formatting"
    echo "  doc             Build documentation"
    echo "  ci              Run full CI pipeline"
    echo ""
    echo -e "${YELLOW}Benchmarks:${NC}"
    echo "  bench           Run benchmark tests"
    echo ""
    echo -e "${YELLOW}Environment:${NC}"
    echo "  OPENSSL_DIR     = $OPENSSL_DIR"
    echo "  PKG_CONFIG_PATH = $PKG_CONFIG_PATH"
    echo ""
    echo -e "${CYAN}Examples:${NC}"
    echo "  ./test.sh test-quick          # Fast sanity check"
    echo "  ./test.sh test-all            # Full test suite"
    echo "  ./test.sh test test_publish   # Run specific tests"
    echo "  ./test.sh ci                  # Full CI pipeline"
    echo ""
}

# =============================================================================
# Main Entry Point
# =============================================================================

main() {
    # Change to script directory (repo root)
    cd "$(dirname "$0")"

    case "${1:-}" in
        # Build commands
        check)          cmd_check ;;
        build)          cmd_build ;;
        build-release)  cmd_build_release ;;
        clean)          cmd_clean ;;
        install)        cmd_install ;;

        # Test commands
        test-all)       cmd_test_all ;;
        test-quick)     cmd_test_quick ;;
        test)           cmd_test_specific "$2" ;;

        # Crate tests
        test-unit)      cmd_test_unit ;;
        test-core)      cmd_test_core ;;
        test-journal)   cmd_test_journal ;;
        test-watcher)   cmd_test_watcher ;;
        test-jj)        cmd_test_jj ;;
        test-cli)       cmd_test_cli ;;

        # Integration tests
        test-integration) cmd_test_integration ;;
        test-workflows)   cmd_test_workflows ;;
        test-publish)     cmd_test_publish ;;
        test-restore)     cmd_test_restore ;;
        test-gc)          cmd_test_gc ;;
        test-edge)        cmd_test_edge ;;

        # Code quality
        lint)           cmd_lint ;;
        fmt)            cmd_fmt ;;
        fmt-fix)        cmd_fmt_fix ;;
        doc)            cmd_doc ;;
        ci)             cmd_ci ;;

        # Benchmarks
        bench)          cmd_bench ;;

        # Help
        -h|--help|help|"")
            show_usage
            ;;

        *)
            print_error "Unknown command: $1"
            show_usage
            exit 1
            ;;
    esac
}

main "$@"
