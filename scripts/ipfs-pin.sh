#!/usr/bin/env bash
# Pins the artifacts of a cycle on IPFS and records which gateway serves them.
#
# Sourced by `publish-editions.sh`; run directly to pin a locally built cycle directory:
#
#   IPFS_KUBO_RPC_URL=http://127.0.0.1:5001 IPFS_GATEWAY_URL=http://127.0.0.1:8080 \
#     scripts/ipfs-pin.sh dist/cycles/2026-10-29
#
# A direct run identifies every artifact that has a `.sha256` companion, pins the directory
# they form and, with RELEASE_UPLOAD=true, uploads the manifest, `.sha256` and `.cid.json` of
# each IPFS-only product to the cycle's Release so that the scheduled publisher lists it.
#
# The CID of every artifact and of the cycle directory is computed locally (`flightbag
# ipfs-cid`, `flightbag ipfs-dir`). A provider is only ever asked to store those CIDs: when it
# reports another CID for the same upload, that is an error and nothing is recorded. Every
# other provider or gateway failure is a warning, because the next run repeats the work, and
# pinning gives up for the run once its time budget is spent, so a stuck provider cannot keep
# the editions themselves from being published.
#
# Environment (all optional; with neither a Kubo node nor a pinning service nothing is pinned):
#   IPFS_KUBO_RPC_URL       Kubo RPC API that imports and pins, e.g. http://127.0.0.1:5001
#   IPFS_KUBO_RPC_TOKEN     bearer token of that API
#   IPFS_PIN_SERVICE_URL    IPFS Pinning Service API endpoint, e.g. https://api.filebase.io/v1/ipfs
#   IPFS_PIN_SERVICE_TOKEN  bearer token of that endpoint
#   IPFS_GATEWAY_URL        gateway whose /ipfs/<cid>/<file> URLs are probed and then published
#   IPFS_BIN                Kubo binary for a throwaway node (default ipfs). A pinning service
#                           stores CIDs, not uploads, so it needs a node to fetch the blocks
#                           from; without IPFS_KUBO_RPC_URL one is started for the run.
#   IPFS_PIN_TIMEOUT_S      longest wait for one upload or pin to complete (default 300)
#   IPFS_RUN_BUDGET_S       pinning starts no further cycle after this many seconds (default 900)
#   IPFS_PIN_POLL_S         pause between pinning-service status polls (default 5)
#   IPFS_PROBE_BYTES        length of each probed byte range (default 16384)

IPFS_PIN_PREFIX="flightbag-airac-"
IPFS_PIN_RECORD="ipfs-pin.json"
IPFS_PIN_TIMEOUT_S="${IPFS_PIN_TIMEOUT_S:-300}"
IPFS_RUN_BUDGET_S="${IPFS_RUN_BUDGET_S:-900}"
IPFS_PIN_POLL_S="${IPFS_PIN_POLL_S:-5}"
IPFS_PROBE_BYTES="${IPFS_PROBE_BYTES:-16384}"
IPFS_GATEWAY_URL="${IPFS_GATEWAY_URL:-}"
IPFS_GATEWAY_URL="${IPFS_GATEWAY_URL%/}"
IPFS_EPHEMERAL_PID=""
IPFS_SCRATCH=""
IPFS_STARTED_S=0

ipfs_pinning_configured() { [ -n "${IPFS_KUBO_RPC_URL:-}" ] || [ -n "${IPFS_PIN_SERVICE_URL:-}" ]; }

# --- Kubo RPC ------------------------------------------------------------------------------
kubo() {
  local path="$1" auth=()
  shift
  [ -z "${IPFS_KUBO_RPC_TOKEN:-}" ] || auth=(-H "Authorization: Bearer $IPFS_KUBO_RPC_TOKEN")
  curl -sS --fail-with-body -X POST ${auth[@]+"${auth[@]}"} "$@" "${IPFS_KUBO_RPC_URL%/}/api/v0/$path"
}

kubo_pinned() { kubo "pin/ls?arg=$1&type=recursive" --max-time 60 >/dev/null 2>&1; }

# Every import parameter is explicit: the node's defaults must not decide the CID.
kubo_add() {
  local path="$1" pin_name="$2"
  kubo "add?cid-version=1&raw-leaves=true&chunker=size-1048576&trickle=false&hash=sha2-256&pin=true&progress=false&pin-name=$(jq -rn --arg n "$pin_name" '$n|@uri')" \
    --max-time "$IPFS_PIN_TIMEOUT_S" -F "file=@$path;filename=$(basename "$path")" | jq -rs 'map(select(.Hash != null)) | last | .Hash'
}

