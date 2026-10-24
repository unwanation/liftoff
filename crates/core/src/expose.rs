//! `lof expose`: the name a local port is reached by.
//!
//! Every exposed port lives under the zone `localhost.<base>`
//! ([`ZONE`]). A `lof expose` run on the daemon's own machine names
//! the port itself (`3000.localhost.<base>`); anywhere else the address
//! is the user's login and an id of the user and the port
//! (`alice-3f9a1c2b.localhost.<base>`): stable from one run to the next,
//! one per user and port, and it tells nothing of the port.
//!
//! Pure: the daemon decides whether the client is on its machine and
//! hands the answer in.

use core::fmt::Write as _;

use sha2::{Digest as _, Sha256};

use crate::domain::LABEL_MAX_LEN;

/// The label under the base that holds every exposed port.
pub const ZONE: &str = "localhost";

/// How many bytes of the digest the id is.
const ID_BYTES: usize = 4;

/// How many hex digits of the id follow the login.
const ID_LEN: usize = 2 * ID_BYTES;

/// The name of `port` exposed by the user `user_id` (`login`).
///
/// The port itself when `on_server` (the client runs on the daemon's
/// machine), else `<login>-<id>`, the login lowercased with every
/// character that is no ASCII letter or digit a `-`, cut so that the
/// name stays a label of [`LABEL_MAX_LEN`] characters. The caller checks
/// that it is an environment name.
#[must_use]
pub fn exposure_name(login: &str, user_id: &str, port: u16, on_server: bool) -> String {
    if on_server {
        return port.to_string();
    }
    let digest = Sha256::new()
        .chain_update(user_id.as_bytes())
        .chain_update(b":")
        .chain_update(port.to_be_bytes())
        .finalize();
    let mut id = String::with_capacity(ID_LEN);
    for byte in digest.iter().take(ID_BYTES) {
        let _infallible = write!(id, "{byte:02x}");
    }
    let mangled: String = login
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let room = LABEL_MAX_LEN - ID_LEN - 1;
    let login: String = mangled.chars().take(room).collect();
    let login = login.trim_matches('-');
    if login.is_empty() {
        id
    } else {
        format!("{login}-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EnvName;

    mod exposure_name {
        use super::*;

        // TC: EXP-01 (the daemon's own machine names the port)
        #[test]
        fn port_itself_when_on_server() {
            assert_eq!(exposure_name("alice", "u1", 3000, true), "3000");
        }

        // TC: EXP-02 (elsewhere: the login and an id, not the port)
        #[test]
        fn login_and_id_without_port_when_elsewhere() {
            let name = exposure_name("alice", "u1", 3000, false);
            assert!(name.starts_with("alice-"), "{name}");
            assert!(!name.contains("3000"), "{name}");
            assert_eq!(name.len(), "alice-".len() + ID_LEN);
        }

        #[test]
        fn same_name_when_exposed_again() {
            assert_eq!(
                exposure_name("alice", "u1", 3000, false),
                exposure_name("alice", "u1", 3000, false)
            );
        }

        #[test]
        fn other_name_when_port_differs() {
            assert_ne!(
                exposure_name("alice", "u1", 3000, false),
                exposure_name("alice", "u1", 3001, false)
            );
        }

        #[test]
        fn other_name_when_user_differs() {
            assert_ne!(
                exposure_name("alice", "u1", 3000, false),
                exposure_name("alice", "u2", 3000, false)
            );
        }

        #[test]
        fn environment_name_when_login_is_long_and_odd() {
            let name = exposure_name("Jean.Pierre_Dupont-de-la-Fontaine", "u1", 80, false);
            assert!(EnvName::new(&name).is_ok(), "{name}");
            assert!(name.starts_with("jean-pierre-dupont-"), "{name}");
        }

        #[test]
        fn only_id_when_login_has_no_letter_or_digit() {
            let name = exposure_name("__", "u1", 80, false);
            assert_eq!(name.len(), ID_LEN);
            assert!(EnvName::new(&name).is_ok(), "{name}");
        }
    }
}
