# shellcheck shell=bash
# Product table of the edition publisher; sourced, never executed.
#
# One row per product of one authority, spread over the functions below. Supporting another
# product means adding its row to each function and, for a product the publisher builds, its
# `flightbag probe-<name>` and `flightbag build-<name>` subcommands.

PRODUCTS="nasr waypoints cifp dtpp vfr_sectional vfr_terminal ifr_low ifr_high"

product_artifact() {
  case "$1" in
    nasr) echo "airspace.pmtiles" ;;
    waypoints) echo "nasr_waypoints.bin" ;;
    cifp) echo "cifp_procedures.bin" ;;
    dtpp) echo "dtpp_plates.bin" ;;
    vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "$1.pmtiles" ;;
    *) return 1 ;;
  esac
}

product_manifest() {
  case "$1" in
    nasr) echo "manifest.json" ;;
    waypoints) echo "waypoints_faa_nasr_waypoints.json" ;;
    cifp) echo "cifp_faa_cifp.json" ;;
    dtpp) echo "dtpp_faa_dtpp.json" ;;
    vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "chart_faa_$1.json" ;;
    *) return 1 ;;
  esac
}

product_authority() {
  case "$1" in
    waypoints) echo "faa_nasr_waypoints" ;;
    nasr | cifp | dtpp | vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "faa_$1" ;;
    *) return 1 ;;
  esac
}

# Where the artifact bytes are published.
#   release  Release asset, copied to the static site when small enough.
#   ipfs     IPFS only: too large for the static site and useless to a browser as a Release
#            asset, which is served without CORS headers. Its Release carries the manifest, the
#            `.sha256` and the `.cid.json` companion, and the bytes are fetched by CID.
product_storage() {
  case "$1" in
    nasr | waypoints | cifp | dtpp) echo "release" ;;
    vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "ipfs" ;;
    *) return 1 ;;
  esac
}

# Who builds an edition.
#   auto    the publisher, as soon as the authority posts it.
#   manual  an operator: the chart tile service serves only the edition in force, so a chart
#           edition cannot be built ahead of its effective time by a scheduled run. Such an
#           edition is listed once its manifest and companions are on the cycle's Release.
product_build() {
  case "$1" in
    nasr | waypoints | cifp | dtpp) echo "auto" ;;
    vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "manual" ;;
    *) return 1 ;;
  esac
}

# Key of the cycle grid in `flightbag plan` that the product follows.
product_cycle_grid() {
  case "$1" in
    nasr | waypoints | cifp | dtpp) echo "faa_nasr_28_day" ;;
    vfr_sectional | vfr_terminal | ifr_low | ifr_high) echo "faa_charts_56_day" ;;
    *) return 1 ;;
  esac
}

# The marker records the upstream validators an edition was built from. It is deleted before
# and uploaded after the edition's other assets, so its presence means the upload completed.
product_marker() { echo "$1.upstream.json"; }

product_cid_record() { echo "$(product_artifact "$1").cid.json"; }
