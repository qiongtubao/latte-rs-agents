#!/usr/bin/env bash
# =============================================================================
# latte-agent install script — one-click global config setup
# =============================================================================
# Usage:
#   ./install.sh              # install configs to ~/.latte/
#   ./install.sh --dry-run    # show what would be installed, don't write
#   ./install.sh --dir /path  # install to a custom directory
#   ./install.sh --force      # overwrite existing files without prompting
#   ./install.sh --help       # show this help
#
# What this installs:
#   ~/.latte/
#   ├── models.toml          # model catalog with tier mappings
#   ├── agents.d/            # one file per role (11 roles)
#   │   ├── pm.toml
#   │   ├── architect.toml
#   │   ├── programmer.toml
#   │   ├── tester.toml
#   │   ├── reviewer.toml
#   │   ├── security.toml
#   │   ├── designer.toml
#   │   ├── devops.toml
#   │   ├── tech_writer.toml
#   │   ├── manager.toml
#   │   └── advisor.toml
#   ├── agents.toml          # merged single-file agent config
#   ├── workflows.d/         # one file per workflow (6 workflows)
#   │   ├── default.toml
#   │   ├── code_review.toml
#   │   ├── bug_triage.toml
#   │   ├── design_brainstorm.toml
#   │   ├── requirements_review.toml
#   │   └── tech_director_dispatch.toml
#   └── discussion.toml      # merged single-file discussion config
#
# After install, these files are used by the three-layer config resolver:
#   ~/.latte/models.toml is the default global model config source.
#   ~/.latte/models.d/*     can hold additional model files (optional).
#   config/models.toml      is the project-level layer (shipped with repo).
#   --api-key / --model-override flags are the CLI layer.
#
# The global layer fills api_key and base_url for any model the project
# config leaves blank — so users can define API keys once in ~/.latte/
# and reuse them across projects.
# =============================================================================

set -euo pipefail

# ─── Configuration ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_DIR="${SCRIPT_DIR}/config"
DEFAULT_TARGET="${LATTE_HOME:-$HOME/.latte}"

DRY_RUN=false
FORCE=false
TARGET_DIR="$DEFAULT_TARGET"

# ─── Help ────────────────────────────────────────────────────────────────

show_help() {
    sed -n '1,/^# =/{ /^# =/d; s/^# //p; s/^#$//p; }' "$0"
    exit 0
}

# ─── Args ────────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h)
            show_help
            ;;
        --dry-run|-n)
            DRY_RUN=true
            shift
            ;;
        --force|-f)
            FORCE=true
            shift
            ;;
        --dir|-d)
            TARGET_DIR="$2"
            shift 2
            ;;
        *)
            echo "Error: unknown flag '$1'. Use --help for usage."
            exit 1
            ;;
    esac
done

# ─── Colors ──────────────────────────────────────────────────────────────

if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' CYAN='' BOLD='' NC=''
fi

# ─── Helpers ─────────────────────────────────────────────────────────────

section() {
    echo ""
    echo -e "${BOLD}${CYAN}═══ $1 ═══${NC}"
}

info()   { echo -e "  ${GREEN}✓${NC} $1"; }
warn()   { echo -e "  ${YELLOW}⚠${NC}  $1"; }
error()  { echo -e "  ${RED}✗${NC} $1"; }
detail() { echo -e "    $1"; }

install_file() {
    local src="$1"
    local dst="$2"
    local name="$3"

    if [[ ! -f "$src" ]]; then
        warn "skipping $name (source not found: $src)"
        return 0
    fi

    if [[ -f "$dst" ]] && ! $FORCE; then
        warn "$name already exists at $dst"
        if [[ -t 0 ]]; then
            read -r -p "    Overwrite? [y/N] " answer
            if [[ ! "$answer" =~ ^[Yy]$ ]]; then
                detail "skipped"
                return 0
            fi
        else
            detail "skipped (non-interactive, use --force to overwrite)"
            return 0
        fi
    fi

    if $DRY_RUN; then
        info "[dry-run] would install $name → $dst"
    else
        mkdir -p "$(dirname "$dst")"
        cp "$src" "$dst"
        info "installed $name → $dst"
    fi
}

# ─── Validation ──────────────────────────────────────────────────────────

section "Validating source config"

if [[ ! -d "$SOURCE_DIR" ]]; then
    error "source config directory not found: $SOURCE_DIR"
    error "run this script from the latte-rs-agents repo root"
    exit 1
fi

# Check critical files exist
MISSING=()
for f in \
    "agents" \
    "agents/pm.toml" \
    "agents/architect.toml" \
    "agents/programmer.toml" \
    "agents/tester.toml" \
    "agents/reviewer.toml" \
    "agents/security.toml" \
    "agents/designer.toml" \
    "agents/devops.toml" \
    "agents/tech_writer.toml" \
    "agents/manager.toml" \
    "agents/advisor.toml" \
    "workflows/default.toml" \
    "workflows/code_review.toml" \
    "workflows/bug_triage.toml" \
    "workflows/design_brainstorm.toml" \
    "workflows/requirements_review.toml" \
    "workflows/tech_director_dispatch.toml" \
    "models.toml" \
    "agents.toml" \
    "discussion.toml"; do
    if [[ ! -e "$SOURCE_DIR/$f" ]]; then
        MISSING+=("config/$f")
    fi
done

