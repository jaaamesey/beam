#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
vcpkg_root="$repo_root/.vcpkg"
vcpkg_revision=3723ec118c8354290925feb58d021a9205a3e772

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    triplet=arm64-osx-static-release
    feature=
    ;;
  Linux:x86_64)
    triplet=x64-linux-static-release
    feature=linux-hardware
    ;;
  *)
    echo "unsupported host: $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

if [ ! -d "$vcpkg_root/.git" ]; then
  git clone https://github.com/microsoft/vcpkg.git "$vcpkg_root"
fi

git -C "$vcpkg_root" fetch --depth 1 origin "$vcpkg_revision"
git -C "$vcpkg_root" checkout --detach "$vcpkg_revision"
"$vcpkg_root/bootstrap-vcpkg.sh" -disableMetrics

set -- install \
  "--triplet=$triplet" \
  "--overlay-triplets=$repo_root/vcpkg-triplets" \
  "--x-install-root=$vcpkg_root/installed" \
  --clean-buildtrees-after-build \
  --clean-packages-after-build

if [ -n "$feature" ]; then
  set -- "$@" "--x-feature=$feature"
fi

"$vcpkg_root/vcpkg" "$@"

test -f "$vcpkg_root/installed/$triplet/lib/libavcodec.a"
echo "Static FFmpeg is ready for Cargo ($triplet)."
