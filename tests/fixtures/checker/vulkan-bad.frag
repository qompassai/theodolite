#version 450
layout(set = 0, binding = 0) uniform sampler2D u_texture;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;
void main() {
    vec3 tint = vec3(1.0);
    out_color = vec4(tint, missing_alpha)
}
