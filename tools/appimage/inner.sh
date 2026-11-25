#!/usr/bin/env bash
# Runs inside the build container (tools/appimage/build.sh): builds the bundled libraries and
# RMC, assembles the AppDir and packs the AppImage into /out.
#
# Bundled: FreeRDP (with RMC's patches), FFmpeg (only what RMC and FreeRDP use, with x264 for
# the camera), libv4l, cJSON, uriparser, opus and further libraries not found on every system.
# OpenSSL 3 is bundled as a fallback only (usr/lib/fallback, used by AppRun when the system has
# no libssl.so.3, e.g. Arch with OpenSSL 4); system libraries such as libcups link the system's
# OpenSSL and Kerberos, so mixing them with bundled copies would break.
# Taken from the system: glibc, graphics drivers (Vulkan, VA-API, CUDA), Wayland/X11, PulseAudio,
# CUPS, Kerberos, GTK/WebKitGTK (login window), D-Bus, the CA certificates (found by AppRun).
set -euo pipefail

# >>> deps (everything up to "<<< deps" decides the cache of the built libraries)
FFMPEG_VERSION=8.1.3
X264_COMMIT=b35605ace3ddf7c1a5d67a2eb553f034aef41d55
NVHEADERS_TAG=n12.1.14.0
FREERDP_TAG=3.33.1
APPIMAGETOOL_VERSION=1.9.1
RUNTIME_VERSION=20251108

