use std::{fmt, hash::Hasher, path::Path, str::FromStr, time::UNIX_EPOCH};

use rustc_hash::FxHasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A 64-bit fingerprint, used by [`LintCache`] to detect changes to files and configurations.
///
/// It is not cryptographic. It is computed with [`FxHasher`], which uses a fixed seed, so it is
/// stable across runs of the same oxlint build. Cache files are invalidated when the oxlint
/// version changes, so it does not need to be stable across versions.
///
/// [`LintCache`]: super::LintCache
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(u64);

impl Fingerprint {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = FingerprintHasher::default();
        hasher.write(bytes);
        hasher.finish()
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for Fingerprint {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        u64::from_str_radix(s, 16).map(Self)
    }
}

/// Serialized as a hex string, as JSON numbers above 2^53 lose precision in JavaScript.
impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Builds a [`Fingerprint`] from a sequence of inputs.
#[derive(Default, Clone)]
pub struct FingerprintHasher(FxHasher);

impl FingerprintHasher {
    pub fn write(&mut self, bytes: &[u8]) {
        // Prefix with the length, so that `["ab", "c"]` and `["a", "bc"]` differ.
        self.0.write_u64(bytes.len() as u64);
        self.0.write(bytes);
    }

    pub fn write_str(&mut self, s: &str) {
        self.write(s.as_bytes());
    }

    pub fn write_fingerprint(&mut self, fingerprint: Fingerprint) {
        self.0.write_u64(fingerprint.0);
    }

    /// Hash `value` as JSON with object keys sorted, so that the order keys were written in
    /// doesn't matter.
    pub fn write_json<T: Serialize>(&mut self, value: &T) {
        fn sort_keys(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    let mut entries = std::mem::take(map).into_iter().collect::<Vec<_>>();
                    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
                    for (key, mut value) in entries {
                        sort_keys(&mut value);
                        map.insert(key, value);
                    }
                }
                Value::Array(values) => values.iter_mut().for_each(sort_keys),
                _ => {}
            }
        }

        let json = serde_json::to_value(value).and_then(|mut value| {
            sort_keys(&mut value);
            serde_json::to_vec(&value)
        });
        match json {
            Ok(json) => self.write(&json),
            Err(error) => self.write_str(&error.to_string()),
        }
    }

    /// Hash the path and contents of a file. A missing or unreadable file hashes as a marker, so
    /// creating it changes the fingerprint.
    pub fn write_file(&mut self, path: &Path) {
        self.write(path.as_os_str().as_encoded_bytes());
        match std::fs::read(path) {
            Ok(bytes) => self.write(&bytes),
            Err(_) => self.write_str("<unreadable>"),
        }
    }

    /// Hash the path, size, and modification time of a file.
    pub fn write_file_metadata(&mut self, path: &Path) {
        self.write(path.as_os_str().as_encoded_bytes());
        let metadata = path.metadata().ok();
        let mtime = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok());
        self.0.write_u64(metadata.map_or(u64::MAX, |metadata| metadata.len()));
        self.0.write_u128(mtime.map_or(u128::MAX, |mtime| mtime.as_nanos()));
    }

    pub fn finish(&self) -> Fingerprint {
        Fingerprint(self.0.finish())
    }
}

#[cfg(test)]
mod test {
    use super::{Fingerprint, FingerprintHasher};

    #[test]
    fn json_key_order_does_not_matter() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
        let c: serde_json::Value = serde_json::from_str(r#"{"b":{"d":3,"c":4},"a":1}"#).unwrap();

        let hash = |value| {
            let mut hasher = FingerprintHasher::default();
            hasher.write_json(value);
            hasher.finish()
        };
        assert_eq!(hash(&a), hash(&b));
        assert_ne!(hash(&a), hash(&c));
    }

    #[test]
    fn inputs_are_delimited() {
        let hash = |parts: &[&str]| {
            let mut hasher = FingerprintHasher::default();
            for part in parts {
                hasher.write_str(part);
            }
            hasher.finish()
        };
        assert_ne!(hash(&["ab", "c"]), hash(&["a", "bc"]));
    }

    #[test]
    fn serializes_as_hex_string() {
        let fingerprint = Fingerprint::of_bytes(b"oxlint");
        let json = serde_json::to_string(&fingerprint).unwrap();
        assert_eq!(json, format!("\"{fingerprint}\""));
        assert_eq!(serde_json::from_str::<Fingerprint>(&json).unwrap(), fingerprint);
    }
}
