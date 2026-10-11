# SPDX-License-Identifier: GPL-3.0-only
#
# Builds the coaxhub-stationd Debian package for one (suite, architecture).
# Driven by packaging/build-deb.sh; the build
# context is the repository root. arm64 is cross-compiled on an amd64
# host: no emulation is needed.
#
# Stages:
#   toolchain  debian:<suite> with the native and cross C toolchains, the
#              amd64 and arm64 -dev libraries radiod links, cmake, protoc
#              and Rust
#   librtlsdr  osmocom librtlsdr at the pinned commit, from the published
#              source tarball (named context `rtlsdr`, its SHA-256 checked):
#              the same library on every suite, private to the package
#   radiod     ka9q-radio at the pinned commit, from the published source
#              tarball (named context `ka9q`), with ARCHOPTS overridden and
#              the package's private paths compiled in; its rtlsdr.so links
#              the private librtlsdr, found through its RUNPATH
#   stationd   stationd --features packaged (named context `rootca` holds
#              the root certificate built into the binary)
#   package    packaging/deb/assemble.sh: the file tree, maintainer
#              scripts, Depends from dpkg-shlibdeps, dpkg-deb
#   export     just the .deb, for `docker build --output type=local`

ARG SUITE=bookworm

FROM debian:${SUITE} AS toolchain
ARG RUST_VERSION=stable
ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# radiod links -lopus -lfftw3f(_threads) -liniparser -lusb-1.0 -lbsd -ldl;
# librtlsdr (built below, not the suite's) links -lusb-1.0. station-proto's
# build script runs protoc.
RUN dpkg --add-architecture arm64 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl file git dpkg-dev \
      build-essential cmake pkg-config protobuf-compiler libprotobuf-dev \
      gcc-aarch64-linux-gnu libc6-dev-arm64-cross binutils-aarch64-linux-gnu \
      libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libusb-1.0-0-dev \
      libbsd-dev:arm64 libfftw3-dev:arm64 libiniparser-dev:arm64 libopus-dev:arm64 \
      libusb-1.0-0-dev:arm64 \
 && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
          --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu \
 && rustc --version

FROM toolchain AS librtlsdr
ARG ARCH=amd64
ARG RTLSDR_COMMIT
ARG RTLSDR_TARBALL_SHA256
COPY --from=rtlsdr rtl-sdr-*.tar.gz /src/
# Only the shared library (no utilities, no udev rule: the package has its
# own), installed under /opt/rtlsdr with its headers for radiod's stage.
# Its SONAME is librtlsdr.so.0; the file is shipped under that name. As in
# Debian's build, it detaches the kernel's DVB driver when that still holds
# the dongle (before the reboot that applies the package's blacklist).
RUN set -eu; \
    short="$(printf %.8s "${RTLSDR_COMMIT}")"; \
    tarball="/src/rtl-sdr-${short}.tar.gz"; \
    printf '%s  %s\n' "${RTLSDR_TARBALL_SHA256}" "${tarball}" | sha256sum --check --strict; \
    tar -xzf "${tarball}" -C /src; \
    case "${ARCH}" in \
      amd64) cross=; set -- ;; \
      arm64) cross=aarch64-linux-gnu-; \
             set -- -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=aarch64; \
             export PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig ;; \
      *) echo "unsupported ARCH ${ARCH}" >&2; exit 1 ;; \
    esac; \
    cmake -S "/src/rtl-sdr-${short}" -B /build/rtl-sdr "$@" \
      -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_COMPILER="${cross}gcc" \
      -DINSTALL_UDEV_RULES=OFF -DDETACH_KERNEL_DRIVER=ON -DENABLE_ZEROCOPY=OFF; \
    cmake --build /build/rtl-sdr --target rtlsdr --parallel "$(nproc)"; \
    mkdir -p /opt/rtlsdr/include /opt/rtlsdr/lib; \
    install -m 0644 "/src/rtl-sdr-${short}/include/rtl-sdr.h" \
      "/src/rtl-sdr-${short}/include/rtl-sdr_export.h" /opt/rtlsdr/include/; \
    install -m 0644 "$(readlink -f /build/rtl-sdr/src/librtlsdr.so.0)" /opt/rtlsdr/lib/librtlsdr.so.0; \
    "${cross}strip" --strip-unneeded /opt/rtlsdr/lib/librtlsdr.so.0; \
    ln -s librtlsdr.so.0 /opt/rtlsdr/lib/librtlsdr.so; \
    "${cross}readelf" -d /opt/rtlsdr/lib/librtlsdr.so.0 | grep -q 'SONAME.*\[librtlsdr\.so\.0\]'