# A pinning service fetches blocks from a peer, so a run without a node of its own starts one
# that lives exactly as long as the run.
ipfs_start_ephemeral_node() {
  local ipfs="${IPFS_BIN:-ipfs}" repo="$IPFS_SCRATCH/node" waited=0 api
  if ! command -v "$ipfs" >/dev/null 2>&1; then
    warn "IPFS: the pinning service needs a node to fetch from; set IPFS_KUBO_RPC_URL or install Kubo"
    return 1
  fi
  mkdir -p "$repo"
  IPFS_PATH="$repo" "$ipfs" init --empty-repo >/dev/null 2>&1 || return 1
  IPFS_PATH="$repo" "$ipfs" config Addresses.API /ip4/127.0.0.1/tcp/0 || return 1
  IPFS_PATH="$repo" "$ipfs" config Addresses.Gateway /ip4/127.0.0.1/tcp/0 || return 1
  IPFS_PATH="$repo" "$ipfs" config --json Addresses.Swarm \
    '["/ip4/0.0.0.0/tcp/0","/ip4/0.0.0.0/udp/0/quic-v1"]' || return 1
  IPFS_PATH="$repo" "$ipfs" daemon >"$repo/daemon.log" 2>&1 &
  IPFS_EPHEMERAL_PID=$!
  while [ "$waited" -lt 60 ]; do
    api="$(sed -n 's|^/ip4/\([0-9.]*\)/tcp/\([0-9]*\).*|http://\1:\2|p' "$repo/api" 2>/dev/null || true)"
    if [ -n "$api" ] && IPFS_KUBO_RPC_URL="$api" kubo id --max-time 5 >/dev/null 2>&1; then
      IPFS_KUBO_RPC_URL="$api"
      log "IPFS: started a throwaway node at $api"
      return 0
    fi
    kill -0 "$IPFS_EPHEMERAL_PID" 2>/dev/null || break
    sleep 1
    waited=$((waited + 1))
  done
  warn "IPFS: the throwaway node did not come up: $(tail -n 1 "$repo/daemon.log" 2>/dev/null || true)"
  ipfs_stop_ephemeral_node
  return 1
}

ipfs_stop_ephemeral_node() {
  if [ -n "$IPFS_EPHEMERAL_PID" ]; then
    kill "$IPFS_EPHEMERAL_PID" 2>/dev/null || true
    wait "$IPFS_EPHEMERAL_PID" 2>/dev/null || true
    IPFS_EPHEMERAL_PID=""
    IPFS_KUBO_RPC_URL=""
  fi
}

# Stops the throwaway node and removes it together with the probe scratch files.
ipfs_teardown() {
  ipfs_stop_ephemeral_node
  if [ -n "$IPFS_SCRATCH" ]; then
    rm -rf "$IPFS_SCRATCH"
    IPFS_SCRATCH=""
  fi
}

# Decides once per run what can be used; a pinning service without a node is switched off.
ipfs_setup() {
  IPFS_STARTED_S="$SECONDS"
  ipfs_pinning_configured || return 0
  if [ "$DRY_RUN" = "true" ]; then
    log "IPFS: dry run, nothing is pinned"
    IPFS_KUBO_RPC_URL="" IPFS_PIN_SERVICE_URL=""
    return 0
  fi
  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ] && [ -z "${IPFS_PIN_SERVICE_TOKEN:-}" ]; then
    warn "IPFS: IPFS_PIN_SERVICE_URL is set without IPFS_PIN_SERVICE_TOKEN; not using the service"
    IPFS_PIN_SERVICE_URL=""
  fi
  IPFS_SCRATCH="$(mktemp -d)"
  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ] && [ -z "${IPFS_KUBO_RPC_URL:-}" ]; then
    ipfs_start_ephemeral_node || IPFS_PIN_SERVICE_URL=""
  elif [ -n "${IPFS_KUBO_RPC_URL:-}" ] && ! kubo id --max-time 30 >/dev/null 2>&1; then
    warn "IPFS: the Kubo RPC API does not answer; nothing is pinned in this run"
    IPFS_KUBO_RPC_URL="" IPFS_PIN_SERVICE_URL=""
  fi
}

ipfs_providers() {
  local providers=()
  # A throwaway node holds nothing once the run ends, so it does not count as a provider.
  if [ -n "${IPFS_KUBO_RPC_URL:-}" ] && [ -z "$IPFS_EPHEMERAL_PID" ]; then providers+=("kubo"); fi
  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ]; then providers+=("pinning_service"); fi
  jq -cn '$ARGS.positional' --args ${providers[@]+"${providers[@]}"}
}

