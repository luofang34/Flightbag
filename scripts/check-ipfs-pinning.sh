#!/usr/bin/env bash
# Guards the IPFS side of the edition publisher without network access: `publish-editions.sh`
# and `ipfs-pin.sh` run against the fake `gh` and upstream of the publisher guard plus a fake
# Kubo RPC API, pinning service and gateway (`scripts/testing/fake_ipfs.py`), which derives
# CIDs on its own. Covered: CIDs in the manifests without any provider, their stability, the
# backfill of an edition published without them, pinning and the verified gateway, a provider
# that stores another CID, each way a gateway can fail its probe, the pinning service, the
# retention window, and an operator-pinned IPFS-only chart archive.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
scratch="$(mktemp -d)"
fake_pid=""
trap '[ -z "$fake_pid" ] || kill "$fake_pid" 2>/dev/null; rm -rf "$scratch"' EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# shellcheck source=scripts/testing/publisher-fakes.sh
source "$root/scripts/testing/publisher-fakes.sh"

for script in ipfs-pin.sh publish-editions.sh; do
    [ "$(wc -l <"$root/scripts/$script")" -le 500 ] || fail "scripts/$script outgrew 500 lines; split it"
done

# --- Fake provider -------------------------------------------------------------------------
provider="$scratch/provider"
start_provider() {
    [ -z "$fake_pid" ] || { kill "$fake_pid" 2>/dev/null; wait "$fake_pid" 2>/dev/null || true; }
    rm -rf "$provider"
    mkdir -p "$provider"
    # The server prints its port once it listens, so reading it is the readiness signal.
    exec 3< <(exec python3 "$root/scripts/testing/fake_ipfs.py" "$provider")
    fake_pid=$!
    read -r port <&3 || fail "fake IPFS provider did not start"
    base="http://127.0.0.1:$port"
    kubo_env=(IPFS_KUBO_RPC_URL="$base" IPFS_GATEWAY_URL="$base")
    psa_env=("${kubo_env[@]}" IPFS_PIN_SERVICE_URL="$base/psa" IPFS_PIN_SERVICE_TOKEN=test-token IPFS_PIN_POLL_S=0)
}
knob() { touch "$provider/knobs/$1"; }
unknob() { rm -f "$provider/knobs/$1"; }
calls() { grep -c "$1" "$provider/log" || true; }
expect_calls() { [ "$(calls "$1")" = "$2" ] || { cat "$scratch/run.log" "$provider/log" >&2 || true; fail "$3"; }; }
forget_calls() { : >"$provider/log"; }
# CID of a single-leaf file, derived apart from the code under test.
raw_cid() {
    python3 -c 'import base64, hashlib, sys
digest = hashlib.sha256(open(sys.argv[1], "rb").read()).digest()
print("b" + base64.b32encode(bytes([1, 0x55, 0x12, 0x20]) + digest).decode().lower().rstrip("="))' "$1"
}
run_ok() { run_publisher "$@" || { cat "$scratch/run.log" >&2; fail "publisher exited non-zero"; }; }

first="2026-11-24T04:00:00Z"
rel="$FAKE_GH_ROOT"
nasr_current='.editions.faa_nasr.current'

# --- No provider configured: CIDs only -----------------------------------------------------
start_provider
reset_world
run_ok "$first"
airspace_cid="$(raw_cid "$rel/airac-2026-10-29/airspace.pmtiles")"
expect_json "$site/cycles.json" "$nasr_current.ipfs_cid == \"$airspace_cid\"" "index must carry the artifact CID"
expect_json "$site/cycles.json" "$nasr_current | .ipfs_url == null and .ipfs_directory_cid != null and .release_url != null and .site_path != null" \
    "without a gateway the index lists a directory CID and no gateway URL"
expect_json "$site/manifest.json" '.schema_version == 2 and .ipfs.import == {cid_version: 1, multihash: "sha2-256", raw_leaves: true, chunker: "size-1048576", layout: "balanced", max_links: 174, directory: "basic"}' \
    "manifest must state the import parameters"
expect_json "$site/manifest.json" '[.entries[] | .ipfs_cid != null and .mirror_urls == [] and .storage == "release"] | all and length == 8' \
    "every published artifact must have a CID"
