use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

pub fn new_session_id() -> String {
    format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8])
}

pub fn run_chat(workspace: PathBuf, codebase: PathBuf) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gait/chat_history");

    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    let mut session_id = new_session_id();
    eprintln!("gait  session {}  (ctrl+d to exit, /new for fresh session)", session_id);

    loop {
        let readline = rl.readline("> ");
        let line = match readline {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };

        let line = line.trim().to_string();
        if line.is_empty() { continue; }
        let _ = rl.add_history_entry(&line);

        match line.as_str() {
            "/new" => {
                session_id = new_session_id();
                eprintln!("new session {}", session_id);
                continue;
            }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let mut child = Command::new(&gait_bin)
            .args([
                "run",
                "--session", &session_id,
                "--task", &line,
                "--workspace", workspace.to_str().unwrap_or("."),
                "--codebase", codebase.to_str().unwrap_or("."),
                "--auto",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdout = child.stdout.take().expect("piped stdout");
        let reader = BufReader::new(stdout);
        let mut renderer = crate::render::Renderer::new();

        for json_line in reader.lines() {
            renderer.handle_line(&json_line?);
        }
        renderer.flush();
        println!();

        child.wait()?;
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_session_id_has_expected_format() {
        let id = new_session_id();
        assert!(id.starts_with('s'), "got: {}", id);
        assert_eq!(id.len(), 9, "got: {}", id);
    }
}
