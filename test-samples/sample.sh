#!/bin/bash
# Bash sample file for tree-sitter syntax highlighting test
# Phase 1 language support

set -euo pipefail

# Variables and string interpolation
PROJECT_NAME="octorus"
VERSION="0.2.8"
BUILD_DIR="${PWD}/target/release"

# Function definition
log_info() {
    local message="$1"
    echo "[INFO] $(date '+%Y-%m-%d %H:%M:%S') - ${message}"
}

log_error() {
    local message="$1"
    echo "[ERROR] $(date '+%Y-%m-%d %H:%M:%S') - ${message}" >&2
}

# Array operations
declare -a LANGUAGES=("lua" "bash" "php" "swift" "haskell" "svelte")

# Loop constructs
for lang in "${LANGUAGES[@]}"; do
    log_info "Testing language: ${lang}"
done

# Conditional statements
check_dependencies() {
    if command -v cargo &> /dev/null; then
        log_info "Cargo is installed"
        return 0
    else
        log_error "Cargo is not installed"
        return 1
    fi
}

# Case statement
handle_action() {
    local action="$1"

    case "${action}" in
        build)
            cargo build --release
            ;;
        test)
            cargo test
            ;;
        clean)
            cargo clean
            ;;
        *)
            log_error "Unknown action: ${action}"
            exit 1
            ;;
    esac
}

# Here document
create_config() {
    cat <<EOF > config.toml
[package]
name = "${PROJECT_NAME}"
version = "${VERSION}"

[dependencies]
tree-sitter = "0.26.3"
EOF
}

# Process substitution and pipes
count_rust_files() {
    find . -name "*.rs" -type f | wc -l
}

# Main execution
main() {
    log_info "Starting ${PROJECT_NAME} v${VERSION}"

    if check_dependencies; then
        handle_action "${1:-build}"
    fi

    local rust_count
    rust_count=$(count_rust_files)
    log_info "Found ${rust_count} Rust files"
}

# Run main with all arguments
main "$@"