expect_json "$rel/airac-2026-10-29/airspace.pmtiles.cid.json" ".cid == \"$airspace_cid\" and .size_bytes == 30" "CID companion must be a Release asset"
[ -f "$site/cycles/2026-10-29/airspace.pmtiles.cid.json" ] || fail "CID companion must be on the site"
[ ! -s "$provider/requests" ] || fail "an unconfigured publisher contacted a provider"
deploy_if_staged
cids_before="$(jq -c '[.ipfs, (.entries[] | .ipfs_cid)]' "$site/manifest.json")"

run_ok "2026-11-24T07:00:00Z"
expect_output 'deploy=false' "idle run"
[ ! -s "$FAKE_LOG" ] || { cat "$FAKE_LOG" >&2; fail "idle run must not write a Release"; }

# A Release that lacks a companion gets one without the edition being rebuilt.
rm "$rel/airac-2026-10-29/airspace.pmtiles.cid.json"
run_ok "2026-11-24T08:00:00Z"
expect_log '^build ' 0 "a missing companion must not trigger a rebuild"
expect_log '^upload airac-2026-10-29 airspace.pmtiles.cid.json$' 1 "the companion must be backfilled"
expect_output 'deploy=false' "backfill run"
[ "$(jq -c '[.ipfs, (.entries[] | .ipfs_cid)]' "$scratch/work/manifest.json")" = "$cids_before" ] || fail "CIDs changed across reruns"

# A gateway or a service URL alone, without credentials or a node, pins nothing.
run_ok "2026-11-24T09:00:00Z" IPFS_GATEWAY_URL="$base" IPFS_PIN_SERVICE_URL="$base/psa" IPFS_BIN="$scratch/no-such-ipfs"
[ ! -s "$provider/requests" ] || fail "a publisher without credentials contacted a provider"
expect_output 'deploy=false' "run without credentials"

# Dry run pins nothing either.
run_publisher "$first" DRY_RUN=true "${psa_env[@]}" || true
[ ! -s "$provider/log" ] || fail "dry run changed provider state"

# A spent time budget leaves pinning to the next run and the editions published.
run_ok "2026-11-24T09:30:00Z" "${kubo_env[@]}" IPFS_RUN_BUDGET_S=0
[ ! -s "$provider/log" ] || fail "a run without pinning budget changed provider state"
grep -q 'time budget' "$scratch/run.log" || fail "a spent pinning budget must be reported"
expect_output 'required_failed=false' "run without pinning budget"

# --- Kubo node and gateway -----------------------------------------------------------------
run_ok "2026-11-24T10:00:00Z" "${kubo_env[@]}"
expect_calls '^kubo add flightbag-airac-' 8 "all artifacts of both cycles must be uploaded"
expect_calls "^kubo add flightbag-airac-2026-10-29/airspace.pmtiles $airspace_cid\$" 1 "the node must store the computed CID"
expect_calls '^kubo pin flightbag-airac-2026-1' 2 "each cycle directory must be pinned"
directory="$(jq -r "$nasr_current.ipfs_directory_cid" "$site/cycles.json")"
expect_calls "^kubo pin flightbag-airac-2026-10-29 $directory\$" 1 "the pinned directory must be the listed one"
expect_json "$rel/airac-2026-10-29/ipfs-pin.json" ".directory_cid == \"$directory\" and .gateway == \"$base\" and .providers == [\"kubo\"]" \
    "a verified gateway must be recorded on the Release"
expect_json "$site/cycles.json" "$nasr_current.ipfs_url == \"$base/ipfs/$directory/airspace.pmtiles\"" "index must list the verified gateway"
expect_json "$site/manifest.json" ".entries[] | select(.file == \"airspace.pmtiles\" and .effective_date == \"2026-10-29\") | .mirror_urls == [\"$base/ipfs/$directory/airspace.pmtiles\"] and (.url | startswith(\"https://github.com/\"))" \
    "a Release artifact keeps its URL and gains the gateway as a mirror"
curl -fsS "$(jq -r "$nasr_current.ipfs_url" "$site/cycles.json")" | cmp -s - "$rel/airac-2026-10-29/airspace.pmtiles" \
    || fail "the listed gateway URL must serve the artifact"
expect_output 'deploy=true' "first pinned run"
expect_output 'required_failed=false' "first pinned run"
deploy_if_staged

forget_calls
run_ok "2026-11-24T13:00:00Z" "${kubo_env[@]}"
[ ! -s "$provider/log" ] && [ ! -s "$FAKE_LOG" ] || fail "an idle run must not pin or write again"
expect_output 'deploy=false' "idle pinned run"

