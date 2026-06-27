use clap_mangen::Man;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "man".to_string()));
    fs::create_dir_all(&out_dir).expect("create output dir");

    let cmd = gait_bin::cli();
    generate(&cmd, &out_dir, None);
}

fn generate(cmd: &clap::Command, out_dir: &PathBuf, parent: Option<&str>) {
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
        generate(sub, out_dir, Some(&qualified));
    }
}
