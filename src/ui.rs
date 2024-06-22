use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use bstr::ByteSlice;

use crate::util::label;

const FILENAME_STYLE: anstyle::Style = anstyle::Style::new().bold();
const STAGE_STYLE: anstyle::Style = anstyle::AnsiColor::Blue.on_default().bold();
const HELP_STYLE: anstyle::Style = anstyle::AnsiColor::Red.on_default().bold();
pub const ERROR_STYLE: anstyle::Style = anstyle::Style::new().bold();
pub const COUNT_STYLE: anstyle::Style = anstyle::Style::new().bold();

/// Start the editor with a file containing the given text. Once the user closes the editor, the
/// updated text will be returned. `None` will be returned if the editor exited with a non-zero
/// error code (for example `:cq` in vim).
fn user_edit(
    text: &[u8],
    editor_cmd: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut editor_cmd = editor_cmd.into_iter();

    // create a memfd file
    let edit_file = unsafe { libc::memfd_create(c"edit".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(edit_file >= 0);
    let mut edit_file = unsafe { File::from_raw_fd(edit_file) };

    let edit_fd = edit_file.as_raw_fd();

    // write the text to the file
    edit_file.write_all(text)?;

    let mut cmd = Command::new(editor_cmd.next().expect("editor_cmd was empty"));
    cmd.args(editor_cmd);
    cmd.arg(format!("/proc/self/fd/{edit_fd}"));

    // remove the CLOEXEC flag after the fork
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(edit_fd, libc::F_GETFD, 0);
            assert!(flags >= 0);
            let flags = flags & !libc::FD_CLOEXEC;
            let rv = libc::fcntl(edit_fd, libc::F_SETFD, flags);
            assert_eq!(rv, 0);
            Ok(())
        });
    }

    if !cmd.status()?.success() {
        return Ok(None);
    }

    // seek to the beginning of the file
    edit_file.rewind()?;

    // read the modified file
    let mut buf = Vec::new();
    edit_file.read_to_end(&mut buf)?;

    Ok(Some(buf))
}

fn menu_prompt(
    patch: &diffy::Patch<[u8]>,
    path: Option<&Path>,
    progress: (u64, u64),
    line_num: u64,
    input: Option<MenuOption>,
) -> MenuOption {
    // format the patch
    let mut patch_bytes = Vec::new();
    diffy::PatchFormatter::new()
        .with_color()
        .write_patch_into(patch, &mut patch_bytes)
        .unwrap();

    let patch_bytes =
        crate::util::rewrite_patch_line_start(&patch_bytes, line_num as i128, true).unwrap();

    let patch = String::from_utf8_lossy(&patch_bytes);
    let mut patch = patch.trim();

    if let Some(path) = path {
        // show the file path
        style_println!(
            &FILENAME_STYLE,
            "diff --{} {}",
            env!("CARGO_PKG_NAME"),
            path.display()
        );
    } else {
        // remove the first two lines ('---' and '+++')
        let start = patch.match_indices('\n').nth(1).unwrap().0 + 1;
        patch = &patch[start..];
    }
    println!("{patch}");

    if let Some(input) = input {
        return input;
    }

    let options = MenuOption::list()
        .iter()
        .map(|x| x.as_char())
        .chain(std::iter::once("?"))
        .collect::<Vec<&str>>()
        .join(",");

    let help = MenuOption::list()
        .iter()
        .map(|x| [x.as_char(), x.help()].join(" - "))
        .chain(std::iter::once("? - print help".to_string()))
        .collect::<Vec<String>>()
        .join("\n");

    loop {
        style_print!(
            &STAGE_STYLE,
            "({}/{}) Apply this patch [{options}]? ",
            progress.0 + 1,
            progress.1,
        );
        std::io::stdout().flush().unwrap();

        // get the command from the user
        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input).unwrap();

        match input.trim().parse() {
            Ok(x) => return x,
            Err(_) => {
                // could not parse the input, so print help text and patch then restart
                style_println!(&HELP_STYLE, "{help}");
                println!("{patch}");
            }
        }
    }
}

pub fn yes_no_prompt(prompt: &str) -> bool {
    loop {
        style_print!(&STAGE_STYLE, "{prompt} ");
        std::io::stdout().flush().unwrap();

        let mut input = String::new();
        std::io::stdin().lock().read_line(&mut input).unwrap();

        match input.trim().chars().next() {
            Some('y') => return true,
            Some('n') => return false,
            _ => {}
        }
    }
}

