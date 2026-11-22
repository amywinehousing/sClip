// Each 8x8 group produces one block in the CPU encoder's scan order.
Texture2D<float4> desktop : register(t0);
SamplerState bilinearClamp : register(s0);
RWStructuredBuffer<float> coefficients : register(u0);
groupshared float tile[64];
groupshared float horizontal[64];

float3 samples(int2 pixel) {
    pixel = clamp(pixel, int2(0, 0), int2(WIDTH - 1, HEIGHT - 1));
    float2 uv = (float2(pixel) + 0.5) / float2(WIDTH, HEIGHT);
    uint3 rgb = uint3(round(saturate(desktop.SampleLevel(bilinearClamp, uv, 0).rgb) * 255));
    float3 color = float3(transfer[rgb.r], transfer[rgb.g], transfer[rgb.b]);
    float y = dot(color, float3(0.2126, 0.7152, 0.0722));
    return float3(876 * y - 448, (color.b - y) * (448 / 0.9278), (color.r - y) * (448 / 0.7874));
}

[numthreads(8, 8, 1)]
void main(uint3 group : SV_GroupID, uint3 lane : SV_GroupThreadID) {
    uint x = lane.x, y = lane.y, block = group.z;
    int2 origin = int2(group.xy * 16);
    uint i = y * 8 + x;
    if (block < 4) {
        int2 p = origin + int2((block % 2) * 8 + x, (block / 2) * 8 + y);
        tile[i] = samples(p).x;
    } else {
        int2 p = origin + int2(x * 2, (block % 2) * 8 + y);
        float3 filtered = (samples(p - int2(1,0)) + 2 * samples(p) + samples(p + int2(1,0))) * 0.25;
        tile[i] = block < 6 ? filtered.y : filtered.z;
    }
    GroupMemoryBarrierWithGroupSync();
    float row = 0;
    [unroll] for (uint k = 0; k < 4; ++k) {
        float a = tile[y * 8 + k], b = tile[y * 8 + 7 - k];
        row += basis[x * 4 + k] * ((x & 1) ? a - b : a + b);
    }
    horizontal[i] = row;
    GroupMemoryBarrierWithGroupSync();
    float col = 0;
    [unroll] for (uint j = 0; j < 4; ++j) {
        float a = horizontal[j * 8 + x], b = horizontal[(7 - j) * 8 + x];
        col += basis[y * 4 + j] * ((y & 1) ? a - b : a + b);
    }
    uint mb = group.y * ((WIDTH + 15) / 16) + group.x;
    coefficients[(mb * 8 + block) * 64 + scan[i]] = col * (4.0 / weights[i]);
}
