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
if grep -Eq 'build-charts|build-nasr|build-cifp|gh release' "$workflow"; then
    fail "edition build and Release logic belongs in scripts/publish-editions.sh, not the YAML"
fi

# --- 2. Publisher against fakes ------------------------------------------------------------
cargo build --quiet --locked -p flightbag-cli
real_bin="$root/target/debug/flightbag"

export FAKE_GH_ROOT="$scratch/gh" FAKE_UPSTREAM="$scratch/upstream" FAKE_LOG="$scratch/log"
export REAL_FLIGHTBAG="$real_bin"

# Fake `gh`: one directory per Release tag, one file per asset, `.prerelease` as a flag.
cat >"$scratch/gh.sh" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = "release" ] || { echo "fake gh: unsupported $*" >&2; exit 2; }
action="$2"
shift 2
case "$action" in
    list)
        [ -d "$FAKE_GH_ROOT" ] && ls "$FAKE_GH_ROOT"
        exit 0
        ;;
esac
tag="$1"
shift
dir="$FAKE_GH_ROOT/$tag"
case "$action" in
    view)
        [ -d "$dir" ] || exit 1
        case "$*" in
            *isPrerelease*) if [ -e "$dir/.prerelease" ]; then echo true; else echo false; fi ;;
            *assets*) ls "$dir" ;;
        esac
        ;;
    create)
        [ ! -d "$dir" ] || exit 1
        mkdir -p "$dir"
        case " $* " in *" --prerelease "*) touch "$dir/.prerelease" ;; esac
        echo "create $tag" >>"$FAKE_LOG"
        ;;
    upload)
        [ -d "$dir" ] || exit 1
        for arg in "$@"; do
            [ "$arg" = "--clobber" ] && continue
            cp "$arg" "$dir/"
            echo "upload $tag $(basename "$arg")" >>"$FAKE_LOG"
        done
        ;;
    delete-asset)
        rm "$dir/$1"
        echo "delete-asset $tag $1" >>"$FAKE_LOG"
        ;;
    edit)
        rm -f "$dir/.prerelease"
        echo "edit $tag" >>"$FAKE_LOG"
        ;;
    download)
        [ -d "$dir" ] || exit 1
        dest="." matched=false patterns=""
        while [ "$#" -gt 0 ]; do
            case "$1" in
                -p) patterns="$patterns $2"; shift 2 ;;
                -D) dest="$2"; shift 2 ;;
                *) shift ;;
            esac
        done
        mkdir -p "$dest"
        for asset in "$dir"/*; do
            [ -f "$asset" ] || continue
            for pattern in $patterns; do
                # shellcheck disable=SC2254
                case "$(basename "$asset")" in
                    $pattern) cp "$asset" "$dest/"; matched=true ;;
                esac
            done
        done
        [ "$matched" = "true" ]
        ;;
    *) echo "fake gh: unsupported release $action" >&2; exit 2 ;;
esac
FAKE

# Fake upstream: `$FAKE_UPSTREAM/<product>-<date>` holds the posting's Last-Modified (or the
# word ERROR for an outage); `fail-build-<product>-<date>` makes that build fail. Cycle
# arithmetic, the rolling manifest and the index come from the real CLI.
cat >"$scratch/flightbag.sh" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
sub="$1"
shift
case "$sub" in
    plan | manifest | index) exec "$REAL_FLIGHTBAG" "$sub" "$@" ;;
esac
product="${sub#*-}" date="" out=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --cycle) date="$2"; shift 2 ;;
        --out-dir) out="$2"; shift 2 ;;
        *) shift ;;
    esac
done
posting="$FAKE_UPSTREAM/$product-$date"
case "$sub" in
    probe-*)
        if [ ! -f "$posting" ]; then
            jq -n --arg d "$date" '{cycle: $d, url: "https://upstream.invalid/\($d)", published: false, upstream: {last_modified: null, etag: null, size_bytes: null}}'
        elif [ "$(cat "$posting")" = "ERROR" ]; then
            exit 1
        else
            jq -n --arg d "$date" --arg lm "$(cat "$posting")" '{cycle: $d, url: "https://upstream.invalid/\($d)", published: true, upstream: {last_modified: $lm, etag: null, size_bytes: 1}}'
        fi
        ;;
    build-*)
        echo "build $product $date" >>"$FAKE_LOG"
        [ ! -e "$FAKE_UPSTREAM/fail-build-$product-$date" ] || exit 1
        case "$product" in
            nasr) artifact=airspace.pmtiles manifest=manifest.json ;;
            cifp) artifact=cifp_procedures.bin manifest=cifp_faa_cifp.json ;;
        esac
        mkdir -p "$out"
        printf '%s %s %s\n' "$product" "$date" "$(cat "$posting")" >"$out/$artifact"
        if command -v sha256sum >/dev/null 2>&1; then
            sha="$(sha256sum "$out/$artifact" | cut -d' ' -f1)"
        else
            sha="$(shasum -a 256 "$out/$artifact" | cut -d' ' -f1)"
        fi
        printf '%s  %s\n' "$sha" "$artifact" >"$out/$artifact.sha256"
        size="$(wc -c <"$out/$artifact" | tr -d ' ')"
        cycle="$("$REAL_FLIGHTBAG" plan --now "${date}T12:00:00Z" | jq '.faa_nasr_28_day.current')"
        record="$(jq -n --arg f "$artifact" --arg s "$sha" --argjson n "$size" \
            '{file: $f, size_bytes: $n, sha256_hex: $s, tile_count: 1, min_zoom: 2, max_zoom: 10, bounds: [-180, -15, 180, 82]}')"
        upstream_sha="$(printf '%064d' 0)"
        base="$(jq -n --argjson c "$cycle" '{schema_version: 1, disclaimer: "test", cycle_index: 0,
            airac_code: $c.airac_code, effective_date: $c.effective_date, expires_date: $c.expires_date,
            effective_from_utc: $c.effective_from_utc, effective_until_utc: $c.effective_until_utc}')"
        if [ "$product" = "nasr" ]; then
            jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                edition_id: "test-nasr", airspace_pmtiles: $r,
                upstream_nasr: {authority: "faa_nasr", url: "https://upstream.invalid", filename: "n.zip", size_bytes: 1, sha256_hex: $u},
                counts: {total: 0, class_b: 0, class_c: 0, class_d: 0, class_e: 0, sua: 0}}' >"$out/$manifest"
        else
            jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                edition_id: "test-cifp", authority: "faa_cifp", upstream_url: "https://upstream.invalid",
                upstream_sha256_hex: $u, cifp_binary: ($r | {file, size_bytes, sha256_hex, bounds}),
                counts: {airports: 0, vhf_navaids: 0, ndbs: 0, enroute_waypoints: 0, terminal_waypoints: 0,
                    runway_thresholds: 0, airways: 0, airway_fixes: 0, sids: 0, stars: 0, approaches: 0, procedure_legs: 0}}' >"$out/$manifest"
        fi
        ;;
esac
FAKE
chmod +x "$scratch/gh.sh" "$scratch/flightbag.sh"

site="$scratch/site"
run_publisher() {
    local now="$1"
    shift
    : >"$FAKE_LOG"
    : >"$scratch/output"
    rm -rf "$scratch/work"
    env FLIGHTBAG_BIN="$scratch/flightbag.sh" GH_BIN="$scratch/gh.sh" REPO_SLUG="example/flightbag" \
        WORK_DIR="$scratch/work" SITE_DIR="$site" CACHE_DIR="$scratch/cache" NOW_UTC="$now" \
        GITHUB_OUTPUT="$scratch/output" LIVE_INDEX_URL="file://$scratch/live/cycles.json" "$@" \
        "$root/scripts/publish-editions.sh" >"$scratch/run.log" 2>&1
}
# Stands in for the Pages deployment that follows a staged site.
deploy_if_staged() {
    if grep -qx 'deploy=true' "$scratch/output"; then
        mkdir -p "$scratch/live"
        cp "$site/cycles.json" "$site/manifest.json" "$scratch/live/"
    fi
}
expect_output() { grep -qx "$1" "$scratch/output" || { cat "$scratch/run.log" >&2; fail "$2: expected $1"; }; }
expect_log() { [ "$(grep -c "$1" "$FAKE_LOG" || true)" = "$2" ] || { cat "$FAKE_LOG" >&2; fail "$3"; }; }
expect_json() { jq -e "$2" "$1" >/dev/null || { cat "$1" >&2; fail "$3"; }; }
post() { mkdir -p "$FAKE_UPSTREAM" && printf '%s' "$2" >"$FAKE_UPSTREAM/$1"; }
reset_world() {
    rm -rf "$FAKE_GH_ROOT" "$FAKE_UPSTREAM" "$site" "$scratch/live"
    mkdir -p "$FAKE_GH_ROOT" "$FAKE_UPSTREAM"
    for edition in nasr-2026-10-29 cifp-2026-10-29 nasr-2026-11-26 cifp-2026-11-26; do
        post "$edition" "first posting"
    done
}

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
