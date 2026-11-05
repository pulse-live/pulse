#!/usr/bin/env bash
# Package a previously built, native musl binary without rebuilding or activating it.
set -euo pipefail

target=${1:?Usage: package-release.sh TARGET [default|telemetry] [OUTPUT_DIR]}
feature_set=${2:-default}
output_dir=${3:-dist}
case "$target" in
    x86_64-unknown-linux-musl) deb_arch=amd64; rpm_arch=x86_64; machine='Advanced Micro Devices X86-64' ;;
    aarch64-unknown-linux-musl) deb_arch=arm64; rpm_arch=aarch64; machine=AArch64 ;;
    *) echo 'Unsupported release target' >&2; exit 1 ;;
esac
package=pulse
suffix=''
variant=()
case "$feature_set" in
    default) ;;
    telemetry) package=pulse-telemetry; suffix=-telemetry; variant=(--variant telemetry) ;;
    *) echo 'Feature set must be default or telemetry' >&2; exit 1 ;;
esac
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')
# Tilde sorts a native prerelease before the corresponding stable package.
# Escape it so Bash cannot expand the replacement to a home directory.
native_version=${version//-/\~}
binary="${CARGO_TARGET_DIR:-target}/$target/release/pulse"
stage_dir=$(mktemp -d)
trap 'rm -rf "$stage_dir"' EXIT

[[ $("$binary" -V) == "pulse $version" ]] || { echo 'Binary version differs from Cargo.toml' >&2; exit 1; }
LC_ALL=C readelf -h -l "$binary" > "$stage_dir/elf.txt"
grep -Fq "$machine" "$stage_dir/elf.txt" || { echo 'Binary architecture differs from target' >&2; exit 1; }
if grep -q INTERP "$stage_dir/elf.txt"; then
    echo 'Release packages require a static binary without an ELF interpreter' >&2
    exit 1
fi
# Validate feature selection without creating an exporter or making a network request.
if env OTEL_EXPORTER_OTLP_ENDPOINT=invalid-endpoint OTEL_EXPORTER_OTLP_PROTOCOL=grpc \
    OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=grpc RUST_LOG=off NO_COLOR=1 \
    "$binary" config validate --file examples/pulse.toml > /dev/null 2> "$stage_dir/feature.txt"; then
    [[ "$feature_set" == default ]] || { echo 'Telemetry capability is missing' >&2; exit 1; }
else
    [[ "$feature_set" == telemetry ]] && grep -q 'invalid OTEL_EXPORTER_OTLP_ENDPOINT' "$stage_dir/feature.txt" || {
        echo 'Binary feature set does not match the package variant' >&2; exit 1;
    }
fi

mkdir -p "$output_dir"
archive="pulse-$version-$target$suffix"
mkdir -p "$stage_dir/$archive/.github"
cp "$binary" LICENSE README.md AGENTS.md "$stage_dir/$archive/"
cp .github/SECURITY.md "$stage_dir/$archive/.github/"
cp -R examples docs proto "$stage_dir/$archive/"
tar -C "$stage_dir" -czf "$output_dir/$archive.tar.gz" "$archive"

deb_name="${package}_${native_version}-1_${deb_arch}.deb"
rpm_name="$package-$native_version-1.$rpm_arch.rpm"
cargo deb --locked --no-build --no-strip --no-dbgsym --target "$target" \
    "${variant[@]}" --deb-version "$native_version-1" --output "$output_dir/$deb_name"
cargo generate-rpm --target "$target" "${variant[@]}" --payload-compress gzip \
    --set-metadata "version = '$native_version'" --output "$output_dir/$rpm_name"

# Check labels, payload identity and install behavior before producing checksums.
[[ $(dpkg-deb -f "$output_dir/$deb_name" Package) == "$package" ]]
[[ $(dpkg-deb -f "$output_dir/$deb_name" Architecture) == "$deb_arch" ]]
[[ $(dpkg-deb -f "$output_dir/$deb_name" Version) == "$native_version-1" ]]
[[ $(rpm -qp --queryformat '%{NAME}' "$output_dir/$rpm_name") == "$package" ]]
[[ $(rpm -qp --queryformat '%{ARCH}' "$output_dir/$rpm_name") == "$rpm_arch" ]]
[[ $(rpm -qp --queryformat '%{VERSION}-%{RELEASE}' "$output_dir/$rpm_name") == "$native_version-1" ]]
dpkg-deb --control "$output_dir/$deb_name" "$stage_dir/control"
for script in preinst postinst prerm postrm triggers; do
    [[ ! -e "$stage_dir/control/$script" ]] || { echo 'Unexpected package installation script' >&2; exit 1; }
done
[[ -z $(rpm -qp --scripts "$output_dir/$rpm_name") ]] || { echo 'Unexpected RPM scriptlet' >&2; exit 1; }

dpkg-deb --extract "$output_dir/$deb_name" "$stage_dir/deb"
mkdir "$stage_dir/rpm"
rpm2cpio "$output_dir/$rpm_name" | (cd "$stage_dir/rpm" && cpio -idm --quiet --no-absolute-filenames --no-preserve-owner)
for format in deb rpm; do
    extracted="$stage_dir/$format/usr/bin/pulse"
    cmp "$binary" "$extracted"
    [[ $(stat -c '%a' "$extracted") == 755 ]]
    "$extracted" --help > /dev/null
    env -u OTEL_EXPORTER_OTLP_ENDPOINT RUST_LOG=off "$extracted" config validate --file "$stage_dir/$format/usr/share/doc/pulse/examples/pulse.toml"
    env -u OTEL_EXPORTER_OTLP_ENDPOINT RUST_LOG=off "$extracted" config validate --file "$stage_dir/$format/usr/share/doc/pulse/examples/managed-edge.toml"
done
(cd "$output_dir" && sha256sum "$archive.tar.gz" > "$archive.tar.gz.sha256" \
    && sha256sum "$deb_name" > "$deb_name.sha256" && sha256sum "$rpm_name" > "$rpm_name.sha256")
