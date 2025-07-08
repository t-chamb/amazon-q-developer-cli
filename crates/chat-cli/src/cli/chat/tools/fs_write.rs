use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use crossterm::queue;
use crossterm::style::{
    self,
    Color,
};
use eyre::{
    ContextCompat as _,
    Result,
    bail,
    eyre,
};
use globset::{
    Glob,
    GlobSetBuilder,
};
use serde::Deserialize;
use similar::DiffableStr;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::{
    LinesWithEndings,
    as_24_bit_terminal_escaped,
};
use tracing::{
    error,
    warn,
};

use super::{
    InvokeOutput,
    format_path,
    sanitize_path_tool_arg,
    supports_truecolor,
};
use crate::cli::agent::{
    Agent,
    PermissionEvalResult,
};
use crate::os::Os;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "command")]
pub enum FsWrite {
    /// The tool spec should only require `file_text`, but the model sometimes doesn't want to
    /// provide it. Thus, including `new_str` as a fallback check, if it's available.
    #[serde(rename = "create")]
    Create {
        path: String,
        file_text: Option<String>,
        new_str: Option<String>,
        summary: Option<String>,
    },
    #[serde(rename = "str_replace")]
    StrReplace {
        path: String,
        old_str: String,
        new_str: String,
        summary: Option<String>,
    },
    #[serde(rename = "insert")]
    Insert {
        path: String,
        insert_line: usize,
        new_str: String,
        summary: Option<String>,
    },
    #[serde(rename = "append")]
    Append {
        path: String,
        new_str: String,
        summary: Option<String>,
    },
}

impl FsWrite {
    pub async fn invoke(&self, os: &Os, output: &mut impl Write) -> Result<InvokeOutput> {
        let cwd = os.env.current_dir()?;
        match self {
            FsWrite::Create { path, .. } => {
                let file_text = self.canonical_create_command_text();
                let path = sanitize_path_tool_arg(os, path);
                if let Some(parent) = path.parent() {
                    os.fs.create_dir_all(parent).await?;
                }

                let invoke_description = if os.fs.exists(&path) {
                    "Replacing: "
                } else {
                    "Creating: "
                };
                queue!(
                    output,
                    style::Print(invoke_description),
                    style::SetForegroundColor(Color::Green),
                    style::Print(format_path(cwd, &path)),
                    style::ResetColor,
                    style::Print("\n"),
                )?;

                write_to_file(os, path, file_text).await?;
                Ok(Default::default())
            },
            FsWrite::StrReplace {
                path, old_str, new_str, ..
            } => {
                let path = sanitize_path_tool_arg(os, path);
                let file = os.fs.read_to_string(&path).await?;
                let matches = file.match_indices(old_str).collect::<Vec<_>>();
                queue!(
                    output,
                    style::Print("Updating: "),
                    style::SetForegroundColor(Color::Green),
                    style::Print(format_path(cwd, &path)),
                    style::ResetColor,
                    style::Print("\n"),
                )?;
                match matches.len() {
                    0 => Err(eyre!("no occurrences of \"{old_str}\" were found")),
                    1 => {
                        let file = file.replacen(old_str, new_str, 1);
                        os.fs.write(path, file).await?;
                        Ok(Default::default())
                    },
                    x => Err(eyre!("{x} occurrences of old_str were found when only 1 is expected")),
                }
            },
            FsWrite::Insert {
                path,
                insert_line,
                new_str,
                ..
            } => {
                let path = sanitize_path_tool_arg(os, path);
                let mut file = os.fs.read_to_string(&path).await?;
                queue!(
                    output,
                    style::Print("Updating: "),
                    style::SetForegroundColor(Color::Green),
                    style::Print(format_path(cwd, &path)),
                    style::ResetColor,
                    style::Print("\n"),
                )?;

                // Get the index of the start of the line to insert at.
                let num_lines = file.lines().enumerate().map(|(i, _)| i + 1).last().unwrap_or(1);
                let insert_line = insert_line.clamp(&0, &num_lines);
                let mut i = 0;
                for _ in 0..*insert_line {
                    let line_len = &file[i..].find("\n").map_or(file[i..].len(), |i| i + 1);
                    i += line_len;
                }
                file.insert_str(i, new_str);
                write_to_file(os, &path, file).await?;
                Ok(Default::default())
            },
            FsWrite::Append { path, new_str, .. } => {
                let path = sanitize_path_tool_arg(os, path);

                queue!(
                    output,
                    style::Print("Appending to: "),
                    style::SetForegroundColor(Color::Green),
                    style::Print(format_path(cwd, &path)),
                    style::ResetColor,
                    style::Print("\n"),
                )?;

                let mut file = os.fs.read_to_string(&path).await?;
                if !file.ends_with_newline() {
                    file.push('\n');
                }
                file.push_str(new_str);
                write_to_file(os, path, file).await?;
                Ok(Default::default())
            },
        }
    }

