//Requirements
const http = require("http");
const WebSocket = require("ws");
const express = require("express");
const path = require("path");
const schema = require("schemapack");
const Noise = require("simplex-noise");
const mysql = require("mysql");
const File = require("fs");
const Yaml = require("js-yaml");
const assert = require("assert");
const pako = require("pako");
const Rando = require("alea");
const Block = require("./public/js/blocks.js");

//Constants
const VERSION = "0.0.16";
const root = path.resolve(__dirname);
const app = express();

//Symbols
const p = Symbol("player");
const clamp = Symbol("clamp");
const loop = Symbol("loop");
const format = Symbol("format");
const uint16 = Symbol("uint16");

//Time
const INIT_TIME = Date.now();
const TICK = 50;
const TICK_SECOND = 1000 / TICK;
const TICK_MINUTE = TICK_SECOND * 60;
const TICK_HOUR = TICK_MINUTE * 60;

//Chunks / Regions
const CHUNK_SIZE = 32;
const CHUNK_CLIP = CHUNK_SIZE - 1;
const CHUNK_BIT = Math.log2(CHUNK_SIZE);
const CHUNK_CUBED = Math.pow(CHUNK_SIZE, 3);
const CHUNK_MAXDIST = 8;
const REGION_SIZE = 16;
const REGION_CLIP = REGION_SIZE - 1;
const REGION_BIT = Math.log2(REGION_SIZE);
const REGION_OPENTICKS = TICK_MINUTE * 10;
const SECTOR_SIZE = 4096;
const HEADER_SIZE = Math.pow(REGION_SIZE, 3) * 2;
const HEADER_SECT = HEADER_SIZE / SECTOR_SIZE;

//Enumerators
const flag = {
	empty: 1,
	full: 2
};
const loginType = {
	guest: 0,
	login: 1,
	signup: 2,
	maintenance: 3
};

//Prototypes
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

//Globals
var tick = 0,
activePlayers = 0, 
players = 0,
avgPack = 0,
avgPackLength = 0,
lastID = 0,
sql, gameLoopTimer, admin;

//Primitives
var emptyFlag = Buffer.alloc(2); emptyFlag[1] = 1;
var emptyData = Buffer.alloc(1);
var emptyHeader = Buffer.alloc(HEADER_SIZE);

//Logging
var log = function(message){
	let string = ("" + message)[format](...Array.prototype.slice.call(arguments, 1));
	console.log(message, ...Array.prototype.slice.call(arguments, 1));
	if (admin && admin.readyState == WebSocket.OPEN)
		admin.send("~" + string + "<br>");
};
var logError = function(message){
	let string = ("" + message)[format](...Array.prototype.slice.call(arguments, 1));
	console.error("ERROR>>> " + message, ...Array.prototype.slice.call(arguments, 1));
	if (admin && admin.readyState == WebSocket.OPEN)
		admin.send("~" + string + "<br>");
};

//Serving
app.use(function (req, res) {
	//log("REQUEST of %s from %s", req.url, req.ip);
	let tag = req.url.split("?")[1];
	if (tag == "environment") 
		res.send(process.env.IS_PUBLIC ? "public" : "private");
	else if (tag == "ip") 
		res.send(req.ip);
	else 
		res.sendFile(`${root}/public/${req.url}`);
});

//Server
const server = http.createServer(app);
const wss = new WebSocket.Server({server});

//Classes
class Thing {
	constructor(outline) {
		this.form = outline;
		this.scheme = schema.build(outline);
		this.parent = null;
	}
	//Creates a new thing, also making it a member of Thing
	static new(name, outline) {
		Thing[name] = new Thing(outline);
		Thing.Forms.push([name, Thing[name].form]);
		Thing[name].ID = Thing.Forms.length - 1;
	}
	//Creates a new thing that extends an existing thing
	static extend(parent, name, outline) {
		let extended = Object.assign({}, outline, Thing[parent].form);
		extended.parent = "string";
		Thing.new(name, extended);
		Thing[name].parent = parent;
	}
	//Creates a new instance of a thing
	create(args) {
		var object = Object.assign(Object.create(this.form), args);
		//@todo: take into account uninitialized parameters
		if (this.parent)
			object.parent = this.parent;
		return object;
	}
}
class World {
	constructor(props) {
		//Setup
		props = Object.assign(World.defaultProps(), props || {});
		this.dir = `data/worlds/${props.name}`;
		this.props = props;
		this.name = props.name;
		this.chunks = new Map();
		this.activeChunks = 0;
		this.cachedChunks = 0;
		this.demoteIndex = 0;
		this.unloadIndex = 0;
		this.noise = new Noise(new Rando(props.seed));
		this.save();
		var world = this;

		//Create timers for chunk management
		new Timer(TICK_SECOND * 5, true, function(){
			chunkDemoter(world);
		}).name = `${this.name} demoter`;
		new Timer(TICK_SECOND * 30, true, function(){
			chunkUnloader(world);
		}).name = `${this.name} unloader`;

		//Add to multiverse
		World.worlds.set(this.name, this);
	}
	static load(name) {
		return new World(Yaml.safeLoad(File.readFileSync(`data/worlds/${name}/world.yaml`, "utf8")));
	}
	static exists(name) {
		return File.existsSync(`data/worlds/${name}`);
	}
	static defaultProps() {
		return {
			name: "default",
			type: "normal",
			seed: 0,
			daytime: 0,
			daycycle: TICK_MINUTE * 20,
			spawn: [282, 242, 268]
		};
	}
	static chunks(name) {
		return World.worlds.get(name).chunks;
	}
	static update() {
		for (let world of World.worlds.values()) {
			world.update();
		}
	}
	static save() {
		for (let world of World.worlds.values()) {
			world.save();
		}
	}
	save() {
		//Directories
		if (!File.existsSync(this.dir)) 
			File.mkdirSync(this.dir);
		if (!File.existsSync(`${this.dir}/regions`)) 
			File.mkdirSync(`${this.dir}/regions`);

		//Write properties file
		File.writeFileSync(`${this.dir}/world.yaml`, Yaml.safeDump(this.props));
	}
	update() {
		if (this.props.daycycle > 0)
			this.props.daytime = (this.props.daytime + (1 / this.props.daycycle)) % 1.0;
	}
}
class Region {
	//Creates a region at chunk pos
	static create(world, pos) {
		//Prevent overwriting
		assert(!Region.exists(world, pos), "REGION OVERWRITE");

		//Create file w/ empty header
		File.writeFileSync(Region.path(world, pos), emptyHeader, {flag:"wx"});
	}

