#!/usr/bin/env bash
#
# Trading Gateway Cleanup Script
#
# Cleans build artifacts, Docker resources, Python example virtualenvs,
# and temporary files to reclaim disk space and reset the dev environment.
#
# Usage:
#   ./scripts/cleanup.sh              # Interactive (prompts before each step)
#   ./scripts/cleanup.sh --all        # Clean everything without prompts
#   ./scripts/cleanup.sh --dry-run    # Show what would be cleaned
#   ./scripts/cleanup.sh --help       # Show help
#

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

cd "$REPO_ROOT"

ALL_MODE=false
DRY_RUN=false

for arg in "$@"; do
    case $arg in
        --all|-a)
            ALL_MODE=true
            shift
            ;;
        --dry-run|-n)
            DRY_RUN=true
            shift
            ;;
        --help|-h)
            echo "Trading Gateway Cleanup Script"
            echo ""
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --all, -a      Clean everything without prompts (skips aggressive cargo cache)"
            echo "  --dry-run, -n  Show what would be cleaned without doing it"
            echo "  --help, -h     Show this help message"
            echo ""
            echo "What gets cleaned:"
            echo "  1. Rust build artifacts (target/)"
            echo "  2. Docker Compose services and volumes"
            echo "  3. Dangling Docker images, stopped containers, unused volumes"
            echo "  4. Python example virtualenvs (.venv/), __pycache__, *.egg-info"
            echo "  5. Cargo registry cache (interactive only — re-downloads on next build)"
            echo ""
            exit 0
            ;;
        *)
            echo "Unknown option: $arg"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

