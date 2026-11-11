//! A small stderr logger with RUST_LOG-style filters: comma-separated
//! `target=level` directives plus an optional bare default level, where the
//! longest matching target prefix wins (e.g. `snack=debug,xmpp=debug,info`).
use log::{LevelFilter, Log, Metadata, Record};

struct Logger {
    default: LevelFilter,
    // (target prefix, level), longest prefix first.
    directives: Vec<(String, LevelFilter)>,
}

fn level(text: &str) -> Option<LevelFilter> {
    text.trim().parse().ok()
}

fn parse(spec: &str) -> Logger {
    let mut logger = Logger { default: LevelFilter::Error, directives: Vec::new() };
    for directive in spec.split(',').map(str::trim).filter(|directive| !directive.is_empty()) {
        match directive.split_once('=') {
            Some((target, value)) => if let Some(value) = level(value) {
                logger.directives.push((target.trim().to_string(), value));
            },
            None => match level(directive) {
                Some(value) => logger.default = value,
                // A bare target enables everything for it, as in env_logger.
                None => logger.directives.push((directive.to_string(), LevelFilter::Trace)),
            },
        }
    }
    logger.directives.sort_by_key(|(target, _)| std::cmp::Reverse(target.len()));
    logger
}

impl Logger {
    fn filter(&self, target: &str) -> LevelFilter {
        self.directives.iter()
            .find(|(prefix, _)| target == prefix || target.strip_prefix(prefix.as_str()).is_some_and(|rest| rest.starts_with("::")))
            .map_or(self.default, |(_, level)| *level)
    }

    fn max(&self) -> LevelFilter {
        self.directives.iter().map(|(_, level)| *level).fold(self.default, Ord::max)
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.filter(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) { return; }
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "[{} {:5} {}] {}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"), record.level(), record.target(), record.args());
    }

    fn flush(&self) {}
}

/// Install the logger, using RUST_LOG when set and `default_spec` otherwise.
pub fn init(default_spec: &str) {
    let spec = std::env::var("RUST_LOG").unwrap_or_else(|_| default_spec.to_string());
    let logger = parse(&spec);
    let max = logger.max();
    // The logger lives for the whole process.
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() { log::set_max_level(max); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_matching_target_prefix_sets_the_level() {
        let logger = parse("snack=debug, xmpp=debug ,info,snack::update=warn,bogus=loud");
        assert_eq!(logger.filter("snack"), LevelFilter::Debug);
        assert_eq!(logger.filter("snack::notifications::macos"), LevelFilter::Debug);
        assert_eq!(logger.filter("snack::update"), LevelFilter::Warn);
        assert_eq!(logger.filter("snackbar"), LevelFilter::Info, "prefixes match whole path segments");
        assert_eq!(logger.filter("iced_winit"), LevelFilter::Info);
        assert_eq!(logger.max(), LevelFilter::Debug);
        assert_eq!(parse("").filter("anything"), LevelFilter::Error);
        assert_eq!(parse("snack").filter("snack::update"), LevelFilter::Trace);
        assert_eq!(parse("off").max(), LevelFilter::Off);
    }
}