	//Returns status of chunk data @ (x, y, z)
	static query(world, pos) {
		//Check if region exists
		assert(Region.exists(world, pos), "REGION DNE");

		//Read chunk info
		let info = Buffer.alloc(2);
		let file = Region.file(Region.path(world, pos));
		File.readSync(file, info, 0, 2, Region.infodex(pos));
		return info.readUInt16BE(0);
	}

	//Returns (compressed) chunk data at chunk pos
	static pull(world, pos) {
		//Get and check sector index
		let i = Region.query(world, pos);
		assert(i > 1, "REGION wrong byte info");

		//Convert sector index to byte index
		i = (i + HEADER_SECT) * SECTOR_SIZE;

		//Read from file
		return Region.read(Region.file(Region.path(world, pos)), i);
	}

	//Writes / appends chunk data to region at chunk pos
	static push(world, chunk) {
		//Open and acquire bytedex
		let i = Region.query(world, chunk.pos);
		let file = Region.file(Region.path(world, chunk.pos));

		//Choose appropriate method
		switch(i) {
			//Append to region
			case 1:
			case 0:
				if (chunk.flags & flag.empty) {
					//Write byte position info as 1 for empty
					File.writeSync(file, emptyFlag, 0, 2, Region.infodex(chunk.pos));
				} else {
					//Append to region
					Region.append(Region.path(world, chunk.pos), file, Region.infodex(chunk.pos), chunk.packed);
				}
				break;
			
			//Write to region
			default:
				//Convert sector index to byte index
				i = (i + HEADER_SECT) * SECTOR_SIZE;
				Region.write(file, i, chunk.packed);
				break;
		}
	}

	//Writes chunk data at byte pos (bytedex) in file
	static write(file, bytedex, buffer) {
		//Write chunk data
		File.writeSync(file, Region.prepare(buffer), 0, SECTOR_SIZE, bytedex);
	}

	//Appends packed chunk data to file
	static append(path, file, infodex, buffer) {
		//Byte position of new chunk data
		let info = Buffer.alloc(2);
		info.writeUInt16BE(Region.sectors(path), 0);

		//Append chunk data
		File.appendFileSync(path, Region.prepare(buffer));
		if (Region.files.has(path)) Region.files.get(path).sectors++;

		//Write byte position info
		File.writeSync(file, info, 0, 2, infodex);
	}

	//Returns prepared buffer for saving
	static prepare(buffer) {
		//Write padded, packed chunk data
		let padded = Buffer.alloc(SECTOR_SIZE);
		padded.set(buffer, 4);

		//Write chunk byte size
		let size = Buffer.alloc(4);
		size.writeInt32BE(buffer.length, 0);
		padded.set(size, 0);
		return padded;
	}

	//Reads chunk data from file (un-zero pads)
	static read(file, byte) {
		//Read byte length
		let length = Buffer.alloc(4);
		File.readSync(file, length, 0, 4, byte);

		//Read and return buffer of read byte length
		let buffer = Buffer.alloc(length.readInt32BE(0));
		File.readSync(file, buffer, 0, buffer.length, byte + 4);
		return buffer;
	}

	//Returns if region exists at chunk position
	static exists(world, pos) {
		return File.existsSync(Region.path(world, pos));
	}

	//Returns byte info at chunk pos
	static infodex(pos) {
		return 2 * (((pos[1] & REGION_CLIP) + (pos[2] & REGION_CLIP) * REGION_SIZE) * REGION_SIZE + (pos[0] & REGION_CLIP));
	}