# --- Pinning Service API -------------------------------------------------------------------
psa() {
  local method="$1" path="$2"
  shift 2
  curl -sS --fail-with-body --max-time 120 -X "$method" \
    -H "Authorization: Bearer $IPFS_PIN_SERVICE_TOKEN" -H "Content-Type: application/json" \
    "$@" "${IPFS_PIN_SERVICE_URL%/}$path"
}

psa_holds() {
  psa GET "/pins?cid=$1&status=pinned&limit=1" 2>/dev/null | jq -e --arg cid "$1" '.results[0].pin.cid == $cid' >/dev/null
}

# Asks the service to pin `cid`, fetching it from the node, and waits until it holds it.
# Returns 2 when the service answers for another CID, 1 on any other failure.
psa_pin() {
  local cid="$1" name="$2" status request origins deadline delegate
  status="$(psa GET "/pins?cid=$cid&status=queued,pinning,pinned&limit=1")" || return 1
  request="$(jq -c '.results[0] // empty' <<<"$status")"
  if [ -z "$request" ]; then
    origins="$(kubo id --max-time 30 | jq -c '[.Addresses[]? | select(test("/(127\\.0\\.0\\.1|::1)/") | not)] | .[:20]')" || return 1
    request="$(jq -cn --arg cid "$cid" --arg name "$name" --argjson origins "$origins" \
      '{cid: $cid, name: $name, origins: $origins}' | psa POST /pins --data-binary @-)" || return 1
    # The service reaches a node behind NAT only through a connection the node opens.
    while IFS= read -r delegate; do
      kubo "swarm/connect?arg=$(jq -rn --arg a "$delegate" '$a|@uri')" --max-time 30 >/dev/null 2>&1 || true
    done < <(jq -r '.delegates[]?' <<<"$request")
  fi
  deadline=$((SECONDS + IPFS_PIN_TIMEOUT_S))
  while :; do
    if [ "$(jq -r '.pin.cid' <<<"$request")" != "$cid" ]; then
      problem current "IPFS: the pinning service answered for $(jq -r '.pin.cid' <<<"$request"), not $cid"
      return 2
    fi
    case "$(jq -r '.status' <<<"$request")" in
      pinned) return 0 ;;
      failed)
        warn "IPFS: the pinning service failed to pin $name ($cid)"
        return 1
        ;;
    esac
    if [ "$SECONDS" -ge "$deadline" ]; then
      warn "IPFS: the pinning service is still fetching $name ($cid); the next run checks again"
      return 1
    fi
    sleep "$IPFS_PIN_POLL_S"
    request="$(psa GET "/pins/$(jq -r '.requestid' <<<"$request")")" || return 1
  done
}

# --- Gateway -------------------------------------------------------------------------------
# A browser reads an archive with cross-origin range requests, so a gateway is only listed
# once it answers them correctly at the start of the file and deep inside it.
gateway_probe() {
  local url="$1" size="$2" local_copy="$3" scratch="$4"
  local last=$((size - 1)) range start end length code
  [ "$size" -gt 0 ] || return 0
  local head_end=$((IPFS_PROBE_BYTES - 1)) tail_start=$((size - IPFS_PROBE_BYTES))
  [ "$head_end" -le "$last" ] || head_end="$last"
  [ "$tail_start" -ge 0 ] || tail_start=0
  for range in "0-$head_end" "$tail_start-$last"; do
    start="${range%-*}" end="${range#*-}"
    length=$((end - start + 1))
    code="$(curl -sS --max-time 120 -o "$scratch/probe.body" -D "$scratch/probe.headers" \
      -H "Range: bytes=$range" -H "Origin: https://flightbag.invalid" -w '%{http_code}' "$url" || true)"
    tr -d '\r' <"$scratch/probe.headers" >"$scratch/probe.clean"
    if [ "$code" != "206" ]; then
      warn "IPFS: $url answered $code to bytes=$range, expected 206"
    elif ! grep -Eiq '^access-control-allow-origin: *(\*|https://flightbag\.invalid) *$' "$scratch/probe.clean"; then
      warn "IPFS: $url sends no Access-Control-Allow-Origin"
    elif ! grep -Eiq "^content-range: *bytes $range/$size *\$" "$scratch/probe.clean"; then
      warn "IPFS: $url did not answer Content-Range: bytes $range/$size"
    elif [ "$(wc -c <"$scratch/probe.body" | tr -d ' ')" != "$length" ]; then
      warn "IPFS: $url returned a body of the wrong length for bytes=$range"
    elif [ -n "$local_copy" ] && ! cmp -s <(tail -c "+$((start + 1))" "$local_copy" | head -c "$length") "$scratch/probe.body"; then
      warn "IPFS: $url returned other bytes than the artifact for bytes=$range"
    else
      continue
    fi
    return 1
  done
}

