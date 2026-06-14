//! gait run command (extracted from bin/gait.rs).
#[derive(Default, Debug)]
pub struct RunArgs {
    pub session_id: Option<String>, pub task: Option<String>, pub auto: bool,
    pub model_override: Option<String>, pub agent_name: Option<String>,
    pub result_schema_path: Option<String>, pub allow: Vec<String>, pub deny: Vec<String>,
    pub max_iter: Option<usize>,
}
impl RunArgs {
    pub fn parse(argv: &[String]) -> Self {
        let mut a = RunArgs::default(); let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--session" => { i += 1; a.session_id = argv.get(i).cloned(); }
                "--task" => { i += 1; a.task = argv.get(i).cloned(); }
                "--model" => { i += 1; a.model_override = argv.get(i).cloned(); }
                "--auto" => { a.auto = true; }
                "--agent" => { i += 1; a.agent_name = argv.get(i).cloned(); }
                "--result-schema" => { i += 1; a.result_schema_path = argv.get(i).cloned(); }
                "--allow" => { i += 1; if let Some(p) = argv.get(i) { a.allow.push(p.clone()); } }
                "--deny" => { i += 1; if let Some(p) = argv.get(i) { a.deny.push(p.clone()); } }
                "--max-iter" => { i += 1; a.max_iter = argv.get(i).and_then(|s| s.parse().ok()); }
                "--gait-dir" | "--codebase" => { i += 1; }
                _ => {}
            }
            i += 1;
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = ["--task","hi","--auto","--model","x/y","--max-iter","7"].iter().map(|s| s.to_string()).collect();
        let a = RunArgs::parse(&argv);
        assert_eq!(a.task.as_deref(), Some("hi")); assert!(a.auto);
        assert_eq!(a.model_override.as_deref(), Some("x/y")); assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() { assert!(RunArgs::parse(&["--auto".to_string()]).task.is_none()); }
}
