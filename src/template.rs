use std::path::Path;

pub fn template_dir() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("PRIOSUN_PATH") {
        return Path::new(&root).join("templates");
    }
    // Fallback for development: relative to executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let local = dir.join("../share/priosun/templates");
            if local.exists() {
                return local;
            }
            let dev = dir.join("templates");
            if dev.exists() {
                return dev;
            }
        }
    }
    Path::new("/usr/local/share/priosun/templates").to_path_buf()
}