VERSION=${VERSION:?}
SRC=/out/cache/src
# Built libraries are cached per version set (rebuilt when a version or this script changes).
KEY=$( { sed -n '/^# >>> deps/,/^# <<< deps/p' /src/tools/appimage/inner.sh
	cat /src/tools/appimage/Containerfile /src/tools/freerdp/patches/*; } | sha256sum | cut -c1-12)
DEPS=/out/cache/deps-$KEY
FREERDP=/out/cache/freerdp-$KEY
WORK=/out/work
APPDIR=$WORK/AppDir
JOBS=$(nproc)
mkdir -p "$SRC" "$WORK"

step() { echo; echo "=== $*"; }

fetch() { # url file
	[[ -s "$SRC/$2" ]] || curl -fL --retry 3 -o "$SRC/$2" "$1"
}

step "Quellen"
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION.tar.xz"
fetch "https://code.videolan.org/videolan/x264/-/archive/$X264_COMMIT/x264-$X264_COMMIT.tar.gz" "x264-$X264_COMMIT.tar.gz"
fetch "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/$NVHEADERS_TAG.tar.gz" "nv-codec-headers-$NVHEADERS_TAG.tar.gz"
fetch "https://github.com/FreeRDP/FreeRDP/archive/refs/tags/$FREERDP_TAG.tar.gz" "FreeRDP-$FREERDP_TAG.tar.gz"
fetch "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage" "appimagetool-$APPIMAGETOOL_VERSION-x86_64.AppImage"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64" "runtime-$RUNTIME_VERSION-x86_64"

B=$WORK/build
rm -rf "$B" && mkdir -p "$B"
export PKG_CONFIG_PATH=$DEPS/lib/pkgconfig

# Unpacked sources are needed for the license files even when the build is cached.
tar -xf "$SRC/x264-$X264_COMMIT.tar.gz" -C "$B"
tar -xf "$SRC/nv-codec-headers-$NVHEADERS_TAG.tar.gz" -C "$B"
tar -xf "$SRC/ffmpeg-$FFMPEG_VERSION.tar.xz" -C "$B"
tar -xf "$SRC/FreeRDP-$FREERDP_TAG.tar.gz" -C "$B"
FS=$B/FreeRDP-$FREERDP_TAG

if [[ ! -f "$DEPS/.done" ]]; then
rm -rf "$DEPS"
step "x264"
(cd "$B/x264-$X264_COMMIT" && ./configure --prefix=$DEPS --enable-shared --disable-cli --enable-pic \
	&& make -j"$JOBS" && make install)

step "nv-codec-headers"
make -C "$B/nv-codec-headers-$NVHEADERS_TAG" PREFIX=$DEPS install

step "FFmpeg $FFMPEG_VERSION"
(cd "$B/ffmpeg-$FFMPEG_VERSION" && ./configure --prefix=$DEPS \
	--enable-shared --disable-static --disable-programs --disable-doc --disable-debug \
	--disable-autodetect --disable-everything --enable-gpl \
	--enable-avcodec --enable-avutil --enable-swscale --enable-swresample \
	--disable-avformat --disable-avdevice --disable-avfilter \
	--enable-libx264 --enable-vaapi --enable-ffnvcodec --enable-nvdec --enable-nvenc \
	--enable-hwaccel=h264_vaapi,h264_nvdec \
	--enable-decoder=h264,mjpeg,aac,aac_fixed,mp3,mp3float,opus,adpcm_ms,adpcm_ima_oki,gsm_ms,g723_1,pcm_alaw,pcm_mulaw,pcm_s16le,pcm_u8,pcm_u16le \
	--enable-encoder=libx264,h264_vaapi,h264_nvenc,aac,adpcm_ms,g723_1,pcm_alaw,pcm_mulaw,pcm_s16le \
	--enable-parser=h264,aac,mpegaudio,mjpeg,opus \
	&& make -j"$JOBS" && make install)
touch "$DEPS/.done"
else
	step "x264, FFmpeg: aus dem Cache ($DEPS)"
fi

if [[ ! -f "$FREERDP/.done" ]]; then
rm -rf "$FREERDP"
step "FreeRDP $FREERDP_TAG"
for p in /src/tools/freerdp/patches/"$FREERDP_TAG"-*.patch; do
	[[ -f "$p" ]] && patch -d "$FS" -p1 <"$p"
done
cmake -S "$FS" -B "$B/freerdp-build" -G Ninja \
	-DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=$FREERDP -DCMAKE_INSTALL_LIBDIR=lib64 \
	-DCMAKE_INSTALL_RPATH='$ORIGIN' -DCMAKE_PREFIX_PATH=$DEPS \
	-DWITH_FFMPEG=ON -DWITH_VIDEO_FFMPEG=ON -DWITH_DSP_FFMPEG=ON -DWITH_SWSCALE=ON \
	-DWITH_FFMPEG_HWACCEL=ON -DWITH_OPENH264=OFF \
	-DWITH_AAD=ON -DWITH_SSO_MIB=OFF -DWITH_WEBVIEW_AAD_AUTH_HELPER=OFF -DWITH_XDG_AAD_AUTH_HELPER=ON \
	-DWITH_CLIENT_SDL=OFF -DWITH_X11=OFF -DWITH_WAYLAND=OFF \
	-DWITH_PULSE=ON -DWITH_ALSA=OFF -DWITH_OSS=OFF -DWITH_CUPS=ON -DWITH_FUSE=OFF \
	-DCHANNEL_RDPECAM=ON -DCHANNEL_RDPECAM_CLIENT=ON -DCHANNEL_URBDRC=OFF \
	-DWITH_PCSC=OFF -DWITH_PKCS11=OFF -DWITH_GSTREAMER_1_0=OFF -DWITH_FAAD2=OFF -DWITH_FAAC=OFF \
	-DWITH_SOXR=OFF -DWITH_LAME=OFF -DWITH_OPUS=ON -DWITH_CJSON_REQUIRED=ON -DWITH_JSONC_REQUIRED=OFF \
	-DWITH_UNICODE_BUILTIN=ON \
	-DWITH_SERVER=OFF -DWITH_SHADOW=OFF -DWITH_PROXY=OFF -DWITH_SAMPLE=OFF \
	-DWITH_MANPAGES=OFF -DBUILD_TESTING=OFF
cmake --build "$B/freerdp-build" -j"$JOBS"
cmake --install "$B/freerdp-build"
touch "$FREERDP/.done"
else
	step "FreeRDP: aus dem Cache ($FREERDP)"
fi
# <<< deps

step "RMC $VERSION"
rm -rf "$WORK/rmc-src" && mkdir -p "$WORK/rmc-src"
(cd /src && tar --exclude=./target --exclude=./local --exclude=./.cargo -cf - .) | tar -xf - -C "$WORK/rmc-src"
(cd "$WORK/rmc-src" && \
	CARGO_HOME=/out/cache/cargo CARGO_TARGET_DIR=$WORK/target \
	FREERDP_PREFIX=$FREERDP RMC_RPATH='$ORIGIN/../lib' \
	cargo build --release --locked -p rmc --bin rmc)

step "AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/licenses"
install -m755 "$WORK/target/release/rmc" "$APPDIR/usr/bin/rmc"
install -m755 "$WORK/target/release/rmc-login" "$APPDIR/usr/bin/rmc-login"
# FreeRDP's browser login helpers (Python scripts; fallback without WebKitGTK).
install -m755 "$FREERDP/libexec/freerdp-xdg-aad-helper" "$FREERDP/libexec/freerdp-aad-handler" "$APPDIR/usr/bin/"

# Libraries from the system: glibc, drivers and desktop integration (excluded from bundling).
EXCLUDE='^(ld-linux.*|libc|libm|libdl|librt|libpthread|libresolv|libutil|libanl|libnsl|libgcc_s|libstdc\+\+|libGL.*|libEGL|libGLX|libGLdispatch|libOpenGL|libvulkan|libdrm.*|libgbm|libwayland-.*|libxkbcommon.*|libX.*|libxcb.*|libva.*|libpulse.*|libcups|libssl|libcrypto|libkrb5.*|libgssapi.*|libk5crypto|libcom_err|libkeyutils|libz|libdbus-1|libsystemd|libudev|libglib-2.0|libgobject-2.0|libgio-2.0|libgmodule-2.0|libgthread-2.0|libffi|libgtk-3|libgdk-3|libwebkit2gtk.*|libjavascriptcore.*|libsoup.*|libsecret-1|libasound|libfontconfig|libfreetype|libharfbuzz|libexpat|libuuid|libblkid|libmount|libselinux|libpcre.*|libcap|libapparmor|libsndfile|libFLAC|libogg|libvorbis.*|libgcrypt|libgpg-error|liblzma|libbz2|libzstd|liblz4|libavahi.*|libgnutls|libnettle|libhogweed|libgmp|libidn2|libunistring|libtasn1|libp11-kit|libbrotli.*|libpng16|libjbig|libtiff|libwebp.*|libsharpyuv|libdeflate|libLerc|libcuda|libnvcuvid|libnvidia.*)\.so'

declare -A seen
bundle() { # object: copy its direct, non-excluded dependencies into usr/lib (recursively)
	local obj=$1 name path
	for name in $(readelf -d "$obj" | awk '/NEEDED/ {gsub(/[][]/, "", $5); print $5}'); do
		[[ "$name" =~ $EXCLUDE ]] && continue
		[[ -n "${seen[$name]:-}" ]] && continue
		seen[$name]=1
		path=$(LD_LIBRARY_PATH=$FREERDP/lib64:$DEPS/lib ldd "$obj" | awk -v n="$name" '$1 == n {print $3}')
		[[ -f "$path" ]] || { echo "nicht gefunden: $name (für $obj)" >&2; exit 1; }
		cp -L "$path" "$APPDIR/usr/lib/$name"
		bundle "$APPDIR/usr/lib/$name"
	done
}
bundle "$APPDIR/usr/bin/rmc"
# Bundled libraries find each other.
for l in "$APPDIR"/usr/lib/*.so*; do patchelf --set-rpath '$ORIGIN' "$l"; done
# OpenSSL 3 as fallback (see above).
mkdir -p "$APPDIR/usr/lib/fallback"
for n in libssl.so.3 libcrypto.so.3; do
	cp -L "$(LD_LIBRARY_PATH=$FREERDP/lib64 ldd "$FREERDP/lib64/libwinpr3.so.3" | awk -v n=$n '$1 == n {print $3}')" "$APPDIR/usr/lib/fallback/$n"
done
# Legacy provider (MD4/RC4 for NTLM, RDP licensing): loaded from OPENSSL_MODULES.
mkdir -p "$APPDIR/usr/lib/fallback/ossl-modules"
cp -L /usr/lib/x86_64-linux-gnu/ossl-modules/legacy.so "$APPDIR/usr/lib/fallback/ossl-modules/"
echo "gebündelt: $(ls "$APPDIR/usr/lib" | tr '\n' ' ')"
echo "vom System: $(for o in "$APPDIR"/usr/bin/rmc "$APPDIR"/usr/lib/*.so*; do readelf -d "$o" | awk '/NEEDED/ {gsub(/[][]/, "", $5); print $5}'; done | sort -u | while read -r n; do [[ -f "$APPDIR/usr/lib/$n" ]] || printf '%s ' "$n"; done)"

# A bundled library must not depend on a library missing from both the bundle and the
# system lists above (would fail on other systems): show anything not resolvable inside.
missing=$(for o in "$APPDIR"/usr/bin/rmc "$APPDIR"/usr/lib/*.so*; do
	LD_LIBRARY_PATH=$APPDIR/usr/lib:$APPDIR/usr/lib/fallback ldd "$o" | awk '/not found/ {print $1}'; done | sort -u)
[[ -z "$missing" ]] || { echo "fehlende Bibliotheken: $missing" >&2; exit 1; }

cp /src/assets/icons/rmc-256.png "$APPDIR/rmc.png"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"
cp /src/assets/icons/rmc-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/rmc.png"
cat >"$APPDIR/rmc.desktop" <<EOD
[Desktop Entry]
Type=Application
Name=RMC
GenericName=Remotedesktop
Comment=Remote Machine Connect: Verbindung zu Windows-Rechnern per RDP
Exec=rmc
Icon=rmc
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=rmc
Keywords=RDP;Remote;Windows;Remotedesktop;RMC;
EOD
cat >"$APPDIR/AppRun" <<'EOD'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
# The bundled OpenSSL needs the system's CA certificates (Entra ID sign-in over HTTPS).
if [ -z "$SSL_CERT_FILE" ]; then
	for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
		/etc/ssl/ca-bundle.pem /etc/ssl/cert.pem /etc/pki/tls/cacert.pem; do
		if [ -r "$f" ]; then export SSL_CERT_FILE="$f"; break; fi
	done
fi
[ -z "$SSL_CERT_DIR" ] && [ -d /etc/ssl/certs ] && export SSL_CERT_DIR=/etc/ssl/certs
# OpenSSL 3 from the system if present (shared with system libraries), else the bundled one.
have_ssl3=
for d in /usr/lib64 /usr/lib /usr/lib/x86_64-linux-gnu /lib64 /lib/x86_64-linux-gnu; do
	[ -e "$d/libssl.so.3" ] && have_ssl3=1 && break
done
if [ -z "$have_ssl3" ]; then
	export LD_LIBRARY_PATH="$HERE/usr/lib/fallback${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
	export OPENSSL_MODULES="$HERE/usr/lib/fallback/ossl-modules"
fi
exec "$HERE/usr/bin/rmc" "$@"
EOD
chmod +x "$APPDIR/AppRun"

# Licenses: RMC, FreeRDP, FFmpeg (GPL build with x264), notice about the source archive.
cp /src/LICENSE "$APPDIR/usr/share/licenses/RMC-LICENSE"
cp "$FS/LICENSE" "$APPDIR/usr/share/licenses/FreeRDP-LICENSE"
cp "$B/ffmpeg-$FFMPEG_VERSION/COPYING.GPLv2" "$APPDIR/usr/share/licenses/FFmpeg-COPYING.GPLv2"
cp "$B/x264-$X264_COMMIT/COPYING" "$APPDIR/usr/share/licenses/x264-COPYING"
for pkg in libssl3 libcjson1 libusb-1.0-0; do
	cp "/usr/share/doc/$pkg/copyright" "$APPDIR/usr/share/licenses/$pkg-copyright"
done
cat >"$APPDIR/usr/share/licenses/README" <<EOD
RMC $VERSION bundles FreeRDP $FREERDP_TAG (Apache-2.0, with RMC's patches), FFmpeg $FFMPEG_VERSION
(GPL-2.0-or-later build, configured with x264) and x264 ($X264_COMMIT, GPL-2.0-or-later),
plus OpenSSL 3 (fallback), cJSON and libusb from Ubuntu 22.04 under their own licenses
(see the *-copyright files). The complete corresponding sources
are published with each release as RMC-$VERSION-third-party-sources.tar.xz.
EOD

step "AppImage"
TOOL=$WORK/appimagetool
cp "$SRC/appimagetool-$APPIMAGETOOL_VERSION-x86_64.AppImage" "$TOOL" && chmod +x "$TOOL"
OUTFILE=/out/RMC-$VERSION-x86_64.AppImage
rm -f "$OUTFILE"
ARCH=x86_64 "$TOOL" --appimage-extract-and-run --no-appstream \
	--runtime-file "$SRC/runtime-$RUNTIME_VERSION-x86_64" "$APPDIR" "$OUTFILE"

step "Quellen der gebündelten Bibliotheken"
SA=$WORK/sources/RMC-$VERSION-third-party-sources
rm -rf "$WORK/sources" && mkdir -p "$SA/freerdp-patches"
cp "$SRC/ffmpeg-$FFMPEG_VERSION.tar.xz" "$SRC/x264-$X264_COMMIT.tar.gz" \
	"$SRC/nv-codec-headers-$NVHEADERS_TAG.tar.gz" "$SRC/FreeRDP-$FREERDP_TAG.tar.gz" "$SA/"
cp /src/tools/freerdp/patches/"$FREERDP_TAG"-*.patch "$SA/freerdp-patches/"
cp /src/tools/appimage/Containerfile /src/tools/appimage/inner.sh "$SA/"
cat >"$SA/README" <<EOD
Sources of the libraries bundled in RMC-$VERSION-x86_64.AppImage. Build: see inner.sh
(FFmpeg configure flags, FreeRDP CMake options); environment: Containerfile (Ubuntu 22.04).
Other bundled Ubuntu 22.04 libraries (libv4l, cJSON, uriparser, opus, ...): Ubuntu source
packages of jammy.
EOD
tar -C "$WORK/sources" -cJf "/out/RMC-$VERSION-third-party-sources.tar.xz" "RMC-$VERSION-third-party-sources"
echo "fertig: $OUTFILE"