	//Returns path of region at chunk position
	static path(world, pos) {
		return `${World.worlds.get(world).dir}/regions/r_${
			pos[0] >> REGION_BIT}_${
			pos[1] >> REGION_BIT}_${
			pos[2] >> REGION_BIT}`;
	}

	//Returns file or opens one
	static file(path) {
		let obj = Region.files.get(path);

		//Open and create file if none exists
		if (!obj) {
			let file = File.openSync(path, "r+");
			obj = {
				file: file,
				timer: new Timer(REGION_OPENTICKS, false, function(){
					File.close(file, errorHandler);
					Region.files.delete(path);
				}),
				sectors: Region.sectors(path)};
			Region.files.set(path, obj);
		}

		//Reset timer and return open file
		obj.timer.reset();
		return obj.file;
	}

	//Returns number of sectors of region
	static sectors(path) {
		let obj = Region.files.get(path);

		//Use statsync if file is not open
		if (!obj)
			return File.statSync(path).size / SECTOR_SIZE - HEADER_SECT;
		else
			return obj.sectors;
	}
}
class Chunk {
	constructor(world, pos, data = null) {
		//Data
		this.world = world;
		this.worldObj = World.worlds.get(world);
		this.pos = pos;
		this.key = pos.join(",");
		this.size = CHUNK_SIZE;
		this.voxels = new Voxels(this.size, data);
		this.packed = null;
		this.flags = data ? 0 : flag.empty;
		this.awaitingSave = false;
		if (data) this.pack();

		//Timing / Versioning
		this.time = tick;
		this.run = 0;
		this.cacherun = -1;

		//Status (1-active, 2-cached)
		this.worldObj.activeChunks++;
		this.status = this._status = 1;

		//Add to world chunk-map
		this.worldObj.chunks.set(this.key, this);
	}

	//Generates a new chunk here
	generate() {
		this.flags |= flag.empty;
		var val = "Air", rock = 0;
		var height = 0;
		let world = this.worldObj;
		for (let x = 0; x < this.size; x++) {
			let gx = this.pos[0] * this.size + x;
			for (let z = 0; z < this.size; z++) {
				let gz = this.pos[2] * this.size + z;

				//Heightmap
				height = 256 + Math.floor(
					world.noise.noise2D(gx / 1000, gz / 1000) * 50
					- world.noise.noise2D(gx / 500, gz / 500) * 30
					+ world.noise.noise2D(gx / 250, gz / 250) * 20
					- world.noise.noise2D(gx / 75, gz / 75) * 10
					+ world.noise.noise2D(gx / 25, gz / 25) * 5);
				
				//Rocks
				rock = world.noise.noise2D(gx / 15, gz / 15) * 5 
					- Math.abs(world.noise.noise2D(gx / 45, gz / 45)) * 10
					- Math.abs(world.noise.noise2D(gx / 25, gz / 25)) * 15;
				
				for (let y = 0; y < this.size; y++) {
					let gy = this.pos[1] * this.size + y;

					//Generate
					switch(world.props.type) {
						case "flat":
							height = 256;

							//Soils gradient
							val = gy <= height
							? gy == height ? "Grass"
							: gy < height - 64 ? "Slate"
							: gy < height - 32 ? "Stone"
							: gy < height - 16 ? "Rocky Dirt"
							: gy < height - 8 ? "Tough Dirt"
							: "Dirt" : "Air";
							break;
						case "normal":
							//Soils gradient
							val = gy <= height
							? gy == height ? "Grass"
							: gy < height - 64 ? "Slate"
							: gy < height - 32 ? "Stone"
							: gy < height - 16 ? "Rocky Dirt"
							: gy < height - 8 ? "Tough Dirt"
							: "Dirt" : "Air";
							
							//Rocks
							if (gy > height - 2 && gy <= height + rock) val = "Stone";
							
							//Caves
							if (val) {
								let cave = Math.abs(world.noise.noise3D(gx / 45, gy / 45, gz / 45));
								val = cave > 0.7 ? "Air" : val;
							}
							break;
					}

					//Assign
					if (val != "Air" && Block[val]) {
						this.voxels.mod(x, y, z, Block[val].id);
						this.flags &= ~flag.empty;
					}
				}
			}
		}
		if (!(this.flags & flag.empty)) this.pack();
	}

	//Unloads from memory
	unload() {
		this.worldObj.cachedChunks--;
		this.worldObj.chunks.delete(this.key);
		delete this.voxels.buffer;
		delete this.voxels;
	}

	//Adds this to lazy save queue (if not already added)
	save() {
		if (!this.awaitingSave) {
			this.awaitingSave = true;
			Queue.lazySaves.push({world: this.world, pos: this.pos});
		}
	}

	//Adds this to normal save queue (if not already added)
	quicksave() {
		if (!this.awaitingSave) {
			this.awaitingSave = true;
			Queue.saves.push({world: this.world, pos: this.pos});
		}
	}

