#!/usr/bin/env bash
# The repository must build from a fresh clone and carry its licence:
#   1. `LICENSE` holds the GNU AGPL v3 text;
#   2. every package declares `AGPL-3.0-or-later`, directly or from the workspace;
#   3. no dependency `path` leaves the repository. A dependency on another
#      repository is a `git` + `rev` dependency; a consumer that builds the
#      sibling locally redirects it with `[patch]`.
set -euo pipefail

root="${1:-$(cd "$(dirname "$0")/.." && pwd)}"

python3 - "$root" <<'PY'
import sys
import tomllib
from pathlib import Path

root = Path(sys.argv[1]).resolve()
LICENSE = "AGPL-3.0-or-later"
errors = []

licence = root / "LICENSE"
if not licence.is_file() or "GNU AFFERO GENERAL PUBLIC LICENSE" not in licence.read_text()[:200]:
    errors.append("LICENSE must hold the GNU AGPL v3 text")


def load(path: Path) -> dict:
    with open(path, "rb") as f:
        return tomllib.load(f)


def dependency_tables(manifest: dict):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        yield key, manifest.get(key, {})
    for target, table in manifest.get("target", {}).items():
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            yield f"target.{target}.{key}", table.get(key, {})


def check_paths(manifest_path: Path, tables) -> None:
    for where, deps in tables:
        for name, spec in deps.items():
            if isinstance(spec, dict) and "path" in spec:
                target = (manifest_path.parent / spec["path"]).resolve()
                if not target.is_relative_to(root):
                    rel = manifest_path.relative_to(root)
                    errors.append(f"{rel} [{where}] {name}: path {spec['path']} leaves the repository")


workspace_manifest = load(root / "Cargo.toml")
workspace = workspace_manifest.get("workspace", {})
workspace_license = workspace.get("package", {}).get("license")
check_paths(root / "Cargo.toml", [("workspace.dependencies", workspace.get("dependencies", {}))])
check_paths(root / "Cargo.toml", dependency_tables(workspace_manifest))

for member in workspace.get("members", []):
    for crate_dir in sorted(root.glob(member)):
        manifest_path = crate_dir / "Cargo.toml"
        if not manifest_path.is_file():
            continue
        manifest = load(manifest_path)
        package = manifest.get("package", {})
        declared = package.get("license")
        if isinstance(declared, dict) and declared.get("workspace"):
            declared = workspace_license
        if declared != LICENSE:
            errors.append(f"{manifest_path.relative_to(root)}: license must be {LICENSE}")
        check_paths(manifest_path, dependency_tables(manifest))

for error in errors:
    print(f"check-manifest: {error}", file=sys.stderr)
sys.exit(1 if errors else 0)
PY