    pub fn queue_description(&self, os: &Os, output: &mut impl Write) -> Result<()> {
        let cwd = os.env.current_dir()?;
        self.print_relative_path(os, output)?;
        match self {
            FsWrite::Create { path, .. } => {
                let file_text = self.canonical_create_command_text();
                let path = sanitize_path_tool_arg(os, path);
                let relative_path = format_path(cwd, &path);
                let prev = if os.fs.exists(&path) {
                    let file = os.fs.read_to_string_sync(&path)?;
                    stylize_output_if_able(os, &path, &file)
                } else {
                    Default::default()
                };
                let new = stylize_output_if_able(os, &relative_path, &file_text);
                print_diff(output, &prev, &new, 1)?;

                // Display summary as purpose if available after the diff
                super::display_purpose(self.get_summary(), output)?;

                Ok(())
            },
            FsWrite::Insert {
                path,
                insert_line,
                new_str,
                ..
            } => {
                let path = sanitize_path_tool_arg(os, path);
                let relative_path = format_path(cwd, &path);
                let file = os.fs.read_to_string_sync(&path)?;

                // Diff the old with the new by adding extra context around the line being inserted
                // at.
                let (prefix, start_line, suffix, _) = get_lines_with_context(&file, *insert_line, *insert_line, 3);
                let insert_line_content = LinesWithEndings::from(&file)
                    // don't include any content if insert_line is 0
                    .nth(insert_line.checked_sub(1).unwrap_or(usize::MAX))
                    .unwrap_or_default();
                let old = [prefix, insert_line_content, suffix].join("");
                let new = [prefix, insert_line_content, new_str, suffix].join("");

                let old = stylize_output_if_able(os, &relative_path, &old);
                let new = stylize_output_if_able(os, &relative_path, &new);
                print_diff(output, &old, &new, start_line)?;

                // Display summary as purpose if available after the diff
                super::display_purpose(self.get_summary(), output)?;

                Ok(())
            },
            FsWrite::StrReplace {
                path, old_str, new_str, ..
            } => {
                let path = sanitize_path_tool_arg(os, path);
                let relative_path = format_path(cwd, &path);
                let file = os.fs.read_to_string_sync(&path)?;
                let (start_line, _) = match line_number_at(&file, old_str) {
                    Some((start_line, end_line)) => (start_line, end_line),
                    _ => (0, 0),
                };
                let old_str = stylize_output_if_able(os, &relative_path, old_str);
                let new_str = stylize_output_if_able(os, &relative_path, new_str);
                print_diff(output, &old_str, &new_str, start_line)?;

                // Display summary as purpose if available after the diff
                super::display_purpose(self.get_summary(), output)?;

                Ok(())
            },
            FsWrite::Append { path, new_str, .. } => {
                let path = sanitize_path_tool_arg(os, path);
                let relative_path = format_path(cwd, &path);
                let start_line = os.fs.read_to_string_sync(&path)?.lines().count() + 1;
                let file = stylize_output_if_able(os, &relative_path, new_str);
                print_diff(output, &Default::default(), &file, start_line)?;

                // Display summary as purpose if available after the diff
                super::display_purpose(self.get_summary(), output)?;

                Ok(())
            },
        }
    }

    pub async fn validate(&mut self, os: &Os) -> Result<()> {
        match self {
            FsWrite::Create { path, .. } => {
                if path.is_empty() {
                    bail!("Path must not be empty")
                };
            },
            FsWrite::StrReplace { path, .. } | FsWrite::Insert { path, .. } => {
                let path = sanitize_path_tool_arg(os, path);
                if !path.exists() {
                    bail!("The provided path must exist in order to replace or insert contents into it")
                }
            },
            FsWrite::Append { path, new_str, .. } => {
                if path.is_empty() {
                    bail!("Path must not be empty")
                };
                if new_str.is_empty() {
                    bail!("Content to append must not be empty")
                };
            },
        }

        Ok(())
    }

