//! Cut an NTFS image down to the parts the file-table reader touches.
//!
//! A whole image is megabytes of mostly empty space. The tests only need
//! the first sector and the file table, so this records which byte ranges
//! the reader asks for and writes just those to a small fixture file.
//!
//! Usage: cargo run --example make_fixture -- <image> <fixture>
//!
//! See tests/fixtures/README.md for how the image itself was made.

use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;

use seekvo_engine::mft::{self, BlockSource};

/// Passes reads through and remembers where they were.
struct Recorder {
    file: File,
    ranges: Vec<(u64, u64)>,
}

impl BlockSource for Recorder {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.ranges.push((offset, offset + buffer.len() as u64));
        self.file.read_at(offset, buffer)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [image, fixture] = args.as_slice() else {
        eprintln!("usage: make_fixture <image> <fixture>");
        return ExitCode::FAILURE;
    };
    match run(image, fixture) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(image: &str, fixture: &str) -> Result<(), String> {
    let file = File::open(image).map_err(|e| format!("cannot open {image}: {e}"))?;
    let mut recorder = Recorder {
        file,
        ranges: Vec::new(),
    };
    let table = mft::read_file_table(&mut recorder).map_err(|e| e.to_string())?;

    // Join ranges that touch or overlap.
    let mut ranges = recorder.ranges;
    ranges.sort_unstable();
    let mut joined: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        match joined.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => joined.push((start, end)),
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"SEEKVO-IMAGE-1\n");
    for &(start, end) in &joined {
        let mut bytes = vec![0u8; (end - start) as usize];
        recorder
            .file
            .read_at(start, &mut bytes)
            .map_err(|e| format!("cannot read {image}: {e}"))?;
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    File::create(fixture)
        .and_then(|mut file| file.write_all(&out))
        .map_err(|e| format!("cannot write {fixture}: {e}"))?;

    println!(
        "{} records in use, {} pieces, {} bytes written to {fixture}",
        table.in_use,
        joined.len(),
        out.len()
    );
    Ok(())
}
