#!/usr/bin/env bash
# Plants one violation per rule in a scratch workspace and requires
# check-manifest.sh to reject each one, and to accept the clean workspace.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/check-manifest.sh"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
tree="$scratch/tree"

clean_tree() {
    rm -rf "$tree"
    mkdir -p "$tree/crates/a" "$tree/crates/b"
    printf '                    GNU AFFERO GENERAL PUBLIC LICENSE\n' > "$tree/LICENSE"
    cat > "$tree/Cargo.toml" <<'TOML'
[workspace]
members = ["crates/*"]
[workspace.package]
license = "AGPL-3.0-or-later"
[workspace.dependencies]
a = { path = "crates/a" }
other = { git = "https://example.invalid/other", rev = "0" }
TOML
    printf '[package]\nname = "a"\nlicense.workspace = true\n' > "$tree/crates/a/Cargo.toml"
    printf '[package]\nname = "b"\nlicense = "AGPL-3.0-or-later"\n[dependencies]\na = { path = "../a" }\n' \
        > "$tree/crates/b/Cargo.toml"
}

expect_pass() {
    clean_tree
    "$script" "$tree" > /dev/null 2>&1 || { echo "FAIL: clean workspace rejected"; exit 1; }
}

expect_reject() {
    local case="$1"
    shift
    clean_tree
    "$@"
    if "$script" "$tree" > /dev/null 2>&1; then
        echo "FAIL: $case was not rejected"
        exit 1
    fi
}

expect_pass
expect_reject "a missing LICENSE" rm "$tree/LICENSE"
expect_reject "another licence text" sh -c "printf 'MIT License\n' > '$tree/LICENSE'"
expect_reject "a package without a licence" \
    sh -c "printf '[package]\nname = \"a\"\n' > '$tree/crates/a/Cargo.toml'"
expect_reject "a package under another licence" \
    sh -c "printf '[package]\nname = \"a\"\nlicense = \"MIT\"\n' > '$tree/crates/a/Cargo.toml'"
expect_reject "a workspace path outside the repository" \
    sh -c "printf 'sibling = { path = \"../sibling/crates/s\" }\n' >> '$tree/Cargo.toml'"
expect_reject "a crate path outside the repository" \
    sh -c "printf 's = { path = \"../../../sibling\" }\n' >> '$tree/crates/b/Cargo.toml'"
expect_reject "a target-specific path outside the repository" \
    sh -c "printf '[target.\"cfg(unix)\".dev-dependencies]\ns = { path = \"../../../sibling\" }\n' >> '$tree/crates/b/Cargo.toml'"
echo "check-manifest tests passed"
