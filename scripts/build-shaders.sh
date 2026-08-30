#!/usr/bin/env bash
# Recompile the committed shaders. Run after editing any .comp; the result is
# committed beside the source so building the crate needs no shader compiler.
#
#   ./scripts/build-shaders.sh
#
# Needs glslang-tools and spirv-tools.
set -eu

cd "$(dirname "$0")/.."
for source in crates/*/shaders/*.comp; do
    out="${source%.comp}.spv"
    glslangValidator -V --target-env vulkan1.1 -DTARGET_VULKAN -e main -o "$out" "$source" > /dev/null
    spirv-val "$out"
    echo "built $out from $source"
    # **One source, one artifact per entry point.** The full-chroma entry is
    # not called from the subsampled one, so a single compile strips it as
    # dead; compiling it under its own name is what keeps both bodies in one
    # file while the device gets one blob each.
    if grep -q 'void body_444' "$source"; then
        out444="${source%.comp}-444.spv"
        # The interface requires every stage's entry point to be named main,
        # and a compiler strips the body the wrapper does not call, so one
        # source file becomes two blobs with no duplicated logic.
        glslangValidator -V --target-env vulkan1.1 -DTARGET_VULKAN -DMAIN_444 -o "$out444" "$source" > /dev/null
        spirv-val "$out444"
        echo "built $out444 from $source"
    fi
done
