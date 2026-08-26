// Two of the render paths: a flat picture, and the same pixels wrapped around the viewer.
//
// Neither draws geometry. The flat path is a screen-space rectangle; the panorama path is a
// full-screen triangle where every fragment turns its own direction into a texture coordinate.
// Doing it per fragment rather than on a tessellated sphere is both cheaper and exact — a sphere
// mesh visibly kinks a panorama at its poles unless it is absurdly dense.

struct Uniforms {
    view: mat4x4<f32>,
    proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    viewport: vec4<f32>,
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct ScreenUniform {
    // Flat: xy is the half-extent of the rectangle in clip space, zw its centre.
    rect: vec4<f32>,
    // Texture-space scale (xy) and offset (zw) that isolate one eye of a stereo pair.
    eye: vec4<f32>,
    // Camera basis for the panorama. w carries tan(fov_y / 2).
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
    // aspect, horizontal arc in radians, vertical arc in radians, 1.0 when the arc is a full turn
    params: vec4<f32>,
};

@group(1) @binding(0) var<uniform> s: ScreenUniform;
@group(1) @binding(1) var tex: texture_2d<f32>;
@group(1) @binding(2) var samp: sampler;

const PI: f32 = 3.14159265358979;

fn corner_of(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return c[vi];
}

// ---------------------------------------------------------------- flat picture

struct FlatOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_flat(@builtin(vertex_index) vi: u32) -> FlatOut {
    let c = corner_of(vi);
    var o: FlatOut;
    o.clip = vec4<f32>(c * s.rect.xy + s.rect.zw, 0.0, 1.0);
    // Texture rows run top to bottom, clip space runs bottom to top.
    let uv = vec2<f32>(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5);
    o.uv = uv * s.eye.xy + s.eye.zw;
    return o;
}

@fragment
fn fs_flat(i: FlatOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, i.uv);
}

// ---------------------------------------------------------------- panorama

struct PanoOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

// One oversized triangle rather than two triangles: no diagonal seam, and no fragment is shaded
// twice along it.
@vertex
fn vs_pano(@builtin(vertex_index) vi: u32) -> PanoOut {
    var c = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0),
    );
    var o: PanoOut;
    o.clip = vec4<f32>(c[vi], 0.0, 1.0);
    o.ndc = c[vi];
    return o;
}

@fragment
fn fs_pano(i: PanoOut) -> @location(0) vec4<f32> {
    let tan_half = s.right.w;
    let aspect = s.params.x;
    let dir = normalize(
        s.forward.xyz
        + s.right.xyz * (i.ndc.x * tan_half * aspect)
        + s.up.xyz * (i.ndc.y * tan_half)
    );

    // Longitude measured from the image centre, which equirectangular puts at -Z.
    let lon = atan2(dir.x, -dir.z);
    let lat = asin(clamp(dir.y, -1.0, 1.0));

    let uv = vec2<f32>(0.5 + lon / s.params.y, 0.5 - lat / s.params.z);

    // Sampled before the bounds test, not after: textureSample has to be reached under uniform
    // control flow, and an early return above it is exactly what breaks that.
    let colour = textureSample(tex, samp, uv * s.eye.xy + s.eye.zw);

    // A partial panorama covers part of the sphere and nothing else. Repeating or clamping it
    // would smear the edge pixels across the rest of the view, which reads as a bug.
    let outside = uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0;
    if (s.params.w < 0.5 && outside) {
        return vec4<f32>(0.055, 0.063, 0.075, 1.0);
    }
    return colour;
}
