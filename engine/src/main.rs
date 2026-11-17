//! Command-line front end for trying the engine out.

use std::hint::black_box;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use seekvo_engine::{Index, Kind, Query, Scope, mft, ntfs, scan, synthetic};

const USAGE: &str = "\
Usage:
  seekvo-engine drive <letter> [words...] [--path] [--limit N]
                      [--reader table|records]
      Index a whole NTFS drive from its file table, then search it.
      Windows only, from an administrator terminal.
      --reader table (the default) reads the table off the drive, with
      file sizes. --reader records asks Windows for a summary of each
      record instead: no sizes, but quick when Windows has the drive's
      table in memory already.
  seekvo-engine image <file> [words...] [--path] [--limit N]
      Index an NTFS volume saved as an image file, then search it.
      Works anywhere.
  seekvo-engine scan <folder> [words...] [--path] [--limit N]
      Index a folder by walking it, then search it. Works anywhere.
  seekvo-engine bench [--files N]
      Build a made-up drive (one million files unless told otherwise)
      and time a set of searches on it.

With no words, drive, image and scan only print the index numbers.

Search words:
  All words must match, in any order and any letter case.
  ext:pdf keeps only files with that extension.
  --path also lists everything inside a folder whose name matches.";

/// Why the tool stopped.
enum Stop {
    /// The command line was wrong; the usage text helps.
    Usage(String),
    /// The command was fine but the work failed.
    Failed(String),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("drive") => run_drive(&args[1..]),
        Some("image") => run_image(&args[1..]),
        Some("scan") => run_scan(&args[1..]),
        Some("bench") => run_bench(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(Stop::Usage(format!("unknown command: {other}"))),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(Stop::Usage(message)) => {
            eprintln!("{message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
        Err(Stop::Failed(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// How `drive` gets a drive's file records.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reader {
    Table,
    Records,
}

/// What `drive`, `image` and `scan` take after the command name.
struct Target<'a> {
    /// The drive letter, the image file or the folder.
    place: &'a str,
    query: Query,
    limit: usize,
    reader: Reader,
}

fn parse_target<'a>(args: &'a [String], what: &str) -> Result<Target<'a>, Stop> {
    let mut place: Option<&str> = None;
    let mut words: Vec<&str> = Vec::new();
    let mut scope = Scope::Smart;
    let mut limit = 20usize;
    let mut reader = Reader::Table;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--path" => scope = Scope::Path,
            "--limit" => limit = number_after(&mut rest, "--limit")?,
            "--reader" => {
                reader = match rest.next().map(String::as_str) {
                    Some("table") => Reader::Table,
                    Some("records") => Reader::Records,
                    _ => return Err(Stop::Usage("--reader needs table or records".to_owned())),
                }
            }
            other if place.is_none() => place = Some(other),
            other => words.push(other),
        }
    }
    Ok(Target {
        place: place.ok_or_else(|| Stop::Usage(format!("this command needs {what}")))?,
        query: Query::parse(&words.join(" ")).with_scope(scope),
        limit,
        reader,
    })
}

fn run_drive(args: &[String]) -> Result<(), Stop> {
    let target = parse_target(args, "a drive letter")?;
    // Accept "C", "c:" and "C:\".
    let letter = match target
        .place
        .trim_end_matches(['\\', '/'])
        .trim_end_matches(':')
    {
        one if one.chars().count() == 1 => one.chars().next(),
        _ => None,
    }
    .ok_or_else(|| Stop::Usage(format!("{} is not a drive letter", target.place)))?;

    let letter = letter.to_ascii_uppercase();
    let started = Instant::now();
    let (records, damaged) = match target.reader {
        Reader::Table => {
            let table = ntfs::read_file_table(letter).map_err(|e| Stop::Failed(e.to_string()))?;
            (table.records, table.damaged)
        }
        Reader::Records => {
            let records = ntfs::read_records(letter).map_err(|e| Stop::Failed(e.to_string()))?;
            (records, 0)
        }
    };
    let read = started.elapsed();
    let (index, stats) = ntfs::build_index(&format!("{letter}:\\"), &records)
        .map_err(|e| Stop::Failed(e.to_string()))?;
    let total = started.elapsed();
    drop(records);
    println!(
        "Indexed {} files and {} folders on {}: in {} ({} reading the {}, {} arranging; {} of memory).",
        group(stats.files),
        group(stats.dirs),
        letter,
        millis(total),
        millis(read),
        match target.reader {
            Reader::Table => "file table",
            Reader::Records => "records from Windows",
        },
        millis(total - read),
        megabytes(index.memory_bytes()),
    );
    if damaged > 0 {
        println!(
            "Skipped {} records that were being written.",
            group(damaged)
        );
    }
    if stats.unreachable > 0 {
        println!(
            "Left out {} records with no reachable folder.",
            group(stats.unreachable)
        );
    }
    search_and_print(&index, &target);
    Ok(())
}