FROM toolchain AS radiod
ARG ARCH=amd64
ARG KA9Q_COMMIT
ARG KA9Q_COMMIT_TIME
ARG KA9Q_SUMMARY
ARG KA9Q_REMOTE=https://github.com/ka9q/ka9q-radio.git
ARG RADIOD_ARCHOPTS=
COPY --from=ka9q ka9q-radio-*.tar.gz /src/
COPY --from=librtlsdr /opt/rtlsdr /opt/rtlsdr
# config_paths.h is written here rather than by upstream's Makefile rule,
# which asks git (the tarball has no .git): same values, and the private
# directories below instead of /usr/local and /etc/radio.
RUN set -eu; \
    short="$(printf %.8s "${KA9Q_COMMIT}")"; \
    tar -xzf "/src/ka9q-radio-${short}.tar.gz" -C /src; \
    cd "/src/ka9q-radio-${short}/src"; \
    lib=/usr/lib/coaxhub/radiod; \
    { printf '#ifndef _CONFIG_PATHS_H\n#define _CONFIG_PATHS_H 1\n'; \
      printf '#define CONFDIR "%s"\n#define STATEDIR "%s"\n' "$lib" /var/lib/coaxhub-radiod; \
      printf '#define PKGDATADIR "%s"\n#define PKGLIBDIR "%s"\n' "$lib" "$lib"; \
      printf '#define GIT_HASH "%s"\n#define GIT_TIME "%s"\n' "${KA9Q_COMMIT}" "${KA9Q_COMMIT_TIME}"; \
      printf '#define GIT_BRANCH " (coaxhub-stationd)"\n#define GIT_SUMMARY "%s"\n' "${KA9Q_SUMMARY}"; \
      printf '#define GIT_VERSION "%s"\n#define GIT_REMOTE_URL "%s"\n#endif\n' "${short}" "${KA9Q_REMOTE}"; \
    } > config_paths.h; \
    touch config_paths.h; \
    case "${ARCH}" in \
      amd64) cross=; archopts="${RADIOD_ARCHOPTS:--march=x86-64-v2 -mtune=generic}" ;; \
      arm64) cross=aarch64-linux-gnu-; archopts="${RADIOD_ARCHOPTS:--march=armv8-a -mtune=cortex-a53}" ;; \
      *) echo "unsupported ARCH ${ARCH}" >&2; exit 1 ;; \
    esac; \
    # upstream's Makefile runs a bare `ar`: point it at the target's.
    mkdir -p /tmp/xbin; ln -sf "/usr/bin/${cross}ar" /tmp/xbin/ar; \
    set -- CC="${cross}gcc" ARCHOPTS="${archopts}" \
      ENABLE_AIRSPY=0 ENABLE_AIRSPYHF=0 ENABLE_BLADERF=0 ENABLE_FOBOS=0 ENABLE_FUNCUBE=0 \
      ENABLE_HACKRF=0 ENABLE_HYDRASDR=0 ENABLE_RTLSDR=1 ENABLE_RX888=0 ENABLE_SDRPLAY=0 \
      ENABLE_SOAPY=0 ENABLE_SIG_GEN=0; \
    PATH="/tmp/xbin:${PATH}" make -j"$(nproc)" "$@" radiod; \
    # rtlsdr.so against the private librtlsdr: its headers, and a RUNPATH
    # to where the package installs it (never the suite's librtlsdr).
    PATH="/tmp/xbin:${PATH}" make -j"$(nproc)" "$@" INCLUDES=-I/opt/rtlsdr/include \
      LDFLAGS="-L/opt/rtlsdr/lib -Wl,--enable-new-dtags,-rpath,$lib" rtlsdr.so; \
    "${cross}readelf" -d rtlsdr.so | grep -q "RUNPATH.*\[$lib\]"; \
    "${cross}readelf" -d rtlsdr.so | grep -q 'NEEDED.*\[librtlsdr\.so\.0\]'; \
    mkdir -p /out; \
    install -m 0755 radiod /out/radiod; \
    install -m 0644 rtlsdr.so /out/rtlsdr.so; \
    install -m 0644 /opt/rtlsdr/lib/librtlsdr.so.0 /out/librtlsdr.so.0; \
    install -m 0644 ../share/presets.conf /out/presets.conf; \
    "${cross}strip" /out/radiod; \
    "${cross}strip" --strip-unneeded /out/rtlsdr.so; \
    printf '%s\n' "${archopts}" > /out/ARCHOPTS