    fn print_relative_path(&self, os: &Os, output: &mut impl Write) -> Result<()> {
        let cwd = os.env.current_dir()?;
        let path = match self {
            FsWrite::Create { path, .. } => path,
            FsWrite::StrReplace { path, .. } => path,
            FsWrite::Insert { path, .. } => path,
            FsWrite::Append { path, .. } => path,
        };
        // Sanitize the path to handle tilde expansion
        let path = sanitize_path_tool_arg(os, path);
        let relative_path = format_path(cwd, &path);
        queue!(
            output,
            style::Print("Path: "),
            style::SetForegroundColor(Color::Green),
            style::Print(&relative_path),
            style::ResetColor,
            style::Print("\n\n"),
        )?;
        Ok(())
    }

    /// Returns the text to use for the [FsWrite::Create] command. This is required since we can't
    /// rely on the model always providing `file_text`.
    fn canonical_create_command_text(&self) -> String {
        match self {
            FsWrite::Create { file_text, new_str, .. } => match (file_text, new_str) {
                (Some(file_text), _) => file_text.clone(),
                (None, Some(new_str)) => {
                    warn!("required field `file_text` is missing, using the provided `new_str` instead");
                    new_str.clone()
                },
                _ => {
                    warn!("no content provided for the create command");
                    String::new()
                },
            },
            _ => String::new(),
        }
    }

    /// Returns the summary from any variant of the FsWrite enum
    pub fn get_summary(&self) -> Option<&String> {
        match self {
            FsWrite::Create { summary, .. } => summary.as_ref(),
            FsWrite::StrReplace { summary, .. } => summary.as_ref(),
            FsWrite::Insert { summary, .. } => summary.as_ref(),
            FsWrite::Append { summary, .. } => summary.as_ref(),
        }
    }

    pub fn eval_perm(&self, agent: &Agent) -> PermissionEvalResult {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Settings {
            #[serde(default)]
            allowed_paths: Vec<String>,
            #[serde(default)]
            denied_paths: Vec<String>,
        }

        let is_in_allowlist = agent.allowed_tools.contains("fs_write");
        match agent.tools_settings.get("fs_write") {
            Some(settings) if is_in_allowlist => {
                let Settings {
                    allowed_paths,
                    denied_paths,
                } = match serde_json::from_value::<Settings>(settings.clone()) {
                    Ok(settings) => settings,
                    Err(e) => {
                        error!("Failed to deserialize tool settings for fs_write: {:?}", e);
                        return PermissionEvalResult::Ask;
                    },
                };
                let allow_set = {
                    let mut builder = GlobSetBuilder::new();
                    for path in &allowed_paths {
                        if let Ok(glob) = Glob::new(path) {
                            builder.add(glob);
                        } else {
                            warn!("Failed to create glob from path given: {path}. Ignoring.");
                        }
                    }
                    builder.build()
                };

                let deny_set = {
                    let mut builder = GlobSetBuilder::new();
                    for path in &denied_paths {
                        if let Ok(glob) = Glob::new(path) {
                            builder.add(glob);
                        } else {
                            warn!("Failed to create glob from path given: {path}. Ignoring.");
                        }
                    }
                    builder.build()
                };

                match (allow_set, deny_set) {
                    (Ok(allow_set), Ok(deny_set)) => {
                        match self {
                            Self::Create { path, .. }
                            | Self::Insert { path, .. }
                            | Self::Append { path, .. }
                            | Self::StrReplace { path, .. } => {
                                if deny_set.is_match(path) {
                                    return PermissionEvalResult::Deny;
                                }
                                if allow_set.is_match(path) {
                                    return PermissionEvalResult::Allow;
                                }
                            },
                        }
                        PermissionEvalResult::Ask
                    },
                    (allow_res, deny_res) => {
                        if let Err(e) = allow_res {
                            warn!("fs_write failed to build allow set: {:?}", e);
                        }
                        if let Err(e) = deny_res {
                            warn!("fs_write failed to build deny set: {:?}", e);
                        }
                        warn!("One or more detailed args failed to parse, falling back to ask");
                        PermissionEvalResult::Ask
                    },
                }
            },
            None if is_in_allowlist => PermissionEvalResult::Allow,
            _ => PermissionEvalResult::Ask,
        }
    }
}

