use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::SystemTime;

use bstr::ByteSlice;
use grep_matcher::{Captures, Matcher};
use grep_regex::RegexMatcher;

pub fn ranges(sorted_list: &[u64], padding: u64) -> Vec<std::ops::RangeInclusive<u64>> {
    let mut ranges = Vec::new();
    let padding = std::num::Saturating(padding);

    for x in sorted_list {
        let x = std::num::Saturating(*x);

        let Some(range) = ranges.last_mut() else {
            let start = x - padding;
            let end = x + padding;
            ranges.push(start.0..=end.0);
            continue;
        };

        if range.contains(&(x - padding).0) {
            if *range.end() < (x + padding).0 {
                let end = x + padding;
                *range = *range.start()..=end.0;
            }
            continue;
        }

        let start = x - padding;
        let end = x + padding;
        ranges.push(start.0..=end.0);
    }

    ranges
}

pub fn replace_file<T>(
    path: impl AsRef<Path>,
    modified_at: Option<SystemTime>,
    f: impl FnOnce(&File, &File) -> (bool, T),
) -> Result<T, ReplaceFileError> {
    let path = path.as_ref();

    if !path.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file").into());
    }

    // TODO: this path may already exist, so choose a better path? (linkat below won't overwrite
    // existing files, so this won't cause us to lose data)
    let tmp_path = {
        let mut ext = path.extension().unwrap_or(OsStr::new("")).to_os_string();
        ext.push(OsStr::new(".asdf123.tmp"));
        path.with_extension(ext)
    };

    let tmp_c_path = CString::new(tmp_path.as_os_str().as_bytes()).unwrap();

    let original = File::open(path)?;

    // for paths like "foo", rust will return a parent of "" which is not useful for syscalls so we
    // replace it with "./"
    let mut parent_path = path.parent().unwrap();
    if parent_path == Path::new("") {
        parent_path = Path::new("./");
    }

    // create an unnamed file on the mount for the path
    let new = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_TMPFILE)
        .open(parent_path)?;

    // copy only the user/group/other read/write/execute permission bits
    let mask = libc::S_IRWXU | libc::S_IRWXG | libc::S_IRWXO;

    // set the permissions after creating the file so that it's not affected by the umask
    new.set_permissions(read_permissions(&original, mask)?)?;

    // the path to the new file in the /proc mount
    let mut procfd_c_path = Vec::new();
    procfd_c_path.extend(b"/proc/self/fd/");
    procfd_c_path.extend(new.as_raw_fd().to_string().as_bytes());
    let procfd_c_path = CString::new(procfd_c_path).unwrap();

    // TODO: use fallocate() to ensure we have approx enough space (the new file might be larger or
    // smaller than the original, but will typically be similar)?

    let (do_replace_file, rv) = f(&original, &new);

    // the user-provided closure asked us to stop
    if !do_replace_file {
        return Ok(rv);
    };

    if let Some(modified_at) = modified_at {
        // the current "modified" time for the file
        let latest_modified = std::fs::metadata(path)?.modified()?;

        // return an error if the file's "modified" timestamps differ
        if latest_modified != modified_at {
            return Err(ReplaceFileError::ModifiedTimeChanged);
        }
    }

    // give the new file a temporary name
    let linkat_rv = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            procfd_c_path.as_ptr(),
            libc::AT_FDCWD,
            tmp_c_path.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if linkat_rv != 0 {
        // may have failed if a file at `tmp_path` already exists
        return Err(std::io::Error::last_os_error().into());
    }

    // replace the original file at `path` with the new file
    std::fs::rename(&tmp_path, path)?;

    Ok(rv)
}

#[derive(Debug)]
pub enum ReplaceFileError {
    Io(std::io::Error),
    ModifiedTimeChanged,
}

impl From<std::io::Error> for ReplaceFileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for ReplaceFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::ModifiedTimeChanged => {
                write!(f, "the file's \"modified\" timestamp unexpectedly changed")
            }
        }
    }
}

impl std::error::Error for ReplaceFileError {}

/// Returns the file permissions without any file type bits. Also applies an additional bitmask to
/// the returned mode.
fn read_permissions(file: &File, mask: u32) -> std::io::Result<std::fs::Permissions> {
    // `std::fs::Metadata::permissions()` contains everything in the `st_mode` stat field, which
    // also contains the file type which we mask out
    let mode = file.metadata()?.permissions().mode() & !libc::S_IFMT;
    let mode = mode & mask;
    Ok(std::fs::Permissions::from_mode(mode))
}

