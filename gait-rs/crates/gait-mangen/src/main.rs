use clap_mangen::Man;
use gait_mangen::render_default_skill;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "man".to_string()));
    let skill_path = args.next().map(PathBuf::from);

    let cmd = gait_bin::cli();
    generate_man(&cmd, &out_dir, None);

    if let Some(path) = skill_path {
        let skill = render_default_skill().expect("render gait-manual");
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).expect("create skill parent directory");
        }
        fs::write(&path, skill).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        println!("wrote {}", path.display());
    }
}

fn generate_man(cmd: &clap::Command, out_dir: &PathBuf, parent: Option<&str>) {
    fs::create_dir_all(out_dir).expect("create output dir");
    let qualified = match parent {
        Some(p) => format!("{}-{}", p, cmd.get_name()),
        None => cmd.get_name().to_string(),
    };

    let mut buf = Vec::new();
    Man::new(cmd.clone())
        .render(&mut buf)
        .expect("render man page");
    let path = out_dir.join(format!("{}.1", qualified));
    fs::write(&path, &buf).unwrap_or_else(|e| panic!("write {}: {}", path.display(), e));
    println!("wrote {}", path.display());

    for sub in cmd.get_subcommands() {
        generate_man(sub, out_dir, Some(&qualified));
    }
}