/// Writes `content` to `path`, adding a newline if necessary.
async fn write_to_file(os: &Os, path: impl AsRef<Path>, mut content: String) -> Result<()> {
    let path_ref = path.as_ref();

    // Log the path being written to
    tracing::debug!("Writing to file: {:?}", path_ref);

    if !content.ends_with_newline() {
        content.push('\n');
    }
    os.fs.write(path.as_ref(), content).await?;
    Ok(())
}

/// Returns a prefix/suffix pair before and after the content dictated by `[start_line, end_line]`
/// within `content`. The updated start and end lines containing the original context along with
/// the suffix and prefix are returned.
///
/// Params:
/// - `start_line` - 1-indexed starting line of the content.
/// - `end_line` - 1-indexed ending line of the content.
/// - `context_lines` - number of lines to include before the start and end.
///
/// Returns `(prefix, new_start_line, suffix, new_end_line)`
fn get_lines_with_context(
    content: &str,
    start_line: usize,
    end_line: usize,
    context_lines: usize,
) -> (&str, usize, &str, usize) {
    let line_count = content.lines().count();
    // We want to support end_line being 0, in which case we should be able to set the first line
    // as the suffix.
    let zero_check_inc = if end_line == 0 { 0 } else { 1 };

    // Convert to 0-indexing.
    let (start_line, end_line) = (
        start_line.saturating_sub(1).clamp(0, line_count - 1),
        end_line.saturating_sub(1).clamp(0, line_count - 1),
    );
    let new_start_line = 0.max(start_line.saturating_sub(context_lines));
    let new_end_line = (line_count - 1).min(end_line + context_lines);

    // Build prefix
    let mut prefix_start = 0;
    for line in LinesWithEndings::from(content).take(new_start_line) {
        prefix_start += line.len();
    }
    let mut prefix_end = prefix_start;
    for line in LinesWithEndings::from(&content[prefix_start..]).take(start_line - new_start_line) {
        prefix_end += line.len();
    }

    // Build suffix
    let mut suffix_start = 0;
    for line in LinesWithEndings::from(content).take(end_line + zero_check_inc) {
        suffix_start += line.len();
    }
    let mut suffix_end = suffix_start;
    for line in LinesWithEndings::from(&content[suffix_start..]).take(new_end_line - end_line) {
        suffix_end += line.len();
    }

    (
        &content[prefix_start..prefix_end],
        new_start_line + 1,
        &content[suffix_start..suffix_end],
        new_end_line + zero_check_inc,
    )
}

