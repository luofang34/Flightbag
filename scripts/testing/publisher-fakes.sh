# shellcheck shell=bash
# Fakes shared by the publisher guards; sourced by `scripts/check-*.sh`, never executed.
#
# Expects `root` (repository) and `scratch` (a directory the caller removes) and provides a fake
# `gh` (Releases as directories), a fake upstream (probe/build) in front of the real
# `flightbag`, `run_publisher`, and the `expect_*` assertions.

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
# arithmetic, content identifiers, the rolling manifest and the index come from the real CLI.
cat >"$scratch/flightbag.sh" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
sub="$1"
shift
case "$sub" in
    plan | manifest | index | ipfs-cid | ipfs-dir) exec "$REAL_FLIGHTBAG" "$sub" "$@" ;;
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
            waypoints) artifact=nasr_waypoints.bin manifest=waypoints_faa_nasr_waypoints.json ;;
            cifp) artifact=cifp_procedures.bin manifest=cifp_faa_cifp.json ;;
            dtpp) artifact=dtpp_plates.bin manifest=dtpp_faa_dtpp.json ;;
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
        case "$product" in
            nasr)
                jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                    edition_id: "test-nasr", airspace_pmtiles: $r,
                    upstream_nasr: {authority: "faa_nasr", url: "https://upstream.invalid", filename: "n.zip", size_bytes: 1, sha256_hex: $u},
                    counts: {total: 0, class_b: 0, class_c: 0, class_d: 0, class_e: 0, sua: 0}}' >"$out/$manifest"
                ;;
            waypoints)
                jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                    edition_id: "test-waypoints", authority: "faa_nasr_waypoints", upstream_url: "https://upstream.invalid",
                    upstream_sha256_hex: $u, waypoints_binary: ($r | {file, size_bytes, sha256_hex, bounds}),
                    counts: {airports: 0, navaids: 0, fixes: 0, total: 0}}' >"$out/$manifest"
                ;;
            cifp)
                jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                    edition_id: "test-cifp", authority: "faa_cifp", upstream_url: "https://upstream.invalid",
                    upstream_sha256_hex: $u, cifp_binary: ($r | {file, size_bytes, sha256_hex, bounds}),
                    counts: {airports: 0, vhf_navaids: 0, ndbs: 0, enroute_waypoints: 0, terminal_waypoints: 0,
                        runway_thresholds: 0, airways: 0, airway_fixes: 0, sids: 0, stars: 0, approaches: 0, procedure_legs: 0}}' >"$out/$manifest"
                ;;
            dtpp)
                jq -n --argjson b "$base" --argjson r "$record" --arg u "$upstream_sha" '$b + {
                    edition_id: "test-dtpp", authority: "faa_dtpp", upstream_url: "https://upstream.invalid",
                    upstream_sha256_hex: $u, dtpp_binary: ($r | {file, size_bytes, sha256_hex, bounds}),
                    counts: {airports: 0, plate_records: 0, unique_pdfs: 0}}' >"$out/$manifest"
                ;;
        esac
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
# The context is printed on a best-effort basis: a missing file must not hide which check failed.
expect_output() { grep -qx "$1" "$scratch/output" || { cat "$scratch/run.log" >&2 || true; fail "$2: expected $1"; }; }
expect_log() { [ "$(grep -c "$1" "$FAKE_LOG" || true)" = "$2" ] || { cat "$FAKE_LOG" >&2 || true; fail "$3"; }; }
expect_json() { jq -e "$2" "$1" >/dev/null 2>&1 || { cat "$1" >&2 || true; fail "$3"; }; }
post() { mkdir -p "$FAKE_UPSTREAM" && printf '%s' "$2" >"$FAKE_UPSTREAM/$1"; }
reset_world() {
    rm -rf "$FAKE_GH_ROOT" "$FAKE_UPSTREAM" "$site" "$scratch/live"
    mkdir -p "$FAKE_GH_ROOT" "$FAKE_UPSTREAM"
    for edition in nasr-2026-10-29 waypoints-2026-10-29 cifp-2026-10-29 dtpp-2026-10-29 nasr-2026-11-26 waypoints-2026-11-26 cifp-2026-11-26 dtpp-2026-11-26; do
        post "$edition" "first posting"
    done
}
