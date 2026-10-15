//! Additive application schema. Imported records are inert historical evidence.
pub(crate) const SQL: &str = "
ALTER TABLE agent_workspaces ADD COLUMN resources TEXT NOT NULL DEFAULT '[]';
CREATE TABLE agent_tool_receipts(task_id TEXT NOT NULL REFERENCES agent_tasks(id),call_id TEXT NOT NULL,tool_name TEXT NOT NULL,arguments_sha256 TEXT NOT NULL CHECK(length(arguments_sha256)=64),result_json TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(task_id,call_id));
CREATE TABLE agent_memories(id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),namespace TEXT NOT NULL,content TEXT NOT NULL,tags_json TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,version INTEGER NOT NULL CHECK(version>=1));
CREATE INDEX agent_memories_owner_namespace ON agent_memories(principal_id,namespace,created_at,id);
CREATE TABLE agent_host_operations(id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),operation TEXT NOT NULL,request_json TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('started','finished','failed','uncertain')),result_json TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE legacy_imports(id TEXT PRIMARY KEY,report_json TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE legacy_records(table_name TEXT NOT NULL,row_number INTEGER NOT NULL,record_id TEXT,payload_json TEXT NOT NULL,PRIMARY KEY(table_name,row_number));
CREATE INDEX legacy_records_identity ON legacy_records(table_name,record_id);
CREATE TABLE legacy_message_details(message_id TEXT PRIMARY KEY,original_status TEXT NOT NULL,original_seq INTEGER NOT NULL,original_created_at TEXT NOT NULL,reasoning TEXT,tool_calls TEXT NOT NULL,tool_call_id TEXT,images TEXT NOT NULL,projection_warning TEXT);
CREATE TABLE legacy_artifacts(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,store_id TEXT NOT NULL,title TEXT NOT NULL,mime TEXT NOT NULL,original_relpath TEXT NOT NULL,original_created_at TEXT NOT NULL,content_kind TEXT NOT NULL CHECK(content_kind IN ('managed_original','derived_reference','external_reference')),bytes BLOB,sha256 TEXT,CHECK((bytes IS NULL AND sha256 IS NULL) OR (bytes IS NOT NULL AND length(sha256)=64)));
CREATE TABLE legacy_tasks(id TEXT PRIMARY KEY,agent_id TEXT,status TEXT NOT NULL,trigger_kind TEXT NOT NULL,input TEXT NOT NULL,output TEXT,error TEXT,transcript TEXT NOT NULL,original_created_at TEXT NOT NULL,original_updated_at TEXT NOT NULL);
";

pub(crate) const REQUIRED: &[(&str, &[&str])] = &[
    ("agent_workspaces", &["resources"]),
    (
        "agent_tool_receipts",
        &[
            "task_id",
            "call_id",
            "tool_name",
            "arguments_sha256",
            "result_json",
            "created_at",
        ],
    ),
    (
        "agent_memories",
        &[
            "id",
            "principal_id",
            "namespace",
            "content",
            "tags_json",
            "created_at",
            "updated_at",
            "version",
        ],
    ),
    (
        "agent_host_operations",
        &[
            "id",
            "principal_id",
            "operation",
            "request_json",
            "state",
            "result_json",
            "created_at",
            "updated_at",
        ],
    ),
    ("legacy_imports", &["id", "report_json", "created_at"]),
    (
        "legacy_records",
        &["table_name", "row_number", "record_id", "payload_json"],
    ),
    (
        "legacy_message_details",
        &[
            "message_id",
            "original_status",
            "original_seq",
            "original_created_at",
            "reasoning",
            "tool_calls",
            "tool_call_id",
            "images",
            "projection_warning",
        ],
    ),
    (
        "legacy_artifacts",
        &[
            "id",
            "session_id",
            "message_id",
            "store_id",
            "title",
            "mime",
            "original_relpath",
            "original_created_at",
            "content_kind",
            "bytes",
            "sha256",
        ],
    ),
    (
        "legacy_tasks",
        &[
            "id",
            "agent_id",
            "status",
            "trigger_kind",
            "input",
            "output",
            "error",
            "transcript",
            "original_created_at",
            "original_updated_at",
        ],
    ),
];
