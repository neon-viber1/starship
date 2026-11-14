use clap::{ValueEnum, builder::PossibleValue};
use nu_ansi_term::AnsiStrings;
use rayon::prelude::*;
use regex::Regex;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::{Debug, Write as FmtWrite};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use terminal_size::terminal_size;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::configs::PROMPT_ORDER;
use crate::context::{Context, Properties, Shell, Target};
use crate::formatter::{StringFormatter, VariableHolder};
use crate::module::ALL_MODULES;
use crate::module::Module;
use crate::modules;
use crate::segment::Segment;
use crate::shadow;
use crate::utils::wrap_colorseq_for_shell;

pub struct Grapheme<'a>(pub &'a str);

impl Grapheme<'_> {
    pub fn width(&self) -> usize {
        self.0
            .chars()
            .filter_map(UnicodeWidthChar::width)
            .max()
            .unwrap_or(0)
    }
}

pub trait UnicodeWidthGraphemes {
    fn width_graphemes(&self) -> usize;
}

static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();

fn ansi_strip() -> &'static Regex {
    ANSI_REGEX.get_or_init(|| Regex::new(r"\x1B\[[0-9;]*m").unwrap())
}

impl<T> UnicodeWidthGraphemes for T
where
    T: AsRef<str>,
{
    fn width_graphemes(&self) -> usize {
        ansi_strip()
            .replace_all(self.as_ref(), "")
            .into_owned()
            .graphemes(true)
            .map(Grapheme)
            .map(|g| g.width())
            .sum()
    }
}

#[test]
fn test_grapheme_aware_width() {
    // UnicodeWidthStr::width would return 8
    assert_eq!(2, "👩‍👩‍👦‍👦".width_graphemes());
    assert_eq!(1, "Ü".width_graphemes());
    assert_eq!(11, "normal text".width_graphemes());
    // Magenta string test
    assert_eq!(11, "\x1B[35;6mnormal text".width_graphemes());
}

pub fn prompt(args: Properties, target: Target) {
    let context = ContextGuard::new(Context::new(args, target));
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    write!(handle, "{}", get_prompt_guarded(&context)).unwrap();
}

pub fn prompt_with_claude_code(args: Properties, target: Target) {
    let claude_data = serde_json::from_reader(io::stdin())
        .inspect_err(|e| log::error!("Failed to read Claude Code JSON from stdin: {e}"))
        .unwrap_or_default();

    let mut context = Context::new(args, target).with_claude_code_data(claude_data);
    context.shell = Shell::Unknown;
    let context = ContextGuard::new(context);
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    write!(handle, "{}", get_prompt_guarded(&context)).unwrap();
}

/// Renders the prompt without any per-module time limit.
pub fn get_prompt(context: &Context) -> String {
    get_prompt_impl(context, None)
}

/// Renders the prompt, skipping modules that exceed `module_timeout`.
fn get_prompt_guarded(context: &ContextGuard) -> String {
    let prefetch = Prefetch::new(context);
    get_prompt_impl(context, Some(&prefetch))
}

