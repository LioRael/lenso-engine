// Vendored from lenso-terminal b08c601; contract import, text-only output, and catalog validation adapted.
//! Clap-backed parsing for validated terminal command catalogs.

use std::collections::{BTreeMap, BTreeSet};

use super::command::{
    CommandDefinition, CommandParameter, OptionalValue, OutputFormat, ParameterKind,
};
use clap::{Arg, ArgAction, Command, error::ErrorKind};

const JSON_ARG_ID: &str = "__lenso_output_json";

/// One catalog command parsed into its provider-owned argument envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedCommand {
    pub id: String,
    pub arguments_json: String,
    pub output_format: OutputFormat,
}

/// A parse can intentionally yield help instead of an invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum ParseOutcome {
    NoMatch,
    Help(String),
    Command(ParsedCommand),
}

/// Catalog or command-line error suitable for a process or TUI surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Default)]
struct CommandNode {
    children: BTreeMap<String, CommandNode>,
    definition: Option<usize>,
}

/// Parses already-tokenized process arguments when they target a catalog leaf.
pub fn parse_args(
    catalog: &[CommandDefinition],
    binary_name: &str,
    args: &[String],
) -> Result<ParseOutcome, ParseError> {
    if !matches_catalog_path(catalog, args) {
        return Ok(ParseOutcome::NoMatch);
    }
    let tree = build_tree(catalog)?;
    let command = build_clap_command(binary_name, &tree, catalog)?;
    let command_line = std::iter::once(binary_name.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    let matches = match command.try_get_matches_from(command_line) {
        Ok(matches) => matches,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::DisplayVersion
            ) =>
        {
            return Ok(ParseOutcome::Help(error.to_string()));
        }
        Err(error) => return Err(ParseError::new(error.to_string())),
    };

    let mut path = Vec::new();
    let mut leaf = &matches;
    while let Some((name, subcommand)) = leaf.subcommand() {
        path.push(name.to_owned());
        leaf = subcommand;
    }
    let definition = catalog
        .iter()
        .find(|definition| definition.path == path)
        .ok_or_else(|| ParseError::new("parsed command does not identify a catalog leaf"))?;
    let arguments_json = collect_arguments(definition, leaf)?;
    let output_format = if leaf
        .try_get_one::<bool>(JSON_ARG_ID)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
    {
        OutputFormat::Json
    } else if supports_output(definition, &OutputFormat::Text) {
        OutputFormat::Text
    } else {
        OutputFormat::Json
    };
    Ok(ParseOutcome::Command(ParsedCommand {
        id: definition.id.clone(),
        arguments_json: serde_json::Value::Object(arguments_json).to_string(),
        output_format,
    }))
}

/// Tokenizes a TUI command line with POSIX shell quoting, then parses it.
pub fn parse_line(
    catalog: &[CommandDefinition],
    surface_name: &str,
    line: &str,
) -> Result<ParseOutcome, ParseError> {
    let args = shell_words::split(line)
        .map_err(|error| ParseError::new(format!("invalid command quoting: {error}")))?;
    parse_args(catalog, surface_name, &args)
}

/// Returns true for one complete catalog leaf or an exact catalog group prefix.
///
/// A product may retain static siblings such as `sessions export` next to
/// contributed commands such as `sessions list`, so an unknown child remains a
/// no-match for the caller's parser.
pub fn matches_catalog_path(catalog: &[CommandDefinition], args: &[String]) -> bool {
    let group = if args
        .last()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        &args[..args.len().saturating_sub(1)]
    } else {
        args
    };
    if group.is_empty() {
        return false;
    }
    catalog.iter().any(|definition| {
        (args.len() >= definition.path.len()
            && definition
                .path
                .iter()
                .zip(args)
                .all(|(expected, actual)| expected == actual))
            || (group.len() < definition.path.len()
                && definition
                    .path
                    .iter()
                    .zip(group)
                    .all(|(expected, actual)| expected == actual))
    })
}

