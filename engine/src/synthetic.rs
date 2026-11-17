//! A made-up drive of any size, for benchmarks and experiments.
//!
//! The tree is shaped like a real one: code projects with deep
//! `node_modules` folders, photo folders with thousands of similar names,
//! and documents with dates in their names. The same size always produces
//! the same tree.

use crate::index::{BuildError, EntryId, Index, IndexBuilder, Kind};

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const MODULES: [&str; 12] = [
    "index", "router", "auth", "database", "config", "logger", "payments", "search", "upload",
    "email", "cache", "utils",
];
const PACKAGES: [&str; 30] = [
    "react",
    "next",
    "typescript",
    "eslint",
    "lodash",
    "zod",
    "express",
    "vite",
    "tailwindcss",
    "prisma",
    "axios",
    "dayjs",
    "sharp",
    "uuid",
    "dotenv",
    "chalk",
    "commander",
    "ws",
    "jsonwebtoken",
    "bcrypt",
    "postcss",
    "esbuild",
    "rollup",
    "vitest",
    "playwright",
    "drizzle-orm",
    "pg",
    "redis",
    "nanoid",
    "date-fns",
];
const TRIPS: [&str; 5] = ["Hunza", "Lahore", "Skardu", "Karachi", "Murree"];

/// A tiny deterministic generator for file sizes.
struct Sizes(u64);

impl Sizes {
    fn next(&mut self, min: u64, max: u64) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        min + self.0 % (max - min)
    }
}

/// Build a drive with at least `files` files.
pub fn build(files: usize) -> Result<Index, BuildError> {
    let mut b = IndexBuilder::new();
    let mut sizes = Sizes(0x9E37_79B9_7F4A_7C15);
    let mut made = 0usize;

    let c = b.add(None, "C:\\", Kind::Dir, 0)?;
    let d = b.add(None, "D:\\", Kind::Dir, 0)?;
    let dev = b.add(Some(c), "Dev", Kind::Dir, 0)?;
    let photos = b.add(Some(d), "Photos", Kind::Dir, 0)?;
    let finance = b.add(Some(d), "Finance", Kind::Dir, 0)?;

    let mut round = 0usize;
    while made < files {
        made += project(&mut b, &mut sizes, dev, round)?;
        if made >= files {
            break;
        }
        made += album(&mut b, &mut sizes, photos, round)?;
        if made >= files {
            break;
        }
        made += paperwork(&mut b, &mut sizes, finance, round)?;
        round += 1;
    }
    Ok(b.finish())
}

/// A code project: a few source files and a `node_modules` folder.
fn project(
    b: &mut IndexBuilder,
    sizes: &mut Sizes,
    dev: EntryId,
    round: usize,
) -> Result<usize, BuildError> {
    let mut made = 0;
    let root = b.add(Some(dev), &format!("project-{round:05}"), Kind::Dir, 0)?;
    b.add(
        Some(root),
        "package.json",
        Kind::File,
        sizes.next(300, 6_000),
    )?;
    made += 1;
    let src = b.add(Some(root), "src", Kind::Dir, 0)?;
    for module in MODULES {
        b.add(
            Some(src),
            &format!("{module}.ts"),
            Kind::File,
            sizes.next(600, 28_000),
        )?;
        made += 1;
    }
    let modules = b.add(Some(root), "node_modules", Kind::Dir, 0)?;
    for package in PACKAGES {
        let folder = b.add(Some(modules), package, Kind::Dir, 0)?;
        b.add(
            Some(folder),
            "index.js",
            Kind::File,
            sizes.next(400, 180_000),
        )?;
        b.add(
            Some(folder),
            "package.json",
            Kind::File,
            sizes.next(300, 6_000),
        )?;
        made += 2;
    }
    Ok(made)
}

/// A photo folder with 200 camera files.
fn album(
    b: &mut IndexBuilder,
    sizes: &mut Sizes,
    photos: EntryId,
    round: usize,
) -> Result<usize, BuildError> {
    let trip = TRIPS[round % TRIPS.len()];
    let year = 2015 + (round / TRIPS.len()) % 12;
    let folder = b.add(
        Some(photos),
        &format!("{year} {trip} {round:05}"),
        Kind::Dir,
        0,
    )?;
    for n in 0..200 {
        let name = format!("IMG_{:04}.jpg", (round * 37 + n) % 10_000);
        b.add(
            Some(folder),
            &name,
            Kind::File,
            sizes.next(2_200_000, 9_500_000),
        )?;
    }
    Ok(200)
}

/// A year of invoices and bank statements.
fn paperwork(
    b: &mut IndexBuilder,
    sizes: &mut Sizes,
    finance: EntryId,
    round: usize,
) -> Result<usize, BuildError> {
    let year = 2000 + round % 27;
    let folder = b.add(Some(finance), &format!("{year}-{round:05}"), Kind::Dir, 0)?;
    for month in MONTHS {
        let invoice = format!("Invoice-{month}-{year}.pdf");
        b.add(
            Some(folder),
            &invoice,
            Kind::File,
            sizes.next(90_000, 4_000_000),
        )?;
        let statement = format!("bank-statement-{month}.pdf");
        b.add(
            Some(folder),
            &statement,
            Kind::File,
            sizes.next(90_000, 4_000_000),
        )?;
    }
    Ok(24)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Kind;
    use crate::query::Query;

    fn count_files(index: &Index) -> usize {
        (0..index.len() as u32)
            .filter(|&id| index.kind(id) == Some(Kind::File))
            .count()
    }

    #[test]
    fn makes_at_least_the_files_asked_for() {
        let index = build(5_000).unwrap();
        let files = count_files(&index);
        assert!(files >= 5_000);
        // It stops within one folder's worth of the target.
        assert!(files < 5_300);
    }

    #[test]
    fn the_same_size_gives_the_same_drive() {
        let first = build(2_000).unwrap();
        let second = build(2_000).unwrap();
        assert_eq!(first.len(), second.len());
        let query = Query::parse("invoice march");
        assert_eq!(first.search(&query, 50), second.search(&query, 50));
        for id in [0, 7, 100, first.len() as u32 - 1] {
            assert_eq!(first.path(id), second.path(id));
            assert_eq!(first.size(id), second.size(id));
        }
    }

    #[test]
    fn holds_the_kinds_of_files_the_benchmark_searches_for() {
        let index = build(3_000).unwrap();
        for text in ["invoice march", "ext:jpg", "react index", "img_00"] {
            assert!(index.search(&Query::parse(text), 1).total > 0, "{text}");
        }
    }
}