fn get_prompt_impl(context: &Context, timed: Option<&Prefetch>) -> String {
    let config = &context.root_config;
    let mut buf = String::new();

    match context.get_env("TERM") {
        Some(term) if term == "dumb" => {
            log::error!("Under a 'dumb' terminal (TERM=dumb).");
            buf.push_str("Starship disabled due to TERM=dumb > ");
            return buf;
        }
        _ => {}
    }

    // A workaround for a fish bug (see #739,#279). Applying it to all shells
    // breaks things (see #808,#824,#834). Should only be printed in fish.
    if Shell::Fish == context.shell && context.target == Target::Main {
        buf.push_str("\x1b[J"); // An ASCII control code to clear screen
    }

    let (formatter, modules) = load_formatter_and_modules(context);
    if let Some(prefetch) = timed {
        prefetch.start_all(context, &modules);
    }

    let formatter = formatter.map_variables_to_segments(|module| {
        // Make $all display all modules not explicitly referenced
        if module == "all" {
            Some(Ok(all_modules_uniq(&modules)
                .par_iter()
                .flat_map(|module| {
                    handle_module(module, context, &modules, timed)
                        .into_iter()
                        .flat_map(|module| module.segments)
                        .collect::<Vec<Segment>>()
                })
                .collect::<Vec<_>>()))
        } else if context.is_module_disabled_in_config(module) {
            None
        } else {
            // Get segments from module
            Some(Ok(handle_module(module, context, &modules, timed)
                .into_iter()
                .flat_map(|module| module.segments)
                .collect::<Vec<Segment>>()))
        }
    });

    // Creates a root module and prints it.
    let mut root_module = Module::new("Starship Root", "The root module", None);
    root_module.set_segments(
        formatter
            .parse(None, Some(context))
            .expect("Unexpected error returned in root format variables"),
    );

    let module_strings = root_module.ansi_strings_for_width(Some(context.width));
    if config.add_newline && context.target != Target::Continuation {
        // continuation prompts normally do not include newlines, but they can
        writeln!(buf).unwrap();
    }
    // AnsiStrings strips redundant ANSI color sequences, so apply it before modifying the ANSI
    // color sequences for this specific shell
    let shell_wrapped_output =
        wrap_colorseq_for_shell(AnsiStrings(&module_strings).to_string(), context.shell);
    write!(buf, "{shell_wrapped_output}").unwrap();

    if context.target == Target::Right {
        // right prompts generally do not allow newlines
        buf = buf.replace('\n', "");
    }

    // escape \n and ! characters for tcsh
    if context.shell == Shell::Tcsh {
        buf = buf.replace('!', "\\!");
        // space is required before newline
        buf = buf.replace('\n', " \\n");
    }

    buf
}

pub fn module(module_name: &str, args: Properties) {
    let context = ContextGuard::new(Context::new(args, Target::Main));
    let module = get_module_guarded(module_name, &context).unwrap_or_default();
    print!("{module}");
}

/// Renders a single module without any time limit.
pub fn get_module(module_name: &str, context: &Context) -> Option<String> {
    handle_with_timeout(module_name, context, None).map(|m| m.to_string())
}

fn get_module_guarded(module_name: &str, context: &ContextGuard) -> Option<String> {
    let prefetch = Prefetch::new(context);
    handle_with_timeout(module_name, context, Some(&prefetch)).map(|m| m.to_string())
}

pub fn timings(args: Properties) {
    let context = ContextGuard::new(Context::new(args, Target::Main));
    let prefetch = Prefetch::new(&context);

    struct ModuleTiming {
        name: String,
        name_len: usize,
        value: String,
        duration: Duration,
        duration_len: usize,
    }

    let mut modules = compute_modules(&context, Some(&prefetch))
        .iter()
        .filter(|module| !module.is_empty() || module.duration.as_millis() > 0)
        .map(|module| ModuleTiming {
            name: String::from(module.get_name().as_str()),
            name_len: module.get_name().width_graphemes(),
            value: nu_ansi_term::AnsiStrings(&module.ansi_strings())
                .to_string()
                .replace('\n', "\\n"),
            duration: module.duration,
            duration_len: format_duration(&module.duration).width_graphemes(),
        })
        .collect::<Vec<ModuleTiming>>();

    modules.sort_by_key(|m| std::cmp::Reverse(m.duration));

    let max_name_width = modules.iter().map(|i| i.name_len).max().unwrap_or(0);
    let max_duration_width = modules.iter().map(|i| i.duration_len).max().unwrap_or(0);

    println!("\n Here are the timings of modules in your prompt (>=1ms or output):");

    // for now we do not expect a wrap around at the end... famous last words
    // Overall a line looks like this: " {module name}  -  {duration}  -  "{module value}"".
    for timing in &modules {
        println!(
            " {}{}  -  {}{}  -   \"{}\"",
            timing.name,
            " ".repeat(max_name_width - (timing.name_len)),
            " ".repeat(max_duration_width - (timing.duration_len)),
            format_duration(&timing.duration),
            timing.value
        );
    }
}

