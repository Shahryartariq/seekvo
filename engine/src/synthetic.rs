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

/// The newest time of last change on the made-up drive: a day in 2026.
const NEWEST: u32 = 1_790_000_000;

/// Add an entry with a made-up time of last change. The times are spread
/// over about six years and are the same on every run.
fn add(
    b: &mut IndexBuilder,
    parent: Option<EntryId>,
    name: &str,
    kind: Kind,
    size: u64,
) -> Result<EntryId, BuildError> {
    let n = b.len() as u64;
    let days = (n.wrapping_mul(2_654_435_761) >> 7) % 2_200;
    let modified = NEWEST - (days * 86_400 + n % 86_400) as u32;
    b.add_dated(parent, name, kind, size, modified)
}

/// Build a drive with at least `files` files.
pub fn build(files: usize) -> Result<Index, BuildError> {
    let mut b = IndexBuilder::new();
    let mut sizes = Sizes(0x9E37_79B9_7F4A_7C15);
    let mut made = 0usize;

    let c = add(&mut b, None, "C:\\", Kind::Dir, 0)?;
    let d = add(&mut b, None, "D:\\", Kind::Dir, 0)?;
    let dev = add(&mut b, Some(c), "Dev", Kind::Dir, 0)?;
    let photos = add(&mut b, Some(d), "Photos", Kind::Dir, 0)?;
    let finance = add(&mut b, Some(d), "Finance", Kind::Dir, 0)?;

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
    let root = add(b, Some(dev), &format!("project-{round:05}"), Kind::Dir, 0)?;
    add(
        b,
        Some(root),
        "package.json",
        Kind::File,
        sizes.next(300, 6_000),
    )?;
    made += 1;
    let src = add(b, Some(root), "src", Kind::Dir, 0)?;
    for module in MODULES {
        add(
            b,
            Some(src),
            &format!("{module}.ts"),
            Kind::File,
            sizes.next(600, 28_000),
        )?;
        made += 1;
    }
    let modules = add(b, Some(root), "node_modules", Kind::Dir, 0)?;
    for package in PACKAGES {
        let folder = add(b, Some(modules), package, Kind::Dir, 0)?;
        add(
            b,
            Some(folder),
            "index.js",
            Kind::File,
            sizes.next(400, 180_000),
        )?;
        add(
            b,
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
    let folder = add(
        b,
        Some(photos),
        &format!("{year} {trip} {round:05}"),
        Kind::Dir,
        0,
    )?;
    for n in 0..200 {
        let name = format!("IMG_{:04}.jpg", (round * 37 + n) % 10_000);
        add(
            b,
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
    let folder = add(
        b,
        Some(finance),
        &format!("{year}-{round:05}"),
        Kind::Dir,
        0,
    )?;
    for month in MONTHS {
        let invoice = format!("Invoice-{month}-{year}.pdf");
        add(
            b,
            Some(folder),
            &invoice,
            Kind::File,
            sizes.next(90_000, 4_000_000),
        )?;
        let statement = format!("bank-statement-{month}.pdf");
        add(
            b,
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
