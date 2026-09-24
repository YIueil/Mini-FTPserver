/// A parsed FTP command line: uppercase verb + verbatim argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpCommand {
    pub verb: String,
    pub arg: String,
}

pub fn parse(line: &str) -> FtpCommand {
    let line = line.trim_end_matches(['\r', '\n']);
    let (verb, arg) = match line.find(' ') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, ""),
    };
    let mut verb = verb.to_uppercase();
    // Original quirk: any command whose last 4 chars are "ABOR" is treated as ABOR.
    if verb.len() >= 4 && verb.ends_with("ABOR") {
        verb = "ABOR".to_string();
    }
    FtpCommand {
        verb,
        arg: arg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verb_and_arg() {
        let cmd = parse("RETR some file.txt\r\n");
        assert_eq!(cmd.verb, "RETR");
        assert_eq!(cmd.arg, "some file.txt");
    }

    #[test]
    fn verb_without_arg() {
        let cmd = parse("PASV");
        assert_eq!(cmd.verb, "PASV");
        assert_eq!(cmd.arg, "");
    }

    #[test]
    fn verb_is_uppercased() {
        assert_eq!(parse("list /tmp").verb, "LIST");
    }

    #[test]
    fn abor_suffix_quirk() {
        assert_eq!(parse("XXABOR").verb, "ABOR");
        assert_eq!(parse("ABOR").verb, "ABOR");
        assert_ne!(parse("ABO").verb, "ABOR");
    }
}