pub fn explain(args: Properties) {
    let context = ContextGuard::new(Context::new(args, Target::Main));
    let prefetch = Prefetch::new(&context);

    struct ModuleInfo {
        value: String,
        value_len: usize,
        desc: String,
        duration: String,
    }

    static DONT_PRINT: &[&str] = &["line_break"];

    let modules = compute_modules(&context, Some(&prefetch))
        .into_iter()
        .filter(|module| !DONT_PRINT.contains(&module.get_name().as_str()))
        // this contains empty modules which should not print
        .filter(|module| !module.is_empty())
        .map(|module| {
            let value = module.get_segments().join("");
            ModuleInfo {
                value: nu_ansi_term::AnsiStrings(&module.ansi_strings()).to_string(),
                value_len: value.width_graphemes()
                    + format_duration(&module.duration).width_graphemes(),
                desc: module.get_description().clone(),
                duration: format_duration(&module.duration),
            }
        })
        .collect::<Vec<ModuleInfo>>();

    let max_module_width = modules.iter().map(|i| i.value_len).max().unwrap_or(0);

    // In addition to the module width itself there are also 11 padding characters in each line.
    // Overall a line looks like this: " "{module value}" ({xxxms})  -  {description}".
    const PADDING_WIDTH: usize = 11;

    let desc_width = terminal_size()
        .map(|(w, _)| w.0 as usize)
        // Add padding length to module length to avoid text overflow. This line also assures desc_width >= 0.
        .map(|width| width - std::cmp::min(width, max_module_width + PADDING_WIDTH));

    println!("\n Here's a breakdown of your prompt:");
    for info in modules {
        if let Some(desc_width) = desc_width {
            // Custom Textwrapping!
            let mut current_pos = 0;
            let mut escaping = false;
            // Print info
            print!(
                " \"{}\" ({}){}  -  ",
                info.value,
                info.duration,
                " ".repeat(max_module_width - (info.value_len))
            );
            for g in info.desc.graphemes(true) {
                // Handle ANSI escape sequences
                if g == "\x1B" {
                    escaping = true;
                }
                if escaping {
                    print!("{g}");
                    escaping = !(("a"..="z").contains(&g) || ("A"..="Z").contains(&g));
                    continue;
                }

                // Handle normal wrapping
                current_pos += Grapheme(g).width();
                // Wrap when hitting max width or newline
                if g == "\n" || current_pos > desc_width {
                    // trim spaces on linebreak
                    if g == " " && desc_width > 1 {
                        continue;
                    }

                    print!("\n{}", " ".repeat(max_module_width + PADDING_WIDTH));
                    if g == "\n" {
                        current_pos = 0;
                        continue;
                    }

                    current_pos = 1;
                }
                print!("{g}");
            }
            println!();
        } else {
            println!(
                " {}{}  -  {}",
                info.value,
                " ".repeat(max_module_width - info.value_len),
                info.desc,
            );
        }
    }
}

fn compute_modules<'a>(context: &'a Context, timed: Option<&Prefetch>) -> Vec<Module<'a>> {
    let mut prompt_order: Vec<Module<'a>> = Vec::new();

    let (_formatter, modules) = load_formatter_and_modules(context);
    if let Some(prefetch) = timed {
        prefetch.start_all(context, &modules);
    }

    for module in &modules {
        // Manually add all modules if `$all` is encountered
        if module == "all" {
            for module in all_modules_uniq(&modules) {
                let modules = handle_module(&module, context, &modules, timed);
                prompt_order.extend(modules);
            }
        } else {
            let modules = handle_module(module, context, &modules, timed);
            prompt_order.extend(modules);
        }
    }

    prompt_order
}

fn handle_module<'a>(
    module: &str,
    context: &'a Context,
    module_list: &BTreeSet<String>,
    timed: Option<&Prefetch>,
) -> Vec<Module<'a>> {
    let mut modules: Vec<Module> = Vec::new();

    if ALL_MODULES.contains(&module) {
        // Write out a module if it isn't disabled
        if !context.is_module_disabled_in_config(module) {
            modules.extend(handle_with_timeout(module, context, timed));
        }
    } else if module.starts_with("custom.") || module.starts_with("env_var.") {
        // custom.<name> and env_var.<name> are special cases and handle disabled modules themselves
        modules.extend(handle_with_timeout(module, context, timed));
    } else if matches!(module, "custom" | "env_var") {
        // env var is a spacial case and may contain a top-level module definition
        if module == "env_var" {
            modules.extend(handle_with_timeout(module, context, timed));
        }

        // Write out all custom modules, except for those that are explicitly set
        modules.extend(
            context
                .config
                .get_config(&[module])
                .and_then(|config| config.as_table().map(toml::map::Map::iter))
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .par_iter()
                .filter_map(|(child, config)| {
                    // Some env var keys may be part of a top-level module definition
                    if module == "env_var" && !config.is_table() {
                        None
                    } else if should_add_implicit_module(module, child, config, module_list) {
                        Some(handle_with_timeout(
                            &format!("{module}.{child}"),
                            context,
                            timed,
                        ))
                    } else {
                        None
                    }
                })
                .flatten()
                .collect::<Vec<Module>>(),
        );
    } else {
        log::debug!(
            "Expected top level format to contain value from {ALL_MODULES:?}. Instead received {module}",
        );
    }

    modules
}