pub fn patch_prompt(
    original: &[u8],
    replaced: &[u8],
    mut src_path: Option<&Path>,
    progress: (u64, u64),
    line_num: u64,
    input: Option<MenuOption>,
) -> PatchOption {
    // use a large context length so that diffy does not do its own hunking
    let mut diff_options = diffy::DiffOptions::new();
    diff_options.set_context_len(usize::MAX);

    // the real patch
    let patch = diff_options.create_patch_bytes(original, replaced);

    const ESC_STYLE: anstyle::Style = anstyle::Style::new().invert();
    let esc_styled = style!("ESC", &ESC_STYLE).to_string();

    // a modified patch that is safe to print to the terminal
    let safe_current = original.replace("\u{001b}", &esc_styled);
    let safe_replaced = replaced.replace("\u{001b}", &esc_styled);
    let safe_patch = diff_options.create_patch_bytes(&safe_current, &safe_replaced);

    label!('patch_prompt: {
        // take the file path so that it's only ever shown once
        let src_path = src_path.take();

        // show the patch to the user and have them choose how to proceed
        match menu_prompt(&safe_patch, src_path, progress, line_num, input) {
            MenuOption::Yes => {
                // apply the patch
                let new_hunk = diffy::apply_bytes(original, &patch).unwrap();
                PatchOption::WriteNew(new_hunk)
            }
            MenuOption::No => PatchOption::WriteOriginal,
            MenuOption::Quit => PatchOption::Quit,
            MenuOption::Edit => label!('edit_prompt: {
                const INVALID_PATCH_PROMPT: &str =
                    "Your patch is invalid. Edit again (saying \"no\" discards!) [y/n]?";
                const DOES_NOT_APPLY_PROMPT: &str =
                    "Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]?";

                let edited = 'edit_hunk: {
                    let editor_cmd = crate::util::editor_cmd();

                    // allow the user to edit the patch
                    let Some(patch) = user_edit(&patch.to_bytes(), editor_cmd).unwrap() else {
                        // the editor didn't exit successfully
                        error!("The editor did not exit successfully.");
                        continue 'patch_prompt;
                    };

                    // if not valid utf-8, then it must not be empty
                    let is_empty = std::str::from_utf8(&patch)
                        .map(|x| x.trim().is_empty())
                        .unwrap_or(false);

                    // this also ignores whitespace since editors may add a newline at the end of
                    // the file
                    if is_empty {
                        // not even the patch header exists anymore
                        error!("The edited patch file was empty.");
                        continue 'patch_prompt;
                    }

                    let patch = crate::util::rewrite_patch_line_counts(&patch);

                    // create and apply the patch
                    let patch = match diffy::Patch::from_bytes(&patch) {
                        Ok(x) => x,
                        Err(e) => {
                            error!("{e}");
                            break 'edit_hunk Err(INVALID_PATCH_PROMPT);
                        }
                    };
                    let new_hunk = match diffy::apply_bytes(original, &patch) {
                        Ok(x) => x,
                        Err(e) => {
                            println!("{e}");
                            break 'edit_hunk Err(DOES_NOT_APPLY_PROMPT);
                        }
                    };

                    Ok(new_hunk)
                };

                match edited {
                    Ok(edited) => PatchOption::WriteNew(edited),
                    Err(msg) => {
                        if yes_no_prompt(msg) {
                            // answered "yes", so edit again
                            continue 'edit_prompt;
                        }
                        // answered "no", so discard and use original
                        PatchOption::WriteOriginal
                    }
                }
            }),
        }
    })
}

pub enum PatchOption {
    WriteNew(Vec<u8>),
    WriteOriginal,
    Quit,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MenuOption {
    Yes,
    No,
    Quit,
    Edit,
}

impl MenuOption {
    pub const fn list() -> &'static [Self] {
        &[Self::Yes, Self::No, Self::Quit, Self::Edit]
    }

    pub const fn as_char(&self) -> &'static str {
        // return a str instead of a char since they're much easier to work with (there is no char
        // -> str const function)
        match self {
            Self::Yes => "y",
            Self::No => "n",
            Self::Quit => "q",
            Self::Edit => "e",
        }
    }

    pub const fn help(&self) -> &'static str {
        match self {
            Self::Yes => "replace this hunk",
            Self::No => "do not replace this hunk",
            Self::Quit => "quit; do not replace this hunk or any future hunks",
            Self::Edit => "manually edit the current hunk",
        }
    }
}

impl std::str::FromStr for MenuOption {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const YES_STR: &str = MenuOption::Yes.as_char();
        const NO_STR: &str = MenuOption::No.as_char();
        const QUIT_STR: &str = MenuOption::Quit.as_char();
        const EDIT_STR: &str = MenuOption::Edit.as_char();

        Ok(match s {
            YES_STR => Self::Yes,
            NO_STR => Self::No,
            QUIT_STR => Self::Quit,
            EDIT_STR => Self::Edit,
            _ => return Err(()),
        })
    }
}

macro_rules! style {
    ($str:expr, $style:expr) => {{
        // for type checking
        let _style: &anstyle::Style = $style;
        format_args!("{}{}{}", $style, $str, anstyle::Reset)
    }};
}
pub(crate) use style;

macro_rules! style_print {
    () => {{
        print!()
    }};
    ($style:expr) => {{
        // for type checking
        let _style: &anstyle::Style = $style;
        print!()
    }};
    ($style:expr, $fmt:literal $($arg:tt)*) => {{
        let style: &anstyle::Style = $style;
        print!("{style}{}{style:#}", format_args!($fmt $($arg)*))
    }};
}
pub(crate) use style_print;

macro_rules! style_println {
    () => {{
        println!()
    }};
    ($style:expr) => {{
        // for type checking
        let _style: &anstyle::Style = $style;
        println!()
    }};
    ($style:expr, $fmt:literal $($arg:tt)*) => {{
        let style: &anstyle::Style = $style;
        println!("{style}{}{style:#}", format_args!($fmt $($arg)*))
    }};
}
pub(crate) use style_println;

macro_rules! error {
    () => {{
        error!("")
    }};
    ($fmt:literal $($arg:tt)*) => {{
        println!("{} {}", style!("ERROR:", &crate::ui::ERROR_STYLE), format_args!($fmt $($arg)*))
    }};
}
pub(crate) use error;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_patch_options() {
        for (option, as_str) in MenuOption::list().iter().map(|x| (*x, x.as_char())) {
            // test round-trip
            assert_eq!(as_str.parse(), Ok(option));
        }
    }
}
