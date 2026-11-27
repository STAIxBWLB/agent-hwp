pub fn replay(target: &str, exercise: impl Fn(&[u8])) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
    let mut seeds = 0;
    for dataset in ["corpus", "artifacts"] {
        let directory = root.join(dataset).join(target);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if dataset == "artifacts" && error.kind() == std::io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(error) => panic!("cannot read {}: {error}", directory.display()),
        };
        let mut files: Vec<_> = entries.map(|entry| entry.expect("corpus entry")).collect();
        files.sort_by_key(|entry| entry.file_name());
        for entry in files {
            if entry.file_type().expect("corpus file type").is_file() {
                let name = entry.file_name();
                let prefix = if dataset == "corpus" {
                    "seed-"
                } else {
                    "regression-"
                };
                if !name.to_string_lossy().starts_with(prefix) {
                    continue;
                }
                seeds += usize::from(dataset == "corpus");
                exercise(&std::fs::read(entry.path()).expect("read corpus sample"));
            }
        }
    }
    assert!(seeds > 0, "missing synthetic seeds for {target}");
}
