//! photo-stamp versions (see `scripts/version.sh`) and their order: `0.7.0` < `0.7.1-dev.3+g1b37dbe`
//! < `0.7.1-dev.12+gab12cd3` < `0.7.1`. Semantic versioning: build metadata (`+…`) doesn't count,
//! a pre-release sorts before its release, and pre-release parts compare numerically when numeric.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release parts ("dev", "7"); empty for a release.
    pub pre: Vec<String>,
    /// Everything as written, build metadata included.
    pub text: String,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let text = s.trim().trim_start_matches('v').to_owned();
        let core_pre = text.split('+').next()?;
        let (core, pre) = match core_pre.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_owned).collect()),
            None => (core_pre, Vec::new()),
        };
        let mut nums = core.split('.').map(|n| n.parse::<u64>().ok());
        let (major, minor, patch) = (nums.next()??, nums.next()??, nums.next()??);
        if nums.next().is_some() {
            return None;
        }
        Some(Self { major, minor, patch, pre, text })
    }

    /// A release (from a tag) rather than a build between releases.
    pub fn is_stable(&self) -> bool {
        self.pre.is_empty()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn cmp_part(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(
            || match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        match cmp_part(a, b) {
                            Ordering::Equal => {}
                            o => return o,
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            },
        )
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn orders_builds_between_releases() {
        let order =
            ["0.6.0", "0.6.1-dev.2+gaaa", "0.6.1-dev.10+gbbb", "0.6.1", "0.7.0-dev.1+gccc", "0.7.0", "1.0.0"];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("v0.7.0").cmp(&v("0.7.0")), Ordering::Equal);
        assert_eq!(
            v("0.7.1-dev.3+gaaa").cmp(&v("0.7.1-dev.3+gbbb.dirty")),
            Ordering::Equal,
            "build metadata doesn't count"
        );
        assert!(v("0.7.0").is_stable() && !v("0.7.1-dev.3+gaaa").is_stable());
        assert_eq!(v("0.7.1-dev.3+gaaa").to_string(), "0.7.1-dev.3+gaaa");
        for bad in ["", "1.2", "1.2.3.4", "x.y.z"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }
}