fn run_image(args: &[String]) -> Result<(), Stop> {
    let target = parse_target(args, "an image file")?;
    let mut file = std::fs::File::open(target.place)
        .map_err(|e| Stop::Failed(format!("cannot open {}: {e}", target.place)))?;

    let started = Instant::now();
    let table = mft::read_file_table(&mut file).map_err(|e| Stop::Failed(e.to_string()))?;
    let (index, stats) = ntfs::build_index(std::path::MAIN_SEPARATOR_STR, &table.records)
        .map_err(|e| Stop::Failed(e.to_string()))?;
    let took = started.elapsed();
    println!(
        "Indexed {} files and {} folders in {} ({} of memory).",
        group(stats.files),
        group(stats.dirs),
        millis(took),
        megabytes(index.memory_bytes()),
    );
    if table.damaged > 0 {
        println!("Skipped {} damaged records.", group(table.damaged));
    }
    search_and_print(&index, &target);
    Ok(())
}

fn run_scan(args: &[String]) -> Result<(), Stop> {
    let target = parse_target(args, "a folder")?;

    let started = Instant::now();
    let (index, stats) =
        scan::scan_dir(Path::new(target.place)).map_err(|e| Stop::Failed(e.to_string()))?;
    let took = started.elapsed();
    println!(
        "Indexed {} files and {} folders in {} ({} of memory).",
        group(stats.files),
        group(stats.dirs),
        millis(took),
        megabytes(index.memory_bytes()),
    );
    if stats.skipped > 0 {
        println!("Skipped {} that could not be read.", group(stats.skipped));
    }
    search_and_print(&index, &target);
    Ok(())
}

fn search_and_print(index: &Index, target: &Target) {
    if target.query.is_empty() {
        return;
    }
    let started = Instant::now();
    let result = index.search(&target.query, target.limit);
    let took = started.elapsed();
    println!("{} matches in {}.", group(result.total), millis(took));
    for &id in &result.hits {
        let Some(path) = index.path(id) else {
            continue;
        };
        match (index.kind(id), index.size(id)) {
            (Some(Kind::File), Some(size)) => println!("  {path}  ({})", file_size(size)),
            _ => println!("  {path}"),
        }
    }
    if result.total > result.hits.len() {
        println!("  (showing the first {})", result.hits.len());
    }
}

fn run_bench(args: &[String]) -> Result<(), Stop> {
    let mut files = 1_000_000usize;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--files" => files = number_after(&mut rest, "--files")?,
            other => return Err(Stop::Usage(format!("unknown option: {other}"))),
        }
    }

    let started = Instant::now();
    let index = synthetic::build(files).map_err(|e| Stop::Failed(e.to_string()))?;
    let built = started.elapsed();
    println!(
        "Made-up drive: {} entries, built in {}, {} of memory.",
        group(index.len()),
        millis(built),
        megabytes(index.memory_bytes()),
    );
    println!();
    println!(
        "{:<28} {:>10} {:>10} {:>10}",
        "search", "matches", "median", "slowest"
    );

    let cases: [(&str, Scope); 9] = [
        ("invoice", Scope::Smart),
        ("invoice march 2026", Scope::Smart),
        ("react index", Scope::Smart),
        ("node_modules react", Scope::Path),
        ("ext:jpg", Scope::Smart),
        ("img_10 hunza", Scope::Smart),
        ("statement ext:pdf", Scope::Smart),
        ("e", Scope::Smart),
        ("zzzz", Scope::Smart),
    ];
    for (text, scope) in cases {
        let query = Query::parse(text).with_scope(scope);
        let (total, median, slowest) = time_search(&index, &query);
        let label = match scope {
            Scope::Smart => text.to_owned(),
            Scope::Path => format!("{text} --path"),
        };
        println!(
            "{:<28} {:>10} {:>10} {:>10}",
            label,
            group(total),
            millis(median),
            millis(slowest)
        );
    }
    Ok(())
}

/// Run one search 30 times and report the match count, the median time and
/// the slowest time.
fn time_search(index: &Index, query: &Query) -> (usize, Duration, Duration) {
    const RUNS: usize = 30;
    let mut times = Vec::with_capacity(RUNS);
    let mut total = 0;
    for _ in 0..RUNS {
        let started = Instant::now();
        let result = index.search(black_box(query), 50);
        times.push(started.elapsed());
        total = black_box(result).total;
    }
    times.sort_unstable();
    (total, times[RUNS / 2], times[RUNS - 1])
}

fn number_after<'a>(
    rest: &mut impl Iterator<Item = &'a String>,
    option: &str,
) -> Result<usize, Stop> {
    let value = rest
        .next()
        .ok_or_else(|| Stop::Usage(format!("{option} needs a number")))?;
    value
        .replace(['_', ','], "")
        .parse()
        .map_err(|_| Stop::Usage(format!("{option} needs a number, got {value}")))
}

fn millis(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1000.0;
    if ms >= 100.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

fn megabytes(bytes: usize) -> String {
    format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// 1536 -> "1.5 KB"
fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 1234567 -> "1,234,567"
fn group(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
