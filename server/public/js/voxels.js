"use strict";

/*global THREE, $, FileReader, 
window, document, WebSocket, schemapack, Worker, ui, pako, Block, buffer*/

/// <reference path="./three.min.js" />
/// <reference path="./jquery.min.js" />
/// <reference path="./schemapack.min.js" />
/// <reference path="./PointerLockControls.js" />
/// <reference path="./SSAOShader.js" />
/// <reference path="./CopyShader.js" />
/// <reference path="./EffectComposer.js" />
/// <reference path="./RenderPass.js" />
/// <reference path="./ShaderPass.js" />
/// <reference path="./MaskPass.js" />
/// <reference path="./blocks.js" />
/// <reference path="./buffer.min.js" />
/// <reference path="./ui.js" />


//Constants
const Buffer = buffer.Buffer;
const log = console.debug;
const CHUNK_SIZE = 32;
const CHUNK_CUBED = Math.pow(CHUNK_SIZE, 3);
const CHUNK_CLIP = CHUNK_SIZE - 1;
const CHUNK_BIT = Math.log2(CHUNK_SIZE);
const REGION_SIZE = 16;
const REGION_CLIP = REGION_SIZE - 1;
const REGION_BIT = Math.log2(REGION_SIZE);
const KEYS = {
	" ": 32,
	"control": 33,
	"shift": 34,
	"alt": 35,
	"tab": 36,
	"return": 37,
	"enter": 38,
	"'": 39,
	"(": 40,
	")": 41,
	"*": 42,
	"+": 43,
	",": 44,
	"-": 45,
	".": 46,
	"/": 47,
	"0": 48,
	"1": 49,
	"2": 50,
	"3": 51,
	"4": 52,
	"5": 53,
	"6": 54,
	"7": 55,
	"8": 56,
	"9": 57,
	":": 58,
	";": 59,
	"<": 60,
	"=": 61,
	">": 62,
	"?": 63,
	"@": 64,
	"A": 65,
	"B": 66,
	"C": 67,
	"D": 68,
	"E": 69,
	"F": 70,
	"G": 71,
	"H": 72,
	"I": 73,
	"J": 74,
	"K": 75,
	"L": 76,
	"M": 77,
	"N": 78,
	"O": 79,
	"P": 80,
	"Q": 81,
	"R": 82,
	"S": 83,
	"T": 84,
	"U": 85,
	"V": 86,
	"W": 87,
	"X": 88,
	"Y": 89,
	"Z": 90,
	"[": 91,
	"": 92,
	"]": 93,
	"^": 94,
	"F1": 95,
	"`": 96,
	"a": 97,
	"b": 98,
	"c": 99,
	"d": 100,
	"e": 101,
	"f": 102,
	"g": 103,
	"h": 104,
	"i": 105,
	"j": 106,
	"k": 107,
	"l": 108,
	"m": 109,
	"n": 110,
	"o": 111,
	"p": 112,
	"q": 113,
	"r": 114,
	"s": 115,
	"t": 116,
	"u": 117,
	"v": 118,
	"w": 119,
	"x": 120,
	"y": 121,
	"z": 122,
	"{": 123,
	"|": 124,
	"}": 125,
	"~": 126
};
const chunkFlag = {
	empty: 1,
	full: 2
};
const
	TOP = 0,
	BOTTOM = 1,
	NORTH = 2,
	SOUTH = 3,
	EAST = 4,
	WEST = 5;

//Features
var fChunkDequeueing = true;

//Globals
var controls, cam, camera, scene, 
	renderer, loader, debugText, imgLoader;
var cacheRadius = 12;
var maxCached = 4096;
var begun = false;
var debugValue = "...";
var ip = "...";
var local = true;
var port = 500;
var protocol = "ws";
var lastFrame = 0;
var fps = 60;
var fps2 = 0;

//Symbols
const clamp = Symbol("clamp");
const loop = Symbol("loop");
const format = Symbol("format");
const uint16 = Symbol("uint16");
const interpose = Symbol("array");
const magnitudal = Symbol("array");
const tail = Symbol("tail");
const head = Symbol("head");
const connecting = Symbol("game: connecting");
const menu = Symbol("game: in menu");
const ingame = Symbol("game: in game");
const stopped = Symbol("game: stopped");

//Basic Prototypes
Array.prototype[interpose] = function(objects, value, max, interval=1) {
	this.splice(Math.round((this.length * (1 - (value / max))) / interval)[clamp](0, this.length) * interval, 0, ...objects);
};
Array.prototype[magnitudal] = function () {
	for (let i = 0; i < this.length; i++) {
		this[i] = Math.abs(this[i]);
	}
};
Number.prototype[clamp] = function (min, max) {
	return Math.min(max, Math.max(this, min));
};
Number.prototype[loop] = function (min, max) {
	return (this > max) ? ((this / max) % 1.0) * max 
		: (this < min) ? max - ((this / max) % 1.0) * max : this;
};
Number.prototype[uint16] = function (max) {
	return Math.floor((this / max) * 65535);
};
String.prototype[format] = function() {
	var i = -1;
	var arg = arguments;
	return this.replace(/(%s)/g, function () {
		i += 1;
		return typeof arg[i] != "undefined" ? arg[i].toString() : "???";
	});
};

//Classes
class Chunk {
	constructor(pos, data = null) {
		//Data
		this.pos = pos;
		this.key = [this.pos.x, this.pos.y, this.pos.z].join(",");
		this.voxels = new Voxels(CHUNK_SIZE);
		this.flags = 0;
		this.max = 0;
		this.built = false;
		this.active = true;
		this.run = 0;
		Chunk.actives++;
	}
	createObject() {
		//Geometry
		this.geometry = new THREE.BufferGeometry();
		this.normals = null;
		this.vertices = null;
		this.faces = null;
		this.UVs = null;
		this.AO = null;

		//Object
		this.object = new THREE.Mesh(this.geometry, materials.atlas);
		this.object.position.copy(this.pos);
		this.object.position.multiplyScalar(CHUNK_SIZE);

		//Boundsbox
		this.boundsBox = new DebugBox(this.object.position, CHUNK_SIZE);
	}
	applyMesh(result) {
		game.chunksToRecMesh--;

		//Initialize
		if (!this.built) {
			this.createObject();
			this.max = 2000 + result.vertices.length * 3;
		}

		//Add vertices to buffer
		if (!this.vertices) {
			var vertices = new Float32Array(this.max);
			vertices.set(result.vertices);
			this.geometry.addAttribute("position", new THREE.BufferAttribute(vertices, 3));
			this.vertices = this.geometry.getAttribute("position");
		}
		else {
			if (result.vertices.length > this.max) {this.recreate(result); return;}
			this.vertices.set(result.vertices, 0);
			this.vertices.needsUpdate = true;
		}
		this.vertices.count = result.vertices.length;

		//Add faces to buffer
		if (!this.faces ) {
			var faces = new Uint16Array(this.max / 2);
			faces.set(result.faces);
			this.geometry.setIndex(new THREE.BufferAttribute(faces, 1));
			this.faces = this.geometry.getIndex();
		}
		else {
			if (result.faces.length > this.max / 2) {this.recreate(result); return;}
			this.faces.set(result.faces, 0);
			this.faces.needsUpdate = true;
		}
		this.faces.count = result.faces.length;

		//Add normals to buffer
		if (!this.normals) {
			var normals = new Float32Array(this.max);
			normals.set(result.normals);
			this.geometry.addAttribute("normal", new THREE.BufferAttribute(normals, 3));
			this.normals = this.geometry.getAttribute("normal");
		}
		else {
			this.normals.set(result.normals, 0);
			this.normals.needsUpdate = true;
		}
		this.normals.count = result.normals.length;

		//Compute Atlas-UVs
		var UVs = null;
		if (!this.UVs) UVs = new Float32Array(this.max);
		var texIndex;
		for (let i = 0; i < result.faces.length / 3; i++) {
			//Compute texture index from normal
			texIndex = Block.blocks[result.blockIDs[Math.floor(i / 2)]]
				.faces[((result.normals[6 * i + 0] + 1) * 3 
				+ (result.normals[6 * i + 1] + 1) * 2 
				+ (result.normals[6 * i + 2] + 1)) % 6 - 1];
			//Assign
			if (UVs) {
				UVs[i * 4 + 0] = UVs[i * 4 + 2] = texIndex % 8;
				UVs[i * 4 + 1] = UVs[i * 4 + 3] = -Math.floor(texIndex / 8);
			} else {
				this.UVs.setXY(i * 2, texIndex % 8, -Math.floor(texIndex / 8));
				this.UVs.setXY(i * 2 + 1, texIndex % 8, -Math.floor(texIndex / 8));
			}
		}

		//Update UVs buffer
		if (!this.UVs) {
			this.geometry.addAttribute("uv", new THREE.BufferAttribute(UVs, 2));
			this.UVs = this.geometry.getAttribute("uv");
		}
		else
			this.UVs.needsUpdate = true;
		this.UVs.count = result.faces.length * 4;

		//Add ambient occlusion to buffer
		if (game.ao) {
			if (!this.AO) {
				var ao = new Float32Array(this.max);
				ao.set(result.ao);
				this.geometry.addAttribute("ao", new THREE.BufferAttribute(ao, 1));
				this.AO = this.geometry.getAttribute("ao");
			} else {
				this.AO.set(result.ao, 0);
				this.AO.needsUpdate = true;
			}
			this.AO.count = result.ao.length;
		}
		
		//Add to scene if not added
		if (!this.built) {
			scene.add(this.object);
			this.built = true;
		}

		//Update "near" chunks
		if (this.pos.distanceTo(player.chunkPos) < 2)
			player.updateNearChunks();
	}
	recreate(result) {
		this.deactivate();
		this.active = true;
		Chunk.actives++;
		Chunk.inactives--;
		this.applyMesh(result);
		log("RECREATED %s", this.key);
	}
	remesh(insertion=tail, value=0, max=0) {
		if (this.flags & chunkFlag.empty) return;
		var obj = {
			pos: [this.pos.x, this.pos.y, this.pos.z],
			data: this.voxels.data,
			size: CHUNK_SIZE
		};
		switch(insertion) {
			case tail:
				game.chunksToMesh.push(obj);
				break;
			case head:
				game.chunksToMesh.splice(0, 0, obj);
				break;
			case interpose:
				game.chunksToMesh[interpose]([obj], value, max);
				break;
		}
		game.chunksToRecMesh++;
	}
	unload() {
		this.deactivate();
		Chunk.inactives--;
		game.chunks.delete(this.key);
		delete this.voxels.data;
		//log("unloaded %s", this.key);
	}
	deactivate() {
		if (this.active) {
			Chunk.inactives++;
			Chunk.actives--;
		}
		this.active = false;
		if (this.built) {
			this.boundsBox.remove();
			this.geometry.dispose();
			scene.remove(this.object);
			delete this.geometry;
			delete this.normals;
			delete this.vertices;
			delete this.faces;
			delete this.UVs;
			delete this.AO;
			delete this.boundsBox;
			delete this.object;
			this.built = false;
		}
	}
	activate() {
		if (!this.active) {
			Chunk.actives++;
			Chunk.inactives--;
		}
		this.active = true;
		net.send("chunk_query", 
			{pos: [this.pos.x, this.pos.y, this.pos.z], run: this.run});
		this.remesh(interpose, player.chunkPos.distanceTo(this.pos), game.loadradius);
	}

