# Flightbag

Cycle-dated, cryptographically fingerprinted **non-moving** aeronautical
editions shared identically across real flight, EFB/C2 (`sokoly.app`), cockpit
displays (`Indicate`) and closed-loop simulation (`Simulate`).

## Domain Boundary (`Flightbag` vs `world`)

- **`Flightbag` (published non-moving baseline)**:
  - 28-day AIRAC cycle schedules (`AiracCycle`) and multi-source edition
    manifests (`EditionManifest`) preserving parallel provenance across FAA
    NASR, CIFP/ARINC-424, d-TPP georeferenced charts, OpenStreetMap aeroway
    graphs and Natural Earth basemaps.
  - Single-copy `AirportSurfaceGraph` (runway thresholds, taxiway edges,
    hold-short nodes and parking/gate spots) so OTW simulator spawn/taxi,
    `sokoly.app` and `Indicate` airport maps read one graph.
  - Standing `SpecialFlightRuleArea` definitions (DC SFRA/FRZ, Grand Canyon,
    Hudson River, etc.) and `GeorefChartRef` plate metadata.
- **`world` (moving physical & operational state)**:
  - Physical baseline (`world-core`, `world-atmosphere`, `world-geomag`) plus
    perishable `world-airmass` weather, `world-surveillance` traffic and
    `world-notices` TFRs, runway closure NOTAMs and SUA hot/cold activations
    keyed against `Flightbag` facility, runway and airspace identifiers.

## Status

Initial `#![no_std]` + `alloc` contract skeleton (`flightbag-core` + `flightbag`
umbrella). Offline NASR/CIFP/OSM conflation and PMTiles/GeoPackage compilation
pipelines will land in follow-on milestones.
