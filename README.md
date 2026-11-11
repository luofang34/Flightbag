# Flightbag

Cycle-dated, cryptographically fingerprinted **non-moving** aeronautical
editions shared identically across real flight, EFB/C2 (`sokoly.app`), cockpit
displays (`Indicate`), 3D/2D cartography (`maplibre-rs`) and closed-loop
simulation (`Simulate`).

## Domain Boundary (`Flightbag` vs `world`)

- **`Flightbag` (what we bring on preflight — static cycle-dated baseline)**:
  - 28-day AIRAC cycle schedules (`AiracCycle`) and multi-source edition
    manifests (`EditionManifest`) preserving parallel SHA-256 provenance across
    FAA NASR, CIFP/ARINC-424, d-TPP georeferenced charts, OpenStreetMap aeroway
    graphs and Natural Earth basemaps.
  - 3D NASR Controlled Airspace (`Class B, C, D, E`) and Special Use Airspace
    (`P, R, W, MOA, A, NSA`) volumes (`AirspaceVolume`) with exact `MSL` / `AGL`
    / `FL` vertical datums, 3D `SUBTR` partial-altitude cutouts, `EXCLUSION='1'`
    VFR corridors, and wedding-cake shelf donut normalization.
  - Pure-Rust Hilbert-ordered MVT 2.1 + `PMTiles` v3 compiler (`flightbag-tiles`)
    producing single-file `airspace.pmtiles` archives served directly via HTTP
    Range requests on GitHub Pages or `file://` offline preflight packages.
  - Single-copy `AirportSurfaceGraph` (runway thresholds, taxiway edges,
    hold-short nodes and parking/gate spots) and standing `SpecialFlightRuleArea`
    definitions.
- **`world` (moving physical & live operational state)**:
  - Physical baseline (`world-core`, `world-atmosphere`, `world-geomag`) plus
    live ADS-B traffic (`world-surveillance`), weather (`world-airmass`), TFRs,
    runway closure NOTAMs and live SUA hot/cold activations (`world-notices`)
    keyed against `Flightbag` facility, runway and `airspace_key` identifiers.

## Workspace Crates

| Crate | `#![no_std]` | Role |
| :--- | :---: | :--- |
| `flightbag-core` | Yes (`alloc`) | Cycle grid (`AiracCycle`), `EditionManifest`, `AirspaceVolume`, `AirportSurfaceGraph`, `SpecialFlightRuleArea` |
| `flightbag` | Yes (`alloc`) | Umbrella re-export crate for flight/EFB/renderer consumers |
| `flightbag-nasr` | `std` | Pure-Rust FAA 28-day NASR fetcher, ESRI Shapefile reader (`Class_Airspace`), AIXM 5.1 SUA 3D CSG parser (`SaaSubscriberFile`), and boolean shelf normalizer |
| `flightbag-tiles` | `std` | Pure-Rust MVT 2.1 protobuf encoder (`z2..=10`), `[-64, 4160]` polygon/line tile clipper, and `PMTiles` v3 + `manifest.json` writer |
| `flightbag-cli` | `std` | `flightbag` CLI (`probe-nasr`, `build-nasr`) |

## Building NASR 3D Airspace Locally

```bash
# Probe whether FAA has published the current or upcoming 28-day NASR edition
cargo run --release -p flightbag-cli -- probe-nasr --cycle current
cargo run --release -p flightbag-cli -- probe-nasr --cycle upcoming

# Download (or reuse cached) 28-day NASR archive and compile airspace.pmtiles + manifest.json
cargo run --release -p flightbag-cli -- build-nasr \
  --cycle current \
  --cache-dir data/cache \
  --out-dir dist/cycles/current
```

## Deploying NASR 3D Airspace (GitHub Releases + GitHub Pages)

`.github/workflows/airac-pages.yml` runs on manual `workflow_dispatch` (no
scheduled cycle cron job), compiles the requested FAA 28-day NASR edition in
pure Rust, publishes an immutable `airac-YYYY-MM-DD` GitHub Release
(`airspace.pmtiles` + `manifest.json`), and deploys static HTTP Range-ready
endpoints to GitHub Pages:

- `cycles.json` — metadata for the deployed edition
- `cycles/current/airspace.pmtiles` & `cycles/current/manifest.json`