# A recorded gateway that later fails is reported; the published index does not flap with it.
knob gateway-down
run_ok "2026-11-24T16:00:00Z" "${kubo_env[@]}"
grep -q 'fails its probe' "$scratch/run.log" || fail "a failing recorded gateway must be reported"
[ ! -s "$provider/log" ] || fail "a gateway outage must not cause uploads while the node holds the cycle"
expect_output 'deploy=false' "run with a failing recorded gateway"
expect_output 'required_failed=false' "run with a failing recorded gateway"
unknob gateway-down

# A provider that dropped a recorded cycle gets it again, without touching the Release.
for pinned in "$directory" "$airspace_cid" "$(raw_cid "$rel/airac-2026-10-29/nasr_waypoints.bin")" "$(raw_cid "$rel/airac-2026-10-29/cifp_procedures.bin")" "$(raw_cid "$rel/airac-2026-10-29/dtpp_plates.bin")"; do
    curl -fsS -X POST "$base/api/v0/pin/rm?arg=$pinned" >/dev/null
done
forget_calls
run_ok "2026-11-24T19:00:00Z" "${kubo_env[@]}"
expect_calls '^kubo add flightbag-airac-2026-10-29/' 4 "a dropped cycle must be uploaded again"
expect_calls "^kubo pin flightbag-airac-2026-10-29 $directory\$" 1 "a dropped cycle must be pinned again"
[ ! -s "$FAKE_LOG" ] || { cat "$FAKE_LOG" >&2; fail "restoring a dropped cycle must not write a Release"; }
expect_output 'deploy=false' "run restoring a dropped cycle"
curl -fsS "$(jq -r "$nasr_current.ipfs_url" "$site/cycles.json")" | cmp -s - "$rel/airac-2026-10-29/airspace.pmtiles" \
    || fail "the gateway must serve a restored cycle"

# --- Retention window ----------------------------------------------------------------------
# Pins of other content, and an old-looking pin of a CID that is still listed, must survive.
curl -fsS -X POST -F "file=@$root/LICENSE" "$base/api/v0/add?cid-version=1&raw-leaves=true&chunker=size-1048576&pin=true&pin-name=flightbag-study-licence" >/dev/null
curl -fsS -X POST "$base/api/v0/pin/rm?arg=$(raw_cid "$rel/airac-2026-11-26/cifp_procedures.bin")" >/dev/null
curl -fsS -X POST -F "file=@$rel/airac-2026-11-26/cifp_procedures.bin" "$base/api/v0/add?cid-version=1&raw-leaves=true&chunker=size-1048576&pin=true&pin-name=flightbag-airac-2026-01-01/still-listed" >/dev/null
for edition in nasr-2026-12-24 waypoints-2026-12-24 cifp-2026-12-24 dtpp-2026-12-24 nasr-2027-01-21 waypoints-2027-01-21 cifp-2027-01-21 dtpp-2027-01-21; do post "$edition" "first posting"; done

forget_calls
run_ok "2026-11-26T09:01:00Z" "${psa_env[@]}"
expect_calls ' unpin ' 0 "the previous cycle must stay pinned"
expect_calls '^psa pin flightbag-airac-' 3 "the service must be asked for each cycle directory"
expect_calls '^kubo connect /ip4/203.0.113.9/' 3 "the node must dial the service's delegates"
expect_json "$rel/airac-2026-11-26/ipfs-pin.json" '.providers == ["kubo", "pinning_service"]' "the record must name both providers"
deploy_if_staged

# While the deployed manifest cannot be read, nothing is removed.
forget_calls
run_ok "2026-12-24T09:01:00Z" "${psa_env[@]}" LIVE_INDEX_URL="file://$scratch/absent/cycles.json"
expect_calls ' unpin ' 0 "nothing may be unpinned while the deployed manifest is unknown"

forget_calls
run_ok "2026-12-24T09:02:00Z" "${psa_env[@]}"
expect_calls '^kubo unpin flightbag-airac-2026-10-29' 5 "the cycle before the previous one must be unpinned on the node"
expect_calls '^psa unpin flightbag-airac-2026-10-29 ' 1 "the cycle before the previous one must be unpinned at the service"
expect_calls ' unpin ' 6 "only the expired cycle may be unpinned"
kept="$(curl -fsS -X POST "$base/api/v0/pin/ls?type=recursive&names=true&stream=true" | jq -r '.Name' | sort | tr '\n' ' ')"
for name in flightbag-study-licence flightbag-airac-2026-01-01/still-listed flightbag-airac-2026-11-26 flightbag-airac-2026-12-24; do
    [[ " $kept" == *" $name "* ]] || fail "$name must stay pinned (kept: $kept)"