	//Updates cached packed data
	pack() {
		if ((this.flags & flag.empty) || this.cacherun == this.run || !this.voxels.buffer) return false;
		let packed = pako.deflate(this.voxels.buffer);

		//Assign packed values or allocate new buffer
		if (this.packed && this.packed.length >= packed.length)
			this.packed.set(packed);
		else
			this.packed = Buffer.from(packed);
		
		avgPack = (this.packed.length + avgPackLength * avgPack) / (avgPackLength + 1);
		avgPackLength++;

		this.cacherun = this.run;
		return true;
	}

	//Updates voxel data from cached data
	unpack() {
		assert(this.packed != undefined || (this.flags & flag.empty), 
			`key=${this.key}; flags=${this.flags}; status=${this._status}; 
			run=${this.run}; crun=${this.cacherun}; voxels=${this.voxels.buffer ? this.voxels.buffer.length : "..."}`);
		if (this.flags & flag.empty)
			this.voxels.buffer = Buffer.alloc(CHUNK_CUBED);
		else
			this.voxels.parse(this.packed);
	}

	//For maintaining chunk-status
	get status() {
		return this._status;
	}
	set status(val) {
		//Cached -> Active
		if (val == 1 && this._status == 2) {
			this.unpack();
			this.worldObj.activeChunks++;
			this.worldObj.cachedChunks--;
			this.time = tick;
		}

		//Active -> Cached
		else if (val == 2 && this._status == 1) {
			if (!this.pack()) return;
			delete this.voxels.buffer;
			this.worldObj.cachedChunks++;
			this.worldObj.activeChunks--;
			this.time = tick;
		}
		this._status = val;
	}

	//Returns chunk at chunk position
	static get(world, pos) {
		return World.chunks(world).get(pos.join(","));
	}

	//Returns chunk at voxel position
	static getVox(world, pos) {
		return Chunk.get(world,
			[pos[0] >> CHUNK_BIT,
			pos[1] >> CHUNK_BIT,
			pos[2] >> CHUNK_BIT]);
	}

	//Returns if chunk at chunk position is loaded
	static loaded(world, pos) {
		return World.chunks(world).has(pos.join(","));
	}

	//Converts voxel coordinates to chunk coordinates
	static pos(pos) {
		return [pos[0] >> CHUNK_BIT, pos[1] >> CHUNK_BIT, pos[2] >> CHUNK_BIT];
	}
}
class Voxels {
	constructor(size, data = null) {
		this.size = size;
		this.buffer = null;

		//Either inflate data or make empty buffer
		if (data)
			this.parse(data);
		else
			this.buffer = Buffer.alloc(size * size * size);
	}
	at(x, y, z) {
		return this.buffer[(y + z * this.size) * this.size + x];
	}
	mod(x, y, z, value) {
		this.buffer[(y + z * this.size) * this.size + x] = value;
	}

