//! Single source of truth for REPL commands — drives dispatch, help,
//! completion, and highlighting so they cannot drift.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplCommandId {
    Help,
    Info,
    Auth,
    Inference,
    Mode,
    Shell,
    New,
    Workflow,
    Undo,
    UndoCode,
    UndoTranscript,
    Sandbox,
    Unlock,
    Skill,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    pub id: ReplCommandId,
    pub args: &'a str,
}

pub struct ReplCommand {
    pub id: ReplCommandId,
    pub slug: &'static str,
    pub aliases: &'static [&'static str],
    pub desc: &'static str,
    pub group: &'static str,
    pub usage: &'static str,
}

pub const COMMANDS: &[ReplCommand] = &[
    ReplCommand {
        id: ReplCommandId::Help,
        slug: "/help",
        aliases: &["/?"],
        desc: "Show this command guide",
        group: "general",
        usage: "/help",
    },
    ReplCommand {
        id: ReplCommandId::Info,
        slug: "/info",
        aliases: &[],
        desc: "Model, session, gait-dir, codebase",
        group: "general",
        usage: "/info",
    },
    ReplCommand {
        id: ReplCommandId::Auth,
        slug: "/auth",
        aliases: &[],
        desc: "List, inspect, link, or unlink credentials",
        group: "general",
        usage: "/auth [list|status <provider>|link <provider> <source> [--home <path>]|unlink <provider>]",
    },
    ReplCommand {
        id: ReplCommandId::Inference,
        slug: "/inference",
        aliases: &[],
        desc: "List, show, add, select, or clear a named inference",
        group: "general",
        usage: "/inference [name|default|show <name>|add <name> <provider/id>]",
    },
    ReplCommand {
        id: ReplCommandId::Mode,
        slug: "/mode",
        aliases: &[],
        desc: "Show/switch operating mode",
        group: "general",
        usage: "/mode [name]",
    },
    ReplCommand {
        id: ReplCommandId::Shell,
        slug: "/sh",
        aliases: &["/e"],
        desc: "One-off: describe a command, optionally run it",
        group: "shell",
        usage: "/sh <describe a command>",
    },
    ReplCommand {
        id: ReplCommandId::New,
        slug: "/new",
        aliases: &["/n"],
        desc: "Start a fresh session",
        group: "session",
        usage: "/new",
    },
    ReplCommand {
        id: ReplCommandId::Workflow,
        slug: "/workflow",
        aliases: &[],
        desc: "Run a workflow",
        group: "session",
        usage: "/workflow <name> [args...]",
    },
    ReplCommand {
        id: ReplCommandId::Undo,
        slug: "/undo",
        aliases: &["/u"],
        desc: "Undo the last turn",
        group: "undo",
        usage: "/undo [N]",
    },
    ReplCommand {
        id: ReplCommandId::UndoCode,
        slug: "/uc",
        aliases: &[],
        desc: "Undo checkpoint",
        group: "undo",
        usage: "/uc [N]",
    },
    ReplCommand {
        id: ReplCommandId::UndoTranscript,
        slug: "/ut",
        aliases: &[],
        desc: "Undo turn",
        group: "undo",
        usage: "/ut [N]",
    },
    ReplCommand {
        id: ReplCommandId::Sandbox,
        slug: "/sandbox",
        aliases: &[],
        desc: "Toggle the sandbox",
        group: "session",
        usage: "/sandbox [off|on]",
    },
    ReplCommand {
        id: ReplCommandId::Unlock,
        slug: "/unlock",
        aliases: &[],
        desc: "Unlock a held session",
        group: "session",
        usage: "/unlock [once|session|global|agent]",
    },
    ReplCommand {
        id: ReplCommandId::Skill,
        slug: "/skill",
        aliases: &[],
        desc: "List, load, or unload skills/mcp/plugins",
        group: "session",
        usage: "/skill [load|unload <name>]",
    },
    ReplCommand {
        id: ReplCommandId::Exit,
        slug: "/exit",
        aliases: &["/quit", "/q"],
        desc: "Exit the REPL",
        group: "general",
        usage: "/exit",
    },
];

/// Parse a complete REPL command prefix and return its typed command and
/// unparsed arguments. A command name must end at whitespace or the end of the
/// line, so `/modefoo` is not accidentally treated as `/mode foo`.
pub fn parse(line: &str) -> Option<ParsedCommand<'_>> {
    let line = line.trim();
    for command in COMMANDS {
        for name in std::iter::once(command.slug).chain(command.aliases.iter().copied()) {
            if line == name {
                return Some(ParsedCommand {
                    id: command.id,
                    args: "",
                });
            }
            if let Some(args) = line.strip_prefix(name) {
                if args.starts_with(char::is_whitespace) {
                    return Some(ParsedCommand {
                        id: command.id,
                        args: args.trim(),
                    });
                }
            }
        }
    }
    match line {
        "exit" | "quit" => Some(ParsedCommand {
            id: ReplCommandId::Exit,
            args: "",
        }),
        _ => None,
    }
}

/// Slugs + aliases, leading `/` stripped — for the driver's prefix match.
pub fn builtin_prefixes() -> Vec<&'static str> {
    let mut v = Vec::new();
    for c in COMMANDS {
        v.push(c.slug.trim_start_matches('/'));
        for a in c.aliases {
            v.push(a.trim_start_matches('/'));
        }
    }
    v
}

/// `(command, description)` pairs for the completer.
pub fn completion_pairs() -> Vec<(&'static str, &'static str)> {
    COMMANDS.iter().map(|c| (c.slug, c.desc)).collect()
}

/// Grouped, human-readable command guide for `/help`.
pub fn help_text() -> String {
    use std::fmt::Write;
    let mut out = String::from("Commands:\n");
    let mut last_group = "";
    for c in COMMANDS {
        if c.group != last_group {
            let _ = writeln!(out, "  [{}]", c.group);
            last_group = c.group;
        }
        let aliases = if c.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.aliases.join(", "))
        };
        let _ = writeln!(out, "    {}{}  — {}", c.slug, aliases, c.desc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_new_commands() {
        let slugs: Vec<_> = COMMANDS.iter().map(|c| c.slug).collect();
        for s in [
            "/help",
            "/info",
            "/auth",
            "/inference",
            "/sh",
            "/new",
            "/workflow",
            "/undo",
        ] {
            assert!(slugs.contains(&s), "missing {s}");
        }
        assert!(help_text().contains("/help"));
        assert!(builtin_prefixes().contains(&"help"));
    }

    #[test]
    fn parser_resolves_aliases_and_arguments() {
        assert_eq!(
            parse("/e list files"),
            Some(ParsedCommand {
                id: ReplCommandId::Shell,
                args: "list files"
            })
        );
        assert_eq!(
            parse("/workflow smoke arg"),
            Some(ParsedCommand {
                id: ReplCommandId::Workflow,
                args: "smoke arg"
            })
        );
        assert_eq!(
            parse("quit"),
            Some(ParsedCommand {
                id: ReplCommandId::Exit,
                args: ""
            })
        );
    }

    #[test]
    fn parser_requires_command_boundary() {
        assert_eq!(parse("/modefoo"), None);
        assert_eq!(parse("/authorize"), None);
    }
}
