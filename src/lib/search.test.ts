import { describe, expect, it } from "vitest";
import { SAMPLE_FILES } from "./sample-files";
import { formatSize, highlight, isEmpty, parseQuery, search } from "./search";

describe("parseQuery", () => {
  it("splits words and lowercases them", () => {
    expect(parseQuery("  Invoice   MARCH ")).toEqual({ terms: ["invoice", "march"], ext: null });
  });

  it("reads an ext: filter with or without a dot", () => {
    expect(parseQuery("report ext:PDF")).toEqual({ terms: ["report"], ext: "pdf" });
    expect(parseQuery("ext:.mp4")).toEqual({ terms: [], ext: "mp4" });
  });

  it("ignores an ext: filter with no value", () => {
    expect(parseQuery("ext:")).toEqual({ terms: [], ext: null });
    expect(isEmpty(parseQuery("ext:"))).toBe(true);
  });
});

describe("search", () => {
  it("returns nothing for an empty query", () => {
    expect(search(SAMPLE_FILES, parseQuery("   "), 8)).toEqual({ total: 0, hits: [] });
  });

  it("requires every word to appear in the path", () => {
    const { total, hits } = search(SAMPLE_FILES, parseQuery("invoice march 2026"), 50);
    expect(total).toBe(2);
    for (const hit of hits) {
      expect(hit.lower).toContain("invoice");
      expect(hit.lower).toContain("march");
      expect(hit.lower).toContain("2026");
    }
  });

  it("matches folder names as well as file names", () => {
    const { total } = search(SAMPLE_FILES, parseQuery("node_modules"), 1);
    expect(total).toBe(240);
  });

  it("filters by extension exactly", () => {
    const { total, hits } = search(SAMPLE_FILES, parseQuery("ext:mp4"), 100);
    expect(total).toBe(60);
    expect(hits.every((hit) => hit.ext === "mp4")).toBe(true);
    expect(search(SAMPLE_FILES, parseQuery("ext:mp"), 1).total).toBe(0);
  });

  it("counts every match but returns only the first few", () => {
    const { total, hits } = search(SAMPLE_FILES, parseQuery("img"), 8);
    expect(total).toBe(1200);
    expect(hits).toHaveLength(8);
  });
});

describe("highlight", () => {
  it("marks each match and keeps the original casing", () => {
    expect(highlight("Invoice-March.pdf", ["invoice", "pdf"])).toEqual([
      { text: "Invoice", hit: true },
      { text: "-March.", hit: false },
      { text: "pdf", hit: true },
    ]);
  });

  it("merges overlapping matches", () => {
    expect(highlight("abcdef", ["abc", "bcd"])).toEqual([
      { text: "abcd", hit: true },
      { text: "ef", hit: false },
    ]);
  });

  it("returns the text unmarked when nothing matches", () => {
    expect(highlight("notes.md", ["xyz"])).toEqual([{ text: "notes.md", hit: false }]);
    expect(highlight("", ["a"])).toEqual([]);
  });

  it("puts the segments back together into the original text", () => {
    const text = "D:\\Photos\\2025\\Hunza\\IMG_2041.jpg";
    const joined = highlight(text, ["hunza", "20", "jpg"])
      .map((segment) => segment.text)
      .join("");
    expect(joined).toBe(text);
  });
});

describe("formatSize", () => {
  it("picks a readable unit", () => {
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(1536)).toBe("1.5 KB");
    expect(formatSize(250 * 1024 * 1024)).toBe("250 MB");
    expect(formatSize(4.2 * 1024 ** 3)).toBe("4.2 GB");
  });
});

describe("sample files", () => {
  it("builds the same list every time, with no repeated paths", () => {
    const paths = new Set(SAMPLE_FILES.map((file) => file.dir + file.name));
    expect(paths.size).toBe(SAMPLE_FILES.length);
    expect(SAMPLE_FILES.length).toBeGreaterThan(2000);
  });
});
