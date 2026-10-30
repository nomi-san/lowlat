#!/usr/bin/env bash
# Regenerate the system video decoder's bindings from the macOS SDK headers.
#
# Run by hand on a Mac, never from a build script: the generated file is
# committed so that no build machine needs the SDK. Needs Xcode or its command
# line tools (the SDK and libclang) and the binding generator:
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-vt-bindings.sh
#
# **The SDK's headers are not vendored**, so the SDK installed on the machine
# is the source; its version is written into the generated file's first line,
# and a regeneration against another one shows in the diff. Every struct
# carries a compile-time layout assertion derived from the header, checked on
# each architecture the crate is built for.
#
# Types, constants and function-pointer types only: the frameworks are opened
# at run time and their entry points resolved by name, so nothing here is
# declared to the linker. The entry points are generated as declarations and
# rewritten into the types of pointers to them, so their signatures are the
# SDK's rather than transcribed.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/crates/drivers/src/ffi/macos"
sdk="$(xcrun --sdk macosx --show-sdk-path)"
version="$(xcrun --sdk macosx --show-sdk-version)"
[ -f "$sdk/System/Library/Frameworks/VideoToolbox.framework/Headers/VideoToolbox.h" ] ||
    { echo "no VideoToolbox headers in $sdk" >&2; exit 1; }
mkdir -p "$out"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/vt.h" <<'EOF'
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <IOSurface/IOSurfaceRef.h>
#include <VideoToolbox/VideoToolbox.h>
EOF

echo "generating vt bindings against macOS SDK $version"
bindgen "$work/vt.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --raw-line "// macOS SDK $version." \
    `# The objects passed by reference, and the plain types their calls take.` \
    --allowlist-type 'CF(TypeRef|AllocatorRef|StringRef|DictionaryRef|MutableDictionaryRef)' \
    --allowlist-type 'CF(ArrayRef|MutableArrayRef|NumberRef|BooleanRef|Index|NumberType)' \
    --allowlist-type 'CF(DictionaryKeyCallBacks|DictionaryValueCallBacks|ArrayCallBacks)' \
    --allowlist-type 'Boolean|OSStatus|OSType' \
    --allowlist-type 'CM(BlockBufferRef|SampleBufferRef|FormatDescriptionRef|VideoFormatDescriptionRef)' \
    --allowlist-type 'CM(ItemCount|BlockBufferFlags|BlockBufferCustomBlockSource|VideoDimensions)' \
    --allowlist-type 'CMSampleTimingInfo|CMTime' \
    --allowlist-type 'CV(PixelBufferRef|ImageBufferRef|BufferRef|Return|OptionFlags|PixelBufferLockFlags)' \
    --allowlist-type 'VT(DecompressionSessionRef|SessionRef|DecodeFrameFlags|DecodeInfoFlags)' \
    --allowlist-type 'VTDecompressionOutputCallback(Record)?' \
    --allowlist-type 'IOSurface(Ref|ID)' \
    `# The constants: the layouts asked for, the flags, the codes answered.` \
    --allowlist-var 'kCVPixelFormatType_(420|444)YpCbCr(8|10)BiPlanar(Video|Full)Range' \
    --allowlist-type '_CVReturn' \
    --allowlist-var 'kCVPixelBufferLock_ReadOnly|kCFNumberSInt32Type' \
    --allowlist-var 'kVTDecodeFrame_.*|kVTDecodeInfo_.*' \
    --allowlist-var 'kVT(InvalidSession|VideoDecoder[A-Za-z]+|CouldNotFindVideoDecoder|ParameterErr|AllocationFailed)Err' \
    --allowlist-var 'kCMVideoCodecType_(H264|HEVC)' \
    `# The entry points the table resolves, rewritten below.` \
    --allowlist-function 'CF(Release|Retain|DictionaryCreateMutable|DictionarySetValue)' \
    --allowlist-function 'CF(ArrayCreateMutable|ArrayAppendValue|NumberCreate|BooleanGetValue)' \
    --allowlist-function 'CMVideoFormatDescriptionCreateFrom(H264|HEVC)ParameterSets' \
    --allowlist-function 'CMBlockBufferCreateWithMemoryBlock|CMSampleBufferCreateReady' \
    --allowlist-function 'CVPixelBufferGet(PixelFormatType|Width|Height|PlaneCount)' \
    --allowlist-function 'CVPixelBufferGet(BytesPerRowOfPlane|BaseAddressOfPlane|HeightOfPlane)' \
    --allowlist-function 'CVPixelBuffer(LockBaseAddress|UnlockBaseAddress|Retain|Release)' \
    `# A picture's surface, for the handle kind.` \
    --allowlist-function 'CVPixelBufferGetIOSurface|IOSurfaceGet(ID|AllocSize)' \
    --allowlist-function 'VTDecompressionSession(Create|DecodeFrame|WaitForAsynchronousFrames)' \
    --allowlist-function 'VTDecompressionSession(Invalidate|CanAcceptFormatDescription)' \
    --allowlist-function 'VTSessionCopyProperty' \
    --merge-extern-blocks \
    -o "$work/vt.rs" \
    -- -x c -isysroot "$sdk" --target=arm64-apple-macos11

# Each declaration becomes the type of a pointer to it, named as the entry
# point is; the extern block goes.
/usr/bin/python3 - "$work/vt.rs" "$out/vt.rs" <<'PY'
import re
import sys

src, dst = sys.argv[1], sys.argv[2]
text = open(src, encoding="utf-8").read()
block = re.search(r'unsafe extern "C" \{(.*?)\n\}\n', text, re.S)
if not block:
    raise SystemExit("no extern block in the generated file")
fns = re.findall(r"pub fn (\w+)\((.*?)\)(\s*->\s*[^;]+?)?;", block.group(1), re.S)
if len(fns) < 20:
    raise SystemExit("only %d entry points; the allowlist or the headers changed" % len(fns))
types = ["pub type %s = unsafe extern \"C\" fn(%s)%s;" % (name, " ".join(args.split()), ret or "")
         for name, args, ret in fns]
text = text[: block.start()] + "\n".join(types) + "\n" + text[block.end():]
if "extern \"C\" {" in text:
    raise SystemExit("an extern block is left")
open(dst, "w", encoding="utf-8", newline="\n").write(text)
print("  %d entry points as pointer types" % len(fns))
PY

rustfmt --edition 2024 "$out/vt.rs"
echo "  $(grep -c '^pub ' "$out/vt.rs") items, $(grep -c 'Size of\|Offset of' "$out/vt.rs") layout assertions"
