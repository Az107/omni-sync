//! Folder registry and device membership.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// A shared folder and its cross-client sync policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub retention_versions: i64,
    pub retention_days: i64,
    pub encrypted: bool,
    /// Path patterns every client ignores, in addition to the built-in
    /// droppings (`.DS_Store`, `*.part`, ...). Matched by
    /// [`omni_core::Ignore`].
    pub exclude: Vec<String>,
    /// When false, clients report `mode = 0` and leave permissions alone on
    /// apply. That is what lets a browser/mobile client share a folder with a
    /// unix daemon without a mode ping-pong.
    pub preserve_permissions: bool,
}

impl Folder {
    /// A folder with the server defaults: no excludes, permissions preserved.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
            exclude: Vec::new(),
            preserve_permissions: true,
        }
    }
}

pub fn create(conn: &Connection, folder: &Folder) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO folders
             (id, name, retention_versions, retention_days, encrypted, exclude, preserve_permissions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            folder.id,
            folder.name,
            folder.retention_versions,
            folder.retention_days,
            folder.encrypted as i64,
            serde_json::to_string(&folder.exclude).unwrap_or_else(|_| "[]".to_string()),
            folder.preserve_permissions as i64
        ],
    )?;
    Ok(())
}

pub fn exists(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Folder>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id, name, retention_versions, retention_days, encrypted, exclude, preserve_permissions
               FROM folders WHERE id = ?1",
            [id],
            row_to_folder,
        )
        .optional()?)
}

pub fn list(conn: &Connection) -> Result<Vec<Folder>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, retention_versions, retention_days, encrypted, exclude, preserve_permissions
           FROM folders ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], row_to_folder)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Update the folder's sync policy. `None` leaves a field unchanged.
///
/// Returns `false` when the folder does not exist, so the HTTP layer can answer
/// 404 instead of silently doing nothing.
pub fn update_settings(
    conn: &Connection,
    id: &str,
    exclude: Option<&[String]>,
    preserve_permissions: Option<bool>,
) -> Result<bool, StoreError> {
    let exclude = exclude
        .map(|patterns| serde_json::to_string(patterns).unwrap_or_else(|_| "[]".to_string()));
    let changed = conn.execute(
        "UPDATE folders
            SET exclude = COALESCE(?2, exclude),
                preserve_permissions = COALESCE(?3, preserve_permissions)
          WHERE id = ?1",
        params![id, exclude, preserve_permissions.map(|value| value as i64)],
    )?;
    Ok(changed > 0)
}

fn row_to_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    let exclude: String = row.get(5)?;
    Ok(Folder {
        id: row.get(0)?,
        name: row.get(1)?,
        retention_versions: row.get(2)?,
        retention_days: row.get(3)?,
        encrypted: row.get::<_, i64>(4)? != 0,
        // A hand-edited or truncated column must not make listing folders
        // unusable; an unreadable policy degrades to "no excludes".
        exclude: serde_json::from_str(&exclude).unwrap_or_default(),
        preserve_permissions: row.get::<_, i64>(6)? != 0,
    })
}

pub fn count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM folders", [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn folder_settings_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut folder = Folder::new("f1", "Vault");
        folder.exclude = vec![".obsidian/workspace.json".into(), ".trash/".into()];
        folder.preserve_permissions = false;
        store.with(|conn| create(conn, &folder)).unwrap();

        assert_eq!(store.with(|conn| get(conn, "f1")).unwrap(), Some(folder));
    }

    #[test]
    fn update_settings_leaves_omitted_fields_alone() {
        let store = Store::open_in_memory().unwrap();
        let folder = Folder::new("f1", "Docs");
        store.with(|conn| create(conn, &folder)).unwrap();

        let exclude = vec!["*.tmp".to_string()];
        assert!(
            store
                .with(|conn| update_settings(conn, "f1", Some(&exclude), None))
                .unwrap()
        );

        let updated = store.with(|conn| get(conn, "f1")).unwrap().unwrap();
        assert_eq!(updated.exclude, exclude);
        assert!(
            updated.preserve_permissions,
            "the omitted field must not move"
        );
    }

    #[test]
    fn updating_a_missing_folder_reports_false() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            !store
                .with(|conn| update_settings(conn, "nope", None, Some(false)))
                .unwrap()
        );
    }
}
