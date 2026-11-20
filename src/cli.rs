use crate::error::{AppError, Result};
use std::path::PathBuf;
#[derive(Debug, PartialEq)]
pub enum OperationMode {
    Serve(ServeOptions),
    Help,
    Version,
}
#[derive(Debug, PartialEq)]
pub struct ServeOptions {
    pub config_path: PathBuf,
}
#[derive(Debug, PartialEq)]
pub struct CliOptions {
    pub mode: OperationMode,
}
pub const HELP: &str = "roro — disk-backed OCI registry\n\nUsage: roro serve --config PATH\n       roro --help | --version\n\nAll image pulls are anonymous. Writes require GitLab CI identity.\n";
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<CliOptions> {
    let args: Vec<_> = args.into_iter().collect();
    let mode = if args.iter().any(|a| a == "--help" || a == "-h") {
        OperationMode::Help
    } else if args.iter().any(|a| a == "--version" || a == "-V") {
        OperationMode::Version
    } else if args.len() == 3
        && args[0] == "serve"
        && args[1] == "--config"
        && !args[2].trim().is_empty()
        && !args[2].starts_with('-')
    {
        OperationMode::Serve(ServeOptions {
            config_path: args[2].clone().into(),
        })
    } else {
        return Err(AppError::bad(
            "ARGUMENT_INVALID",
            "Expected serve --config PATH. Unknown argument or missing value. Use --help for usage.",
        ));
    };
    Ok(CliOptions { mode })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsing() {
        assert_eq!(
            parse(vec!["--help".into()]).unwrap().mode,
            OperationMode::Help
        );
        assert_eq!(
            parse(vec!["--version".into()]).unwrap().mode,
            OperationMode::Version
        );
        for args in [
            vec![],
            vec!["serve"],
            vec!["--config", "x", "serve"],
            vec!["serve", "--config", ""],
        ] {
            assert!(parse(args.into_iter().map(String::from)).is_err());
        }
        assert!(parse(["serve", "--config", "x"].map(String::from)).is_ok());
    }
}