# Bytes are compared wherever this run handled a copy of the artifact (`$scratch/local`).
gateway_probe_cycle() {
  local report="$1" scratch="$2" file size local_copy
  while IFS=$'\t' read -r file size; do
    local_copy=""
    [ ! -f "$scratch/local/$file" ] || local_copy="$scratch/local/$file"
    gateway_probe "$IPFS_GATEWAY_URL/ipfs/$(jq -r '.directory_cid' <<<"$report")/$file" "$size" \
      "$local_copy" "$scratch" || return 1
  done < <(jq -r '.files[] | [.file, .size_bytes] | @tsv' <<<"$report")
}

# --- One cycle -----------------------------------------------------------------------------
# Imports what the node lacks, then pins the directory that links every artifact. Returns 2
# when the node reports another CID than the one computed here, 1 on any other failure.
kubo_pin_cycle() {
  local date="$1" report="$2" scratch="$3"
  local directory file cid path got complete=true
  directory="$(jq -r '.directory_cid' <<<"$report")"
  kubo_pinned "$directory" && return 0
  while IFS=$'\t' read -r file cid; do
    kubo_pinned "$cid" && continue
    if ! path="$(ipfs_artifact_path "$date" "$file")"; then
      warn "IPFS: $date/$file ($cid) is neither on the node nor available to upload"
      complete=false
      continue
    fi
    ln -sf "$path" "$scratch/local/$file"
    log "IPFS: uploading $date/$file"
    got="$(kubo_add "$path" "$IPFS_PIN_PREFIX$date/$file")" || {
      warn "IPFS: uploading $date/$file failed"
      return 1
    }
    if [ "$got" != "$cid" ]; then
      problem current "IPFS: the node imported $date/$file as $got, expected $cid"
      return 2
    fi
  done < <(jq -r '.files[] | [.file, .cid] | @tsv' <<<"$report")

  got="$(kubo "block/put?cid-codec=dag-pb&mhtype=sha2-256&pin=false" --max-time 60 \
    -F "data=@$scratch/directory.block" | jq -r '.Key')" || {
    warn "IPFS: storing the directory of $date failed"
    return 1
  }
  if [ "$got" != "$directory" ]; then
    problem current "IPFS: the node stored the directory of $date as $got, expected $directory"
    return 2
  fi
  # A recursive pin of a directory with a missing child would wait for the network to supply it.
  [ "$complete" = "true" ] || return 1
  kubo "pin/add?arg=$directory&recursive=true&name=$IPFS_PIN_PREFIX$date" \
    --max-time "$IPFS_PIN_TIMEOUT_S" >/dev/null || {
    warn "IPFS: pinning the directory of $date failed"
    return 1
  }
}

