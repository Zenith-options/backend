//! SEP-7 transaction signing URI construction.
//!
//! This module only formats a deep link for a transaction envelope created
//! elsewhere. It does not construct settlement operations, validate signed
//! XDR, or submit transactions.

use data_encoding::BASE64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sep7Error {
    InvalidXdr,
    InvalidNetworkPassphrase,
    InvalidCallback,
}

impl std::fmt::Display for Sep7Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidXdr => write!(f, "transaction XDR must be valid non-empty base64"),
            Self::InvalidNetworkPassphrase => write!(f, "network passphrase is required"),
            Self::InvalidCallback => write!(f, "callback must be an absolute HTTPS URL"),
        }
    }
}

impl std::error::Error for Sep7Error {}

/// Build a SEP-7 `web+stellar:tx` URI for a base64-encoded transaction
/// envelope. The callback is supplied by the integrating backend and should
/// include an opaque, single-use request identifier.
pub fn transaction_signing_uri(
    envelope_xdr: &str,
    network_passphrase: &str,
    callback_url: &str,
) -> Result<String, Sep7Error> {
    if envelope_xdr.is_empty()
        || BASE64
            .decode(envelope_xdr.as_bytes())
            .map(|bytes| bytes.is_empty())
            .unwrap_or(true)
    {
        return Err(Sep7Error::InvalidXdr);
    }
    if network_passphrase.trim().is_empty() {
        return Err(Sep7Error::InvalidNetworkPassphrase);
    }
    let callback_host = callback_url
        .strip_prefix("https://")
        .and_then(|url| url.split(['/', '?', '#']).next())
        .unwrap_or_default();
    if callback_host.is_empty()
        || callback_host.contains('@')
        || callback_host.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || callback_url.chars().any(char::is_control)
    {
        return Err(Sep7Error::InvalidCallback);
    }

    Ok(format!(
        "web+stellar:tx?xdr={}&network_passphrase={}&callback={}",
        percent_encode(envelope_xdr.as_bytes()),
        percent_encode(network_passphrase.as_bytes()),
        percent_encode(callback_url.as_bytes())
    ))
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}
