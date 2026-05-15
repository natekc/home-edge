#!/usr/bin/env bash
# check-ux-parity.sh — Offline token-drift audit between home-edge and HA upstream.
#
# Usage:
#   scripts/check-ux-parity.sh              # offline check only (no network)
#   scripts/check-ux-parity.sh --fetch      # also diff against HA upstream on GitHub
#
# What it does:
#   1. Verify docs/ux-parity.toml parses (basic TOML syntax check).
#   2. Cross-check that every token in [css_tokens.required] is declared in _css.html.
#   3. Cross-check that every [[pages]].template file exists on disk.
#   4. (--fetch) Fetch HA's ha-style.ts from GitHub and report NEW tokens upstream
#      that aren't in our required list yet.
#
# Complements the Rust tests (which enforce structural correctness at compile/test time).
# This script is for periodic maintenance: run it after HA releases to catch new tokens.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
export MANIFEST="${REPO_ROOT}/docs/ux-parity.toml"
export CSS_FILE="${REPO_ROOT}/crates/controller/templates/_css.html"
export TEMPLATES_DIR="${REPO_ROOT}/crates/controller/templates"

RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
RESET='\033[0m'

pass() { echo -e "${GREEN}✓${RESET} $*"; }
warn() { echo -e "${YELLOW}⚠${RESET} $*"; }
fail() { echo -e "${RED}✗${RESET} $*"; }
info() { echo -e "${CYAN}ℹ${RESET} $*"; }

ERRORS=0
error() { fail "$*"; (( ERRORS++ )) || true; }

# ---------------------------------------------------------------------------
# 1. Basic manifest existence / syntax
# ---------------------------------------------------------------------------
echo ""
info "Checking docs/ux-parity.toml …"

if [[ ! -f "${MANIFEST}" ]]; then
    error "docs/ux-parity.toml not found at ${MANIFEST}"
    exit 1
fi

# Quick TOML syntax check via Python (available on macOS and most CI images).
if command -v python3 &>/dev/null; then
    python3 - <<EOF
import sys
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        print("  (skipping TOML syntax check: no tomllib/tomli available)")
        sys.exit(0)
with open("${MANIFEST}", "rb") as f:
    tomllib.load(f)
print("  TOML syntax OK")
EOF
else
    warn "python3 not found; skipping TOML syntax check"
fi

# ---------------------------------------------------------------------------
# 2. CSS token coverage: every required token must be in _css.html
# ---------------------------------------------------------------------------
echo ""
info "Checking CSS token declarations in _css.html …"

if [[ ! -f "${CSS_FILE}" ]]; then
    error "_css.html not found at ${CSS_FILE}"
