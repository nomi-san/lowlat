// The plane split: a decoded picture copied out of the decoder's surface into
// one texture per plane, which another device on the adapter opens by handle.
//
// Compiled ahead of time, one variant per layout (scripts/gen-d3d11-split.sh).
// Every sample is read as an integer and written as the exact normalised value
// of the same integer, so the planes hold the decoded samples bit for bit; a
// ten-bit sample is written into the high bits of sixteen, as the two-plane
// ten-bit layout holds it. The output views' own sizes bound the work, so no
// constant is needed: each thread moves one chroma sample and every luma
// sample it covers, or one sample of each plane at full chroma.

#if defined(PLANAR8) || defined(PLANAR16)

#if defined(PLANAR8)
#define SCALE (1.0 / 255.0)
#else
#define SCALE (1.0 / 65535.0)
#endif

// The luma and the interleaved chroma of one slice of the decoder's surfaces.
Texture2DArray<uint> luma : register(t0);
Texture2DArray<uint2> chroma : register(t1);
RWTexture2D<float> out_y : register(u0);
RWTexture2D<float2> out_uv : register(u1);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint cw, ch, yw, yh;
    out_uv.GetDimensions(cw, ch);
    out_y.GetDimensions(yw, yh);
    if (id.x >= cw || id.y >= ch)
        return;
    out_uv[id.xy] = float2(chroma.Load(int4(id.xy, 0, 0))) * SCALE;
    [unroll] for (uint dy = 0; dy < 2; dy++) {
        [unroll] for (uint dx = 0; dx < 2; dx++) {
            uint2 p = id.xy * 2 + uint2(dx, dy);
            if (p.x < yw && p.y < yh)
                out_y[p] = float(luma.Load(int4(p, 0, 0))) * SCALE;
        }
    }
}

#elif defined(VUYA) || defined(Y410)

// One slice of the packed full-chroma surfaces: eight-bit V, U, Y, A in byte
// order, or ten-bit U, Y, V from the low bits up.
Texture2DArray<uint4> packed : register(t0);
RWTexture2D<float> out_y : register(u0);
RWTexture2D<float> out_u : register(u1);
RWTexture2D<float> out_v : register(u2);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint w, h;
    out_y.GetDimensions(w, h);
    if (id.x >= w || id.y >= h)
        return;
    uint4 c = packed.Load(int4(id.xy, 0, 0));
#if defined(VUYA)
    out_y[id.xy] = float(c.b) * (1.0 / 255.0);
    out_u[id.xy] = float(c.g) * (1.0 / 255.0);
    out_v[id.xy] = float(c.r) * (1.0 / 255.0);
#else
    out_y[id.xy] = float(c.g << 6) * (1.0 / 65535.0);
    out_u[id.xy] = float(c.r << 6) * (1.0 / 65535.0);
    out_v[id.xy] = float(c.b << 6) * (1.0 / 65535.0);
#endif
}

#endif
