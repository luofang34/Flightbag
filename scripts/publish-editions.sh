#!/usr/bin/env bash
# Publishes the effective and the next pre-staged edition of every product, then stages the
# static site. Safe to run at any time and any number of times:
#   - the instant is resolved once (`flightbag plan`) and every later step receives explicit
#     cycle dates and that instant, so a run straddling a cutover cannot change its mind;
#   - an edition is rebuilt only when its `airac-<date>` Release lacks an asset or the upstream
#     validators recorded at its last build differ from what the authority serves now;
#   - `current`/`upcoming` come from `flightbag index`, a pure function of the published
#     editions and the instant, never from which trigger started the run;
#   - a failing `upcoming` edition is reported and skipped; a missing `current` NASR edition
#     leaves the deployed site untouched;
#   - every artifact gets an IPFS CID companion, and when a node or pinning service is
#     configured (`scripts/ipfs-pin.sh` lists the IPFS_* variables) each cycle is pinned and
#     the gateway verified to serve it is listed. Pinning never fails a scheduled run, except
#     when a provider stores an upload under another CID than the one computed here.
#
# Environment:
#   FLIGHTBAG_BIN   flightbag CLI                     (default ./target/release/flightbag)
#   GH_BIN          GitHub CLI                        (default gh)
#   REPO_SLUG       owner/repo                        (default $GITHUB_REPOSITORY)
#   WORK_DIR        scratch directory                 (default dist)
#   SITE_DIR        staged static site                (default _site)
#   CACHE_DIR       upstream download cache           (default data/cache)
#   NOW_UTC         instant override YYYY-MM-DDTHH:MM:SSZ (default wall clock)
#   LIVE_INDEX_URL  deployed cycles.json (manifest.json beside it); identical files skip the redeploy
#   FORCE_REBUILD   true rebuilds both editions of every product
#   FORCE_DEPLOY    true stages the site even when the index is unchanged
#   DRY_RUN         true builds and stages locally but performs no Release write and pins nothing
#   GITHUB_OUTPUT   receives deploy=<bool> and required_failed=<bool>
set -euo pipefail

BIN="${FLIGHTBAG_BIN:-./target/release/flightbag}"
GH="${GH_BIN:-gh}"
REPO_SLUG="${REPO_SLUG:-${GITHUB_REPOSITORY:?set REPO_SLUG or GITHUB_REPOSITORY}}"
WORK="${WORK_DIR:-dist}"
SITE="${SITE_DIR:-_site}"
CACHE="${CACHE_DIR:-data/cache}"
FORCE_REBUILD="${FORCE_REBUILD:-false}"
FORCE_DEPLOY="${FORCE_DEPLOY:-false}"
DRY_RUN="${DRY_RUN:-false}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# GitHub Pages rejects files of 100 MB and caps a site near 1 GB; the same per-file limit
# decides which artifacts `flightbag index` gives a site path.
SITE_MAX_FILE_BYTES="${SITE_MAX_FILE_BYTES:-90000000}"
SITE_MAX_TOTAL_BYTES="${SITE_MAX_TOTAL_BYTES:-900000000}"
MANIFEST_TAG="flightbag-manifest"
export GH_REPO="$REPO_SLUG"

# shellcheck source=scripts/edition-products.sh
source "$ROOT/scripts/edition-products.sh"
# shellcheck source=scripts/ipfs-pin.sh
source "$ROOT/scripts/ipfs-pin.sh"

# --- Reporting -----------------------------------------------------------------------------
REQUIRED_FAILED=false

log() { echo "==> $*" >&2; }
notice() { echo "::notice::$*" >&2; }
warn() { echo "::warning::$*" >&2; }

# A problem with the `current` edition must turn the run red; the same problem with the
# `upcoming` edition is retried by the next run and must not.
problem() {
  local slot="$1"
  shift
  if [ "$slot" = "current" ]; then
    echo "::error::$*" >&2
    REQUIRED_FAILED=true
  else
    warn "$*"
  fi
}

