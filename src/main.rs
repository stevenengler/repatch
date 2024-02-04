#![deny(unsafe_op_in_unsafe_fn)]

mod cli;
mod parse;
mod ui;
mod util;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Context as anyhowContext;
use bstr::ByteSlice;
use clap::Parser;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::Bytes;
use grep_searcher::Searcher;
use ignore::WalkBuilder;

use crate::cli::{Args, Context};
use crate::ui::{error, style, MenuOption, PatchOption, COUNT_STYLE};
use crate::util::ReplaceFileError;

fn main() -> ExitCode {
    if let Err(e) = run(Args::parse()) {
        error!("{e:#}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run(args: Args) -> anyhow::Result<()> {
    let mut matcher = RegexMatcherBuilder::new();
    matcher.case_insensitive(args.ignore_case);
    let matcher = matcher.build(&args.find)?;

    let mut matches = match find_matches(&matcher, &args.paths, args.ignore_errors) {
        Ok(x) => x,
        Err(num_errors) => anyhow::bail!(
            "found {} error{}",
            style!(num_errors, &COUNT_STYLE),
            if num_errors == 1 { "" } else { "s" },
        ),
    };

    let match_count = matches.values().map(|i| i.lines.len()).sum::<usize>();
    println!(
        "Found {} match{} in {} file{}.",
        style!(match_count, &COUNT_STYLE),
        if match_count == 1 { "" } else { "es" },
        style!(matches.len(), &COUNT_STYLE),
        if matches.len() == 1 { "" } else { "s" },
    );

    // common options we'll use during the find & replace process across all files
    let config = ReplaceOptions {
        matcher: &matcher,
        replace_with: args.replace.as_bytes(),
        padding: match args.context {
            Context::Num(x) => x,
            Context::Infinite => u64::MAX,
        },
    };

    // loop over each file that has matches
    for (path, match_info) in matches.iter_mut() {
        // separate files by a newline
        println!();

        // If '--show' is set, the program should effectively do a dry run where it shows the
        // changes without making any modifications. While we could write a simpler function, we
        // instead use the same `replace_file` function to ensure that the behaviour is the same as
        // what would normally happen.

        if args.show {
            // we want to only show the patches, but not actually change anything
            let src = std::fs::File::open(path).unwrap();

            // perform the find & replace, but with no output file
            let (cont, write_file) = replace_matches(
                &config,
                path,
                &src,
                None,
                &mut match_info.lines,
                Some(MenuOption::No),
            );

            // we provided `MenuOption::No`, so we shouldn't expect it to want to write
            assert_eq!(cont, Continue::Yes);
            assert_eq!(write_file, WriteFile::No);
        } else {
            // replace the file with a new file that we'll write to
            let cont =
                crate::util::replace_file(path, Some(match_info.modified), |original, new| {
                    // perform the find & replace
                    let (cont, write_file) = replace_matches(
                        &config,
                        path,
                        original,
                        Some(new),
                        &mut match_info.lines,
                        args.apply.then_some(MenuOption::Yes),
                    );

                    // inform `replace_file` whether it should replace the file or not
                    (write_file == WriteFile::Yes, cont)
                });

            // handle errors
            let cont = match cont {
                Ok(x) => x,
                Err(ReplaceFileError::Io(e)) => {
                    return Err(e)
                        .with_context(|| format!("could not replace file '{}'", path.display()))
                }
                Err(ReplaceFileError::ModifiedTimeChanged) => {
                    return Err(anyhow::anyhow!(
                        "the file '{}' was modified by another program\n\
                        Discarding all patches to this file and exiting.",
                        path.display(),
                    ))
                }
            };

            if cont == Continue::No {
                break;
            }
        }
    }

    Ok(())
}

/// Find matches. Any errors will be printed to stdout. If there is an error:
/// - If `continue_on_err` is true, the error will be printed.
/// - If `continue_on_err` is false, the error will be printed and it will continue to walk the
///   filesystem looking for more errors, but it will stop searching files.
fn find_matches(
    matcher: &RegexMatcher,
    paths: &[impl AsRef<Path>],
    continue_on_err: bool,
) -> Result<BTreeMap<PathBuf, MatchInfo>, u64> {
    let mut matches = BTreeMap::new();
    let mut num_errors = 0;

    if paths.is_empty() {
        return Ok(matches);
    }

    let mut searcher = Searcher::new();

    let mut walk = WalkBuilder::new(paths.first().unwrap());
    for path in &paths[1..] {
        walk.add(path);
    }
    let walk = walk.build();

    for result in walk {
        match result {
            Ok(entry) => {
                let path = entry.path();
                let meta = match std::fs::metadata(path) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("{}: {e}", path.display());
                        num_errors += 1;
                        continue;
                    }
                };
                let modified_time = meta.modified().unwrap();

                // this is only a very basic check; we may have already visited this file through
                // some other path (relative or absolute path, another hard link to the same file,
                // etc) and we don't defend against these here
                if matches.contains_key(path) {
                    // already visited this path and it had a match
                    continue;
                }

                if meta.is_dir() {
                    continue;
                }

                if num_errors == 0 || continue_on_err {
                    let sink = Bytes(|line_num, _line| {
                        // TODO: even though we found a match, we might want to replace it with the
                        // same value (ex: "foo" -> "foo"), so we should also do a replace here and
                        // see if we really should record this
                        let MatchInfo { lines, .. } = matches
                            .entry(path.to_path_buf())
                            .or_insert(MatchInfo::new(modified_time));

                        // line numbers are given starting from 1
                        lines.push(line_num.checked_sub(1).unwrap());

                        Ok(true)
                    });

                    if let Err(e) = searcher.search_path(matcher, path, sink) {
                        // could not read the file
                        error!("{}: {e}", path.display());
                        num_errors += 1;
                    }
                } else {
                    // if we've already had an error, we still check if we can open the remaining
                    // files
                    if let Err(e) = File::open(path) {
                        // could not read the file
                        error!("{}: {e}", path.display());
                        num_errors += 1;
                    }
                }
            }
            Err(e) => {
                error!("{e}");
                num_errors += 1;
            }
        }
    }

    if num_errors == 0 || continue_on_err {
        Ok(matches)
    } else {
        Err(num_errors)
    }
}

