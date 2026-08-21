use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;
use walkdir::WalkDir;

const DEFAULT_IGNORES: &[&str] = &[
    "**/node_modules",
    "**/dist",
    "**/.git",
    "**/LICENSE*",
    "**/*.png",
    "**/*.jpg",
    "**/*.jpeg",
    "**/*.gif",
    "**/*.tif",
    "**/*.ico",
    "**/*.json",
    "**/*.zip",
    "**/*.tgz",
];

const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid glob pattern `{pattern}`: {source}")]
    Glob {
        pattern: String,
        source: globset::Error,
    },
    #[error("Invalid regex `{pattern}`: {source}")]
    Regex {
        pattern: String,
        source: regex::Error,
    },
    #[error("License check failed. {count} file(s) did not have the license.")]
    CheckFailed { count: usize, missing: Vec<PathBuf> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Check,
    Add,
    Remove,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct LineFormat {
    pub append: Option<String>,
    pub prepend: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Format {
    pub append: Option<String>,
    pub prepend: Option<String>,
    #[serde(rename = "eachLine")]
    pub each_line: Option<LineFormat>,
    pub file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InputConfig {
    license: Option<String>,
    #[serde(rename = "ignoreDefaultIgnores")]
    ignore_default_ignores: Option<bool>,
    ignore: Option<Vec<String>>,
    #[serde(rename = "ignoreFile")]
    ignore_file: Option<String>,
    #[serde(rename = "trailingWhitespace")]
    trailing_whitespace: Option<String>,
    output: Option<String>,
    #[serde(rename = "regexIdentifier")]
    regex_identifier: Option<String>,
    #[serde(rename = "defaultFormat")]
    default_format: Option<Format>,
    #[serde(rename = "licenseFormats")]
    license_formats: Option<HashMap<String, Format>>,
}

#[derive(Clone, Debug)]
struct RegexConfig {
    identifier: String,
    replacements: Option<Vec<String>>,
}

#[derive(Debug)]
struct Config {
    default_format: Format,
    ignore_default_ignores: bool,
    ignore: Vec<String>,
    ignore_file: Option<PathBuf>,
    license: String,
    license_formats: HashMap<String, Format>,
    output: Option<PathBuf>,
    regex: Option<RegexConfig>,
    trim_trailing_whitespace: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub inserted: Vec<PathBuf>,
    pub missing: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

pub fn execute(
    root: &Path,
    config_path: &Path,
    mode: Mode,
    replacements: Option<Vec<String>>,
) -> Result<Report, AppError> {
    let config = load_config(root, config_path, mode, replacements)?;
    let paths = discover_files(root, &config)?;
    manage_files(root, &config, paths, mode)
}

fn load_config(
    root: &Path,
    config_path: &Path,
    mode: Mode,
    replacements: Option<Vec<String>>,
) -> Result<Config, AppError> {
    let config_text = fs::read_to_string(resolve_path(root, config_path))?;
    let input: InputConfig = serde_json::from_str(&config_text)?;
    let license_path = input
        .license
        .ok_or_else(|| AppError::Message("Missing required field in config: license".to_owned()))?;
    let regex = input.regex_identifier.map(|identifier| RegexConfig {
        identifier,
        replacements,
    });

    if mode == Mode::Add
        && regex.is_some()
        && regex
            .as_ref()
            .and_then(|value| value.replacements.as_ref())
            .is_none()
    {
        return Err(AppError::Message(
            "Must supply regexReplacements option when using regexIdentifier in config when in INSERT mode"
                .to_owned(),
        ));
    }

    let trim_trailing_whitespace = input
        .trailing_whitespace
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("TRIM"));

    Ok(Config {
        default_format: input.default_format.unwrap_or_else(default_format),
        ignore_default_ignores: input.ignore_default_ignores.unwrap_or(false),
        ignore: input.ignore.unwrap_or_default(),
        ignore_file: input.ignore_file.map(PathBuf::from),
        license: fs::read_to_string(resolve_path(root, Path::new(&license_path)))?,
        license_formats: input.license_formats.unwrap_or_default(),
        output: input
            .output
            .map(|path| resolve_path(root, Path::new(&path))),
        regex,
        trim_trailing_whitespace,
    })
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}

fn discover_files(root: &Path, config: &Config) -> Result<Vec<PathBuf>, AppError> {
    let mut rules = Vec::new();

    if let Some(ignore_file) = &config.ignore_file {
        let ignore_path = resolve_path(root, ignore_file);
        let contents = fs::read_to_string(&ignore_path)?;
        for line in contents.lines() {
            if let Some(rule) = IgnoreRule::parse(line, true) {
                rules.push(rule?);
            }
        }
    }

    for pattern in &config.ignore {
        if let Some(rule) = IgnoreRule::parse(pattern, false) {
            rules.push(rule?);
        }
    }

    if !config.ignore_default_ignores {
        for pattern in DEFAULT_IGNORES {
            rules.push(
                IgnoreRule::parse(pattern, false).expect("default ignore patterns are valid")?,
            );
        }
    }

    let ignore_file = config
        .ignore_file
        .as_ref()
        .map(|path| normalize_relative_path(root, &resolve_path(root, path)));
    let mut paths = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            error
                .into_io_error()
                .unwrap_or_else(|| io::Error::other("failed to walk directory"))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path().to_owned();
        let relative = normalize_relative_path(root, &path);
        if ignore_file.as_deref() == Some(relative.as_str())
            || is_ignored(&relative, false, &rules)?
        {
            continue;
        }

        paths.push(path);
    }

    Ok(paths)
}

fn normalize_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_owned()
}

fn manage_files(
    root: &Path,
    config: &Config,
    paths: Vec<PathBuf>,
    mode: Mode,
) -> Result<Report, AppError> {
    let formatter = Formatter::new(root, config);
    let mut formatted_cache: HashMap<String, String> = HashMap::new();
    let mut report = Report::default();

    for path in paths {
        let contents = fs::read_to_string(&path)?;
        let relative = normalize_relative_path(root, &path);
        let key = format_key(&path);
        let formatted = if let Some(value) = formatted_cache.get(&key) {
            value.clone()
        } else {
            let value = formatter.format(&key, &config.license)?;
            formatted_cache.insert(key, value.clone());
            value
        };
        let normalized_license = build_match_pattern(
            &formatted,
            config.trim_trailing_whitespace,
            config.regex.as_ref(),
        )?;
        let normalized_contents = normalize_for_check(
            &contents,
            config.trim_trailing_whitespace,
            config.regex.as_ref(),
        )?;
        let has_license = Regex::new(&normalized_license)
            .map_err(|source| AppError::Regex {
                pattern: normalized_license.clone(),
                source,
            })?
            .is_match(&normalized_contents);

        if !has_license {
            match mode {
                Mode::Add => {
                    let inserted = apply_replacements(&formatted, config.regex.as_ref())?;
                    let new_contents = insert_license(&contents, &inserted);
                    fs::write(&path, new_contents)?;
                    report.inserted.push(PathBuf::from(relative));
                }
                Mode::Check => report.missing.push(PathBuf::from(relative)),
                Mode::Remove => {}
            }
        } else if mode == Mode::Remove && remove_license(&contents, &normalized_license)? {
            fs::write(
                &path,
                remove_matching_license(&contents, &normalized_license)?,
            )?;
            report.removed.push(PathBuf::from(relative));
        }
    }

    if let Some(output) = &config.output {
        let selected = match mode {
            Mode::Add => &report.inserted,
            Mode::Check => &report.missing,
            Mode::Remove => &report.removed,
        };
        let output_text = selected
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(EOL);
        fs::write(output, output_text)?;
    }

    if mode == Mode::Check && !report.missing.is_empty() {
        return Err(AppError::CheckFailed {
            count: report.missing.len(),
            missing: report.missing,
        });
    }

    Ok(report)
}

fn format_key(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_owned()
        })
}

