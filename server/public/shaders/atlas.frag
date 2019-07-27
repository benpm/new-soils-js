const float LOG2 = 1.442695;

uniform sampler2D texture;
uniform vec2 tileOffset;
uniform float tileSize;
uniform bool ambientOcclusion;

varying vec3 vNormal;
varying vec3 vPosition;
varying vec2 vUV;
varying float vAO;

///:fog_pars_fragment

float whiteCompliment( in float a ) { return saturate( 1.0 - a ); }
vec2  whiteCompliment( in vec2 a )  { return saturate( vec2(1.0) - a ); }
vec3  whiteCompliment( in vec3 a )  { return saturate( vec3(1.0) - a ); }
vec4  whiteCompliment( in vec4 a )  { return saturate( vec4(1.0) - a ); }

vec4 fourTapSample(vec2 tileUV) {
	//Initialize accumulators
	vec4 color = vec4(0.0, 0.0, 0.0, 0.0);
	float totalWeight = 0.0;

	//Fourtap
	for(int dx=0; dx<2; ++dx)
	for(int dy=0; dy<2; ++dy) {
		//Compute coordinate in 2x2 tile patch
		vec2 tileCoord = 2.0 * fract(0.5 * (tileUV + vec2(dx, dy)));

		//Weight sample based on distance to center
		float w = pow(abs(1.0 - max(abs(tileCoord.x-1.0), abs(tileCoord.y-1.0))), 16.0);

		//Compute atlas coord
		vec2 atlasUV = tileOffset + tileSize * (2.0 * vUV +  tileCoord);

		//Sample and accumulate
		color += w * texture2D(texture, atlasUV);
		totalWeight += w;
	}

	//Return weighted color
	return color / totalWeight;
}

void main() {
	vec2 tileUV = vec2(dot(vNormal.zxy, vPosition), dot(vNormal.yzx, vPosition));

	if (vNormal.z < 0.0) {
		tileUV.y = 1.0 - tileUV.y;
	}

	if (vNormal.x < 0.0) {
		float r = tileUV.x;
        tileUV.x = 1.0 - tileUV.y;
        tileUV.y = 1.0 - r;
	} else if (vNormal.x > 0.0) {
		float r = tileUV.x;
        tileUV.x = 1.0 - tileUV.y;
        tileUV.y = r;
	}
	
	/*if (mod(tileUV.x, 2.0) <= 1.0) {
        tileUV.x = 1.0 - tileUV.x;
        tileUV.y = 1.0 - tileUV.y;
	}
	
	if (mod(tileUV.y, 3.0) <= 1.0) {
        tileUV.x = 1.0 - tileUV.x;
        tileUV.y = 1.0 - tileUV.y;
	}
	
	if (mod(tileUV.x, 4.0) <= 1.0) {
        tileUV.x = 1.0 - tileUV.x;
        tileUV.y = 1.0 - tileUV.y;
	}*/
	
	gl_FragColor = fourTapSample(tileUV);
	if (ambientOcclusion) gl_FragColor *= vec4(vAO, vAO, vAO, 0);
	gl_FragColor *= vec4(1.0 + abs(vNormal.x + vNormal.y) * 0.2, 1.0 + abs(vNormal.x + vNormal.y) * 0.2, 1.0 + abs(vNormal.x + vNormal.y) * 0.2, 0);

	/*float depth = gl_FragCoord.z / gl_FragCoord.w;
	float fogFactor = exp2( - 0.005 * 0.005 * depth * depth * 1.442695 );
	fogFactor = 1.0 - clamp( fogFactor, 0.0, 1.0 );
	gl_FragColor = mix( gl_FragColor, vec4( vec3(1, 1, 1), gl_FragColor.w ), fogFactor );*/

	///:fog_fragment
}