struct MatchInfo {
    modified: SystemTime,
    lines: Vec<u64>,
}

impl MatchInfo {
    pub fn new(modified: SystemTime) -> Self {
        Self {
            modified,
            lines: Vec::new(),
        }
    }
}

fn replace_matches(
    options: &ReplaceOptions,
    path: &Path,
    src: &File,
    empty_dest: Option<&File>,
    line_nums: &mut [u64],
    input: Option<MenuOption>,
) -> (Continue, WriteFile) {
    let mut src = BufReader::new(src);
    let mut dest = empty_dest.map(BufWriter::new);

    // group adjacent lines into ranges
    line_nums.sort();
    let hunk_ranges = crate::util::ranges(line_nums, options.padding);
    let hunk_count: u64 = hunk_ranges.len().try_into().unwrap();

    // current line of `src`
    let mut current_line = 0;

    // did we make any of our own changes to `dest`?
    let mut made_change = false;

    // do we want the program to continue after we return?
    let mut cont = Continue::Yes;

    // a reusable buffer
    let mut buf = Vec::new();

    for (hunk_idx, hunk_range) in hunk_ranges.into_iter().enumerate() {
        let hunk_idx: u64 = hunk_idx.try_into().unwrap();
        let path = (hunk_idx == 0).then_some(path);

        // copy file lines to dest file until we get to the first line of the hunk
        while !hunk_range.contains(&current_line) {
            buf.clear();
            src.read_until(b'\n', &mut buf).unwrap();
            if buf.is_empty() {
                // EOF
                break;
            }
            if let Some(ref mut dest) = dest {
                dest.write_all(&buf).unwrap();
            }
            current_line += 1;
        }

        let mut current_hunk = Vec::new();
        let hunk_start_line = current_line;

        // copy file lines to buffer until we read all lines of the hunk
        while hunk_range.contains(&current_line) {
            let initial_len = current_hunk.len();
            src.read_until(b'\n', &mut current_hunk).unwrap();
            if current_hunk.len() == initial_len {
                // EOF
                break;
            }
            current_line += 1;
        }

        // find & replace within this hunk
        let mut replaced_hunk = Vec::new();
        crate::util::replace_regex(
            options.matcher,
            options.replace_with,
            &current_hunk,
            &mut replaced_hunk,
        )
        .unwrap();

        // check if anything changed
        if current_hunk == replaced_hunk {
            // nothing changed, so write the original hunk without applying any patch
            if let Some(ref mut dest) = dest {
                dest.write_all(&current_hunk).unwrap();
            }
            continue;
        }

        // ask the user what to do
        match crate::ui::patch_prompt(
            &current_hunk,
            &replaced_hunk,
            path,
            (hunk_idx, hunk_count),
            hunk_start_line,
            input,
        ) {
            PatchOption::WriteNew(x) => {
                // this theoretically shouldn't be needed and it might panic on false positives, but
                // it's unlikely that a patch would remove all lines of the hunk
                if x.trim().is_empty() {
                    // TODO: remove this when we're more confident in the patches
                    let msg = "This patch removes all lines of the hunk. Are you sure that you want to continue [y/n]?";
                    if !crate::ui::yes_no_prompt(msg) {
                        // write the hunk without applying the patch
                        if let Some(ref mut dest) = dest {
                            dest.write_all(&current_hunk).unwrap();
                        }

                        cont = Continue::No;
                        break;
                    }
                }
                // write the new hunk
                if let Some(ref mut dest) = dest {
                    dest.write_all(&x).unwrap();
                    made_change = true;
                }
            }
            PatchOption::WriteOriginal => {
                // write the hunk without applying the patch
                if let Some(ref mut dest) = dest {
                    dest.write_all(&current_hunk).unwrap();
                }
            }
            PatchOption::Quit => {
                // write the hunk without applying the patch
                if let Some(ref mut dest) = dest {
                    dest.write_all(&current_hunk).unwrap();
                }

                cont = Continue::No;
                break;
            }
        }
    }

    if !made_change {
        return (cont, WriteFile::No);
    }

    // if we made changes, there must have been a destination file
    let Some(mut dest) = dest else {
        panic!("Changes were apparently written, but we have no dest file");
    };

    // TODO: we could possibly make this copy faster on specific Linux filesystems using
    // `FICLONERANGE`

    // write out any internally buffered data in `src`
    std::io::copy(&mut src.buffer(), &mut dest).unwrap();

    // convert back to `File` to hopefully take advantage of `copy_file_range` during
    // `std::io::copy`
    let mut src: &File = src.into_inner();
    let mut dest: &File = dest.into_inner().unwrap();

    // write remainder of file
    std::io::copy(&mut src, &mut dest).unwrap();

    (cont, WriteFile::Yes)
}

pub struct ReplaceOptions<'a> {
    matcher: &'a RegexMatcher,
    replace_with: &'a [u8],
    padding: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WriteFile {
    Yes,
    No,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Continue {
    Yes,
    No,
}
