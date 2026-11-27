use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct InputFile(PathBuf);

impl InputFile {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "agent-hwp-fuzz-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            match std::fs::File::create_new(&path) {
                Ok(_) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create fuzz staging file: {error}"),
            }
        }
    }
}

impl Drop for InputFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

thread_local! {
    static INPUT: InputFile = InputFile::new();
}

pub fn with_input(data: &[u8], read: impl FnOnce(&std::path::Path)) {
    INPUT.with(|file| {
        std::fs::write(&file.0, data).expect("write fuzz staging file");
        read(&file.0);
    });
}