FROM toolchain AS stationd
ARG ARCH=amd64
ARG SUITE
ARG COAXHUB_DOMAIN
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY proto ./proto
COPY crates ./crates
COPY --from=rootca root.pem /rootca/root.pem
RUN --mount=type=cache,id=coaxhub-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=coaxhub-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=coaxhub-stationd-target-${SUITE},target=/build/target \
    set -eu; \
    case "${ARCH}" in \
      amd64) triple=x86_64-unknown-linux-gnu; cross= ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cross=aarch64-linux-gnu- ;; \
      *) echo "unsupported ARCH ${ARCH}" >&2; exit 1 ;; \
    esac; \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
           CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
           AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
           COAXHUB_DOMAIN="${COAXHUB_DOMAIN}" COAXHUB_ROOT_CA=/rootca/root.pem; \
    cargo build --release --locked -p stationd --features packaged --target "${triple}"; \
    mkdir -p /out; \
    install -m 0755 "target/${triple}/release/stationd" /out/stationd; \
    "${cross}strip" /out/stationd; \
    # The crates linked into the binary, for the copyright file.
    cargo tree --locked -p stationd --features packaged --target "${triple}" -e normal \
      --prefix none --format '{p} {l}' | sed 's/ (\/build[^)]*)//; s/ (\*)$//' | sort -u > /out/rust-crates.txt

FROM toolchain AS package
ARG ARCH=amd64
ARG SUITE
ARG VERSION
ARG COAXHUB_DOMAIN
ARG KA9Q_COMMIT
ARG KA9Q_TARBALL_SHA256
ARG RTLSDR_VERSION
ARG RTLSDR_COMMIT
ARG RTLSDR_TARBALL_SHA256
ARG SOURCE_DATE_EPOCH
COPY packaging /pkgsrc
COPY --from=radiod /out /in/radiod
COPY --from=stationd /out /in/stationd
RUN ARCH="${ARCH}" SUITE="${SUITE}" VERSION="${VERSION}" COAXHUB_DOMAIN="${COAXHUB_DOMAIN}" \
    KA9Q_COMMIT="${KA9Q_COMMIT}" KA9Q_TARBALL_SHA256="${KA9Q_TARBALL_SHA256}" \
    RTLSDR_VERSION="${RTLSDR_VERSION}" RTLSDR_COMMIT="${RTLSDR_COMMIT}" \
    RTLSDR_TARBALL_SHA256="${RTLSDR_TARBALL_SHA256}" SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" \
    bash /pkgsrc/deb/assemble.sh /in /pkgsrc /out

FROM scratch AS export
COPY --from=package /out/ /
