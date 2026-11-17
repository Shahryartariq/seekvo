/**
 * A made-up drive for the search demo on the landing page.
 * Every path here is invented; the list is built the same way on every load.
 */

export type SampleFile = {
  /** Folder part, with a trailing backslash. */
  dir: string;
  name: string;
  /** Lowercase extension without the dot. */
  ext: string;
  /** Size in bytes. */
  size: number;
  /** Lowercase full path, used for matching. */
  lower: string;
};

const MONTHS = [
  "january", "february", "march", "april", "may", "june",
  "july", "august", "september", "october", "november", "december",
];
const YEARS = [2023, 2024, 2025, 2026];
const TRIPS = ["Hunza", "Lahore", "Skardu", "Karachi", "Murree"];
const CLIENTS = ["northwind", "acme", "globex", "initech", "umbrella", "hooli"];
const PROJECTS = [
  "storefront", "invoice-api", "dashboard", "mobile-app",
  "blog", "chat-server", "wallet-tracker", "portfolio",
];
const MODULES = [
  "index", "router", "auth", "database", "config", "logger",
  "payments", "search", "upload", "email", "cache", "utils",
];
const PACKAGES = [
  "react", "next", "typescript", "eslint", "lodash", "zod", "express", "vite",
  "tailwindcss", "prisma", "axios", "dayjs", "sharp", "uuid", "dotenv", "chalk",
  "commander", "ws", "jsonwebtoken", "bcrypt", "postcss", "esbuild", "rollup",
  "vitest", "playwright", "drizzle-orm", "pg", "redis", "nanoid", "date-fns",
];
const ARTISTS = [
  "Nusrat Fateh Ali Khan", "Coke Studio", "Atif Aslam", "Abida Parveen", "Junoon",
  "Strings", "Ali Sethi", "Vital Signs", "Noori", "Hasan Raheem",
];
const APPS = [
  "vscode", "node", "git", "python", "chrome", "vlc", "7zip", "obs",
  "docker-desktop", "postman",
];
const TOPICS = [
  "roadmap", "pricing", "onboarding", "quarterly-review",
  "hiring", "security", "launch", "budget",
];
const DEPARTMENTS = ["sales", "marketing", "engineering", "support", "finance", "operations"];
const NOTES = [
  "meeting-notes", "ideas", "reading-list", "rust-notes", "sql-cheatsheet", "git-commands",
  "travel-plan", "recipes", "interview-prep", "todo", "books", "passwords-hint",
  "blockchain-notes", "ai-prompts", "workout", "expenses", "gift-ideas", "quotes",
  "house-move", "car-service", "tax-checklist", "resume-draft", "cover-letter", "goals-2026",
  "linux-commands", "docker-notes", "typescript-tips", "regex", "keyboard-shortcuts", "wifi",
];

/** Rough size range per extension, in bytes: [min, max]. */
const SIZE_RANGE: Record<string, [number, number]> = {
  pdf: [90_000, 4_000_000],
  docx: [40_000, 900_000],
  xlsx: [30_000, 2_500_000],
  pptx: [900_000, 40_000_000],
  jpg: [2_200_000, 9_500_000],
  png: [180_000, 3_200_000],
  mp4: [120_000_000, 2_400_000_000],
  mp3: [3_500_000, 11_000_000],
  exe: [40_000_000, 620_000_000],
  zip: [300_000_000, 9_000_000_000],
  ts: [600, 28_000],
  js: [400, 180_000],
  json: [300, 6_000],
  md: [400, 30_000],
};

function pad(n: number, width: number): string {
  return String(n).padStart(width, "0");
}

/** Small deterministic generator so sizes are stable between server and browser. */
function sizeFor(ext: string, index: number): number {
  const [min, max] = SIZE_RANGE[ext] ?? [1_000, 100_000];
  const mixed = Math.imul(index + 1, 2654435761) >>> 0;
  return Math.round(min + (mixed / 0xffffffff) * (max - min));
}

function build(): SampleFile[] {
  const files: SampleFile[] = [];
  const add = (dir: string, name: string) => {
    const dot = name.lastIndexOf(".");
    const ext = dot === -1 ? "" : name.slice(dot + 1).toLowerCase();
    files.push({
      dir,
      name,
      ext,
      size: sizeFor(ext, files.length),
      lower: (dir + name).toLowerCase(),
    });
  };

  for (const year of YEARS) {
    MONTHS.forEach((month, i) => {
      add(`D:\\Finance\\${year}\\`, `invoice-${month}-${year}.pdf`);
      add(`D:\\Finance\\${year}\\`, `invoice-${month}-${year}-paid.pdf`);
      add(`D:\\Finance\\${year}\\`, `bank-statement-${month}.pdf`);
      add("D:\\Backups\\", `backup-${year}-${pad(i + 1, 2)}.zip`);
    });
    for (let q = 1; q <= 4; q++) {
      add("C:\\Users\\sam\\Documents\\Reports\\", `report-q${q}-${year}.docx`);
    }
    for (const client of CLIENTS) {
      add("C:\\Users\\sam\\Documents\\Contracts\\", `contract-${client}-${year}.pdf`);
    }
    for (const dept of DEPARTMENTS) {
      add("D:\\Work\\Spreadsheets\\", `budget-${year}-${dept}.xlsx`);
    }
    TRIPS.forEach((trip, t) => {
      for (let n = 0; n < 60; n++) {
        add(`D:\\Photos\\${year}\\${trip}\\`, `IMG_${pad(1000 + t * 300 + n, 4)}.jpg`);
      }
    });
  }

  for (const trip of TRIPS) {
    for (let n = 1; n <= 12; n++) {
      add(`D:\\Videos\\${trip}\\`, `clip-${pad(n, 2)}.mp4`);
    }
  }

  for (const project of PROJECTS) {
    add(`C:\\Dev\\${project}\\`, "package.json");
    for (const mod of MODULES) {
      add(`C:\\Dev\\${project}\\src\\`, `${mod}.ts`);
    }
    for (const pkg of PACKAGES) {
      add(`C:\\Dev\\${project}\\node_modules\\${pkg}\\`, "index.js");
    }
  }

  for (const artist of ARTISTS) {
    for (let n = 1; n <= 12; n++) {
      add(`D:\\Music\\${artist}\\`, `${pad(n, 2)}-track.mp3`);
    }
  }

  for (const app of APPS) {
    for (let v = 1; v <= 4; v++) {
      add("C:\\Users\\sam\\Downloads\\", `setup-${app}-${v}.${v * 2}.0.exe`);
    }
  }

  for (const topic of TOPICS) {
    for (let v = 1; v <= 3; v++) {
      add("D:\\Work\\Presentations\\", `deck-${topic}-v${v}.pptx`);
    }
  }

  for (const note of NOTES) {
    add("C:\\Users\\sam\\Documents\\Notes\\", `${note}.md`);
  }

  for (let n = 1; n <= 120; n++) {
    const month = pad(((n - 1) % 12) + 1, 2);
    const day = pad(((n * 7) % 28) + 1, 2);
    add("C:\\Users\\sam\\Desktop\\", `Screenshot 2026-${month}-${day} ${pad(n, 3)}.png`);
  }

  for (let n = 1; n <= 200; n++) {
    add("D:\\Scans\\", `Scan_${pad(n, 4)}.pdf`);
  }

  return files;
}

export const SAMPLE_FILES: readonly SampleFile[] = build();
