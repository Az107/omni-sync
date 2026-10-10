//! Folder registry and device membership.

use rusqlite::{Connection, params};

use crate::StoreError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub retention_versions: i64,
    pub retention_days: i64,
    pub encrypted: bool,
}

pub fn create(conn: &Connection, folder: &Folder) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO folders (id, name, retention_versions, retention_days, encrypted)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            folder.id,
            folder.name,
            folder.retention_versions,
            folder.retention_days,
            folder.encrypted as i64
        ],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<Folder>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, retention_versions, retention_days, encrypted
           FROM folders ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Folder {
            id: row.get(0)?,
            name: row.get(1)?,
            retention_versions: row.get(2)?,
            retention_days: row.get(3)?,
            encrypted: row.get::<_, i64>(4)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM folders", [], |row| row.get(0))?)
}
