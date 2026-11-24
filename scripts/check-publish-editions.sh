#!/usr/bin/env bash
# Guards the edition publisher without network access or a real Release:
#   1. every workflow file parses as YAML and the publishing workflow keeps its invariants;
#   2. `publish-editions.sh` runs against a fake `gh` (Releases as directories) and a fake
#      upstream (probe/build), with the real `flightbag plan|manifest|index`, across the cases
#      that decide what consumers see: first run, idle rerun, upstream re-post, both sides of
#      the 09:01Z cutover, a failing upcoming build, a missing effective edition, an upstream
#      outage, dry-run, and the static-site size limit.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# --- 1. Workflow files ---------------------------------------------------------------------
workflow="$root/.github/workflows/airac-pages.yml"
if python3 -c 'import yaml' 2>/dev/null; then
    python3 - "$root"/.github/workflows/*.yml <<'PY'
import sys, yaml
for path in sys.argv[1:]:
    with open(path) as handle:
        yaml.safe_load(handle)
PY
elif command -v ruby >/dev/null 2>&1; then
    ruby -ryaml -e 'ARGV.each { |path| YAML.safe_load(File.read(path), aliases: true) }' \
        "$root"/.github/workflows/*.yml
else
    fail "neither PyYAML nor ruby is available to parse the workflow files"
fi

grep -q '^  schedule:' "$workflow" || fail "publishing workflow lost its schedule trigger"
grep -q 'cancel-in-progress: false' "$workflow" || fail "publishing runs must queue, not cancel"
grep -q 'scripts/publish-editions.sh' "$workflow" || fail "workflow must call publish-editions.sh"
for setting in IPFS_KUBO_RPC_URL IPFS_KUBO_RPC_TOKEN IPFS_PIN_SERVICE_URL IPFS_PIN_SERVICE_TOKEN IPFS_GATEWAY_URL; do
    grep -Eq "^ +$setting: \\$\\{\\{ (vars|secrets)\\.$setting \\}\\}$" "$workflow" \
        || fail "workflow must hand $setting to the publisher"
done
if grep -Eq 'IPFS_[A-Z_]*TOKEN: \$\{\{ vars\.' "$workflow"; then
    fail "IPFS tokens are secrets, not repository variables"
fi
grep -Eq '^sha512="[0-9a-f]{128}"$' "$root/scripts/install-kubo.sh" || fail "the Kubo download must be pinned by SHA-512"
if grep -Eq 'build-charts|build-nasr|build-cifp|gh release' "$workflow"; then
    fail "edition build and Release logic belongs in scripts/publish-editions.sh, not the YAML"
fi

# --- 2. Publisher against fakes ------------------------------------------------------------
# shellcheck source=scripts/testing/publisher-fakes.sh
source "$root/scripts/testing/publisher-fakes.sh"

before="2026-11-26T09:00:59Z"
cutover="2026-11-26T09:01:00Z"

# First run on a Release that holds only an artifact and its manifest, with no completion marker.
reset_world
mkdir -p "$FAKE_GH_ROOT/airac-2026-10-29"
echo legacy >"$FAKE_GH_ROOT/airac-2026-10-29/airspace.pmtiles"
echo '{}' >"$FAKE_GH_ROOT/airac-2026-10-29/manifest.json"
run_publisher "2026-11-24T04:00:00Z" || fail "first run exited non-zero"
expect_output 'deploy=true' "first run"
expect_output 'required_failed=false' "first run"
expect_log '^build ' 4 "first run must build both products of both editions"
[ -e "$FAKE_GH_ROOT/airac-2026-11-26/.prerelease" ] || fail "pre-staged Release must be a prerelease"
[ ! -e "$FAKE_GH_ROOT/airac-2026-10-29/.prerelease" ] || fail "effective Release must not be a prerelease"
expect_json "$site/cycles.json" '.current.effective_date == "2026-10-29"' "current must be the effective edition"
expect_json "$site/cycles.json" '.editions.faa_nasr.upcoming.effective_from_utc == "2026-11-26T09:01:00Z"' "upcoming must be listed with its effective time"
expect_json "$site/cycles.json" '.editions.faa_cifp.upcoming.site_path == "cycles/2026-11-26/cifp_procedures.bin"' "upcoming CIFP must be pre-staged"
cmp -s "$site/cycles/current/airspace.pmtiles" "$site/cycles/2026-10-29/airspace.pmtiles" || fail "alias must copy the effective edition"
[ -f "$site/cycles/2026-11-26/airspace.pmtiles" ] || fail "upcoming edition must be pre-staged under its own path"
[ ! -e "$site/styles/airspace_vfr_sectional.json" ] || fail "sectional style needs archives the site cannot host"
[ -f "$FAKE_GH_ROOT/flightbag-manifest/manifest.json" ] || fail "rolling manifest Release missing"
deploy_if_staged

# Idle rerun: nothing new upstream, nothing to write, nothing to deploy.
run_publisher "2026-11-24T07:00:00Z" || fail "idle run exited non-zero"
expect_output 'deploy=false' "idle run"
[ ! -s "$FAKE_LOG" ] || { cat "$FAKE_LOG" >&2; fail "idle run must not build or write a Release"; }

# Upstream re-posts the pre-staged NASR edition: rebuild that edition only.
current_before="$(cat "$FAKE_GH_ROOT/airac-2026-10-29/airspace.pmtiles")"
post nasr-2026-11-26 "second posting"
run_publisher "2026-11-24T10:00:00Z" || fail "re-post run exited non-zero"
expect_log '^build ' 1 "a re-post must rebuild exactly one edition"
expect_log '^build nasr 2026-11-26$' 1 "the re-posted edition must be the one rebuilt"
[ "$(cat "$FAKE_GH_ROOT/airac-2026-10-29/airspace.pmtiles")" = "$current_before" ] || fail "re-post touched the effective edition"
grep -q 'second posting' "$FAKE_GH_ROOT/airac-2026-11-26/airspace.pmtiles" || fail "re-posted edition was not replaced"
expect_output 'deploy=true' "re-post run"
deploy_if_staged

# One second before the cutover the pointer must not move; at the cutover it must.
run_publisher "$before" || fail "pre-cutover run exited non-zero"
expect_output 'deploy=false' "pre-cutover run"
run_publisher "$cutover" || fail "cutover run exited non-zero"
expect_output 'deploy=true' "cutover run"
expect_output 'required_failed=false' "cutover run"
expect_log '^build ' 0 "the cutover must reuse the pre-staged edition"
expect_json "$site/cycles.json" '.current.effective_date == "2026-11-26" and .editions.faa_nasr.upcoming == null' "pointer must flip at the effective second"
cmp -s "$site/cycles/current/airspace.pmtiles" "$site/cycles/2026-11-26/airspace.pmtiles" || fail "alias must follow the flip"
[ ! -e "$FAKE_GH_ROOT/airac-2026-11-26/.prerelease" ] || fail "effective Release must leave prerelease"
deploy_if_staged

# An upstream outage keeps the published edition and does not fail the run.
post nasr-2026-11-26 "ERROR"
run_publisher "2026-11-27T00:00:00Z" || fail "outage run exited non-zero"
expect_output 'required_failed=false' "outage run"
expect_log '^build ' 0 "an outage must not trigger a rebuild"

# A failing upcoming build leaves current published and the run green.
reset_world
touch "$FAKE_UPSTREAM/fail-build-nasr-2026-11-26"
run_publisher "2026-11-24T04:00:00Z" || fail "failing-upcoming run exited non-zero"
expect_output 'deploy=true' "failing-upcoming run"
expect_output 'required_failed=false' "failing-upcoming run"
expect_json "$site/cycles.json" '.current.effective_date == "2026-10-29" and .editions.faa_nasr.upcoming == null and .editions.faa_cifp.upcoming != null' "a failed upcoming edition must only be absent"
deploy_if_staged
staged_before="$(cat "$site/cycles.json")"

# The cutover arrives and the effective NASR edition was never built: keep the old site, go red.
run_publisher "$cutover" || fail "missing-current run must report through its outputs"
expect_output 'deploy=false' "missing-current run"
expect_output 'required_failed=true' "missing-current run"
[ "$(cat "$site/cycles.json")" = "$staged_before" ] || fail "a missing effective edition must not restage the site"

# Dry run performs no Release write.
reset_world
run_publisher "2026-11-24T04:00:00Z" DRY_RUN=true || true
[ -z "$(ls -A "$FAKE_GH_ROOT")" ] || fail "dry run wrote a Release"

# The static site refuses anything beyond its size limits.
reset_world
if run_publisher "2026-11-24T04:00:00Z" SITE_MAX_TOTAL_BYTES=64; then
    fail "an oversized site must fail the run before deployment"
fi
if grep -qx 'deploy=true' "$scratch/output"; then fail "an oversized site must not be marked for deployment"; fi

echo "publish-editions checks passed"
