# Changelog - TerrainLab

* [x] Integrate all noise functions from [this GLSL shader](https://gist.githubusercontent.com/benpm/4f8ad4c320ca68e443c62bfa67755068/raw/3aff41d16b00cb1279a5a820d30fc49b906cd736/noise.glsl)
  — `Noise` + `Fractal Noise` nodes with a 10-mode dropdown (Value, Perlin,
  Simplex, Worley, Voronoi, Gabor, Crater, Wool, Stone, Wavelet), ported to
  both the CPU oracle (`soils-worldgen::noise_modes`, f32) and the GPU shader
  from the same source. Excluded as not world-coordinate fields: the
  pixel-index hashes (blue/hilbert/ign/golden_ign), `scratches` (needs
  `fwidth`), and the 3D `*13` variants (the graph is strictly 2D).