print_header() {
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}  $1${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

print_step() {
    echo -e "${GREEN}→${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}⚠${NC}  $1"
}

print_success() {
    echo -e "${GREEN}✓${NC}  $1"
}

get_size() {
    local path="$1"
    if [ -e "$path" ]; then
        du -sh "$path" 2>/dev/null | cut -f1
    else
        echo "0"
    fi
}

confirm() {
    if [ "$ALL_MODE" = true ]; then
        return 0
    fi

    local prompt="$1"
    read -p "$prompt [y/N] " -n 1 -r
    echo
    [[ $REPLY =~ ^[Yy]$ ]]
}

run_cmd() {
    if [ "$DRY_RUN" = true ]; then
        echo "  [DRY RUN] $*"
    else
        "$@"
    fi
}

calculate_space() {
    du -s "$REPO_ROOT" 2>/dev/null | cut -f1 || echo "0"
}

SPACE_BEFORE=$(calculate_space)

print_header "Trading Gateway Cleanup"

if [ "$DRY_RUN" = true ]; then
    print_warning "DRY RUN MODE - No changes will be made"
fi

echo ""
echo "Repository: $REPO_ROOT"
echo ""

# ============================================================================
# 1. Rust Build Artifacts
# ============================================================================
print_header "1. Rust Build Artifacts"

if [ -d "target" ]; then
    TARGET_SIZE=$(get_size "target")
    print_step "target/ directory: $TARGET_SIZE"

    if confirm "Clean Rust build artifacts (cargo clean)?"; then
        print_step "Running cargo clean..."
        run_cmd cargo clean
        print_success "Rust build artifacts cleaned"
    else
        print_warning "Skipped cargo clean"
    fi
else
    print_success "target/ directory not found (already clean)"
fi

# ============================================================================
# 2. Docker Compose Services
# ============================================================================
print_header "2. Docker Compose Services"

if command -v docker &> /dev/null; then
    if [ -f "docker-compose.yml" ] || [ -f "compose.yml" ]; then
        RUNNING=$(docker compose ps -q 2>/dev/null | wc -l | tr -d ' ')

        if [ "$RUNNING" -gt 0 ]; then
            print_step "Found $RUNNING running container(s)"
            docker compose ps 2>/dev/null || true
            echo ""
        fi

        if confirm "Stop and remove Docker Compose services (including volumes)?"; then
            print_step "Stopping Docker Compose services..."
            run_cmd docker compose down -v --remove-orphans
            print_success "Docker Compose services stopped and volumes removed"
        else
            print_warning "Skipped Docker Compose cleanup"
        fi
    else
        print_warning "docker-compose.yml not found"
    fi
else
    print_warning "Docker not available"
fi

# ============================================================================
# 3. Docker System Cleanup
# ============================================================================
print_header "3. Docker System Cleanup"

if command -v docker &> /dev/null; then
    print_step "Current Docker disk usage:"
    docker system df 2>/dev/null || true
    echo ""

    DANGLING=$(docker images -f "dangling=true" -q 2>/dev/null | wc -l | tr -d ' ')
    if [ "$DANGLING" -gt 0 ]; then
        print_step "Found $DANGLING dangling image(s)"

        if confirm "Remove dangling Docker images?"; then
            run_cmd docker image prune -f
            print_success "Dangling images removed"
        else
            print_warning "Skipped dangling images cleanup"
        fi
    else
        print_success "No dangling images found"
    fi

    STOPPED=$(docker ps -a -f "status=exited" -q 2>/dev/null | wc -l | tr -d ' ')
    if [ "$STOPPED" -gt 0 ]; then
        print_step "Found $STOPPED stopped container(s)"

        if confirm "Remove stopped containers?"; then
            run_cmd docker container prune -f
            print_success "Stopped containers removed"
        else
            print_warning "Skipped stopped containers cleanup"
        fi
    else
        print_success "No stopped containers found"
    fi

    VOLUMES=$(docker volume ls -q -f "dangling=true" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$VOLUMES" -gt 0 ]; then
        print_step "Found $VOLUMES unused volume(s)"

        if confirm "Remove unused Docker volumes?"; then
            run_cmd docker volume prune -f
            print_success "Unused volumes removed"
        else
            print_warning "Skipped volume cleanup"
        fi
    else
        print_success "No unused volumes found"
    fi
else
    print_warning "Docker not available"
fi

# ============================================================================
# 4. Python Example Artifacts
# ============================================================================
print_header "4. Python Example Artifacts"

if [ -d "examples/python" ]; then
    VENVS=$(find examples/python -maxdepth 3 -type d -name ".venv" 2>/dev/null || true)
    # Skip pycaches/egg-info inside .venv — they go when the venv goes.
    PYCACHES=$(find examples/python -type d -name "__pycache__" -not -path "*/.venv/*" 2>/dev/null || true)
    EGGINFO=$(find examples/python -type d -name "*.egg-info" -not -path "*/.venv/*" 2>/dev/null || true)

    if [ -n "$VENVS" ] || [ -n "$PYCACHES" ] || [ -n "$EGGINFO" ]; then
        if [ -n "$VENVS" ]; then
            while IFS= read -r v; do
                [ -n "$v" ] && print_step "$(get_size "$v")  $v"
            done <<< "$VENVS"
        fi
        if [ -n "$PYCACHES" ]; then
            CACHE_COUNT=$(echo "$PYCACHES" | wc -l | tr -d ' ')
            print_step "$CACHE_COUNT __pycache__ directories"
        fi
        if [ -n "$EGGINFO" ]; then
            EGG_COUNT=$(echo "$EGGINFO" | wc -l | tr -d ' ')
            print_step "$EGG_COUNT *.egg-info directories"
        fi

        if confirm "Remove Python venvs, __pycache__, and *.egg-info under examples/python?"; then
            if [ -n "$VENVS" ]; then
                while IFS= read -r v; do
                    [ -n "$v" ] && run_cmd rm -rf "$v"
                done <<< "$VENVS"
            fi
            if [ -n "$PYCACHES" ]; then
                while IFS= read -r p; do
                    [ -n "$p" ] && run_cmd rm -rf "$p"
                done <<< "$PYCACHES"
            fi
            if [ -n "$EGGINFO" ]; then
                while IFS= read -r e; do
                    [ -n "$e" ] && run_cmd rm -rf "$e"
                done <<< "$EGGINFO"
            fi
            print_success "Python artifacts removed"
        else
            print_warning "Skipped Python artifacts cleanup"
        fi
    else
        print_success "No Python artifacts found (already clean)"
    fi
else
    print_success "examples/python/ not present"
fi

# ============================================================================
# 5. Cargo Registry Cache (Optional - Aggressive)
# ============================================================================
print_header "5. Cargo Cache (Optional)"

CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
REGISTRY_CACHE="$CARGO_HOME/registry/cache"
REGISTRY_SRC="$CARGO_HOME/registry/src"

if [ -d "$REGISTRY_CACHE" ] || [ -d "$REGISTRY_SRC" ]; then
    CACHE_SIZE=$(get_size "$REGISTRY_CACHE")
    SRC_SIZE=$(get_size "$REGISTRY_SRC")

    print_warning "Cargo registry cache: $CACHE_SIZE"
    print_warning "Cargo registry src: $SRC_SIZE"
    print_warning "Cleaning these will slow down next build (re-downloads dependencies)"

    if [ "$ALL_MODE" = false ]; then
        if confirm "Clean cargo registry cache? (SLOW next build)"; then
            run_cmd rm -rf "$REGISTRY_CACHE"
            run_cmd rm -rf "$REGISTRY_SRC"
            print_success "Cargo registry cache cleaned"
        else
            print_warning "Skipped cargo cache cleanup"
        fi
    else
        print_warning "Skipped cargo cache in --all mode (too aggressive)"
    fi
else
    print_success "No cargo cache to clean"
fi

# ============================================================================
# Summary
# ============================================================================
print_header "Cleanup Summary"

SPACE_AFTER=$(calculate_space)

if [ "$DRY_RUN" = false ]; then
    SAVED=$((SPACE_BEFORE - SPACE_AFTER))

    if [ "$SAVED" -gt 1048576 ]; then
        SAVED_HUMAN="$((SAVED / 1048576)) GB"
    elif [ "$SAVED" -gt 1024 ]; then
        SAVED_HUMAN="$((SAVED / 1024)) MB"
    else
        SAVED_HUMAN="$SAVED KB"
    fi

    echo -e "${GREEN}Space recovered: ~$SAVED_HUMAN${NC}"
fi

echo ""
print_success "Cleanup complete!"
echo ""

echo "Next steps:"
echo "  • Build project:          cargo build"
echo "  • Run quality gate:       make check"
echo ""
