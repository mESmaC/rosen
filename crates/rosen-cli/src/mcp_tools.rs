//! The tools `rosen mcp` offers, read from the command definitions.
//!
//! Each tool is one `rosen` command. Its name, description and arguments
//! come from the same clap definitions `--help` prints, so a command gained
//! or changed in `commands.rs` is a tool gained or changed with no edit here.

use clap::{Arg, ArgAction, CommandFactory};
use serde_json::{json, Map, Value};

use crate::commands::Cli;

/// Commands that are never tools.
/// - `mcp` is this server.
/// - `wait`, `events` and `workspace watch` run until something happens
///   elsewhere, and a tool call has to return.
/// - `import` and `server start` are the operator's: the host that starts
///   this server has already started the project's server.
const NOT_TOOLS: [&str; 6] = [
    "mcp",
    "wait",
    "events",
    "workspace watch",
    "import",
    "server start",
];

/// Enrollment is the operator's too, and `machine key create` prints a
/// secret, so no `machine` command is a tool.
const NOT_TOOLS_UNDER: &str = "machine ";

/// A reviewer's or checker's commands, offered only when asked for
/// (`rosen mcp --landing`): an author's agent has no use for them.
const LANDING: [&str; 3] = ["review approve", "receipt submit", "land"];

/// One `rosen` command as a tool.
pub struct Tool {
    /// The command's words joined by `_`, such as `change_new`.
    pub name: String,
    /// The command's words, such as `change new`.
    words: Vec<String>,
    description: String,
    inputs: Vec<Input>,
}

/// One argument of a command.
struct Input {
    name: String,
    /// The option's long name, or `None` for a positional argument.
    option: Option<String>,
    /// A positional argument's place among the positionals.
    index: usize,
    kind: Kind,
    required: bool,
    help: String,
    choices: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A flag with no value.
    Switch,
    One,
    Many,
}

/// Every tool this server offers, in the order `rosen --help` lists them.
pub fn tools(landing: bool) -> Vec<Tool> {
    let mut root = Cli::command();
    // Fills in what the derive leaves for later, such as positional indexes.
    root.build();
    let mut found = Vec::new();
    collect(&root, &mut Vec::new(), &mut found);
    found.retain(|tool| offered(&tool.words.join(" "), landing));
    found
}

fn offered(command: &str, landing: bool) -> bool {
    let never = NOT_TOOLS.contains(&command) || command.starts_with(NOT_TOOLS_UNDER);
    !never && (landing || !LANDING.contains(&command))
}

/// Adds a tool for each command under `command` that has no subcommands.
fn collect(command: &clap::Command, words: &mut Vec<String>, found: &mut Vec<Tool>) {
    let subcommands: Vec<&clap::Command> = command
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help")
        .collect();
    if subcommands.is_empty() {
        if !words.is_empty() {
            found.push(Tool::of(command, words));
        }
        return;
    }
    for sub in subcommands {
        words.push(sub.get_name().to_owned());
        collect(sub, words, found);
        words.pop();
    }
}

impl Tool {
    fn of(command: &clap::Command, words: &[String]) -> Tool {
        let about = command.get_long_about().or_else(|| command.get_about());
        let mut inputs: Vec<Input> = command.get_arguments().filter_map(Input::of).collect();
        inputs.sort_by_key(|input| input.index);
        Tool {
            name: words.join("_"),
            words: words.to_vec(),
            description: about.map(ToString::to_string).unwrap_or_default(),
            inputs,
        }
    }