if [[ ${#MISSING[@]} -gt 0 ]]; then
    error "missing config files:"
    for m in "${MISSING[@]}"; do
        detail "$m"
    done
    exit 1
fi
info "all $(ls -1 "$SOURCE_DIR"/agents/*.toml | wc -l | tr -d ' ') agent files found"
info "all $(ls -1 "$SOURCE_DIR"/workflows/*.toml | wc -l | tr -d ' ') workflow files found"
info "models.toml, agents.toml, discussion.toml found"

# ─── Install ─────────────────────────────────────────────────────────────

section "Installing to ${TARGET_DIR}"

if $DRY_RUN; then
    detail "DRY RUN — no files will be written"
    echo ""
fi

# Install agent configs (one per file)
install_file "$SOURCE_DIR/agents/pm.toml"            "$TARGET_DIR/agents.d/pm.toml"            "PM role"
install_file "$SOURCE_DIR/agents/architect.toml"     "$TARGET_DIR/agents.d/architect.toml"     "Architect role"
install_file "$SOURCE_DIR/agents/programmer.toml"    "$TARGET_DIR/agents.d/programmer.toml"    "Programmer role"
install_file "$SOURCE_DIR/agents/tester.toml"        "$TARGET_DIR/agents.d/tester.toml"        "QA role"
install_file "$SOURCE_DIR/agents/reviewer.toml"      "$TARGET_DIR/agents.d/reviewer.toml"      "Reviewer role"
install_file "$SOURCE_DIR/agents/security.toml"      "$TARGET_DIR/agents.d/security.toml"      "Security role"
install_file "$SOURCE_DIR/agents/designer.toml"      "$TARGET_DIR/agents.d/designer.toml"      "Designer role"
install_file "$SOURCE_DIR/agents/devops.toml"        "$TARGET_DIR/agents.d/devops.toml"        "DevOps role"
install_file "$SOURCE_DIR/agents/tech_writer.toml"   "$TARGET_DIR/agents.d/tech_writer.toml"   "Tech Writer role"
install_file "$SOURCE_DIR/agents/manager.toml"       "$TARGET_DIR/agents.d/manager.toml"       "Manager role"
install_file "$SOURCE_DIR/agents/advisor.toml"       "$TARGET_DIR/agents.d/advisor.toml"       "Advisor role"

# Install workflow configs (one per file)
install_file "$SOURCE_DIR/workflows/default.toml"                   "$TARGET_DIR/workflows.d/default.toml"                   "Default workflow"
install_file "$SOURCE_DIR/workflows/code_review.toml"               "$TARGET_DIR/workflows.d/code_review.toml"               "Code Review workflow"
install_file "$SOURCE_DIR/workflows/bug_triage.toml"                "$TARGET_DIR/workflows.d/bug_triage.toml"                "Bug Triage workflow"
install_file "$SOURCE_DIR/workflows/design_brainstorm.toml"         "$TARGET_DIR/workflows.d/design_brainstorm.toml"         "Design Brainstorm workflow"
install_file "$SOURCE_DIR/workflows/requirements_review.toml"       "$TARGET_DIR/workflows.d/requirements_review.toml"       "Requirements Review workflow"
install_file "$SOURCE_DIR/workflows/tech_director_dispatch.toml"    "$TARGET_DIR/workflows.d/tech_director_dispatch.toml"    "Tech Director workflow"

# Install model catalog and merged single-file configs
install_file "$SOURCE_DIR/models.toml"      "$TARGET_DIR/models.toml"      "Model catalog"
install_file "$SOURCE_DIR/agents.toml"      "$TARGET_DIR/agents.toml"      "Merged agent config"
install_file "$SOURCE_DIR/discussion.toml"  "$TARGET_DIR/discussion.toml"  "Merged workflow config"

# ─── Model catalog detection message ────────────────────────────────────

section "Model catalog"

if $DRY_RUN; then
    info "[dry-run] would detect models.toml location"
else
    # Check which format the resolver will use
    if [[ -f "$TARGET_DIR/models.toml" ]]; then
        MODEL_COUNT=$(grep -c '^id =' "$TARGET_DIR/models.toml" 2>/dev/null || echo "?")
        info "model catalog installed: $MODEL_COUNT models defined"
        detail "models include:"
        grep '^name =' "$TARGET_DIR/models.toml" | while read -r line; do
            detail "  $line"
        done
    fi
fi

# ─── Summary ─────────────────────────────────────────────────────────────

section "Installation complete"
echo ""
if $DRY_RUN; then
    echo -e "  ${YELLOW}Dry run finished — no files were written.${NC}"
    echo "  Run without --dry-run to install."
else
    echo -e "  ${GREEN}Config files installed to ${TARGET_DIR}${NC}"
    echo ""
    echo "  Next steps:"
    echo "    1. Set API keys in ~/.latte/models.toml or use environment variables:"
    echo "       export ANTHROPIC_API_KEY=sk-ant-..."
    echo "       export DEEPSEEK_API_KEY=sk-..."
    echo "       export OPENAI_API_KEY=sk-..."
    echo ""
    echo "    2. Verify installation:"
    echo "       latte-agent config show"
    echo ""
    echo "    3. Test a discussion:"
    echo '       latte-agent workflow run default --topic "Design REST API"'
    echo ""
    echo "    4. List available roles and workflows:"
    echo "       latte-agent list roles"
    echo "       latte-agent list workflows"
fi
echo ""

# ─── Validate TOML syntax of installed files ────────────────────────────

if ! $DRY_RUN && command -v python3 &>/dev/null; then
    section "Validating installed TOML files"
    FAILED=0
    while IFS= read -r -d '' f; do
        if ! python3 -c "import tomllib; tomllib.load(open('$f','rb'))" 2>/dev/null; then
            error "TOML parse error in: $f"
            FAILED=1
        fi
    done < <(find "$TARGET_DIR" -name '*.toml' -print0)
    if [[ $FAILED -eq 0 ]]; then
        info "all TOML files parse correctly"
    fi
fi

exit 0
