//! Memory curation is additive. Historical memories project as unpinned and
//! active; their existing version advances atomically with every metadata edit.
pub(crate) const SQL: &str = "
CREATE TABLE memory_metadata(memory_id TEXT PRIMARY KEY REFERENCES agent_memories(id) ON DELETE CASCADE,pinned INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN(0,1)),deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)),updated_at INTEGER NOT NULL);
";
pub(crate) const REQUIRED: &[(&str, &[&str])] = &[(
    "memory_metadata",
    &["memory_id", "pinned", "deleted", "updated_at"],
)];
