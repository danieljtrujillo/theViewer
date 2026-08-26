// Three of the six render paths: mesh raster, point sprite, sorted splat.
// One module so the uniform block is declared once.

struct Uniforms {
    view: mat4x4<f32>,
    proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    // width, height, focal_x, focal_y
    viewport: vec4<f32>,
    // point_size, znear, unused, unused
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

fn quad_corner(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return c[vi];
}

// ---------------------------------------------------------------- sorted splat

struct SplatOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) col: vec4<f32>,
    // Inverse of the projected 2D covariance, packed as (xx, xy, yy).
    @location(1) conic: vec3<f32>,
    // Pixel offset from the splat centre.
    @location(2) d: vec2<f32>,
};

fn degenerate() -> SplatOut {
    var o: SplatOut;
    // Outside the clip volume, so the rasterizer drops it.
    o.clip = vec4<f32>(0.0, 0.0, 2.0, 1.0);
    o.col = vec4<f32>(0.0);
    o.conic = vec3<f32>(1.0, 0.0, 1.0);
    o.d = vec2<f32>(0.0);
    return o;
}

// 64 bytes, matching GpuSplat on the Rust side. Splats live in a storage buffer rather than an
// instance buffer so that a depth sort rewrites only the 4-byte-per-splat order list. At the
// counts these files reach, rewriting the full array every sort is not affordable.
struct Splat {
    pos: vec3<f32>,
    opacity: f32,
    scale: vec3<f32>,
    pad0: f32,
    rot: vec4<f32>,
    rgb: vec3<f32>,
    pad1: f32,
};

@group(1) @binding(0) var<storage, read> splats: array<Splat>;
@group(1) @binding(1) var<storage, read> order: array<u32>;

@vertex
fn vs_splat(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
) -> SplatOut {
    let s = splats[order[ii]];
    let pos = s.pos;
    let scale = s.scale;
    let rot = s.rot;
    let col = vec4<f32>(s.rgb, s.opacity);

    let p_view = (u.view * vec4<f32>(pos, 1.0)).xyz;
    if (p_view.z > -u.params.y) {
        return degenerate();
    }

    // Rotation from the quaternion, stored x,y,z,w.
    let x = rot.x;
    let y = rot.y;
    let z = rot.z;
    let w = rot.w;
    let r = mat3x3<f32>(
        vec3<f32>(1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y + w * z), 2.0 * (x * z - w * y)),
        vec3<f32>(2.0 * (x * y - w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z + w * x)),
        vec3<f32>(2.0 * (x * z + w * y), 2.0 * (y * z - w * x), 1.0 - 2.0 * (x * x + y * y)),
    );
    let m = mat3x3<f32>(r[0] * scale.x, r[1] * scale.y, r[2] * scale.z);
    let sigma = m * transpose(m);

    // EWA projection: the Jacobian of the perspective divide at this splat's view position.
    let fx = u.viewport.z;
    let fy = u.viewport.w;
    let zv = p_view.z;
    let j = mat3x3<f32>(
        vec3<f32>(fx / zv, 0.0, 0.0),
        vec3<f32>(0.0, fy / zv, 0.0),
        vec3<f32>(-fx * p_view.x / (zv * zv), -fy * p_view.y / (zv * zv), 0.0),
    );
    let wm = mat3x3<f32>(u.view[0].xyz, u.view[1].xyz, u.view[2].xyz);
    let t = j * wm;
    let cov = t * sigma * transpose(t);

    // Dilate by half a pixel in each axis so sub-pixel splats stay visible.
    let a = cov[0][0] + 0.3;
    let b = cov[0][1];
    let c = cov[1][1] + 0.3;
    let det = a * c - b * b;
    if (det <= 0.0) {
        return degenerate();
    }
    let inv_det = 1.0 / det;

    let mid = 0.5 * (a + c);
    let disc = sqrt(max(mid * mid - det, 0.0));
    let radius = 3.0 * sqrt(max(mid + disc, 1e-6));

    let corner = quad_corner(vi);
    let centre = u.proj * vec4<f32>(p_view, 1.0);
    let offset = corner * radius / u.viewport.xy * 2.0 * centre.w;

    var o: SplatOut;
    o.clip = vec4<f32>(centre.xy + offset, centre.z, centre.w);
    o.col = col;
    o.conic = vec3<f32>(c * inv_det, -b * inv_det, a * inv_det);
    o.d = corner * radius;
    return o;
}

@fragment
fn fs_splat(i: SplatOut) -> @location(0) vec4<f32> {
    let power = -0.5 * (i.conic.x * i.d.x * i.d.x + i.conic.z * i.d.y * i.d.y)
        - i.conic.y * i.d.x * i.d.y;
    if (power > 0.0) {
        discard;
    }
    let alpha = min(0.99, i.col.a * exp(power));
    if (alpha < 0.00392) {
        discard;
    }
    // Premultiplied, to match the One / OneMinusSrcAlpha blend state.
    return vec4<f32>(i.col.rgb * alpha, alpha);
}
