//! `--direct-io` (012 FR-003, 023 FR-010): whether the engine's files bypass the kernel's page
//! cache. The engine opens every file with `O_DIRECT`; the option says what it does where the file
//! system does not take it.

/// `--direct-io`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectIo {
    /// `O_DIRECT` where the file system takes it, the page cache elsewhere.
    #[default]
    Auto,
    /// `O_DIRECT`; a file the file system cannot open so fails to open.
    On,
    /// The kernel's page cache for every file.
    Off,
}

impl DirectIo {
    /// The value of `--direct-io`.
    pub fn parse(s: &str) -> Option<DirectIo> {
        match s {
            "auto" => Some(DirectIo::Auto),
            "on" => Some(DirectIo::On),
            "off" => Some(DirectIo::Off),
            _ => None,
        }
    }

    /// The options of the engine's command line that give it: `--relaxed-dma` lets a file
    /// system without `O_DIRECT` fall back to the page cache, `--kernel-page-cache` uses the page
    /// cache everywhere.
    #[must_use]
    pub fn engine_args(self) -> Vec<String> {
        match self {
            DirectIo::Auto => vec!["--relaxed-dma".to_owned()],
            DirectIo::On => Vec::new(),
            DirectIo::Off => vec!["--kernel-page-cache".to_owned(), "1".to_owned()],
        }
    }

    /// How the node's log says it.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            DirectIo::Auto => "disk: O_DIRECT where the file system takes it",
            DirectIo::On => "disk: O_DIRECT",
            DirectIo::Off => "disk: the kernel's page cache",
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_option_parses_and_gives_the_engine_its_arguments() {
        assert_eq!(DirectIo::parse("auto"), Some(DirectIo::Auto));
        assert_eq!(DirectIo::parse("on"), Some(DirectIo::On));
        assert_eq!(DirectIo::parse("off"), Some(DirectIo::Off));
        assert_eq!(DirectIo::parse("1"), None);
        assert_eq!(DirectIo::default(), DirectIo::Auto);
        assert_eq!(DirectIo::Auto.engine_args(), ["--relaxed-dma"]);
        assert!(DirectIo::On.engine_args().is_empty());
        assert_eq!(DirectIo::Off.engine_args(), ["--kernel-page-cache", "1"]);
        assert!(DirectIo::Off.describe().contains("page cache"));
        assert!(DirectIo::On.describe().ends_with("O_DIRECT"));
        assert!(DirectIo::Auto.describe().contains("where"));
    }
}