struct Formatter<'a> {
    root: &'a Path,
    config: &'a Config,
    formats: HashMap<String, Format>,
}

impl<'a> Formatter<'a> {
    fn new(root: &'a Path, config: &'a Config) -> Self {
        let mut formats = HashMap::new();
        add_formats(
            &mut formats,
            &[
                (
                    "gitignore|npmignore|eslintignore|dockerignore|sh|py",
                    each_line("# ", None),
                ),
                ("html|xml|svg", wrapped("<!--", "-->")),
                (
                    "js|ts|css|scss|less|php|as|c|java|cpp|go|cto|acl",
                    wrapped("/*", "*/"),
                ),
                ("txt", Format::default()),
            ],
        );
        add_formats_map(&mut formats, &config.license_formats);
        Self {
            root,
            config,
            formats,
        }
    }

    fn format(&self, key: &str, license: &str) -> Result<String, AppError> {
        let format = self.formats.get(key).unwrap_or(&self.config.default_format);
        if let Some(file) = &format.file {
            return Ok(fs::read_to_string(resolve_path(
                self.root,
                Path::new(file),
            ))?);
        }

        let mut text = trim_license_line_endings(license).to_owned();
        if let Some(each_line) = &format.each_line {
            text = split_lines(&text)
                .into_iter()
                .map(|mut line| {
                    if let Some(prepend) = &each_line.prepend {
                        line = format!("{prepend}{line}");
                    }
                    if let Some(append) = &each_line.append {
                        line.push_str(append);
                    }
                    if self.config.trim_trailing_whitespace {
                        line = line.trim_end_matches(char::is_whitespace).to_owned();
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join(EOL);
        }
        if let Some(prepend) = &format.prepend {
            text = format!("{prepend}{EOL}{text}");
        }
        if let Some(append) = &format.append {
            text = format!("{text}{EOL}{append}");
        }
        Ok(text)
    }
}

fn each_line(prepend: &str, append: Option<&str>) -> Format {
    Format {
        each_line: Some(LineFormat {
            append: append.map(str::to_owned),
            prepend: Some(prepend.to_owned()),
        }),
        ..Format::default()
    }
}

fn wrapped(prepend: &str, append: &str) -> Format {
    Format {
        append: Some(append.to_owned()),
        prepend: Some(prepend.to_owned()),
        ..Format::default()
    }
}

fn add_formats(formats: &mut HashMap<String, Format>, entries: &[(&str, Format)]) {
    for (keys, format) in entries {
        for key in keys.split('|') {
            formats.insert(format_key_name(key), format.clone());
        }
    }
}

fn add_formats_map(formats: &mut HashMap<String, Format>, declared: &HashMap<String, Format>) {
    for (keys, format) in declared {
        for key in keys.split('|') {
            formats.insert(format_key_name(key), format.clone());
        }
    }
}

fn format_key_name(key: &str) -> String {
    key.strip_prefix('^')
        .map_or_else(|| format!(".{key}"), str::to_owned)
}

fn default_format() -> Format {
    Format {
        append: Some("*/".to_owned()),
        prepend: Some("/*".to_owned()),
        ..Format::default()
    }
}

fn trim_license_line_endings(text: &str) -> &str {
    let mut start = 0;
    let mut end = text.len();
    while start < end {
        if text[start..].starts_with("\r\n") {
            start += 2;
        } else if text[start..].starts_with('\n') {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        if text[..end].ends_with("\r\n") {
            end -= 2;
        } else if text[..end].ends_with('\n') {
            end -= 1;
        } else {
            break;
        }
    }
    &text[start..end]
}

fn split_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn normalize_for_check(
    text: &str,
    trim_trailing_whitespace: bool,
    regex_config: Option<&RegexConfig>,
) -> Result<String, AppError> {
    if let Some(config) = regex_config {
        validate_markers(text, &config.identifier)?;
    }
    let lines = split_lines(text)
        .into_iter()
        .map(|line| {
            if trim_trailing_whitespace {
                line
            } else {
                line.trim_end_matches(char::is_whitespace).to_owned()
            }
        })
        .collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

fn build_match_pattern(
    text: &str,
    trim_trailing_whitespace: bool,
    regex_config: Option<&RegexConfig>,
) -> Result<String, AppError> {
    let normalized = normalize_for_check(text, trim_trailing_whitespace, regex_config)?;
    if let Some(config) = regex_config {
        let parts = normalized.split(&config.identifier).collect::<Vec<_>>();
        let mut pattern = String::new();
        for (index, part) in parts.iter().enumerate() {
            if index % 2 == 0 {
                pattern.push_str(&regex::escape(part));
            } else {
                pattern.push_str(part);
            }
        }
        Ok(pattern)
    } else {
        Ok(regex::escape(&normalized))
    }
}

fn validate_markers(text: &str, identifier: &str) -> Result<(), AppError> {
    if identifier.is_empty() {
        return Err(AppError::Message(
            "Regex identifier cannot be empty".to_owned(),
        ));
    }
    if text.split(identifier).count().is_multiple_of(2) {
        return Err(AppError::Message(
            "Odd number of regex identifiers found. One must be missing its close".to_owned(),
        ));
    }
    Ok(())
}

fn apply_replacements(text: &str, regex_config: Option<&RegexConfig>) -> Result<String, AppError> {
    let Some(config) = regex_config else {
        return Ok(text.to_owned());
    };
    let replacements = config.replacements.as_deref().unwrap_or_default();
    let mut replacement_index = 0;
    let mut lines = Vec::new();

    for line in split_lines(text) {
        let parts = line.split(&config.identifier).collect::<Vec<_>>();
        let mut replaced = String::new();
        for (index, part) in parts.iter().enumerate() {
            if index % 2 == 0 {
                replaced.push_str(part);
                continue;
            }

            let replacement = if replacements.len() == 1 {
                &replacements[0]
            } else {
                let Some(value) = replacements.get(replacement_index) else {
                    return Err(AppError::Message(format!(
                        "Too few replacement values passed. Found at least {} regex values. Only have {} replacements",
                        replacement_index + 1,
                        replacements.len()
                    )));
                };
                replacement_index += 1;
                value
            };
            let matcher = Regex::new(part).map_err(|source| AppError::Regex {
                pattern: (*part).to_owned(),
                source,
            })?;
            if !matcher.is_match(replacement) {
                return Err(AppError::Message(format!(
                    "Replacement value {replacement} does not match regex it is to replace: {part}"
                )));
            }
            replaced.push_str(replacement);
        }
        lines.push(replaced);
    }
    Ok(lines.join(EOL))
}

fn insert_license(contents: &str, license: &str) -> String {
    if let Some(first_line_end) = contents.find(['\n', '\r'])
        && contents.starts_with("#!")
    {
        let shebang = &contents[..first_line_end];
        let line_end_len = if contents[first_line_end..].starts_with("\r\n") {
            2
        } else {
            1
        };
        let rest = &contents[first_line_end + line_end_len..];
        return format!("{shebang}{EOL}{license}{EOL}{rest}");
    }
    if contents.starts_with("#!") {
        return format!("{contents}{EOL}{license}{EOL}");
    }
    format!("{license}{EOL}{contents}")
}

fn remove_license(contents: &str, normalized_license: &str) -> Result<bool, AppError> {
    let file_lines = split_lines(contents);
    let license_lines = normalized_license.split('\n').collect::<Vec<_>>();
    for start in 0..file_lines.len() {
        if license_lines.iter().enumerate().all(|(offset, pattern)| {
            Regex::new(&format!("^{pattern}"))
                .map(|matcher| {
                    file_lines
                        .get(start + offset)
                        .is_some_and(|line| matcher.is_match(line))
                })
                .unwrap_or(false)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_matching_license(contents: &str, normalized_license: &str) -> Result<String, AppError> {
    let mut file_lines = split_lines(contents);
    let license_lines = normalized_license.split('\n').collect::<Vec<_>>();
    for start in 0..file_lines.len() {
        let matches = license_lines.iter().enumerate().all(|(offset, pattern)| {
            Regex::new(&format!("^{pattern}"))
                .map(|matcher| {
                    file_lines
                        .get(start + offset)
                        .is_some_and(|line| matcher.is_match(line))
                })
                .unwrap_or(false)
        });
        if matches {
            file_lines.drain(start..start + license_lines.len());
            return Ok(file_lines.join(EOL));
        }
    }
    Ok(contents.to_owned())
}

#[derive(Debug)]
struct IgnoreRule {
    pattern: String,
    negated: bool,
    git_style: bool,
    directory_only: bool,
}

impl IgnoreRule {
    fn parse(raw: &str, git_style: bool) -> Option<Result<Self, AppError>> {
        let raw = raw.trim();
        if raw.is_empty() || (git_style && raw.starts_with('#')) {
            return None;
        }
        let negated = raw.starts_with('!');
        let mut pattern = if negated { &raw[1..] } else { raw };
        let directory_only = pattern.ends_with('/');
        if directory_only {
            pattern = pattern.trim_end_matches('/');
        }
        let pattern = pattern.trim_start_matches('/').to_owned();
        Some(Ok(Self {
            pattern,
            negated,
            git_style,
            directory_only,
        }))
    }

    fn matches(&self, relative: &str, is_directory: bool) -> Result<bool, AppError> {
        if self.directory_only && !is_directory {
            return Ok(false);
        }
        let candidates = path_prefixes(relative);
        if self.git_style && !self.pattern.contains('/') {
            for candidate in candidates {
                if let Some(name) = candidate.rsplit('/').next()
                    && glob_matches(&self.pattern, name)?
                {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        for candidate in candidates {
            if glob_matches(&self.pattern, &candidate)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn path_prefixes(relative: &str) -> Vec<String> {
    let components = relative.split('/').collect::<Vec<_>>();
    (1..=components.len())
        .map(|end| components[..end].join("/"))
        .collect()
}

fn is_ignored(relative: &str, is_directory: bool, rules: &[IgnoreRule]) -> Result<bool, AppError> {
    let mut ignored = false;
    for rule in rules {
        if rule.matches(relative, is_directory)? {
            ignored = !rule.negated;
        }
    }
    Ok(ignored)
}

fn glob_matches(pattern: &str, value: &str) -> Result<bool, AppError> {
    if pattern.contains("!(") {
        return negated_extglob_matches(pattern, value);
    }
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|source| AppError::Glob {
            pattern: pattern.to_owned(),
            source,
        })?;
    Ok(glob.compile_matcher().is_match(value))
}

fn negated_extglob_matches(pattern: &str, value: &str) -> Result<bool, AppError> {
    let open = pattern
        .find("!(")
        .ok_or_else(|| AppError::Message(format!("Unsupported ignore pattern: {pattern}")))?;
    let close = pattern[open + 2..]
        .find(')')
        .map(|index| open + 2 + index)
        .ok_or_else(|| AppError::Message(format!("Unsupported ignore pattern: {pattern}")))?;
    let prefix = glob_fragment_regex(&pattern[..open]);
    let suffix = glob_fragment_regex(&pattern[close + 1..]);
    let regex =
        Regex::new(&format!("^{prefix}([^/]+){suffix}$")).map_err(|source| AppError::Regex {
            pattern: pattern.to_owned(),
            source,
        })?;
    let Some(captures) = regex.captures(value) else {
        return Ok(false);
    };
    let extglob_value = captures
        .get(1)
        .map(|value| value.as_str())
        .unwrap_or_default();
    Ok(!glob_matches(&pattern[open + 2..close], extglob_value)?)
}

fn glob_fragment_regex(fragment: &str) -> String {
    let mut output = String::new();
    let characters = fragment.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' if characters.get(index + 1) == Some(&'*') => {
                if characters.get(index + 2) == Some(&'/') {
                    output.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    output.push_str(".*");
                    index += 2;
                }
            }
            '*' => {
                output.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                output.push_str("[^/]");
                index += 1;
            }
            character => {
                output.push_str(&regex::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    output
}

pub fn print_report(report: &Report, mode: Mode) {
    match mode {
        Mode::Add => println!(
            "\x1b[33m!\x1b[0m Inserted license into {} file(s)",
            report.inserted.len()
        ),
        Mode::Check => println!("\x1b[32m\u{2714}\x1b[0m All files have licenses."),
        Mode::Remove => println!(
            "\x1b[32m\u{2714}\x1b[0m Removed license from {} file(s).",
            report.removed.len()
        ),
    }
}

pub fn print_missing(error: &AppError) {
    if let AppError::CheckFailed { missing, .. } = error {
        for path in missing {
            eprintln!(
                "\x1b[31m\u{2717}\x1b[0m License not found in {}",
                path.to_string_lossy()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_each_line_and_trims_generated_whitespace() {
        let config = Config {
            default_format: Format::default(),
            ignore_default_ignores: true,
            ignore: Vec::new(),
            ignore_file: None,
            license: String::new(),
            license_formats: HashMap::new(),
            output: None,
            regex: None,
            trim_trailing_whitespace: true,
        };
        let formatter = Formatter::new(Path::new("."), &config);
        let format = Format {
            each_line: Some(LineFormat {
                append: None,
                prepend: Some("# ".to_owned()),
            }),
            ..Format::default()
        };
        let mut formats = HashMap::new();
        formats.insert(".txt".to_owned(), format);
        let formatter = Formatter {
            formats,
            ..formatter
        };
        assert_eq!(
            formatter
                .format(".txt", "license\nwith\n\nblank\n")
                .unwrap(),
            ["# license", "# with", "#", "# blank"].join(EOL)
        );
    }

    #[test]
    fn builds_literal_and_regex_parts() {
        let regex_config = RegexConfig {
            identifier: "##".to_owned(),
            replacements: None,
        };
        let pattern =
            build_match_pattern("Copyright (c) ##[0-9]{4}##", false, Some(&regex_config)).unwrap();
        assert!(Regex::new(&pattern).unwrap().is_match("Copyright (c) 2026"));
        assert!(!Regex::new(&pattern).unwrap().is_match("Copyright (c) 26"));
    }

    #[test]
    fn replaces_markers_and_rejects_invalid_values() {
        let config = RegexConfig {
            identifier: "##".to_owned(),
            replacements: Some(vec!["bell002".to_owned(), "2021".to_owned()]),
        };
        assert_eq!(
            apply_replacements("##[a-z]{4}[0-9]{3}## ##[0-9]{4}##", Some(&config)).unwrap(),
            "bell002 2021"
        );
        let invalid = RegexConfig {
            replacements: Some(vec!["bellO02".to_owned(), "2021".to_owned()]),
            ..config
        };
        assert!(apply_replacements("##[a-z]{4}[0-9]{3}## ##[0-9]{4}##", Some(&invalid)).is_err());
    }

    #[test]
    fn inserts_after_shebang_and_removes_first_matching_block() {
        let contents = "#!/bin/sh\necho hi\n";
        let inserted = insert_license(contents, "# license");
        assert_eq!(inserted, format!("#!/bin/sh{EOL}# license{EOL}echo hi\n"));
        let contents = "# license\nbody\n# license\nbody\n";
        assert!(remove_license(contents, "# license").unwrap());
        assert_eq!(
            remove_matching_license(contents, "# license").unwrap(),
            format!("body{EOL}# license{EOL}body{EOL}")
        );
    }

    #[test]
    fn supports_fixture_extglob_and_gitignore_exception() {
        assert!(glob_matches("**/!(text_file_to_not_ignore).txt", "sub/odd.txt").unwrap());
        assert!(
            !glob_matches(
                "**/!(text_file_to_not_ignore).txt",
                "sub/text_file_to_not_ignore.txt"
            )
            .unwrap()
        );
        let rules = vec![
            IgnoreRule::parse("*.txt", true).unwrap().unwrap(),
            IgnoreRule::parse("!sub/text.txt", true).unwrap().unwrap(),
        ];
        assert!(!is_ignored("sub/text.txt", false, &rules).unwrap());
    }

    #[test]
    fn executes_add_check_and_remove() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("LICENSE"), "license text\n").unwrap();
        fs::write(directory.path().join("file.txt"), "body\n").unwrap();
        fs::write(
            directory.path().join("config.json"),
            r#"{"license":"LICENSE","ignore":["config.json"],"licenseFormats":{"txt":{}}}"#,
        )
        .unwrap();
        let config_path = Path::new("config.json");
        let report = execute(directory.path(), config_path, Mode::Add, None).unwrap();
        assert_eq!(report.inserted, vec![PathBuf::from("file.txt")]);
        let report = execute(directory.path(), config_path, Mode::Check, None).unwrap();
        assert!(report.missing.is_empty());
        let report = execute(directory.path(), config_path, Mode::Remove, None).unwrap();
        assert_eq!(report.removed.len(), 1);
    }
}