/// Prints a git-diff style comparison between `old_str` and `new_str`.
/// - `start_line` - 1-indexed line number that `old_str` and `new_str` start at.
fn print_diff(
    output: &mut impl Write,
    old_str: &StylizedFile,
    new_str: &StylizedFile,
    start_line: usize,
) -> Result<()> {
    let diff = similar::TextDiff::from_lines(&old_str.content, &new_str.content);

    // First, get the gutter width required for both the old and new lines.
    let (mut max_old_i, mut max_new_i) = (1, 1);
    for change in diff.iter_all_changes() {
        if let Some(i) = change.old_index() {
            max_old_i = i + start_line;
        }
        if let Some(i) = change.new_index() {
            max_new_i = i + start_line;
        }
    }
    let old_line_num_width = terminal_width_required_for_line_count(max_old_i);
    let new_line_num_width = terminal_width_required_for_line_count(max_new_i);

    // Now, print
    fn fmt_index(i: Option<usize>, start_line: usize) -> String {
        match i {
            Some(i) => (i + start_line).to_string(),
            _ => " ".to_string(),
        }
    }
    for change in diff.iter_all_changes() {
        // Define the colors per line.
        let (text_color, gutter_bg_color, line_bg_color) = match (change.tag(), new_str.truecolor) {
            (similar::ChangeTag::Equal, true) => (style::Color::Reset, new_str.gutter_bg, new_str.line_bg),
            (similar::ChangeTag::Delete, true) => (
                style::Color::Reset,
                style::Color::Rgb { r: 79, g: 40, b: 40 },
                style::Color::Rgb { r: 36, g: 25, b: 28 },
            ),
            (similar::ChangeTag::Insert, true) => (
                style::Color::Reset,
                style::Color::Rgb { r: 40, g: 67, b: 43 },
                style::Color::Rgb { r: 24, g: 38, b: 30 },
            ),
            (similar::ChangeTag::Equal, false) => (style::Color::Reset, new_str.gutter_bg, new_str.line_bg),
            (similar::ChangeTag::Delete, false) => (style::Color::Red, new_str.gutter_bg, new_str.line_bg),
            (similar::ChangeTag::Insert, false) => (style::Color::Green, new_str.gutter_bg, new_str.line_bg),
        };
        // Define the change tag character to print, if any.
        let sign = match change.tag() {
            similar::ChangeTag::Equal => " ",
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
        };

        let old_i_str = fmt_index(change.old_index(), start_line);
        let new_i_str = fmt_index(change.new_index(), start_line);

        // Print the gutter and line numbers.
        queue!(output, style::SetBackgroundColor(gutter_bg_color))?;
        queue!(
            output,
            style::SetForegroundColor(text_color),
            style::Print(sign),
            style::Print(" ")
        )?;
        queue!(
            output,
            style::Print(format!(
                "{:>old_line_num_width$}",
                old_i_str,
                old_line_num_width = old_line_num_width
            ))
        )?;
        if sign == " " {
            queue!(output, style::Print(", "))?;
        } else {
            queue!(output, style::Print("  "))?;
        }
        queue!(
            output,
            style::Print(format!(
                "{:>new_line_num_width$}",
                new_i_str,
                new_line_num_width = new_line_num_width
            ))
        )?;
        // Print the line.
        queue!(
            output,
            style::SetForegroundColor(style::Color::Reset),
            style::Print(":"),
            style::SetForegroundColor(text_color),
            style::SetBackgroundColor(line_bg_color),
            style::Print(" "),
            style::Print(change),
            style::ResetColor,
        )?;
    }
    queue!(
        output,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::UntilNewLine),
        style::Print("\n"),
    )?;

    Ok(())
}

/// Returns a 1-indexed line number range of the start and end of `needle` inside `file`.
fn line_number_at(file: impl AsRef<str>, needle: impl AsRef<str>) -> Option<(usize, usize)> {
    let file = file.as_ref();
    let needle = needle.as_ref();
    if let Some((i, _)) = file.match_indices(needle).next() {
        let start = file[..i].matches("\n").count();
        let end = needle.matches("\n").count();
        Some((start + 1, start + end + 1))
    } else {
        None
    }
}

/// Returns the number of terminal cells required for displaying line numbers. This is used to
/// determine how many characters the gutter should allocate when displaying line numbers for a
/// text file.
///
/// For example, `10` and `99` both take 2 cells, whereas `100` and `999` take 3.
fn terminal_width_required_for_line_count(line_count: usize) -> usize {
    line_count.to_string().chars().count()
}

fn stylize_output_if_able(os: &Os, path: impl AsRef<Path>, file_text: &str) -> StylizedFile {
    if supports_truecolor(os) {
        match stylized_file(path, file_text) {
            Ok(s) => return s,
            Err(err) => {
                error!(?err, "unable to syntax highlight the output");
            },
        }
    }
    StylizedFile {
        truecolor: false,
        content: file_text.to_string(),
        gutter_bg: style::Color::Reset,
        line_bg: style::Color::Reset,
    }
}

/// Represents a [String] that is potentially stylized with truecolor escape codes.
#[derive(Debug)]
struct StylizedFile {
    /// Whether or not the file is stylized with 24bit color.
    truecolor: bool,
    /// File content. If [Self::truecolor] is true, then it has escape codes for styling with 24bit
    /// color.
    content: String,
    /// Background color for the gutter.
    gutter_bg: style::Color,
    /// Background color for the line content.
    line_bg: style::Color,
}

impl Default for StylizedFile {
    fn default() -> Self {
        Self {
            truecolor: false,
            content: Default::default(),
            gutter_bg: style::Color::Reset,
            line_bg: style::Color::Reset,
        }
    }
}

