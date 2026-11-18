#!/usr/bin/env bash
set -euo pipefail

app_id=io.github.potik.Downloads
runtime_version=50
rust_extension=org.freedesktop.Sdk.Extension.rust-stable
aria_version=1.37.0
aria_sha256=60a420ad7085eb616cb6e2bdf0a7206d68ff3d37fb5a956dc44242eb2f79b66b
flathub_repo=https://dl.flathub.org/repo/flathub.flatpakrepo

project=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
arch=$(flatpak --default-arch)
case "$arch" in
    x86_64|aarch64) ;;
    *) printf 'Unsupported Flatpak architecture: %s\n' "$arch" >&2; exit 1 ;;
esac
output=$(realpath -m -- "${1:-Potik-$arch.flatpak}")
jobs=${POTIK_BUILD_JOBS:-4}
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT

source_dir="$stage/source"
build_dir="$stage/flatpak"
mkdir -p -- "$source_dir/.cargo" "$(dirname -- "$output")"
cp -R -- "$project/src" "$project/packaging" "$project/build.rs" "$project/Cargo.toml" +    "$project/Cargo.lock" "$source_dir/"
(
    cd -- "$source_dir"
    cargo vendor --locked vendor > .cargo/config.toml
)

archive="$source_dir/aria2-$aria_version.tar.xz"
if [[ -n "${POTIK_ARIA_SOURCE:-}" ]]; then
    cp -- "$POTIK_ARIA_SOURCE" "$archive"
else
    curl --fail --location --retry 3 --output "$archive" \
        "https://github.com/aria2/aria2/releases/download/release-$aria_version/aria2-$aria_version.tar.xz"
fi
printf '%s  %s\n' "$aria_sha256" "$archive" | sha256sum --check

flatpak build-init --arch="$arch" --sdk-extension="$rust_extension" \
    "$build_dir" "$app_id" org.gnome.Sdk org.gnome.Platform "$runtime_version"

flatpak build --bind-mount="/run/build/potik=$source_dir" --build-dir=/run/build/potik \
    "$build_dir" /bin/bash -se -- "$app_id" "$aria_version" "$jobs" <<'BUILD'
app_id=$1
aria_version=$2
jobs=$3
export PATH="/usr/lib/sdk/rust-stable/bin:$PATH"
export CARGO_HOME=/run/build/potik/.cargo
export CARGO_PROFILE_RELEASE_STRIP=symbols

cargo build --release --locked --offline --jobs "$jobs"
install -Dm755 target/release/potik /app/bin/potik

tar -xf "aria2-$aria_version.tar.xz"
cd -- "aria2-$aria_version"
CXXFLAGS='-O2 -g0 -std=c++17' ./configure --prefix=/app \
    --disable-nls --without-gnutls --without-libgcrypt --without-libssh2
make -C lib -j "$jobs"
make -C deps -j "$jobs"
make -C src -j "$jobs" aria2c
install -m755 src/aria2c /app/bin/aria2c
strip --strip-unneeded /app/bin/aria2c
install -Dm644 COPYING /app/share/licenses/aria2/COPYING
cd ..
install -Dm644 "aria2-$aria_version.tar.xz" "/app/share/potik/source/aria2-$aria_version.tar.xz"

install -Dm644 "packaging/$app_id.desktop" "/app/share/applications/$app_id.desktop"
install -Dm644 "packaging/$app_id.metainfo.xml" "/app/share/metainfo/$app_id.metainfo.xml"
install -Dm644 "packaging/$app_id.png" "/app/share/icons/hicolor/256x256/apps/$app_id.png"
BUILD

flatpak build-finish --command=potik --share=network --share=ipc \
    --socket=wayland --socket=fallback-x11 --filesystem=xdg-download:create \
    --env=GSK_RENDERER=cairo "$build_dir"
flatpak build-export "$stage/repo" "$build_dir" stable
flatpak build-bundle --arch="$arch" --runtime-repo="$flathub_repo" \
    "$stage/repo" "$output" "$app_id" stable
printf '\n%s\n' "$output"
