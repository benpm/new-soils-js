/*global postMessage, window*/

var GreedyMesh = (function () {
	//Cache buffer internally
	const EDGE = 3;
	const CORNER = 2;
	var mask = new Int32Array(4096);
	var aoff = [
		[-1, -1],
		[-1,  1],
		[ 1, -1],
		[ 1,  1],
		[-1,  0],
		[ 1,  0],
		[ 0, -1],
		[ 0,  1]
	];
	var aol = [
		[0,  1, 0, 1, 0, 1],
		[0, -1, 0, 1, 0, 1],
		[ 1, 0, 0, 0, 1, 1],
		[-1, 0, 0, 0, 1, 1],
		[0, 0,  1, 1, 2, 0],
		[0, 0, -1, 1, 2, 0]
	];
	var aoOffsets = [[-1, 0], [-1, -1], [0, -1], [0, 0]];
	function occlusion(side1, side2, corner) {
		if (side1 && side2) {
			return 0;
		}
		return 3 - (side1 + side2 + corner);
	}

	return function (volume, dims, doAO) {
		function aoat(clearx, cleary, norm, x, y, z, ix, iy) {
			return query(
				x + norm[0] + clearx[0] * ix + cleary[0] * iy,
				y + norm[1] + clearx[1] * ix + cleary[1] * iy,
				z + norm[2] + clearx[2] * ix + cleary[2] * iy
			);
		}
		function aof(i, j, k) {
			var r = 0; var ii = 0;
			for(var iao = 0; iao < 6; iao++) {
				if (!query(i + aol[iao][0], j + aol[iao][1], k + aol[iao][2])) {
					for(ii = 0; ii < 4; ii++) {
						if (query(
							i + aol[iao][0] + aoff[ii][0] * aol[iao][3],
							j + aol[iao][1] + aoff[ii][0] * (aol[iao][4] & 1) + aoff[ii][1] * (aol[iao][4] & 2),
							k + aol[iao][2] + aoff[ii][1] * aol[iao][5])) {
							r += (ii + 1) * (iao + 1);
						}
					}
					for(ii = 4; ii < 8; ii++) {
						if (query(
							i + aol[iao][0] + aoff[ii][0] * aol[iao][3],
							j + aol[iao][1] + aoff[ii][0] * (aol[iao][4] & 1) + aoff[ii][1] * (aol[iao][4] & 2),
							k + aol[iao][2] + aoff[ii][1] * aol[iao][5])) {
							r += (ii) * (iao + 2);
						}
					}
				}
			}
			return r;
		}
		function query(i, j, k) {
			return volume[i + dims[0] * (j + dims[1] * k)];
		}
		function queryAO(i, j, k) {
			var val = volume[i + dims[0] * (j + dims[1] * k)];
			if (val) return val + aof(i, j, k) * 256;
			else return 0;
		}

		var vf = query;
		var vertex_count = 0;
		if (doAO) vf = queryAO;

		//Sweep over 3-axes
		var vertices = [], faces = [], blockIDs = [], normals = [], ao = [];
		for (var d = 0; d < 3; ++d) {
			var w, ii, i, j, k, l, width, height, u = (d + 1) % 3,
				v = (d + 2) % 3,
				x = [0, 0, 0],
				q = [0, 0, 0],
				pp = [0, 0, 0],
				du = [0, 0, 0],
				dv = [0, 0, 0],
				cx = [0, 0, 0],
				cy = [0, 0, 0],
				norm = [0, 0, 0];
			if (mask.length < dims[u] * dims[v]) {
				mask = new Int32Array(dims[u] * dims[v]);
			}
			q[d] = 1;
			for (x[d] = -1; x[d] < dims[d];) {
				//Compute mask
				var n = 0, c = 0, a = 0, b = 0, done = false;
				for (x[v] = 0; x[v] < dims[v]; ++x[v])
					for (x[u] = 0; x[u] < dims[u]; ++x[u], ++n) {
						a = (0 <= x[d] ? vf(x[0], x[1], x[2]) : 0);
						b = (x[d] < dims[d] - 1 ? vf(x[0] + q[0], x[1] + q[1], x[2] + q[2]) : 0);
						if (!!a == !!b) {
							mask[n] = 0;
						} else if (a) {
							mask[n] = a;
						} else {
							mask[n] = -b;
						}
					}
				//Increment x[d]
				++x[d];
				//Generate mesh for mask using lexicographic ordering
				n = 0;
				for (j = 0; j < dims[v]; ++j)
					for (i = 0; i < dims[u];) {
						c = mask[n];
						if (c) {
							//Compute width
							for (width = 1; c === mask[n + width] && i + width < dims[u]; ++width) {continue;}
							//Compute height (this is slightly awkward
							done = false;
							for (height = 1; j + height < dims[v]; ++height) {
								for (k = 0; k < width; ++k) {
									if (c !== mask[n + k + height * dims[u]]) {
										done = true;
										break;
									}
								}
								if (done) {
									break;
								}
							}

							//Setup
							x[u] = i; x[v] = j;
							du.fill(0); dv.fill(0);
							cx.fill(0); cy.fill(0);

							//Determine normals
							if (c > 0) {
								dv[v] = height;
								du[u] = width;
								norm[0] = Number(d == 0);
								norm[1] = Number(d == 1);
								norm[2] = Number(d == 2);
								cy[(d + 2) % 3] = cx[(d + 1) % 3] = 1;
							} else {
								c = -c;
								du[v] = height;
								dv[u] = width;
								norm[0] = -Number(d == 0);
								norm[1] = -Number(d == 1);
								norm[2] = -Number(d == 2);
								cx[(d + 2) % 3] = cy[(d + 1) % 3] = 1;
							}
							normals.push(norm[0], norm[1], norm[2]);
							normals.push(norm[0], norm[1], norm[2]);
							normals.push(norm[0], norm[1], norm[2]);
							normals.push(norm[0], norm[1], norm[2]);

							//Add vertices
							vertex_count = vertices.length / 3;
							vertices.push(x[0], x[1], x[2]);
							vertices.push(x[0] + du[0], x[1] + du[1], x[2] + du[2]);
							vertices.push(x[0] + du[0] + dv[0], x[1] + du[1] + dv[1], x[2] + du[2] + dv[2]);
							vertices.push(x[0] + dv[0], x[1] + dv[1], x[2] + dv[2]);

							//Add faces
							faces.push(vertex_count, vertex_count + 1, vertex_count + 2);
							faces.push(vertex_count, vertex_count + 2, vertex_count + 3);

							//Add blockID
							blockIDs.push(c & 255);

							//Calculate AO value
							if (doAO) {
								if (c >> 8
									&& x[0] > 0 && x[0] < dims[0] - 1
									&& x[1] > 0 && x[1] < dims[1] - 1
									&& x[2] > 0 && x[2] < dims[2] - 1) {
									for (ii = vertex_count; ii < vertex_count + 4; ii++) {
										pp[0] = vertices[ii * 3 + 0];
										pp[1] = vertices[ii * 3 + 1];
										pp[2] = vertices[ii * 3 + 2];
										w = ii & 3;
										if (norm[1]) w = w == 1 ? 3 : w == 3 ? 1 : w;
										if (norm[0] == 1) norm[0] = 0;
										if (norm[1] == 1) norm[1] = 0;
										if (norm[2] == 1) norm[2] = 0;
										ao.push(0.1 + occlusion(
											!!aoat(cx, cy, norm, pp[0], pp[1], pp[2],
											aoOffsets[(w + 0) % 4][0], aoOffsets[(w + 0) % 4][1]),
											!!aoat(cx, cy, norm, pp[0], pp[1], pp[2],
											aoOffsets[(w + 2) % 4][0], aoOffsets[(w + 2) % 4][1]),
											!!aoat(cx, cy, norm, pp[0], pp[1], pp[2],
											aoOffsets[(w + 1) % 4][0], aoOffsets[(w + 1) % 4][1])) * 0.3);
									}
								} else {
									ao.push(1);
									ao.push(1);
									ao.push(1);
									ao.push(1);
								}
							}

							//Zero-out mask
							for (l = 0; l < height; ++l)
								for (k = 0; k < width; ++k) {
									mask[n + k + l * dims[u]] = 0;
								}
							
							//Increment counters and continue
							i += width;
							n += width;
						} else {
							++i;
							++n;
						}
					}
			}
		}
		return {
			vertices: vertices,
			faces: faces,
			blockIDs: blockIDs,
			normals: normals,
			ao: ao
		};
	};
})();

onmessage = function(msg) {
	var chunk = msg.data;
	var result = GreedyMesh(chunk.data, [chunk.size, chunk.size, chunk.size], chunk.ao);
	postMessage({pos: chunk.pos, result: result, id: chunk.id});
};


