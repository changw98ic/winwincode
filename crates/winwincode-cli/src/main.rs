use winwincode_cli::{SystemLocalLauncher, default_state_root, run_cli};

fn main() {
    let state_root = match default_state_root() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("WinWinCode 本地状态目录不可用：{error}");
            std::process::exit(5);
        }
    };
    let launcher = SystemLocalLauncher::new(state_root);
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let outcome = run_cli(&arguments, &launcher);
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    std::process::exit(outcome.code);
}