# Pins the cycle in `dir` and, once the gateway serves it, records that in the cycle's pin
# record. The record is what lets a manifest list the gateway: probing on every run instead
# would make the published index depend on the gateway's mood.
ipfs_pin_cycle() {
  local date="$1" dir="$2"
  local scratch="$IPFS_SCRATCH/$date" report directory providers recorded outcome on_node=false at_service=false
  ipfs_pinning_configured || return 0
  if [ "$((SECONDS - IPFS_STARTED_S))" -ge "$IPFS_RUN_BUDGET_S" ]; then
    warn "IPFS: the time budget of this run is spent; $date is left to the next run"
    return 0
  fi
  mkdir -p "$scratch/local"
  report="$("$BIN" ipfs-dir --cycle-dir "$dir" --block-out "$scratch/directory.block")" || return 0
  directory="$(jq -r '.directory_cid // empty' <<<"$report")"
  [ -n "$directory" ] || return 0
  providers="$(ipfs_providers)"

  recorded="$(jq -c '{directory_cid, gateway, providers}' "$dir/$IPFS_PIN_RECORD" 2>/dev/null || true)"
  if [ -n "$IPFS_GATEWAY_URL" ] && [ "$recorded" = "$(jq -cn --arg d "$directory" \
    --arg g "$IPFS_GATEWAY_URL" --argjson p "$providers" '{directory_cid: $d, gateway: $g, providers: $p}')" ]; then
    gateway_probe_cycle "$report" "$scratch" \
      || warn "IPFS: the gateway recorded for $date ($directory) fails its probe"
    return 0
  fi

  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ] && psa_holds "$directory"; then at_service=true; fi
  # A throwaway node is only a source for the service, so it is not filled for a cycle the
  # service already holds.
  if [ -n "${IPFS_KUBO_RPC_URL:-}" ] && { [ -z "$IPFS_EPHEMERAL_PID" ] || [ "$at_service" = "false" ]; }; then
    outcome=0
    kubo_pin_cycle "$date" "$report" "$scratch" || outcome=$?
    [ "$outcome" -ne 2 ] || return 0
    if [ "$outcome" -eq 0 ] && [ -z "$IPFS_EPHEMERAL_PID" ]; then on_node=true; fi
  fi
  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ] && [ "$at_service" = "false" ]; then
    psa_pin "$directory" "$IPFS_PIN_PREFIX$date" || return 0
    at_service=true
  fi
  [ "$on_node" = "true" ] || [ "$at_service" = "true" ] || return 0
  log "IPFS: $date is pinned as $directory"
  [ -n "$IPFS_GATEWAY_URL" ] || return 0
  if ! gateway_probe_cycle "$report" "$scratch"; then
    warn "IPFS: $date is pinned but the gateway does not serve it yet; it is not listed"
    return 0
  fi
  jq -n --arg date "$date" --arg d "$directory" --arg g "$IPFS_GATEWAY_URL" --argjson p "$providers" \
    '{schema_version: 1, effective_date: $date, directory_cid: $d, gateway: $g, providers: $p}' \
    >"$dir/$IPFS_PIN_RECORD"
  ipfs_record_published "$date" "$dir/$IPFS_PIN_RECORD" \
    || warn "IPFS: publishing the pin record of $date failed"
}

# --- Retention -----------------------------------------------------------------------------
# Removes this publisher's pins of cycles older than `oldest_kept` (YYYY-MM-DD). `keep_file`
# lists CIDs that stay whatever their age; a pin is recognised by its exact name shape, so a
# node shared with other content is never touched beyond what was pinned here.
ipfs_unpin_expired() {
  local oldest_kept="$1" keep_file="$2" cid name id
  expired() {
    [[ "$2" =~ ^${IPFS_PIN_PREFIX}([0-9]{4}-[0-9]{2}-[0-9]{2})(/.+)?$ ]] || return 1
    [[ "${BASH_REMATCH[1]}" < "$oldest_kept" ]] && ! grep -qxF -- "$1" "$keep_file"
  }
  if [ -n "${IPFS_KUBO_RPC_URL:-}" ] && [ -z "$IPFS_EPHEMERAL_PID" ]; then
    while IFS=$'\t' read -r cid name; do
      expired "$cid" "$name" || continue
      log "IPFS: unpinning $name ($cid)"
      kubo "pin/rm?arg=$cid&recursive=true" --max-time 120 >/dev/null || warn "IPFS: unpinning $name failed"
    done < <(kubo "pin/ls?type=recursive&names=true&stream=true&name=$IPFS_PIN_PREFIX" --max-time 300 \
      | jq -r 'if .Keys then (.Keys | to_entries[] | [.key, .value.Name]) else [.Cid, .Name] end | @tsv' || true)
  fi
  if [ -n "${IPFS_PIN_SERVICE_URL:-}" ]; then
    while IFS=$'\t' read -r cid name id; do
      expired "$cid" "$name" || continue
      log "IPFS: unpinning $name ($cid) at the pinning service"
      psa DELETE "/pins/$id" >/dev/null || warn "IPFS: unpinning $name at the pinning service failed"
    done < <(psa GET "/pins?name=$IPFS_PIN_PREFIX&match=partial&status=queued,pinning,pinned,failed&limit=1000" \
      | jq -r '.results[]? | [.pin.cid, .pin.name, .requestid] | @tsv' || true)
  fi
}

