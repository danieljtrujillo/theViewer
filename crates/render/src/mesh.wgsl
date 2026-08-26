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

// ---------------------------------------------------------------- mesh raster

// Per-draw material. Group 2 so the splat bindings at group 1 stay untouched.
struct MaterialUniform {
    base_color: vec4<f32>,
    emissive: vec4<f32>,
    // has_texture, alpha_cutoff, unlit, unused
    flags: vec4<f32>,
};

@group(1) @binding(0) var<uniform> mat: MaterialUniform;
@group(1) @binding(1) var base_tex: texture_2d<f32>;
@group(1) @binding(2) var base_samp: sampler;

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) nrm: vec3<f32>,
    @location(1) col: vec4<f32>,
    @location(2) world: vec3<f32>,
    @location(3) uv: vec2<f32>,
};

@vertex
fn vs_mesh(
    @location(0) pos: vec3<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) col: vec4<f32>,
    @location(3) uv: vec2<f32>,
) -> MeshOut {
    var o: MeshOut;
    o.clip = u.view_proj * vec4<f32>(pos, 1.0);
    o.nrm = nrm;
    o.col = col;
    o.world = pos;
    o.uv = uv;
    return o;
}

@fragment
fn fs_mesh(i: MeshOut) -> @location(0) vec4<f32> {
    var albedo = i.col * mat.base_color;
    if (mat.flags.x > 0.5) {
        albedo = albedo * textureSample(base_tex, base_samp, i.uv);
    }
    if (albedo.a < mat.flags.y) {
        discard;
    }

    // Volumetric captures and photogrammetry carry baked lighting in their textures. Shading them
    // again doubles the lighting, so textured meshes render unlit.
    if (mat.flags.z > 0.5) {
        return vec4<f32>(albedo.rgb + mat.emissive.rgb, albedo.a);
    }

    var n = vec3<f32>(0.0, 1.0, 0.0);
    let len = length(i.nrm);
    if (len > 1e-6) {
        n = i.nrm / len;
    }
    // Scans and per-frame reconstructions carry inconsistent winding, so shade two-sided.
    let v = normalize(u.cam_pos.xyz - i.world);
    if (dot(n, v) < 0.0) {
        n = -n;
    }
    let key = max(dot(n, normalize(vec3<f32>(0.4, 0.85, 0.5))), 0.0) * 0.75;
    let fill = max(dot(n, normalize(vec3<f32>(-0.5, 0.15, -0.65))), 0.0) * 0.3;
    return vec4<f32>(albedo.rgb * (0.24 + key + fill) + mat.emissive.rgb, albedo.a);
}

// ---------------------------------------------------------------- point sprite

struct PointOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) col: vec4<f32>,
    @location(1) uv: vec2<f32>,
};

@vertex
fn vs_point(
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec3<f32>,
    @location(1) col: vec4<f32>,
) -> PointOut {
    let corner = quad_corner(vi);
    let clip = u.view_proj * vec4<f32>(pos, 1.0);
    var o: PointOut;
    // Constant pixel size regardless of depth.
    let offset = corner * u.params.x / u.viewport.xy * 2.0 * clip.w;
    o.clip = vec4<f32>(clip.xy + offset, clip.z, clip.w);
    o.col = col;
    o.uv = corner;
    return o;
}

@fragment
fn fs_point(i: PointOut) -> @location(0) vec4<f32> {
    if (dot(i.uv, i.uv) > 1.0) {
        discard;
    }
    return i.col;
}