/// Returns a 24bit terminal escaped syntax-highlighted [String] of the file pointed to by `path`,
/// if able.
fn stylized_file(path: impl AsRef<Path>, file_text: impl AsRef<str>) -> Result<StylizedFile> {
    let ps = &*SYNTAX_SET;
    let ts = &*THEME_SET;

    let extension = path
        .as_ref()
        .extension()
        .wrap_err("missing extension")?
        .to_str()
        .wrap_err("not utf8")?;

    let syntax = ps
        .find_syntax_by_extension(extension)
        .wrap_err_with(|| format!("missing extension: {}", extension))?;

    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);
    let file_text = file_text.as_ref().lines();
    let mut file = String::new();
    for line in file_text {
        let mut ranges = Vec::new();
        ranges.append(&mut highlighter.highlight_line(line, ps)?);
        let mut escaped_line = as_24_bit_terminal_escaped(&ranges[..], false);
        escaped_line.push_str(&format!(
            "{}\n",
            crossterm::terminal::Clear(crossterm::terminal::ClearType::UntilNewLine),
        ));
        file.push_str(&escaped_line);
    }

    let (line_bg, gutter_bg) = match (theme.settings.background, theme.settings.gutter) {
        (Some(line_bg), Some(gutter_bg)) => (line_bg, gutter_bg),
        (Some(line_bg), None) => (line_bg, line_bg),
        _ => bail!("missing theme"),
    };
    Ok(StylizedFile {
        truecolor: true,
        content: file,
        gutter_bg: syntect_to_crossterm_color(gutter_bg),
        line_bg: syntect_to_crossterm_color(line_bg),
    })
}