pub fn editor_cmd() -> impl Iterator<Item = impl AsRef<OsStr>> + Clone {
    static EDITOR_CMD: OnceLock<Vec<OsString>> = OnceLock::new();

    // this is roughly what `sudo -e` does when parsing env variables
    fn split_whitespace(bytes: &[u8]) -> Vec<OsString> {
        bytes
            .fields()
            .map(|x| OsString::from_vec(x.to_vec()))
            .collect()
    }

    // returns `None` if `name` isn't set or if empty
    fn env_var(name: &str) -> Option<Vec<OsString>> {
        if let Some(cmd) = std::env::var_os(name) {
            let cmd = split_whitespace(cmd.as_bytes());
            if !cmd.is_empty() {
                return Some(cmd);
            }
        }
        None
    }

    let cmd = EDITOR_CMD.get_or_init(|| {
        if let Some(cmd) = env_var("VISUAL") {
            return cmd;
        }

        if let Some(cmd) = env_var("EDITOR") {
            return cmd;
        }

        if let Some(cmd) = env_var("GIT_EDITOR") {
            return cmd;
        }

        if let Ok(output) = Command::new("git")
            .arg("config")
            .arg("--null")
            .arg("core.editor")
            .output()
        {
            let mut output = output.stdout;
            // the last byte should be a nul
            assert_eq!(Some(0), output.pop());

            if !output.is_empty() {
                let cmd = split_whitespace(&output);
                if !cmd.is_empty() {
                    return cmd;
                }
            }
        }

        // if we can't find an editor, choose the best editor
        [OsString::from_vec(b"vim".to_vec())].to_vec()
    });

    assert!(!cmd.is_empty());

    cmd.iter()
}

pub fn replace_regex(
    matcher: &RegexMatcher,
    replacement: &[u8],
    haystack: &[u8],
    dest: &mut Vec<u8>,
) -> Result<(), <RegexMatcher as Matcher>::Error> {
    let mut captures = matcher.new_captures().unwrap();
    matcher.replace_with_captures(haystack, &mut captures, dest, |caps, dest| {
        caps.interpolate(
            |name| matcher.capture_index(name),
            haystack,
            replacement,
            dest,
        );
        true
    })
}

pub fn rewrite_patch_line_counts(bytes: &[u8]) -> std::borrow::Cow<[u8]> {
    let result = (|| {
        let mut lines = crate::parse::lines_with_pos(bytes);

        let (header, header_start) = lines.nth(2)?;

        let (range_1, range_2) = crate::parse::patch_block_header(header)?;

        let mut content_start = None;
        let mut line_counts = (0, 0);

        // count the number of + and - lines
        for (line, pos) in lines {
            if content_start.is_none() {
                content_start = Some(pos);
            }

            match line.first() {
                Some(b' ') | None => {
                    line_counts.0 += 1;
                    line_counts.1 += 1;
                }
                Some(b'-') => line_counts.0 += 1,
                Some(b'+') => line_counts.1 += 1,
                _ => return None,
            }
        }

        if (range_1.1, range_2.1) == line_counts {
            // no need to change the patch
            return None;
        }

        let content_start = content_start?;

        // build the new patch
        let mut new_patch = Vec::new();

        // add the header
        new_patch.extend_from_slice(&bytes[..header_start]);

        // write the new line numbers
        writeln!(
            &mut new_patch,
            "@@ -{},{} +{},{} @@",
            range_1.0, line_counts.0, range_2.0, line_counts.1,
        )
        .ok()?;

        // add the patch contents
        new_patch.extend_from_slice(&bytes[content_start..]);

        Some(new_patch)
    })();

    match result {
        Some(x) => std::borrow::Cow::Owned(x),
        None => std::borrow::Cow::Borrowed(bytes),
    }
}

pub fn rewrite_patch_line_start(bytes: &[u8], offset: i128, ansi: bool) -> Option<Vec<u8>> {
    let mut lines = crate::parse::lines_with_pos(bytes);
    let (mut header, header_start) = lines.nth(2)?;
    let (_, content_start) = lines.next()?;

    const ANSI_RESET: &[u8] = b"\x1b[0m";
    const ANSI_HEADER_COLOR: &[u8] = b"\x1b[36m";

    if ansi {
        header = header.strip_prefix(ANSI_RESET)?;
        header = header.strip_prefix(ANSI_HEADER_COLOR)?;
        header = header.strip_suffix(ANSI_RESET)?;
    }

    let (mut pair_1, mut pair_2) = crate::parse::patch_block_header(header)?;

    let (offset, positive_offset) = if offset >= 0 {
        (u64::try_from(offset).ok()?, true)
    } else {
        (u64::try_from(-offset).ok()?, false)
    };

    if positive_offset {
        pair_1.0 = pair_1.0.checked_add(offset)?;
        pair_2.0 = pair_2.0.checked_add(offset)?;
    } else {
        pair_1.0 = pair_1.0.checked_sub(offset)?;
        pair_2.0 = pair_2.0.checked_sub(offset)?;
    }

    // build the new patch
    let mut new_patch = Vec::new();

    // add the header
    new_patch.extend_from_slice(&bytes[..header_start]);

    if ansi {
        new_patch.extend_from_slice(ANSI_RESET);
        new_patch.extend_from_slice(ANSI_HEADER_COLOR);
    }

    // write the new line numbers
    write!(
        &mut new_patch,
        "@@ -{},{} +{},{} @@",
        pair_1.0, pair_1.1, pair_2.0, pair_2.1,
    )
    .ok()?;

    if ansi {
        new_patch.extend_from_slice(ANSI_RESET);
    }

    writeln!(&mut new_patch).unwrap();

    // add the patch contents
    new_patch.extend_from_slice(&bytes[content_start..]);

    Some(new_patch)
}

