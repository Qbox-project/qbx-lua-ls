/// `qbx-lua-ls --index <dir>` indexes a folder once and reports what it found, for benchmarking.
fn print_index_stats(dir: std::path::PathBuf) {
    let mut workspace = qbx_lua_ls::workspace::Workspace::default();
    workspace.roots = vec![std::path::absolute(&dir).unwrap_or(dir)];
    workspace.load_stubs();
    let stats = workspace.scan();
    let (mut globals, mut members, mut classes, mut events) = (0, 0, 0, 0);
    for (_, file) in workspace.index.files() {
        globals += file.index.globals.len();
        members += file.index.members.len();
        classes += file.index.classes.len() + file.index.aliases.len();
        events += file.index.events.len();
    }
    println!(
        "{} files, {} resources in {} ms: {globals} globals, {members} members, {classes} types, {events} events",
        stats.files, stats.resources, stats.millis
    );
}

fn main() {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("qbx-lua-ls {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some(dir) = std::env::args().skip_while(|arg| arg != "--index").nth(1) {
        print_index_stats(dir.into());
        return;
    }
    // Deeply nested Lua is walked recursively; the default main-thread stack is too small for that on Windows.
    let server = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(qbx_lua_ls::server::run);
    let outcome = server.expect("failed to start the server thread").join();
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("qbx-lua-ls: {error}");
            std::process::exit(1);
        }
        Err(_) => std::process::exit(101),
    }
}
