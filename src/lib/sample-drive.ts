/**
 * Made-up numbers for the pictures on the landing page:
 * the disk map, the duplicate groups and the rename suggestions.
 */

export type FolderKind =
  | "photos"
  | "media"
  | "apps"
  | "code"
  | "archives"
  | "system"
  | "documents";

export type Folder = {
  name: string;
  /** Size in gigabytes. */
  value: number;
  kind: FolderKind;
};

export const DRIVE_CAPACITY_GB = 512;

export const FOLDERS: readonly Folder[] = [
  { name: "Videos", value: 92, kind: "media" },
  { name: "Photos", value: 74, kind: "photos" },
  { name: "Games", value: 61, kind: "apps" },
  { name: "Backups", value: 42, kind: "archives" },
  { name: "node_modules", value: 38, kind: "code" },
  { name: "Virtual machines", value: 36, kind: "system" },
  { name: "Downloads", value: 31, kind: "archives" },
  { name: "Windows", value: 29, kind: "system" },
  { name: "Program Files", value: 24, kind: "apps" },
  { name: "Music", value: 16, kind: "media" },
  { name: "Dev projects", value: 11, kind: "code" },
  { name: "Recycle Bin", value: 9, kind: "system" },
  { name: "Documents", value: 8, kind: "documents" },
  { name: "Temp files", value: 7, kind: "system" },
  { name: "Scans", value: 5, kind: "documents" },
  { name: "Desktop", value: 3, kind: "documents" },
];

export const DRIVE_USED_GB = FOLDERS.reduce((total, folder) => total + folder.value, 0);

export const KIND_LABELS: Record<FolderKind, string> = {
  photos: "Photos",
  media: "Video and music",
  apps: "Apps and games",
  code: "Code",
  archives: "Archives and downloads",
  system: "System",
  documents: "Documents",
};

export type DuplicateGroup = {
  name: string;
  each: string;
  locations: string[];
};

export const DUPLICATE_GROUPS: readonly DuplicateGroup[] = [
  {
    name: "wedding-film-final.mp4",
    each: "4.2 GB",
    locations: ["D:\\Videos\\Family\\", "D:\\Backups\\old-laptop\\Videos\\", "C:\\Users\\sam\\Desktop\\"],
  },
  {
    name: "setup-docker-desktop-4.8.0.exe",
    each: "580 MB",
    locations: ["C:\\Users\\sam\\Downloads\\", "D:\\Installers\\"],
  },
  {
    name: "IMG_2041.jpg",
    each: "6.1 MB",
    locations: [
      "D:\\Photos\\2025\\Hunza\\",
      "D:\\Photos\\To sort\\",
      "D:\\Backups\\phone-2025\\",
      "C:\\Users\\sam\\Pictures\\WhatsApp\\",
    ],
  },
];

export type RenameSuggestion = {
  before: string;
  after: string;
  folder: string;
};

export const RENAME_SUGGESTIONS: readonly RenameSuggestion[] = [
  {
    before: "Scan_0043.pdf",
    after: "Electricity bill, October 2026.pdf",
    folder: "D:\\Finance\\2026\\Bills\\",
  },
  {
    before: "IMG_1187.jpg",
    after: "Hunza valley at sunrise.jpg",
    folder: "D:\\Photos\\2025\\Hunza\\",
  },
  {
    before: "document (3).docx",
    after: "Rental agreement, second draft.docx",
    folder: "C:\\Users\\sam\\Documents\\Contracts\\",
  },
  {
    before: "Screenshot 2026-03-14 041.png",
    after: "Flight booking confirmation.png",
    folder: "C:\\Users\\sam\\Documents\\Travel\\",
  },
];
