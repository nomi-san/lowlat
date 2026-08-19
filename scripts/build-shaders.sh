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
    glslangValidator -V --target-env vulkan1.1 -o "$out" "$source" > /dev/null
    spirv-val "$out"
    echo "built $out from $source"
done