	//Chunk at world chunk position
	static at(x, y, z) {
		return game.chunks.get(`${x},${y},${z}`);
	}

	//Chunk at world voxel position
	static voxAt(x, y, z) {
		return Chunk.at(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
	}

	//Create chunk from data (also mesh it)
	static instantiate(x, y, z, data, flags, run) {
		
		//Create chunk
		var chunk;
		if (Chunk.at(x, y, z)) {chunk = Chunk.at(x, y, z); log("replace %s", chunk.key);}
		else chunk = new Chunk(new THREE.Vector3(x, y, z));
		chunk.flags = flags;
		chunk.run = run;
		chunk.active = true;

		//Unpack packed chunk data
		if (!(flags & chunkFlag.empty)) {
			if (!chunk.voxels.data)
				chunk.voxels.data = Buffer.from(pako.inflate(data));
			else
				chunk.voxels.data.set(pako.inflate(data));
		}

		//Is empty chunk
		else chunk.voxels.data = Buffer.alloc(CHUNK_CUBED);

		//Set chunk in map
		game.chunks.set(`${x},${y},${z}`, chunk);

		//Mesh if not empty
		if (!(chunk.flags & chunkFlag.empty))
			chunk.remesh(interpose, player.chunkPos.distanceTo(chunk.pos), game.loadradius);
		return chunk;
	}

	//Deactivates all chunks
	static deactivateAll() {
		for (let chunk of game.chunks.values()) {
			chunk.deactivate();
		}
	}

	//Unloads all chunks
	static unloadAll() {
		for (let chunk of game.chunks.values()) {
			chunk.unload();
		}
	}
}
class Voxels {
	constructor(size) {
		this.data = null;
	}

	//Direct voxel manipulation
	at(x, y, z) {
		return this.data[(y + z * CHUNK_SIZE) * CHUNK_SIZE + x];
	}
	mod(x, y, z, value) {
		this.data[(y + z * CHUNK_SIZE) * CHUNK_SIZE + x] = value;
	}
	aoQuery(chunk, clearx, cleary, norm, x, y, z, ix, iy) {
		if (x <= 0 || x >= CHUNK_SIZE - 1
			|| y <= 0 || y >= CHUNK_SIZE - 1
			|| z <= 0 || z >= CHUNK_SIZE - 1)
			return Voxels.atPos(
				chunk.pos.x * CHUNK_SIZE + x + norm.x + clearx[0] * ix + cleary[0] * iy,
				chunk.pos.y * CHUNK_SIZE + y + norm.y + clearx[1] * ix + cleary[1] * iy,
				chunk.pos.z * CHUNK_SIZE + z + norm.z + clearx[2] * ix + cleary[2] * iy
			);
		else
			return this.at(
				x + norm.x + clearx[0] * ix + cleary[0] * iy,
				y + norm.y + clearx[1] * ix + cleary[1] * iy,
				z + norm.z + clearx[2] * ix + cleary[2] * iy
			);
	}

	//Ambient Occlusion calculation
	static occlusion(side1, side2, corner) {
		if (side1 && side2) {
			return 0;
		}
		return 3 - (side1 + side2 + corner);
	}

	//Edit voxel value @ world position
	static edit(x, y, z, value, send = true) {

		//Edit voxel value (if chunk exists)
		let chunk = Chunk.voxAt(x, y, z);
		if (chunk && chunk.active) {
			//Change value
			chunk.voxels.mod(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);

			//Send change to server
			if (send) {
				net.send("edit", {
					pos: [x, y, z],
					value: value
				});
			}

			//Remesh chunk
			chunk.flags = 0;
			chunk.run++;
			chunk.remesh();
		}
	}

	//Get voxel value @ vox world pos
	static grab(x, y, z) {
		if (Chunk.voxAt(x, y, z))
			return Chunk.voxAt(x, y, z).voxels.at(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP);
		else
			return 0;
	}
	static grabVec(pos) {
		if (Chunk.voxAt(pos.x, pos.y, pos.z))
			return Chunk.voxAt(pos.x, pos.y, pos.z).voxels.at(
				pos.x & CHUNK_CLIP, pos.y & CHUNK_CLIP, pos.z & CHUNK_CLIP);
		else
			return 0;
	}

