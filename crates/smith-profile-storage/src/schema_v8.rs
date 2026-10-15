//! Saved-agent definitions and per-message feedback. No permissions activate
//! merely by storing a definition; launch requires the application grant checks.
pub(crate) const SQL: &str = "
CREATE TABLE agent_definitions(id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),revision INTEGER NOT NULL CHECK(revision>=1),body_json TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)),created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
CREATE INDEX agent_definitions_owner_updated ON agent_definitions(principal_id,deleted,updated_at,id);
CREATE TABLE agent_definition_launches(request_id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),definition_id TEXT NOT NULL,definition_revision INTEGER NOT NULL CHECK(definition_revision>=1),task_id TEXT NOT NULL UNIQUE REFERENCES agent_tasks(id),request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64),created_at INTEGER NOT NULL);
CREATE TABLE message_feedback(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN('','thumbs_up','thumbs_down')),revision INTEGER NOT NULL CHECK(revision>=1),updated_at INTEGER NOT NULL);
";

pub(crate) const REQUIRED: &[(&str, &[&str])] = &[
    (
        "agent_definitions",
        &[
            "id",
            "principal_id",
            "revision",
            "body_json",
            "deleted",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "agent_definition_launches",
        &[
            "request_id",
            "principal_id",
            "definition_id",
            "definition_revision",
            "task_id",
            "request_sha256",
            "created_at",
        ],
    ),
    (
        "message_feedback",
        &["message_id", "kind", "revision", "updated_at"],
    ),
];
