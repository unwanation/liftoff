#![cfg(test)]
//! The SQLite store: its plumbing, and the `Store` contract it shares with
//! `InMemoryStore`.

use liftoff_store::SqliteStore;

mod open {
    use super::*;

    /// The journal mode is persistent in the file: a second connection
    /// reads it back.
    fn journal_mode(path: &std::path::Path) -> String {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn answers_ping_when_file_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("liftoff.db")).unwrap();
        assert!(store.ping().await.is_ok(), "a fresh database answers");
    }

    #[test]
    fn uses_wal_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        let _store = SqliteStore::open(&path).unwrap();
        assert_eq!(journal_mode(&path), "wal");
    }

    #[test]
    fn fails_when_file_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        std::fs::write(&path, "not a database, just text that is long enough").unwrap();
        assert!(SqliteStore::open(&path).is_err(), "garbage is refused");
    }

    #[tokio::test]
    async fn answers_ping_when_in_memory() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.ping().await.is_ok(), "in-memory database answers");
    }
}

mod contract {
    use super::*;

    liftoff_core::store_contract_tests!(|| SqliteStore::open_in_memory().unwrap());
}

mod file {
    use liftoff_core::domain::{Login, UserRole};
    use liftoff_core::fakes::Records;
    use liftoff_core::ports::Store as _;

    use super::*;

    /// Rows written through one store are read back after the file is
    /// opened again: the schema is migrated once and the data persists.
    #[tokio::test]
    async fn keeps_rows_when_file_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        let owner = Records::default().user("owner", UserRole::Admin).unwrap();
        SqliteStore::open(&path)
            .unwrap()
            .insert_user(&owner)
            .await
            .unwrap();
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .user_by_login(&Login::new("owner").unwrap())
                .await
                .unwrap(),
            Some(owner)
        );
    }
}

mod one_time_links {
    use liftoff_core::domain::{LinkSecret, LinkTarget, OneTimeLink, SecretBody, UserRole};
    use liftoff_core::fakes::Records;
    use liftoff_core::ports::Store as _;
    use time::Duration;

    use super::*;

    // TC: OTL-05 (the row holds the SHA-256 of the token, never the token)
    #[tokio::test]
    async fn stores_sha256_of_token_and_never_token_when_inserted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        let records = Records::default();
        let owner = records.user("owner", UserRole::Admin).unwrap();
        let shop = records
            .project("shop", "git.example.com", "owner/shop", &owner)
            .unwrap();
        let tunnel = records.tunnel(&shop, "shop-dev", &owner).unwrap();
        let store = SqliteStore::open(&path).unwrap();
        store.insert_user(&owner).await.unwrap();
        store.insert_project(&shop).await.unwrap();
        store.insert_environment(&tunnel).await.unwrap();
        let secret = LinkSecret::from_body(&SecretBody::new(&"k".repeat(40)).unwrap());
        store
            .insert_one_time_link(&OneTimeLink {
                token_hash: secret.hash(),
                user_id: owner.id,
                target: LinkTarget::Environment(tunnel.id),
                expires_at: owner.created_at.saturating_add(Duration::minutes(10)),
                used_at: None,
                created_at: owner.created_at,
            })
            .await
            .unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&path).unwrap();
        let (hash, row): (Vec<u8>, String) = conn
            .query_row(
                "SELECT token_hash, quote(token_hash) || user_id || target || expires_at FROM one_time_links",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (
                hash,
                row.contains(secret.expose()),
                row.contains(&format!("environment:{}", tunnel.id))
            ),
            (secret.hash().as_bytes().to_vec(), false, true)
        );
    }
}

mod component_hash {
    use liftoff_core::domain::{ComponentHash, DeployStatus, UserRole};
    use liftoff_core::fakes::Records;
    use liftoff_core::ports::Store as _;

    use super::*;

    /// A file store holding a function deploy with a valid component
    /// hash; the path of its file.
    async fn stored_function_deploy(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("liftoff.db");
        let records = Records::default();
        let owner = records.user("owner", UserRole::Admin).unwrap();
        let shop = records
            .project("shop", "git.example.com", "owner/shop", &owner)
            .unwrap();
        let function = records.function(&shop, "api").unwrap();
        let mut deploy = records
            .deploy(&function, 1, DeployStatus::Succeeded, &owner)
            .unwrap();
        deploy.component_hash = Some(ComponentHash::of(b"\0asm component"));
        let store = SqliteStore::open(&path).unwrap();
        store.insert_user(&owner).await.unwrap();
        store.insert_project(&shop).await.unwrap();
        store.insert_environment(&function).await.unwrap();
        store.insert_deploy(&deploy).await.unwrap();
        path
    }

    /// Whether the database refuses to set the stored deploy's
    /// `component_hash` to `raw` behind the store's back.
    async fn refuses(raw: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = stored_function_deploy(&dir).await;
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("UPDATE deploys SET component_hash = ?1", [raw])
            .is_err()
    }

    /// The SHA-256 of `abc`.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    // TC: FDEP-05 (the CHECK accepts what `ComponentHash::of` gives)
    #[tokio::test]
    async fn accepts_lower_case_sha256_hex_when_written_raw() {
        assert!(!refuses(ABC).await);
    }

    // TC: FDEP-05 (migration 0010: 64 lower-case hex characters)
    #[tokio::test]
    async fn refuses_upper_case_when_written_raw() {
        assert!(refuses(&ABC.to_ascii_uppercase()).await);
    }

    // TC: FDEP-05
    #[tokio::test]
    async fn refuses_63_characters_when_written_raw() {
        assert!(refuses(ABC.strip_suffix('d').unwrap()).await);
    }

    // TC: FDEP-05
    #[tokio::test]
    async fn refuses_non_hex_when_written_raw() {
        assert!(refuses(&ABC.replacen('b', "g", 1)).await);
    }
}
