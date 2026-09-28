"""Check that all packages represent one release before staging artifacts."""

import json
import re
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
NODE_TARGETS = {
    "win32-x64-msvc",
    "linux-x64-gnu",
    "linux-arm64-gnu",
    "darwin-x64",
    "darwin-arm64",
}


def toml_version(path: str, section: str) -> str:
    with (ROOT / path).open("rb") as source:
        return tomllib.load(source)[section]["version"]


def json_version(path: Path) -> str:
    return json.loads(path.read_text(encoding="utf-8"))["version"]


def main() -> int:
    if len(sys.argv) != 2 or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", sys.argv[1]):
        print("usage: check_release_versions.py MAJOR.MINOR.PATCH", file=sys.stderr)
        return 2

    expected = sys.argv[1]
    versions = {
        "Cargo core": toml_version("Cargo.toml", "package"),
        "Cargo ACP": toml_version("crates/agentpools-acp/Cargo.toml", "package"),
        "Cargo Transport": toml_version("crates/agentpools-transport/Cargo.toml", "package"),
        "Cargo Runtime": toml_version("crates/agentpools-runtime/Cargo.toml", "package"),
        "Node": json_version(ROOT / "bindings/node/package.json"),
        "Python": toml_version("bindings/python/pyproject.toml", "project"),
    }
    manifests = sorted((ROOT / "bindings/node/npm").glob("*/package.json"))
    found = {manifest.parent.name for manifest in manifests}
    if found != NODE_TARGETS:
        print(f"Node platform manifests: {sorted(found)}, expected {sorted(NODE_TARGETS)}", file=sys.stderr)
        return 1
    for manifest in manifests:
        versions[f"Node {manifest.parent.name}"] = json_version(manifest)

    mismatches = {name: version for name, version in versions.items() if version != expected}
    if mismatches:
        for name, version in mismatches.items():
            print(f"{name}: {version}, expected {expected}", file=sys.stderr)
        return 1

    print(f"All {len(versions)} package versions match {expected}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