fn build_tree(catalog: &[CommandDefinition]) -> Result<CommandNode, ParseError> {
    let mut root = CommandNode::default();
    let mut ids = BTreeSet::new();
    for (index, definition) in catalog.iter().enumerate() {
        if definition.path.is_empty() || !ids.insert(definition.id.as_str()) {
            return Err(ParseError::new(format!(
                "invalid or duplicate terminal command id `{}`",
                definition.id
            )));
        }
        validate_definition(definition)?;
        let mut node = &mut root;
        for segment in &definition.path {
            node = node.children.entry(segment.clone()).or_default();
        }
        if node.definition.replace(index).is_some() {
            return Err(ParseError::new(format!(
                "duplicate terminal command path `{}`",
                definition.path.join(" ")
            )));
        }
    }
    reject_prefix_leaves(&root, &mut Vec::new())?;
    Ok(root)
}

fn validate_definition(definition: &CommandDefinition) -> Result<(), ParseError> {
    let name = |value: &str| {
        !value.is_empty()
            && !value.starts_with('-')
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if definition.path.iter().any(|segment| !name(segment)) || definition.output_formats.is_empty()
    {
        return Err(ParseError::new("invalid command path or output formats"));
    }
    let mut ids = BTreeSet::from([JSON_ARG_ID, "help"]);
    let mut longs = BTreeSet::from(["help"]);
    if supports_output(definition, &OutputFormat::Json) {
        longs.insert("json");
    }
    let mut shorts = BTreeSet::from(["h"]);
    let mut optional_position = false;
    let mut variadic_position = false;
    for parameter in &definition.parameters {
        if !name(&parameter.id) || !ids.insert(&parameter.id) {
            return Err(ParseError::new("invalid or duplicate parameter id"));
        }
        if matches!(parameter.kind, ParameterKind::Positional) {
            if variadic_position || (optional_position && parameter.required) {
                return Err(ParseError::new("invalid positional parameter order"));
            }
            optional_position |= !parameter.required;
            variadic_position |= parameter.multiple;
        } else {
            let long = optional_string(parameter.long.as_ref().map(Option::as_ref))
                .ok_or_else(|| ParseError::new("missing long option"))?;
            if !name(long) || !longs.insert(long) {
                return Err(ParseError::new("invalid or duplicate long option"));
            }
            if let Some(short) = optional_string(parameter.short.as_ref().map(Option::as_ref))
                && (short.len() != 1
                    || !short.chars().all(|c| c.is_ascii_alphanumeric())
                    || !shorts.insert(short))
            {
                return Err(ParseError::new("invalid or duplicate short option"));
            }
            if matches!(parameter.kind, ParameterKind::Flag)
                && (parameter.required || parameter.multiple)
            {
                return Err(ParseError::new("flags cannot be required or repeated"));
            }
        }
    }
    Ok(())
}

fn reject_prefix_leaves(node: &CommandNode, path: &mut Vec<String>) -> Result<(), ParseError> {
    if node.definition.is_some() && !node.children.is_empty() {
        return Err(ParseError::new(format!(
            "terminal command path `{}` is also a command group",
            path.join(" ")
        )));
    }
    for (name, child) in &node.children {
        path.push(name.clone());
        reject_prefix_leaves(child, path)?;
        path.pop();
    }
    Ok(())
}

fn build_clap_command(
    binary_name: &str,
    root: &CommandNode,
    catalog: &[CommandDefinition],
) -> Result<Command, ParseError> {
    let mut command = Command::new(binary_name.to_owned())
        .disable_help_subcommand(true)
        .subcommand_required(true)
        .arg_required_else_help(true);
    for (name, node) in &root.children {
        command = command.subcommand(build_clap_node(name, node, catalog)?);
    }
    Ok(command)
}

fn build_clap_node(
    name: &str,
    node: &CommandNode,
    catalog: &[CommandDefinition],
) -> Result<Command, ParseError> {
    let mut command = Command::new(name.to_owned());
    if let Some(index) = node.definition {
        let definition = &catalog[index];
        command = command
            .about(definition.summary.clone())
            .long_about(definition.description.clone());
        let mut positional_index = 1usize;
        for parameter in &definition.parameters {
            let (arg, consumed_position) = build_arg(parameter, positional_index)?;
            positional_index += usize::from(consumed_position);
            command = command.arg(arg);
        }
        if supports_output(definition, &OutputFormat::Json) {
            command = command.arg(
                Arg::new(JSON_ARG_ID)
                    .long("json")
                    .help("Emit the command result as JSON")
                    .action(ArgAction::SetTrue),
            );
        }
    }
    for (child_name, child) in &node.children {
        command = command.subcommand(build_clap_node(child_name, child, catalog)?);
    }
    if node.definition.is_none() && !node.children.is_empty() {
        command = command
            .subcommand_required(true)
            .arg_required_else_help(true);
    }
    Ok(command)
}

fn build_arg(
    parameter: &CommandParameter,
    positional_index: usize,
) -> Result<(Arg, bool), ParseError> {
    let value_name = optional_string(parameter.value_name.as_ref().map(Option::as_ref))
        .unwrap_or(parameter.id.as_str())
        .to_owned();
    let mut arg = Arg::new(parameter.id.clone())
        .help(parameter.description.clone())
        .required(parameter.required);
    match parameter.kind {
        ParameterKind::Positional => {
            arg = arg.index(positional_index).value_name(value_name);
            if parameter.multiple {
                arg = arg.num_args(1..).action(ArgAction::Append);
            }
            Ok((arg, true))
        }
        ParameterKind::Option => {
            let long =
                optional_string(parameter.long.as_ref().map(Option::as_ref)).ok_or_else(|| {
                    ParseError::new(format!("option `{}` has no long name", parameter.id))
                })?;
            arg = arg.long(long.to_owned()).value_name(value_name);
            if let Some(short) = optional_string(parameter.short.as_ref().map(Option::as_ref)) {
                let short = short
                    .chars()
                    .next()
                    .ok_or_else(|| ParseError::new("empty short option"))?;
                arg = arg.short(short);
            }
            if parameter.multiple {
                arg = arg.action(ArgAction::Append);
            }
            Ok((arg, false))
        }
        ParameterKind::Flag => {
            let long =
                optional_string(parameter.long.as_ref().map(Option::as_ref)).ok_or_else(|| {
                    ParseError::new(format!("flag `{}` has no long name", parameter.id))
                })?;
            arg = arg.long(long.to_owned()).action(ArgAction::SetTrue);
            if let Some(short) = optional_string(parameter.short.as_ref().map(Option::as_ref)) {
                let short = short
                    .chars()
                    .next()
                    .ok_or_else(|| ParseError::new("empty short option"))?;
                arg = arg.short(short);
            }
            Ok((arg, false))
        }
    }
}

fn collect_arguments(
    definition: &CommandDefinition,
    matches: &clap::ArgMatches,
) -> Result<serde_json::Map<String, serde_json::Value>, ParseError> {
    let mut values = serde_json::Map::new();
    for parameter in &definition.parameters {
        match parameter.kind {
            ParameterKind::Flag => {
                if matches.get_flag(&parameter.id) {
                    values.insert(parameter.id.clone(), serde_json::Value::Bool(true));
                }
            }
            ParameterKind::Positional | ParameterKind::Option if parameter.multiple => {
                if let Some(found) = matches.get_many::<String>(&parameter.id) {
                    let found = found.cloned().collect::<Vec<_>>();
                    validate_choices(parameter, &found)?;
                    values.insert(parameter.id.clone(), serde_json::json!(found));
                }
            }
            ParameterKind::Positional | ParameterKind::Option => {
                if let Some(found) = matches.get_one::<String>(&parameter.id) {
                    validate_choices(parameter, std::slice::from_ref(found))?;
                    values.insert(
                        parameter.id.clone(),
                        serde_json::Value::String(found.clone()),
                    );
                }
            }
        }
    }
    Ok(values)
}

fn validate_choices(parameter: &CommandParameter, values: &[String]) -> Result<(), ParseError> {
    if !parameter.choices.is_empty()
        && values
            .iter()
            .any(|value| !parameter.choices.contains(value))
    {
        return Err(ParseError::new(format!(
            "invalid value for `{}`; expected one of {}",
            parameter.id,
            parameter.choices.join(", ")
        )));
    }
    Ok(())
}

fn supports_output(definition: &CommandDefinition, expected: &OutputFormat) -> bool {
    definition.output_formats.iter().any(|format| {
        matches!(
            (format, expected),
            (OutputFormat::Text, OutputFormat::Text) | (OutputFormat::Json, OutputFormat::Json)
        )
    })
}

fn optional_string(value: OptionalValue<&String>) -> Option<&str> {
    value.flatten().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<CommandDefinition> {
        vec![CommandDefinition {
            id: "agent.session.show".to_owned(),
            path: vec!["sessions".to_owned(), "show".to_owned()],
            summary: "Show a session".to_owned(),
            description: "Show one durable session.".to_owned(),
            parameters: vec![
                CommandParameter {
                    id: "session_id".to_owned(),
                    kind: ParameterKind::Positional,
                    long: None,
                    short: None,
                    value_name: Some(Some("SESSION_ID".to_owned())),
                    description: String::new(),
                    required: true,
                    multiple: false,
                    choices: Vec::new(),
                },
                CommandParameter {
                    id: "limit".to_owned(),
                    kind: ParameterKind::Option,
                    long: Some(Some("limit".to_owned())),
                    short: Some(Some("n".to_owned())),
                    value_name: Some(Some("LIMIT".to_owned())),
                    description: String::new(),
                    required: false,
                    multiple: false,
                    choices: Vec::new(),
                },
            ],
            output_formats: vec![OutputFormat::Text, OutputFormat::Json],
        }]
    }

    #[test]
    fn text_only_command_has_no_json_flag() {
        let mut commands = catalog();
        commands[0].output_formats = vec![OutputFormat::Text];
        assert!(matches!(
            parse_line(&commands, "app", "sessions show one").unwrap(),
            ParseOutcome::Command(_)
        ));
    }

    #[test]
    fn invalid_catalog_options_return_errors_instead_of_panicking() {
        let mut commands = catalog();
        commands[0].parameters[1].long = Some(Some("help".into()));
        assert!(parse_line(&commands, "app", "sessions show one").is_err());
    }

    #[test]
    fn leaves_unrelated_cli_commands_for_the_product_parser() {
        let args = vec!["sessions".to_owned(), "export".to_owned()];
        assert_eq!(
            parse_args(&catalog(), "agent", &args).unwrap(),
            ParseOutcome::NoMatch
        );
    }

    #[test]
    fn renders_group_help_without_claiming_static_siblings() {
        let args = ["sessions"].map(str::to_owned);
        let ParseOutcome::Help(help) = parse_args(&catalog(), "agent", &args).unwrap() else {
            panic!("expected catalog group help");
        };
        assert!(help.contains("show"));

        let static_sibling = ["sessions", "export"].map(str::to_owned);
        assert_eq!(
            parse_args(&catalog(), "agent", &static_sibling).unwrap(),
            ParseOutcome::NoMatch
        );
    }

    #[test]
    fn parses_nested_commands_into_provider_owned_json() {
        let args = ["sessions", "show", "s-123", "--limit", "40", "--json"].map(str::to_owned);
        let ParseOutcome::Command(parsed) = parse_args(&catalog(), "agent", &args).unwrap() else {
            panic!("expected a parsed command");
        };
        assert_eq!(parsed.id, "agent.session.show");
        assert_eq!(parsed.output_format, OutputFormat::Json);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed.arguments_json).unwrap(),
            serde_json::json!({"session_id": "s-123", "limit": "40"})
        );
    }

    #[test]
    fn supports_tui_quoting() {
        let parsed = parse_line(&catalog(), "agent", "sessions show 'session one'").unwrap();
        let ParseOutcome::Command(parsed) = parsed else {
            panic!("expected a parsed command");
        };
        assert!(parsed.arguments_json.contains("session one"));
    }

    #[test]
    fn rejects_leaf_group_ambiguity() {
        let mut catalog = catalog();
        let mut parent = catalog[0].clone();
        parent.id = "agent.sessions".to_owned();
        parent.path.pop();
        catalog.push(parent);
        let error = build_tree(&catalog).err().expect("prefix leaf must fail");
        assert!(error.to_string().contains("also a command group"));
    }
}
