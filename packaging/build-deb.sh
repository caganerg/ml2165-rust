#!/bin/bash
# Build the ml2165-rust Debian package.
#
#   packaging/build-deb.sh [amd64|arm64]      (default: amd64)
#
# Writes dist/ml2165-rust_<version>_<arch>.deb. Nothing here runs as root or
# touches the system; installing the result is a separate, deliberate step
# (`sudo apt install ./dist/...`).
#
# The version comes from the first line of packaging/debian/changelog. The
# script refuses to build if Cargo.toml or the PPD's *FileVersion disagree,
# and if the binary needs a newer glibc/libgcc than control.in declares.
#
# Packages are reproducible: every timestamp is set to the last commit's date
# (SOURCE_DATE_EPOCH) and ownership is forced to root:root, so the same commit
# and toolchain give a byte-identical .deb whoever builds it and whatever their
# umask.
set -euo pipefail
umask 022
# Tool output (readelf, dpkg-deb) is parsed below and must not be localised.
export LC_ALL=C

die() { echo "build-deb: error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."

PKG=ml2165-rust
ARCH=${1:-amd64}
case "$ARCH" in
    amd64) TARGET=x86_64-unknown-linux-gnu ;;
    arm64) TARGET=aarch64-unknown-linux-gnu
           # Cross-linking with the Debian cross toolchain (gcc-aarch64-linux-gnu).
           export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-aarch64-linux-gnu-gcc} ;;
    *) die "unknown architecture '$ARCH' (expected amd64 or arm64)" ;;
esac

# ---- Version: one source, two cross-checks ---------------------------------
DEB_VERSION=$(sed -n "1s/^$PKG (\([^)]*\)).*/\1/p" packaging/debian/changelog)
[ -n "$DEB_VERSION" ] || die "cannot read '$PKG (<version>)' from the first line of packaging/debian/changelog"
UPSTREAM=${DEB_VERSION%-*}
CARGO_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
PPD_VERSION=$(sed -n 's/^\*FileVersion: "\(.*\)"$/\1/p' ppd/samsung-ml2165.ppd)
[ "$CARGO_VERSION" = "$UPSTREAM" ] || die "Cargo.toml says $CARGO_VERSION, changelog says $UPSTREAM"
[ "$PPD_VERSION" = "$UPSTREAM" ] || die "PPD *FileVersion says $PPD_VERSION, changelog says $UPSTREAM"

if [ -n "$(git status --porcelain)" ]; then
    echo "build-deb: warning: uncommitted changes; this package does not match any commit" >&2
fi

# ---- Prerequisites for cross builds ----------------------------------------
if [ "$ARCH" = arm64 ]; then
    libdir=$(rustc --print target-libdir --target "$TARGET")
    compgen -G "$libdir/libstd-*.rlib" >/dev/null ||
        die "no Rust standard library for $TARGET in $libdir; install libstd-rust-dev:arm64 (see README)"
    command -v "$CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER" >/dev/null ||
        die "linker $CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER not found; install gcc-aarch64-linux-gnu"
fi

# ---- Test and build ---------------------------------------------------------
# Tests run on the build machine; the code has no architecture-specific paths.
cargo test --release --locked
cargo build --release --locked --target "$TARGET"
BIN=target/$TARGET/release/rastertospl-rust

case "$ARCH" in
    amd64) want_machine='X86-64' ;;
    arm64) want_machine='AArch64' ;;
esac
machine=$(readelf -h "$BIN" | sed -n 's/^ *Machine: *//p')
[[ "$machine" == *"$want_machine"* ]] || die "$BIN is a '$machine' binary, not $ARCH"

# ---- Declared vs. required library versions --------------------------------
# The highest symbol version the binary uses must not exceed what control.in
# promises, or the package would install on systems where it cannot run.
check_dep() {   # $1 = symbol prefix, $2 = Debian package name
    local need decl
    need=$(readelf -W --dyn-syms "$BIN" | grep -o "@$1_[0-9.]*" | sed "s/^@$1_//" | sort -uV | tail -1)
    decl=$(sed -n "s/.*$2 (>= \([0-9.]*\)).*/\1/p" packaging/debian/control.in)
    [ -n "$decl" ] || die "control.in declares no '$2 (>= ...)' dependency"
    # Debian's symbols files map e.g. GCC_4.2.0 to libgcc-s1 (>= 4.2), and
    # dpkg orders 4.2.0 after 4.2, so trailing ".0" components are dropped.
    local need_cmp
    need_cmp=$(sed -E 's/(\.0)+$//' <<<"$need")
    if [ -n "$need" ] && ! dpkg --compare-versions "$need_cmp" le "$decl"; then
        die "binary needs $1_$need but control.in declares $2 (>= $decl)"
    fi
    echo "build-deb: $2: binary needs ${need:-nothing}, control.in declares >= $decl"
}
check_dep GLIBC libc6
check_dep GCC libgcc-s1

if command -v cupstestppd >/dev/null; then
    # -I filters: the filter is not necessarily installed on the build machine.
    cupstestppd -q -I filters ppd/samsung-ml2165.ppd || die "cupstestppd rejected the PPD"
fi

# ---- Assemble the package tree ----------------------------------------------
STAGE=target/deb/$ARCH
ROOT=$STAGE/root
DOC=usr/share/doc/$PKG
rm -rf "$STAGE"
mkdir -p "$ROOT"
chmod 755 "$ROOT"

install -D -m 755 "$BIN" "$ROOT/usr/lib/cups/filter/rastertospl-rust"
install -D -m 644 ppd/samsung-ml2165.ppd "$ROOT/usr/share/ppd/$PKG/samsung-ml2165.ppd"
install -D -m 644 packaging/debian/copyright "$ROOT/$DOC/copyright"
gzip -9n < packaging/debian/changelog > "$ROOT/$DOC/changelog.Debian.gz"
gzip -9n < README.md > "$ROOT/$DOC/README.md.gz"
chmod 644 "$ROOT/$DOC"/*.gz

INSTALLED_SIZE=$(du -sk --apparent-size "$ROOT" | cut -f1)
mkdir -m 755 "$ROOT/DEBIAN"
sed -e "s/@VERSION@/$DEB_VERSION/" -e "s/@ARCH@/$ARCH/" packaging/debian/control.in |
    awk -v s="$INSTALLED_SIZE" '/^Description:/ && !d { print "Installed-Size: " s; d = 1 } { print }' \
    > "$ROOT/DEBIAN/control"
( cd "$ROOT" && find usr -type f -print0 | LC_ALL=C sort -z | xargs -0 md5sum ) > "$ROOT/DEBIAN/md5sums"
chmod 644 "$ROOT/DEBIAN/control" "$ROOT/DEBIAN/md5sums"
if grep '@[A-Z]*@' "$ROOT/DEBIAN/control"; then die "unfilled placeholder in control"; fi

# ---- Build ------------------------------------------------------------------
mkdir -p dist
OUT=dist/${PKG}_${DEB_VERSION}_${ARCH}.deb
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) \
    dpkg-deb --root-owner-group -Zxz --build "$ROOT" "$OUT" >/dev/null

echo
dpkg-deb --info "$OUT"
echo
dpkg-deb --contents "$OUT"
echo
sha256sum "$OUT"
