import type { SampleFile } from "./sample-files";

export type Query = {
  /** Lowercase words; a file must contain all of them somewhere in its path. */
  terms: string[];
  /** Lowercase extension from an `ext:` filter, or null. */
  ext: string | null;
};

export type SearchResult = {
  total: number;
  hits: SampleFile[];
};

export type Segment = { text: string; hit: boolean };

/** Turn what the person typed into words and an optional `ext:` filter. */
export function parseQuery(raw: string): Query {
  const terms: string[] = [];
  let ext: string | null = null;
  for (const word of raw.toLowerCase().split(/\s+/)) {
    if (!word) continue;
    if (word.startsWith("ext:")) {
      const value = word.slice(4).replace(/^\./, "");
      if (value) ext = value;
      continue;
    }
    terms.push(word);
  }
  return { terms, ext };
}

export function isEmpty(query: Query): boolean {
  return query.terms.length === 0 && query.ext === null;
}

/** Count every match and return the first `limit` of them. */
export function search(
  files: readonly SampleFile[],
  query: Query,
  limit: number,
): SearchResult {
  if (isEmpty(query)) return { total: 0, hits: [] };
  const hits: SampleFile[] = [];
  let total = 0;
  for (const file of files) {
    if (query.ext !== null && file.ext !== query.ext) continue;
    let matches = true;
    for (const term of query.terms) {
      if (!file.lower.includes(term)) {
        matches = false;
        break;
      }
    }
    if (!matches) continue;
    total++;
    if (hits.length < limit) hits.push(file);
  }
  return { total, hits };
}

/** Split text into runs that match one of the terms and runs that don't. */
export function highlight(text: string, terms: readonly string[]): Segment[] {
  const lower = text.toLowerCase();
  const ranges: Array<[number, number]> = [];
  for (const term of terms) {
    if (!term) continue;
    let from = 0;
    for (;;) {
      const at = lower.indexOf(term, from);
      if (at === -1) break;
      ranges.push([at, at + term.length]);
      from = at + term.length;
    }
  }
  if (ranges.length === 0) return text ? [{ text, hit: false }] : [];

  ranges.sort((a, b) => a[0] - b[0]);
  const merged: Array<[number, number]> = [];
  for (const [start, end] of ranges) {
    const last = merged[merged.length - 1];
    if (last && start <= last[1]) last[1] = Math.max(last[1], end);
    else merged.push([start, end]);
  }

  const segments: Segment[] = [];
  let cursor = 0;
  for (const [start, end] of merged) {
    if (start > cursor) segments.push({ text: text.slice(cursor, start), hit: false });
    segments.push({ text: text.slice(start, end), hit: true });
    cursor = end;
  }
  if (cursor < text.length) segments.push({ text: text.slice(cursor), hit: false });
  return segments;
}

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

export function formatSize(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit++;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}