done
deploy_if_staged

# Once neither manifest lists the previous cycle, the window alone keeps it pinned; the
# old-looking pin of a CID that is listed nowhere any more goes.
forget_calls
run_ok "2026-12-24T12:00:00Z" "${psa_env[@]}"
expect_calls '^kubo unpin flightbag-airac-2026-01-01/still-listed ' 1 "an expired pin nobody lists must be unpinned"
expect_calls ' unpin ' 1 "the previous cycle must stay pinned after it left the manifests"
kept="$(curl -fsS -X POST "$base/api/v0/pin/ls?type=recursive&names=true&stream=true" | jq -r '.Name' | sort | tr '\n' ' ')"
for name in flightbag-study-licence flightbag-airac-2026-11-26 flightbag-airac-2026-11-26/airspace.pmtiles flightbag-airac-2026-12-24; do
    [[ " $kept" == *" $name "* ]] || fail "$name must stay pinned (kept: $kept)"
done

# --- A provider that stores another CID ----------------------------------------------------
start_provider
reset_world
knob wrong-cid
run_ok "$first" "${kubo_env[@]}"
expect_output 'required_failed=true' "a provider storing another CID"
grep -q '::error::IPFS: the node imported 2026-10-29/' "$scratch/run.log" || fail "a CID mismatch must be an error"
[ ! -e "$rel/airac-2026-10-29/ipfs-pin.json" ] || fail "a mismatching provider must not be recorded"
expect_calls '^kubo pin ' 0 "a mismatching upload must not be pinned as the cycle"
expect_json "$site/cycles.json" "$nasr_current | .ipfs_cid == \"$(raw_cid "$rel/airac-2026-10-29/airspace.pmtiles")\" and .ipfs_url == null" \
    "the manifest must keep the computed CID and list no gateway"
unknob wrong-cid

# --- Gateway probe failures ----------------------------------------------------------------
for fault in gateway-200 gateway-no-cors gateway-bad-range gateway-down; do
    start_provider
    reset_world
    knob "$fault"
    run_ok "$first" "${kubo_env[@]}"
    expect_calls '^kubo pin ' 2 "$fault: the cycles must still be pinned"
    [ ! -e "$rel/airac-2026-10-29/ipfs-pin.json" ] || fail "$fault: an unverified gateway must not be recorded"
    expect_json "$site/cycles.json" "$nasr_current | .ipfs_url == null and .ipfs_cid != null" "$fault: an unverified gateway must not be listed"
    expect_output 'required_failed=false' "$fault"
    expect_output 'deploy=true' "$fault"
    deploy_if_staged
    unknob "$fault"
    forget_calls
    run_ok "2026-11-24T07:00:00Z" "${kubo_env[@]}"
    [ ! -s "$provider/log" ] || fail "$fault: a pinned cycle must not be uploaded again"
    expect_json "$site/cycles.json" "$nasr_current.ipfs_url != null" "$fault: the gateway must be listed once it passes"
done

# --- Pinning service faults ----------------------------------------------------------------
start_provider
reset_world
knob psa-stuck
run_ok "$first" "${psa_env[@]}" IPFS_PIN_TIMEOUT_S=0
expect_calls '^psa pin ' 2 "the service must be asked once per cycle"
[ ! -e "$rel/airac-2026-10-29/ipfs-pin.json" ] || fail "a pin the service has not completed must not be recorded"
expect_output 'required_failed=false' "a slow pinning service"
unknob psa-stuck
forget_calls
run_ok "2026-11-24T07:00:00Z" "${psa_env[@]}"
expect_calls '^psa pin ' 0 "a pending request must be awaited, not repeated"
expect_json "$rel/airac-2026-10-29/ipfs-pin.json" '.providers == ["kubo", "pinning_service"]' "a completed pin must be recorded"

start_provider
reset_world
knob psa-wrong-cid
run_ok "$first" "${psa_env[@]}"
expect_output 'required_failed=true' "a service answering for another CID"
[ ! -e "$rel/airac-2026-10-29/ipfs-pin.json" ] || fail "a service answering for another CID must not be recorded"

# --- Operator-pinned IPFS-only chart archive -----------------------------------------------
start_provider
reset_world
run_ok "$first" "${kubo_env[@]}"
directory_before="$(jq -r "$nasr_current.ipfs_directory_cid" "$site/cycles.json")"

