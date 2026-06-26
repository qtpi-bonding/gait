//! Single source of truth for REPL slash-commands — drives dispatch, help,
//! completion, and highlighting so they cannot drift.

pub struct ReplCommand {
    pub slug: &'static str,
    pub aliases: &'static [&'static str],
    pub desc: &'static str,
    pub group: &'static str,
}

pub const COMMANDS: &[ReplCommand] = &[
    ReplCommand {
        slug: "/help",
        aliases: &["/?"],
        desc: "Show this command guide",
        group: "general",
    },
    ReplCommand {
        slug: "/info",
        aliases: &[],
        desc: "Model, session, gait-dir, codebase",
        group: "general",
    },
    ReplCommand {
        slug: "/model",
        aliases: &[],
        desc: "Show or switch model (provider/id)",
        group: "general",
    },
    ReplCommand {
        slug: "/sh",
        aliases: &["/e"],
        desc: "One-off: describe a command, optionally run it",
        group: "shell",
    },
    ReplCommand {
        slug: "/new",
        aliases: &[],
        desc: "Start a fresh session",
        group: "session",
    },
    ReplCommand {
        slug: "/workflow",
        aliases: &[],
        desc: "Run a workflow",
        group: "session",
    },
    ReplCommand {
        slug: "/undo",
        aliases: &[],
        desc: "Undo the last turn",
        group: "undo",
    },
    ReplCommand {
        slug: "/uc",
        aliases: &[],
        desc: "Undo checkpoint",
        group: "undo",
    },
    ReplCommand {
        slug: "/ut",
        aliases: &[],
        desc: "Undo turn",
        group: "undo",
    },
    ReplCommand {
        slug: "/sandbox",
        aliases: &[],
        desc: "Toggle the sandbox",
        group: "session",
    },
    ReplCommand {
        slug: "/unlock",
        aliases: &[],
        desc: "Unlock a held session",
        group: "session",
    },
    ReplCommand {
        slug: "/exit",
        aliases: &["/quit"],
        desc: "Exit the REPL",
        group: "general",
    },
];

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
            "/model",
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
}
