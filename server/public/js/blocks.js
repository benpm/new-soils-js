/// <reference path="./js-yaml.min.js" />

var Yaml = (typeof require !== "undefined") ? require("js-yaml") : jsyaml;

class Block {
	constructor(id, name, top, sides, bottom) {
		//Assign ID if others not specified
		top = top ? top : id;
		sides = sides ? sides : top;
		bottom = bottom ? bottom : top;

		//Create index array []
		this.faces = [sides, top, sides, bottom, sides];
		this.name = name;
		this.id = id;
	}
	static create(name="", indices=[0, 0, 0]) {
		Block[name] = new Block(
			Block.blocks.length,
			name,
			indices[0],
			indices[1],
			indices[2]);
		Block.blocks.push(Block[name]);
	}
	static parseYaml(data) {
		let parsed = Yaml.safeLoad(data);
		for (let name in parsed) {
			Block.create(name, parsed[name].faces);
		}
	}
}
Block.blocks = [];

if (typeof module !== "undefined") module.exports = Block;