fn should_add_implicit_module(
    parent_module: &str,
    child_module: &str,
    config: &toml::Value,
    module_list: &BTreeSet<String>,
) -> bool {
    let explicit_module_name = format!("{parent_module}.{child_module}");
    let is_explicitly_specified = module_list.contains(&explicit_module_name);

    if is_explicitly_specified {
        // The module is already specified explicitly, so we skip it
        return false;
    }

    let false_value = toml::Value::Boolean(false);

    !config
        .get("disabled")
        .unwrap_or(&false_value)
        .as_bool()
        .unwrap_or(false)
}

pub fn format_duration(duration: &Duration) -> String {
    let milis = duration.as_millis();
    if milis == 0 {
        "<1ms".to_string()
    } else {
        format!("{milis:?}ms")
    }
}

/// Return the modules from $all that are not already in the list
fn all_modules_uniq(module_list: &BTreeSet<String>) -> Vec<String> {
    let mut prompt_order: Vec<String> = Vec::new();
    for module in PROMPT_ORDER {
        if !module_list.contains(*module) {
            prompt_order.push(String::from(*module));
        }
    }

    prompt_order
}

/// Load the correct formatter for the context (ie left prompt or right prompt)
/// and the list of all modules used in a format string
fn load_formatter_and_modules<'a>(context: &'a Context) -> (StringFormatter<'a>, BTreeSet<String>) {
    let config = &context.root_config;

    if context.target == Target::Continuation {
        let cf = &config.continuation_prompt;
        let formatter = StringFormatter::new(cf);
        return match formatter {
            Ok(f) => {
                let modules = f.get_variables().into_iter().collect();
                (f, modules)
            }
            Err(e) => {
                log::error!("Error parsing continuation prompt: {e}");
                (StringFormatter::raw(">"), BTreeSet::new())
            }
        };
    }

    let (left_format_str, right_format_str): (&str, &str) = match context.target {
        Target::Main | Target::Right => (&config.format, &config.right_format),
        Target::Profile(ref name) => {
            if let Some(lf) = config
                .user_profiles
                .get(name)
                .or_else(|| config.internal_profiles.get(name))
            {
                (lf, "")
            } else {
                log::error!("Profile {name:?} not found");
                return (StringFormatter::raw(">"), BTreeSet::new());
            }
        }
        Target::Continuation => unreachable!("Continuation prompt should have been handled above"),
    };

    let lf = StringFormatter::new(left_format_str);
    let rf = StringFormatter::new(right_format_str);

    if let Err(ref e) = lf {
        let name = if let Target::Profile(ref profile_name) = context.target {
            format!("profile.{profile_name}")
        } else {
            "format".to_string()
        };
        log::error!("Error parsing {name:?}: {e}");
    }

    if let Err(ref e) = rf {
        log::error!("Error parsing right_format: {e}");
    }

    let modules = [&lf, &rf]
        .into_iter()
        .flatten()
        .flat_map(VariableHolder::get_variables)
        .collect();

    let main_formatter = match context.target {
        Target::Main | Target::Profile(_) => lf,
        Target::Right => rf,
        Target::Continuation => unreachable!("Continuation prompt should have been handled above"),
    };

    match main_formatter {
        Ok(f) => (f, modules),
        _ => (StringFormatter::raw(">"), BTreeSet::new()),
    }
}

#[cfg(feature = "config-schema")]
pub fn print_schema() {
    let schema = schemars::schema_for!(crate::configs::FullConfig);
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

#[derive(Clone, Debug)]
pub struct Preset(pub &'static str);

impl ValueEnum for Preset {
    fn value_variants<'a>() -> &'a [Self] {
        shadow::get_preset_list()
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0))
    }
}

