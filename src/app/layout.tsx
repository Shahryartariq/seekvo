import type { Metadata, Viewport } from "next";
import "@fontsource-variable/atkinson-hyperlegible-next/wght.css";
import "@fontsource-variable/atkinson-hyperlegible-mono/wght.css";
import "./globals.css";

const siteUrl = process.env.SITE_URL ?? "https://seekvo.vercel.app";
const title = "Seekvo: instant file search and cleanup for Windows";
const description =
  "Seekvo is a Windows app in development. It finds any file as you type, shows where your disk space went, clears duplicate files and tidies folders with AI that runs on your own PC.";

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  title,
  description,
  applicationName: "Seekvo",
  authors: [{ name: "Muhammad Shehryar", url: "https://github.com/Shahryartariq" }],
  alternates: { canonical: "/" },
  openGraph: {
    type: "website",
    url: "/",
    siteName: "Seekvo",
    title,
    description,
  },
  twitter: { card: "summary", title, description },
};

export const viewport: Viewport = {
  themeColor: "#0c3b2e",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
