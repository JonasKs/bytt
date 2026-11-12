//! The macOS Keychain, through `/usr/bin/security`.
//!
//! Claude Code reads and writes its login with the `security` tool, so the item's
//! access list names that binary. Going through the same binary means no "bytt wants
//! to use your keychain" prompt, now or after an upgrade replaces the bytt binary.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// A place to keep secrets, keyed by service and account.
pub trait Secrets: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

/// Absolute path: a `security` earlier on `PATH` must not get to see a login.
const SECURITY: &str = "/usr/bin/security";
/// Exit code of `security` for "no such item".
const NOT_FOUND: i32 = 44;
/// A healthy Keychain answers in milliseconds; a locked one may never answer.
const TIMEOUT: Duration = Duration::from_secs(5);
/// `security -i` reads commands into a 4096-byte line buffer and truncates longer ones.
const STDIN_LINE_LIMIT: usize = 4096 - 64;

pub struct Keychain;

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn security(args: &[&str], stdin: Option<&str>) -> Result<Output> {
    let mut child = Command::new(SECURITY)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running /usr/bin/security")?;

    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())?;
    }
    // Drain both pipes off-thread so a chatty child can't block on a full pipe
    // while we wait for it to exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the Keychain did not answer within {}s (is it locked?)",
                TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    Ok(Output {
        code: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn failed(what: &str, out: &Output) -> anyhow::Error {
    anyhow::anyhow!(
        "Keychain {what} failed (exit {}): {}",
        out.code
            .map_or_else(|| "signal".to_string(), |c| c.to_string()),
        out.stderr.trim()
    )
}

/// Quote a value for a `security -i` command line, which is parsed shell-style.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// `find-generic-password -w` prints a secret containing non-ASCII bytes as hex.
/// Logins are JSON, so anything that is pure hex is such an encoding.
fn from_hex(text: &str) -> Option<String> {
    if text.is_empty()
        || !text.len().is_multiple_of(2)
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    String::from_utf8(bytes).ok()
}

impl Secrets for Keychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = security(
            &["find-generic-password", "-a", account, "-s", service, "-w"],
            None,
        )?;
        match out.code {
            Some(0) => {
                let text = out.stdout.strip_suffix('\n').unwrap_or(&out.stdout);
                Ok(Some(from_hex(text).unwrap_or_else(|| text.to_string())))
            }
            Some(NOT_FOUND) => Ok(None),
            _ => Err(failed("read", &out)),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let hex = to_hex(secret.as_bytes());
        // Through stdin the secret never shows up in the process list. A command too
        // long for the stdin line buffer would be truncated and fail, so those go
        // through argv, hex-encoded.
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            quote(account),
            quote(service)
        );
        let out = if line.len() <= STDIN_LINE_LIMIT {
            security(&["-i"], Some(&line))?
        } else {
            security(
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    account,
                    "-s",
                    service,
                    "-X",
                    &hex,
                ],
                None,
            )?
        };
        if out.code == Some(0) {
            Ok(())
        } else {
            Err(failed("write", &out))
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let out = security(
            &["delete-generic-password", "-a", account, "-s", service],
            None,
        )?;
        match out.code {
            Some(0 | NOT_FOUND) => Ok(()),
            _ => Err(failed("delete", &out)),
        }
    }
}

#[cfg(test)]
pub mod testing {
    use super::{Result, Secrets};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory stand-in for the Keychain.
    #[derive(Default)]
    pub struct MemorySecrets(Mutex<HashMap<(String, String), String>>);

    impl Secrets for MemorySecrets {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
            let key = (service.to_string(), account.to_string());
            Ok(self.0.lock().unwrap().get(&key).cloned())
        }
        fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
            let key = (service.to_string(), account.to_string());
            self.0.lock().unwrap().insert(key, secret.to_string());
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            let key = (service.to_string(), account.to_string());
            self.0.lock().unwrap().remove(&key);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let secret = r#"{"name":"Blåbær"}"#;
        assert_eq!(
            from_hex(&to_hex(secret.as_bytes())).as_deref(),
            Some(secret)
        );
        assert_eq!(from_hex(secret), None);
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