pub fn preset_command(name: Option<Preset>, output: Option<PathBuf>, force: bool, list: bool) {
    if list {
        println!("{}", preset_list());
        return;
    }
    let variant = name.expect("name argument must be specified");
    let content = shadow::get_preset_content(variant.0);
    if let Some(output) = output {
        if let Err(e) = crate::utils::write_file_atomic(&output, content, force) {
            eprintln!("Error writing preset to {output:?}: {e}");
            std::process::exit(1);
        }
    } else if let Err(err) = std::io::stdout().write_all(content.as_bytes()) {
        eprintln!("Error writing preset to stdout: {err}");
        std::process::exit(1);
    }
}

fn preset_list() -> String {
    Preset::value_variants()
        .iter()
        .fold(String::new(), |mut output, b| {
            let _ = writeln!(output, "{}", b.0);
            output
        })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::default_context;
    use crate::utils;

    const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

    #[test]
    fn a_dumb_terminal_gets_the_standard_fallback() {
        let mut context = default_context();
        context.env.insert("TERM", "dumb".to_owned());

        assert_eq!(
            "Starship disabled due to TERM=dumb > ",
            get_prompt(&context)
        );
    }

    #[test]
    fn main_prompt() {
        let mut context = default_context().set_config(toml::toml! {
                add_newline=false
                format="$character"
                [character]
                format=">\n>"
        });
        context.target = Target::Main;

        let expected = String::from(">\n>");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn right_prompt() {
        let mut context = default_context().set_config(toml::toml! {
                right_format="$character"
                [character]
                format=">\n>"
        });
        context.target = Target::Right;

        let expected = String::from(">>"); // should strip new lines
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn prompt_with_all() -> io::Result<()> {
        let mut context = default_context().set_config(toml::toml! {
                add_newline = false
                right_format= "$directory$line_break"
                format="$all"
                [character]
                format=">"
        });
        context.env.insert("HOME", NULL_DEVICE.to_string());
        let dir = tempfile::tempdir().unwrap();
        context.current_dir = dir.path().to_path_buf();

        let expected = String::from(">");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn rprompt_with_all() -> io::Result<()> {
        let mut context = default_context().set_config(toml::toml! {
            format= "$directory$line_break"
            right_format="$all"
            [character]
            format=">"
        });
        context.env.insert("HOME", NULL_DEVICE.to_string());
        let dir = tempfile::tempdir().unwrap();
        context.current_dir = dir.path().to_path_buf();

        context.target = Target::Right;

        let expected = String::from(">");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn custom_prompt() {
        let mut context = default_context().set_config(toml::toml! {
            add_newline = false
            [profiles]
            test="0_0$character"
            [character]
            format=">>"
        });
        context.target = Target::Profile("test".to_string());

        let expected = String::from("0_0>>");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn custom_prompt_fallback() {
        let mut context = default_context().set_config(toml::toml! {
                add_newline=false
                [profiles]
                test="0_0$character"
                [character]
                format=">>"
        });
        context.target = Target::Profile("wrong_prompt".to_string());

        let expected = String::from(">");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn continuation_prompt() {
        let mut context = default_context().set_config(toml::toml! {
                continuation_prompt="><>"
        });
        context.target = Target::Continuation;

        let expected = String::from("><>");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn preset_list_returns_one_or_more_items() {
        assert!(preset_list().lines().count() > 0);
    }

    #[test]
    fn preset_command_does_not_panic_on_correct_inputs() {
        preset_command(None, None, false, true);
        for v in Preset::value_variants() {
            preset_command(Some(v.clone()), None, false, false);
        }
    }

    #[test]
    fn preset_command_output_to_file() -> std::io::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preset.toml");
        preset_command(
            Some(Preset("nerd-font-symbols")),
            Some(path.clone()),
            false,
            false,
        );

        let actual = utils::read_file(&path)?;
        let expected = include_str!("../docs/public/presets/toml/nerd-font-symbols.toml");
        assert_eq!(actual, expected);
        dir.close()
    }

    #[test]
    fn preset_command_output_existing_file_force() -> io::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preset.toml");
        utils::write_file(&path, "existing content")?;

        preset_command(
            Some(Preset("nerd-font-symbols")),
            Some(path.clone()),
            true,
            false,
        );

        let actual = utils::read_file(&path).unwrap();
        let expected = include_str!("../docs/public/presets/toml/nerd-font-symbols.toml");
        assert_eq!(actual, expected);
        dir.close()
    }

    #[test]
    #[cfg(feature = "config-schema")]
    fn print_schema_does_not_panic() {
        print_schema();
    }

    #[test]
    fn custom_expands() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut context = default_context().set_config(toml::toml! {
                format="$custom"
                [custom.a]
                when=true
                format="a"
                [custom.b]
                when=true
                format="b"
        });
        context.current_dir = dir.path().to_path_buf();

        let expected = String::from("\nab");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn env_expands() {
        let mut context = default_context().set_config(toml::toml! {
                format="$env_var"
                [env_var]
                format="$env_value"
                variable = "a"
                [env_var.b]
                format="$env_value"
                [env_var.c]
                format="$env_value"
        });
        context.env.insert("a", "a".to_string());
        context.env.insert("b", "b".to_string());
        context.env.insert("c", "c".to_string());

        let expected = String::from("\nabc");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn custom_mixed() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut context = default_context().set_config(toml::toml! {
                format="${custom.c}$custom${custom.b}"
                [custom.a]
                when=true
                format="a"
                [custom.b]
                when=true
                format="b"
                [custom.c]
                when=true
                format="c"
        });
        context.current_dir = dir.path().to_path_buf();

        let expected = String::from("\ncab");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn env_mixed() {
        let mut context = default_context().set_config(toml::toml! {
                format="${env_var.c}$env_var${env_var.b}"
                [env_var]
                format="$env_value"
                variable = "d"
                [env_var.a]
                format="$env_value"
                [env_var.b]
                format="$env_value"
                [env_var.c]
                format="$env_value"
        });
        context.env.insert("a", "a".to_string());
        context.env.insert("b", "b".to_string());
        context.env.insert("c", "c".to_string());
        context.env.insert("d", "d".to_string());

        let expected = String::from("\ncdab");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }

    #[test]
    fn custom_subset() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut context = default_context().set_config(toml::toml! {
                format="${custom.b}"
                [custom.a]
                when=true
                format="a"
                [custom.b]
                when=true
                format="b"
        });
        context.current_dir = dir.path().to_path_buf();

        let expected = String::from("\nb");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn custom_missing() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut context = default_context().set_config(toml::toml! {
                format="${custom.b}"
                [custom.a]
                when=true
                format="a"
        });
        context.current_dir = dir.path().to_path_buf();

        let expected = String::from("\n");
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
        dir.close()
    }

    #[test]
    fn test_prefer_user_profile() {
        let mut context = default_context().set_config(toml::toml! {
            add_newline = false
            [profiles]
            claude-code = "user profile"
        });
        context.target = Target::Profile("claude-code".to_string());

        let expected = "user profile";
        let actual = get_prompt(&context);
        assert_eq!(expected, actual);
    }
}

/// Set once any module worker has been abandoned after exceeding `module_timeout`.
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// Owns a `Context` and keeps it alive forever (leaks it) if a module worker was abandoned,
/// because that worker may still hold a reference to the context.
struct ContextGuard<'a>(std::mem::ManuallyDrop<Context<'a>>);

impl<'a> ContextGuard<'a> {
    fn new(context: Context<'a>) -> Self {
        Self(std::mem::ManuallyDrop::new(context))
    }
}

impl<'a> std::ops::Deref for ContextGuard<'a> {
    type Target = Context<'a>;
    fn deref(&self) -> &Context<'a> {
        &self.0
    }
}

impl<'a> std::ops::DerefMut for ContextGuard<'a> {
    fn deref_mut(&mut self) -> &mut Context<'a> {
        &mut self.0
    }
}

impl Drop for ContextGuard<'_> {
    fn drop(&mut self) {
        if !TIMED_OUT.load(Ordering::SeqCst) {
            // SAFETY: no worker was abandoned, so nothing else references the context, and
            // it is dropped exactly once here.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.0) };
        }
    }
}

/// A module computation running on its own dedicated thread.
struct Pending {
    // A one-thread rayon pool, so any parallel iterators the module uses run on that thread
    // and never depend on the (possibly all waiting) global pool.
    pool: rayon::ThreadPool,
    rx: std::sync::mpsc::Receiver<Option<Module<'static>>>,
    deadline: Instant,
}

/// Starts module computations eagerly, each on its own thread, and lets callers wait for them
/// with a deadline measured from when the computation started. Because every module runs
/// concurrently and has its own thread, hung modules neither delay nor starve healthy ones,
/// and the time spent waiting in the pool or queue never counts against a module.
///
/// A `Prefetch` can only be built from a `ContextGuard`, which is what makes lending the
/// context to detached threads sound (see `ContextGuard`).
struct Prefetch<'g> {
    ctx_addr: usize,
    timeout: Duration,
    pending: Mutex<HashMap<String, Pending>>,
    _guard: PhantomData<&'g ()>,
}

