// Own test binary: `init` sets the global subscriber once per process.
#[test]
fn writes_to_log_dir() {
    let dir = std::env::temp_dir().join(format!("tommybot-log-{}", std::process::id()));
    std::env::set_var("LOG_DIR", &dir);
    let guard = tommybot_core::log::init("test", "info".into());
    tracing::info!("hello file");
    drop(guard);
    let text: String = std::fs::read_dir(&dir)
        .unwrap()
        .map(|f| std::fs::read_to_string(f.unwrap().path()).unwrap())
        .collect();
    std::fs::remove_dir_all(&dir).ok();
    assert!(text.contains("hello file"), "{text}");
    assert!(!text.contains('\x1b'), "no ANSI colours in the file");
}
