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
| `flightbag-cli` | `std` | `flightbag` CLI (`plan`, `probe-*`, `build-*`, `ipfs-cid`, `ipfs-dir`, `manifest`, `index`, `sync`) |

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

## Publishing Editions (GitHub Releases + GitHub Pages + IPFS)

`.github/workflows/airac-pages.yml` runs `scripts/publish-editions.sh` every three hours, three
more times just after the Thursday 09:01Z AIRAC cutover, and on manual `workflow_dispatch`.
Every run does the same thing, so a delayed or dropped scheduled run is made up by the next one:

1. Probe FAA for the effective and the next 28-day NASR and CIFP editions.
2. Build an edition only when its `airac-YYYY-MM-DD` Release is incomplete or FAA re-posted the
   archive (`Last-Modified` or size changed since the recorded build). A run with nothing new
   stops after the probes.
3. Publish `airspace.pmtiles`, `cifp_procedures.bin`, their `.sha256` and `.cid.json`
   companions and manifests to the cycle's Release. The next edition is published as a
   prerelease as soon as FAA posts it, weeks before it is effective.
4. When an IPFS node or pinning service is configured, pin each cycle's directory, probe the
   gateway with cross-origin range requests, and record a gateway that passed in the Release's
   `ipfs-pin.json`. Pins of cycles older than the previous one are removed.
5. Regenerate the rolling manifest (`flightbag-manifest` Release) and `cycles.json`, and
   redeploy GitHub Pages when either changed.

`current` is decided by the wall clock against each edition's `effective_from_utc`, so an
edition can be downloaded ahead of time but is never `current` before its effective instant.
If the next edition fails to build, the run reports it and leaves `current` alone; if no
effective NASR edition is published, the run fails and the deployed site stays as it was.

GitHub Pages (CORS and HTTP `Range`, usable from a browser):

- `cycles.json` — per authority, the `current` and `upcoming` edition with effective window,
  size, SHA-256, Release URL, site path, IPFS CID, cycle directory CID and gateway URL
- `manifest.json` — rolling manifest of every published edition (`flightbag sync` input), with
  the IPFS import parameters and each cycle's directory CID
- `cycles/YYYY-MM-DD/` — artifacts of the effective and the pre-staged edition
- `cycles/current/` — copy of the effective edition; it changes only when a run completes
  after the cutover, so clients that must switch on time read `effective_until_utc` and the
  `upcoming` entry from `cycles.json`
- `styles/airspace_vfr.json` — MapLibre VFR 3D volumetric style

Release assets answer HTTP `Range` but send no CORS headers, so `pmtiles://` URLs pointing at
them work in native clients only. Artifacts above 90 MB are never copied to Pages. A browser
reads an edition from its `site_path`, or else from its `ipfs_url`; a native client can also
use `release_url`; any client with IPFS access can fetch `ipfs_cid`, or
`<ipfs_directory_cid>/<file>`, from a node or gateway of its choice.

### IPFS

Every artifact has a CIDv1 computed by `flightbag ipfs-cid` without an IPFS node. It equals what
`ipfs add --cid-version=1 --raw-leaves --chunker=size-1048576` assigns (1 MiB raw leaves,
balanced DAG of at most 174 links per node, SHA-256); the rolling manifest repeats these
parameters under `ipfs.import`. The artifacts of a cycle are linked under their filenames by
one UnixFS directory (`flightbag ipfs-dir`), whose CID is the cycle's `directory_cid`.

Pinning is off until configured through repository variables and secrets, read by
`scripts/ipfs-pin.sh`:

| Setting | Kind | Meaning |
| :--- | :--- | :--- |
| `IPFS_KUBO_RPC_URL` | variable | Kubo RPC API that imports and pins the artifacts |
| `IPFS_KUBO_RPC_TOKEN` | secret | Bearer token of that API, if it requires one |
| `IPFS_PIN_SERVICE_URL` | variable | IPFS Pinning Service API endpoint that pins each cycle directory |
| `IPFS_PIN_SERVICE_TOKEN` | secret | Bearer token of that endpoint |
| `IPFS_GATEWAY_URL` | variable | Gateway whose `/ipfs/<directory CID>/<file>` URLs are probed and published |

A pinning service fetches blocks from a peer instead of accepting uploads, so with a service
and no Kubo API the workflow installs Kubo (`scripts/install-kubo.sh`) and the publisher runs a
node for the duration of the run. A provider that reports another CID than the computed one
fails the run and is not recorded; any other pinning or gateway failure is a warning and the
next run tries again. A gateway is listed only for a directory it served correctly (`206`,
`Access-Control-Allow-Origin`, exact `Content-Range`, first and last bytes of every artifact).

Raster chart archives (`vfr_sectional.pmtiles`, `vfr_terminal.pmtiles`, `ifr_low.pmtiles`,
`ifr_high.pmtiles`) are stored on IPFS only: their Release carries the chart manifest, the
`.sha256` and the `.cid.json`, and `cycles.json` lists them with an `ipfs_url` and neither a
Release URL nor a site path. The workflow does not build them, because FAA's tile service
serves only the edition in force, so a chart edition cannot be pre-staged from it. An operator
builds and pins one, after which the scheduled runs list it:

```bash
cargo run --release -p flightbag-cli -- build-charts --out-dir dist/cycles/2026-10-29
IPFS_KUBO_RPC_URL=http://127.0.0.1:5001 IPFS_GATEWAY_URL=https://gateway.example \
  REPO_SLUG=luofang34/Flightbag RELEASE_UPLOAD=true scripts/ipfs-pin.sh dist/cycles/2026-10-29
```

## Viewing 3D Airspace in `maplibre-rs`

```bash
# Stream live from Flightbag GitHub Pages via standard --style URL
cargo run --release -p maplibre-demo -- headed \
  --style https://luofang34.github.io/Flightbag/styles/airspace_vfr.json

# Or run 100% offline against a locally built cycle directory
cargo run --release -p maplibre-demo -- headed \
  --style /path/to/Flightbag/dist/cycles/current/airspace_vfr.json
```
