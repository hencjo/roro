use std::process::Command;

#[test]
fn help_and_version_work_without_configuration() {
    for flag in ["--help", "-h", "--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_roro"))
            .arg(flag)
            .env_remove("RUST_LOG")
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}: {:?}", output.stderr);
        let stdout = String::from_utf8(output.stdout).unwrap();
        if flag == "--version" || flag == "-V" {
            assert_eq!(stdout, format!("roro {}\n", env!("CARGO_PKG_VERSION")));
        } else {
            assert!(stdout.starts_with("roro —"));
            assert!(stdout.contains("Usage: roro serve --config PATH"));
        }
    }
}