gh_write() {
  if [ "$DRY_RUN" = "true" ]; then
    echo "[dry-run] gh $*" >&2
  else
    "$GH" "$@"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

release_assets() {
  "$GH" release view "$1" --json assets --jq '.assets[].name' 2>/dev/null || true
}

has_line() { grep -qxF -- "$2" <<<"$1"; }

# --- Build and release one edition ---------------------------------------------------------
upload_edition() {
  local product="$1" date="$2" slot="$3" out="$4" fingerprint="$5" assets="$6"
  local tag="airac-$date" artifact manifest marker
  artifact="$(product_artifact "$product")"
  manifest="$(product_manifest "$product")"
  marker="$(product_marker "$product")"

  if ! "$GH" release view "$tag" --json tagName >/dev/null 2>&1; then
    local title="Flightbag FAA editions — ${date} [Experimental Prototype]"
    local notes="**EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT.** Cycle-stamped FAA editions effective ${date}T09:01:00Z. Verify every artifact against its .sha256 companion."
    if [ "$slot" = "upcoming" ]; then
      # A pre-staged edition must not become `releases/latest` before it is effective.
      gh_write release create "$tag" --title "$title" --notes "$notes" --prerelease --latest=false || return 1
    else
      gh_write release create "$tag" --title "$title" --notes "$notes" || return 1
    fi
  fi
  if has_line "$assets" "$marker"; then
    gh_write release delete-asset "$tag" "$marker" -y || return 1
  fi
  "$BIN" ipfs-cid --write "$out/$artifact" >/dev/null || return 1
  gh_write release upload "$tag" "$out/$artifact" "$out/$artifact.sha256" "$out/$artifact.cid.json" \
    "$out/$manifest" --clobber || return 1
  printf '%s\n' "$fingerprint" >"$out/$marker"
  gh_write release upload "$tag" "$out/$marker" --clobber || return 1
}

ensure_edition() {
  local product="$1" date="$2" slot="$3"
  local tag="airac-$date" artifact manifest marker assets complete=true name probe fingerprint
  artifact="$(product_artifact "$product")"
  manifest="$(product_manifest "$product")"
  marker="$(product_marker "$product")"
  assets="$(release_assets "$tag")"
  for name in "$artifact" "$artifact.sha256" "$manifest" "$marker"; do
    has_line "$assets" "$name" || complete=false
  done

  # An upstream outage is neither "not posted" nor "re-posted": keep what is published.
  if ! probe="$("$BIN" "probe-$product" --cycle "$date")"; then
    if [ "$complete" = "true" ]; then
      warn "$product $date: upstream probe failed; keeping the published edition"
    else
      problem "$slot" "$product $date: upstream probe failed and no complete edition is published"
    fi
    return 0
  fi
  if [ "$(jq -r '.published' <<<"$probe")" != "true" ]; then
    if [ "$complete" = "true" ]; then
      warn "$product $date: upstream no longer serves this edition; keeping the published one"
    elif [ "$slot" = "current" ]; then
      problem "$slot" "$product $date: the effective edition is not served upstream"
    else
      notice "$product $date: not posted upstream yet"
    fi
    return 0
  fi

  # ETags are left out: they can differ between the mirrors behind one upstream host, which
  # would read as a re-post on every run.
  fingerprint="$(jq -cS '{url, last_modified: .upstream.last_modified, size_bytes: .upstream.size_bytes}' <<<"$probe")"
  if [ "$complete" = "true" ] && [ "$FORCE_REBUILD" != "true" ]; then
    local recorded_dir="$WORK/recorded/$date" recorded=""
    mkdir -p "$recorded_dir"
    if "$GH" release download "$tag" -p "$marker" -D "$recorded_dir" --clobber >/dev/null 2>&1; then
      recorded="$(jq -cS '.' "$recorded_dir/$marker" 2>/dev/null || true)"
    fi
    if [ "$recorded" = "$fingerprint" ]; then
      log "$product $date ($slot): published and upstream unchanged"
      return 0
    fi
    log "$product $date ($slot): upstream posting changed; rebuilding"
  fi

  local out="$WORK/build/$date"
  mkdir -p "$out"
  log "$product $date ($slot): building"
  if ! "$BIN" "build-$product" --cycle "$date" --cache-dir "$CACHE" --out-dir "$out" >"$out/build-$product.log"; then
    problem "$slot" "$product $date: build failed"
    return 0
  fi
  if [ "$(sha256_of "$out/$artifact")" != "$(cut -d' ' -f1 "$out/$artifact.sha256")" ]; then
    problem "$slot" "$product $date: built $artifact does not match its .sha256 companion"
    return 0
  fi
  if ! upload_edition "$product" "$date" "$slot" "$out" "$fingerprint" "$assets"; then
    problem "$slot" "$product $date: Release upload failed"
  fi
}

# --- Index ---------------------------------------------------------------------------------
manifest_sha256() {
  jq -r --arg f "$2" '[.. | objects | select(.file? == $f) | .sha256_hex] | first // empty' "$1"
}

# Echoes a local copy of a published artifact whose SHA-256 is `want` (by default the one its
# CID companion records), downloading the Release asset when this run did not build it.
ipfs_artifact_path() {
  local date="$1" file="$2" want="${3:-}" candidate fetched="$WORK/artifacts/$1"
  [ -n "$want" ] || want="$(jq -r '.sha256_hex' "$WORK/cycles/$date/$file.cid.json" 2>/dev/null || true)"
  [ -n "$want" ] || return 1
  for candidate in "$WORK/build/$date/$file" "$fetched/$file"; do
    if [ -f "$candidate" ] && [ "$(sha256_of "$candidate")" = "$want" ]; then
      echo "$candidate"
      return 0
    fi
  done
  "$GH" release download "airac-$date" -p "$file" -D "$fetched" --clobber >/dev/null 2>&1 || return 1
  [ "$(sha256_of "$fetched/$file")" = "$want" ] && echo "$fetched/$file"
}

ipfs_record_published() { gh_write release upload "airac-$1" "$2" --clobber; }

# An edition published without a CID companion gets one computed from its Release asset, so
# no edition has to be rebuilt to be addressable by content. A companion left without a
# matching asset is emptied: no CID is better than the CID of other bytes.
backfill_cid_record() {
  local product="$1" date="$2" record="$3" manifest="$4" artifact path
  artifact="$(product_artifact "$product")"
  : >"$record"
  if ! path="$(ipfs_artifact_path "$date" "$artifact" "$(manifest_sha256 "$manifest" "$artifact")")"; then
    warn "$product $date: no asset matches the published manifest; its CID is not recorded"
    return 0
  fi
  "$BIN" ipfs-cid --write "$path" >/dev/null || return 0
  cp "$path.cid.json" "$record"
  gh_write release upload "airac-$date" "$record" --clobber \
    || warn "$product $date: uploading $(basename "$record") failed"
}

# Downloads the manifest of every completely uploaded edition that can still be effective and
# derives the rolling manifest and the index from them, so the result reflects what is
# actually published rather than what this run happened to build.
collect_published_manifests() {
  local plan="$1" tag date assets product dir floor manifest record artifact
  mkdir -p "$WORK/cycles"
  find "$WORK/cycles" -mindepth 1 -delete
  floor="$(jq -r '[.[] | objects | .current.effective_date] | min' <<<"$plan")"
  while IFS= read -r tag; do
    [[ "$tag" =~ ^airac-[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || continue
    date="${tag#airac-}"
    [[ "$date" < "$floor" ]] && continue
    assets="$(release_assets "$tag")"
    dir="$WORK/cycles/$date"
    mkdir -p "$dir"
    for product in $PRODUCTS; do
      artifact="$(product_artifact "$product")"
      manifest="$(product_manifest "$product")"
      record="$(product_cid_record "$product")"
      # Every companion is fetched, listed or not: the cycle directory links all of them, so
      # its CID stays the same when the edition of one product expires before the others.
      if has_line "$assets" "$record"; then
        "$GH" release download "$tag" -p "$record" -D "$dir" --clobber >/dev/null
      fi
      [[ "$date" < "$(jq -r --arg g "$(product_cycle_grid "$product")" '.[$g].current.effective_date' <<<"$plan")" ]] && continue
      has_line "$assets" "$manifest" || continue
      if [ "$(product_storage "$product")" = "release" ]; then
        has_line "$assets" "$(product_marker "$product")" || continue
      elif [ ! -f "$dir/$record" ]; then
        # Without its companion an IPFS-only artifact has no address, so it is not listed.
        continue
      fi
      "$GH" release download "$tag" -p "$manifest" -D "$dir" --clobber >/dev/null
      if [ "$(jq -r '.sha256_hex' "$dir/$record" 2>/dev/null || true)" \
        = "$(manifest_sha256 "$dir/$manifest" "$artifact")" ]; then
        continue
      fi
      if [ "$(product_storage "$product")" = "release" ]; then
        backfill_cid_record "$product" "$date" "$dir/$record" "$dir/$manifest"
      else
        warn "$product $date: $record does not describe the artifact of $manifest; not listed"
        find "$dir" -name "$manifest" -delete
      fi
    done
    find "$dir" -name '*.cid.json' -empty -delete
    if has_line "$assets" "$IPFS_PIN_RECORD"; then
      "$GH" release download "$tag" -p "$IPFS_PIN_RECORD" -D "$dir" --clobber >/dev/null
    fi
  done < <("$GH" release list --limit 200 --json tagName --jq '.[].tagName' | sort)
}

publish_rolling_manifest() {
  local previous="$WORK/previous-manifest"
  rm -rf "$previous"
  mkdir -p "$previous"
  if "$GH" release download "$MANIFEST_TAG" -p manifest.json -D "$previous" --clobber >/dev/null 2>&1 \
    && cmp -s "$previous/manifest.json" "$WORK/manifest.json"; then
    log "rolling manifest unchanged"
    return 0
  fi
  if ! "$GH" release view "$MANIFEST_TAG" --json tagName >/dev/null 2>&1; then
    gh_write release create "$MANIFEST_TAG" --latest=false \
      --title "Flightbag Rolling Manifest [Experimental Prototype]" \
      --notes "Rolling pointer to every published edition with its effective window, size and SHA-256. Stable URL: releases/download/${MANIFEST_TAG}/manifest.json"
  fi
  gh_write release upload "$MANIFEST_TAG" "$WORK/manifest.json" --clobber
}

# --- Static site ---------------------------------------------------------------------------
stage_site() {
  local authority date file sha dest current_date
  rm -rf "$SITE"
  mkdir -p "$SITE/styles" "$SITE/cycles"
  cp "$ROOT/styles/airspace_vfr.json" "$SITE/styles/airspace_vfr.json"

  while IFS=$'\t' read -r authority date file sha; do
    dest="$SITE/cycles/$date"
    mkdir -p "$dest"
    if [ -f "$WORK/build/$date/$file" ] && [ "$(sha256_of "$WORK/build/$date/$file")" = "$sha" ]; then
      cp "$WORK/build/$date/$file" "$dest/$file"
    else
      "$GH" release download "airac-$date" -p "$file" -D "$dest" --clobber >/dev/null
    fi
    if [ "$(sha256_of "$dest/$file")" != "$sha" ]; then
      echo "::error::$authority $date: $file does not match the SHA-256 in its published manifest" >&2
      return 1
    fi
    printf '%s  %s\n' "$sha" "$file" >"$dest/$file.sha256"
    find "$WORK/cycles/$date" -maxdepth 1 -name '*.json' -exec cp {} "$dest/" \;
  done < <(jq -r '
    .editions | to_entries[] | .key as $authority | .value
    | (.current, .upcoming) | select(. != null and .site_path != null)
    | [$authority, .effective_date, .file, .sha256_hex] | @tsv' "$WORK/cycles.json")

  current_date="$(jq -r '.current.effective_date' "$WORK/cycles.json")"
  mkdir -p "$SITE/cycles/current"
  cp -R "$SITE/cycles/$current_date/." "$SITE/cycles/current/"
  cp "$WORK/manifest.json" "$SITE/manifest.json"
  cp "$WORK/cycles.json" "$SITE/cycles.json"
  touch "$SITE/.nojekyll"
  write_site_index_page
  check_site_limits
}

write_site_index_page() {
  local rows
  rows="$(jq -r '
    .editions | to_entries[] | .key as $authority | .value
    | (["current", .current], ["upcoming", .upcoming]) | select(.[1] != null)
    | .[0] as $slot | .[1]
    | "<tr><td><code>\($authority)</code></td><td>\($slot)</td><td>\(.effective_from_utc)</td><td>\(.effective_until_utc)</td><td>"
      + (if .site_path then "<a href=\"\(.site_path)\"><code>\(.site_path)</code></a>"
         elif .ipfs_url then "<a href=\"\(.ipfs_url)\">IPFS gateway</a>"
         elif .release_url then "<a href=\"\(.release_url)\">Release asset</a>"
         else "<code>ipfs://\(.ipfs_directory_cid)/\(.file)</code>" end)
      + "</td></tr>"' "$WORK/cycles.json")"
  cat >"$SITE/index.html" <<EOF
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Flightbag — FAA Editions (Experimental Prototype)</title>
  <style>
    body{font:15px/1.5 system-ui,sans-serif;max-width:920px;margin:3rem auto;padding:0 1rem;color:#111}
    code{background:#f3f4f6;padding:2px 6px;border-radius:4px}
    table{border-collapse:collapse}td,th{border:1px solid #d1d5db;padding:4px 8px;text-align:left}
    .warning{background:#fef2f2;border:2px solid #dc2626;border-radius:8px;padding:1rem 1.25rem;margin:1.25rem 0;color:#7f1d1d}
  </style>
</head>
<body>
  <h1>Flightbag — FAA Editions</h1>
  <div class="warning" role="alert">
    <strong>EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT.</strong><br>
    These archives and metadata are unverified experimental software outputs intended solely for research, visualization, and simulation. Always consult official FAA aeronautical charts and NOTAMs.
  </div>
  <ul>
    <li><a href="cycles.json"><code>cycles.json</code></a> — effective and pre-staged edition per authority</li>
    <li><a href="manifest.json"><code>manifest.json</code></a> — rolling SHA-256 manifest of every published edition</li>
    <li><a href="styles/airspace_vfr.json"><code>styles/airspace_vfr.json</code></a> — MapLibre VFR 3D volumetric style</li>
    <li><a href="cycles/current/airspace.pmtiles"><code>cycles/current/airspace.pmtiles</code></a> — alias of the effective NASR edition</li>
  </ul>
  <table>
    <tr><th>Authority</th><th>Slot</th><th>Effective from (UTC)</th><th>Effective until (UTC)</th><th>Artifact</th></tr>
    ${rows}
  </table>
</body>
</html>
EOF
}

check_site_limits() {
  local total=0 size path
  while IFS= read -r path; do
    size="$(wc -c <"$path" | tr -d ' ')"
    if [ "$size" -gt "$SITE_MAX_FILE_BYTES" ]; then
      echo "::error::$path is $size bytes; the static site accepts at most $SITE_MAX_FILE_BYTES per file" >&2
      return 1
    fi
    total=$((total + size))
  done < <(find "$SITE" -type f)
  if [ "$total" -gt "$SITE_MAX_TOTAL_BYTES" ]; then
    echo "::error::staged site is $total bytes; the limit is $SITE_MAX_TOTAL_BYTES" >&2
    return 1
  fi
  log "staged site: $total bytes"
}

# --- Run -----------------------------------------------------------------------------------
main() {
  local plan now current upcoming product deploy=true live_dir ipfs_args=()
  mkdir -p "$WORK"
  trap ipfs_teardown EXIT
  if [ -n "${NOW_UTC:-}" ]; then
    plan="$("$BIN" plan --now "$NOW_UTC")"
  else
    plan="$("$BIN" plan)"
  fi
  now="$(jq -r '.now_utc' <<<"$plan")"
  current="$(jq -r '.faa_nasr_28_day.current.effective_date' <<<"$plan")"
  upcoming="$(jq -r '.faa_nasr_28_day.upcoming.effective_date' <<<"$plan")"
  log "instant $now: current $current, upcoming $upcoming"

  for product in $PRODUCTS; do
    if [ "$(product_storage "$product")" = "ipfs" ]; then
      ipfs_args+=(--ipfs-only "$(product_artifact "$product")")
    fi
    [ "$(product_build "$product")" = "auto" ] || continue
    ensure_edition "$product" "$current" current
    ensure_edition "$product" "$upcoming" upcoming
  done

  collect_published_manifests "$plan"
  pin_collected_cycles
  [ -z "$IPFS_GATEWAY_URL" ] || ipfs_args+=(--ipfs-gateway "$IPFS_GATEWAY_URL")
  "$BIN" manifest --cycles-dir "$WORK/cycles" \
    --url-template "https://github.com/${REPO_SLUG}/releases/download/airac-{date}/{file}" \
    ${ipfs_args[@]+"${ipfs_args[@]}"} --out "$WORK/manifest.json" >/dev/null
  "$BIN" index --manifest "$WORK/manifest.json" --now "$now" \
    --site-max-bytes "$SITE_MAX_FILE_BYTES" --out "$WORK/cycles.json" >/dev/null

  for product in $PRODUCTS; do
    [ "$(product_build "$product")" = "auto" ] || continue
    if ! jq -e --arg a "$(product_authority "$product")" '.editions[$a].current != null' \
      "$WORK/cycles.json" >/dev/null; then
      problem current "$product: no effective edition is published for $now"
    fi
  done

  if ! jq -e '.current != null' "$WORK/cycles.json" >/dev/null; then
    # Deploying now would drop `cycles/current/`; the previous site is the safer thing to serve.
    deploy=false
  else
    publish_rolling_manifest
    if [ "$("$GH" release view "airac-$current" --json isPrerelease --jq '.isPrerelease' 2>/dev/null || true)" = "true" ]; then
      gh_write release edit "airac-$current" --prerelease=false --latest
    fi
    # The rolling manifest is compared as well: a re-post that changes only upstream
    # provenance leaves the index identical but must still reach the site.
    live_dir="$WORK/live"
    mkdir -p "$live_dir"
    if [ "$FORCE_DEPLOY" != "true" ] && [ -n "${LIVE_INDEX_URL:-}" ] \
      && curl -fsSL --max-time 30 -o "$live_dir/cycles.json" "$LIVE_INDEX_URL" 2>/dev/null \
      && curl -fsSL --max-time 30 -o "$live_dir/manifest.json" "${LIVE_INDEX_URL%cycles.json}manifest.json" 2>/dev/null \
      && cmp -s "$live_dir/cycles.json" "$WORK/cycles.json" \
      && cmp -s "$live_dir/manifest.json" "$WORK/manifest.json"; then
      log "deployed index and manifest are already up to date"
      deploy=false
    fi
    unpin_expired_cycles "$current" || warn "IPFS: removing expired pins did not complete"
  fi

  if [ "$deploy" = "true" ]; then
    stage_site
  fi
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
      echo "deploy=$deploy"
      echo "required_failed=$REQUIRED_FAILED"
    } >>"$GITHUB_OUTPUT"
  fi
  log "deploy=$deploy required_failed=$REQUIRED_FAILED"
}

main "$@"