impl<'g> Prefetch<'g> {
    fn new(context: &'g ContextGuard) -> Self {
        Self {
            ctx_addr: std::ptr::from_ref::<Context>(context) as usize,
            timeout: Duration::from_millis(context.root_config.module_timeout),
            pending: Mutex::new(HashMap::new()),
            _guard: PhantomData,
        }
    }

    fn enabled(&self) -> bool {
        !self.timeout.is_zero()
    }

    fn spawn(&self, module: &str) -> Option<Pending> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(|_| "starship-module".to_owned())
            .build()
            .ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx_addr = self.ctx_addr;
        let name = module.to_owned();
        pool.spawn(move || {
            // SAFETY: the address comes from a live `ContextGuard` (see `Prefetch::new`). If
            // this worker is abandoned, `TIMED_OUT` is set before the guard is dropped (here
            // or in `Drop for Prefetch`), and the guard then leaks the context, so it stays
            // valid for as long as this worker may run.
            let ctx: &'static Context = unsafe { &*(ctx_addr as *const Context) };
            // The receiver may be gone if we gave up; ignore the error.
            let _ = tx.send(modules::handle(&name, ctx));
        });
        Some(Pending {
            pool,
            rx,
            deadline: Instant::now() + self.timeout,
        })
    }

    fn pending_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `handle_module` would compute this module name.
    fn wanted(context: &Context, module: &str) -> bool {
        if ALL_MODULES.contains(&module) {
            !context.is_module_disabled_in_config(module)
        } else {
            module.starts_with("custom.") || module.starts_with("env_var.")
        }
    }

    /// Start every module of the prompt up front so they all run concurrently.
    fn start_all(&self, context: &Context, modules: &BTreeSet<String>) {
        if !self.enabled() || std::ptr::from_ref(context) as usize != self.ctx_addr {
            return;
        }
        let mut names: Vec<String> = Vec::new();
        for module in modules {
            if module == "all" {
                names.extend(all_modules_uniq(modules));
            } else {
                names.push(module.clone());
            }
        }
        for name in names {
            if !Self::wanted(context, &name) {
                continue;
            }
            if let Some(pending) = self.spawn(&name) {
                self.pending_lock().insert(name, pending);
            }
        }
    }
}

