import { describe, expect, it } from "vitest";
import { FOLDERS } from "./sample-drive";
import { squarify } from "./treemap";

const WIDTH = 1600;
const HEIGHT = 800;

describe("squarify", () => {
  const tiles = squarify(FOLDERS, WIDTH, HEIGHT);
  const total = FOLDERS.reduce((sum, folder) => sum + folder.value, 0);

  it("makes one tile per folder", () => {
    expect(tiles).toHaveLength(FOLDERS.length);
  });

  it("gives each tile an area in proportion to its size", () => {
    for (const tile of tiles) {
      const expected = (tile.item.value / total) * WIDTH * HEIGHT;
      expect(tile.w * tile.h).toBeCloseTo(expected, 4);
    }
  });

  it("keeps every tile inside the frame", () => {
    for (const tile of tiles) {
      expect(tile.x).toBeGreaterThanOrEqual(-1e-6);
      expect(tile.y).toBeGreaterThanOrEqual(-1e-6);
      expect(tile.x + tile.w).toBeLessThanOrEqual(WIDTH + 1e-6);
      expect(tile.y + tile.h).toBeLessThanOrEqual(HEIGHT + 1e-6);
    }
  });

  it("does not let tiles overlap", () => {
    for (let a = 0; a < tiles.length; a++) {
      for (let b = a + 1; b < tiles.length; b++) {
        const first = tiles[a]!;
        const second = tiles[b]!;
        const overlapX = Math.min(first.x + first.w, second.x + second.w) - Math.max(first.x, second.x);
        const overlapY = Math.min(first.y + first.h, second.y + second.h) - Math.max(first.y, second.y);
        expect(overlapX <= 1e-6 || overlapY <= 1e-6).toBe(true);
      }
    }
  });

  it("returns nothing for an empty list or a frame with no area", () => {
    expect(squarify([], WIDTH, HEIGHT)).toEqual([]);
    expect(squarify(FOLDERS, 0, HEIGHT)).toEqual([]);
    expect(squarify([{ value: 0 }], WIDTH, HEIGHT)).toEqual([]);
  });
});
