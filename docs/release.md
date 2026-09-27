# Build and release workflows

The repository has two GitHub Actions workflows:

- [`ci.yml`](../.github/workflows/ci.yml) runs Rust tests and lint, then builds
  and tests the Node and Python bindings on Windows x64, Linux x64/arm64, and
  macOS x64/arm64. It uploads the five Node addons and five Python wheels as
  run artifacts. Pushes to `master` or `main` and pull requests trigger it.
- [`release.yml`](../.github/workflows/release.yml) runs only when started
  manually. With `publish=false` (the default), it repeats the build and
  prepares six npm tarballs and five wheels for inspection. It does not upload
  anything to a package registry. With `publish=true`, it also publishes the
  two Rust crates, the five npm platform packages and root package, and the
  PyPI wheels, in that order.

## Before the first publish

1. Push this repository to GitHub. The workflows have not run remotely until
   then. Confirm the package names are available and fill in the intended
   license and repository metadata in the manifests.
2. Keep `Cargo.toml`, `crates/agentpools-acp/Cargo.toml`,
   `bindings/node/package.json`, every `bindings/node/npm/*/package.json`, and
   `bindings/python/pyproject.toml` on the same version. Check with
   `python scripts/check_release_versions.py 0.1.0` (substitute your version).
3. Configure the `crates-io` GitHub environment with a
   `CARGO_REGISTRY_TOKEN` secret, and the `npm` environment with an
   `NPM_TOKEN` secret. Configure PyPI Trusted Publishing for this GitHub
   repository, workflow filename `release.yml`, and environment `pypi`.
   Environment reviewers can be added to require approval before each
   registry upload.
4. Run **Prepare or publish release** with the version and `publish=false`.
   Inspect its `release-npm` and `release-wheels` artifacts and the CI test
   results. This run only builds and stages packages.
5. When ready to publish, create and push a tag `vMAJOR.MINOR.PATCH` on the
   reviewed commit. Run **Prepare or publish release** from that tag with the
   matching version and `publish=true`. The preflight rejects a different ref
   before any registry upload.

The release workflow does not create a GitHub Release or a version tag. Cargo
publishes the core crate before its ACP adapter and waits for the core version
to appear in the registry. npm publishes platform packages before the root
package. PyPI Trusted Publishing runs in a separate job after the other two
registries succeed. Each registry is irreversible; a failure partway through
requires inspecting which packages were published before rerunning.

The current matrix covers the five targets declared in `bindings/node/package.json`.
It does not produce Linux musl, Windows ARM, Python free-threaded wheels, or a
Python source distribution. The Python extension uses PyO3's `abi3-py39`
feature, so each platform wheel covers supported regular CPython versions
from 3.9 onward.