impl Drop for Prefetch<'_> {
    fn drop(&mut self) {
        // Workers that were started but are still running reference the context.
        let pending = std::mem::take(&mut *self.pending_lock());
        for (_, p) in pending {
            if matches!(p.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                TIMED_OUT.store(true, Ordering::SeqCst);
                std::mem::forget(p.pool);
            }
        }
    }
}

/// Like `modules::handle`, but gives up on modules that take longer than `module_timeout`
/// (measured from when the module started running).
///
/// Without a `Prefetch` (callers holding a bare `Context`) the module runs on the current
/// thread with no limit, so the `unsafe` code in `Prefetch` is unreachable for any caller that
/// cannot uphold its invariant.
fn handle_with_timeout<'a>(
    module: &str,
    context: &'a Context,
    prefetch: Option<&Prefetch>,
) -> Option<Module<'a>> {
    let Some(prefetch) =
        prefetch.filter(|p| p.enabled() && std::ptr::from_ref(context) as usize == p.ctx_addr)
    else {
        return modules::handle(module, context);
    };
    let taken = prefetch.pending_lock().remove(module);
    let Some(pending) = taken.or_else(|| prefetch.spawn(module)) else {
        return modules::handle(module, context);
    };
    let remaining = pending.deadline.saturating_duration_since(Instant::now());
    match pending.rx.recv_timeout(remaining) {
        // SAFETY: shortens the lifetime back to the caller's borrow of the context.
        Ok(m) => unsafe { std::mem::transmute::<Option<Module<'static>>, Option<Module<'a>>>(m) },
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            TIMED_OUT.store(true, Ordering::SeqCst);
            // The worker is still running; never tear its pool down underneath it.
            std::mem::forget(pending.pool);
            log::warn!(
                "Module '{module}' did not finish within module_timeout ({}ms) and was skipped. \
                 You can set module_timeout in your config to a higher value (or 0 to disable it).",
                context.root_config.module_timeout
            );
            None
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    fn slow_config(timeout: u64) -> toml::Table {
        toml::toml! {
            command_timeout = 100000
            format = "${custom.slow}${custom.fast}"
            [custom.slow]
            command = "sleep 3; echo slow"
            format = "$output"
            shell = ["sh"]
            when = true
            [custom.fast]
            command = "echo fast"
            format = "$output"
            shell = ["sh"]
            when = true
        }
        .into_iter()
        .chain([(
            "module_timeout".to_owned(),
            toml::Value::Integer(timeout as i64),
        )])
        .collect()
    }

    fn context_with(config: toml::Table, dir: &std::path::Path) -> ContextGuard<'static> {
        ContextGuard::new(Context::from(
            crate::test::ModuleRenderer::new("custom.fast")
                .path(dir)
                .config(config),
        ))
    }

    #[test]
    fn module_timeout_config_default_and_user_value() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(toml::Table::new(), dir.path());
        assert_eq!(ctx.root_config.module_timeout, 5000);
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(slow_config(123), dir.path());
        assert_eq!(ctx.root_config.module_timeout, 123);
    }

    #[test]
    fn fast_module_is_unaffected_by_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(slow_config(5000), dir.path());
        let out = get_module_guarded("custom.fast", &ctx).unwrap();
        assert!(out.contains("fast"), "{out:?}");
    }

    #[test]
    fn slow_module_is_dropped_and_rest_of_prompt_renders() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(slow_config(100), dir.path());
        let start = std::time::Instant::now();
        assert_eq!(get_module_guarded("custom.slow", &ctx), None);
        let prompt = get_prompt_guarded(&ctx);
        assert!(start.elapsed() < Duration::from_millis(2500));
        assert!(prompt.contains("fast"), "{prompt:?}");
        assert!(!prompt.contains("slow"), "{prompt:?}");
        // The abandoned worker still references the context: the guard must leak it.
        assert!(TIMED_OUT.load(Ordering::SeqCst));
    }

    #[test]
    fn zero_module_timeout_disables_limit() {
        let mut config = slow_config(0);
        config.insert("command_timeout".into(), toml::Value::Integer(100000));
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(config, dir.path());
        let out = get_module_guarded("custom.fast", &ctx).unwrap();
        assert!(out.contains("fast"));
    }

    #[test]
    fn many_hung_modules_do_not_starve_healthy_ones() {
        let n = 24;
        let mut text = String::from("command_timeout = 100000\nmodule_timeout = 300\nformat = \"");
        for i in 0..n {
            text.push_str(&format!("${{custom.s{i}}}"));
        }
        text.push_str("${custom.fast}\"\n");
        for i in 0..n {
            text.push_str(&format!(
                "[custom.s{i}]\ncommand = \"sleep 3; echo slow\"\nformat = \"$output\"\nshell = [\"sh\"]\nwhen = true\n"
            ));
        }
        text.push_str(
            "[custom.fast]\ncommand = \"echo fast\"\nformat = \"$output\"\nshell = [\"sh\"]\nwhen = true\n",
        );
        let config: toml::Table = text.parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = context_with(config, dir.path());
        let start = std::time::Instant::now();
        let prompt = get_prompt_guarded(&ctx);
        assert!(
            start.elapsed() < Duration::from_millis(2500),
            "{:?}",
            start.elapsed()
        );
        assert!(prompt.contains("fast"), "{prompt:?}");
        assert!(!prompt.contains("slow"), "{prompt:?}");
    }
}
