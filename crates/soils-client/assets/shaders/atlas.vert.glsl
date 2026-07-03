attribute float ao;

varying vec3 vNormal;
varying vec3 vPosition;
varying vec2 vUV;
varying float vAO;

///:fog_pars_vertex

void main() {
    vAO = float(ao);
    vNormal = normal;
    vPosition = position;
    vUV = uv;
    vec4 mvPosition = modelViewMatrix * vec4( position, 1.0 );
    gl_Position = projectionMatrix * mvPosition;
    ///:fog_vertex
}