	//Get voxel value @ real world pos
	static atPos(x, y, z) {
		if (Chunk.voxAt(Math.floor(x), Math.floor(y), Math.floor(z)))
			return Chunk.voxAt(Math.floor(x), Math.floor(y), Math.floor(z))
				.voxels.at(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP);
		else
			return 0;
	}
}
class DebugBox {
	constructor(pos, size) {
		var geometry;
		if (typeof size == "number")
			geometry = new THREE.EdgesGeometry(new THREE.BoxBufferGeometry(size, size, size), 1);
		else
			geometry = new THREE.EdgesGeometry(new THREE.BoxBufferGeometry(size.x, size.y, size.z), 1);
		this.object = new THREE.LineSegments(geometry, materials.wire);
		this.object.position.copy(pos);
		this.object.position.addScalar(size / 2);
		game.addBox(this.object);
	}
	remove() {
		game.removeBox(this.object);
		this.object.geometry.dispose();
	}
}
class Actor {
	constructor(id) {
		this.id = id;
		this.geometry = new THREE.CylinderBufferGeometry(0.5, 0.5, 1.8);
		this.obj = new THREE.Mesh(this.geometry, materials.default);
		scene.add(this.obj);
		this.targetPos = new THREE.Vector3();
		this.velocity = new THREE.Vector3();
		this.targetVelocity = new THREE.Vector3();
		this.n = new THREE.Vector3();
		this.p = new THREE.Vector3();
		//log("CREATED ACTOR %s", this.id);
	}
	update() {
		//log("UPDATED ACTOR %s to (%s,%s,%s)", this.id, x, y, z);
		this.velocity.lerp(this.targetVelocity, 0.4);
		this.p.copy(this.velocity);
		this.p.multiplyScalar(1.25);
		this.n.copy(this.obj.position);
		this.n.add(this.p);
		this.obj.position.lerpVectors(this.n, this.targetPos, 0.1);
		if (this.targetPos.distanceTo(player.pos) > 8.5 * CHUNK_SIZE)
			this.remove();
	}
	remove() {
		game.actors.delete(this.id);
		scene.remove(this.obj);
		this.geometry.dispose();
		//log("DELETED ACTOR %s", this.id);
	}
	get arrayPos() {
		return [this.obj.position.x, this.obj.position.y, this.obj.position.z];
	}
}
class TextureAtlas {
	constructor(texture, cellSize, cells) {
		this.cellSize = cellSize;
		this.cells = cells;

		//Generate atlas
		var texCanvas = $("body").append("<canvas id='tex-canvas'>");
		var texImg = texture.image;
		texCanvas = $("#tex-canvas")[0].getContext("2d");
		texCanvas.canvas.width = cellSize * cells * 2;
		texCanvas.canvas.height = cellSize * cells * 2;
		for (let x = 0; x < cells * 2; x++) {
			for (let y = 0; y < cells * 2; y++) {
				texCanvas.drawImage(texImg,
					Math.floor(x / 2.0) * cellSize,
					Math.floor(y / 2.0) * cellSize,
					cellSize, cellSize,
					x * cellSize, y * cellSize,
					cellSize, cellSize);
			}
		}

		//Assign to texture, setup
		texture.image.src = texCanvas.canvas.toDataURL();
		texture.magFilter = THREE.NearestFilter;
		texture.minFilter = THREE.NearestMipMapLinearFilter;
		texture.wrapS = THREE.RepeatWrapping;
		texture.wrapT = THREE.RepeatWrapping;

		//Create material
		this.material = new THREE.ShaderMaterial({
			uniforms: {
				"tileOffset": {
					value: new THREE.Vector2(0, -(1.0 / cells))
				},
				"tileSize": {
					value: 0.5 / cells
				},
				"texture": {
					value: texture
				},
				"ambientOcclusion": {
					value: game.ao
				}
			},
			vertexShader: materials.shaders.atlasVert,
			fragmentShader: materials.shaders.atlasFrag,
			fog: true
		});
		Object.assign(this.material.uniforms, THREE.UniformsLib.fog);
		this.material["atlas"] = this;
		$(texCanvas.canvas).hide();
	}
}
class BoundsBox {
	constructor(size, pos) {
		this.pos = pos.clone();
		this.head = new THREE.Vector3();
		this.size = size.clone();
		this.halfSize = size.clone().multiplyScalar(0.5);
		this.box = new THREE.Box3();
		this.debug = new DebugBox(this.pos, this.size);
		this.debugHead = new DebugBox(this.head, 1);
		this.voxSize = this.size.clone().floor();
		this.velocity = new THREE.Vector3();
		this.voxMin = new THREE.Vector3();
		this.voxMax = new THREE.Vector3();
		this.collState = [false, false, false, false, false, false];

		this._short = 0;
		this._norms = [0, 0, 0, 0, 0, 0];
		this._snorms = [0, 0, 0, 0, 0, 0];
	}
	move(pos, velocity) {
		//Velocity
		this.velocity.copy(velocity);

		//Player pos is top of box, this.pos is center
		this.pos.copy(pos).y -= this.size.y / 2;

		//Assign position and update box
		this._update();
		this.collState.fill(false);

		//March over voxel volume, check for collisions
		debugValue = "";
		let finish = false;
		check:
		for (let x = this.voxMin.x; x <= this.voxMax.x && !finish; x++) {
			for (let y = this.voxMin.y; y <= this.voxMax.y && !finish; y++) {
				for (let z = this.voxMin.z; z <= this.voxMax.z && !finish; z++) {
					if (Voxels.grab(x, y, z) != 0) {
						/*
							//Bottom
							if (y == this.voxMin.y && this.velocity.y != 0) {
								this.pos.y = Math.floor(this.pos.y + 1) - 0.25;
								this.velocity.y = 0;
								//this._update();
							} else if (y > this.voxMin.y) {

								//West and East
								if (x == this.voxMin.x && this.velocity.x != 0) {
									this.pos.x = Math.floor(this.pos.x) + this.halfSize.x + this.velocity.x;
									this.velocity.x = 0;
								}
								else if (x == this.voxMax.x && this.velocity.x != 0) {
									this.pos.x = Math.ceil(this.pos.x) - this.halfSize.x + this.velocity.x;
									this.velocity.x = 0;
								}

								//North and South
								if (z == this.voxMin.z && this.velocity.z != 0) {
									this.pos.z = Math.floor(this.pos.z) + this.halfSize.z + this.velocity.z;
									this.velocity.z = 0;
								}
								else if (z == this.voxMax.z && this.velocity.z != 0) {
									this.pos.z = Math.ceil(this.pos.z) - this.halfSize.z + this.velocity.z;
									this.velocity.z = 0;
								}
							}
						*/

						

						//Calculate impulses
						this._snorms[EAST] = this._norms[EAST] = this.pos.x - 1 + this.pos.x;
						this._snorms[TOP] = this._norms[TOP] = this.pos.y - 1 + this.pos.y;
						this._snorms[NORTH] = this._norms[NORTH] = this.pos.z - 1 + this.pos.z;
						this._snorms[WEST] = this._norms[WEST] = this.pos.x - this.size.x + x;
						this._snorms[BOTTOM] = this._norms[BOTTOM] = this.pos.y - this.size.y + y;
						this._snorms[SOUTH] = this._norms[SOUTH] = this.pos.z - this.size.z + z;

						//Resolve shortest impulse
						this._snorms[magnitudal]();
						this._short = Math.min(...(this._snorms));
						switch (this._short) {
							case this._snorms[WEST]:
								//if (this.collState[WEST]) break;
								this.pos.x = x + this.size.x;
								//this.velocity.x = 0;
								this.collState[WEST] = true;
								debugValue += "west, ";
								break;
							case this._snorms[BOTTOM]:
								//if (this.collState[BOTTOM]) break;
								this.pos.y = y + this.size.y;
								this.velocity.y = 0;
								this.collState[BOTTOM] = true;
								debugValue += "bottom, ";
								break;
							case this._snorms[SOUTH]:
								//if (this.collState[SOUTH]) break;
								this.pos.z = z + this.size.z;
								//this.velocity.z = 0;
								this.collState[SOUTH] = true;
								debugValue += "south, ";
								break;
							case this._snorms[EAST]:
								//if (this.collState[EAST]) break;
								this.pos.x = x - 1;
								//this.velocity.x = 0;
								this.collState[EAST] = true;
								debugValue += "east, ";
								break;
							case this._snorms[TOP]:
								//if (this.collState[TOP]) break;
								this.pos.y = y - 1;
								this.velocity.y = -this.velocity.y / 2 - 0.1;
								this.collState[TOP] = true;
								debugValue += "top, ";
								break;
							case this._snorms[NORTH]:
								//if (this.collState[NORTH]) break;
								this.pos.z = z - 1;
								//this.velocity.z = 0;
								this.collState[NORTH] = true;
								debugValue += "north, ";
								break;

							default: log("malcollision");
						}
						this._update();
						continue check;
						//finish = true;
					}
				}
			}
		}

		debugValue += ` collisions, ${this.collState.toString()}`;

		//Set head position
		this.head.copy(this.pos).y += this.size.y / 2;

		//Update debug boxes
		this.debug.object.position.copy(this.pos);
		this.debugHead.object.position.copy(this.head);
	}
	_update() {
		this.box.setFromCenterAndSize(this.pos, this.size);
		this.voxMin = this.box.min.clone().floor();
		this.voxMax = this.box.max.clone().floor();
	}
}
class Timer {
	constructor(length, repeat=true, handler=null) {
		//Length in ticks
		this.length = length;
		this.repeat = repeat;
		this.handler = handler;
		this.ticked = false;
		this._t = 0;
		this.name = "...";
		Timer.timers.push(this);
	}
	tick(num=1) {
		this.ticked = false;
		this._t += num;
		if (this._t >= this.length) {
			this._t = 0;
			this.ticked = true;
			if (this.handler) this.handler();
			if (!this.repeat) {
				Timer.timers.splice(Timer.timers.indexOf(this), 1);
				delete this.handler;
			}
		}
	}
	reset() {
		this._t = 0;
	}
	static update() {
		for (let timer of Timer.timers) {
			timer.tick();
		}
	}
}
class ColorRamp {
	constructor(list) {
		this.list = list;
		this.color = new THREE.Color();
	}