# --- Publisher run: every collected cycle ---------------------------------------------------
# Pins every cycle under `$WORK/cycles`. Nothing here may stop the editions from being
# published, so a failure that was not handled where it arose is reduced to a warning.
pin_collected_cycles() {
  local dir date
  ipfs_setup
  for dir in "$WORK"/cycles/*/; do
    date="$(basename "$dir")"
    ipfs_pin_cycle "$date" "${dir%/}" || warn "IPFS: pinning $date did not complete"
  done
}

# Pins are kept for the effective, the pre-staged and the previous cycle. Nothing is removed
# while the new or the deployed manifest still lists it, or while the deployed one is unknown.
unpin_expired_cycles() {
  local current="$1" keep="$WORK/ipfs-keep" previous listed
  ipfs_pinning_configured || return 0
  listed='[.entries[].ipfs_cid, (.ipfs.cycles // {} | .[].directory_cid)] | .[] | select(. != null)'
  jq -r "$listed" "$WORK/manifest.json" >"$keep"
  if [ -n "${LIVE_INDEX_URL:-}" ]; then
    if ! curl -fsSL --max-time 30 "${LIVE_INDEX_URL%cycles.json}manifest.json" 2>/dev/null \
      | jq -r "$listed" >>"$keep" 2>/dev/null; then
      warn "IPFS: the deployed manifest is unreadable; no pin is removed in this run"
      return 0
    fi
  fi
  previous="$("$BIN" plan --now "${current}T00:00:00Z" | jq -r '.faa_nasr_28_day.current.effective_date')"
  ipfs_unpin_expired "$previous" "$keep"
}

# --- Direct run: pin a locally built cycle directory ---------------------------------------
ipfs_pin_directory_main() {
  local dir="${1:?usage: ipfs-pin.sh <cycle directory named YYYY-MM-DD>}" date sidecar artifact product
  dir="$(cd "$dir" && pwd)"
  date="$(basename "$dir")"
  [[ "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || { echo "error: $dir is not named after a cycle date" >&2; return 2; }
  for sidecar in "$dir"/*.sha256; do
    artifact="${sidecar%.sha256}"
    [ -f "$artifact" ] || continue
    "$BIN" ipfs-cid --write "$artifact" >"$dir/.ipfs-cid.json"
    if [ "$(jq -r '.[0].sha256_hex' "$dir/.ipfs-cid.json")" != "$(cut -d' ' -f1 "$sidecar")" ]; then
      echo "error: $artifact does not match its .sha256 companion" >&2
      return 1
    fi
  done
  rm -f "$dir/.ipfs-cid.json"
  ipfs_artifact_path() { [ -f "$dir/$2" ] && echo "$dir/$2"; }
  ipfs_record_published() { :; }
  ipfs_setup
  if [ -n "$IPFS_SCRATCH" ]; then
    # Every artifact is at hand here, so the gateway's bytes are compared for all of them.
    mkdir -p "$IPFS_SCRATCH/$date/local"
    for sidecar in "$dir"/*.sha256; do
      [ ! -f "${sidecar%.sha256}" ] || ln -sf "${sidecar%.sha256}" "$IPFS_SCRATCH/$date/local/"
    done
  fi
  ipfs_pin_cycle "$date" "$dir"
  "$BIN" ipfs-dir --cycle-dir "$dir"
  [ ! -f "$dir/$IPFS_PIN_RECORD" ] || jq -c '{verified_gateway: "\(.gateway)/ipfs/\(.directory_cid)"}' "$dir/$IPFS_PIN_RECORD" >&2
  if [ "${RELEASE_UPLOAD:-false}" = "true" ] && [ "$FAILED" = "false" ]; then
    for product in $PRODUCTS; do
      [ "$(product_storage "$product")" = "ipfs" ] || continue
      artifact="$(product_artifact "$product")"
      [ -f "$dir/$artifact.cid.json" ] && [ -f "$dir/$(product_manifest "$product")" ] || continue
      log "uploading the records of $artifact to airac-$date"
      "${GH_BIN:-gh}" release upload "airac-$date" "$dir/$(product_manifest "$product")" \
        "$dir/$artifact.sha256" "$dir/$artifact.cid.json" --clobber
    done
  fi
  [ "$FAILED" = "false" ]
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  # shellcheck source=scripts/edition-products.sh
  source "$(dirname "$0")/edition-products.sh"
  BIN="${FLIGHTBAG_BIN:-./target/release/flightbag}"
  DRY_RUN="${DRY_RUN:-false}"
  FAILED=false
  [ -z "${REPO_SLUG:-}" ] || export GH_REPO="$REPO_SLUG"
  log() { echo "==> $*" >&2; }
  warn() { echo "warning: $*" >&2; }
  problem() {
    shift
    echo "error: $*" >&2
    FAILED=true
  }
  trap ipfs_teardown EXIT
  ipfs_pin_directory_main "$@"
fi
