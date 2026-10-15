//! Declarative extension records and immutable import receipts. These tables do
//! not grant tool authority or start work; imported definitions begin disabled.
pub(crate) const SQL: &str = "
CREATE TABLE agent_extensions(id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),kind TEXT NOT NULL CHECK(kind IN('skill','method','plugin','recipe')),revision INTEGER NOT NULL CHECK(revision>=1),body_json TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN(0,1)),deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)),provenance_json TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
CREATE INDEX agent_extensions_owner_page ON agent_extensions(principal_id,kind,deleted,id);
CREATE TABLE agent_extension_imports(request_id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),source_sha256 TEXT NOT NULL CHECK(length(source_sha256)=64),request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64),extension_id TEXT NOT NULL,result_json TEXT NOT NULL,created_at INTEGER NOT NULL);
";

pub(crate) const REQUIRED: &[(&str, &[&str])] = &[
    (
        "agent_extensions",
        &[
            "id",
            "principal_id",
            "kind",
            "revision",
            "body_json",
            "enabled",
            "deleted",
            "provenance_json",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "agent_extension_imports",
        &[
            "request_id",
            "principal_id",
            "source_sha256",
            "request_sha256",
            "extension_id",
            "result_json",
            "created_at",
        ],
    ),
];
