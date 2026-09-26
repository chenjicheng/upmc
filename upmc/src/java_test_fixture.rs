//! Real Java fixture: exercise the native launcher and record actual output paths.
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub(crate) fn jar_bytes() -> &'static [u8] {
    static JAR: OnceLock<Vec<u8>> = OnceLock::new();
    JAR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("PathProbe.java"),
            include_str!("../tests/PathProbe.java"),
        )
        .unwrap();
        let java =
            crate::config::find_java().expect("path tests require a JDK (CI installs Java 21)");
        let javac = java.with_file_name("javac.exe");
        let output = run(Command::new(javac)
            .args([
                "-encoding",
                "UTF-8",
                "-source",
                "8",
                "-target",
                "8",
                "PathProbe.java",
            ])
            .current_dir(dir.path()));
        assert!(
            output.status.success(),
            "javac: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        zip.write_all(b"Manifest-Version: 1.0\r\nMain-Class: PathProbe\r\n\r\n")
            .unwrap();
        zip.start_file("PathProbe.class", options).unwrap();
        zip.write_all(&std::fs::read(dir.path().join("PathProbe.class")).unwrap())
            .unwrap();
        zip.finish().unwrap().into_inner()
    })
}

/// Files avoid waiting for inherited pipe handles after the direct child exits.
pub(crate) fn run(command: &mut Command) -> std::process::Output {
    use std::time::{Duration, Instant};
    let capture = tempfile::tempdir().unwrap();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .creation_flags(crate::config::CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("terminate timed-out test child");
            let stop_deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                assert!(
                    Instant::now() < stop_deadline,
                    "test child termination unconfirmed"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("test child exceeded 30 seconds: {command:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    std::process::Output {
        status,
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read(stderr).unwrap(),
    }
}

pub(crate) fn install_root(parent: &Path) -> PathBuf {
    // Chinese + Arabic + spaces: at least one script is outside common Windows ANSI codepages.
    let root = parent
        .join("Administrator")
        .join("Documents")
        .join("CJC整合包 العربية 空格");
    let vanilla = root.join(".minecraft/versions/1.21.11");
    std::fs::create_dir_all(&vanilla).unwrap();
    std::fs::write(vanilla.join("1.21.11.json"), b"{}").unwrap();
    std::fs::write(vanilla.join("1.21.11.jar"), b"synthetic vanilla fixture").unwrap();
    std::fs::create_dir_all(root.join("updater")).unwrap();
    root
}
