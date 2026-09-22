//! Recording what was actually run.
//!
//! Every external tool this crate drives goes through a `Command` somewhere, and
//! when a conversion fails or a remote call takes a minute the first question is
//! always "what exactly did it run". These helpers render a command the way a
//! shell would show it so the answer is in the log rather than reconstructed
//! from the code.

use std::ffi::OsStr;
use std::process::Command;

/// Stand-in for a value that must not reach the log.
const REDACTED: &str = "<redacted>";

/// Renders a command as a copy-pasteable line, with secrets removed.
///
/// Arguments containing whitespace are quoted so the word count in the log
/// matches the word count the process saw.
pub fn describe(cmd: &Command) -> String {
    let mut out = quote(cmd.get_program());
    let mut redact_next = false;
    for arg in cmd.get_args() {
        out.push(' ');
        if std::mem::take(&mut redact_next) {
            out.push_str(REDACTED);
            continue;
        }
        let text = arg.to_string_lossy();
        // `rclone rcd` takes the control-API password on its command line, so
        // rendering the arguments verbatim would put it in every debug log.
        redact_next = is_secret_flag(&text);
        out.push_str(&quote(arg));
    }
    out
}

/// Whether the value *after* this flag is a secret.
///
/// Matched by substring rather than an exact list: a flag nobody thought to
/// enumerate is better over-redacted than leaked.
fn is_secret_flag(arg: &str) -> bool {
    if !arg.starts_with('-') {
        return false;
    }
    let lower = arg.to_ascii_lowercase();
    ["pass", "secret", "token", "key"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn quote(text: &OsStr) -> String {
    let text = text.to_string_lossy();
    if text.is_empty() || text.contains(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_command_as_a_shell_line() {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-i"]).arg("/tmp/in.mp4");
        assert_eq!(describe(&cmd), "ffmpeg -v error -i /tmp/in.mp4");
    }

    #[test]
    fn quotes_arguments_with_spaces() {
        let mut cmd = Command::new("cjxl");
        cmd.arg("/tmp/holiday photo.jpg").arg("");
        assert_eq!(describe(&cmd), "cjxl \"/tmp/holiday photo.jpg\" \"\"");
    }

    /// The rc password is passed on the command line, so this is the difference
    /// between a debug log being shareable and not.
    #[test]
    fn the_rc_password_never_reaches_the_log() {
        let mut cmd = Command::new("rclone");
        cmd.args([
            "rcd",
            "--fast-list",
            "--rc-addr",
            "127.0.0.1:5572",
            "--rc-user",
            "verified-recompress",
            "--rc-pass",
            "hunter2hunter2hunter2hunter2hunt",
        ]);
        let line = describe(&cmd);
        assert!(!line.contains("hunter2"), "{line}");
        assert!(line.ends_with("--rc-pass <redacted>"), "{line}");
        // Everything that is not a secret still has to be there, or the log
        // stops answering the question it exists for.
        assert!(line.contains("--fast-list"), "{line}");
        assert!(line.contains("127.0.0.1:5572"), "{line}");
        assert!(line.contains("verified-recompress"), "{line}");
    }

    #[test]
    fn redacts_by_shape_not_by_a_fixed_list() {
        for flag in ["--pass", "--password", "--api-key", "--token", "--SECRET"] {
            let mut cmd = Command::new("tool");
            cmd.arg(flag).arg("value");
            assert!(
                describe(&cmd).ends_with(REDACTED),
                "{flag} should redact what follows it"
            );
        }
    }

    /// Only the value after a *flag* is hidden. A path that happens to contain
    /// "key" is not a secret, and blanking it would make the log misleading.
    #[test]
    fn a_plain_argument_is_not_treated_as_a_flag() {
        let mut cmd = Command::new("cjxl");
        cmd.arg("/photos/keys.jpg").arg("/out/keys.jxl");
        assert_eq!(describe(&cmd), "cjxl /photos/keys.jpg /out/keys.jxl");
    }
}