charts="$scratch/charts/2026-10-29"
mkdir -p "$charts"
printf 'sectional raster archive\n' >"$charts/vfr_sectional.pmtiles"
chart_sha="$(python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$charts/vfr_sectional.pmtiles")"
printf '%s  vfr_sectional.pmtiles\n' "$chart_sha" >"$charts/vfr_sectional.pmtiles.sha256"
"$real_bin" plan --now "$first" | jq --arg sha "$chart_sha" '.faa_charts_56_day.current | {
    schema_version: 1, disclaimer: "test", edition_id: "test-chart", authority: "faa_vfr_sectional",
    airac_code, cycle_index, effective_date, expires_date, effective_from_utc, effective_until_utc,
    upstream_url: "https://upstream.invalid", upstream_sha256_hex: $sha,
    chart_pmtiles: {file: "vfr_sectional.pmtiles", size_bytes: 25, sha256_hex: $sha, tile_count: 1,
        min_zoom: 0, max_zoom: 11, bounds: [-180, -15, 180, 82]}}' >"$charts/chart_faa_vfr_sectional.json"
forget_calls
env FLIGHTBAG_BIN="$real_bin" GH_BIN="$scratch/gh.sh" REPO_SLUG=example/flightbag RELEASE_UPLOAD=true "${kubo_env[@]}" \
    "$root/scripts/ipfs-pin.sh" "$charts" >"$scratch/pin.json" 2>"$scratch/run.log" || { cat "$scratch/run.log" >&2; fail "pinning a chart directory failed"; }
chart_cid="$(raw_cid "$charts/vfr_sectional.pmtiles")"
expect_calls "^kubo add flightbag-airac-2026-10-29/vfr_sectional.pmtiles $chart_cid\$" 1 "the operator run must upload the chart archive"
[ -f "$rel/airac-2026-10-29/chart_faa_vfr_sectional.json" ] && [ -f "$rel/airac-2026-10-29/vfr_sectional.pmtiles.cid.json" ] \
    || fail "the chart manifest and companion must reach the Release"
[ ! -e "$rel/airac-2026-10-29/vfr_sectional.pmtiles" ] || fail "an IPFS-only archive must not become a Release asset"

sectional='.editions.faa_vfr_sectional.current'
forget_calls
run_ok "2026-11-24T07:00:00Z" "${kubo_env[@]}"
expect_calls '^kubo add ' 0 "the publisher must not need the bytes of an archive the node holds"
directory="$(jq -r "$sectional.ipfs_directory_cid" "$site/cycles.json")"
[ "$directory" != "$directory_before" ] || fail "the cycle directory must include the chart archive"
expect_json "$site/cycles.json" "$sectional | .ipfs_cid == \"$chart_cid\" and .release_url == null and .site_path == null and .ipfs_url == \"$base/ipfs/$directory/vfr_sectional.pmtiles\"" \
    "an IPFS-only archive is listed at the gateway alone"
expect_json "$site/manifest.json" ".entries[] | select(.file == \"vfr_sectional.pmtiles\") | .storage == \"ipfs\" and .url == \"$base/ipfs/$directory/vfr_sectional.pmtiles\"" \
    "the gateway is the primary URL of an IPFS-only archive"
curl -fsS "$(jq -r "$sectional.ipfs_url" "$site/cycles.json")" | cmp -s - "$charts/vfr_sectional.pmtiles" || fail "the gateway must serve the chart archive"
[ ! -e "$site/cycles/2026-10-29/vfr_sectional.pmtiles" ] || fail "an IPFS-only archive must not be staged on the site"
grep -q 'IPFS gateway' "$site/index.html" || fail "the site page must link the gateway"
deploy_if_staged

# The chart edition outlives the NASR edition of the same date: it stays listed after the
# NASR cutover, under the same directory.
run_ok "2026-11-26T09:01:00Z" "${kubo_env[@]}"
expect_json "$site/cycles.json" "$sectional.ipfs_directory_cid == \"$directory\" and .current.effective_date == \"2026-11-26\"" \
    "a 56-day edition must stay listed, under an unchanged directory, across a 28-day cutover"

# Without a provider the archive is still listed, at its content address.
run_ok "2026-11-26T12:00:00Z"
expect_json "$scratch/work/manifest.json" ".entries[] | select(.file == \"vfr_sectional.pmtiles\") | .url == \"ipfs://$directory/vfr_sectional.pmtiles\"" \
    "without a verified gateway an IPFS-only archive is listed by content address"

echo "ipfs-pinning checks passed"