    /// The tool as `tools/list` describes it.
    pub fn listing(&self) -> Value {
        let properties: Map<String, Value> = self
            .inputs
            .iter()
            .map(|input| (input.name.clone(), input.schema()))
            .collect();
        let required: Vec<&str> = self
            .inputs
            .iter()
            .filter(|input| input.required)
            .map(|input| input.name.as_str())
            .collect();
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        })
    }

    /// The words after `rosen` that run this tool with `arguments`. Options
    /// are written `--name=value` and positional values follow `--`, so no
    /// value is ever read as an option.
    pub fn command_line(&self, arguments: &Value) -> Result<Vec<String>, String> {
        let empty = Map::new();
        let given = match arguments {
            Value::Null => &empty,
            Value::Object(given) => given,
            _ => return Err("the arguments must be an object".to_owned()),
        };
        if let Some(unknown) = given.keys().find(|key| self.input(key).is_none()) {
            let known: Vec<&str> = self.inputs.iter().map(|i| i.name.as_str()).collect();
            return Err(format!(
                "`{}` has no argument `{unknown}`. Its arguments: {}",
                self.name,
                known.join(", ")
            ));
        }
        let mut words = self.words.clone();
        let mut positional = Vec::new();
        for input in &self.inputs {
            let Some(value) = given.get(&input.name).filter(|value| !value.is_null()) else {
                continue;
            };
            let values = input.values(value)?;
            match &input.option {
                Some(_) => words.extend(values),
                None => positional.extend(values),
            }
        }
        if !positional.is_empty() {
            words.push("--".to_owned());
            words.extend(positional);
        }
        Ok(words)
    }

    fn input(&self, name: &str) -> Option<&Input> {
        self.inputs.iter().find(|input| input.name == name)
    }
}

impl Input {
    /// `None` for an argument a tool call cannot set: the global options,
    /// which the server passes itself, and help.
    fn of(arg: &Arg) -> Option<Input> {
        let name = arg.get_id().as_str();
        if arg.is_global_set() || arg.is_hide_set() || matches!(name, "help" | "version") {
            return None;
        }
        let option = arg.get_long().map(str::to_owned);
        if option.is_none() && !arg.is_positional() {
            return None;
        }
        let kind = match arg.get_action() {
            ArgAction::SetTrue | ArgAction::SetFalse => Kind::Switch,
            ArgAction::Append => Kind::Many,
            _ => Kind::One,
        };
        let help = arg.get_long_help().or_else(|| arg.get_help());
        let choices = match kind {
            Kind::Switch => Vec::new(),
            Kind::One | Kind::Many => arg
                .get_possible_values()
                .iter()
                .map(|choice| choice.get_name().to_owned())
                .collect(),
        };
        Some(Input {
            name: name.to_owned(),
            option,
            index: arg.get_index().unwrap_or(0),
            kind,
            required: arg.is_required_set(),
            help: help.map(ToString::to_string).unwrap_or_default(),
            choices,
        })
    }

    fn schema(&self) -> Value {
        let mut one = json!({ "type": "string" });
        if !self.choices.is_empty() {
            one["enum"] = json!(self.choices);
        }
        let mut schema = match self.kind {
            Kind::Switch => json!({ "type": "boolean" }),
            Kind::One => one,
            Kind::Many => json!({ "type": "array", "items": one }),
        };
        if !self.help.is_empty() {
            schema["description"] = json!(self.help);
        }
        schema
    }

    /// The command-line words that give this argument `value`.
    fn values(&self, value: &Value) -> Result<Vec<String>, String> {
        let wrong = |expected: &str| format!("`{}` must be {expected}", self.name);
        let texts = match (self.kind, value) {
            (Kind::Switch, Value::Bool(false)) => return Ok(Vec::new()),
            (Kind::Switch, Value::Bool(true)) => {
                return Ok(self.option.iter().map(|name| format!("--{name}")).collect());
            }
            (Kind::Switch, _) => return Err(wrong("true or false")),
            (Kind::Many, Value::Array(items)) => items
                .iter()
                .map(|item| text(item).ok_or_else(|| wrong("a list of strings")))
                .collect::<Result<Vec<String>, String>>()?,
            (_, value) => vec![text(value).ok_or_else(|| wrong("a string"))?],
        };
        Ok(match &self.option {
            Some(name) => texts
                .iter()
                .map(|text| format!("--{name}={text}"))
                .collect(),
            None => texts,
        })
    }
}

/// A string, or a number written as the command line would take it.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}
