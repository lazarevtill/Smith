//! Bounded built-in catalog command. The host supplies its fixed mode prefix;
//! this path exits before desktop ownership, Tokio, profiles or network setup.
use std::{ffi::OsString, io::Write};

pub(crate) const ECHO_MODE: &str = "--smith-catalog-echo";
const MAX_ARGUMENTS: usize = 64;
const MAX_OUTPUT_BYTES: usize = 8192;

fn prepare(args: impl IntoIterator<Item = OsString>) -> Option<Result<Vec<u8>, ()>> {
    let mut args = args.into_iter();
    if args.next()?.as_os_str() != ECHO_MODE {
        return None;
    }
    let mut output = Vec::new();
    for (index, argument) in args.enumerate() {
        let Some(argument) = argument.to_str() else {
            return Some(Err(()));
        };
        let separator = usize::from(index != 0);
        if index >= MAX_ARGUMENTS
            || argument.chars().any(char::is_control)
            || output.len() + separator + argument.len() + 1 > MAX_OUTPUT_BYTES
        {
            return Some(Err(()));
        }
        if separator != 0 {
            output.push(b' ');
        }
        output.extend_from_slice(argument.as_bytes());
    }
    output.push(b'\n');
    Some(Ok(output))
}

pub(crate) fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    match prepare(args)? {
        Ok(output) => Some(if std::io::stdout().lock().write_all(&output).is_ok() {
            0
        } else {
            74
        }),
        Err(()) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(b"Smith catalog echo requires at most 64 UTF-8 arguments and 8192 output bytes, without control characters.\n");
            Some(64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    #[test]
    fn mode_is_exact_and_arguments_have_no_option_or_shell_semantics() {
        assert_eq!(prepare(args(&[])), None);
        assert_eq!(prepare(args(&["--smith-catalog-echo-other"])), None);
        assert_eq!(prepare(args(&[ECHO_MODE])), Some(Ok(b"\n".to_vec())));
        assert_eq!(
            prepare(args(&[ECHO_MODE, "-n", "$(touch nowhere)", "é", ""])),
            Some(Ok("-n $(touch nowhere) é \n".as_bytes().to_vec()))
        );
    }
    #[test]
    fn oversized_or_control_input_never_produces_partial_output() {
        let exact = "x".repeat(MAX_OUTPUT_BYTES - 1);
        assert_eq!(
            prepare(args(&[ECHO_MODE, &exact])).unwrap().unwrap().len(),
            MAX_OUTPUT_BYTES
        );
        assert_eq!(prepare(args(&[ECHO_MODE, &(exact + "x")])), Some(Err(())));
        assert_eq!(
            prepare(args(&[ECHO_MODE, "good", "bad\nline"])),
            Some(Err(()))
        );
        let mut many = vec![OsString::from(ECHO_MODE)];
        many.extend((0..MAX_ARGUMENTS + 1).map(|_| OsString::from("x")));
        assert_eq!(prepare(many), Some(Err(())));
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_are_rejected_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(
            prepare([OsString::from(ECHO_MODE), OsString::from_vec(vec![255])]),
            Some(Err(()))
        );
    }
}