	at(t) {
		//Find indexes
		let start = Math.floor(t * this.list.length)[clamp](0, this.list.length - 1);
		let end = (start + 1) % this.list.length;

		//Amount between two colors
		let tt = (t * this.list.length)[clamp](0, this.list.length - 1) - start;

		//Copy and interpolate, return color
		this.color.copy(this.list[start]);
		this.color.lerp(this.list[end], tt);
		return this.color;
	}
}

//Instances
Timer.timers = [];
new Timer(5, true, function(){fps2 = Math.floor(fps);});
Voxels.aoOffsets = [[-1, 0], [-1, -1], [0, -1], [0, 0]];
Chunk.actives = 0;
Chunk.inactives = 0;

//Objects
var mouse = {
	x: 0,
	y: 0,
	left: false,
	right: false,
	leftPress: false,
	rightPress: false,
	lastx: 0,
	lasty: 0
};
var player = {
	//Properties
	name: "guest",
	velocity: new THREE.Vector3(0, 0, 0),
	voxPos: new THREE.Vector3(0, 0, 0),
	chunkPos: new THREE.Vector3(0, 0, 0),
	regionPos: new THREE.Vector3(0, 0, 0),
	speed: new THREE.Vector3(0, 0, 0),
	dir: new THREE.Vector3(),
	flying: false,
	armLength: 4,
	jumpPower: 0.225,
	moveSpeed: 0.03,
	placeBlock: "Stone Bricks",
	selectBlock: Block["Air"],
	nearChunks: [],
	box: null,
	movedChunk: false,

	//Private members
	_ray: new THREE.Raycaster(),
	_allowInput: true,
	_posTimer: new Timer(10),
	_editTimer: new Timer(5),
	_rayOn: new THREE.Vector3(),
	_rayOff: new THREE.Vector3(),
	_objPos: {
		pos: [0.0, 0.0, 0.0],
		velocity: [0.0, 0.0, 0.0]
	},
	_lastChunkPos: new THREE.Vector3(),

	//Functions
	init: function() {
		this.box = new BoundsBox(new THREE.Vector3(0.8, 1.8, 0.8), 
			new THREE.Vector3());
		this._ray.near = 0.5;
		this._ray.far = this.armLength;
	},
	update: function () {
		camera.getWorldDirection(this.dir);
		/*debugValue = "%s,%s,%s"[format](
			this.dir.x.toFixed(2),
			this.dir.y.toFixed(2),
			this.dir.z.toFixed(2));*/

		//Movement
		this.speed.multiplyScalar(0);
		if (this._allowInput) {
			//Flying
			if (keyPress("f")) {
				this.flying = !this.flying;
				this.velocity.multiplyScalar(0);
				log(this.flying ? "flying" : "not flying");
			}

			//Walking / Flying forwards
			if (keyDown("w"))
				this.speed.z = this.moveSpeed;
			if (keyDown("s"))
				this.speed.z = -this.moveSpeed;

			//Strafe
			if (keyDown("a"))
				this.speed.x = -this.moveSpeed;
			if (keyDown("d"))
				this.speed.x = this.moveSpeed;
			
			//Speed
			if (keyDown("shift"))
				this.speed.multiplyScalar(4);
		}

		//Simulation
		this.velocity.x += this.dir.x * this.speed.z;
		this.velocity.z += this.dir.z * this.speed.z;
		if (this.flying) this.velocity.y += this.dir.y * this.speed.z;

		if (!this.flying) {

			//Bounding box sim
			this.velocity.y -= 0.016;
			this.box.move(this.pos, this.velocity);
			this.pos.copy(this.box.head);
			this.velocity.copy(this.box.velocity);

			//Jump
			if (this._allowInput && this.velocity.y == 0 && keyPress(" "))
				this.velocity.y = this.jumpPower;
		}

		//Empty chunk fix
		if (!Chunk.voxAt(this.voxPos.x, this.voxPos.y, this.voxPos.z))
			this.velocity.multiplyScalar(0);

		//Apply velocity
		cam.position.x += this.velocity.x;
		this.velocity.x *= 0.75;
		cam.position.y += this.velocity.y;
		if (this.flying) this.velocity.y *= 0.75;
		cam.position.z += this.velocity.z;
		this.velocity.z *= 0.75;

		//Position update
		this._objPos.pos[0] = this.pos.x;
		this._objPos.pos[1] = this.pos.y;
		this._objPos.pos[2] = this.pos.z;
		this._objPos.velocity[0] = this.velocity.x;
		this._objPos.velocity[1] = this.velocity.y;
		this._objPos.velocity[2] = this.velocity.z;
		this.voxPos.copy(this.pos).floor();
		this.chunkPos.copy(this.voxPos).divideScalar(CHUNK_SIZE).floor();
		this.regionPos.copy(this.chunkPos).divideScalar(REGION_SIZE).floor();

		//Update near chunks
		this.movedChunk = false;
		if (!this.chunkPos.equals(this._lastChunkPos)) {
			this.updateNearChunks();
			this.movedChunk = true;
			this._lastChunkPos.copy(this.chunkPos);
		}

		//Send pos
		if (this._posTimer.ticked) {
			net.send("move", this._objPos);
		}

		//Input
		if (this._allowInput) {

			//Voxel editing
			this._ray.set(this.pos, this.dir);
			let hit = this._ray.intersectObjects(this.nearChunks)[0];
			if (hit) {
				//Get voxel coordinates
				this._rayOn.set(
					Math.floor(hit.point.x - hit.face.normal.x * 0.25),
					Math.floor(hit.point.y - hit.face.normal.y * 0.25),
					Math.floor(hit.point.z - hit.face.normal.z * 0.25)
				);
				this._rayOff.set(
					Math.floor(hit.point.x + hit.face.normal.x * 0.25),
					Math.floor(hit.point.y + hit.face.normal.y * 0.25),
					Math.floor(hit.point.z + hit.face.normal.z * 0.25)
				);
				game.moveSelect(this._rayOn, hit.point);
				game.showSelect = true;
				this.selectBlock = Block.blocks[Voxels.grabVec(this._rayOn)];

				//Edit voxel
				if (mouse.leftPress || mouse.rightPress)
					this._editTimer.tick(this._editTimer.length);
				if (this._editTimer.ticked) {
					if (mouse.left)
						Voxels.edit(
							this._rayOn.x, 
							this._rayOn.y, 
							this._rayOn.z, Block["Air"].id);
					else if (mouse.right)
						Voxels.edit(
							this._rayOff.x, 
							this._rayOff.y, 
							this._rayOff.z, Block[this.placeBlock].id);						
				}
			} else {
				game.showSelect = false;
				this.selectBlock = Block["Air"];
			}

			//Change voxel
			if (keyPress("1")) this.placeBlock = "Cobblestone";
			if (keyPress("2")) this.placeBlock = "Moss Stone";
			if (keyPress("3")) this.placeBlock = "Stone Bricks";
			if (keyPress("4")) this.placeBlock = "Dirt";
			if (keyPress("5")) this.placeBlock = "Grass";
			if (keyPress("6")) this.placeBlock = "Wooden Crate";
			if (keyPress("7")) this.placeBlock = "Clay Pot";
			if (keyPress("8")) this.placeBlock = "Log";
			if (keyPress("9")) this.placeBlock = "Leaves";
		}
	},
	get pos() {
		return cam.position;
	},
	set pos(vector) {
		cam.position.copy(vector);
	},
	get allowInput() {
		return this._allowInput;
	},
	set allowInput(value) {
		this._allowInput = value;
		if (controls)
			controls.enabled = value;
	},
	move: function (x, y, z) {
		log("set pos to %s %s %s", x, y, z);
		cam.position.set(x, y, z);
	},
	updateNearChunks: function () {
		//Clear
		this.nearChunks.length = 0;
		let chunk = null;

		//Add new
		for (let x = this.chunkPos.x - 1; x <= this.chunkPos.x + 1; ++x) {
			for (let y = this.chunkPos.y - 1; y <= this.chunkPos.y + 1; ++y) {
				for (let z = this.chunkPos.z - 1; z <= this.chunkPos.z + 1; ++z) {
					chunk = Chunk.at(x, y, z);
					if (chunk && chunk.built)
						this.nearChunks.push(chunk.object);
				}
			}
		}
	}
};
var commands = {
	visible: false,
	execute: function (command) {
		log("Command: '%s'", command);
		if (command[0] == ":") {
			log("eval %s", command.slice(1));
			eval(command.slice(1));
		} else {
			var terms = command.split(" ");
			if (terms.length == 0) console.error("Command parse error");
			var name = terms[0];
			switch (name) {
				case "loadradius":
					game.loadradius = parseInt(terms[1])[clamp](2, 8);
					break;
				case "shaders":
					postprocessing.enabled = Boolean(terms[1] == "on");
					break;
				case "fog":
					scene.fog.density = (terms[1] == "on") ? sky.baseFogDensity / game.loadradius : 0;
					break;
				case "tp":
				case "teleport":
					if (terms.length == 4) {
						player.move(
							parseInt(terms[1]), 
							parseInt(terms[2]), 
							parseInt(terms[3]));
					} else if (terms.length == 2) {
						net.send("tp_to", {name: terms[1]});
					}
					break;
				case "wireframe":
					materials.atlas.wireframe = (terms[1] == "on");
					break;
				case "warp":
					net.send("warp", {world: terms[1], worldinfo: ""});
					break;
				case "daytime":
					net.send("time", {world: game.world, daytime: Math.floor(parseFloat(terms[1]) * 65535)});
					break;
				case "occlusion":
					game.ao = (terms[1] == "on");
					Chunk.deactivateAll();
					game.forceReload = true;
					break;
				case "webworkercount":
					workers.control(parseInt(terms[1])[clamp](1, 6));
					break;
				default:
					console.warn("%s not recognized command", name);
					break;
			}
		}
		this.hide();
		return false;
	},
	prompt: function () {
		$("#command").css("display", "block");
		$("#command input").focus();
		$("#command input").val("");
		player.allowInput = false;
		this.visible = true;
	},
	hide: function () {
		$("#command").css("display", "none");
		$("#command input").blur();
		$("#command input").val("");
		player.allowInput = true;
		this.visible = false;
	}
};
var net = {
	things: {
		names: []
	},
	start: function () {
		$("#msg").text("Setting Up");

		//Create WebWorkers
		workers.control(2);

		//Create blob reader
		this.deblobber = (function () {
			var reader = new FileReader();
			reader.addEventListener("loadend", function (event) {
				net.recieve(Buffer.from(this.result));
			});

			return {
				read: function (data) {
					if (reader.readyState == reader.LOADING)
						setTimeout(function () {
							net.deblobber.read(data);
						}, 2);
					else
						reader.readAsArrayBuffer(data);
				}
			};
		}());

		//Connect
		$("#msg").text("Creating Socket");
		let url = `${protocol}://${window.location.hostname}:${port}`;
		log("attempt to connect to %s", url);
		this.socket = new WebSocket(url);
		$("#msg").text("Connecting to " + url);

		//Create socket handlers
		this.socket.onopen = function (event) {
			log("socket opened");
			$("#msg").text("Connected...?");
		};
		this.socket.onmessage = function (msg) {
			if (typeof msg.data == "string") {
				$("#msg").text("Connected! Parsing schemes...");
				//Load "Things" from server, and generate schemes
				let types = JSON.parse(msg.data);
				for (let i = 0; i < types.length; i++) {
					net.things.names.push(types[i][0]);
					net.things[types[i][0]] = {
						scheme: schemapack.build(types[i][1]),
						ID: i
					};
				}
				$("#msg").text("Connected! Waiting for valid login...");
			} else
				net.deblobber.read(msg.data);
		};
		this.socket.onclose = function (event) {
			game.stop("websocket connection closed (refresh page to attempt reconnection)");
			gui.loginMenu.hidden = true;
		};
		this.socket.onerror = function (event) {
			game.stop("websocket error (refresh page to attempt reconnection)");
		};
	},
	send: function (name, instance) {
		let encoded = this.things[name].scheme.encode(instance);
		let array = Buffer.alloc(encoded.length + 1);
		array[0] = this.things[name].ID;
		array.set(encoded, 1);
		this.socket.send(array);
		//log("sent %s", name);
	},
	recieve: function (obj) {
		var name = this.things.names[obj[0]];
		var data = this.things[name].scheme.decode(obj.slice(1));
		var chunk, actor, info;
		if (data.worldinfo)
			info = JSON.parse(data.worldinfo);

		switch (name) {
			case "init": //name, pos, id
				$("#msg").text("Recieved init! Starting world...");
				log("VERSION: %s", data.version);
				game.start();
				game.hidden = true;
				game.updateWorld(info);
				player.name = data.name;
				player.move(data.pos[0], data.pos[1], data.pos[2]);
				player.update();
				game.update();
				$("#msg").text("Waiting for chunks (0)");
				break;
			case "player": //{name: "string", pos: ["float32"]}
				break;
			case "goodbye": //{reason: "string"}
				game.stop(data.reason);
				break;
			case "chunk":
				//Parse
				game.chunksToRec.delete(data.pos.join(","));
				chunk = Chunk.instantiate(
					data.pos[0], data.pos[1], data.pos[2], data.voxels, data.flags, data.run);
				
				//Halted
				if (game.halted) {
					game.update();
					$("#msg").text("Waiting for chunks (%s)"[format](game.chunksToRec.size));
				}
				if (game.halted && game.chunksToRec.size == 0) {
					log("recieved enough chunks!");
					game.halted = false;
					game.status = ingame;
					game.hidden = false;
					player.allowInput = false;
				}
				break;
			case "chunk_query":
				Chunk.at(data.pos[0], data.pos[1], data.pos[2]).remesh();
				break;
			case "bundle":
				for (let i = 0; i < data.flags.length; i++) {
					//Parse
					game.chunksToRec.delete(`${data.pos[i*3+0]},${data.pos[i*3+1]},${data.pos[i*3+2]}`);
					chunk = Chunk.instantiate(
						data.pos[i*3+0], data.pos[i*3+1], data.pos[i*3+2], data.voxels[i], data.flags[i], data.run[i]);

					//Halted
					if (game.halted) {
						game.update();
						$("#msg").text("Waiting for chunks (%s)" [format](game.chunksToRec.size));
					}
					if (game.halted && game.chunksToRec.size == 0) {
						log("recieved enough chunks!");
						game.halted = false;
						game.status = ingame;
						game.hidden = false;
						player.allowInput = false;
					}
				}
				break;
			case "actor_update":
				for (let i = 0; i < data.id.length; i++) {
					actor = game.actors.get(data.id[i]);

					//New actor
					if (!actor) {
						actor = new Actor(data.id[i]);
						game.actors.set(data.id[i], actor);
						actor.obj.position.set(
							data.pos[i * 3 + 0],
							data.pos[i * 3 + 1] - 0.9,
							data.pos[i * 3 + 2]);
						actor.velocity.set(
							data.velocity[i * 3 + 0],
							data.velocity[i * 3 + 1],
							data.velocity[i * 3 + 2]);
					}

					//Update position
					actor.targetPos.set(
						data.pos[i * 3 + 0],
						data.pos[i * 3 + 1] - 0.9,
						data.pos[i * 3 + 2]);
					actor.targetVelocity.set(
						data.velocity[i * 3 + 0],
						data.velocity[i * 3 + 1],
						data.velocity[i * 3 + 2]);
				}
				break;
			case "actor_remove":
				if (game.actors.has(data.id)) {
					game.actors.get(data.id).remove();
					game.actors.delete(data.id);
				}
				break;
			case "edit":
				Voxels.edit(
					data.pos[0], data.pos[1], data.pos[2], data.value, false);
				break;
			case "login_error":
				$("#msg").text(data.message);
				console.warn("login error! %s", data.message);
				gui.loginMenu.loading = false;
				break;
			case "warp":
				game.updateWorld(info);
				break;
			case "pos":
				player.move(data.pos[0], data.pos[1], data.pos[2]);
				break;
			case "time":
				game.dayTime = data.daytime / 65535;
				break;
			default:
				console.error("unhandled data type from server: %s", name);
				break;
		}

		//log("recieved %s", name);
	},
	socket: null,
	deblobber: null
};
var workers = {
	list: new Array(),
	control: function(num) {
		while (this.list.length != num) {
			if (this.list.length < num)
				this.create();
			if (this.list.length > num)
				this.destroy();
		}
	},
	create: function() {
		var obj = {
			ready: true,
			worker: new Worker("js/mesher_worker.js")
		};
		this.list.push(obj);
		obj.worker.onmessage = this.onmsg;
	},
	destroy: function() {
		if (this.list.length > 0)
			this.list.pop().worker.terminate();
	},
	onmsg: function (msg) {
		let data = msg.data;
		if (Chunk.at(data.pos[0], data.pos[1], data.pos[2]))
			Chunk.at(data.pos[0], data.pos[1], data.pos[2]).applyMesh(data.result);
		else {
			console.warn("recieved mesh for chunk that does not exist");
			game.chunksToRecMesh--;
		}
		workers.list[data.id].ready = true;
	}
};
var game = (function () {
	//Private
	var halted = true;
	var debug = true;
	var gridHelper = new THREE.GridHelper(CHUNK_SIZE * 8, 8);
	var axisHelper = new THREE.AxisHelper(2);
	var boundsBoxes = new THREE.Object3D();
	var unloadTimer = new Timer(240);
	var vector = new THREE.Vector3(0, 0, 0);
	var select = null;
	var selectBall = null;
	var _hidden = true;
	var world = "default";
	var ao = true;
	var requestArray = [];
	var loadradius = 4;

	//Public
	return {

		//Functions
		start: function () {
			log("STARTED GAME");

			//Add debug objects to scene
			scene.add(gridHelper);
			scene.add(boundsBoxes);
			scene.add(axisHelper);
			axisHelper.position.set(CHUNK_SIZE / 2, 1, CHUNK_SIZE / 2);

			select = new DebugBox(new THREE.Vector3(0, 0, 0), 1.05);
			scene.add(select.object);
			selectBall = new THREE.Mesh(new THREE.SphereBufferGeometry(0.05), materials.debug);
			scene.add(selectBall);

			//Initialize things
			postprocessing.init();
			sky.init();
			controls = new THREE.PointerLockControls(camera);
			cam = controls.getObject();
			scene.add(cam);
			document.addEventListener("pointerlockchange", pointerlockchange, false);
			player.init();

			//Hide everything, set to halted
			this.halted = true;
			this.hidden = false;
		},
		stop: function (reason = "game ended") {
			if (game.status != stopped) {
				console.error("game stopped: %s", reason);
				$("#msg").text(reason);
				this.halted = true;
				this.hidden = true;
				this.showDebug = false;
				this.status = stopped;
			}
		},
		update: function () {
			//Update player
			if (!this.halted) {
				player.update();
				axisHelper.position.copy(player.box.head);
			}

			//Update actors
			for (let actor of this.actors.values()) {
				actor.update();
			}

			//Chunk request loop
			if (player.movedChunk || this.halted || this.forceReload) {
				var key, chunk, dist;
				for (let x = player.chunkPos.x - this.loadradius; x <= player.chunkPos.x + this.loadradius; x++) {
					for (let y = player.chunkPos.y - this.loadradius; y <= player.chunkPos.y + this.loadradius; y++) {
						for (let z = player.chunkPos.z - this.loadradius; z <= player.chunkPos.z + this.loadradius; z++) {
							key = `${x},${y},${z}`;
							vector.set(x, y, z);
							if (player.chunkPos.distanceTo(vector) <= this.loadradius && !this.chunksToRec.has(key)) {
								dist = player.chunkPos.distanceTo(vector);
								chunk = this.chunks.get(key);

								//Chunk DNE
								if (!this.chunks.has(key)) {
									requestArray[interpose]([x, y, z], dist, this.loadradius, 3);
									this.chunksToRec.set(key, true);
								}

								//Chunk is inactive
								else if (!chunk.active) {
									chunk.activate();
								}

								//Chunk failed to mesh
								else if (!chunk.built && this.chunksToRecMesh == 0) {
									chunk.remesh();
								}
							}
						}
					}
				}
				this.forceReload = false;
			}

			//Chunk deactivating/unloading loop
			if (unloadTimer.ticked) {
				let count = this.chunks.size;
				for (let chunk of this.chunks.values()) {
					vector.copy(chunk.pos);
					if (player.chunkPos.distanceTo(vector) > this.loadradius) {
						if (chunk.active)
							chunk.deactivate();
						else if (player.chunkPos.distanceTo(vector) > cacheRadius || count > maxCached) {
							chunk.unload();
							count--;
						}
					}
				}
			}

			//Chunk mesh dequeueing
			if (fChunkDequeueing && player.movedChunk && this.chunksToMesh.length > 0) {
				for (let i = this.chunksToMesh.length - 1; i >= 0; i--) {
					vector.fromArray(this.chunksToMesh[i].pos);
					if (Math.ceil(vector.distanceTo(player.chunkPos)) > this.loadradius) {
						this.chunksToMesh.pop();
						this.chunksToRecMesh--;
					}
				}
			}

			//Chunk requesting
			if (requestArray.length > 0) {
				net.send("req_chunks", {
					pos: requestArray
				});
				requestArray.length = 0;
			}

			//Chunk meshing threading
			for (let workerID = 0; workerID < workers.list.length; workerID++) {
				if (this.chunksToMesh.length && workers.list[workerID].ready) {
					//Get chunk message
					let chunk = this.chunksToMesh.pop();

					//Add worker ID, then send
					chunk.id = workerID;
					chunk.ao = game.ao;
					workers.list[workerID].worker.postMessage(chunk);
					workers.list[workerID].ready = false;
				}
			}

			//Sky and Time
			this.dayTime = (this.dayTime + this.dayTimeSpeed)[loop](0.0, 1.0);
			sky.update();

			//Timers
			Timer.update();
		},
		addBox: function (boundsBox) {
			boundsBoxes.add(boundsBox);
		},
		removeBox: function (boundsBox) {
			boundsBoxes.remove(boundsBox);
		},
		moveSelect: function (voxPos, realPos) {
			selectBall.position.copy(realPos);
			select.object.position.copy(voxPos);
			select.object.position.addScalar(0.5);
		},
		updateWorld: function (props) {
			log(props);
			game.world = props.name;
			game.dayTime = props.daytime;
			game.dayTimeSpeed = props.daycycle ? 1 / (props.daycycle * 3) : 0;
		},

		//Getters / setters
		get halted() {
			return halted;
		},
		set halted(value) {
			halted = value;
			if (!halted) render();
			$("#lock").css("display",
				(halted && this.status == ingame) ? "block" : "none");
			if (halted) commands.hide();
			if (controls) controls.enabled = !halted;
			player.allowInput = !halted;
			if (gui.pauseMenu) gui.pauseMenu.hidden = true;
			if (!halted)
				this.showDebug = false;
			if (gui.loginMenu) gui.loginMenu.hidden = !halted;
			$("#title").css("display", halted ? "" : "none");
			$("body").css("background-image", halted ? "url('img/menu-bg.jpg')" : "none");
			log("halted: %s", halted);
		},
		get showDebug() {
			return debug;
		},
		set showDebug(value) {
			debug = value;
			boundsBoxes.visible = debug;
			gridHelper.visible = debug;
			axisHelper.visible = debug;
			selectBall.visible = debug;
			$("#debug-container").css("display", value ? "inline" : "none");
		},
		get hidden() {
			return _hidden;
		},
		set hidden(value) {
			_hidden = Boolean(value);
			$(".game").css("display", value ? "none" : "initial");
			$("canvas").css("opacity", value ? "0" : "1");
			$("#msg").css("display", !value ? "none" : "initial");
		},
		get showSelect() {
			return select.object.visible;
		},
		set showSelect(value) {
			select.object.visible = Boolean(value);
		},
		get world() {
			return world;
		},
		set world(name) {
			world = name;
			Chunk.unloadAll();
			game.actors.clear();
		},
		get ao() {
			return ao;
		},
		set ao(val) {
			ao = val;
			materials.atlas.uniforms["ambientOcclusion"].value = ao;
		},
		get loadradius() {
			return loadradius;
		},
		set loadradius(val) {
			loadradius = val;
			if (scene.fog && scene.fog.density > 0) {
				scene.fog.density = sky.baseFogDensity / loadradius;
				//postprocessing.ssaoPass.uniforms["cameraFar"].value = (game.loadradius * SCALE * CHUNK_SIZE) / 2;
			}
			this.forceReload = true;
		},

		//Status (connecting, ingame, menu, stopped)
		status: connecting,

		//Properties
		chunks: new Map(),
		chunksToMesh: new Array(),
		chunksToRecMesh: 0,
		chunksToReq: new Map(),
		chunksToRec: new Map(),
		actors: new Map(),
		forceReload: false,

		//Day/Night cycle
		dayTime: 0, //0.0 = noon, 0.5 = dusk, 1.0 = midnight
		dayTimeSpeed: 1 / (60*60*20) //20 minute cycle
	};
}());
var postprocessing = {
	enabled: false,
	init: function () {
		// Setup render pass
		var renderPass = new THREE.RenderPass(scene, camera);

		// Setup depth pass
		this.depthMaterial = new THREE.MeshDepthMaterial();
		this.depthMaterial.depthPacking = THREE.RGBADepthPacking;
		this.depthMaterial.blending = THREE.NoBlending;

		var pars = {
			minFilter: THREE.LinearFilter,
			magFilter: THREE.LinearFilter
		};
		this.depthRenderTarget = new THREE.WebGLRenderTarget(window.innerWidth, window.innerHeight, pars);
		this.depthRenderTarget.texture.name = "SSAOShader.rt";

		// Setup SSAO pass
		this.ssaoPass = new THREE.ShaderPass(THREE.SSAOShader);
		this.ssaoPass.renderToScreen = true;
		this.ssaoPass.uniforms["tDepth"].value = this.depthRenderTarget.texture;
		this.ssaoPass.uniforms["size"].value.set(window.innerWidth, window.innerHeight);
		this.ssaoPass.uniforms["cameraNear"].value = 0.1;
		this.ssaoPass.uniforms["cameraFar"].value = 200;
		this.ssaoPass.uniforms["onlyAO"].value = 0;
		this.ssaoPass.uniforms["aoClamp"].value = 0.3;
		this.ssaoPass.uniforms["lumInfluence"].value = 0.5;

		//Setup outline pass
		this.edgePass = new THREE.ShaderPass(THREE.EdgeShader2);
		this.edgePass.renderToScreen = true;
		this.edgePass.uniforms["aspect"].value.set(1.0 / window.innerWidth, 1.0 / window.innerHeight);

		// Add pass to effect composer
		this.effectComposer = new THREE.EffectComposer(renderer);
		this.effectComposer.addPass(renderPass);
		//this.effectComposer.addPass(this.edgePass);
		this.effectComposer.addPass(this.ssaoPass);
	},
	resize: function (width, height) {
		if (this.effectComposer) {
			//Shader uniforms
			this.ssaoPass.uniforms["size"].value.set(width, height);
			this.edgePass.uniforms["aspect"].value.set(1.0 / width, 1.0 / height);

			//Targets / Composer
			let pixelRatio = renderer.getPixelRatio();
			let newWidth = Math.floor(window.innerWidth / pixelRatio) || 1;
			let newHeight = Math.floor(window.innerHeight / pixelRatio) || 1;
			this.depthRenderTarget.setSize(newWidth, newHeight);
			this.effectComposer.setSize(newWidth, newHeight);
		}
	}
};
var sky = {
	init: function () {
		//Create sky
		this.sky = new THREE.Sky();
		scene.add(this.sky.mesh);
		this.sunSphere = new THREE.Mesh(
			new THREE.SphereBufferGeometry(20000, 16, 8),
			new THREE.MeshBasicMaterial({
				color: 0xffffff
			})
		);
		scene.add(this.sunSphere);

		//Sky and sun settings
		this.sky.uniforms.turbidity.value = 10;
		this.sky.uniforms.rayleigh.value = 2;
		this.sky.uniforms.luminance.value = 1;
		this.sky.uniforms.mieCoefficient.value = 0.005;
		this.sky.uniforms.mieDirectionalG.value = 0.8;
		this.sunSphere.visible = false;

		//Create ambient light
		this.ambient = new THREE.AmbientLight(0x111133);
		scene.add(this.ambient);

		//Create sun light
		this.sunLight = new THREE.DirectionalLight(0xeeeeee);
		scene.add(this.sunLight);
		scene.add(this.sunLight.target);

		//Fog
		this.baseFogDensity = 0.04;
		/*this.fogramp = new ColorRamp([
			new THREE.Color(0xd7eff7),
			new THREE.Color(0x070f16),
			new THREE.Color(0x0b1926),
			new THREE.Color(0x070f16)
		]);*/
		this.fogramp = new ColorRamp([
			new THREE.Color(0x070f16),
			new THREE.Color(0x253843),
			new THREE.Color(0x253843),
			new THREE.Color(0x253843),
			new THREE.Color(0xcf7756),
			new THREE.Color(0xb5977e),
			new THREE.Color(0xb5977e),
			new THREE.Color(0xd5dbdf)
		]);
		scene.fog = new THREE.FogExp2(0xdddddd, this.baseFogDensity / game.loadradius);

		//Initial update
		this.update();
	},
	update: function () {
		//Update sky
		let theta = Math.PI * (game.dayTime * 2 - 0.5);
		let phi = 2 * Math.PI * (0.25 - 0.5);
		this.sunSphere.position.x = 400000 * Math.cos(phi);
		this.sunSphere.position.y = 400000 * Math.sin(phi) * Math.sin(theta);
		this.sunSphere.position.z = 400000 * Math.sin(phi) * Math.cos(theta);
		this.sky.uniforms.sunPosition.value.copy(this.sunSphere.position);

		//Update lights
		this.sunLight.target.position.copy(this.sunSphere.position);
		this.sunLight.target.position.normalize();
		this.sunLight.target.position.multiplyScalar(-1.0);
		let brightness = ease10(game.dayTime * 2 - 1);
		this.sunLight.color.r = this.sunLight.color.g = this.sunLight.color.b = brightness;
		this.ambient.color.r = this.ambient.color.g = this.ambient.color.b = (brightness / 3)[clamp](0.2, 1);
		scene.fog.color.copy(this.fogramp.at(brightness));

		//Move sky with player
		if (cam)
			this.sky.mesh.position.copy(cam.position);
	}
};
var materials = {
	shaders: {
		atlasFrag: null,
		atlasVert: null
	},
	shaderPatch: function(code) {
		var results = code.match(/(\/\/\/:)\w+/g);
		var shadername = "";
		if (results)
		for (let result of results) {
			shadername = result.split(":")[1];
			code = code.replace(result, THREE.ShaderChunk[shadername]);
		}
		return code;
	}
};
var gui = {
	loginPanel: null, 
	loginMenu: null,
	pauseMenu: null,
	pointerID: 0,
	init: function() {
		log("Init GUI");

		//Login menu
		this.loginMenu = new ui.Tabs({center: true});

		//Login tab
		this.loginPanel = new ui.Panel({label: "Login",height: 85}, this.loginMenu);
		new ui.Input({name: "username"}, this.loginPanel);
		new ui.Input({name: "password", type: "password"}, this.loginPanel);
		new ui.Button({
			name: "Submit",
			action: function(){
				gui.loginMenu.loading = true;
				net.send("login", {
					type: 1,
					user: $(this).parent().find("input[name='username']").val(),
					pass: $(this).parent().find("input[name='password']").val(),
					ip: ip
				});
			}
		}, this.loginPanel);

		//Signup tab
		var signup = new ui.Panel({
			label: "Signup",
			height: 85
		}, this.loginMenu);
		new ui.Input({name: "username"}, signup);
		new ui.Input({name: "password", type: "password"}, signup);
		new ui.Input({
			name: "confirm password",
			type: "password"
		}, signup);
		new ui.Button({
			name: "Submit",
			action: function(){
				let user = $(this).parent().find("input[name='username']").val();
				let pass = $(this).parent().find("input[name='password']").val();
				let verify = $(this).parent().find("input[name='confirm password']").val();
				gui.loginMenu.loading = true;
				if (pass == verify)
					net.send("login", {type: 2, user: user, pass: pass, ip: ip});
				else {
					$("#msg").text("Passwords do not match");
					gui.loginMenu.loading = false;
				}
			}
		}, signup);
		this.loginMenu.current = this.loginPanel;

		//Pause menu
		this.pauseMenu = new ui.Tabs({hidden: true, center: true});

		//Settings
		var settings = new ui.Panel({label: "Settings", height: 180}, this.pauseMenu);
		new ui.Button({
			name: "Log Out", stacked: true,
			action: function() {
				net.send("logout", {ip: ip, user: player.name});
			}
		}, settings);
		new ui.Button({
			name: "Back to Game", stacked: true,
			action: function() {
				document.body.requestPointerLock();
			}
		}, settings);
		new ui.Slider({
			name: "Volume",
			stacked: true,
			value: 100,
			action: function (value, element) {
				console.log(value);
			}
		}, settings);
		new ui.Slider({
			name: "Load Radius",
			display: ui.wholeNumber,
			value: 4,
			bounds: [2, 8],
			stacked: true,
			action: function (value, element) {
				game.loadradius = value;
				game.forceReload = true;
			}
		}, settings);
		new ui.Checkbox({
			name: "Shaders",
			action: function(event) {
				postprocessing.enabled = event.target.checked;
			}
		}, settings);
		new ui.Checkbox({
			name: "Debug",
			action: function(event) {
				game.showDebug = event.target.checked;
			}
		}, settings);
		new ui.Checkbox({
			name: "Occlusion",
			checked: true,
			action: function(event) {
				game.ao = event.target.checked;
			}
		}, settings);
		new ui.Checkbox({
			name: "Fog",
			checked: true,
			action: function(event) {
				scene.fog.density = event.target.checked ? sky.baseFogDensity / game.loadradius : 0;
			}
		}, settings);

		//About
		var about = new ui.Panel({label: "About"}, this.pauseMenu);
		new ui.Text({
			text: `New Soils is a Minecraft-esque game with a twist.
			Visit <a href="https://www.newsoils.us">New Soils</a> now!
			<br>
			<br> Keyboard Shortcuts:
			<br> / - Open Console
			<br> P - Show Debug Stuff
			<br> F - Toggle Flying
			<br> Shift - Go Fast`
		}, about);
		this.pauseMenu.current = settings;
	}
};

//Helpful variables
var keys = new Uint8Array(128);
var keysPressed = new Uint8Array(128);

//Helpful functions
function lerp(a, b, amnt) {
	return a + (b - a) * amnt;
}
function ease10(t) {
	return (t < 0.5 ? 512 * (Math.pow(t, 10)) : -512 * (Math.pow(t - 1, 10)) + 1)[clamp](0, 1);
}
function ease4(t) {
	return (t < 0.5 ? 512 * (Math.pow(t, 4)) : -512 * (Math.pow(t - 1, 4)) + 1)[clamp](0, 1);
}
function keyDown(key) {
	return keys[KEYS[key]] == 1;
}
function keyPress(key) {
	return keysPressed[KEYS[key]] == 1;
}

//Listeners
$(document).mousedown(function f(event) {
	mouse.left = Boolean(event.buttons & 1);
	mouse.right = Boolean(event.buttons & 2);
	mouse.leftPress = mouse.left;
	mouse.rightPress = mouse.right;
	if (game.status == ingame && gui.pauseMenu.hidden) document.body.requestPointerLock();
});
$(document).mouseup(function f(event) {
	mouse.left = Boolean(event.buttons & 1);
	mouse.right = Boolean(event.buttons & 2);
});
$(window).resize(function () {
	//Camera
	camera.aspect = window.innerWidth / window.innerHeight;
	camera.updateProjectionMatrix();

	//Renderer
	renderer.setSize(window.innerWidth, window.innerHeight);
	postprocessing.resize(window.innerWidth, window.innerHeight);
});
$(document).keydown(function (event) {
	keys[KEYS[event.key.toLowerCase()]] = 1;
	keysPressed[KEYS[event.key.toLowerCase()]] = 1;

	if (event.key == "g"
	&& game.halted) net.send("login", {type: 0, user: "", pass: "", ip: ip});

	if (player.allowInput)
		switch (event.key) {
			case "p":
				game.showDebug = !game.showDebug;
				break;
			case "/":
				commands.prompt();
				break;
			case "k":
				Voxels.edit(player.voxPos.x, player.voxPos.y, player.voxPos.z, 2);
				break;
			case "u":
				window.open(renderer.domElement.toDataURL("image/png"), "_blank");
				break;
		}
});
$(document).keyup(function (event) {
	keys[KEYS[event.key.toLowerCase()]] = 0;
	switch (event.key) {
		case "/":
			$("#command input").val("");
			break;
	}
});
function pointerlockchange(event) {
	if (document.pointerLockElement === document.body) {
		if (!player.allowInput) {
			mouse.rightPress = false;
			mouse.leftPress = false;
		}
		commands.hide();
		player.allowInput = true;
		if (game.status == ingame) {
			gui.pauseMenu.hidden = true;
			$(renderer.domElement).css("opacity", "1");
		}
	} else {
		player.allowInput = false;
		if (game.status == ingame) {
			gui.pauseMenu.hidden = false;
			$(renderer.domElement).css("opacity", "0.5");
		}
	}
}

//First initializer
function begin() {
	//Renderer setup
	scene = new THREE.Scene();
	camera = new THREE.PerspectiveCamera(65, window.innerWidth / window.innerHeight, 0.1, 2000000);
	renderer = new THREE.WebGLRenderer({
		stencil: false,
		alpha: false,
		preserveDrawingBuffer: true,
		precision: "mediump"
	});
	renderer.setSize(window.innerWidth, window.innerHeight);
	renderer.sortObjects = true;
	log(renderer);
	document.body.appendChild(renderer.domElement);

	//Materials setup
	materials.atlas = new TextureAtlas(voxTex, 16, 8).material;
	materials.default = new THREE.MeshPhongMaterial({
		color: 0xffffff,
		shininess: 0,
		shading: THREE.FlatShading,
		fog: true
	});
	materials.wire = new THREE.LineBasicMaterial({
		color: 0xffffff,
		lights: false,
		fog: false
	});
	materials.debug = new THREE.MeshBasicMaterial({
		color: 0xffffff,
		lights: false,
		fog: false
	});

	//DOM
	debugText = $("#debug");

	//Start
	game.halted = true; game.ao = true;
	net.start();
	gui.init();
}

//Rendering Loop
var render = function () {
	window.requestAnimationFrame(render);
	fps = ((1 / ((Date.now() - lastFrame) / 1000)) + fps) / 2;
	lastFrame = Date.now();

	//Update game
	if (!game.halted) {
		game.update();
	}

	//Update Metrics
	if (game.status == ingame && game.showDebug) {
		debugText.html(`
		<br>FPS: ${fps2}
		<br>time: ${game.dayTime.toFixed(4)}
		<br>world: ${game.world}
		<br>chunks: ${game.chunks.size}
		<br>chunks (active): ${Chunk.actives}
		<br>chunks (inactive): ${Chunk.inactives}
		<br>vox pos: ${player.voxPos.toArray()}
		<br>chunk pos: ${player.chunkPos.toArray()}
		<br>region pos: ${player.regionPos.toArray()}
		<br>faces: ${renderer.info.render.faces}
		<br>calls: ${renderer.info.render.calls}
		<br>verts: ${renderer.info.render.vertices}
		<br>geom memory: ${renderer.info.memory.geometries}
		<br>webworkers: ${workers.list.length}
		<br>meshing: ${game.chunksToRecMesh}
		<br>recieving: ${game.chunksToRec.size}
		<br>actors: ${game.actors.size}
		<br>selected: ${player.selectBlock.name}
		<br>debug: ${debugValue}`);
	}

	//Post updates
	keysPressed.fill(0);
	mouse.leftPress = mouse.rightPress = false;

	//Render
	if (game.status == ingame) {
		if (postprocessing.enabled) {
			//Render depth into depthRenderTarget
			scene.overrideMaterial = postprocessing.depthMaterial;
			renderer.render(scene, camera, postprocessing.depthRenderTarget, true);

			//Render renderPasses
			scene.overrideMaterial = null;
			postprocessing.effectComposer.render();
		} else {
			//Standard render
			renderer.render(scene, camera);
		}
	}
};








//Load assets
loader = new THREE.FileLoader();
imgLoader = new THREE.TextureLoader();
loader.load("shaders/atlas.frag", function(data){
	materials.shaders.atlasFrag = materials.shaderPatch(data);
});
loader.load("shaders/atlas.vert", function(data){
	materials.shaders.atlasVert = materials.shaderPatch(data);
});
var voxTex = imgLoader.load("img/blocks.png");

//Get blocks
$.get("files/blocks.yaml", function(data){
	Block.parseYaml(data);
	player.selectBlock = Block["Air"];
});

//Get environment
$.get("?environment", function (data) {
	local = (data == "private");
	if (local) {
		log("PRIVATE server");
		port = 8080;
		protocol = "ws";
	} else {
		log("PUBLIC server");
		port = 500;
		protocol = "wss";
	}

	//Set time until begin
	setTimeout(begin, 500);
});

//Get IP address
$.get("?ip", function (data) {
	ip = data;
});
