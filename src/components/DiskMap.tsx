import {
  DRIVE_CAPACITY_GB,
  DRIVE_USED_GB,
  FOLDERS,
  KIND_LABELS,
  type Folder,
  type FolderKind,
} from "@/lib/sample-drive";
import { squarify } from "@/lib/treemap";

const KINDS = Object.keys(KIND_LABELS) as FolderKind[];

/** Rough label metrics in CSS pixels, used to leave out labels that would be cut off. */
const CHAR_WIDTH = 7.6;
const TILE_PADDING = 18;
const LINE_HEIGHT = 17;

/** How many lines a name needs in the given width, or null if a word is too long to fit. */
function linesNeeded(name: string, available: number): number | null {
  let lines = 1;
  let used = 0;
  for (const word of name.split(" ")) {
    const wordWidth = word.length * CHAR_WIDTH;
    if (wordWidth > available) return null;
    const withSpace = used === 0 ? wordWidth : used + CHAR_WIDTH + wordWidth;
    if (withSpace > available) {
      lines++;
      used = wordWidth;
    } else {
      used = withSpace;
    }
  }
  return lines;
}

type FrameProps = {
  width: number;
  height: number;
  shape: "wide" | "tall";
  /** About how wide the frame is drawn on screen, in CSS pixels. */
  drawnWidth: number;
};

function Frame({ width, height, shape, drawnWidth }: FrameProps) {
  const tiles = squarify<Folder>(FOLDERS, width, height);
  const scale = drawnWidth / width;
  return (
    <div className={`map-frame map-frame-${shape}`}>
      {tiles.map((tile) => {
        const drawnW = tile.w * scale;
        const drawnH = tile.h * scale;
        const lines = linesNeeded(tile.item.name, drawnW - TILE_PADDING);
        const nameHeight = lines === null ? Infinity : TILE_PADDING + lines * LINE_HEIGHT;
        const showName = drawnH >= nameHeight;
        const showSize = drawnH >= nameHeight + LINE_HEIGHT;
        return (
          <div
            key={tile.item.name}
            className={`map-tile map-${tile.item.kind}`}
            style={{
              left: `${(tile.x / width) * 100}%`,
              top: `${(tile.y / height) * 100}%`,
              width: `${(tile.w / width) * 100}%`,
              height: `${(tile.h / height) * 100}%`,
            }}
          >
            {showName ? <span className="map-name">{tile.item.name}</span> : null}
            {showSize ? <span className="map-size">{tile.item.value} GB</span> : null}
          </div>
        );
      })}
    </div>
  );
}

export function DiskMap() {
  return (
    <figure className="map">
      {/* The picture is decorative for assistive tech; the same numbers follow as a list. */}
      <div aria-hidden="true">
        <Frame width={1600} height={800} shape="wide" drawnWidth={590} />
        <Frame width={800} height={1000} shape="tall" drawnWidth={320} />
        <ul className="map-key">
          {KINDS.map((kind) => (
            <li key={kind}>
              <span className={`map-swatch map-${kind}`} />
              {KIND_LABELS[kind]}
            </li>
          ))}
        </ul>
      </div>
      <figcaption>
        A sample {DRIVE_CAPACITY_GB} GB drive with {DRIVE_USED_GB} GB in use. Each tile is drawn to
        scale.
      </figcaption>
      <ul className="visually-hidden">
        {FOLDERS.map((folder) => (
          <li key={folder.name}>
            {folder.name}: {folder.value} GB, {KIND_LABELS[folder.kind]}
          </li>
        ))}
      </ul>
    </figure>
  );
}