	//Deflate and assign from given buffer
	parse(buffer) {
		if (this.buffer)
			this.buffer.set(pako.inflate(buffer));
		else
			this.buffer = Buffer.from(pako.inflate(buffer.subarray(0)));
	}
	static mod(world, x, y, z, value) {
		let chunk = Chunk.get(world, [x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT]);
		assert(chunk, "attempt to modify unloaded chunk");
		chunk.flags = 0;
		chunk.time = tick;
		chunk.run++;
		chunk.status = 1;
		chunk.voxels.mod(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
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
class Queue {
	constructor(interval, max, handler) {
		let queue = this;
		this.items = [];
		this.max = max;
		this.timer = new Timer(interval, true, function(){
			for(let i = 0; i < queue.max; i++)
				if (queue.items.length) handler(queue.items.pop());
		});
	}
	push(obj) {
		this.items.push(obj);
	}
	flush() {
		while(this.length)
			this.timer.tick(this.timer.length);
	}
	get length() {
		return this.items.length;
	}
}

//Instances
Timer.timers = [];
Region.files = new Map();
World.worlds = new Map();

//Objects
Queue.requests = new Queue(1, 32, stepRequests);
Queue.generation = new Queue(1, 4, stepGeneration);
Queue.loads = new Queue(1, 32, stepLoads);
Queue.saves = new Queue(1, 16, stepSaves);
Queue.lazySaves = new Queue(TICK_MINUTE, 64, stepSaves);
Queue.unloads = new Queue(20, 32, stepUnloads);

//Create thing types
Thing.Forms = new Array();
Thing.new("actor", {
	id: "uint16",
	pos: ["float32", "float32", "float32"],
	velocity: ["float32", "float32", "float32"],
	world: "string",
	active: "bool"
});
Thing.extend("actor", "player", {
	name: "string"
});
Thing.extend("player", "init", {
	version: "string",
	worldinfo: "string"
});
Thing.new("goodbye", {
	reason: "string"
});
Thing.new("login", {
	//type 0 - guest
	//type 1 - login
	//type 2 - new account
	//type 3 - maintenance
	type: "uint8",
	user: "string",
	pass: "string",
	ip: "string"
});
Thing.new("login_error", {
	message: "string"
});
Thing.new("logout", {
	user: "string",
	ip: "string" //For validation
});
Thing.new("chunk", {
	pos: ["int32", "int32", "int32"],
	flags: "int32",
	voxels: "buffer",
	run: "uint32"
});
Thing.new("bundle", {
	pos: ["int32"],
	flags: ["int32"],
	voxels: ["buffer"],
	run: ["uint32"]
});
Thing.new("req_chunk", {
	pos: ["int32", "int32", "int32"]
});
Thing.new("req_chunks", {
	pos: ["int32"]
});
Thing.new("chunk_query", {
	pos: ["int32", "int32", "int32"],
	run: "uint32"
});
Thing.new("pos", {
	pos: ["float32", "float32", "float32"]
});
Thing.new("move", {
	pos: ["float32", "float32", "float32"],
	velocity: ["float32", "float32", "float32"]
});
Thing.new("edit", {
	pos: ["int32", "int32", "int32"],
	value: "uint8"
});
Thing.new("warp", {
	world: "string",
	worldinfo: "string"
});
Thing.new("tp_to", {
	name: "string"
});
Thing.new("time", {
	world: "string",
	daytime: "uint16"
});

//Strictly server -> client
Thing.new("actor_update", {
	id: ["uint16"],
	pos: ["float32"],
	velocity: ["float32"]
});
Thing.new("actor_remove", {
	id: "uint16"
});



//Game
function gameLoop() {
	//Send actor updates
	let actorUpdate = {id: [], pos: [], velocity: []};
	activePlayers = 0;
	players = 0;
	for (let ws of wss.clients) {
		//Send if active
		if (ws[p].active) {
			actorUpdate.id.length = 0;
			actorUpdate.pos.length = 0;
			actorUpdate.velocity.length = 0;
			for (let other of wss.clients) {
				if (!other[p].active
				|| other[p].id == ws[p].id
				|| other[p].world != ws[p].world
				|| distance(other[p].pos, ws[p].pos) > CHUNK_MAXDIST * CHUNK_SIZE) continue;
				actorUpdate.id.push(other[p].id);
				actorUpdate.pos.push(other[p].pos[0]);
				actorUpdate.pos.push(other[p].pos[1]);
				actorUpdate.pos.push(other[p].pos[2]);
				actorUpdate.velocity.push(other[p].velocity[0]);
				actorUpdate.velocity.push(other[p].velocity[1]);
				actorUpdate.velocity.push(other[p].velocity[2]);
			}
			send(ws, "actor_update", actorUpdate);
			activePlayers++;
		}
		players++;
	}

	//Send info to admin
	if (admin && admin.readyState == admin.OPEN) {
		let string = `
			<br> NEW SOILS v${VERSION}
			<br> SERVER INFORMATION
			<br> ==================
			<br> mem usage: ${process.memoryUsage().heapUsed / 1e6} MB
			<br> connections: ${players}
			<br> active clients: ${activePlayers}
			<br> chunk requests: ${Queue.requests.length}
			<br> chunk generations: ${Queue.generation.length}
			<br> chunk loads: ${Queue.loads.length}
			<br> chunk saves: ${Queue.saves.length + Queue.lazySaves.length}
			<br> chunk unloads: ${Queue.unloads.length}
			<br> avg packed chunk bytes: ${Math.floor(avgPack)}
			<br> open region files: ${Region.files.size}
			<br> time: ${time()}
			<br> tick: ${tick}
			<br>
			<br> ENVIRONMENT VARIABLES
			<br> =====================
			<br> process.env.PORT: ${process.env.PORT}
			<br> process.env.IS_PUBLIC: ${process.env.IS_PUBLIC}
			<br> process.env.RDS_HOSTNAME: ${process.env.RDS_HOSTNAME}
			<br> process.env.RDS_USERNAME: ${process.env.RDS_USERNAME}
			<br> process.env.RDS_PASSWORD: ${process.env.RDS_PASSWORD}
			<br> process.env.RDS_PORT: ${process.env.RDS_PORT}
			<br> process.env.RDS_DB_NAME: ${process.env.RDS_DB_NAME}
			<br>
			<br> PLAYERS
			<br> =====================
		`;
		for (let client of wss.clients) {
			if (client[p].active) {
				string += `<br> ${client[p].name} @ 
					${Math.floor(client[p].pos[0])},
					${Math.floor(client[p].pos[1])},
					${Math.floor(client[p].pos[2])}`;
			}
		}
		string += "<br><br> TIMERS<br> =====================";
		for (let timer of Timer.timers) {
			string += `<br> ${timer.name} ${timer.length - timer._t}`;
		}
		string += "<br><br> WORLDS<br> =====================";
		for (let world of World.worlds.values()) {
			string+=`
			<br> ${world.name}
			<br> 	- time of day: ${world.props.daytime}
			<br> 	- chunks in memory: ${world.chunks.size}
			<br> 	- active chunks: ${world.activeChunks}
			<br> 	- cached chunks: ${world.cachedChunks}`;
		}
		admin.send(string);
	}

	//Updates
	World.update();
	Timer.update();
	tick++;
}
function newID() {
	return (++lastID) % 65535;
}
function findPlayer(name) {
	return Array.from(wss.clients).find(function(ws, i){
		return (ws[p].name == name);
	});
}
function time() {
	return Math.floor((Date.now() - INIT_TIME) / 1000);
}
function stepRequests(info) {
	//info = {world: string, pos: [x,y,z, x,y,z...], count: #, client: ws}

	for (let i = 0; i < info.count; i++) {

		var pos = [info.pos[i*3+0], info.pos[i*3+1], info.pos[i*3+2]];
		var obj = {world: info.world, pos: pos, client: info.client};

		//If in memory, send
		if (Chunk.loaded(info.world, pos)) {
			sendChunk(info.client, pos);
			continue;
		}

		//If region doesn't exist, add to generation queue
		if (!Region.exists(info.world, pos)) {
			Queue.generation.push(obj);
			continue;
		}

		//Query region
		switch (Region.query(info.world, pos)) {
			case 0: //Chunk not present
				Queue.generation.push(obj);
				break;
			case 1: //Chunk is empty
				sendChunk(info.client, pos, new Chunk(info.world, pos));
				break;
			default: //Chunk load
				Queue.loads.push(obj);
				break;
		}
	}
}
function stepGeneration(info) {
	//Generates new chunk at pos
	let chunk = new Chunk(info.world, info.pos);
	chunk.generate();
	if (info.client) sendChunk(info.client, info.pos, chunk);
	chunk.quicksave();
}
function stepLoads(info) {
	//Load and send
	let chunk = new Chunk(info.world, info.pos, Region.pull(info.world, info.pos));
	if (info.client) sendChunk(info.client, info.pos, chunk);
}
function stepSaves(info) {
	//Save chunk
	if (!Region.exists(info.world, info.pos)) Region.create(info.world, info.pos);
	let chunk = Chunk.get(info.world, info.pos);
	chunk.pack();
	Region.push(info.world, chunk);
	chunk.awaitingSave = false;
}
function stepUnloads(info) {
	Chunk.get(info.world, info.pos).unload();
}
function chunkDemoter(world) {
	//Cache chunks > 1 min old (100 at a time)
	let i = 0;
	let stop = world.demoteIndex + 100;
	for (let chunk of world.chunks.values()) {
		if (++i < world.demoteIndex) continue;
		if (chunk.status == 1 && tick - chunk.time > TICK_MINUTE && !chunk.awaitingSave)
			chunk.status = 2;
		world.demoteIndex++;
		if (world.demoteIndex >= stop) return;
	}
	world.demoteIndex = 0;
}
function chunkUnloader(world) {
	//Set chunks to unload > 10 mins old (50 at a time)
	let i = 0;
	let stop = world.unloadIndex + 50;
	let skip = false;
	for (let chunk of world.chunks.values()) {
		if (++i < world.unloadIndex) continue;

		if (chunk.status == 2 && tick - chunk.time > TICK_MINUTE * 10 && !chunk.awaitingSave) {
			//Don't unload chunks that are near players
			skip = false;
			for (let client of wss.clients) {
				if (client[p].world == world.name 
					&& distance(chunk.pos, Chunk.pos(client[p].pos)) < 12) {
					skip = true;
					break;
				}
			}

			//Conditional push to unload
			if (!skip) {
				chunk.status = 3;
				Queue.unloads.push({
					world: world.name,
					pos: chunk.pos
				});
			}
		}

		world.unloadIndex++;
		if (world.unloadIndex >= stop) return;
	}
	world.unloadIndex = 0;
}

//Network
function recieve(msg) {

	//Check admin
	if (typeof msg == "string" && msg == "admin") {
		log("admin login");
		admin = this;
		this["admin"] = true;
		this[p].name = "__ADMIN__";
		this[p].pass = "__ADMIN__";
		return;
	}

	//Parse data
	let obj = new Uint8Array(msg);
	let thing = Thing.Forms[obj[0]][0];
	let data = Thing[thing].scheme.decode(obj.slice(1));
	let ws = this;
	let player = ws[p];
	let world = ws[p].world;
	let chunk, other;
	//log("RECIEVE %s from %s", thing, this[p].id);

	//Handle data
	switch (thing) {
	case "move":
		player.pos[0] = data.pos[0];
		player.pos[1] = data.pos[1];
		player.pos[2] = data.pos[2];
		player.velocity[0] = data.velocity[0];
		player.velocity[1] = data.velocity[1];
		player.velocity[2] = data.velocity[2];
		break;
	case "pos":
		this[p].pos[0] = data.pos[0];
		this[p].pos[1] = data.pos[1];
		this[p].pos[2] = data.pos[2];
		break;
	case "login":
		//Log login attempt
		log("LOGIN from %s @ %s: %s user:'%s' pass:'%s'", 
			this[p].id, data.ip, data.type, data.user, data.pass);
		
		//Allow guests
		if(data.type == loginType.guest)
			login(ws, data);
		
		//Query user database (LOGIN or SIGNUP)
		if (data.type == loginType.login || data.type == loginType.signup)
		sql.query("SELECT pass, status FROM users WHERE user = ?;", [data.user],
			function(error, result) {
				//Reject if error
				if (error) {
					logError("QUERY ERROR:", error);
					return;
				}

				//Reject if signup
				if (data.type == loginType.signup && result.length > 0) {
					send(ws, "login_error", {message: "user already exists"});
					return;
				}

				//User exists
				if (result.length > 0) {
					//Password match
					if (result[0].pass == data.pass) {
						//Check not already online
						if (result[0].status == "online") {
							send(ws, "login_error", {message: "user already logged in"});
							return;
						}
						login(ws, data);
						sql.query("UPDATE users SET status='online' WHERE user=?", 
							[ws[p].name], errorHandler);
					}

					//Password mismatch
					else {
						log("FAIL LOGIN %s", data.user);
						send(ws, "login_error", {
							message: "wrong password"
						});
					}
				}

				//User doesn't exist
				else {
					if (data.type == loginType.signup){
						//Check username length
						if (data.user.length < 3 || data.user.length > 12) {
							send(ws, "login_error", 
								{message: "username length must be between 3 and 12 characters"});
							return;
						}

						//Check password length
						if (data.pass.length < 3 || data.pass.length > 12) {
							send(ws, "login_error", 
								{message: "password length must be between 3 and 12 characters"});
							return;
						}

						//Create new user
						log("NEW USER %s", data.user);
						sql.query("INSERT INTO users (user, pass, user_data, status) VALUES (?, ?, ?, 'online');", 
							[data.user, data.pass, JSON.stringify(ws[p])], errorHandler);
						login(ws, data);
					} else {
						send(ws, "login_error", {message: `user ${data.user} not found`});
					}
				}
			});
		break;
	case "logout":
		log("%s quit", player.name);
		ws.close();
		break;
	case "req_chunk":
		Queue.requests.push({
			world: world,
			pos: data.pos,
			client: ws
		});
		break;
	case "req_chunks":
		Queue.requests.push({
			world: world,
			pos: data.pos,
			count: data.pos.length / 3,
			client: ws
		});
		break;
	case "chunk_query":
		chunk = Chunk.get(world, data.pos);
		if (!chunk || chunk.run != data.run) {
			Queue.requests.push({
				world: world,
				pos: data.pos,
				count: 1,
				client: ws
			});
		} else {
			//send(ws, "chunk_query", data);
		}
		break;
	case "edit":
		Voxels.mod(world, data.pos[0], data.pos[1], data.pos[2], data.value);
		narrowcast(thing, data, Chunk.pos(data.pos), CHUNK_MAXDIST, this[p].id);
		Chunk.getVox(ws[p].world, data.pos).save();
		break;
	case "warp":
		if (World.worlds.has(data.world)) {
			player.world = data.world;
			send(ws, "warp", {
				world: player.world,
				worldinfo: JSON.stringify(World.worlds.get(data.world).props)
			});
			send(ws, "pos", {
				pos: World.worlds.get(data.world).props.spawn
			});
		}
		break;
	case "tp_to":
		if ((other = findPlayer(data.name))) {
			if (other[p].world == player.world)
				send(ws, "pos", {pos: other[p].pos});
		}
		break;
	case "time":
		World.worlds.get(data.world).props.daytime = data.daytime / 65535;
		broadcast("time", {
			world: "",
			daytime: data.daytime
		});
		break;
	default:
		log("RECIEVE UNHANDLED: %s", thing);
		break;
	}
}
function send(client, thingName, instance) {
	//Error check
	if (client.readyState != client.OPEN) return;
	
	//Send
	client.send(encode(thingName, instance));
	//log(`SENT ${thingName} to ${client[p].id}`);
}
function sendChunk(client, pos, chunk=null) {
	//Get chunk if one is not provided
	if (!chunk) chunk = Chunk.get(client[p].world, pos);
	assert(chunk);

	//Conditionally create packed version
	chunk.pack();
	
	//Send to client
	send(client, "chunk", {
		pos: pos,
		flags: chunk.flags,
		run: chunk.run,
		voxels: (chunk.flags & flag.empty) ? emptyData : chunk.packed
	});
}
function encode(thingName, instance) {
	//Encode, add messagetype byte
	let encoded = Thing[thingName].scheme.encode(instance);
	let array = new Uint8Array(encoded.length + 1);
	array[0] = Thing[thingName].ID;
	array.set(encoded, 1);

	return array;
}
function broadcast(thingName, instance, excludeID=-1, includeInactive = false) {
	wss.clients.forEach(function(ws) {
		if (ws[p].id != excludeID 
			&& (ws[p].active || includeInactive))
			send(ws, thingName, instance);
	});
}
function narrowcast(thingName, instance, pos, maxdist, 
	excludeID=-1, includeInactive = false) {
	wss.clients.forEach(function(ws) {
		if (ws[p].id != excludeID 
			&& (ws[p].active || includeInactive)
			&& distance(pos, Chunk.pos(ws[p].pos)) < maxdist)
			send(ws, thingName, instance);
	});
}
function closed(event) {
	//Security
	let ws = this;
	if (!ws[p]) return;
	log("CONNECTION CLOSED %s %s", ws[p].name, ws["ip"]);

	//Check if admin
	if (ws["admin"]) {
		admin = null;
		delete ws[p];
		return;
	}

	//Save player
	ws[p].pos.forEach(function(val, i, arr){
		arr[i] = Math.floor(val);
	});
	sql.query("UPDATE users SET status='offline' WHERE user=?;", 
		[ws[p].name], errorHandler);
	sql.query("UPDATE users SET user_data=? WHERE user=?;", 
		[JSON.stringify(ws[p]), ws[p].name], errorHandler);

	//Remove player client-side
	broadcast("actor_remove", {id: ws[p].id}, ws[p].id);
	delete ws[p];
}
function heartbeat() {
	this.isAlive = true;
}
function living() {
	wss.clients.forEach(function (ws) {
		if (ws["isAlive"] === false) {
			log("TIMEDOUT: %s", ws["ip"]);
			send(ws, "goodbye", {
				reason: "you timed out"
			});
			return ws.terminate();
		}

		ws["isAlive"] = false;
		ws.ping("", false, true);
	});
}
function serverEnd(reason="interrupt") {
	log("EXITING SERVER - " + reason);

	//Stop
	gameLoopTimer.unref();

	//Save chunks
	Queue.saves.flush();
	Queue.lazySaves.flush();
	log("QUEUE flushed saves");

	//Save worlds
	World.save();
	log("saved worlds");

	//Close sockets (and thereby save players)
	for (let client of wss.clients) {
		client.close();
	}

	//Send goodbye message
	broadcast("goodbye", {
		reason: "server terminated"
	});

	//Wait 250ms to exit
	setTimeout(process.exit, 250);
}
function errorHandler(err) {
	if (err) logError(err);
}
function login(ws, data) {
	log("SUCCESS LOGIN %s", data.user);
	ws[p].active = true;
	ws[p].name = data.user || "guest";
	ws["ip"] = data.ip;
	let init_msg = Object.assign({
		version: VERSION,
		worldinfo: JSON.stringify(World.worlds.get(ws[p].world).props)
	}, ws[p]);
	if (data.user)
		sql.query("SELECT user_data FROM users WHERE user=?", [data.user], function(err, data){
			errorHandler(err);
			if (data.length > 0) {
				let obj = JSON.parse(data[0].user_data);
				ws[p].world = obj.world || "default";
				ws[p].pos[0] = obj.pos[0];
				ws[p].pos[1] = obj.pos[1];
				ws[p].pos[2] = obj.pos[2];
				send(ws, "init", init_msg);
			}
		});
	else
		send(ws, "init", init_msg);
}
function connected(ws) {
	ws["ip"] = "...";

	//Assign methods
	ws.ping("", false, true);
	ws.on("pong", heartbeat);
	ws.on("message", recieve);
	ws.on("close", closed);
	ws.on("close", closed);

	//Send array of types of things
	ws.send(JSON.stringify(Thing.Forms));

	//Initialize player
	ws[p] = Thing["player"].create({
		name: "guest",
		pos: World.worlds.get("default").props.spawn.slice(),
		velocity: [0, 0, 0],
		world: "default",
		id: newID(),
		active: false
	});
	log("CONNECTION: from %s ID[%s]", ws["ip"], ws[p].id);
}
function begin() {
	//Server game process
	gameLoopTimer = setInterval(gameLoop, TICK);

	//Websocket activity
	wss.on("connection", connected);

	//Timeout interval
	setInterval(living, 30000);

	//Set all to offline
	sql.query("UPDATE users SET status='offline'", errorHandler);

	//Start server
	log("New Soils Server v" + VERSION);
	server.listen(process.env.PORT || "8080", function listening() {
		log("Listening on %s:%s in %s",
			server.address().address, server.address().port, root);
	});

	//Server termination handlers
	process.on("SIGINT", serverEnd);
	process.on("SIGTERM", serverEnd);
	//process.on("exit", serverEnd);
	//process.on("disconnect", serverEnd);
}

//Other
function distance(a, b) {
	return Math.sqrt(
		Math.pow(a[0] - b[0], 2) 
		+ Math.pow(a[1] - b[1], 2) 
		+ Math.pow(a[2] - b[2], 2));
}

//Startup
console.log("\n\nSTARTING UP SERVER...");
if (!File.existsSync("data")) File.mkdirSync("data");
if (!File.existsSync("data/worlds")) File.mkdirSync("data/worlds");
Block.parseYaml(File.readFileSync(`${root}/public/files/blocks.yaml`));
World.exists("default") ? World.load("default") : new World();
World.exists("flatland") ? World.load("flatland") : new World({
	name: "flatland", type: "flat", seed: 0, spawn: [272, 258, 272], daycycle: 0
});

//Database
sql = mysql.createConnection({
	host: process.env.RDS_HOSTNAME || "", //AWS hostname
	user: process.env.RDS_USERNAME || "", //AWS username
	password: process.env.RDS_PASSWORD || "", //AWS password
	port: process.env.RDS_PORT || "", //AWS port
	database: process.env.RDS_DB_NAME || "" //AWS database name
});
sql.connect(function (error) {
	if (error)
		logError("Cannot connect to database:\n", error);
	else
		log("Database connected!");
	begin();
});
