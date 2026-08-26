// Volume raymarch.
//
// Conventions follow the WebGL2 volume viewer in Josh Med by the same author: two modes, one
// plane at a time or ray marching through the whole grid; window and level applied at sample
// time so both stay live; and a view-to-texture permutation that maps superior onto screen up so
// a horizontal drag yaws rather than rolls.
//
// The box is drawn as ordinary geometry rather than a fullscreen pass, so the volume occludes and
// is occluded by anything else in the scene.

struct Uniforms {
    view: mat4x4<f32>,
    proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    viewport: vec4<f32>,
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VolumeUniform {
    box_min: vec4<f32>,
    box_max: vec4<f32>,
    // window, level, mode (0 slice, 1 raycast), axis (0 axial, 1 coronal, 2 sagittal)
    a: vec4<f32>,
    // slice 0..1, projection (0 composite, 1 maximum intensity), threshold, opacity scale
    b: vec4<f32>,
};

@group(1) @binding(0) var<uniform> vol: VolumeUniform;
@group(1) @binding(1) var vol_tex: texture_3d<f32>;
@group(1) @binding(2) var vol_samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_volume(@location(0) unit: vec3<f32>) -> VsOut {
    let world = mix(vol.box_min.xyz, vol.box_max.xyz, unit);
    var o: VsOut;
    o.clip = u.view_proj * vec4<f32>(world, 1.0);
    o.world = world;
    return o;
}

/// Window and level, matching the console convention: level is the centre of the displayed range
/// and window is its full width.
fn windowed(v: f32) -> f32 {
    let win = max(vol.a.x, 0.0001);
    let level = vol.a.y;
    return clamp((v - (level - win * 0.5)) / win, 0.0, 1.0);
}

fn sample_at(q: vec3<f32>) -> f32 {
    return windowed(textureSampleLevel(vol_tex, vol_samp, q, 0.0).r);
}

/// Slab intersection against the unit cube.
fn hit_box(ro: vec3<f32>, rd: vec3<f32>) -> vec2<f32> {
    let inv = 1.0 / rd;
    let a = (vec3<f32>(0.0) - ro) * inv;
    let b = (vec3<f32>(1.0) - ro) * inv;
    let lo = min(a, b);
    let hi = max(a, b);
    let t0 = max(max(lo.x, lo.y), lo.z);
    let t1 = min(min(hi.x, hi.y), hi.z);
    return vec2<f32>(t0, t1);
}

@fragment
fn fs_volume(i: VsOut) -> @location(0) vec4<f32> {
    let extent = max(vol.box_max.xyz - vol.box_min.xyz, vec3<f32>(1e-6));
    // Work in normalized volume space so the box intersection is against the unit cube.
    let ro = (u.cam_pos.xyz - vol.box_min.xyz) / extent;
    let entry = (i.world - vol.box_min.xyz) / extent;
    let rd = normalize(entry - ro);

    let hit = hit_box(ro, rd);
    let t0 = max(hit.x, 0.0);
    let t1 = hit.y;
    if (t1 <= t0) {
        discard;
    }

    // Slice mode: intersect the chosen plane and sample once.
    if (vol.a.z < 0.5) {
        let axis = i32(vol.a.w);
        let plane = vol.b.x;
        var denom = rd.z;
        var origin = ro.z;
        if (axis == 1) {
            denom = rd.y;
            origin = ro.y;
        } else if (axis == 2) {
            denom = rd.x;
            origin = ro.x;
        }
        if (abs(denom) < 1e-6) {
            discard;
        }
        let t = (plane - origin) / denom;
        if (t < t0 || t > t1) {
            discard;
        }
        let q = ro + rd * t;
        if (any(q < vec3<f32>(0.0)) || any(q > vec3<f32>(1.0))) {
            discard;
        }
        let g = sample_at(q);
        return vec4<f32>(vec3<f32>(g), 1.0);
    }

    // Ray march. A fixed step count keeps the cost predictable regardless of how obliquely the ray
    // crosses the grid.
    let steps = 256;
    let dt = (t1 - t0) / f32(steps);
    var acc = vec3<f32>(0.0);
    var alpha = 0.0;
    var peak = 0.0;
    let threshold = vol.b.z;
    let opacity = vol.b.w;
    let mip = vol.b.y > 0.5;

    for (var s = 0; s < steps; s = s + 1) {
        let q = ro + rd * (t0 + dt * f32(s));
        let g = sample_at(q);
        if (mip) {
            peak = max(peak, g);
        } else if (g > threshold) {
            let a = (g - threshold) * opacity;
            acc += (1.0 - alpha) * a * vec3<f32>(g);
            alpha += (1.0 - alpha) * a;
            // Everything behind this point is invisible, so stop paying for it.
            if (alpha > 0.985) {
                break;
            }
        }
    }

    if (mip) {
        if (peak <= 0.001) {
            discard;
        }
        return vec4<f32>(vec3<f32>(peak), 1.0);
    }
    if (alpha < 0.004) {
        discard;
    }
    // Premultiplied, matching the blend state.
    return vec4<f32>(acc, alpha);
}
