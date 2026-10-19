//! Serde helpers that store secrets (cookies, header values) DPAPI-encrypted as
//! `"dpapi:<base64>"`. Plain strings from older files are accepted on load.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::winsec;

const PREFIX: &str = "dpapi:";

pub fn seal(plain: &str) -> String {
    match winsec::protect(plain.as_bytes()) {
        Some(enc) => format!("{PREFIX}{}", STANDARD.encode(enc)),
        // Never write the secret in clear text if encryption fails.
        None => String::new(),
    }
}

/// `None` if the value is encrypted but cannot be decrypted (e.g. another user/machine).
pub fn open(stored: &str) -> Option<String> {
    match stored.strip_prefix(PREFIX) {
        Some(b64) => {
            let bytes = STANDARD.decode(b64).ok()?;
            String::from_utf8(winsec::unprotect(&bytes)?).ok()
        }
        None => Some(stored.to_string()),
    }
}

pub mod opt {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
        v.as_deref()
            .map(seal)
            .filter(|x| !x.is_empty())
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(Option::<String>::deserialize(d)?
            .and_then(|s| open(&s))
            .filter(|s| !s.is_empty()))
    }
}

/// Header names are kept readable; values are sealed.
pub mod headers {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[(String, String)], s: S) -> Result<S::Ok, S::Error> {
        v.iter()
            .map(|(k, val)| (k.clone(), seal(val)))
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
        Ok(Vec::<(String, String)>::deserialize(d)?
            .into_iter()
            .filter_map(|(k, v)| open(&v).filter(|v| !v.is_empty()).map(|v| (k, v)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open() {
        let s = seal("session=abc");
        assert!(s.starts_with(PREFIX));
        assert!(!s.contains("abc"));
        assert_eq!(open(&s).as_deref(), Some("session=abc"));
        assert_eq!(open("legacy=plain").as_deref(), Some("legacy=plain"));
        assert_eq!(open("dpapi:!!!"), None);
    }
}
