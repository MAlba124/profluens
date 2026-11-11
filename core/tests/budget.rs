use std::path::{Path, PathBuf};

const BUDGET: usize = 23_100;

fn code_lines(text: &str) -> usize {
    let is_test_start = |l: &&str| l.starts_with("#[cfg(test)]") || l.starts_with("#[cfg(all(test");
    text.lines().take_while(|l| !is_test_start(l)).count()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn code_lines_stop_at_the_test_module() {
    assert_eq!(code_lines("a\nb\n#[cfg(test)]\nmod tests {}\n"), 2);
    assert_eq!(code_lines("a\n#[cfg(all(test, not(loom)))]\nmod tests {}\n"), 1);
    assert_eq!(code_lines("a\n    #[cfg(test)]\nb\n"), 3, "only column 0 starts a test module");
}

#[test]
fn core_src_stays_within_its_line_budget() {
    let mut files = Vec::new();
    rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 10, "found only {} source files", files.len());
    let total: usize = files
        .iter()
        .map(|f| code_lines(&std::fs::read_to_string(f).expect("readable source file")))
        .sum();
    println!("core/src is {total} lines outside test modules, budget {BUDGET}");
    assert!(total <= BUDGET, "core/src is {total} lines outside test modules, budget {BUDGET}");
}
