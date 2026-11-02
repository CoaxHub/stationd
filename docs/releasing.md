<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Releasing

A release is a tag. This repository builds, tests and attaches the packages to a GitHub release. It never signs anything and never publishes the APT repository: no signing key exists here.

## Making a release

1. Set the version in `crates/stationd/Cargo.toml` (the package version is `<crate version>-<revision>~debN`, see [packaging.md](packaging.md)) and merge it to `main` with CI green.
2. Tag the commit and push the tag:

   ```sh
   git tag -s vX.Y.Z -m "stationd X.Y.Z"
   git push origin vX.Y.Z
   ```

3. The release workflow runs on the tag. It:
   - builds the ten packages (bookworm, trixie, jammy, noble and resolute × amd64 and arm64) with the production domain and the production root certificate, which is public and committed under `packaging/roots/`;
   - runs the package, systemd and installer tests (`packaging/test/test-packages.sh`, `packaging/test/test-systemd.sh`, `packaging/test/test-installer.sh`) on every suite;
   - creates the GitHub release and attaches the ten `.deb` files, `SHA256SUMS`, the installer template (`installer/station.sh`, with its two placeholders unfilled) and the source tarballs of radiod, `ka9q-radio-<commit8>.tar.gz` with our patches to it, `ka9q-radio-<commit8>-<name>.patch`, and of librtlsdr, `rtl-sdr-<commit8>.tar.gz`.
4. CoaxHub's own tooling then publishes from those assets: it downloads them, checks them against `SHA256SUMS`, signs the APT repository at `apt.<domain>`, fills the repository URL and the key fingerprint into the installer template, and serves the result at `get.<domain>/station`. That tooling is not in this repository.

The domain is a repository variable of the workflow and never a default in the tree. The workflow refuses to build a release with `coaxhub.test`.

## What a release contains

| Asset | What |
|---|---|
| `coaxhub-stationd_<v>-<r>~<suffix>_<arch>.deb` for the suffixes `deb12`, `deb13`, `ubuntu22.04`, `ubuntu24.04` and `ubuntu26.04` and the architectures `amd64` and `arm64` | the ten packages, unsigned; the repository serves them as the suites `bookworm`, `trixie`, `jammy`, `noble` and `resolute` |
| `SHA256SUMS` | the SHA-256 of every other asset |
| `station.sh` | the installer template: it refuses to run until its two placeholders are replaced |
| `ka9q-radio-<commit8>.tar.gz` | the upstream source of the bundled radiod, at the pinned commit |
| `ka9q-radio-<commit8>-<name>.patch` | each of our patches to it (`packaging/patches/ka9q-radio/`), applied in order: the tarball and these are exactly what was compiled |
| `rtl-sdr-<commit8>.tar.gz` | the source of the bundled osmocom librtlsdr, exactly what was compiled |

The tarballs and the patches are the GPL source offer for radiod and librtlsdr. The packages' copyright file names each with its SHA-256, and the APT repository serves the same files next to the packages. Checkout of the tag gives the source of stationd and the scripts that build the packages.

The build is deterministic in its timestamps: every file's modification time is the tag commit's time.

## Verifying a package

Packages from the APT repository are verified by `apt` against the repository's signing key. The fingerprint of that key is published on the website's [station security page](https://coaxhub.com/docs/station-security). The installer pins the same fingerprint and refuses a key file that holds any other key, or a second one.

To verify a package you downloaded from a GitHub release:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

The checksums prove that the file is the one the release workflow built. To check what the package trusts, compare the root certificate in `packaging/roots/` at the release tag with the one the package was built from. The build refuses a certificate file that holds a private key.

To check the radiod and librtlsdr sources:

```sh
sha256sum ka9q-radio-<commit8>.tar.gz ka9q-radio-<commit8>-*.patch rtl-sdr-<commit8>.tar.gz   # equal the SHA-256s in /usr/share/doc/coaxhub-stationd/copyright
```

## Adding or dropping a system

The supported systems are rows of [packaging/suites.sh](../packaging/suites.sh) (the suite, its Docker image, version suffix, `/etc/os-release` IDs, the first `base-files` version of the release, and where its units go), plus the `case` of the installer (`installer/station.sh`, `check_system`), the CI matrix (`.github/workflows/ci.yml`) and the wrong-package pairs of `packaging/test/test-packages.sh`; [packaging.md §1](packaging.md#1-targets-and-versions) is the policy. A new Debian stable or Ubuntu LTS is added in the first release after it comes out; the first release after a release's standard support ends (Debian's security team or LTS; Ubuntu's five years, not ESM) drops it: its row, its installer case, its CI entry. The packages already in the repository stay but get no new versions. The repository then serves one suite per row (`scripts/apt-repo.sh` in CoaxHub's tooling lists them).

## Changing the root certificate

The root certificate in `packaging/roots/` is the trust anchor that every release package is built with. A change to it needs the same review as a change to the code that uses it, and a release. Stations on old packages keep trusting the old root until they upgrade.

## Changing the radiod and librtlsdr pins

The ka9q-radio commit is pinned in `packaging/build-deb.sh`, in `scripts/build-radiod.sh` and in `crates/backend-ka9q/vendor/ka9q-radio/VERSION`, which `scripts/update-ka9q-headers.sh` regenerates together with the vendored headers. A new pin means re-reading [radiod-notes.md](radiod-notes.md) against the new source, and checking our patches against it.

Our patches to ka9q-radio are in `packaging/patches/ka9q-radio/`, numbered in the order they apply (`0001-…`). The package build and `scripts/build-radiod.sh` apply every one, and a patch that doesn't apply fails the build. Each carries a header that says what it fixes and whether it is upstream yet. When upstream has the fix, move the pin past it and delete the patch. A release publishes the patches next to radiod's tarball (above).

The osmocom librtlsdr commit (`RTLSDR_COMMIT`, with its version `RTLSDR_VERSION`) is pinned in `packaging/build-deb.sh` and moves with its security and V4 fixes; a new pin means checking ka9q-radio's `src/rtlsdr.c` still builds against it ([packaging.md](packaging.md#the-bundled-librtlsdr)).
