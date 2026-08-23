#[path = "../raw_exec.rs"]
mod raw_exec;

use std::ffi::{OsStr, OsString};
use std::path::Path;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some(OsStr::new("--version")) {
        println!("agsh-exec-helper {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if first.as_deref() != Some(OsStr::new(raw_exec::INTERNAL_EXEC_HELPER_FLAG))
        || arguments.next().as_deref() != Some(OsStr::new("--"))
    {
        eprintln!("agsh-exec-helper: invalid internal invocation");
        std::process::exit(2);
    }
    let argv: Vec<OsString> = arguments.collect();
    let error = raw_exec::execve_with_text_fallback(&argv);
    report_exec_failure(&argv, &error);
}

fn report_exec_failure(argv: &[OsString], error: &std::io::Error) -> ! {
    let name = argv
        .first()
        .map(|program| Path::new(program).display().to_string())
        .unwrap_or_else(|| "exec".to_string());
    eprintln!("agsh: exec: {name}: {error}");
    let disappeared = argv.first().is_some_and(|path| {
        matches!(
            std::fs::metadata(path),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                )
        )
    });
    std::process::exit(if disappeared { 127 } else { 126 });
}
