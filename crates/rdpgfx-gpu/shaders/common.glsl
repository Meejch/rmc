// Shared declarations of the AVC444 compute shaders.
// Mirrors rdpgfx-model (crates/rdpgfx-model/src/avc444.rs): same indices, same integer math,
// so GPU and CPU results must be bit-identical.
#extension GL_EXT_shader_8bit_storage : require

layout(local_size_x = 16, local_size_y = 16) in;

// YUV 4:4:4 state of one surface: three planes, `stride` x `rows` bytes each (macroblock padded).
layout(std430, set = 0, binding = 1) buffer StateY { uint8_t sy[]; };
layout(std430, set = 0, binding = 2) buffer StateU { uint8_t su[]; };
layout(std430, set = 0, binding = 3) buffer StateV { uint8_t sv[]; };

layout(push_constant) uniform Params {
    uint stride;     // padded width
    uint rows;       // padded height
    uint width;      // visible width
    uint height;     // visible height
    uvec4 area;      // l, t, r, b: chroma-block aligned update area (exclusive r/b)
    uvec4 luma;      // l, t, r, b: luma rectangle (exclusive r/b)
    uint layout_v2;  // 0 = AVC444 v1, 1 = AVC444v2
    uint reverse;    // 1 = apply the reverse chroma filter when converting to RGB
    uint color;      // fill: BGRA bytes as little-endian uint
    uint pad0;
} pc;

// Surface pixels: RGBA8 image viewed as R32_UINT, one BGRA texel per uint (binding 4).
#define SURFACE_IMAGE layout(set = 0, binding = 4, r32ui) uniform writeonly uimage2D surface_img;

// Pixel of the dispatch inside rectangle `r` (l, t, r, b exclusive).
bool rect_pixel(uvec4 r, out uint row, out uint col) {
    col = r.x + gl_GlobalInvocationID.x;
    row = r.y + gl_GlobalInvocationID.y;
    return col < r.z && row < r.w;
}

bool area_pixel(out uint row, out uint col) {
    col = pc.area.x + gl_GlobalInvocationID.x;
    row = pc.area.y + gl_GlobalInvocationID.y;
    return col < pc.area.z && row < pc.area.w;
}
