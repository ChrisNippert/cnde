//! Custom GLES shaders for things the stock elements can't draw.

use smithay::{
    backend::renderer::{
        element::Kind,
        gles::{GlesPixelProgram, Uniform, UniformName, UniformType, element::PixelShaderElement},
    },
    utils::{Logical, Rectangle},
};

/// A filled rounded rectangle with a thin border, anti-aliased, from a
/// signed distance field. Colours are premultiplied; `radius` and
/// `border_width` are in physical pixels (`size` is the physical size
/// smithay passes in).
pub const ROUNDED_RECT: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec4 color;
uniform vec4 border_color;
uniform float radius;
uniform float border_width;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 half_size = size * 0.5;
    vec2 p = v_coords * size - half_size;
    float d = rounded_box(p, half_size, min(radius, min(half_size.x, half_size.y)));
    float outside = clamp(0.5 + d, 0.0, 1.0);
    float inside_border = clamp(0.5 - (d + border_width), 0.0, 1.0);
    vec4 c = mix(border_color, color, inside_border);
    gl_FragColor = c * (1.0 - outside) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
"#;

pub fn rounded_rect_uniforms() -> [UniformName<'static>; 4] {
    [
        UniformName::new("color", UniformType::_4f),
        UniformName::new("border_color", UniformType::_4f),
        UniformName::new("radius", UniformType::_1f),
        UniformName::new("border_width", UniformType::_1f),
    ]
}

/// A rounded rectangle over `area` (logical, output-local).
pub fn rounded_rect(
    shader: GlesPixelProgram,
    area: Rectangle<i32, Logical>,
    radius: f64,
    color: [f32; 4],
    border: [f32; 4],
    scale: f64,
    alpha: f32,
) -> PixelShaderElement {
    PixelShaderElement::new(
        shader,
        area,
        None,
        alpha,
        vec![
            Uniform::new("color", color),
            Uniform::new("border_color", border),
            Uniform::new("radius", (radius * scale) as f32),
            Uniform::new("border_width", scale.max(1.0) as f32),
        ],
        Kind::Unspecified,
    )
}

/// A texture clipped to a rounded rectangle: the frosted wallpaper behind
/// the dock. `crop` is where the drawn part sits in the texture (x0, y0,
/// x1, y1, in texture coordinates); `rect_size` and `radius` are physical.
pub const ROUNDED_TEXTURE: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

uniform vec4 crop;
uniform vec2 rect_size;
uniform float radius;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    vec2 local = (v_coords - crop.xy) / max(crop.zw - crop.xy, vec2(1e-6));
    vec2 half_size = rect_size * 0.5;
    vec2 p = local * rect_size - half_size;
    float d = rounded_box(p, half_size, min(radius, min(half_size.x, half_size.y)));
    float outside = clamp(0.5 + d, 0.0, 1.0);
    gl_FragColor = color * (1.0 - outside) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
"#;

pub fn rounded_texture_uniforms() -> [UniformName<'static>; 3] {
    [
        UniformName::new("crop", UniformType::_4f),
        UniformName::new("rect_size", UniformType::_2f),
        UniformName::new("radius", UniformType::_1f),
    ]
}