fn syntect_to_crossterm_color(syntect: syntect::highlighting::Color) -> style::Color {
    style::Color::Rgb {
        r: syntect.r,
        g: syntect.g,
        b: syntect.b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::chat::util::test::{
        TEST_FILE_CONTENTS,
        TEST_FILE_PATH,
        setup_test_directory,
    };

    #[test]
    fn test_fs_write_deserialize() {
        let path = "/my-file";
        let file_text = "hello world";

        // create
        let v = serde_json::json!({
            "path": path,
            "command": "create",
            "file_text": file_text
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::Create { .. }));

        // str_replace
        let v = serde_json::json!({
            "path": path,
            "command": "str_replace",
            "old_str": "prev string",
            "new_str": "new string",
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::StrReplace { .. }));

        // insert
        let v = serde_json::json!({
            "path": path,
            "command": "insert",
            "insert_line": 3,
            "new_str": "new string",
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::Insert { .. }));

        // append
        let v = serde_json::json!({
            "path": path,
            "command": "append",
            "new_str": "appended content",
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::Append { .. }));
    }

    #[test]
    fn test_fs_write_deserialize_with_summary() {
        let path = "/my-file";
        let file_text = "hello world";
        let summary = "Added hello world content";

        // create with summary
        let v = serde_json::json!({
            "path": path,
            "command": "create",
            "file_text": file_text,
            "summary": summary
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::Create { .. }));
        if let FsWrite::Create { summary: s, .. } = &fw {
            assert_eq!(s.as_ref().unwrap(), summary);
        }

        // str_replace with summary
        let v = serde_json::json!({
            "path": path,
            "command": "str_replace",
            "old_str": "prev string",
            "new_str": "new string",
            "summary": summary
        });
        let fw = serde_json::from_value::<FsWrite>(v).unwrap();
        assert!(matches!(fw, FsWrite::StrReplace { .. }));
        if let FsWrite::StrReplace { summary: s, .. } = &fw {
            assert_eq!(s.as_ref().unwrap(), summary);
        }
    }

    #[tokio::test]
    async fn test_fs_write_tool_create() {
        let os = setup_test_directory().await;
        let mut stdout = std::io::stdout();

        let file_text = "Hello, world!";
        let v = serde_json::json!({
            "path": "/my-file",
            "command": "create",
            "file_text": file_text
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();

        assert_eq!(
            os.fs.read_to_string("/my-file").await.unwrap(),
            format!("{}\n", file_text)
        );

        let file_text = "Goodbye, world!\nSee you later";
        let v = serde_json::json!({
            "path": "/my-file",
            "command": "create",
            "file_text": file_text
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();

        // File should end with a newline
        assert_eq!(
            os.fs.read_to_string("/my-file").await.unwrap(),
            format!("{}\n", file_text)
        );

        let file_text = "This is a new string";
        let v = serde_json::json!({
            "path": "/my-file",
            "command": "create",
            "new_str": file_text
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();

        assert_eq!(
            os.fs.read_to_string("/my-file").await.unwrap(),
            format!("{}\n", file_text)
        );
    }

    #[tokio::test]
    async fn test_fs_write_tool_str_replace() {
        let os = setup_test_directory().await;
        let mut stdout = std::io::stdout();

        // No instances found
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "str_replace",
            "old_str": "asjidfopjaieopr",
            "new_str": "1623749",
        });
        assert!(
            serde_json::from_value::<FsWrite>(v)
                .unwrap()
                .invoke(&os, &mut stdout)
                .await
                .is_err()
        );

        // Multiple instances found
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "str_replace",
            "old_str": "Hello world!",
            "new_str": "Goodbye world!",
        });
        assert!(
            serde_json::from_value::<FsWrite>(v)
                .unwrap()
                .invoke(&os, &mut stdout)
                .await
                .is_err()
        );

        // Single instance found and replaced
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "str_replace",
            "old_str": "1: Hello world!",
            "new_str": "1: Goodbye world!",
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();
        assert_eq!(
            os.fs
                .read_to_string(TEST_FILE_PATH)
                .await
                .unwrap()
                .lines()
                .next()
                .unwrap(),
            "1: Goodbye world!",
            "expected the only occurrence to be replaced"
        );
    }

    #[tokio::test]
    async fn test_fs_write_tool_insert_at_beginning() {
        let os = setup_test_directory().await;
        let mut stdout = std::io::stdout();

        let new_str = "1: New first line!\n";
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "insert",
            "insert_line": 0,
            "new_str": new_str,
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();
        let actual = os.fs.read_to_string(TEST_FILE_PATH).await.unwrap();
        assert_eq!(
            format!("{}\n", actual.lines().next().unwrap()),
            new_str,
            "expected the first line to be updated to '{}'",
            new_str
        );
        assert_eq!(
            actual.lines().skip(1).collect::<Vec<_>>(),
            TEST_FILE_CONTENTS.lines().collect::<Vec<_>>(),
            "the rest of the file should not have been updated"
        );
    }

    #[tokio::test]
    async fn test_fs_write_tool_insert_after_first_line() {
        let os = setup_test_directory().await;
        let mut stdout = std::io::stdout();

        let new_str = "2: New second line!\n";
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "insert",
            "insert_line": 1,
            "new_str": new_str,
        });

        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();
        let actual = os.fs.read_to_string(TEST_FILE_PATH).await.unwrap();
        assert_eq!(
            format!("{}\n", actual.lines().nth(1).unwrap()),
            new_str,
            "expected the second line to be updated to '{}'",
            new_str
        );
        assert_eq!(
            actual.lines().skip(2).collect::<Vec<_>>(),
            TEST_FILE_CONTENTS.lines().skip(1).collect::<Vec<_>>(),
            "the rest of the file should not have been updated"
        );
    }

    #[tokio::test]
    async fn test_fs_write_tool_insert_when_no_newlines_in_file() {
        let os = Os::new().await.unwrap();
        let mut stdout = std::io::stdout();

        let test_file_path = "/file.txt";
        let test_file_contents = "hello there";
        os.fs.write(test_file_path, test_file_contents).await.unwrap();

        let new_str = "test";

        // First, test appending
        let v = serde_json::json!({
            "path": test_file_path,
            "command": "insert",
            "insert_line": 1,
            "new_str": new_str,
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();
        let actual = os.fs.read_to_string(test_file_path).await.unwrap();
        assert_eq!(actual, format!("{}{}\n", test_file_contents, new_str));

        // Then, test prepending
        let v = serde_json::json!({
            "path": test_file_path,
            "command": "insert",
            "insert_line": 0,
            "new_str": new_str,
        });
        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();
        let actual = os.fs.read_to_string(test_file_path).await.unwrap();
        assert_eq!(actual, format!("{}{}{}\n", new_str, test_file_contents, new_str));
    }

    #[tokio::test]
    async fn test_fs_write_tool_append() {
        let os = setup_test_directory().await;
        let mut stdout = std::io::stdout();

        // Test appending to existing file
        let content_to_append = "5: Appended line";
        let v = serde_json::json!({
            "path": TEST_FILE_PATH,
            "command": "append",
            "new_str": content_to_append,
        });

        serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await
            .unwrap();

        let actual = os.fs.read_to_string(TEST_FILE_PATH).await.unwrap();
        assert_eq!(
            actual,
            format!("{}{}\n", TEST_FILE_CONTENTS, content_to_append),
            "Content should be appended to the end of the file with a newline added"
        );

        // Test appending to non-existent file (should fail)
        let new_file_path = "/new_append_file.txt";
        let content = "This is a new file created by append";
        let v = serde_json::json!({
            "path": new_file_path,
            "command": "append",
            "new_str": content,
        });

        let result = serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await;

        assert!(result.is_err(), "Appending to non-existent file should fail");
    }

    #[test]
    fn test_lines_with_context() {
        let content = "Hello\nWorld!\nhow\nare\nyou\ntoday?";
        assert_eq!(get_lines_with_context(content, 1, 1, 1), ("", 1, "World!\n", 2));
        assert_eq!(get_lines_with_context(content, 0, 0, 2), ("", 1, "Hello\nWorld!\n", 2));
        assert_eq!(
            get_lines_with_context(content, 2, 4, 50),
            ("Hello\n", 1, "you\ntoday?", 6)
        );
        assert_eq!(get_lines_with_context(content, 4, 100, 2), ("World!\nhow\n", 2, "", 6));
    }

    #[test]
    fn test_gutter_width() {
        assert_eq!(terminal_width_required_for_line_count(1), 1);
        assert_eq!(terminal_width_required_for_line_count(9), 1);
        assert_eq!(terminal_width_required_for_line_count(10), 2);
        assert_eq!(terminal_width_required_for_line_count(99), 2);
        assert_eq!(terminal_width_required_for_line_count(100), 3);
        assert_eq!(terminal_width_required_for_line_count(999), 3);
    }

    #[tokio::test]
    async fn test_fs_write_with_tilde_paths() {
        // Create a test context
        let os = Os::new().await.unwrap();
        let mut stdout = std::io::stdout();

        // Get the home directory from the context
        let home_dir = os.env.home().unwrap_or_default();
        println!("Test home directory: {:?}", home_dir);

        // Create a file directly in the home directory first to ensure it exists
        let home_path = os.fs.chroot_path(&home_dir);
        println!("Chrooted home path: {:?}", home_path);

        // Ensure the home directory exists
        os.fs.create_dir_all(&home_path).await.unwrap();

        let v = serde_json::json!({
            "path": "~/file.txt",
            "command": "create",
            "file_text": "content in home file"
        });

        let result = serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await;

        match &result {
            Ok(_) => println!("Writing to ~/file.txt succeeded"),
            Err(e) => println!("Writing to ~/file.txt failed: {:?}", e),
        }

        assert!(result.is_ok(), "Writing to ~/file.txt should succeed");

        // Verify content was written correctly
        let file_path = home_path.join("file.txt");
        println!("Checking file at: {:?}", file_path);

        let content_result = os.fs.read_to_string(&file_path).await;
        match &content_result {
            Ok(content) => println!("Read content: {:?}", content),
            Err(e) => println!("Failed to read content: {:?}", e),
        }

        assert!(content_result.is_ok(), "Should be able to read from expanded path");
        assert_eq!(content_result.unwrap(), "content in home file\n");

        // Test with "~/nested/path/file.txt" to ensure deep paths work
        let nested_dir = home_path.join("nested").join("path");
        os.fs.create_dir_all(&nested_dir).await.unwrap();

        let v = serde_json::json!({
            "path": "~/nested/path/file.txt",
            "command": "create",
            "file_text": "content in nested path"
        });

        let result = serde_json::from_value::<FsWrite>(v)
            .unwrap()
            .invoke(&os, &mut stdout)
            .await;

        assert!(result.is_ok(), "Writing to ~/nested/path/file.txt should succeed");

        // Verify nested path content
        let nested_file_path = nested_dir.join("file.txt");
        let nested_content = os.fs.read_to_string(&nested_file_path).await.unwrap();
        assert_eq!(nested_content, "content in nested path\n");
    }
}
