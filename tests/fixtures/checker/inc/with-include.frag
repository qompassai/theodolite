#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
layout(location = 0) out vec4 out_color;
void main() { out_color = estate_color(); }