/// A label you can jump to using `continue`.
///
/// ```
/// let x: u32 = label!('start {
///     let input = todo!();
///     match input {
///         "retry" => continue 'start,
///         "one" => 1,
///         "two" => 2,
///         _ => input.parse().unwrap(),
///     }
/// });
/// ```
macro_rules! label {
    ($label:lifetime: $code:block) => {
        $label: loop {
            let _rv = {
                $code
            };
            #[allow(unreachable_code)]
            {
                break $label _rv;
            }
        }
    };
}
pub(crate) use label;

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};

    #[test]
    fn test_ranges() {
        let list = [1, 2, 10, 12, 35, 38, 55, u64::MAX];
        let padding = 5;
        assert_eq!(
            ranges(&list, padding),
            [0..=17, 30..=43, 50..=60, u64::MAX - 5..=u64::MAX],
        );

        let list = [1, 2, 10, 12, 35, 38, 55, u64::MAX];
        let padding = u64::MAX;
        assert_eq!(ranges(&list, padding), [0..=u64::MAX]);

        let list = [];
        let padding = 5;
        assert_eq!(ranges(&list, padding), []);

        let list = [1, 2, 5, 7, 100];
        let padding = 0;
        assert_eq!(
            ranges(&list, padding),
            [1..=1, 2..=2, 5..=5, 7..=7, 100..=100]
        );

        let list = [1, 2, 5, 7, 100];
        let padding = 1;
        assert_eq!(ranges(&list, padding), [0..=3, 4..=8, 99..=101]);
    }

    #[test]
    fn test_replace_file() {
        let mut file = tempfile::Builder::new().tempfile().unwrap();
        file.write_all(b"hello world\n").unwrap();

        replace_file(file.path(), None, |mut original, mut new| {
            new.write_all(b"foo ").unwrap();
            let mut buf = Vec::new();
            original.read_to_end(&mut buf).unwrap();
            new.write_all(&buf).unwrap();
            (true, ())
        })
        .unwrap();

        // `file` doesn't point to the new file located at `file.path()`, so it's confusing to leave
        // the file open
        let file = file.into_temp_path();

        // verify the nre file has the correct contents
        assert_eq!(std::fs::read(&file).unwrap(), b"foo hello world\n");

        /////////

        let mut file = tempfile::Builder::new().tempfile().unwrap();
        file.write_all(b"hello world\n").unwrap();

        replace_file(file.path(), None, |mut original, mut new| {
            new.write_all(b"foo ").unwrap();
            let mut buf = Vec::new();
            original.read_to_end(&mut buf).unwrap();
            new.write_all(&buf).unwrap();
            (false, ())
        })
        .unwrap();

        // verify the file has the same contents
        assert_eq!(std::fs::read(file.path()).unwrap(), b"hello world\n");

        /////////

        let mut file = tempfile::Builder::new().tempfile().unwrap();
        file.write_all(b"hello world\n").unwrap();

        // user readable and executable
        let target_permissions = std::fs::Permissions::from_mode(libc::S_IXUSR | libc::S_IRUSR);

        // set the permissions for the file
        file.as_file()
            .set_permissions(target_permissions.clone())
            .unwrap();
        assert_eq!(
            read_permissions(&file.as_file(), u32::MAX).unwrap(),
            target_permissions,
        );

        replace_file(file.path(), None, |mut original, mut new| {
            new.write_all(b"foo ").unwrap();
            let mut buf = Vec::new();
            original.read_to_end(&mut buf).unwrap();
            new.write_all(&buf).unwrap();
            (true, ())
        })
        .unwrap();

        // `file` doesn't point to the new file located at `file.path()`, so it's confusing to leave
        // the file open
        let file = file.into_temp_path();

        // verify the nre file has the correct contents
        assert_eq!(std::fs::read(&file).unwrap(), b"foo hello world\n");

        // verify the new file has the same permissions
        assert_eq!(
            read_permissions(&File::open(&file).unwrap(), u32::MAX).unwrap(),
            target_permissions,
        );
    }
}
