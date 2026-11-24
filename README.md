# Flightbag

> [!WARNING]
> **Experimental Prototype — Not for Navigation or Flight.**
> This repository and all generated artifacts (`airspace.pmtiles`, `manifest.json`, and GitHub Pages/Release datasets) are an **experimental software prototype** intended solely for research, 3D cartographic visualization, and simulation. They are **not** certified, verified, or approved by the FAA or any aviation authority, and **must never** be used for real-world navigation, preflight or in-flight planning, situational awareness, or operational flight decisions. Always consult current official FAA aeronautical charts, Chart Supplements, and NOTAMs.

Cycle-dated, cryptographically fingerprinted **non-moving** aeronautical
editions shared identically across experimental EFB/C2 prototypes (`sokoly.app`),
cockpit display research (`Indicate`), 3D/2D cartography (`maplibre-rs`), and
closed-loop simulation (`Simulate`).

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
| `flightbag-cli` | `std` | `flightbag` CLI (`plan`, `probe-*`, `build-*`, `manifest`, `index`, `sync`) |

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

## Publishing Editions (GitHub Releases + GitHub Pages)

`.github/workflows/airac-pages.yml` runs `scripts/publish-editions.sh` every three hours, three
more times just after the Thursday 09:01Z AIRAC cutover, and on manual `workflow_dispatch`.
Every run does the same thing, so a delayed or dropped scheduled run is made up by the next one:

1. Probe FAA for the effective and the next 28-day NASR and CIFP editions.
2. Build an edition only when its `airac-YYYY-MM-DD` Release is incomplete or FAA re-posted the
   archive (`Last-Modified` or size changed since the recorded build). A run with nothing new
   stops after the probes.
3. Publish `airspace.pmtiles`, `cifp_procedures.bin`, their `.sha256` companions and manifests
   to the cycle's Release. The next edition is published as a prerelease as soon as FAA posts
   it, weeks before it is effective.
4. Regenerate the rolling manifest (`flightbag-manifest` Release) and `cycles.json`, and
   redeploy GitHub Pages when `cycles.json` changed.

`current` is decided by the wall clock against each edition's `effective_from_utc`, so an
edition can be downloaded ahead of time but is never `current` before its effective instant.
If the next edition fails to build, the run reports it and leaves `current` alone; if no
effective NASR edition is published, the run fails and the deployed site stays as it was.

GitHub Pages (CORS and HTTP `Range`, usable from a browser):

- `cycles.json` — per authority, the `current` and `upcoming` edition with effective window,
  size, SHA-256, Release URL and site path
- `manifest.json` — rolling manifest of every published edition (`flightbag sync` input)
- `cycles/YYYY-MM-DD/` — artifacts of the effective and the pre-staged edition
- `cycles/current/` — copy of the effective edition; it changes only when a run completes
  after the cutover, so clients that must switch on time read `effective_until_utc` and the
  `upcoming` entry from `cycles.json`
- `styles/airspace_vfr.json` — MapLibre VFR 3D volumetric style

Release assets answer HTTP `Range` but send no CORS headers, so `pmtiles://` URLs pointing at
them work in native clients only. Artifacts above 90 MB are never copied to Pages; their
`cycles.json` entry carries a Release URL and no site path.

Raster chart archives (`build-charts`) are not built by the workflow: FAA's tile service
serves only the edition in force, so a chart edition cannot be pre-staged from it, and the
national archives exceed what Pages can host. Chart manifests uploaded to a cycle's Release
by hand are listed in the rolling manifest and `cycles.json`.

## Viewing 3D Airspace in `maplibre-rs`

```bash
# Stream live from Flightbag GitHub Pages via standard --style URL
cargo run --release -p maplibre-demo -- headed \
  --style https://luofang34.github.io/Flightbag/styles/airspace_vfr.json

# Or run 100% offline against a locally built cycle directory
cargo run --release -p maplibre-demo -- headed \
  --style /path/to/Flightbag/dist/cycles/current/airspace_vfr.json
```
