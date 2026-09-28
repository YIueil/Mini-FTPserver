use encoding_rs::GBK;

/// Control-connection text encoding. Windows FTP clients (Explorer, ftp.exe)
/// speak the local ANSI code page — GBK on zh-CN systems — while RFC 2640
/// clients switch to UTF-8 via `OPTS UTF8 ON`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Ansi,
    Utf8,
}

impl Encoding {
    pub fn decode(self, bytes: &[u8]) -> String {
        match self {
            // GBK-encoded CJK text is never valid UTF-8, so trying UTF-8
            // first is safe and also serves clients that silently use UTF-8.
            Self::Ansi => match std::str::from_utf8(bytes) {
                Ok(s) => s.to_owned(),
                Err(_) => GBK.decode(bytes).0.into_owned(),
            },
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    pub fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Self::Ansi => GBK.encode(text).0.into_owned(),
            Self::Utf8 => text.as_bytes().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_round_trips_gbk() {
        let gbk = GBK.encode("中文目录").0;
        assert_eq!(Encoding::Ansi.decode(&gbk), "中文目录");
        assert_eq!(Encoding::Ansi.encode("中文目录"), gbk.as_ref());
    }

    #[test]
    fn ansi_accepts_utf8_transparently() {
        assert_eq!(Encoding::Ansi.decode("中文目录".as_bytes()), "中文目录");
    }

    #[test]
    fn ascii_is_identical_in_both_modes() {
        assert_eq!(Encoding::Ansi.encode("LIST /pub"), b"LIST /pub");
        assert_eq!(Encoding::Utf8.encode("LIST /pub"), b"LIST /pub");
    }
}
