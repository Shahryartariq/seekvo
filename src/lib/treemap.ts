/**
 * Squarified treemap layout (Bruls, Huizing and van Wijk).
 * Splits a rectangle into tiles whose areas are proportional to their values,
 * keeping the tiles as close to square as it can.
 */

export type TreemapItem = { value: number };

export type Tile<T> = {
  item: T;
  x: number;
  y: number;
  w: number;
  h: number;
};

type Sized<T> = { item: T; area: number };

function sumAreas<T>(row: readonly Sized<T>[]): number {
  let total = 0;
  for (const entry of row) total += entry.area;
  return total;
}

/** The worst (largest) aspect ratio a row would have when laid along `side`. */
function worst<T>(row: readonly Sized<T>[], side: number): number {
  const total = sumAreas(row);
  let max = 0;
  let min = Infinity;
  for (const entry of row) {
    if (entry.area > max) max = entry.area;
    if (entry.area < min) min = entry.area;
  }
  const sideSquared = side * side;
  const totalSquared = total * total;
  return Math.max((sideSquared * max) / totalSquared, totalSquared / (sideSquared * min));
}

export function squarify<T extends TreemapItem>(
  items: readonly T[],
  width: number,
  height: number,
): Tile<T>[] {
  const positive = items.filter((item) => item.value > 0);
  const totalValue = positive.reduce((acc, item) => acc + item.value, 0);
  if (totalValue === 0 || width <= 0 || height <= 0) return [];

  const scale = (width * height) / totalValue;
  const queue: Sized<T>[] = [...positive]
    .sort((a, b) => b.value - a.value)
    .map((item) => ({ item, area: item.value * scale }));

  const tiles: Tile<T>[] = [];
  let x = 0;
  let y = 0;
  let w = width;
  let h = height;
  let row: Sized<T>[] = [];

  const flush = () => {
    const total = sumAreas(row);
    if (w >= h) {
      // Stack the row top to bottom in a strip on the left.
      const thickness = total / h;
      let offset = 0;
      for (const entry of row) {
        const length = entry.area / thickness;
        tiles.push({ item: entry.item, x, y: y + offset, w: thickness, h: length });
        offset += length;
      }
      x += thickness;
      w -= thickness;
    } else {
      // Lay the row left to right in a strip along the top.
      const thickness = total / w;
      let offset = 0;
      for (const entry of row) {
        const length = entry.area / thickness;
        tiles.push({ item: entry.item, x: x + offset, y, w: length, h: thickness });
        offset += length;
      }
      y += thickness;
      h -= thickness;
    }
    row = [];
  };

  for (const entry of queue) {
    const side = Math.min(w, h);
    if (row.length === 0 || worst([...row, entry], side) <= worst(row, side)) {
      row.push(entry);
    } else {
      flush();
      row.push(entry);
    }
  }
  if (row.length > 0) flush();

  return tiles;
}