else
    # Extract required token names from the manifest.
    # Format in TOML: one per line inside required = [ "--token", ... ]
    REQUIRED_TOKENS=$(python3 - <<'EOF'
import sys, re
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        sys.exit(0)
import os
with open(os.environ["MANIFEST"], "rb") as f:
    data = tomllib.load(f)
for token in data.get("css_tokens", {}).get("required", []):
    print(token)
EOF
    )

    MISSING_TOKENS=()
    while IFS= read -r token; do
        [[ -z "$token" ]] && continue
        needle="${token}:"
        if ! grep -qF -- "${needle}" "${CSS_FILE}"; then
            MISSING_TOKENS+=("${token}")
        fi
    done <<< "${REQUIRED_TOKENS}"

    if [[ ${#MISSING_TOKENS[@]} -eq 0 ]]; then
        pass "All required CSS tokens are declared in _css.html"
    else
        for t in "${MISSING_TOKENS[@]}"; do
            error "CSS token '${t}' missing from _css.html"
        done
    fi
fi

# ---------------------------------------------------------------------------
# 3. Template file existence: every [[pages]].template must exist on disk
# ---------------------------------------------------------------------------
echo ""
info "Checking that all [[pages]].template files exist …"

PAGE_TEMPLATES=$(python3 - <<'EOF'
import sys, os
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        sys.exit(0)
with open(os.environ["MANIFEST"], "rb") as f:
    data = tomllib.load(f)
for page in data.get("pages", []):
    print(page["template"])
EOF
)

MISSING_TEMPLATES=()
while IFS= read -r tmpl; do
    [[ -z "$tmpl" ]] && continue
    if [[ ! -f "${TEMPLATES_DIR}/${tmpl}" ]]; then
        MISSING_TEMPLATES+=("${tmpl}")
    fi
done <<< "${PAGE_TEMPLATES}"

if [[ ${#MISSING_TEMPLATES[@]} -eq 0 ]]; then
    pass "All [[pages]].template files exist on disk"
else
    for t in "${MISSING_TEMPLATES[@]}"; do
        error "Template '${t}' declared in ux-parity.toml does not exist in templates/"
    done
fi

# ---------------------------------------------------------------------------
# 4. Nav href reachability: every [[nav]].href must have a matching route
#    (cheap heuristic: check http.rs for the path string)
# ---------------------------------------------------------------------------
echo ""
info "Checking [[nav]].href values appear as routes in http.rs …"

HTTP_RS="${REPO_ROOT}/crates/controller/src/http.rs"
NAV_HREFS=$(python3 - <<'EOF'
import sys, os
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        sys.exit(0)
with open(os.environ["MANIFEST"], "rb") as f:
    data = tomllib.load(f)
for item in data.get("nav", []):
    href = item.get("href", "")
    if href and href != "/":
        print(href)
EOF
)

MISSING_ROUTES=()
while IFS= read -r href; do
    [[ -z "$href" ]] && continue
    # Strip leading slash and look for the path in route definitions.
    path="${href#/}"
    if ! grep -qF -- "\"/${path}\"" "${HTTP_RS}" 2>/dev/null; then
        MISSING_ROUTES+=("${href}")
    fi
done <<< "${NAV_HREFS}"

if [[ ${#MISSING_ROUTES[@]} -eq 0 ]]; then
    pass "All [[nav]].href values appear as routes in http.rs"
else
    for r in "${MISSING_ROUTES[@]}"; do
        warn "Nav href '${r}' not found as a route in http.rs (may be a false positive)"
    done
fi

# ---------------------------------------------------------------------------
# 5. (Optional) Upstream token drift: diff against HA frontend source
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--fetch" ]]; then
    echo ""
    info "Fetching HA upstream ha-style.ts for token drift check …"

    HA_STYLE_URL="https://raw.githubusercontent.com/home-assistant/frontend/dev/src/resources/ha-style.ts"
    TMP_HA_STYLE=$(mktemp /tmp/ha-style-XXXXXX.ts)
    trap "rm -f ${TMP_HA_STYLE}" EXIT

    if ! curl -fsSL "${HA_STYLE_URL}" -o "${TMP_HA_STYLE}" 2>/dev/null; then
        warn "Could not fetch ${HA_STYLE_URL} — skipping upstream drift check"
    else
        # Extract CSS custom-property names from the upstream file.
        # They appear as strings like "--primary-color" inside cssString.
        UPSTREAM_TOKENS=$(grep -oE '"--[a-z][a-z0-9-]+"' "${TMP_HA_STYLE}" \
            | tr -d '"' | sort -u || true)

        # Extract what we already declare as required.
        REQUIRED_TOKENS_SET=$(python3 - <<'EOF'
import sys, os
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        sys.exit(0)
with open(os.environ["MANIFEST"], "rb") as f:
    data = tomllib.load(f)
for token in data.get("css_tokens", {}).get("required", []):
    print(token)
EOF
        )

        NEW_UPSTREAM=()
        while IFS= read -r token; do
            [[ -z "$token" ]] && continue
            # Skip tokens that are already in our required list.
            if ! echo "${REQUIRED_TOKENS_SET}" | grep -qxF "${token}"; then
                # Only report tokens that HA defines AND we don't yet require.
                NEW_UPSTREAM+=("${token}")
            fi
        done <<< "${UPSTREAM_TOKENS}"

        if [[ ${#NEW_UPSTREAM[@]} -eq 0 ]]; then
            pass "No new tokens in HA upstream not already covered by our manifest"
        else
            echo ""
            warn "New tokens found in HA upstream (${HA_STYLE_URL}):"
            warn "Consider adding these to [css_tokens.required] in docs/ux-parity.toml:"
            for t in "${NEW_UPSTREAM[@]}"; do
                echo "    ${t}"
            done
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
if [[ ${ERRORS} -eq 0 ]]; then
    pass "All UX parity checks passed."
    exit 0
else
    fail "${ERRORS} UX parity check(s) failed. See above for details."
    exit 1
fi
